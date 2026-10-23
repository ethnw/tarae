//! Overlaps (`overlap.rs`): two checkouts of a fixture repository, their working trees merged on workers — what's
//! said, to whom, and what's never written.

use std::process::Command;

use serde_json::json;

use super::*;
use crate::config::Config;
use crate::key::{Code, Key};
use crate::scratch::Scratch;

/// git in the test's own repository: its own name, no signing, no hooks, none of the machine's config.
fn git(dir: &Path, args: &[&str]) {
    crate::gitrepo::not_in_this_repository(dir, args);
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(["-c", "init.defaultBranch=main", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Twelve lines: `line 1` … `line 12`.
fn twelve() -> String {
    (1..=12).map(|n| format!("line {n}\n")).collect()
}

struct Scene {
    ed: Editor,
    /// The repository's first checkout (the home session's, `main`) and a second (`fix`, a worktree, in front).
    main: PathBuf,
    fix: PathBuf,
    fix_id: SessionId,
    dir: Scratch,
}

/// A repository (`a.rs`, `b.rs`, `server.pem` committed) and a second checkout of it: you in `fix` with its `a.rs`
/// open, its base `main`; overlaps looked for at once (no quiet), in the test's own folder.
fn scene(name: &str) -> Scene {
    scene_with(name, &[], None)
}

/// `scene`, with `more` files committed too, and the user's own git config (`user`: text, in the test's folder) —
/// what tarae's git reads as the user's, never held.
fn scene_with(name: &str, more: &[(&str, &str)], user: Option<&str>) -> Scene {
    let dir = Scratch::new(name);
    if let Some(text) = user {
        std::fs::write(dir.join("user.gitconfig"), text).unwrap();
        crate::gitrepo::global_config_for(&dir, &dir.join("user.gitconfig"));
    }
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    for f in ["a.rs", "b.rs", "server.pem"] {
        std::fs::write(main.join(f), twelve()).unwrap();
    }
    for (f, text) in more {
        std::fs::write(main.join(f), text).unwrap();
    }
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "first"]);
    git(&main, &["worktree", "add", "-q", "../fix"]);
    let fix = std::fs::canonicalize(dir.join("fix")).unwrap();
    let (ed, fix_id) = editor_on(&main, &fix, &dir.join("overlap"));
    Scene { ed, main, fix, fix_id, dir }
}

/// A tarae on the two checkouts — its own folder in `base` (`own_in`), you in `fix` with its `a.rs` open.
fn editor_on(main: &Path, fix: &Path, base: &Path) -> (Editor, SessionId) {
    let mut ed = Editor::new(Config::default());
    ed.session.root = main.to_path_buf();
    ed.session.name = "main".into();
    ed.session.git_branch = Some("main".into());
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.overlap.dir = Some(own_in(base));
    ed.overlap.quiet = Duration::ZERO;
    let fix_id = ed.session_add(fix.to_path_buf());
    let l = ed.session_by_id(fix_id).unwrap();
    l.name = "fix".into();
    l.base = Some("main".into());
    assert!(ed.session_switch(fix_id));
    ed.open(&fix.join("a.rs")).unwrap();
    (ed, fix_id)
}

/// Line `n` (from 1) of `path` on disk made `text`, said as a save (`Editor::wrote`).
fn change(ed: &mut Editor, path: &Path, n: usize, text: &str) {
    let now = std::fs::read_to_string(path).unwrap();
    let lines: Vec<String> = now
        .lines()
        .enumerate()
        .map(|(i, l)| if i + 1 == n { text.to_string() } else { l.to_string() })
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
    ed.wrote(path);
}

/// Events until `rounds` rounds have landed and nothing more is coming.
fn settle(ed: &mut Editor, rounds: u64) {
    sync(ed);
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while ed.overlap.rounds < rounds || ed.overlap.waiting || ed.overlap.running {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("round {rounds}: no event in time"));
        ed.handle_event(ev);
    }
}

/// Events until `rounds` rounds have landed — not the one it armed after.
fn landed_to(ed: &mut Editor, rounds: u64) {
    sync(ed);
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while ed.overlap.rounds < rounds {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("round {rounds}: no event in time"));
        ed.handle_event(ev);
    }
}

fn press(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_event(crate::event::Event::Key(k));
    }
}

/// The messages said with `needle` in them.
fn said(ed: &Editor, needle: &str) -> Vec<String> {
    ed.messages.iter().filter(|m| m.2.contains(needle)).map(|m| m.2.clone()).collect()
}

/// Every file under `dir`, by its path from there (the repository's git folder: what's written there).
fn files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            match m.is_dir() {
                true => todo.push(e.path()),
                false => out.push((e.path().strip_prefix(dir).unwrap().to_path_buf(), m.len())),
            }
        }
    }
    out.sort();
    out
}

/// A measurement, not a check (`cargo perf`): a round's cost on this repository's files as of `tests/perf.rs`'s
/// `CORPUS`, two checkouts each with a few files changed and one file both changed on the same line — the first
/// round (both indexes seeded, every change hashed, the merges) and the round once one file of one workspace moved
/// (the plan's §0: 57–89 ms a snapshot, 17–42 ms a merge-tree, on nine live checkouts).
#[test]
fn perf_overlap_round() {
    if crate::test_results::unoptimized("overlap::tests::perf_overlap_round") {
        return;
    }
    use std::io::Write as _;
    const CORPUS: &str = "53843b6";
    const PATHS: [&str; 8] =
        ["src", "runtime", "docs", "tests", "xtask/src", "Cargo.toml", "README.md", "CONTRIBUTING.md"];
    let s = Scratch::new("overlap-perf");
    let main = s.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let archive = Command::new("git")
        .args(["archive", "--format=tar", CORPUS, "--"])
        .args(PATHS)
        .current_dir(&here)
        .output();
    let Some(archive) = archive.ok().filter(|a| a.status.success()) else {
        let why = format!("overlap::tests::perf_overlap_round — no commit {CORPUS} here");
        return crate::test_results::said_skip(&why);
    };
    let mut tar = Command::new("tar")
        .args(["-xf", "-"])
        .current_dir(&main)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    tar.stdin.take().unwrap().write_all(&archive.stdout).unwrap();
    assert!(tar.wait().unwrap().success());
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "corpus"]);
    git(&main, &["worktree", "add", "-q", "../fix"]);
    let fix = std::fs::canonicalize(s.join("fix")).unwrap();
    let edit = |path: &Path, n: usize, text: &str| {
        let now = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> =
            now.lines().enumerate().map(|(i, l)| if i + 1 == n { text } else { l }).collect();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    };
    for f in ["src/term.rs", "src/chat.rs", "src/editor.rs", "docs/features.md"] {
        edit(&fix.join(f), 100, "// fix was here");
    }
    for f in ["src/term.rs", "src/sandbox.rs", "README.md"] {
        edit(&main.join(f), 100, "// main was here");
    }
    let dir = s.join("overlap");
    let ws = |id: SessionId, root: &Path, base: Option<&str>, had: Option<Snap>| Ws {
        id,
        root: root.to_path_buf(),
        base: base.map(str::to_string),
        folder: Arc::new(Folder::new(folder_of(&dir, root))),
        take: true,
        had,
        max: OBJECTS_MAX,
    };
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let r = round(vec![ws(0, &main, None, None), ws(1, &fix, Some("main"), None)], &HashMap::new(), None);
    let cold = ms(t);
    assert_eq!(
        r.now.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(),
        ["src/term.rs"],
        "the fixture conflicts"
    );
    let snap = |r: &Round, id: SessionId| r.snaps.iter().find(|s| s.0 == id).and_then(|s| s.2.clone());
    let (mut main_snap, mut merges) = (snap(&r, 0), r.merges);
    let mut warm = Vec::new();
    for k in 0..5 {
        edit(&fix.join("src/chat.rs"), 200, &format!("// fix, again {k}"));
        let mut a = ws(0, &main, None, main_snap.clone());
        a.take = false;
        let t = Instant::now();
        let r = round(vec![a, ws(1, &fix, Some("main"), None)], &merges, None);
        warm.push(ms(t));
        assert_eq!(r.merged, 1, "only the pair whose side moved");
        (main_snap, merges) = (snap(&r, 0), r.merges);
    }
    warm.sort_by(f64::total_cmp);
    let line = format!(
        "PERF overlap_round_first_ms {cold:.1} noise=100\nPERF overlap_round_one_moved_ms {:.1} noise=40\n",
        warm[warm.len() / 2]
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// A change of yours in `fix` that meets one in `main` on the same line is said once — a toast naming the pair, the
/// file and fix's lines — while changes of one file far apart aren't; a later round that finds it again says nothing,
/// a new file is said again, and one that goes is said in `:messages` only — back, it's said again; its lines
/// elsewhere in the file, it's said again; a session renamed is the same session, not said again. (A key-sequence
/// test: your edit and `:w`.)
#[test]
fn an_overlap_is_said_once_far_apart_isnt() {
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene("overlap-said");
    change(&mut ed, &main.join("a.rs"), 3, "three, as main has it");
    change(&mut ed, &main.join("b.rs"), 1, "one, as main has it");
    change(&mut ed, &fix.join("b.rs"), 12, "twelve, as fix has it");
    // (yours: line 3 of a.rs, saved)
    press(&mut ed, "3ggA, as fix has it<esc>:w<ret>");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while ed.doc().is_modified() {
        let left = end.saturating_duration_since(std::time::Instant::now());
        ed.handle_event(ed.events.recv_timeout(left).expect("the save"));
    }
    assert!(std::fs::read_to_string(fix.join("a.rs")).unwrap().contains("line 3, as fix has it"));
    settle(&mut ed, 1);
    let only: Vec<(&str, Option<Range<usize>>)> =
        ed.overlap.now.iter().map(|o| (o.path.as_str(), o.lines.clone())).collect();
    assert_eq!(only, [("a.rs", Some(2..3))], "b.rs's changes are far apart: {:?}", ed.overlap.now);
    assert_eq!(ed.overlap.now[0].a, fix_id);
    assert_eq!(said(&ed, "would conflict"), ["fix ≈ main · a.rs L3 — would conflict"]);
    assert!(ed.toasts.iter().any(|t| t.text.contains("fix ≈ main · a.rs L3")), "a toast");
    // Found again: not said again
    change(&mut ed, &fix.join("a.rs"), 4, "four, as fix has it");
    settle(&mut ed, 2);
    assert_eq!(ed.overlap.now.len(), 1);
    assert_eq!(said(&ed, "would conflict").len(), 1, "said once");
    // A new file: said
    change(&mut ed, &main.join("b.rs"), 12, "twelve, as main has it");
    settle(&mut ed, 3);
    assert_eq!(said(&ed, "would conflict").last().unwrap(), "fix ≈ main · b.rs L12 — would conflict");
    // Gone: in `:messages` only
    std::fs::write(fix.join("a.rs"), twelve()).unwrap();
    ed.wrote(&fix.join("a.rs"));
    settle(&mut ed, 4);
    assert_eq!(ed.overlap.now.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["b.rs"]);
    assert_eq!(said(&ed, "no longer conflicts"), ["fix ≈ main · a.rs — no longer conflicts"]);
    assert!(!ed.toasts.iter().any(|t| t.text.contains("no longer")), "no toast for one gone");
    // Back again: said again
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix again");
    settle(&mut ed, 5);
    assert_eq!(ed.overlap.now.len(), 2);
    assert_eq!(said(&ed, "would conflict").len(), 3, "gone, then back: said again");
    assert_eq!(said(&ed, "would conflict").last().unwrap(), "fix ≈ main · a.rs L3 — would conflict");
    // Its lines elsewhere in the file — line 3 back as main's base has it, line 10 changed both ways: said again
    change(&mut ed, &main.join("a.rs"), 10, "ten, as main has it");
    settle(&mut ed, 6);
    assert_eq!(said(&ed, "would conflict").len(), 3, "main's line 10 alone meets nothing of fix's");
    change(&mut ed, &fix.join("a.rs"), 3, "line 3");
    change(&mut ed, &fix.join("a.rs"), 10, "ten, as fix has it");
    settle(&mut ed, 7);
    assert_eq!(said(&ed, "would conflict").last().unwrap(), "fix ≈ main · a.rs L10 — would conflict");
    assert_eq!(said(&ed, "would conflict").len(), 4);
    // fix renamed: the same session — found again, not said again
    ed.session_by_id(fix_id).unwrap().name = "repair".into();
    change(&mut ed, &fix.join("a.rs"), 10, "ten, as repair has it");
    settle(&mut ed, 8);
    assert_eq!(ed.overlap.now.len(), 2);
    assert_eq!(said(&ed, "would conflict").len(), 4, "a rename isn't a new overlap");
    // (and nothing armed once it's all landed: an idle tarae isn't woken)
    assert!(!ed.overlap.waiting && !ed.overlap.running);
}

/// Nothing a round makes lands in the repository: its git folder holds the same files after (the snapshots' and
/// merges' objects are in tarae's own folder); that folder goes when the workspace ends.
#[test]
fn nothing_lands_in_the_repository_and_the_folder_goes_with_the_workspace() {
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene("overlap-objects");
    let before = files(&main.join(".git"));
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    std::fs::write(fix.join("new.rs"), "// untracked, new\n").unwrap();
    settle(&mut ed, 1);
    assert_eq!(ed.overlap.now.len(), 1, "it merged: {:?}", ed.overlap.now);
    assert_eq!(files(&main.join(".git")), before, "nothing written in the repository");
    let folder = ed.overlap.folder(fix_id).unwrap();
    assert!(files(&folder.join("objects")).len() > 2, "its objects are tarae's");
    let _ = ed.session_close(&fix);
    assert!(ed.overlap.now.is_empty(), "its overlaps went with it");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while folder.exists() {
        assert!(std::time::Instant::now() < end, "its folder removed");
        std::thread::yield_now();
    }
}

/// Two workspaces that changed no path in common are never merged; neither are a workspace and a base whose tip
/// changed nothing since they parted.
#[test]
fn a_pair_sharing_no_path_is_never_merged() {
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene("overlap-apart");
    change(&mut ed, &main.join("b.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    settle(&mut ed, 1);
    assert_eq!((ed.overlap.merged, ed.overlap.now.len()), (0, 0));
    // (the base moves: main commits its b.rs — still nothing shared with fix)
    git(&main, &["commit", "-qam", "b"]);
    crate::overlap::moved(&mut ed, &main);
    settle(&mut ed, 2);
    assert_eq!((ed.overlap.merged, ed.overlap.now.len()), (0, 0));
}

/// A conflict against the base's tip — main committed a change of the line fix changed — is said as the base's,
/// in the name of the session whose checkout has that branch; a file named like a key is never named.
#[test]
fn the_bases_tip_counts_and_a_key_named_file_is_never_named() {
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene("overlap-base");
    assert!(crate::sandbox::key_named_in(&fix, &fix.join("server.pem")), "the fixture's key-like name");
    change(&mut ed, &main.join("a.rs"), 5, "five, committed on main");
    change(&mut ed, &main.join("server.pem"), 5, "five, committed on main");
    git(&main, &["commit", "-qam", "five"]);
    change(&mut ed, &fix.join("a.rs"), 5, "five, fix");
    change(&mut ed, &fix.join("server.pem"), 5, "five, fix");
    settle(&mut ed, 1);
    let paths: Vec<&str> = ed.overlap.now.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(paths, ["a.rs"], "the key-named file left out");
    assert_eq!(ed.overlap.now[0].b, Other::Session(0), "main's tip, as the main checkout's session");
    assert!(said(&ed, "server.pem").is_empty());
    assert!(listed(&ed, ed.session.id).iter().all(|l| !l.contains("pem")));
}

/// Call `n`'s answer (a change's review answered `y` as it comes).
fn answer(ed: &mut Editor, n: u64) -> String {
    let end = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = crate::tools::ANSWERED
            .with(|a| a.borrow().iter().find(|(i, _)| *i == json!(n)).map(|(_, r)| r.clone()));
        if let Some(r) = got {
            return r.unwrap_or_else(|e| e);
        }
        assert!(std::time::Instant::now() < end, "call {n} answered");
        if ed.session.review.as_ref().is_some_and(|r| r.ready) {
            crate::llm::review_key(ed, Key::plain(Code::Char('y')));
            crate::refactor::next(ed);
        }
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// Claude's `read` of `file` (call `n`), answered.
fn read(ed: &mut Editor, n: u64, file: &str) -> String {
    crate::readtool::call(ed, crate::tools::Call::test(n), &json!({"files": [file]})).unwrap();
    answer(ed, n)
}

/// Claude's `edit` of `file`, `from` → `to` (call `n`), answered.
fn edit(ed: &mut Editor, n: u64, file: &str, from: &str, to: &str) -> String {
    let args = json!({"edits": [{"file_path": file, "old_string": from, "new_string": to}]});
    crate::edittool::call(ed, crate::tools::Call::test(n), &args).unwrap();
    answer(ed, n)
}

/// A chat for the session in front (its claude a stand-in: what it's sent, read from the receiver).
fn chat_in(ed: &mut Editor) -> std::sync::mpsc::Receiver<String> {
    let mut c = crate::chat::Chat::new();
    let rx = crate::chat::stand_in(&mut c);
    ed.session.chat = Some(c);
    rx
}

/// The last message the stand-in claude was sent (its context and text), its turn then over.
fn message(ed: &mut Editor, rx: &std::sync::mpsc::Receiver<String>) -> String {
    let got = rx.try_iter().filter(|l| l.contains("<editor-context>")).last().expect("a message sent");
    crate::chat::stand_in_done(ed);
    got
}

/// Claude hears nothing of the others (the user, 2026-10-23: the line its `edit`/`read` answers carried changed nothing
/// it did in 18 live runs): with an overlap on the file and its woven view showing the conflict, Claude's `read` and
/// `edit` of it — the conflicting line itself too — answer with the file and the change alone; nothing at Claude's
/// level of the UI state either, while the person's toast and list say it. Nor does a message sent with the woven view in
/// front carry any of it by itself — not its text, its lines on screen, nor a selection in it; a quote of it (`space
/// >`) does: that's the person handing it over.
#[test]
fn claude_hears_nothing_of_the_others() {
    let Scene { mut ed, main, dir: _dir, .. } = scene("overlap-unheard");
    let rx = chat_in(&mut ed);
    ed.session.git_branch = Some("fix".into());
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    // (fix's typed in its buffer and saved: the woven view reads the buffer)
    press(&mut ed, "3ggA, three, fix<esc>:w<ret>");
    settle(&mut ed, 1);
    assert_eq!(ed.overlap.now.len(), 1, "the overlap");
    // (the woven view of it, its conflict worked out)
    crate::typed::execute(&mut ed, "weave").unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while crate::weave::of(ed.doc()).is_none_or(|m| m.merges == 0 || m.asking) {
        assert!(std::time::Instant::now() < end, "the merge: {:?}", ed.messages);
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
    assert_eq!(crate::weave::of(ed.doc()).unwrap().merge.conflicts.len(), 1, "the woven conflict");
    // A message with the woven view in front, on screen: none of it goes
    let of_it = |m: &str| {
        ["three, main", "<file-content>", "<excerpt", "<selection", "on screen", "woven"]
            .into_iter()
            .filter(|w| m.contains(w))
            .collect::<Vec<_>>()
    };
    frame(&mut ed);
    assert!(crate::chat::submit(&mut ed, "what's this?", None, None));
    let m = message(&mut ed, &rx);
    assert!(of_it(&m).is_empty() && m.contains(crate::weave::NOT_SENT), "{:?}: {m}", of_it(&m));
    // Nor a selection in it
    press(&mut ed, "%");
    frame(&mut ed);
    assert!(crate::chat::submit(&mut ed, "and this?", None, None));
    let m = message(&mut ed, &rx);
    assert!(of_it(&m).is_empty(), "{:?}: {m}", of_it(&m));
    // Quoted, it goes: the person's act
    press(&mut ed, "<space>>");
    assert!(ed.session.chat.as_ref().is_some_and(|c| c.quotes.len() == 1), "quoted");
    press(&mut ed, "see<ret>");
    let m = message(&mut ed, &rx);
    assert!(m.contains("<quoted") && m.contains("three, main"), "{m}");
    ed.session.at = crate::session::At::Code;
    let heard = |s: &str| ["conflict", "main's", "merged with"].iter().any(|w| s.contains(w));
    let a = read(&mut ed, 71, "a.rs");
    assert!(a.contains("three, fix") && !heard(&a), "{a}");
    let e = edit(&mut ed, 72, "a.rs", "three, fix", "three, fixed");
    assert!(!heard(&e), "{e}");
    let a = read(&mut ed, 73, "a.rs");
    assert!(a.contains("three, fixed") && !heard(&a), "{a}");
    // (the state at Claude's level: never — nor its toast, nor the context's `ui:` line)
    assert!(ed.ui_state_claude().sessions.iter().all(|l| l.overlaps.is_empty()));
    assert!(ed.toasts.iter().any(|t| t.text.contains("would conflict")), "the toast is up");
    let claudes = ed.ui_state_claude();
    assert!(claudes.toasts.iter().all(|t| !t.text.contains("conflict")), "{:?}", claudes.toasts);
    let line = claudes.context_line().map(|l| l.0).unwrap_or_default();
    assert!(!line.contains("conflict"), "{line}");
    assert!(!ed.ui_state().sessions.iter().all(|l| l.overlaps.is_empty()), "the person's level lists it");
}

/// A theme scope's colour as the frame paints it.
fn fg(ed: &Editor, scope: &str) -> vt100::Color {
    match ed.theme.try_get(scope).and_then(|s| s.fg) {
        Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        c => panic!("{scope}: {c:?}"),
    }
}

/// The frame, 160 wide.
fn frame(ed: &mut Editor) -> vt100::Parser {
    crossterm::style::force_color_output(true);
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, 160, 30).unwrap();
    let mut p = vt100::Parser::new(30, 160, 0);
    p.process(&bytes);
    p
}

/// The colour of `word`'s first cell on the row holding `row`, the first `word` there after `after`.
fn fg_at(p: &vt100::Parser, row: &str, after: &str, word: &str) -> vt100::Color {
    let rows: Vec<String> = p.screen().rows(0, 160).collect();
    let y = rows.iter().position(|r| r.contains(row)).unwrap_or_else(|| panic!("{row:?} in {rows:#?}"));
    let from =
        rows[y].find(row).unwrap() + rows[y][rows[y].find(row).unwrap()..].find(after).unwrap() + after.len();
    let at = from + rows[y][from..].find(word).unwrap_or_else(|| panic!("{word:?} in {}", rows[y]));
    p.screen().cell(y as u16, rows[y][..at].chars().count() as u16).unwrap().fgcolor()
}

/// Where you see it, each name in its colour and `≈` in warning wherever it's said (what needs you): the toast
/// (`fix ≈ main · a.rs L3`), the strip over the chat and fix's row of the list (`≈ main` after the name), and
/// `≈m` on the file in the side panel's tree (main's initial, bold in its colour), the row in hand in words at the
/// panel's foot.
#[test]
fn the_strip_the_list_and_the_tree_say_it() {
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene("overlap-shown");
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    settle(&mut ed, 1);
    let (fix_c, main_c, warning) = (
        fg(&ed, crate::session::PARTICIPANTS[ed.session.colour as usize]),
        fg(&ed, crate::session::PARTICIPANTS[ed.sessions[0].colour as usize]),
        fg(&ed, "warning"),
    );
    // The toast (whole, not fading in: what's read is its colours)
    ed.config.motion = "calm".into();
    let p = frame(&mut ed);
    let toast = "fix ≈ main · a.rs L3";
    assert_eq!(fg_at(&p, toast, "", "fix"), fix_c, "the toast's fix");
    assert_eq!(fg_at(&p, toast, "", "≈"), warning, "the toast's ≈");
    assert_eq!(fg_at(&p, toast, "≈", "main"), main_c, "the toast's main");
    assert_ne!(fg_at(&p, toast, "a.rs", "L3"), main_c, "the rest as the text");
    assert_eq!(others(&ed, fix_id), [("main".to_string(), Some(ed.sessions[0].colour))]);
    assert_eq!(others(&ed, 0), [("fix".to_string(), Some(ed.session.colour))], "both sides");
    // The list
    let rows = ed.sessions_rows();
    let fix_row = rows.iter().find(|r| r.name == "fix").unwrap();
    use crate::sessionpane::{Col, Ink};
    let main_colour = ed.sessions[0].colour;
    let overlap = fix_row.cell(Col::Overlap);
    assert_eq!(overlap, [("≈ ".to_string(), Ink::Warn), ("main".to_string(), Ink::Participant(main_colour))]);
    let main_row = rows.iter().find(|r| r.name == "main").unwrap();
    assert_eq!(main_row.text(Col::Overlap), "≈ fix");
    // The strip over a chat
    ed.session.chat = Some(crate::chat::Chat::new());
    ed.session.at = crate::session::At::Chat;
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 160, 30).unwrap();
    let mut p = vt100::Parser::new(30, 160, 0);
    p.process(&bytes);
    let strip =
        p.screen().rows(0, 160).find(|r| r.contains("· fix") || r.starts_with("main")).unwrap_or_default();
    assert!(strip.contains("main ≈ fix") && strip.contains("fix ≈ main"), "{strip}");
    let p = frame(&mut ed);
    assert_eq!(fg_at(&p, "fix ≈ main ·", "", "≈"), warning, "the strip's ≈");
    assert_eq!(fg_at(&p, "fix ≈ main ·", "≈", "main"), main_c, "the strip's main");
    // The tree
    ed.session.chat = None;
    ed.session.at = crate::session::At::Code;
    press(&mut ed, "<space>e");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let p = loop {
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 160, 30).unwrap();
        let mut p = vt100::Parser::new(30, 160, 0);
        p.process(&bytes);
        if !p.screen().contents().contains("listing files") {
            break p;
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        ed.handle_event(ed.events.recv_timeout(left).expect("the files listed"));
    };
    let all: Vec<String> = p.screen().rows(0, 160).collect();
    let row = all.iter().find(|r| r.contains("a.rs") && r.contains('≈')).cloned().unwrap_or_default();
    assert!(row.split('│').next().unwrap().trim_end().ends_with("≈m"), "the tree's row: {all:#?}");
    assert_eq!(fg_at(&p, "≈m", "", "≈"), warning, "the tree's ≈");
    assert_eq!(fg_at(&p, "≈m", "≈", "m"), main_c, "the tree's m");
    let y = all.iter().position(|r| r == &row).unwrap();
    let x = row[..row.find("≈m").unwrap()].chars().count() + 1;
    assert!(p.screen().cell(y as u16, x as u16).unwrap().bold(), "the initial bold");
    let b = p.screen().rows(0, 160).find(|r| r.contains("b.rs")).unwrap_or_default();
    assert!(!b.contains('≈'), "{b}");
    assert!(all.iter().any(|r| r.contains("a.rs — main would conflict L3")), "the foot: {all:#?}");
}

/// Two taraes on one repository each keep their own folder (`own_in`, its lock held while it lives): one ending its
/// `fix` removes nothing the other's kept snapshots stand on — the other, taking main again against its kept `fix`,
/// still says the overlap. A tarae gone leaves its folder; the next one's first round removes it, never a live one's.
#[test]
fn two_taraes_keep_their_own_folders_and_a_gone_ones_is_pruned() {
    let Scene { ed: mut a, main, fix, dir, .. } = scene("overlap-two");
    let base = dir.join("overlap");
    change(&mut a, &main.join("a.rs"), 3, "three, main");
    change(&mut a, &fix.join("a.rs"), 3, "three, fix");
    settle(&mut a, 1);
    let (mut b, b_fix) = editor_on(&main, &fix, &base);
    settle(&mut b, 1);
    assert_eq!((a.overlap.now.len(), b.overlap.now.len()), (1, 1), "each found it");
    let b_folder = b.overlap.folder(b_fix).unwrap();
    assert_ne!(a.overlap.dir, b.overlap.dir, "each its own");
    // (a ends its fix: its folder goes on a worker)
    let a_fix = a.overlap.folder(a.session.id).unwrap();
    let _ = a.session_close(&fix);
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while a_fix.exists() {
        assert!(std::time::Instant::now() < end, "a's fix folder removed");
        std::thread::yield_now();
    }
    assert!(b_folder.join("objects").is_dir(), "b's own untouched");
    // (b takes main again: merged against its kept fix — whose objects must still be there)
    change(&mut b, &main.join("a.rs"), 5, "five, main");
    settle(&mut b, 2);
    assert_eq!(
        b.overlap.now.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(),
        ["a.rs"],
        "b still says it"
    );
    // A tarae gone (its lock let go): its folder pruned by the next one's first round — b's, alive, kept
    let (mut c, _) = editor_on(&main, &fix, &base);
    settle(&mut c, 1);
    let c_dir = c.overlap.dir.clone().unwrap();
    assert!(c_dir.join(".lock").exists());
    drop(c);
    let (mut d, _) = editor_on(&main, &fix, &base);
    settle(&mut d, 1);
    assert!(!c_dir.exists(), "the gone tarae's folder pruned");
    assert!(
        b.overlap.dir.as_ref().unwrap().exists() && b_folder.join("objects").is_dir(),
        "a live one's kept"
    );
}

/// No program of the user's or the repository's runs for an overlap: a file a clean filter takes (`*.dat`, as
/// git-lfs's) stays as the checkout's index has it — not changed against its pointer, its filter never run — and a
/// conflict in a file with a merge driver (`*.m`) is merged by git's own (said all the same). The user's own config
/// sets both, never held: each would touch a marker in the test's folder.
#[test]
fn no_filter_or_merge_driver_runs_and_a_filtered_file_stays_as_its_index_has_it() {
    let marks = |d: &Path| d.join("marks");
    let dir_hint = Scratch::new("overlap-filters-marks");
    std::fs::create_dir_all(marks(&dir_hint)).unwrap();
    let m = marks(&dir_hint);
    let user = format!(
        // (each value in double quotes: a `;` outside them starts a comment in git's config)
        "[filter \"mark\"]\n\tclean = \"sh -c 'touch {0}/clean; cat'\"\n\tsmudge = cat\n\
         [merge \"mk\"]\n\tdriver = \"sh -c 'touch {0}/merge; exit 1'\"\n",
        m.display()
    );
    let more = [
        (".gitattributes", "*.dat filter=mark\n*.m merge=mk\n"),
        ("a.dat", "pointer\n"),
        ("x.m", "one\ntwo\nthree\n"),
    ];
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene_with("overlap-filters", &more, Some(&user));
    change(&mut ed, &fix.join("a.dat"), 1, "the large file's text, as the filter would take it");
    // (staged too, and a new one: still as HEAD has them — the pointer, and none)
    std::fs::write(fix.join("new.dat"), "another large file\n").unwrap();
    git(&fix, &["add", "a.dat", "new.dat"]);
    // (and changed again since: a text the repository has no blob of)
    change(&mut ed, &fix.join("a.dat"), 1, "the large file's text, changed since it was staged");
    change(&mut ed, &main.join("x.m"), 2, "two, main");
    change(&mut ed, &fix.join("x.m"), 2, "two, fix");
    settle(&mut ed, 1);
    let ran: Vec<_> = std::fs::read_dir(&m).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(ran.is_empty(), "nothing ran: {ran:?}");
    let snap = ed.overlap.places[&fix_id].snap.clone().expect("fix taken");
    assert!(
        !snap.changed.iter().any(|p| p.ends_with(".dat")),
        "the filtered files as HEAD has them, staged or new: {:?}",
        snap.changed
    );
    assert!(snap.changed.iter().any(|p| p == "x.m"), "the rest taken: {:?}", snap.changed);
    // (nor hashed: a large file's text never written into tarae's folder each round)
    let out = Command::new("git")
        .args(["hash-object", "--no-filters", "a.dat"])
        .current_dir(&fix)
        .output()
        .unwrap();
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let folder = ed.overlap.folder(fix_id).unwrap();
    assert!(!folder.join("objects").join(&id[..2]).join(&id[2..]).exists(), "a.dat's text hashed ({id})");
    let paths: Vec<&str> = ed.overlap.now.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(paths, ["x.m"], "merged by git's own driver");
}

/// No fsmonitor runs for an overlap, nor a renormalizing merge: the user's config sets `core.fsmonitor` to a hook and
/// `merge.renormalize = true`, with a filter on `*.dat` — the hook would touch a marker as a round's snapshot reads
/// its index (a round's git: its objects in tarae's folder — the session list's `git status` in fix runs the user's
/// hook as the user's own would),
/// the filter as the merge renormalizes the conflicted `c.dat` (both sides committed a change of its line 3) — and
/// git, given what every git of a round is given, reads both off.
#[test]
fn no_fsmonitor_or_renormalizing_merge_runs() {
    let marks = Scratch::new("overlap-fsmonitor-marks");
    let m = marks.join("marks");
    std::fs::create_dir_all(&m).unwrap();
    let hook = marks.join("fsmonitor.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncase \"$GIT_OBJECT_DIRECTORY\" in */overlap/*) touch '{}/fsmonitor';; esac\nexit 1\n",
            m.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let user = format!(
        "[core]\n\tfsmonitor = \"{0}\"\n[merge]\n\trenormalize = true\n\
         [filter \"mark\"]\n\tclean = \"sh -c 'touch {1}/clean; cat'\"\n\tsmudge = \"sh -c 'touch {1}/smudge; cat'\"\n",
        hook.display(),
        m.display()
    );
    let c = twelve();
    let more = [(".gitattributes", "*.dat filter=mark\n"), ("c.dat", c.as_str())];
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene_with("overlap-fsmonitor", &more, Some(&user));
    // (both sides commit a change of c.dat's line 3 — the test's own git, none of the user's config)
    let commit = |dir: &Path, text: &str| {
        let mut lines: Vec<String> = twelve().lines().map(str::to_string).collect();
        lines[2] = text.to_string();
        std::fs::write(dir.join("c.dat"), lines.join("\n") + "\n").unwrap();
        git(dir, &["commit", "-qam", text]);
    };
    commit(&main, "three, main");
    commit(&fix, "three, fix");
    change(&mut ed, &fix.join("a.rs"), 5, "five, fix");
    settle(&mut ed, 1);
    let paths: Vec<&str> = ed.overlap.now.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(paths, ["c.dat"], "the merge ran on the filtered file");
    let ran: Vec<_> = std::fs::read_dir(&m).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(ran.is_empty(), "nothing ran: {ran:?}");
    // (as git reads them, given a round's first words)
    let cmd = |args: &[&str]| crate::snapshot::command_env(&fix, args, None, &[]);
    let pre = guards(&cmd).unwrap();
    let read = |key: &str| {
        let args: Vec<&str> = pre.iter().map(String::as_str).chain(["config", "--get", key]).collect();
        let out = crate::gitrepo::finish(&cmd(&args).unwrap(), None).unwrap();
        String::from_utf8_lossy(&out).trim().to_string()
    };
    assert_eq!((read("core.fsmonitor"), read("merge.renormalize")), ("false".into(), "false".into()));
    // (and the user's, read without them: the config is in place)
    let out = crate::gitrepo::finish(&cmd(&["config", "--get", "merge.renormalize"]).unwrap(), None).unwrap();
    assert_eq!(String::from_utf8_lossy(&out).trim(), "true");
}

/// A tree that moved without a save tarae saw is taken again: its HEAD moved (`git_reload_bases_in`, as the HEAD
/// watch calls it) and a run of Claude's that changed files (`tools::run_changes`).
#[test]
fn a_head_move_or_a_runs_changes_take_the_tree_again() {
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene("overlap-moved");
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    settle(&mut ed, 1);
    assert!(ed.overlap.now.is_empty());
    // (fix's line 3 changed behind tarae's back, then its HEAD moves)
    let edit = |path: &Path, n: usize, text: &str| {
        let now = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> =
            now.lines().enumerate().map(|(i, l)| if i + 1 == n { text } else { l }).collect();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    };
    edit(&fix.join("a.rs"), 3, "three, fix");
    ed.git_reload_bases_in(&fix);
    settle(&mut ed, 2);
    assert_eq!(ed.overlap.now.len(), 1, "taken again on its HEAD's move");
    // (a run of Claude's in fix puts the line back: taken again when its changes are found)
    let snap = crate::snapshot::Before::take(&fix);
    edit(&fix.join("a.rs"), 3, "line 3");
    crate::tools::run_changes(&mut ed, snap, 0, "true".into(), |_, _| {});
    settle(&mut ed, 3);
    assert!(ed.overlap.now.is_empty(), "taken again after the run: {:?}", ed.overlap.now);
}

/// A partial clone missing the base tip's blob of a file both sides changed: the merge that needs it never fetches
/// it from the remote (`GIT_NO_LAZY_FETCH`) — the blob is still missing after, and nothing is said of that file.
#[test]
fn a_partial_clones_missing_blob_is_never_fetched() {
    let dir = Scratch::new("overlap-partial");
    // (the user's config lets file:// through, so a fetch would happen if git were let)
    std::fs::write(dir.join("user.gitconfig"), "[protocol \"file\"]\n\tallow = always\n").unwrap();
    crate::gitrepo::global_config_for(&dir, &dir.join("user.gitconfig"));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("a.rs"), twelve()).unwrap();
    git(&origin, &["init", "-q"]);
    git(&origin, &["config", "uploadpack.allowFilter", "true"]);
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-qm", "first"]);
    git(&origin, &["checkout", "-qb", "side"]);
    std::fs::write(origin.join("a.rs"), twelve().replace("line 3\n", "three, side\n")).unwrap();
    git(&origin, &["commit", "-qam", "side"]);
    git(&origin, &["checkout", "-q", "main"]);
    let url = format!("file://{}", origin.display());
    let clone = dir.join("main");
    let c = ["-c", "protocol.file.allow=always", "clone", "-q", "--filter=blob:none", "-b", "main"];
    git(&dir, &[&c[..], &[&url, clone.to_str().unwrap()]].concat());
    let clone = std::fs::canonicalize(clone).unwrap();
    git(&clone, &["branch", "-q", "side", "origin/side"]);
    git(&clone, &["worktree", "add", "-q", "../fix"]);
    let fix = std::fs::canonicalize(dir.join("fix")).unwrap();
    let missing = || {
        let out = Command::new("git")
            .args(["cat-file", "-e", "side:a.rs"])
            .current_dir(&clone)
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        !out.status.success()
    };
    assert!(missing(), "the stand-in: side's blob not in the clone");
    let (mut ed, fix_id) = editor_on(&clone, &fix, &dir.join("overlap"));
    ed.session_by_id(fix_id).unwrap().base = Some("side".into());
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    settle(&mut ed, 1);
    assert_eq!(ed.overlap.merged, 1, "the merge that needs the blob was run");
    assert!(missing(), "never fetched");
    assert!(ed.overlap.now.is_empty(), "nothing said of a merge that couldn't be made: {:?}", ed.overlap.now);
}

/// Another tarae's prune between this one's lock file being made and locked: it finds the lock free and removes
/// the folder — this one sees its lock is on a file no longer there and makes them again, so it ends holding
/// `own/.lock` itself, which a later prune leaves alone.
#[test]
fn a_prune_between_making_the_lock_and_taking_it_is_survived() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = Scratch::new("overlap-hold-race");
    let base = dir.join("overlap");
    let own = own_in(&base);
    let first = std::cell::Cell::new(true);
    let f = hold_with(&own, &|| {
        if first.replace(false) {
            assert_eq!(prune(&base, &own_in(&base)), 1, "the other's prune removed it (the race)");
        }
    })
    .unwrap();
    let there = std::fs::metadata(own.join(LOCK)).expect("its lock is there");
    let held = f.metadata().unwrap();
    assert_eq!((there.dev(), there.ino()), (held.dev(), held.ino()), "the lock held is own/.lock");
    assert_eq!(prune(&base, &own_in(&base)), 0, "a later prune leaves it");
    assert!(own.join(LOCK).exists());
}

/// This tarae's own lock busy a moment while it's still the file there — another tarae's prune just before its
/// removal: waited for and taken, never given up on.
#[test]
fn a_lock_busy_a_moment_is_waited_for() {
    use std::os::fd::AsRawFd as _;
    let dir = Scratch::new("overlap-hold-busy");
    let own = own_in(&dir.join("overlap"));
    let pruner: std::cell::RefCell<Option<std::fs::File>> = Default::default();
    let calls = std::cell::Cell::new(0);
    let f = hold_with(&own, &|| {
        calls.set(calls.get() + 1);
        match calls.get() {
            // (the first time: another's prune holds the lock on our file)
            1 => {
                let f = std::fs::OpenOptions::new().read(true).write(true).open(own.join(LOCK)).unwrap();
                // SAFETY: flock on a descriptor of the test's own.
                assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
                *pruner.borrow_mut() = Some(f);
            }
            // (then it lets go — its removal didn't happen: what's there is still ours)
            _ => drop(pruner.borrow_mut().take()),
        }
    })
    .expect("taken once the prune let go");
    assert!(calls.get() >= 2, "tried again");
    use std::os::unix::fs::MetadataExt as _;
    let (there, held) = (std::fs::metadata(own.join(LOCK)).unwrap(), f.metadata().unwrap());
    assert_eq!((there.dev(), there.ino()), (held.dev(), held.ino()));
}

/// A round whose lock can't be taken (here `.lock` is a folder) writes nothing in this tarae's folder and takes
/// no snapshot — a folder no lock marks is one another tarae's prune removes.
#[test]
fn a_round_without_its_lock_writes_nothing() {
    let Scene { mut ed, main, fix, fix_id, dir } = scene("overlap-no-lock");
    let own = own_in(&dir.join("overlap-elsewhere"));
    std::fs::create_dir_all(own.join(LOCK)).unwrap();
    ed.overlap.dir = Some(own.clone());
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    landed_to(&mut ed, 1);
    assert!(ed.overlap.waiting, "taken again on the quiet");
    // (`TRIES` times more, then it waits for the next change)
    settle(&mut ed, 1);
    assert_eq!(ed.overlap.rounds, 1 + u64::from(TRIES));
    let names: Vec<_> = std::fs::read_dir(&own).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(names, [std::ffi::OsString::from(LOCK)], "nothing written beside it");
    assert!(ed.overlap.places[&fix_id].snap.is_none() && ed.overlap.now.is_empty(), "nothing taken");
    assert_eq!(ed.overlap.merged, 0);
    // The lock free: the next change takes it, and the overlap
    std::fs::remove_dir(own.join(LOCK)).unwrap();
    change(&mut ed, &fix.join("a.rs"), 4, "four, fix");
    let next = ed.overlap.rounds + 1;
    settle(&mut ed, next);
    assert!(ed.overlap.lock.is_some());
    assert_eq!(ed.overlap.now.len(), 1, "found: {:?}", ed.overlap.now);
}

/// A take that failed (here a file where fix's folder goes) is taken again on the quiet, as any change: once the
/// cause is gone, the overlap is found. One that keeps failing is tried `TRIES` times more, then waits for its next
/// change — an idle tarae isn't woken every few seconds.
#[test]
fn a_take_that_failed_is_taken_again() {
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene("overlap-retake");
    sync(&mut ed);
    let folder = ed.overlap.folder(fix_id).unwrap();
    std::fs::create_dir_all(folder.parent().unwrap()).unwrap();
    std::fs::write(&folder, "in the way").unwrap();
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    landed_to(&mut ed, 1);
    assert!(ed.overlap.places[&fix_id].snap.is_none() && ed.overlap.now.is_empty(), "fix not taken");
    assert!(ed.overlap.waiting, "taken again on the quiet");
    std::fs::remove_file(&folder).unwrap();
    settle(&mut ed, 2);
    assert_eq!(ed.overlap.now.len(), 1, "found once it could be taken: {:?}", ed.overlap.now);
    // Failing for good: `TRIES` rounds more, then nothing armed
    std::fs::remove_dir_all(&folder).unwrap();
    std::fs::write(&folder, "in the way").unwrap();
    change(&mut ed, &fix.join("a.rs"), 4, "four, fix");
    let rounds = ed.overlap.rounds;
    settle(&mut ed, rounds + 1);
    assert_eq!(ed.overlap.rounds, rounds + 1 + u64::from(TRIES));
    assert!(!ed.overlap.waiting && !ed.overlap.running, "then it waits for its next change");
}

/// A take and a merge that fail a moment while an overlap stands — here a file where fix's folder goes: its take fails,
/// and so does its merge with main's moved tree (fix's objects were there) — leave the overlap as it was: not gone, not
/// said again; once fix is taken again it's the same overlap, still said once.
#[test]
fn an_overlap_stands_through_a_take_and_a_merge_that_failed() {
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene("overlap-stands");
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    settle(&mut ed, 1);
    assert_eq!(ed.overlap.now.len(), 1, "{:?}", ed.overlap.now);
    let folder = ed.overlap.folder(fix_id).unwrap();
    std::fs::remove_dir_all(&folder).unwrap();
    std::fs::write(&folder, "in the way").unwrap();
    change(&mut ed, &fix.join("a.rs"), 4, "four, fix");
    change(&mut ed, &main.join("a.rs"), 5, "five, main");
    let merged = ed.overlap.merged;
    landed_to(&mut ed, 2);
    assert_eq!(ed.overlap.merged, merged + 1, "main moved: their merge ran again (and failed)");
    assert_eq!(ed.overlap.now.len(), 1, "it stands: {:?}", ed.overlap.now);
    assert!(said(&ed, "no longer conflicts").is_empty(), "not gone");
    assert_eq!(said(&ed, "would conflict").len(), 1);
    // Fix's way clear: taken again on the quiet — the same overlap, not said again
    std::fs::remove_file(&folder).unwrap();
    settle(&mut ed, 3);
    assert!(ed.overlap.places[&fix_id].snap.as_ref().is_some_and(|s| s.changed.contains(&"a.rs".into())));
    assert_eq!(ed.overlap.now.len(), 1);
    assert!(said(&ed, "no longer conflicts").is_empty());
    assert_eq!(said(&ed, "would conflict").len(), 1, "said once");
}

/// A workspace's objects are bounded: past `objects_max` its folder starts anew — its next take writes it again, the
/// same commit — so many rounds of new text never grow it past the bound and one round's writes, and what's found
/// holds through it.
#[test]
fn a_workspaces_objects_stay_bounded() {
    let Scene { mut ed, main, fix, fix_id, dir: _dir } = scene("overlap-bounded");
    const MAX: u64 = 96 << 10;
    ed.overlap.objects_max = MAX;
    change(&mut ed, &main.join("a.rs"), 3, "three, main");
    change(&mut ed, &fix.join("a.rs"), 3, "three, fix");
    // (a text no round wrote before: 34 KB of hex, about half that compressed)
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = || {
        (0..2000)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                format!("{seed:016x}\n")
            })
            .collect::<String>()
    };
    let mut most = 0;
    for round in 1..=16 {
        std::fs::write(fix.join("big.txt"), noise()).unwrap();
        ed.wrote(&fix.join("big.txt"));
        settle(&mut ed, round);
        let objects = ed.overlap.folder(fix_id).unwrap().join("objects");
        let size: u64 = files(&objects).iter().map(|f| f.1).sum();
        most = most.max(size);
        assert_eq!(ed.overlap.now.len(), 1, "round {round}: still found: {:?}", ed.overlap.now);
    }
    assert!(ed.overlap.renewed >= 1, "started anew");
    assert!(most <= MAX + (64 << 10), "{most} bytes at most (the bound {MAX} and a round's)");
    // (every folder past its bound — main's too, unmoved since its first take: taken again all the same)
    ed.overlap.objects_max = 0;
    let renewed = ed.overlap.renewed;
    change(&mut ed, &fix.join("a.rs"), 4, "four, fix");
    settle(&mut ed, 17);
    assert!(ed.overlap.renewed >= renewed + 2, "both started anew");
    assert_eq!(ed.overlap.now.len(), 1, "still found: {:?}", ed.overlap.now);
}

/// What a prune leaves alone, whatever its lock says: a link named as a tarae's folder, a folder others can read, a
/// name not shaped `<pid>-<16 hex>`, a folder with no lock yet younger than `UNLOCKED_KEPT`, its own — and what it
/// removes: a folder whose lock is free, one with no lock past `UNLOCKED_KEPT`.
#[test]
fn a_prune_leaves_what_isnt_a_gone_taraes_folder() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Scratch::new("overlap-prune");
    let base = dir.join("overlap");
    std::fs::create_dir_all(&base).unwrap();
    let own = own_in(&base);
    let hex = "0123456789abcdef";
    // (each: a folder of the test's own, 0700, with a free lock unless said)
    let folder = |name: &str, lock: bool| {
        let p = base.join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        if lock {
            std::fs::write(p.join(LOCK), "").unwrap();
        }
        p
    };
    let gone = folder(&format!("1-{hex}"), true);
    let old = folder(&format!("2-{hex}"), false);
    let then = std::time::SystemTime::now() - UNLOCKED_KEPT - Duration::from_secs(60);
    std::fs::File::open(&old).unwrap().set_modified(then).unwrap();
    let young = folder(&format!("3-{hex}"), false);
    let open = folder(&format!("4-{hex}"), true);
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    let target = folder("elsewhere", true);
    let link = base.join(format!("5-{hex}"));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // (a link's own mode as a folder of a tarae's would have it, where the system lets one change: only the link
    // check stands between it and the prune — on Linux a link is always 0777, and the mode check stops it too)
    #[cfg(target_os = "macos")]
    {
        let c = std::ffi::CString::new(link.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a path of the test's own, NUL-ended.
        let set = unsafe { libc::fchmodat(libc::AT_FDCWD, c.as_ptr(), 0o700, libc::AT_SYMLINK_NOFOLLOW) };
        assert_eq!(set, 0, "the link's mode: {}", std::io::Error::last_os_error());
    }
    let shapes: Vec<PathBuf> = ["6-0123", "x-0123456789abcdef", "7-0123456789abcdeg", "8_0123456789abcdef"]
        .iter()
        .map(|n| folder(n, true))
        .collect();
    let mine = folder(own.file_name().unwrap().to_str().unwrap(), true);
    assert_eq!(prune(&base, &own), 2, "the gone ones only");
    assert!(!gone.exists() && !old.exists(), "a free lock, an old lockless one: removed");
    assert!(young.exists(), "a lockless one too young to tell");
    assert!(open.join(LOCK).exists(), "a folder others can read");
    let link_there = std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink());
    assert!(link_there && target.join(LOCK).exists(), "a link, and what it leads to");
    for s in &shapes {
        assert!(s.join(LOCK).exists(), "a name not a tarae's: {}", s.display());
    }
    assert!(mine.join(LOCK).exists(), "its own");
}

#[test]
fn regions_are_where_two_sides_changes_meet() {
    let base = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
    let a = "1\n2\nthree\n4\n5\n6\n7\n8\n9\n";
    let b = "1\n2\nTHREE\n4\n5\n6\n7\n8\nNINE\n";
    assert_eq!(regions(base, a, b), (Some(2..3), Some(2..3)));
    // (far apart: nothing meets)
    let a = "one\n2\n3\n4\n5\n6\n7\n8\n9\n";
    assert_eq!(regions(base, a, b), (None, None));
}
