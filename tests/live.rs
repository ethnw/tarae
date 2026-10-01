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
    let ok = found.is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipped: {tool} isn't on PATH");
    }
    ok
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
    t.wait_for("the session over", |s| !s.contains("F10 over"));
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
