//! `:messages` — the messages since tarae started as a floating list (panes P3 step 7c, plan §3.9; `floatlist.rs`):
//! oldest first, newest last — a log's order, as vim's `:messages` and a terminal's scrollback — opened at the newest
//! (or at a toast's message, its `enter`), in the session list's frame (`Frame::LIST`), focused at once (your key
//! opened it). A row: `▲` for an error or a warning in its colour, the text's lines joined with ` · `, its age at the
//! right. Its keys are a floating list's, and `y` copies the row's message whole, `enter` opens one too long for its
//! row whole in a buffer (the list used: it closes).

use crate::editor::{Editor, ToastKind};
use crate::pane::GroupKind;

/// A message's row.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Its id — the count of messages said when it came (`toast#N`).
    pub id: u64,
    pub kind: ToastKind,
    /// Its lines joined with ` · ` (what the row shows and its search matches).
    pub text: String,
    /// As it was said, lines and all.
    pub whole: String,
    pub at: std::time::Instant,
}

/// The messages' rows, oldest first.
pub fn rows(ed: &Editor) -> Vec<Row> {
    let ids = ed.message_ids();
    ed.messages
        .iter()
        .zip(ids)
        .map(|((at, kind, text), id)| Row {
            id,
            kind: *kind,
            text: one_line(text),
            whole: text.clone(),
            at: *at,
        })
        .collect()
}

/// A message's lines as one: its blank lines left out, the rest joined with ` · `.
fn one_line(text: &str) -> String {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ")
}

impl Editor {
    /// The messages' ids, oldest first: the newest is the count said (`Editor.said`), each before it one less — as
    /// `toast#N` names them.
    pub fn message_ids(&self) -> Vec<u64> {
        let n = self.messages.len() as u64;
        let first = (self.said + 1).saturating_sub(n);
        (first..first + n).collect()
    }

    /// `:messages` (or a focused toast's `enter`, `select` its message): the list up with the keys, at the newest —
    /// nothing said yet, said instead.
    pub fn messages_open(&mut self, select: Option<u64>) {
        let Some(&newest) = self.message_ids().last() else {
            return self.set_status("no messages yet");
        };
        let fresh = self.messages_pane.is_none();
        let select = select.or(fresh.then_some(newest));
        self.float_list_open(GroupKind::Messages, newest, select);
        // The toasts go: their words are the list's rows now — the same words never twice (user, 2026-10-23); what's
        // said while it's up is a row of it where the frame shows that row, else a toast, as once it closes
        // (`rows_drawn`)
        self.toasts_put_away();
    }

    /// `enter` on the list: the row's message whole in a buffer when its row can't show it all (lines of its own, or
    /// cut) — the list used, it closes; one shown whole is said.
    pub(crate) fn messages_enter(&mut self) {
        let Some(id) = self.float_list_here(GroupKind::Messages) else { return };
        let Some(row) = rows(self).into_iter().find(|r| r.id == id) else { return };
        let room = self.screen.messages_room.get();
        let cut = room > 0 && unicode_width::UnicodeWidthStr::width(row.text.as_str()) > room;
        if !cut && !row.whole.trim().contains('\n') {
            return self.set_status("shown whole — y copies it");
        }
        self.float_list_used(GroupKind::Messages);
        self.open_virtual(&format!("message #{id}"), &row.whole);
    }

    /// A letter on the list: `y` copies the row's message; the rest are said.
    pub(crate) fn messages_key(&mut self, c: char) {
        match c {
            'y' => {
                let id = self.float_list_here(GroupKind::Messages);
                let Some(row) = rows(self).into_iter().find(|r| Some(r.id) == id) else { return };
                crate::clipboard::copy(&self.events.jobs(), row.whole, "the message".into());
            }
            _ => self.key_unused(
                crate::key::Key::plain(crate::key::Code::Char(c)),
                "the messages",
                "y copies, / searches, esc code",
            ),
        }
    }
}

/// After a frame (`term::render`, `frame` as it goes out): a message said while the list is up (`Toast.row`) is only its
/// row where the finished frame shows that row whole (`term::shown_whole` — every later layer drawn: a floating pane
/// with the keys, a card, which-key, a picker); else — the list zoomed away, covered, or scrolled off its newest row
/// — it comes as a toast as usual (an error or a warning never only on the status line), the list's selection where it
/// was. True when one came as a toast: the frame is drawn again with it, the rest judged on that frame (a toast drawn
/// may cover a row); none: those left are rows, gone from the card's list.
pub fn rows_drawn(ed: &mut Editor, frame: &[u8], w: u16, h: u16) -> bool {
    if !ed.toasts.iter().any(|t| t.row) {
        return false;
    }
    let whole = crate::term::shown_whole(ed, frame, w, h);
    let mut came = false;
    for t in ed.toasts.iter_mut().filter(|t| t.row) {
        if !whole.contains(&format!("messages:toast#{}", t.id)) {
            (t.row, came) = (false, true);
        }
    }
    if !came {
        ed.toasts.retain(|t| !t.row);
    }
    came
}

/// The list's keys for its top line: `enter` where the row in hand is longer than its row, then `y`, `/`, `esc`, `q`.
pub fn hints(ed: &Editor) -> Vec<(&'static str, String)> {
    let id = ed.float_list_here(GroupKind::Messages);
    let room = ed.screen.messages_room.get();
    let long = rows(ed).into_iter().find(|r| Some(r.id) == id).is_some_and(|r| {
        r.whole.trim().contains('\n')
            || (room > 0 && unicode_width::UnicodeWidthStr::width(r.text.as_str()) > room)
    });
    let mut v: Vec<(&'static str, String)> = Vec::new();
    v.extend(long.then(|| ("enter", "open whole".to_string())));
    v.extend([
        ("y", "copy".to_string()),
        ("/", "search".to_string()),
        ("esc", "code".to_string()),
        ("q", "close".to_string()),
    ]);
    v
}

#[cfg(test)]
mod tests;
