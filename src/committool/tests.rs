//! The `commit` tool on the test's own repository: only the named paths go in, as the card showed them; what
//! else is staged stays staged; no hook runs (planted ones leave a marker if they do); what can't be committed
//! refuses the call, named; and the card's `y` and `n`.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;

/// git in the test's own repository — no hooks, no signing.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository in the test's scratch folder, its identity in its own config: `a.txt`, `b.txt`, `gone.txt`
/// committed; hooks planted in `.git/hooks` and in a `core.hooksPath` folder of the checkout, each leaving a
/// marker in the folder if it ever runs — and its commands trusted.
fn repo(s: &crate::scratch::Scratch) -> PathBuf {
    let dir = std::fs::canonicalize(s.path()).unwrap();
    for (p, t) in
        [("a.txt", "one\n"), ("b.txt", "bee\n"), ("gone.txt", "gone soon\n"), (".gitignore", "*.log\n")]
    {
        std::fs::write(dir.join(p), t).unwrap();
    }
    git_in(&dir, &["init", "-q", "-b", "main"]);
    git_in(&dir, &["config", "user.name", "Commit Tester"]);
    git_in(&dir, &["config", "user.email", "tester@example.invalid"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    plant_hooks(&dir, &dir.join(".git/hooks"));
    plant_hooks(&dir, &dir.join("hooks"));
    git_in(&dir, &["config", "core.hooksPath", "hooks"]);
    // (trusted, as the user's `y` on its card: nothing held — what keeps the hooks off is the tool's own)
    let r = crate::gitrepo::repo(&dir).unwrap();
    crate::gitrepo::answered(&r.config_file(), r.local.digest(), true);
    dir
}

const HOOKS: [&str; 6] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "reference-transaction",
    "post-index-change",
];

/// Every commit-time hook in `at`: each writes a marker into `dir` (the test's own folder) — nothing else.
fn plant_hooks(dir: &Path, at: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(at).unwrap();
    for h in HOOKS {
        let p = at.join(h);
        std::fs::write(&p, format!("#!/bin/sh\ntouch '{}'\n", dir.join(format!("ran-{h}")).display()))
            .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn no_hook_ran(dir: &Path) {
    for h in HOOKS {
        assert!(!dir.join(format!("ran-{h}")).exists(), "the {h} hook ran");
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Only the named paths go in — a change, a new file, a deleted one — as they were when the card was made (a
/// later write isn't in it); a change staged outside them stays staged, its blob as it was; the message as
/// given, no trailer; the author is the repository's identity; no hook runs, `n` or `y`.
#[test]
fn only_the_named_paths_go_in_and_what_else_is_staged_stays() {
    let s = crate::scratch::Scratch::new("commit-named");
    let dir = repo(&s);
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    std::fs::write(dir.join("b.txt"), "bee staged\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    let staged_b = git_in(&dir, &["ls-files", "-s", "b.txt"]);
    let head = git_in(&dir, &["rev-parse", "HEAD"]);
    let ask = prepare(
        &dir,
        "Add new, drop gone  \n\nThe body.\n",
        &strings(&["a.txt", "new.txt", "gone.txt"]),
        &[],
    )
    .unwrap();
    assert_eq!(ask.branch.as_deref(), Some("main"));
    assert_eq!(ask.staged, ["b.txt"], "what else is staged, named");
    assert!(ask.partial.is_empty() && ask.unsaved.is_empty());
    let status: Vec<(char, &str)> = ask.files.iter().map(|f| (f.status, f.shown.as_str())).collect();
    assert_eq!(status, [('M', "a.txt"), ('D', "gone.txt"), ('A', "new.txt")]);
    assert!(ask.patch.contains("+two") && ask.patch.contains("-gone soon"), "{}", ask.patch);
    assert!(!ask.patch.contains("bee"), "nothing of b.txt: {}", ask.patch);
    assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), head, "nothing committed before y");
    let temp: Vec<_> = std::fs::read_dir(dir.join(".git"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("tarae-commit-"))
        .collect();
    assert!(temp.is_empty(), "the temporary index is removed");
    // (written after the card: not in the commit)
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let done = commit(&ask).unwrap();
    assert_eq!(done.index, None);
    assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]).trim(), done.id);
    assert_eq!(
        git_in(&dir, &["log", "-1", "--format=%B"]),
        "Add new, drop gone\n\nThe body.\n\n",
        "as given, no trailer"
    );
    assert_eq!(
        git_in(&dir, &["log", "-1", "--format=%an <%ae>|%cn"]).trim(),
        "Commit Tester <tester@example.invalid>|Commit Tester"
    );
    assert_eq!(git_in(&dir, &["show", "HEAD:a.txt"]), "one\ntwo\n", "what the card showed");
    let what = git_in(&dir, &["diff-tree", "-r", "--no-commit-id", "--name-status", "HEAD"]);
    assert_eq!(what, "M\ta.txt\nD\tgone.txt\nA\tnew.txt\n");
    assert_eq!(git_in(&dir, &["ls-files", "-s", "b.txt"]), staged_b, "b.txt's staged blob as it was");
    assert_eq!(git_in(&dir, &["diff", "--cached", "--name-only"]), "b.txt\n", "only b.txt still staged");
    assert_eq!(git_in(&dir, &["diff", "--name-only"]), "a.txt\n", "the later write: a change on disk");
    assert_eq!(git_in(&dir, &["reflog", "-1", "--format=%gs"]).trim(), "commit (claude): Add new, drop gone");
    no_hook_ran(&dir);
}

/// A named file with part of it staged goes in whole, said; one with unsaved edits is said; the first commit of
/// an unborn branch; HEAD moved since the card refuses `y`, nothing committed.
#[test]
fn partly_staged_unsaved_first_commit_and_a_moved_head() {
    let s = crate::scratch::Scratch::new("commit-partial");
    let dir = repo(&s);
    std::fs::write(dir.join("a.txt"), "one\nstaged\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    std::fs::write(dir.join("a.txt"), "one\nstaged\nnot staged\n").unwrap();
    let ask = prepare(&dir, "a", &strings(&["a.txt"]), &[dir.join("a.txt")]).unwrap();
    assert_eq!(ask.partial, ["a.txt"]);
    assert_eq!(ask.unsaved, ["a.txt"]);
    let o = card(Commit { call: Call::test(1), ask: ask.clone() });
    let lines: Vec<&str> = o.question.split('\n').collect();
    let warned: Vec<&str> = o.warnings.iter().flat_map(|r| lines[r.clone()].iter().copied()).collect();
    assert_eq!(
        warned,
        [
            "a.txt: part of it was staged — the whole file goes in.",
            "a.txt has unsaved changes — what's on disk goes in."
        ]
    );
    // HEAD moved meanwhile
    git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "meanwhile"]);
    let moved = git_in(&dir, &["rev-parse", "HEAD"]);
    let e = commit(&ask).unwrap_err();
    assert!(e.contains("HEAD moved since the card"), "{e}");
    assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), moved, "nothing committed");
    // An unborn branch: the first commit
    let s2 = crate::scratch::Scratch::new("commit-first");
    let fresh = std::fs::canonicalize(s2.path()).unwrap();
    git_in(&fresh, &["init", "-q", "-b", "trunk"]);
    git_in(&fresh, &["config", "user.name", "First"]);
    git_in(&fresh, &["config", "user.email", "first@example.invalid"]);
    std::fs::write(fresh.join("x.txt"), "x\n").unwrap();
    std::fs::write(fresh.join("y.txt"), "y\n").unwrap();
    let ask = prepare(&fresh, "First", &strings(&["x.txt"]), &[]).unwrap();
    assert_eq!((ask.parent.as_deref(), ask.branch.as_deref()), (None, Some("trunk")));
    assert!(
        card(Commit { call: Call::test(2), ask: ask.clone() }).question.contains("The first commit on trunk")
    );
    commit(&ask).unwrap();
    assert_eq!(git_in(&fresh, &["ls-tree", "--name-only", "HEAD"]), "x.txt\n");
    assert_eq!(git_in(&fresh, &["ls-files"]), "x.txt\n", "the index has it, nothing else");
}

/// What can't be committed refuses the whole call, named: outside the project, git's own folder, a folder, a
/// link to a folder outside, an ignored file, one with no change, no such file, another repository inside; an
/// empty message; a rebase or a merge under way, a conflicted index. Nothing is staged or committed by any.
#[test]
fn what_cant_be_committed_refuses_the_call_named() {
    let s = crate::scratch::Scratch::new("commit-refused");
    let dir = repo(&s);
    let outside = crate::scratch::Scratch::new("commit-refused-outside");
    std::fs::write(outside.path().join("o.txt"), "o\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.join("out")).unwrap();
    std::fs::create_dir_all(dir.join("sub/inner")).unwrap();
    git_in(&dir.join("sub/inner"), &["init", "-q"]);
    std::fs::write(dir.join("sub/inner/f.txt"), "f\n").unwrap();
    std::fs::write(dir.join("debug.log"), "log\n").unwrap();
    std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
    let index = std::fs::read(dir.join(".git/index")).unwrap();
    let head = git_in(&dir, &["rev-parse", "HEAD"]);
    let refused = |paths: &[&str]| prepare(&dir, "m", &strings(paths), &[]).unwrap_err();
    for (paths, said) in [
        (&["../x.txt"][..], "outside the project"),
        (&["out/o.txt"], "outside the project"),
        (&[".git/config"], "in git's own folder"),
        (&["sub"], "is a folder"),
        (&["sub/inner/f.txt"], "is in another repository (sub/inner)"),
        (&["debug.log"], "debug.log: ignored"),
        (&["a.txt", "b.txt"], "b.txt: no change from HEAD"),
        (&["nope.txt"], "nope.txt: no such file"),
    ] {
        let e = refused(paths);
        assert!(e.contains(said), "{paths:?}: {e}");
    }
    assert!(prepare(&dir, " \n\n", &strings(&["a.txt"]), &[]).unwrap_err().contains("message: empty"));
    // A rebase or a merge under way (git's own markers of one), a conflicted index
    std::fs::create_dir_all(dir.join(".git/rebase-merge")).unwrap();
    assert!(refused(&["a.txt"]).contains("a rebase is under way"));
    std::fs::remove_dir_all(dir.join(".git/rebase-merge")).unwrap();
    std::fs::write(dir.join(".git/MERGE_HEAD"), &head).unwrap();
    assert!(refused(&["a.txt"]).contains("a merge is under way"));
    std::fs::remove_file(dir.join(".git/MERGE_HEAD")).unwrap();
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index, "no refusal touched the index");
    let blob = git_in(&dir, &["rev-parse", "HEAD:b.txt"]);
    let (blob, zero) = (blob.trim(), "0".repeat(blob.trim().len()));
    let info = format!("0 {zero}\tb.txt\n100644 {blob} 1\tb.txt\n100644 {blob} 2\tb.txt\n");
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(&dir).args(["-c", "core.hooksPath=/dev/null", "update-index", "--index-info"]);
    cmd.stdin(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), info.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(refused(&["a.txt"]).contains("the index has conflicts"));
    assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), head, "nothing committed");
    no_hook_ran(&dir);
}

/// The card: the message decided, a row per file with its count, a protected name and a key file in the
/// warning colour, the diff's start in the diff's colours; `y` commits and answers Claude with the commit, `n`
/// answers no and commits nothing; a second call while one waits is refused at once.
#[test]
fn the_card_says_what_goes_in_and_y_commits_n_doesnt() {
    let s = crate::scratch::Scratch::new("commit-card");
    let dir = repo(&s);
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
    std::fs::write(dir.join(".env"), "X=1\n").unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    let args = json!({"message": "Build it", "paths": ["a.txt", "Makefile", ".env"]});
    call(&mut ed, Call::test(51), &args).unwrap();
    pump(&mut ed, "the card", |ed| ed.offers.iter().any(|o| o.kind() == "commit"));
    let again = call(&mut ed, Call::test(52), &json!({"message": "m", "paths": ["b.txt"]})).unwrap_err();
    assert!(again.contains("already waiting"), "{again}");
    let o = ed.offers.iter().find(|o| o.kind() == "commit").unwrap();
    assert_eq!(o.title, "Claude commits 3 files");
    assert_eq!(o.answers(false), [('y', "Commit"), ('v', "See the diff"), ('n', "Don't")]);
    assert!(o.weighty() && o.claudes());
    let lines: Vec<&str> = o.question.split('\n').collect();
    assert_eq!(&lines[o.decided_lines()], ["Build it"]);
    let warned: Vec<&str> = o.warnings.iter().flat_map(|r| lines[r.clone()].iter().copied()).collect();
    assert_eq!(warned.len(), 4, "{warned:?}");
    assert!(warned[0].starts_with("  .env") && warned[0].ends_with(KEY.0), "{warned:?}");
    assert!(warned[1].starts_with("  Makefile") && warned[1].ends_with(PROTECTED.0), "{warned:?}");
    assert_eq!(warned[2..], [PROTECTED.1, KEY.1], "what each means, once");
    assert!(lines[o.diff.clone().unwrap()].contains(&"+two"), "{lines:?}");
    // `n`: no, nothing committed
    let head = git_in(&dir, &["rev-parse", "HEAD"]);
    ed.offer_dismiss();
    assert_eq!(answered(51), Some(Err("the user said no — nothing was committed".into())));
    assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), head);
    // `y`: committed, Claude told which
    call(&mut ed, Call::test(53), &json!({"message": "Two", "paths": ["a.txt"]})).unwrap();
    pump(&mut ed, "the card", |ed| ed.offers.iter().any(|o| o.kind() == "commit"));
    ed.offer_accept();
    pump(&mut ed, "the commit", |_| answered(53).is_some());
    let said = answered(53).unwrap().unwrap();
    let short = git_in(&dir, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(
        said,
        format!("Committed {} on main: Two (1 file). Not pushed; nothing else staged went in.", short.trim())
    );
    assert!(!ed.offers.iter().any(|o| o.kind() == "commit"), "the card went");
    no_hook_ran(&dir);
}

/// auditfix2 F11: a file of the checkout's own hooks folder is warned about — once committed it runs as the
/// user with no hooks card ("as the last commit has it") — also while the repository's `core.hooksPath` waits
/// for its trust card (a `y` there later makes it the hooks folder); a file beside it isn't.
#[test]
fn a_hook_is_warned_about() {
    let s = crate::scratch::Scratch::new("commit-hook");
    let dir = repo(&s);
    std::fs::write(dir.join("b.txt"), "bee bee\n").unwrap();
    let a = prepare(&dir, "Lint", &strings(&["hooks/pre-commit", "b.txt"]), &[]).unwrap();
    let warned: Vec<(&str, Option<&str>)> = a.files.iter().map(|f| (f.shown.as_str(), f.warn)).collect();
    assert_eq!(warned, [("b.txt", None), ("hooks/pre-commit", Some("a git hook"))]);
    let o = card(Commit { call: Call::test(61), ask: a });
    let lines: Vec<&str> = o.question.split('\n').collect();
    let warned: Vec<&str> = o.warnings.iter().flat_map(|r| lines[r.clone()].iter().copied()).collect();
    assert!(warned.iter().any(|l| l.starts_with("A git hook: runs as you")), "{warned:?}");
    // Not trusted yet: its hooks folder held, still warned about
    let s = crate::scratch::Scratch::new("commit-hook-held");
    let dir = std::fs::canonicalize(s.path()).unwrap();
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git_in(&dir, &["init", "-q", "-b", "main"]);
    git_in(&dir, &["-c", "user.name=t", "-c", "user.email=t@example.invalid", "add", "-A"]);
    git_in(&dir, &["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "base"]);
    git_in(&dir, &["config", "core.hooksPath", ".githooks"]);
    std::fs::create_dir_all(dir.join(".githooks")).unwrap();
    std::fs::write(dir.join(".githooks/pre-push"), "#!/bin/sh\n").unwrap();
    let a = prepare(&dir, "Push check", &strings(&[".githooks/pre-push"]), &[]).unwrap();
    assert_eq!(a.files[0].warn, Some("a git hook"));
}

/// Events until `done`.
fn pump(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + crate::test_results::PATIENCE;
    while !done(ed) {
        assert!(std::time::Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// What call `id` was answered (`tools::ANSWERED`).
fn answered(id: u64) -> Option<Result<String, String>> {
    crate::tools::ANSWERED.with(|a| a.borrow().iter().find(|(i, _)| *i == json!(id)).map(|(_, r)| r.clone()))
}
