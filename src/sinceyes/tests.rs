//! What changed since a card's answer: the project's hunks outside the sandbox (never a file outside it), a
//! script's diff, a pack's entry files' and the build files', the rest named, "unchanged" — and the card that
//! says it first, still waiting for its key.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;

use super::*;

/// git in the test's own repository — no hooks, no signing, a name of its own.
fn git_in(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
        .args(["-c", "user.email=t@example.invalid", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repository in the test's temp folder: the project `proj/` in it, and `other.txt` beside it — outside
/// the project. (repository, project)
fn repo(name: &str) -> (PathBuf, PathBuf) {
    let dir = crate::scratch::Scratch::new(&format!("sinceyes-{name}")).into_path();
    std::fs::create_dir_all(dir.join("proj/src")).unwrap();
    std::fs::write(dir.join("proj/src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(dir.join("other.txt"), "outside-marker one\n").unwrap();
    git_in(&dir, &["init", "-q"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    (dir.clone(), dir.join("proj"))
}

fn yes(root: &Path, cmd: &str) -> Answered {
    Answered::new((41, 'y'), (true, None), cmd, root, SystemTime::now(), take(root, root, cmd))
}

/// Outside the sandbox, a command that runs the project's code: what changed since the yes is the project's
/// hunks — a line changed, a file new — and nothing of a file outside the project; nothing changed, said so
/// on the first line. git's own index is left as it was (the tree is staged into a copy). A file a card can't
/// show as it is: named only.
#[test]
fn the_projects_changes_since_the_yes_and_nothing_outside_it() {
    let (dir, root) = repo("tree");
    let cmd = "cargo test lib_a";
    // (an edit before the yes is part of what was said yes to)
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 1 }\n").unwrap();
    let index = std::fs::read(dir.join(".git/index")).unwrap();
    let y = yes(&root, cmd);
    assert!(matches!(y.kept, Kept::Tree(_)), "{:?}", y.kept);
    let s = y.since(&root, &root, cmd, true);
    assert!(s.changed == Changed::Nothing && s.unchanged, "{s:?}");
    let same = format!("Same as card#41 — y at {}", s.at);
    assert_eq!(s.head(), format!("{same}\n  build files unchanged\n  nothing else in the project changed"));
    assert!(s.below().0.is_empty() && s.at.len() == 5 && s.at.as_bytes()[2] == b':', "{}", s.at);
    // A line changed, a file new — and the file outside the project changed too
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    std::fs::write(root.join("src/new.rs"), "fn c() {}\n").unwrap();
    std::fs::write(dir.join("other.txt"), "outside-marker two\n").unwrap();
    let s = y.since(&root, &root, cmd, true);
    let Changed::Lines { of, lines } = &s.changed else { panic!("{s:?}") };
    assert_eq!(of, "the project");
    let has = |l: &str| lines.iter().any(|x| x == l);
    assert!(has("src/lib.rs") && has("-fn b() { 1 }") && has("+fn b() { 2 }"), "{lines:#?}");
    assert!(has("src/new.rs (new)") && has("+fn c() {}"), "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.contains("outside-marker")),
        "nothing from outside the project: {lines:#?}"
    );
    assert!(lines.iter().any(|l| l.starts_with("@@ ")), "its hunks' headers: {lines:#?}");
    assert!(
        s.head().ends_with("2 other project files changed since: src/lib.rs, src/new.rs") && !s.unchanged,
        "{}",
        s.head()
    );
    let (card, diff) = s.below();
    assert_eq!(card[0], "Changed since card#41, in the project:");
    assert_eq!(diff, 1..card.len(), "the diff's lines are drawn as one");
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index, "git's own index untouched");
    // One a card can't show as it is (a bidi mark): named, not shown
    std::fs::write(root.join("src/new.rs"), "fn c() {}\n// \u{202E}reversed\n").unwrap();
    let s = y.since(&root, &root, cmd, true);
    let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
    assert!(
        lines.iter().any(|l| l.starts_with("src/new.rs (new) has characters a card can't show")),
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l.contains('\u{202E}')), "{lines:#?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A script: its own diff since the yes — not the project's — and one outside the project is named as
/// changed, its content never shown. Not a git checkout: no matter, for a script. A command that isn't a
/// script outside a git checkout: it can't tell, and says so.
#[test]
fn a_scripts_own_diff_and_one_outside_the_project_named_only() {
    let scratch = crate::scratch::Scratch::new("sinceyes-script");
    let dir = scratch.to_path_buf();
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    let root = dir.join("proj");
    std::fs::write(root.join("check.sh"), "echo one\necho two\n").unwrap();
    std::fs::write(dir.join("outside.sh"), "echo outside-marker\n").unwrap();
    let cmd = "sh check.sh && sh ../outside.sh";
    let y = yes(&root, cmd);
    assert!(
        matches!(&y.kept, Kept::Scripts { scripts, project: None, .. } if scripts.len() == 2),
        "{:?}",
        y.kept
    );
    let s = y.since(&root, &root, cmd, true);
    assert!(s.changed == Changed::Nothing && s.unchanged, "{s:?}");
    let outside = dir.join("outside.sh").display().to_string();
    assert!(s.head().ends_with(&format!("\n  script unchanged (check.sh, {outside})")), "{}", s.head());
    std::fs::write(root.join("check.sh"), "echo one\necho three\n").unwrap();
    std::fs::write(dir.join("outside.sh"), "echo outside-marker changed\n").unwrap();
    // (in the sandbox or outside it: a script's own changes are shown)
    for whole in [true, false] {
        let s = y.since(&root, &root, cmd, whole);
        let Changed::Lines { of, lines } = &s.changed else { panic!("{s:?}") };
        assert!(of.starts_with("check.sh, "), "{of}");
        assert_eq!(lines[..4], ["check.sh", "@@ line 2", "-echo two", "+echo three"], "{lines:#?}");
        assert!(
            lines[4].ends_with("outside.sh — changed; outside the project, not shown here"),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("outside-marker")), "{lines:#?}");
        let said = format!("\n  script changed: check.sh, {outside}");
        assert!(s.head().ends_with(&said) && !s.unchanged, "{}", s.head());
    }
    // Not a script, not a git checkout: it can't tell
    let y = yes(&root, "make check");
    let s = y.since(&root, &root, "make check", true);
    assert!(matches!(&s.changed, Changed::Unknown(w) if w.contains("not a git checkout")), "{s:?}");
    assert!(s.head().contains("read it as new") && !s.unchanged, "{}", s.head());
    std::fs::remove_dir_all(&dir).ok();
}

/// The same run is what runs (#158): the program and its arguments before the first pipe — another filter
/// after it is the same run, another argument before it isn't — and a script by its file, whatever its
/// arguments. A line the shell reader can't read is its whole self.
#[test]
fn the_same_run_is_what_runs_not_the_filter_after_it() {
    let same = |a: &str, b: &str| key(a) == key(b);
    assert!(same("cargo test lib_a 2>&1 | grep FAIL", "cargo test lib_a | tail -5"));
    assert!(same("cargo test lib_a", "cargo test lib_a | head"));
    assert!(!same("cargo test lib_a | tail", "cargo test lib_b | tail"), "another argument: another run");
    assert!(!same("cargo test lib_a", "cd sub && cargo test lib_a"), "from another folder");
    assert!(!same("A=1 cargo test", "cargo test"), "what it sets is part of what runs");
    assert!(!same("cargo test 'a b'", "cargo test a b"), "words stay words");
    // A script: by its file, whatever its arguments — not by another interpreter, nor another file
    assert!(same("sh target/x/setup.sh one", "sh target/x/setup.sh two 2>&1 | tail"));
    assert!(same("./run.sh --fast", "./run.sh"));
    assert!(!same("sh setup.sh one", "sh other.sh one"));
    assert!(!same("sh setup.sh", "bash setup.sh"));
    assert!(!same("sh setup.sh && cargo test", "sh setup.sh"), "a script and more: by its words");
    // Unreadable: the whole line
    assert!(same("cargo test $X | tail", "cargo test $X | tail"));
    assert!(!same("cargo test $X | tail", "cargo test $X | head"));
}

/// frictions29 14: what a script reads by name is compared with it — a file its text names (`fetch.sh` curls
/// what `urls.txt` lists), one handed to it (`vm.sh run.sh`), ignored ones under `target/sb` too. Unchanged:
/// said so, by name; changed: its hunks shown with the script's, the card no longer "unchanged" and warned of as
/// a changed script is. A word that's no file, or a file outside the project, isn't kept.
#[test]
fn the_files_a_script_names_are_compared_with_it() {
    let (dir, root) = repo("inputs");
    std::fs::create_dir_all(root.join("target/sb")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let sb = root.join("target/sb");
    std::fs::write(
        sb.join("fetch.sh"),
        "while read u; do curl -s \"$u\"; done < urls.txt\ncat ../../../other.txt\n",
    )
    .unwrap();
    std::fs::write(sb.join("urls.txt"), "https://a.example.invalid\n").unwrap();
    std::fs::write(sb.join("vm.sh"), "scp \"$1\" vm: && ssh vm sh \"$1\"\n").unwrap();
    std::fs::write(sb.join("run.sh"), "echo one\n").unwrap();
    for (cmd, file, name) in [
        ("sh target/sb/fetch.sh", "urls.txt", "target/sb/urls.txt"),
        ("bash target/sb/vm.sh target/sb/run.sh --quick", "run.sh", "target/sb/run.sh"),
    ] {
        let y = yes(&root, cmd);
        let Kept::Scripts { inputs, .. } = &y.kept else { panic!("{:?}", y.kept) };
        let kept: Vec<String> = inputs.iter().map(|(p, _)| crate::chat::shown(&root, p)).collect();
        assert_eq!(kept, [name], "{cmd}: no word that's no file, nothing outside the project");
        let s = y.since(&root, &root, cmd, false);
        assert!(s.unchanged && s.changed == Changed::Nothing, "{s:?}");
        assert!(s.head().contains(&format!("\n  files it names unchanged ({name})")), "{}", s.head());
        let was = std::fs::read_to_string(sb.join(file)).unwrap();
        std::fs::write(sb.join(file), format!("{was}more-marker\n")).unwrap();
        let s = y.since(&root, &root, cmd, false);
        assert!(!s.unchanged && s.script_changed, "{s:?}");
        assert!(s.head().contains(&format!("\n  files it names changed: {name}")), "{}", s.head());
        let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
        assert!(lines.iter().any(|l| l == "+more-marker"), "{lines:#?}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions29 31: a program that isn't text, under an ignored `target/`, rebuilt between the yes and the ask — kept
/// by its digest, so "script changed" (its sizes, never its bytes), never "unchanged"; the same bytes: unchanged. One a
/// script runs by a relative path, past what a named file is kept to, rebuilt: "files it names changed". One that
/// can't be read then nor now: can't tell, and why.
#[test]
fn a_program_git_ignores_is_compared_by_its_bytes() {
    let (dir, root) = repo("program");
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let sb = root.join("target/sb");
    std::fs::create_dir_all(&sb).unwrap();
    let build = |name: &str, len: usize, n: &str| {
        let mut b = b"\x7fELF\xff\x00".to_vec();
        b.resize(len.max(6), 0xff);
        b.extend(n.bytes());
        std::fs::write(sb.join(name), b).unwrap();
    };
    build("probe", 0, "one");
    let cmd = "./target/sb/probe";
    let y = yes(&root, cmd);
    let s = y.since(&root, &root, cmd, false);
    assert!(s.unchanged && !s.script_changed, "{s:?}");
    build("probe", 0, "two, rebuilt");
    for outside in [false, true] {
        let s = y.since(&root, &root, cmd, outside);
        assert!(!s.unchanged && s.script_changed, "{s:?}");
        assert!(s.head().contains("\n  script changed: target/sb/probe"), "{}", s.head());
        let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
        let said = "target/sb/probe — not text: 9 → 18 bytes";
        assert!(lines.iter().any(|l| l == said), "{lines:#?}");
    }
    // A program a script runs, bigger than a named file is kept to: rebuilt, the same size
    std::fs::write(sb.join("run.sh"), "set -e\n./probe-big --drain\n").unwrap();
    build("probe-big", 300 << 10, "a");
    let cmd = "sh target/sb/run.sh";
    let y = yes(&root, cmd);
    let Kept::Scripts { inputs, .. } = &y.kept else { panic!("{:?}", y.kept) };
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert!(y.since(&root, &root, cmd, false).unchanged);
    build("probe-big", 300 << 10, "b");
    let s = y.since(&root, &root, cmd, false);
    assert!(!s.unchanged && s.script_changed, "{s:?}");
    assert!(s.head().contains("\n  files it names changed: target/sb/probe-big"), "{}", s.head());
    // Not there then nor now: can't tell, never unchanged
    let cmd = "sh target/sb/gone.sh";
    let s = yes(&root, cmd).since(&root, &root, cmd, false);
    let why = "can't tell whether target/sb/gone.sh changed — git ignores it, and it couldn't be read";
    assert_eq!(s.changed, Changed::Unknown(why.into()), "{s:?}");
    assert!(!s.unchanged && s.head().contains("; read it as new."), "{}", s.head());
    std::fs::remove_dir_all(&dir).ok();
}

/// The reviewer's review of frictions32: a script with a byte that isn't UTF-8 (a Latin-1 comment) is still text —
/// its diff shown with that byte replaced, the files it names followed; a change in that byte alone is said, never
/// "unchanged". A file past the digest's limit is kept by its stamp: left alone, unchanged; written again with the
/// same bytes, changed (as the docs say).
#[test]
fn a_script_not_utf8_throughout_is_shown_and_a_big_one_kept_by_its_stamp() {
    let (dir, root) = repo("latin1");
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let sb = root.join("target/sb");
    std::fs::create_dir_all(&sb).unwrap();
    std::fs::write(sb.join("hosts.txt"), "a\n").unwrap();
    let script = |echo: &str, byte: u8| {
        let mut b = b"# caf".to_vec();
        b.push(byte);
        b.extend(format!("\ncat hosts.txt\necho {echo}\n").bytes());
        std::fs::write(sb.join("go.sh"), b).unwrap();
    };
    script("one", 0xe9);
    let cmd = "sh target/sb/go.sh";
    let y = yes(&root, cmd);
    let Kept::Scripts { inputs, .. } = &y.kept else { panic!("{:?}", y.kept) };
    assert_eq!(inputs.len(), 1, "the file it names, followed: {inputs:?}");
    assert!(y.since(&root, &root, cmd, false).unchanged);
    script("two", 0xe9);
    let s = y.since(&root, &root, cmd, false);
    let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
    assert!(lines.iter().any(|l| l == "-echo one") && lines.iter().any(|l| l == "+echo two"), "{lines:#?}");
    assert!(!lines.iter().any(|l| l.contains('\0')), "{lines:#?}");
    // Only the byte that isn't UTF-8 changed
    script("one", 0xe8);
    let s = y.since(&root, &root, cmd, false);
    assert!(s.script_changed && !s.unchanged, "{s:?}");
    let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
    let said = "target/sb/go.sh — changed only in bytes that aren't UTF-8 — not shown";
    assert!(lines.iter().any(|l| l == said), "{lines:#?}");
    // Past the digest's limit: by its stamp
    HASH_LIMIT.with(|m| m.set(16));
    let y = yes(&root, cmd);
    assert!(y.since(&root, &root, cmd, false).unchanged, "left alone");
    let bytes = std::fs::read(sb.join("go.sh")).unwrap();
    std::fs::write(sb.join("go.sh"), &bytes).unwrap();
    let s = y.since(&root, &root, cmd, false);
    HASH_LIMIT.with(|m| m.set(HASH_MAX));
    assert!(s.script_changed && !s.unchanged, "the same bytes written again: by its stamp, changed — {s:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions30's review (6, 8, 9): of what a script names, what it writes (`> out.log`, `tee`) isn't compared — a
/// run changes it itself; a file made since the yes isn't either (the run's own, or named by a script whose own diff
/// shows it); one gone since is said; one named like a key or holding a token is compared by its hash, its change
/// said and never shown.
#[test]
fn what_a_script_writes_made_or_keeps_secret_is_said_so() {
    let (dir, root) = repo("inputs-kinds");
    let sb = root.join("target/sb");
    std::fs::create_dir_all(&sb).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let script =
        "cat hosts.txt later.txt\nset -a; . ./run.env; set +a\necho ok > out.log\necho 2 | tee -a log.txt\n";
    std::fs::write(sb.join("go.sh"), script).unwrap();
    for (f, text) in [
        ("hosts.txt", "a\n"),
        ("out.log", "old\n"),
        ("log.txt", "1\n"),
        ("run.env", "API_TOKEN=abc123secret\n"),
    ] {
        std::fs::write(sb.join(f), text).unwrap();
    }
    let cmd = "sh target/sb/go.sh";
    let y = yes(&root, cmd);
    let Kept::Scripts { inputs, .. } = &y.kept else { panic!("{:?}", y.kept) };
    let kept: Vec<String> = inputs.iter().map(|(p, _)| crate::chat::shown(&root, p)).collect();
    assert_eq!(kept, ["target/sb/hosts.txt", "target/sb/run.env"], "what it writes isn't what it reads");
    let env = inputs.iter().find(|(p, _)| p.ends_with("run.env")).unwrap();
    assert!(env.1.as_deref().is_some_and(|t| !t.contains("secret")), "its hash, not its lines");
    // The run writes its own files, and makes one it names: nothing changed
    std::fs::write(sb.join("out.log"), "ok\n").unwrap();
    std::fs::write(sb.join("log.txt"), "1\n2\n").unwrap();
    std::fs::write(sb.join("later.txt"), "made since\n").unwrap();
    let s = y.since(&root, &root, cmd, false);
    assert!(s.unchanged && !s.script_changed, "{s:?}");
    // The settings file changed: said, never shown
    std::fs::write(sb.join("run.env"), "API_TOKEN=xyz789secret\n").unwrap();
    let s = y.since(&root, &root, cmd, false);
    let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with("run.env — changed; named like a key, or it holds a token: not shown"))
    );
    assert!(!lines.iter().any(|l| l.contains("secret")), "{lines:#?}");
    // A file it read is gone
    std::fs::remove_file(sb.join("hosts.txt")).unwrap();
    let s = y.since(&root, &root, cmd, false);
    let Changed::Lines { lines, .. } = &s.changed else { panic!("{s:?}") };
    assert!(lines.iter().any(|l| l == "target/sb/hosts.txt — gone since"), "{lines:#?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A script that runs the project's code isn't all that runs: since the yes, the project's changes come
/// with the script's (the script itself left out of them) — "unchanged" only when neither changed. One that
/// runs nothing of the project: its own diff only, as before.
#[test]
fn a_script_that_runs_the_projects_code_shows_the_projects_changes_too() {
    let (dir, root) = repo("runs");
    std::fs::write(root.join("check.sh"), "set -e\nRUST_LOG=1 cargo test lib_a -- --nocapture\n").unwrap();
    std::fs::write(root.join("say.sh"), "# cargo test would run the project\necho hello\n").unwrap();
    let cmd = "sh check.sh one";
    let y = yes(&root, cmd);
    assert!(matches!(&y.kept, Kept::Scripts { project: Some(Ok(_)), .. }), "{:?}", y.kept);
    // Another argument, the same script: nothing changed
    let s = y.since(&root, &root, "sh check.sh two", true);
    assert_eq!(s.changed, Changed::Nothing);
    assert_eq!(s.was.as_deref(), Some(cmd));
    assert!(s.head().starts_with("Same as card#41 (`sh check.sh one`) — y at "), "{}", s.head());
    // The project changed, the script didn't: outside the sandbox, the project's diff; in it, named
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    let s = y.since(&root, &root, cmd, false);
    assert!(s.changed == Changed::Nothing && !s.unchanged, "{s:?}");
    assert!(
        s.head().ends_with(
            "\n  script unchanged (check.sh)\n  build files unchanged\n  1 other project file changed since: src/lib.rs"
        ),
        "{}",
        s.head()
    );
    let s = y.since(&root, &root, cmd, true);
    let Changed::Lines { of, lines } = &s.changed else { panic!("{s:?}") };
    assert_eq!(of, "the project check.sh runs (check.sh itself unchanged)");
    assert!(
        lines.iter().any(|l| l == "+fn b() { 2 }") && lines.iter().any(|l| l == "src/lib.rs"),
        "{lines:#?}"
    );
    // Both: the script's hunks, then the project's — the script not twice
    std::fs::write(root.join("check.sh"), "set -e\ncargo test lib_b\n").unwrap();
    let s = y.since(&root, &root, cmd, true);
    let Changed::Lines { of, lines } = &s.changed else { panic!("{s:?}") };
    assert_eq!(of, "check.sh, and the project it runs");
    assert_eq!(lines[0], "check.sh");
    assert_eq!(lines.iter().filter(|l| l.starts_with("check.sh")).count(), 1, "{lines:#?}");
    assert!(lines.iter().any(|l| l == "+cargo test lib_b") && lines.iter().any(|l| l == "+fn b() { 2 }"));
    // A script that runs nothing of the project: the project isn't kept, its changes aren't said
    let y = yes(&root, "sh say.sh");
    assert!(matches!(&y.kept, Kept::Scripts { project: None, .. }), "{:?}", y.kept);
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 3 }\n").unwrap();
    assert_eq!(y.since(&root, &root, "sh say.sh", true).changed, Changed::Nothing);
    // What reads as running the project's code, line by line
    for l in [
        "cargo test",
        "  make check",
        "A=1 B=2 go test ./...",
        "./target/debug/tarae --version",
        "x && npm run t",
    ] {
        assert!(runs_project_line(l), "{l}");
    }
    for l in ["echo cargo", "# cargo test", "grep -r x src/", "cat README.md", "wc -l notes.txt"] {
        assert!(!runs_project_line(l), "{l}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The card: Claude asks again to run, outside the sandbox, a command the user said yes to — its first line
/// says which card and when (`Same as card#N — y at …`), and under the command what changed since in the
/// project (the diff decided, in its colours' range); nothing since: said on that line. It still waits for
/// its key — never answered for the user — weighty as before, its warning (what yes means outside the
/// sandbox) still in the warning colour. Another command: a first card, as ever.
#[test]
fn the_card_says_what_changed_since_your_y() {
    let (dir, root) = repo("card");
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = root.clone();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let generation = c.generation();
    let pump = |ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str| {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(ed) {
            assert!(Instant::now() < end, "{what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    };
    let answered = |n: u64| {
        crate::tools::ANSWERED
            .with(|a| a.borrow().iter().find(|(id, _)| *id == json!(n)).map(|(_, r)| r.clone()))
    };
    // (a harmless command: it reads nothing, writes nothing)
    let ask = |ed: &mut crate::editor::Editor, n: u64, cmd: &str| {
        let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": "run",
            "arguments": {"command": cmd, "outside_sandbox": true, "reason": "it needs the network"}}});
        assert!(crate::tools::mcp(ed, generation, &format!("r{n}"), &msg).is_none(), "it waits for the user");
        pump(ed, &|ed| !ed.offers.is_empty(), "its card");
        ed.offers[0].question.clone()
    };
    let yes = |ed: &mut crate::editor::Editor, n: u64| {
        ed.offer_accept();
        pump(ed, &|_| answered(n).is_some(), "it ran");
        assert!(answered(n).is_some_and(|r| r.is_ok()), "{:?}", answered(n));
    };
    let warned = |ed: &crate::editor::Editor| {
        let o = &ed.offers[0];
        let lines: Vec<&str> = o.question.split('\n').collect();
        let w =
            lines.iter().position(|l| l.starts_with("Outside the sandbox it runs with all your")).unwrap();
        (o.weighty(), o.warning.as_ref().is_some_and(|r| r.contains(&w)))
    };
    // The first time: no word of an earlier card
    let q = ask(&mut ed, 1, "true");
    assert!(!q.contains("Same as card#"), "{q}");
    let first = (ed.offers[0].id, warned(&ed));
    yes(&mut ed, 1);
    // A change in the project, then the same again: which card, when, and the change
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    let q = ask(&mut ed, 2, "true");
    let same = format!("Same as card#{} — y at ", first.0);
    assert!(q.starts_with(&same), "the first line: {q}");
    assert!(q.contains("Changed since card#") && q.contains(", in the project:"), "{q}");
    assert!(q.contains("\n-fn b() {}\n+fn b() { 2 }"), "{q}");
    assert_eq!(warned(&ed), first.1, "as weighty, its warning still in the warning colour");
    let o = &ed.offers[0];
    let lines: Vec<&str> = o.question.split('\n').collect();
    let plus = lines.iter().position(|l| *l == "+fn b() { 2 }").unwrap();
    assert!(o.diff.as_ref().is_some_and(|d| d.contains(&plus)), "the diff's colours: {:?}", o.diff);
    assert!(o.decided_lines().contains(&plus) && o.decided_lines().contains(&0), "what's decided");
    let second = o.id;
    // (drawn: its rows on the card)
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 160, 50).unwrap();
    let mut vt = vt100::Parser::new(50, 160, 0);
    vt.process(&bytes);
    let s = vt.screen().contents();
    assert!(s.contains("+fn b() { 2 }") && s.contains("Same as card#"), "{s}");
    yes(&mut ed, 2);
    // Nothing since that yes: said — and the card still waits for its answer
    let q = ask(&mut ed, 3, "true");
    assert!(q.starts_with(&format!("Same as card#{second} — y at ")), "{q}");
    assert!(q.split("\n\n").next().unwrap().ends_with("\n  nothing else in the project changed"), "{q}");
    assert!(answered(3).is_none() && !ed.offers.is_empty(), "never answered for the user");
    assert_eq!(warned(&ed), first.1);
    let third = ed.offers[0].id;
    ed.offer_dismiss();
    pump(&mut ed, &|_| answered(3).is_some(), "told no");
    // (a no is kept too, once what it would have run is taken — on a worker)
    pump(&mut ed, &|ed| ed.session.answered.iter().any(|a| a.card == third), "the no kept");
    // Another command: a first card
    let q = ask(&mut ed, 4, "true && true");
    assert!(!q.contains("Same as card#"), "{q}");
    ed.offer_dismiss();
    pump(&mut ed, &|_| answered(4).is_some(), "told no");
    // The same run through another filter (#158): the last answer counts — a no — and the card names the line
    let q = ask(&mut ed, 5, "true | head -1");
    assert!(q.starts_with(&format!("Same as card#{third} (`true`) — n at ")), "{q}");
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}

/// cardburden 3, in the sandbox (step 6c: `cargo xtask linux clippy` carded 7 times, each read again by hand
/// for "has anything it runs changed?"): the same run asked again — through another filter too — says which
/// card and its answer first, then part by part what it runs: the pack's entry files (`xtask/`), the build
/// files, the rest of the project named, not shown; what it runs that changed is shown under the command. The
/// `card` line says `same=#N` or `same=#N:changed`, `tarae state` the card's `same_as`. Each card still waits
/// for its key. (Every answer here is `n`: nothing runs.)
#[test]
fn the_same_run_in_the_sandbox_says_what_changed_in_what_it_runs() {
    let (dir, root) = repo("insandbox");
    std::fs::create_dir_all(root.join("xtask/src")).unwrap();
    std::fs::write(root.join("xtask/src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    // (today's gate, for the cards: in the sandbox they run — P3; piece 5 keeps `sinceyes` for runs past it)
    ed.config.llm.sandbox = false;
    ed.session.root = root.clone();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let generation = c.generation();
    let pump = |ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str| {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(ed) {
            assert!(Instant::now() < end, "{what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    };
    let answered = |n: u64| crate::tools::ANSWERED.with(|a| a.borrow().iter().any(|(id, _)| *id == json!(n)));
    let ask = |ed: &mut crate::editor::Editor, n: u64, cmd: &str| {
        let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": "run",
            "arguments": {"command": cmd}}});
        assert!(crate::tools::mcp(ed, generation, &format!("r{n}"), &msg).is_none(), "it waits for the user");
        pump(ed, &|ed| !ed.offers.is_empty(), "its card");
        (ed.offers[0].id, ed.offers[0].question.clone())
    };
    // `n`, and the answer kept (what it would have run, taken on a worker)
    let no = |ed: &mut crate::editor::Editor, n: u64, card: u64| {
        assert!(!answered(n) && !ed.offers.is_empty(), "never answered for the user");
        ed.offer_dismiss();
        pump(ed, &|_| answered(n), "told no");
        pump(ed, &|ed| ed.session.answered.iter().any(|a| a.card == card), "the no kept");
    };
    // (card `card`'s own `card` line, by its id)
    let logged = |card: u64| {
        let lines = crate::cards::logged_lines("card ");
        let mine: Vec<String> =
            lines.into_iter().filter(|l| l.contains(&format!(" · card#{card} · "))).collect();
        assert_eq!(mine.len(), 1, "card#{card}'s one line: {mine:?}");
        mine[0].clone()
    };
    let cmd = "cargo xtask linux clippy";
    let (first, q) = ask(&mut ed, 1, cmd);
    assert!(!q.contains("Same as card#"), "a first card: {q}");
    no(&mut ed, 1, first);
    assert!(!logged(first).contains("same="), "a first card: {}", logged(first));
    // Only the project's own code changed, through another filter: named, nothing shown
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    // (the card's top: up to the blank line before the command)
    let top = |q: &str| q.split("\n\n").next().unwrap().to_string();
    let (second, q) = ask(&mut ed, 2, "cargo xtask linux clippy 2>&1 | tail -3");
    let head = top(&q);
    assert!(head.starts_with(&format!("Same as card#{first} (`{cmd}`) — n at ")), "{q}");
    assert!(
        head.ends_with(
            "\n  xtask/ unchanged\n  build files unchanged\n  1 other project file changed since: src/lib.rs"
        ),
        "{head}"
    );
    assert!(!q.contains("Changed since") && ed.offers[0].diff.is_none(), "nothing it runs changed: {q}");
    let same = ed.ui_cards()[0].same_as.clone().unwrap();
    assert_eq!((same.card, same.answer, same.unchanged), (first, 'n', false));
    no(&mut ed, 2, second);
    assert!(logged(second).contains(&format!(" · same=#{first}:changed · ")), "{}", logged(second));
    // What it runs changed — the xtask and a build file: on the first line, and shown under the command
    std::fs::write(root.join("xtask/src/main.rs"), "fn main() { linux() }\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"xtask\"]\n").unwrap();
    let (third, q) = ask(&mut ed, 3, cmd);
    let head = top(&q);
    assert!(head.starts_with(&format!("Same as card#{second} (`cargo xtask linux clippy 2>&1 | tail -3`)")));
    assert!(
        head.ends_with(
            "\n  xtask/ changed: xtask/src/main.rs\n  build files changed: Cargo.toml\n  nothing \
             else in the project changed"
        ),
        "{head}"
    );
    assert!(q.contains(&format!("\nChanged since card#{second}, in the project:\n")), "{q}");
    assert!(q.contains("\n+fn main() { linux() }") && q.contains("\n+members = [\"xtask\"]"), "{q}");
    assert!(!q.contains("fn b() { 2 }"), "the rest isn't shown: {q}");
    let o = &ed.offers[0];
    let plus = o.question.split('\n').position(|l| l == "+fn main() { linux() }").unwrap();
    assert!(o.diff.as_ref().is_some_and(|d| d.contains(&plus)) && o.decided_lines().contains(&plus));
    no(&mut ed, 3, third);
    // Nothing since: said, and the card still waits for its key
    let (fourth, q) = ask(&mut ed, 4, cmd);
    let head = top(&q);
    assert!(head.starts_with(&format!("Same as card#{third} — n at ")), "{head}");
    assert!(
        head.ends_with(
            "\n  xtask/ unchanged\n  build files unchanged\n  nothing else in the project changed"
        )
    );
    assert!(ed.ui_cards()[0].same_as.as_ref().is_some_and(|s| s.unchanged));
    no(&mut ed, 4, fourth);
    assert!(logged(fourth).contains(&format!(" · same=#{third} · ")), "{}", logged(fourth));
    // The same line asked outside the sandbox isn't the same card: an answer in it isn't one outside it
    let msg = json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "run",
        "arguments": {"command": cmd, "outside_sandbox": true, "reason": "it needs the network"}}});
    assert!(crate::tools::mcp(&mut ed, generation, "r5", &msg).is_none());
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "its card");
    assert!(!ed.offers[0].question.contains("Same as card#"), "{}", ed.offers[0].question);
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}

/// cardburden 3: a `y`'s run takes its place in line at once — what it runs is kept from the run's own
/// snapshot, right before it starts (`tools::go`), not a second one first — so what the run itself made is a
/// change since that answer. (The run: `touch` in the test's own temp folder.)
#[test]
fn what_a_run_made_is_a_change_since_its_answer() {
    let (dir, root) = repo("ranmade");
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    // (today's gate, for its card: in the sandbox it runs — P3)
    ed.config.llm.sandbox = false;
    ed.session.root = root.clone();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let generation = c.generation();
    let pump = |ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str| {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(ed) {
            assert!(Instant::now() < end, "{what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    };
    let answered = |n: u64| crate::tools::ANSWERED.with(|a| a.borrow().iter().any(|(id, _)| *id == json!(n)));
    let cmd = "touch made.txt && echo made";
    let ask = |ed: &mut crate::editor::Editor, n: u64| {
        let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": "run",
            "arguments": {"command": cmd}}});
        assert!(crate::tools::mcp(ed, generation, &format!("r{n}"), &msg).is_none(), "it waits for the user");
        pump(ed, &|ed| !ed.offers.is_empty(), "its card");
        (ed.offers[0].id, ed.offers[0].question.clone())
    };
    let (first, q) = ask(&mut ed, 1);
    assert!(!q.contains("Same as card#"), "{q}");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(1), "it ran");
    assert!(root.join("made.txt").exists());
    let (_, q) = ask(&mut ed, 2);
    let top = q.split("\n\n").next().unwrap();
    assert!(top.starts_with(&format!("Same as card#{first} — y at ")), "{q}");
    assert!(top.ends_with("\n  1 other project file changed since: made.txt"), "what the run made: {top}");
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}

/// An editor in `root` with Claude's chat mid-turn (a stand-in claude that reads and says nothing) — and its
/// chat's generation, what a call is Claude's by.
fn chat_in(root: &Path) -> (crate::editor::Editor, u64) {
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    // (today's gate, where a run card comes for each command here: in the sandbox they run — P3)
    ed.config.llm.sandbox = false;
    ed.session.root = root.to_path_buf();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let generation = c.generation();
    (ed, generation)
}

/// Events handled until `done` (at most 20 s — then `what` didn't happen).
fn pump_until(ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done(ed) {
        assert!(Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// Claude's `run` call `n` with `args`, waiting for its card: the card's id and text.
fn ask_run(
    ed: &mut crate::editor::Editor,
    generation: u64,
    n: u64,
    args: serde_json::Value,
) -> (u64, String) {
    let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": "run", "arguments": args}});
    assert!(crate::tools::mcp(ed, generation, &format!("r{n}"), &msg).is_none(), "it waits for the user");
    pump_until(ed, &|ed| !ed.offers.is_empty(), "its card");
    (ed.offers[0].id, ed.offers[0].question.clone())
}

/// `n` on the front card for call `n`, and its answer kept (what it would have run, taken on a worker).
fn no_to(ed: &mut crate::editor::Editor, n: u64, card: u64) {
    ed.offer_dismiss();
    pump_until(
        ed,
        &|_| crate::tools::ANSWERED.with(|a| a.borrow().iter().any(|(id, _)| *id == json!(n))),
        "told no",
    );
    pump_until(ed, &|ed| ed.session.answered.iter().any(|a| a.card == card), "the no kept");
}

/// cardburden 3 (the reviewer, item 3): a card with a socket — Docker's is root's — is compared as one
/// outside the sandbox: its "same as" shows the project's whole diff since the answer, not only names; and
/// only a card asking the same socket is the same. (Every answer `n`: nothing runs, nothing connects.)
#[test]
fn a_socket_cards_same_as_shows_the_whole_diff() {
    let (dir, root) = repo("socket");
    let sockets = crate::scratch::Scratch::new("qs");
    std::fs::create_dir_all(sockets.join("c")).unwrap();
    let (docker, other) = (sockets.join("c/docker.sock"), sockets.join("c/other.sock"));
    let _listening = [&docker, &other].map(|s| std::os::unix::net::UnixListener::bind(s).unwrap());
    let (mut ed, generation) = chat_in(&root);
    // (not a check — it asks; never run)
    let with =
        |s: &Path| json!({ "command": "make demo", "unix_socket": s.to_str().unwrap(), "reason": "docker" });
    let (first, q) = ask_run(&mut ed, generation, 1, with(&docker));
    assert!(!q.contains("Same as card#"), "{q}");
    no_to(&mut ed, 1, first);
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    let (second, q) = ask_run(&mut ed, generation, 2, with(&docker));
    assert!(q.starts_with(&format!("Same as card#{first} — n at ")), "{q}");
    assert!(q.split("\n\n").next().unwrap().ends_with("\n  1 other project file changed since: src/lib.rs"));
    assert!(
        q.contains(&format!("\nChanged since card#{first}, in the project:\nsrc/lib.rs\n")),
        "shown: {q}"
    );
    assert!(q.contains("\n-fn b() {}\n+fn b() { 2 }"), "{q}");
    no_to(&mut ed, 2, second);
    // Another socket: not the same card
    let (_, q) = ask_run(&mut ed, generation, 3, with(&other));
    assert!(!q.contains("Same as card#"), "{q}");
    ed.offer_dismiss();
    std::fs::remove_dir_all(&sockets).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// A script that runs the project's code (`cargo test` in it), in the sandbox: its own diff and the build
/// files' hunks are shown together, under one header; the rest of the project only named.
#[test]
fn a_changed_script_and_build_file_are_shown_together() {
    let (dir, root) = repo("merged");
    std::fs::write(root.join("check.sh"), "set -e\ncargo test lib_a\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    let cmd = "sh check.sh";
    let y = yes(&root, cmd);
    std::fs::write(root.join("check.sh"), "set -e\ncargo test lib_b\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"x\"]\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    let s = y.since(&root, &root, cmd, false);
    let Changed::Lines { of, lines } = &s.changed else { panic!("{s:?}") };
    assert_eq!(of, "check.sh, and the project");
    let has = |l: &str| lines.iter().any(|x| x == l);
    assert!(has("+cargo test lib_b") && has("Cargo.toml") && has("+members = [\"x\"]"), "{lines:#?}");
    assert!(!has("+fn b() { 2 }"), "the rest isn't shown: {lines:#?}");
    assert!(
        s.head().ends_with(
            "\n  script changed: check.sh\n  build files changed: Cargo.toml\n  1 other project file changed since: \
             src/lib.rs"
        ),
        "{}",
        s.head()
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `with_runs`, case by case: what can't be told is never "nothing changed" — a script that can't be told,
/// or a project that can't (git failed), reads as new, whatever the other says; nothing of what it runs in
/// the project changed: the script's own; both: one after the other.
#[test]
fn with_runs_never_says_nothing_of_what_it_cant_tell() {
    let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let script = Changed::Lines { of: "x.sh".into(), lines: lines(&["x.sh", "+a"]) };
    let project = lines(&["Cargo.toml", "+b"]);
    let unknown = Changed::Unknown("it runs y.sh now, not what it ran then".into());
    assert_eq!(with_runs(Changed::Nothing, None), Changed::Nothing);
    assert_eq!(with_runs(script.clone(), None), script);
    assert_eq!(with_runs(unknown.clone(), Some(Ok(project.clone()))), unknown, "the script can't be told");
    assert_eq!(with_runs(unknown.clone(), Some(Ok(Vec::new()))), unknown);
    for own in [Changed::Nothing, script.clone()] {
        let c = with_runs(own, Some(Err("git failed".into())));
        assert!(matches!(&c, Changed::Unknown(w) if w.contains("can't be told (git failed)")), "{c:?}");
    }
    assert_eq!(with_runs(script.clone(), Some(Ok(Vec::new()))), script);
    assert_eq!(with_runs(Changed::Nothing, Some(Ok(Vec::new()))), Changed::Nothing);
    assert_eq!(
        with_runs(Changed::Nothing, Some(Ok(project.clone()))),
        Changed::Lines { of: "the project".into(), lines: project.clone() }
    );
    assert_eq!(
        with_runs(script, Some(Ok(project))),
        Changed::Lines {
            of: "x.sh, and the project".into(),
            lines: lines(&["x.sh", "+a", "Cargo.toml", "+b"])
        }
    );
}

/// The answer kept is the one given: `y` (once), `a` (this chat), `A` (always) — through `run_answer`, as a
/// card's key calls it. (`a` and `A` let the same run through unasked after, so no card says them here; the
/// head's letter is `the_card_says_what_changed_since_your_y`'s.) (The runs: `touch` in the test's own temp
/// folder.)
#[test]
fn the_answer_kept_is_the_key_pressed() {
    use crate::allow::Scope;
    let (dir, root) = repo("letters");
    let (mut ed, generation) = chat_in(&root);
    for (n, scope, letter) in [(1, Scope::Once, 'y'), (2, Scope::ThisChat, 'a'), (3, Scope::Always, 'A')] {
        let cmd = format!("touch made{n}.txt && echo made");
        let (id, _) = ask_run(&mut ed, generation, n, json!({ "command": cmd }));
        let crate::offer::What::Run(card) = &ed.offers[0].what else { panic!("a run card") };
        let card = (**card).clone();
        crate::tools::run_answer(&mut ed, id, card, Some(scope));
        pump_until(&mut ed, &|ed| ed.session.answered.iter().any(|a| a.card == id), "the answer kept");
        let a = ed.session.answered.iter().find(|a| a.card == id).unwrap();
        assert_eq!(a.answer, letter, "{scope:?}");
        pump_until(&mut ed, &|_| root.join(format!("made{n}.txt")).exists(), "it ran");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Answers are kept until tarae quits, not with the conversation: after a new one (`:chat-new`), the same
/// run's card still says which card it's the same as. (Answered `n`: nothing runs.)
#[test]
fn an_answer_outlasts_the_conversation() {
    let (dir, root) = repo("newchat");
    let (mut ed, generation) = chat_in(&root);
    let args = json!({ "command": "make demo" });
    let (first, _) = ask_run(&mut ed, generation, 1, args.clone());
    no_to(&mut ed, 1, first);
    crate::chat::reset(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    (c.state, c.focused) = (crate::chat::State::Thinking, false);
    let generation = c.generation();
    let (_, q) = ask_run(&mut ed, generation, 2, args);
    assert!(q.starts_with(&format!("Same as card#{first} — n at ")), "{q}");
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}

/// A pack's `entry` (`cargo xtask` runs `xtask/`): a part of the line before its first pipe with its words in
/// a row — after variables and a wrapper too — never what only filters it, nor another command.
#[test]
fn an_entry_is_the_commands_words_in_a_row() {
    let e = [crate::sandboxpack::Entry {
        run: vec!["cargo".into(), "xtask".into()],
        files: vec!["xtask/".into()],
    }];
    let files = |cmd: &str| entry_files_in(cmd, &e);
    for cmd in [
        "cargo xtask linux clippy",
        "DOCKER_HOST=x timeout 1800 cargo xtask linux test",
        "cd sub && cargo xtask shot",
        "cargo xtask lint 2>&1 | tail",
    ] {
        assert_eq!(files(cmd), ["xtask/"], "{cmd}");
    }
    for cmd in ["cargo test", "cargo xtasks", "echo | cargo xtask", "cargo build xtask"] {
        assert!(files(cmd).is_empty(), "{cmd}");
    }
    // (the built-in packs: the rust pack's)
    assert_eq!(entry_files("cargo xtask linux clippy"), ["xtask/"]);
    // A folder holds what's in it, not what only starts the same
    assert!(in_entry("xtask/src/main.rs", "xtask/") && in_entry("a.sh", "a.sh"));
    assert!(!in_entry("xtasks/x.rs", "xtask/") && !in_entry("xtask", "xtask/"));
}
