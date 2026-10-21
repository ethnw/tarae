use super::*;

/// git in `dir` as a test sets up its repository (no launcher: what's set up is what the launcher is asked about).
fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(["-c", "protocol.file.allow=always", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repository's folders as given, nothing of its own found (no git run).
fn plain(git_dir: PathBuf, work_tree: Option<PathBuf>, dir: PathBuf) -> Repo {
    Repo {
        common_dir: git_dir.clone(),
        git_dir,
        work_tree,
        dir,
        local: Arc::default(),
        held: None,
        asks: false,
        stamps: Vec::new(),
        racy: false,
        subs: Vec::new(),
        dormant: Vec::new(),
    }
}

/// Past `RACY` since the test's last change: a resolve now is kept.
fn settled() {
    std::thread::sleep(RACY + std::time::Duration::from_millis(100));
}

/// A fresh folder of this test's own in the temp folder, real path.
fn temp(name: &str) -> PathBuf {
    crate::scratch::Scratch::new(&format!("gitrepo-{name}")).into_path()
}

/// A repository with one commit of `files` (path, text) in `dir`.
fn repository(dir: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    git_in(dir, &["init", "-q"]);
    git_in(dir, &["add", "-A"]);
    git_in(dir, &["commit", "-qm", "base"]);
}

/// The tripwire's question: this crate's folders (a subfolder too, and one that isn't there yet) are
/// this repository; a repository of a test's own, even one made under `target/`, is not.
#[test]
fn tests_know_this_repository() {
    let own = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(in_this_repository(own));
    assert!(in_this_repository(&own.join("src")));
    assert!(in_this_repository(&own.join("target/not-there-yet")));
    let dir = own.join(format!("target/tarae-own-repo-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let ok = Command::new("git").args(["init", "-q"]).current_dir(&dir).output().unwrap();
    assert!(ok.status.success());
    assert!(!in_this_repository(&dir), "a repository of its own");
    assert!(!in_this_repository(&std::env::temp_dir()));
    std::fs::remove_dir_all(&dir).ok();
}

/// From a root in a subfolder: the repository's folders, the top as the work tree, git run in the root (its
/// `ls-files` of that folder) — and a file's git from the top, its path from there.
#[test]
fn a_root_resolves_once_and_runs_in_itself() {
    let dir = temp("root");
    repository(&dir, &[("pkg/a.txt", "a\n"), ("b.txt", "b\n")]);
    settled();
    let root = dir.join("pkg");
    let r = repo(&root).unwrap();
    assert_eq!(
        (&r.git_dir, &r.common_dir, &r.work_tree, &r.dir),
        (&dir.join(".git"), &dir.join(".git"), &Some(dir.clone()), &root)
    );
    assert!(r.held.is_none() && !r.asks, "nothing of its own runs commands");
    assert!(Arc::ptr_eq(&r, &repo(&root).unwrap()), "kept");
    assert_eq!(git(&root, &["ls-files"], None).unwrap(), "a.txt\n");
    let (at, rel) = for_file(&root, &dir.join("b.txt")).unwrap();
    assert_eq!(
        (at.dir.as_path(), rel.as_str()),
        (dir.as_path(), "b.txt"),
        "a file outside the root, in the tree"
    );
    assert_eq!(at.run(&["show", &format!(":{rel}")], None).unwrap(), "b\n");
    assert!(for_file(&root, &std::env::temp_dir().join("elsewhere.txt")).is_none());
    std::fs::remove_dir_all(&dir).ok();
}

/// Kept by the root's real path: two spellings of one folder (a link to it) are one entry, one resolving.
#[test]
fn a_root_is_kept_by_its_real_path() {
    let dir = temp("spelling");
    let real = dir.join("r");
    std::fs::create_dir_all(&real).unwrap();
    repository(&real, &[("a.txt", "a\n")]);
    std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
    settled();
    let a = repo(&real).unwrap();
    assert!(Arc::ptr_eq(&a, &repo(&dir.join("link")).unwrap()), "one entry for both spellings");
    assert!(cached(&real).is_some_and(|c| Arc::ptr_eq(&c, &a)));
    std::fs::remove_dir_all(&dir).ok();
}

/// A folder first resolved inside an outer repository, given a repository of its own since: resolved again
/// (its own `.git` isn't the one kept).
#[test]
fn a_root_with_a_git_of_its_own_since_is_resolved_again() {
    let dir = temp("inner");
    repository(&dir, &[("inner/a.txt", "a\n")]);
    let inner = dir.join("inner");
    assert_eq!(repo(&inner).unwrap().work_tree.as_deref(), Some(dir.as_path()));
    git_in(&inner, &["init", "-q"]);
    settled();
    assert_eq!(repo(&inner).unwrap().work_tree.as_deref(), Some(inner.as_path()), "its own now");
    assert_eq!(cached(&inner).map(|r| r.git_dir.clone()), Some(inner.join(".git")));
    std::fs::remove_dir_all(&dir).ok();
}

/// At most `cap` roots kept; the least lately used goes first — a root asked again stays.
#[test]
fn the_kept_roots_are_capped_least_lately_used_first() {
    let dir = temp("cap");
    let root = |i: usize| dir.join(format!("d{i}"));
    let mut kept = Kept { cap: 3, entries: Vec::new(), epoch_now: || 0 };
    for i in 0..5 {
        std::fs::create_dir_all(root(i).join(".git")).unwrap();
        let r = plain(root(i).join(".git"), Some(root(i)), root(i));
        kept.put(root(i), Arc::new(r), 0);
        if i == 2 {
            assert!(kept.get(&root(0)).is_some(), "0 used again: the newest");
        }
    }
    let left: Vec<PathBuf> = kept.entries.iter().map(|(k, ..)| k.clone()).collect();
    assert_eq!(left, [root(0), root(3), root(4)]);
    assert_eq!(KEPT_MAX, 64);
    std::fs::remove_dir_all(&dir).ok();
}

/// Not a repository: an error, and not kept — a `git init` there later is seen.
#[test]
fn no_repository_is_said_and_not_kept() {
    let dir = temp("none");
    let e = repo(&dir).unwrap_err();
    assert!(e.contains("not a git repository"), "{e}");
    git_in(&dir, &["init", "-q"]);
    assert_eq!(repo(&dir).unwrap().work_tree.as_deref(), Some(dir.as_path()));
    std::fs::remove_dir_all(&dir).ok();
}

/// A root that is a bare-looking folder (`HEAD`, `objects/`, `refs/` — what files in a project can be) is
/// never taken for a repository: `safe.bareRepository=explicit`.
#[test]
fn a_bare_looking_folder_is_no_repository() {
    let dir = temp("bare");
    std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(dir.join("objects")).unwrap();
    std::fs::create_dir_all(dir.join("refs")).unwrap();
    std::fs::write(dir.join("config"), "[core]\n\tbare = true\n").unwrap();
    let e = repo(&dir).unwrap_err();
    assert!(e.contains("bare repository"), "{e}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A linked checkout: its own git folder, the common one, and its own top; the common one named alone runs
/// what outlives the checkout (`worktree list`).
#[test]
fn a_linked_checkout_has_its_own_git_folder() {
    let dir = temp("linked");
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    repository(&main, &[("a.txt", "a\n")]);
    git_in(&main, &["worktree", "add", "-q", "-b", "side", "../side"]);
    let side = dir.join("side");
    let r = repo(&side).unwrap();
    assert_eq!(r.git_dir, main.join(".git/worktrees/side"));
    assert_eq!(r.common_dir, main.join(".git"));
    assert_eq!(r.work_tree.as_deref(), Some(side.as_path()));
    assert_eq!(git(&side, &["symbolic-ref", "--short", "HEAD"], None).unwrap(), "side\n");
    let list = Repo::git_folder(&r.common_dir).run(&["worktree", "list", "--porcelain"], None).unwrap();
    assert_eq!(list.lines().filter(|l| l.starts_with("worktree ")).count(), 2, "{list}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A file in no session's repository (`for_file_in`): its own repository, found from its folder, when no run
/// writes there — but not one whose git folder is where runs write (a linked checkout outside of a repository
/// inside), nor one inside a write place (told: something there passes for a repository).
#[test]
fn a_file_outside_the_sessions_is_read_only_away_from_writes() {
    let dir = temp("outside");
    let (w, plain, linked) = (dir.join("w"), dir.join("plain"), dir.join("linked"));
    std::fs::create_dir_all(w.join("main")).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    repository(&plain, &[("a.txt", "plain\n")]);
    repository(&w.join("main"), &[("a.txt", "main\n")]);
    git_in(&w.join("main"), &["worktree", "add", "-q", "-b", "side", linked.to_str().unwrap()]);
    let writes = vec![w.clone()];
    let places = Places { roots: Vec::new(), writes: Arc::new(move || Some(writes.clone())) };
    let (r, rel) = for_file_in(&places, &plain.join("a.txt")).unwrap();
    assert_eq!((r.git_dir.clone(), rel.as_str()), (plain.join(".git"), "a.txt"));
    assert_eq!(for_file_in(&places, &linked.join("a.txt")).err(), Some(NoGit::Writable { looks: true }));
    assert_eq!(for_file_in(&places, &w.join("main/a.txt")).err(), Some(NoGit::Writable { looks: true }));
    std::fs::write(w.join("loose.txt"), "x\n").unwrap();
    assert_eq!(for_file_in(&places, &w.join("loose.txt")).err(), Some(NoGit::Writable { looks: false }));
    // Runs not sandboxed: nothing held back — found from the file's folder, wherever (a bare look-alike still not)
    let anywhere = Places { roots: Vec::new(), writes: Arc::new(|| None) };
    let (r, _) = for_file_in(&anywhere, &w.join("main/a.txt")).unwrap();
    assert_eq!(r.git_dir, w.join("main/.git"));
    let bare = w.join("bare");
    for d in ["objects", "refs"] {
        std::fs::create_dir_all(bare.join(d)).unwrap();
    }
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(bare.join("x.txt"), "x\n").unwrap();
    assert_eq!(for_file_in(&anywhere, &bare.join("x.txt")).err(), Some(NoGit::None));
    std::fs::remove_dir_all(&dir).ok();
}

/// The shell line the rebase runs: both folders given, quoted, the guard on, no fetch.
#[test]
fn the_shell_line_names_the_repository() {
    let mut r = plain("/r/it's/.git".into(), Some("/r/it's".into()), "/r/it's".into());
    let start = r"git --git-dir '/r/it'\''s/.git' --work-tree '/r/it'\''s' -c safe.bareRepository=explicit";
    let no_fetch = ("GIT_NO_LAZY_FETCH".to_string(), "1".to_string());
    assert_eq!(r.shell_line(), Line { text: start.into(), env: vec![no_fetch.clone()] });
    // (what's held goes in as on every run: each value in a variable of its own, named by `--config-env` — a
    // key that holds a `=` whole — the values only in the environment, the network's held too; no flags)
    let config = vec![
        ("merge.m=x.driver".to_string(), "git merge-file %A".to_string()),
        ("credential.helper".to_string(), "store --file /x/tarae-fake-token".to_string()),
        ("filter.f.clean".to_string(), "it's".to_string()),
    ];
    let env = vec![("GIT_PROXY_COMMAND", "false")];
    let flags = vec![("--no-textconv", &["blame"][..])];
    r.held = Some(Arc::new(Hold { config, env, flags, ..Hold::default() }));
    let line = r.shell_line();
    assert_eq!(
        line.text,
        format!(
            "{start} '--config-env=merge.m=x.driver=TARAE_GIT_HELD_0' '--config-env=credential.helper=TARAE_GIT_HELD_1' '--config-env=filter.f.clean=TARAE_GIT_HELD_2'"
        )
    );
    let env = [
        ("GIT_NO_LAZY_FETCH", "1"),
        ("TARAE_GIT_HELD_0", "git merge-file %A"),
        ("TARAE_GIT_HELD_1", "store --file /x/tarae-fake-token"),
        ("TARAE_GIT_HELD_2", "it's"),
        ("GIT_PROXY_COMMAND", "false"),
    ];
    assert_eq!(line.env, env.map(|(k, v)| (k.to_string(), v.to_string())));
}

/// A file in a submodule: its own repository, from the git folder the session's records (`modules/…`), its
/// path from the submodule's top — the gutter's base its index's. A folder with a `.git` that the index has
/// no gitlink for, or a gitlink whose `.git` names a folder outside `modules/`, is the session's repository's
/// (nothing taken from it).
#[test]
fn a_submodule_file_takes_the_submodules_recorded_repository() {
    let dir = temp("sub");
    let (lib, top) = (dir.join("lib"), dir.join("top"));
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(&top).unwrap();
    repository(&lib, &[("src/x.txt", "lib's\n")]);
    repository(&top, &[("a.txt", "top's\n")]);
    git_in(&top, &["submodule", "add", "-q", lib.to_str().unwrap(), "vendor/lib"]);
    git_in(&top, &["commit", "-qm", "lib"]);
    let file = top.join("vendor/lib/src/x.txt");
    std::fs::write(&file, "lib's\nchanged\n").unwrap();
    let (r, rel) = for_file(&top, &file).unwrap();
    assert_eq!(r.git_dir, top.join(".git/modules/vendor/lib"));
    assert_eq!(r.work_tree.as_deref(), Some(top.join("vendor/lib").as_path()));
    assert_eq!(rel, "src/x.txt");
    assert_eq!(crate::git::index_blob(&Places::only(&top), &file).unwrap().as_deref(), Some("lib's\n"));

    // Not a gitlink: a folder of the session's repository with a `.git` naming the module's folder
    std::fs::create_dir_all(top.join("other")).unwrap();
    std::fs::write(top.join("other/y.txt"), "y\n").unwrap();
    git_in(&top, &["add", "other/y.txt"]);
    std::fs::write(top.join("other/.git"), "gitdir: ../.git/modules/vendor/lib\n").unwrap();
    let (r, rel) = for_file(&top, &top.join("other/y.txt")).unwrap();
    assert_eq!((r.git_dir.clone(), rel.as_str()), (top.join(".git"), "other/y.txt"));
    assert_eq!(
        crate::git::index_blob(&Places::only(&top), &top.join("other/y.txt")).unwrap().as_deref(),
        Some("y\n")
    );

    // A gitlink whose `.git` names a git folder outside `modules/`: not followed
    std::fs::write(top.join("vendor/lib/.git"), format!("gitdir: {}\n", lib.join(".git").display())).unwrap();
    let (r, rel) = for_file(&top, &file).unwrap();
    assert_eq!((r.git_dir.clone(), rel.as_str()), (top.join(".git"), "vendor/lib/src/x.txt"));
    assert_eq!(
        crate::git::index_blob(&Places::only(&top), &file).unwrap(),
        None,
        "the session's index has only the gitlink"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A test's git reads no global ignore, not even from its default place, which git reads with no config at all
/// (`~/.config/git/ignore`): with a stand-in home whose ignore lists `.env`, `.env` isn't ignored.
#[test]
fn a_tests_git_reads_no_global_ignore_from_its_default_place() {
    let dir = temp("xdg");
    git_in(&dir, &["init", "-q"]);
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::write(home.join(".config/git/ignore"), ".env\n").unwrap();
    let out = base(&dir).env("HOME", &home).args(["check-ignore", "-q", ".env"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "not ignored: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_dir_all(&dir).ok();
}
