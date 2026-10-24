//! Workspaces kept close (`keepclose.rs`): a fixture repository with three workspaces on `main` — `fix` (clean, a
//! change in no commit), `up` (clean) and `db` (its own commit on the line `main` changes) — and `main` moving.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::*;
use crate::config::Config;
use crate::key::{Code, Key};
use crate::scratch::Scratch;

/// The fixture's own git: its own name, no signing, no hooks, none of the machine's config.
fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn twelve() -> String {
    (1..=12).map(|n| format!("line {n}\n")).collect()
}

/// Line `n` (from 1) of `path` made `text`.
fn line(path: &Path, n: usize, text: &str) {
    let now = std::fs::read_to_string(path).unwrap();
    let lines: Vec<String> = now
        .lines()
        .enumerate()
        .map(|(i, l)| if i + 1 == n { text.to_string() } else { l.to_string() })
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

struct Scene {
    ed: Editor,
    main: PathBuf,
    /// fix, up, db: (session, checkout).
    ws: Vec<(SessionId, PathBuf)>,
    base: Base,
    _dir: Scratch,
}

impl Scene {
    fn id(&self, name: &str) -> SessionId {
        self.ws[["fix", "up", "db"].iter().position(|n| *n == name).unwrap()].0
    }

    fn root(&self, name: &str) -> PathBuf {
        self.ws[["fix", "up", "db"].iter().position(|n| *n == name).unwrap()].1.clone()
    }

    /// Whether `name`'s HEAD has main's tip under it.
    fn on_tip(&self, name: &str) -> bool {
        let tip = git(&self.main, &["rev-parse", "main"]);
        Command::new("git")
            .args(["merge-base", "--is-ancestor", &tip, "HEAD"])
            .current_dir(self.root(name))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success()
    }

    /// main's line 3 changed and committed in the main checkout, its tip said as the watch would.
    fn move_main(&mut self, text: &str) {
        line(&self.main.join("a.rs"), 3, text);
        git(&self.main, &["commit", "-qam", text]);
        let t = git(&self.main, &["rev-parse", "main"]);
        tip(&mut self.ed, self.base.clone(), t);
    }
}

/// The repository and its three workspaces, you in main; overlaps looked for at once, in the test's own folder; main's
/// tip read once (the baseline).
fn scene(name: &str) -> Scene {
    let dir = Scratch::new(name);
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    std::fs::write(main.join("a.rs"), twelve()).unwrap();
    std::fs::write(main.join("b.rs"), twelve()).unwrap();
    git(&main, &["init", "-q"]);
    // (tarae's own git replays the commits: who it is, unsigned — the repository's, so the machine's never counts)
    for (k, v) in [("user.name", "t"), ("user.email", "t@t"), ("commit.gpgsign", "false")] {
        git(&main, &["config", k, v]);
    }
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "first"]);
    let mut ed = Editor::new(Config::default());
    ed.session.root = main.clone();
    ed.session.name = "main".into();
    ed.session.git_branch = Some("main".into());
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.overlap.dir = Some(crate::overlap::own_in(&dir.join("overlap")));
    ed.overlap.quiet = Duration::ZERO;
    let mut ws = Vec::new();
    for name in ["fix", "up", "db"] {
        git(&main, &["worktree", "add", "-q", "-b", name, &format!("../{name}")]);
        let root = std::fs::canonicalize(dir.join(name)).unwrap();
        let id = ed.session_add(root.clone());
        let l = ed.session_by_id(id).unwrap();
        l.name = name.into();
        l.base = Some("main".into());
        l.git_branch = Some(name.into());
        ws.push((id, root));
    }
    // (db's own commit on the line main changes — a conflict; fix's change in no commit, far from it)
    let db = &ws[2].1;
    line(&db.join("a.rs"), 3, "line 3, db's");
    git(db, &["commit", "-qam", "db"]);
    line(&ws[0].1.join("b.rs"), 10, "line 10, fix's own");
    let common = crate::gitrepo::repo(&main).unwrap().common_dir.clone();
    let base = (common, "main".to_string());
    let t = git(&main, &["rev-parse", "main"]);
    tip(&mut ed, base.clone(), t);
    Scene { ed, main, ws, base, _dir: dir }
}

/// Events until `done` (20 s at most).
fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(20);
    ed.handle_event(crate::event::Event::Resize);
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("{what}: nothing came in time"));
        ed.handle_event(ev);
    }
}

/// Events while any come within a moment — what's in flight landed.
fn quiet(ed: &mut Editor) {
    ed.handle_event(crate::event::Event::Resize);
    while let Some(ev) = ed.events.recv_timeout(Duration::from_millis(300)) {
        ed.handle_event(ev);
    }
}

fn card(ed: &Editor) -> Option<&Offer> {
    ed.offers.iter().find(|o| matches!(o.what, What::BaseMoved(_)))
}

fn moves(ed: &Editor) -> &Moves {
    match &card(ed).expect("the base-moved card").what {
        What::BaseMoved(m) => m,
        _ => unreachable!(),
    }
}

/// `c` on the card, drawn and in sight long enough for a key to answer it.
fn key(ed: &mut Editor, c: char) {
    crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
    let o = card(ed).map(|o| (o.id, o.rev)).expect("the base-moved card");
    ed.card_focus = Some((o.0, o.1));
    ed.handle_key(Key::plain(Code::Char(c)));
}

fn said(ed: &Editor, part: &str) -> usize {
    ed.messages.iter().filter(|m| m.2.contains(part)).count()
}

/// The card asked once main moved: its workspaces behind it are named, and what would conflict where.
fn asked(s: &mut Scene) {
    s.move_main("line 3, main's");
    until(&mut s.ed, "the card", |ed| card(ed).is_some() && ed.keep_close.moving.is_none());
}

#[test]
fn one_card_names_every_workspace_behind_and_what_would_conflict_where() {
    let mut s = scene("keepclose-card");
    asked(&mut s);
    let q = card(&s.ed).unwrap().question.clone();
    assert_eq!(card(&s.ed).unwrap().title, "main moved");
    assert!(q.starts_with("1 new commit — 3 workspaces behind it:"), "{q}");
    assert!(q.contains("  fix — 1 behind, its changed file goes with it"), "{q}");
    assert!(q.contains("  up — 1 behind\n"), "{q}");
    assert!(q.contains("  db — 1 behind, would conflict in a.rs L3"), "{q}");
    assert!(q.contains("c rebases the conflicting one too"), "{q}");
    let answers: Vec<char> = card(&s.ed).unwrap().answers(false).iter().map(|a| a.0).collect();
    assert_eq!(answers, ['y', 'c', 'n']);
    // Each workspace named in its own colour, as the strip and the list name it (`Offer.paint`)
    let colour = |name: &str| {
        let c = s.ed.sessions.iter().find(|l| l.id == s.id(name)).unwrap().colour;
        crate::session::PARTICIPANTS[c as usize % crate::session::COLOURS]
    };
    let want: Vec<(String, &str)> = ["fix", "up", "db"].iter().map(|n| (n.to_string(), colour(n))).collect();
    assert_eq!(card(&s.ed).unwrap().paint, want);
    assert_eq!(want.iter().map(|w| w.1).collect::<std::collections::HashSet<_>>().len(), 3, "three colours");
    // (one card: a frame more, nothing more asked)
    quiet(&mut s.ed);
    assert_eq!(s.ed.offers.iter().filter(|o| matches!(o.what, What::BaseMoved(_))).count(), 1);
}

#[test]
fn y_moves_the_clean_ones_their_changes_along_and_says_each_once() {
    let mut s = scene("keepclose-y");
    asked(&mut s);
    key(&mut s.ed, 'y');
    assert!(card(&s.ed).is_none(), "{:?}", s.ed.messages.last());
    // (one at a time: never two checkouts moving at once)
    until(&mut s.ed, "two moves", |ed| {
        let moving = std::iter::once(&ed.session).chain(&ed.sessions).filter(|l| l.moving.is_some()).count();
        assert!(moving <= 1, "{moving} checkouts moving at once");
        ed.keep_close.moved == 2 && ed.keep_close.moving.is_none()
    });
    assert!(s.on_tip("fix") && s.on_tip("up"), "the clean ones moved");
    assert!(!s.on_tip("db"), "the conflicting one stays where it was");
    let b = std::fs::read_to_string(s.root("fix").join("b.rs")).unwrap();
    assert!(b.contains("line 10, fix's own"), "fix's change in no commit came along: {b}");
    let a = std::fs::read_to_string(s.root("fix").join("a.rs")).unwrap();
    assert!(a.contains("line 3, main's"), "{a}");
    assert_eq!(said(&s.ed, "fix · moved onto main — 1 commit under it, now at "), 1);
    assert_eq!(said(&s.ed, "up · moved onto main — 1 commit under it, now at "), 1);
    let all: Vec<&String> = s.ed.messages.iter().map(|m| &m.2).collect();
    assert_eq!(said(&s.ed, "db · moved"), 0, "{all:#?}");
    assert!(s.ed.keep_close.waits.is_empty());
}

#[test]
fn c_rebases_the_conflicting_one_too_through_its_resolve_loop() {
    let mut s = scene("keepclose-c");
    asked(&mut s);
    key(&mut s.ed, 'c');
    let db = s.id("db");
    until(&mut s.ed, "db's rebase stopped at its conflict", |ed| {
        let l = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == db).unwrap();
        l.rebase.as_ref().is_some_and(|r| r.stop.files == ["a.rs"])
    });
    until(&mut s.ed, "the clean ones", |ed| ed.keep_close.moved == 2 && ed.keep_close.moving.is_none());
    assert!(s.on_tip("fix") && s.on_tip("up"));
}

#[test]
fn n_leaves_them_and_the_next_move_asks_again() {
    let mut s = scene("keepclose-n");
    asked(&mut s);
    key(&mut s.ed, 'n');
    assert!(card(&s.ed).is_none());
    quiet(&mut s.ed);
    assert_eq!(s.ed.keep_close.moved, 0);
    assert!(!s.on_tip("fix") && !s.on_tip("up"));
    assert_eq!(said(&s.ed, "Left behind main"), 1);
    // (main moves again: asked again, from where they are)
    s.move_main("line 3, main's again");
    until(&mut s.ed, "the card again", |ed| card(ed).is_some());
    let q = &card(&s.ed).unwrap().question;
    assert!(q.starts_with("1 new commit —") && q.contains("fix — 2 behind"), "{q}");
}

#[test]
fn a_newer_tip_takes_the_cards_place() {
    let mut s = scene("keepclose-newer");
    asked(&mut s);
    let first = card(&s.ed).unwrap().id;
    s.move_main("line 3, main's newer");
    let newest = git(&s.main, &["rev-parse", "main"]);
    until(&mut s.ed, "the card on the newer tip", |ed| card(ed).is_some_and(|_| moves(ed).to == newest));
    assert_eq!(s.ed.offers.iter().filter(|o| matches!(o.what, What::BaseMoved(_))).count(), 1);
    assert_eq!(card(&s.ed).unwrap().id, first, "replaced in place, one card");
    // (its count from the first unanswered move)
    assert!(card(&s.ed).unwrap().question.starts_with("2 new commits —"));
    key(&mut s.ed, 'y');
    until(&mut s.ed, "two moves", |ed| ed.keep_close.moved == 2 && ed.keep_close.moving.is_none());
    let fix = git(&s.root("fix"), &["log", "-1", "--format=%s", "HEAD~0"]);
    assert_eq!(fix, "line 3, main's newer", "onto the newest tip");
}

/// A guard put on (true) or taken off.
type Guard = fn(&mut Scene, bool);

/// What keeps one workspace busy, put on `fix` — and taken off.
fn guards() -> Vec<(&'static str, Guard)> {
    vec![
        ("its Claude at work", |s, on| {
            let id = s.id("fix");
            let l = s.ed.session_by_id(id).unwrap();
            let c = l.chat.get_or_insert_with(|| {
                let mut c = crate::chat::Chat::new();
                let _ = crate::chat::stand_in(&mut c);
                c
            });
            c.state = if on { crate::chat::State::Thinking } else { crate::chat::State::Idle };
        }),
        ("unsaved buffers there", |s, on| {
            let path = s.root("fix").join("a.rs");
            s.ed.session_switch(s.id("fix"));
            s.ed.open(&path).unwrap();
            match on {
                true => {
                    for c in ['i', 'x'] {
                        s.ed.handle_key(Key::plain(Code::Char(c)));
                    }
                    s.ed.handle_key(Key::plain(Code::Esc));
                    assert!(s.ed.doc().is_modified());
                }
                false => s.ed.doc_mut().mark_saved(),
            }
            s.ed.session_switch(0);
        }),
        ("a review open", |s, on| {
            let (id, root) = (s.id("fix"), s.root("fix"));
            let turn = crate::codereview::Base::Turn { at: 0, question: "do it".into() };
            s.ed.session_by_id(id).unwrap().code_review =
                on.then(|| crate::codereview::CodeReview::for_test(turn, root, Vec::new()));
        }),
        ("a run under way", |s, on| {
            let (id, root) = (s.id("fix"), s.root("fix"));
            let task = crate::task::Task { name: "build".into(), cmd: "true".into(), cwd: root };
            let mut r = crate::task::Run::for_test(task);
            r.state = if on { crate::task::State::Running } else { crate::task::State::Passed };
            s.ed.session_by_id(id).unwrap().task_run = Some(r);
        }),
        ("in the bring queue", |s, on| {
            s.ed.bring_queue.run = on.then(|| crate::bringqueue::Run {
                left: [s.id("fix")].into(),
                now: None,
                brought: Vec::new(),
                out: Vec::new(),
                again: false,
                total: 1,
                names: Vec::new(),
            });
        }),
        // (a real one: stopped at a `break` in fix's checkout, taken up as tarae takes one up; `:rebase-abort` ends it)
        ("a rebase under way", |s, on| {
            let (id, root) = (s.id("fix"), s.root("fix"));
            match on {
                true => {
                    let editor = s._dir.join("break.sh");
                    std::fs::write(&editor, "#!/bin/sh\nprintf 'break\\n' > \"$1\"\n").unwrap();
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
                    let editor = format!("sequence.editor={}", editor.display());
                    git(&root, &["-c", &editor, "rebase", "-i", "--autostash", "HEAD"]);
                    let stop = crate::rebase::under_way(&root).expect("fix's rebase stopped");
                    s.ed.with_session(id, |ed| ed.rebase_adopt(stop));
                }
                false => {
                    s.ed.with_session(id, |ed| ed.rebase_abort().unwrap());
                }
            }
        }),
    ]
}

#[test]
fn a_busy_workspace_waits_until_its_idle() {
    for (why, set) in guards() {
        let mut s = scene(&format!("keepclose-busy-{}", why.replace(' ', "-")));
        asked(&mut s);
        set(&mut s, true);
        let fix = s.id("fix");
        let l = std::iter::once(&s.ed.session).chain(&s.ed.sessions).find(|l| l.id == fix).unwrap();
        assert_eq!(busy(&s.ed, l), Some(why));
        key(&mut s.ed, 'y');
        until(&mut s.ed, why, |ed| ed.keep_close.moved == 1 && ed.keep_close.moving.is_none());
        quiet(&mut s.ed);
        assert!(s.on_tip("up"), "{why}: the idle one moved");
        assert!(!s.on_tip("fix"), "{why}: fix moved while busy");
        assert!(s.ed.keep_close.waits.iter().any(|w| w.ws.id == fix), "{why}: fix still waits");
        set(&mut s, false);
        until(&mut s.ed, why, |ed| ed.keep_close.moved == 2 && ed.keep_close.moving.is_none());
        assert!(s.on_tip("fix"), "{why}: fix moved once idle");
    }
}

#[test]
fn while_it_moves_its_messages_and_runs_wait() {
    let mut s = scene("keepclose-holds");
    let fix = s.id("fix");
    s.ed.session_switch(fix);
    crate::chat::open(&mut s.ed);
    let rx = crate::chat::stand_in(s.ed.session.chat.as_mut().unwrap());
    s.ed.session.moving = Some("main".into());
    s.ed.session.chat.as_mut().unwrap().input.set("go on");
    crate::chat::send(&mut s.ed);
    let c = s.ed.session.chat.as_ref().unwrap();
    assert!(c.queued.is_some() && !c.busy(), "the message waits for the move");
    crate::chat::send_queued(&mut s.ed);
    assert!(s.ed.session.chat.as_ref().unwrap().queued.is_some(), "still waits");
    let task = crate::task::Task { name: "build".into(), cmd: "true".into(), cwd: s.root("fix") };
    s.ed.task_start(task);
    assert_eq!(s.ed.session.runs_next.len(), 1, "the run waits too");
    assert!(s.ed.session.task_run.is_none());
    s.ed.session.moving = None;
    crate::chat::send_queued(&mut s.ed);
    assert!(s.ed.session.chat.as_ref().unwrap().busy(), "sent once it's moved");
    assert!(rx.try_iter().any(|l| l.contains("go on")));
}

#[test]
fn a_save_while_it_moves_waits_and_is_written_once_it_lands() {
    let mut s = scene("keepclose-save");
    let fix = s.id("fix");
    let path = s.root("fix").join("b.rs");
    s.ed.session_switch(fix);
    s.ed.open(&path).unwrap();
    s.ed.session_switch(0);
    asked(&mut s);
    key(&mut s.ed, 'y');
    assert_eq!(s.ed.keep_close.moving, Some(fix), "fix moves first");
    // (typed and saved while git rebases its checkout)
    s.ed.session_switch(fix);
    for c in ['g', 'g', 'O', 'n', 'e', 'w'] {
        s.ed.handle_key(Key::plain(Code::Char(c)));
    }
    s.ed.handle_key(Key::plain(Code::Esc));
    let id = s.ed.doc().id;
    crate::typed::save_doc(&mut s.ed, id, false).unwrap();
    assert!(s.ed.doc().is_modified(), "held, not written mid-move");
    assert_eq!(s.ed.keep_close.saves.len(), 1);
    assert_eq!(said(&s.ed, "b.rs waits — fix is moving onto main; written once it lands"), 1);
    s.ed.session_switch(0);
    until(&mut s.ed, "both moved, the save written", |ed| {
        ed.keep_close.moved == 2 && ed.docs.iter().find(|d| d.id == id).is_some_and(|d| !d.is_modified())
    });
    assert!(s.on_tip("fix"));
    let b = std::fs::read_to_string(&path).unwrap();
    assert!(
        b.starts_with("new\n") && b.contains("line 10, fix's own"),
        "the save and fix's change both: {b}"
    );
    assert!(s.ed.keep_close.saves.is_empty());
}

#[test]
fn space_u_while_it_moves_is_refused_and_said_and_takes_it_back_once_it_lands() {
    let mut s = scene("keepclose-space-u");
    let fix = s.id("fix");
    let path = s.root("fix").join("b.rs");
    // (a turn of fix's Claude: its line 10, the change in no commit)
    let after = ropey::Rope::from_str(&std::fs::read_to_string(&path).unwrap());
    let t = crate::checkpoint::Touched {
        path: path.clone(),
        before: Some(ropey::Rope::from_str(&twelve())),
        after,
        ..Default::default()
    };
    let turn = crate::checkpoint::Checkpoint::new("line 10".into(), vec![t]);
    s.ed.session_by_id(fix).unwrap().checkpoints.list.push(turn);
    s.ed.session_switch(fix);
    s.ed.open(&path).unwrap();
    s.ed.session_switch(0);
    asked(&mut s);
    key(&mut s.ed, 'y');
    assert_eq!(s.ed.keep_close.moving, Some(fix), "fix moves first");
    s.ed.session_switch(fix);
    for c in [' ', 'u'] {
        s.ed.handle_key(Key::plain(Code::Char(c)));
    }
    assert_eq!(said(&s.ed, "fix is moving onto main — take it back once it lands"), 1);
    assert!(!s.ed.session.checkpoints.list[0].undone, "refused, nothing queued");
    // (`:w` to a new name there: refused the same way)
    for c in ":w c.rs".chars() {
        s.ed.handle_key(Key::plain(Code::Char(c)));
    }
    s.ed.handle_key(Key::plain(Code::Enter));
    assert_eq!(said(&s.ed, "fix is moving onto main — write it once it lands"), 1);
    assert!(!s.root("fix").join("c.rs").exists());
    s.ed.session_switch(0);
    until(&mut s.ed, "both moved", |ed| ed.keep_close.moved == 2 && ed.keep_close.moving.is_none());
    quiet(&mut s.ed);
    assert!(s.on_tip("fix"));
    let b = std::fs::read_to_string(&path).unwrap();
    assert!(b.contains("line 10, fix's own"), "not taken back by itself: {b}");
    s.ed.session_switch(fix);
    crate::checkpoint::step(&mut s.ed, true).unwrap();
    let b = std::fs::read_to_string(&path).unwrap();
    assert!(!b.contains("fix's own") && b.contains("line 10\n"), "taken back once it landed: {b}");
}

#[test]
fn a_stash_pushed_in_another_checkout_meanwhile_isnt_the_moves() {
    let s = scene("keepclose-stash-elsewhere");
    line(&s.main.join("a.rs"), 3, "line 3, main's");
    git(&s.main, &["commit", "-qam", "main"]);
    let up = s.root("up");
    let elsewhere = || {
        line(&up.join("b.rs"), 1, "line 1, up's");
        git(&up, &["stash", "push", "-q", "-m", "up's own"]);
    };
    let r = move_onto(&s.root("fix"), "main", &elsewhere);
    assert!(matches!(r, Landed::Done { kept: false, .. }), "{r:?}");
    assert!(s.on_tip("fix"));
    let b = std::fs::read_to_string(s.root("fix").join("b.rs")).unwrap();
    assert!(b.contains("line 10, fix's own"), "{b}");
    assert!(git(&s.main, &["stash", "list"]).contains("up's own"), "up's stash left as it is");
}

#[test]
fn changes_that_dont_go_back_after_the_move_are_said_in_the_stash() {
    let mut s = scene("keepclose-kept");
    asked(&mut s);
    // (main moves again on fix's changed line, unseen by any round: clean as predicted, its change won't go back)
    line(&s.main.join("b.rs"), 10, "line 10, main's");
    git(&s.main, &["commit", "-qam", "main on b.rs"]);
    key(&mut s.ed, 'y');
    until(&mut s.ed, "both moved", |ed| ed.keep_close.moved == 2 && ed.keep_close.moving.is_none());
    assert!(s.on_tip("fix"));
    let all: Vec<&String> = s.ed.messages.iter().map(|m| &m.2).collect();
    assert_eq!(said(&s.ed, "fix · moved onto main"), 1, "{all:#?}");
    assert_eq!(said(&s.ed, "its changes didn't go back cleanly — they're in git's stash"), 1, "{all:#?}");
    assert_eq!(said(&s.ed, "up · moved onto main — 2 commits under it, now at "), 1, "{all:#?}");
    assert!(git(&s.main, &["stash", "list"]).contains("autostash"));
}

#[test]
fn a_clean_one_that_conflicts_after_all_is_put_back_and_said() {
    let mut s = scene("keepclose-after-all");
    asked(&mut s);
    // (up commits on line 5 and main does too, unseen by any round; up's change in no commit comes back)
    let up = s.root("up");
    line(&up.join("a.rs"), 5, "line 5, up's");
    git(&up, &["commit", "-qam", "up"]);
    let head = git(&up, &["rev-parse", "HEAD"]);
    line(&up.join("b.rs"), 1, "line 1, up's own");
    line(&s.main.join("a.rs"), 5, "line 5, main's");
    git(&s.main, &["commit", "-qam", "main on line 5"]);
    key(&mut s.ed, 'y');
    until(&mut s.ed, "both looked at", |ed| ed.keep_close.moved == 2 && ed.keep_close.moving.is_none());
    let all: Vec<&String> = s.ed.messages.iter().map(|m| &m.2).collect();
    assert_eq!(said(&s.ed, "up · would conflict on main after all — left as it was"), 1, "{all:#?}");
    assert!(crate::rebase::under_way(&up).is_none(), "aborted");
    assert_eq!(git(&up, &["rev-parse", "HEAD"]), head, "its branch where it was");
    let b = std::fs::read_to_string(up.join("b.rs")).unwrap();
    assert!(b.contains("line 1, up's own"), "its change in no commit back: {b}");
    assert!(s.on_tip("fix"), "the other clean one moved");
}

#[test]
fn v_shows_the_instruction_files_the_main_checkout_changed_since_each_copy() {
    let mut s = scene("keepclose-instructions");
    let common = crate::gitrepo::repo(&s.main).unwrap().common_dir.clone();
    std::fs::write(common.join("info/exclude"), "CLAUDE.md\n").unwrap();
    std::fs::write(s.main.join("CLAUDE.md"), "rules\n").unwrap();
    // (fix's copy with its base; up's with its own line and no base — whose change is whose can't be told)
    std::fs::write(s.root("fix").join("CLAUDE.md"), "rules\n").unwrap();
    crate::instructions::keep_bases(&s.main, &s.root("fix"), &["CLAUDE.md".to_string()]);
    std::fs::write(s.root("up").join("CLAUDE.md"), "rules\nup's own\n").unwrap();
    std::fs::write(s.main.join("CLAUDE.md"), "rules\nmain's new\n").unwrap();
    asked(&mut s);
    let m = moves(&s.ed);
    let of = |name: &str| m.ws.iter().find(|w| w.name == name).unwrap().instructions.clone();
    assert_eq!(of("fix"), [("CLAUDE.md".to_string(), vec!["+ main's new".to_string()])]);
    assert_eq!(of("up"), [], "no base: left out");
    let answers: Vec<char> = card(&s.ed).unwrap().answers(false).iter().map(|a| a.0).collect();
    assert_eq!(answers, ['y', 'c', 'v', 'n']);
    key(&mut s.ed, 'v');
    let text = s.ed.doc().text.to_string();
    assert!(
        text.contains("fix · CLAUDE.md — the main checkout's changes since its copy\n+ main's new"),
        "{text}"
    );
    assert!(!text.contains("up's own") && !text.contains("up ·"), "{text}");
    assert!(card(&s.ed).is_some(), "the card stays");
}

#[test]
fn idle_moves_the_clean_ones_unasked_and_asks_only_for_the_conflicting() {
    let mut s = scene("keepclose-idle");
    s.ed.config.workspace_sync = "idle".into();
    s.move_main("line 3, main's");
    until(&mut s.ed, "the moves and the card", |ed| {
        card(ed).is_some() && ed.keep_close.moved == 2 && ed.keep_close.moving.is_none()
    });
    assert!(s.on_tip("fix") && s.on_tip("up") && !s.on_tip("db"));
    let m = moves(&s.ed);
    assert_eq!(m.ws.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["db"]);
    let q = &card(&s.ed).unwrap().question;
    assert!(q.contains("fix, up are moving by themselves (workspace.sync = \"idle\")"), "{q}");
    assert_eq!(said(&s.ed, "fix · moved onto main (workspace.sync = \"idle\")"), 1);
}

#[test]
fn off_leaves_them_unasked() {
    let mut s = scene("keepclose-off");
    s.ed.config.workspace_sync = "off".into();
    s.move_main("line 3, main's");
    quiet(&mut s.ed);
    assert!(card(&s.ed).is_none());
    assert_eq!(s.ed.keep_close.moved, 0);
}

#[test]
fn the_tip_is_read_from_the_loose_ref_or_packed_refs_and_never_past_refs_heads() {
    let s = scene("keepclose-tip");
    let (common, _) = &s.base;
    let t = git(&s.main, &["rev-parse", "main"]);
    assert_eq!(read_tip(common, "main").as_deref(), Some(t.as_str()));
    git(&s.main, &["pack-refs", "--all"]);
    assert!(!common.join("refs/heads/main").exists());
    assert_eq!(read_tip(common, "main").as_deref(), Some(t.as_str()), "from packed-refs");
    for bad in ["../HEAD", "a/../../HEAD", "", "/main", "main/"] {
        assert_eq!(read_tip(common, bad), None, "{bad}");
    }
    assert_eq!(read_tip(common, "nope"), None);
}
