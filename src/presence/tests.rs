//! Presence (`presence.rs`): where each session is, mapped across checkouts, drawn, followed.

use std::process::Command;

use super::*;
use crate::chat::{Look, State};
use crate::config::Config;
use crate::event::{Mouse, MouseKind};
use crate::key::{Code, Key};
use crate::scratch::Scratch;

const TEXT: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";

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

struct Scene {
    ed: Editor,
    /// The repository's first checkout, and the second (a worktree) — real paths.
    main: PathBuf,
    other: PathBuf,
    /// The second's session (in front).
    fix: SessionId,
    _dir: Scratch,
}

/// A repository (`main/`, `a.rs` committed) and a second checkout of it (`fix/`, a worktree) whose `a.rs` has two
/// lines more at its top: you in `fix` with its `a.rs` open, the main checkout's session behind it, its Claude at
/// work (follow on).
fn scene(name: &str) -> Scene {
    let dir = Scratch::new(name);
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    std::fs::write(main.join("a.rs"), TEXT).unwrap();
    std::fs::write(main.join("only.rs"), "// main's alone\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "first"]);
    git(&main, &["worktree", "add", "-q", "../fix"]);
    let other = std::fs::canonicalize(dir.join("fix")).unwrap();
    std::fs::write(other.join("a.rs"), format!("// one\n// two\n{TEXT}")).unwrap();
    let mut ed = Editor::new(Config::default());
    ed.session.root = main.clone();
    ed.session.name = "main".into();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    let mut c = crate::chat::Chat::new();
    let _ = crate::chat::stand_in(&mut c);
    c.state = State::Thinking;
    c.follow.on = true;
    ed.session.chat = Some(c);
    let fix = ed.session_add(other.clone());
    ed.session_by_id(fix).unwrap().name = "fix".into();
    assert!(ed.session_switch(fix));
    ed.open(&other.join("a.rs")).unwrap();
    Scene { ed, main, other, fix, _dir: dir }
}

/// The main checkout's Claude looks at `lines` of its `file` (a read's gaze).
fn gaze(ed: &mut Editor, path: PathBuf, lines: Range<usize>) {
    let l = ed.sessions.iter_mut().chain(std::iter::once(&mut ed.session)).find(|l| l.id == 0).unwrap();
    let look = Look { verb: "read", what: String::new(), pattern: false, range: String::new(), link: None };
    l.chat.as_mut().unwrap().follow.gaze = Some(crate::follow::Gaze::new(look, Some(path), Some(lines)));
}

/// The main checkout's Claude wrote `path` a moment ago: its tag shows there for `TAG_STAY`.
fn just_wrote(ed: &mut Editor, path: PathBuf) {
    ed.presence.wrote.insert((0, path), crate::clock::now());
}

/// Events until `done` holds (a line map is worked out on a worker).
fn settle(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
    sync(ed);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let Some(ev) = ed.events.recv_timeout(left) else {
            let maps: Vec<_> =
                ed.presence.maps.iter().map(|(k, v)| (k, v.of, v.map.is_some(), v.asking)).collect();
            panic!(
                "no event before the time's up — now {:#?}, same {:?}, maps {maps:?}, doc {:?}",
                ed.presence.now,
                ed.presence.same,
                ed.doc().path
            );
        };
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

/// The frame as a terminal shows it, `w` wide: its rows, and the screen to read cells from.
fn frame(ed: &mut Editor, w: u16) -> vt100::Parser {
    crossterm::style::force_color_output(true);
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, w, 30).unwrap();
    let mut parser = vt100::Parser::new(30, w, 0);
    parser.process(&bytes);
    parser
}

fn rows(p: &vt100::Parser) -> Vec<String> {
    p.screen().rows(0, p.screen().size().1).collect()
}

/// Session `id`'s participant colour at full strength, as the frame paints it.
fn colour_of(ed: &Editor, id: SessionId) -> vt100::Color {
    let l = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == id).unwrap();
    let k = crate::session::PARTICIPANTS[l.colour as usize];
    match ed.theme.try_get(k).and_then(|s| s.fg) {
        Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        c => panic!("{k}: {c:?}"),
    }
}

/// The row whose text ends with `needle` (or with `needle` and a name tag after it).
fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle:?} in {rows:#?}"))
}

/// The sign column's cell of row `y` — the code's first (no side panel: the pane starts at the screen's edge).
fn sign(p: &vt100::Parser, y: usize) -> (String, vt100::Color) {
    let c = p.screen().cell(y as u16, 0).unwrap();
    (c.contents().to_string(), c.fgcolor())
}

/// A's lines in sight in the front pane, mapped: the marks of the main checkout's Claude.
fn main_marks(ed: &Editor) -> Vec<Mark> {
    marks(ed, ed.doc()).into_iter().filter(|m| m.session == 0).collect()
}

/// A Claude's gaze in another checkout of the repository is drawn on the same file here at its lines as they are
/// here (two lines more on top: two lines down) — `▎` in the sign column, dim, its name tag at the first line's end;
/// the list says where it is, the files tab marks the file. A file it isn't in shows nothing.
#[test]
fn presence_maps_lines_across_checkouts() {
    let Scene { mut ed, main, other, _dir, .. } = scene("presence-maps");
    gaze(&mut ed, main.join("a.rs"), 1..3);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    let m = &main_marks(&ed)[0];
    assert_eq!((m.lines.clone(), m.act, m.here, m.followed), (3..5, Act::Reading, true, false));
    // (its tag: only while it says something new — here a write there a moment ago)
    just_wrote(&mut ed, main.join("a.rs"));
    let p = frame(&mut ed, 100);
    let r = rows(&p);
    let (a, b, c) = (row_of(&r, "let a = 1;"), row_of(&r, "let b = 2;"), row_of(&r, "let c = 3;"));
    assert!(r[a].trim_end().ends_with("let a = 1;   ‹main reading›"), "{}", r[a]);
    assert!(!r[b].contains('‹'), "the tag on its first line only: {}", r[b]);
    // (each session its own colour, for its life — main's not fix's)
    assert_ne!(ed.session.colour, ed.sessions[0].colour);
    let accent = colour_of(&ed, 0);
    for y in [a, b] {
        let (g, fg) = sign(&p, y);
        assert_eq!(g, "▎", "row {y}: {}", r[y]);
        assert_ne!(fg, accent, "main's colour, dim: not the one you follow");
        assert!(matches!(fg, vt100::Color::Rgb(..)), "a colour of its own, not the default");
    }
    assert_eq!(sign(&p, c).0, " ", "none past its lines");
    // Where each is: the list's row
    let at = ed.session_items();
    assert!(at.iter().any(|i| i.label == "main" && i.hint.contains("a.rs:2 · reading")), "{at:#?}");
    // …the UI state's; never Claude's level (C1 tells Claude nothing)
    let main_at =
        |s: crate::uistate::UiState| s.sessions.iter().find(|s| s.name == "main").and_then(|s| s.at.clone());
    assert_eq!(main_at(ed.ui_state()).as_deref(), Some("a.rs:2 · reading"));
    assert_eq!(main_at(ed.ui_state_claude()), None);
    assert!(ed.ui_state().json()["sessions"].to_string().contains("a.rs:2 · reading"));
    // The files tab: main's initial on the file
    ed.session.sidebar =
        Some(crate::sidebar::Sidebar::with_files(other.clone(), vec!["a.rs".into(), "b.rs".into()]));
    let r = rows(&frame(&mut ed, 100));
    let last_word = |r: &str| r.split('│').next().unwrap_or(r).split_whitespace().last().map(str::to_string);
    assert!(r.iter().any(|r| r.contains("a.rs") && last_word(r).as_deref() == Some("m")), "{r:#?}");
    assert!(r.iter().any(|r| r.contains("b.rs") && last_word(r).as_deref() == Some("b.rs")), "{r:#?}");
    // The strip over a chat: its file after its mark (where the chat's line has room for it)
    ed.session.chat = Some(crate::chat::Chat::new());
    let r = rows(&frame(&mut ed, 160));
    assert!(r.iter().any(|r| r.contains("main working a.rs")), "{r:#?}");
    ed.session.chat = None;
    // A file it isn't in: nothing
    gaze(&mut ed, main.join("only.rs"), 0..1);
    sync(&mut ed);
    assert!(main_marks(&ed).is_empty());
}

/// The map follows this buffer: a line typed above the lines moves the mark down with them once its map is worked
/// out again (the last one drawn meanwhile); the turn over, the mark and the tag go; and a presence held wakes
/// nothing — no job, no event, while nothing moves.
#[test]
fn the_mark_follows_the_buffer_and_goes_with_the_turn() {
    let Scene { mut ed, main, _dir, .. } = scene("presence-follows");
    gaze(&mut ed, main.join("a.rs"), 1..2);
    settle(&mut ed, |ed| !main_marks(ed).is_empty() && ed.presence.asking_same.is_empty());
    assert_eq!(main_marks(&ed)[0].lines, 3..4);
    // (held: nothing asked again — no job, so no event)
    sync(&mut ed);
    assert!(
        ed.presence.maps.values().all(|k| !k.asking) && ed.presence.asking_same.is_empty(),
        "a held presence asked again"
    );
    press(&mut ed, "ggO// three<esc>");
    settle(&mut ed, |ed| main_marks(ed).first().is_some_and(|m| m.lines == (4..5)));
    // The turn over: gone
    let l = ed.sessions.iter_mut().find(|l| l.id == 0).unwrap();
    l.chat.as_mut().unwrap().state = State::Idle;
    sync(&mut ed);
    assert!(main_marks(&ed).is_empty());
    let r = rows(&frame(&mut ed, 100));
    assert!(!r.iter().any(|r| r.contains('‹')), "{r:#?}");
}

/// The other checkout's file open nowhere: a change on disk from elsewhere (a save in another editor, a `git
/// checkout`) moves its stamp — looked at again on an event once `LOOK` has gone by, never sooner — and the map is
/// worked out again: the marks on the lines they are now.
#[test]
fn a_change_on_disk_from_elsewhere_maps_the_lines_again() {
    let Scene { mut ed, main, _dir, .. } = scene("presence-disk");
    gaze(&mut ed, main.join("a.rs"), 1..3);
    settle(&mut ed, |ed| !main_marks(ed).is_empty() && ed.presence.asking_same.is_empty());
    assert_eq!(main_marks(&ed)[0].lines, 3..5);
    // The main checkout's a.rs becomes fix's (the two lines on top), written by no one here
    std::fs::write(main.join("a.rs"), format!("// one\n// two\n{TEXT}")).unwrap();
    sync(&mut ed);
    assert!(ed.presence.disk.values().all(|s| !s.asking), "looked at again sooner than LOOK");
    crate::clock::advance(LOOK);
    settle(&mut ed, |ed| main_marks(ed).first().is_some_and(|m| m.lines == (1..3)));
}

/// One session open: nothing new is drawn — no one else to show (the status line's `● follow` without a name).
#[test]
fn one_session_shows_no_presence() {
    let Scene { mut ed, main, fix, _dir, .. } = scene("presence-one");
    assert!(ed.session_switch(0));
    ed.sessions.retain(|l| l.id != fix);
    gaze(&mut ed, main.join("a.rs"), 1..2);
    ed.open(&main.join("a.rs")).unwrap();
    sync(&mut ed);
    assert!(ed.presence.now.is_empty() && marks(&ed, ed.doc()).is_empty());
}

/// `space F`: the next session at work followed — into its checkout, the editor where its Claude is, `● follow` on the
/// status line and the chip after it saying whom (`● main`), its tag and mark in its colour at full strength; your key stops it there, said; `space F` again follows it
/// again.
#[test]
fn space_f_follows_the_next_session_at_work() {
    let Scene { mut ed, main, fix, _dir, .. } = scene("presence-follow-next");
    gaze(&mut ed, main.join("a.rs"), 2..3);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    assert_eq!(ed.session.id, fix);
    press(&mut ed, "<space>F");
    assert_eq!(ed.session.id, 0, "into its checkout");
    assert_eq!(
        (ed.doc().path.clone(), crate::follow::view_line(&ed), ed.cursor_line()),
        (Some(main.join("a.rs")), Some(2), 0),
        "the view where its Claude is, your cursor where it was"
    );
    assert_eq!(followed(&ed), Some(0));
    just_wrote(&mut ed, main.join("a.rs"));
    let p = frame(&mut ed, 100);
    let r = rows(&p);
    let status = r.last().unwrap();
    assert!(status.contains("● follow   main"), "the word alone, whom the chip: {status}");
    let b = row_of(&r, "let b = 2;");
    assert!(r[b].trim_end().ends_with("‹main reading›"), "{}", r[b]);
    assert_eq!(sign(&p, b), ("▎".to_string(), colour_of(&ed, 0)), "the one you follow: its colour, full");
    // Your key: stopped there, acting at your cursor
    press(&mut ed, "j");
    assert_eq!((ed.session.id, ed.cursor_line()), (0, 1));
    assert_eq!(followed(&ed), None);
    assert!(ed.toasts.iter().any(|t| t.text.contains("you took over")), "said");
    let r = rows(&frame(&mut ed, 100));
    assert!(r.last().unwrap().contains("● normal"), "{}", r.last().unwrap());
    // Again: followed again (the only one at work)
    press(&mut ed, "<space>F");
    assert_eq!((followed(&ed), crate::follow::view_line(&ed), ed.cursor_line()), (Some(0), Some(2), 1));
}

/// Two Claudes on one line — fix's own, reading its file, followed, and main's in its checkout — each its tag in its
/// own participant colour: the one you follow first and full, the other dim; never the accent.
#[test]
fn two_sessions_on_one_line_each_its_colour() {
    let Scene { mut ed, main, other, fix, _dir } = scene("presence-two");
    gaze(&mut ed, main.join("a.rs"), 1..3);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    let mut c = crate::chat::Chat::new();
    let _ = crate::chat::stand_in(&mut c);
    c.state = State::Thinking;
    c.follow.on = true;
    let look = Look { verb: "read", what: String::new(), pattern: false, range: String::new(), link: None };
    c.follow.gaze = Some(crate::follow::Gaze::new(look, Some(other.join("a.rs")), Some(3..5)));
    ed.session.chat = Some(c);
    sync(&mut ed);
    // (both wrote there a moment ago: each its tag)
    let at = crate::clock::now();
    ed.presence.wrote.insert((0, main.join("a.rs")), at);
    ed.presence.wrote.insert((fix, other.join("a.rs")), at);
    ed.session.at = crate::session::At::Code;
    let p = frame(&mut ed, 120);
    let r = rows(&p);
    let y = row_of(&r, "let a = 1;");
    assert!(r[y].trim_end().ends_with("‹fix reading› ‹main reading›"), "{}", r[y]);
    let fg_at = |needle: &str| {
        let x = r[y][..r[y].find(needle).unwrap()].chars().count() + 1;
        p.screen().cell(y as u16, x as u16).unwrap().fgcolor()
    };
    let (theirs, mine) = (fg_at("‹main"), fg_at("‹fix"));
    assert_eq!(mine, colour_of(&ed, fix), "the one you follow: its colour, full");
    assert_ne!(theirs, colour_of(&ed, 0), "the other: its colour, dim");
    assert_ne!(theirs, mine, "each its own");
    assert_ne!(mine, claude_accent(&ed), "never the accent");
}

fn claude_accent(ed: &Editor) -> vt100::Color {
    match ed.theme.try_get("ui.accent").and_then(|s| s.fg) {
        Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        c => panic!("ui.accent: {c:?}"),
    }
}

/// A session is named one way in the chrome — its name in its participant colour, full, no `●` before it, its state
/// after it (main at work, fix in front, docs waiting on a card behind — its `▲` in warning): on its row of the list
/// (`space W`), on the chat's top line and folded there, in the status line's chip, and there after `● follow` (the
/// word alone: the chip says whom). A branch is a branch: no participant's colour.
#[test]
fn every_session_is_named_one_way_in_the_chrome() {
    let Scene { mut ed, main, _dir, .. } = scene("presence-colours");
    gaze(&mut ed, main.join("a.rs"), 1..2);
    let docs = _dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let third = ed.session_add(docs);
    ed.session_by_id(third).unwrap().name = "docs".into();
    let all: Vec<(SessionId, String)> =
        ed.sessions_in_order().iter().map(|l| (l.id, l.label().to_string())).collect();
    let colours: Vec<vt100::Color> = all.iter().map(|(id, _)| colour_of(&ed, *id)).collect();
    assert!(colours[0] != colours[1] && colours[1] != colours[2] && colours[0] != colours[2], "each its own");
    // (docs waits on a card of its Claude's, behind)
    let what = crate::offer::What::Screen { call: crate::tools::Call::test(1) };
    let card = crate::offer::Offer::new("Claude asks".into(), "to see the screen", "", String::new(), what);
    ed.session_by_id(third).unwrap().parked.push(card);
    let warning = match ed.theme.try_get("warning").and_then(|s| s.fg) {
        Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        c => panic!("warning: {c:?}"),
    };
    // (`name` on row `y`, from byte `from`: its first cell's colour — and no `●` before it)
    let named = |p: &vt100::Parser, rows: &[String], y: usize, name: &str, from: usize| {
        let at = from + rows[y][from..].find(name).unwrap_or_else(|| panic!("{name:?} in {}", rows[y]));
        assert!(!rows[y][..at].trim_end().ends_with('●'), "no dot before {name}: {}", rows[y]);
        p.screen().cell(y as u16, rows[y][..at].chars().count() as u16).unwrap().fgcolor()
    };
    let fg_of = |p: &vt100::Parser, rows: &[String], y: usize, s: &str| {
        let at = rows[y].find(s).unwrap_or_else(|| panic!("{s:?} in {}", rows[y]));
        p.screen().cell(y as u16, rows[y][..at].chars().count() as u16).unwrap().fgcolor()
    };
    // The list: each row's name in its colour, no glyph; what needs you (docs' `▲ waits`) in warning
    press(&mut ed, "<space>W");
    let p = frame(&mut ed, 160);
    let r = rows(&p);
    for ((_, name), &c) in all.iter().zip(&colours) {
        assert_eq!(
            named(&p, &r, row_of(&r, &format!("  {name} ")), name, 0),
            c,
            "{name} in the list: {r:#?}"
        );
    }
    let docs_row = row_of(&r, "▲ waits ");
    assert_eq!(fg_of(&p, &r, docs_row, "▲"), warning, "what needs you, in warning: {}", r[docs_row]);
    press(&mut ed, "q");
    // The strip over a chat: each name in its colour, its state after it — docs' `▲ waits` in warning (the chat with
    // the keys: a surface without them fades all its text alike)
    ed.session.chat = Some(crate::chat::Chat::new());
    ed.session.at = crate::session::At::Chat;
    let p = frame(&mut ed, 160);
    let r = rows(&p);
    let y = r.iter().position(|r| r.contains("main working") && r.contains("· docs")).expect("the strip");
    let strip = r[y].find("main").unwrap();
    for ((_, name), &c) in all.iter().zip(&colours) {
        assert_eq!(named(&p, &r, y, name, strip), c, "{name} in the strip: {}", r[y]);
    }
    assert_eq!(fg_of(&p, &r, y, "▲"), warning, "docs waits, in warning: {}", r[y]);
    // (narrow, the states folded to what needs you: each name still in its colour)
    let fix = all.iter().position(|(_, n)| n == "fix").unwrap();
    let p = frame(&mut ed, 80);
    let r = rows(&p);
    let y = row_of(&r, "main · fix · docs ▲ · +");
    assert_eq!(named(&p, &r, y, "fix", r[y].find("main").unwrap()), colours[fix], "folded: {}", r[y]);
    // (no participant's colour that isn't a name's: a branch is a branch)
    let ink = |c: &vt100::Color| !colours.contains(c);
    // The status line with the chat open (no chip): a branch is a branch — the one that is the session's name too
    let branch_fg = |ed: &mut Editor, b: &str| {
        ed.session.git_branch = Some(b.into());
        let p = frame(ed, 160);
        let status = rows(&p).pop().unwrap();
        let x = status.rfind(&format!("   {b}")).unwrap_or_else(|| panic!("the branch: {status}")) + 3;
        p.screen().cell(29, status[..x].chars().count() as u16).unwrap().fgcolor()
    };
    for b in ["fix", "login-form"] {
        assert!(ink(&branch_fg(&mut ed, b)), "the branch {b}: no participant's colour");
    }
    ed.session.git_branch = None;
    // The chat closed: the chip names the front session as the strip does, `fix` in its colour — the one `●` the mode's
    ed.session.chat = None;
    ed.session.at = crate::session::At::Code;
    let chip = |ed: &mut Editor, name: &str, mode: &str| {
        let p = frame(ed, 160);
        let r = rows(&p);
        let status = r.last().unwrap().clone();
        let after = status.find(mode).unwrap_or_else(|| panic!("{mode}: {status}")) + mode.len();
        (named(&p, &r, 29, name, after), status)
    };
    let (fg, status) = chip(&mut ed, "fix", "normal");
    assert_eq!(fg, colours[fix], "the chip: {status}");
    assert_eq!(status.matches('●').count(), 1, "the mode's dot only: {status}");
    // Following main (`space F`: into its workspace): `● follow` alone, the chip after it `main` in its colour
    press(&mut ed, "<space>F");
    assert_eq!(followed(&ed), Some(0));
    let main = all.iter().position(|(id, _)| *id == 0).unwrap();
    let (fg, status) = chip(&mut ed, "main", "follow");
    assert!(status.contains("● follow   main") && status.matches('●').count() == 1, "{status}");
    assert_eq!(fg, colours[main], "followed: {status}");
}

/// A click on a session's name tag follows it; `f` on its row in the list too.
#[test]
fn a_click_on_the_tag_or_f_in_the_list_follows() {
    let Scene { mut ed, main, fix, _dir, .. } = scene("presence-follow-click");
    gaze(&mut ed, main.join("a.rs"), 1..2);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    just_wrote(&mut ed, main.join("a.rs"));
    let r = rows(&frame(&mut ed, 100));
    let y = row_of(&r, "‹main reading›");
    let x = r[y][..r[y].find('‹').unwrap()].chars().count() + 3;
    ed.handle_mouse(Mouse { kind: MouseKind::Down, x: x as u16, y: y as u16, alt: false });
    assert_eq!((ed.session.id, followed(&ed)), (0, Some(0)), "followed from its tag");
    // Back in fix; the list's `f`
    assert!(ed.session_switch(fix));
    press(&mut ed, "<space>W");
    let main_row = ed.session_items().iter().position(|i| i.label == "main").unwrap();
    let here = ed.session_items().iter().position(|i| i.label == "fix").unwrap();
    let down = if main_row > here { "j" } else { "k" };
    press(&mut ed, down);
    let r = rows(&frame(&mut ed, 160));
    assert!(r.iter().any(|r| r.contains("f follow")), "the list says it: {r:#?}");
    press(&mut ed, "f");
    assert!(!ed.sessions_list_keys(), "the keys there");
    assert_eq!((ed.session.id, followed(&ed)), (0, Some(0)), "followed from the list");
}

/// A session's name tag shows only while it says something new (the user, 2026-10-22): a Claude only reading there —
/// its bar alone; the mouse over its bar — its tag, gone as the mouse leaves; your cursor on its line — its tag; a
/// write there — its tag, `TAG_STAY` after, then a `TAG_FADE` fade (a frame wanted at each, none past them: an idle
/// tarae draws nothing), then gone.
#[test]
fn a_tag_shows_only_while_it_says_something_new() {
    let Scene { mut ed, main, _dir, .. } = scene("presence-tag-new");
    gaze(&mut ed, main.join("a.rs"), 1..2);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    let tag = |ed: &mut Editor| rows(&frame(ed, 100)).iter().any(|r| r.contains("‹main"));
    let bar = |ed: &mut Editor| {
        let p = frame(ed, 100);
        let r = rows(&p);
        let y = row_of(&r, "let a = 1;");
        (sign(&p, y).0, y)
    };
    assert_eq!(main_marks(&ed)[0].tag, 0);
    let (g, y) = bar(&mut ed);
    assert_eq!(g, "▎", "its bar stays");
    assert!(!tag(&mut ed), "reading only: no tag");
    assert_eq!(tag_wake(&ed), None, "nothing on a clock");
    // The mouse over its bar, then away
    ed.handle_event(crate::event::Event::Hover(0, y as u16));
    assert!(tag(&mut ed), "over its mark: its tag");
    ed.handle_event(crate::event::Event::Hover(0, 0));
    assert!(!tag(&mut ed), "away: gone");
    // (a move that changes nothing draws nothing)
    ed.unchanged = false;
    ed.handle_event(crate::event::Event::Hover(5, 0));
    assert!(ed.unchanged, "no frame for it");
    // The mouse still over where its bar was, the bar gone from under it (its gaze moved down): the tag goes with
    // the next frame
    ed.handle_event(crate::event::Event::Hover(0, y as u16));
    assert!(tag(&mut ed));
    gaze(&mut ed, main.join("a.rs"), 3..4);
    settle(&mut ed, |ed| main_marks(ed).first().is_some_and(|m| m.lines.start != 3));
    assert!(!tag(&mut ed), "its bar gone from under the mouse: its tag too");
    assert_eq!(ed.presence.hover, None);
    // Over it again, then the terminal loses focus: gone
    let y2 = row_of(&rows(&frame(&mut ed, 100)), "let c = 3;");
    ed.handle_event(crate::event::Event::Hover(0, y2 as u16));
    assert!(tag(&mut ed), "over its bar again");
    ed.handle_event(crate::event::Event::Focus(false));
    assert!(!tag(&mut ed), "the terminal lost focus: no tag");
    ed.handle_event(crate::event::Event::Focus(true));
    gaze(&mut ed, main.join("a.rs"), 1..2);
    settle(&mut ed, |ed| main_marks(ed).first().is_some_and(|m| m.lines.start == 3));
    // Your cursor on its line (fix's a.rs: two lines more on top — `let a = 1;` is line 4)
    press(&mut ed, "4gg");
    assert!(tag(&mut ed), "your cursor on its line: its tag");
    press(&mut ed, "gg");
    assert!(!tag(&mut ed));
    // A write there: its tag, then its stay, its fade, gone
    ed.presence.wrote.insert((0, main.join("a.rs")), crate::clock::now());
    assert!(tag(&mut ed), "just wrote: its tag");
    let stay =
        |w: std::time::Duration| w <= TAG_STAY + std::time::Duration::from_millis(2) && w > TAG_STAY / 2;
    assert!(ed.tick_wanted().is_some_and(stay), "the main loop's wake is the tag's: {:?}", ed.tick_wanted());
    let wake = tag_wake(&ed).expect("a frame wanted as its stay ends");
    assert!(wake <= TAG_STAY + std::time::Duration::from_millis(2) && wake > TAG_STAY / 2, "{wake:?}");
    crate::clock::advance(TAG_STAY + TAG_FADE / 2);
    let m = main_marks(&ed)[0].clone();
    assert!(m.tag > 0 && m.tag < 100, "fading: {}", m.tag);
    assert_eq!(tag_wake(&ed), Some(TAG_STEP), "a frame a step while it fades");
    crate::clock::advance(TAG_FADE);
    assert_eq!(main_marks(&ed)[0].tag, 0);
    assert!(!tag(&mut ed), "gone");
    assert_eq!(tag_wake(&ed), None, "no frame past it");
    assert_eq!(ed.tick_wanted(), None, "an idle tarae isn't woken");
}

/// Performance budget: a sweep of the mouse is dozens of hovers a second, each only a look at what's under it
/// (`presence::hovered`) — none of the work every other event does (`Editor::handle_event`): over a session's bar,
/// still there, away, still away — under 200 ns a hover (4 measured).
#[test]
fn perf_budget_hover_burst() {
    if crate::test_results::unoptimized("presence::tests::perf_budget_hover_burst") {
        return;
    }
    let Scene { mut ed, main, _dir, .. } = scene("presence-hover-burst");
    gaze(&mut ed, main.join("a.rs"), 1..2);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    let y = row_of(&rows(&frame(&mut ed, 100)), "let a = 1;") as u16;
    let at = [(0, y), (0, y), (0, 0), (5, 0)];
    const N: usize = 40_000;
    let mut runs: Vec<f64> = (0..9)
        .map(|_| {
            let t = std::time::Instant::now();
            for i in 0..N {
                let (x, y) = at[i & 3];
                ed.handle_event(crate::event::Event::Hover(x, y));
            }
            t.elapsed().as_secs_f64() * 1e9 / N as f64
        })
        .collect();
    crate::sorting::by(&mut runs, f64::total_cmp);
    let ns = runs[runs.len() / 2];
    let line = format!("PERF hover_ns {ns:.0} budget=200 noise=5\n");
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    // (and it saw them: over the bar, its tag; away, none)
    ed.handle_event(crate::event::Event::Hover(0, y));
    assert_eq!(ed.presence.hover, Some(0));
    ed.handle_event(crate::event::Event::Hover(0, 0));
    assert_eq!(ed.presence.hover, None);
    assert!(ns < 200.0, "{ns:.0} ns a hover (budget 200)");
}

/// The side panel's file rows: each session by the shortest prefix of its name that tells it from the other open
/// sessions' (`m` · `f`; `ma` · `me`), bold in its colour, after the name — `≈` in warning before one whose changes
/// would conflict with yours, past three `+N`; the row in hand in words at the panel's foot.
#[test]
fn the_side_panel_says_each_session_by_its_initial() {
    let v = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(prefixes(&v(&["main", "fix", "docs"])), v(&["m", "f", "d"]));
    assert_eq!(prefixes(&v(&["main", "merge", "fix"])), v(&["ma", "me", "f"]));
    assert_eq!(prefixes(&v(&["Main", "merge"])), v(&["Ma", "me"]), "case aside, as written");
    assert_eq!(prefixes(&v(&["main", "main2"])), v(&["main", "main2"]), "one a prefix of the other: whole");
    let mark = |p: &str, c: u8, overlap: bool| FileMark { prefix: p.into(), colour: Some(c), overlap };
    let (text, paint) = file_hint(&[mark("m", 1, true), mark("f", 2, false)], "M");
    assert_eq!(text, "≈m f  M");
    let paint = paint.unwrap();
    let (p1, p2) = (crate::session::PARTICIPANTS[1], crate::session::PARTICIPANTS[2]);
    let at = |s: &str| text.find(s).unwrap();
    assert_eq!(
        paint.hint,
        [(0.."≈".len(), "warning"), (at("m")..at("m") + 1, p1), (at("f")..at("f") + 1, p2)]
    );
    assert_eq!(paint.bold, [at("m")..at("m") + 1, at("f")..at("f") + 1], "each initial bold");
    let many: Vec<FileMark> = ["a", "b", "c", "d", "e"].iter().map(|p| mark(p, 0, false)).collect();
    assert_eq!(file_hint(&many, "").0, "a b c +2");
    // On screen: the files tab, the row in hand's words at its foot
    let Scene { mut ed, main, other, _dir, .. } = scene("presence-initials");
    gaze(&mut ed, main.join("a.rs"), 1..3);
    settle(&mut ed, |ed| !main_marks(ed).is_empty());
    let mut sb = crate::sidebar::Sidebar::with_files(other.clone(), vec!["a.rs".into(), "b.rs".into()]);
    sb.selected = 0;
    ed.session.sidebar = Some(sb);
    ed.session.at = crate::session::At::Side;
    let r = rows(&frame(&mut ed, 100));
    assert!(r.iter().any(|r| r.contains("a.rs — main reading")), "the row in hand in words: {r:#?}");
}

/// A line map follows the lines kept, and a changed line goes where its change went.
#[test]
fn a_line_map_follows_the_lines_kept() {
    let from = "a\nb\nc\nd\ne\n";
    let to = "new\nnew\na\nb\nC\nd\ne\n";
    let m = LineMap::of(from, to);
    assert_eq!(m.range(0..1), 2..3, "a, two lines down");
    assert_eq!(m.range(1..2), 3..4);
    assert_eq!(m.range(2..3), 4..5, "c, changed: where its change is");
    assert_eq!(m.range(3..5), 5..7);
    // (past the end: the last line, never empty)
    assert_eq!(m.range(9..12), 6..7);
}
