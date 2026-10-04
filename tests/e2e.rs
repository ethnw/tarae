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

mod harness;

use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;

use harness::*;

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn version_and_help_print_without_a_terminal() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("tarae {}", env!("CARGO_PKG_VERSION")));
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage: tarae [FILE]"));
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
    assert!(!t.screen().contains("NORMAL"), "back on the main screen:\n{}", t.screen());
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
    t.wait_for("a redraw at 20x5 with the status line", |s| s.rows.len() == 5 && s.status().contains("NOR"));
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
    // `?` opens the palette: input row `› …  commands  n/n`
    t.send("?");
    t.wait_for("the command palette", |s| s.rows.iter().any(|r| r.contains('›') && r.contains("commands")));
    t.send("save");
    t.wait_for("the query in the palette", |s| s.contains("› save"));
    let s = t.esc("the palette to close", |s| !s.contains("commands") && s.contains("palette test"));
    assert!(s.status().contains("NORMAL"), "{s}");
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
    t.wait_for("INSERT mode", |s| s.status().contains("INSERT"));
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
/// back, `space T` hides it.
#[test]
fn terminal_panel_runs_the_shell_and_gives_the_keys_back() {
    let sb = Sandbox::new("terminal");
    sb.write("t.txt", "the buffer\n");
    let mut t = sb.spawn(&["t.txt"]);
    t.wait_text("the buffer");
    t.send(" T");
    t.wait_text("TERMINAL");
    // The shell may still be starting — its line discipline keeps what's typed until it reads
    t.send("echo sum-$((40+2))\r");
    t.wait_text("sum-42");
    assert!(!t.screen().contains("the bufferecho"), "the buffer never saw the keys");
    t.send("\x1c");
    t.send("i");
    t.wait_for("INSERT mode in the code", |s| s.status().contains("INSERT"));
    t.normal_mode();
    t.send(" T");
    t.wait_for("the terminal hidden", |s| !s.contains("TERMINAL") && !s.contains("sum-42"));
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
    t.wait_text("TERMINAL");
    t.send("yes flood-line\r");
    t.wait_text("flood-line");
    std::thread::sleep(std::time::Duration::from_secs(1)); // a second of it piling up
    let asked = std::time::Instant::now();
    t.send("\x1c");
    t.send("i");
    // (an editor the flood takes never handles them — the bound only has to outlast a slow runner)
    let inserting = t.poll_for(std::time::Duration::from_secs(5), |s| s.status().contains("INSERT"));
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
    t.wait_text("TERMINAL");
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
    t.send("\x1c");
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
/// files on disk as they were — unsaved text is lost either way (nothing keeps it), never written.
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
    t.wait_for("the task failed", |s| s.contains("check ▲  │  failed") && s.contains("checking"));
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
    t.send(" gg");
    t.wait_for("the change listed", |s| s.contains("Changes") && s.contains("a.txt"));
    // The selection starts on the file being edited
    t.send("s");
    t.wait_text("Staged a.txt");
    // Right away — before the status after the stage is read back (as a quick hand would)
    t.send("c");
    t.wait_for("the message buffer, typing", |s| {
        s.contains("COMMIT_EDITMSG") && s.status().contains("INSERT")
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

/// N6: while Claude works the waiting animation draws 10 frames a second — in a terminal that never says
/// where its focus is, too. The terminal saying focus went elsewhere (`CSI O`, focus reporting being on —
/// CSI ?1004) rests it: nothing goes out after the last frame and the whole-screen redraw that follows
/// half a second later. Focus back (`CSI I`) and it goes on.
#[test]
fn the_waiting_animation_rests_while_the_terminal_is_unfocused() {
    let sb = Sandbox::new("waiting-focus");
    sb.write("a.txt", "some text\n");
    let mut t = sb.waiting_turn(&["a.txt"], COLS, ROWS, 60);
    let frames_in = |t: &Term, d: Duration| {
        let (before, start) = (t.frames(), std::time::Instant::now());
        std::thread::sleep(d);
        (t.frames() - before, start.elapsed())
    };
    // Frames a second over the time that actually passed — a runner stretches a sleep (CI's macOS: 5 ms → 25).
    // At most the 10 a second (what's guarded: never more); at least a few — it animates — not near 10: a
    // slow runner draws fewer of them, and a stopped animation draws none.
    let per_second = |(n, took): (usize, Duration)| (n as f64 / took.as_secs_f64()).round() as usize;
    let second = Duration::from_secs(1);
    let n = per_second(frames_in(&t, second));
    assert!((3..=12).contains(&n), "{n} frames a second, no focus event yet");
    t.send("\x1b[O");
    // The tick already set, then the redraw of the whole screen when nothing more came (`RESTING_REDRAW`,
    // half a second): waited for — a stretch with no frame longer than that — not a fixed time
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while frames_in(&t, Duration::from_millis(700)).0 > 0 && std::time::Instant::now() < end {}
    assert_eq!(frames_in(&t, Duration::from_millis(1500)).0, 0, "unfocused: nothing drawn");
    t.send("\x1b[I");
    let n = per_second(frames_in(&t, second));
    assert!((3..=13).contains(&n), "{n} frames a second after focus came back");
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
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("two");
    // (the line's blame lands late — git on a worker; a loaded machine drew it inside the measured window)
    t.wait_for("the line's blame", |s| s.contains("just now"));
    let idle = |t: &mut Term, what: &str| {
        // Settled: what lands late (the git bar, the blame, the whole-screen redraw that follows frames sent
        // in parts — `RESTING_REDRAW`) is drawn — a second with no frame; a loaded machine takes longer, so
        // that's waited for, not a fixed time. One that never goes quiet is caught below all the same.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let frames = t.frames();
            std::thread::sleep(std::time::Duration::from_secs(1));
            if t.frames() == frames || std::time::Instant::now() > end {
                break;
            }
        }
        let (frames, before, was) = (t.frames(), t.usage(), t.screen());
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (drawn, used) = (t.frames() - frames, t.usage() - before);
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
        assert!(
            drawn == 0 && used.switches < 30,
            "{what}: {drawn} frames, {used:?} in 2 s — changed:\n{}\n{now}",
            changed.join("\n")
        );
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
    // …and one watching: told only of changes, after frames drawn anyway — none while idle
    let watching = t.watch("");
    idle(&mut t, "a file, a watcher connected");
    assert_eq!(watching.events().len(), 1, "only its `watching` line: {:#?}", watching.events());
    drop(watching);
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
    t.wait_for("the terminal", |s| s.contains("TERMINAL") && (s.contains("$ ") || s.contains("# ")));
    idle(&mut t, "the terminal");
    t.send("\x1c");
    t.wait_state("the code has the keys", |s| s["keys"]["region"] == "code");
    t.send(" l");
    t.wait_text("Ask about");
    idle(&mut t, "the chat");
    drop(t);
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("two");
    t.send(" f");
    t.wait_for("the picker", |s| s.contains("files in work"));
    sb.write_config(&format!("{BASE_CONFIG}tab-width = \"wide\"\n"));
    t.wait_for("the error on the status line", |s| s.status().contains("config:"));
    idle(&mut t, "a picker over an error");
    // The picker gone, the card comes up — its time starts and its fade-in gets frames (first drawn
    // transparent, it would stay so without them)
    // (counted until the fade is over — frames stop for a while — not in a fixed window; and "it animated",
    // not how far: a fade lasts a fixed time, so a slow runner draws fewer frames of it — CI's macOS drew 2.
    // Without the fade's frames only the one that shows the card comes.)
    let frames = t.frames();
    t.esc("the card", |s| !s.contains("files in work") && s.contains("expected integer"));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let before = t.frames();
        std::thread::sleep(std::time::Duration::from_millis(400));
        if t.frames() == before || std::time::Instant::now() > end {
            break;
        }
    }
    assert!(t.frames() - frames >= 2, "{} frames for the card's fade-in", t.frames() - frames);
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
/// and its socket (`tarae state`): the code's keys; the terminal's program having every key, then tarae's
/// keys on its panel (the status line says `i type in it`); a card the chat's draft keeps from its `y`;
/// `--after-input` answering about the keys just sent. The terminal's shell finds this tarae (`TARAE_UI`).
#[test]
fn a_driver_reads_where_the_keys_go() {
    let sb = Sandbox::new("uistate");
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("one");
    let s = t.state_json(0);
    assert_eq!(s["v"], 2, "the schema's version (`uistate::V`)");
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
    // tarae's keys on its panel: `C-\` back to the code, `C-w j` down to it
    let input = t.state_json(0)["input"].as_u64().unwrap();
    t.send("\x1c\x17j");
    let s = t.state_json(input + 3);
    assert_eq!(
        (s["keys"]["region"].as_str(), s["keys"]["takes"].as_str()),
        (Some("slot:terminal"), Some("commands"))
    );
    t.wait_for("the status line says so", |s| {
        s.status().contains("TERMINAL") && s.status().contains("i type in it")
    });
    assert!(t.state(&[]).lines().nth(1).unwrap().starts_with("keys → terminal (commands)"));
    drop(t);
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
    let card = t.wait_state("the trust card settled", |s| {
        s["cards"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["kind"] == "project-trust" && c["settled"] == true)
    });
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
        s.rows.iter().any(|r| r.contains("● warm until ") && r.contains("ctx 2%  $0.041"))
    });
    let s = t.wait_state("the session's usage", |s| s["sessions"][0]["usage"]["cache"] == "warm");
    let u = &s["sessions"][0]["usage"];
    assert_eq!((u["ttl"].as_str(), u["context"]["tokens"].as_u64()), (Some("5m"), Some(24_006)), "{u}");
    assert_eq!(u["last_turn"]["cost"], 0.0412);
    t.send("\x1b");
    t.wait_state("the code has the keys", |s| s["keys"]["region"] == "code");
    t.command("usage");
    t.wait_for("the usage view", |s| {
        s.contains("# Claude usage")
            && s.contains("| total | 1 turn")
            && s.contains("Every session: $0.041 in 1 turn")
    });
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
    t.wait_for("src.rs at the hit", |s| s.rows[0].contains("›  src.rs") && s.status().contains(" 3:4 "));
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

/// `:restart`'s card (which checkout it builds in, #108) answered `y` once it's in front and has been in sight
/// a moment — the project's trust card, up since the start (its `.tarae.toml` sets the binary), set aside
/// with `n` first.
fn yes_to_restart(t: &mut Term) {
    // (a card's answer counts once it has been in sight as it is — the state says when: `settled`; it slept
    // 800 ms and hoped)
    let front = |s: &serde_json::Value, kind: &str| {
        s["cards"].as_array().into_iter().flatten().any(|c| c["up"] == "front" && c["kind"] == kind)
    };
    let settled = |s: &serde_json::Value| {
        s["cards"].as_array().into_iter().flatten().any(|c| c["up"] == "front" && c["settled"] == true)
    };
    let s = t.wait_state("a card settled in front", |s| {
        settled(s) && (front(s, "restart") || front(s, "project-trust"))
    });
    if front(&s, "project-trust") {
        t.send("n");
        t.wait_state("the restart card settled in front", |s| front(s, "restart") && settled(s));
    }
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
    t.send("\x17w");
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
    t.wait_for("INSERT mode", |s| s.status().contains("INSERT"));
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
    for _ in 0..20 {
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
    sb.write_config(&format!(
        "{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\nreview-at-end = true\n",
        script.display()
    ));
    let mut t = Term::spawn(&sb, &["main.txt"], 120, 40);
    t.send(" l");
    t.send("change it\r");
    // Live: each stop in order, each within a bound of the one before (they come 0.4 s apart)
    let removed = |s: &Screen, gone: &str| s.rows.iter().any(|r| r.contains('−') && r.contains(gone));
    type Seen = Box<dyn Fn(&Screen) -> bool>;
    let stops: [(&str, Seen); 7] = [
        ("the read", Box::new(|s| s.contains("● read  main.txt  L1–3"))),
        (
            "edit 1, its line and the one it replaced",
            Box::new(move |s| {
                s.contains("● edit  edit.txt  L3 ") && s.contains("three") && removed(s, "line 3")
            }),
        ),
        ("edit 2", Box::new(|s| s.contains("● edit  edit.txt  L11–12") && s.contains("added b"))),
        (
            "edit 3, a line gone",
            Box::new(move |s| s.contains("● edit  edit.txt  L22") && removed(s, "line 20")),
        ),
        ("edit 4", Box::new(|s| s.contains("● edit  edit.txt  L1 ") && s.contains("zero"))),
        (
            "the search, named, with its count",
            Box::new(|s| s.contains("● search (project)  line 5") && s.contains("1 matching line in 1 file")),
        ),
        ("the last read", Box::new(|s| s.contains("● read  lib.txt  L8–9"))),
    ];
    // (a stop skipped is never seen whatever the bound — it only has to outlast a slow runner's lag)
    for (what, seen) in &stops {
        if t.poll_for(Duration::from_secs(4), |s| seen(s)).is_none() {
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
    step(&mut t, "[r", "the newest stop", &|s| s.contains("● read  lib.txt  L8–9") && s.contains("7/7"));
    step(&mut t, "[r", "the search", &|s| s.contains("● search (project)") && s.contains("6/7"));
    step(&mut t, "[r", "edit 4", &|s| s.contains("● edit  edit.txt  L1 ") && s.contains("5/7"));
    step(&mut t, "[r[r[r", "edit 1, a line down since edit 4", &|s| {
        s.contains("● edit  edit.txt  L4") && s.contains("2/7") && removed(s, "line 3")
    });
    step(&mut t, "]r]r]r]r]r]r", "past the newest", &|s| s.contains("that was the newest stop"));
}
