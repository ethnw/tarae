use super::*;
use crate::key::{Code, Key};
use crate::session::{At, Doing};
use crate::worktree::{Facts, Shown};

/// A session as the list says it, every column filled.
fn full(id: SessionId, name: &str, doing: Doing) -> Shown {
    Shown {
        id,
        name: name.into(),
        branch: Some(format!("{name}-branch")),
        facts: Some(Facts { ahead: 2, behind: 3, changed: 4 }),
        doing,
        front: id == 0,
        usage: Some((12.0, 300 << 20)),
        rebasing: None,
        reloaded: 1,
        compacted: true,
        reviewer: "¶ reviewing".into(),
        claude: Some(("● warm until 14:32".into(), "$0.42".into())),
        billing: crate::billing::Billing::Api,
        build: Some((2 << 20, true)),
        queue: None,
        at: "src/a.rs:12 · writing".into(),
        colour: id as u8,
        overlap: vec![("main".into(), Some(0))],
        waits: (2, 1),
        report: "I will switch it to the entry API so a repeat keeps the first strand".into(),
        context: Some(84),
    }
}

/// The pattern the list lights (`floatlist::lit`).
fn lit(ed: &Editor) -> Option<regex::Regex> {
    crate::floatlist::lit(ed, GroupKind::Sessions)
}

fn cols(laid: &(usize, Vec<(Col, usize)>)) -> Vec<Col> {
    laid.1.iter().map(|c| c.0).collect()
}

/// The manager's row (P3 step 7, §3.6): its state in the strip's words and inks, what waits for you counted, whom it
/// overlaps (each name its colour), `↓N` behind, its context (warning past 80 %), its last report line, then the rest
/// the list always said — empty cells left out.
#[test]
fn a_row_says_what_the_manager_needs() {
    let r = row(&full(1, "fix", Doing::Waiting));
    assert_eq!(r.cell(Col::State), [("▲ waits".to_string(), Ink::Warn)]);
    assert_eq!(r.text(Col::Waits), "2 cards · 1 edit");
    assert_eq!(r.cell(Col::Overlap), [("≈ ".into(), Ink::Warn), ("main".into(), Ink::Participant(0))]);
    assert_eq!((r.text(Col::Behind), r.text(Col::Ahead)), ("↓3".into(), "↑2".into()));
    assert_eq!(r.cell(Col::Context), [("ctx ".into(), Ink::Faint), ("84%".into(), Ink::Warn)]);
    assert_eq!(r.text(Col::Report), "I will switch it to the entry API so a repeat keeps the first strand");
    for (doing, said, ink) in [
        (Doing::Working, "working", Ink::Faint),
        (Doing::Done, "done", Ink::Accent),
        (Doing::Queued { pos: 2 }, "queued 2nd", Ink::Faint),
        (Doing::Stuck { tool: "symbols", secs: 42 }, "◦ stuck? symbols 42s", Ink::Warn),
    ] {
        assert_eq!(row(&full(1, "fix", doing)).cell(Col::State), [(said.to_string(), ink)]);
    }
    let idle = Shown { waits: (0, 0), overlap: Vec::new(), context: None, ..full(0, "main", Doing::Idle) };
    let r = row(&idle);
    assert!(
        r.cell(Col::State).is_empty() && r.cell(Col::Waits).is_empty() && r.cell(Col::Overlap).is_empty()
    );
    assert!(r.front, "the front's name bold");
    // (a rebase and the bring queue before the state)
    let rebasing = Shown { rebasing: Some("rebasing 1/2".into()), ..full(1, "fix", Doing::Working) };
    assert_eq!(row(&rebasing).text(Col::State), "rebasing 1/2 · working");
}

/// The list's width at 160 (the frame 96 wide) and at 80 (48): whole columns fold in `FOLDS`' order — the rest first,
/// then the report (cut to what's there before it goes), the context, behind, the overlaps; what waits for you and the
/// state last, the name never. Every narrower room shows a subset of the wider's.
#[test]
fn the_row_folds_at_80_in_its_order() {
    let rows = [row(&full(0, "main", Doing::Working)), row(&full(1, "fix-login", Doing::Waiting))];
    // (the rows' room in a frame `w` wide: its sides, the bar's cells, a cell before the side)
    let room = |w: usize| w - 2 - 2 - 1;
    let wide = lay(&rows, 400);
    assert_eq!(cols(&wide), COLS, "every column with room for all");
    let at160 = lay(&rows, room(96));
    let shown = cols(&at160);
    assert!(
        [Col::State, Col::Waits, Col::Overlap, Col::Behind, Col::Context, Col::Report]
            .iter()
            .all(|c| shown.contains(c)),
        "at 160 the manager's columns all: {shown:?}"
    );
    let at80 = cols(&lay(&rows, room(48)));
    assert!(!at80.contains(&Col::Report), "at 80 the report first: {at80:?}");
    assert!(
        at80.contains(&Col::State) && at80.contains(&Col::Waits),
        "what waits and the state stay: {at80:?}"
    );
    // (from wide to nothing, one room at a time: what goes, goes in `FOLDS`' order, and never comes back)
    let mut gone: Vec<Col> = Vec::new();
    let mut was = cols(&wide);
    for r in (0..400).rev() {
        let now = cols(&lay(&rows, r));
        assert!(now.iter().all(|c| was.contains(c)), "{r}: {now:?} ⊄ {was:?}");
        gone.extend(was.iter().filter(|c| !now.contains(c)));
        was = now;
    }
    assert_eq!(gone, FOLDS, "the order they fold in");
    // (the report cut before it goes, once the rest went: to its least)
    // (from wide to narrow: the first room it's cut in)
    let cut = (0..400)
        .rev()
        .map(|r| lay(&rows, r))
        .find(|l| l.1.iter().any(|c| c.0 == Col::Report && c.1 < REPORT_MAX));
    let cut = cut.expect("cut once").1;
    assert!(cut.iter().all(|c| !FOLDS[..12].contains(&c.0)), "only once the rest went: {cut:?}");
    let (name_w, _) = lay(&rows, 3);
    assert_eq!(name_w, 3, "the names cut to the room, never gone");
}

/// Its Claude's last report line: the last sentence of its last answer, Markdown's marks out; nothing before one.
#[test]
fn the_report_line_is_its_claudes_last_sentence() {
    use crate::chat::{Chat, Msg, Role};
    assert_eq!(report(None), "");
    let mut c = Chat::new();
    c.msgs.push(Msg::new(Role::User, "why?", ""));
    assert_eq!(report(Some(&c)), "", "no answer yet");
    c.msgs.push(Msg::new(
        Role::Assistant,
        "First. The `name` is **cloned** here.\n\n```rust\nx\n```\n- So it keys by value. Done now.",
        "",
    ));
    assert_eq!(report(Some(&c)), "Done now.");
    c.msgs.push(Msg::new(Role::Assistant, "## Fixed the `insert`", ""));
    assert_eq!(report(Some(&c)), "Fixed the insert");
}

fn frame(ed: &mut Editor, w: u16, h: u16) -> String {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), w, h).unwrap();
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), w, h).unwrap()
}

fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(k);
        frame(ed, 160, 40);
    }
}

/// `space W` floats the list in a named frame with the keys, its rows the sessions; `esc` gives the keys back (it
/// stays, its frame unlit); a press on it, `space W` again, `C-w w` give them to it; it moves and sizes as any floating
/// pane, its frame the viewer's (kept with the sizes); `C-w e`, `C-w H` say it floats; `q` closes it.
#[test]
fn the_session_list_floats_and_stays_while_you_work() {
    let dir = crate::scratch::Scratch::new("sessionpane");
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.to_path_buf(), "main".into());
    feed(&mut ed, " W");
    let p = ed.sessions_pane.clone().expect("up");
    assert_eq!(ed.session.at, At::List(p.list));
    let s = frame(&mut ed, 160, 40);
    let f = ed.screen.floating.borrow().iter().find(|f| f.0 == GroupKind::Sessions).unwrap().1;
    assert_eq!(
        f,
        crate::split::Rect { x: 32, y: 10, w: 96, h: 3 },
        "its top at 1/4, 3/5 wide, as tall as its row"
    );
    let top = s.lines().nth(f.y).unwrap();
    assert!(top.contains("╭─ sessions ── 1 session") && top.contains("enter switch"), "{top}");
    assert!(s.lines().nth(f.y + 1).unwrap().contains("▎ main"), "its row, selected");
    feed(&mut ed, "<esc>");
    assert!(ed.sessions_pane.is_some() && ed.session.at == At::Code, "esc: the keys back, it stays");
    assert!(
        !frame(&mut ed, 160, 40).lines().nth(f.y).unwrap().contains("enter switch"),
        "its keys only with them"
    );
    ed.handle_mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        x: (f.x + 4) as u16,
        y: (f.y + 1) as u16,
        alt: false,
    });
    assert!(ed.sessions_list_keys(), "a press on its row");
    // `C-w w` goes round to it after the docked panes (its id among the leaves, reviewer 7a); `space W` again
    feed(&mut ed, "<esc><C-w>w");
    assert!(ed.sessions_list_keys(), "C-w w: the list");
    feed(&mut ed, "<C-w>w");
    assert_eq!(ed.session.at, At::Code, "C-w w on: round to the code");
    feed(&mut ed, " W");
    assert!(ed.sessions_list_keys(), "space W again");
    feed(&mut ed, "<C-w>e");
    assert!(ed.messages.last().unwrap().2.contains("the session list floats"));
    feed(&mut ed, "<C-w>H");
    assert!(ed.messages.last().unwrap().2.contains("the session list floats"));
    feed(&mut ed, "<C-w>mj<ret><C-w>>");
    let moved = ed.screen.floating.borrow().iter().find(|f| f.0 == GroupKind::Sessions).unwrap().1;
    assert_eq!((moved.y, moved.w), (f.y + 2, f.w + crate::sizes::STEP_COLS));
    assert!(ed.sizes.sessions.is_some(), "kept with the sizes");
    feed(&mut ed, "q");
    assert!(ed.sessions_pane.is_none() && ed.screen.floating.borrow().is_empty());
    feed(&mut ed, " W");
    let again = ed.screen.floating.borrow().iter().find(|f| f.0 == GroupKind::Sessions).unwrap().1;
    assert_eq!(again, moved, "where you put it");
}

/// A card that comes while the list has the keys waits to be focused (P3 §3.8, the rule 7a's stand-in folded into):
/// its letters stay the list's — `n` its search, never the card's no; `v` the list's —; the status line says the
/// card waits and `C-q` reaches it; there `n` answers it.
#[test]
fn a_card_waits_while_the_session_list_has_the_keys() {
    let dir = crate::scratch::Scratch::new("sessionpane");
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.to_path_buf(), "main".into());
    feed(&mut ed, " W");
    ed.push_offer(crate::offer::Offer::new(
        "End the review?".into(),
        "2 files not viewed.",
        "",
        String::new(),
        crate::offer::What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    ));
    let s = frame(&mut ed, 160, 40);
    assert!(!ed.card_has_keys(), "the card waits");
    assert_eq!(ed.keys_to().region, crate::uistate::Region::List(GroupKind::Sessions));
    assert_eq!(ed.keys_to().waiting, Some(format!("card#{}", ed.offer_seq)), "the state says it waits");
    let state = ed.ui_state().json();
    assert_eq!(state["cards"][0]["waits"], "focus", "…and the card");
    let status = s.lines().nth(39).unwrap_or_default();
    assert!(status.contains("waiting") && status.contains("C-q"), "said: {status}");
    feed(&mut ed, "n");
    assert_eq!(ed.offers.len(), 1, "n: not the card's");
    assert!(ed.messages.last().unwrap().2.contains("/ searches"), "the list's n");
    feed(&mut ed, "v");
    assert_eq!(ed.offers.len(), 1, "v: not the card's");
    feed(&mut ed, "<C-q>n");
    assert!(ed.offers.is_empty(), "C-q: the card's keys, n its no");
    // The one the list's own letter asked for (`b`'s, `B`'s — the user's own) takes the keys at once
    let was = ed.events.set_asked(Some(ed.input_seq));
    ed.push_offer(crate::offer::Offer::new(
        "End the review?".into(),
        "2 files not viewed.",
        "",
        String::new(),
        crate::offer::What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    ));
    ed.events.set_asked(was);
    frame(&mut ed, 160, 40);
    assert!(ed.sessions_list_keys() && ed.card_has_keys(), "yours: its keys");
    assert_eq!(ed.keys_to().waiting, None);
}

/// The list is searched with `/` as every pane is (user, 2026-10-23), never type-to-narrow: its own prompt, the
/// selection moving to the next match as the pattern is typed, `esc` back to where it was; `enter` keeps it as the
/// list's search (never the code's), its matches lit, `n`/`N` the next and the one before, round the ends (said); a
/// pattern matching nothing said; `esc` unlights it. `o` makes a new session (`n` is the search's); another letter
/// says `/` searches.
#[test]
fn the_session_list_is_searched_with_slash() {
    let dir = crate::scratch::Scratch::new("sessionpane");
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.to_path_buf(), "main".into());
    for name in ["fix-login", "docs"] {
        ed.session_add(dir.join(name));
        ed.sessions.last_mut().unwrap().name = name.into();
    }
    let at = |ed: &Editor| {
        let id = ed.sessions_here().unwrap();
        ed.sessions_in_order().iter().find(|l| l.id == id).unwrap().name.clone()
    };
    let said = |ed: &Editor| ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
    feed(&mut ed, " W");
    assert!(super::hints(&ed).iter().any(|h| h.0 == "/" && h.1 == "search"), "its keys say it");
    // Typing: the selection goes to the next match; esc back where it was
    feed(&mut ed, "/do");
    assert_eq!(at(&ed), "docs", "as it's typed");
    assert!(lit(&ed).is_some(), "its matches lit as it's typed");
    assert!(ed.search_hits.is_none(), "the code's matches untouched");
    feed(&mut ed, "<esc>");
    assert_eq!((at(&ed), ed.prompt.is_none()), ("main".into(), true), "esc: back where it was");
    assert!(ed.sessions_list_keys(), "the keys still the list's");
    // Entered: the list's search, lit, `n`/`N` round it
    feed(&mut ed, "/login|docs<ret>");
    assert_eq!(at(&ed), "fix-login");
    let p = ed.sessions_pane.as_ref().unwrap();
    assert_eq!((p.search.as_deref(), p.hl), (Some("login|docs"), true));
    assert_eq!(ed.search, None, "never the code's search");
    feed(&mut ed, "n");
    assert_eq!(at(&ed), "docs");
    feed(&mut ed, "n");
    assert_eq!((at(&ed), said(&ed)), ("fix-login".into(), "search wrapped around".into()));
    feed(&mut ed, "N");
    assert_eq!(at(&ed), "docs", "N: the one before, round the start");
    // Lit: the rows' cells drawn otherwise than unlit
    let shown = |ed: &mut Editor| {
        frame(ed, 160, 40);
        ed.last_frame.as_ref().unwrap().0.clone()
    };
    let with = shown(&mut ed);
    ed.sessions_pane.as_mut().unwrap().hl = false;
    assert_ne!(with, shown(&mut ed), "its matches lit");
    ed.sessions_pane.as_mut().unwrap().hl = true;
    feed(&mut ed, "/zzz<ret>");
    assert_eq!((at(&ed), said(&ed)), ("docs".into(), "no session matches /zzz/".into()), "nothing: said");
    // esc: unlit, the keys back to the code
    feed(&mut ed, "<esc>");
    assert!(!ed.sessions_pane.as_ref().unwrap().hl && lit(&ed).is_none());
    feed(&mut ed, " Wz");
    assert!(said(&ed).contains("/ searches"), "another letter: {}", said(&ed));
    feed(&mut ed, "o");
    assert_eq!(ed.prompt.as_ref().map(|p| p.kind), Some(crate::editor::PromptKind::Session), "o: new");
}

/// A list floats 3/5 wide and as tall as its rows, up to half the screen (user, 2026-10-23), its top on the line every
/// floating pane's is — 1/4 of the screen's height, a full 3/5 × 1/2 one's (centred) — for 1, 3 or many rows: a
/// session added grows it downward, its top never moving, opened again too; a move or a width keeps it as tall as its
/// rows, a height you set (`C-w + -`) is kept, `C-w =` gives it its rows again on that line.
#[test]
fn the_list_is_as_tall_as_its_rows() {
    let dir = crate::scratch::Scratch::new("sessionpane");
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.to_path_buf(), "main".into());
    let rect = |ed: &mut Editor| {
        frame(ed, 160, 40);
        ed.screen.floating.borrow().iter().find(|f| f.0 == GroupKind::Sessions).unwrap().1
    };
    // (a full floating pane, 3/5 × 1/2 — the tests' — is centred: its top at 1/4, row 10 of 39)
    let full = crate::pane::Frame::CENTRED.rect(160, 39);
    assert_eq!((full.y, full.h), (10, 20), "a full one: 10 rows above, 9 below");
    let rect_of = |y, h| crate::split::Rect { x: 32, y, w: 96, h };
    feed(&mut ed, " W");
    assert_eq!(rect(&mut ed), rect_of(full.y, 3), "one row: its top a full one's");
    ed.session_add(dir.join("a"));
    ed.session_add(dir.join("b"));
    assert_eq!(rect(&mut ed), rect_of(full.y, 5), "three: grown downward, its top line staying");
    feed(&mut ed, "q W");
    assert_eq!(rect(&mut ed), rect_of(full.y, 5), "opened again: the same line");
    for i in 0..30 {
        ed.session_add(dir.join(format!("s{i}")));
    }
    assert_eq!(rect(&mut ed), rect_of(full.y, 19), "many: up to half the screen (39 rows), the same top");
    for _ in 0..30 {
        let id = ed.sessions.last().unwrap().id;
        ed.sessions.retain(|l| l.id != id);
    }
    // A move and a width: still as tall as its rows
    feed(&mut ed, "<C-w>mj<ret><C-w>>");
    assert_eq!(ed.sizes.sessions.map(|f| f.h), Some(0), "kept, its height its rows'");
    ed.session_add(dir.join("c"));
    assert_eq!(rect(&mut ed), crate::split::Rect { x: 32, y: 12, w: 100, h: 6 }, "moved 2, wider 4, grown");
    // As short as it goes: `C-w -` says so, never taller
    feed(&mut ed, "<C-w>-");
    assert_eq!(rect(&mut ed).h, 6, "no shorter");
    assert!(ed.messages.last().unwrap().2.contains("as short as it goes"));
    // A height you set is kept: a session added doesn't grow it
    feed(&mut ed, "<C-w>+");
    let set = rect(&mut ed);
    assert_eq!(set.h, 8);
    ed.session_add(dir.join("d"));
    assert_eq!(rect(&mut ed), set, "the height you set");
    feed(&mut ed, "<C-w>=");
    assert_eq!(
        (ed.sizes.sessions, rect(&mut ed)),
        (None, rect_of(full.y, 7)),
        "its rows again, its top at 1/4"
    );
}
