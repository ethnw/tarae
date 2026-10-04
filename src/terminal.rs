//! The built-in terminal — your shell (or Claude Code, when there's no zellij or tmux to open it beside
//! tarae) in the bottom panel. `space T` shows it with the keys, and hides it (it keeps running); `C-\`
//! moves the keys between the code and it (`editor.terminal-escape` inside it, a keymap command outside).
//! The wheel over it scrolls back; a click in it gives it the keys, a click outside takes them back; a drag
//! selects text and copies it. A program that asks for the mouse (`htop`, `vim`, `lazygit`) gets it instead,
//! and a full-screen one without it gets the wheel as arrow keys. A paste goes to it marked as one when it
//! asked for that (bracketed paste).
//!
//! A pseudo-terminal (openpty) runs the program in a session of its own; `vt100` turns its output into a
//! screen of cells, drawn with the outer terminal's own palette. The main thread never waits on it: a reader
//! thread sends output as events, a writer thread takes the keys.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use crate::editor::Editor;
use crate::event::MouseKind;
use crate::key::{Code, Key};

pub struct Terminal {
    pub parser: vt100::Parser,
    /// Keys go to it.
    pub focus: bool,
    /// Showing in the bottom panel (hidden, it keeps running).
    pub visible: bool,
    /// What runs (`zsh`, `claude`).
    pub title: String,
    /// It exited, with this code.
    pub exited: Option<i32>,
    /// Text selected with the mouse — screen cells (row, column), where the drag began → where it is — and
    /// whether the drag is still going.
    pub selection: Option<((u16, u16), (u16, u16))>,
    pub selecting: bool,
    master: OwnedFd,
    writer: Sender<Vec<u8>>,
    generation: u64,
    size: (u16, u16),
    /// Its session's leader (the session is its process group) — hung up when it goes.
    pid: u32,
}

impl Terminal {
    /// Its program's process, while it runs (`:procs`).
    pub fn pid(&self) -> Option<u32> {
        self.exited.is_none().then_some(self.pid)
    }

    /// Rows × columns the program sees — the panel's; it's told when that changes (SIGWINCH).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(10));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        self.parser.screen_mut().set_size(rows, cols);
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ on our own pty master with a valid winsize.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    fn send(&self, bytes: Vec<u8>) {
        let _ = self.writer.send(bytes);
    }

    /// The program's output onto the screen — and the answers to what it asks the terminal.
    fn take(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        for reply in answers(bytes) {
            self.send(reply.to_vec());
        }
    }

    /// The selection, first cell to last, in reading order (on the screen as it is now — it may have
    /// shrunk since).
    pub fn selected(&self) -> Option<((u16, u16), (u16, u16))> {
        let (rows, cols) = self.parser.screen().size();
        let fit = |(r, c): (u16, u16)| (r.min(rows.saturating_sub(1)), c.min(cols.saturating_sub(1)));
        self.selection.map(|(a, b)| (fit(a), fit(b))).map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
    }

    /// The text selected (lines as the program wrote them — a wrapped one stays one).
    pub fn selected_text(&self) -> String {
        let Some((a, b)) = self.selected() else { return String::new() };
        self.parser.screen().contents_between(a.0, a.1, b.0, b.1 + 1)
    }
}

/// A mouse event as the bytes a program that asked for the mouse reads — in its encoding (SGR, or the old
/// one-byte-per-number one, which can't say a column past 223); None when its mode doesn't want it.
fn mouse_bytes(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    kind: MouseKind,
    (row, col): (u16, u16),
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode as M;
    let wanted = match kind {
        MouseKind::Down | MouseKind::ScrollUp | MouseKind::ScrollDown => mode != M::None,
        MouseKind::Up => matches!(mode, M::PressRelease | M::ButtonMotion | M::AnyMotion),
        MouseKind::Drag => matches!(mode, M::ButtonMotion | M::AnyMotion),
    };
    if !wanted {
        return None;
    }
    let (button, press) = match kind {
        MouseKind::Down => (0u32, true),
        MouseKind::Drag => (32, true),
        MouseKind::Up => (0, false),
        MouseKind::ScrollUp => (64, true),
        MouseKind::ScrollDown => (65, true),
    };
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            Some(format!("\x1b[<{button};{x};{y}{}", if press { 'M' } else { 'm' }).into_bytes())
        }
        enc => {
            let b = if press { button } else { 3 };
            let mut out = b"\x1b[M".to_vec();
            for v in [b, x, y] {
                let v = v + 32;
                match enc {
                    vt100::MouseProtocolEncoding::Utf8 => {
                        out.extend(char::from_u32(v)?.to_string().bytes());
                    }
                    _ => out.push(u8::try_from(v).ok()?),
                }
            }
            Some(out)
        }
    }
}

/// A key as the bytes a terminal sends for it (`app_cursor`: arrows as `ESC O A`, when the program asked).
pub fn encode(key: Key, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: u8| if app_cursor { vec![0x1b, b'O', c] } else { vec![0x1b, b'[', c] };
    let mut out = match key.code {
        Code::Char(c) if key.ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                _ => return None,
            };
            vec![b]
        }
        Code::Char(c) => c.to_string().into_bytes(),
        Code::Enter => vec![b'\r'],
        Code::Esc => vec![0x1b],
        Code::Tab => vec![b'\t'],
        Code::BackTab => b"\x1b[Z".to_vec(),
        Code::Backspace => vec![0x7f],
        Code::Delete => b"\x1b[3~".to_vec(),
        Code::Up => arrow(b'A'),
        Code::Down => arrow(b'B'),
        Code::Right => arrow(b'C'),
        Code::Left => arrow(b'D'),
        Code::Home => arrow(b'H'),
        Code::End => arrow(b'F'),
        Code::PageUp => b"\x1b[5~".to_vec(),
        Code::PageDown => b"\x1b[6~".to_vec(),
        Code::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + n - 1],
        Code::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return None,
            };
            format!("\x1b[{code}~").into_bytes()
        }
    };
    // Alt = ESC before it
    if key.alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Starts `argv` in `cwd` on a new pseudo-terminal of `rows` × `cols`, `env` added.
fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    rows: u16,
    cols: u16,
    events: crate::event::Tx,
    generation: u64,
) -> Result<Terminal, String> {
    let (mut master, mut slave) = (0, 0);
    let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty fills two fds; the name/termios pointers may be null. (Mutable pointers: macOS wants
    // them, Linux's const ones take them too.)
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(ws),
        )
    };
    if r != 0 {
        return Err(format!("openpty: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: both fds were just opened by openpty and are owned by nobody else.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let io = |fd: &OwnedFd| fd.try_clone().map(Stdio::from).map_err(|e| e.to_string());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("TARAE_SHOT_XDG")
        .stdin(io(&slave)?)
        .stdout(io(&slave)?)
        .stderr(io(&slave)?);
    for (k, v) in env {
        cmd.env(k, v);
    }
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid and ioctl are async-signal-safe, called between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
    let pid = child.id();
    // Our copies of the slave side go now (the command holds three) — on Linux the master only reads
    // end-of-file once no one has the slave open, so an exited program would never look exited
    drop(cmd);
    drop(slave);
    // Output → events (batched by what one read returns); the end → its exit code
    let mut reader = std::fs::File::from(master.try_clone().map_err(|e| e.to_string())?);
    let fd = reader.as_raw_fd();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        // One batch at a time on the main loop: the next is read while it's shown and sent once it has
        // been. A program writing faster than the screen reads it (vt100 takes ~10 ms a MB) then waits on
        // its terminal, the pty full — instead of batches piling up in the queue the keys go through too.
        let mut shown: Option<std::sync::mpsc::Receiver<()>> = None;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // What's already there comes along (a flood is a few events, not one per read)
                    let mut bytes = buf[..n].to_vec();
                    while bytes.len() < BATCH && ready(fd) {
                        match reader.read(&mut buf) {
                            Ok(n) if n > 0 => bytes.extend_from_slice(&buf[..n]),
                            _ => break,
                        }
                    }
                    // (an error: the batch was dropped unshown — the editor is going)
                    if let Some(rx) = shown.take() {
                        let _ = rx.recv();
                    }
                    let (done, rx) = std::sync::mpsc::sync_channel(1);
                    shown = Some(rx);
                    let apply = move |ed: &mut Editor| {
                        output(ed, generation, &bytes);
                        let _ = done.send(());
                    };
                    if events.send(crate::event::Event::Job(Box::new(apply))).is_err() {
                        return;
                    }
                }
            }
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let done = move |ed: &mut Editor| ended(ed, generation, code);
        let _ = events.send(crate::event::Event::Job(Box::new(done)));
    });
    // Keys → the program (a full pty buffer blocks this thread, never the editor)
    let (writer, keys) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut out = std::fs::File::from(master.try_clone().map_err(|e| e.to_string())?);
    std::thread::spawn(move || {
        for bytes in keys {
            if out.write_all(&bytes).is_err() {
                break;
            }
        }
    });
    let title = Path::new(&argv[0]).file_name().map_or(argv[0].clone(), |n| n.to_string_lossy().into_owned());
    Ok(Terminal {
        parser: vt100::Parser::new(rows, cols, 5000),
        focus: true,
        visible: true,
        title,
        exited: None,
        selection: None,
        selecting: false,
        master,
        writer,
        generation,
        size: (rows, cols),
        pid,
    })
}

/// What a terminal answers to what the program asks of it in `bytes`: the Primary Device Attributes query
/// (`CSI c` / `CSI 0 c`) — a VT220 with ANSI color, as many terminals say. Unanswered, fish waits 10 s for
/// it before its first prompt (measured: a blank panel, then "could not read response to Primary Device
/// Attribute query").
fn answers(bytes: &[u8]) -> Vec<&'static [u8]> {
    const DA1: &[u8] = b"\x1b[?62;22c";
    let asks = |q: &[u8]| bytes.windows(q.len()).filter(|w| *w == q).count();
    std::iter::repeat_n(DA1, asks(b"\x1b[c") + asks(b"\x1b[0c")).collect()
}

/// Output of run `generation` — kept until the terminal is in place if it comes first (the reader starts
/// before the worker that started the program hands it over).
fn output(ed: &mut Editor, generation: u64, bytes: &[u8]) {
    if let Some(t) = ed.lane.terminal.as_mut().filter(|t| t.generation == generation) {
        t.take(bytes);
        // What was selected has likely moved (not while it's being dragged)
        if !t.selecting {
            t.selection = None;
        }
    } else if ed.lane.terminal_run == generation {
        ed.lane.terminal_early.0.extend_from_slice(bytes);
    }
}

/// Run `generation` ended with `code`.
fn ended(ed: &mut Editor, generation: u64, code: i32) {
    if let Some(t) = ed.lane.terminal.as_mut().filter(|t| t.generation == generation) {
        t.exited = Some(code);
        // (filling the screen, it goes on doing so — its last lines to read)
        if std::mem::take(&mut t.focus) && ed.zoom == Some(crate::sizes::Zoom::Bottom) {
            ed.bottom_focus = true;
        }
    } else if ed.lane.terminal_run == generation {
        ed.lane.terminal_early.1 = Some(code);
    }
}

/// The started terminal in place — with what it printed (and whether it ended) before.
fn install(ed: &mut Editor, mut t: Terminal) {
    let (bytes, exited) = std::mem::take(&mut ed.lane.terminal_early);
    t.take(&bytes);
    if let Some(code) = exited {
        t.exited = Some(code);
        t.focus = false;
    }
    ed.lane.terminal = Some(t);
}

/// Output read in one go at most — a few ms for the main loop to take in.
const BATCH: usize = 256 * 1024;

/// More to read right now on `fd`?
pub(crate) fn ready(fd: std::os::fd::RawFd) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one pollfd, no wait.
    unsafe { libc::poll(&mut p, 1, 0) > 0 && p.revents & libc::POLLIN != 0 }
}

impl Drop for Terminal {
    /// The program and everything it started hang up (its reader thread then reads the end and stops) —
    /// closing our end alone wouldn't do: the reader holds a copy.
    fn drop(&mut self) {
        if self.exited.is_none() {
            // SAFETY: a signal to the process group of the session we started for it.
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGHUP);
            }
        }
    }
}

impl Editor {
    /// `space T` — showing: hide it (it keeps running); hidden: show it with the keys (your `$SHELL`, started
    /// if nothing runs there).
    pub fn terminal_toggle(&mut self) {
        // (shown but behind another tab of the slot: to the front, with the keys)
        let front = self.slot() == Some(crate::sizes::Slot::Terminal);
        if !front && self.lane.terminal.as_ref().is_some_and(|t| t.visible && t.exited.is_none()) {
            self.lane.slot_front = Some(crate::sizes::Slot::Terminal);
            if let Some(t) = self.lane.terminal.as_mut() {
                t.focus = true;
            }
            return;
        }
        match self.lane.terminal.as_mut() {
            Some(t) if t.visible => {
                t.visible = false;
                t.focus = false;
            }
            Some(t) if t.exited.is_none() => {
                t.visible = true;
                t.focus = true;
            }
            _ => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                self.terminal_run(vec![shell], Vec::new());
            }
        }
    }

    /// `C-\` in the code — the keys to the terminal (showing it; starting a shell if nothing runs there).
    pub fn terminal_focus(&mut self) {
        match self.lane.terminal.as_mut() {
            Some(t) if t.exited.is_none() => {
                t.visible = true;
                t.focus = true;
                self.lane.slot_front = Some(crate::sizes::Slot::Terminal);
            }
            _ => self.terminal_toggle(),
        }
    }

    /// Run `argv` in the terminal, replacing what ran there — started on a worker (a fork and exec stay off
    /// the keys' path); false if something's running there already (it isn't replaced).
    pub fn terminal_run(&mut self, argv: Vec<String>, env: Vec<(String, String)>) -> bool {
        if self.lane.terminal.as_ref().is_some_and(|t| t.exited.is_none()) && !self.lane.terminal_starting {
            // Asked for a replacement explicitly? Only for an exited one — a live shell is never dropped here
            if let Some(t) = self.lane.terminal.as_mut() {
                t.visible = true;
                t.focus = true;
            }
            self.lane.slot_front = Some(crate::sizes::Slot::Terminal);
            return false;
        }
        let (rows, cols) = self.screen.terminal_size.get().unwrap_or((12, 80));
        self.terminal_generation += 1;
        self.lane.terminal_early = Default::default();
        let generation = self.terminal_generation;
        self.lane.terminal_run = generation;
        self.lane.terminal_starting = true;
        let cwd = self.lane.root.clone();
        let events = self.events.sender();
        self.events.jobs().spawn(move || {
            let result = spawn(&argv, &cwd, &env, rows, cols, events, generation);
            move |ed: &mut Editor| {
                if ed.lane.terminal_run != generation {
                    return;
                }
                ed.lane.terminal_starting = false;
                match result {
                    Ok(t) => install(ed, t),
                    Err(e) => ed.set_error(format!("terminal: {e}")),
                }
            }
        });
        true
    }

    /// Keys while the terminal has them: the escape key (`editor.terminal-escape`, `C-\`) gives them back to
    /// the code; the rest go to the program.
    pub fn terminal_key(&mut self, key: Key) -> bool {
        let escape: Option<Key> = self.config.terminal_escape.parse().ok();
        let Some(t) = self.lane.terminal.as_mut().filter(|t| t.focus && t.visible && t.exited.is_none())
        else {
            return false;
        };
        if Some(key) == escape {
            t.focus = false;
            // Filling the screen (`C-w z`): tarae's keys stay on it — the code isn't in sight
            self.bottom_focus = self.zoom == Some(crate::sizes::Zoom::Bottom);
            return true;
        }
        // Typing scrolls back to the bottom
        t.parser.screen_mut().set_scrollback(0);
        t.selection = None;
        let app_cursor = t.parser.screen().application_cursor();
        if let Some(bytes) = encode(key, app_cursor) {
            t.send(bytes);
        }
        true
    }

    /// Pasted text while the terminal has the keys — marked as a paste when the program asked for that
    /// (a shell then doesn't run each line as it comes); newlines as the Enter key sends them. False if it
    /// isn't the terminal's.
    pub fn terminal_paste(&mut self, text: &str) -> bool {
        let Some(t) = self.lane.terminal.as_mut().filter(|t| t.focus && t.visible && t.exited.is_none())
        else {
            return false;
        };
        t.parser.screen_mut().set_scrollback(0);
        t.selection = None;
        let body = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if t.parser.screen().bracketed_paste() {
            // The end marker inside the text would end the paste early — it can't be pasted
            format!("\x1b[200~{}\x1b[201~", body.replace("\x1b[201~", ""))
        } else {
            body
        };
        t.send(bytes.into_bytes());
        true
    }

    /// The mouse over the terminal's screen, at `cell` (row, column; None = its title row) — or anywhere,
    /// while a selection is being dragged (clamped to it). A program that asked for the mouse gets it; else
    /// the wheel scrolls back (or, full-screen, is arrow keys), a click gives it the keys and a drag selects,
    /// copied when it ends.
    /// Alt with the press: tarae's own selection even over a program that has the mouse (most terminals
    /// keep shift-drag for their own selection, across the whole screen).
    pub fn terminal_mouse(&mut self, kind: MouseKind, cell: Option<(u16, u16)>, alt: bool) {
        let Some(t) = self.lane.terminal.as_mut() else { return };
        let screen = t.parser.screen();
        let (rows, cols) = screen.size();
        let cell = cell.map(|(r, c)| (r.min(rows.saturating_sub(1)), c.min(cols.saturating_sub(1))));
        let live = t.exited.is_none();
        let (mode, encoding) = (screen.mouse_protocol_mode(), screen.mouse_protocol_encoding());
        let (full_screen, app_cursor) = (screen.alternate_screen(), screen.application_cursor());
        let ours = alt && kind == MouseKind::Down;
        if live && !t.selecting && !ours && mode != vt100::MouseProtocolMode::None {
            if kind == MouseKind::Down {
                t.focus = true;
            }
            // (what its mode doesn't ask for is dropped)
            if let Some(bytes) = cell.and_then(|c| mouse_bytes(mode, encoding, kind, c)) {
                t.send(bytes);
            }
            return;
        }
        match kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let up = kind == MouseKind::ScrollUp;
                if live && full_screen {
                    let key = Key::plain(if up { Code::Up } else { Code::Down });
                    t.send(encode(key, app_cursor).unwrap_or_default().repeat(3));
                } else {
                    t.selection = None;
                    self.terminal_scroll(up);
                }
            }
            MouseKind::Down => {
                t.focus = live;
                t.selection = cell.map(|c| (c, c));
                t.selecting = cell.is_some();
            }
            MouseKind::Drag => {
                if let (true, Some(sel), Some(c)) = (t.selecting, t.selection.as_mut(), cell) {
                    sel.1 = c;
                }
            }
            MouseKind::Up => {
                if !std::mem::take(&mut t.selecting) {
                    return;
                }
                // A click, not a drag: nothing selected
                let text =
                    if t.selection.is_some_and(|(a, b)| a != b) { t.selected_text() } else { String::new() };
                if text.trim().is_empty() {
                    t.selection = None;
                    return;
                }
                let lines = text.lines().count();
                crate::clipboard::copy(&self.events.jobs(), text.clone());
                self.set_success(match lines {
                    0 | 1 => format!("copied {} characters", text.chars().count()),
                    n => format!("copied {n} lines"),
                });
            }
        }
    }

    /// The wheel over the terminal: scroll back (up) / forward (down) three lines.
    pub fn terminal_scroll(&mut self, up: bool) {
        self.terminal_scroll_by(if up { 3 } else { -3 });
    }

    /// `back` lines further back (forward when negative) — as far as it keeps (vt100 clamps).
    pub fn terminal_scroll_by(&mut self, back: isize) {
        if let Some(t) = self.lane.terminal.as_mut() {
            let now = t.parser.screen().scrollback();
            t.parser.screen_mut().set_scrollback(now.saturating_add_signed(back));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_as_terminal_bytes() {
        let k = |code, ctrl, alt| encode(Key { code, ctrl, alt }, false).unwrap();
        assert_eq!(k(Code::Char('c'), true, false), [3], "C-c");
        assert_eq!(k(Code::Char('a'), false, true), [0x1b, b'a'], "A-a");
        assert_eq!(k(Code::Char('타'), false, false), "타".as_bytes());
        assert_eq!(k(Code::Up, false, false), b"\x1b[A");
        assert_eq!(
            encode(Key { code: Code::Up, ctrl: false, alt: false }, true).unwrap(),
            b"\x1bOA",
            "app cursor"
        );
        assert_eq!(k(Code::Enter, false, false), b"\r");
        assert_eq!(k(Code::Backspace, false, false), [0x7f]);
        assert_eq!(k(Code::F(5), false, false), b"\x1b[15~");
    }

    /// A real program on a real pty: what it prints lands on the screen, bold and all; its exit code is kept.
    #[test]
    fn a_program_on_a_pty() {
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        let argv =
            vec!["/bin/sh".to_string(), "-c".into(), "printf 'hello \\033[1mtarae\\033[0m'; exit 3".into()];
        ed.terminal_run(argv, Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ed.lane.terminal.as_ref().is_none_or(|t| t.exited.is_none()) && std::time::Instant::now() < end
        {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        let t = ed.lane.terminal.as_ref().unwrap();
        assert_eq!(t.exited, Some(3));
        let screen = t.parser.screen();
        assert!(screen.contents().starts_with("hello tarae"), "{:?}", screen.contents());
        assert!(screen.cell(0, 6).is_some_and(|c| c.bold()), "bold");
        assert!(!t.focus, "an exited program gives the keys back");
    }

    /// A program that asks the terminal what it is (Primary Device Attributes — fish does, and waits 10 s for
    /// the answer before its first prompt) gets an answer at once. The stand-in asks, reads the reply raw (5 s
    /// at most) and prints it with ESC as `E`.
    #[test]
    fn a_program_asking_what_the_terminal_is_gets_an_answer() {
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        let script =
            r"stty -icanon -echo min 9 time 50; printf '\033[c'; head -c 9 | tr '\033' E; printf '|'";
        ed.terminal_run(vec!["/bin/sh".to_string(), "-c".into(), script.into()], Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ed.lane.terminal.as_ref().is_none_or(|t| t.exited.is_none()) && std::time::Instant::now() < end
        {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        let contents = ed.lane.terminal.as_ref().unwrap().parser.screen().contents();
        assert!(contents.contains("E[?62;22c|"), "{contents:?}");
        assert_eq!(answers(b"x\x1b[0cy\x1b[c"), [b"\x1b[?62;22c"; 2], "both forms, each answered");
        assert!(answers(b"\x1b[>c\x1b[?1;2c").is_empty(), "not the secondary query, nor a reply");
    }

    /// Output (and the end) that comes before the terminal is in place isn't lost — the reader thread starts
    /// before the worker hands the terminal over, and a quick program prints (or exits) in between.
    #[test]
    fn output_before_the_terminal_is_in_place_is_kept() {
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        (ed.terminal_generation, ed.lane.terminal_run) = (7, 7);
        let argv = ["/bin/sh".to_string(), "-c".into(), "sleep 5".into()];
        let t = spawn(&argv, Path::new("/"), &[], 5, 40, ed.events.sender(), 7).unwrap();
        output(&mut ed, 7, b"early \x1b[1mbird");
        ended(&mut ed, 7, 0);
        output(&mut ed, 6, b"an older run's");
        install(&mut ed, t);
        let t = ed.lane.terminal.as_ref().unwrap();
        assert_eq!(t.parser.screen().contents().trim(), "early bird");
        assert_eq!(t.exited, Some(0));
    }

    #[test]
    fn mouse_as_the_program_reads_it() {
        use vt100::MouseProtocolEncoding as E;
        use vt100::MouseProtocolMode as M;
        let b = |mode, enc, kind, cell| {
            mouse_bytes(mode, enc, kind, cell).map(|v| String::from_utf8_lossy(&v).into_owned())
        };
        assert_eq!(b(M::PressRelease, E::Sgr, MouseKind::Down, (2, 9)).as_deref(), Some("\x1b[<0;10;3M"));
        assert_eq!(b(M::PressRelease, E::Sgr, MouseKind::Up, (2, 9)).as_deref(), Some("\x1b[<0;10;3m"));
        assert_eq!(b(M::ButtonMotion, E::Sgr, MouseKind::Drag, (0, 0)).as_deref(), Some("\x1b[<32;1;1M"));
        assert_eq!(b(M::Press, E::Sgr, MouseKind::ScrollDown, (0, 0)).as_deref(), Some("\x1b[<65;1;1M"));
        assert_eq!(b(M::Press, E::Sgr, MouseKind::Up, (0, 0)), None, "X10 mode: presses only");
        assert_eq!(b(M::PressRelease, E::Sgr, MouseKind::Drag, (0, 0)), None, "no motion asked for");
        assert_eq!(
            mouse_bytes(M::PressRelease, E::Default, MouseKind::Up, (0, 4)).unwrap(),
            [0x1b, b'[', b'M', 32 + 3, 32 + 5, 32 + 1],
            "the old encoding: release is button 3"
        );
        assert_eq!(
            mouse_bytes(M::PressRelease, E::Default, MouseKind::Down, (0, 300)),
            None,
            "past column 223"
        );
        assert!(
            mouse_bytes(M::PressRelease, E::Utf8, MouseKind::Down, (0, 300)).is_some(),
            "UTF-8 can say it"
        );
    }

    /// Run `script` in the terminal and handle events until `done` holds (5 s at most).
    fn run_until(script: &str, done: impl Fn(&Editor) -> bool) -> Editor {
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), script.into()], Vec::new());
        wait(&mut ed, done);
        ed
    }

    fn wait(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(100)) {
                ed.handle_event(ev);
            }
        }
    }

    fn shows(ed: &Editor, s: &str) -> bool {
        ed.lane.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains(s))
    }

    /// A drag over a program that didn't ask for the mouse selects (backwards too) and copies on release;
    /// its next output clears the highlight.
    #[test]
    fn a_drag_selects_and_copies() {
        let mut ed = run_until("printf 'hello world\\nsecond line\\n'; sleep 30", |ed| shows(ed, "second"));
        ed.terminal_mouse(MouseKind::Down, Some((1, 5)), false);
        ed.terminal_mouse(MouseKind::Drag, Some((0, 6)), false);
        assert_eq!(ed.lane.terminal.as_ref().unwrap().selected_text(), "world\nsecond");
        ed.terminal_mouse(MouseKind::Up, Some((0, 6)), false);
        assert!(
            ed.toasts.iter().any(|t| t.text == "copied 2 lines"),
            "{:?}",
            ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        assert!(ed.lane.terminal.as_ref().unwrap().selection.is_some(), "stays highlighted");
        // A click without a drag selects nothing
        ed.terminal_mouse(MouseKind::Down, Some((0, 0)), false);
        ed.terminal_mouse(MouseKind::Up, Some((0, 0)), false);
        assert!(ed.lane.terminal.as_ref().unwrap().selection.is_none());
    }

    /// A program that asked for the mouse gets it (SGR here); a full-screen one that didn't gets the wheel as
    /// arrow keys.
    #[test]
    fn the_mouse_goes_to_a_program_that_asks() {
        let script = "stty raw -echo; printf '\\033[?1000h\\033[?1006hready'; cat -v";
        let mut ed = run_until(script, |ed| shows(ed, "ready"));
        ed.terminal_mouse(MouseKind::Down, Some((3, 7)), false);
        ed.terminal_mouse(MouseKind::Up, Some((3, 7)), false);
        wait(&mut ed, |ed| shows(ed, "^[[<0;8;4m"));
        assert!(
            shows(&ed, "^[[<0;8;4M^[[<0;8;4m"),
            "{:?}",
            ed.lane.terminal.as_ref().unwrap().parser.screen().contents()
        );
        assert!(ed.lane.terminal.as_ref().unwrap().selection.is_none(), "not a selection");
        // Alt with the press: tarae's own selection over it, nothing sent
        let sent = ed.lane.terminal.as_ref().unwrap().parser.screen().contents();
        ed.terminal_mouse(MouseKind::Down, Some((0, 0)), true);
        ed.terminal_mouse(MouseKind::Drag, Some((0, 4)), false);
        ed.terminal_mouse(MouseKind::Up, Some((0, 4)), false);
        assert_eq!(ed.lane.terminal.as_ref().unwrap().selected_text(), "ready");
        // (a moment for anything sent to come back)
        let end = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while let Some(ev) = ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()))
        {
            ed.handle_event(ev);
        }
        assert_eq!(
            ed.lane.terminal.as_ref().unwrap().parser.screen().contents(),
            sent,
            "the program got nothing"
        );
        let script = "stty raw -echo; printf '\\033[?1049hfull'; cat -v";
        let mut ed = run_until(script, |ed| shows(ed, "full"));
        ed.terminal_mouse(MouseKind::ScrollUp, Some((0, 0)), false);
        wait(&mut ed, |ed| shows(ed, "^[[A^[[A^[[A"));
        assert!(shows(&ed, "^[[A^[[A^[[A"));
    }

    /// A paste goes to the program with the keys — between the paste markers once it asked for them
    /// (`ESC [?2004h`), newlines as Enter.
    #[test]
    fn a_paste_is_marked_when_the_program_asks() {
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        // (raw: cat gets the end marker too — it has no newline after it)
        let script = "stty raw -echo; printf '\\033[?2004hready'; cat -v";
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), script.into()], Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let shows = |ed: &Editor, s: &str| {
            ed.lane.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains(s))
        };
        while !shows(&ed, "ready") && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        ed.handle_event(crate::event::Event::Paste("one\ntwo\n".into()));
        while !shows(&ed, "^[[201~") && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        let text = ed.lane.terminal.as_ref().unwrap().parser.screen().contents();
        assert!(
            text.contains("^[[200~one^Mtwo^M^[[201~"),
            "newlines as Enter, between the markers: {text:?}"
        );
        assert_eq!(ed.doc().text.to_string(), "", "not the buffer's");
    }

    /// Keys go to the program while it has them (the buffer is untouched); `C-\\` gives them back.
    #[test]
    fn keys_go_to_the_program_until_ctrl_backslash() {
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        // (an interactive shell keeps its history on exit — not in the user's ~/.bash_history)
        ed.terminal_run(
            vec!["/bin/sh".into()],
            vec![("PS1".into(), "$ ".into()), ("HISTFILE".into(), "/dev/null".into())],
        );
        while ed.lane.terminal.is_none() {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_secs(5)) {
                ed.handle_event(ev);
            }
        }
        let key = |c: char| Key { code: Code::Char(c), ctrl: false, alt: false };
        for c in "echo hi-there".chars() {
            ed.handle_key(key(c));
        }
        ed.handle_key(Key::plain(Code::Enter));
        assert_eq!(ed.doc().text.to_string(), "", "the buffer never saw them");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // Its output on a line of its own — after the prompt, depending on how fast the shell started
        let seen = |ed: &Editor| {
            ed.lane.terminal.as_ref().is_some_and(|t| {
                t.parser.screen().contents().lines().any(|l| l.ends_with("hi-there") && !l.contains("echo"))
            })
        };
        while !seen(&ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        assert!(seen(&ed), "{:?}", ed.lane.terminal.as_ref().map(|t| t.parser.screen().contents()));
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false });
        assert!(!ed.lane.terminal.as_ref().unwrap().focus);
        ed.handle_key(key('i'));
        assert_eq!(ed.mode, crate::editor::Mode::Insert, "the code has the keys again");
        ed.handle_key(Key::plain(Code::Esc));
        // C-\\ from the code gives the keys back to it; space T hides it, again shows it
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false });
        assert!(ed.lane.terminal.as_ref().unwrap().focus);
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false });
        for k in [' ', 'T'] {
            ed.handle_key(key(k));
        }
        assert!(!ed.lane.terminal.as_ref().unwrap().visible, "hidden");
        for k in [' ', 'T'] {
            ed.handle_key(key(k));
        }
        let t = ed.lane.terminal.as_ref().unwrap();
        assert!(t.visible && t.focus, "shown with the keys, the same shell");
    }

    /// `C-w j` puts tarae's keys on the terminal (not the program's — `C-w z` can be typed there), `C-w z`
    /// fills the screen with it, `i` types in it; its escape key then leaves the keys on it while it fills
    /// the screen, and Esc goes back to the code — which ends it.
    #[test]
    fn the_terminal_fills_the_screen_and_its_escape_stays_on_it() {
        use crate::sizes::Zoom;
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        ed.terminal_run(
            vec!["/bin/sh".into()],
            vec![("PS1".into(), "$ ".into()), ("HISTFILE".into(), "/dev/null".into())],
        );
        while ed.lane.terminal.is_none() {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_secs(5)) {
                ed.handle_event(ev);
            }
        }
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
        };
        let key = |c: char| Key { code: Code::Char(c), ctrl: false, alt: false };
        let ctrl = |c: char| Key { code: Code::Char(c), ctrl: true, alt: false };
        let focus = |ed: &Editor| ed.lane.terminal.as_ref().unwrap().focus;
        draw(&mut ed);
        let small = ed.screen.bottom_h;
        // Its escape, not filling the screen: back to the code
        ed.handle_key(ctrl('\\'));
        assert!(!focus(&ed) && !ed.bottom_focus);
        for k in [ctrl('w'), key('j'), ctrl('w'), key('z')] {
            ed.handle_key(k);
        }
        assert!(ed.bottom_focus && !focus(&ed), "tarae's keys on it");
        draw(&mut ed);
        assert_eq!(ed.zoomed(), Some(Zoom::Bottom));
        assert_eq!(ed.screen.bottom_h, ed.screen.body_h, "all the rows");
        ed.handle_key(key('i'));
        assert!(focus(&ed) && !ed.bottom_focus, "the program's keys");
        for c in "echo zoomed-in".chars() {
            ed.handle_key(key(c));
        }
        ed.handle_key(Key::plain(Code::Enter));
        assert_eq!(ed.doc().text.to_string(), "", "the buffer never saw them");
        assert_eq!(ed.zoomed(), Some(Zoom::Bottom), "typing in it, it still fills the screen");
        ed.handle_key(ctrl('\\'));
        assert!(!focus(&ed) && ed.bottom_focus, "its escape: the keys stay on it");
        assert_eq!(ed.zoomed(), Some(Zoom::Bottom));
        ed.handle_key(Key::plain(Code::Esc));
        assert!(!ed.bottom_focus);
        assert_eq!(ed.zoom, None, "back to the code: it ended");
        draw(&mut ed);
        assert_eq!(ed.screen.bottom_h, small);
    }
}
