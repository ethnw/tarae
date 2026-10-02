//! Terminal UI — redraws the whole frame every time over crossterm (synchronized updates, so no flicker).
//! A cell-diff buffer gets added when it becomes necessary (remote/slow terminals).

use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::Result;
use crossterm::cursor::{Hide, MoveTo, SetCursorStyle, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as CtEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetUnderlineColor};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{CursorShape, LineNumber};
use crate::editor::{Editor, Mode, cwd};
use crate::event::Event;
use crate::filetree::Kind;
use crate::graphemes;
use crate::key::Key;
use crate::llm::Review;
use crate::movement as mv;
use crate::picker::{Action, Picker};
use crate::selection::Range;
use crate::syntax;
use crate::theme::{Style, Theme};
use tree_sitter::QueryCursor;

pub fn run(editor: &mut Editor) -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange,
        EnableBracketedPaste
    )?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        prev(info);
    }));
    let res = event_loop(editor);
    restore()?;
    res
}

/// Is the terminal background light — asked once at startup to pick the `default` theme (meok/hanji).
/// Order: `TARAE_BACKGROUND=light|dark` → query terminal bg color (OSC 11) → `COLORFGBG` → unknown (dark).
/// DA1 (`CSI c`) is sent right after the query — nearly every terminal answers DA1, so even terminals that
/// don't know OSC 11 finish immediately instead of waiting for the timeout (usually under 1 ms).
pub fn detect_light_background() -> Option<bool> {
    match std::env::var("TARAE_BACKGROUND").as_deref() {
        Ok("light") => return Some(true),
        Ok("dark") => return Some(false),
        _ => {}
    }
    query_background().or_else(|| {
        // "fg;bg" — light if bg is 7 (white) or 15 (bright white)
        let v = std::env::var("COLORFGBG").ok()?;
        let bg: u8 = v.rsplit(';').next()?.parse().ok()?;
        Some(bg == 7 || bg == 15)
    })
}

fn query_background() -> Option<bool> {
    use std::io::Read;
    // SAFETY: isatty only looks at the fd.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        return None;
    }
    terminal::enable_raw_mode().ok()?;
    let mut reply = Vec::new();
    let mut out = io::stdout();
    let sent = out.write_all(b"\x1b]11;?\x1b\\\x1b[c").and_then(|_| out.flush()).is_ok();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let da1_done =
        |r: &[u8]| r.windows(3).position(|w| w == b"\x1b[?").is_some_and(|i| r[i..].contains(&b'c'));
    while sent && !da1_done(&reply) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: pfd is a single valid pollfd on this stack.
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) } <= 0 {
            break;
        }
        let mut buf = [0u8; 256];
        match io::stdin().read(&mut buf) {
            Ok(n) if n > 0 => reply.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let _ = terminal::disable_raw_mode();
    parse_osc11(&reply).map(|(r, g, b)| 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5)
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (1–4 hex digits each) → 0..1 components.
fn parse_osc11(reply: &[u8]) -> Option<(f64, f64, f64)> {
    let s = String::from_utf8_lossy(reply);
    let rgb = s.split("11;rgb:").nth(1)?;
    let mut it = rgb.split(['/', '\x1b', '\x07']).take(3).map(|h| {
        let h: String = h.chars().take_while(char::is_ascii_hexdigit).collect();
        let max = 16f64.powi(h.len() as i32) - 1.0;
        u32::from_str_radix(&h, 16).ok().map(|v| v as f64 / max)
    });
    Some((it.next()??, it.next()??, it.next()??))
}

fn restore() -> io::Result<()> {
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        Show,
        LeaveAlternateScreen
    )?;
    terminal::disable_raw_mode()
}

// Tests draw a scrolled line both ways — from the view's edge, and from the line's head — and compare.
#[cfg(test)]
thread_local!(static SKIP_SEEK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

fn tests_skip_seek() -> bool {
    #[cfg(test)]
    return SKIP_SEEK.get();
    #[cfg(not(test))]
    false
}

/// How long input rests before a screen sent in parts is drawn whole again (`event_loop`).
const RESTING_REDRAW: std::time::Duration = std::time::Duration::from_millis(500);

fn event_loop(editor: &mut Editor) -> Result<()> {
    spawn_input(editor.events.sender());
    crate::config::watch(editor.events.sender());
    crate::git::watch(editor.events.sender());
    let mut out = io::stdout();
    let mut draw = true;
    // Frames are drawn whole, but only what changed goes to the terminal (`screen.rs`): the main loop
    // waits while the terminal reads what it writes — measured, a whole screen was 24 KB a key typing.
    // It's drawn whole again when input rests after that (below), after a resize or focus coming back,
    // which a terminal may redraw its own way, and on `:redraw`.
    let mut frame = Vec::with_capacity(1 << 16);
    let mut screen = crate::screen::Screen::new();
    while !editor.should_quit {
        if draw {
            let (w, h) = terminal::size()?;
            if std::mem::take(&mut editor.redraw) {
                screen.invalidate();
            }
            frame.clear();
            render(editor, &mut frame, w, h)?;
            let bytes = screen.present(&frame, w, h);
            if !bytes.is_empty() {
                out.write_all(bytes)?;
                out.flush()?; // stdout holds back what has no newline
            }
        }
        // After only parts went out, the whole screen goes again once input rests: what a terminal leaves of
        // its own (Terminal.app, measured: a cursor drawn past its cell next to a wide character, a line up)
        // or anything else that drifted is gone a moment later — the next frame used to wipe it anyway.
        let ev = if screen.patched() {
            match editor.events.recv_within(RESTING_REDRAW) {
                Ok(Some(ev)) => ev,
                Ok(None) => {
                    screen.invalidate();
                    draw = true;
                    continue;
                }
                Err(()) => break,
            }
        } else {
            let Some(ev) = editor.events.recv() else { break };
            ev
        };
        if matches!(ev, Event::Resize | Event::Focus(true)) {
            screen.invalidate();
        }
        // Drawn unless every event of the batch said it changed nothing
        let mut unchanged = handle_noting(editor, ev);
        // Process all queued events (pastes, key bursts, job results), then draw only once — or after a
        // frame's time, so a flood of them can't keep the screen from updating.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(16);
        while !editor.should_quit
            && std::time::Instant::now() < until
            && let Some(ev) = editor.events.try_recv()
        {
            if matches!(ev, Event::Resize | Event::Focus(true)) {
                screen.invalidate();
            }
            unchanged &= handle_noting(editor, ev);
        }
        draw = !unchanged;
    }
    // (quitting waited for them already — this covers any other way out)
    editor.flush_saves();
    Ok(())
}

/// Handles `ev` — whether it said it changed nothing on screen.
fn handle_noting(editor: &mut Editor, ev: crate::event::Event) -> bool {
    editor.unchanged = false;
    editor.handle_event(ev);
    std::mem::take(&mut editor.unchanged)
}

/// Input thread — so terminal reads don't block the main loop.
fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            let ev = match ev {
                CtEvent::Key(k) if k.kind != KeyEventKind::Release => Key::from_event(k).map(Event::Key),
                CtEvent::Resize(..) => Some(Event::Resize),
                CtEvent::FocusGained => Some(Event::Focus(true)),
                CtEvent::FocusLost => Some(Event::Focus(false)),
                CtEvent::Paste(text) => Some(Event::Paste(text)),
                CtEvent::Mouse(m) => {
                    use crate::event::{Mouse, MouseKind};
                    let kind = match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => Some(MouseKind::Down),
                        MouseEventKind::Drag(MouseButton::Left) => Some(MouseKind::Drag),
                        MouseEventKind::Up(MouseButton::Left) => Some(MouseKind::Up),
                        MouseEventKind::ScrollUp => Some(MouseKind::ScrollUp),
                        MouseEventKind::ScrollDown => Some(MouseKind::ScrollDown),
                        _ => None,
                    };
                    kind.map(|kind| {
                        Event::Mouse(Mouse {
                            kind,
                            x: m.column,
                            y: m.row,
                            alt: m.modifiers.contains(KeyModifiers::ALT),
                        })
                    })
                }
                _ => None,
            };
            if let Some(ev) = ev
                && tx.send(ev).is_err()
            {
                break;
            }
        }
    });
}

/// One frame: layout → scroll → draw. Callable without a terminal (performance budget test).
pub fn render(editor: &mut Editor, out: &mut impl Write, w: u16, h: u16) -> io::Result<()> {
    // Reading an answer moved the editor to what the chat now shows (its links are only known once drawn)
    let mut frame = Vec::with_capacity(1 << 16);
    render_frame(editor, &mut frame, w, h)?;
    if crate::follow::reading(editor) {
        frame.clear();
        render_frame(editor, &mut frame, w, h)?;
    }
    out.write_all(&frame)
}

fn render_frame(editor: &mut Editor, out: &mut impl Write, w: u16, h: u16) -> io::Result<()> {
    let lay = Layout::new(editor, w, h);
    let ui = Ui::new(editor);
    let offer = OfferCard::new(editor, &lay, &ui);
    // The front card in sight since when (a new one, or new text, starts it over)
    let front = offer.as_ref().and(editor.offers.first()).map(|o| (o.id, o.rev));
    if front != editor.offer_seen.get().map(|(id, rev, _)| (id, rev)) {
        editor.offer_seen.set(front.map(|(id, rev)| (id, rev, std::time::Instant::now())));
    }
    // With split windows, scroll/viewport follow the focused pane
    let focus_lay = panes(editor, &lay).into_iter().find(|p| p.6).map_or_else(|| lay.clone(), |p| p.2);
    editor.viewport = (focus_lay.text_rows, focus_lay.text_cols);
    editor.screen = crate::editor::Screen {
        panes: Vec::new(),
        chat_x: (lay.chat_w > 0).then_some(lay.edit_w),
        picker_list: editor
            .picker
            .as_ref()
            .map(|p| (lay.text_top as usize + 3, PickerBox::new(&lay, p).list_rows)),
        picker_preview_x: editor.picker.as_ref().and_then(|p| PickerBox::new(&lay, p).preview_x()),
        popup: std::cell::Cell::new(None),
        offer_buttons: offer.as_ref().map(|c| c.buttons.clone()).unwrap_or_default(),
        chat_links: Default::default(),
        chat_mini: Default::default(),
        review_rail: Default::default(),
        sidebar: Default::default(),
        edit_w: Some(lay.edit_w),
        terminal_rows: (lay.debug_h > 0
            && editor.dap.is_none()
            && editor.terminal.as_ref().is_some_and(|t| t.visible))
        .then(|| {
            (lay.text_top as usize + lay.text_rows, lay.text_top as usize + lay.text_rows + lay.debug_h - 1)
        }),
        terminal_size: std::cell::Cell::new(Some((
            terminal_panel_h(lay.text_rows + lay.debug_h).saturating_sub(1).max(2) as u16,
            lay.edit_w.saturating_sub(2) as u16,
        ))),
        test_panel: Default::default(),
        plugin_panel: Default::default(),
        note_card: Default::default(),
        thought_card: Default::default(),
        toast_card: Default::default(),
    };
    editor.refresh_search(focus_lay.text_rows);
    // During `/` preview the screen follows the match, not the cursor
    if editor.search_origin.is_none() {
        scroll_to_cursor(editor, &focus_lay);
    }
    // The terminal is told its panel's size (it redraws for it)
    // (not while the debugger has the slot — the program would draw for a size nobody sees)
    if let Some(t) = editor.terminal.as_mut().filter(|t| t.visible)
        && lay.debug_h > 1
        && editor.dap.is_none()
    {
        t.resize((lay.debug_h - 1) as u16, lay.edit_w.saturating_sub(2) as u16);
    }
    if lay.rail_w > 0
        && editor.code_review.is_none()
        && let Some(sb) = editor.sidebar.as_mut()
    {
        let depths: Vec<usize> = sb.lines().iter().map(|l| l.depth).collect();
        sb.fit(sidebar_list_rows(&lay), &depths);
    }
    if lay.rail_w > 0
        && let Some(marked) = editor.code_review.as_ref().map(|r| r.marked(editor))
        && let Some(r) = editor.code_review.as_mut()
    {
        r.fit(marked, review_rail_room(&lay));
    }
    if let Some(p) = editor.picker.as_mut() {
        let pb = PickerBox::new(&lay, p);
        p.view = p.visible(pb.list_rows);
        if pb.preview_w > 0 {
            request_preview(p, &editor.events.jobs());
        }
    }
    // Reviewing: put the current change's head at the top 1/4 of the screen (follow — off once j/k scroll)
    let mut review_rows = None;
    if let Some(rev) = editor.review.as_mut().filter(|r| r.ready)
        && let Some(doc) = editor.docs.iter().find(|d| d.id == rev.doc_id)
    {
        let rows = rev.rows(&doc.text);
        if rev.follow
            && let Some(h) = rows.iter().position(|r| *r == crate::llm::RRow::Header(rev.current))
        {
            rev.scroll = h.saturating_sub(lay.text_rows.saturating_sub(1) / 4);
        }
        rev.scroll = rev.scroll.min(rows.len().saturating_sub(1));
        review_rows = Some(rows);
    }
    // Per pane, screen row → doc line (for the mouse); the focused pane's also goes into editor.view
    let mut screen_panes = Vec::new();
    for (id, r, pl, idx, top, left, focused) in panes(editor, &lay) {
        let view = build_view(editor, &editor.docs[idx], top, left, &pl);
        screen_panes.push((id, r, pl.gutter, view.iter().map(|v| v.origin(left)).collect()));
        if focused {
            editor.view = view;
        }
    }
    editor.screen.panes = screen_panes;
    draw(editor, &lay, &ui, review_rows.as_deref(), offer.as_ref(), out)
}

#[derive(Clone)]
struct Layout {
    width: usize,
    /// Leftmost column of this pane (differs per pane when split).
    x0: usize,
    /// Editing area width (minus the chat panel on the right) · chat panel width (0 when closed).
    edit_w: usize,
    chat_w: usize,
    /// Code review's file rail at the left of the editing area (0 when none) — panes start after it.
    rail_w: usize,
    header: bool,
    /// Rightmost cell of the editing area = scrollbar (only when the doc is taller than the screen).
    scrollbar: bool,
    text_top: u16,
    text_rows: usize,
    gutter: usize,
    text_cols: usize,
    /// Debug panel height (0 without a session) — just below the editing area (text_rows); chat panel beside.
    debug_h: usize,
    status_y: u16,
    cmd_y: u16,
}

impl Layout {
    fn new(editor: &Editor, w: u16, h: u16) -> Self {
        let header = editor.config.header || editor.docs.len() > 1;
        let text_top = header as u16;
        let total = (h as usize).saturating_sub(2 + text_top as usize).max(1);
        let terminal = editor.terminal.as_ref().is_some_and(|t| t.visible) && editor.dap.is_none();
        let debug_h = if terminal {
            terminal_panel_h(total)
        } else if editor.dap.is_some()
            || editor.test_run.is_some()
            || editor.task_run.is_some()
            || editor.plugins.as_ref().is_some_and(|p| p.panel.is_some())
        {
            (total / 3).clamp(7, 14).min(total.saturating_sub(4))
        } else {
            0
        };
        let text_rows = (total - debug_h).max(1);
        let digits = editor.doc().text.len_lines().to_string().len();
        // Docs with a language server or breakpoints get one sign column before the line numbers
        let doc = editor.doc();
        let signs = wants_signs(editor, doc);
        let gutter = digits.max(3) + 1 + signs as usize;
        let chat_w = chat_width(editor, w as usize);
        let scrollbar = editor.config.scrollbar && doc.text.len_lines() > total;
        let edit_w = w as usize - chat_w;
        // The review's file rail when there's room for it and the code (else the header says where you are)
        // …or the side panel (files / outline) — folded away when the code would get too narrow
        let review = editor.code_review.is_some();
        // Wider on a wide terminal (names need the room) — as long as the code keeps 100 columns; it only
        // changes with the terminal, never while you move in it
        let rail_w = if (review && edit_w >= 90)
            || (!review && editor.sidebar.is_some() && edit_w >= crate::sidebar::MIN_EDIT_W)
        {
            RAIL_W + (edit_w.saturating_sub(RAIL_W + 100) / 2).min(RAIL_MORE)
        } else {
            0
        };
        Self {
            width: w as usize,
            x0: 0,
            edit_w,
            chat_w,
            rail_w,
            header,
            scrollbar,
            text_top,
            text_rows,
            gutter,
            text_cols: edit_w.saturating_sub(rail_w + gutter + scrollbar as usize).max(1),
            debug_h,
            status_y: text_top + total as u16,
            cmd_y: text_top + total as u16 + 1,
        }
    }
}

impl Layout {
    /// One split pane's layout: in its rect (below any title row), the doc's line-number column + scrollbar.
    fn pane(
        &self,
        doc: &crate::document::Document,
        r: crate::split::Rect,
        title: bool,
        editor: &Editor,
    ) -> Layout {
        let rows = r.h.saturating_sub(title as usize).max(1);
        let digits = doc.text.len_lines().to_string().len();
        let signs = wants_signs(editor, doc);
        let gutter = digits.max(3) + 1 + signs as usize;
        let scrollbar = editor.config.scrollbar && doc.text.len_lines() > rows;
        Layout {
            x0: r.x,
            edit_w: r.x + r.w,
            text_top: (r.y + title as usize) as u16,
            text_rows: rows,
            gutter,
            scrollbar,
            text_cols: r.w.saturating_sub(gutter + scrollbar as usize).max(1),
            ..self.clone()
        }
    }
}

/// Sign column before line numbers needed? language server · diagnostics · breakpoints · debug session.
fn wants_signs(editor: &Editor, doc: &crate::document::Document) -> bool {
    doc.lsp.clients().next().is_some()
        || !doc.lsp.diagnostics.is_empty()
        || editor.dap.is_some()
        || doc.path.as_ref().is_some_and(|p| editor.breakpoints.get(p).is_some_and(|b| !b.is_empty()))
}

/// Panes: (view, rect, pane layout, doc index, top line, h-scroll, focused?). Left first (clearing to line
/// end wipes the pane to its right, so left goes first). One window = one pane as before (no title row).
#[allow(clippy::type_complexity)]
fn panes(
    editor: &Editor,
    lay: &Layout,
) -> Vec<(crate::split::ViewId, crate::split::Rect, Layout, usize, usize, usize, bool)> {
    let area = crate::split::Rect {
        x: lay.rail_w,
        y: lay.text_top as usize,
        w: lay.edit_w - lay.rail_w,
        h: lay.text_rows,
    };
    let many = editor.views.len() > 1;
    let mut out: Vec<_> = editor
        .split
        .layout(area)
        .into_iter()
        .filter_map(|(id, r)| {
            let v = editor.views.iter().find(|v| v.id == id)?;
            let focused = id == editor.focus;
            let idx =
                if focused { editor.current } else { editor.docs.iter().position(|d| d.id == v.doc)? };
            let doc = &editor.docs[idx];
            // Focused pane uses the doc's scroll (moved by scroll_to_cursor each frame); others their own —
            // clamped, since the doc may have shrunk under them (edited in another pane)
            let (top, left) =
                if focused { (doc.top, doc.left) } else { (v.top.min(mv::last_line(&doc.text)), v.left) };
            Some((id, r, lay.pane(doc, r, many, editor), idx, top, left, focused))
        })
        .collect();
    out.sort_by_key(|p| (p.1.x, p.1.y));
    out
}

/// Bottom debug panel: header row (status pill · what · key hints) + three columns — variables (colored
/// by shape) · call stack (current frame ▶) · output (stderr red). Card background sets it off from editing.
fn draw_debug_panel(
    d: &crate::dap::Dap,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::dap::State;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let color = |k: &str| t.try_get(k).and_then(|s| s.fg);
    let y0 = lay.text_top as usize + lay.text_rows;
    let w = lay.edit_w;
    // ── Header row
    let (label, pill) = match &d.state {
        State::Building => ("BUILDING", ui.accent.fg),
        State::Starting => ("STARTING", ui.accent.fg),
        State::Running => ("RUNNING", color("diff.plus")),
        State::Stopped { .. } => ("PAUSED", color("warning")),
        State::Exited(_) => ("EXITED", ui.linenr.fg),
    };
    queue!(out, MoveTo(0, y0 as u16))?;
    let mut used = panel_pill(out, card, pill, label)?;
    let what = match &d.state {
        State::Building => format!("  {}  {}", d.program, wave(d.started)),
        State::Stopped { reason } => {
            let at = d
                .frames
                .first()
                .map(|f| {
                    let file =
                        f.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                    format!(" · {}:{}", file.unwrap_or_default(), f.line + 1)
                })
                .unwrap_or_default();
            format!("  {}  {reason}{at}", d.program)
        }
        State::Exited(Some(c)) => format!("  {}  exit code {c}", d.program),
        _ => format!("  {}", d.program),
    };
    let what = fit_ellipsis(&what, w.saturating_sub(used + 2));
    apply(out, strong)?;
    queue!(out, Print(&what))?;
    used += what.width();
    let hints: &[(&str, &str)] = match d.state {
        State::Stopped { .. } if d.request == "attach" => {
            &[("F5", "continue"), ("F10", "over"), ("F11", "into"), ("F12", "out"), ("space G t", "detach")]
        }
        State::Stopped { .. } => {
            &[("F5", "continue"), ("F10", "over"), ("F11", "into"), ("F12", "out"), ("space G t", "stop")]
        }
        State::Exited(_) => &[("F5", "restart"), ("space G t", "close")],
        // Attached session: only detach (the remote program keeps running)
        _ if d.request == "attach" => &[("space G p", "pause"), ("space G t", "detach")],
        _ => &[("space G p", "pause"), ("space G t", "stop")],
    };
    panel_hints(out, hints, used, w, card, dim, faint)?;
    // ── Three columns: variables 45% · stack 25% · output the rest
    let rows = lay.debug_h.saturating_sub(1);
    let cw = [w * 45 / 100, w * 25 / 100];
    let cols = [(0, cw[0]), (cw[0] + 1, cw[1]), (cw[0] + cw[1] + 2, w.saturating_sub(cw[0] + cw[1] + 2))];
    let var_scope = color("variable").or(ui.base.fg);
    let cwd = canonical_cwd();
    let fn_color = color("function");
    for r in 0..rows {
        let y = (y0 + 1 + r) as u16;
        queue!(out, MoveTo(0, y))?;
        apply(out, card)?;
        queue!(out, Spaces(w))?;
        for (c, &(x, cw)) in cols.iter().enumerate() {
            if c > 0 {
                apply(out, faint)?;
                queue!(out, MoveTo((x - 1) as u16, y), Print("│"))?;
            }
            queue!(out, MoveTo((x + 1) as u16, y))?;
            let inner = cw.saturating_sub(2);
            let nw = editor.watches.len();
            if r == 0 {
                apply(out, Style { bold: true, ..faint })?;
                let first = if nw > 0 { "WATCH" } else { "VARIABLES" };
                queue!(out, Print([first, "CALL STACK", "OUTPUT"][c]))?;
                continue;
            }
            let i = r - 1;
            match c {
                // Watches on top, if any — `◦ expr  value`; if unresolvable in this frame, the reason dimmed
                0 if i < nw => {
                    let expr = &editor.watches[i];
                    apply(out, Style { fg: ui.accent.fg, ..card })?;
                    queue!(out, Print("◦ "))?;
                    let name = fit_ellipsis(expr, inner.saturating_sub(2).min(24));
                    apply(out, Style { fg: var_scope, ..card })?;
                    queue!(out, Print(&name))?;
                    let rest = inner.saturating_sub(name.width() + 4);
                    apply(out, card)?;
                    queue!(out, Print("  "))?;
                    match d.watches.get(expr) {
                        Some(crate::dap::Watch::Value { value, ty }) => {
                            value_and_type(out, editor, ui, card, value, ty, rest)?
                        }
                        Some(crate::dap::Watch::Error(m)) => {
                            apply(out, Style { italic: true, ..faint })?;
                            queue!(out, Print(fit_ellipsis(m, rest)))?;
                        }
                        _ => {
                            apply(out, faint)?;
                            queue!(out, Print(if d.stopped() { "…" } else { "—" }))?;
                        }
                    }
                }
                0 if nw > 0 && i == nw => {
                    apply(out, Style { bold: true, ..faint })?;
                    queue!(out, Print("VARIABLES"))?;
                }
                0 => {
                    let i = if nw > 0 { i - nw - 1 } else { i };
                    let Some(v) = d.vars.get(i) else {
                        if i == 0 {
                            apply(out, faint)?;
                            let msg =
                                if d.stopped() { "no locals" } else { "run to a breakpoint (F9 on a line)" };
                            queue!(out, Print(fit(msg, inner)))?;
                        }
                        continue;
                    };
                    // Expandable values (structs, lists) get ▸
                    apply(out, faint)?;
                    queue!(out, Print(if v.reference > 0 { "▸ " } else { "  " }))?;
                    let name = fit_ellipsis(&v.name, inner.saturating_sub(2).min(18));
                    apply(out, Style { fg: var_scope, ..card })?;
                    queue!(out, Print(&name))?;
                    let rest = inner.saturating_sub(name.width() + 4);
                    apply(out, card)?;
                    queue!(out, Print("  "))?;
                    value_and_type(out, editor, ui, card, &v.value, &v.ty, rest)?;
                }
                1 => {
                    let Some(f) = d.frames.get(i) else {
                        if i == 0 {
                            apply(out, faint)?;
                            // (paused, the frames may still be on their way)
                            let msg = if d.stopped() { "…" } else { "the call stack, when paused" };
                            queue!(out, Print(fit_ellipsis(msg, inner)))?;
                        }
                        continue;
                    };
                    let current = i == 0;
                    // Only my code's frames (inside the working dir) are crisp — stdlib/runtime frames dimmed
                    let mine =
                        f.path.as_ref().is_some_and(|p| cwd.as_ref().is_some_and(|c| p.starts_with(c)));
                    apply(out, if current { Style { fg: color("warning"), ..card } } else { faint })?;
                    queue!(out, Print(if current { "▶ " } else { "  " }))?;
                    let file = f
                        .path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| format!("{}:{}", n.to_string_lossy(), f.line + 1))
                        .unwrap_or_default();
                    let name =
                        fit_ellipsis(&f.name, inner.saturating_sub(2 + file.width().min(inner / 2) + 1));
                    apply(
                        out,
                        match (current, mine) {
                            (true, _) => Style { fg: fn_color, ..card },
                            (false, true) => card,
                            (false, false) => faint,
                        },
                    )?;
                    queue!(out, Print(&name))?;
                    let room = inner.saturating_sub(2 + name.width() + 1);
                    if room >= 4 {
                        apply(out, faint)?;
                        queue!(out, Print(" "), Print(fit_ellipsis(&file, room)))?;
                    }
                }
                _ => {
                    // Output fills from the bottom (so the latest is visible)
                    let lines = rows - 1;
                    let start = d.output.len().saturating_sub(lines);
                    let Some((cat, line)) = d.output.get(start + i) else {
                        if i == 0 && d.output.is_empty() {
                            apply(out, faint)?;
                            queue!(out, Print(fit_ellipsis("the program's output", inner)))?;
                        }
                        continue;
                    };
                    let st = match cat.as_str() {
                        "stderr" => Style { fg: ui.error.fg, ..card },
                        "stdout" => card,
                        _ => faint,
                    };
                    apply(out, st)?;
                    queue!(out, Print(fit(&line.replace('\t', "  "), inner)))?;
                }
            }
        }
    }
    Ok(())
}

/// Bottom panel header start: ` ▐ LABEL ▌` — a pill in `pill` color. Returns the cells used.
fn panel_pill(
    out: &mut impl Write,
    card: Style,
    pill: Option<crossterm::style::Color>,
    label: &str,
) -> io::Result<usize> {
    apply(out, card)?;
    queue!(out, Print(" "))?;
    let edge = Style { fg: pill, ..card };
    apply(out, edge)?;
    queue!(out, Print("▐"))?;
    apply(out, Style { fg: card.bg, bg: pill, bold: true, ..Style::default() })?;
    queue!(out, Print(format!(" {label} ")))?;
    apply(out, edge)?;
    queue!(out, Print("▌"))?;
    Ok(3 + label.width() + 2)
}

/// Rest of a bottom panel header (`used` cells drawn of `w`): key hints at the right end if they fit.
fn panel_hints(
    out: &mut impl Write,
    hints: &[(&str, &str)],
    mut used: usize,
    w: usize,
    card: Style,
    dim: Style,
    faint: Style,
) -> io::Result<()> {
    // As many as fit, from the first (the most useful) on
    let one = |(k, v): &(&str, &str)| k.width() + v.width() + 3;
    let (mut n, mut hint_w) = (0, 0);
    for h in hints {
        if used + hint_w + one(h) + 2 > w {
            break;
        }
        hint_w += one(h);
        n += 1;
    }
    if n > 0 {
        apply(out, card)?;
        queue!(out, Spaces(w - used - hint_w - 1))?;
        for (k, v) in &hints[..n] {
            apply(out, Style { bold: true, ..dim })?;
            queue!(out, Print(k))?;
            apply(out, faint)?;
            queue!(out, Print(format!(" {v}  ")))?;
        }
        used = w - 1;
    }
    apply(out, card)?;
    queue!(out, Spaces(w.saturating_sub(used)))
}

/// Debug panel value colored by shape, its type faint at the right end of `rest` cells (dropped if no room).
fn value_and_type(
    out: &mut impl Write,
    editor: &Editor,
    ui: &Ui,
    card: Style,
    value: &str,
    ty: &str,
    rest: usize,
) -> io::Result<()> {
    let ty = if ty.is_empty() { String::new() } else { fit_ellipsis(ty, 14) };
    let val_room = rest.saturating_sub(if ty.is_empty() { 0 } else { ty.width() + 2 });
    let shown = fit_ellipsis(value, val_room);
    let vc = editor.theme.try_get(crate::dap::value_scope(value)).and_then(|s| s.fg).or(ui.base.fg);
    apply(out, Style { fg: vc, ..card })?;
    queue!(out, Print(&shown))?;
    if !ty.is_empty() && rest >= shown.width() + ty.width() + 2 {
        apply(out, Style { fg: ui.linenr.fg, ..card })?;
        queue!(out, Spaces(rest - shown.width() - ty.width()), Print(&ty))?;
    }
    Ok(())
}

/// The terminal panel's height for `total` rows of editing area — more than the other panels (a shell wants
/// room), at most 24.
fn terminal_panel_h(total: usize) -> usize {
    // Never more than leaves 4 rows of code; too little room for a header and a row → none
    let h = (total * 2 / 5).clamp(8, 24).min(total.saturating_sub(4));
    if h < 2 { 0 } else { h }
}

/// A terminal color as the outer terminal's own (its palette for the 256 indexed ones), None = the panel's.
fn vt_color(c: vt100::Color) -> Option<crossterm::style::Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(crossterm::style::Color::AnsiValue(i)),
        vt100::Color::Rgb(r, g, b) => Some(crossterm::style::Color::Rgb { r, g, b }),
    }
}

/// The built-in terminal, below the code: a pill (TERMINAL — accent while it has the keys — or EXITED), what
/// runs, how far it's scrolled back, the keys; then its screen, cell by cell, in the program's colors (the
/// outer terminal's palette) over the panel's. Returns where its cursor goes when it has the keys.
fn draw_terminal_panel(
    t: &crate::terminal::Terminal,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<Option<(u16, u16)>> {
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong =
        card.patch(editor.theme.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let y0 = lay.text_top as usize + lay.text_rows;
    let w = lay.edit_w;
    let screen = t.parser.screen();
    queue!(out, MoveTo(0, y0 as u16))?;
    let (label, pill) = match (t.exited, t.focus) {
        (Some(_), _) => ("EXITED", ui.linenr.fg),
        (None, true) => ("TERMINAL", ui.accent.fg),
        (None, false) => ("TERMINAL", ui.virt.fg),
    };
    let mut used = panel_pill(out, card, pill, label)?;
    let mut head = vec![(format!("  {}", t.title), strong)];
    if let Some(code) = t.exited {
        head.push((
            format!("  exited {code}"),
            if code == 0 { dim } else { Style { fg: ui.error.fg, ..card } },
        ));
    }
    if screen.scrollback() > 0 {
        head.push((format!("  {} lines back", screen.scrollback()), faint));
    }
    for (text, st) in head {
        let text = fit_ellipsis(&text, w.saturating_sub(used + 2));
        apply(out, st)?;
        queue!(out, Print(&text))?;
        used += text.width();
    }
    let esc = editor.config.terminal_escape.as_str();
    let mut hints: Vec<(&str, &str)> = match (t.exited, t.focus) {
        (Some(_), _) => vec![("space T", "close")],
        (None, true) => vec![(esc, "back to the code")],
        (None, false) => vec![(esc, "type in it"), ("space T", "hide")],
    };
    // The program has the mouse: how to select in it anyway
    if t.exited.is_none() && screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
        hints.insert(0, ("alt+drag", "select"));
    }
    panel_hints(out, &hints, used, w, card, dim, faint)?;
    let rows = lay.debug_h.saturating_sub(1);
    let cols = w.saturating_sub(2);
    let sel = t.selected();
    // (the primary selection's color — the plain one barely shows on a card)
    let selection_bg = ["ui.selection.primary", "ui.selection"]
        .iter()
        .find_map(|k| editor.theme.try_get(k).and_then(|s| s.bg));
    for r in 0..rows {
        let y = (y0 + 1 + r) as u16;
        queue!(out, MoveTo(0, y))?;
        apply(out, card)?;
        queue!(out, Print(" "))?;
        let mut current = Some(card);
        let mut x = 0;
        while x < cols {
            let Some(cell) = screen.cell(r as u16, x as u16) else { break };
            if cell.is_wide_continuation() {
                x += 1;
                continue;
            }
            let (mut fg, mut bg) =
                (vt_color(cell.fgcolor()).or(card.fg), vt_color(cell.bgcolor()).or(card.bg));
            if cell.inverse() {
                (fg, bg) = (bg.or(card.bg), fg.or(card.fg));
            }
            // Selected with the mouse: on the selection's color, like text in the code
            if sel.is_some_and(|(a, b)| (a..=b).contains(&(r as u16, x as u16))) {
                bg = selection_bg.or(bg);
            }
            let st = Style {
                fg,
                bg,
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
                dim: cell.dim(),
                ..Style::default()
            };
            // Escapes only where the style changes (a screenful of plain text is one)
            if current != Some(st) {
                apply(out, st)?;
                current = Some(st);
            }
            let text = cell.contents();
            let wide = cell.is_wide() && x + 1 < cols;
            match text {
                "" => queue!(out, Print(" "))?,
                t if t.chars().next().is_some_and(char::is_control) => queue!(out, Print(" "))?,
                t => queue!(out, Print(t))?,
            }
            x += if wide { 2 } else { 1 };
        }
        apply(out, card)?;
        queue!(out, Spaces(w.saturating_sub(1 + x)))?;
    }
    let (cr, cc) = screen.cursor_position();
    let show = t.focus && t.exited.is_none() && !screen.hide_cursor() && screen.scrollback() == 0;
    Ok((show && (cr as usize) < rows).then(|| (1 + cc, (y0 + 1) as u16 + cr)))
}

/// A line of a test's or task's output: passing in green, failing (and errors) in red, stderr faint.
fn output_style(
    err: bool,
    l: &str,
    card: Style,
    good: Option<crossterm::style::Color>,
    bad: Option<crossterm::style::Color>,
    faint: Style,
) -> Style {
    let l = l.trim_start();
    let passing = l.starts_with("test result: ok")
        || l.starts_with("BUILD SUCCESS")
        || (l.starts_with("test ") && l.ends_with(" ok"))
        || l.starts_with("--- PASS")
        || l.starts_with("ok ")
        || l == "PASS"
        || l.starts_with("Finished")
        || (l.starts_with("===") && l.contains(" passed") && !l.contains(" failed"));
    let failing = !passing
        && (l.contains("FAILED")
            || l.contains("panicked at")
            || l.starts_with("--- FAIL")
            || l.starts_with("FAIL")
            || l.starts_with("E ")
            || l.starts_with("error")
            || l.contains(": error")
            || (l.contains(" failed") && !l.contains(" 0 failed")));
    let warning = !passing && !failing && (l.starts_with("warning") || l.contains(": warning"));
    if passing {
        Style { fg: good, ..card }
    } else if failing {
        Style { fg: bad, ..card }
    } else if warning {
        Style { fg: None, bold: true, ..card }
    } else if err {
        faint
    } else {
        card
    }
}

/// The build task's panel (below the code, where tests show theirs): a pill (running · ok · failed), the
/// task, what went into the list (`● 2  ▲ 1`), the time, the keys; then its output, the end showing.
fn draw_task_panel(
    r: &crate::task::Run,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::task::State;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let color = |k: &str| t.try_get(k).and_then(|s| s.fg);
    let (good, bad, warn) = (color("diff.plus"), ui.error.fg, color("warning"));
    let y0 = lay.text_top as usize + lay.text_rows;
    let w = lay.edit_w;
    let (label, pill) = match r.state {
        State::Running => ("RUNNING", ui.accent.fg),
        State::Passed if r.found == (0, 0) => ("OK", good),
        State::Passed => ("WARNINGS", warn),
        State::Failed(_) => ("FAILED", bad),
    };
    queue!(out, MoveTo(0, y0 as u16))?;
    let mut used = panel_pill(out, card, pill, label)?;
    let name = fit_ellipsis(&format!("  {}", r.task.name), (w / 3).max(12));
    apply(out, strong)?;
    queue!(out, Print(&name))?;
    used += name.width();
    let mut summary: Vec<(String, Style)> = Vec::new();
    for (n, g, c) in [(r.found.0, "●", bad), (r.found.1, "▲", warn)] {
        if n > 0 {
            summary.push((format!("   {g} "), Style { fg: c, ..card }));
            summary.push((n.to_string(), Style { bold: true, ..card }));
        }
    }
    let when = match (&r.state, r.took) {
        (State::Running, _) => format!("   {}", wave(r.started)),
        (_, Some(d)) => format!("   {}", duration(d.as_secs_f32())),
        _ => String::new(),
    };
    // The wave in the accent, as everywhere something is under way
    let running = matches!(r.state, State::Running);
    summary.push((when, if running { Style { fg: ui.accent.fg, ..card } } else { dim }));
    let sum_w: usize = summary.iter().map(|(s, _)| s.width()).sum();
    if used + sum_w + 2 < w {
        for (s, st) in &summary {
            apply(out, *st)?;
            queue!(out, Print(s))?;
        }
        used += sum_w;
    }
    // (`space m m` runs the user's task again — not an install, not Claude's command)
    let mut hints: Vec<(&str, &str)> = match (&r.state, r.found) {
        (State::Running, _) => vec![("space m c", "stop")],
        _ if r.install => vec![("space m c", "close")],
        (_, (0, 0)) => vec![("space m m", "again"), ("space m c", "close")],
        _ => vec![("]q", "next"), ("space q", "list"), ("space m m", "again"), ("space m c", "close")],
    };
    if r.by_claude {
        hints.retain(|h| h.0 != "space m m");
    }
    panel_hints(out, &hints, used, w, card, dim, faint)?;
    let rows = lay.debug_h.saturating_sub(1);
    let start = r.output.len().saturating_sub(rows);
    for i in 0..rows {
        let y = (y0 + 1 + i) as u16;
        queue!(out, MoveTo(0, y))?;
        apply(out, card)?;
        queue!(out, Spaces(w), MoveTo(2, y))?;
        match r.output.get(start + i) {
            Some((err, line)) => {
                apply(out, output_style(*err, line, card, good, bad, faint))?;
                queue!(out, Print(fit(&line.replace('\t', "  "), w.saturating_sub(4))))?;
            }
            None if i == 0 && r.output.is_empty() => {
                apply(out, faint)?;
                queue!(out, Print(fit_ellipsis(&r.task.cmd, w.saturating_sub(4))))?;
            }
            None => {}
        }
    }
    Ok(())
}

/// A plugin's panel (in place of the debug panel): its name (the accent while it has the keys) and title,
/// the keys it offers; then its table — columns as wide as their widest cell (the last takes what's left),
/// the selected row marked while focused — and its status line at the bottom.
///
/// ```text
///  KUBERNETES  pods · all namespaces                                   d describe  l logs  esc back
///  NAMESPACE    NAME                   READY  STATUS   RESTARTS
/// ▎default      web-fbc446b7c-8f5mj    1/1    Running  0
///  8 pods · watching
/// ```
fn draw_plugin_panel<W: Write>(
    st: &crate::plugin::PluginPanelState,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut W,
) -> io::Result<()> {
    let panel = &st.panel;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let selected = selected_row(ui, editor, card);
    let clean = |s: &str| crate::plugin::shown(s).replace(['\n', '\t'], " ");
    let y0 = lay.text_top as usize + lay.text_rows;
    let w = lay.edit_w;
    queue!(out, MoveTo(0, y0 as u16))?;
    let pill = if st.focus { ui.accent.fg } else { ui.linenr.fg };
    let mut used = panel_pill(out, card, pill, &st.plugin.to_uppercase())?;
    let title = fit_ellipsis(&format!("  {}", clean(&panel.title)), (w / 2).max(12));
    apply(out, strong)?;
    queue!(out, Print(&title))?;
    used += title.width();
    let keys: Vec<(String, String)> = panel.keys.iter().map(|(k, d)| (clean(k), clean(d))).collect();
    let mut hints: Vec<(&str, &str)> = keys.iter().map(|(k, d)| (k.as_str(), d.as_str())).collect();
    hints.push(if st.focus { ("esc", "back") } else { ("click", "keys") });
    panel_hints(out, &hints, used, w, card, dim, faint)?;
    // The table, and the status line under it
    let header = !panel.columns.is_empty();
    let body = lay.debug_h.saturating_sub(2 + header as usize);
    let n = panel.rows.len();
    // The selected row in sight
    let mut scroll = st.scroll.get().min(n.saturating_sub(body));
    if st.selected < scroll {
        scroll = st.selected;
    } else if body > 0 && st.selected >= scroll + body {
        scroll = st.selected + 1 - body;
    }
    st.scroll.set(scroll);
    let cols = st.widths.len();
    let widths: Vec<usize> = st.widths.iter().map(|&c| c.min(w / 2)).collect();
    let draw_cells = |out: &mut W, y: usize, cells: &[String], style: Style, mark: bool| -> io::Result<()> {
        queue!(out, MoveTo(0, y as u16))?;
        apply(out, if mark { selected } else { card })?;
        queue!(out, Spaces(w))?;
        if mark {
            apply(out, Style { fg: ui.accent.fg, ..selected })?;
            queue!(out, MoveTo(0, y as u16), Print("▎"))?;
        }
        let mut x = 2;
        for (c, cell) in cells.iter().enumerate() {
            let room =
                if c + 1 == cols { w.saturating_sub(x + 2) } else { widths[c].min(w.saturating_sub(x + 2)) };
            if room == 0 {
                break;
            }
            apply(out, if mark { selected } else { style })?;
            queue!(out, MoveTo(x as u16, y as u16), Print(fit_ellipsis(&clean(cell), room)))?;
            x += widths[c] + 2;
        }
        Ok(())
    };
    let mut y = y0 + 1;
    if header {
        draw_cells(out, y, &panel.columns, faint, false)?;
        y += 1;
    }
    let first_row_y = y;
    for i in 0..body {
        match panel.rows.get(scroll + i) {
            Some(r) => draw_cells(out, y, r, card, st.focus && scroll + i == st.selected)?,
            None => {
                queue!(out, MoveTo(0, y as u16))?;
                apply(out, card)?;
                queue!(out, Spaces(w))?;
            }
        }
        y += 1;
    }
    queue!(out, MoveTo(0, y as u16))?;
    apply(out, card)?;
    queue!(out, Spaces(w), MoveTo(2, y as u16))?;
    apply(out, dim)?;
    queue!(out, Print(fit_ellipsis(&clean(&panel.status), w.saturating_sub(4))))?;
    // For the mouse: the rows' screen lines, and the first row shown
    editor.screen.plugin_panel.set(Some((y0, y, first_row_y, scroll)));
    Ok(())
}

/// Test panel (in place of the debug panel):
///
/// ```text
///  FAILED  math::tests::*   ● 2  ▲ 1  ◦ 1   0.4s                 space x l again · space x d debug …
///  TESTS                          │ FAILURE  src/math.rs:21                          ]x next · [x prev
///  ● adds                         │ assertion `left == right` failed: 7 / 2 rounds down
/// ▎▲ divides                      │   left: 3
///  ◦ slow                         │  right: 4
/// ```
///
/// Until results can be read (building, compile error), the whole output. Shapes differ too
/// (● passed · ▲ failed · ◦ skipped).
fn draw_test_panel(
    r: &crate::testing::TestRun,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::test_results::Status;
    use crate::testing::RunState;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let color = |k: &str| t.try_get(k).and_then(|s| s.fg);
    let (good, bad) = (color("diff.plus"), ui.error.fg);
    let mark = |st: Status| match st {
        Status::Passed => ("●", Style { fg: good, ..card }),
        Status::Failed => ("▲", Style { fg: bad, ..card }),
        Status::Skipped => ("◦", faint),
    };
    let y0 = lay.text_top as usize + lay.text_rows;
    let w = lay.edit_w;
    let count = |st: Status| r.cases.iter().filter(|c| c.status == st).count();
    let (passed, failed, skipped) = (count(Status::Passed), count(Status::Failed), count(Status::Skipped));
    // Judge by results (if the build tool exits non-zero but no test failed, the reason is in the output)
    let (label, pill) = match r.state {
        RunState::Running => ("RUNNING", ui.accent.fg),
        RunState::Passed => ("PASSED", good),
        RunState::Failed(_) => ("FAILED", bad),
        RunState::NoTests => ("NO TESTS", color("warning")),
    };
    // ── Header row
    queue!(out, MoveTo(0, y0 as u16))?;
    let mut used = panel_pill(out, card, pill, label)?;
    let name = fit_ellipsis(&format!("  {}", r.target.label), (w / 3).max(12));
    apply(out, strong)?;
    queue!(out, Print(&name))?;
    used += name.width();
    // Count summary · elapsed time (wave while running)
    let mut summary: Vec<(String, Style)> = Vec::new();
    for (n, st) in [(passed, Status::Passed), (failed, Status::Failed), (skipped, Status::Skipped)] {
        if n > 0 {
            let (g, s) = mark(st);
            summary.push((format!("   {g} "), s));
            summary.push((n.to_string(), Style { bold: true, ..card }));
        }
    }
    let when = match (&r.state, r.took) {
        (RunState::Running, _) => format!("   {}", wave(r.started)),
        (_, Some(d)) => format!("   {}", duration(d.as_secs_f32())),
        _ => String::new(),
    };
    let running = matches!(r.state, RunState::Running);
    summary.push((when, if running { Style { fg: ui.accent.fg, ..card } } else { dim }));
    let sum_w: usize = summary.iter().map(|(s, _)| s.width()).sum();
    if used + sum_w + 2 < w {
        for (s, st) in &summary {
            apply(out, *st)?;
            queue!(out, Print(s))?;
        }
        used += sum_w;
    }
    let hints: &[(&str, &str)] = match r.state {
        RunState::Running => &[("space x c", "stop")],
        // (most useful first — the ones that don't fit fold away from the end)
        _ => {
            &[("space x l", "again"), ("space x o", "output"), ("space x c", "close"), ("space x d", "debug")]
        }
    };
    panel_hints(out, hints, used, w, card, dim, faint)?;

    let rows = lay.debug_h.saturating_sub(1);
    let line_style = |err: bool, l: &str| output_style(err, l, card, good, bad, faint);
    // The wheel's thumb on the card's right edge, when the detail doesn't fit
    let thumb = |detail_rows: usize, top: usize| {
        let n = r.detail_len();
        (n > detail_rows).then(|| scroll_thumb(detail_rows, n, top))
    };
    // No results yet — the whole output (following its end, unless the wheel moved it); a run where no
    // test ran says so above it (its last line may well read "ok")
    if r.cases.is_empty() {
        let note = usize::from(r.state == RunState::NoTests);
        let out_rows = rows.saturating_sub(note);
        editor.screen.test_panel.set(Some((y0, y0 + rows, out_rows)));
        let start = r.detail_top(out_rows);
        let bar = thumb(out_rows, start);
        for i in 0..rows {
            let y = (y0 + 1 + i) as u16;
            queue!(out, MoveTo(0, y))?;
            apply(out, card)?;
            queue!(out, Spaces(w), MoveTo(2, y))?;
            if i < note {
                apply(out, Style { fg: color("warning"), ..card })?;
                let why = "no test ran — nothing matched it. What the runner did:";
                queue!(out, Print(fit_ellipsis(why, w.saturating_sub(4))))?;
                continue;
            }
            let i = i - note;
            match r.output.get(start + i) {
                Some((err, line)) => {
                    apply(out, line_style(*err, line))?;
                    queue!(out, Print(fit(&line.replace('\t', "  "), w.saturating_sub(4))))?;
                }
                None if i == 0 && r.output.is_empty() => {
                    apply(out, faint)?;
                    queue!(out, Print(fit_ellipsis(&r.target.argv().join(" "), w.saturating_sub(4))))?;
                }
                None => {}
            }
            if bar.as_ref().is_some_and(|b| b.contains(&i)) {
                apply(out, faint)?;
                queue!(out, MoveTo(w.saturating_sub(1) as u16, y), Print("▐"))?;
            }
        }
        return Ok(());
    }
    // ── Two columns: result list · selected failure (or output if none)
    // List names drop the shared prefix (`math::tests::adds` → `adds` — the full name is in the header)
    let short = short_names(r.cases.iter().map(|c| c.name.as_str()));
    let name_w = short.iter().map(|n| n.width()).max().unwrap_or(10);
    let left_w = (name_w + 14).clamp(26, (w * 45 / 100).max(26)).min(w.saturating_sub(20));
    let right_x = left_w + 1;
    let right_w = w.saturating_sub(right_x + 1);
    let selected = r.picked();
    let list_rows = rows.saturating_sub(1);
    editor.screen.test_panel.set(Some((y0, y0 + rows, list_rows)));
    // Keep the selection visible; while running, keep the latest visible
    let first = if r.state == RunState::Running {
        r.cases.len().saturating_sub(list_rows)
    } else {
        r.selected.saturating_sub(list_rows.saturating_sub(1))
    };
    let cwd = &r.target.cwd;
    let rel = |p: &std::path::Path| p.strip_prefix(cwd).unwrap_or(p).display().to_string();
    let fails = r.failures();
    // Right column's lines — the window the wheel is at (a message from its top, the output at its end)
    let top = r.detail_top(list_rows);
    let bar = thumb(list_rows, top);
    let right: Vec<(String, Style)> = match selected {
        Some(c) if c.message.is_empty() => vec![("failed (no message)".into(), faint)],
        Some(c) => c
            .message
            .iter()
            .enumerate()
            .skip(top)
            .take(list_rows)
            .map(|(i, m)| (m.replace('\t', "  "), if i == 0 { Style { fg: bad, ..card } } else { card }))
            .collect(),
        None => r
            .output
            .iter()
            .skip(top)
            .take(list_rows)
            .map(|(e, l)| (l.replace('\t', "  "), line_style(*e, l)))
            .collect(),
    };
    for i in 0..rows {
        let y = (y0 + 1 + i) as u16;
        queue!(out, MoveTo(0, y))?;
        apply(out, card)?;
        queue!(out, Spaces(w))?;
        apply(out, faint)?;
        queue!(out, MoveTo(left_w as u16, y), Print("│"))?;
        if i == 0 {
            // Column title
            apply(out, Style { bold: true, ..faint })?;
            queue!(out, MoveTo(2, y), Print("TESTS"))?;
            queue!(out, MoveTo((right_x + 1) as u16, y))?;
            match selected {
                Some(c) => {
                    queue!(out, Print("FAILURE"))?;
                    let h = (fails.len() > 1)
                        .then(|| {
                            format!(
                                "]x next  [x prev   {}/{}",
                                fails.iter().position(|&f| f == r.selected).unwrap_or(0) + 1,
                                fails.len()
                            )
                        })
                        .filter(|h| right_w > h.width() + 30);
                    // The place keeps its file and line when it has to give way to the hint
                    if let Some((p, l)) = &c.at {
                        let room = right_w.saturating_sub(10 + h.as_ref().map_or(0, |h| h.width() + 3));
                        apply(out, faint)?;
                        queue!(out, Print("  "), Print(fit_left(&format!("{}:{}", rel(p), l + 1), room)))?;
                    }
                    if let Some(h) = h {
                        apply(out, faint)?;
                        queue!(out, MoveTo((w - h.width() - 2) as u16, y), Print(h))?;
                    }
                }
                None => queue!(out, Print("OUTPUT"))?,
            }
            continue;
        }
        // List
        if let Some(c) = r.cases.get(first + i - 1) {
            let idx = first + i - 1;
            let is_sel = selected.is_some() && idx == r.selected;
            let row_bg = if is_sel { selected_row(ui, editor, card) } else { card };
            queue!(out, MoveTo(0, y))?;
            apply(out, if is_sel { Style { fg: ui.accent.fg, ..row_bg } } else { row_bg })?;
            queue!(out, Print(if is_sel { "▎" } else { " " }))?;
            let (g, st) = mark(c.status);
            apply(out, Style { bg: row_bg.bg, ..st })?;
            queue!(out, Print(format!(" {g} ")))?;
            let secs = c.secs.filter(|s| *s >= 0.001).map(duration).unwrap_or_default();
            let room = left_w.saturating_sub(5 + secs.width() + 2);
            let nm = fit_ellipsis(&short[idx], room);
            apply(
                out,
                if c.status == Status::Failed || is_sel {
                    Style { bold: is_sel, ..row_bg }
                } else {
                    Style { fg: dim.fg, ..row_bg }
                },
            )?;
            queue!(out, Print(&nm))?;
            apply(out, Style { fg: faint.fg, ..row_bg })?;
            queue!(
                out,
                Spaces(left_w.saturating_sub(4 + nm.width() + secs.width() + 1)),
                Print(&secs),
                Print(" ")
            )?;
        }
        // Right
        if let Some((text, st)) = right.get(i - 1) {
            apply(out, *st)?;
            queue!(out, MoveTo((right_x + 1) as u16, y), Print(fit(text, right_w.saturating_sub(1))))?;
        }
        if bar.as_ref().is_some_and(|b| b.contains(&(i - 1))) {
            apply(out, faint)?;
            queue!(out, MoveTo(w.saturating_sub(1) as u16, y), Print("▐"))?;
        }
    }
    Ok(())
}

/// The working directory, canonical (frame paths are) — `canonicalize` hits the disk, so only once.
fn canonical_cwd() -> Option<&'static std::path::Path> {
    static CANON: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    CANON.get_or_init(|| std::fs::canonicalize(cwd()).ok()).as_deref()
}

/// Strip test names' shared prefix (up to a `::` · `.` · `/` boundary) — if only one, keep the last segment.
fn short_names<'a>(names: impl Iterator<Item = &'a str> + Clone) -> Vec<String> {
    let seps = |s: &str| {
        s.char_indices()
            .filter(|(_, c)| matches!(c, ':' | '.' | '/'))
            .map(|(i, c)| i + c.len_utf8())
            .collect::<Vec<_>>()
    };
    let list: Vec<&str> = names.collect();
    let cut = match list.as_slice() {
        [] => 0,
        [one] => seps(one).last().copied().unwrap_or(0),
        [first, rest @ ..] => seps(first)
            .into_iter()
            .rev()
            .find(|&i| rest.iter().all(|n| n.len() > i && n.as_bytes()[..i] == first.as_bytes()[..i]))
            .unwrap_or(0),
    };
    list.iter().map(|n| n[cut.min(n.len())..].to_string()).collect()
}

/// Draws the panes (one if single window). Several get a title row each and dividers between adjacent ones.
/// Returns the focused pane's cursor position.
fn draw_panes(
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<Option<(u16, u16)>> {
    let ps = panes(editor, lay);
    let many = ps.len() > 1;
    let mut cursor = None;
    for (_, r, pl, idx, top, left, focused) in &ps {
        let doc = &editor.docs[*idx];
        let own;
        let view = if *focused {
            &editor.view
        } else {
            own = build_view(editor, doc, *top, *left, pl);
            &own
        };
        if many {
            draw_pane_title(editor, doc, *r, *focused, ui, out)?;
        }
        let c = draw_text(editor, doc, *top, *left, *focused, view, ui, pl, out)?;
        if *focused {
            cursor = c;
        }
    }
    // Dividers between side-by-side panes (plain line at title-row height, no ┬) — last (after left clears)
    // In the theme's `ui.window` (Helix's key for them), else faint
    let window = editor.theme.try_get("ui.window").and_then(|s| s.fg).or(ui.linenr.fg);
    let faint = Style { fg: window, ..ui.base };
    for (_, r, ..) in ps.iter().filter(|p| p.1.x > 0) {
        apply(out, faint)?;
        for y in r.y..r.y + r.h {
            queue!(out, MoveTo((r.x - 1) as u16, y as u16), Print("│"))?;
        }
    }
    Ok(cursor)
}

/// Pane title row: `▎ main.rs ●` — focused pane gets accent bar, bold name, faint band; others dimmed.
fn draw_pane_title(
    editor: &Editor,
    doc: &crate::document::Document,
    r: crate::split::Rect,
    focused: bool,
    ui: &Ui,
    out: &mut impl Write,
) -> io::Result<()> {
    let t = &editor.theme;
    let band = if focused { blend(ui.accent.fg, ui.tint, 0.10).or(ui.base.bg) } else { ui.base.bg };
    let base = Style { bg: band, ..ui.base };
    queue!(out, MoveTo(r.x as u16, r.y as u16))?;
    apply(
        out,
        if focused { Style { fg: ui.accent.fg, ..base } } else { Style { fg: ui.linenr.fg, ..base } },
    )?;
    queue!(out, Print(" ▎ "))?;
    let name = fit_ellipsis(&doc.display_name(), r.w.saturating_sub(8));
    let name_style = if focused {
        base.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }))
    } else {
        Style { fg: ui.virt.fg, ..base }
    };
    apply(out, Style { bg: band, ..name_style })?;
    queue!(out, Print(&name))?;
    let mut used = 3 + name.width();
    if doc.is_modified() {
        apply(out, Style { fg: ui.accent.fg, ..base })?;
        queue!(out, Print(" ●"))?;
        used += 2;
    }
    apply(out, base)?;
    queue!(out, Spaces(r.w.saturating_sub(used)))
}

/// Chat panel width: 2/5 of the screen (36–72 cells). If editing area < 40 cells, it takes the whole screen.
fn chat_width(editor: &Editor, w: usize) -> usize {
    if editor.chat.is_none() {
        return 0;
    }
    // Following: minimized to a floating card — the code gets the whole width
    if editor.chat.as_ref().is_some_and(|c| c.compact()) {
        return 0;
    }
    let c = (w * 2 / 5).clamp(36, 72);
    if w < c + 40 { w } else { c }
}

/// Line-number column width (excluding the diagnostic sign column).
fn num_w(doc: &crate::document::Document) -> usize {
    doc.text.len_lines().to_string().len().max(3) + 1
}

/// Cursor char to draw: in insert mode the head (between chars), otherwise the block cursor char.
fn cursor_idx(mode: Mode, r: Range, text: &ropey::Rope) -> usize {
    if mode == Mode::Insert { r.head } else { r.cursor(text) }
}

fn scroll_to_cursor(editor: &mut Editor, lay: &Layout) {
    let so = editor.config.scrolloff.min(lay.text_rows.saturating_sub(1) / 2);
    let tab = editor.config.tab_width;
    let mode = editor.mode;
    let wrap_w = editor.wraps(editor.doc()).then_some(lay.text_cols);
    let doc = editor.doc_mut();
    let pos = cursor_idx(mode, doc.selection().primary(), &doc.text);
    let line = mv::line_of(&doc.text, pos);
    // Rows a line takes: its wrapped rows, plus the removed rows a code review shows above it
    let reviewing = doc.review_base.is_some();
    if wrap_w.is_some() || reviewing {
        let doc = &*doc;
        let wrapped = |l: usize| match wrap_w {
            Some(w) => crate::wrap::rows(&doc.text, l, w, tab),
            None => vec![crate::wrap::Row { start: 0, col: 0, end_col: usize::MAX, x0: 0 }],
        };
        let rows = |l: usize| wrapped(l).len() + removed_above(doc, l);
        let here = wrapped(line);
        // Screen rows between the top and the cursor's row: its row in the line, below the removed ones
        let k = if wrap_w.is_some() { crate::wrap::row_of(&here, pos) } else { 0 } + removed_above(doc, line);
        let mut above = k;
        let mut top = line;
        while top > 0 && above < so {
            top -= 1;
            above += rows(top);
        }
        let mut new_top = doc.top;
        if new_top > top {
            new_top = top;
        } else {
            // Each line is at least a row — a top more than a screen above can't show the cursor
            if line >= new_top + lay.text_rows {
                new_top = line + 1 - lay.text_rows;
            }
            // Rows from the top through the cursor's, plus `so` below, must fit
            let mut used: usize = (new_top..line).map(rows).sum::<usize>() + k + 1;
            while new_top < line && used + so > lay.text_rows {
                used -= rows(new_top);
                new_top += 1;
            }
        }
        let wraps_here = here.len() > 1
            || mv::line_end(&doc.text, line) - mv::line_start(&doc.text, line) <= crate::wrap::MAX_LINE;
        let doc = editor.doc_mut();
        doc.top = new_top;
        // Wrapped lines never scroll sideways (a line too long to wrap still does)
        if wrap_w.is_some() && wraps_here {
            doc.left = 0;
            return;
        }
    } else if line < doc.top + so {
        doc.top = line.saturating_sub(so);
    } else if line + so >= doc.top + lay.text_rows {
        doc.top = line + so + 1 - lay.text_rows;
    }
    let doc = editor.doc_mut();
    let col = mv::visual_col(&doc.text, pos, tab);
    // The whole cursor cell must fit (a wide char at the right edge would be clipped, hiding the cursor)
    let g = doc.text.byte_slice(pos..graphemes::next_boundary(&doc.text, pos)).to_string();
    let end = col + graphemes::cluster_width(&g, col, tab.max(1)).max(1);
    if col < doc.left {
        doc.left = col;
    } else if end > doc.left + lay.text_cols {
        doc.left = end.saturating_sub(lay.text_cols).min(col);
    }
}

// ── Theme → this frame's UI styles ─────────────────────────────────────────

/// UI keys the theme lacks use built-in values — so a partial user theme never hides the selection or cursor.
fn builtin() -> &'static Theme {
    static B: OnceLock<Theme> = OnceLock::new();
    B.get_or_init(Theme::builtin)
}

#[derive(Clone)]
struct Ui {
    base: Style,
    /// Base bg for blending faint colors — usually the bg; for transparent themes the original (`ui.tint`).
    /// Painting uses `base.bg` (terminal bg if none).
    tint: Option<crossterm::style::Color>,
    selection: Style,
    selection_primary: Style,
    cursor: Style,
    cursor_primary: Style,
    cursorline: Style,
    linenr: Style,
    linenr_selected: Style,
    status: Style,
    status_mode: Style,
    virt: Style,
    error: Style,
    diff_plus: Style,
    diff_minus: Style,
    /// Accent color (ui.accent → primary cursor bg → special) — selected-row bar, typed chars, mode pill…
    accent: Style,
    /// Diagnostics [_, error, warning, info, hint]: underline style, sign (●) color
    diag: [Style; 5],
    sign: [Style; 5],
    /// Set while drawing the focused pane: the cursor line's diagnostics all fit at its end (no card needed).
    cursor_diags_shown: std::cell::Cell<bool>,
}

impl Ui {
    fn new(editor: &Editor) -> Self {
        let t = &editor.theme;
        let get = |k: &str| t.try_get(k).or_else(|| builtin().try_get(k)).unwrap_or_default();
        let mode = match editor.mode {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Select => "select",
        };
        let base = t.get("ui.background").patch(get("ui.text"));
        let tint = t.try_get("ui.tint").and_then(|s| s.bg).or(base.bg);
        let status = base.patch(get("ui.statusline"));
        let selection = base.patch(get("ui.selection"));
        let cursor = base.patch(get("ui.cursor"));
        Ui {
            tint,
            selection_primary: t.try_get("ui.selection.primary").map_or(selection, |s| base.patch(s)),
            cursor_primary: t
                .try_get(&format!("ui.cursor.primary.{mode}"))
                .or_else(|| t.try_get("ui.cursor.primary"))
                .map_or(cursor, |s| base.patch(s)),
            cursorline: base.patch(get("ui.cursorline.primary")),
            linenr: base.patch(get("ui.linenr")),
            // Current line number in the mode color too (same as the pill — the mode shows where the eye is)
            linenr_selected: {
                let sel = base.patch(get("ui.linenr.selected"));
                match t.try_get(&format!("ui.statusline.{mode}")).and_then(|s| s.bg) {
                    Some(c) if editor.config.color_modes && mode != "normal" => Style { fg: Some(c), ..sel },
                    _ => sel,
                }
            },
            status_mode: if editor.config.color_modes {
                status.patch(get(&format!("ui.statusline.{mode}")))
            } else {
                Style { bold: true, ..status }
            },
            accent: Style { fg: accent_color(t), ..Style::default() },
            virt: base.patch(get("ui.virtual")),
            error: base.patch(get("error")),
            diff_plus: base.patch(get("diff.plus")),
            diff_minus: base.patch(get("diff.minus")),
            diag: ["", "error", "warning", "info", "hint"].map(|k| get(&format!("diagnostic.{k}"))),
            sign: ["", "error", "warning", "info", "hint"]
                .map(|k| Style { fg: get(k).fg, ..Style::default() }),
            cursor_diags_shown: std::cell::Cell::new(false),
            base,
            selection,
            cursor,
            status,
        }
    }
}

/// The one accent: `ui.accent` (tarae's own key), else the primary cursor's background, else `special` —
/// themes written for Helix get theirs from what they already say.
fn accent_color(t: &Theme) -> Option<crossterm::style::Color> {
    t.try_get("ui.accent")
        .and_then(|s| s.fg)
        .or_else(|| t.try_get("ui.cursor.primary").and_then(|s| s.bg))
        .or_else(|| t.try_get("special").and_then(|s| s.fg))
        .or_else(|| builtin().try_get("ui.accent").and_then(|s| s.fg))
}

/// Apply a style wholesale (clearing the previous one).
fn apply(out: &mut impl Write, s: Style) -> io::Result<()> {
    // One sequence — reset, colors, attributes (`CSI 0;38;2;r;g;b;48;…;1 m`): the most frequent write of a
    // frame, built by hand rather than as crossterm's sequence a part, each through `fmt`
    let colors = !crossterm::style::Colored::ansi_color_disabled_memoized();
    let mut buf = [0u8; 64]; // the longest: 3 + 2 × 17 (`;38;2;255;255;255`) + 6 × 2 + 1
    let mut seq = io::Cursor::new(&mut buf[..]);
    seq.write_all(b"\x1b[0")?;
    for (on, c) in [(b";38", s.fg), (b";48", s.bg)] {
        if let Some(c) = c.filter(|_| colors) {
            sgr_color(&mut seq, on, c)?;
        }
    }
    let fancy = undercurl_supported();
    let curly = s.curly && fancy;
    for (on, a) in [
        (s.bold, b";1"),
        (s.italic, b";3"),
        (s.underline && !curly, b";4"),
        (s.dim, b";2"),
        (s.reversed, b";7"),
        (s.crossed, b";9"),
    ] {
        if on {
            seq.write_all(a)?;
        }
    }
    seq.write_all(b"m")?;
    let n = seq.position() as usize;
    out.write_all(&buf[..n])?;
    // Undercurl and its color apart — a colon subparameter (`4:3`) is safest in a sequence of its own
    if fancy
        && colors
        && let Some(c) = s.ul_color
    {
        queue!(out, SetUnderlineColor(c))?;
    }
    if s.underline && curly {
        queue!(out, SetAttribute(Attribute::Undercurled))?;
    }
    Ok(())
}

/// Per byte of `range`: the capture that paints it (`u32::MAX` none) — spans in draw order, so outer ones go
/// first and inner·later patterns win.
fn paint_spans(spans: &[(usize, usize, u32)], range: std::ops::Range<usize>) -> Vec<u32> {
    let mut paint = vec![u32::MAX; range.len()];
    for &(a, b, c) in spans {
        let (a, b) = (a.max(range.start), b.min(range.end));
        if a < b {
            paint[a - range.start..b - range.start].fill(c);
        }
    }
    paint
}

/// `n` blanks — padding, written without building a string of them.
struct Spaces(usize);

impl crossterm::Command for Spaces {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        const BLANKS: &str = "                                                                ";
        let mut n = self.0;
        while n > 0 {
            let k = n.min(BLANKS.len());
            f.write_str(&BLANKS[..k])?;
            n -= k;
        }
        Ok(())
    }
}

/// `;38;2;r;g;b` · `;38;5;n` (`on` = `;38` foreground, `;48` background) — what crossterm writes for a color.
fn sgr_color(out: &mut impl Write, on: &[u8], c: Color) -> io::Result<()> {
    let num = |out: &mut dyn Write, n: u8| {
        let d = [b';', b'0' + n / 100, b'0' + n / 10 % 10, b'0' + n % 10];
        let lead = match n {
            100.. => 0,
            10.. => 1,
            _ => 2,
        };
        out.write_all(&[d[0]])?;
        out.write_all(&d[1 + lead..])
    };
    let ansi = match c {
        Color::Rgb { r, g, b } => {
            out.write_all(on)?;
            out.write_all(b";2")?;
            for v in [r, g, b] {
                num(out, v)?;
            }
            return Ok(());
        }
        // Default color: 39 / 49
        Color::Reset => return out.write_all(if on == b";38" { b";39" } else { b";49" }),
        Color::AnsiValue(v) => v,
        Color::Black => 0,
        Color::DarkRed => 1,
        Color::DarkGreen => 2,
        Color::DarkYellow => 3,
        Color::DarkBlue => 4,
        Color::DarkMagenta => 5,
        Color::DarkCyan => 6,
        Color::Grey => 7,
        Color::DarkGrey => 8,
        Color::Red => 9,
        Color::Green => 10,
        Color::Yellow => 11,
        Color::Blue => 12,
        Color::Magenta => 13,
        Color::Cyan => 14,
        Color::White => 15,
    };
    out.write_all(on)?;
    out.write_all(b";5")?;
    num(out, ansi)
}

/// Does the terminal grok undercurl/underline color (`CSI 4:3 m`, `CSI 58…`)? Unknown terminals print them
/// as text ("3m") — so only where certain, plain underline elsewhere. Force with `TARAE_UNDERCURL=1/0`.
fn undercurl_supported() -> bool {
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        match env("TARAE_UNDERCURL").as_str() {
            "1" => return true,
            "0" => return false,
            _ => {}
        }
        let (term, prog) = (env("TERM"), env("TERM_PROGRAM"));
        ["kitty", "wezterm", "ghostty", "alacritty", "foot", "contour"].iter().any(|t| term.contains(t))
            || ["WezTerm", "ghostty", "iTerm.app", "vscode", "kitty"].contains(&prog.as_str())
            || std::env::var_os("ZELLIJ").is_some()
    })
}

/// Clear the rest of the line with the default style (background).
fn clear_rest(out: &mut impl Write, base: Style) -> io::Result<()> {
    apply(out, base)?;
    queue!(out, Clear(ClearType::UntilNewLine))
}

fn cursor_shape(editor: &Editor) -> CursorShape {
    let (n, i, s) = editor.config.cursor_shape;
    match editor.mode {
        Mode::Normal => n,
        Mode::Insert => i,
        Mode::Select => s,
    }
}

/// `review_rows` = the ready review's rows, `offer` = the front offer card (both worked out by `render`).
fn draw(
    editor: &Editor,
    lay: &Layout,
    ui: &Ui,
    review_rows: Option<&[crate::llm::RRow]>,
    offer: Option<&OfferCard>,
    out: &mut impl Write,
) -> io::Result<()> {
    queue!(out, BeginSynchronizedUpdate, Hide)?;
    if lay.header {
        draw_header(editor, ui, lay, out)?;
    }
    // Narrow screen with the chat open: the chat takes it all — no text, nothing floating over it
    let text_cursor = match editor.review.as_ref().filter(|r| r.ready) {
        _ if lay.edit_w == 0 => None,
        Some(rev) => {
            if let Some(rows) = review_rows {
                draw_review(rev, rows, editor, ui, lay, out)?;
            }
            None
        }
        None if editor.welcome() => {
            draw_welcome(editor, ui, lay, out)?;
            None
        }
        None => draw_panes(editor, ui, lay, out)?,
    };
    if let Some(r) = &editor.code_review
        && lay.rail_w > 0
        && !editor.review.as_ref().is_some_and(|r| r.ready)
        && !editor.welcome()
    {
        draw_review_rail(r, editor, ui, lay, out)?;
    } else if let Some(sb) = &editor.sidebar
        && lay.rail_w > 0
        && editor.code_review.is_none()
        && !editor.review.as_ref().is_some_and(|r| r.ready)
    {
        draw_sidebar(sb, editor, ui, lay, out)?;
    }
    let mut terminal_cursor = None;
    if lay.debug_h > 0 && lay.edit_w > 0 {
        if let Some(d) = &editor.dap {
            draw_debug_panel(d, editor, ui, lay, out)?;
        } else if let Some(t) = editor.terminal.as_ref().filter(|t| t.visible) {
            terminal_cursor = draw_terminal_panel(t, editor, ui, lay, out)?;
        } else if let Some(st) = editor.plugins.as_ref().and_then(|p| p.panel.as_ref()) {
            draw_plugin_panel(st, editor, ui, lay, out)?;
        } else {
            // A test run and a task share the slot: the one started last shows
            match (&editor.test_run, &editor.task_run) {
                (Some(t), Some(k)) if k.started > t.started => draw_task_panel(k, editor, ui, lay, out)?,
                (Some(t), _) => draw_test_panel(t, editor, ui, lay, out)?,
                (None, Some(k)) => draw_task_panel(k, editor, ui, lay, out)?,
                (None, None) => {}
            }
        }
    }
    let chat_cursor = match &editor.chat {
        Some(c) if lay.chat_w > 0 => draw_chat(c, editor, ui, lay, out)?,
        _ => None,
    };
    if let Some(rev) = editor.review.as_ref().filter(|r| !r.ready) {
        draw_ask_stream(rev, editor, ui, lay, out)?;
    }
    // Cards over the code share the screen without overlapping: the note at the cursor and an offer (a
    // command Claude wants to run) are placed first, the minimized chat takes a free corner, the thought
    // card what's left (each skipped if nothing is)
    let note_rect = text_cursor.and_then(|at| note_card_geom(editor, at, ui, lay)).map(|g| g.rect());
    let offer_rect = offer.as_ref().map(|o| o.rect());
    let mini_rect = editor.chat.as_ref().filter(|c| c.compact()).and_then(|c| {
        let avoid: Vec<Rect4> = [note_rect, offer_rect].into_iter().flatten().collect();
        chat_mini_rect(c, editor, ui, lay, &avoid)
    });
    // (after the turn, a walk-through's stop keeps its card while the cursor is in it — `space n` asks there)
    if let Some(c) = editor.chat.as_ref().filter(|c| {
        c.follow.on && (c.busy() || c.follow.touring() || crate::follow::stop_here(editor).is_some())
    }) && editor.review.is_none()
        && lay.edit_w > 0
        && !editor.welcome()
    {
        let taken: Vec<Rect4> = [note_rect, mini_rect, offer_rect].into_iter().flatten().collect();
        draw_thought_card(c, editor, ui, &taken, out)?;
    }
    if let (Some(c), Some(rect)) = (editor.chat.as_ref(), mini_rect) {
        draw_chat_mini(c, editor, ui, rect, out)?;
    }
    if let (Some(lines), Some(at)) = (&editor.popup, text_cursor) {
        draw_popup(lines, at, ui, editor, lay, out)?;
    }
    if let (Some(sig), Some(at)) = (&editor.signature, text_cursor) {
        draw_signature(sig, at, ui, editor, lay, out)?;
    }
    if let (Some(c), Some(at)) = (&editor.completion, text_cursor) {
        draw_completion(c, at, ui, editor, lay, out)?;
    }
    // The note under the cursor comes first; the diagnostic card otherwise
    if let Some(at) = text_cursor
        && !draw_note_card(editor, at, ui, lay, out)?
        && !draw_conflict_card(editor, at, ui, lay, out)?
    {
        draw_diagnostic_card(editor, at, ui, lay, out)?;
    }
    // The notification card keeps out of every other card's way — or folds into the status line
    let taken: Vec<Rect4> =
        [note_rect, mini_rect, editor.screen.thought_card.get(), offer_rect].into_iter().flatten().collect();
    let toast_folded = !draw_toasts(editor, ui, lay, &taken, out)?;
    if let Some(card) = offer {
        card.draw(editor, ui, out)?;
    }
    if !editor.pending.is_empty() && editor.picker.is_none() {
        draw_which_key(editor, ui, lay, out)?;
    }
    let picker_cursor = match &editor.picker {
        Some(p) => Some(draw_picker(p, editor, ui, lay, out)?),
        None => None,
    };
    draw_statusline(editor, ui, lay, toast_folded.then(|| editor.toasts.last()).flatten(), out)?;
    let completion = editor.cmdline_completion();
    draw_cmdline_menu(editor, completion.as_ref(), ui, lay, out)?;
    let prompt_cursor = draw_cmdline(editor, completion.as_ref(), ui, lay, out)?
        .or(picker_cursor)
        .or(chat_cursor)
        .or(terminal_cursor);

    let (pos, shape) = match prompt_cursor {
        // A shell's cursor is a block
        Some(p) if Some(p) == terminal_cursor => (Some(p), CursorShape::Block),
        Some(p) => (Some(p), CursorShape::Bar),
        None => (text_cursor, cursor_shape(editor)),
    };
    if let Some((x, y)) = pos {
        let style = match shape {
            CursorShape::Block => SetCursorStyle::SteadyBlock,
            CursorShape::Bar => SetCursorStyle::SteadyBar,
            CursorShape::Underline => SetCursorStyle::SteadyUnderScore,
        };
        queue!(out, MoveTo(x, y), style, Show)?;
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor, EndSynchronizedUpdate)?;
    out.flush()
}

/// Draw the text area and return the primary cursor's screen position.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    editor: &Editor,
    doc: &crate::document::Document,
    top: usize,
    left: usize,
    focused: bool,
    view: &[crate::doccomment::ViewRow],
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<Option<(u16, u16)>> {
    // The frame's flag, not the copy's below
    let cursor_diags_shown = &ui.cursor_diags_shown;
    // Unfocused panes are toned down a step (dim attribute — set on the line bg, all text inherits it)
    let ui = &Ui {
        base: Style { dim: !focused, ..ui.base },
        linenr: Style { dim: !focused, ..ui.linenr },
        ..ui.clone()
    };
    let text = &doc.text;
    let len = text.len_bytes();
    let tab = editor.config.tab_width;
    let sel = doc.selection();
    let primary = cursor_idx(editor.mode, sel.primary(), text);
    let cur_line = mv::line_of(text, primary);
    // The phantom line (after the final newline) shows only when the cursor is there.
    let max_line = mv::last_line(text).max(cur_line);
    // Screen row → doc line (not 1:1: folded doc-comment blocks). Per-line tables index by "Nth row from top"
    let span = view.iter().map(|v| v.last_line()).max().map_or(1, |m| m + 1 - top.min(m)).max(1);
    let paint_primary = focused && editor.mode != Mode::Insert && cursor_shape(editor) == CursorShape::Block;
    // Syntax highlight: capture name → theme style once per frame (hundreds of lookups, µs).
    let syntax = doc.syntax.as_ref().filter(|s| s.tree.is_some());
    let capture_styles = editor.theme.capture_styles();
    let mut qcursor = QueryCursor::new();
    let mut cursor = None;
    // Only diagnostics on visible lines (so cells don't scan all) + the most severe per line (sign column)
    let last_row_line = (top + span).min(max_line + 1);
    let (vis_from, vis_to) =
        (mv::line_start(text, top.min(max_line)), mv::line_full_end(text, last_row_line.max(1) - 1));
    // The ranges on screen — sorted and apart, so both their ends climb and a cell finds its range and cursor
    // by halving (with 20k cursors, looking through all of them per cell took a frame 20 ms)
    let all = sel.ranges();
    let lo = all.partition_point(|r| r.to() < vis_from);
    let ranges = &all[lo..lo + all[lo..].partition_point(|r| r.from() <= vis_to)];
    // Cursor position once per frame (per cell, grapheme math would run cells × ranges times).
    let cursors: Vec<usize> = ranges.iter().map(|&r| cursor_idx(editor.mode, r, text)).collect();
    // The primary's index among them (past the end when it's off screen)
    let primary_idx = sel.primary_index().wrapping_sub(lo);
    let visible_diags: Vec<&crate::lsp::Diagnostic> =
        doc.lsp.diagnostics.iter().filter(|d| d.to >= vis_from && d.from <= vis_to).collect();
    let mut line_severity = vec![0u8; span];
    // End-of-line message (Error Lens): 1st line of the most severe diagnostic starting on that line + N more
    // + whether that line is all of its message
    let mut line_message: Vec<Option<(u8, &str, usize, bool)>> = vec![None; span];
    if editor.config.inline_diagnostics {
        for d in &visible_diags {
            let Some(slot) = mv::line_of(text, d.from).checked_sub(top).and_then(|r| line_message.get_mut(r))
            else {
                continue;
            };
            let msg = d.message.lines().next().unwrap_or_default();
            let whole =
                d.message.lines().filter(|l| !l.trim().is_empty() && !lint_origin(l)).nth(1).is_none();
            *slot = match *slot {
                None => Some((d.severity, msg, 0, whole)),
                Some((s, _, n, _)) if d.severity < s => Some((d.severity, msg, n + 1, whole)),
                Some((s, m, n, w)) => Some((s, m, n + 1, w)),
            };
        }
    }
    // Claude's notes: the line end of each note's first line (ahead of diagnostics — they're rarer and meant)
    // (the note the card shows — the focused one, else the first — and how many start on that line)
    let mut note_at: Vec<Option<(&crate::notes::Note, usize)>> = vec![None; span];
    if let Some(p) = doc.path.as_deref() {
        for n in editor.notes.list.iter().filter(|n| n.path == p) {
            if let Some(slot) = n.lines.start.checked_sub(top).and_then(|r| note_at.get_mut(r)) {
                *slot = Some(match *slot {
                    None => (n, 1),
                    Some((m, k)) => (if Some(n.id) == editor.notes.focus { n } else { m }, k + 1),
                });
            }
        }
    }
    // A plugin's hints (a cluster's state beside a manifest) — where no note or diagnostic speaks
    let mut hint_at: Vec<Option<(&str, crate::plugin::PluginTone)>> = vec![None; span];
    for (line, text, tone) in editor.plugin_hints_for(doc) {
        if let Some(slot) = line.checked_sub(top).and_then(|r| hint_at.get_mut(r)) {
            slot.get_or_insert((text, tone));
        }
    }
    // The list's places (a build's errors) where no diagnostic speaks already — they follow edits by mark
    if let (Some(list), Some(p)) = (&editor.qlist, doc.path.as_deref()) {
        let marks: std::collections::HashMap<u64, usize> = if list.items.iter().any(|it| it.mark.is_some()) {
            doc.mark_heads().collect()
        } else {
            Default::default()
        };
        for it in list.items.iter().filter(|it| it.severity > 0) {
            let line = match it.mark {
                Some((id, m)) if id == doc.id => match marks.get(&m) {
                    Some(&head) => {
                        mv::line_of(text, crate::graphemes::snap(text, head.min(text.len_bytes())))
                    }
                    None => continue,
                },
                Some(_) => continue,
                None if it.path == p => it.line,
                None => continue,
            };
            if let Some(slot @ None) = line.checked_sub(top).and_then(|r| line_message.get_mut(r)) {
                let msg = it.message.lines().next().unwrap_or_default();
                *slot = Some((it.severity, msg, 0, it.message.lines().nth(1).is_none()));
            }
        }
    }
    // Failed test sites: same ▲ as the results panel + first message line (ahead of diagnostics — just run)
    let mut test_fail = vec![false; span];
    if let (Some(r), Some(p)) = (&editor.test_run, doc.path.as_deref()) {
        for c in r.cases.iter().filter(|c| c.status == crate::test_results::Status::Failed) {
            let Some((fp, l)) = &c.at else { continue };
            if fp.as_path() != p {
                continue;
            }
            if let Some(row) = l.checked_sub(top).filter(|&row| row < span) {
                let msg = c.message.iter().map(|m| m.trim()).find(|m| !m.is_empty()).unwrap_or("test failed");
                line_message[row] = Some((1, msg, 0, false));
                test_fail[row] = true;
            }
        }
    }
    for d in &visible_diags {
        let row = mv::line_of(text, d.from).wrapping_sub(top);
        if let Some(s) = line_severity.get_mut(row)
            && (*s == 0 || d.severity < *s)
        {
            *s = d.severity;
        }
    }
    // Query the whole visible area once (instead of 50 times per line). If the area is abnormally large
    // (very long lines), split per line — then the loop below queries each line chunk separately.
    let whole: Option<Vec<(usize, usize, u32)>> = syntax
        .filter(|_| vis_to - vis_from <= 1 << 20)
        .map(|syn| syntax::highlights(syn, text, vis_from..vis_to, &mut qcursor));
    // …painted once for the frame (rows take their slice), not every span looked at for every row
    let paint_all = whole.as_deref().map_or_else(Vec::new, |w| paint_spans(w, vis_from..vis_to));

    // Inlay hints: visible range only, off while h-scrolled (so virtual text and scroll column don't drift)
    let inlay_style = ui.base.patch(
        editor.theme.try_get("ui.virtual.inlay-hint").unwrap_or(Style { fg: ui.virt.fg, ..Style::default() }),
    );
    let wrapping = editor.wraps(doc);
    let hints: &[crate::lsp::InlayHint] = if editor.config.inlay_hints && left == 0 {
        let h = &doc.lsp.inlay;
        &h[h.partition_point(|x| x.pos < vis_from)..h.partition_point(|x| x.pos <= vis_to)]
    } else {
        &[]
    };
    // Debugger: this doc's breakpoints · stopped line · variable values to append at line end
    let doc_bps = doc.path.as_ref().and_then(|p| editor.breakpoints.get(p));
    let stop_color = editor.theme.try_get("warning").and_then(|s| s.fg).or(ui.accent.fg);
    let stop_line = editor
        .dap
        .as_ref()
        .and_then(|d| d.stop_at())
        .filter(|(p, _)| doc.path.as_deref() == Some(*p))
        .map(|(_, l)| l);
    let inline_vals = match (stop_line, editor.dap.as_ref()) {
        (Some(l), Some(d)) => crate::dap::inline_values(text, l, &d.vars),
        _ => Default::default(),
    };
    // Search matches (visible span only) — faint accent background, the currently selected match stronger
    let accent_bg = |t: f32| blend(ui.accent.fg, ui.tint, t);
    let hit_style = Style { bg: accent_bg(0.22), ..Style::default() };
    let hit_current = Style { bg: accent_bg(0.5), bold: true, ..Style::default() };
    let visible_hits: &[(usize, usize)] = match &editor.search_hits {
        Some(h) if hit_style.bg.is_some() => {
            let m = &h.matches;
            &m[m.partition_point(|x| x.1 <= vis_from)..m.partition_point(|x| x.0 <= vis_to)]
        }
        _ => &[],
    };
    let primary_range = sel.primary();
    // `gw` labels: label char per position (after the first key, only the matching labels' second char);
    // the rest of the text steps back so they stand out
    let mut label_cells: std::collections::HashMap<usize, char> = std::collections::HashMap::new();
    let labels_on = match editor.jump_labels.as_ref().filter(|l| focused && l.doc == doc.id) {
        Some(l) => {
            for (i, &(s, _)) in l.targets.iter().enumerate() {
                let (a, b) = crate::labels::Labels::label(i);
                match l.typed {
                    None => {
                        label_cells.insert(s, a);
                        label_cells.insert(graphemes::next_boundary(text, s), b);
                    }
                    Some(t) if t == a => {
                        label_cells.insert(s, b);
                    }
                    Some(_) => {}
                }
            }
            true
        }
        None => false,
    };
    let label_style =
        Style { fg: ui.accent.fg, bg: blend(ui.accent.fg, ui.tint, 0.16), bold: true, ..Style::default() };
    // Lines Claude is reading right now (chat) — a faint accent wash
    let gaze = editor
        .chat
        .as_ref()
        .and_then(|c| c.follow.gaze.as_ref())
        .filter(|g| g.path.is_some() && g.path == doc.path)
        .and_then(|g| g.lines.clone());
    let gaze_bg = blend(ui.accent.fg, ui.tint, 0.09).or(ui.base.bg);
    // Code review: added and changed lines on a green wash; removed ones are rows of their own (red)
    let reviewing = doc.review_base.is_some();
    let added_bg = blend(ui.diff_plus.fg, ui.tint, 0.10).or(ui.base.bg);
    let removed_bg = blend(ui.diff_minus.fg, ui.tint, 0.14).or(ui.base.bg);
    let added = |line: usize| {
        reviewing
            && doc
                .git_hunks
                .iter()
                .any(|h| h.kind != crate::git::HunkKind::Deleted && h.lines.contains(&line))
    };
    let code_lang = doc.syntax.as_ref().map(|s| s.lang.name.clone());
    // Merge conflicts: ours on the green wash, theirs on the changed one's, the base and markers fainter
    let delta_fg = editor.theme.try_get("diff.delta").and_then(|s| s.fg);
    let conflict_bg = |line: usize| -> Option<Style> {
        use crate::conflict::Part;
        let part = doc.conflicts.iter().find_map(|c| c.part(line))?;
        let (color, k) = match part {
            Part::Ours => (ui.diff_plus.fg, 0.12),
            Part::Theirs => (delta_fg, 0.14),
            Part::Base => (ui.linenr.fg, 0.08),
            Part::Marker => (ui.linenr.fg, 0.18),
        };
        Some(Style { bg: blend(color, ui.tint, k).or(ui.base.bg), ..ui.base })
    };
    // git: sign per visible line (glyph, color)
    let git_color = |k: &str| Style { fg: editor.theme.try_get(k).and_then(|s| s.fg), ..ui.base };
    let mut git_marks: Vec<Option<(&str, Style)>> = vec![None; span];
    for h in &doc.git_hunks {
        let (glyph, st, lines) = match h.kind {
            crate::git::HunkKind::Added => ("▎", git_color("diff.plus"), h.lines.clone()),
            crate::git::HunkKind::Modified => ("▎", git_color("diff.delta"), h.lines.clone()),
            // Deleted spots get an underline under the preceding line — not in a review (the removed rows
            // are right there)
            crate::git::HunkKind::Deleted if reviewing => continue,
            crate::git::HunkKind::Deleted => {
                let l = h.lines.start.saturating_sub(1);
                ("▁", git_color("diff.minus"), l..l + 1)
            }
        };
        for l in lines {
            if let Some(m) = l.checked_sub(top).and_then(|r| git_marks.get_mut(r)) {
                *m = Some((glyph, st));
            }
        }
    }
    // Indent guides: per line, the depth to draw guides to (blank lines take the shallower of the nearest
    // lines above/below — so guides don't break inside a block)
    let unit = tab.max(1);
    let guide_style = editor
        .theme
        .try_get("ui.virtual.indent-guide")
        .map(|s| Style { bg: None, ..s })
        .unwrap_or(Style { fg: ui.linenr.fg, ..Style::default() });
    let depths: Vec<usize> = if editor.config.indent_guides && left == 0 {
        let first = top.saturating_sub(40);
        let last = (top + span + 40).min(max_line);
        let raw: Vec<Option<usize>> = (first..=last).map(|l| line_indent(text, l, tab)).collect();
        let mut up = vec![0usize; raw.len()];
        let mut down = vec![0usize; raw.len()];
        let mut last_seen = 0;
        for (i, r) in raw.iter().enumerate() {
            if let Some(w) = r {
                last_seen = *w;
            }
            up[i] = last_seen;
        }
        last_seen = 0;
        for (i, r) in raw.iter().enumerate().rev() {
            if let Some(w) = r {
                last_seen = *w;
            }
            down[i] = last_seen;
        }
        (0..span)
            .map(|row| {
                let i = top + row - first;
                match raw.get(i) {
                    Some(Some(w)) => *w,
                    Some(None) => up[i].min(down[i]),
                    None => 0,
                }
            })
            .collect()
    } else {
        vec![0; span]
    };
    // Guides of the block holding the cursor are a step brighter (that column, that line range)
    let active_guide: Option<(usize, std::ops::Range<usize>)> = cur_line.checked_sub(top).and_then(|r| {
        let d = *depths.get(r)?;
        let next_deeper = (cur_line < max_line)
            .then(|| line_indent(text, cur_line + 1, tab))
            .flatten()
            .is_some_and(|n| n > d);
        let col = if next_deeper { d } else { d.checked_sub(unit)? };
        if col == 0 {
            return None;
        }
        let mut a = r;
        while a > 0 && depths[a - 1] > col {
            a -= 1;
        }
        let mut b = r + 1;
        while b < depths.len() && depths[b] > col {
            b += 1;
        }
        if next_deeper {
            a = r + 1; // the head line itself has no guide
        }
        Some((col, a..b))
    });
    let active_guide_style = Style { fg: ui.virt.fg, ..Style::default() };
    let signs = lay.gutter - num_w(doc);
    for row in 0..lay.text_rows {
        let y = lay.text_top + row as u16;
        queue!(out, MoveTo(lay.x0 as u16, y))?;
        let line = match view.get(row) {
            None => {
                clear_rest(out, ui.base)?;
                continue;
            }
            // Folded doc comment: line number only on the block's first row; git sign if any in the block
            Some(crate::doccomment::ViewRow::Doc { start, end, indent, index, spans, code }) => {
                apply(out, ui.base)?;
                queue!(out, Spaces(signs))?;
                // Cursor inside a folded block (normal mode): current-line number color, accent bar
                let here = (*start..=*end).contains(&cur_line);
                apply(out, if focused && here { ui.linenr_selected } else { ui.linenr })?;
                let w = lay.gutter - 1 - signs;
                if *index == 0 {
                    let num = match editor.config.line_number {
                        LineNumber::Relative if !here => start.abs_diff(cur_line),
                        _ => start + 1,
                    };
                    queue!(out, Print(format!("{num:>w$}")))?;
                } else {
                    queue!(out, Spaces(w))?;
                }
                let touched = *index == 0
                    && doc.git_hunks.iter().any(|h| h.lines.start <= *end && h.lines.end > *start);
                if touched {
                    apply(out, git_color("diff.delta"))?;
                    queue!(out, Print("▎"))?;
                } else {
                    apply(out, ui.base)?;
                    queue!(out, Print(" "))?;
                }
                draw_folded_row(out, editor, ui, lay, *indent, spans, *code, focused && here, labels_on)?;
                continue;
            }
            // A line the review's base had here: `−` where the number goes (the old numbers would read as
            // the new ones — or clash with relative numbers), its text in the language's colors
            Some(crate::doccomment::ViewRow::Removed { text: gone, .. }) => {
                let base = Style { bg: removed_bg, ..ui.base };
                apply(out, base)?;
                queue!(out, Spaces(lay.gutter - 1))?;
                apply(out, Style { fg: ui.diff_minus.fg, bold: true, ..base })?;
                queue!(out, Print("−"))?;
                let gone = gone.replace('\t', &" ".repeat(tab));
                let (mut skip, mut room) = (left, lay.text_cols);
                for l in crate::markdown::code_lines(&gone, code_lang.as_deref(), &editor.theme) {
                    let crate::markdown::Line::Code(spans) = l else { continue };
                    for sp in spans {
                        let mut piece = String::new();
                        for ch in sp.text.chars() {
                            let cw = ch.width().unwrap_or(0);
                            if skip > 0 {
                                skip = skip.saturating_sub(cw);
                                continue;
                            }
                            if cw > room {
                                room = 0;
                                break;
                            }
                            room -= cw;
                            piece.push(ch);
                        }
                        apply(out, base.patch(Style { bg: None, ..sp.style }))?;
                        queue!(out, Print(&piece))?;
                    }
                }
                clear_rest(out, base)?;
                continue;
            }
            Some(crate::doccomment::ViewRow::Line(l)) => (*l, None),
            Some(crate::doccomment::ViewRow::Part { line, row, index, last }) => {
                (*line, Some((*row, *index, *last)))
            }
        };
        let (line, part) = line;
        // This row's slice of the line: display columns [row_left, row_end), drawn from screen x `x0`
        let (row_left, row_end, x0, first, last) = match part {
            Some((r, index, last)) => (r.col, r.end_col, r.x0, index == 0, last),
            None => (left, usize::MAX, 0, true, true),
        };
        let li = line - top;
        let depth = depths.get(li).copied().unwrap_or(0);
        let guide_at = |col: usize| {
            let active = active_guide.as_ref().is_some_and(|(c, rows)| *c == col && rows.contains(&li));
            if active { active_guide_style } else { guide_style }
        };
        if line > max_line {
            clear_rest(out, ui.base)?;
            continue;
        }
        let num = match editor.config.line_number {
            LineNumber::Relative if line != cur_line => line.abs_diff(cur_line),
            _ => line + 1,
        };
        if !first {
            // Continuation row of a wrapped line: no sign or number
            apply(out, ui.base)?;
            queue!(out, Spaces(lay.gutter - 1))?;
        } else if signs > 0 {
            // Sign column: stopped line ▶ > breakpoint ● > diagnostic bar
            let sev = line_severity.get(line.wrapping_sub(top)).copied().unwrap_or(0);
            if stop_line == Some(line) {
                apply(out, Style { fg: stop_color, bold: true, ..ui.base })?;
                queue!(out, Print("▶"))?;
            } else if let Some(bp) = doc_bps.and_then(|b| b.get(&line)) {
                // Breakpoint = red · condition = orange · logpoint = accent (the line-end text says which)
                let fg = if bp.log.is_some() {
                    ui.accent.fg
                } else if bp.condition.is_some() {
                    stop_color
                } else {
                    ui.error.fg
                };
                apply(out, Style { fg, ..ui.base })?;
                queue!(out, Print("●"))?;
            } else {
                apply(out, if sev > 0 { ui.base.patch(ui.sign[sev as usize]) } else { ui.base })?;
                queue!(out, Print(if sev > 0 { "▎" } else { " " }))?;
            }
        }
        if first {
            apply(out, if focused && line == cur_line { ui.linenr_selected } else { ui.linenr })?;
            queue!(out, Print(format!("{num:>w$}", w = lay.gutter - 1 - signs)))?;
        }
        // Between line number and text: git bar (added green · modified yellow · deleted red underline) —
        // the bar runs down a wrapped line's rows, the underline goes under its last
        match git_marks.get(li).copied().flatten().filter(|(g, _)| last || *g != "▁") {
            Some((glyph, st)) => {
                apply(out, st)?;
                queue!(out, Print(glyph))?;
            }
            None => queue!(out, Print(" "))?,
        }

        // Current-line highlight: this line's background = ui.cursorline.primary over the default
        let line_message_here = line_message.get(li).copied().flatten();
        let line_base = if stop_line == Some(line) {
            // Stopped line: accent band (takes precedence over the cursor line)
            Style { bg: blend(stop_color, ui.tint, 0.18).or(ui.base.bg), ..ui.base }
        } else if focused && editor.config.cursorline && line == cur_line {
            ui.cursorline
        } else if let Some(st) = conflict_bg(line) {
            st
        } else {
            // Error/warning lines get a very faint wash of that color — so problems stand out when skimming
            match line_message_here {
                Some((sev @ 1..=2, ..)) => {
                    Style { bg: blend(ui.sign[sev as usize].fg, ui.tint, 0.07).or(ui.base.bg), ..ui.base }
                }
                _ if gaze.as_ref().is_some_and(|g| g.contains(&line)) => Style { bg: gaze_bg, ..ui.base },
                _ if added(line) => Style { bg: added_bg, ..ui.base },
                _ => ui.base,
            }
        };
        // A wrapped line's row is read from its own start (a long paragraph isn't re-read from its head for
        // each of its rows); the rest from the line's
        let (start, col0) = match part {
            Some((r, ..)) => (r.start, r.col),
            None => (mv::line_start(text, line), 0),
        };
        // Fetch only visible width + slack — so even a one-line giant file isn't copied whole every frame.
        let (sc, ec) = (text.byte_to_char(start), text.byte_to_char(mv::line_full_end(text, line)));
        let end = text.char_to_byte(ec.min(sc + (row_left - col0.min(row_left)) + lay.text_cols + 256));
        let s = text.byte_slice(start..end).to_string();
        // Scrolled far right: start at the view's edge, not the line's head (a 5 MB line was read cell by cell
        // up to the edge every frame) — unless a hint before it shifts what's shown
        let (start, col0, s) = match graphemes::seek(&s, col0, tab, row_left) {
            (b, c)
                if b > 0
                    && part.is_none()
                    && !hints.iter().any(|h| h.pos >= start && h.pos < start + b)
                    && !tests_skip_seek() =>
            {
                (start + b, c, s[b..].to_string())
            }
            _ => (start, col0, s),
        };
        // Per-byte captures for this line chunk — the frame's, else queried for this chunk alone
        let per_line;
        let paint: &[u32] = match syntax {
            None => &[],
            Some(syn) => match paint_all.get(start.wrapping_sub(vis_from)..end.wrapping_sub(vis_from)) {
                Some(p) if whole.is_some() => p,
                _ => {
                    per_line =
                        paint_spans(&syntax::highlights(syn, text, start..end, &mut qcursor), start..end);
                    &per_line
                }
            },
        };
        let mut col = 0;
        let mut current: Option<Style> = None;
        // This line's hints (virtual text) — inserted before the char there; later chars shift by `shift`
        let line_end = mv::line_end(text, line);
        let mut line_hints = hints.iter().filter(|h| h.pos >= start && h.pos <= line_end).peekable();
        let mut shift = 0;
        let mut clipped = false;
        // Wrapped: hints only where the whole line plus them fits one row (they'd push code off it)
        let hint_room = match (wrapping, part) {
            (false, _) => usize::MAX,
            (true, Some(_)) => 0,
            (true, None) if line_hints.peek().is_some() => {
                lay.text_cols.saturating_sub(mv::visual_col(text, line_end, tab))
            }
            (true, None) => 0, // no hints to place
        };
        // Continuation row: its indent (with the line's guides)
        if x0 > 0 {
            for k in 0..x0 {
                let guide = k > 0 && k < depth && k % unit == 0;
                apply(out, if guide { line_base.patch(guide_at(k)) } else { line_base })?;
                queue!(out, Print(if guide { "│" } else { " " }))?;
            }
            current = None;
        }
        for c in graphemes::cells_from(&s, start, col0, tab) {
            let g = &s[c.bytes.clone()];
            if c.col >= row_end {
                clipped = true; // the rest is the next row's
                break;
            }
            col = c.col + c.width;
            if c.col < row_left {
                // A wide char/tab cut by the scroll edge: blank its visible part so what follows lines up
                if col > row_left {
                    apply(out, line_base)?;
                    current = Some(line_base);
                    queue!(out, Spaces((col - row_left).min(lay.text_cols)))?;
                }
                continue;
            }
            if c.width == 0 && g == "\r" {
                continue;
            }
            while let Some(h) = line_hints.next_if(|h| h.pos <= c.pos) {
                // Wrapped: a hint that doesn't fit whole stays out (half a type name misleads)
                if h.text.width() > hint_room.saturating_sub(shift) {
                    continue;
                }
                let room = lay.text_cols.saturating_sub(c.col - row_left + shift);
                let t = fit(&h.text, room);
                apply(out, line_base.patch(Style { bg: None, ..inlay_style }))?;
                current = None;
                queue!(out, Print(&t))?;
                shift += t.width();
            }
            let x = x0 + c.col - row_left + shift;
            if x + c.width > lay.text_cols {
                clipped = true;
                break;
            }
            if (c.pos..c.pos + c.len).contains(&primary) {
                cursor = Some(((lay.x0 + lay.gutter + x) as u16, y));
            }
            // Style = default (cursorline if current line) → syntax → selection → cursor
            let mut st = line_base;
            if let Some(&cap) = paint.get(c.bytes.start)
                && let Some(Some(hs)) = capture_styles.get(cap as usize)
            {
                st = st.patch(*hs);
            }
            let span = c.pos..c.pos + c.len.max(1);
            let k = cursors.partition_point(|&p| p < span.start);
            let hit = cursors.get(k).filter(|&&p| p < span.end).map(|_| k);
            let k = ranges.partition_point(|r| r.to() <= c.pos);
            let selected = ranges.get(k).filter(|r| r.from() <= c.pos).map(|_| k);
            // Underline diagnostic ranges (the most severe)
            if let Some(d) = visible_diags.iter().filter(|d| underlines(d, c.pos)).min_by_key(|d| d.severity)
            {
                st = st.patch(ui.diag[d.severity as usize]);
            }
            if !visible_hits.is_empty() {
                let k = visible_hits.partition_point(|m| m.1 <= c.pos);
                if let Some(&(a, b)) = visible_hits.get(k)
                    && a <= c.pos
                {
                    let current = (a, b) == (primary_range.from(), primary_range.to());
                    st = st.patch(if current { hit_current } else { hit_style });
                }
            }
            if let Some(i) = selected {
                st = st.patch(if i == primary_idx { ui.selection_primary } else { ui.selection });
            }
            match hit {
                Some(i) if i == primary_idx && paint_primary => st = st.patch(ui.cursor_primary),
                Some(i) if i != primary_idx => st = st.patch(ui.cursor),
                _ => {}
            }
            let eol = g == "\n" || g == "\r\n";
            if eol && selected.is_none() && hit.is_none() {
                col = c.col;
                continue;
            }
            // A space inside indentation at a guide position becomes │ (only without selection/cursor)
            if g == " "
                && c.col > 0
                && c.col < depth
                && c.col % unit == 0
                && selected.is_none()
                && hit.is_none()
            {
                let gs = line_base.patch(guide_at(c.col));
                apply(out, gs)?;
                current = Some(gs);
                queue!(out, Print("│"))?;
                continue;
            }
            if labels_on {
                if let Some(&ch) = label_cells.get(&c.pos) {
                    apply(out, line_base.patch(label_style))?;
                    current = None;
                    queue!(out, Print(ch), Spaces(c.width.saturating_sub(1)))?;
                    continue;
                }
                st = Style { fg: ui.virt.fg, ..st };
            }
            if current != Some(st) {
                apply(out, st)?;
                current = Some(st);
            }
            if eol || g == "\t" {
                queue!(out, Spaces(c.width))?;
            } else if g.chars().next().is_some_and(char::is_control) {
                queue!(out, Print('\u{fffd}'))?;
            } else {
                out.write_all(g.as_bytes())?;
            }
            if eol {
                col = c.col;
            }
        }
        // Guides on blank lines — no chars there, so fill spaces up to that spot
        if col == 0 && depth > unit && s.trim().is_empty() {
            let mut at = usize::from(line == cur_line && hit_eol(&cursors, ranges, start, &s));
            for k in (unit..depth).step_by(unit) {
                if k < at || k >= lay.text_cols {
                    continue;
                }
                apply(out, line_base)?;
                queue!(out, Spaces(k - at))?;
                apply(out, line_base.patch(guide_at(k)))?;
                queue!(out, Print("│"))?;
                at = k + 1;
            }
        }
        // Hints left at line end (a line without a newline, like the last one)
        if !clipped {
            for h in line_hints {
                if h.text.width() > hint_room.saturating_sub(shift) {
                    continue;
                }
                let room = lay.text_cols.saturating_sub(x0 + col.saturating_sub(row_left) + shift);
                let t = fit(&h.text, room);
                apply(out, line_base.patch(Style { bg: None, ..inlay_style }))?;
                queue!(out, Print(&t))?;
                shift += t.width();
            }
        }
        // Cursor at end of doc (EOF) — there's no corresponding char.
        if primary == len && line == cur_line && cursor.is_none() && col >= row_left && last {
            let x = x0 + col - row_left + shift;
            if x < lay.text_cols {
                cursor = Some(((lay.x0 + lay.gutter + x) as u16, y));
            }
        }
        // Stopped: line-end values of vars used there (`count = 12  total = 15`) — names dim, values by shape
        let mut extra_used = 0;
        if let Some(vals) = inline_vals.get(&line)
            && !clipped
        {
            let used = x0 + col.saturating_sub(row_left) + shift + 1;
            let mut room = lay.text_cols.saturating_sub(used + 3);
            if room > 6 {
                apply(out, line_base)?;
                queue!(out, Print("   "))?;
                extra_used += 3;
                for (k, (name, value)) in vals.iter().enumerate() {
                    let sep = if k > 0 { "  " } else { "" };
                    let value = fit_ellipsis(value, 32);
                    let w = sep.width() + name.width() + 3 + value.width();
                    if w > room {
                        break;
                    }
                    apply(out, Style { fg: ui.linenr.fg, italic: true, ..line_base })?;
                    queue!(out, Print(sep), Print(name), Print(" = "))?;
                    let vc = editor
                        .theme
                        .try_get(crate::dap::value_scope(&value))
                        .and_then(|s| s.fg)
                        .or(ui.base.fg);
                    apply(out, Style { fg: vc, italic: true, ..line_base })?;
                    queue!(out, Print(&value))?;
                    room -= w;
                    extra_used += w;
                }
            }
        }
        // Condition/logpoint at line end: `● when n > 3` / `● log step {i}` — reason in red if rejected
        if let Some(bp) = doc_bps
            .and_then(|b| b.get(&line))
            .filter(|b| b.condition.is_some() || b.log.is_some() || b.rejected.is_some())
            && !clipped
        {
            let used = x0 + col.saturating_sub(row_left) + shift + 1 + extra_used;
            let mut room = lay.text_cols.saturating_sub(used + 3);
            if room >= 10 {
                let mut parts: Vec<(&str, String)> = Vec::new();
                if let Some(c) = &bp.condition {
                    parts.push(("when ", c.clone()));
                }
                if let Some(m) = &bp.log {
                    parts.push(("log ", m.clone()));
                }
                apply(out, line_base)?;
                queue!(out, Print("   "))?;
                extra_used += 3;
                let glyph_fg = if bp.log.is_some() { ui.accent.fg } else { stop_color };
                apply(out, Style { fg: glyph_fg, ..line_base })?;
                queue!(out, Print("● "))?;
                (extra_used, room) = (extra_used + 2, room - 2);
                for (k, (label, body)) in parts.iter().enumerate() {
                    let sep = if k > 0 { "  " } else { "" };
                    let body = fit_ellipsis(body, room.saturating_sub(sep.len() + label.len()).min(48));
                    apply(out, Style { fg: ui.linenr.fg, italic: true, ..line_base })?;
                    queue!(out, Print(sep), Print(label))?;
                    apply(out, Style { fg: ui.virt.fg, italic: true, ..line_base })?;
                    queue!(out, Print(&body))?;
                    let w = sep.len() + label.len() + body.width();
                    (extra_used, room) = (extra_used + w, room.saturating_sub(w));
                }
                if let Some(why) = &bp.rejected {
                    let why = fit_ellipsis(why, room.saturating_sub(2));
                    apply(out, Style { fg: ui.error.fg, italic: true, ..line_base })?;
                    queue!(out, Print("  "), Print(&why))?;
                    extra_used += 2 + why.width();
                }
            }
        }
        // Line-end note: `  ¶ first words +2` — accent glyph, the text a tone lower, replies faint
        let note_here = note_at.get(li).copied().flatten();
        if let Some((n, count)) = note_here
            && last
            && !clipped
        {
            let used = x0 + col.saturating_sub(row_left) + shift + 1 + extra_used;
            let room = lay.text_cols.saturating_sub(used + 3);
            if room >= 8 {
                let tail = match (n.waiting, n.thread.len()) {
                    (true, _) => "  · replying…".to_string(),
                    (false, 1) => String::new(),
                    (false, k) => format!("  +{}", k - 1),
                };
                // Several on the line: how many, then the one the card shows; a draft says so
                let draft = if n.thread.iter().any(|e| e.draft) { "draft · " } else { "" };
                let gist = if count > 1 {
                    format!("{count} notes · {draft}{}", n.gist())
                } else {
                    format!("{draft}{}", n.gist())
                };
                let body = fit_ellipsis(&gist, room.saturating_sub(2 + tail.width()));
                apply(out, line_base)?;
                queue!(out, Print("   "))?;
                apply(out, Style { fg: ui.accent.fg, ..line_base })?;
                queue!(out, Print("¶ "))?;
                let fg = blend(ui.accent.fg, line_base.bg.or(ui.tint), 0.62).or(ui.virt.fg);
                apply(out, Style { fg, italic: true, ..line_base })?;
                queue!(out, Print(&body))?;
                apply(out, Style { fg: ui.linenr.fg, ..line_base })?;
                queue!(out, Print(&tail))?;
            }
        }
        // Line-end diagnostic: `  ● message +2` — symbol in severity color, text a tone lower. …if no room
        if let Some((sev, msg, more, whole)) = line_message_here
            && !clipped
            && note_here.is_none()
        {
            let used = x0 + col.saturating_sub(row_left) + shift + 1 + extra_used;
            let room = lay.text_cols.saturating_sub(used + 3);
            if room >= 8 {
                let color = ui.sign[sev as usize].fg;
                let glyph = match sev {
                    _ if test_fail.get(li).copied().unwrap_or(false) => "▲",
                    1 => "●",
                    2 => "▲",
                    3 => "●",
                    _ => "·",
                };
                let more = if more > 0 { format!("  +{more}") } else { String::new() };
                let body = fit_ellipsis(msg, room.saturating_sub(2 + more.width()));
                if focused && line == cur_line && whole && more.is_empty() && body == msg {
                    cursor_diags_shown.set(true);
                }
                apply(out, line_base)?;
                queue!(out, Print("   "))?;
                apply(out, Style { fg: color, ..line_base })?;
                queue!(out, Print(glyph), Print(" "))?;
                apply(
                    out,
                    Style {
                        fg: blend(color, line_base.bg.or(ui.tint), 0.72).or(color),
                        italic: true,
                        ..line_base
                    },
                )?;
                queue!(out, Print(&body))?;
                apply(out, Style { fg: ui.linenr.fg, ..line_base })?;
                queue!(out, Print(&more))?;
            }
        }
        // A plugin's hint: `  ● web · 2/3 ready` — the glyph in its tone's color, the text a tone lower
        let hint_here =
            hint_at.get(li).copied().flatten().filter(|_| line_message_here.is_none() && note_here.is_none());
        if let Some((text, tone)) = hint_here
            && last
            && !clipped
        {
            let used = x0 + col.saturating_sub(row_left) + shift + 1 + extra_used;
            let room = lay.text_cols.saturating_sub(used + 3);
            if room >= 8 {
                use crate::plugin::PluginTone as T;
                let color = match tone {
                    T::Ok => ui.diff_plus.fg,
                    T::Info => ui.virt.fg,
                    T::Warning => ui.sign[2].fg,
                    T::Error => ui.sign[1].fg,
                };
                let body = fit_ellipsis(text, room.saturating_sub(2));
                apply(out, line_base)?;
                queue!(out, Print("   "))?;
                apply(out, Style { fg: color, ..line_base })?;
                queue!(out, Print("● "))?;
                let fg = if tone == T::Info {
                    ui.linenr.fg
                } else {
                    blend(color, line_base.bg.or(ui.tint), 0.72).or(color)
                };
                apply(out, Style { fg, italic: true, ..line_base })?;
                queue!(out, Print(&body))?;
            }
        }
        // Blame (`b` in the git tab): who changed the cursor line, when, and why — faint, only where the line end is
        // free (a diagnostic or debugger values take it first), and not while typing
        if focused
            && editor.mode != Mode::Insert
            && line == cur_line
            && last
            && !clipped
            && line_message_here.is_none()
            && note_here.is_none()
            && hint_here.is_none()
            && extra_used == 0
            && let Some(b) = editor.blame_here(line)
        {
            let used = x0 + col.saturating_sub(row_left) + shift + 1;
            let room = lay.text_cols.saturating_sub(used + 3);
            if room >= 12 {
                apply(out, Style { fg: ui.linenr.fg, italic: true, ..line_base })?;
                queue!(out, Print("   "), Print(fit_ellipsis(&b, room)))?;
            }
        }
        clear_rest(out, line_base)?;
    }
    if lay.scrollbar {
        draw_scrollbar(doc, top, ui, lay, out)?;
    }
    Ok(cursor)
}

/// The review base's removed lines that go above lines `from..=to`: (line they go above, text).
fn removed_rows(doc: &crate::document::Document, from: usize, to: usize) -> Vec<(usize, String)> {
    let Some(base) = &doc.review_base else { return Vec::new() };
    let mut out = Vec::new();
    for h in doc.git_hunks.iter().filter(|h| h.removed > 0 && (from..=to).contains(&h.lines.start)) {
        for b in h.base.clone().filter(|&b| b < base.len_lines()) {
            let text = base.line(b).to_string();
            out.push((h.lines.start, text.trim_end_matches(['\n', '\r']).to_string()));
        }
    }
    out
}

/// How many removed rows a code review puts above `line`.
pub fn removed_above(doc: &crate::document::Document, line: usize) -> usize {
    if doc.review_base.is_none() {
        return 0;
    }
    doc.git_hunks.iter().filter(|h| h.lines.start == line).map(|h| h.removed).sum()
}

/// Screen rows: doc lines in order, but doc-comment blocks without cursor/selection are **folded** (text
/// rewrapped to width, no blank lines/fences) into fewer rows. Once the cursor enters, it unfolds to raw.
/// Detection looks up to 200 lines above/below the screen (so a `/** */` block starting off-screen connects).
fn build_view(
    editor: &Editor,
    doc: &crate::document::Document,
    top: usize,
    left: usize,
    lay: &Layout,
) -> Vec<crate::doccomment::ViewRow> {
    use crate::doccomment::{ViewRow, parts};
    let text = &doc.text;
    let rows = lay.text_rows;
    let last = mv::last_line(text);
    let cur_line = mv::line_of(text, doc.selection().primary().cursor(text));
    let max_line = last.max(cur_line);
    let lang = doc.lang_name();
    // Soft wrap: a line → its rows (the top line taller than the screen shows from the cursor's part)
    let wrap_w = editor.wraps(doc).then_some(lay.text_cols);
    let tab = editor.config.tab_width;
    let push_line = |view: &mut Vec<ViewRow>, line: usize| {
        let Some(w) = wrap_w else { return view.push(ViewRow::Line(line)) };
        let parts = crate::wrap::rows(text, line, w, tab);
        if parts.len() == 1 {
            return view.push(ViewRow::Line(line));
        }
        let skip = if line == top && line == cur_line {
            let k = crate::wrap::row_of(&parts, doc.selection().primary().cursor(text));
            (k + 1).saturating_sub(rows)
        } else {
            0
        };
        let n = parts.len();
        for (index, row) in parts.into_iter().enumerate().skip(skip) {
            if view.len() == rows {
                break;
            }
            view.push(ViewRow::Part { line, row, index, last: index + 1 == n });
        }
    };
    // Code review: the base's removed lines go in above the line they preceded (raw text — no folding)
    if doc.review_base.is_some() {
        let removed = removed_rows(doc, top, top + rows);
        let mut next = removed.iter().peekable();
        let mut view = Vec::with_capacity(rows);
        let mut line = top;
        while view.len() < rows && line <= max_line + 1 {
            while let Some(r) = next.next_if(|r| r.0 <= line) {
                if view.len() < rows {
                    view.push(ViewRow::Removed { before: r.0, text: r.1.clone() });
                }
            }
            if line <= max_line {
                push_line(&mut view, line);
            }
            line += 1;
        }
        return view;
    }
    if !editor.config.render_doc_comments
        || left > 0
        || editor.review.is_some()
        || !crate::doccomment::applies(lang)
    {
        let mut view = Vec::with_capacity(rows);
        let mut line = top;
        while view.len() < rows && line <= max_line {
            push_line(&mut view, line);
            line += 1;
        }
        return view;
    }
    // Find blocks: runs of consecutive doc-comment lines within [w0, w1]
    let (w0, w1) = (top.saturating_sub(200), (top + rows * 3 + 200).min(last));
    let info: Vec<Option<(usize, String)>> = crate::doccomment::scan(text, w0..=w1).collect();
    // Unfold only when editing/selecting: INSERT/SELECT mode, or a selection wider than one char touching it.
    // In normal mode a passing cursor leaves it folded (j/k skip the block like one line — commands.rs)
    let opening = matches!(editor.mode, Mode::Insert | Mode::Select);
    // (only the ranges within the scanned lines — sorted, so found by halving)
    let all = doc.selection().ranges();
    let (b0, b1) = (mv::line_start(text, w0), mv::line_full_end(text, w1));
    let lo = all.partition_point(|r| r.to() < b0);
    let sel_lines: Vec<(usize, usize)> = all[lo..lo + all[lo..].partition_point(|r| r.from() <= b1)]
        .iter()
        .filter(|r| opening || r.to() > crate::graphemes::next_boundary(text, r.from()))
        .map(|r| (text.byte_to_line(r.from()), text.byte_to_line(r.to().max(r.from()))))
        .collect();
    // (start, end, indent, texts) — blocks being edited or selected are left out (raw)
    let mut blocks: Vec<(usize, usize, usize, Vec<String>)> = Vec::new();
    let mut i = 0;
    while i < info.len() {
        if info[i].is_none() {
            i += 1;
            continue;
        }
        let start = i;
        while i < info.len() && info[i].is_some() {
            i += 1;
        }
        let (a, b) = (w0 + start, w0 + i - 1);
        if sel_lines.iter().any(|&(x, y)| x <= b && y >= a) {
            continue;
        }
        let bodies =
            info[start..i].iter().map(|x| x.as_ref().map(|v| v.1.clone()).unwrap_or_default()).collect();
        blocks.push((a, b, info[start].as_ref().map_or(0, |v| v.0), bodies));
    }
    let lang = doc.syntax.as_ref().map(|s| s.lang.name.clone());
    let mut view = Vec::with_capacity(rows);
    let mut line = top;
    while view.len() < rows && line <= max_line {
        match blocks.iter().find(|b| b.0 <= line && line <= b.1) {
            Some((a, b, indent, bodies)) => {
                let width = lay.text_cols.saturating_sub(indent + 3).max(8);
                let folded = fold_rows(editor, &parts(bodies), width, lang.as_deref());
                // If the top of the screen is mid-block, skip that proportion
                let skip = (line - a).min(folded.len().saturating_sub(1));
                for (k, (spans, code)) in folded.into_iter().enumerate().skip(skip) {
                    if view.len() == rows {
                        break;
                    }
                    view.push(ViewRow::Doc { start: *a, end: *b, indent: *indent, index: k, spans, code });
                }
                line = b + 1;
            }
            None => {
                push_line(&mut view, line);
                line += 1;
            }
        }
    }
    view
}

/// Screen rows of a folded doc-comment block: (chunk, is example code line).
fn fold_rows(
    editor: &Editor,
    parts: &[crate::doccomment::Part],
    width: usize,
    lang: Option<&str>,
) -> Vec<(Vec<crate::markdown::Span>, bool)> {
    use crate::doccomment::{Chunk, chunks};
    use crate::markdown::{self as md, Line, Span};
    let t = &editor.theme;
    let accent = accent_color(t);
    let plain = |text: String, style: Style| Span { text, style };
    let mut out = Vec::new();
    let wrap = |spans: Vec<Span>, indent: usize, out: &mut Vec<(Vec<Span>, bool)>| {
        for row in md::wrap(&[Line::Text { spans, indent }], width, Style::default()) {
            out.push((row, false));
        }
    };
    for c in chunks(parts) {
        match c {
            Chunk::Heading(h) => {
                out.push((vec![plain(h, Style { fg: accent, bold: true, ..Style::default() })], false))
            }
            Chunk::Para(p) => wrap(md::inline_spans(&p, t), 0, &mut out),
            Chunk::Item(p) => {
                let mut v = vec![plain("• ".into(), Style { fg: accent, ..Style::default() })];
                v.extend(md::inline_spans(&p, t));
                wrap(v, 2, &mut out);
            }
            Chunk::Tag(name, rest) => {
                let kw =
                    t.try_get("keyword").map(|s| Style { bg: None, bold: true, ..s }).unwrap_or_default();
                let mut v = vec![plain(format!("@{name} "), kw)];
                v.extend(md::inline_spans(&rest, t));
                wrap(v, 2, &mut out);
            }
            // Blank line between paragraphs — the panel continues, only the text is empty
            Chunk::Gap => out.push((Vec::new(), false)),
            Chunk::Code(line) => {
                let spans = match md::code_lines(&line, lang, t).into_iter().next() {
                    Some(Line::Code(s)) => s,
                    _ => Vec::new(),
                };
                out.push((spans, true));
            }
        }
    }
    if out.is_empty() {
        out.push((Vec::new(), false));
    }
    out
}

/// One screen row of a folded doc comment: indent · bar · text on a faint panel. Body a tone lower — reads
/// smaller, quieter than code (terminals can't resize fonts: contrast); example code = a well of editor bg.
#[allow(clippy::too_many_arguments)]
fn draw_folded_row(
    out: &mut impl Write,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    indent: usize,
    spans: &[crate::markdown::Span],
    code: bool,
    current: bool,
    dim: bool,
) -> io::Result<()> {
    let comment_fg = editor.theme.try_get("comment").and_then(|s| s.fg).or(ui.virt.fg);
    let panel_bg = blend(comment_fg, ui.tint, if current { 0.16 } else { 0.09 }).or(ui.base.bg);
    let panel = Style { bg: panel_bg, fg: blend(ui.base.fg, comment_fg, 0.55).or(ui.base.fg), ..ui.base };
    let well = Style { bg: ui.base.bg, ..panel };
    let width = lay.text_cols;
    let indent = indent.min(width.saturating_sub(4));
    apply(out, ui.base)?;
    queue!(out, Spaces(indent))?;
    apply(out, Style { fg: if current { ui.accent.fg.or(comment_fg) } else { comment_fg }, ..panel })?;
    queue!(out, Print("▎ "))?;
    let mut used = indent + 2;
    let base = if code { well } else { panel };
    if code {
        apply(out, well)?;
        queue!(out, Print(" "))?;
        used += 1;
    }
    for sp in spans {
        if used >= width {
            break;
        }
        let text = fit(&sp.text, width - used);
        if text.is_empty() {
            continue;
        }
        let st = base.patch(Style { bg: None, ..sp.style });
        // `gw` labels up: step back like the rest of the text
        apply(out, if dim { Style { fg: ui.virt.fg, ..st } } else { st })?;
        queue!(out, Print(&text))?;
        used += text.width();
    }
    apply(out, if code { well } else { panel })?;
    let pad = if code { (width.saturating_sub(used)).saturating_sub(1) } else { width.saturating_sub(used) };
    queue!(out, Spaces(pad))?;
    if code {
        apply(out, panel)?;
        queue!(out, Print(" "))?;
    }
    clear_rest(out, ui.base)
}

/// Indent width of a line (None if blank).
fn line_indent(text: &ropey::Rope, line: usize, tab: usize) -> Option<usize> {
    let mut w = 0;
    for ch in text.line(line).chars() {
        match ch {
            ' ' => w += 1,
            '\t' => w += tab - w % tab.max(1),
            '\n' | '\r' => return None,
            _ => return Some(w),
        }
    }
    None
}

/// Was the blank line's newline cell already drawn (did a cursor/selection there take a cell)?
fn hit_eol(cursors: &[usize], ranges: &[Range], start: usize, s: &str) -> bool {
    let nl = start + s.trim_end_matches(['\r', '\n']).len();
    !s.is_empty() && (cursors.contains(&nl) || ranges.iter().any(|r| r.from() <= nl && nl < r.to()))
}

/// Right scrollbar: visible span = dim thumb, error/warning lines across the file = ticks in that color.
fn draw_scrollbar(
    doc: &crate::document::Document,
    top: usize,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    let total = doc.text.len_lines().max(1);
    let rows = lay.text_rows.max(1);
    let (a, b) = (top * rows / total, ((top + rows) * rows).div_ceil(total).max(top * rows / total + 1));
    let mut marks = vec![0u8; rows];
    // git changes (only cells without diagnostics): 7 = added/modified, 8 = deleted
    for h in &doc.git_hunks {
        let r = (h.lines.start * rows / total).min(rows - 1);
        let end = ((h.lines.end.max(h.lines.start + 1)) * rows).div_ceil(total).clamp(r + 1, rows);
        for m in &mut marks[r..end] {
            if *m == 0 {
                *m = if h.kind == crate::git::HunkKind::Deleted { 8 } else { 7 };
            }
        }
    }
    for d in &doc.lsp.diagnostics {
        let r = (doc.text.byte_to_line(d.from.min(doc.text.len_bytes())) * rows / total).min(rows - 1);
        if d.severity <= 2 && (marks[r] == 0 || marks[r] > 2 || d.severity < marks[r]) {
            marks[r] = d.severity;
        }
    }
    let x = lay.edit_w.saturating_sub(1) as u16;
    for (row, &mark) in marks.iter().enumerate() {
        queue!(out, MoveTo(x, lay.text_top + row as u16))?;
        let st = match mark {
            0 if (a..b).contains(&row) => Style { fg: ui.linenr.fg, ..ui.base },
            0 => {
                apply(out, ui.base)?;
                queue!(out, Print(" "))?;
                continue;
            }
            7 => Style { fg: ui.diff_plus.fg, ..ui.base },
            8 => Style { fg: ui.diff_minus.fg, ..ui.base },
            s => ui.base.patch(ui.sign[s as usize]),
        };
        apply(out, st)?;
        queue!(out, Print("▐"))?;
    }
    Ok(())
}

/// Blend two colors (truecolor only — None for named colors).
fn blend(
    a: Option<crossterm::style::Color>,
    b: Option<crossterm::style::Color>,
    t: f32,
) -> Option<crossterm::style::Color> {
    use crossterm::style::Color::Rgb;
    match (a?, b?) {
        (Rgb { r, g, b: bb }, Rgb { r: r2, g: g2, b: b2 }) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Some(Rgb { r: m(r2, r), g: m(g2, g), b: m(b2, bb) })
        }
        _ => None,
    }
}

/// Rows of Claude's reason under a review's title.
const REASON_ROWS: usize = 2;

/// Review screen — **inside the buffer** (like Cursor): the whole file as in the editor, changes expanded.
/// Deleted lines on faint red, added on faint green bg, keeping syntax colors. Each change has a head row —
/// the current one an accent band with key hints, decided ones "accepted"/"rejected" (accept leaves only
/// the new text visible, reject only the original). The top row is a fixed title (instruction · # decided).
fn draw_review(
    rev: &Review,
    rows: &[crate::llm::RRow],
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::llm::{Decision, RRow};
    let t = &editor.theme;
    let Some(doc) = editor.docs.iter().find(|d| d.id == rev.doc_id) else { return Ok(()) };
    let text = &doc.text;
    let lang = doc.syntax.as_ref().map(|s| s.lang.name.clone());
    let syntax = doc.syntax.as_ref().filter(|s| s.tree.is_some());
    let capture_styles = t.capture_styles();
    let dim = Style { fg: ui.virt.fg, ..ui.base };
    let faint = Style { fg: ui.linenr.fg, ..ui.base };
    let strong =
        ui.base.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let accent = Style { fg: ui.accent.fg, ..ui.base };
    let (minus_fg, plus_fg) = (ui.diff_minus.fg, ui.diff_plus.fg);
    let minus_bg = blend(minus_fg, ui.tint, 0.16);
    let plus_bg = blend(plus_fg, ui.tint, 0.16);
    // Head band is neutral card color — an accent tint (hanji's vermilion) would look like deleted-line red
    let band = card_style(ui, editor, "ui.popup").bg;
    let width = lay.edit_w;
    let num_w = lay.gutter.saturating_sub(2).max(3);
    let code_w = width.saturating_sub(num_w + 3);
    // Why, under the title: the end of what Claude thought before asking (its conclusion), two rows at most
    let reason: Vec<Vec<crate::markdown::Span>> = if rev.reason.is_empty() {
        Vec::new()
    } else {
        let rows = thought_rows(&rev.reason, width.saturating_sub(7), dim, faint, editor);
        let cut = rows.len() > REASON_ROWS;
        let mut rows = rows[rows.len().saturating_sub(REASON_ROWS)..].to_vec();
        if cut && let Some(first) = rows.first_mut() {
            first.insert(0, crate::markdown::Span { text: "… ".into(), style: faint });
        }
        rows
    };
    let title_h = 1 + reason.len();
    let body = lay.text_rows.saturating_sub(title_h);
    let shown: &[RRow] = &rows[rev.scroll.min(rows.len())..];
    let shown = &shown[..shown.len().min(body)];
    // Syntax highlight for all visible doc lines at once
    let doc_lines: Vec<usize> =
        shown.iter().filter_map(|r| if let RRow::Doc(l) = r { Some(*l) } else { None }).collect();
    let spans = match (syntax, doc_lines.first(), doc_lines.last()) {
        (Some(syn), Some(&a), Some(&b)) => syntax::highlights(
            syn,
            text,
            mv::line_start(text, a)..mv::line_full_end(text, b),
            &mut QueryCursor::new(),
        ),
        _ => Vec::new(),
    };
    // Fixed title
    let n = rev.hunks.len();
    let decided = rev.hunks.iter().filter(|h| h.decision != Decision::Undecided).count();
    queue!(out, MoveTo(0, lay.text_top))?;
    apply(out, accent)?;
    queue!(out, Print(" ▎ "))?;
    apply(out, strong)?;
    queue!(out, Print(rev.by))?;
    let right = format!("{decided} of {n} decided ");
    // Human instructions in quotes, agent proposals as their description as-is
    let said = if rev.by == "Claude" {
        format!("   “{}”", rev.instruction)
    } else {
        format!("   {}", rev.instruction)
    };
    let head = 3 + rev.by.width();
    let instr = fit_ellipsis(&said, width.saturating_sub(head + right.len() + 2));
    apply(out, dim)?;
    queue!(out, Print(&instr), Spaces(width.saturating_sub(head + instr.width() + right.len())))?;
    apply(out, faint)?;
    queue!(out, Print(&right))?;
    for (k, row) in reason.iter().enumerate() {
        queue!(out, MoveTo(0, lay.text_top + 1 + k as u16))?;
        apply(out, ui.base)?;
        queue!(out, Print("   "))?;
        let used = chat_spans(out, row, ui.base, width.saturating_sub(4))?;
        apply(out, ui.base)?;
        queue!(out, Spaces(width.saturating_sub(3 + used)))?;
    }
    for (k, row) in shown.iter().enumerate() {
        let y = lay.text_top + title_h as u16 + k as u16;
        queue!(out, MoveTo(0, y))?;
        let mut used;
        match row {
            RRow::Doc(l) => {
                apply(out, faint)?;
                queue!(out, Print(format!("{:>num_w$}   ", l + 1)))?;
                used = num_w + 3;
                used += draw_code_line(
                    out,
                    text,
                    *l,
                    code_w,
                    editor.config.tab_width,
                    &spans,
                    &capture_styles,
                    ui.base,
                    (0, usize::MAX),
                )?;
            }
            RRow::Header(i) => {
                let h = &rev.hunks[*i];
                let current = *i == rev.current && h.decision == Decision::Undecided && !h.conflict;
                let bg = if current { band.or(ui.base.bg) } else { ui.base.bg };
                let base = Style { bg, ..ui.base };
                let line = text.byte_to_line(h.from.min(text.len_bytes())) + 1;
                apply(out, base)?;
                queue!(out, Spaces(num_w + 1))?;
                apply(
                    out,
                    if current {
                        Style { fg: ui.accent.fg, ..base }
                    } else {
                        Style { fg: ui.linenr.fg, ..base }
                    },
                )?;
                queue!(out, Print("▎ "))?;
                let label = format!("Change {} of {n} · line {line}", i + 1);
                apply(
                    out,
                    if current {
                        Style { bold: true, ..base.patch(Style { fg: ui.accent.fg, ..Style::default() }) }
                    } else {
                        Style { fg: ui.virt.fg, ..base }
                    },
                )?;
                queue!(out, Print(&label))?;
                used = num_w + 3 + label.width();
                let (status, st): (Vec<(&str, &str)>, Style) = match (h.conflict, h.decision) {
                    (true, _) => (vec![("", "text changed while waiting — skipped")], base.patch(ui.error)),
                    (_, Decision::Accept) => (vec![("", "accepted")], Style { fg: plus_fg, ..base }),
                    (_, Decision::Reject) => (vec![("", "rejected")], Style { fg: minus_fg, ..base }),
                    _ if current => {
                        (vec![("y", "accept"), ("n", "reject"), ("a", "all"), ("tab", "next")], base)
                    }
                    _ => (vec![("", "pending")], Style { fg: ui.linenr.fg, ..base }),
                };
                for (key, desc) in status {
                    let piece =
                        if key.is_empty() { format!("   {desc}") } else { format!("   {key} {desc}") };
                    if used + piece.width() > width {
                        break;
                    }
                    if key.is_empty() {
                        apply(out, st)?;
                        queue!(out, Print(&piece))?;
                    } else {
                        apply(out, Style { fg: ui.accent.fg, bold: true, ..base })?;
                        queue!(out, Print(format!("   {key}")))?;
                        apply(out, Style { fg: ui.virt.fg, ..base })?;
                        queue!(out, Print(format!(" {desc}")))?;
                    }
                    used += piece.width();
                }
                apply(out, base)?;
                queue!(out, Spaces(width.saturating_sub(used)))?;
                used = width;
            }
            RRow::Diff { kind, text: line, old_no, hunk } => {
                let decided = rev.hunks[*hunk].decision != Decision::Undecided;
                let bg = match kind {
                    '-' => minus_bg,
                    '+' if !decided => plus_bg,
                    _ => None,
                }
                .or(ui.base.bg);
                let base = Style { bg, ..ui.base };
                apply(out, Style { fg: ui.linenr.fg, ..base })?;
                queue!(
                    out,
                    Print(match old_no {
                        Some(no) => format!("{no:>num_w$} "),
                        None => " ".repeat(num_w + 1),
                    })
                )?;
                let (sign, sign_st) = match kind {
                    '-' => ("−", Style { fg: minus_fg, bold: true, ..base }),
                    '+' => ("▎", Style { fg: plus_fg, ..base }),
                    _ => (" ", base),
                };
                apply(out, sign_st)?;
                queue!(out, Print(sign), Print(" "))?;
                used = num_w + 3;
                for l in crate::markdown::code_lines(line, lang.as_deref(), t) {
                    if let crate::markdown::Line::Code(sp) = l {
                        for s in sp {
                            let txt = fit(&s.text, width.saturating_sub(used));
                            apply(out, base.patch(Style { bg: None, ..s.style }))?;
                            queue!(out, Print(&txt))?;
                            used += txt.width();
                        }
                    }
                }
                apply(out, base)?;
                queue!(out, Spaces(width.saturating_sub(used)))?;
                used = width;
            }
        }
        apply(out, ui.base)?;
        queue!(out, Spaces(width.saturating_sub(used)))?;
    }
    for k in shown.len()..body {
        queue!(out, MoveTo(0, lay.text_top + title_h as u16 + k as u16))?;
        apply(out, ui.base)?;
        queue!(out, Spaces(width))?;
    }
    Ok(())
}

/// Picker box placement: floats over the text area.
struct PickerBox {
    /// Box's leftmost column (centered for the small window).
    x: usize,
    compact: bool,
    list_rows: usize,
    /// Text width of left card (input, list) · right card (preview) (preview 0 when narrow — one card).
    list_w: usize,
    preview_w: usize,
}

impl PickerBox {
    /// Leftmost column of the preview card (None without one).
    fn preview_x(&self) -> Option<usize> {
        (self.preview_w > 0).then(|| self.x + self.list_w + 2 + 1)
    }

    /// Covers the whole text area — so text/line numbers behind don't leak. Cards inset one cell each side;
    /// when wide enough (≥ 100 cells) left list card / one-cell gap / right preview card.
    /// Vertical: half-row edge · input row · divider · list… · half-row edge.
    fn new(lay: &Layout, p: &Picker) -> Self {
        if p.compact {
            // Small card at top center: up to 60 cells wide, list as tall as its items (up to 12 rows)
            let w = lay.edit_w.saturating_sub(4).clamp(24, 60);
            let list_rows = p.counts().0.clamp(3, 12).min(lay.text_rows.saturating_sub(4).max(1));
            return PickerBox {
                x: lay.edit_w.saturating_sub(w) / 2,
                compact: true,
                list_rows,
                list_w: w - 2,
                preview_w: 0,
            };
        }
        let preview = p.preview;
        let w = lay.width.max(24);
        let h = lay.text_rows.max(6);
        let (left, right) = if preview && w >= 100 {
            let l = (w * 2 / 5).max(40);
            (l, w - 3 - l)
        } else {
            (w - 2, 0)
        };
        PickerBox {
            x: 0,
            compact: false,
            list_rows: h - 4,
            list_w: left - 2,
            preview_w: right.saturating_sub(2),
        }
    }
}

/// Have a worker thread read the selected item's file (if not loaded yet).
fn request_preview(p: &mut Picker, jobs: &crate::event::Jobs) {
    let Some(path) = p.current().and_then(|i| i.action.preview_key()) else {
        return;
    };
    if p.previews.contains_key(&path) {
        return;
    }
    if p.previews.len() >= crate::picker::PREVIEW_CACHE {
        p.previews.clear();
    }
    p.previews.insert(path.clone(), crate::picker::Preview::Loading);
    let id = p.id;
    jobs.spawn(move || {
        let preview = crate::picker::load_preview(&path);
        move |ed: &mut Editor| {
            if let Some(p) = ed.picker.as_mut().filter(|p| p.id == id) {
                p.previews.insert(path, preview);
            }
        }
    });
}

/// One line of code within `width` cells (applying syntax colors `spans`). Returns the number of cells used.
#[allow(clippy::too_many_arguments)]
fn draw_code_line(
    out: &mut impl Write,
    text: &ropey::Rope,
    line: usize,
    width: usize,
    tab: usize,
    spans: &[(usize, usize, u32)],
    capture_styles: &[Option<Style>],
    base: Style,
    (from, to): (usize, usize),
) -> io::Result<usize> {
    let start = mv::line_start(text, line);
    let (sc, ec) = (text.byte_to_char(start), text.byte_to_char(mv::line_end(text, line)));
    let end = text.char_to_byte(ec.min(sc + to.min(from + width) + 64));
    let s = text.byte_slice(start..end).to_string();
    let mut paint = vec![u32::MAX; s.len()];
    for &(a, b, c) in spans {
        let (a, b) = (a.max(start), b.min(end));
        if a < b {
            paint[a - start..b - start].fill(c);
        }
    }
    let mut current: Option<Style> = None;
    let mut used = 0;
    // Display columns [from, to) of the line (a wrapped row), drawn from the current cell
    for c in graphemes::cells(&s, start, tab) {
        if c.col < from {
            continue;
        }
        if c.col >= to || c.col - from + c.width > width {
            break;
        }
        let g = &s[c.bytes.clone()];
        let mut st = base;
        if let Some(Some(hs)) = paint.get(c.bytes.start).and_then(|&cap| capture_styles.get(cap as usize)) {
            st = st.patch(*hs);
        }
        if current != Some(st) {
            apply(out, st)?;
            current = Some(st);
        }
        if g == "\t" {
            queue!(out, Spaces(c.width))?;
        } else if g.chars().next().is_some_and(char::is_control) {
            queue!(out, Print('\u{fffd}'))?;
        } else {
            queue!(out, Print(g))?;
        }
        used = c.col + c.width - from;
    }
    Ok(used)
}

/// One list row (pickers, the side panel): accent bar when selected (`highlight` = on the selected
/// background too), tree indentation and guides, the fold mark or kind glyph, the label with its matched
/// letters, the hint at the right. Draws `2 + lw` cells but for the right padding; returns the cells used
/// after the bar.
#[allow(clippy::too_many_arguments)]
fn draw_list_row(
    out: &mut impl Write,
    r: &crate::picker::Row,
    lw: usize,
    editor: &Editor,
    ui: &Ui,
    card: Style,
    selected: Style,
    highlight: bool,
) -> io::Result<usize> {
    let t = &editor.theme;
    let matched = Style { fg: ui.accent.fg, bold: true, ..Style::default() };
    let mut used = 0;
    let base = if r.selected && highlight { selected } else { card };
    if r.selected {
        apply(out, Style { fg: ui.accent.fg, ..base })?;
        queue!(out, Print("▎ "))?;
    } else {
        apply(out, base)?;
        queue!(out, Print("  "))?;
    }
    let name = if r.selected { base.patch(t.try_get("ui.text.focus").unwrap_or_default()) } else { base };
    let name = Style { bg: base.bg, ..name };
    let (dim, faint) = (Style { fg: ui.virt.fg, ..base }, Style { fg: ui.linenr.fg, ..base });
    // Tree rows: indented by depth, then a column for `+`/`−` (folders) or the kind glyph,
    // so names at one depth line up
    if let Some((depth, _)) = r.tree {
        // A faint guide under each enclosing folder's fold column
        apply(out, Style { fg: ui.linenr.fg, ..base })?;
        let guides = tree_guides(depth);
        queue!(out, Print(&guides))?;
        used += guides.width();
    }
    let fold = match r.tree {
        // (a note: its place, in line with the names above)
        Some((_, Kind::Note)) => Some(" "),
        Some((_, Kind::Dir { open: Some(true) })) => Some("−"),
        Some((_, Kind::Dir { open: Some(false) })) => Some("+"),
        Some(_) if r.glyph.is_none() => Some(" "),
        _ => None,
    };
    if let Some(g) = fold {
        apply(out, dim)?;
        queue!(out, Print(g), Print(" "))?;
        used += 2;
    } else if let Some((g, scope)) = r.glyph {
        // Kind glyph (same color as in code; `ui.accent` through its fallbacks)
        let color = if scope == "ui.accent" { ui.accent.fg } else { t.try_get(scope).and_then(|s| s.fg) };
        let color = color.or(ui.virt.fg);
        apply(out, Style { fg: color, ..base })?;
        queue!(out, Print(g), Print(" "))?;
        used += 2;
    }
    let dir = matches!(r.tree, Some((_, Kind::Dir { .. })));
    // Annotation (keys, a line of code …) dimmed at the right end — the label comes first, the
    // annotation gets what's left (cut with …). An annotation that's what the row is for (a plugin tab's
    // state) comes first instead, the label keeping a few cells at least — every row says it.
    let room = if r.hint_first {
        lw.saturating_sub(1 + used + usize::from(dir) + 2 + 8)
    } else {
        lw.saturating_sub(1 + used + r.label.width() + usize::from(dir) + 2)
    };
    let hint = if room >= 4 { fit_ellipsis(&r.hint, room) } else { String::new() };
    let hint = hint.as_str();
    let hint_w = if hint.is_empty() { 0 } else { hint.width() + 2 };
    let label_room = lw.saturating_sub(1 + hint_w);
    // A label that doesn't fit: a file's or folder's name keeps its end (`user_prof…view.tsx` — the
    // extension, `_test`, `.spec` tell siblings apart), anything else ends in `…` (a folder keeps a cell for
    // its `/`)
    let fits = label_room.saturating_sub(used + usize::from(dir));
    let cut;
    let r = if r.label.width() > fits {
        let named = matches!(r.tree, Some((_, Kind::File | Kind::Dir { .. })));
        let (label, marks) = if named {
            fit_middle(&r.label, &r.marks, fits)
        } else {
            (fit_ellipsis(&r.label, fits), r.marks.clone())
        };
        cut = crate::picker::Row { label, marks, ..r.clone() };
        &cut
    } else {
        r
    };
    match r.tree {
        // A section heading (git tab: Staged / Changes) strong
        Some((_, Kind::Section)) => {
            let st =
                name.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
            let st = Style { bg: base.bg, ..st };
            used += print_matched(out, &r.label, &r.marks, st, matched, label_room.saturating_sub(used))?;
        }
        // Folders dim with a faint `/`; a hit's line number faint
        Some((_, Kind::Dir { .. })) => {
            let st = if r.selected { name } else { dim };
            used += print_matched(out, &r.label, &r.marks, st, matched, label_room.saturating_sub(used))?;
            if used < label_room {
                apply(out, faint)?;
                queue!(out, Print("/"))?;
                used += 1;
            }
        }
        Some((_, Kind::Hit)) => {
            let num = r.label.chars().take_while(|c| c.is_ascii_digit() || *c == ':').count();
            let (head, tail) = r.label.split_at(num);
            let (a, b): (Vec<u32>, Vec<u32>) = r.marks.iter().partition(|&&m| (m as usize) < num);
            let b: Vec<u32> = b.into_iter().map(|m| m - num as u32).collect();
            used += print_matched(out, head, &a, faint, matched, label_room.saturating_sub(used))?;
            used += print_matched(out, tail, &b, name, matched, label_room.saturating_sub(used))?;
        }
        // A note: in its glyph's color, italic
        Some((_, Kind::Note)) => {
            let scope = r.glyph.map_or("comment", |g| g.1);
            let fg = t.try_get(scope).and_then(|s| s.fg).or(ui.virt.fg);
            let st = Style { fg, italic: true, ..base };
            used += print_matched(out, &r.label, &r.marks, st, matched, label_room.saturating_sub(used))?;
        }
        _ => used += print_matched(out, &r.label, &r.marks, name, matched, label_room.saturating_sub(used))?,
    }
    apply(out, base)?;
    if hint_w > 0 && hint_w < lw {
        queue!(out, Spaces(lw - used - hint_w + 1))?;
        apply(out, Style { fg: ui.virt.fg, ..base })?;
        queue!(out, Print(hint), Print(" "))?;
        apply(out, base)?;
        used = lw;
    }
    Ok(used)
}

/// A tree row's guides: `│ ` under each enclosing folder — past the third, `│` alone (deep trees keep
/// room for names).
fn tree_guides(depth: usize) -> String {
    let wide = depth.min(3);
    format!("{}{}", "│ ".repeat(wide), "│".repeat(depth - wide))
}

/// `label` in `width` cells with its middle out — the start and the end stay (about 3/5 and 2/5, the end at
/// least the extension when it can); match marks (char indices) follow the cut.
fn fit_middle(label: &str, marks: &[u32], width: usize) -> (String, Vec<u32>) {
    let chars: Vec<char> = label.chars().collect();
    if label.width() <= width {
        return (label.to_string(), marks.to_vec());
    }
    if width < 5 {
        return (fit_ellipsis(label, width), marks.to_vec());
    }
    let ext = label.rfind('.').map_or(0, |i| label[i..].chars().count());
    let tail_w = ((width - 1) * 2 / 5).max(ext.min(width / 2));
    let head_w = width - 1 - tail_w;
    let cw = |c: &char| c.width().unwrap_or(0);
    let (mut head, mut w) = (0, 0);
    while head < chars.len() && w + cw(&chars[head]) <= head_w {
        w += cw(&chars[head]);
        head += 1;
    }
    let (mut tail, mut w) = (chars.len(), 0);
    while tail > head && w + cw(&chars[tail - 1]) <= tail_w {
        tail -= 1;
        w += cw(&chars[tail]);
    }
    // The end starts on a word (`…spec.ts`, not `…pec.ts`) when one begins a few characters back — the
    // start gives up that room
    let word = |j: usize| j > 0 && matches!(chars[j - 1], '_' | '.' | '-' | ' ');
    if !word(tail)
        && let Some(j) = (tail.saturating_sub(4)..tail).rev().find(|&j| j > 3 && word(j))
    {
        tail = j;
        let room = width - 1 - chars[tail..].iter().map(cw).sum::<usize>();
        let (mut h, mut w) = (0, 0);
        while h < tail && w + cw(&chars[h]) <= room {
            w += cw(&chars[h]);
            h += 1;
        }
        head = h;
    }
    let text: String = chars[..head].iter().chain(std::iter::once(&'…')).chain(&chars[tail..]).collect();
    let marks = marks
        .iter()
        .filter_map(|&m| {
            let m = m as usize;
            if m < head {
                Some(m as u32)
            } else if m >= tail {
                Some((m - tail + head + 1) as u32)
            } else {
                None
            }
        })
        .collect();
    (text, marks)
}

/// Bordered box + input row + list. Returns the input row's cursor position.
fn draw_picker(
    p: &Picker,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<(u16, u16)> {
    let b = PickerBox::new(lay, p);
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let selected = selected_row(ui, editor, card);
    let lw = b.list_w;
    let (m, n) = p.counts();
    // `m` as wide as `n`: typing changes it, and the title left of it stays put
    let count =
        if p.loading { "loading…".to_string() } else { format!("{m:>w$}/{n}", w = n.to_string().len()) };
    // Preview target
    let target = p.current().map(|i| i.action.clone());
    let preview_title = match &target {
        Some(Action::Open(path) | Action::Goto { path, .. }) => {
            path.strip_prefix(cwd()).unwrap_or(path).display().to_string()
        }
        Some(Action::Buffer(id) | Action::Jump { doc: id, .. }) => {
            editor.docs.iter().find(|d| d.id == *id).map(|d| d.display_name()).unwrap_or_default()
        }
        Some(Action::Code(i)) => match editor.action_preview(*i) {
            Some(crate::lsp_editor::ActionPreview::Ready(files)) => match files.as_slice() {
                [f] => format!(
                    "{}  {}",
                    rel_path(&f.path),
                    crate::editdiff::summary(files).split("  ").nth(1).unwrap_or("")
                ),
                _ => crate::editdiff::summary(files),
            },
            _ => String::new(),
        },
        Some(Action::Dir(rel)) => format!("{rel}/"),
        Some(Action::ChangedFile(n)) => match editor.changed_files.get(*n) {
            Some(c) => format!(
                "{}  {}",
                rel_path(&c.path),
                crate::editdiff::summary(&c.diff).split("  ").nth(1).unwrap_or("")
            ),
            None => String::new(),
        },
        Some(Action::ReplaceFile(n)) => match editor.replace_plan.get(*n) {
            Some(f) => format!(
                "{}  {}",
                rel_path(&f.path),
                crate::editdiff::summary(&f.diff).split("  ").nth(1).unwrap_or("")
            ),
            None => String::new(),
        },
        Some(Action::Commit { hash, .. }) => format!("commit {hash}"),
        Some(Action::Conversation { .. }) => "the conversation".into(),
        Some(
            Action::Command(_)
            | Action::Typed(_)
            | Action::Theme(_)
            | Action::Message
            | Action::Plugin { .. },
        )
        | None => String::new(),
    };
    let rows = b.list_rows + 4;
    let preview = if b.preview_w > 0 { preview_lines(p, editor, b.list_rows, b.preview_w) } else { None };
    // Diffs scroll by dropping rows from the top (the last page stays); lines wider than the pane
    // continue on the next rows
    let preview = preview.map(|v| match v {
        PreviewView::Diff { files, mut rows, styles } => {
            let skip = p.preview_scroll.min(rows.len().saturating_sub(b.list_rows));
            rows.drain(..skip);
            let room = b.preview_w.saturating_sub(7); // "{:>4} " + sign + space
            let tab = editor.config.tab_width;
            let rows = rows
                .into_iter()
                .take(b.list_rows)
                .flat_map(|(fi, li, _)| {
                    let line = li.and_then(|li| files.get(fi)?.lines.get(li));
                    match line {
                        Some(l) if l.text.width() > room && l.kind != crate::editdiff::LineKind::Gap => {
                            let r = ropey::Rope::from_str(&l.text);
                            crate::wrap::rows(&r, 0, room, tab)
                                .into_iter()
                                .map(|part| (fi, li, Some(part)))
                                .collect()
                        }
                        _ => vec![(fi, li, None)],
                    }
                })
                .collect();
            PreviewView::Diff { files, rows, styles }
        }
        v => v,
    });
    let (left_w, right_w) = (lw + 2, if b.preview_w > 0 { b.preview_w + 2 } else { 0 });
    let edge = Style { fg: card.bg, bg: ui.base.bg, ..Style::default() };
    for row in 0..rows {
        let y = lay.text_top + row as u16;
        if b.compact {
            queue!(out, MoveTo(b.x as u16, y))?;
        } else {
            queue!(out, MoveTo(0, y))?;
            apply(out, ui.base)?;
            queue!(out, Print(" "))?;
        }
        let last = row + 1 == rows;
        // ── Left card
        if row == 0 || last {
            if pad == 1 {
                apply(out, edge)?;
                queue!(out, Print((if row == 0 { "▄" } else { "▀" }).repeat(left_w)))?;
            } else {
                apply(out, faint)?;
                queue!(out, Print("─".repeat(left_w)))?;
            }
        } else if row == 1 {
            // Input row: › query ……… title · count
            apply(out, Style { fg: ui.accent.fg, bold: true, ..card })?;
            queue!(out, Print(" › "))?;
            let (q, right) = picker_input(p, lw, &count);
            apply(out, strong)?;
            queue!(out, Print(&q))?;
            apply(out, card)?;
            queue!(out, Spaces((lw + 2).saturating_sub(3 + q.width() + right.width())))?;
            apply(out, faint)?;
            queue!(out, Print(&right))?;
        } else if row == 2 {
            apply(out, card)?;
            queue!(out, Print(" "))?;
            apply(out, faint)?;
            queue!(out, Print("─".repeat(lw)))?;
            apply(out, card)?;
            queue!(out, Print(" "))?;
        } else {
            let mut used = 0;
            match p.view.get(row - 3) {
                Some(r) => used = draw_list_row(out, r, lw, editor, ui, card, selected, true)?,
                None => {
                    apply(out, card)?;
                    queue!(out, Print("  "))?;
                    // Nothing to show: say why, faint, where the first row would be
                    if row == 3 && p.view.is_empty() {
                        let why = match () {
                            _ if p.loading => "loading…",
                            _ if p.query.is_empty() => "nothing here",
                            _ => "no matches",
                        };
                        let why = fit(why, lw.saturating_sub(1));
                        apply(out, faint)?;
                        queue!(out, Print(&why))?;
                        used = why.width();
                    }
                }
            }
            queue!(out, Spaces(lw - used))?;
        }
        // ── Gap + right card (preview)
        if right_w > 0 {
            apply(out, ui.base)?;
            queue!(out, Print(" "))?;
            if row == 0 || last {
                if pad == 1 {
                    apply(out, edge)?;
                    queue!(out, Print((if row == 0 { "▄" } else { "▀" }).repeat(right_w)))?;
                } else {
                    apply(out, faint)?;
                    queue!(out, Print("─".repeat(right_w)))?;
                }
            } else if row == 1 {
                // Title: folder dimmed, file name crisp
                let (dir, file) = match preview_title.rfind('/') {
                    Some(k) => preview_title.split_at(k + 1),
                    None => ("", preview_title.as_str()),
                };
                let dir = fit_left(dir, b.preview_w.saturating_sub(file.width()));
                let file = fit(file, b.preview_w - dir.width());
                apply(out, dim)?;
                queue!(out, Print(" "), Print(&dir))?;
                apply(out, strong)?;
                queue!(out, Print(&file))?;
                apply(out, card)?;
                queue!(out, Spaces(b.preview_w + 1 - dir.width() - file.width()))?;
            } else if row == 2 {
                apply(out, card)?;
                queue!(out, Print(" "))?;
                apply(out, faint)?;
                queue!(out, Print("─".repeat(b.preview_w)))?;
                apply(out, card)?;
                queue!(out, Print(" "))?;
            } else {
                apply(out, card)?;
                queue!(out, Print(" "))?;
                let used = match &preview {
                    Some(pv) => draw_preview_row(out, pv, row - 3, b.preview_w, editor, ui, card)?,
                    None => 0,
                };
                apply(out, card)?;
                queue!(out, Spaces(b.preview_w - used + 1))?;
            }
        }
        if !b.compact {
            clear_rest(out, ui.base)?;
        }
    }
    let (q, _) = picker_input(p, lw, &count);
    Ok(((b.x + 1 + 3 + q.width()) as u16 - u16::from(b.compact), lay.text_top + 1))
}

/// The picker's input row after ` › `, in `lw - 1` cells: the query, and right of it the title and the count.
/// What's typed must show: the query comes first, then the count, and the title gets what's left, cut with
/// `…` (a long folder name in the title used to leave the query a single `…`). When everything fits, the
/// title keeps two blanks from the query.
fn picker_input(p: &Picker, lw: usize, count: &str) -> (String, String) {
    let tail = format!("  {count} ");
    let q = fit_left(&p.query, lw.saturating_sub(tail.width() + 3));
    let title = fit_ellipsis(&p.title, lw.saturating_sub(q.width() + tail.width() + 3));
    let right = fit_ellipsis(&format!("{title}{tail}"), lw.saturating_sub(q.width() + 1));
    (q, right)
}

/// One preview page: text, syntax, first line, line to highlight — or a hint message.
/// A diff preview row: file index, line in it (None = the file's header), which part of a wrapped line.
type DiffRow = (usize, Option<usize>, Option<crate::wrap::Row>);

enum PreviewView<'a> {
    Code {
        text: &'a ropey::Rope,
        /// Screen rows: (line, the part of it this row shows, is its first row) — long lines wrap.
        rows: Vec<(usize, crate::wrap::Row, bool)>,
        focus: Option<usize>,
        spans: Vec<(usize, usize, u32)>,
    },
    /// Changes the code action would make — per screen row (file index, line number | None = file header,
    /// the part of a wrapped line). Blank row between files = usize::MAX.
    Diff {
        files: std::sync::Arc<Vec<crate::editdiff::FileDiff>>,
        rows: Vec<DiffRow>,
        styles: std::sync::Arc<Vec<Option<Style>>>,
    },
    Note(String),
}

fn preview_lines<'a>(
    p: &'a Picker,
    editor: &'a Editor,
    rows: usize,
    width: usize,
) -> Option<PreviewView<'a>> {
    let action = &p.current()?.action;
    let (text, syntax, focus) = match action {
        Action::Buffer(id) => {
            let d = editor.docs.iter().find(|d| d.id == *id)?;
            let cur = mv::line_of(&d.text, d.selection().primary().head);
            (&d.text, d.syntax.as_ref(), Some(cur))
        }
        Action::Dir(_) => {
            let n = p.current().map_or("", |i| i.hint.as_str());
            return Some(PreviewView::Note(format!("folder · {n} files — enter or → opens it, ← folds it")));
        }
        Action::ChangedFile(n) => {
            let c = editor.changed_files.get(*n)?;
            if c.diff[0].lines.is_empty() {
                return Some(PreviewView::Note("no line changes (mode or binary)".into()));
            }
            let rows = (0..c.diff[0].lines.len()).map(|li| (0, Some(li), None)).collect();
            let styles = editor.theme.capture_styles();
            return Some(PreviewView::Diff { files: c.diff.clone(), rows, styles });
        }
        Action::ReplaceFile(n) => {
            let f = editor.replace_plan.get(*n)?;
            let rows = (0..f.diff[0].lines.len()).map(|li| (0, Some(li), None)).collect();
            let styles = editor.theme.capture_styles();
            return Some(PreviewView::Diff { files: f.diff.clone(), rows, styles });
        }
        Action::Jump { doc, line, .. } => {
            let d = editor.docs.iter().find(|d| d.id == *doc)?;
            (&d.text, d.syntax.as_ref(), Some(*line))
        }
        Action::Code(i) => {
            use crate::lsp_editor::ActionPreview;
            return Some(match editor.action_preview(*i)? {
                ActionPreview::Working => PreviewView::Note("working out the change…".into()),
                ActionPreview::CommandOnly => {
                    PreviewView::Note("runs a server command — nothing to preview".into())
                }
                ActionPreview::Failed(why) => PreviewView::Note(format!("no preview: {why}")),
                ActionPreview::Ready(files)
                    if files.iter().all(|f| f.lines.is_empty() && f.op == crate::editdiff::FileOp::Edit) =>
                {
                    PreviewView::Note("no changes".into())
                }
                ActionPreview::Ready(files) => {
                    let mut rows = Vec::new();
                    let headers =
                        files.len() > 1 || files.iter().any(|f| f.op != crate::editdiff::FileOp::Edit);
                    for (fi, f) in files.iter().enumerate() {
                        if fi > 0 {
                            rows.push((usize::MAX, None, None));
                        }
                        if headers {
                            rows.push((fi, None, None));
                        }
                        rows.extend((0..f.lines.len()).map(|li| (fi, Some(li), None)));
                    }
                    let styles = editor.theme.capture_styles();
                    PreviewView::Diff { files: files.clone(), rows, styles }
                }
            });
        }
        _ => {
            let focus = action.preview_target().and_then(|(_, f)| f);
            match p.previews.get(&action.preview_key()?)? {
                crate::picker::Preview::Loading => return Some(PreviewView::Note("loading…".into())),
                crate::picker::Preview::Unavailable(why) => return Some(PreviewView::Note(why.to_string())),
                crate::picker::Preview::Ready { text, syntax } => (text, syntax.as_ref(), focus),
            }
        }
    };
    let last = mv::last_line(text);
    // Scrolled (wheel / C-f C-b) from where it opens, but not past the last page
    let first = (focus.map_or(0, |f| f.saturating_sub(rows / 3)) + p.preview_scroll)
        .min(last.saturating_sub(rows.saturating_sub(1)).max(focus.unwrap_or(0).min(last)));
    // Screen rows from `first` — a line wider than the pane continues on the next rows (no number there)
    let tab = editor.config.tab_width;
    let room = width.saturating_sub(6); // "{:>4}  "
    let mut vis = Vec::with_capacity(rows);
    let mut line = first;
    while vis.len() < rows && line <= last {
        for (k, part) in crate::wrap::rows(text, line, room, tab).into_iter().enumerate() {
            if vis.len() == rows {
                break;
            }
            vis.push((line, part, k == 0));
        }
        line += 1;
    }
    let end_line = line.saturating_sub(1).max(first);
    let spans = match syntax.filter(|s| s.tree.is_some()) {
        Some(syn) => crate::syntax::highlights(
            syn,
            text,
            mv::line_start(text, first)..mv::line_full_end(text, end_line),
            &mut QueryCursor::new(),
        ),
        None => Vec::new(),
    };
    Some(PreviewView::Code { text, rows: vis, focus, spans })
}

fn draw_preview_row(
    out: &mut impl Write,
    pv: &PreviewView,
    row: usize,
    width: usize,
    editor: &Editor,
    ui: &Ui,
    popup: Style,
) -> io::Result<usize> {
    match pv {
        PreviewView::Diff { files, rows, styles } => {
            draw_diff_row(out, files, rows.get(row).copied(), width, editor, ui, popup, styles)
        }
        PreviewView::Note(msg) => {
            if row != 0 {
                return Ok(0);
            }
            let msg = fit_ellipsis(&format!("  {msg}"), width);
            apply(out, Style { fg: ui.virt.fg, ..popup })?;
            queue!(out, Print(&msg))?;
            Ok(msg.width())
        }
        PreviewView::Code { text, rows, focus, spans } => {
            let Some(&(line, part, first_row)) = rows.get(row) else { return Ok(0) };
            let base = if *focus == Some(line) {
                popup.patch(Style { bg: ui.cursorline.bg, ..Style::default() })
            } else {
                popup
            };
            // A wrapped line's later rows: no number, indented like the line
            let num = if first_row { format!("{:>4}  ", line + 1) } else { " ".repeat(6 + part.x0) };
            apply(out, Style { fg: ui.linenr.fg, ..base })?;
            queue!(out, Print(&num))?;
            let styles = editor.theme.capture_styles();
            let used = draw_code_line(
                out,
                text,
                line,
                width.saturating_sub(num.len()),
                editor.config.tab_width,
                spans,
                &styles,
                base,
                (part.col, part.end_col),
            )?;
            if *focus == Some(line) {
                apply(out, base)?;
                queue!(out, Spaces(width.saturating_sub(num.len() + used)))?;
                return Ok(width);
            }
            Ok(num.len() + used)
        }
    }
}

/// Path relative to the current folder (unchanged if outside).
fn rel_path(p: &std::path::Path) -> String {
    p.strip_prefix(cwd()).unwrap_or(p).display().to_string()
}

/// Code action preview line: file header (what it does + path + lines added/removed) or one diff line
/// (line number · −/+ · syntax-colored text — deleted lines on faint red, inserted on faint green background,
/// the changed middle a tone stronger).
#[allow(clippy::too_many_arguments)]
fn draw_diff_row(
    out: &mut impl Write,
    files: &[crate::editdiff::FileDiff],
    at: Option<DiffRow>,
    width: usize,
    editor: &Editor,
    ui: &Ui,
    card: Style,
    styles: &[Option<Style>],
) -> io::Result<usize> {
    use crate::editdiff::{FileOp, LineKind};
    let Some((fi, li, part)) = at else { return Ok(0) };
    let Some(f) = files.get(fi) else { return Ok(0) };
    let t = &editor.theme;
    let faint = Style { fg: ui.linenr.fg, ..card };
    let dim = Style { fg: ui.virt.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let (minus_fg, plus_fg) = (ui.diff_minus.fg, ui.diff_plus.fg);
    let Some(li) = li else {
        // File header
        let path = rel_path(&f.path);
        let mut parts: Vec<(String, Style)> = match &f.op {
            FileOp::Edit => vec![("● ".into(), Style { fg: ui.accent.fg, ..card }), (path, strong)],
            FileOp::Create => vec![
                ("+ ".into(), Style { fg: plus_fg, bold: true, ..card }),
                (path, strong),
                ("  new file".into(), dim),
            ],
            FileOp::Rename(from) => vec![
                ("→ ".into(), Style { fg: ui.accent.fg, bold: true, ..card }),
                (rel_path(from), dim),
                ("  →  ".into(), faint),
                (path, strong),
            ],
            FileOp::Delete => vec![
                ("− ".into(), Style { fg: minus_fg, bold: true, ..card }),
                (path, Style { crossed: true, ..strong }),
                ("  deleted".into(), dim),
            ],
        };
        if f.added + f.removed > 0 && f.op != FileOp::Delete {
            parts.push((format!("  +{}", f.added), Style { fg: plus_fg, ..card }));
            parts.push((format!(" −{}", f.removed), Style { fg: minus_fg, ..card }));
        }
        let mut used = 0;
        for (text, st) in parts {
            let s = fit(&text, width.saturating_sub(used));
            apply(out, st)?;
            queue!(out, Print(&s))?;
            used += s.width();
        }
        return Ok(used);
    };
    let Some(l) = f.lines.get(li) else { return Ok(0) };
    if l.kind == LineKind::Gap {
        apply(out, faint)?;
        queue!(out, Print("      …"))?;
        return Ok(7);
    }
    let (bg, strong_bg, sign, sign_fg) = match l.kind {
        LineKind::Minus => (blend(minus_fg, card.bg, 0.16), blend(minus_fg, card.bg, 0.34), "−", minus_fg),
        LineKind::Plus => (blend(plus_fg, card.bg, 0.16), blend(plus_fg, card.bg, 0.34), "+", plus_fg),
        _ => (card.bg, card.bg, " ", ui.linenr.fg),
    };
    let base = Style { bg: bg.or(card.bg), ..card };
    // Without truecolor (can't blend), use foreground color instead of background
    let tinted = bg.is_some() && l.kind != LineKind::Context;
    // A wrapped line's later rows: no number or sign, indented like the line
    let (from, to, x0) = part.map_or((0, usize::MAX, 0), |r| (r.col, r.end_col, r.x0));
    let first_row = from == 0;
    apply(out, Style { fg: ui.linenr.fg, ..base })?;
    let num = if first_row { format!("{:>4} ", l.number) } else { " ".repeat(5) };
    queue!(out, Print(&num))?;
    apply(out, Style { fg: sign_fg, bold: true, ..base })?;
    queue!(out, Print(if first_row { sign } else { " " }), Print(" "), Spaces(x0))?;
    let mut used = num.len() + 2 + x0;
    let room = width.saturating_sub(used);
    let mut paint = vec![u32::MAX; l.text.len()];
    for &(a, b, c) in &l.spans {
        let (a, b) = (a.min(l.text.len()), b.min(l.text.len()));
        if a < b {
            paint[a..b].fill(c);
        }
    }
    let mut current: Option<Style> = None;
    let mut col = 0;
    for c in graphemes::cells(&l.text, 0, editor.config.tab_width) {
        if c.col < from {
            continue;
        }
        if c.col >= to || c.col - from + c.width > room {
            break;
        }
        let g = &l.text[c.bytes.clone()];
        let mut st = base;
        if let Some(Some(hs)) = paint.get(c.bytes.start).and_then(|&cap| styles.get(cap as usize)) {
            st = st.patch(Style { bg: None, ..*hs });
        }
        if !tinted && l.kind != LineKind::Context && st.fg == card.fg {
            st.fg = sign_fg; // terminal can't paint backgrounds: distinguish by foreground
        }
        if let Some((a, b)) = l.emph
            && (a..b).contains(&c.bytes.start)
        {
            st.bg = strong_bg.or(st.bg);
        }
        if current != Some(st) {
            apply(out, st)?;
            current = Some(st);
        }
        if g == "\t" {
            queue!(out, Spaces(c.width))?;
        } else if g.chars().next().is_some_and(char::is_control) {
            queue!(out, Print('\u{fffd}'))?;
        } else {
            queue!(out, Print(g))?;
        }
        col = c.col + c.width - from;
    }
    used += col;
    // Deleted/inserted lines get background to the end
    if tinted {
        apply(out, base)?;
        queue!(out, Spaces(width.saturating_sub(used)))?;
        used = width;
    }
    Ok(used)
}

fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out
}

/// Top row: `project › folder › file ● › ƒ definition` ……… open buffers (current one bold).
/// When narrow, drop less important parts first (project, folders, outer definitions).
fn draw_header(editor: &Editor, ui: &Ui, lay: &Layout, out: &mut impl Write) -> io::Result<()> {
    let doc = editor.doc();
    let t = &editor.theme;
    let dim = Style { fg: ui.virt.fg, ..ui.base };
    let faint = Style { fg: ui.linenr.fg, ..ui.base };
    let strong =
        ui.base.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let sep = |keep| Seg { text: "  ›  ".into(), style: faint, keep };
    let mut left = vec![Seg { text: " ".into(), style: ui.base, keep: 255 }];
    let project = cwd().file_name().map(|p| p.to_string_lossy().into_owned());
    // Tests: a fixed project name (the checkout folder's name varies — worktrees, CI)
    #[cfg(test)]
    let project = project.map(|_| "tarae".to_string());
    if let Some(p) = project {
        left.push(Seg { text: p, style: dim, keep: 20 });
        left.push(sep(20));
    }
    let rel = doc.path.as_ref().map(|p| p.strip_prefix(cwd()).unwrap_or(p));
    let dirs: Vec<String> = rel
        .and_then(|r| r.parent())
        .map(|d| d.iter().map(|c| c.to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    for (i, d) in dirs.iter().enumerate() {
        let keep = 30 + i as u8; // folders closer to the file survive longer
        left.push(Seg { text: d.clone(), style: dim, keep });
        left.push(sep(keep));
    }
    if editor.welcome() {
        left.pop(); // separator after the project
    } else {
        left.push(Seg { text: doc.display_name_short(), style: strong, keep: 250 });
    }
    if doc.loading {
        left.push(Seg { text: " …".into(), style: dim, keep: 240 });
    } else if doc.is_modified() {
        left.push(Seg { text: " ●".into(), style: ui.base.patch(ui.accent), keep: 245 });
    }
    // `:diff`: what it's compared with
    if let Some(of) = &doc.diff_of {
        left.push(Seg { text: "  against ".into(), style: dim, keep: 244 });
        left.push(Seg { text: of.clone(), style: ui.base.patch(ui.accent), keep: 244 });
    }
    if doc.disk_conflict {
        let warn = t.try_get("warning").map(|s| Style { bg: None, ..s }).unwrap_or_default();
        left.push(Seg { text: "  ▲ changed on disk".into(), style: ui.base.patch(warn), keep: 246 });
    }
    if let Some(syn) = doc.syntax.as_ref() {
        let head = doc.selection().primary().cursor(&doc.text);
        let path = syntax::scope_path(syn, &doc.text, head);
        let n = path.len();
        for (i, (kind, name)) in path.into_iter().enumerate() {
            let keep = 100 + (i * 10 / n.max(1)) as u8; // inner definitions survive longer
            let (glyph, scope) = match kind {
                'f' => ("ƒ", "function"),
                'm' => ("§", "namespace"),
                _ => ("τ", "type"),
            };
            left.push(sep(keep));
            let color = t.try_get(scope).map_or(dim, |s| Style { bg: None, bold: false, italic: false, ..s });
            left.push(Seg { text: format!("{glyph} "), style: ui.base.patch(color), keep });
            left.push(Seg { text: name, style: dim, keep });
        }
    }
    let mut right = Vec::new();
    if editor.docs.len() > 1 {
        for (i, d) in editor.docs.iter().enumerate() {
            let current = i == editor.current;
            let mark = if d.is_modified() { " ●" } else { "" };
            let keep = if current { 90 } else { 10 };
            right.push(Seg {
                text: format!("{}{mark}  ", d.display_name_short()),
                style: if current { strong } else { dim },
                keep,
            });
        }
    }
    fit_segments(&mut left, &mut right, lay.width);
    queue!(out, MoveTo(0, 0))?;
    let mut used = 0;
    for s in &left {
        apply(out, s.style)?;
        queue!(out, Print(&s.text))?;
        used += s.text.width();
    }
    let right_w: usize = right.iter().map(|s| s.text.width()).sum();
    apply(out, ui.base)?;
    queue!(out, Spaces(lay.width.saturating_sub(used + right_w)))?;
    for s in &right {
        apply(out, s.style)?;
        queue!(out, Print(&s.text))?;
    }
    clear_rest(out, ui.base)
}

/// A statusline segment. Lowest `keep` is dropped first (when width runs short).
struct Seg {
    text: String,
    style: Style,
    keep: u8,
}

/// Drop less important segments from left/right until they fit in `width`.
fn fit_segments(left: &mut Vec<Seg>, right: &mut Vec<Seg>, width: usize) {
    let total = |l: &Vec<Seg>, r: &Vec<Seg>| l.iter().chain(r).map(|s| s.text.width()).sum::<usize>();
    // A blank spacer right after a segment with the same `keep` belongs to it — they go together, or a
    // stray gap is left behind (and the fill that should separate left from right goes missing)
    let drop = |v: &mut Vec<Seg>, i: usize| {
        let keep = v.remove(i).keep;
        if v.get(i).is_some_and(|s| s.keep == keep && s.text.trim().is_empty()) {
            v.remove(i);
        }
    };
    // Left and right never touch (`main● 1`) — keep at least a two-cell gap while both sides have content
    let gap = |l: &Vec<Seg>, r: &Vec<Seg>| if l.is_empty() || r.is_empty() { 0 } else { 2 };
    while total(left, right) + gap(left, right) > width {
        let lmin = left.iter().enumerate().min_by_key(|(_, s)| s.keep).map(|(i, s)| (s.keep, i));
        let rmin = right.iter().enumerate().min_by_key(|(_, s)| s.keep).map(|(i, s)| (s.keep, i));
        match (lmin, rmin) {
            (Some((lk, li)), Some((rk, _))) if lk <= rk => drop(left, li),
            (_, Some((_, ri))) => drop(right, ri),
            (Some((_, li)), None) => drop(left, li),
            (None, None) => break,
        }
    }
}

/// tarae statusline: `▐ NORMAL ▌  branch  progress ……… claude · diag · lang · encoding EOL · sel · pos`
/// Mode = pill (half blocks widen its bg half a cell each side), the rest dimmed — only mode, diagnostics,
/// position catch the eye. With the header off, the file name goes here too. Symbols only from common
/// monospace fonts (● ▲ · … ▐ ▌).
/// `folded`: the newest notification, when its card found no free place over the code.
fn draw_statusline(
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    folded: Option<&crate::editor::Toast>,
    out: &mut impl Write,
) -> io::Result<()> {
    let doc = editor.doc();
    let t = &editor.theme;
    let dim = Style { fg: ui.virt.fg.or(ui.linenr.fg), ..ui.status };
    let accent = ui.status.patch(ui.accent);
    let strong =
        ui.status.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let mode = match editor.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Select => "SELECT",
    };
    let mut left = Vec::new();
    match ui.status_mode.bg.filter(|bg| Some(*bg) != ui.status.bg) {
        Some(pill) => {
            let edge = Style { fg: Some(pill), bg: ui.status.bg, ..Style::default() };
            left.push(Seg { text: " ▐".into(), style: edge, keep: 255 });
            left.push(Seg { text: format!(" {mode} "), style: ui.status_mode, keep: 255 });
            left.push(Seg { text: "▌".into(), style: edge, keep: 255 });
        }
        None => left.push(Seg { text: format!(" {} ", &mode[..3]), style: ui.status_mode, keep: 255 }),
    }
    if !lay.header {
        let modified = if doc.is_modified() { " ●" } else { "" };
        left.push(Seg { text: format!("  {}", doc.display_name()), style: strong, keep: 200 });
        left.push(Seg { text: modified.into(), style: accent, keep: 200 });
    }
    if let Some(b) = &editor.git_branch {
        left.push(Seg { text: format!("   {b}"), style: dim, keep: 80 });
    }
    // How much this file changed from HEAD: +added/modified lines −removed lines (colors match the git bar)
    let (added, removed) = doc.git_hunks.iter().fold((0, 0), |(a, r), h| (a + h.lines.len(), r + h.removed));
    if added + removed > 0 {
        let color =
            |k: &str| ui.status.patch(Style { fg: t.try_get(k).and_then(|s| s.fg), ..Style::default() });
        if added > 0 {
            left.push(Seg { text: format!("  +{added}"), style: color("diff.plus"), keep: 85 });
        }
        if removed > 0 {
            left.push(Seg { text: format!("  −{removed}"), style: color("diff.minus"), keep: 85 });
        }
    }
    if let Some(toast) = folded {
        let bar = Style { fg: toast_color(editor, ui, toast.kind), ..ui.status };
        let text = toast.text.lines().next().unwrap_or_default();
        left.push(Seg { text: "   ▎ ".into(), style: bar, keep: 90 });
        left.push(Seg { text: fit_ellipsis(text, 56), style: ui.status, keep: 90 });
    }
    // Language server work, on the left: it changes with every report, and only here does its length
    // move nothing else
    if let Some(p) = editor.lsp_progress() {
        left.push(Seg { text: format!("   {}", fit_ellipsis(&p, 40)), style: dim, keep: 100 });
    }
    // What plugins show for this buffer (`status-bar`): first to go when the line runs short
    for s in editor.plugin_status() {
        left.push(Seg { text: format!("   {s}"), style: dim, keep: 70 });
    }

    let sel = doc.selection();
    let pos = cursor_idx(editor.mode, sel.primary(), &doc.text);
    let line = mv::line_of(&doc.text, pos);
    let col = doc.text.byte_slice(mv::line_start(&doc.text, line)..pos).len_chars() + 1; // char col for users
    let gap = |keep| Seg { text: "   ".into(), style: ui.status, keep };
    let mut right = Vec::new();
    match &editor.review {
        Some(r) if r.ready => {
            right.push(Seg {
                text: "claude ● review".into(),
                style: Style { bold: true, ..accent },
                keep: 220,
            });
            right.push(gap(220));
        }
        Some(r) => {
            right.push(Seg { text: format!("{} claude", wave(r.started)), style: accent, keep: 220 });
            right.push(gap(220));
        }
        None => {}
    }
    // Claude Code in the adjacent pane is attached (sees selection/diagnostics; edits come here as reviews)
    if editor.agent.as_ref().is_some_and(|a| a.connected()) && editor.review.is_none() {
        right.push(Seg { text: "◦ claude code".into(), style: accent, keep: 150 });
        right.push(gap(150));
    }
    // A plugin's badge (the cluster it's on), on every buffer — its tone's color when it's not just fine
    if let Some((_, text, tone)) = editor.plugins.as_ref().and_then(|p| p.badge.as_ref()) {
        use crate::plugin::PluginTone as T;
        let style = match tone {
            T::Ok | T::Info => dim,
            T::Warning => {
                ui.status.patch(t.try_get("warning").map(|s| Style { bg: None, ..s }).unwrap_or_default())
            }
            T::Error => ui.status.patch(Style { bg: None, ..ui.error }),
        };
        right.push(Seg { text: text.clone(), style, keep: 190 });
        right.push(gap(190));
    }
    let [errors, warnings] = doc.diagnostic_counts();
    if errors > 0 {
        right.push(Seg {
            text: format!("● {errors}"),
            style: ui.status.patch(Style { bg: None, ..ui.error }),
            keep: 210,
        });
        right.push(gap(210));
    }
    if warnings > 0 {
        let warn = t.try_get("warning").map(|s| Style { bg: None, ..s }).unwrap_or_default();
        right.push(Seg { text: format!("▲ {warnings}"), style: ui.status.patch(warn), keep: 210 });
        right.push(gap(210));
    }
    if let Some(h) = &editor.search_hits {
        let n = h.matches.len();
        let count = match h.current(sel.primary()) {
            Some(i) => format!("{i:>w$}/{n}{}", if h.capped { "+" } else { "" }, w = n.to_string().len()),
            None if n == 0 && !h.capped => "no matches".to_string(),
            None => format!("{n}{} matches", if h.capped { "+" } else { "" }),
        };
        right.push(Seg { text: format!("/{}  ", fit_ellipsis(&h.pattern, 24)), style: dim, keep: 205 });
        right.push(Seg { text: count, style: accent, keep: 206 });
        right.push(gap(206));
    }
    if let Some(s) = &doc.syntax {
        // The language name says how its server is doing — no extra cells: dim italic starting · success
        // color connected · error color failed/exited · faintest when none is installed · plain dim otherwise
        use crate::lsp_editor::LspDot;
        let style = match editor.lsp_state().map(|s| s.0) {
            Some(LspDot::Starting) => Style { italic: true, ..dim },
            Some(LspDot::Ready) => {
                Style { fg: editor.theme.try_get("diff.plus").and_then(|s| s.fg).or(ui.accent.fg), ..dim }
            }
            Some(LspDot::Failed) => Style { fg: ui.error.fg, ..dim },
            Some(LspDot::Missing) => Style { fg: ui.linenr.fg, ..dim },
            None => dim,
        };
        right.push(Seg { text: s.lang.name.clone(), style, keep: 60 });
        // Servers beside the main one, faint: `python + ruff`
        let also: Vec<String> = doc
            .lsp
            .beside()
            .filter_map(|cid| editor.lsp.clients.iter().find(|c| c.id == cid))
            .map(|c| format!(" + {}", c.name))
            .collect();
        if !also.is_empty() {
            right.push(Seg { text: also.concat(), style: Style { fg: ui.linenr.fg, ..dim }, keep: 30 });
        }
        right.push(gap(60));
    }
    let crlf = doc.text.len_lines() > 1 && mv::line_full_end(&doc.text, 0) - mv::line_end(&doc.text, 0) == 2;
    right.push(Seg { text: format!("utf-8 {}", if crlf { "CRLF" } else { "LF" }), style: dim, keep: 40 });
    right.push(gap(40));
    if sel.len() > 1 {
        right.push(Seg { text: format!("{} sels", sel.len()), style: accent, keep: 150 });
        right.push(gap(150));
    }
    // The column two cells at least: the right side is right-aligned, and `l`/`w` crossing column 10 would
    // slide everything left of it back and forth (the line number changes digits far less often)
    right.push(Seg { text: format!("{}:{:<2} ", line + 1, col), style: strong, keep: 250 });

    fit_segments(&mut left, &mut right, lay.width);
    queue!(out, MoveTo(0, lay.status_y))?;
    let mut used = 0;
    for s in &left {
        apply(out, s.style)?;
        queue!(out, Print(&s.text))?;
        used += s.text.width();
    }
    let right_w: usize = right.iter().map(|s| s.text.width()).sum();
    apply(out, ui.status)?;
    queue!(out, Spaces(lay.width.saturating_sub(used + right_w)))?;
    for s in &right {
        apply(out, s.style)?;
        queue!(out, Print(&s.text))?;
    }
    clear_rest(out, ui.status)
}

/// Completion list — below the word so the name column lines up with the typed word (above if no room).
///
/// ```text
/// ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   ← half-row margin (card: when the theme has a floating bg color)
/// ▎ƒ get          Option<&V>  ← selected row: accent bar + strong background
///  ƒ get_mut   Option<&mut V>▐ ← kind glyph in code colors, typed chars in accent, right edge = scrollbar
/// ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
/// ```
fn draw_completion(
    c: &crate::completion::Completion,
    (cx, cy): (u16, u16),
    ui: &Ui,
    editor: &Editor,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    const ROWS: usize = 10;
    const LABEL_MAX: usize = 42;
    const DETAIL_MAX: usize = 30;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.menu");
    let selected = selected_row(ui, editor, card);
    let dim = Style { fg: ui.virt.fg, ..card };
    let matched = Style { bold: true, ..Style { fg: ui.accent.fg, ..Style::default() } };
    let pad = card_padding(ui, card);
    let n = c.shown.len();
    let rows = n.min(ROWS).min(lay.text_rows.saturating_sub(2 * pad));
    if rows == 0 {
        return Ok(());
    }
    let first = c.selected.map_or(0, |s| s.saturating_sub(rows - 1));
    let visible = first..first + rows;
    let label_w = visible.clone().map(|i| c.item(i).label.width()).max().unwrap_or(0).min(LABEL_MAX);
    let detail_w = visible.clone().map(|i| c.item(i).detail.width()).max().unwrap_or(0).min(DETAIL_MAX);
    let scroll = n > rows;
    // [bar][glyph][ ][name][   detail][ |scroll]
    let width = (3 + label_w + if detail_w > 0 { 3 + detail_w } else { 0 } + 2).min(lay.edit_w);
    let doc = editor.doc();
    let head = doc.selection().primary().head;
    let typed = mv::visual_col(&doc.text, head, editor.config.tab_width)
        - mv::visual_col(&doc.text, c.start.min(head), editor.config.tab_width);
    let word_x = (cx as usize).saturating_sub(typed);
    let x0 = word_x.saturating_sub(3).min(lay.edit_w.saturating_sub(width));
    let (top, bottom) = (lay.text_top as usize, lay.text_top as usize + lay.text_rows);
    let h = rows + 2 * pad;
    let below = cy as usize + 1 + h <= bottom;
    let y0 = if below { cy as usize + 1 } else { (cy as usize).saturating_sub(h).max(top) };
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    let thumb = scroll_thumb(rows, n, first);
    for row in 0..rows {
        let i = first + row;
        let item = c.item(i);
        let is_sel = c.selected == Some(i);
        let base = if is_sel { selected } else { card };
        queue!(out, MoveTo(x0 as u16, (y0 + pad + row) as u16))?;
        if is_sel {
            apply(out, Style { fg: ui.accent.fg, ..base })?;
            queue!(out, Print("▎"))?;
        } else {
            apply(out, base)?;
            queue!(out, Print(" "))?;
        }
        let glyph = crate::completion::kind_glyph(item.kind);
        let color = t.try_get(crate::completion::kind_scope(item.kind)).map_or(dim, |s| Style {
            bg: None,
            bold: false,
            italic: false,
            ..s
        });
        apply(out, base.patch(color))?;
        queue!(out, Print(glyph), Print(" "))?;
        // Name: highlight typed chars, … on overflow
        let label = fit_ellipsis(&item.label, label_w);
        let hits = c.hits.get(i).map_or(&[][..], Vec::as_slice);
        let name = if is_sel { base.patch(t.try_get("ui.text.focus").unwrap_or_default()) } else { base };
        let name = Style { bg: base.bg, ..name };
        print_matched(out, &label, hits, name, matched, label_w)?;
        apply(out, base)?;
        queue!(out, Spaces(label_w - label.width()))?;
        let mut used = 3 + label_w;
        if detail_w > 0 {
            let d = fit_ellipsis(&item.detail, detail_w);
            apply(out, Style { fg: ui.virt.fg, ..base })?;
            queue!(out, Print("   "), Spaces(detail_w - d.width()), Print(&d))?;
            used += 3 + detail_w;
        }
        apply(out, base)?;
        queue!(out, Spaces(width.saturating_sub(used + 1)))?;
        if scroll && thumb.contains(&row) {
            apply(out, Style { fg: ui.linenr.fg, ..card })?;
            queue!(out, Print("▐"))?;
        } else {
            apply(out, card)?;
            queue!(out, Print(" "))?;
        }
    }
    card_edge(out, ui, card, x0, y0 + pad + rows, width, pad, false)?;
    // Selected item's docs: one cell right of the list (left if no room), top-aligned with the list
    if let Some((_, lines)) = c.docs.as_ref().filter(|(_, l)| !l.is_empty()) {
        const MIN: usize = 24;
        let natural = lines.iter().map(crate::markdown::Line::width).max().unwrap_or(0).max(MIN);
        let right = lay.edit_w.saturating_sub(x0 + width + 1);
        // If the list floats above the cursor, keep the docs off the cursor line too
        let limit = if below { bottom } else { cy as usize };
        let side = if right >= MIN + 4 {
            Some((x0 + width + 1, natural.min(64).min(right - 4)))
        } else if x0 > MIN + 4 {
            let inner = natural.min(64).min(x0 - 5);
            Some((x0 - inner - 5, inner))
        } else {
            None
        };
        match side {
            Some((x, inner)) => {
                let wrapped = crate::markdown::wrap(lines, inner, dim);
                let max_rows = limit.saturating_sub(y0 + 2).clamp(1, 16);
                draw_doc_box(&wrapped, (x, y0), inner, max_rows, ui, editor, out)?;
                // The cell between the two cards: blank where both are (not a letter of the code behind)
                let gap = if x > x0 { x0 + width } else { x0 - 1 };
                apply(out, ui.base)?;
                for y in y0..y0 + h.min(wrapped.len().min(max_rows) + 2) {
                    queue!(out, MoveTo(gap as u16, y as u16), Print(" "))?;
                }
            }
            // No room beside: outside the list (below a downward list, above an upward one), same width
            None => {
                let inner = width.max(MIN + 4).min(lay.edit_w).saturating_sub(4);
                let wrapped = crate::markdown::wrap(lines, inner, dim);
                let room = if below { bottom.saturating_sub(y0 + h) } else { y0.saturating_sub(top) };
                if room >= 4 {
                    let max_rows = (room - 2).min(12);
                    let dh = wrapped.len().min(max_rows) + 2;
                    let y = if below { y0 + h } else { y0 - dh };
                    draw_doc_box(&wrapped, (x0, y), inner, max_rows, ui, editor, out)?;
                }
            }
        }
    }
    Ok(())
}

/// Signature card — **above** the cursor line (completion opens below, so no overlap), below if no room above
/// (skipped if the list is open). Colored by that language's grammar; current param bold accent + underline.
/// Long signatures fold the front into `…` to keep the current param visible. Param docs dimmed, ≤ 3 lines.
fn draw_signature(
    sig: &crate::signature::Signature,
    (cx, cy): (u16, u16),
    ui: &Ui,
    editor: &Editor,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    if sig.doc != editor.doc().id {
        return Ok(());
    }
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let lang = editor.doc().syntax.as_ref().map(|s| s.lang.name.clone());
    let label = sig.label.replace(['\n', '\t'], " ");
    // Signature → (char, style) cells, overlaying emphasis on the current param span only
    let mut cells: Vec<(char, Style)> = Vec::new();
    let mut byte = 0;
    let active = Style { fg: ui.accent.fg, bold: true, underline: true, ..Style::default() };
    for line in crate::markdown::code_lines(&label, lang.as_deref(), &editor.theme) {
        if let crate::markdown::Line::Code(spans) = line {
            for sp in spans {
                for ch in sp.text.chars() {
                    let on = sig.active.is_some_and(|(a, b)| a <= byte && byte < b);
                    cells.push((
                        ch,
                        if on { card.patch(sp.style).patch(active) } else { card.patch(sp.style) },
                    ));
                    byte += ch.len_utf8();
                }
            }
        }
    }
    let counter = if sig.count > 1 { format!("  {}/{}", sig.index + 1, sig.count) } else { String::new() };
    let full: usize = cells.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
    // So room (= inner − count indicator) never goes below 0 even on very narrow screens
    let inner =
        (full + counter.width()).clamp(10, 96).min(lay.edit_w.saturating_sub(4)).max(counter.width() + 4);
    let room = inner - counter.width();
    // If long, cut the front so the current param lands at the 1/3 mark
    let active_col: usize = sig
        .active
        .map(|(a, _)| label[..a.min(label.len())].chars().map(|c| c.width().unwrap_or(0)).sum())
        .unwrap_or(0);
    let skip_cols = if full > room { active_col.saturating_sub(room / 3).min(full - room + 1) } else { 0 };
    let docs = if sig.docs.is_empty() {
        Vec::new()
    } else {
        let lines = crate::markdown::render(&sig.docs, &editor.theme, lang.as_deref());
        crate::markdown::wrap(&lines, inner, Style { fg: ui.virt.fg, ..Style::default() })
    };
    let doc_rows = docs.len().min(3);
    let h = 1 + doc_rows + 2 * pad;
    let (top, bottom) = (lay.text_top as usize, lay.text_top as usize + lay.text_rows);
    let y0 = if cy as usize >= top + h {
        cy as usize - h
    } else if editor.completion.is_none() && cy as usize + 1 + h <= bottom {
        cy as usize + 1
    } else {
        return Ok(());
    };
    let width = inner + 4;
    let x0 = (cx as usize).saturating_sub(2).min(lay.edit_w.saturating_sub(width));
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    // Signature row
    let y = y0 + pad;
    queue!(out, MoveTo(x0 as u16, y as u16))?;
    apply(out, card)?;
    queue!(out, Print("  "))?;
    let mut used = 0;
    let mut col = 0;
    if skip_cols > 0 {
        apply(out, Style { fg: ui.virt.fg, ..card })?;
        queue!(out, Print("…"))?;
        used = 1;
    }
    let mut current = Style::default();
    for &(ch, st) in &cells {
        let w = ch.width().unwrap_or(0);
        col += w;
        if col <= skip_cols {
            continue;
        }
        if used + w > room {
            break;
        }
        if st != current {
            apply(out, st)?;
            current = st;
        }
        queue!(out, Print(ch))?;
        used += w;
    }
    apply(out, card)?;
    queue!(out, Spaces(inner - used - counter.width()))?;
    apply(out, Style { fg: ui.linenr.fg, ..card })?;
    queue!(out, Print(&counter), Print("  "))?;
    // Param docs
    for (i, spans) in docs.iter().take(doc_rows).enumerate() {
        queue!(out, MoveTo(x0 as u16, (y + 1 + i) as u16))?;
        apply(out, card)?;
        queue!(out, Print("  "))?;
        let mut used = 0;
        for sp in spans {
            let t = fit(&sp.text, inner - used);
            apply(
                out,
                card.patch(Style { fg: ui.virt.fg, ..Style::default() })
                    .patch(Style { fg: sp.style.fg.or(ui.virt.fg), ..sp.style }),
            )?;
            queue!(out, Print(&t))?;
            used += t.width();
        }
        apply(out, card)?;
        queue!(out, Spaces(inner - used + 2))?;
    }
    card_edge(out, ui, card, x0, y + 1 + doc_rows, width, pad, false)
}

/// Welcome screen (no file): spaced-out name · one-line intro · first-step keys · recent files (number keys).
/// One block in the middle — first-timers instantly know what to press.
fn draw_welcome(editor: &Editor, ui: &Ui, lay: &Layout, out: &mut impl Write) -> io::Result<()> {
    let t = &editor.theme;
    let dim = Style { fg: ui.virt.fg, ..ui.base };
    let faint = Style { fg: ui.linenr.fg, ..ui.base };
    let strong =
        ui.base.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let key = Style { fg: ui.accent.fg, bold: true, ..ui.base };
    let home = std::env::var("HOME").unwrap_or_default();
    // Right of the side panel when it's open
    let (ox, ew) = (lay.rail_w, lay.edit_w - lay.rail_w);
    let mut rows: Vec<Vec<(String, Style)>> = vec![
        vec![("t a r a e".into(), Style { fg: ui.accent.fg, bold: true, ..ui.base })],
        vec![("타래 · Helix keys, Claude inside".into(), dim)],
        vec![],
    ];
    // This folder's last session first — the most likely next step when there is one
    if let Some(files) = editor.last_session.as_ref().filter(|f| !f.is_empty()) {
        let name = files[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let more = if files.len() > 1 { format!(" +{}", files.len() - 1) } else { String::new() };
        rows.push(vec![
            (format!("{:<9}", "r"), key),
            ("Continue where you left off  ".into(), strong),
            (format!("{name}{more}"), faint),
        ]);
        rows.push(vec![]);
    }
    for (k, d) in [
        ("space f", "Open a file"),
        ("space e", "Browse the files"),
        ("space /", "Search in project"),
        ("space l", "Chat with Claude"),
        ("space ?", "Find a command"),
        (":tutor", "Learn the keys in 10 minutes"),
        (":q", "Quit"),
    ] {
        rows.push(vec![(format!("{k:<9}"), key), (d.into(), ui.base)]);
    }
    // Rows from here on are recent files (their folders are cut from the left)
    let mut recent_from = usize::MAX;
    if !editor.recent.is_empty() {
        rows.push(vec![]);
        rows.push(vec![("Recent".into(), faint)]);
        recent_from = rows.len();
        for (i, p) in editor.recent.iter().take(5).enumerate() {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = p.parent().map(|d| d.display().to_string()).unwrap_or_default();
            let dir = match dir.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => dir,
            };
            rows.push(vec![(format!("{:<9}", i + 1), key), (format!("{name}  "), strong), (dir, faint)]);
        }
    }
    let block_w = rows
        .iter()
        .map(|r| r.iter().map(|(s, _)| s.width()).sum::<usize>())
        .max()
        .unwrap_or(0)
        .min(ew.saturating_sub(4))
        .max(32);
    let top = lay.text_top as usize + lay.text_rows.saturating_sub(rows.len() + 2) / 2;
    let x0 = ox + ew.saturating_sub(block_w) / 2;
    for row in 0..lay.text_rows {
        let y = lay.text_top as usize + row;
        queue!(out, MoveTo(ox as u16, y as u16))?;
        apply(out, ui.base)?;
        queue!(out, Spaces(ew))?;
        if let Some(r) = (y >= top).then(|| rows.get(y - top)).flatten() {
            // First two rows (name, intro) centered, the rest aligned to the block's left
            let w: usize = r.iter().map(|(s, _)| s.width()).sum();
            let x = if y - top < 2 { ox + ew.saturating_sub(w) / 2 } else { x0 };
            queue!(out, MoveTo(x as u16, y as u16))?;
            let mut used = 0;
            for (k, (s, st)) in r.iter().enumerate() {
                // A recent file's folder (its last part) is cut from the left: its nearest folders tell
                let room = block_w.saturating_sub(used);
                let folder = y - top >= recent_from && k + 1 == r.len();
                let s = if folder { fit_left(s, room) } else { fit_ellipsis(s, room) };
                apply(out, *st)?;
                queue!(out, Print(&s))?;
                used += s.width();
            }
        }
    }
    // Bottom center: version · theme
    let foot = format!("tarae {} · {}", env!("CARGO_PKG_VERSION"), t.name);
    queue!(
        out,
        MoveTo((ox + ew.saturating_sub(foot.width()) / 2) as u16, lay.text_top + lay.text_rows as u16 - 1)
    )?;
    apply(out, faint)?;
    queue!(out, Print(foot))?;
    Ok(())
}

/// A notification kind's bar color: accent (info) · diff.plus (success) · warning · error.
fn toast_color(editor: &Editor, ui: &Ui, kind: crate::editor::ToastKind) -> Option<crossterm::style::Color> {
    use crate::editor::ToastKind;
    let t = &editor.theme;
    match kind {
        ToastKind::Info => ui.accent.fg,
        ToastKind::Success => t.try_get("diff.plus").and_then(|s| s.fg),
        ToastKind::Warning => t.try_get("warning").and_then(|s| s.fg),
        ToastKind::Error => ui.error.fg,
    }
}

/// The notification card's row: the top of the editing rows `[top, bottom)`, else their bottom (a row above
/// the end), wherever it overlaps none of `taken` — None if neither is free.
fn toast_place(
    (x, w, h): (usize, usize, usize),
    (top, bottom): (usize, usize),
    taken: &[Rect4],
) -> Option<usize> {
    let free = |y: usize| y >= top && y + h <= bottom && taken.iter().all(|&r| !overlaps((x, y, w, h), r));
    [top, bottom.saturating_sub(h + 1)].into_iter().find(|&y| free(y))
}

/// Older messages a card shows under the newest (one row each) — more are counted.
const TOAST_OLDER: usize = 2;

/// The notification card: what came together is one card — the newest message on top (its kind's bar,
/// wrapped up to three rows), the ones before it under it, dimmer, a row each, the rest counted. It fades
/// in with its first message and out with its last. It keeps out of the other cards' way (`taken`: the
/// note, the minimized chat, the thought card, a grammar offer): the top right of the editing area, else its
/// bottom right. No free place → false (the status line carries the newest message).
///
/// ```text
///  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
///   ▎ Saved lib.rs · 42 lines
///   ▎ rust-analyzer ready            ← older: dim
///     +2 earlier · :messages
///  ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
/// ```
fn draw_toasts(
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    taken: &[Rect4],
    out: &mut impl Write,
) -> io::Result<bool> {
    use crate::editor::ToastKind;
    let Some(newest) = editor.toasts.last() else { return Ok(true) };
    // A picker covers the code: the message goes to the status line under it, not unseen
    if editor.picker.is_some() {
        return Ok(false);
    }
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let max_inner = (lay.edit_w / 2).clamp(24, 56).min(lay.edit_w.saturating_sub(8));
    let a = editor.toast_alpha();
    let bg = blend(card.bg, ui.tint, a).or(card.bg);
    let fade = |c: Option<crossterm::style::Color>| blend(c, bg, a).or(c);
    let body = Style { fg: fade(card.fg), bg, ..Style::default() };
    let dim = Style { fg: fade(ui.virt.fg.or(card.fg)), bg, ..Style::default() };
    let faint = Style { fg: fade(ui.linenr.fg.or(card.fg)), bg, ..Style::default() };
    let bar = |kind| Style { fg: fade(toast_color(editor, ui, kind)), bg, ..Style::default() };

    // (bar, text, style) per row
    let mut rows: Vec<(Option<Style>, String, Style)> = Vec::new();
    // The newest: messages with newlines (language server notices etc.) wrap per paragraph
    let para: Vec<crate::markdown::Line> = newest
        .text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| crate::markdown::Line::Text {
            spans: vec![crate::markdown::Span { text: l.trim_end().to_string(), style: Style::default() }],
            indent: 0,
        })
        .collect();
    let natural = newest.text.lines().map(|l| l.trim_end().width()).max().unwrap_or(0).min(max_inner);
    let wrapped = crate::markdown::wrap(&para, natural.max(1), Style::default());
    let cut = wrapped.len() > 3;
    for (i, l) in wrapped.into_iter().take(3).enumerate() {
        let mut text: String = l.into_iter().map(|s| s.text).collect();
        if cut && i == 2 {
            text = fit_ellipsis(&format!("{text} …"), max_inner);
        }
        rows.push(((i == 0).then(|| bar(newest.kind)), text, body));
    }
    let older = editor.toasts.len() - 1;
    for t in editor.toasts.iter().rev().skip(1).take(TOAST_OLDER) {
        let first = t.text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
        // An earlier error or warning keeps its weight — only the routine ones step back
        let (b, style) = match t.kind {
            ToastKind::Error | ToastKind::Warning => (bar(t.kind), body),
            _ => (Style { fg: blend(toast_color(editor, ui, t.kind), bg, 0.45), ..bar(t.kind) }, dim),
        };
        rows.push((Some(b), fit_ellipsis(first.trim_end(), max_inner), style));
    }
    if older > TOAST_OLDER {
        rows.push((None, format!("+{} earlier · :messages", older - TOAST_OLDER), faint));
    }
    let inner = rows.iter().map(|r| r.1.width()).max().unwrap_or(0);
    let width = inner + 5; // margin 1 · bar · margin 1 · text · margin 2
    let h = rows.len() + 2 * pad;
    let x0 = lay.edit_w.saturating_sub(width + 1 + usize::from(lay.scrollbar));
    let rows_at = (lay.text_top as usize, lay.text_top as usize + lay.text_rows);
    let Some(y0) = toast_place((x0, width, h), rows_at, taken) else { return Ok(false) };
    editor.screen.toast_card.set(Some((x0, y0, width, h)));
    let edge_card = Style { bg, ..card };
    card_edge(out, ui, edge_card, x0, y0, width, pad, true)?;
    for (i, (b, text, style)) in rows.iter().enumerate() {
        queue!(out, MoveTo(x0 as u16, (y0 + pad + i) as u16))?;
        apply(out, body)?;
        queue!(out, Print(" "))?;
        match b {
            Some(b) => {
                apply(out, *b)?;
                queue!(out, Print("▎"))?;
            }
            None => queue!(out, Print(" "))?,
        }
        apply(out, *style)?;
        queue!(out, Print(" "), Print(text), Spaces(inner - text.width() + 2))?;
    }
    card_edge(out, ui, edge_card, x0, y0 + pad + rows.len(), width, pad, false)?;
    Ok(true)
}

/// Grammar download offer (like IntelliJ's "install plugin" notice) — bottom right of the editing area:
///
/// ```text
///  ▎ Rust syntax colors
///    Download and build the tree-sitter grammar?
///
///    y Install   n Not now
/// ```
///
/// An install (a language server, a debugger) also has `N Never`.
///
/// While downloading, a wave + "Fetching and building…"; on failure the reason + `y Retry` `n Dismiss`.
struct OfferCard {
    x: usize,
    y: usize,
    inner: usize,
    /// (text, dimmed?)
    lines: Vec<(String, bool)>,
    /// Button row (None while downloading)
    buttons_row: Option<usize>,
    /// (row, start col, end col, key) — for mouse (`Screen.offer_buttons`; digit = bring back card N forward)
    buttons: Vec<(usize, usize, usize, char)>,
    /// The buttons: (key, label) — the first bright, the rest dim.
    labels: Vec<(char, &'static str)>,
    pad: usize,
    failed: bool,
    /// When stacked, the "1/3" at the right of the title row (else empty).
    badge: String,
    /// Downloading — the wave at the start of the second line in accent.
    busy: bool,
    /// Cards stacked behind (nearest first): (depth 1.., title, start time if downloading, failed?).
    peeks: Vec<(usize, String, Option<std::time::Instant>, bool)>,
}

/// How many cells a back card is inset on each side relative to the one in front.
const PEEK_INSET: usize = 2;

impl OfferCard {
    fn new(editor: &Editor, lay: &Layout, ui: &Ui) -> Option<Self> {
        use crate::offer::OfferState;
        let o = editor.offers.first()?;
        if editor.picker.is_some() || !editor.pending.is_empty() || lay.text_rows < 8 || lay.edit_w == 0 {
            return None;
        }
        let pad = card_padding(ui, card_style(ui, editor, "ui.popup"));
        let max_inner = (lay.edit_w / 2).clamp(30, 56).min(lay.edit_w.saturating_sub(8));
        let count = editor.offers.len();
        let badge = if count > 1 { format!("1/{count}") } else { String::new() };
        let title = o.title.clone();
        // A paragraph wrapped to the card, up to `n` lines (a command to run is shown whole)
        let wrapped = |text: &str, n: usize| -> Vec<(String, bool)> {
            let para = [crate::markdown::Line::Text {
                spans: vec![crate::markdown::Span { text: text.to_string(), style: Style::default() }],
                indent: 0,
            }];
            let mut all: Vec<(String, bool)> = crate::markdown::wrap(&para, max_inner, Style::default())
                .into_iter()
                .map(|l| (l.into_iter().map(|s| s.text).collect::<String>(), true))
                .collect();
            if all.len() > n {
                all.truncate(n);
                // (cut: says so)
                if let Some(last) = all.last_mut() {
                    last.0 = fit_ellipsis(&format!("{}…", last.0), max_inner);
                }
            }
            all
        };
        let (mut lines, labels) = match &o.state {
            OfferState::Asking => {
                let mut b = vec![('y', "Install"), ('n', "Not now")];
                match o.what {
                    crate::offer::What::Install { .. } => b.push(('N', "Never")),
                    crate::offer::What::PluginPermissions { .. } => {
                        b = vec![('y', "Allow"), ('n', "Not now")]
                    }
                    crate::offer::What::PluginAsk { .. } => b = vec![('y', "Allow"), ('n', "No")],
                    crate::offer::What::Run { .. } => b = vec![('y', "Run"), ('n', "No")],
                    _ => {}
                }
                // A command is shown whole (`tools::COMMAND_MAX` keeps it to a card's height)
                let most = if matches!(o.what, crate::offer::What::Run { .. }) { 24 } else { 4 };
                let mut v = vec![(title, false)];
                for part in o.question.split('\n') {
                    v.extend(wrapped(part, most));
                }
                (v, Some(b))
            }
            OfferState::Installing(t) => {
                (vec![(title, false), (format!("{} {}", wave(*t), o.working), true)], None)
            }
            OfferState::Failed(why) => {
                let mut v = vec![(o.failed.clone(), false)];
                v.extend(wrapped(why, 3));
                (v, Some(vec![('y', "Retry"), ('n', "Dismiss")]))
            }
        };
        let badge_w = if badge.is_empty() { 0 } else { badge.width() + 2 };
        for (i, l) in lines.iter_mut().enumerate() {
            l.0 = fit_ellipsis(&l.0, max_inner.saturating_sub(if i == 0 { badge_w } else { 0 }));
        }
        let labels = labels.unwrap_or_default();
        let has_buttons = !labels.is_empty();
        // Button row: "y Install   n Not now" — each "k label", three spaces between
        let btn_w =
            labels.iter().map(|(_, l)| 2 + l.width()).sum::<usize>() + 3 * labels.len().saturating_sub(1);
        // Keep card width steady across states (ask → downloading → failed): ask text width is the floor
        // The longer of the ask and downloading texts (including the four wave cells)
        let ask = o.question.split('\n').map(|l| l.width()).max().unwrap_or(0).max(o.working.width() + 4);
        let inner = lines
            .iter()
            .enumerate()
            .map(|(i, l)| l.0.width() + if i == 0 { badge_w } else { 0 })
            .max()
            .unwrap_or(0)
            .max(if has_buttons { btn_w } else { 0 })
            .max(ask.min(max_inner));
        let width = inner + 5;
        // Too narrow for it: not drawn (and an install's `y` isn't taken — `offer_key`)
        if width + 1 > lay.edit_w {
            return None;
        }
        let h = lines.len() + if has_buttons { 2 } else { 0 } + 2 * pad;
        let top = lay.text_top as usize;
        let bottom = top + lay.text_rows;
        let y = bottom.checked_sub(h + 1)?.max(top);
        let x = lay.edit_w.saturating_sub(width + 1 + usize::from(lay.scrollbar));
        // Back cards: one row each above the front card (as room allows, max three — beyond that "1/N" tells)
        let room = y.saturating_sub(top + pad);
        let peeks = editor.offers[1..]
            .iter()
            .take(3.min(room))
            .enumerate()
            .filter(|(k, _)| width > 2 * PEEK_INSET * (k + 1) + 8)
            .map(|(k, p)| {
                let started = match p.state {
                    OfferState::Installing(t) => Some(t),
                    _ => None,
                };
                (k + 1, p.title.clone(), started, matches!(p.state, OfferState::Failed(_)))
            })
            .collect::<Vec<_>>();
        let buttons_row = has_buttons.then_some(y + pad + lines.len() + 1);
        let mut buttons = Vec::new();
        if let Some(row) = buttons_row {
            let mut at = x + 3;
            for (k, l) in &labels {
                let end = at + 2 + l.width();
                buttons.push((row, at, end, *k));
                at = end + 3;
            }
        }
        for (k, ..) in &peeks {
            let px = x + PEEK_INSET * k;
            buttons.push((y - k, px, px + width - 2 * PEEK_INSET * k, char::from(b'0' + *k as u8)));
        }
        let failed = matches!(o.state, OfferState::Failed(_));
        let busy = o.installing();
        Some(Self { x, y, inner, lines, buttons_row, buttons, labels, pad, failed, badge, busy, peeks })
    }

    /// Where it sits, back cards included — the notification card keeps out of it.
    fn rect(&self) -> Rect4 {
        let rows = self.lines.len() + if self.buttons_row.is_some() { 2 } else { 0 } + 2 * self.pad;
        let back = self.peeks.len();
        (self.x, self.y - back, self.inner + 5, rows + back)
    }

    fn draw(&self, editor: &Editor, ui: &Ui, out: &mut impl Write) -> io::Result<()> {
        let card = card_style(ui, editor, "ui.popup");
        let body = Style { fg: card.fg, bg: card.bg, ..Style::default() };
        let dim = Style { fg: ui.virt.fg.or(card.fg), bg: card.bg, ..Style::default() };
        let bar = Style {
            fg: if self.failed { ui.error.fg } else { ui.accent.fg },
            bg: card.bg,
            ..Style::default()
        };
        let strong = Style { bold: true, ..body };
        let key = Style { fg: ui.accent.fg, bold: true, ..body };
        let width = self.inner + 5;
        // Back cards first (deeper = narrower, dimmer) — one title row each, the topmost up to the top edge
        let peek_bg = |k: usize| blend(card.bg, ui.tint, 1.0 - 0.3 * k as f32).or(card.bg);
        for (k, title, started, failed) in self.peeks.iter().rev() {
            let (px, pw, py) = (self.x + PEEK_INSET * k, width - 2 * PEEK_INSET * k, self.y - k);
            let bg = peek_bg(*k);
            let text = Style { fg: blend(card.fg, bg, 0.5).or(card.fg), bg, ..Style::default() };
            let mark = Style {
                fg: blend(if *failed { ui.error.fg } else { ui.accent.fg }, bg, 0.6),
                bg,
                ..Style::default()
            };
            if self.pad > 0 && *k == self.peeks.len() {
                apply(out, Style { fg: bg, bg: ui.base.bg, ..Style::default() })?;
                queue!(out, MoveTo(px as u16, (py - 1) as u16), Print("▄".repeat(pw)))?;
            }
            let tail = started.map(|t| format!(" {}", wave(t))).unwrap_or_default();
            let room = pw.saturating_sub(5 + tail.width());
            let t = fit_ellipsis(title, room);
            queue!(out, MoveTo(px as u16, py as u16))?;
            apply(out, text)?;
            queue!(out, Print(" "))?;
            apply(out, mark)?;
            queue!(out, Print("▎"))?;
            apply(out, text)?;
            queue!(out, Print(" "), Print(&t), Spaces(pw.saturating_sub(3 + t.width() + tail.width())))?;
            apply(out, mark)?;
            queue!(out, Print(&tail))?;
        }
        // Front card's top edge: over the back card's color where there is one (so they look stacked)
        if self.pad > 0 {
            let behind =
                self.peeks.first().map(|(k, ..)| (self.x + PEEK_INSET * k, width - 2 * PEEK_INSET * k));
            queue!(out, MoveTo(self.x as u16, self.y as u16))?;
            for col in 0..width {
                let under = behind.is_some_and(|(bx, bw)| (bx..bx + bw).contains(&(self.x + col)));
                let bg = if under { peek_bg(1) } else { ui.base.bg };
                apply(out, Style { fg: card.bg, bg, ..Style::default() })?;
                queue!(out, Print("▄"))?;
            }
        }
        let mut row = self.y + self.pad;
        for (i, (text, faint)) in self.lines.iter().enumerate() {
            queue!(out, MoveTo(self.x as u16, row as u16))?;
            apply(out, body)?;
            queue!(out, Print(" "))?;
            apply(out, bar)?;
            queue!(out, Print(if i == 0 { "▎" } else { " " }))?;
            apply(out, body)?;
            queue!(out, Print(" "))?;
            let badge = if i == 0 { self.badge.as_str() } else { "" };
            apply(
                out,
                if *faint {
                    dim
                } else if i == 0 {
                    strong
                } else {
                    body
                },
            )?;
            match text.split_once(' ').filter(|_| self.busy && i == 1) {
                // Downloading: wave in accent, text dimmed
                Some((w, rest)) => {
                    apply(out, Style { fg: ui.accent.fg, ..body })?;
                    queue!(out, Print(w))?;
                    apply(out, dim)?;
                    queue!(out, Print(" "), Print(rest))?;
                }
                None => queue!(out, Print(text))?,
            }
            apply(out, body)?;
            queue!(out, Spaces(self.inner - text.width() - badge.width()))?;
            apply(out, dim)?;
            queue!(out, Print(badge))?;
            apply(out, body)?;
            queue!(out, Print("  "))?;
            row += 1;
        }
        if self.buttons_row.is_some() {
            apply(out, body)?;
            queue!(out, MoveTo(self.x as u16, row as u16), Spaces(width))?;
            row += 1;
            queue!(out, MoveTo(self.x as u16, row as u16))?;
            apply(out, body)?;
            queue!(out, Print("   "))?;
            let mut used = 3;
            for (i, (k, l)) in self.labels.iter().enumerate() {
                if i > 0 {
                    apply(out, body)?;
                    queue!(out, Print("   "))?;
                    used += 3;
                }
                apply(out, key)?;
                queue!(out, Print(k))?;
                apply(out, if i == 0 { body } else { dim })?;
                queue!(out, Print(" "), Print(l))?;
                used += 2 + l.width();
            }
            apply(out, body)?;
            queue!(out, Spaces(width.saturating_sub(used)))?;
            row += 1;
        }
        card_edge(out, ui, card, self.x, row, width, self.pad, false)
    }
}

/// which-key card: after a prefix key (`space`·`g`·`m`), shows the next possible keys and meanings at the
/// bottom right of the editing area — usable without memorizing keys. Many → multiple columns. Submodes: `…`.
fn draw_which_key(editor: &Editor, ui: &Ui, lay: &Layout, out: &mut impl Write) -> io::Result<()> {
    let entries = editor.keymaps.children(editor.mode, &editor.pending);
    if entries.is_empty() {
        return Ok(());
    }
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let key_style = Style { fg: ui.accent.fg, bold: true, ..card };
    let dim = Style { fg: ui.virt.fg, ..card };
    let pending: Vec<String> = editor.pending.iter().map(|k| k.to_string()).collect();
    // The group's name (`space g` → git); the first key's otherwise
    let title = match crate::keymap::group_name(&pending.join(" ")) {
        Some(name) => name.to_lowercase(),
        None => match pending.first().map(String::as_str) {
            Some("space") => "space",
            Some("z") | Some("Z") => "view",
            _ => "keys",
        }
        .to_string(),
    };
    let title = title.as_str();
    let kw = entries.iter().map(|(k, _, _)| k.to_string().width()).max().unwrap_or(1);
    const DESC: usize = 30;
    let dw = entries.iter().map(|(_, d, _)| d.width()).max().unwrap_or(0).min(DESC);
    let cell = kw + 2 + dw;
    let max_rows = lay.text_rows.saturating_sub(2 * pad + 3).max(1);
    let cols = entries.len().div_ceil(max_rows).min((lay.edit_w.saturating_sub(4) / (cell + 3)).max(1));
    let rows = entries.len().div_ceil(cols);
    let inner = cols * cell + (cols - 1) * 3;
    let width = inner + 4;
    let h = rows + 2 + 2 * pad;
    if h > lay.text_rows || width >= lay.edit_w {
        return Ok(());
    }
    let x0 = lay.edit_w - width - 1;
    let y0 = lay.text_top as usize + lay.text_rows - h;
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    // Title: pressed keys · mode name
    let y = y0 + pad;
    queue!(out, MoveTo(x0 as u16, y as u16))?;
    apply(out, card)?;
    queue!(out, Print("  "))?;
    apply(out, Style { bold: true, ..card })?;
    let head = pending.join(" ");
    queue!(out, Print(&head))?;
    apply(out, dim)?;
    let rest = if head == title { String::new() } else { format!("  {title}") };
    queue!(out, Print(&rest), Spaces((inner + 2).saturating_sub(head.width() + rest.width())))?;
    queue!(out, MoveTo(x0 as u16, (y + 1) as u16))?;
    apply(out, card)?;
    queue!(out, Spaces(width))?;
    for r in 0..rows {
        queue!(out, MoveTo(x0 as u16, (y + 2 + r) as u16))?;
        apply(out, card)?;
        queue!(out, Print("  "))?;
        for c in 0..cols {
            let i = c * rows + r;
            let used = match entries.get(i) {
                Some((k, d, group)) => {
                    let ks = k.to_string();
                    apply(out, key_style)?;
                    queue!(out, Print(format!("{ks:<kw$}  ")))?;
                    let d = fit_ellipsis(d, dw);
                    apply(out, if *group { dim } else { card })?;
                    queue!(out, Print(&d))?;
                    kw + 2 + d.width()
                }
                None => 0,
            };
            apply(out, card)?;
            let gap = if c + 1 < cols { 3 } else { 0 };
            queue!(out, Spaces(cell - used + gap))?;
        }
        queue!(out, Print("  "))?;
    }
    card_edge(out, ui, card, x0, y + 2 + rows, width, pad, false)
}

/// How long something took (seconds), the one way it's written: `412ms` under a second, else `1.2s`. Takes
/// the plain number — test reports can say anything (`Infinity`), and a `Duration` would panic on it.
fn duration(s: f32) -> String {
    if s < 1.0 { format!("{:.0}ms", s * 1000.0) } else { format!("{s:.1}s") }
}

/// Waiting animation: a three-cell equalizer wave (`▂▅▇` …) — only block chars in common monospace fonts.
fn wave(since: std::time::Instant) -> String {
    const W: [char; 10] = ['▂', '▃', '▄', '▅', '▆', '▇', '▆', '▅', '▄', '▃'];
    let f = (since.elapsed().as_millis() / crate::editor::TICK.as_millis()) as usize;
    (0..3).map(|k| W[(f + k * 3) % W.len()]).collect()
}

/// While an ask answer streams: last few lines being written, in a card at the editing area's bottom right
/// (in that language's colors). Editing isn't blocked — the card covers only the corner.
fn draw_ask_stream(
    rev: &crate::llm::Review,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    const ROWS: usize = 6;
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let lang = editor
        .docs
        .iter()
        .find(|d| d.id == rev.doc_id)
        .and_then(|d| d.syntax.as_ref())
        .map(|s| s.lang.name.clone());
    let preview = crate::llm::preview(&rev.streamed, ROWS);
    let code = crate::markdown::code_lines(&preview.join("\n"), lang.as_deref(), &editor.theme);
    let inner = (lay.edit_w / 2).clamp(30, 72).min(lay.edit_w.saturating_sub(4));
    let body = if rev.streamed.is_empty() { 0 } else { code.len().min(ROWS) };
    let h = 1 + body + 2 * pad;
    if lay.text_rows < h + 1 {
        return Ok(());
    }
    let (x0, y0) = (lay.edit_w.saturating_sub(inner + 5), lay.text_top as usize + lay.text_rows - h);
    let width = inner + 4;
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    // Title: wave · status · instruction ……… elapsed
    let y = y0 + pad;
    let secs = format!("{:>4.1}s", rev.started.elapsed().as_secs_f32());
    let phase = if rev.thinking || rev.streamed.is_empty() { "thinking" } else { "writing" };
    queue!(out, MoveTo(x0 as u16, y as u16))?;
    apply(out, card)?;
    queue!(out, Print("  "))?;
    apply(out, Style { fg: ui.accent.fg, ..card })?;
    queue!(out, Print(wave(rev.started)))?;
    apply(out, Style { bold: true, ..card })?;
    queue!(out, Print(format!(" claude {phase}")))?;
    let instr = fit_ellipsis(
        &format!("  “{}”", rev.instruction),
        // One row = margin 2 + wave 3 + " claude status" + instruction + gap + elapsed + margin 2 = inner + 4
        (inner + 4).saturating_sub(2 + 3 + 8 + phase.len() + 1 + secs.len() + 2),
    );
    apply(out, Style { fg: ui.virt.fg, ..card })?;
    queue!(out, Print(&instr))?;
    let used = 2 + 3 + 8 + phase.len() + instr.width();
    queue!(out, Spaces((inner + 4).saturating_sub(used + secs.len() + 2)))?;
    apply(out, Style { fg: ui.linenr.fg, ..card })?;
    queue!(out, Print(&secs), Print("  "))?;
    for (i, line) in code.iter().take(body).enumerate() {
        queue!(out, MoveTo(x0 as u16, (y + 1 + i) as u16))?;
        apply(out, card)?;
        queue!(out, Print("  "))?;
        let mut used = 0;
        if let crate::markdown::Line::Code(spans) = line {
            for sp in spans {
                let t = fit(&sp.text, inner - used);
                apply(out, card.patch(sp.style))?;
                queue!(out, Print(&t))?;
                used += t.width();
            }
        }
        // End of last line = where it's writing
        if i + 1 == body && used < inner {
            apply(out, Style { fg: ui.accent.fg, ..card })?;
            queue!(out, Print("▍"))?;
            used += 1;
        }
        apply(out, card)?;
        queue!(out, Spaces(inner - used + 2))?;
    }
    card_edge(out, ui, card, x0, y + 1 + body, width, pad, false)
}

/// Follow mode: what Claude is doing and its latest words, in a card beside the code it's reading —
/// under the lines read if there's room, else above them, else at the bottom of the focused pane.
///
/// ```text
///      ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
///        ● reading  src/llm.rs  L230–249      3.2s
///        The chat swaps the empty tool list here;
///        ask never calls it, so it stays tool-less.
///      ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
/// ```
fn draw_thought_card(
    c: &crate::chat::Chat,
    editor: &Editor,
    ui: &Ui,
    taken: &[Rect4],
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::chat::Role;
    use crate::markdown::Span;
    const BODY: usize = 6;
    let Some((_, rect, _, rows)) = editor.screen.panes.iter().find(|p| p.0 == editor.focus) else {
        return Ok(());
    };
    let title = usize::from(editor.views.len() > 1);
    let (px, py, pw, ph) = (rect.x, rect.y + title, rect.w, rect.h.saturating_sub(title));
    let inner = (pw * 2 / 3).clamp(36, 64).min(pw.saturating_sub(6));
    if inner < 20 {
        return Ok(());
    }
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let accent = Style { fg: ui.accent.fg, ..card };
    let string = card.patch(editor.theme.try_get("string").unwrap_or(dim));
    // Header: what it's doing
    let gaze = c.follow.gaze.as_ref();
    let mut head: Vec<Span> = match gaze {
        Some(g) => {
            let verb = match g.look.verb {
                "read" => "reading",
                "search" => "searching",
                "find" => "finding",
                "note" => "pinning a note",
                "reply" => "replying on",
                "show" => "showing",
                "look" => "looking at",
                "test" => "testing",
                "run" => "running",
                "plan" => "planning",
                "edit" => "editing",
                "write" => "writing",
                "failed" => "test failed",
                _ => "using",
            };
            // The target first; the range/folder gets what's left (dropped if that's too little)
            let room = inner.saturating_sub(2 + verb.len() + 2 + 8);
            let what =
                if g.look.pattern { fit_ellipsis(&g.look.what, room) } else { fit_left(&g.look.what, room) };
            let left = room.saturating_sub(what.width() + 2);
            // A note's words are in its own card
            let range = match g.look.range.width() {
                _ if matches!(g.look.verb, "note" | "reply") => String::new(),
                0 => String::new(),
                n if n <= left => format!("  {}", g.look.range),
                _ if left >= 8 => format!("  {}", fit_left(&g.look.range, left)),
                _ => String::new(),
            };
            // A failure speaks like the test pane's: `▲` in the error color
            let glyph = match g.look.verb {
                "failed" => Span { text: "▲ ".into(), style: Style { fg: ui.error.fg, ..card } },
                _ => Span { text: "● ".into(), style: accent },
            };
            vec![
                glyph,
                Span { text: format!("{verb}  "), style: Style { bold: true, ..dim } },
                Span { text: what, style: if g.look.pattern { string } else { card } },
                Span { text: range, style: faint },
            ]
        }
        None => vec![
            Span { text: wave(c.started), style: accent },
            Span { text: " thinking".into(), style: Style { bold: true, ..dim } },
        ],
    };
    // Working: how long · a walk-through after the answer: which stop
    let secs = match gaze.and_then(|g| g.step) {
        Some((k, n)) if !c.busy() => format!("{k}/{n}"),
        _ => format!("{:>4.1}s", c.started.elapsed().as_secs_f32()),
    };
    let used: usize = head.iter().map(|s| s.text.width()).sum();
    head.push(Span { text: " ".repeat(inner.saturating_sub(used + secs.len())), style: card });
    head.push(Span { text: secs, style: faint });
    // Body: this turn's words as a running log — its thinking (italic) and what it said between lookups, the
    // newest at the bottom. A new thought goes under the last one instead of taking its place (thinking
    // comes in bursts: a lone "I should" for seconds, the whole thought before it gone, read as flicker)
    let turn = c.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
    let why = gaze.and_then(|g| g.why.as_deref()).filter(|w| !w.is_empty());
    let lang = editor.doc().syntax.as_ref().map(|s| s.lang.name.clone());
    let body: Vec<Vec<Span>> = match why {
        // What it's showing, in its words (until it moves on)
        Some(w) => thought_rows(w, inner, card, faint, editor),
        None => {
            let mut body: Vec<Vec<Span>> = Vec::new();
            let voices =
                c.msgs[turn..].iter().rev().filter(|m| matches!(m.role, Role::Thought | Role::Assistant));
            // (newest first, only as far back as the card shows)
            for m in voices {
                let mut rows = if m.role == Role::Thought {
                    thought_rows(&m.text, inner, dim, faint, editor)
                } else {
                    let text = crate::chat::displayed(&m.text);
                    let rendered = crate::markdown::render(text.trim(), &editor.theme, lang.as_deref());
                    chat_markdown(&rendered, inner, (faint, dim), ui.base.bg)
                };
                while rows.last().is_some_and(|r| blank(r)) {
                    rows.pop();
                }
                if rows.is_empty() {
                    continue;
                }
                if !body.is_empty() {
                    rows.push(Vec::new());
                }
                rows.append(&mut body);
                body = rows;
                if body.len() > BODY {
                    break;
                }
            }
            body
        }
    };
    // A walk-through's stop: how to ask about it, under its words
    // (only once the answer is done — a question can't go while Claude still writes)
    let asking = !c.busy()
        && gaze.is_some_and(|g| g.step.is_some() && why.is_some())
        && crate::follow::stop_here(editor).is_some()
        && !crate::codereview::drafting(editor);
    let hint = asking.then(|| {
        let key = Style { bold: true, ..dim };
        vec![
            Span { text: "space n".into(), style: key },
            Span { text: " ask about this".into(), style: faint },
        ]
    });
    // The newest rows; cut mid-way → `…` in front, and no blank row on top
    let body_max = BODY - usize::from(hint.is_some());
    let from = log_cut(&body, body_max);
    let mut body = body[from..].to_vec();
    if from > 0
        && let Some(first) = body.first_mut()
    {
        first.insert(0, Span { text: "… ".into(), style: faint });
    }
    body.extend(hint);
    let full_h = 1 + body.len() + 2 * pad;
    let min_h = 1 + body.len().min(2) + 2 * pad;
    if ph < min_h + 2 {
        return Ok(());
    }
    let width = inner + 4;
    let x0 = (px + pw).saturating_sub(width + 1);
    // Rows other cards hold in this card's columns
    let blocks: Vec<(usize, usize)> =
        taken.iter().filter(|r| r.0 < x0 + width && x0 < r.0 + r.2).map(|r| (r.1, r.1 + r.3)).collect();
    let free =
        |y: usize, h: usize| y >= py && y + h <= py + ph && blocks.iter().all(|&(a, b)| y + h <= a || y >= b);
    // Where: above the first line read (the context before it — shrinks to the room there) · under the
    // lines read · the bottom of the pane · its top. Nowhere free → not drawn this frame.
    let top0 = rect.y + title;
    let row_y = |line: usize| rows.iter().position(|r| r.0 == line).map(|r| top0 + r);
    let lines =
        gaze.filter(|g| g.path.is_some() && g.path == editor.doc().path).and_then(|g| g.lines.clone());
    let mut placed: Option<(usize, usize)> = None;
    if let Some(l) = lines {
        if let Some(start) = row_y(l.start) {
            let ceiling = blocks.iter().filter(|b| b.0 < start).map(|b| b.1).max().unwrap_or(0).max(py);
            if start >= ceiling + min_h {
                let h = full_h.min(start - ceiling);
                placed = Some((start - h, h));
            }
        }
        let below = rows.iter().rposition(|r| l.contains(&r.0)).map(|r| top0 + r + 1);
        if placed.is_none()
            && let Some(y) = below.filter(|&y| free(y, full_h))
        {
            placed = Some((y, full_h));
        }
    }
    let bottom = (py + ph).saturating_sub(full_h);
    let placed = placed.or_else(|| [bottom, py].into_iter().find(|&y| free(y, full_h)).map(|y| (y, full_h)));
    let Some((y0, h)) = placed else { return Ok(()) };
    editor.screen.thought_card.set(Some((x0, y0, width, h)));
    // Shrunk: keep the newest rows
    let body = &body[body.len() - (h - 1 - 2 * pad)..];
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    let mut y = y0 + pad;
    for row in std::iter::once(&head[..]).chain(body.iter().map(|r| &r[..])) {
        queue!(out, MoveTo(x0 as u16, y as u16))?;
        apply(out, card)?;
        queue!(out, Print("  "))?;
        let used = chat_spans(out, row, card, inner)?;
        apply(out, card)?;
        queue!(out, Spaces(inner.saturating_sub(used) + 2))?;
        y += 1;
    }
    card_edge(out, ui, card, x0, y, width, pad, false)
}

/// List rows of the side panel: all but the header, a gap under it, and the gap and two key rows at the
/// bottom.
fn sidebar_list_rows(lay: &Layout) -> usize {
    lay.text_rows.saturating_sub(5).max(1)
}

/// The side panel, left of the code — a card like the review rail (edge `▌` facing the code): tabs `files
/// outline` (the bar accent while it has the keys), the rows as the pickers draw them (folders pinned over
/// the top), the keys at the bottom.
///
/// ```text
///   ▎ files  outline       tarae▌
///                               ▌
///     − src/                    ▌
///     │ ▎ editor.rs             ▌
///     │   filetree.rs           ▌
///     + tests/                1 ▌
///                               ▌
///     enter open · tab outline  ▌
///     esc code · q close        ▌
/// ```
fn draw_sidebar(
    sb: &crate::sidebar::Sidebar,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::markdown::Span;
    use crate::sidebar::Mode as SMode;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let faint = Style { fg: ui.linenr.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let selected = selected_row(ui, editor, card);
    let key = Style { bold: true, fg: ui.virt.fg, ..card };
    let edge = Style { fg: card.bg, bg: ui.base.bg, ..Style::default() };
    let sp = |text: &str, style: Style| Span { text: text.to_string(), style };
    let w = lay.rail_w - 3; // margin 2 · edge 1
    let lines = sb.lines();
    let list_rows = sidebar_list_rows(lay);
    let shown = sb.shown_rows(list_rows, lines.len());
    // Unfocused, only the row the editor is on is marked (nothing, on a file outside the list)
    let mark = if sb.focus { Some(sb.selected) } else { sb.here(editor) };
    // Header: the views as tabs (the list's only when there is one), the one showing strong; what it's of
    // at the right
    let tab_style = |m: SMode| if sb.mode == m { strong } else { faint };
    let side = editor.plugins.as_ref().and_then(|p| p.side.as_ref()).map(|(_, s)| s);
    let has_list = editor.qlist.as_ref().is_some_and(|l| !l.items.is_empty());
    let of = match sb.mode {
        SMode::Files => sb.root_name(),
        SMode::Outline => editor.doc().display_name_short(),
        SMode::List => editor.qlist.as_ref().map(|l| l.title.clone()).unwrap_or_default(),
        SMode::Git => editor.git_status.as_ref().map(|s| s.branch_line()).unwrap_or_default(),
        SMode::Plugin => side.map(|s| s.note.clone()).unwrap_or_default(),
    };
    let bar = Style { fg: if sb.focus { ui.accent.fg } else { ui.linenr.fg }, ..card };
    let mut head = vec![
        sp("▎ ", bar),
        sp("files", tab_style(SMode::Files)),
        sp("  ", card),
        sp("outline", tab_style(SMode::Outline)),
    ];
    if has_list || sb.mode == SMode::List {
        head.push(sp("  ", card));
        head.push(sp("list", tab_style(SMode::List)));
    }
    head.push(sp("  ", card));
    head.push(sp("git", tab_style(SMode::Git)));
    if let Some(s) = side {
        head.push(sp("  ", card));
        head.push(sp(&s.title, tab_style(SMode::Plugin)));
    }
    // …only when it fits whole (it's what the tabs already say, in other words)
    let used: usize = head.iter().map(|x| x.text.width()).sum();
    let room = w.saturating_sub(used + 2);
    if of.width() + 2 <= room {
        head.push(sp(&pad_left(&of, room), faint));
    }
    let tab = match sb.mode {
        SMode::Files => " outline",
        SMode::Outline if has_list => " list",
        SMode::Outline | SMode::List => " git",
        SMode::Git if side.is_some() => " plugin",
        SMode::Git | SMode::Plugin => " files",
    };
    let h = |pairs: &[(&str, &str)]| hint_spans(pairs, key, faint);
    let hints: [Vec<Span>; 2] = if sb.focus && sb.mode == SMode::Git {
        // The selected row's keys: a hunk's, a conflict's, else the tab's
        match lines.get(sb.selected).map(|l| &l.target) {
            Some(crate::sidebar::Target::Hunk { .. }) => {
                [h(&[("s", "stage"), ("d", "discard")]), h(&[("enter", "there"), ("?", "all keys")])]
            }
            Some(crate::sidebar::Target::Conflict { .. }) => {
                [h(&[("o t a", "ours theirs both")]), h(&[("enter", "there"), ("?", "all keys")])]
            }
            _ => [
                h(&[("s u", "stage"), ("d", "discard"), ("?", "")]),
                h(&[("c", "commit"), ("C", "Claude drafts")]),
            ],
        }
    } else if sb.focus && sb.mode == SMode::Plugin {
        // Its keys, then the panel's
        let keys: Vec<(&str, &str)> =
            side.map(|s| s.keys.iter().map(|(k, d)| (k.as_str(), d.as_str())).collect()).unwrap_or_default();
        [h(&keys), h(&[("enter", "go"), ("esc", "code"), ("tab", "files")])]
    } else if sb.focus {
        [h(&[("enter", "open"), ("tab", tab.trim_start())]), h(&[("esc", "code"), ("q", "close")])]
    } else if sb.mode == SMode::Git {
        [h(&[("space g", "git")]), h(&[("space e", "files")])]
    } else {
        [h(&[("space e", "files")]), h(&[("space o", "outline")])]
    };
    let empty = if lines.is_empty() {
        let note = crate::markdown::Line::Text { spans: vec![sp(sb.empty_note(editor), faint)], indent: 0 };
        crate::markdown::wrap(&[note], w, faint)
    } else {
        Vec::new()
    };
    let mut links = editor.screen.sidebar.borrow_mut();
    let mut overflow = None;
    for k in 0..lay.text_rows {
        let y = lay.text_top as usize + k;
        queue!(out, MoveTo(0, y as u16))?;
        apply(out, card)?;
        queue!(out, Print("  "))?;
        let list = k.checked_sub(2).filter(|&i| i < list_rows);
        let used = match (k, list) {
            (0, _) => chat_spans(out, &head, card, w)?,
            (_, Some(i)) => match shown.get(i).map(|&r| (r, &lines[r])) {
                Some((r, l)) => {
                    let row = crate::picker::Row {
                        label: l.label.clone(),
                        marks: Vec::new(),
                        selected: Some(r) == mark,
                        hint: l.hint.clone(),
                        hint_first: sb.mode == SMode::Plugin,
                        glyph: l.glyph,
                        tree: Some((l.depth, l.kind)),
                    };
                    links.push((y, r));
                    // The selected row's name cut short: whole, over the code (drawn after the rest) — with
                    // its state after it, when that's what took the room
                    let state = if row.hint_first && !row.hint.is_empty() { row.hint.width() + 2 } else { 0 };
                    let whole = 4
                        + tree_guides(l.depth).width()
                        + l.label.width()
                        + usize::from(matches!(l.kind, crate::filetree::Kind::Dir { .. }))
                        + state;
                    if row.selected && sb.focus && whole > w - 2 {
                        overflow = Some((y, row.clone(), whole));
                    }
                    // The row's own background runs on to the edge
                    let used = draw_list_row(out, &row, w - 2, editor, ui, card, selected, sb.focus)?;
                    queue!(out, Spaces((w - 2).saturating_sub(used)))?;
                    w
                }
                // Why it's empty, wrapped by word — cut, it read "nothing to commit — the tree"
                None if lines.is_empty() => match empty.get(i) {
                    Some(row) => chat_spans(out, row, card, w)?,
                    None => 0,
                },
                None => 0,
            },
            _ if k + 2 == lay.text_rows => chat_spans(out, &hints[0], card, w)?,
            _ if k + 1 == lay.text_rows => chat_spans(out, &hints[1], card, w)?,
            _ => 0,
        };
        apply(out, card)?;
        queue!(out, Spaces(w.saturating_sub(used)))?;
        apply(out, edge)?;
        queue!(out, Print("▌"))?;
    }
    // The whole name on its row, running on over the code — ending in the panel's edge, in the row's color
    if let Some((y, row, whole)) = overflow
        && 2 + whole < lay.edit_w
    {
        queue!(out, MoveTo(2, y as u16))?;
        draw_list_row(out, &row, whole + 1, editor, ui, card, selected, true)?;
        apply(out, Style { fg: selected.bg, bg: ui.base.bg, ..Style::default() })?;
        queue!(out, Print("▌"))?;
    }
    Ok(())
}

/// Width of the side panel and the code review's file rail — and how much wider a wide terminal makes it.
const RAIL_W: usize = 32;
const RAIL_MORE: usize = 12;

/// The code review's files, left of the code: what's being compared, then one row per file — `●` viewed /
/// `◦` not, the name (folder faint), `+a −r`, `¶n` comments — the current one on the accent bar; the keys at
/// the bottom. A card like the chat panel, mirrored (its edge `▌` faces the code).
///
/// ```text
///   ▎ review          uncommitted▌
///     4 files  +82 −17           ▌
///                                ▌
///   ▎ ● src/notes.rs   +40 −7    ▌
///     ◦ src/chat.rs    +12 −3 ¶2 ▌
///     ◦ src/edits.rs  +214   new ▌
///                                ▌
///     ]g next change             ▌
///     space n comment            ▌
///     space v s send 2 · q end   ▌
/// ```
/// The review rail's rows above its files (title, counts, viewed, a blank) and the key hints under them
/// (a blank and three rows).
const RAIL_HEAD: usize = 4;
const RAIL_HINTS: usize = 4;

/// Rows the review rail has for its files.
fn review_rail_room(lay: &Layout) -> usize {
    lay.text_rows.saturating_sub(RAIL_HEAD + RAIL_HINTS + 1).max(1)
}

fn draw_review_rail(
    r: &crate::codereview::CodeReview,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::markdown::Span;
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let accent = Style { fg: ui.accent.fg, ..card };
    let strong =
        card.patch(editor.theme.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let plus = Style { fg: ui.diff_plus.fg, ..card };
    let minus = Style { fg: ui.diff_minus.fg, ..card };
    let selected = selected_row(ui, editor, card);
    let key = Style { bold: true, ..dim };
    let w = lay.rail_w - 3; // margin 2 · edge 1
    let sp = |text: String, style: Style| Span { text, style };
    let edge = Style { fg: card.bg, bg: ui.base.bg, ..Style::default() };
    let drafts_in = |f: &crate::codereview::File| {
        editor.notes.list.iter().filter(|n| n.path == f.path && n.thread.iter().any(|e| e.draft)).count()
    };
    let drafts: usize = editor.notes.list.iter().filter(|n| n.thread.iter().any(|e| e.draft)).count();
    let (added, removed): (usize, usize) =
        r.files.iter().fold((0, 0), |(a, b), f| (a + f.added, b + f.removed));
    let n = r.files.len();
    let viewed = r.files.iter().filter(|f| r.viewed(editor, f)).count();
    // Head
    let mut rows: Vec<(Vec<Span>, Style, Option<usize>)> = Vec::new();
    let label = r.base.label();
    let head_room = w.saturating_sub(8);
    rows.push((
        vec![
            sp("▎ ".into(), accent),
            sp("review".into(), strong),
            sp(pad_left(&fit_left(&label, head_room), head_room), faint),
        ],
        card,
        None,
    ));
    let mut summary = vec![sp(format!("  {n} file{}", if n == 1 { "" } else { "s" }), dim)];
    if added > 0 {
        summary.push(sp(format!("  +{added}"), plus));
    }
    if removed > 0 {
        summary.push(sp(format!(" −{removed}"), minus));
    }
    rows.push((summary, card, None));
    rows.push((vec![sp(format!("  {viewed} of {n} viewed"), faint)], card, None));
    // Files under their folders (filetree.rs, in the review's order) — from its scroll (`CodeReview::fit`),
    // the files out of sight counted in the blank rows around them
    let room = review_rail_room(lay);
    let tree = r.rail_rows();
    let files_in = |rows: &[crate::filetree::Row]| rows.iter().filter(|t| t.item.is_some()).count();
    let more = |k: usize, arrow: &str| {
        let text = if k > 0 { vec![sp(format!("  {arrow} {k} more"), faint)] } else { Vec::new() };
        (text, card, None)
    };
    rows.push(more(files_in(&tree[..r.scroll.min(tree.len())]), "↑"));
    // The keys in the rail: its selection is the row marked (else the file being read)
    let marked = r.marked(editor);
    for t in tree.iter().skip(r.scroll).take(room) {
        let piece: String = r.files[t.source].rel.chars().skip(t.span.start).take(t.span.len()).collect();
        let guides = sp(tree_guides(t.depth), faint);
        let Some(i) = t.item else {
            // A folder heading: dim, a faint `/`
            let name = fit_middle(&piece, &[], w.saturating_sub(5 + guides.text.width())).0;
            rows.push((
                vec![
                    sp("  ".into(), card),
                    guides,
                    sp("  ".into(), card),
                    sp(name, dim),
                    sp("/".into(), faint),
                ],
                card,
                None,
            ));
            continue;
        };
        let f = &r.files[i];
        let here = marked == Some(i);
        let bg = if here { selected } else { card };
        let seen = r.viewed(editor, f);
        let mut tail = Vec::new();
        if f.binary {
            tail.push(sp(" binary".into(), Style { fg: ui.linenr.fg, ..bg }));
        } else {
            if f.added > 0 {
                tail.push(sp(format!(" +{}", f.added), Style { fg: ui.diff_plus.fg, ..bg }));
            }
            if f.removed > 0 {
                tail.push(sp(format!(" −{}", f.removed), Style { fg: ui.diff_minus.fg, ..bg }));
            }
        }
        if f.new {
            tail.push(sp(" new".into(), Style { fg: ui.linenr.fg, ..bg }));
        } else if f.gone {
            tail.push(sp(" gone".into(), Style { fg: ui.linenr.fg, ..bg }));
        }
        let k = drafts_in(f);
        if k > 0 {
            tail.push(sp(format!(" ¶{k}"), Style { fg: ui.accent.fg, ..bg }));
        }
        let tail_w: usize = tail.iter().map(|s| s.text.width()).sum();
        let indent = guides.text.width();
        // The file name plain (dimmed once viewed) — its folders are the headings above
        let shown = fit_middle(&piece, &[], w.saturating_sub(4 + indent + tail_w)).0;
        let name_st = if seen { Style { fg: ui.virt.fg, ..bg } } else { bg };
        let mut row = vec![
            sp(if here { "▎ ".into() } else { "  ".into() }, Style { fg: ui.accent.fg, ..bg }),
            sp(guides.text, Style { fg: ui.linenr.fg, ..bg }),
            sp(
                if seen { "● ".into() } else { "◦ ".into() },
                Style { fg: if seen { ui.diff_plus.fg } else { ui.linenr.fg }, ..bg },
            ),
            sp(shown.clone(), name_st),
        ];
        let used = 4 + indent + shown.width();
        row.push(sp(" ".repeat(w.saturating_sub(used + tail_w)), bg));
        row.extend(tail);
        rows.push((row, bg, Some(i)));
    }
    rows.push(more(files_in(tree.get(r.scroll + room..).unwrap_or_default()), "↓"));
    // Keys at the bottom
    let fill = lay.text_rows.saturating_sub(rows.len() + RAIL_HINTS);
    rows.extend(std::iter::repeat_n((Vec::new(), card, None), fill));
    rows.push((Vec::new(), card, None));
    let hint = |pairs: &[(&str, &str)]| {
        let mut row = vec![sp("  ".into(), faint)];
        row.extend(hint_spans(pairs, key, faint));
        (row, card, None)
    };
    if r.focus {
        rows.push(hint(&[("j k", "files"), ("enter", "open")]));
        rows.push(hint(&[("m", "viewed, on to the next")]));
        rows.push(hint(&[("esc", "back to the code")]));
    } else {
        rows.push(hint(&[("]g", "change"), ("space e", "files")]));
        rows.push(hint(&[("space n", "comment")]));
        // `space v` once, its keys after it (`s send 2 · q end`) — the pairs wouldn't fit the rail
        let send = if drafts > 0 {
            vec![
                sp("  space v s".into(), key),
                sp(format!(" send {drafts}"), accent),
                sp(" · q end".into(), faint),
            ]
        } else {
            vec![sp("  space v".into(), key), sp(" m viewed · q end".into(), faint)]
        };
        rows.push((send, card, None));
    }
    let mut links = editor.screen.review_rail.borrow_mut();
    for (k, (row, bg, file)) in rows.iter().take(lay.text_rows).enumerate() {
        let y = lay.text_top as usize + k;
        queue!(out, MoveTo(0, y as u16))?;
        apply(out, *bg)?;
        queue!(out, Print("  "))?;
        let used = chat_spans(out, row, *bg, w)?;
        apply(out, *bg)?;
        queue!(out, Spaces(w.saturating_sub(used)))?;
        apply(out, edge)?;
        queue!(out, Print("▌"))?;
        if let Some(i) = file {
            links.push((y, *i));
        }
    }
    Ok(())
}

/// Chat panel — right of the editing area. Half-block left edge (`▐`) so it floats a bit; accent if focused.
///
/// ```text
/// ▐ claude                ▂▅▇ writing 1.2s
/// ▐ ────────────────────────────────────
/// ▐ main.rs · L12–18                       ← context chip sent
/// ▐ ▎ explain this function                ← my message (accent bar)
/// ▐ It does … (markdown, code in color)▍   ← streaming answer
/// ▐                                  2.1s
/// ▐ ────────────────────────────────────
/// ▐ › input…
/// ▐ enter send · esc editor · C-r apply
/// ```
/// Returns the input cursor position (when focused).
fn draw_chat(
    c: &crate::chat::Chat,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<Option<(u16, u16)>> {
    use crate::chat::{Role, State};
    use crate::markdown::{self as md, Span};
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let accent = Style { fg: ui.accent.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let x0 = lay.edit_w;
    let w = lay.chat_w.saturating_sub(4).max(8); // text width: edge 1 + margin 2 + right margin 1
    let top = lay.text_top as usize;
    let rows = lay.text_rows + lay.debug_h;
    let lang = editor.doc().syntax.as_ref().map(|s| s.lang.name.clone());
    // ── Input (wrapped, up to 5 rows) + cursor position
    let placeholder = format!("Ask about {}…", editor.doc().display_name_short());
    let mut input_rows: Vec<String> = vec![String::new()];
    let (mut cur_row, mut cur_col) = (0, 0);
    let mut col = 0;
    for (b, ch) in c.input.char_indices() {
        if b == c.cursor {
            (cur_row, cur_col) = (input_rows.len() - 1, col);
        }
        let cw = ch.width().unwrap_or(0);
        if ch == '\n' || col + cw > w - 2 {
            input_rows.push(String::new());
            col = 0;
            if ch == '\n' {
                continue;
            }
        }
        input_rows.last_mut().unwrap().push(ch);
        col += cw;
    }
    if c.cursor >= c.input.len() {
        (cur_row, cur_col) = (input_rows.len() - 1, col);
    }
    let first_input = input_rows.len().saturating_sub(5).min(cur_row);
    let input_h = input_rows.len().min(5);
    // ── Claude's plan, pinned under the title (one row when the panel is short) + a divider under it
    let room = rows.saturating_sub(input_h + 4);
    let plan_rows = match &c.plan {
        Some(p) => plan_block(p, w, if room >= 16 { PLAN_ROWS } else { 1 }, card, faint, dim, accent, strong),
        None => Vec::new(),
    };
    let plan_h = plan_rows.len() + usize::from(!plan_rows.is_empty());
    // ── Message rows (filled from the bottom)
    let area = rows.saturating_sub(input_h + 4 + plan_h); // title·divider + plan + divider·hint
    let mut lines: Vec<Vec<Span>> = Vec::new();
    // Rows that are links as a whole (tool calls), parallel to `lines`
    let mut row_links: Vec<Option<crate::chat::Link>> = Vec::new();
    let sp = |text: String, style: Style| Span { text, style };
    let n = c.msgs.len();
    let string = card.patch(t.try_get("string").unwrap_or(dim));
    // The newest tool call is marked while Claude is still at it
    let exploring = c.busy() && c.msgs.last().is_some_and(|m| m.role == Role::Tool);
    for (i, m) in c.msgs.iter().enumerate() {
        row_links.resize(lines.len(), None);
        match m.role {
            Role::User => {
                if !lines.is_empty() {
                    lines.push(Vec::new());
                }
                if !m.chip.is_empty() {
                    lines.push(vec![sp(fit_ellipsis(&m.chip, w), faint)]);
                }
                let para = [md::Line::Text { spans: vec![sp(m.text.clone(), strong)], indent: 0 }];
                for l in md::wrap(&para, w - 2, faint) {
                    let mut row = vec![sp("▎ ".into(), accent)];
                    row.extend(l);
                    lines.push(row);
                }
            }
            Role::Assistant => {
                lines.push(Vec::new());
                let mut cache = m.cache.borrow_mut();
                if !matches!(&*cache, Some((len, cw, _)) if *len == m.text.len() && *cw == w) {
                    let rendered = md::render(&crate::chat::displayed(&m.text), t, lang.as_deref());
                    *cache = Some((m.text.len(), w, chat_markdown(&rendered, w, (faint, dim), ui.base.bg)));
                }
                let body = &cache.as_ref().unwrap().2;
                lines.extend(body.iter().cloned());
                if i + 1 == n && c.state == State::Writing {
                    match lines.last_mut() {
                        Some(l) => l.push(sp("▍".into(), accent)),
                        None => lines.push(vec![sp("▍".into(), accent)]),
                    }
                }
            }
            // Thinking: an aside — faint bar, italic; finished ones fold to two rows
            Role::Thought => {
                lines.push(Vec::new());
                let mut cache = m.cache.borrow_mut();
                if !matches!(&*cache, Some((len, cw, _)) if *len == m.text.len() && *cw == w) {
                    *cache = Some((m.text.len(), w, thought_rows(&m.text, w - 2, dim, faint, editor)));
                }
                let live = i + 1 == n && c.busy();
                let keep = if live { 8 } else { 2 };
                // Folded: the first two rows with words (a bold step title and the start of its text)
                let body: Vec<&Vec<Span>> =
                    cache.as_ref().unwrap().2.iter().filter(|r| live || !blank(r)).collect();
                let shown = if live {
                    &body[body.len().saturating_sub(keep)..]
                } else {
                    &body[..body.len().min(keep)]
                };
                for (k, row) in shown.iter().enumerate() {
                    let mut r = vec![sp("▎ ".into(), faint)];
                    r.extend(row.iter().cloned());
                    if live && k == 0 && body.len() > keep {
                        r.insert(1, sp("… ".into(), faint));
                    }
                    if !live && k + 1 == shown.len() && body.len() > keep {
                        r.push(sp("…".into(), faint));
                    }
                    lines.push(r);
                }
            }
            // `◦ read  src/lib.rs  L10–49` — one quiet row per call; a run of them stays together
            Role::Tool => {
                let Some(look) = &m.look else { continue };
                if i == 0 || c.msgs[i - 1].role != Role::Tool {
                    lines.push(Vec::new());
                }
                let styles = TrailStyles { faint, dim, string, accent };
                lines.push(trail_row(look, exploring && i + 1 == n, w, false, &styles));
                row_links.resize(lines.len(), None);
                *row_links.last_mut().expect("just pushed") = look.link.clone();
            }
            // Elapsed time dimmed at the right end; stopped/error on the left
            Role::Note => {
                let timing = m.text.ends_with('s') && m.text[..m.text.len() - 1].parse::<f32>().is_ok();
                if timing {
                    lines.push(vec![sp(format!("{:>w$}", m.text), faint)]);
                } else {
                    lines.push(vec![sp(fit_ellipsis(&format!("· {}", m.text), w), dim)]);
                }
            }
        }
    }
    let thought_live = c.msgs.last().is_some_and(|m| m.role == Role::Thought);
    if c.state == State::Thinking && !exploring && !thought_live {
        lines.push(Vec::new());
        lines.push(vec![sp(wave(c.started), accent), sp(" thinking…".into(), dim)]);
    }
    // Empty chat: guidance + example questions in the middle
    if c.msgs.is_empty() && !c.busy() {
        let name = editor.doc().display_name_short();
        let mut intro = vec![
            vec![sp("▎ ".into(), accent), sp("Ask Claude".into(), strong)],
            vec![],
            vec![sp(fit_ellipsis("It reads the whole project,", w), dim)],
            vec![sp(fit_ellipsis(&format!("starting from {name}."), w), dim)],
            vec![],
            vec![sp("tab ".into(), Style { bold: true, ..dim }), sp("for a suggestion".into(), faint)],
        ];
        // (the key for the model sits with the other first keys; the title names the one in use)
        let model_key =
            vec![sp("C-t ".into(), Style { bold: true, ..dim }), sp("for another model".into(), faint)];
        for s in crate::chat::SUGGESTIONS {
            intro.push(vec![sp("› ".into(), faint), sp(fit_ellipsis(s, w - 2), dim)]);
        }
        intro.push(vec![]);
        intro.push(model_key);
        // Earlier conversations in this project: the newest two, and the key for all of them
        let past: Vec<_> = c.past.iter().filter(|p| Some(&p.0) != c.session.as_ref()).collect();
        if !past.is_empty() {
            let now = crate::chats::now();
            intro.push(vec![]);
            intro.push(vec![
                sp("C-p ".into(), Style { bold: true, ..dim }),
                sp("to continue an earlier one".into(), faint),
            ]);
            for (_, title, updated) in past.iter().take(2) {
                let when = format!("  {}", crate::chats::ago(*updated, now));
                let first = title.lines().next().unwrap_or_default();
                intro.push(vec![
                    sp("◦ ".into(), faint),
                    sp(fit_ellipsis(first, w.saturating_sub(2 + when.width())), dim),
                    sp(when, faint),
                ]);
            }
        }
        let pad = area.saturating_sub(intro.len()) / 2;
        lines = std::iter::repeat_n(Vec::new(), pad).chain(intro).collect();
    }
    row_links.resize(lines.len(), None);
    let max_scroll = lines.len().saturating_sub(area);
    let scroll = c.scroll.min(max_scroll);
    let from = lines.len().saturating_sub(area + scroll);
    let shown = &lines[from..lines.len() - scroll];
    let mut chat_links = editor.screen.chat_links.borrow_mut();

    // ── Draw row by row: [edge][margin 2][text w][margin 1]
    let edge = Style { fg: card.bg, bg: ui.base.bg, ..Style::default() };
    let mut y = top;
    // Title
    chat_row_start(out, x0, y, edge, card)?;
    apply(out, if c.focused { strong } else { dim })?;
    queue!(out, Print("claude"))?;
    // Follow badge: accent while following, faint once the user took over
    // (paused lasts until the next question — only worth saying while Claude works)
    let paused = c.follow.paused && c.busy();
    let badge = match (c.follow.on, paused) {
        (false, _) => "",
        (true, false) => " › follow",
        (true, true) => " › paused",
    };
    apply(out, if paused { faint } else { accent })?;
    queue!(out, Print(badge))?;
    let title_w = 6 + badge.width();
    // Idle: the model it's talking to (`Opus 5.5 · high`), else "ready"
    let status: Vec<Span> = match c.state {
        State::Idle => match crate::models::label(editor) {
            Some(m) => vec![sp(fit_ellipsis(&m, w.saturating_sub(title_w + 2)), faint)],
            None => vec![sp("ready".into(), faint)],
        },
        s => vec![
            sp(wave(c.started), accent),
            sp(
                format!(
                    " {} {:>4.1}s",
                    if exploring {
                        "exploring"
                    } else if s == State::Thinking {
                        "thinking"
                    } else {
                        "writing"
                    },
                    c.started.elapsed().as_secs_f32()
                ),
                dim,
            ),
        ],
    };
    let sw: usize = status.iter().map(|s| s.text.width()).sum();
    apply(out, card)?;
    queue!(out, Spaces(w.saturating_sub(title_w + sw)))?;
    let used = title_w + w.saturating_sub(title_w + sw) + chat_spans(out, &status, card, w)?;
    chat_row_end(out, card, lay.chat_w, used)?;
    y += 1;
    chat_row_start(out, x0, y, edge, card)?;
    apply(out, faint)?;
    queue!(out, Print("─".repeat(w)))?;
    chat_row_end(out, card, lay.chat_w, w)?;
    y += 1;
    if !plan_rows.is_empty() {
        for row in &plan_rows {
            chat_row_start(out, x0, y, edge, card)?;
            let used = chat_spans(out, row, card, w)?;
            chat_row_end(out, card, lay.chat_w, used)?;
            y += 1;
        }
        chat_row_start(out, x0, y, edge, card)?;
        apply(out, faint)?;
        queue!(out, Print("─".repeat(w)))?;
        chat_row_end(out, card, lay.chat_w, w)?;
        y += 1;
    }
    let text_x = x0 + 3;
    for i in 0..area {
        chat_row_start(out, x0, y, edge, card)?;
        let used = match shown.get(i) {
            Some(l) => match &row_links[from + i] {
                Some(link) => {
                    chat_links.push((y, text_x, text_x + w, link.clone()));
                    chat_spans(out, l, card, w)?
                }
                // `path:line` in an answer: underlined, clickable
                None => {
                    let (l, found) = underline_refs(l);
                    for (a, b, link) in found {
                        chat_links.push((y, text_x + a, text_x + b, link));
                    }
                    chat_spans(out, &l, card, w)?
                }
            },
            None => 0,
        };
        chat_row_end(out, card, lay.chat_w, used)?;
        y += 1;
    }
    // Input
    chat_row_start(out, x0, y, edge, card)?;
    apply(out, faint)?;
    queue!(out, Print("─".repeat(w)))?;
    chat_row_end(out, card, lay.chat_w, w)?;
    y += 1;
    let input_top = y;
    for (i, r) in input_rows.iter().skip(first_input).take(input_h).enumerate() {
        chat_row_start(out, x0, y, edge, card)?;
        let prompt = if i == 0 { "› " } else { "  " };
        apply(out, if c.focused { Style { bold: true, ..accent } } else { faint })?;
        queue!(out, Print(prompt))?;
        let used = if c.input.is_empty() {
            apply(out, faint)?;
            let p = fit_ellipsis(&placeholder, w - 2);
            queue!(out, Print(&p))?;
            p.width()
        } else {
            apply(out, card)?;
            queue!(out, Print(r))?;
            r.width()
        };
        chat_row_end(out, card, lay.chat_w, 2 + used)?;
        y += 1;
    }
    // Hint
    chat_row_start(out, x0, y, edge, card)?;
    let key = Style { bold: true, ..dim };
    let mut hints: Vec<(&str, &str)> = Vec::new();
    if !c.focused {
        hints.push(("space l", "focus"));
        hints.push(("space L", "close"));
    } else if c.busy() {
        hints.push(("C-c", "stop"));
        hints.push(("esc", "editor"));
        hints.push(("C-f", if c.follow.active() { "unfollow" } else { "follow" }));
    } else {
        let code = c.last_code().is_some();
        hints.push(("enter", "send"));
        hints.push(("esc", "editor"));
        if crate::checkpoint::can_undo(editor) {
            hints.push(("C-z", "undo turn"));
        }
        if code {
            hints.push(("C-r", "apply"));
        }
        if !c.links().is_empty() {
            hints.push(("C-]", "go to"));
        }
        if code {
            hints.push(("C-y", "copy"));
        }
        hints.push(("C-l", "new"));
        hints.push(("C-f", if c.follow.on { "unfollow" } else { "follow" }));
    }
    let mut used = 0;
    for (k, d) in hints {
        let wd = k.width() + d.width() + 3;
        if used + wd > w {
            break;
        }
        apply(out, key)?;
        queue!(out, Print(k))?;
        apply(out, faint)?;
        queue!(out, Print(format!(" {d}  ")))?;
        used += wd;
    }
    chat_row_end(out, card, lay.chat_w, used)?;
    y += 1;
    // Remaining rows (when the panel is taller than the input)
    while y < top + rows {
        chat_row_start(out, x0, y, edge, card)?;
        chat_row_end(out, card, lay.chat_w, 0)?;
        y += 1;
    }
    Ok(c.focused.then(|| {
        let r = cur_row.saturating_sub(first_input).min(input_h - 1);
        ((x0 + 3 + 2 + cur_col) as u16, (input_top + r) as u16)
    }))
}

/// Answer markdown → rows. Code blocks as a "well" of editor bg (sunk a tone inside the card) — text and
/// code separate at a glance. Code is truncated, not wrapped (`…` at the end if cut). Rules in `rule` (faint),
/// a stacked table's header names in `label` (dim — they say what each value is).
fn chat_markdown(
    lines: &[crate::markdown::Line],
    w: usize,
    (rule, label): (Style, Style),
    well: Option<crossterm::style::Color>,
) -> Vec<Vec<crate::markdown::Span>> {
    use crate::markdown::{self as md, Line, Span};
    let mut out = Vec::new();
    for l in lines {
        match l {
            Line::Code(spans) => {
                let full: usize = spans.iter().map(|s| s.text.width()).sum();
                let inner = w.saturating_sub(2);
                let mut row = vec![Span { text: " ".into(), style: Style { bg: well, ..Style::default() } }];
                let mut used = 0;
                for s in spans {
                    let room = inner.saturating_sub(used + usize::from(full > inner));
                    let t = fit(&s.text, room);
                    used += t.width();
                    row.push(Span { text: t, style: Style { bg: well, ..s.style } });
                }
                if full > inner {
                    row.push(Span {
                        text: "…".into(),
                        style: Style { bg: well, fg: rule.fg, ..Style::default() },
                    });
                    used += 1;
                }
                row.push(Span {
                    text: " ".repeat(inner.saturating_sub(used) + 1),
                    style: Style { bg: well, ..Style::default() },
                });
                out.push(row);
            }
            other => out.extend(md::wrap_with(std::slice::from_ref(other), w, rule, label)),
        }
    }
    out
}

/// Colors of a trail row.
struct TrailStyles {
    faint: Style,
    dim: Style,
    string: Style,
    accent: Style,
}

/// Most rows the chat's plan takes (its title and steps).
const PLAN_ROWS: usize = 7;

/// Claude's plan as the chat pins it: its title and how far it is, then the steps around the one in hand —
/// done ones receding (`●` faint), the one in hand in the accent (`›`), the rest `◦`. Finished: the title
/// row alone. `max` 1: the title row carries the step in hand.
#[allow(clippy::too_many_arguments)]
fn plan_block(
    p: &crate::plan::Plan,
    w: usize,
    max: usize,
    card: Style,
    faint: Style,
    dim: Style,
    accent: Style,
    strong: Style,
) -> Vec<Vec<crate::markdown::Span>> {
    use crate::markdown::Span;
    use crate::plan::Status;
    let sp = |text: String, style: Style| Span { text, style };
    let n = p.steps.len();
    let count = if p.finished() { format!("all {n} done") } else { format!("{}/{n}", p.done()) };
    let title = if p.title.is_empty() { "plan".to_string() } else { p.title.clone() };
    // (one row with a step in hand: the title gives it room)
    let title_room = w.saturating_sub(4 + count.width());
    let title_room = if max <= 1 && !p.finished() { title_room.min(w / 3) } else { title_room };
    let mut head = vec![sp("▎ ".into(), accent), sp(fit_ellipsis(&title, title_room), strong)];
    // One row: the step in hand after the title, as far as it fits
    if max <= 1
        && let Some(i) = p.current().filter(|_| !p.finished())
    {
        let used: usize = head.iter().map(|s| s.text.width()).sum();
        let room = w.saturating_sub(used + count.width() + 6);
        if room >= 8 {
            head.push(sp(" › ".into(), accent));
            head.push(sp(fit_ellipsis(&p.steps[i].text, room), dim));
        }
    }
    let used: usize = head.iter().map(|s| s.text.width()).sum();
    head.push(sp(" ".repeat(w.saturating_sub(used + count.width())), card));
    head.push(sp(count, faint));
    let mut out = vec![head];
    if max <= 1 || p.finished() {
        return out;
    }
    let (before, shown, after) = p.window(max - 1);
    if before > 0 {
        out.push(vec![sp(format!("  … {before} done"), faint)]);
    }
    for s in &p.steps[shown] {
        let (glyph, gs, ts) = match s.status {
            Status::Done => ("● ", faint, faint),
            Status::Doing => ("› ", accent, strong),
            Status::Todo => ("◦ ", faint, dim),
        };
        out.push(vec![sp(glyph.into(), gs), sp(fit_ellipsis(&s.text, w.saturating_sub(2)), ts)]);
    }
    if after > 0 {
        out.push(vec![sp(format!("  … {after} more"), faint)]);
    }
    out
}

/// The step in hand as one row (`› 2/5 Wire it into the config`) — the minimized chat's.
fn plan_now(
    p: &crate::plan::Plan,
    w: usize,
    faint: Style,
    accent: Style,
    dim: Style,
) -> Option<Vec<crate::markdown::Span>> {
    use crate::markdown::Span;
    let i = p.current()?;
    let at = format!("{}/{} ", i + 1, p.steps.len());
    Some(vec![
        Span { text: "› ".into(), style: accent },
        Span { text: at.clone(), style: faint },
        Span { text: fit_ellipsis(&p.steps[i].text, w.saturating_sub(2 + at.width())), style: dim },
    ])
}

/// One lookup as a row: `◦ read  src/lib.rs  L10–49`. `slim` (the minimized chat) drops the verb and keeps
/// only the file name — patterns stand apart in string color. `live` = the one in progress (`●`).
fn trail_row(
    look: &crate::chat::Look,
    live: bool,
    w: usize,
    slim: bool,
    st: &TrailStyles,
) -> Vec<crate::markdown::Span> {
    use crate::markdown::Span;
    let sp = |text: String, style: Style| Span { text, style };
    let glyph = if live { sp("●".into(), st.accent) } else { sp("◦".into(), st.faint) };
    let head = if slim { " ".to_string() } else { format!(" {:<6} ", look.verb) };
    let name = match std::path::Path::new(&look.what).file_name() {
        // (only files — a plan's title may have a `/` in it)
        Some(f) if slim && !look.pattern && look.link.is_some() => f.to_string_lossy().into_owned(),
        _ => look.what.clone(),
    };
    let mut range = if look.range.is_empty() { String::new() } else { format!("  {}", look.range) };
    if slim && w.saturating_sub(1 + head.width() + name.width()) < range.width() {
        range.clear();
    }
    // The target first; the suffix (line range, a note's words) keeps at most 10 cells of its room
    let room = w.saturating_sub(1 + head.width() + range.width().min(10)).max(4);
    let what = if look.pattern { fit_ellipsis(&name, room) } else { fit_left(&name, room) };
    let mut row = vec![glyph, sp(head, st.faint), sp(what, if look.pattern { st.string } else { st.dim })];
    let used: usize = row.iter().map(|s| s.text.width()).sum();
    // (a short suffix — a command's `ok` — whenever it fits whole)
    if w.saturating_sub(used) >= range.width().min(6) && !range.is_empty() {
        row.push(sp(fit_ellipsis(&range, w - used), st.faint));
    }
    row
}

/// A card's place: (x, y, width, height).
type Rect4 = (usize, usize, usize, usize);

fn overlaps(a: Rect4, b: Rect4) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// Where the minimized chat sits: the bottom right of the editing area, like a picture-in-picture (the
/// right edge of code is mostly empty; the thought card takes the room above the lines being read) — the
/// top right if a card in `avoid` (the note, an offer) holds that corner, nowhere if they hold both. `None`
/// also on a tiny screen.
fn chat_mini_rect(
    c: &crate::chat::Chat,
    editor: &Editor,
    ui: &Ui,
    lay: &Layout,
    avoid: &[Rect4],
) -> Option<Rect4> {
    const INNER: usize = 32;
    if lay.edit_w < INNER + 30 || lay.text_rows < 12 {
        return None;
    }
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let turn = c.msgs.iter().rposition(|m| m.role == crate::chat::Role::User).unwrap_or(0);
    let trail = c.msgs[turn..]
        .iter()
        .filter(|m| m.look.as_ref().is_some_and(|l| l.verb != "plan"))
        .count()
        .clamp(1, MINI_TRAIL);
    // title · question · plan · trail · hint
    let plan = usize::from(c.plan.as_ref().is_some_and(|p| p.current().is_some()));
    let h = 1 + 1 + plan + trail + 1 + 2 * pad;
    let width = INNER + 4;
    let x = lay.edit_w.saturating_sub(width + 1);
    let (top, bottom) = (lay.text_top as usize, (lay.text_top as usize + lay.text_rows).saturating_sub(h));
    [bottom, top].into_iter().map(|y| (x, y, width, h)).find(|&r| !avoid.iter().any(|&a| overlaps(r, a)))
}

/// Trail rows the minimized chat shows (the newest).
const MINI_TRAIL: usize = 3;

/// The chat minimized while Claude works in follow mode (bottom right) — the thought card beside the code
/// carries its words; this keeps the question, where it has been and how to get the panel back.
///
/// ```text
///   ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
///     claude › follow              ▂▅▇
///     ▎ does ask get tools too?
///     ◦ with_tools  in src
///     ● llm.rs  L287–302
///     C-c stop  space l open
///   ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
/// ```
fn draw_chat_mini(
    c: &crate::chat::Chat,
    editor: &Editor,
    ui: &Ui,
    rect: Rect4,
    out: &mut impl Write,
) -> io::Result<()> {
    use crate::chat::Role;
    use crate::markdown::Span;
    let (x0, y0, width, _) = rect;
    editor.screen.chat_mini.set(Some(rect));
    let inner = width - 4;
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.popup");
    let pad = card_padding(ui, card);
    let dim = Style { fg: ui.virt.fg, ..card };
    let faint = Style { fg: ui.linenr.fg, ..card };
    let accent = Style { fg: ui.accent.fg, ..card };
    let strong = card.patch(t.try_get("ui.text.focus").unwrap_or(Style { bold: true, ..Style::default() }));
    let string = card.patch(t.try_get("string").unwrap_or(dim));
    let sp = |text: String, style: Style| Span { text, style };
    let turn = c.msgs.iter().rposition(|m| m.role == Role::User).unwrap_or(0);
    let mut rows: Vec<(Vec<Span>, Option<crate::chat::Link>)> = Vec::new();
    // Title: claude › follow ……… wave
    let badge = if c.follow.paused { " › paused" } else { " › follow" };
    let wv = wave(c.started);
    let gap = inner.saturating_sub(6 + badge.width() + wv.width());
    rows.push((
        vec![
            sp("claude".into(), if c.focused { strong } else { dim }),
            sp(badge.into(), if c.follow.paused { faint } else { accent }),
            sp(" ".repeat(gap), card),
            sp(wv, accent),
        ],
        None,
    ));
    let question =
        c.msgs.get(turn).map(|m| m.text.lines().next().unwrap_or_default().to_string()).unwrap_or_default();
    rows.push((vec![sp("▎ ".into(), accent), sp(fit_ellipsis(&question, inner - 2), strong)], None));
    // Where the plan is: the step in hand
    if let Some(row) = c.plan.as_ref().and_then(|p| plan_now(p, inner, faint, accent, dim)) {
        rows.push((row, None));
    }
    // (the plan has its own row above)
    let looks: Vec<&crate::chat::Look> =
        c.msgs[turn..].iter().filter_map(|m| m.look.as_ref()).filter(|l| l.verb != "plan").collect();
    let styles = TrailStyles { faint, dim, string, accent };
    let exploring = c.msgs.last().is_some_and(|m| m.role == Role::Tool);
    if looks.is_empty() {
        rows.push((vec![sp("◦ thinking…".into(), faint)], None));
    }
    let skip = looks.len().saturating_sub(MINI_TRAIL);
    for (k, look) in looks.iter().enumerate().skip(skip) {
        let mut row = trail_row(look, exploring && k + 1 == looks.len(), inner, true, &styles);
        // Earlier lookups folded into a count on the oldest row shown
        if k == skip && skip > 0 {
            let more = format!("  +{skip}");
            let used: usize = row.iter().map(|s| s.text.width()).sum();
            if used + more.width() <= inner {
                row.push(sp(more, faint));
            }
        }
        rows.push((row, look.link.clone()));
    }
    let key = Style { bold: true, ..dim };
    let hint: Vec<Span> = if c.focused {
        vec![
            sp("C-c".into(), key),
            sp(" stop  ".into(), faint),
            sp("C-f".into(), key),
            sp(" unfollow".into(), faint),
        ]
    } else {
        vec![sp("space l".into(), key), sp(" open the chat".into(), faint)]
    };
    rows.push((hint, None));
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    let mut links = editor.screen.chat_links.borrow_mut();
    let mut y = y0 + pad;
    for (row, link) in &rows {
        queue!(out, MoveTo(x0 as u16, y as u16))?;
        apply(out, card)?;
        queue!(out, Print("  "))?;
        let used = chat_spans(out, row, card, inner)?;
        apply(out, card)?;
        queue!(out, Spaces(inner.saturating_sub(used) + 2))?;
        if let Some(l) = link {
            links.push((y, x0 + 2, x0 + 2 + inner, l.clone()));
        }
        y += 1;
    }
    card_edge(out, ui, card, x0, y, width, pad, false)
}

/// A row with nothing but spaces.
/// Where a log shown in `max` rows starts: the newest rows — but a cut paragraph's last row alone says little
/// (`… filtering logic.`), so the next paragraph starts it then, and never a blank row.
fn log_cut(body: &[Vec<crate::markdown::Span>], max: usize) -> usize {
    let mut from = body.len().saturating_sub(max);
    let rest = |i: usize| body[i..].iter().position(|r| blank(r)).unwrap_or(body.len() - i);
    if from > 0 && !blank(&body[from - 1]) && rest(from) == 1 && from + 1 < body.len() {
        from += 1;
    }
    while from < body.len() && blank(&body[from]) {
        from += 1;
    }
    from
}

fn blank(row: &[crate::markdown::Span]) -> bool {
    row.iter().all(|s| s.text.trim().is_empty())
}

/// Thinking summary → rows: italic in the dim tone (bold kept — summaries title their steps in bold).
fn thought_rows(
    text: &str,
    w: usize,
    dim: Style,
    faint: Style,
    editor: &Editor,
) -> Vec<Vec<crate::markdown::Span>> {
    let rendered = crate::markdown::render(text.trim(), &editor.theme, None);
    chat_markdown(&rendered, w, (faint, dim), None)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|s| crate::markdown::Span {
                    style: Style { italic: !s.style.bold, bold: s.style.bold, ..dim },
                    text: s.text,
                })
                .collect()
        })
        .collect()
}

/// A clickable span in the chat pane: (start col, end col, target).
type RefSpot = (usize, usize, crate::chat::Link);

/// Underlines the `path:line` references in one answer row → (row, where they are).
fn underline_refs(
    row: &[crate::markdown::Span],
) -> (std::borrow::Cow<'_, [crate::markdown::Span]>, Vec<RefSpot>) {
    use crate::markdown::Span;
    let joined: String = row.iter().map(|s| s.text.as_str()).collect();
    let refs = crate::chat::refs_in(&joined);
    if refs.is_empty() {
        return (std::borrow::Cow::Borrowed(row), Vec::new());
    }
    let mut out = Vec::new();
    let mut at = 0; // byte offset of the span in `joined`
    for s in row {
        let end = at + s.text.len();
        // Cut the span at every ref edge inside it
        let mut cuts = vec![at, end];
        for (r, _) in &refs {
            cuts.extend([r.start, r.end].into_iter().filter(|&b| b > at && b < end));
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let inside = refs.iter().any(|(r, _)| r.start <= a && b <= r.end);
            out.push(Span {
                text: joined[a..b].to_string(),
                style: Style { underline: s.style.underline || inside, ..s.style },
            });
        }
        at = end;
    }
    let col = |b: usize| joined[..b].width();
    let found = refs.into_iter().map(|(r, link)| (col(r.start), col(r.end), link)).collect();
    (std::borrow::Cow::Owned(out), found)
}

/// `s` right-aligned in `width` cells — by display width (`format!("{:>w$}")` counts chars, so Korean or
/// emoji would overflow).
fn pad_left(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(s.width())))
}

/// Keeps the end (file name) of a path that doesn't fit: `…/editor/mod.rs`. No room at all: nothing (a
/// lone `…` would still take a cell).
fn fit_left(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut w = 1;
    let mut tail: Vec<char> = Vec::new();
    for ch in s.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        w += cw;
        tail.push(ch);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

fn chat_row_start(out: &mut impl Write, x0: usize, y: usize, edge: Style, card: Style) -> io::Result<()> {
    queue!(out, MoveTo(x0 as u16, y as u16))?;
    apply(out, edge)?;
    queue!(out, Print("▐"))?;
    apply(out, card)?;
    queue!(out, Print("  "))
}

fn chat_row_end(out: &mut impl Write, card: Style, chat_w: usize, used: usize) -> io::Result<()> {
    apply(out, card)?;
    queue!(out, Spaces(chat_w.saturating_sub(3 + used)))
}

fn chat_spans(
    out: &mut impl Write,
    spans: &[crate::markdown::Span],
    base: Style,
    w: usize,
) -> io::Result<usize> {
    let mut used = 0;
    for s in spans {
        let text = fit(&s.text, w.saturating_sub(used));
        apply(out, base.patch(s.style))?;
        queue!(out, Print(&text))?;
        used += text.width();
    }
    Ok(used)
}

/// Key hints the one way they're written everywhere: `enter open  esc close` — the key in `key` (bold
/// dim), what it does in `faint`, two cells between pairs. An empty description leaves the key alone.
fn hint_spans(pairs: &[(&str, &str)], key: Style, faint: Style) -> Vec<crate::markdown::Span> {
    use crate::markdown::Span;
    let mut out = Vec::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(Span { text: "  ".into(), style: faint });
        }
        out.push(Span { text: k.to_string(), style: Style { bold: true, ..key } });
        if !v.is_empty() {
            out.push(Span { text: format!(" {v}"), style: faint });
        }
    }
    out
}

/// A list's selected row on `card`: `ui.menu.selected` (else the primary selection's colors) — every list
/// (pickers, completion, `:` menu, side panel, review rail, test results) marks it the same way, with the
/// accent `▎` at its start.
fn selected_row(ui: &Ui, editor: &Editor, card: Style) -> Style {
    card.patch(editor.theme.try_get("ui.menu.selected").unwrap_or(ui.selection_primary))
}

/// Background of floating cards (ui.menu / ui.popup, else the built-in theme's).
fn card_style(ui: &Ui, editor: &Editor, key: &str) -> Style {
    let t = &editor.theme;
    ui.base.patch(
        t.try_get(key)
            .or_else(|| t.try_get("ui.popup"))
            .or_else(|| builtin().try_get(key))
            .unwrap_or_default(),
    )
}

/// For cards (bg differs from the editor), one half-row margin row above and below, else 0.
fn card_padding(ui: &Ui, card: Style) -> usize {
    usize::from(card.bg.is_some() && card.bg != ui.base.bg)
}

/// Card top (`▄`) and bottom (`▀`) edges — half blocks paint only half a row so the margin looks thin.
#[allow(clippy::too_many_arguments)]
fn card_edge(
    out: &mut impl Write,
    ui: &Ui,
    card: Style,
    x: usize,
    y: usize,
    width: usize,
    pad: usize,
    top: bool,
) -> io::Result<()> {
    if pad == 0 {
        return Ok(());
    }
    apply(out, Style { fg: card.bg, bg: ui.base.bg, ..Style::default() })?;
    queue!(out, MoveTo(x as u16, y as u16), Print((if top { "▄" } else { "▀" }).repeat(width)))
}

/// A label with fuzzy-matched chars (`hits` = ascending char indices) in `matched`, up to `room` cells.
/// Returns the cells used.
fn print_matched(
    out: &mut impl Write,
    label: &str,
    hits: &[u32],
    name: Style,
    matched: Style,
    room: usize,
) -> io::Result<usize> {
    let mut hit = hits.iter().peekable();
    let mut current = None;
    let mut used = 0;
    for (ci, ch) in label.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > room {
            break;
        }
        while hit.next_if(|&&h| (h as usize) < ci).is_some() {}
        let want = if hit.peek().is_some_and(|&&h| h as usize == ci) { name.patch(matched) } else { name };
        if current != Some(want) {
            apply(out, want)?;
            current = Some(want);
        }
        queue!(out, Print(ch))?;
        used += cw;
    }
    Ok(used)
}

/// List scrollbar thumb: rows `first..` of `n` shown in `rows` — thumb sized by the visible share.
fn scroll_thumb(rows: usize, n: usize, first: usize) -> std::ops::Range<usize> {
    let len = (rows * rows).div_ceil(n).clamp(1, rows);
    let at = if n > rows { first * (rows - len) / (n - rows) } else { 0 };
    at..at + len
}

/// Truncate to width, ending with '…' if cut — nothing when there's no room at all (the `…` is a cell too).
fn fit_ellipsis(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = fit(s, width.saturating_sub(1));
    out.push('…');
    out
}

/// Floating docs (hover) — below the cursor (above if no room), text's first cell at the cursor column.
fn draw_popup(
    lines: &[crate::markdown::Line],
    at: (u16, u16),
    ui: &Ui,
    editor: &Editor,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    let rect = draw_float(lines, at, editor.popup_scroll, 20, ui, editor, lay, out)?;
    editor.screen.popup.set(rect);
    Ok(())
}

/// Lines in a doc box by the cursor (hover, diagnostic card): at most `cap` rows, scrolled `scroll` rows.
#[allow(clippy::too_many_arguments)]
/// Where a floating card goes, with its wrapped rows — placement only (`draw_float` draws).
struct FloatGeom {
    x0: usize,
    y0: usize,
    inner: usize,
    h: usize,
    max_rows: usize,
    rows: Vec<Vec<crate::markdown::Span>>,
}

impl FloatGeom {
    fn rect(&self) -> Rect4 {
        (self.x0, self.y0, self.inner + 4, self.h)
    }
}

fn float_geom(
    lines: &[crate::markdown::Line],
    (cx, cy): (u16, u16),
    cap: usize,
    ui: &Ui,
    editor: &Editor,
    lay: &Layout,
) -> Option<FloatGeom> {
    float_geom_around(lines, cx, (cy as usize, cy as usize), cap, ui, editor, lay)
}

/// A floating card that keeps screen rows `first..=last` in sight (the lines it's about).
fn float_geom_around(
    lines: &[crate::markdown::Line],
    cx: u16,
    (first, last): (usize, usize),
    cap: usize,
    ui: &Ui,
    editor: &Editor,
    lay: &Layout,
) -> Option<FloatGeom> {
    let natural = lines.iter().map(crate::markdown::Line::width).max().unwrap_or(0);
    let inner = natural.clamp(10, 76).min(lay.edit_w.saturating_sub(4));
    let rows = crate::markdown::wrap(lines, inner, dim_style(ui, editor));
    // Never cover those rows: below if it fits, else above; if neither fits, the larger side, truncated
    let (top, bottom) = (lay.text_top as usize, lay.text_top as usize + lay.text_rows);
    let (below, above) = (bottom.saturating_sub(last + 1), first.saturating_sub(top));
    let want = rows.len().min(cap) + 2;
    let down = want <= below || (want > above && below >= above);
    let room = if down { below } else { above };
    if room < 3 {
        return None;
    }
    let max_rows = (room - 2).min(cap);
    let h = rows.len().min(max_rows) + 2;
    let y0 = if down { last + 1 } else { first - h };
    let x0 = (cx as usize).saturating_sub(2).min(lay.edit_w.saturating_sub(inner + 4));
    Some(FloatGeom { x0, y0, inner, h, max_rows, rows })
}

#[allow(clippy::too_many_arguments)]
fn draw_float(
    lines: &[crate::markdown::Line],
    at: (u16, u16),
    scroll: usize,
    cap: usize,
    ui: &Ui,
    editor: &Editor,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<Option<(usize, usize, usize, usize)>> {
    let Some(g) = float_geom(lines, at, cap, ui, editor, lay) else { return Ok(None) };
    // Skip as far as scrolled, but stop at the last page (so more C-d doesn't give an empty box)
    let skip = scroll.min(g.rows.len().saturating_sub(g.max_rows));
    draw_doc_box(&g.rows[skip..], (g.x0, g.y0), g.inner, g.max_rows, ui, editor, out)?;
    Ok(Some(g.rect()))
}

fn dim_style(ui: &Ui, editor: &Editor) -> Style {
    Style { fg: ui.virt.fg, ..card_style(ui, editor, "ui.popup") }
}

/// Doc box: card-like padding, no border, if the theme has a bg color; if bg = the editor's, rounded border.
/// Width = inner + 4 (padding 1 per side + border/padding 1), height = visible rows + 2. "+N" below if cut.
fn draw_doc_box(
    rows: &[Vec<crate::markdown::Span>],
    (x0, y0): (usize, usize),
    inner: usize,
    max_rows: usize,
    ui: &Ui,
    editor: &Editor,
    out: &mut impl Write,
) -> io::Result<()> {
    let popup = card_style(ui, editor, "ui.popup");
    let dim = Style { fg: ui.virt.fg, ..popup };
    let card = card_padding(ui, popup) == 1;
    let shown = rows.len().min(max_rows);
    let width = inner + 4;
    let more = if rows.len() > shown { format!(" +{} ", rows.len() - shown) } else { String::new() };
    // Top edge: half-row block for cards, else a rounded border
    if card {
        card_edge(out, ui, popup, x0, y0, width, 1, true)?;
    } else {
        apply(out, dim)?;
        queue!(out, MoveTo(x0 as u16, y0 as u16), Print(format!("╭{}╮", "─".repeat(inner + 2))))?;
    }
    let side = if card { " " } else { "│" };
    for (i, spans) in rows.iter().take(shown).enumerate() {
        apply(out, dim)?;
        queue!(out, MoveTo(x0 as u16, (y0 + 1 + i) as u16), Print(side), Print(" "))?;
        let mut used = 0;
        for sp in spans {
            let text = fit(&sp.text, inner - used);
            apply(out, popup.patch(sp.style))?;
            queue!(out, Print(&text))?;
            used += text.width();
            if used >= inner {
                break;
            }
        }
        apply(out, popup)?;
        // Overflowed row count dimmed at the right end of the last row (if there's room)
        let last = i + 1 == shown && !more.is_empty() && inner >= used + more.width();
        if last && card {
            queue!(out, Spaces(inner - used - more.width() + 1))?;
            apply(out, Style { fg: ui.linenr.fg, ..popup })?;
            queue!(out, Print(more.trim_end()), Print(" "))?;
        } else {
            queue!(out, Spaces(inner - used + 1))?;
        }
        apply(out, dim)?;
        queue!(out, Print(side))?;
    }
    let y = y0 + 1 + shown;
    if card {
        card_edge(out, ui, popup, x0, y, width, 1, false)
    } else {
        apply(out, dim)?;
        queue!(
            out,
            MoveTo(x0 as u16, y as u16),
            Print(format!("╰{}{more}╯", "─".repeat((inner + 2).saturating_sub(more.width()))))
        )
    }
}

/// Whether diagnostic `d` underlines the cell at `pos` (an empty range marks the one cell at its start).
fn underlines(d: &crate::lsp::Diagnostic, pos: usize) -> bool {
    d.from <= pos && (pos < d.to || (d.from == d.to && pos == d.from))
}

/// The diagnostics under the cursor in full — the line end shows only a first line, cut to fit. A card by
/// the cursor, only when it sits on an underline whose message the line end didn't show whole, and
/// nothing else floats (normal/select mode).
/// The note under the cursor, whole thread, below the cursor (it stays while you type a reply). True if drawn.
///
/// ```text
///   ¶ claude
///     Lex errors lose their position here — pass the `Span` along.
///   › you
///     why not return early?
///   ▂▅▇ claude is replying…
///
///   space n reply   ]n next   space N all
/// ```
fn draw_note_card(
    editor: &Editor,
    at: (u16, u16),
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<bool> {
    let Some(g) = note_card_geom(editor, at, ui, lay) else { return Ok(false) };
    editor.screen.note_card.set(Some(g.rect()));
    // Short on room: the newest part (the latest word and the keys) stays in view — under the name of whoever
    // says it, the cut words going on with `…`
    let skip = g.rows.len().saturating_sub(g.max_rows);
    let speaker =
        |r: &Vec<crate::markdown::Span>| r.first().is_some_and(|s| s.text == "¶ " || s.text == "› ");
    let rows = match g.rows[..=skip.min(g.rows.len().saturating_sub(1))].iter().rposition(speaker) {
        Some(h) if h < skip => {
            let mut rows = vec![g.rows[h].clone()];
            rows.extend(g.rows[skip + 1..].iter().cloned());
            if let Some(first) = rows.get_mut(1) {
                if let Some(s) = first.first_mut().filter(|s| s.text.starts_with("  ")) {
                    s.text.replace_range(..2, "");
                }
                let faint = Style { fg: ui.linenr.fg, ..Style::default() };
                first.insert(0, crate::markdown::Span { text: "… ".into(), style: faint });
            }
            rows
        }
        _ => g.rows[skip..].to_vec(),
    };
    draw_doc_box(&rows, (g.x0, g.y0), g.inner, g.max_rows, ui, editor, out)?;
    Ok(true)
}

/// Most rows the note card takes.
const NOTE_CAP: usize = 18;

/// The note card's place and rows, if one shows now (the cursor on a note, nothing else in the way).
fn note_card_geom(editor: &Editor, at: (u16, u16), ui: &Ui, lay: &Layout) -> Option<FloatGeom> {
    let replying = matches!(editor.prompt.as_ref().map(|p| p.kind), Some(crate::editor::PromptKind::Note(_)));
    let busy = editor.mode == Mode::Insert
        || editor.popup.is_some()
        || editor.completion.is_some()
        || editor.signature.is_some()
        || editor.picker.is_some()
        || (editor.prompt.is_some() && !replying)
        || !editor.pending.is_empty();
    let n = crate::notes::here(editor).filter(|_| !busy)?;
    let all = crate::notes::all_here(editor);
    let of = (all.len() > 1).then(|| (all.iter().position(|m| m.id == n.id).unwrap_or(0) + 1, all.len()));
    let lines = note_lines(n, of, editor, ui, replying);
    // Beside the note's lines, not over them (the code it's about stays in sight) — the rows of them on
    // screen (`view` rows count from the focused pane's top), the cursor's row among them
    let on = |r: &crate::doccomment::ViewRow| {
        matches!(r, crate::doccomment::ViewRow::Line(_) | crate::doccomment::ViewRow::Part { .. })
            && n.lines.contains(&r.line())
    };
    let pane = panes(editor, lay).into_iter().find(|p| p.6).map_or_else(|| lay.clone(), |p| p.2);
    let row = |r: usize| pane.text_top as usize + r;
    let span = editor.view.iter().position(on).zip(editor.view.iter().rposition(on));
    let cursor = float_geom(&lines, at, NOTE_CAP, ui, editor, lay);
    let Some((a, b)) = span.map(|(a, b)| (row(a).min(at.1 as usize), row(b).max(at.1 as usize))) else {
        return cursor;
    };
    let around = float_geom_around(&lines, at.0, (a, b), NOTE_CAP, ui, editor, lay);
    // Lines too many to keep clear: whichever shows more of the thread (the cursor's own row stays clear)
    match (around, cursor) {
        (Some(g), Some(c)) if g.max_rows < c.max_rows.min(g.rows.len()) => Some(c),
        (g, c) => g.or(c),
    }
}

/// A note's thread as card lines — who said it (Claude in accent, you dim), the text with `code` colored,
/// a waiting row while Claude answers, and the keys.
fn note_lines(
    n: &crate::notes::Note,
    of: Option<(usize, usize)>,
    editor: &Editor,
    ui: &Ui,
    replying: bool,
) -> Vec<crate::markdown::Line> {
    use crate::markdown::{Line, Span};
    use crate::notes::By;
    let lang = editor.doc().syntax.as_ref().map(|s| s.lang.name.clone());
    let sp = |text: &str, style: Style| Span { text: text.to_string(), style };
    let accent = Style { fg: ui.accent.fg, ..Style::default() };
    let dim = Style { fg: ui.virt.fg, ..Style::default() };
    let faint = Style { fg: ui.linenr.fg, ..Style::default() };
    let key = Style { bold: true, ..dim };
    let mut out = Vec::new();
    // Several notes here: which one this is (]n walks them)
    if let Some((i, k)) = of {
        out.push(Line::Text {
            spans: vec![sp(&format!("note {i} of {k} here · ]n next"), faint)],
            indent: 0,
        });
    }
    // The newest three; older ones fold into a count
    const SHOWN: usize = 3;
    let skip = n.thread.len().saturating_sub(SHOWN);
    if skip > 0 {
        out.push(Line::Text {
            spans: vec![sp(&format!("… {skip} earlier — the chat has them"), faint)],
            indent: 0,
        });
        out.push(Line::Blank);
    }
    for (i, e) in n.thread.iter().enumerate().skip(skip) {
        if i > skip {
            out.push(Line::Blank);
        }
        out.push(Line::Text {
            spans: match e.by {
                By::Claude => vec![sp("¶ ", accent), sp("claude", Style { bold: true, ..accent })],
                By::You if e.draft => {
                    vec![sp("› ", dim), sp("you", Style { bold: true, ..dim }), sp(" · draft", faint)]
                }
                By::You => vec![sp("› ", dim), sp("you", Style { bold: true, ..dim })],
            },
            indent: 0,
        });
        for l in e.text.lines().map(str::trim_end) {
            out.push(if l.trim().is_empty() {
                Line::Blank
            } else {
                let mut spans = vec![sp("  ", Style::default())];
                spans.extend(message_spans(l, Style::default(), lang.as_deref(), editor));
                Line::Text { spans, indent: 2 }
            });
        }
    }
    if n.waiting {
        let started = editor.chat.as_ref().map_or_else(std::time::Instant::now, |c| c.started);
        out.push(Line::Blank);
        out.push(Line::Text {
            spans: vec![sp(&wave(started), accent), sp(" claude is replying…", dim)],
            indent: 0,
        });
    }
    out.push(Line::Blank);
    let drafting = crate::codereview::drafting(editor);
    let hints: &[(&str, &str)] = if replying && drafting {
        &[("enter", "save draft"), ("esc", "cancel")]
    } else if replying {
        &[("enter", "send"), ("esc", "cancel")]
    } else if n.thread.iter().any(|e| e.draft) {
        &[("space n", "reply"), ("space v s", "send the review"), ("]n", "next")]
    } else {
        &[("space n", "reply"), ("]n", "next"), ("space N", "all"), (":note-close", "")]
    };
    out.push(Line::Text { spans: hint_spans(hints, key, faint), indent: 0 });
    out
}

/// The merge conflict under the cursor: which of how many, and the keys that settle it (the labels git gave
/// each side). True if drawn.
fn draw_conflict_card(
    editor: &Editor,
    at: (u16, u16),
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<bool> {
    use crate::markdown::{Line, Span};
    let busy = editor.mode == Mode::Insert
        || editor.popup.is_some()
        || editor.picker.is_some()
        || editor.prompt.is_some()
        || !editor.pending.is_empty();
    let Some((i, c)) = editor.conflict_here().filter(|_| !busy) else { return Ok(false) };
    let n = editor.doc().conflicts.len();
    let key = Style { fg: ui.accent.fg, bold: true, ..Style::default() };
    let faint = Style { fg: ui.linenr.fg, ..Style::default() };
    let plain = Style::default();
    // Each side in its wash's color, so the card reads as a legend
    let ours_st = Style { fg: ui.diff_plus.fg, ..plain };
    let theirs_st = Style { fg: editor.theme.try_get("diff.delta").and_then(|s| s.fg), ..plain };
    let side = |label: &str, what: &str| {
        if label.is_empty() { what.to_string() } else { format!("{what} ({label})") }
    };
    let sp = |text: String, style: Style| Span { text, style };
    let lines = vec![
        Line::Text {
            spans: vec![
                sp("● ".into(), Style { fg: ui.error.fg, ..plain }),
                sp("merge conflict".into(), Style { bold: true, ..plain }),
                sp(format!("  {} of {n}", i + 1), faint),
            ],
            indent: 2,
        },
        Line::Text {
            spans: vec![
                sp("space g o".into(), key),
                sp(format!(" {}", side(&c.ours, "ours")), ours_st),
                sp("  ".into(), faint),
                sp("t".into(), key),
                sp(format!(" {}", side(&c.theirs, "theirs")), theirs_st),
                sp("  ".into(), faint),
                sp("a".into(), key),
                sp(" both".into(), plain),
            ],
            indent: 2,
        },
        // Walking on is an ordinary hint — the accent is for the answer the card asks for
        Line::Text {
            spans: hint_spans(
                &[("]m", "next"), ("[m", "previous")],
                Style { fg: ui.virt.fg, ..plain },
                faint,
            ),
            indent: 2,
        },
    ];
    // Below the conflict (so both sides stay in sight) — at the cursor if its end is off screen
    let end_row = editor.view.iter().rposition(|r| {
        matches!(r, crate::doccomment::ViewRow::Line(_) | crate::doccomment::ViewRow::Part { .. })
            && r.line() == c.end
    });
    // (`view` rows count from the focused pane's top; the card is five rows)
    let pane = panes(editor, lay).into_iter().find(|p| p.6).map_or_else(|| lay.clone(), |p| p.2);
    let at = match end_row {
        Some(r) if r + 1 + 5 <= pane.text_rows => (at.0, pane.text_top + r as u16),
        _ => at,
    };
    draw_float(&lines, at, 0, 12, ui, editor, lay, out)?;
    Ok(true)
}

fn draw_diagnostic_card(
    editor: &Editor,
    at: (u16, u16),
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    let busy = editor.mode == Mode::Insert
        || editor.popup.is_some()
        || editor.completion.is_some()
        || editor.signature.is_some()
        || editor.picker.is_some()
        || editor.prompt.is_some()
        || !editor.pending.is_empty()
        || editor.chat.as_ref().is_some_and(|c| c.focused);
    if !editor.config.cursor_diagnostics
        || busy
        || editor.diag_card_hidden == Some((editor.doc().id, editor.cursor_line()))
    {
        return Ok(());
    }
    let here = cursor_diagnostics(editor);
    // The line end shows the line's only diagnostic whole — and that's the one under the cursor
    let line = editor.cursor_line();
    let text = &editor.doc().text;
    if ui.cursor_diags_shown.get() && here.iter().all(|d| mv::line_of(text, d.from) == line) {
        return Ok(());
    }
    let lines = diagnostic_lines(&here, editor, ui);
    if lines.is_empty() {
        return Ok(());
    }
    draw_float(&lines, at, 0, 12, ui, editor, lay, out).map(|_| ())
}

/// Diagnostics underlining the cell under the cursor — most severe first.
fn cursor_diagnostics(editor: &Editor) -> Vec<&crate::lsp::Diagnostic> {
    let doc = editor.doc();
    let head = doc.selection().primary().cursor(&doc.text);
    let mut list: Vec<_> = doc.lsp.diagnostics.iter().filter(|d| underlines(d, head)).collect();
    list.sort_by_key(|d| (d.severity, d.from));
    list
}

/// Card text: per diagnostic `● first line  source code`, then the rest of its lines; `code` in the
/// buffer's syntax colors. A blank row between diagnostics.
fn diagnostic_lines(
    diags: &[&crate::lsp::Diagnostic],
    editor: &Editor,
    ui: &Ui,
) -> Vec<crate::markdown::Line> {
    use crate::markdown::{Line, Span};
    let lang = editor.doc().syntax.as_ref().map(|s| s.lang.name.clone());
    let faint = Style { fg: ui.linenr.fg, ..Style::default() };
    let mut out = Vec::new();
    let mut last_source = None;
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push(Line::Blank);
        }
        let glyph = match d.severity {
            2 => "▲",
            1 | 3 => "●",
            _ => "·",
        };
        let mut lines =
            d.message.lines().map(str::trim_end).filter(|l| !l.trim().is_empty() && !lint_origin(l));
        let color = Style { fg: ui.sign[d.severity.min(4) as usize].fg, ..Style::default() };
        let mut spans = vec![Span { text: format!("{glyph} "), style: color }];
        // Errors/warnings lead in bold; info/hints (often "expected due to this") step back
        let head_style = if d.severity <= 2 {
            Style { bold: true, ..Style::default() }
        } else {
            Style { fg: ui.virt.fg, ..Style::default() }
        };
        let head = lines.next().unwrap_or_default();
        spans.extend(message_spans(head, head_style, lang.as_deref(), editor));
        // Source only when it changes (a hint right after its error repeats it)
        let source = diagnostic_source(&d.raw);
        if let Some(src) = source.as_ref().filter(|&s| last_source.as_ref() != Some(s)) {
            spans.push(Span { text: format!("  {src}"), style: faint });
        }
        last_source = source;
        out.push(Line::Text { spans, indent: 2 });
        for l in lines {
            let mut spans = vec![Span { text: "  ".into(), style: Style::default() }];
            spans.extend(message_spans(l, Style::default(), lang.as_deref(), editor));
            out.push(Line::Text { spans, indent: 2 });
        }
    }
    out
}

/// Message text → spans: `code` (backticks dropped) colored as code of `lang`, the rest in `base`.
fn message_spans(text: &str, base: Style, lang: Option<&str>, editor: &Editor) -> Vec<crate::markdown::Span> {
    use crate::markdown::{Line, Span};
    let t = &editor.theme;
    let raw = t.try_get("markup.raw.inline").or_else(|| t.try_get("markup.raw")).unwrap_or_default();
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let Some(len) = rest[open + 1..].find('`') else { break };
        if open > 0 {
            out.push(Span { text: rest[..open].to_string(), style: base });
        }
        let code = &rest[open + 1..open + 1 + len];
        // Inline-code color underneath, syntax colors on top (unparsed bits still read as code)
        match crate::markdown::code_lines(code, lang, t).into_iter().next() {
            Some(Line::Code(spans)) => {
                out.extend(spans.into_iter().map(|sp| Span { style: raw.patch(sp.style), ..sp }))
            }
            _ => out.push(Span { text: code.to_string(), style: raw }),
        }
        rest = &rest[open + 1 + len + 1..];
    }
    if !rest.is_empty() {
        out.push(Span { text: rest.to_string(), style: base });
    }
    out
}

/// rustc's "`#[warn(unused_variables)]` (part of …) on by default" — says where a lint comes from, not
/// what's wrong. Left out of the card.
fn lint_origin(line: &str) -> bool {
    let l = line.trim();
    l.starts_with("`#[") && l.ends_with("on by default")
}

/// `rustc E0308` — who reported it and its code, if the server says.
fn diagnostic_source(raw: &serde_json::Value) -> Option<String> {
    let code = match &raw["code"] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let parts: Vec<String> = raw["source"].as_str().map(str::to_string).into_iter().chain(code).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn cursor_diagnostic(editor: &Editor) -> Option<&crate::lsp::Diagnostic> {
    let doc = editor.doc();
    let head = doc.selection().primary().cursor(&doc.text);
    let line = mv::line_of(&doc.text, head);
    let list = &doc.lsp.diagnostics;
    list.iter()
        .filter(|d| d.from <= head && head <= d.to)
        .min_by_key(|d| d.severity)
        .or_else(|| list.iter().filter(|d| mv::line_of(&doc.text, d.from) == line).min_by_key(|d| d.severity))
}

/// Keys for whatever is floating now (shown dimmed when the command line is empty).
fn key_hints(editor: &Editor) -> Option<&'static [(&'static str, &'static str)]> {
    let replace_list = |p: &Picker| p.current().is_some_and(|i| matches!(i.action, Action::ReplaceFile(_)));
    if editor.completion.is_some() {
        Some(&[("tab", "select"), ("enter", "accept"), ("C-x", "complete"), ("esc", "close")])
    } else if editor.picker.as_ref().is_some_and(|p| p.grep.is_some()) {
        Some(&[("enter", "open"), ("C-r", "replace the listed matches"), ("esc", "close")])
    } else if editor.picker.as_ref().is_some_and(replace_list) {
        Some(&[("enter", "replace in every listed file"), ("type", "narrow"), ("esc", "cancel")])
    } else if editor.popup.is_some() {
        Some(&[("C-d", "scroll down"), ("C-u", "scroll up"), ("esc", "close")])
    } else {
        None
    }
}

/// Command line row. Returns the prompt's cursor position while typing.
/// `:` completion list — a card attached just above the statusline, first cell at the word being completed.
/// One row = [bar][name (typed chars highlighted, folders in accent)][   description·aliases dimmed][scroll].
fn draw_cmdline_menu(
    editor: &Editor,
    completion: Option<&crate::cmdline::Completion>,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<()> {
    const ROWS: usize = 8;
    const LABEL_MAX: usize = 30;
    const HINT_MAX: usize = 60;
    let Some(p) = editor.prompt.as_ref() else { return Ok(()) };
    let Some(c) = completion else { return Ok(()) };
    let n = c.cands.len();
    let t = &editor.theme;
    let card = card_style(ui, editor, "ui.menu");
    let pad = card_padding(ui, card);
    let status_y = lay.cmd_y as usize - 1;
    let rows = n.min(ROWS).min(status_y.saturating_sub(lay.text_top as usize + 2 * pad));
    if rows == 0 {
        return Ok(());
    }
    let selected_i = p.cycle.as_ref().map(|(_, i)| *i);
    let first = selected_i.map_or(0, |s| s.saturating_sub(rows - 1));
    let visible = first..first + rows;
    let label_w = visible.clone().map(|i| c.cands[i].label.width()).max().unwrap_or(0).min(LABEL_MAX);
    let hint_w = visible.clone().map(|i| c.cands[i].hint.width()).max().unwrap_or(0).min(HINT_MAX);
    let scroll = n > rows;
    let width = (2 + label_w + if hint_w > 0 { 3 + hint_w } else { 0 } + 2).min(lay.width);
    // Column of the word being completed: prompt head (`:`) + text before the word
    let base = p.cycle.as_ref().map_or(p.text.as_str(), |(b, _)| b.as_str());
    let col = prompt_head(p).width() + base[..c.start.min(base.len())].width();
    let x0 = col.saturating_sub(2).min(lay.width.saturating_sub(width));
    let h = rows + 2 * pad;
    let y0 = status_y - h;
    let selected = selected_row(ui, editor, card);
    let matched = Style { fg: ui.accent.fg, bold: true, ..Style::default() };
    let dir_fg = t.try_get("ui.text.directory").and_then(|s| s.fg).or(ui.accent.fg);
    card_edge(out, ui, card, x0, y0, width, pad, true)?;
    let thumb = scroll_thumb(rows, n, first);
    for row in 0..rows {
        let i = first + row;
        let cand = &c.cands[i];
        let is_sel = selected_i == Some(i);
        let base_st = if is_sel { selected } else { card };
        queue!(out, MoveTo(x0 as u16, (y0 + pad + row) as u16))?;
        apply(out, if is_sel { Style { fg: ui.accent.fg, ..base_st } } else { base_st })?;
        queue!(out, Print(if is_sel { "▎ " } else { "  " }))?;
        let name =
            if is_sel { base_st.patch(t.try_get("ui.text.focus").unwrap_or_default()) } else { base_st };
        let name = Style { bg: base_st.bg, fg: if cand.dir { dir_fg } else { name.fg }, ..name };
        let label = fit_ellipsis(&cand.label, label_w);
        print_matched(out, &label, &cand.hits, name, matched, label_w)?;
        apply(out, base_st)?;
        queue!(out, Spaces(label_w - label.width()))?;
        let mut used = 2 + label_w;
        if hint_w > 0 {
            let hnt = fit_ellipsis(&cand.hint, hint_w);
            apply(out, Style { fg: ui.virt.fg, ..base_st })?;
            queue!(out, Print("   "), Print(&hnt), Spaces(hint_w - hnt.width()))?;
            used += 3 + hint_w;
        }
        apply(out, base_st)?;
        queue!(out, Spaces(width.saturating_sub(used + 1)))?;
        if scroll && thumb.contains(&row) {
            apply(out, Style { fg: ui.linenr.fg, ..card })?;
            queue!(out, Print("▐"))?;
        } else {
            apply(out, card)?;
            queue!(out, Print(" "))?;
        }
    }
    card_edge(out, ui, card, x0, y0 + pad + rows, width, pad, false)
}

/// What the command line starts with: `:` `/` `?` as they are (the input's own first character); a word
/// (`grep:`, `rename:`) says what the line is for — drawn dim, a space before the input.
fn prompt_head(p: &crate::editor::Prompt) -> String {
    let label = p.label.as_deref().unwrap_or(p.kind.label());
    if label.len() > 1 { format!("{label} ") } else { label.to_string() }
}

fn draw_cmdline(
    editor: &Editor,
    completion: Option<&crate::cmdline::Completion>,
    ui: &Ui,
    lay: &Layout,
    out: &mut impl Write,
) -> io::Result<Option<(u16, u16)>> {
    queue!(out, MoveTo(0, lay.cmd_y))?;
    clear_rest(out, ui.base)?;
    if let Some(prompt) = &editor.prompt {
        let head = prompt_head(prompt);
        // A long input keeps its end in sight — that's where the cursor is
        let text = fit_left(&prompt.text, lay.width.saturating_sub(head.width() + 1));
        let word = head.width() > 1;
        apply(out, if word { Style { fg: ui.virt.fg, ..ui.base } } else { ui.base })?;
        queue!(out, Print(&head))?;
        apply(out, ui.base)?;
        queue!(out, Print(&text))?;
        let shown = format!("{head}{text}");
        // Rest of the first candidate dimmed after the cursor (accept with →)
        if let Some(c) = completion.filter(|_| prompt.cycle.is_none())
            && let Some(g) = crate::cmdline::ghost(&prompt.text, c)
        {
            apply(out, Style { fg: ui.linenr.fg, ..ui.base })?;
            queue!(out, Print(fit(g, lay.width.saturating_sub(shown.width() + 1))))?;
            apply(out, ui.base)?;
        }
        return Ok(Some((shown.width() as u16, lay.cmd_y)));
    }
    let pending = editor.pending_display();
    let room = lay.width.saturating_sub(pending.width() + 1);
    // Status messages go to top-right toasts — the command line has only input, key hints, cursor diagnostics
    if let Some(hints) = key_hints(editor) {
        // If something is floating, show its keys: keys crisp, descriptions dimmed
        let key = Style { fg: ui.virt.fg, bold: true, ..ui.base };
        let desc = Style { fg: ui.linenr.fg, ..ui.base };
        let mut used = 0;
        for (k, d) in hints {
            let w = k.width() + d.width() + 4;
            if used + w > room {
                break;
            }
            apply(out, key)?;
            queue!(out, Print(" "), Print(k))?;
            apply(out, desc)?;
            queue!(out, Print(" "), Print(d), Print("  "))?;
            used += w;
        }
        apply(out, ui.base)?;
    } else if let Some(d) = cursor_diagnostic(editor) {
        // If the cursor is on a diagnostic (else that line), show its message
        let msg = format!("● {}", d.message.lines().next().unwrap_or_default());
        apply(out, ui.base.patch(ui.sign[d.severity as usize]))?;
        queue!(out, Print(fit_ellipsis(&msg, room)))?;
        apply(out, ui.base)?;
    }
    if !pending.is_empty() {
        queue!(out, MoveTo(lay.width.saturating_sub(pending.width()) as u16, lay.cmd_y), Print(&pending))?;
    }
    Ok(None)
}

#[cfg(test)]
#[path = "term_snapshots.rs"]
mod snapshot_tests;

#[cfg(test)]
mod tests {
    /// The chat's tool row keeps its suffix when it fits whole — a command's `ok` too, the command cut for it.
    #[test]
    fn a_short_suffix_stays_on_a_long_row() {
        let st = TrailStyles {
            faint: Style::default(),
            dim: Style::default(),
            string: Style::default(),
            accent: Style::default(),
        };
        let look = |range: &str| crate::chat::Look {
            verb: "run",
            what: "git commit -m 'Add sub' src/lib.rs".into(),
            pattern: true,
            range: range.into(),
            link: None,
        };
        let text = |l: &crate::chat::Look, slim| -> String {
            trail_row(l, false, 30, slim, &st).into_iter().map(|s| s.text).collect()
        };
        let row = text(&look("ok"), false);
        assert!(row.ends_with("  ok") && row.contains("git commit"), "{row:?}");
        assert!(row.width() <= 30, "{row:?}");
        let row = text(&look("exit 101"), false);
        assert!(row.ends_with("  exit 101"), "{row:?}");
        // (the minimized chat keeps the name instead — its rows are slim)
        assert!(!text(&look("ok"), true).ends_with("ok"));
    }

    /// The notification card takes the top right, else the bottom right, never over another card; with
    /// both taken it has no place (the status line carries it).
    #[test]
    fn toast_card_keeps_out_of_the_other_cards() {
        let card = (60, 30, 4); // x, width, height
        let rows = (1, 29);
        assert_eq!(toast_place(card, rows, &[]), Some(1));
        let thought = (50, 2, 40, 8);
        assert_eq!(toast_place(card, rows, &[thought]), Some(24), "under the thought card: the bottom");
        let mini = (58, 20, 36, 8);
        assert_eq!(toast_place(card, rows, &[thought, mini]), None, "no room left");
        assert_eq!(toast_place(card, rows, &[(0, 2, 20, 8)]), Some(1), "a card elsewhere doesn't matter");
    }

    use std::time::{Duration, Instant};

    use ropey::Rope;
    use std::path::PathBuf;

    use super::*;

    /// The thought card's log keeps its newest rows, but not a lone fragment of a cut paragraph on top.
    #[test]
    fn a_log_cut_skips_a_lone_fragment() {
        let row = |t: &str| vec![crate::markdown::Span { text: t.into(), style: Style::default() }];
        let body: Vec<_> = ["a1", "a2", "a3", "", "b1", "b2", "", "c1"].iter().map(|t| row(t)).collect();
        assert_eq!(log_cut(&body, 8), 0, "all of it fits");
        assert_eq!(log_cut(&body, 6), 4, "a3 alone isn't shown — b starts it");
        assert_eq!(log_cut(&body, 5), 4, "a blank row never tops it");
        assert_eq!(log_cut(&body, 3), 7, "b2 alone (its paragraph cut): c starts it");
        assert_eq!(log_cut(&body, 7), 1, "two rows of a are worth showing");
    }
    use crate::config::Config;

    #[test]
    fn doc_comments_render_until_cursor_enters() {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str("/// Adds `two` numbers.\nfn add() {}\n");
        ed.docs[0].path = Some("/tmp/x.rs".into()); // doc comments only in languages that use that marker
        ed.docs[0].set_selection(crate::selection::Selection::point(24)); // fn line
        let mut buf = Vec::new();
        render(&mut ed, &mut buf, 80, 10).unwrap();
        let screen = String::from_utf8_lossy(&buf);
        assert!(!screen.contains("///") && screen.contains("▎ ") && screen.contains("numbers."), "{screen}");
        ed.docs[0].set_selection(crate::selection::Selection::point(5)); // inside the comment
        buf.clear();
        render(&mut ed, &mut buf, 80, 10).unwrap();
        assert!(!String::from_utf8_lossy(&buf).contains("///"), "passing cursor in normal mode stays folded");
        ed.mode = Mode::Insert;
        buf.clear();
        render(&mut ed, &mut buf, 80, 10).unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("///"), "entering to edit (INSERT) shows raw");
        ed.mode = Mode::Normal;
        ed.docs[0].set_selection(crate::selection::Selection::single(crate::selection::Range::new(4, 9)));
        buf.clear();
        render(&mut ed, &mut buf, 80, 10).unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("///"), "a selection wider than a char shows raw");
        // Markdown (`///` inside a code block) and unknown files stay as-is
        ed.docs[0].path = Some("/tmp/x.md".into());
        ed.docs[0].set_selection(crate::selection::Selection::point(24));
        buf.clear();
        render(&mut ed, &mut buf, 80, 10).unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("///"));
    }

    /// Minimal terminal: applies `MoveTo`, `Clear(UntilNewLine)` and prints (other escapes ignored) —
    /// the screen rows as text (a wide char's second cell is empty).
    fn screen(bytes: &[u8], w: usize, h: usize) -> Vec<String> {
        let s = String::from_utf8_lossy(bytes);
        let mut grid = vec![vec![" ".to_string(); w]; h];
        let (mut x, mut y) = (0, 0);
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                if it.next_if_eq(&'[').is_none() {
                    it.next();
                    continue;
                }
                let mut params = String::new();
                while let Some(p) = it.next_if(|p| ('\x20'..='\x3f').contains(p)) {
                    params.push(p);
                }
                match it.next() {
                    Some('H') => {
                        let mut n = params.split(';').map(|n| n.parse::<usize>().unwrap_or(1));
                        (y, x) = (n.next().unwrap_or(1) - 1, n.next().unwrap_or(1) - 1);
                    }
                    Some('K') if y < h => grid[y][x.min(w)..].fill(" ".into()),
                    _ => {}
                }
                continue;
            }
            let cw = c.width().unwrap_or(0);
            if y < h && x + cw <= w && cw > 0 {
                grid[y][x] = c.to_string();
                grid[y][x + 1..x + cw].fill(String::new());
            }
            x += cw;
        }
        grid.into_iter().map(|r| r.concat()).collect()
    }

    #[test]
    fn unfocused_pane_survives_its_doc_shrinking() {
        let mut ed = Editor::new(Config::default());
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        ed.docs[0].text = Rope::from_str(&text);
        ed.docs[0].path = Some("/tmp/x.rs".into()); // the doc-comment scan runs for languages that use it
        let at = mv::line_start(&ed.docs[0].text, 980);
        ed.docs[0].set_selection(crate::selection::Selection::point(at));
        let mut buf = Vec::new();
        render(&mut ed, &mut buf, 80, 24).unwrap();
        ed.split_view(crate::split::Dir::Vertical);
        for k in ["%", "d"] {
            ed.handle_key(k.parse().unwrap());
        }
        assert_eq!(ed.docs[0].text.len_bytes(), 0);
        buf.clear();
        render(&mut ed, &mut buf, 80, 24).unwrap(); // the other pane still scrolled to ~960
    }

    #[test]
    fn narrow_screens_never_panic() {
        let text: String = (0..100).map(|i| format!("fn line_{i}() {{ 타래 }}\n")).collect();
        let lines =
            crate::markdown::render("**hover** docs that are long enough to wrap a bit", builtin(), None);
        for chat in [false, true] {
            let mut ed = Editor::new(Config::default());
            ed.config.llm.command = "/nonexistent/tarae-test-claude".into();
            ed.docs[0].text = Rope::from_str(&text);
            let at = mv::line_start(&ed.docs[0].text, 50);
            ed.docs[0].set_selection(crate::selection::Selection::point(at));
            if chat {
                crate::chat::open(&mut ed);
                ed.chat.as_mut().unwrap().focused = false;
            }
            ed.push_offer(crate::offer::Offer {
                id: 0,
                title: "Rust syntax colors".into(),
                question: "Download and build the tree-sitter grammar?".into(),
                working: "Fetching and building…".into(),
                failed: "Couldn't build".into(),
                what: crate::offer::What::Grammars { lang: "Rust".into(), names: vec![], then: None },
                state: crate::offer::OfferState::Asking,
                rev: 0,
            });
            let result =
                serde_json::json!([{"label": "line_one", "detail": "fn line_one()"}, {"label": "lx"}]);
            let doc_id = ed.docs[0].id;
            let completion = || {
                let mut c = crate::completion::Completion::new(0, doc_id, at, result.clone());
                c.filter("l");
                c.selected = Some(0);
                c.docs = Some((0, lines.clone()));
                c
            };
            let mut buf = Vec::new();
            for h in [3, 10, 24] {
                for w in 1..=90 {
                    let ctx = format!("chat {chat}, {w}x{h}");
                    ed.popup = Some(lines.clone());
                    buf.clear();
                    render(&mut ed, &mut buf, w, h).unwrap_or_else(|e| panic!("{ctx}: {e}"));
                    ed.popup = None;
                    ed.completion = Some(completion());
                    render(&mut ed, &mut buf, w, h).unwrap();
                    ed.completion = None;
                    ed.pending = vec!["space".parse().unwrap()];
                    render(&mut ed, &mut buf, w, h).unwrap();
                    ed.pending.clear();
                    let mut p = crate::picker::Picker::new("themes", Vec::new(), false);
                    p.compact = true;
                    ed.picker = Some(p);
                    render(&mut ed, &mut buf, w, h).unwrap();
                    ed.picker = None;
                }
            }
        }
    }

    /// Horizontal scroll: a wide char cut by the left edge leaves a blank (what follows keeps its column),
    /// and a wide char under the cursor at the right edge scrolls fully into view (cursor stays visible).
    #[test]
    fn wide_chars_at_scroll_edges() {
        let mut ed = Editor::new(Config::default());
        ed.config.soft_wrap = "never".into(); // sideways scrolling
        ed.docs[0].text = Rope::from_str(&"漢".repeat(30));
        ed.docs[0].set_selection(crate::selection::Selection::point(29 * 3));
        let (w, h) = (21, 6); // gutter 4 → 17 text columns
        let mut buf = Vec::new();
        render(&mut ed, &mut buf, w, h).unwrap();
        assert_eq!(ed.docs[0].left, 60 - 17, "the last 漢 (cols 58–59) fits whole");
        assert!(String::from_utf8_lossy(&buf).contains("\x1b[?25h"), "cursor shown");
        let rows = screen(&buf, w as usize, h as usize);
        let row = rows.iter().find(|r| r.contains('漢')).unwrap();
        // 漢 at col 42 straddles left = 43 → one blank, then 漢 from col 44 at x = 1
        assert_eq!(row.trim_end(), format!("  1  {}", "漢".repeat(8)), "{rows:#?}");
    }

    /// Prose wraps at word boundaries with list items hanging; `j`/`k` go by rows; a click on a
    /// continuation row lands in that row; a line taller than the screen shows the cursor's part.
    #[test]
    fn soft_wrap_rows_motion_and_clicks() {
        let mut ed = Editor::new(Config::default());
        let src = "- one two three four five six\nnext\n";
        ed.docs[0].text = Rope::from_str(src);
        let (w, h) = (20, 8); // gutter 4 → 16 text columns
        let mut buf = Vec::new();
        render(&mut ed, &mut buf, w, h).unwrap();
        let rows = screen(&buf, w as usize, h as usize);
        // Row 0 is the path bar; line numbers are relative
        assert_eq!(rows[1].trim_end(), "  1 - one two three", "{rows:#?}");
        assert_eq!(rows[2].trim_end(), "      four five six", "no number, hangs under the text");
        assert_eq!(rows[3].trim_end(), "  1 next");
        let head = |ed: &Editor| ed.doc().selection().primary().cursor(&ed.doc().text);
        let key = |ed: &mut Editor, k: &str| {
            ed.handle_key(k.parse().unwrap());
            render(ed, &mut Vec::new(), w, h).unwrap();
        };
        key(&mut ed, "j");
        assert_eq!(head(&ed), src.find("four").unwrap(), "down one row, same screen x");
        key(&mut ed, "j");
        assert_eq!(head(&ed), src.find("next").unwrap());
        key(&mut ed, "k");
        assert_eq!(head(&ed), src.find("four").unwrap());
        let click =
            crate::event::Mouse { kind: crate::event::MouseKind::Down, x: 4 + 2 + 5, y: 2, alt: false };
        ed.handle_mouse(click);
        assert_eq!(head(&ed), src.find("five").unwrap());
        // One line taller than the screen: the rows around the cursor show
        let long = "word ".repeat(60);
        ed.docs[0].text = Rope::from_str(&long);
        ed.docs[0].set_selection(crate::selection::Selection::point(long.len() - 3));
        let mut buf = Vec::new();
        render(&mut ed, &mut buf, w, h).unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("\x1b[?25h"), "cursor shown");
        assert_eq!(ed.docs[0].left, 0, "never sideways");
    }

    /// `gw` puts two-letter labels on the words (nearest first); the first key narrows them to their second
    /// letter, the second picks the word (a jump). Any other key cancels.
    #[test]
    fn gw_labels_pick_a_word() {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str("alpha beta gamma\n");
        let (w, h) = (30, 6);
        let key = |ed: &mut Editor, k: &str| {
            ed.handle_key(k.parse().unwrap());
            let mut buf = Vec::new();
            render(ed, &mut buf, w, h).unwrap();
            screen(&buf, w as usize, h as usize)[1].trim_end().to_string()
        };
        render(&mut ed, &mut Vec::new(), w, h).unwrap();
        key(&mut ed, "g");
        // After the cursor first: beta = aa, then alpha (the cursor's own word) = ab, gamma = ac
        assert_eq!(key(&mut ed, "w"), "  1 abpha aata acmma", "labels cover the first two chars");
        assert_eq!(key(&mut ed, "a"), "  1 blpha aeta camma", "only the second letters are left");
        key(&mut ed, "c");
        let r = ed.doc().selection().primary();
        assert_eq!((r.from(), r.to()), (11, 16), "gamma selected");
        assert!(ed.jump_labels.is_none());
        key(&mut ed, "g");
        key(&mut ed, "w");
        assert_eq!(key(&mut ed, "esc"), "  1 alpha beta gamma", "cancelled");
        assert_eq!(ed.doc().selection().primary(), r);
    }

    /// `z` mode places the cursor line (scrolloff still applies) and scrolls without moving the cursor;
    /// `Z` keeps scrolling until Esc; `gt`/`gb` send the cursor to the screen's edges.
    #[test]
    fn view_mode_and_window_moves() {
        let mut ed = Editor::new(Config::default());
        ed.config.soft_wrap = "never".into();
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        ed.docs[0].text = Rope::from_str(&text);
        let (w, h) = (30, 20); // 17 text rows, scrolloff 5
        let keys = |ed: &mut Editor, ks: &str| {
            for k in ks.split(' ') {
                ed.handle_key(k.parse().unwrap());
                render(ed, &mut Vec::new(), w, h).unwrap();
            }
        };
        render(&mut ed, &mut Vec::new(), w, h).unwrap();
        keys(&mut ed, "5 0 G");
        let top = |ed: &Editor| ed.docs[0].top;
        keys(&mut ed, "z t");
        assert_eq!(top(&ed), 44, "line 49 five rows below the top");
        keys(&mut ed, "z b");
        assert_eq!(top(&ed), 38, "line 49 five rows above the bottom");
        keys(&mut ed, "z z");
        assert_eq!(top(&ed), 41);
        keys(&mut ed, "z j");
        assert_eq!((top(&ed), ed.cursor_line()), (42, 49), "the view moves, not the cursor");
        keys(&mut ed, "Z j j esc j");
        assert_eq!((top(&ed), ed.cursor_line()), (44, 50), "Z stays for j j; after Esc, j moves the cursor");
        keys(&mut ed, "g t");
        assert_eq!(ed.cursor_line(), 49, "top of the screen + scrolloff");
        keys(&mut ed, "g b");
        assert_eq!(ed.cursor_line(), 55);
    }

    /// The wheel scrolls what it's over: floating docs, a picker's preview (C-f/C-b too), or the list.
    #[test]
    fn wheel_scrolls_popups_and_previews() {
        use crate::event::{Mouse, MouseKind};
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str("x\n");
        let (w, h) = (120, 30);
        render(&mut ed, &mut Vec::new(), w, h).unwrap();
        let docs: Vec<crate::markdown::Line> = (0..60)
            .map(|i| {
                crate::markdown::Line::Code(vec![crate::markdown::Span {
                    text: format!("doc {i}"),
                    style: Style::default(),
                }])
            })
            .collect();
        ed.popup = Some(docs);
        render(&mut ed, &mut Vec::new(), w, h).unwrap();
        let (px, py, ..) = ed.screen.popup.get().expect("docs card drawn");
        ed.handle_mouse(Mouse {
            kind: MouseKind::ScrollDown,
            x: (px + 2) as u16,
            y: (py + 1) as u16,
            alt: false,
        });
        assert_eq!(ed.popup_scroll, 3);
        ed.popup = None;
        // A picker with a preview: over the preview it scrolls, over the list it moves
        let item = |n: usize| crate::picker::Item {
            label: format!("buffer {n}"),
            action: Action::Buffer(ed.docs[0].id),
            hint: String::new(),
            glyph: None,
        };
        let items = vec![item(1), item(2), item(3)];
        ed.open_picker(Picker::new("buffers", items, false), None);
        render(&mut ed, &mut Vec::new(), w, h).unwrap();
        let x = ed.screen.picker_preview_x.expect("preview card at this width");
        ed.handle_mouse(Mouse { kind: MouseKind::ScrollDown, x: (x + 3) as u16, y: 10, alt: false });
        assert_eq!(ed.picker.as_ref().map(|p| (p.preview_scroll, p.selected)), Some((3, 0)));
        ed.handle_key("C-b".parse().unwrap());
        assert_eq!(ed.picker.as_ref().map(|p| p.preview_scroll), Some(0));
        ed.handle_mouse(Mouse { kind: MouseKind::ScrollDown, x: 5, y: 10, alt: false });
        assert_eq!(
            ed.picker.as_ref().map(|p| (p.preview_scroll, p.selected)),
            Some((0, 0)),
            "3 of 3 wraps to 0"
        );
        ed.handle_key("C-f".parse().unwrap());
        assert_eq!(ed.picker.as_ref().map(|p| p.preview_scroll), Some(10));
    }

    /// A style goes out as one SGR sequence that a terminal reads just like crossterm's sequence a part.
    #[test]
    fn a_style_is_one_sequence() {
        use crossterm::style::{SetBackgroundColor, SetForegroundColor};
        crossterm::style::force_color_output(true);
        let st = Style {
            fg: Some(Color::Rgb { r: 1, g: 22, b: 255 }),
            bg: Some(Color::AnsiValue(8)),
            bold: true,
            italic: true,
            underline: true,
            dim: true,
            reversed: true,
            crossed: true,
            ..Style::default()
        };
        let mut out = Vec::new();
        apply(&mut out, st).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[0;38;2;1;22;255;48;5;8;1;3;4;2;7;9m");
        let colors =
            [Color::Rgb { r: 0, g: 9, b: 100 }, Color::AnsiValue(0), Color::AnsiValue(99), Color::DarkGrey];
        for (fg, bg) in colors.iter().flat_map(|&f| colors.iter().map(move |&b| (f, b))) {
            let st = Style { fg: Some(fg), bg: Some(bg), bold: true, ..Style::default() };
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            apply(&mut ours, st).unwrap();
            queue!(
                theirs,
                SetAttribute(Attribute::Reset),
                ResetColor,
                SetForegroundColor(fg),
                SetBackgroundColor(bg),
                SetAttribute(Attribute::Bold)
            )
            .unwrap();
            let cell = |bytes: &[u8]| {
                let mut p = vt100::Parser::new(1, 4, 0);
                p.process(b"\x1b[3;7m");
                p.process(bytes);
                p.process(b"x");
                let c = p.screen().cell(0, 0).unwrap().clone();
                (c.fgcolor(), c.bgcolor(), c.bold(), c.italic(), c.inverse())
            };
            assert_eq!(cell(&ours), cell(&theirs), "{fg:?} on {bg:?}");
        }
    }

    #[test]
    fn osc11_replies() {
        let dark = parse_osc11(b"\x1b]11;rgb:1010/1212/1717\x1b\\\x1b[?62;22c").unwrap();
        assert!(dark.0 < 0.1);
        let light = parse_osc11(b"\x1b]11;rgb:f5/f1/e8\x07").unwrap();
        assert!(light.0 > 0.9 && light.2 > 0.85);
        assert_eq!(parse_osc11(b"\x1b[?1;2c"), None, "terminal that doesn't know OSC 11 (DA1 only)");
    }

    /// Time for one key → one frame. Key handling once (it changes state), drawing the fastest of 3 —
    /// excludes scheduling noise from parallel tests (ones spawning processes, etc.) to time just the code.
    fn key_to_frame(ed: &mut Editor, buf: &mut Vec<u8>, k: &str, w: u16, h: u16) -> Duration {
        let t = Instant::now();
        ed.handle_key(k.parse().unwrap());
        let key = t.elapsed();
        let frame = (0..3)
            .map(|_| {
                buf.clear();
                let t = Instant::now();
                render(ed, buf, w, h).unwrap();
                t.elapsed()
            })
            .min()
            .unwrap();
        key + frame
    }

    /// One frame at 60 Hz. `TARAE_PERF_SLACK` (a multiplier) loosens it on slow shared machines — CI sets 3,
    /// which still catches an order-of-magnitude regression.
    fn frame_budget() -> Duration {
        let slack = std::env::var("TARAE_PERF_SLACK").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
        Duration::from_millis(16) * slack.max(1)
    }

    /// Performance budget: in a 200k-line file, key input → finished frame within one frame (16 ms, 60 Hz).
    /// Held even for debug builds — release is much faster.
    #[test]
    fn perf_budget_key_to_frame_on_large_file() {
        let budget = frame_budget();
        let text: String =
            (0..200_000).map(|i| format!("line {i} 타래 fn main() {{ let x = {i}; }}\n")).collect();
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str(&text);
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 120, 40).unwrap();

        let keys = [
            "j", "j", "w", "w", "e", "x", "x", "C", "C", "g", "e", "g", "g", "5", "0", "0", "0", "G", "i",
            "a", "esc", "u", "%", ";", "C-d", "C-u",
        ];
        let mut worst = (Duration::ZERO, "");
        for k in keys {
            let dt = key_to_frame(&mut ed, &mut buf, k, 120, 40);
            if dt > worst.0 {
                worst = (dt, k);
            }
        }
        assert!(worst.0 < budget, "key {:?} took {:?} (budget {:?})", worst.1, worst.0, budget);
        eprintln!("perf: worst key->frame {:?} on {:?}", worst.0, worst.1);
    }

    /// Same budget with a thousand notes on the file (every event re-reads their marks; every frame looks
    /// for the ones on screen and the one under the cursor).
    #[test]
    fn perf_budget_with_many_notes() {
        let budget = frame_budget();
        let dir = std::env::temp_dir().join(format!("tarae-perf-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.rs");
        let text: String = (0..20_000).map(|i| format!("let x{i} = {i}; // line {i}\n")).collect();
        std::fs::write(&path, &text).unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        let path = ed.doc().path.clone().unwrap();
        for i in 0..1000 {
            crate::notes::add(
                &mut ed,
                &path,
                i * 20..i * 20 + 3,
                crate::notes::By::Claude,
                "a note about this",
            );
        }
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 120, 40).unwrap();
        let mut worst = Duration::ZERO;
        for keys in
            [&["j"][..], &["j"], &["]", "n"], &["]", "n"], &["o"], &["esc"], &["u"], &["G"], &["[", "n"]]
        {
            let t = Instant::now();
            for k in keys {
                ed.handle_event(crate::event::Event::Key(k.parse().unwrap()));
            }
            buf.clear();
            render(&mut ed, &mut buf, 120, 40).unwrap();
            worst = worst.max(t.elapsed());
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(worst < budget, "worst event→frame {worst:?} with 1000 notes (budget {budget:?})");
        eprintln!("perf: worst event->frame with 1000 notes {worst:?}");
    }

    /// Same budget on an 8 MB file while a plugin runs away (the demo plugin's endless loop, then a command
    /// that gets the whole text): plugins have their own thread, and asking one copies nothing.
    #[test]
    fn perf_budget_with_a_runaway_plugin() {
        let budget = frame_budget();
        let dir = std::env::temp_dir().join(format!("tarae-perf-plugin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.wasm"), include_bytes!("../plugin-sdk/demo.wasm")).unwrap();
        let text: String =
            (0..240_000).map(|i| format!("line {i} 타래 fn main() {{ let x = {i}; }}   \n")).collect();
        assert!(text.len() > 8 << 20);
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str(&text);
        ed.plugins_sync(crate::plugin::Watch::new(Some(dir.clone())).changed().unwrap());
        while ed.plugin_command("spin").is_none() {
            let ev = ed.events.recv_timeout(Duration::from_secs(20)).expect("the plugin loads");
            ed.handle_event(ev);
        }
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 120, 40).unwrap();
        let mut worst = (Duration::ZERO, "");
        for (i, k) in
            ["j", "w", "x", "G", "g", "i", "a", "esc", "u", "C-d", "C-u", "e"].into_iter().enumerate()
        {
            if i == 0 {
                crate::typed::execute(&mut ed, "spin").unwrap();
                crate::typed::execute(&mut ed, "trim-trailing").unwrap();
            }
            let dt = key_to_frame(&mut ed, &mut buf, k, 120, 40);
            if dt > worst.0 {
                worst = (dt, k);
            }
        }
        assert!(
            worst.0 < budget,
            "key {:?} took {:?} with a plugin running (budget {budget:?})",
            worst.1,
            worst.0
        );
        eprintln!("perf: worst key->frame with a runaway plugin {:?} on {:?}", worst.0, worst.1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Same budget at the bottom of one huge top-level item (a 6000-method `impl`, 437 KB): the locals query
    /// must not read every sibling method above the screen on every frame.
    #[test]
    fn perf_budget_with_locals_in_a_huge_item() {
        let budget = frame_budget();
        let Ok(lang) = crate::syntax::Loader::global().load(crate::syntax::spec("rust").unwrap()) else {
            return;
        };
        let mut body = String::from("impl Big {\n");
        for i in 0..6_000 {
            body.push_str(&format!(
                "    fn m{i}(&self, count: u32) -> u32 {{ let x = count + {i}; x * count }}\n"
            ));
        }
        body.push_str("}\n");
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str(&body);
        let mut syn = crate::syntax::Syntax::new(lang);
        let job = syn.start_parse(&ed.docs[0].text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
        ed.docs[0].syntax = Some(syn);
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 160, 50).unwrap();
        let mut worst = (Duration::ZERO, "");
        for k in ["G", "k", "k", "w", "x", "C-u", "j", "i", "a", "esc"] {
            let dt = key_to_frame(&mut ed, &mut buf, k, 160, 50);
            if dt > worst.0 {
                worst = (dt, k);
            }
        }
        assert!(worst.0 < budget, "key {:?} took {:?} (budget {:?})", worst.1, worst.0, budget);
        eprintln!(
            "perf+locals: worst key->frame {:?} on {:?} (a {} KB item)",
            worst.0,
            worst.1,
            body.len() / 1024
        );
    }

    /// A long name loses its middle, not its end — the extension and the suffix that tells siblings apart
    /// stay; match marks follow the cut; wide characters count their cells.
    #[test]
    fn long_names_keep_their_ends() {
        let (t, m) = fit_middle("user_profile_settings_view_controller.tsx", &[0, 1, 38, 40], 24);
        assert_eq!(t, "user_profile_s…oller.tsx");
        assert_eq!(t.width(), 24);
        assert_eq!(m, [0, 1, 21, 23], "marks on both kept ends, shifted past the cut");
        assert_eq!(fit_middle("short.rs", &[], 24).0, "short.rs");
        // The end starts on a word when one begins a few characters back
        assert_eq!(fit_middle("user_profile_settings_view.spec.ts", &[], 16).0, "user_pro…spec.ts");
        assert_eq!(
            fit_middle("organization_membership_service_client_test.ts", &[], 18).0,
            "organizati…test.ts"
        );
        let (t, _) = fit_middle("타래_실타래_감기_모듈_테스트.rs", &[], 16);
        assert!(t.width() <= 16 && t.ends_with(".rs") && t.contains('…'), "{t}");
        assert_eq!(tree_guides(2), "│ │ ");
        assert_eq!(tree_guides(5), "│ │ │ ││", "past the third, one cell each");
    }

    /// The side panel grows with a wide terminal — up to 12 more columns, the code keeping 100.
    #[test]
    fn the_panel_is_wider_on_a_wide_terminal() {
        let mut ed = Editor::new(Config::default());
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, PathBuf::from("/x")));
        let rail = |ed: &Editor, w| Layout::new(ed, w, 40).rail_w;
        assert_eq!(rail(&ed, 112), 32);
        assert_eq!(rail(&ed, 140), 36);
        assert_eq!(rail(&ed, 200), 44);
    }

    #[test]
    fn cuts_keep_to_their_width() {
        assert_eq!(fit_ellipsis("terminal", 5), "term…");
        assert_eq!(fit_left("src/editor.rs", 8), "…itor.rs");
        assert_eq!(fit_left("타래타래/a.rs", 8), "…래/a.rs", "by display width");
        // No room: nothing — a lone `…` would still take a cell past the end
        assert_eq!(fit_ellipsis("exited 1", 0), "");
        assert_eq!(fit_left("src/a.rs", 0), "");
        assert_eq!(pad_left("타래", 6), "  타래");
        assert_eq!(duration(0.412), "412ms");
        assert_eq!(duration(f32::INFINITY), "infs", "a report's odd number is drawn, not a panic");
    }

    #[test]
    fn test_names_drop_their_shared_prefix() {
        let v = |x: &[&str]| short_names(x.iter().copied());
        assert_eq!(v(&["math::tests::adds", "math::tests::divides"]), ["adds", "divides"]);
        assert_eq!(v(&["AppTest.greets", "AppTest.Edge.empty"]), ["greets", "Edge.empty"]);
        assert_eq!(
            v(&["TestAdd", "TestAddNegative"]),
            ["TestAdd", "TestAddNegative"],
            "no stripping when not at a boundary"
        );
        assert_eq!(v(&["TestDiv::test_half"]), ["test_half"]);
        assert_eq!(v(&["test_add", "TestDiv::test_half"]), ["test_add", "TestDiv::test_half"]);
    }

    #[test]
    fn statusline_drops_least_important_first() {
        let seg = |t: &str, keep| Seg { text: t.into(), style: Style::default(), keep };
        let fit = |width| {
            let mut l = vec![seg(" NOR ", 255), seg("  tarae · main", 80), seg("  a.rs", 200)];
            let mut r = vec![seg("rust  ", 60), seg("utf-8 LF  ", 40), seg("1:1 ", 250)];
            fit_segments(&mut l, &mut r, width);
            l.iter().chain(&r).map(|s| s.text.clone()).collect::<Vec<_>>()
        };
        assert_eq!(fit(31), [" NOR ", "  tarae · main", "  a.rs", "1:1 "], "drops encoding, then language");
        // One cell less and `a.rs` would touch `1:1` — the branch goes instead
        assert_eq!(fit(30), [" NOR ", "  a.rs", "1:1 "]);
    }

    /// Same budget with 20k cursors (`%s` of a word on every line): a cell finds its range and cursor by
    /// halving, and copying them all a line down (`C`) sorts and merges once — both were per cursor.
    #[test]
    fn perf_budget_with_20k_cursors() {
        let budget = frame_budget();
        let text: String = (0..20_000).map(|i| format!("let x{i} = {i};\n// line {i}\n")).collect();
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str(&text);
        for k in ["%", "s", "l", "e", "t", "ret"] {
            ed.handle_key(k.parse().unwrap());
        }
        assert_eq!(ed.doc().selection().len(), 20_000);
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 120, 40).unwrap();
        // `C` itself: 20k copies, sorted and merged once (per copy it took seconds)
        let t = Instant::now();
        ed.handle_key("C".parse().unwrap());
        let copy = t.elapsed();
        assert_eq!(ed.doc().selection().len(), 40_000, "C copied every cursor");
        assert!(copy < budget * 10, "C took {copy:?} with 20k cursors (budget {:?})", budget * 10);
        // Frames over 40k cursors, moved about (moving each is the keys' own work — not timed)
        let mut worst = Duration::ZERO;
        for k in ["w", "e", "b"] {
            ed.handle_key(k.parse().unwrap());
            buf.clear();
            let t = Instant::now();
            render(&mut ed, &mut buf, 120, 40).unwrap();
            worst = worst.max(t.elapsed());
        }
        assert!(worst < budget, "a frame took {worst:?} with 40k cursors (budget {budget:?})");
        eprintln!("perf+cursors: worst frame {worst:?} with 40k cursors, C over 20k {copy:?}");
    }

    /// Same budget with syntax highlighting on — a big Rust file (only on machines with the grammar).
    #[test]
    fn perf_budget_with_syntax_highlighting() {
        let budget = frame_budget();
        let Ok(lang) = crate::syntax::Loader::global().load(crate::syntax::spec("rust").unwrap()) else {
            return;
        };
        let body: String = (0..20_000)
            .map(|i| {
                format!("fn f{i}(x: &str) -> Option<u32> {{ let s = \"타래 {i}\"; x.parse().ok() }} // c\n")
            })
            .collect();
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str(&body);
        let mut syn = crate::syntax::Syntax::new(lang);
        let job = syn.start_parse(&ed.docs[0].text);
        let (generation, t0) = (job.generation, Instant::now());
        syn.finish_parse(generation, job.run());
        let full_parse = t0.elapsed();
        ed.docs[0].syntax = Some(syn);
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 160, 50).unwrap();
        let mut worst = (Duration::ZERO, "");
        // Enter, o and O right after edits: the tree is behind, so the indentation reparses on the spot
        for k in [
            "j", "w", "x", "d", "u", "g", "e", "C-u", "%", ";", "i", "a", "ret", "esc", "o", "esc", "O",
            "esc",
        ] {
            let dt = key_to_frame(&mut ed, &mut buf, k, 160, 50);
            if dt > worst.0 {
                worst = (dt, k);
            }
        }
        assert!(worst.0 < budget, "key {:?} took {:?} (budget {:?})", worst.1, worst.0, budget);
        eprintln!(
            "perf+syntax: worst key->frame {:?} on {:?} (full parse of 20k lines {full_parse:?}, off main thread)",
            worst.0, worst.1
        );
    }

    /// Keys (sent as events — every event ends with the panel following the editor) → frame, worst of all.
    /// Like `key_to_frame`: the events once, the frame the fastest of 3 (scheduling noise from parallel tests).
    fn events_to_frames(ed: &mut Editor, keys: &[&str], w: u16, h: u16) -> (Duration, String) {
        let mut buf = Vec::with_capacity(1 << 16);
        render(ed, &mut buf, w, h).unwrap();
        // What goes to the terminal is worked out from the frame too (only what changed) — in the budget
        let mut screen = crate::screen::Screen::new();
        screen.present(&buf, w, h);
        let mut worst = (Duration::ZERO, String::new());
        for k in keys {
            let t = Instant::now();
            for k in k.split(' ') {
                ed.handle_event(crate::event::Event::Key(k.parse().unwrap()));
            }
            let events = t.elapsed();
            let frame = (0..3)
                .map(|_| {
                    buf.clear();
                    let t = Instant::now();
                    render(ed, &mut buf, w, h).unwrap();
                    t.elapsed()
                })
                .min()
                .unwrap();
            let t = Instant::now();
            screen.present(&buf, w, h);
            let frame = frame + t.elapsed();
            if events + frame > worst.0 {
                worst = (events + frame, k.to_string());
            }
        }
        worst
    }

    /// Same budget with the side panel on a 200k-file project, thousands of rows unfolded — its rows are
    /// built when a folder opens or closes, not on every frame.
    #[test]
    fn perf_budget_side_panel_with_200k_files() {
        let budget = frame_budget();
        let root = PathBuf::from("/nowhere/big");
        let rels: Vec<String> = (0..20)
            .flat_map(|a| (0..100).flat_map(move |b| (0..100).map(move |c| format!("d{a}/e{b}/f{c}.rs"))))
            .collect();
        let mut sb = crate::sidebar::Sidebar::with_files(root.clone(), rels);
        let dirs: Vec<String> =
            (0..20).map(|a| format!("d{a}")).chain((0..30).map(|b| format!("d0/e{b}"))).collect();
        sb.set_open(dirs.iter().map(String::as_str), true);
        let mut ed = Editor::new(Config::default());
        ed.docs[0].path = Some(root.join("d0/e29/f50.rs"));
        ed.docs[0].text = Rope::from_str("fn main() {}\n");
        ed.sidebar = Some(sb);
        let rows = ed.sidebar.as_ref().unwrap().lines().len();
        let keys = [
            "j", "j", "k", "C-d", "G", "g", "l", "l", "h", "h", "k", "ret", "esc", "j", "w", "x", "i", "a",
            "esc",
        ];
        let (worst, key) = events_to_frames(&mut ed, &keys, 180, 50);
        assert!(worst < budget, "key {key:?} took {worst:?} with {rows} panel rows (budget {budget:?})");
        eprintln!("perf+side panel: worst event->frame {worst:?} on {key:?} ({rows} rows of 200k files)");
    }

    /// Same budget typing in the file picker over 200k files — every key refilters them all and lays the
    /// best matches out as a tree.
    #[test]
    fn perf_budget_file_picker_typing_in_200k_files() {
        let budget = frame_budget();
        let root = PathBuf::from("/nowhere/big");
        let items: Vec<crate::picker::Item> = (0..20)
            .flat_map(|a| (0..100).flat_map(move |b| (0..100).map(move |c| format!("d{a}/e{b}/f{c}.rs"))))
            .map(|rel| crate::picker::Item {
                action: crate::picker::Action::Open(root.join(&rel)),
                label: rel,
                hint: String::new(),
                glyph: None,
            })
            .collect();
        let mut p = crate::picker::Picker::new("files", Vec::new(), true).with_tree(root, None);
        p.set_items(items.clone());
        let mut ed = Editor::new(Config::default());
        ed.picker = Some(p);
        let keys = ["d", "1", "e", "4", "f", "backspace", "backspace", "C-n", "C-n", "down"];
        let (worst, key) = events_to_frames(&mut ed, &keys, 180, 50);
        assert!(worst < budget, "key {key:?} took {worst:?} (budget {budget:?})");
        // As the editor opens it: filtered on a worker — a key only hands it over, a result lands on the
        // main loop (taken, laid out, drawn)
        let mut p = crate::picker::Picker::new("files", Vec::new(), true)
            .with_tree(PathBuf::from("/nowhere/big"), None);
        p.set_items(items);
        p.asynchronous = true;
        ed.picker = Some(p);
        let (typing, _) = events_to_frames(&mut ed, &["d", "1", "e", "4"], 180, 50);
        let mut buf = Vec::new();
        let mut landing = Duration::ZERO;
        while ed.picker.as_ref().is_some_and(|p| p.filtering) {
            let Some(ev) = ed.events.recv_timeout(Duration::from_secs(5)) else { panic!("no result") };
            let t = Instant::now();
            ed.handle_event(ev);
            buf.clear();
            render(&mut ed, &mut buf, 180, 50).unwrap();
            landing = landing.max(t.elapsed());
        }
        assert!(
            typing.max(landing) < budget,
            "a key {typing:?}, a result landing {landing:?} (budget {budget:?})"
        );
        eprintln!(
            "perf+file picker: worst key->frame {worst:?} on {key:?} in place; on a worker a key {typing:?}, a result {landing:?} (200k files)"
        );
    }

    /// Same budget with the git tab showing 20k new files — and a fresh status arriving (its rows are
    /// built then, once).
    #[test]
    fn perf_budget_git_tab_with_20k_changes() {
        let budget = frame_budget();
        let root = PathBuf::from("/nowhere/repo");
        let porcelain: String = (0..20_000).map(|i| format!("? new/d{}/f{i}.txt\0", i / 100)).collect();
        let status = |generation| {
            let mut st = crate::gitpanel::parse_v2(&porcelain);
            st.top = root.clone();
            st.generation = generation;
            st.with_rows() // (as the worker that reads it does)
        };
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str("fn main() {}\n");
        ed.git_status = Some(status(1));
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, root.clone()));
        let (worst, key) = events_to_frames(&mut ed, &["j", "j", "C-d", "G", "k", "g", "esc", "w"], 180, 50);
        assert!(worst < budget, "key {key:?} took {worst:?} (budget {budget:?})");
        // A new status: the rows are built again on the next event
        ed.git_status = Some(status(2));
        let (fresh, _) = events_to_frames(&mut ed, &["j"], 180, 50);
        assert!(fresh < budget, "a fresh status took {fresh:?} to show (budget {budget:?})");
        eprintln!(
            "perf+git tab: worst key->frame {worst:?} on {key:?}, a fresh status {fresh:?} (20k files)"
        );
    }

    /// Same budget with a program flooding the terminal panel — each batch of its output (a screen of
    /// colored lines, as the reader thread sends it) processed and drawn.
    #[test]
    fn perf_budget_terminal_flood() {
        let budget = frame_budget();
        let mut ed = Editor::new(Config::default());
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()], Vec::new());
        let end = Instant::now() + Duration::from_secs(5);
        while ed.terminal.is_none() && Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(100)) {
                ed.handle_event(ev);
            }
        }
        let Some(t) = ed.terminal.as_mut() else { panic!("the terminal didn't start") };
        t.focus = false;
        let batch: String = (0..200)
            .map(|i| {
                format!(
                    "\x1b[1;3{}m{i:>5}\x1b[0m compiling \x1b[32mcrate-{i}\x1b[0m v0.1.{i} 타래 …\r\n",
                    i % 8
                )
            })
            .collect();
        let mut buf = Vec::with_capacity(1 << 16);
        render(&mut ed, &mut buf, 180, 50).unwrap();
        let mut worst = Duration::ZERO;
        for _ in 0..30 {
            let t0 = Instant::now();
            ed.terminal.as_mut().unwrap().parser.process(batch.as_bytes());
            buf.clear();
            render(&mut ed, &mut buf, 180, 50).unwrap();
            worst = worst.max(t0.elapsed());
        }
        ed.terminal = None; // hangs it up
        assert!(worst < budget, "a batch of output took {worst:?} to show (budget {budget:?})");
        eprintln!("perf+terminal: worst output batch->frame {worst:?} ({} KB a batch)", batch.len() / 1024);
    }

    /// Same budget moving between merge conflicts in a 200k-line file (500 of them, the card on each).
    #[test]
    fn perf_budget_conflicts_in_a_huge_file() {
        let budget = frame_budget();
        let mut text = String::new();
        for i in 0..200_000 {
            if i % 400 == 0 {
                text.push_str("<<<<<<< HEAD\nours 타래\n=======\ntheirs\n>>>>>>> feature\n");
            }
            text.push_str(&format!("line {i} fn main() {{ let x = {i}; }}\n"));
        }
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = Rope::from_str(&text);
        let t0 = Instant::now();
        ed.conflicts_sync();
        let scan = t0.elapsed();
        assert_eq!(ed.doc().conflicts.len(), 500);
        let keys = ["] m", "] m", "] m", "[ m", "j", "k", "G", "[ m", "g g"];
        let (worst, key) = events_to_frames(&mut ed, &keys, 160, 50);
        assert!(worst < budget, "key {key:?} took {worst:?} (budget {budget:?})");
        eprintln!(
            "perf+conflicts: worst key->frame {worst:?} on {key:?} (a full scan {scan:?}, off main thread)"
        );
    }
}
