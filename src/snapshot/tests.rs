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
    let Found { files: c, skipped, left_out, ignored_gone, protected, head } = before.since().unwrap();
    assert!(skipped.is_empty() && left_out == 0, "{skipped:?} {left_out}");
    assert_eq!(head, None, "HEAD stayed");
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

/// A save's temp file (`disk::write_atomic`'s, there while a save is being written) isn't the run's: never in
/// the snapshot, so never a change of its — the run's own new file beside it is.
#[test]
fn a_saves_temp_file_isnt_the_runs() {
    let dir = repo("temp");
    let before = Before::take(&dir);
    std::fs::write(dir.join("src/.a.txt.tarae-123-0.tmp"), "half").unwrap();
    std::fs::write(dir.join(".untracked.txt.tarae-123-1.tmp"), "half").unwrap();
    std::fs::write(dir.join("src/made.txt"), "the run's\n").unwrap();
    let found = before.since().unwrap();
    let names: Vec<_> = found.files.iter().map(|c| c.path.clone()).collect();
    assert_eq!(names, [dir.join("src/made.txt")], "{found:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A save still being written as Claude's run ends (a slow disk): what it changed is taken once the save is
/// written — never halfway — and the save isn't the run's: the report and the turn name only what the run did.
/// (A harmless command: it waits for the save's temp file in the test's own repository, then makes a file.)
#[test]
fn a_runs_changes_wait_for_a_save_still_being_written() {
    let dir = repo("saving");
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    ed.config.llm.sandbox = false;
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.saver.slow = Duration::from_millis(1500);
    ed.open(&dir.join("src/a.txt")).unwrap();
    let doc = ed.doc().id;
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let generation = c.generation();
    let answered = || {
        crate::tools::ANSWERED
            .with(|a| a.borrow().iter().find(|(id, _)| *id == json!(1)).map(|(_, r)| r.clone()))
    };
    let pump = |ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str| {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(ed) {
            assert!(Instant::now() < end, "{what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    };
    let cmd =
        "while ! ls src/.a.txt.tarae-*.tmp >/dev/null 2>&1; do sleep 0.05; done; printf 'x\\n' > made.txt";
    let call = crate::tools::Call::of(generation, "r1", json!(1));
    let job = crate::tools::Job::Command(cmd.into(), dir.clone());
    crate::tools::guard_and_go(&mut ed, crate::tools::For::Claude(call), job, false, Default::default());
    pump(
        &mut ed,
        &|ed| ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running),
        "it runs",
    );
    let d = ed.docs.iter_mut().find(|d| d.id == doc).unwrap();
    d.apply(&crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(0, "saved\n")]));
    ed.save_in_background(doc, false, crate::saver::Then::Auto(" · idle")).unwrap();
    pump(&mut ed, &|_| answered().is_some(), "it ran");
    let saving = ed.docs.iter().find(|d| d.id == doc).unwrap().saving;
    assert_eq!(saving, 0, "its changes were taken once the save was written");
    assert_eq!(std::fs::read_to_string(dir.join("src/a.txt")).unwrap(), "saved\none\ntwo\n");
    let report = answered().unwrap().unwrap();
    // (the command names the temp file: the line of what changed is the one read)
    let changed = report.lines().find(|l| l.starts_with("It changed in the checkout: "));
    assert!(
        changed.is_some_and(|l| l.starts_with("It changed in the checkout: made.txt (new) —")),
        "the save isn't the run's: {report}"
    );
    let turn: Vec<_> = ed.session.checkpoints.turn.iter().map(|t| t.path.clone()).collect();
    assert_eq!(turn, [dir.join("made.txt")]);
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
    let found = paths(&before.since().unwrap().protected);
    assert_eq!(found, [".envrc", "src/.cargo/config.toml", "sub/Cargo.toml"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// The names of what a run made, as `Found.protected` has them (sorted).
fn paths(made: &[Made]) -> Vec<String> {
    made.iter().map(|m| m.path.clone()).collect()
}

/// G23: past `NEW_MAX` new files the tree keeps none of them — their names are looked at all the same (no
/// contents): a protected name and a link to a folder among them are found; one that was there, left out
/// before the run too, isn't the run's.
#[test]
fn past_new_max_the_new_names_are_still_looked_at() {
    let dir = repo("many-names");
    std::fs::create_dir_all(dir.join("gen/sub")).unwrap();
    for i in 0..=NEW_MAX {
        std::fs::write(dir.join(format!("gen/f{i}.txt")), "x\n").unwrap();
    }
    std::fs::write(dir.join("gen/Makefile"), "all:\n").unwrap();
    let before = Before::take(&dir);
    assert_eq!(before.tree.as_ref().unwrap().flagged, ["gen/Makefile"], "left out, named");
    std::fs::write(dir.join("gen/sub/Cargo.toml"), "[package]\n").unwrap();
    std::os::unix::fs::symlink("../src", dir.join("gen/up")).unwrap();
    let found = before.since().unwrap();
    assert!(found.left_out > NEW_MAX, "{}", found.left_out);
    assert_eq!(paths(&found.protected), ["gen/sub/Cargo.toml", "gen/up"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// G23: a new ignored folder (git lists it whole) is looked through by name: a protected name deep in it is
/// found, and a link to a folder; a new folder whose own name is protected is named itself; nothing below a
/// tool's output (`target/`, `node_modules/`), and not a file of another name.
#[test]
fn the_names_in_a_new_ignored_folder_are_looked_at() {
    let dir = repo("ignored-walk");
    std::fs::write(dir.join(".gitignore"), "build/\n.env.local\nout/\n.cargo/\n").unwrap();
    let before = Before::take(&dir);
    for (rel, text) in [
        ("out/deep/er/Makefile", "all:\n"),
        ("out/notes.txt", "x\n"),
        ("out/target/pkg/build.rs", "fn main() {}\n"),
        ("out/node_modules/p/Cargo.toml", "[package]\n"),
        (".cargo/config.toml", "[build]\n"),
    ] {
        std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
        std::fs::write(dir.join(rel), text).unwrap();
    }
    std::os::unix::fs::symlink("../src", dir.join("out/up")).unwrap();
    std::os::unix::fs::symlink("../../src", dir.join("out/target/up")).unwrap();
    let found = before.since().unwrap();
    assert_eq!(paths(&found.protected), [".cargo", "out/deep/er/Makefile", "out/up"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// The reviewer's finding: a made file is hashed within a cap — a sparse file far past it (a run's 100 GB
/// `build.rs` costs it no disk) isn't read: it's told by its size, inode and change time, a write tells it apart,
/// and it's never kept. A folder past its names likewise; within the caps, hashed as ever. (A sparse stand-in in
/// the test's temp folder, the caps passed in.)
#[test]
fn a_made_name_past_the_cap_isnt_read_and_isnt_kept() {
    use std::io::Write as _;
    let dir = repo("big-hash");
    let cap = (1024, 4);
    std::fs::File::create(dir.join("build.rs")).unwrap().set_len(64 << 20).unwrap();
    let hash = made_hash_within(&dir, "build.rs", cap);
    assert!(hash.starts_with(BIG), "{hash}");
    assert!(!Made { path: "build.rs".into(), hash: hash.clone() }.keepable());
    std::fs::OpenOptions::new().append(true).open(dir.join("build.rs")).unwrap().write_all(b"x").unwrap();
    assert_ne!(made_hash_within(&dir, "build.rs", cap), hash, "written since: told apart");
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    for i in 0..5 {
        std::fs::write(dir.join(format!(".cargo/f{i}")), "x\n").unwrap();
    }
    assert!(made_hash_within(&dir, ".cargo", cap).starts_with(BIG), "a folder past its names");
    std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
    let small = made_hash_within(&dir, "Makefile", cap);
    assert!(!small.starts_with(BIG) && !small.is_empty(), "{small}");
    assert!(Made { path: "Makefile".into(), hash: small }.keepable());
    std::fs::remove_dir_all(&dir).ok();
}

/// A made name whose content can't be told — a FIFO, a file of mode 000 — hashes to "" and is never kept: a
/// kept "" would match any later unreadable one, never asked again. (Stand-ins in the test's temp folder.)
#[test]
fn a_made_name_that_cant_be_read_isnt_kept() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = repo("unreadable");
    let fifo = std::ffi::CString::new(dir.join("Makefile").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path of the test's own folder, live for the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
    std::fs::set_permissions(dir.join("build.rs"), std::fs::Permissions::from_mode(0o000)).unwrap();
    for p in ["Makefile", "build.rs"] {
        let made = Made { path: p.into(), hash: made_hash(&dir, p) };
        assert_eq!(made.hash, "", "{p}");
        assert!(!made.keepable(), "{p}");
    }
    std::fs::set_permissions(dir.join("build.rs"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

/// The reviewer's finding: `r`'s two refusals. A folder isn't removed (only files and links — it and what's
/// in it stay, and the answer says why); a file that holds something else than when the card asked is left
/// as it is. One as it was asked about is removed. (Stand-ins in the test's temp folder.)
#[test]
fn r_leaves_a_folder_and_a_file_changed_since_it_was_asked() {
    let dir = repo("remove-refused");
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join(".cargo/config.toml"), "[build]\n").unwrap();
    std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
    std::fs::write(dir.join("sub/Cargo.toml"), "[package]\n").unwrap();
    let made = |p: &str| Made { path: p.into(), hash: made_hash(&dir, p) };
    let asked = [made(".cargo"), made("Makefile"), made("sub/Cargo.toml")];
    std::fs::write(dir.join("Makefile"), "all: changed\n").unwrap();
    let gone = remove_made(&dir, &asked);
    let said = |p: &str| gone.iter().find(|(m, _)| m.path == p).map(|(_, r)| r.clone()).unwrap();
    let folder = said(".cargo").unwrap_err();
    assert!(folder.contains("is a folder"), "{folder}");
    assert!(dir.join(".cargo/config.toml").exists());
    let changed = said("Makefile").unwrap_err();
    assert!(changed.contains("changed since it was asked about"), "{changed}");
    assert_eq!(std::fs::read_to_string(dir.join("Makefile")).unwrap(), "all: changed\n");
    assert!(said("sub/Cargo.toml").is_ok() && !dir.join("sub/Cargo.toml").exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// A first build's new ignored `target/` (or `node_modules/`, or a folder inside one) is a tool's output: not
/// walked and not named — else past the walk's limit the folder itself would be asked about after most projects'
/// first build. Another new folder past the limit is named (the limit holds). (Names only, the test's folder.)
#[test]
fn a_new_output_folder_is_neither_walked_nor_named() {
    let dir = repo("output-walk");
    for d in ["target/debug", "out/node_modules/p", "gen"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
        for i in 0..4 {
            std::fs::write(dir.join(d).join(format!("f{i}")), "x\n").unwrap();
        }
    }
    let walked = |folder: &str| {
        let mut out = Vec::new();
        flagged_below(&dir, folder, 2, &mut out);
        out
    };
    assert_eq!(walked("target/"), Vec::<String>::new());
    assert_eq!(walked("out/node_modules/"), Vec::<String>::new());
    assert_eq!(walked("gen/"), ["gen"], "past the limit: named");
    std::fs::remove_dir_all(&dir).ok();
}

/// G23: links and repositories aren't files a tree takes back (named in `skipped`) — they're looked at all
/// the same: a link named like a protected name, a link to a folder anywhere (by its own name — the folder it
/// leads to isn't walked), a new repository by its `.git`; not a link to a file of another name, nor a link to
/// a folder below a tool's output.
#[test]
fn links_and_repositories_a_run_made_are_looked_at() {
    let dir = repo("links");
    let before = Before::take(&dir);
    std::os::unix::fs::symlink("a.txt", dir.join("src/build.rs")).unwrap();
    std::os::unix::fs::symlink("src", dir.join("docs")).unwrap();
    std::os::unix::fs::symlink("src/a.txt", dir.join("notes")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::os::unix::fs::symlink("../src", dir.join("target/up")).unwrap();
    let lib = dir.join("vendor/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("x.txt"), "x\n").unwrap();
    git_in(&lib, &["init", "-q"]);
    git_in(&lib, &["add", "-A"]);
    git_in(&lib, &["commit", "-q", "-m", "lib"]);
    let found = before.since().unwrap();
    assert_eq!(paths(&found.protected), ["docs", "src/build.rs", "vendor/lib/.git"]);
    assert!(
        found.skipped.iter().any(|s| s.starts_with("docs ")),
        "still not taken back: {:?}",
        found.skipped
    );
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

/// #225: a run that moves HEAD (Claude's `git merge --ff-only main`, with the user's yes) changed the
/// checkout's files with it — they aren't the turn's changes: left out of the turn, its review and `space u`,
/// and said instead ("HEAD moved: fast-forward to <short id>") — to Claude, in the chat, at the head of the
/// turn's review. A file the run changed besides, one the move brought too, is the turn's all the same —
/// from what the move left it: `space u` takes back the run's part, never the move's.
#[test]
fn what_came_with_heads_move_isnt_the_turns() {
    let dir = repo("headmove");
    // (a branch one commit ahead: a.txt changed, came.txt new)
    git_in(&dir, &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.join("src/a.txt"), "one\nSIDE\n").unwrap();
    std::fs::write(dir.join("src/came.txt"), "came\n").unwrap();
    git_in(&dir, &["add", "src"]);
    git_in(&dir, &["commit", "-q", "-m", "side"]);
    git_in(&dir, &["checkout", "-q", "-"]);
    let short =
        std::process::Command::new("git").arg("-C").arg(&dir).args(["rev-parse", "--short", "side"]).output();
    let short = String::from_utf8(short.unwrap().stdout).unwrap().trim().to_string();
    let said = format!("HEAD moved: fast-forward to {short}");
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
    // (the test's own repository, no hooks; then a change of the run's own over a file the move brought)
    let cmd = "git -c core.hooksPath=/dev/null merge -q --ff-only side && printf 'one\\nRUN\\n' > src/a.txt \
               && printf 'mine, changed\\n' > untracked.txt";
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run",
        "arguments": {"command": cmd, "outside_sandbox": true, "reason": "a test of its own files"}}});
    assert!(crate::tools::mcp(&mut ed, generation, "r1", &msg).is_none(), "it waits for the user");
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "its card");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(1).is_some(), "it ran");
    let report = answered(1).unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("src/came.txt")).unwrap(), "came\n", "{report}");
    // Said to Claude, the move's files not among the run's
    assert!(
        report.contains(&format!("{said} — the file it brought isn't one of this turn's changes:")),
        "{report}"
    );
    assert!(!report.contains("came.txt"), "{report}");
    // The turn: the run's own changes only — a.txt's from what the move brought
    let turn = &ed.session.checkpoints.turn;
    let mut paths: Vec<&Path> = turn.iter().map(|t| t.path.strip_prefix(&dir).unwrap()).collect();
    paths.sort();
    assert_eq!(paths, [Path::new("src/a.txt"), Path::new("untracked.txt")], "{turn:#?}");
    let a = turn.iter().find(|t| t.path.ends_with("a.txt")).unwrap();
    assert_eq!(a.before.as_ref().map(|b| b.to_string()).as_deref(), Some("one\nSIDE\n"));
    let saids = |h: &[HeadMove]| h.iter().map(HeadMove::said).collect::<Vec<_>>();
    assert_eq!(saids(&ed.session.checkpoints.head), std::slice::from_ref(&said));
    // Said in the chat
    let chat = ed.session.chat.as_ref().unwrap();
    assert!(chat.msgs.iter().any(|m| m.role == crate::chat::Role::Note && m.text == said));
    // The turn ends: its checkpoint has the two files and says the move — at the head of its review
    ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
    assert!(crate::checkpoint::gather(&mut ed, "bring main in"), "written unseen: gathered");
    pump(&mut ed, &|ed| ed.session.checkpoints.list.len() == 1, "its checkpoint");
    let cp = &ed.session.checkpoints.list[0];
    assert_eq!((cp.files.len(), saids(&cp.head)), (2, vec![said.clone()]));
    assert!(ed.session.checkpoints.head.is_empty(), "the next turn starts with none");
    crate::codereview::start_turn(&mut ed, 0);
    pump(&mut ed, &|ed| ed.session.code_review.as_ref().is_some_and(|r| r.files.len() == 2), "its review");
    // (said as it opens too: a pane too narrow for the rail shows no head)
    let toast = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    let not = format!("review: 2 files Claude changed (not the 1 file that came with HEAD → {short})");
    assert!(toast.starts_with(&not), "{toast}");
    let mut out = Vec::new();
    crate::term::render(&mut ed, &mut out, 240, 30).unwrap();
    let mut screen = vt100::Parser::new(30, 240, 0);
    screen.process(&out);
    // (the rail's side only: the chat on the right says it too)
    let rail: Vec<String> = screen.screen().rows(0, 60).collect();
    let row = |s: &str| rail.iter().position(|r| r.contains(s));
    let moved = rail.iter().position(|r| r.contains("HEAD") && r.contains(&short));
    let (q, files) = (row("“bring main in”"), row("2 files"));
    assert!(q.is_some() && q < moved && moved < files, "one row, under the question: {rail:#?}");
    // `space u`: the run's own changes back — the move's file stays, as git has it
    crate::checkpoint::step(&mut ed, true).unwrap();
    pump(&mut ed, &|_| std::fs::read_to_string(dir.join("untracked.txt")).unwrap() == "mine\n", "undone");
    assert!(dir.join("src/came.txt").exists(), "what came with HEAD isn't taken back");
    assert_eq!(std::fs::read_to_string(dir.join("src/a.txt")).unwrap(), "one\nSIDE\n", "as HEAD has it");
    std::fs::remove_dir_all(&dir).ok();
}

/// A commit the run made keeps every file the turn's (review finding on #225, user decision 2026-10-14):
/// `edit && git commit -am x` moves HEAD and the files' blobs are the two commits' — yet they're the turn's.
/// A fast-forward and then a commit of the run's isn't a pure fast-forward: what the fast-forward brought
/// stays too, as before #225 — only said. (Plain git in the test's own repository.)
#[test]
fn a_commit_the_run_made_keeps_its_files() {
    let dir = repo("commit");
    // (a branch one commit ahead: came.txt new)
    git_in(&dir, &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.join("src/came.txt"), "came\n").unwrap();
    git_in(&dir, &["add", "src"]);
    git_in(&dir, &["commit", "-q", "-m", "side"]);
    git_in(&dir, &["checkout", "-q", "-"]);
    let paths =
        |f: &Found| f.files.iter().map(|c| c.path.strip_prefix(&dir).unwrap().to_owned()).collect::<Vec<_>>();
    let said = "HEAD moved: a commit made by this run — its files stay in this turn";
    // A fast-forward, then a commit of its own: not a pure fast-forward — every file stays
    let before = Before::take(&dir);
    git_in(&dir, &["merge", "-q", "--ff-only", "side"]);
    std::fs::write(dir.join("src/mine.txt"), "mine\n").unwrap();
    git_in(&dir, &["add", "src/mine.txt"]);
    git_in(&dir, &["commit", "-q", "-m", "the run's own, after the merge"]);
    let found = before.since().unwrap();
    assert_eq!(paths(&found), [Path::new("src/came.txt"), Path::new("src/mine.txt")], "{found:?}");
    let h = found.head.expect("HEAD moved");
    assert!(!h.came && h.forward && h.files == 0 && h.own == 1, "{h:?}");
    assert_eq!(h.said(), said);
    // An edit and its commit: the turn's, said
    let before = Before::take(&dir);
    std::fs::write(dir.join("src/a.txt"), "one\nCOMMITTED\n").unwrap();
    git_in(&dir, &["commit", "-q", "-am", "the run's own"]);
    let found = before.since().unwrap();
    assert_eq!(paths(&found), [Path::new("src/a.txt")], "{found:?}");
    let a = &found.files[0];
    assert_eq!(a.before.as_ref().and_then(|b| b.text.as_deref()), Some("one\ntwo\n"));
    let h = found.head.expect("HEAD moved");
    assert!(!h.came && h.files == 0 && h.own == 1, "{h:?}");
    assert_eq!(h.said(), said);
    std::fs::remove_dir_all(&dir).ok();
}

/// A merge the run made that isn't a fast-forward, and a reset back (review findings, user decision
/// 2026-10-14): neither is a pure fast-forward, so every file stays the turn's as before #225 — what both
/// sides of the merge changed keeps its text from before the run (never the other side's, which the checkout
/// never had), what the reset changed is the turn's — and the move is only said. (Plain git in the test's own
/// repository.)
#[test]
fn a_merge_and_a_move_back_keep_their_files() {
    let dir = repo("merge");
    // (the other side: a.txt's last line, came.txt new; this side: a.txt's first line — apart: no conflict)
    std::fs::write(dir.join("src/a.txt"), "one\ntwo\n3\n4\nfive\n").unwrap();
    git_in(&dir, &["commit", "-q", "-am", "longer"]);
    git_in(&dir, &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.join("src/a.txt"), "one\ntwo\n3\n4\nSIDE\n").unwrap();
    std::fs::write(dir.join("src/came.txt"), "came\n").unwrap();
    git_in(&dir, &["add", "src"]);
    git_in(&dir, &["commit", "-q", "-m", "side"]);
    git_in(&dir, &["checkout", "-q", "-"]);
    std::fs::write(dir.join("src/a.txt"), "ONE\ntwo\n3\n4\nfive\n").unwrap();
    git_in(&dir, &["commit", "-q", "-am", "this side"]);
    let text = |c: &Change, before: bool| {
        let b = if before { &c.before } else { &c.after };
        b.as_ref().and_then(|b| b.text.clone())
    };
    // A merge made by the run: its own commit — a.txt from this side's text, came.txt the turn's
    let before = Before::take(&dir);
    git_in(&dir, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    assert_eq!(std::fs::read_to_string(dir.join("src/a.txt")).unwrap(), "ONE\ntwo\n3\n4\nSIDE\n");
    let found = before.since().unwrap();
    let a = file(&found.files, &dir, "src/a.txt");
    let (was, is) = ("ONE\ntwo\n3\n4\nfive\n", "ONE\ntwo\n3\n4\nSIDE\n");
    assert_eq!((text(&a, true), text(&a, false)), (Some(was.into()), Some(is.into())));
    file(&found.files, &dir, "src/came.txt");
    let h = found.head.expect("HEAD moved");
    assert!(!h.came && h.own == 1 && h.files == 0, "{h:?}");
    assert_eq!(h.said(), "HEAD moved: a commit made by this run — its files stay in this turn");
    // Back where it was (a reset to an ancestor): its files stay the turn's — a.txt back, came.txt gone
    let before = Before::take(&dir);
    git_in(&dir, &["reset", "-q", "--hard", "HEAD~1"]);
    assert!(!dir.join("src/came.txt").exists());
    let found = before.since().unwrap();
    let a = file(&found.files, &dir, "src/a.txt");
    assert_eq!((text(&a, true), text(&a, false)), (Some(is.into()), Some(was.into())));
    assert!(file(&found.files, &dir, "src/came.txt").after.is_none());
    let h = found.head.expect("HEAD moved");
    assert!(!h.came && !h.forward && h.files == 0 && h.own == 0, "{h:?}");
    let said = format!("HEAD moved to {} — not a fast-forward; its files stay in this turn", h.short);
    assert_eq!(h.said(), said);
    std::fs::remove_dir_all(&dir).ok();
}

/// An edit and its commit in one run of Claude's (`edit && git commit -am x`): the commit didn't exist before
/// the run, so its file is the turn's — in it from the text before the run, `space u` takes it back — and the
/// move is said as the run's own ("HEAD moved: a commit made by this run — its files stay in this turn"), to
/// Claude and in the chat.
/// (The test's own repository, no hooks.)
#[test]
fn an_edit_and_its_commit_in_one_run_stay_the_turns() {
    let dir = repo("editcommit");
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
    let said = "HEAD moved: a commit made by this run — its files stay in this turn";
    let cmd = "printf 'one\\nCOMMITTED\\n' > src/a.txt && git -c core.hooksPath=/dev/null -c commit.gpgsign=false \
               -c user.email=t@example.invalid -c user.name=t commit -q -am 'the run s own'";
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run",
        "arguments": {"command": cmd, "outside_sandbox": true, "reason": "a test of its own files"}}});
    assert!(crate::tools::mcp(&mut ed, generation, "r1", &msg).is_none(), "it waits for the user");
    pump(&mut ed, &|ed| !ed.offers.is_empty(), "its card");
    ed.offer_accept();
    pump(&mut ed, &|_| answered(1).is_some(), "it ran");
    let report = answered(1).unwrap().unwrap();
    // Said to Claude and in the chat as the run's own; its file the turn's
    assert!(report.contains(&format!("{said}.")), "{report}");
    assert!(report.contains("It changed in the checkout: src/a.txt"), "{report}");
    let chat = ed.session.chat.as_ref().unwrap();
    assert!(chat.msgs.iter().any(|m| m.role == crate::chat::Role::Note && m.text == said));
    let turn = &ed.session.checkpoints.turn;
    assert_eq!(turn.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), [dir.join("src/a.txt")]);
    assert_eq!(turn[0].before.as_ref().map(|b| b.to_string()).as_deref(), Some("one\ntwo\n"));
    // The turn ends: its review has the file; `space u` takes the edit back
    ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
    assert!(crate::checkpoint::gather(&mut ed, "fix and commit"), "written unseen: gathered");
    pump(&mut ed, &|ed| ed.session.checkpoints.list.len() == 1, "its checkpoint");
    assert_eq!(ed.session.checkpoints.list[0].files.len(), 1);
    crate::checkpoint::step(&mut ed, true).unwrap();
    pump(&mut ed, &|_| std::fs::read_to_string(dir.join("src/a.txt")).unwrap() == "one\ntwo\n", "undone");
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
            lines: crate::test_results::Rules::builtin("python"),
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

/// An editor in the test's repository `dir` with Claude's turn under way (no sandbox: the commands are
/// harmless, in that folder) — and its chat's generation.
fn turn_in(dir: &Path) -> (crate::editor::Editor, u64) {
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = dir.to_path_buf();
    ed.config.llm.sandbox = false;
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let generation = c.generation();
    (ed, generation)
}

fn pump_until(ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done(ed) {
        assert!(Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// Claude's run `n` of `cmd` in `dir`, and its answer once it's come.
fn claude_runs(ed: &mut crate::editor::Editor, (generation, dir): (u64, &Path), n: u64, cmd: &str) {
    let call = crate::tools::Call::of(generation, &format!("r{n}"), json!(n));
    let job = crate::tools::Job::Command(cmd.into(), dir.to_path_buf());
    crate::tools::guard_and_go(ed, crate::tools::For::Claude(call), job, false, Default::default());
}

fn answered(n: u64) -> Option<Result<String, String>> {
    crate::tools::ANSWERED.with(|a| a.borrow().iter().find(|(id, _)| *id == json!(n)).map(|(_, r)| r.clone()))
}

/// The front card in sight long enough, and read whole (as drawing it would).
fn settled(ed: &crate::editor::Editor) {
    let o = (ed.offers[0].id, ed.offers[0].rev);
    ed.offer_read_whole(o);
    ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE)));
}

/// G23: your own runs in the checkout wait on the card too — a task (your build) and the preflight run what's
/// there outside the sandbox: `n` runs nothing, `y` runs it, and the one after doesn't ask. Not Claude's card:
/// its turn ending doesn't take it. (Harmless commands: markers in the test's own repository.)
#[test]
fn your_task_and_preflight_wait_on_what_claudes_run_made() {
    let dir = repo("yours");
    let (mut ed, generation) = turn_in(&dir);
    let me = (generation, dir.as_path());
    claude_runs(&mut ed, me, 1, "mkdir sub && printf '[package]\\n' > sub/Cargo.toml");
    pump_until(&mut ed, &|_| answered(1).is_some(), "Claude's run");
    let task = crate::task::Task { name: "build".into(), cmd: "touch built".into(), cwd: PathBuf::new() };
    ed.task_start(task.clone());
    pump_until(&mut ed, &|ed| !ed.offers.is_empty(), "the card before your task");
    let o = &ed.offers[0];
    assert_eq!(o.title, crate::tools::MADE_YOURS_TITLE);
    assert!(o.question.starts_with("touch built\n\nClaude's last run made sub/Cargo.toml"), "{}", o.question);
    assert!(!o.claudes() && o.weighty(), "yours, and it runs something");
    ed.offer_dismiss();
    assert!(ed.offers.is_empty() && !dir.join("built").exists(), "n: nothing ran");
    // The preflight: asked the same, `y` runs it
    let step = |cmd: &str| crate::preflight::Step {
        name: "check".into(),
        cmd: cmd.into(),
        note: None,
        from: "test".into(),
        full: false,
    };
    // (`p` on the bring card: `n` here says the session wasn't brought, and nothing waits to bring it)
    ed.session.bring_after_preflight = true;
    let runs = ed.task_generation;
    ed.preflight_run(vec![step("touch checked")], crate::preflight::Set::Quick).unwrap();
    pump_until(&mut ed, &|ed| !ed.offers.is_empty(), "the card before the preflight, to bring");
    ed.offer_dismiss();
    assert!(!ed.session.bring_after_preflight, "nothing waits to bring it");
    assert!(ed.session.preflight.is_none() && ed.task_generation == runs, "no preflight started");
    let said = ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ");
    assert!(said.contains("not brought — the preflight didn't run"), "{said}");
    ed.preflight_run(vec![step("touch checked")], crate::preflight::Set::Quick).unwrap();
    pump_until(&mut ed, &|ed| !ed.offers.is_empty(), "the card before the preflight");
    assert!(ed.offers[0].question.contains("check\n\nClaude's last run made sub/Cargo.toml"));
    crate::tools::turn_over(&mut ed);
    assert_eq!(ed.offers.len(), 1, "Claude's turn ending leaves your card");
    settled(&ed);
    ed.offer_accept();
    let ended = |ed: &crate::editor::Editor| {
        ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
    };
    pump_until(&mut ed, &|ed| dir.join("checked").exists() && ended(ed), "the preflight");
    // Kept: your task runs unasked
    ed.task_start(task);
    pump_until(&mut ed, &|_| dir.join("built").exists(), "your task, unasked");
    assert!(ed.offers.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

/// What `r` couldn't remove is asked about again before the run goes on: the card comes back with only that
/// name, the run hasn't run, what was removed is gone — and `n` there tells Claude no. (A file changed after
/// the card asked stands in for one `r` refuses; harmless files in the test's repository.)
#[test]
fn what_r_couldnt_remove_is_asked_again_before_the_run() {
    let dir = repo("remove-again");
    let (mut ed, generation) = turn_in(&dir);
    let me = (generation, dir.as_path());
    claude_runs(
        &mut ed,
        me,
        1,
        "mkdir sub && printf '[package]\\n' > sub/Cargo.toml && printf 'all:\\n' > Makefile",
    );
    pump_until(&mut ed, &|_| answered(1).is_some(), "the first run");
    claude_runs(&mut ed, me, 2, "touch two");
    pump_until(&mut ed, &|ed| !ed.offers.is_empty(), "the card");
    std::fs::write(dir.join("sub/Cargo.toml"), "[package]\nname = \"changed\"\n").unwrap();
    settled(&ed);
    ed.offer_one_by_one();
    let again = |ed: &crate::editor::Editor| {
        ed.offers.first().is_some_and(|o| {
            !o.installing()
                && matches!(&o.what, crate::offer::What::ProtectedMade { made, .. } if paths(made) == ["sub/Cargo.toml"])
        })
    };
    pump_until(&mut ed, &again, "the card again, for what stayed");
    assert!(!dir.join("Makefile").exists(), "what it could remove is gone");
    assert!(dir.join("sub/Cargo.toml").exists() && !dir.join("two").exists(), "the run waits");
    assert!(answered(2).is_none());
    ed.offer_dismiss();
    let no = answered(2).unwrap().unwrap_err();
    assert!(no.contains("chose not to run `touch two`"), "{no}");
    assert!(!dir.join("two").exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// G23: the card's answers. `v` shows what each name holds (a file's text, a link's target — not followed)
/// under the card's text; `r` removes only those names — a link itself, never the folder it leads to — and
/// the run goes on, Claude told; `y` keeps them, remembered by path and content: made again holding the same,
/// not asked about; holding something else, asked. (Harmless files and commands in the test's repository.)
#[test]
fn the_cards_answers_view_remove_and_keep_by_content() {
    let dir = repo("answers");
    let (mut ed, generation) = turn_in(&dir);
    let me = (generation, dir.as_path());
    claude_runs(&mut ed, me, 1, "mkdir sub && printf '[package]\\n' > sub/Cargo.toml && ln -s ../src sub/up");
    pump_until(&mut ed, &|_| answered(1).is_some(), "the first run");
    claude_runs(&mut ed, me, 2, "touch two");
    pump_until(&mut ed, &|ed| !ed.offers.is_empty(), "the card");
    settled(&ed);
    // v
    ed.offer_view();
    let view = |ed: &crate::editor::Editor| {
        ed.docs
            .iter()
            .find(|d| d.title.as_deref().is_some_and(|t| t.starts_with(crate::offer::CARD_VIEW)))
            .map(|d| d.text.to_string())
    };
    pump_until(&mut ed, &|ed| view(ed).is_some(), "its view");
    let text = view(&ed).unwrap();
    assert!(text.contains("── sub/Cargo.toml: \n    [package]"), "{text}");
    assert!(text.contains("── sub/up: a link to ../src — not followed"), "{text}");
    // r
    ed.offer_one_by_one();
    pump_until(&mut ed, &|_| answered(2).is_some(), "the run after r");
    assert!(dir.join("two").exists(), "it ran");
    assert!(!dir.join("sub/Cargo.toml").exists() && std::fs::symlink_metadata(dir.join("sub/up")).is_err());
    assert!(dir.join("src/a.txt").exists(), "not what the link led to");
    assert!(ed.session.made_protected.is_empty());
    assert!(ed.session.checkpoints.tell.iter().any(|t| t.contains("The user removed sub/Cargo.toml")));
    // y: kept as it is
    claude_runs(&mut ed, me, 3, "printf 'all:\\n' > Makefile");
    pump_until(&mut ed, &|_| answered(3).is_some(), "the third run");
    claude_runs(&mut ed, me, 4, "rm Makefile");
    pump_until(&mut ed, &|ed| !ed.offers.is_empty(), "the card for Makefile");
    settled(&ed);
    ed.offer_accept();
    pump_until(&mut ed, &|_| answered(4).is_some(), "the fourth run");
    // Made again, the same: not asked
    claude_runs(&mut ed, me, 5, "printf 'all:\\n' > Makefile");
    pump_until(&mut ed, &|_| answered(5).is_some(), "the fifth run");
    assert!(ed.session.made_protected.is_empty(), "kept, holding the same");
    // Made again, holding something else: asked
    claude_runs(&mut ed, me, 6, "rm Makefile");
    pump_until(&mut ed, &|_| answered(6).is_some(), "the sixth run");
    claude_runs(&mut ed, me, 7, "printf 'all: x\\n' > Makefile");
    pump_until(&mut ed, &|_| answered(7).is_some(), "the seventh run");
    assert_eq!(paths(&ed.session.made_protected), ["Makefile"], "changed: asked again");
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
    assert!(cp.files.len() == 1 && cp.files.paths()[0].ends_with("src/late.txt"), "{cp:?}");
    assert_eq!(ed.session.checkpoints.turn.len(), 1, "the turn in flight keeps only its own");
    let row = ed.session.chat.as_ref().unwrap().msgs.last().unwrap().text.clone();
    assert!(row.contains("late.txt after the turn"), "{row}");
    std::fs::remove_dir_all(&dir).ok();
}
