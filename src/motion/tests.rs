use super::*;
use crate::key::{Code, Key};

/// Keys as written (`<esc>`, `<ret>` named), each an event as the loop hands it in.
fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_event(crate::event::Event::Key(k));
    }
}

fn editor(text: &str) -> Editor {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str(text);
    ed
}

/// The flash on the document as it is now: its act and the text of each range.
fn flashed(ed: &Editor) -> Option<(Act, Vec<String>)> {
    let What::Flash { ranges, act, doc, version } = &ed.motions.last()?.what;
    if (*doc, *version) != (ed.doc().id, ed.doc().version()) {
        return None;
    }
    let text = &ed.doc().text;
    Some((*act, ranges.iter().map(|r| text.byte_slice(r.clone()).to_string()).collect()))
}

/// `u` after an edit at two cursors two lines apart: what each took back flashes, not the line between them; `U` the
/// same.
#[test]
fn undo_flashes_what_it_took_back_at_every_cursor() {
    let mut ed = editor("ab\ncd\nab\n");
    feed(&mut ed, "%sab<ret>cX<esc>");
    assert_eq!(ed.doc().text.to_string(), "X\ncd\nX\n");
    assert_eq!(flashed(&ed), None, "typing flashes nothing: you saw it");
    feed(&mut ed, "u");
    assert_eq!(ed.doc().text.to_string(), "ab\ncd\nab\n");
    assert_eq!(flashed(&ed), Some((Act::Undo, vec!["ab".into(), "ab".into()])));
    feed(&mut ed, "U");
    assert_eq!(flashed(&ed), Some((Act::Redo, vec!["X".into(), "X".into()])));
    // (lines it put in or took out, not one for one: the lines whole)
    let mut ed = editor("a\nb\nc\n");
    feed(&mut ed, "xdjjxdu");
    assert_eq!(flashed(&ed), Some((Act::Undo, vec!["c\n".into()])));
    feed(&mut ed, "u");
    assert_eq!(flashed(&ed), Some((Act::Undo, vec!["a\n".into()])));
    assert_eq!(ed.motions.len(), 1, "one flash a document: the newer takes its place");
    // (within one line: the span itself; what it only took out, the character where it was)
    let mut ed = editor("one two\n");
    feed(&mut ed, "ecsix<esc>u");
    assert_eq!(flashed(&ed), Some((Act::Undo, vec!["one".into()])));
    feed(&mut ed, "U");
    assert_eq!(flashed(&ed), Some((Act::Redo, vec!["six".into()])));
    let mut ed = editor("one two\n");
    feed(&mut ed, "ea!<esc>u");
    assert_eq!(flashed(&ed), Some((Act::Undo, vec![" ".into()])), "the join");
    // (nothing to undo: nothing flashes)
    let mut ed = editor("one\n");
    feed(&mut ed, "u");
    assert_eq!(flashed(&ed), None);
}

/// `.` at three cursors: what it typed again flashes at each, nothing between.
#[test]
fn repeat_flashes_what_it_typed_at_every_cursor() {
    let mut ed = editor("a\nb\nc\nd\ne\nf\n");
    feed(&mut ed, "iX<esc>");
    assert_eq!(flashed(&ed), None);
    feed(&mut ed, "jjjCC.");
    assert_eq!(ed.doc().text.to_string(), "Xa\nb\nc\ndX\neX\nfX\n");
    assert_eq!(flashed(&ed), Some((Act::Repeat, vec!["X".into(); 3])));
}

/// A yank flashes what went in; a paste in normal mode doesn't (what it put in is selected); one while inserting
/// does (its cursors end after it).
#[test]
fn a_yank_and_an_insert_paste_flash_a_normal_paste_does_not() {
    let mut ed = editor("one\ntwo\n");
    feed(&mut ed, "xy");
    assert_eq!(flashed(&ed), Some((Act::Yank, vec!["one\n".into()])));
    let mut ed = editor("one\ntwo\n");
    feed(&mut ed, "x\"_y");
    assert_eq!(flashed(&ed), None, "the black hole takes nothing in");
    feed(&mut ed, "xyjp");
    assert_eq!(flashed(&ed), None, "p selects what it put: nothing flashes");
    let mut ed = editor("ab\ncd\n");
    ed.registers.insert('"', vec!["ab\n".into()]);
    feed(&mut ed, "Ci");
    ed.handle_event(crate::event::Event::Paste("hey".into()));
    assert_eq!(ed.doc().text.to_string(), "heyab\nheycd\n");
    assert_eq!(flashed(&ed), Some((Act::Paste, vec!["hey".into(); 2])));
    feed(&mut ed, "<esc>i<C-r>\"");
    assert_eq!(flashed(&ed), Some((Act::Paste, vec!["ab\n".into(); 2])), "C-r too");
    feed(&mut ed, "z");
    assert_eq!(flashed(&ed).map(|f| f.0), None, "a key after it: the version moved, nothing drawn");
}

/// A yank's "yanked …" is a note only where a frame drew its flash: kept in `:messages` at once, no toast — to a
/// register and `space y` (once the clipboard took it) by the same rule; a frame that didn't draw it (not drawn: the
/// tests' default; the chat's transcript — chatdoc's view tests) toasts it after, once; one gone before any frame
/// drew it, as it goes; the black hole and `none` flash nothing — a toast at once.
#[test]
fn a_yank_is_a_note_where_its_flash_is_drawn_else_a_toast() {
    let toasts = |ed: &Editor| ed.toasts.iter().map(|t| t.text.clone()).collect::<Vec<_>>();
    let last = |ed: &Editor| ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
    let frame = |ed: &mut Editor| crate::term::render(ed, &mut Vec::new(), 80, 12).unwrap();
    let mut ed = editor("one\ntwo\n");
    ed.motion_drawn = true;
    feed(&mut ed, "xy");
    assert_eq!((last(&ed), toasts(&ed)), ("yanked 1 selection".into(), vec![]), "kept at once, no toast yet");
    frame(&mut ed);
    assert_eq!(toasts(&ed), Vec::<String>::new(), "drawn: a note");
    feed(&mut ed, "\"ay");
    frame(&mut ed);
    assert_eq!((last(&ed), toasts(&ed)), ("yanked 1 selection to register a".into(), vec![]));
    feed(&mut ed, " y");
    let ev = ed.events.recv_timeout(Duration::from_secs(5)).expect("the copy's result");
    ed.handle_event(ev);
    frame(&mut ed);
    assert_eq!((last(&ed), toasts(&ed)), ("copied 1 selection".into(), vec![]), "space y: the same rule");
    feed(&mut ed, "\"_y");
    assert_eq!(toasts(&ed), ["yanked 1 selection to register _"], "nothing flashed: a toast at once");
    // (not drawn: the frame after it toasts it, the words kept once)
    let mut ed = editor("one\ntwo\n");
    feed(&mut ed, "xy");
    frame(&mut ed);
    assert_eq!(toasts(&ed), ["yanked 1 selection"], "not drawn: a toast");
    assert_eq!(ed.messages.iter().filter(|m| m.2 == "yanked 1 selection").count(), 1, "kept once");
    // (gone before a frame: another flash on it took its place)
    let mut ed = editor("one\ntwo\n");
    ed.motion_drawn = true;
    feed(&mut ed, "xyjxdu");
    assert_eq!(toasts(&ed), ["yanked 1 selection"], "replaced undrawn: a toast as it goes");
    // (`none`: no flash, a toast at once)
    let mut ed = editor("one\ntwo\n");
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    ed.toasts.clear();
    feed(&mut ed, "xy");
    assert_eq!((toasts(&ed), ed.motions.len()), (vec!["yanked 1 selection".to_string()], 0), "none: a toast");
}

/// What a flash draws: at its peak every cell of its ranges, at `calm` the same, at `none` nothing; past its time
/// nothing; the accent over the ground, dried by the step.
#[test]
fn a_flash_is_drawn_on_its_cells_and_dries() {
    let cells = |ed: &mut Editor| {
        crate::term::MOTION_SEEN.with_borrow_mut(Vec::clear);
        crate::term::render(ed, &mut Vec::new(), 80, 12).unwrap();
        crate::term::MOTION_SEEN.with_borrow(|s| s.iter().map(|(_, _, g)| g.as_str()).collect::<String>())
    };
    let mut ed = editor("one two\n");
    feed(&mut ed, "ecsix<esc>u");
    assert_eq!(cells(&mut ed), "", "not drawn in tests unless turned on");
    ed.motion_drawn = true;
    assert_eq!(cells(&mut ed), "one");
    crate::typed::execute(&mut ed, "set editor.motion calm").unwrap();
    assert_eq!(cells(&mut ed), "one", "calm: whole");
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    assert_eq!(cells(&mut ed), "");
    crate::typed::execute(&mut ed, "set editor.motion full").unwrap();
    ed.motion_at = Some(Duration::from_millis(100));
    assert_eq!(cells(&mut ed), "one", "its step after the peak");
    ed.motion_at = Some(FLASH);
    assert_eq!(cells(&mut ed), "", "gone at its end");
    // (the drying: the accent's mix at the peak, near half of it a step on, the ground at the end)
    let m = &ed.motions[0];
    assert_eq!(strength(Duration::ZERO, m.life), 1.0);
    assert_eq!((strength(Duration::from_millis(150), m.life) * 100.0).round(), 44.0, "read at its step");
    assert_eq!(strength(FLASH, m.life), 0.0);
}

/// The curve: an ease-out at whole steps, over under `GONE`.
#[test]
fn the_drying_eases_out_and_ends() {
    let s = Duration::from_secs(1);
    let steps: Vec<f32> = (0..10).map(|k| (strength(TICK * k, s) * 100.0).round() / 100.0).collect();
    assert_eq!(steps, [1.0, 0.81, 0.64, 0.49, 0.36, 0.25, 0.16, 0.09, 0.0, 0.0]);
    assert_eq!(shown_for(s), Duration::from_millis(800));
    assert_eq!(shown_for(FLASH), FLASH);
    assert_eq!(shown_for(Duration::from_millis(300)), Duration::from_millis(300));
}

/// The wakes: a frame each step on the flash's own clock, one as it goes, then none — at `calm` one, at `none`
/// and pinned (`TARAE_MOTION_AT`) none.
#[test]
fn a_flash_wakes_on_its_steps_then_never() {
    let mut ed = editor("one two\n");
    crate::clock::hold();
    feed(&mut ed, "ecsix<esc>u");
    let ms = |d: Option<Duration>| d.map(|d| d.as_millis());
    assert_eq!(ms(ed.motion_wake()), Some(102), "a full TICK after the key");
    crate::clock::advance(Duration::from_millis(40));
    assert_eq!(ms(ed.motion_wake()), Some(62), "on its own clock, not the wave's");
    crate::clock::advance(Duration::from_millis(62));
    assert_eq!(ms(ed.motion_wake()), Some(100));
    crate::clock::advance(Duration::from_millis(100));
    assert_eq!(ms(ed.motion_wake()), Some(100));
    crate::clock::advance(Duration::from_millis(100));
    assert_eq!(ms(ed.motion_wake()), Some(0), "over: the frame that draws it gone");
    ed.schedule_tick();
    assert!(ed.motions.is_empty(), "let go");
    assert!(!ed.unchanged, "its going is drawn");
    assert_eq!(ed.motion_wake(), None, "and nothing more");
    feed(&mut ed, "U");
    crate::typed::execute(&mut ed, "set editor.motion calm").unwrap();
    assert_eq!(ms(ed.motion_wake()), Some(302), "calm: once, as it goes");
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    assert_eq!(ms(ed.motion_wake()), Some(302), "none: no steps, one wake as it goes");
    crate::typed::execute(&mut ed, "set editor.motion full").unwrap();
    ed.motion_at = Some(Duration::ZERO);
    assert_eq!(ed.motion_wake(), None, "pinned: no frames");
    crate::clock::release();
}

/// Nothing stays listed past its time: at `none` no flash is made (the state's `motions` empty, no wake); one made
/// before a switch to `none`, or while unfocused, asks one wake at its end and is let go by it — listed nowhere when
/// focus comes back.
#[test]
fn a_flash_never_outlives_its_time_in_the_state() {
    let listed = |ed: &Editor| ed.ui_state().motions.len();
    let mut ed = editor("one two\n");
    crate::clock::hold();
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    feed(&mut ed, "ecsix<esc>u");
    assert_eq!((listed(&ed), ed.motion_wake()), (0, None), "none: nothing made");
    crate::typed::execute(&mut ed, "set editor.motion full").unwrap();
    feed(&mut ed, "U");
    assert_eq!(listed(&ed), 1);
    ed.unfocused = true;
    let ms = |d: Option<Duration>| d.map(|d| d.as_millis());
    assert_eq!(ms(ed.motion_wake()), Some(302), "unfocused: one wake, at its end");
    crate::clock::advance(Duration::from_millis(302));
    ed.schedule_tick();
    ed.unfocused = false;
    assert_eq!((listed(&ed), ed.motion_wake()), (0, None), "let go by its time, none when focus is back");
    feed(&mut ed, "u");
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    crate::clock::advance(Duration::from_millis(100));
    assert_eq!(ms(ed.motion_wake()), Some(202), "made before `none`: one wake at its end");
    crate::clock::advance(Duration::from_millis(202));
    ed.schedule_tick();
    assert_eq!(listed(&ed), 0);
    crate::clock::release();
}
