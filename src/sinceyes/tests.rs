//! What changed since a yes: the project's hunks (never a file outside it), a script's diff, "unchanged" —
//! and the card that says it, still waiting for `y`.

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
    let dir = std::env::temp_dir().join(format!("tarae-sinceyes-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("proj/src")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join("proj/src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(dir.join("other.txt"), "outside-marker one\n").unwrap();
    git_in(&dir, &["init", "-q"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    (dir.clone(), dir.join("proj"))
}

fn yes(root: &Path, cmd: &str) -> Yes {
    Yes { cmd: cmd.into(), cwd: root.into(), at: SystemTime::now(), kept: take(root, root, cmd) }
}

/// A command that runs the project's code: what changed since the yes is the project's hunks — a line
/// changed, a file new — and nothing of a file outside the project; nothing changed, "unchanged". git's own
/// index is left as it was (the tree is staged into a copy). A file a card can't show as it is: named only.
#[test]
fn the_projects_changes_since_the_yes_and_nothing_outside_it() {
    let (dir, root) = repo("tree");
    let cmd = "cargo test lib_a";
    // (an edit before the yes is part of what was said yes to)
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 1 }\n").unwrap();
    let index = std::fs::read(dir.join(".git/index")).unwrap();
    let y = yes(&root, cmd);
    assert!(matches!(y.kept, Kept::Tree(_)), "{:?}", y.kept);
    let s = y.since(&root, &root, cmd);
    assert_eq!(s.changed, Changed::Nothing);
    let (lines, diff) = s.card_lines();
    assert_eq!(lines, [format!("Unchanged since your y at {} — the same as it ran then.", s.at)]);
    assert!(diff.is_empty() && s.at.len() == 5 && s.at.as_bytes()[2] == b':', "{}", s.at);
    // A line changed, a file new — and the file outside the project changed too
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    std::fs::write(root.join("src/new.rs"), "fn c() {}\n").unwrap();
    std::fs::write(dir.join("other.txt"), "outside-marker two\n").unwrap();
    let s = y.since(&root, &root, cmd);
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
    let (card, diff) = s.card_lines();
    assert_eq!(card[0], format!("Since your y at {}, changed in the project:", s.at));
    assert_eq!(diff, 1..card.len(), "the diff's lines are drawn as one");
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index, "git's own index untouched");
    // One a card can't show as it is (a bidi mark): named, not shown
    std::fs::write(root.join("src/new.rs"), "fn c() {}\n// \u{202E}reversed\n").unwrap();
    let s = y.since(&root, &root, cmd);
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
    let dir = std::env::temp_dir().join(format!("tarae-sinceyes-script-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let root = dir.join("proj");
    std::fs::write(root.join("check.sh"), "echo one\necho two\n").unwrap();
    std::fs::write(dir.join("outside.sh"), "echo outside-marker\n").unwrap();
    let cmd = "sh check.sh && sh ../outside.sh";
    let y = yes(&root, cmd);
    assert!(matches!(&y.kept, Kept::Scripts(s) if s.len() == 2), "{:?}", y.kept);
    assert_eq!(y.since(&root, &root, cmd).changed, Changed::Nothing);
    std::fs::write(root.join("check.sh"), "echo one\necho three\n").unwrap();
    std::fs::write(dir.join("outside.sh"), "echo outside-marker changed\n").unwrap();
    let s = y.since(&root, &root, cmd);
    let Changed::Lines { of, lines } = &s.changed else { panic!("{s:?}") };
    assert!(of.starts_with("check.sh, "), "{of}");
    assert_eq!(lines[..4], ["check.sh", "@@ line 2", "-echo two", "+echo three"], "{lines:#?}");
    assert!(lines[4].ends_with("outside.sh — changed; outside the project, not shown here"), "{lines:#?}");
    assert!(!lines.iter().any(|l| l.contains("outside-marker")), "{lines:#?}");
    // Not a script, not a git checkout: it can't tell
    let y = yes(&root, "make check");
    let s = y.since(&root, &root, "make check");
    assert!(matches!(&s.changed, Changed::Unknown(w) if w.contains("not a git checkout")), "{s:?}");
    assert!(s.card_lines().0[0].contains("read it as new"), "{:?}", s.card_lines());
    std::fs::remove_dir_all(&dir).ok();
}

/// The card: Claude asks again to run, outside the sandbox, a command the user said yes to in this chat —
/// it says when that yes was and shows what changed since in the project (the diff decided, in its
/// colours' range); nothing since: "unchanged since your y at …". It still waits for `y` — never answered
/// for the user. Another command: a first card, as ever.
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
    // The first time: no word of an earlier yes
    let q = ask(&mut ed, 1, "true");
    assert!(!q.contains("your y at"), "{q}");
    yes(&mut ed, 1);
    // A change in the project, then the same again: when that yes was, and the change
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() { 2 }\n").unwrap();
    let q = ask(&mut ed, 2, "true");
    assert!(q.contains("Since your y at ") && q.contains(", changed in the project:"), "{q}");
    assert!(q.contains("\n-fn b() {}\n+fn b() { 2 }"), "{q}");
    let o = &ed.offers[0];
    let lines: Vec<&str> = o.question.split('\n').collect();
    let plus = lines.iter().position(|l| *l == "+fn b() { 2 }").unwrap();
    assert!(o.diff.as_ref().is_some_and(|d| d.contains(&plus)), "the diff's colours: {:?}", o.diff);
    assert!(o.decided_lines().contains(&plus), "what changed is what's decided");
    // (drawn: its rows on the card)
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 160, 50).unwrap();
    let mut vt = vt100::Parser::new(50, 160, 0);
    vt.process(&bytes);
    let s = vt.screen().contents();
    assert!(s.contains("+fn b() { 2 }") && s.contains("Since your y at"), "{s}");
    yes(&mut ed, 2);
    // Nothing since that yes: said — and the card still waits for its answer
    let q = ask(&mut ed, 3, "true");
    assert!(q.contains("Unchanged since your y at "), "{q}");
    assert!(answered(3).is_none() && !ed.offers.is_empty(), "never answered for the user");
    ed.offer_dismiss();
    pump(&mut ed, &|_| answered(3).is_some(), "told no");
    // Another command: a first card
    let q = ask(&mut ed, 4, "true && true");
    assert!(!q.contains("your y at"), "{q}");
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}
