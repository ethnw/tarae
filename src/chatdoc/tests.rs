//! The transcript's text from the chat's rows, and the document following what frames lay out.

use super::*;
use crate::markdown::Span;
use crate::theme::Style;

fn row(texts: &[&str]) -> ChatRow {
    let spans = texts.iter().map(|t| Span { text: t.to_string(), style: Style::default() }).collect();
    ChatRow { spans, ..ChatRow::default() }
}

fn rows(v: Vec<ChatRow>) -> Arc<[ChatRow]> {
    v.into()
}

/// A paragraph the layout wrapped is one line again (the space its break took put back), a bar and a hanging
/// indent left out; a tool row's knot and stamps aren't text; a cut code row's line is whole, its `…` not drawn
/// text.
#[test]
fn the_rows_are_paragraphs_with_their_decorations_left_out() {
    let question = vec![
        ChatRow { skip: 2, ..row(&["▎ ", "why does the"]) },
        ChatRow { skip: 4, join: Some(true), ..row(&["▎ ", "  frame wait?"]) },
    ];
    let (text, at) = lines_of(&question);
    assert_eq!(text, "why does the frame wait?\n");
    assert_eq!(at[1], Place { line: 0, from: 13, col: 13, len: 11, x0: 4 }, "its place, its indent's cells");
    let tool = ChatRow { skip: 2, tail: 3, ..row(&["◦", " read   src/lib.rs", "   ", "▐y▌"]) };
    assert_eq!(lines_of(&[tool]).0, "read   src/lib.rs\n");
    let code = ChatRow { skip: 2, rest: Some("_long(x);".into()), ..row(&["│ ", "let a = f…"]) };
    let (text, at) = lines_of(&[code]);
    assert_eq!((text.as_str(), at[0].len), ("let a = f_long(x);\n", "let a = f".len()));
    assert_eq!(lines_of(&[]).0, "", "a message with no rows has no lines");
}

/// What a frame lays out comes in: each message its lines, air an empty line, the frame's own rows not at all; one
/// written on replaces only its own lines, the cursor where it was; laid out again the same (a resize) changes
/// nothing; another conversation is made anew.
#[test]
fn it_follows_what_the_frames_lay_out() {
    let mut t = Transcript::new(crate::document::DocId::default());
    let q = rows(vec![ChatRow { skip: 2, ..row(&["▎ ", "a question"]) }]);
    let a1 = rows(vec![row(&[]), row(&["the answer"])]);
    let wait = rows(vec![row(&["  waiting on a lock"])]);
    let laid: Laid = vec![(Of::Msg(0), q.clone()), (Of::Msg(1), a1), (Of::Own, wait.clone())];
    follow(&mut t, &laid);
    let text = |t: &Transcript| t.pane.doc().text.to_string();
    assert_eq!(text(&t), "a question\n\nthe answer\n");
    assert_eq!((t.rows(), t.row(2).map(|r| (r.line, r.start, r.len))), (3, Some((2, 12, 10))));
    // (the cursor on "question"; then the answer is written on)
    let d = t.pane.doc_mut();
    d.set_selection(crate::selection::Selection::point(2));
    let v = d.version();
    let a2 = rows(vec![row(&[]), row(&["the answer goes"]), row(&["on"])]);
    let laid: Laid = vec![(Of::Msg(0), q.clone()), (Of::Msg(1), a2.clone()), (Of::Own, wait)];
    follow(&mut t, &laid);
    assert_eq!(text(&t), "a question\n\nthe answer goes\non\n");
    assert_eq!(t.pane.doc().selection().primary().cursor(&t.pane.doc().text), 2, "where it was");
    let v2 = t.pane.doc().version();
    assert!(v2 != v, "its lines replaced");
    // (laid out again as it was: new rows, the same text)
    let again: Laid = vec![(Of::Msg(0), rows(q.to_vec())), (Of::Msg(1), rows(a2.to_vec()))];
    follow(&mut t, &again);
    assert_eq!(t.pane.doc().version(), v2, "nothing changed");
    // (air before the next question: an empty line)
    let q2 = rows(vec![ChatRow { skip: 2, ..row(&["▎ ", "more?"]) }]);
    let blank = rows(vec![ChatRow::default()]);
    let more: Laid = [again.clone(), vec![(Of::Air, blank), (Of::Msg(2), q2)]].concat();
    follow(&mut t, &more);
    assert_eq!(text(&t), "a question\n\nthe answer goes\non\n\nmore?\n");
    // (the next frame lays the same out, its air a row of its own: nothing made again — not even the rows)
    let given = t.pane.doc().rows.clone().expect("its rows");
    let next: Laid =
        [again.clone(), vec![(Of::Air, rows(vec![ChatRow::default()])), more[3].clone()]].concat();
    follow(&mut t, &next);
    assert!(Arc::ptr_eq(&given, t.pane.doc().rows.as_ref().expect("its rows")), "its rows kept as they were");
    // (another conversation)
    let other: Laid = vec![(Of::Msg(0), rows(vec![row(&["▎ ", "hi"])]))];
    follow(&mut t, &other);
    assert_eq!(text(&t), "▎ hi\n", "its rows' text, no skip given");
    follow(&mut t, &Vec::new());
    assert_eq!((text(&t).as_str(), t.rows()), ("", 1), "an empty conversation: one row of nothing");
}

/// A frame that changes only the answer being written makes only its rows again: the parts of every piece before it
/// are the same `Arc`s as before (the same storage, not equal copies), and the rows are what a transcript made
/// from scratch has; one changed in the middle makes its own and the ones after it again.
#[test]
fn a_frame_writing_the_last_piece_keeps_the_earlier_rows() {
    let msg = |i: usize, words: &str| (Of::Msg(i), rows(vec![row(&[]), row(&[words]), row(&["and on"])]));
    let air = || (Of::Air, rows(vec![ChatRow::default()]));
    let mut laid: Laid = vec![msg(0, "a question"), msg(1, "an answer"), air(), msg(2, "another")];
    laid.push(msg(3, "being written"));
    let mut t = Transcript::new(crate::document::DocId::default());
    follow(&mut t, &laid);
    let parts = |t: &Transcript| t.pane.doc().rows.as_deref().expect("its rows").parts().to_vec();
    let before = parts(&t);
    assert_eq!(before.len(), 5, "a part a piece");
    // (the answer being written grows: a frame lays it out again, the rest as they were — air in a new `Arc`)
    laid[2] = air();
    laid[4] = msg(3, "being written, more words");
    follow(&mut t, &laid);
    let after = parts(&t);
    let same: Vec<bool> = before.iter().zip(&after).map(|(a, b)| Arc::ptr_eq(a, b)).collect();
    assert_eq!(same, [true, true, true, true, false], "only the last piece's rows made again");
    // (what they say: a transcript made from scratch has the same rows)
    let mut fresh = Transcript::new(crate::document::DocId::default());
    follow(&mut fresh, &laid);
    let all = |t: &Transcript| -> Vec<Option<crate::wrap::Row>> {
        let g = t.pane.doc().rows.clone().expect("its rows");
        (0..=g.len()).map(|i| g.get(i).copied()).collect()
    };
    assert_eq!(all(&t), all(&fresh));
    // (one in the middle written on: its rows and those after it, which moved, made again)
    laid[1] = msg(1, "an answer, longer now");
    follow(&mut t, &laid);
    let moved = parts(&t);
    let same: Vec<bool> = after.iter().zip(&moved).map(|(a, b)| Arc::ptr_eq(a, b)).collect();
    assert_eq!(same, [true, false, false, false, false]);
    let mut fresh = Transcript::new(crate::document::DocId::default());
    follow(&mut fresh, &laid);
    assert_eq!(all(&t), all(&fresh));
}

/// The view's top follows by rows: the cursor's row kept in sight, a paragraph's rows counted one by one.
#[test]
fn the_cursor_is_kept_in_sight_by_the_rows_drawn() {
    let mut t = Transcript::new(crate::document::DocId::default());
    let para: Vec<ChatRow> =
        (0..10).map(|i| ChatRow { join: (i > 0).then_some(true), ..row(&[&format!("w{i}")]) }).collect();
    follow(&mut t, &vec![(Of::Msg(0), rows(para))]);
    let d = t.pane.doc_mut();
    assert_eq!(mv::last_line(&d.text), 0, "one line, ten rows");
    let end = d.text.len_bytes() - 2;
    d.set_selection(crate::selection::Selection::point(end));
    in_sight(d, 4, 0);
    assert_eq!((d.top, d.top_skip()), (0, 6), "its last row at the bottom of four");
    assert_eq!(t.top_row(), 6);
}
