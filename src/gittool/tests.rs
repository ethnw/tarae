use super::*;

use std::path::PathBuf;
use std::process::Command;

/// git in `dir` as a test sets up its repository (its own name, no signing, no hooks).
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(["-c", "init.defaultBranch=main", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

/// `git_cmd`, which must succeed.
fn git_in(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir, args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repository in the test's own temp folder: `files` committed, then each of `changed` written over.
fn repo(name: &str, files: &[(&str, &str)], changed: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tarae-gittool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    git_in(&dir, &["init", "-q"]);
    for (f, t) in files {
        std::fs::write(dir.join(f), t).unwrap();
    }
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-qm", "first"]);
    for (f, t) in changed {
        std::fs::write(dir.join(f), t).unwrap();
    }
    dir
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

fn git(dir: &Path, w: &[&str]) -> Result<String, String> {
    let p = parse(&words(w))?;
    run(dir, &crate::reads::Keys::none_opened(dir), &p, 0)
}

/// Only the reads, and only the options each takes (an allowlist): a subcommand that writes, git's own options
/// before it, and after it any option not listed — what writes a file, reads one anywhere, runs a program, goes
/// into submodules, renames files in the output — spelled out, abbreviated, with its value, or in a cluster.
/// The listed ones pass, a value apart or attached.
#[test]
fn only_reads_pass_and_only_the_options_listed() {
    for w in [
        &["commit", "-m", "x"][..],
        &["-c", "core.pager=x", "log"],
        &["--git-dir=/elsewhere", "log"],
        &["config", "--list"],
        &["log", "--output=/tmp/x"],
        &["log", "--outp", "/tmp/x"],
        &["diff", "--ext-diff"],
        &["show", "--textconv", "HEAD:a"],
        &["grep", "-nO", "x"],
        &["grep", "--open-files-in-pager=vi", "x"],
        &["diff", "-Oorder.txt"],
        &["diff", "--no-index", "/etc/hosts", "a"],
        &["blame", "--contents", "/etc/hosts", "a"],
        &["log", "--show-signature"],
        &["log", "--format=%H %G?"],
        &["log", "--format=%+G?"],
        &["log", "--pretty=%-GS"],
        &["log", "--format=% GK"],
        // (a format only by its name: no `%` string at all)
        &["log", "--format=%h %s"],
        &["log", "--pretty=format:%h"],
        &["log", "--format=tformat:%s"],
        // (blame's copy and move search: other files, a key file among them, named as a line's source)
        &["blame", "-C", "--", "a"],
        &["blame", "-M", "--", "a"],
        &["grep", "--recurse-submodules", "x"],
        // (what renames a file or a patch's lines in the output, or isn't listed at all)
        &["log", "-p", "--color=always"],
        &["log", "--sta"],
        &["log", "-L1,5:a.rs"],
        &["status", "-v"],
        &["log", "--format", "%H"],
        &["diff", "--color-words"],
        &["diff", "--no-prefix"],
        &["diff", "--src-prefix=x/"],
        &["diff", "--line-prefix=>"],
        &["log", "--output-indicator-new=>", "-p"],
        &["diff", "--word-diff=porcelain"],
        &["show", "--binary"],
        &["log", "-z"],
        &["grep", "--null", "x"],
        // (a file read from anywhere — its lines quoted back in git's error)
        &["grep", "-f", "/etc/hosts"],
        &["grep", "-nf", "/etc/hosts"],
        &["blame", "-S", "/etc/hosts", "a"],
        &["grep", "-e"],
        &[],
    ] {
        assert!(parse(&words(w)).is_err(), "{w:?}");
    }
    for w in [
        &["log", "--oneline", "-20", "--", "src/a.rs"][..],
        &["diff", "--stat", "HEAD~1"],
        &["log", "-S", "needle", "--before=2026-01-01", "--numstat"],
        &["grep", "-n", "-e", "x", "main"],
        &["show", "HEAD~1:src/a.rs"],
        &["status", "--short"],
        &["ls-tree", "-r", "HEAD"],
        &["blame", "-L", "1,5", "--", "a"],
        &["log", "-n5", "-U3", "-M50%", "--format=short", "-p"],
        &["log", "--pretty=fuller"],
        &["grep", "-nwi", "-C2", "-h", "--heading", "x"],
        // (after `--` only paths: a file named like an option is a file)
        &["log", "--", "--output"],
    ] {
        assert!(parse(&words(w)).is_ok(), "{w:?}: {:?}", parse(&words(w)));
    }
    // What each part is: grep's pattern, the revisions, the paths
    let p = parse(&words(&["grep", "-n", "fn main", "HEAD~1", "--", "src"])).unwrap();
    assert_eq!(
        (p.pattern.as_deref(), p.revs, p.paths),
        (Some("fn main"), words(&["HEAD~1"]), words(&["src"]))
    );
    assert_eq!(parse(&words(&["log", "-pn", "3"])).unwrap().opts, words(&["-pn", "3"]), "-n's value apart");
}

/// A diff, a textconv filter, a pager and gpg the user's config names never run from here: the reads come back
/// and the marker each would leave isn't there. (The repository's own commands are held besides — `gitrepo`.)
#[test]
fn no_external_diff_textconv_or_pager_runs() {
    let dir = repo(
        "programs",
        &[("a.rs", "fn a() {}\n"), (".gitattributes", "* diff=x\n")],
        &[("a.rs", "fn b() {}\n")],
    );
    let marker = dir.join("ran");
    let script = dir.join("..").join(format!("tarae-gittool-program-{}", std::process::id()));
    std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display())).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cfg = dir.join("..").join(format!("tarae-gittool-config-{}", std::process::id()));
    let s = script.display();
    std::fs::write(
        &cfg,
        format!("[diff]\n\texternal = {s}\n[diff \"x\"]\n\ttextconv = {s}\n[core]\n\tpager = {s}\n"),
    )
    .unwrap();
    crate::gitrepo::global_config_for(&dir, &cfg);
    for w in [
        &["diff"][..],
        &["diff", "HEAD"],
        &["log", "-p"],
        &["show", "HEAD"],
        &["show", "HEAD:a.rs"],
        &["blame", "--", "a.rs"],
        &["grep", "-n", "fn", "HEAD"],
    ] {
        let out = git(&dir, w).unwrap_or_else(|e| panic!("{w:?}: {e}"));
        assert!(out.contains("fn"), "{w:?}: {out}");
        assert!(!marker.exists(), "{w:?} ran the user's program");
    }
    for f in [script, cfg] {
        std::fs::remove_file(f).ok();
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A file named like a key stays out as `search` keeps it out — on git's side: named, refused before git runs;
/// otherwise never read (exclusion pathspecs), however the output would lay it out — each of the review's
/// bypasses of the output filters this replaced is a case here, and every case that leaks is named at once.
#[test]
fn a_file_named_like_a_key_stays_out() {
    let token = "tarae-fake-token-1234";
    let env = format!("K={token}\n");
    let dir = repo(
        "keys",
        &[("a.rs", "fn a() {}\n"), (".env", &env)],
        &[("a.rs", "fn b() {}\n"), (".env", "K=other\n")],
    );
    // Named: refused before git runs
    for w in [&["show", "HEAD:.env"][..], &["blame", "--", ".env"], &["log", "-p", "--", ".env"]] {
        let e = git(&dir, w).unwrap_err();
        assert!(e.contains("named like a key"), "{w:?}: {e}");
    }
    // Never read by git, however the output is laid out (each case that leaks is named)
    let leaked = std::cell::RefCell::new(Vec::new());
    let never = |case: &str, w: &[&str]| {
        let out = git(&dir, w).unwrap_or_else(|e| panic!("{case} {w:?}: {e}"));
        if out.contains(token) || out.contains("K=other") {
            leaked.borrow_mut().push(format!("{case} {w:?}"));
        }
        out
    };
    never("a patch", &["log", "-p"]);
    never("a commit", &["show", "HEAD"]);
    never("grep at a revision", &["grep", "-n", "K=", "HEAD"]);
    never("grep, no file names", &["grep", "-h", "K="]);
    never("grep, names on lines of their own, function lines", &["grep", "--heading", "-p", "K="]);
    never("grep, a glob that takes it", &["grep", "-e", "K=", "--", "*env*"]);
    // A search of every diff (`-S`, `-G`) asked again and again is a match oracle: it never searches it either
    for w in [&["log", "-G", "K=tarae", "--oneline"][..], &["log", "-S", "tarae-fake", "--oneline"]] {
        let out = git(&dir, w).unwrap_or_else(|e| panic!("{w:?}: {e}"));
        if out.contains("first") {
            leaked.borrow_mut().push(format!("a search of every diff: its commit found {w:?}"));
        }
    }
    let out = never("the worktree", &["diff"]);
    assert!(out.contains("+fn b() {}"), "the rest stays: {out}");
    // A path given before `--` isn't taken for a revision
    let e = git(&dir, &["log", "a.rs"]).unwrap_err();
    assert!(e.contains("paths go after --"), "{e}");
    // Its contents by id (what `ls-tree` shows), or a tag of it: refused, named nowhere as it is
    let id = |rev: &str| {
        let out = Command::new("git").args(["rev-parse", rev]).current_dir(&dir).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let (blob, other) = (id("HEAD:.env"), id("HEAD:a.rs"));
    git_in(&dir, &["tag", "keytag", &blob]);
    for w in
        [vec!["show", blob.as_str()], vec!["diff", other.as_str(), blob.as_str()], vec!["show", "keytag"]]
    {
        let e = git(&dir, &w).unwrap_err();
        assert!(e.contains("by its id"), "{w:?}: {e}");
    }
    // The user's own settings that color the output or rename its files: overridden — and the file still unread
    let cfg = dir.join("..").join(format!("tarae-gittool-keys-config-{}", std::process::id()));
    let renaming = "[color]\n\tui = always\n[diff]\n\tnoprefix = true\n\tmnemonicPrefix = true\n";
    std::fs::write(&cfg, renaming).unwrap();
    crate::gitrepo::global_config_for(&dir, &cfg);
    for (case, w) in [
        ("color.ui=always, noprefix, mnemonicPrefix: a patch", &["log", "-p"][..]),
        ("color.ui=always, noprefix, mnemonicPrefix: the worktree", &["diff"]),
        ("color.ui=always: grep", &["grep", "-n", "K="]),
    ] {
        let out = never(case, w);
        assert!(!out.contains('\x1b'), "{w:?}: no color — {out}");
    }
    assert!(leaked.borrow().is_empty(), "the key file's text came out:\n  {}", leaked.borrow().join("\n  "));
    std::fs::remove_file(cfg).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// A merge whose sides both changed the key file: its combined diff (`diff --cc .env`) never has it — git
/// doesn't read it.
#[test]
fn a_merges_combined_diff_never_has_the_key_file() {
    let dir = repo("merge", &[("a.rs", "fn a() {}\n"), (".env", "K=base\n")], &[]);
    git_in(&dir, &["checkout", "-qb", "side"]);
    std::fs::write(dir.join(".env"), "K=tarae-fake-side\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn side() {}\n").unwrap();
    git_in(&dir, &["commit", "-qam", "side"]);
    git_in(&dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join(".env"), "K=tarae-fake-main\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    git_in(&dir, &["commit", "-qam", "main"]);
    // (both sides changed both files: resolved by hand, a combined diff for each)
    // (it stops on the conflicts — after it began, which needs a committer: without one, as root in a container,
    // it refused and the commit below had one parent)
    let _ = git_cmd(&dir, &["merge", "-q", "side"]).output().unwrap();
    assert!(dir.join(".git/MERGE_HEAD").exists(), "the merge began");
    std::fs::write(dir.join(".env"), "K=tarae-fake-merged\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn merged() {}\n").unwrap();
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-qm", "merged"]);
    for w in [&["show", "HEAD"][..], &["log", "-p", "-n", "1"]] {
        let out = git(&dir, w).unwrap_or_else(|e| panic!("{w:?}: {e}"));
        assert!(!out.contains("tarae-fake"), "{w:?}: {out}");
    }
    let out = git(&dir, &["show", "HEAD"]).unwrap();
    assert!(out.contains("diff --cc a.rs") && out.contains("merged"), "the merge's own diff stays: {out}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Long output comes a page at a time: the lines asked for, how many there are, the offset of the next; past
/// the end it says so; a long line is cut; output git was stopped in says so.
#[test]
fn long_output_comes_a_page_at_a_time() {
    let text: String = (1..=PAGE + 5).map(|i| format!("line {i}\n")).collect();
    let first = page(&text, 0, false);
    assert!(first.starts_with(&format!("lines 1–{PAGE} of {}\nline 1\n", PAGE + 5)), "{first}");
    assert!(first.ends_with(&format!("… 5 more — offset: {PAGE} for the next ones\n")), "{first}");
    let next = page(&text, PAGE, false);
    assert!(next.contains(&format!("line {}\n", PAGE + 5)) && !next.contains("more — offset"), "{next}");
    assert_eq!(page(&text, PAGE + 9, false), format!("{} lines; none past offset {}", PAGE + 5, PAGE + 9));
    let long = "x".repeat(WIDTH + 10);
    assert_eq!(page(&long, 0, false), format!("{}…\n", "x".repeat(WIDTH)));
    assert!(page("a\n", 0, true).contains("stopped after 4 MB"));
    assert_eq!(page("", 0, false), "(no output)");
}

/// Past `CAP`, git is stopped: what was read comes back, said as cut, and the call doesn't wait for the rest.
#[test]
fn git_is_stopped_past_the_cap() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "yes 0123456789abcdef"]);
    let (out, _, ok, cut) = read_capped(&cmd).unwrap();
    assert!(ok && cut && out.len() == CAP);
}
