//! Claude sees the screen — tarae's `screen` tool: the editor's screen as text, as the user sees it. Asked
//! on a card each time: `y` once · `a` for the rest of the conversation (`Chat.sees`) · `n` no. What goes is
//! the frame drawn after the yes (`after_frame`, from `term::render`), so the card asking isn't on it. It
//! only reads — no keys, no clicks. The terminal panel's contents go too, and the card says so; with
//! `llm.screen-terminal` off its rows go blank (`screen::text_hiding`).

use serde_json::{Value, json};

use crate::allow::Scope;
use crate::editor::Editor;
use crate::tools::Call;

pub const TITLE: &str = "Claude wants to see your screen";

/// The note in place of the terminal panel when it's left out.
pub const LEFT_OUT: &str = "[the terminal panel — left out: llm.screen-terminal]";

pub fn tool_schema() -> Value {
    json!({
        "name": "screen",
        "description": "See the user's editor screen as text — what they see now: the code panes, side panel, \
    chat, cards, messages, status line, and the terminal panel's output when it's open (unless they leave it out). \
    For when what the user means is on their screen but not in the editor context. The user is asked first \
    (once, or for this conversation). It only looks: it can't type or click.",
        "inputSchema": { "type": "object", "properties": {} }
    })
}

/// A `screen` call: sent with the next frame if this conversation allows it, else asked on a card.
pub fn call(ed: &mut Editor, call: Call) {
    if ed.session.chat.as_ref().is_some_and(|c| c.sees) {
        return ed.screen_sends.push(call);
    }
    // (one card at a time — the same title would replace the card, and its call with it)
    if ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Screen { .. })) {
        let e = "the user is already being asked to show the screen — wait for that answer";
        return crate::tools::answer(ed, call, Err(e.into()));
    }
    let q = question(ed);
    let o =
        crate::offer::Offer::new(TITLE.into(), &q, "", String::new(), crate::offer::What::Screen { call });
    // (the terminal line at full strength: what the user may not think of as "the screen")
    ed.push_offer(o.deciding(1..2));
}

/// The card's text: what goes, and the terminal panel's part in it said plainly.
fn question(ed: &Editor) -> String {
    let size = ed.last_frame.as_ref().map(|f| format!(" ({}×{})", f.1, f.2)).unwrap_or_default();
    let terminal = match (ed.config.llm.screen_terminal, ed.screen.terminal_rows) {
        (true, Some((y0, y1))) => format!("Includes the terminal panel ({} rows).", y1 - y0 + 1),
        (true, None) => "The terminal panel too, once it's open.".to_string(),
        (false, _) => "Not the terminal panel (llm.screen-terminal is off).".to_string(),
    };
    format!(
        "The editor's screen goes to Claude as text{size} — the code, panels and chat as they are when you \
         answer. It can only look: no keys, no clicks.\n{terminal}"
    )
}

/// The user's answer: None = no; else it goes with the next frame (`a`: and later calls, unasked).
pub fn answer(ed: &mut Editor, id: u64, call: Call, scope: Option<Scope>) {
    ed.offer_finished(id, Ok(()));
    match scope {
        None => crate::tools::answer(ed, call, Err("the user said no to showing their screen".into())),
        Some(scope) => {
            if scope != Scope::Once
                && let Some(c) = ed.session.chat.as_mut()
            {
                c.sees = true;
            }
            ed.screen_sends.push(call);
        }
    }
}

/// A frame was drawn: the calls waiting for it get it, as text.
pub fn after_frame(ed: &mut Editor) {
    if ed.screen_sends.is_empty() {
        return;
    }
    let calls = std::mem::take(&mut ed.screen_sends);
    let text = text(ed);
    for call in calls {
        crate::tools::answer(ed, call, text.clone());
    }
}

/// The last frame as the tool's answer — the terminal panel blanked when it's left out.
fn text(ed: &Editor) -> Result<String, String> {
    let (frame, cols, rows) = ed.last_frame.as_ref().ok_or("nothing is on screen yet")?;
    let terminal = ed.screen.terminal_rows;
    let hide = terminal.filter(|_| !ed.config.llm.screen_terminal).map(|rows| crate::screen::Hidden {
        rows,
        cols_end: ed.screen.chat_x.unwrap_or(*cols as usize),
        note: LEFT_OUT.into(),
    });
    let text = crate::screen::text_hiding(frame, *cols, *rows, hide.as_ref())
        .ok_or("this screen can't be read as text")?;
    let part = match (terminal, &hide) {
        (Some(_), None) => " — the terminal panel included",
        (Some(_), Some(_)) => " — the terminal panel left out",
        (None, _) => "",
    };
    Ok(format!("The user's editor screen, {cols}×{rows}, as text (no colors){part}:\n\n{text}"))
}

/// The chat turn ended: a card still asking goes; its call needs no answer.
pub fn turn_over(ed: &mut Editor) {
    ed.offers.retain(|o| !matches!(o.what, crate::offer::What::Screen { .. }));
}
