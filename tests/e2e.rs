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

#[test]
fn claude_code_lock_file_stays_in_the_sandbox_and_is_removed_on_quit() {
    let sb = Sandbox::new("lock");
    sb.write("x.txt", "x\n");
    // Others' lock files in the sandbox's CLAUDE_CONFIG_DIR (its HOME is a temp one): a dead tarae's and
    // another IDE's
    let ide = sb.claude_ide();
    fs::create_dir_all(&ide).unwrap();
    let others =
        [("1.lock", r#"{"pid":999999,"ideName":"tarae"}"#), ("2.lock", r#"{"pid":1,"ideName":"VS Code"}"#)];
    for (name, body) in others {
        fs::write(ide.join(name), body).unwrap();
    }
    let mut t = sb.spawn(&["x.txt"]);
    t.wait_text("x");
    assert_eq!(sb.lock_files().len(), 2, "nothing of tarae's before space c, nothing of others' gone");
    // space c: the connection opens (claude isn't on the sandbox's PATH — it just doesn't start)
    t.send(" c");
    let ours =
        |sb: &Sandbox| sb.lock_files().into_iter().find(|p| !others.iter().any(|(n, _)| p.ends_with(n)));
    let deadline = std::time::Instant::now() + sb.timeout();
    while ours(&sb).is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let lock = ours(&sb).expect("our lock file after space c");
    let body = fs::read_to_string(&lock).unwrap();
    assert!(body.contains("\"ideName\":\"tarae\""), "{body}");
    assert!(body.contains(&format!("\"pid\":{}", t.child.id())), "{body}");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
    assert!(!lock.exists(), "ours removed on quit");
    for (name, body) in others {
        assert_eq!(fs::read_to_string(ide.join(name)).unwrap(), body, "{name} left as it was");
    }
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
    t.wait_for("the panel focused", |s| s.contains("esc code"));
    t.send("q");
    t.wait_for("the panel gone", |s| !s.contains("notes.md"));
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
    let inserting = t.poll_for(std::time::Duration::from_secs(2), |s| s.status().contains("INSERT"));
    let took = asked.elapsed();
    eprintln!("flood: keys landed after {took:?}");
    assert!(inserting.is_some(), "keys still not handled after {took:?} of a flood");
    t.normal_mode();
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
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
    t.wait_for("the task failed", |s| s.contains("FAILED") && s.contains("checking"));
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
        let (frames, before) = (t.frames(), t.usage());
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (drawn, used) = (t.frames() - frames, t.usage() - before);
        // (a wake is a few switches: 14 a second came to ~100 in 2 s; at rest it's under 10)
        assert!(drawn == 0 && used.switches < 30, "{what}: {drawn} frames, {used:?} in 2 s\n{}", t.screen());
    };
    idle(&mut t, "a file");
    t.send(" e");
    t.wait_for("the side panel", |s| s.contains("esc code"));
    idle(&mut t, "the side panel");
    t.esc("the code has the keys", |s| s.contains("space e files"));
    t.send(" g");
    t.wait_for("the git tab", |s| s.contains("Changes"));
    idle(&mut t, "the git tab");
    t.esc("the code has the keys", |s| s.contains("space e files"));
    t.send(" T");
    t.wait_for("the terminal", |s| s.contains("TERMINAL") && s.contains("$"));
    idle(&mut t, "the terminal");
    t.send("\x1c");
    t.wait_for("the code has the keys", |s| s.contains("C-\\ type in it"));
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
    let frames = t.frames();
    t.esc("the card", |s| !s.contains("files in work") && s.contains("expected integer"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(t.frames() - frames >= 3, "{} frames for the card's fade-in", t.frames() - frames);
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

/// `tarae <folder>`: tarae works there on the start screen — even with a session saved there — and the file
/// picker lists its files; `editor.restore-session` brings the session back. A quit from the start screen
/// keeps the session; closing every file clears it. A file given beside it is read from where the command ran.
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
    // leaves the session alone
    let mut t = sb.spawn(&["app"]);
    let s = t.wait_text("t a r a e");
    assert!(!s.contains("pub fn lib"), "{s}");
    assert!(s.contains("Continue where you left off") && s.contains("lib.rs"), "offered: {s}");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));

    // With the setting on it comes back; closing it before quitting clears the session
    sb.write_config(&format!("{BASE_CONFIG}restore-session = true\n"));
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
    t.wait_raw("a second tarae entering the alternate screen", |r| entered(r) >= 2);
    let s = t.wait_text("Restarted · 2 files as they were");
    assert!(t.running(), "the same process, still running");
    assert!(s.contains("bee two") && s.contains("line 6"), "both panes:\n{s}");
    assert!(s.status().contains("2:1"), "b focused, its cursor kept:\n{s}");
    t.send("\x17w");
    t.wait_for("the other pane: a at line 6", |s| s.status().contains("6:1"));
    // No Claude Code lock file: none before `space c`, none left behind by the restart
    let locks = sb.lock_files();
    assert!(locks.is_empty(), "{locks:?}");

    t.command("restart!");
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
    t.wait_text("Restarted · 1 file as it was");
    undo_reaches_back(&mut t);
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
}
