//! The commit's tree, written apart. Fixtures are git repositories in the test's own temp folder (never tarae's),
//! made with the user's and system git config left out.

use super::*;

/// The test's own folder: `p/` a repository with one commit (`a.txt` = `committed`), then `a.txt` changed and
/// `new.txt` made in its working tree — and the base folder beside it.
fn place(name: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("tarae-basetree-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p/src")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let p = dir.join("p");
    std::fs::write(p.join("a.txt"), "committed\n").unwrap();
    std::fs::write(p.join("src/b.txt"), "b\n").unwrap();
    git(&p, &["init", "-q"]);
    git(&p, &["add", "-A"]);
    git(&p, &["commit", "-q", "-m", "one"]);
    std::fs::write(p.join("a.txt"), "working\n").unwrap();
    std::fs::write(p.join("new.txt"), "new\n").unwrap();
    (dir, p)
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The commit's tree as committed — not the working tree's changes, nor its untracked files — and the checkout
/// untouched: its working tree and its index as they were. Written again, what was there before goes.
#[test]
fn the_commits_tree_is_written_apart_and_the_checkout_is_untouched() {
    let (dir, p) = place("tree");
    let status = git(&p, &["status", "--porcelain"]);
    let b = Base { rev: "HEAD".into(), tree: dir.join("base/k/tree"), ..Default::default() };
    let (sha, claim) = export(&p, &b).unwrap();
    drop(claim);
    assert_eq!(sha, git(&p, &["rev-parse", "HEAD"]).trim());
    assert_eq!(std::fs::read_to_string(b.tree.join("a.txt")).unwrap(), "committed\n");
    assert_eq!(std::fs::read_to_string(b.tree.join("src/b.txt")).unwrap(), "b\n");
    assert!(!b.tree.join("new.txt").exists(), "untracked: not the commit's");
    assert!(!b.tree.join(".git").exists());
    assert_eq!(git(&p, &["status", "--porcelain"]), status, "the checkout as it was");
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "working\n");
    // Again: fresh — a file a run left there is gone, the index of its own too
    std::fs::write(b.tree.join("left.txt"), "").unwrap();
    drop(export(&p, &b).unwrap());
    assert!(!b.tree.join("left.txt").exists());
    assert!(!dir.join("base/k/tree.index").exists());
    assert_eq!(Base { sha: Some(sha.clone()), ..b.clone() }.said(), format!("HEAD ({})", &sha[..7]));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Review of #212: a tree is one run's at a time, across sessions — writing it waits for another run's claim and
/// says so if it isn't let go; the last run's removal holds the claim until it's done, so a run asked meanwhile
/// writes its tree after the removal, never under it. A claim is let go when dropped, however its run ended.
#[test]
fn a_tree_is_one_runs_at_a_time() {
    let (dir, p) = place("claim");
    let b = Base { rev: "HEAD".into(), tree: dir.join("base/k/tree"), ..Default::default() };
    let (_, first) = export(&p, &b).unwrap();
    // Another run's still holds it: refused once the wait is over
    let e = export_waiting(&p, &b, Duration::from_millis(50)).unwrap_err();
    assert!(e.contains("another run on this project's commit tree is under way"), "{e}");
    assert!(b.tree.join("a.txt").exists(), "the first run's tree untouched");
    // Its removal under its claim, a run asked meanwhile: written after the removal, there whole
    let (root, next) = (p.clone(), b.clone());
    let waiting = std::thread::spawn(move || export_waiting(&root, &next, Duration::from_secs(10)));
    remove(&b.tree).unwrap();
    drop(first);
    let (_, second) = waiting.join().unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(b.tree.join("a.txt")).unwrap(), "committed\n");
    // Dropped (the run over, or never started): free
    drop(second);
    assert!(Claim::take(&b.tree, Duration::ZERO).is_ok());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Where the tree goes, a link is removed as a link — what it points to stays as it was; a link inside the last
/// tree likewise. A rev that isn't a commit, or looks like an option, writes nothing.
#[test]
fn nothing_is_written_through_a_link_nor_for_a_wrong_rev() {
    let (dir, p) = place("links");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("marker"), "kept").unwrap();
    let tree = dir.join("base/k/tree");
    std::fs::create_dir_all(tree.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &tree).unwrap();
    let b = Base { rev: "HEAD".into(), tree: tree.clone(), ..Default::default() };
    drop(export(&p, &b).unwrap());
    assert!(!std::fs::symlink_metadata(&tree).unwrap().file_type().is_symlink(), "a folder of its own now");
    assert_eq!(std::fs::read_to_string(elsewhere.join("marker")).unwrap(), "kept");
    assert!(!elsewhere.join("a.txt").exists(), "nothing written through the link");
    // A link a run left inside: removed with the tree, not gone through
    std::os::unix::fs::symlink(&elsewhere, tree.join("out")).unwrap();
    remove(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(std::fs::read_to_string(elsewhere.join("marker")).unwrap(), "kept");
    for rev in ["no-such", "-x", "a b", ""] {
        let e = export(&p, &Base { rev: rev.into(), ..b.clone() }).unwrap_err();
        assert!(e.starts_with(&format!("base {rev}:")), "{e}");
        assert!(!tree.exists(), "{rev}: nothing left");
    }
    assert!(rev_ok("HEAD~2") && rev_ok("main^") && rev_ok("be2fc5dd") && !rev_ok("--all"));
    std::fs::remove_dir_all(&dir).unwrap();
}
