//! A run's changes: the trees before and after, what changed between them (text, not text, deleted, new),
//! a blob written back only over what the run left — and the whole way through Claude's `run`: recorded in
//! the turn, said to Claude, taken back by `space u` and brought back by `space U`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// A repository in the test's temp folder: text, a file that isn't text, one to delete, an untracked one,
/// an ignored build folder and an ignored file of its own.
fn repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tarae-snapshot-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join("src/a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.join("b.bin"), b"\x00\x01bin-before").unwrap();
    std::fs::write(dir.join("src/gone.txt"), "kept-marker\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "build/\n.env.local\n").unwrap();
    std::fs::write(dir.join("build/out.o"), "built\n").unwrap();
    std::fs::write(dir.join(".env.local"), "TOKEN=marker\n").unwrap();
    git_in(&dir, &["init", "-q"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.join("untracked.txt"), "mine\n").unwrap();
    dir
}

fn file(c: &[Change], dir: &Path, rel: &str) -> Change {
    c.iter().find(|c| c.path == dir.join(rel)).cloned().unwrap_or_else(|| panic!("{rel} in {c:#?}"))
}

/// What a run changed: a line (text both sides), a file that isn't text (by its blobs), one deleted, one
/// new, an untracked one changed — and nothing of the ignored build folder. git's own index is left as it
/// was; tarae's is kept for the next one. Nothing changed: no changes. A file the editor wrote meanwhile
/// isn't the run's.
#[test]
fn what_a_run_changed_text_blobs_deleted_new_and_not_ignored_files() {
    let dir = repo("changes");
    let index = std::fs::read(dir.join(".git/index")).unwrap();
    let before = Before::take(&dir);
    assert!(before.tree.is_ok(), "{:?}", before.tree);
    assert!(dir.join(".git").join(INDEX).exists(), "tarae's own index, kept");
    assert_eq!(before.since().unwrap(), Found::default(), "nothing changed");
    std::fs::write(dir.join("src/a.txt"), "one\nTWO\n").unwrap();
    std::fs::write(dir.join("b.bin"), b"\x00\x02bin-after").unwrap();
    std::fs::remove_file(dir.join("src/gone.txt")).unwrap();
    std::fs::write(dir.join("src/new.txt"), "fresh\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "mine, changed\n").unwrap();
    std::fs::write(dir.join("build/out.o"), "built again\n").unwrap();
    let Found { files: c, skipped, left_out, ignored_gone, protected } = before.since().unwrap();
    assert!(skipped.is_empty() && left_out == 0, "{skipped:?} {left_out}");
    assert!(protected.is_empty(), "no protected name made: {protected:?}");
    assert!(ignored_gone.is_empty(), "an ignored file changed isn't one deleted: {ignored_gone:?}");
    assert_eq!(c.len(), 5, "{c:#?}");
    let a = file(&c, &dir, "src/a.txt");
    assert!(a.text(), "{a:?}");
    assert_eq!(a.before.unwrap().text.as_deref(), Some("one\ntwo\n"));
    assert_eq!(a.after.unwrap().text.as_deref(), Some("one\nTWO\n"));
    let b = file(&c, &dir, "b.bin");
    assert!(!b.text() && b.before.as_ref().is_some_and(|s| s.text.is_none()), "{b:?}");
    let gone = file(&c, &dir, "src/gone.txt");
    assert_eq!((gone.before.unwrap().text.as_deref(), gone.after), (Some("kept-marker\n"), None));
    let new = file(&c, &dir, "src/new.txt");
    assert!(new.before.is_none() && new.after.is_some_and(|s| s.text.as_deref() == Some("fresh\n")));
    file(&c, &dir, "untracked.txt");
    assert!(!c.iter().any(|c| c.path.starts_with(dir.join("build"))), "ignored files aren't kept: {c:#?}");
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index, "git's own index untouched");
    // (what the editor wrote meanwhile isn't the run's)
    let mut before = before;
    before.wrote.push(dir.join("src/a.txt"));
    assert!(!before.since().unwrap().files.iter().any(|c| c.path.ends_with("a.txt")));
    std::fs::remove_dir_all(&dir).ok();
}

/// More new files at once than `NEW_MAX` aren't kept (a tree of 200k took 91 s and 800 MB): the snapshot
/// keeps what it had, as it is now — a change to a file it had is still found — and says how many it left
/// out. Fewer again: kept as ever.
#[test]
fn too_many_new_files_at_once_are_left_out_and_said() {
    let dir = repo("many");
    let before = Before::take(&dir);
    std::fs::create_dir_all(dir.join("gen")).unwrap();
    for i in 0..=NEW_MAX {
        std::fs::write(dir.join(format!("gen/f{i}.txt")), "x\n").unwrap();
    }
    std::fs::write(dir.join("src/a.txt"), "one\nchanged\n").unwrap();
    let found = before.since().unwrap();
    assert_eq!(found.left_out, NEW_MAX + 1);
    assert_eq!(found.files.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), [dir.join("src/a.txt")]);
    std::fs::remove_dir_all(dir.join("gen")).unwrap();
    std::fs::write(dir.join("src/b.txt"), "new\n").unwrap();
    let found = Before::take(&dir).since().unwrap();
    assert_eq!(found, Found::default(), "{found:?}");
    let t = snap(&dir).unwrap();
    assert_eq!(t.left_out, 0);
    std::fs::remove_dir_all(&dir).ok();
}

/// Ignored files a run deleted are named, not kept: an ignored file, an ignored folder whole — not a file
/// inside an ignored folder (that folder is one entry, never walked), and not one the run left.
#[test]
fn ignored_files_a_run_deleted_are_named() {
    let dir = repo("ignored");
    let before = Before::take(&dir);
    assert_eq!(before.ignored.as_deref(), Some(&[".env.local".to_string(), "build/".to_string()][..]));
    std::fs::remove_file(dir.join("build/out.o")).unwrap();
    assert!(before.since().unwrap().ignored_gone.is_empty(), "inside an ignored folder: not seen");
    std::fs::remove_file(dir.join(".env.local")).unwrap();
    std::fs::remove_dir(dir.join("build")).unwrap();
    let found = before.since().unwrap();
    assert_eq!(found.ignored_gone, [".env.local", "build/"]);
    assert!(found.files.is_empty(), "nothing a tree has: {found:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The protected names a run made where none was — what Landlock can't refuse: a manifest in a new folder,
/// a `.cargo` folder in `src/`, an ignored `.envrc` (a tree hasn't it; the ignored entries before and after
/// have) — not one below `target/` (a tool's output), not a file of another name, not a change to a file
/// that was there, and not what the editor wrote meanwhile (Claude's reviewed Edit).
#[test]
fn protected_names_a_run_made_are_found() {
    let dir = repo("protected");
    std::fs::write(dir.join(".gitignore"), "build/\n.env.local\n.envrc\n").unwrap();
    let mut before = Before::take(&dir);
    for (rel, text) in [
        ("sub/Cargo.toml", "[package]\n"),
        ("src/.cargo/config.toml", "[build]\n"),
        (".envrc", "export X=1\n"),
        ("target/pkg/Cargo.toml", "[package]\n"),
        ("src/lib.rs", "fn f() {}\n"),
        ("src/a.txt", "one\nchanged\n"),
        ("edited/build.rs", "fn main() {}\n"),
    ] {
        std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
        std::fs::write(dir.join(rel), text).unwrap();
    }
    before.wrote.push(dir.join("edited/build.rs"));
    let mut found = before.since().unwrap().protected;
    found.sort();
    assert_eq!(found, [".envrc", "src/.cargo/config.toml", "sub/Cargo.toml"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// A blob goes back only over what the run left: the file still as the run made it → its blob before
/// (or removed, when there was none); changed since → left as it is, and said.
#[test]
fn a_blob_goes_back_only_over_what_the_run_left() {
    let dir = repo("put");
    let before = Before::take(&dir);
    std::fs::write(dir.join("b.bin"), b"\x00\x02bin-after").unwrap();
    std::fs::write(dir.join("made.bin"), b"\x00made").unwrap();
    let c = before.since().unwrap().files;
    let (b, made) = (file(&c, &dir, "b.bin"), file(&c, &dir, "made.bin"));
    let id = |s: &Option<Blob>| s.as_ref().map(|s| s.id.clone());
    put(&dir, &b.path, id(&b.after).as_deref(), id(&b.before).as_deref()).unwrap();
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), b"\x00\x01bin-before");
    put(&dir, &made.path, id(&made.after).as_deref(), None).unwrap();
    assert!(!made.path.exists());
    // (changed since: left as it is)
    std::fs::write(dir.join("b.bin"), b"\x00theirs").unwrap();
    let e = put(&dir, &b.path, id(&b.before).as_deref(), id(&b.after).as_deref()).unwrap_err();
    assert!(e.contains("changed since"), "{e}");
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), b"\x00theirs");
    std::fs::remove_dir_all(&dir).ok();
}

/// What a snapshot costs in a tree of many untracked files (`TARAE_PERF_SNAP_FILES`, 20k by default; the
/// design's question was 200k): the first (past `NEW_MAX` new files: they're counted, not kept), the next
/// with nothing changed (tarae's own index kept: stats only), a run's whole after-side with ten files changed
/// (tree, diff, blobs) — and the way it was before, a fresh copy of the real index each time, which hashes
/// every untracked file again. Plus what the first one wrote to disk.
#[test]
fn perf_snapshot_of_untracked_files() {
    if crate::test_results::unoptimized("snapshot::tests::perf_snapshot_of_untracked_files") {
        return;
    }
    let n: usize = std::env::var("TARAE_PERF_SNAP_FILES").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let dir = std::env::temp_dir().join(format!("tarae-snapshot-perf-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join("README"), "tracked\n").unwrap();
    git_in(&dir, &["init", "-q"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    for d in 0..n.div_ceil(1000) {
        let sub = dir.join(format!("d{d}"));
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..1000.min(n - d * 1000) {
            std::fs::write(sub.join(format!("f{f}.txt")), format!("file {d}/{f}\n")).unwrap();
        }
    }
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let first = snap(&dir).unwrap();
    let first_ms = ms(t);
    fn du(p: &Path) -> u64 {
        match std::fs::read_dir(p) {
            Err(_) => std::fs::metadata(p).map_or(0, |m| m.len()),
            Ok(d) => d.flatten().map(|e| du(&e.path())).sum(),
        }
    }
    let (objects, index) = (du(&dir.join(".git/objects")), du(&dir.join(".git").join(INDEX)));
    let mut again = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        assert_eq!(snap(&dir).unwrap(), first);
        again.push(ms(t));
    }
    crate::sorting::by(&mut again, f64::total_cmp);
    // (ten files it had changed: in the first ones, kept whatever the count)
    let before = Before::take(&dir);
    std::fs::write(dir.join("README"), "changed\n").unwrap();
    let t = Instant::now();
    let found = before.since().unwrap();
    let since_ms = ms(t);
    assert_eq!(found.files.len(), 1, "{found:?}");
    // (the way before: a fresh copy of the real index — every untracked file hashed again)
    let temp = dir.join(".git/copy.index");
    let t = Instant::now();
    std::fs::copy(dir.join(".git/index"), &temp).unwrap();
    stage(&dir, &temp).unwrap();
    let copy_ms = ms(t);
    std::fs::remove_dir_all(&dir).ok();
    let lines = [
        format!("PERF snapshot_first_{n}_files_ms {first_ms:.1} budget=600000 noise=500"),
        format!("PERF snapshot_again_{n}_files_ms {:.1} budget=5000 noise=20", again[again.len() / 2]),
        format!("PERF snapshot_run_changes_{n}_files_ms {since_ms:.1} budget=5000 noise=20"),
        format!(
            "(left out: {} · a fresh copy of the real index each time: {copy_ms:.1} ms · first: {:.1} MB of \
             objects, {:.1} MB index)",
            first.left_out,
            objects as f64 / 1e6,
            index as f64 / 1e6
        ),
    ];
    let _ = std::io::Write::write_all(&mut std::io::stderr(), (lines.join("\n") + "\n").as_bytes());
}

/// What a check pays for being covered, in a checkout like this repository's: 600 tracked files, an ignored
/// `target/` of 20k files the check writes 1,000 more into — the snapshot before it (ignored listing, tree)
/// and the one after (tree, ignored listing, nothing to diff). Medians of 7.
#[test]
fn perf_snapshot_around_a_check() {
    if crate::test_results::unoptimized("snapshot::tests::perf_snapshot_around_a_check") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("tarae-snapshot-check-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    for d in 0..30 {
        std::fs::create_dir_all(dir.join(format!("src/m{d}"))).unwrap();
        for f in 0..20 {
            std::fs::write(dir.join(format!("src/m{d}/f{f}.rs")), format!("fn f{d}_{f}() {{}}\n").repeat(40))
                .unwrap();
        }
    }
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    git_in(&dir, &["init", "-q"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    for d in 0..20 {
        std::fs::create_dir_all(dir.join(format!("target/debug/d{d}"))).unwrap();
        for f in 0..1000 {
            std::fs::write(dir.join(format!("target/debug/d{d}/o{f}")), "obj\n").unwrap();
        }
    }
    Before::take(&dir); // (tarae's index made: the first of a checkout's snapshots, once)
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let (mut before, mut after) = (Vec::new(), Vec::new());
    for k in 0..7 {
        let t = Instant::now();
        let snap = Before::take(&dir);
        before.push(ms(t));
        std::fs::create_dir_all(dir.join(format!("target/debug/new{k}"))).unwrap();
        for f in 0..1000 {
            std::fs::write(dir.join(format!("target/debug/new{k}/o{f}")), "obj\n").unwrap();
        }
        let t = Instant::now();
        let found = snap.since().unwrap();
        after.push(ms(t));
        assert_eq!(found, Found::default(), "a check's build output isn't a change");
    }
    std::fs::remove_dir_all(&dir).ok();
    crate::sorting::by(&mut before, f64::total_cmp);
    crate::sorting::by(&mut after, f64::total_cmp);
    let lines = [
        format!("PERF snapshot_before_a_check_ms {:.1} budget=500 noise=10", before[3]),
        format!("PERF snapshot_after_a_check_ms {:.1} budget=500 noise=10", after[3]),
    ];
    let _ = std::io::Write::write_all(&mut std::io::stderr(), (lines.join("\n") + "\n").as_bytes());
}

/// Claude's command that isn't a check, all the way: the checkout is taken before it runs; what it deleted,
/// made and changed (text and not) joins the turn as written unseen and Claude's report names it; the turn
/// ends in a checkpoint; `space u` brings the deleted files back — text and blob — and takes the new one
/// away; `space U` does the run's changes again. (A harmless command: it changes only the test's own
/// files.)
#[test]
fn a_runs_changes_join_the_turn_and_undo_takes_them_back() {
    let dir = repo("undo");
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
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
    let cmd =
        "rm src/gone.txt b.bin && printf 'one\\nTHREE\\n' > src/a.txt && printf 'made\\n' > src/new.txt";
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run",
        "arguments": {"command": cmd, "outside_sandbox": true, "reason": "a test of its own files"}}});
    assert!(crate::tools::mcp(&mut ed, generation, "r1", &msg).is_none(), "it waits for the user");
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "its card");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(1).is_some(), "it ran");
    let report = answered(1).unwrap().unwrap();
    assert!(!dir.join("src/gone.txt").exists() && !dir.join("b.bin").exists(), "{report}");
    assert!(
        report.contains("It changed in the checkout: ")
            && ["b.bin (deleted)", "src/a.txt", "src/gone.txt (deleted)", "src/new.txt (new)"]
                .iter()
                .all(|n| report.contains(n)),
        "{report}"
    );
    let turn = &ed.session.checkpoints.turn;
    assert_eq!(turn.len(), 4, "{turn:#?}");
    assert!(turn.iter().all(|t| t.unseen), "{turn:#?}");
    let gone = turn.iter().find(|t| t.path.ends_with("gone.txt")).unwrap();
    assert!(gone.gone && gone.before.as_ref().is_some_and(|b| b == "kept-marker\n"), "{gone:?}");
    assert!(turn.iter().any(|t| t.path.ends_with("b.bin") && t.bytes.is_some() && t.gone));
    // The turn ends: a checkpoint (its review not open here — the files are what's checked)
    ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
    assert!(crate::checkpoint::gather(&mut ed, "tidy up"), "written unseen: gathered");
    pump(&mut ed, &|ed| ed.session.checkpoints.list.len() == 1, "its checkpoint");
    assert!(!ed.session.checkpoints.list[0].seen, "reviewed at the turn's end, like Claude's edits");
    // space u: back as they were
    crate::checkpoint::step(&mut ed, true).unwrap();
    pump(&mut ed, &|_| dir.join("b.bin").exists(), "the blob written back");
    assert_eq!(std::fs::read_to_string(dir.join("src/gone.txt")).unwrap(), "kept-marker\n");
    assert_eq!(std::fs::read_to_string(dir.join("src/a.txt")).unwrap(), "one\ntwo\n");
    assert!(!dir.join("src/new.txt").exists(), "the file it made goes");
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), b"\x00\x01bin-before");
    // space U: the run's changes again
    crate::checkpoint::step(&mut ed, false).unwrap();
    pump(&mut ed, &|_| !dir.join("b.bin").exists(), "the blob gone again");
    assert!(!dir.join("src/gone.txt").exists(), "deleted again");
    assert_eq!(std::fs::read_to_string(dir.join("src/new.txt")).unwrap(), "made\n");
    assert_eq!(std::fs::read_to_string(dir.join("src/a.txt")).unwrap(), "one\nTHREE\n");
    std::fs::remove_dir_all(&dir).ok();
}

/// Checks are covered too (a test that deletes files is what undo is for): a check through `run` (`make`)
/// and Claude's tests in the test pane — what each deleted joins the turn, the report says so,
/// and `space u` brings it back. An ignored file one deleted is named in the report, as not kept. (No
/// sandbox here — `llm.sandbox` off; harmless commands: the test's own files.)
#[test]
fn checks_and_tests_are_covered_and_ignored_deletions_said() {
    let dir = repo("checks");
    std::fs::write(dir.join("del.mk"), "all:\n\trm -f src/gone.txt .env.local\n").unwrap();
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    ed.config.llm.sandbox = false;
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
    // A check, through `run` (`make` is one — it feeds the user's list — though it asks before it runs)
    assert!(crate::tools::is_check("make -f del.mk"));
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run",
        "arguments": {"command": "make -f del.mk"}}});
    assert!(crate::tools::mcp(&mut ed, generation, "r1", &msg).is_none());
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "its card");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(1).is_some(), "the check ran");
    let report = answered(1).unwrap().unwrap();
    assert!(!dir.join("src/gone.txt").exists() && !dir.join(".env.local").exists(), "{report}");
    assert!(report.contains("It changed in the checkout: src/gone.txt (deleted)"), "{report}");
    assert!(
        report.contains(
            "It deleted ignored files, which aren't kept — the user can't take them back: .env.local."
        ),
        "{report}"
    );
    // Claude's tests, in the test pane
    let target = crate::testing::Target {
        label: "deletes".into(),
        cwd: dir.clone(),
        how: crate::testing::How::Pack {
            lang: "python".into(),
            argv: vec!["sh".into(), "-c".into(), "rm src/a.txt && echo ok".into()],
            debug: None,
            parse: "python".into(),
            build: None,
            debug_run: None,
            junit: None,
        },
    };
    let call = crate::tools::Call::of(generation, "r2", json!(2));
    let job = crate::tools::Job::Tests(Box::new(target));
    crate::tools::guard_and_go(&mut ed, crate::tools::For::Claude(call), job, false, Default::default());
    pump(&mut ed, &|_| answered(2).is_some(), "the tests ran");
    let report = answered(2).unwrap().unwrap();
    assert!(report.contains("It changed in the checkout: src/a.txt (deleted)"), "{report}");
    let turn: Vec<_> = ed.session.checkpoints.turn.iter().map(|t| (t.path.clone(), t.gone)).collect();
    assert_eq!(turn, [(dir.join("src/gone.txt"), true), (dir.join("src/a.txt"), true)]);
    // space u: both back
    ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
    assert!(crate::checkpoint::gather(&mut ed, "run the tests"));
    pump(&mut ed, &|ed| ed.session.checkpoints.list.len() == 1, "its checkpoint");
    crate::checkpoint::step(&mut ed, true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("src/gone.txt")).unwrap(), "kept-marker\n");
    assert_eq!(std::fs::read_to_string(dir.join("src/a.txt")).unwrap(), "one\ntwo\n");
    assert!(!dir.join(".env.local").exists(), "an ignored file isn't kept: not back");
    std::fs::remove_dir_all(&dir).ok();
}

/// A run of Claude's that makes a protected name where none was: Claude's report says so, and its next run
/// waits on a card naming it — `n` tells Claude no and nothing runs, the next asks again; `y` runs it, and
/// the one after doesn't ask. (Harmless commands in the test's own repository.)
#[test]
fn a_protected_name_a_run_made_is_asked_about_before_the_next_run() {
    let dir = repo("made");
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    ed.config.llm.sandbox = false;
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
    let run = |ed: &mut crate::editor::Editor, n: u64, cmd: &str| {
        let call = crate::tools::Call::of(generation, &format!("r{n}"), json!(n));
        let job = crate::tools::Job::Command(cmd.into(), dir.clone());
        crate::tools::guard_and_go(ed, crate::tools::For::Claude(call), job, false, Default::default());
    };
    run(&mut ed, 1, "mkdir sub && printf '[package]\\n' > sub/Cargo.toml");
    pump(&mut ed, &|_| answered(1).is_some(), "the first run");
    let report = answered(1).unwrap().unwrap();
    assert!(report.contains("It made sub/Cargo.toml, a name the sandbox keeps"), "{report}");
    // n: nothing runs, Claude hears why
    run(&mut ed, 2, "touch two");
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "the card");
    let o = &ed.offers[0];
    assert!(matches!(o.what, crate::offer::What::ProtectedMade { .. }) && o.weighty(), "{}", o.title);
    assert_eq!(o.title, crate::tools::MADE_TITLE);
    assert!(o.question.starts_with("touch two\n\nIts last run made sub/Cargo.toml"), "{}", o.question);
    assert!(o.question.contains("a name the sandbox keeps") && o.question.contains("what's in it runs"));
    ed.offer_dismiss();
    let no = answered(2).unwrap().unwrap_err();
    assert!(no.contains("made sub/Cargo.toml") && no.contains("chose not to run `touch two`"), "{no}");
    assert!(!dir.join("two").exists());
    // Asked again; y: it runs, and the next doesn't ask
    run(&mut ed, 3, "touch three");
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "the card again");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(3).is_some(), "the third run");
    assert!(dir.join("three").exists() && ed.session.made_protected.is_empty());
    run(&mut ed, 4, "touch four .envrc sub/build.rs");
    pump(&mut ed, &|_| answered(4).is_some(), "the fourth run, unasked");
    assert!(dir.join("four").exists());
    let report = answered(4).unwrap().unwrap();
    assert!(report.contains("It made .envrc, sub/build.rs — names the sandbox keeps"), "{report}");
    // Two: said as two
    run(&mut ed, 5, "touch five");
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "the card for both");
    let q = &ed.offers[0].question;
    assert!(q.contains("made .envrc, sub/build.rs — names") && q.contains("they're among"), "{q}");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(5).is_some(), "the fifth run");
    assert!(dir.join("five").exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// A run the user stops: what it changed by then joins the turn all the same. A run that ends after its
/// turn did (stopped meanwhile): what it changed is a checkpoint of its own, named by the command, and the
/// chat says so — never left in a turn that's over. (Harmless commands: the test's own files, `sleep`.)
#[test]
fn a_stopped_run_and_one_past_its_turn_are_kept_too() {
    let dir = repo("stopped");
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
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
    let run = |ed: &mut crate::editor::Editor, n: u64, cmd: &str| {
        let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": "run",
            "arguments": {"command": cmd, "outside_sandbox": true, "reason": "a test of its own files"}}});
        assert!(crate::tools::mcp(ed, generation, &format!("r{n}"), &msg).is_none());
        pump(ed, &|ed| !ed.offers.is_empty(), "its card");
        ed.offer_accept();
    };
    let running = |ed: &crate::editor::Editor| {
        ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running)
    };
    // Stopped by the user after it deleted a file
    run(&mut ed, 1, "rm src/gone.txt && sleep 30");
    pump(&mut ed, &|ed| running(ed) && !dir.join("src/gone.txt").exists(), "it deleted it");
    ed.task_close();
    pump(&mut ed, &|ed| !ed.session.checkpoints.turn.is_empty(), "what it changed, recorded");
    let t = &ed.session.checkpoints.turn[0];
    assert!(t.path.ends_with("src/gone.txt") && t.gone && t.unseen, "{t:?}");
    // Its turn over while it runs: a checkpoint of its own
    run(&mut ed, 2, "sleep 0.4 && printf 'late\\n' > src/late.txt");
    pump(&mut ed, &running, "it runs");
    ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
    pump(&mut ed, &|ed| !ed.session.checkpoints.list.is_empty(), "a checkpoint of its own");
    let cp = &ed.session.checkpoints.list[0];
    assert!(cp.question.starts_with("ran `sleep 0.4") && !cp.seen, "{cp:?}");
    assert!(cp.files.len() == 1 && cp.files[0].path.ends_with("src/late.txt"), "{cp:?}");
    assert_eq!(ed.session.checkpoints.turn.len(), 1, "the turn in flight keeps only its own");
    let row = ed.session.chat.as_ref().unwrap().msgs.last().unwrap().text.clone();
    assert!(row.contains("late.txt after the turn"), "{row}");
    std::fs::remove_dir_all(&dir).ok();
}
