use super::*;
use crate::refactor::Whose;
use crate::scratch::Scratch;

/// The test's own git in its own folder: none of the user's settings, no hooks.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, files: &[(&str, Option<&str>)]) {
    for (p, t) in files {
        let f = dir.join(p);
        match t {
            Some(t) => {
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, t).unwrap();
            }
            None => std::fs::remove_file(f).unwrap(),
        }
    }
}

/// A project in the test's own temp folder — a git repository of its own — with `files` committed.
fn project(name: &str, files: &[(&str, &str)]) -> Scratch {
    let s = Scratch::new(&format!("apply-{name}"));
    write(s.path(), &files.iter().map(|(p, t)| (*p, Some(*t))).collect::<Vec<_>>());
    git(s.path(), &["init", "-q", "-b", "main"]);
    git(s.path(), &["add", "-A"]);
    git(s.path(), &["commit", "-q", "-m", "base"]);
    s
}

/// The patch that takes the project's HEAD to `files` (None: deleted) — `git diff`'s, the files put back.
fn diff_of(dir: &Path, files: &[(&str, Option<&str>)]) -> String {
    write(dir, files);
    git(dir, &["add", "-A"]);
    let d = git(dir, &["diff", "--cached", "HEAD"]);
    git(dir, &["reset", "-q", "--hard", "HEAD"]);
    d
}

/// `text` as `x.patch` in a folder of its own, outside the project (the other tree's).
fn patch_file(text: &str) -> (Scratch, PathBuf) {
    let s = Scratch::new("apply-patches");
    let p = s.join("x.patch");
    std::fs::write(&p, text).unwrap();
    (s, p)
}

fn editor(dir: &Path) -> Editor {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.to_path_buf();
    ed
}

/// Events until `done` (the patch is worked out and written on workers).
fn pump(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(ed) {
        assert!(std::time::Instant::now() < end, "{what} — said: {}", said(ed));
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// Everything said so far.
fn said(ed: &Editor) -> String {
    ed.messages.iter().map(|(_, _, t)| t.as_str()).collect::<Vec<_>>().join("\n")
}

/// The user's own change set's card, if up: its id, its text, its warning lines, whether a move or delete is
/// among them.
fn card(ed: &Editor) -> Option<(u64, String, usize, bool)> {
    ed.offers.iter().find_map(|o| match o.what {
        crate::offer::What::ChangeSet { whose: Whose::Yours, ops } => {
            Some((o.id, o.question.clone(), o.warnings.len(), ops))
        }
        _ => None,
    })
}

/// `:session-apply <args>`, then events until its card is up or it's refused (Err: what it said).
fn apply(ed: &mut Editor, args: &str) -> Result<(u64, String, usize, bool), String> {
    let before = ed.messages.len();
    crate::typed::execute(ed, &format!("session-apply {args}"))?;
    let refused = |ed: &Editor| {
        ed.messages[before..]
            .iter()
            .find(|(_, k, _)| *k == crate::editor::ToastKind::Error)
            .map(|m| m.2.clone())
    };
    pump(ed, "the card, or why not", |ed| card(ed).is_some() || refused(ed).is_some());
    card(ed).ok_or_else(|| refused(ed).unwrap())
}

/// `y` on the card, then events until every file is done.
fn accept(ed: &mut Editor, id: u64) {
    crate::refactor::accept_all(ed, id, Whose::Yours);
    pump(ed, "the files written", |ed| ed.session.applying.is_none());
}

/// `n` on the card.
fn decline(ed: &mut Editor) {
    ed.offers.clear();
    crate::refactor::decline_all(ed, Whose::Yours);
    pump(ed, "declined", |ed| ed.session.applying.is_none());
}

fn read(dir: &Path, p: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(p)).ok()
}

/// A patch made in another tree lands on its card — what was read and the base said — and `y` writes every
/// file; the whole is one step of the user's own that `space u` takes back. (The project, its git and the
/// patch are the test's own, in its temp folders.)
#[test]
fn a_clean_patch_lands_on_its_card_and_is_one_step_back() {
    let s = project("clean", &[("src/a.rs", "fn a() {}\nfn b() {}\n"), ("b.txt", "one\ntwo\n")]);
    let dir = s.path();
    let after = [("src/a.rs", Some("fn a() {}\nfn bee() {}\n")), ("b.txt", Some("one\nTWO\n"))];
    let (_p, patch) = patch_file(&diff_of(dir, &after));
    let mut ed = editor(dir);
    let (id, q, warned, ops) = apply(&mut ed, &patch.display().to_string()).unwrap();
    assert!(q.starts_with("apply x.patch\n\n2 changes in 2 files:"), "{q}");
    assert!(q.contains("src/a.rs") && q.contains("b.txt"), "{q}");
    assert!(q.contains("read ") && q.contains("x.patch · ") && q.contains("· 2 files"), "what was read: {q}");
    assert!(q.contains("merged three-way from each file's base"), "{q}");
    assert_eq!((warned, ops), (0, false));
    assert_eq!(read(dir, "b.txt").as_deref(), Some("one\ntwo\n"), "nothing written before y");
    accept(&mut ed, id);
    assert_eq!(read(dir, "src/a.rs").as_deref(), Some("fn a() {}\nfn bee() {}\n"));
    assert_eq!(read(dir, "b.txt").as_deref(), Some("one\nTWO\n"));
    assert!(said(&ed).contains("apply x.patch — 2 files changed"), "{}", said(&ed));
    // One step of its own, back with `space u`
    let last = ed.session.checkpoints.list.last().map(|c| (c.question.clone(), c.files.len()));
    assert_eq!(last, Some(("apply x.patch".into(), 2)));
    crate::checkpoint::step(&mut ed, true).unwrap();
    pump(&mut ed, "taken back", |_| read(dir, "b.txt").as_deref() == Some("one\ntwo\n"));
    assert_eq!(read(dir, "src/a.rs").as_deref(), Some("fn a() {}\nfn b() {}\n"));
    // One patch at a time
    let (_id, ..) = apply(&mut ed, &patch.display().to_string()).unwrap();
    let again = crate::typed::execute(&mut ed, &format!("session-apply {}", patch.display()));
    assert!(again.unwrap_err().contains("a patch is landing here already"));
}

/// The session's file moved on since the patch's base (lines added above): from its base the patch merges
/// cleanly; with no base, its hunk goes on the one place its lines are, said; where they're in two places
/// and neither is where it says, it's refused, naming them — never picked.
#[test]
fn context_that_moved_merges_from_the_base_or_is_found_once_or_refused() {
    let s = project("moved", &[("a.rs", "x\ny\nz\n")]);
    let dir = s.path();
    let text = diff_of(dir, &[("a.rs", Some("x\nY\nz\n"))]);
    write(dir, &[("a.rs", Some("top1\ntop2\ntop3\nx\ny\nz\n"))]);
    let mut ed = editor(dir);
    // From its base (the index line): merged, nothing more said
    let (_p, patch) = patch_file(&text);
    let (id, q, warned, _) = apply(&mut ed, &patch.display().to_string()).unwrap();
    assert!(!q.contains("lower") && warned == 0, "{q}");
    accept(&mut ed, id);
    assert_eq!(read(dir, "a.rs").as_deref(), Some("top1\ntop2\ntop3\nx\nY\nz\n"));
    // No base: found 3 lines lower, said
    write(dir, &[("a.rs", Some("top1\ntop2\ntop3\nx\ny\nz\n"))]);
    let bare: String = text.split_inclusive('\n').filter(|l| !l.starts_with("index ")).collect();
    let (_q, bare_patch) = patch_file(&bare);
    let (id, q, ..) = apply(&mut ed, &bare_patch.display().to_string()).unwrap();
    assert!(q.contains("no base named") && q.contains("a.rs: hunk 1 laid on 3 lines lower"), "{q}");
    accept(&mut ed, id);
    assert_eq!(read(dir, "a.rs").as_deref(), Some("top1\ntop2\ntop3\nx\nY\nz\n"));
    // Its lines twice, neither where it says: refused, nothing changed
    write(dir, &[("a.rs", Some("q\nx\ny\nz\nq\nx\ny\nz\n"))]);
    let e = apply(&mut ed, &bare_patch.display().to_string()).unwrap_err();
    assert!(
        e.contains("a.rs: it doesn't lay on — hunk 1") && e.contains("at lines 2, 6 — not one place"),
        "{e}"
    );
    assert!(e.contains("nothing changed"), "{e}");
    assert_eq!(read(dir, "a.rs").as_deref(), Some("q\nx\ny\nz\nq\nx\ny\nz\n"));
    // A base named that doesn't hold the file, or isn't a commit
    let e = apply(&mut ed, &format!("--base nosuch {}", patch.display())).unwrap_err();
    assert!(e.contains("there's no commit nosuch here"), "{e}");
    assert!(
        crate::typed::execute(&mut ed, "session-apply --base -x p").unwrap_err().contains("isn't a commit")
    );
}

/// Both changed the same line: the file gets a diff3 block (`conflict.rs` finds it), and the card says so in
/// the warning colour before anything is written.
#[test]
fn a_conflict_lands_as_markers_said_on_the_card() {
    let s = project("conflict", &[("a.rs", "1\n2\n3\n4\n5\n")]);
    let dir = s.path();
    let head = git(dir, &["rev-parse", "--short", "HEAD"]);
    let (_p, patch) = patch_file(&diff_of(dir, &[("a.rs", Some("1\n2\nTHREE\n4\n5\n"))]));
    write(dir, &[("a.rs", Some("1\n2\nthree\n4\n5\n"))]);
    let mut ed = editor(dir);
    let (id, q, warned, _) = apply(&mut ed, &format!("--base {} {}", head.trim(), patch.display())).unwrap();
    assert!(q.contains("▲ a.rs: 1 conflict — they land as conflict markers"), "{q}");
    assert!(q.contains(&format!("merged three-way from {}", head.trim())), "{q}");
    assert_eq!(warned, 1, "the conflict's line in the warning colour");
    accept(&mut ed, id);
    let a = read(dir, "a.rs").unwrap();
    let block = format!(
        "<<<<<<< this session\nthree\n||||||| base {}\n3\n=======\nTHREE\n>>>>>>> x.patch\n",
        head.trim()
    );
    assert_eq!(a, format!("1\n2\n{block}4\n5\n"));
    assert_eq!(crate::conflict::find(&Rope::from_str(&a)).len(), 1);
}

/// A new file and a deleted one are the card's (`new`, `deleted`; `r` refused — no buffer shows a delete), and
/// `y` makes and deletes them; a file changed here since the patch's text of it isn't deleted — refused.
#[test]
fn new_and_deleted_files_are_ops_on_the_card() {
    let s = project("ops", &[("gone.rs", "bye\n"), ("kept.rs", "k\n")]);
    let dir = s.path();
    let made = "pub fn attrs() {}\n";
    let text = diff_of(dir, &[("src/sandbox/supervisor/attrs.rs", Some(made)), ("gone.rs", None)]);
    let (_p, patch) = patch_file(&text);
    let mut ed = editor(dir);
    let (id, q, _, ops) = apply(&mut ed, &patch.display().to_string()).unwrap();
    assert!(ops, "a delete among them: {q}");
    let tags: Vec<&str> =
        q.lines().filter(|l| l.starts_with("  ")).map(|l| l.split_whitespace().last().unwrap()).collect();
    assert_eq!(tags, ["deleted", "new"], "{q}");
    crate::refactor::one_by_one(&mut ed, id, Whose::Yours);
    assert!(card(&ed).is_some(), "r refused: the card is still up");
    accept(&mut ed, id);
    assert_eq!(read(dir, "src/sandbox/supervisor/attrs.rs").as_deref(), Some(made));
    assert_eq!(read(dir, "gone.rs"), None);
    // Changed here since: not deleted
    git(dir, &["checkout", "-q", "--", "gone.rs"]);
    write(dir, &[("gone.rs", Some("bye\nmine\n"))]);
    std::fs::remove_file(dir.join("src/sandbox/supervisor/attrs.rs")).unwrap();
    let e = apply(&mut ed, &patch.display().to_string()).unwrap_err();
    assert!(e.contains("gone.rs: changed here since the patch's text of it — not deleted"), "{e}");
    assert!(dir.join("gone.rs").exists() && !dir.join("src/sandbox/supervisor/attrs.rs").exists());
}

/// A name outside the project — `..` past it, an absolute path, inside `.git` — refuses the whole patch: the
/// file inside it isn't changed either. (The absolute path is a stand-in in the test's own folder.)
#[test]
fn a_path_outside_refuses_the_whole_patch() {
    let s = project("outside", &[("in.txt", "a\n")]);
    let dir = s.path();
    let mut ed = editor(dir);
    let hunk = "@@ -1 +1 @@\n-a\n+b\n";
    let inside = format!("--- a/in.txt\n+++ b/in.txt\n{hunk}");
    let (other, _) = patch_file("");
    let stand_in = other.join("outside.txt");
    std::fs::write(&stand_in, "a\n").unwrap();
    let abs = stand_in.display().to_string();
    // (a link in the project to its `.git`: where a name really leads is what's judged)
    std::os::unix::fs::symlink(".git", dir.join("g")).unwrap();
    let config = read(dir, ".git/config");
    for (name, why) in [
        ("a/../escape.txt".to_string(), "is outside the project"),
        (abs.clone(), "is an absolute path"),
        ("a/.git/config".to_string(), "is inside .git"),
        ("a/g/config".to_string(), "g/config leads inside .git (.git/config)"),
    ] {
        let b = name.replacen("a/", "b/", 1);
        let (_p, patch) = patch_file(&format!("{inside}--- {name}\n+++ {b}\n{hunk}"));
        let e = apply(&mut ed, &patch.display().to_string()).unwrap_err();
        assert!(e.contains(why) && e.contains("nothing changed — 1 of 2 files"), "{name}: {e}");
    }
    assert_eq!(read(dir, "in.txt").as_deref(), Some("a\n"));
    assert_eq!(std::fs::read_to_string(&stand_in).unwrap(), "a\n");
    assert_eq!(read(dir, ".git/config"), config);
}

/// What was read is one row on the card whatever the path's length: `read`, then the path cut on the left so its
/// name, size and count stay (`read ~/…/its-patches/x.patch · 42 B · 1 file`) — never wrapped, at 120 columns nor in a narrow
/// card at 64; the whole path is `v`'s view's. (The patch is the test's own, deep in its temp folder.)
#[test]
fn what_was_read_is_one_row_that_keeps_its_end() {
    let s = project("read-row", &[("a.txt", "a\n")]);
    let dir = s.path();
    let deep = Scratch::new("apply-a-patch-from-a-rather-long-folder-name");
    let folder = deep.join("another-tree-of-the-same-repository/at-an-older-commit/its-patches");
    std::fs::create_dir_all(&folder).unwrap();
    let patch = folder.join("x.patch");
    let text = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\n";
    std::fs::write(&patch, text).unwrap();
    let end = format!("/its-patches/x.patch · {} B · 1 file", text.len());
    let mut ed = editor(dir);
    apply(&mut ed, &patch.display().to_string()).unwrap();
    let whole = crate::allow::tilde(&patch);
    for (cols, rows) in [(120u16, 32u16), (64, 30)] {
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, cols, rows).unwrap();
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(&bytes);
        let screen = p.screen().contents();
        let read: Vec<&str> =
            screen.lines().filter(|l| l.contains("read ") || l.trim_end().ends_with("read")).collect();
        assert_eq!(read.len(), 1, "{cols} columns: one row\n{screen}");
        // (inside its card's frame)
        let row = read[0].trim_matches([' ', '│']).trim();
        // (its start kept, cut at a folder — never mid-name)
        let cut = row.starts_with("read ~/…/") || row.starts_with("read /…/");
        assert!(cut && row.ends_with(&end), "{cols}: {row:?}\n{screen}");
        assert!(!screen.contains(&whole), "{cols}: cut, not whole\n{screen}");
    }
    // Whole in `v`'s view
    crate::refactor::view_all(&mut ed, Whose::Yours);
    pump(&mut ed, "the view", |ed| {
        ed.docs.iter().any(|d| d.text.to_string().contains(&format!("read {whole} · ")))
    });
}

/// `--base` and its revision apart by any space, a wide one too (U+3000, a no-break space) — never a panic.
#[test]
fn the_base_is_read_past_any_space() {
    assert_eq!(args_of("--base\u{3000}abc p.patch"), Ok((Some("abc".into()), "p.patch")));
    assert_eq!(args_of("--base\u{a0}abc\u{a0}p.patch"), Ok((Some("abc".into()), "p.patch")));
    assert_eq!(args_of("--base=abc p.patch"), Ok((Some("abc".into()), "p.patch")));
    assert_eq!(args_of("p.patch"), Ok((None, "p.patch")));
    assert_eq!(args_of("--base\u{3000}"), Err(USAGE.into()));
    assert_eq!(args_of("--base"), Err(USAGE.into()));
}

/// `diff -u x.txt.orig x.txt` and `diff -ru old/ new/` name one file twice: it's changed in place, not moved.
#[test]
fn a_plain_diff_changes_the_file_it_names() {
    let s = project("plain", &[("x.txt", "a\nb\n"), ("src/y.txt", "c\n")]);
    let dir = s.path();
    let mut ed = editor(dir);
    let text = "--- x.txt.orig\t2026-10-19 10:00:00\n+++ x.txt\t2026-10-19 10:01:00\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n\
                --- old/src/y.txt\n+++ new/src/y.txt\n@@ -1 +1 @@\n-c\n+C\n";
    let (_p, patch) = patch_file(text);
    let (id, q, _, ops) = apply(&mut ed, &patch.display().to_string()).unwrap();
    assert!(!ops && q.contains("no base named"), "no move among them: {q}");
    accept(&mut ed, id);
    assert_eq!(read(dir, "x.txt").as_deref(), Some("a\nB\n"));
    assert_eq!(read(dir, "src/y.txt").as_deref(), Some("C\n"));
    assert_eq!(read(dir, "x.txt.orig"), None);
}

/// `r`: each file in the in-buffer review, its `y` saving it (" · the patch") and joining the patch's one
/// `space u` step; a Claude turn ending mid-review leaves the review and the landing as they are.
#[test]
fn reviewed_one_by_one_each_file_joins_one_step_and_a_turns_end_leaves_it() {
    use crate::key::{Code, Key};
    let s = project("by-one", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let dir = s.path();
    let (_p, patch) = patch_file(&diff_of(dir, &[("a.txt", Some("A\n")), ("b.txt", Some("B\n"))]));
    let mut ed = editor(dir);
    let (id, _, _, ops) = apply(&mut ed, &patch.display().to_string()).unwrap();
    assert!(!ops);
    crate::refactor::one_by_one(&mut ed, id, Whose::Yours);
    for (f, after) in [("a.txt", "A\n"), ("b.txt", "B\n")] {
        pump(&mut ed, "its review", |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join(f).as_path()));
        crate::refactor::turn_over(&mut ed);
        assert!(ed.session.review.is_some() && ed.session.applying.is_some(), "{f}: still up");
        assert!(crate::llm::review_key(&mut ed, Key::plain(Code::Char('y'))));
        pump(&mut ed, "saved", |_| read(dir, f).as_deref() == Some(after));
    }
    pump(&mut ed, "done", |ed| ed.session.applying.is_none());
    assert!(said(&ed).contains("apply x.patch — 2 files changed"), "{}", said(&ed));
    let last = ed.session.checkpoints.list.last().map(|c| (c.question.clone(), c.files.len()));
    assert_eq!(last, Some(("apply x.patch".into(), 2)), "one step, both files");
    crate::checkpoint::step(&mut ed, true).unwrap();
    pump(&mut ed, "taken back", |_| read(dir, "a.txt").as_deref() == Some("a\n"));
    pump(&mut ed, "both", |_| read(dir, "b.txt").as_deref() == Some("b\n"));
}

/// A protected name's change is on the card, marked (its review warns, it's never unseen); deleting one is
/// refused. A binary change refuses the patch.
#[test]
fn a_protected_name_is_marked_and_never_deleted_and_a_binary_change_is_refused() {
    let s = project("protected", &[("Cargo.toml", "[package]\nname = \"a\"\n"), ("src/a.rs", "a\n")]);
    let dir = s.path();
    let mut ed = editor(dir);
    let text = diff_of(dir, &[("Cargo.toml", Some("[package]\nname = \"b\"\n")), ("src/a.rs", Some("b\n"))]);
    let (_p, patch) = patch_file(&text);
    apply(&mut ed, &patch.display().to_string()).unwrap();
    let protected: Vec<(String, bool)> =
        (ed.session.applying.as_ref().unwrap().review.iter()).map(|f| (f.rel.clone(), f.protected)).collect();
    assert_eq!(protected, [("Cargo.toml".into(), true), ("src/a.rs".into(), false)]);
    decline(&mut ed);
    assert_eq!(read(dir, "src/a.rs").as_deref(), Some("a\n"), "declined: as it was");
    let (_q, patch) = patch_file(&diff_of(dir, &[("Cargo.toml", None)]));
    let e = apply(&mut ed, &patch.display().to_string()).unwrap_err();
    assert!(e.contains("Cargo.toml: Cargo.toml is a build script, manifest or setting"), "{e}");
    assert!(dir.join("Cargo.toml").exists());
    let bin = "diff --git a/i.png b/i.png\nindex 1111111..2222222 100644\nBinary files a/i.png and b/i.png differ\n";
    let (_r, patch) = patch_file(bin);
    let e = apply(&mut ed, &patch.display().to_string()).unwrap_err();
    assert!(e.contains("i.png: a binary change") && e.contains("nothing changed"), "{e}");
}
