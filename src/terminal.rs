//! The built-in terminal — your shell in the bottom panel. `space T` shows it with the keys, and hides it (it keeps running); `C-\`
//! goes round: the code → the program → its view mode (`view.rs`: its scrollback and screen as a document,
//! held still — the program's output kept back until you catch up) → the code. The keys taken from the
//! program are the keymap's `[terminal]` rows. The wheel over it scrolls back in its view mode; a click in it
//! gives the program the keys, a click outside takes them back; a drag selects text and copies it. A program that asks for the mouse (`htop`, `vim`, `lazygit`) gets it instead,
//! and a full-screen one without it gets the wheel as arrow keys. A paste goes to it marked as one when it
//! asked for that (bracketed paste).
//!
//! A pseudo-terminal (openpty) runs the program in a process group of its own (`setsid`, the pty its terminal); `vt100` turns its output into a
//! screen of cells, drawn with the outer terminal's own palette. The main thread never waits on it: a reader
//! thread sends output as events, a writer thread takes the keys.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
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
    /// While its view mode lasts: the program's output, kept back — the screen holds still as you read it (the
    /// answers to what the program asks are sent at once all the same). Into the screen once caught up
    /// (`release`). At most one batch: its go-ahead is kept (`waiting`).
    held: Option<Vec<u8>>,
    /// The reader's go-aheads kept back while held (`output_acked`): it reads no more, the pty's buffer fills and
    /// the program waits on its write — a real terminal's flow control — until it's caught up (`release`).
    waiting: Vec<std::sync::mpsc::SyncSender<()>>,
    /// Odd while held, one more at each hold and release (and as it goes): the wait thread's drain counts only
    /// while nothing is held and none began or ended meanwhile — so the program's end comes after what the
    /// reader holds (`spawn`).
    holds: std::sync::Arc<std::sync::atomic::AtomicU64>,
    master: OwnedFd,
    writer: Sender<Vec<u8>>,
    generation: u64,
    size: (u16, u16),
    /// Its process group's leader (`setsid`) — hung up when it goes.
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

    /// The program's output onto the screen (kept back while held). What it asks of the terminal was answered as
    /// it was read (`answer`, the reader's).
    fn take(&mut self, bytes: &[u8]) {
        match self.held.as_mut() {
            Some(h) => h.extend_from_slice(bytes),
            None => self.parser.process(bytes),
        }
    }

    /// Its run (a replacement is another).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Held still from now: the program's output is kept back (`take`).
    pub fn hold(&mut self) {
        if self.held.is_none() {
            self.held = Some(Vec::new());
            self.holds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// What came while held goes onto the screen, and it's no longer held — scrolled back, it stays on the rows
    /// it showed (vt100 keeps a scrolled-back view where it is as rows come).
    pub fn release(&mut self) {
        if let Some(h) = self.held.take() {
            self.parser.process(&h);
            self.holds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // (the reader goes on: the program writes again)
        for go in self.waiting.drain(..) {
            let _ = go.send(());
        }
    }

    /// Output came while held (`● view · output waits`).
    pub fn came(&self) -> bool {
        self.held.as_ref().is_some_and(|h| !h.is_empty())
    }

    /// Held still now (its view mode) — the simulation checks it's held exactly while viewed.
    #[cfg(test)]
    pub fn holding(&self) -> bool {
        self.held.is_some()
    }

    /// Bytes kept back now (the tests and the simulation: one batch at most).
    #[cfg(test)]
    pub fn held_len(&self) -> usize {
        self.held.as_ref().map_or(0, Vec::len)
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
        // (shifted, as terminals send it: `S-pageup` to a full-screen program)
        Code::PageUp if key.shift => b"\x1b[5;2~".to_vec(),
        Code::PageDown if key.shift => b"\x1b[6;2~".to_vec(),
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

/// The terminal's environment additions, `inside` a sandbox (tarae's own — what it starts is in it too):
/// `GIT_OPTIONAL_LOCKS=0`, as Claude's runs have it (`sandbox::inside_env`) — a plain `git status` there
/// otherwise refreshes the index on the side, a write into the repository's `.git` (a session's
/// `.git/worktrees/<name>/index.lock`) the sandbox denies (#140).
fn inside_env(mut env: Vec<(String, String)>, inside: bool) -> Vec<(String, String)> {
    if inside && !env.iter().any(|(k, _)| k == "GIT_OPTIONAL_LOCKS") {
        env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
    }
    env
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
    // (close-on-exec from the start: no other program tarae starts holds the master, so when tarae goes —
    // killed too — the program here hangs up)
    let (master, slave) = crate::pty::open(rows, cols).map_err(|e| format!("pty: {e}"))?;
    let io = |fd: &OwnedFd| fd.try_clone().map(Stdio::from).map_err(|e| e.to_string());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("TARAE_SHOT_XDG")
        .env_remove("TARAE_UI")
        .stdin(io(&slave)?)
        .stdout(io(&slave)?)
        .stderr(io(&slave)?);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // (a script run in it finds this tarae's UI state: `tarae state`)
    if let Some(ui) = crate::uisocket::bound() {
        cmd.env("TARAE_UI", ui);
    }
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid, ioctl and `restore`'s setrlimit are async-signal-safe, called between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                // (the descriptor limit tarae started with — your shell's `ulimit -n` as outside tarae)
                crate::nofile::restore();
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
    let (eof, read_all) = std::sync::mpsc::channel::<()>();
    let reading = events.clone();
    // Keys → the program (a full pty buffer blocks this thread, never the editor) — and the reader's answers
    let (writer, keys) = std::sync::mpsc::channel::<Vec<u8>>();
    let answering = writer.clone();
    let holds = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let held_meanwhile = holds.clone();
    // The shell waited for as it ends — a program it left behind holding the terminal (one that ignored
    // the hangup) mustn't keep it a zombie (#131); then what's left to read, `DRAIN_AFTER_EXIT` at most — counted
    // only while nothing is held: while the view holds, the reader waits for a go-ahead and reaches no end, so
    // the end waits too, and comes after the batches it holds (a whole drain once the view lets go)
    crate::event::thread("tarae-term-wait", move || {
        use std::sync::atomic::Ordering::SeqCst;
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        loop {
            let seen = held_meanwhile.load(SeqCst);
            match read_all.recv_timeout(crate::spawn::DRAIN_AFTER_EXIT) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if seen % 2 == 1 || held_meanwhile.load(SeqCst) != seen => {}
                _ => break,
            }
        }
        let done = move |ed: &mut Editor| ended(ed, generation, code);
        let _ = events.send(crate::event::Event::Job(Box::new(done)));
    });
    let events = reading;
    crate::event::thread("tarae-term-read", move || {
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
                    // Its questions answered now, as it's read — before it waits for the last batch's go-ahead,
                    // kept while the view holds: a question never waits with the output (fish waits 10 s)
                    answer(&answering, &bytes);
                    // (an error: the batch was dropped unshown — the editor is going)
                    if let Some(rx) = shown.take() {
                        let _ = rx.recv();
                    }
                    let (done, rx) = std::sync::mpsc::sync_channel(1);
                    shown = Some(rx);
                    let apply = move |ed: &mut Editor| output_acked(ed, generation, &bytes, done);
                    if events.send(crate::event::Event::Job(Box::new(apply))).is_err() {
                        return;
                    }
                }
            }
        }
        // (its batches are queued before its end is told — held too: the wait thread waits while it's held)
        let _ = eof.send(());
    });
    let mut out = std::fs::File::from(master.try_clone().map_err(|e| e.to_string())?);
    crate::event::thread("tarae-term-keys", move || {
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
        held: None,
        waiting: Vec::new(),
        holds,
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

/// The reader's answers to what a batch it read asks of the terminal, sent to the program at once.
fn answer(writer: &Sender<Vec<u8>>, bytes: &[u8]) {
    for reply in answers(bytes) {
        let _ = writer.send(reply.to_vec());
    }
}

/// For the tests and the simulation, whose stand-in has no reader thread: what the reader does as it reads a
/// batch — its questions answered at once, before the batch is handed on (`output_acked`) or waits on a go-ahead.
#[cfg(test)]
pub(crate) fn reader_reads(ed: &Editor, bytes: &[u8]) {
    if let Some(t) = ed.session.terminal.as_ref() {
        answer(&t.writer, bytes);
    }
}

/// Output of run `generation` — kept until the terminal is in place if it comes first (the reader starts
/// before the worker that started the program hands it over).
pub(crate) fn output(ed: &mut Editor, generation: u64, bytes: &[u8]) {
    if let Some(t) = ed.session.terminal.as_mut().filter(|t| t.generation == generation) {
        t.take(bytes);
        // What was selected has likely moved (not while it's being dragged, nor while held still)
        if !t.selecting && t.held.is_none() {
            t.selection = None;
        }
    } else if ed.session.terminal_run == generation {
        ed.session.terminal_early.0.extend_from_slice(bytes);
    }
}

/// A reader's batch (`spawn`'s job): onto the screen or held (`output`) — its go-ahead (`done`: the reader waits
/// for it before it sends the next) at once, or, while held, kept until it's caught up (`release`): the reader
/// reads no more, the pty's buffer fills and the program's write waits — nothing piles up in tarae, at most one
/// batch is held, and taking it in costs no more than a batch ever does.
pub(crate) fn output_acked(
    ed: &mut Editor,
    generation: u64,
    bytes: &[u8],
    done: std::sync::mpsc::SyncSender<()>,
) {
    output(ed, generation, bytes);
    match ed.session.terminal.as_mut().filter(|t| t.generation == generation && t.held.is_some()) {
        Some(t) => t.waiting.push(done),
        None => {
            let _ = done.send(());
        }
    }
}

/// Run `generation` ended with `code`.
fn ended(ed: &mut Editor, generation: u64, code: i32) {
    if let Some(t) = ed.session.terminal.as_mut().filter(|t| t.generation == generation) {
        t.exited = Some(code);
        // (filling the screen, it goes on doing so — its last lines to read)
        if std::mem::take(&mut t.focus) && ed.zoom == Some(crate::sizes::Zoom::Bottom) {
            ed.bottom_focus = true;
        }
    } else if ed.session.terminal_run == generation {
        ed.session.terminal_early.1 = Some(code);
    }
}

/// The started terminal in place — with what it printed (and whether it ended) before.
fn install(ed: &mut Editor, mut t: Terminal) {
    let (bytes, exited) = std::mem::take(&mut ed.session.terminal_early);
    t.take(&bytes);
    if let Some(code) = exited {
        t.exited = Some(code);
        t.focus = false;
    }
    ed.session.terminal = Some(t);
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
    /// The program hangs up, and so does what has the terminal in front (a shell's job), as when a terminal
    /// window closes (its reader thread then reads the end and stops) — closing our end alone wouldn't do:
    /// the reader holds a copy. A shell passes it on to its other jobs; one that ignores it (`nohup`) runs
    /// on, its terminal gone. Killed, tarae can't: the master's last copy closes with it and the kernel
    /// hangs the shell up (`pty::open` — no other program has one).
    fn drop(&mut self) {
        // (held no more: its wait thread's drain counts again — its end, another run's now, is let go)
        if self.held.take().is_some() {
            self.holds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // (no process of its own — a test's stand-in: `-0` and `-1` would be this process group and everyone)
        if self.exited.is_some() || self.pid <= 1 {
            return;
        }
        // SAFETY: tcgetpgrp/getsid only read; the signals go to the process group we started (`setsid`) and
        // to the one in front on our pty — only while it's in that program's session.
        unsafe {
            let (program, front) = (self.pid as libc::pid_t, libc::tcgetpgrp(self.master.as_raw_fd()));
            if front > 0 && front != program && libc::getsid(front) == program {
                libc::kill(-front, libc::SIGHUP);
            }
            libc::kill(-program, libc::SIGHUP);
        }
    }
}

impl Editor {
    /// `space T` — the terminal with the keys: shown (your `$SHELL`, started if nothing runs there), brought
    /// in front of the slot's other tabs, given the keys. It hides (running on) only from inside — tarae's
    /// keys on its panel (#144: shown without the keys, it was hidden in front of the other tabs and given
    /// the keys behind them). From the program's keys `space T` is the program's.
    pub fn terminal_toggle(&mut self) {
        let front = self.slot() == Some(crate::sizes::Slot::Terminal);
        let inside = front && self.bottom_focus;
        if !inside && self.session.terminal.as_ref().is_some_and(|t| t.visible && t.exited.is_none()) {
            self.session.slot_front = Some(crate::sizes::Slot::Terminal);
            if let Some(t) = self.session.terminal.as_mut() {
                t.focus = true;
            }
            return;
        }
        match self.session.terminal.as_mut() {
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
        match self.session.terminal.as_mut() {
            Some(t) if t.exited.is_none() => {
                t.visible = true;
                t.focus = true;
                self.session.slot_front = Some(crate::sizes::Slot::Terminal);
            }
            _ => self.terminal_toggle(),
        }
    }

    /// Run `argv` in the terminal, replacing what ran there — started on a worker (a fork and exec stay off
    /// the keys' path); false if something's running there already (it isn't replaced).
    pub fn terminal_run(&mut self, argv: Vec<String>, env: Vec<(String, String)>) -> bool {
        if self.session.terminal.as_ref().is_some_and(|t| t.exited.is_none())
            && !self.session.terminal_starting
        {
            // Asked for a replacement explicitly? Only for an exited one — a live shell is never dropped here
            if let Some(t) = self.session.terminal.as_mut() {
                t.visible = true;
                t.focus = true;
            }
            self.session.slot_front = Some(crate::sizes::Slot::Terminal);
            return false;
        }
        let (rows, cols) = self.screen.terminal_size.get().unwrap_or((12, 80));
        self.terminal_generation += 1;
        self.session.terminal_early = Default::default();
        let generation = self.terminal_generation;
        self.session.terminal_run = generation;
        self.session.terminal_starting = true;
        let cwd = self.session.root.clone();
        let events = self.events.sender();
        self.events.jobs().spawn(move || {
            let env = inside_env(env, crate::sandbox::inside());
            let result = spawn(&argv, &cwd, &env, rows, cols, events, generation);
            move |ed: &mut Editor| {
                if ed.session.terminal_run != generation {
                    return;
                }
                ed.session.terminal_starting = false;
                match result {
                    Ok(t) => install(ed, t),
                    Err(e) => ed.set_error(format!("terminal: {e}")),
                }
            }
        });
        true
    }

    /// Keys while the terminal has them: the keymap's `[terminal]` rows run their commands (`C-\` → its view
    /// mode); `S-pageup`/`S-pagedown` scroll its past a page, as every terminal emulator's do (uxaudit Z2 — the
    /// next key the program gets brings it back down); the rest go to the program.
    pub fn terminal_key(&mut self, key: Key) -> bool {
        if !self.session.terminal.as_ref().is_some_and(|t| t.focus && t.visible && t.exited.is_none()) {
            return false;
        }
        if let Some(cmds) = self.keymaps.terminal_takes(key) {
            let cmds = cmds.to_vec();
            self.run(&cmds, None);
            return true;
        }
        let Some(t) = self.session.terminal.as_mut() else { return false };
        // (a full-screen program's alternate screen keeps no past: the key is the program's, as a terminal sends it)
        if key.shift
            && !key.ctrl
            && !key.alt
            && matches!(key.code, Code::PageUp | Code::PageDown)
            && !t.parser.screen().alternate_screen()
        {
            let page = t.parser.screen().size().0.max(1) as isize;
            self.terminal_scroll_by(if key.code == Code::PageUp { page } else { -page });
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

    /// `back` lines further back (forward when negative) — as far as it keeps (vt100 clamps).
    pub fn terminal_scroll_by(&mut self, back: isize) {
        if let Some(t) = self.session.terminal.as_mut() {
            let now = t.parser.screen().scrollback();
            t.parser.screen_mut().set_scrollback(now.saturating_add_signed(back));
        }
    }

    /// Pasted text while the terminal has the keys — marked as a paste when the program asked for that
    /// (a shell then doesn't run each line as it comes); newlines as the Enter key sends them. False if it
    /// isn't the terminal's.
    pub fn terminal_paste(&mut self, text: &str) -> bool {
        let Some(t) = self.session.terminal.as_mut().filter(|t| t.focus && t.visible && t.exited.is_none())
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
        // In its view mode the mouse is the view's, whoever asked for it: the program's screen is held still, maybe
        // scrolled back — a click there isn't on what the program drew now. The wheel scrolls the view, a press puts
        // its cursor on that cell, as a click in the code; a drag waits for P4 (as its selection)
        if crate::view::terminal_viewed(self) {
            let row = cell.map(|c| c.0 as usize);
            return match kind {
                MouseKind::ScrollUp | MouseKind::ScrollDown => {
                    crate::view::terminal_wheel(self, row, kind == MouseKind::ScrollUp)
                }
                MouseKind::Down => crate::view::terminal_click(self, cell),
                // (no silent drag: what selects there now, said)
                MouseKind::Drag => self
                    .set_status("a drag doesn't select in view mode yet — x or v selects, y copies, i types"),
                MouseKind::Up => {}
            };
        }
        let Some(t) = self.session.terminal.as_mut() else { return };
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
                    // (up: its view mode at the row under the pointer — tmux's copy mode by the wheel, §3.8)
                    t.selection = None;
                    crate::view::terminal_wheel(self, cell.map(|c| c.0 as usize), up);
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
                let what = match text.lines().count() {
                    0 | 1 => format!("{} characters", text.chars().count()),
                    n => format!("{n} lines"),
                };
                crate::clipboard::copy(&self.events.jobs(), text, what);
            }
        }
    }
}

/// A terminal with no program: what `output` gives it is its screen, what it sends goes to the receiver — for
/// tests and the simulation, which need no pty (its `Drop` signals nothing: no process of its own).
#[cfg(test)]
pub(crate) fn stand_in(
    ed: &mut Editor,
    rows: u16,
    cols: u16,
    scrollback: usize,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (writer, sent) = std::sync::mpsc::channel();
    let master = std::fs::File::open("/dev/null").expect("/dev/null").into();
    ed.terminal_generation += 1;
    ed.session.terminal_run = ed.terminal_generation;
    ed.session.terminal = Some(Terminal {
        parser: vt100::Parser::new(rows, cols, scrollback),
        focus: true,
        visible: true,
        title: "sh".into(),
        exited: None,
        selection: None,
        selecting: false,
        held: None,
        waiting: Vec::new(),
        holds: Default::default(),
        master,
        writer,
        generation: ed.terminal_generation,
        size: (rows, cols),
        pid: 0,
    });
    ed.session.slot_front = Some(crate::sizes::Slot::Terminal);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `test` (its name in this module) skips itself, saying why: it needs a real pty, and inside macOS's
    /// sandbox a pty's other end doesn't open (EPERM, measured — `sandbox::TERMINALS`) — the test would fail, or
    /// wait forever on output that never comes.
    fn no_pty_here(test: &str) -> bool {
        crate::sandbox::no_pty_here(&format!("terminal::tests::{test}"))
    }

    #[test]
    fn keys_as_terminal_bytes() {
        let k = |code, ctrl, alt| encode(Key { code, ctrl, alt, shift: false }, false).unwrap();
        assert_eq!(k(Code::Char('c'), true, false), [3], "C-c");
        assert_eq!(k(Code::Char('a'), false, true), [0x1b, b'a'], "A-a");
        assert_eq!(k(Code::Char('타'), false, false), "타".as_bytes());
        assert_eq!(k(Code::Up, false, false), b"\x1b[A");
        assert_eq!(
            encode(Key { code: Code::Up, ctrl: false, alt: false, shift: false }, true).unwrap(),
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
        if no_pty_here("a_program_on_a_pty") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        let argv =
            vec!["/bin/sh".to_string(), "-c".into(), "printf 'hello \\033[1mtarae\\033[0m'; exit 3".into()];
        ed.terminal_run(argv, Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ed.session.terminal.as_ref().is_none_or(|t| t.exited.is_none())
            && std::time::Instant::now() < end
        {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        let t = ed.session.terminal.as_ref().unwrap();
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
        if no_pty_here("a_program_asking_what_the_terminal_is_gets_an_answer") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        let script =
            r"stty -icanon -echo min 9 time 50; printf '\033[c'; head -c 9 | tr '\033' E; printf '|'";
        ed.terminal_run(vec!["/bin/sh".to_string(), "-c".into(), script.into()], Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ed.session.terminal.as_ref().is_none_or(|t| t.exited.is_none())
            && std::time::Instant::now() < end
        {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        let contents = ed.session.terminal.as_ref().unwrap().parser.screen().contents();
        assert!(contents.contains("E[?62;22c|"), "{contents:?}");
        assert_eq!(answers(b"x\x1b[0cy\x1b[c"), [b"\x1b[?62;22c"; 2], "both forms, each answered");
        assert!(answers(b"\x1b[>c\x1b[?1;2c").is_empty(), "not the secondary query, nor a reply");
    }

    /// #144: `space T` with the terminal shown but without the keys gives it the keys — in front of the
    /// slot or behind another tab alike. From inside (tarae's keys on its panel: its view mode) `space` is
    /// said, and `q` hides it. (The program is the test's own `sleep`.)
    #[test]
    fn space_t_gives_a_shown_terminal_the_keys_and_hides_it_only_from_inside() {
        if no_pty_here("space_t_gives_a_shown_terminal_the_keys_and_hides_it_only_from_inside") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), "sleep 10".into()], Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ed.session.terminal.is_none() && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
        let space_t = |ed: &mut Editor| {
            ed.handle_key(crate::key::Key::plain(crate::key::Code::Char(' ')));
            ed.handle_key(crate::key::Key::plain(crate::key::Code::Char('T')));
        };
        let state = |ed: &Editor| ed.session.terminal.as_ref().map(|t| (t.visible, t.focus)).unwrap();
        // Shown, in front, the keys back in the code
        ed.session.terminal.as_mut().unwrap().focus = false;
        assert_eq!(ed.slot(), Some(crate::sizes::Slot::Terminal));
        space_t(&mut ed);
        assert_eq!(state(&ed), (true, true), "shown without the keys: given them, not hidden");
        // From inside — its view mode: `space` said, `q` hides it, running on
        ed.bottom_take_keys();
        space_t(&mut ed);
        assert_eq!(state(&ed), (true, false), "space in view mode: said");
        // (its `T` waits for a character, as in the code: esc lets it go)
        ed.handle_key(crate::key::Key::plain(crate::key::Code::Esc));
        ed.handle_key(crate::key::Key::plain(crate::key::Code::Char('q')));
        assert_eq!(state(&ed), (false, false), "hidden from inside");
        assert!(ed.session.terminal.as_ref().unwrap().exited.is_none(), "running on");
        // Hidden: shown with the keys
        space_t(&mut ed);
        assert_eq!(state(&ed), (true, true));
        ed.session.terminal = None;
    }

    /// #140: inside a sandbox the terminal's `git status` leaves the index alone (`GIT_OPTIONAL_LOCKS=0`, as
    /// Claude's runs have it) — the side refresh is a write into `.git` the sandbox denies. Outside one
    /// nothing's added; a value the terminal is given stays. (A repository of the test's own; a file
    /// written again, same bytes, so status would refresh its stat in the index.)
    #[test]
    fn inside_a_sandbox_the_terminals_git_status_leaves_the_index_alone() {
        assert!(inside_env(Vec::new(), false).is_empty(), "outside one: nothing added");
        let given = vec![("GIT_OPTIONAL_LOCKS".to_string(), "1".to_string())];
        assert_eq!(inside_env(given.clone(), true), given, "a value it's given stays");
        // (said, not inherited: a test run inside tarae's sandbox has it from there already)
        assert_eq!(inside_env(Vec::new(), true), [("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string())]);
        // (the terminal is a pty)
        if no_pty_here("inside_a_sandbox_the_terminals_git_status_leaves_the_index_alone") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("term-git");
        let dir = scratch.to_path_buf();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.hooksPath=/dev/null"])
                .args(args)
                .current_dir(&dir)
                .env_remove("GIT_OPTIONAL_LOCKS")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        for args in [&["init", "-q"][..], &["add", "a.txt"], &["commit", "-q", "-m", "one"]] {
            git(args);
        }
        // `git status` in the terminal with `env`: whether it wrote the index
        let writes_index = |env: Vec<(String, String)>| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(dir.join("a.txt"), "one\n").unwrap();
            let before = std::fs::metadata(dir.join(".git/index")).unwrap().modified().unwrap();
            let (config, _) = crate::config::parse("");
            let mut ed = Editor::new(config);
            (ed.terminal_generation, ed.session.terminal_run) = (1, 1);
            let argv = ["/bin/sh".to_string(), "-c".into(), "git status --porcelain".into()];
            let t = spawn(&argv, &dir, &inside_env(env, true), 5, 60, ed.events.sender(), 1).unwrap();
            install(&mut ed, t);
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while ed.session.terminal.as_ref().is_none_or(|t| t.exited.is_none())
                && std::time::Instant::now() < end
            {
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(100)) {
                    ed.handle_event(ev);
                }
            }
            assert_eq!(ed.session.terminal.as_ref().and_then(|t| t.exited), Some(0));
            std::fs::metadata(dir.join(".git/index")).unwrap().modified().unwrap() != before
        };
        let refreshes = writes_index(vec![("GIT_OPTIONAL_LOCKS".into(), "1".into())]);
        let inside = writes_index(Vec::new());
        std::fs::remove_dir_all(&dir).ok();
        assert!(refreshes, "a status may refresh the index (else this test sees nothing)");
        assert!(!inside, "inside a sandbox: no write into .git");
    }

    /// Output (and the end) that comes before the terminal is in place isn't lost — the reader thread starts
    /// before the worker hands the terminal over, and a quick program prints (or exits) in between.
    #[test]
    fn output_before_the_terminal_is_in_place_is_kept() {
        if no_pty_here("output_before_the_terminal_is_in_place_is_kept") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        (ed.terminal_generation, ed.session.terminal_run) = (7, 7);
        let argv = ["/bin/sh".to_string(), "-c".into(), "sleep 5".into()];
        let t = spawn(&argv, Path::new("/"), &[], 5, 40, ed.events.sender(), 7).unwrap();
        output(&mut ed, 7, b"early \x1b[1mbird");
        ended(&mut ed, 7, 0);
        output(&mut ed, 6, b"an older run's");
        install(&mut ed, t);
        let t = ed.session.terminal.as_ref().unwrap();
        assert_eq!(t.parser.screen().contents().trim(), "early bird");
        assert_eq!(t.exited, Some(0));
    }

    /// The terminal going (a session ended, tarae quitting) hangs up the program in front on it, even under a
    /// shell that wouldn't pass the hangup on (`trap : HUP`) — that shell itself runs on, as it would when a
    /// terminal window closes.
    #[test]
    fn dropping_the_terminal_hangs_up_the_program_in_front() {
        if no_pty_here("dropping_the_terminal_hangs_up_the_program_in_front") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-terminal-hangup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        ed.session.root = dir.clone();
        ed.terminal_run(
            vec!["/bin/sh".into()],
            vec![("PS1".into(), "$ ".into()), ("HISTFILE".into(), "/dev/null".into())],
        );
        wait(&mut ed, |ed| ed.session.terminal.is_some());
        let job = dir.join("job.pid");
        let t = ed.session.terminal.as_ref().unwrap();
        // (caught, not ignored: an ignored signal would stay ignored in the job too)
        t.send(b"trap : HUP\r".to_vec());
        t.send(format!("sh -c 'echo $$ > {}; exec sleep 60'\r", job.display()).into_bytes());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&job).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                break pid;
            }
            assert!(std::time::Instant::now() < end, "the job never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let shell = t.pid as i32;
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) == 0 };
        assert!(alive(pid));
        ed.session.terminal = None;
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(pid) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let left = alive(pid);
        let shell_left = alive(shell);
        // SAFETY: the shell is this test's own child (started above), still running as it traps SIGHUP — its
        // job goes with it (its process group, in the shell's session).
        unsafe {
            if left && libc::getsid(pid) == shell {
                libc::kill(-libc::getpgid(pid), libc::SIGKILL);
            }
            libc::kill(shell, libc::SIGKILL);
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(!left, "the program in front ({pid}) lives on");
        assert!(shell_left, "the shell trapping SIGHUP runs on");
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
        wait_up_to(ed, 5, done)
    }

    fn wait_up_to(ed: &mut Editor, secs: u64, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(100)) {
                ed.handle_event(ev);
            }
        }
    }

    fn shows(ed: &Editor, s: &str) -> bool {
        ed.session.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains(s))
    }

    /// A FIFO in `dir` a test's program reads a line from to go on (`read -r _ < fifo`).
    fn fifo_in(dir: &std::path::Path) -> std::path::PathBuf {
        let fifo = dir.join("go");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-ended path of the test's own folder.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "{}", std::io::Error::last_os_error());
        fifo
    }

    /// The program waiting on `fifo` goes on: a line written once it has it open (5 s at most — never an open
    /// that waits for good on a program that's gone).
    fn let_go(fifo: &std::path::Path) {
        use std::os::unix::fs::OpenOptionsExt as _;
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut w = loop {
            match std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(fifo) {
                Ok(w) => break w,
                Err(e) if e.raw_os_error() == Some(libc::ENXIO) && std::time::Instant::now() < end => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(e) => panic!("the program never opened {} to read: {e}", fifo.display()),
            }
        };
        std::io::Write::write_all(&mut w, b"\n").unwrap();
    }

    /// A drag over a program that didn't ask for the mouse selects (backwards too) and copies on release;
    /// its next output clears the highlight. The drag comes once the program's output is all in: it ends
    /// with `end`, nothing after it — not even its newline, which the pty can hand over in a read of its
    /// own after the drag (CI, twice: first ` line` after `second`, then the `\r\n` after `end`), and any
    /// output after the release clears the highlight. Its next output is the test's to send: the program
    /// waits on a FIFO for it, then prints the newline and `more`.
    #[test]
    fn a_drag_selects_and_copies() {
        if no_pty_here("a_drag_selects_and_copies") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("terminal-drag");
        let fifo = fifo_in(&scratch);
        let script = format!(
            "printf 'hello world\\nsecond line\\nend'; read -r _ < '{}'; printf '\\nmore\\n'; sleep 30",
            fifo.display()
        );
        let contents = |ed: &Editor| ed.session.terminal.as_ref().map(|t| t.parser.screen().contents());
        let mut ed = run_until(&script, |ed| shows(ed, "end"));
        assert!(shows(&ed, "end"), "its output, all in: {:?}", contents(&ed));
        let selection = |ed: &Editor| ed.session.terminal.as_ref().unwrap().selection.is_some();
        ed.terminal_mouse(MouseKind::Down, Some((1, 5)), false);
        ed.terminal_mouse(MouseKind::Drag, Some((0, 6)), false);
        assert_eq!(ed.session.terminal.as_ref().unwrap().selected_text(), "world\nsecond");
        ed.terminal_mouse(MouseKind::Up, Some((0, 6)), false);
        wait(&mut ed, |ed| ed.toasts.iter().any(|t| t.text.starts_with("copied")));
        assert!(
            ed.toasts.iter().any(|t| t.text == "copied 2 lines"),
            "{:?}",
            ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        assert!(selection(&ed), "stays highlighted");
        // Its next output clears the highlight
        let_go(&fifo);
        wait(&mut ed, |ed| shows(ed, "more"));
        assert!(shows(&ed, "more"), "its next output: {:?}", contents(&ed));
        assert!(!selection(&ed), "its next output clears the highlight");
        // A click without a drag selects nothing
        ed.terminal_mouse(MouseKind::Down, Some((1, 5)), false);
        ed.terminal_mouse(MouseKind::Drag, Some((0, 6)), false);
        assert!(selection(&ed));
        ed.terminal_mouse(MouseKind::Down, Some((0, 0)), false);
        ed.terminal_mouse(MouseKind::Up, Some((0, 0)), false);
        assert!(!selection(&ed));
    }

    /// A program that asked for the mouse gets it (SGR here); a full-screen one that didn't gets the wheel as
    /// arrow keys.
    #[test]
    fn the_mouse_goes_to_a_program_that_asks() {
        if no_pty_here("the_mouse_goes_to_a_program_that_asks") {
            return;
        }
        let script = "stty raw -echo; printf '\\033[?1000h\\033[?1006hready'; cat -v";
        let mut ed = run_until(script, |ed| shows(ed, "ready"));
        ed.terminal_mouse(MouseKind::Down, Some((3, 7)), false);
        ed.terminal_mouse(MouseKind::Up, Some((3, 7)), false);
        wait(&mut ed, |ed| shows(ed, "^[[<0;8;4m"));
        assert!(
            shows(&ed, "^[[<0;8;4M^[[<0;8;4m"),
            "{:?}",
            ed.session.terminal.as_ref().unwrap().parser.screen().contents()
        );
        assert!(ed.session.terminal.as_ref().unwrap().selection.is_none(), "not a selection");
        // Alt with the press: tarae's own selection over it, nothing sent
        let sent = ed.session.terminal.as_ref().unwrap().parser.screen().contents();
        ed.terminal_mouse(MouseKind::Down, Some((0, 0)), true);
        ed.terminal_mouse(MouseKind::Drag, Some((0, 4)), false);
        ed.terminal_mouse(MouseKind::Up, Some((0, 4)), false);
        assert_eq!(ed.session.terminal.as_ref().unwrap().selected_text(), "ready");
        // (a moment for anything sent to come back)
        let end = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while let Some(ev) = ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()))
        {
            ed.handle_event(ev);
        }
        assert_eq!(
            ed.session.terminal.as_ref().unwrap().parser.screen().contents(),
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
        if no_pty_here("a_paste_is_marked_when_the_program_asks") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        // (raw: cat gets the end marker too — it has no newline after it)
        let script = "stty raw -echo; printf '\\033[?2004hready'; cat -v";
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), script.into()], Vec::new());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let shows = |ed: &Editor, s: &str| {
            ed.session.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains(s))
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
        let text = ed.session.terminal.as_ref().unwrap().parser.screen().contents();
        assert!(
            text.contains("^[[200~one^Mtwo^M^[[201~"),
            "newlines as Enter, between the markers: {text:?}"
        );
        assert_eq!(ed.doc().text.to_string(), "", "not the buffer's");
    }

    /// Keys go to the program while it has them (the buffer is untouched); `C-\\` goes round: its view mode, the
    /// code, the program again.
    #[test]
    fn keys_go_to_the_program_until_ctrl_backslash() {
        if no_pty_here("keys_go_to_the_program_until_ctrl_backslash") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        // (an interactive shell keeps its history on exit — not in the user's ~/.bash_history)
        ed.terminal_run(
            vec!["/bin/sh".into()],
            vec![("PS1".into(), "$ ".into()), ("HISTFILE".into(), "/dev/null".into())],
        );
        while ed.session.terminal.is_none() {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_secs(5)) {
                ed.handle_event(ev);
            }
        }
        let key = |c: char| Key { code: Code::Char(c), ctrl: false, alt: false, shift: false };
        for c in "echo hi-there".chars() {
            ed.handle_key(key(c));
        }
        ed.handle_key(Key::plain(Code::Enter));
        assert_eq!(ed.doc().text.to_string(), "", "the buffer never saw them");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // Its output on a line of its own — after the prompt, depending on how fast the shell started
        let seen = |ed: &Editor| {
            ed.session.terminal.as_ref().is_some_and(|t| {
                t.parser.screen().contents().lines().any(|l| l.ends_with("hi-there") && !l.contains("echo"))
            })
        };
        while !seen(&ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        assert!(seen(&ed), "{:?}", ed.session.terminal.as_ref().map(|t| t.parser.screen().contents()));
        let ctrl_backslash = Key { code: Code::Char('\\'), ctrl: true, alt: false, shift: false };
        ed.handle_key(ctrl_backslash);
        assert!(!ed.session.terminal.as_ref().unwrap().focus);
        assert!(crate::view::terminal_viewed(&ed), "its view mode");
        ed.handle_key(ctrl_backslash);
        assert!(!crate::view::terminal_viewed(&ed) && !ed.bottom_focus);
        ed.handle_key(key('i'));
        assert_eq!(ed.mode, crate::editor::Mode::Insert, "the code has the keys again");
        ed.handle_key(Key::plain(Code::Esc));
        // C-\\ from the code gives the keys back to it; `q` in its view mode hides it, the shell running on — as
        // every list or output panel's `q` closes it (uxaudit D2); `space T` shows it with the keys (#144)
        ed.handle_key(ctrl_backslash);
        assert!(ed.session.terminal.as_ref().unwrap().focus);
        ed.handle_key(ctrl_backslash);
        ed.handle_key(key('q'));
        let t = ed.session.terminal.as_ref().unwrap();
        assert!(!t.visible && t.exited.is_none(), "hidden, running");
        assert!(!ed.bottom_focus);
        for k in [' ', 'T'] {
            ed.handle_key(key(k));
        }
        let t = ed.session.terminal.as_ref().unwrap();
        assert!(t.visible && t.focus, "shown with the keys, the same shell");
        // `C-w q` in its view mode too (D3)
        ed.handle_key(ctrl_backslash);
        ed.handle_key(Key { code: Code::Char('w'), ctrl: true, alt: false, shift: false });
        ed.handle_key(key('q'));
        assert!(!ed.session.terminal.as_ref().unwrap().visible, "C-w q: hidden too");
    }

    /// `S-pageup`/`S-pagedown` scroll the terminal's past a page while its program has the keys — every terminal
    /// emulator's keys (uxaudit Z2) — the keys staying the program's; the next key it gets brings it back down.
    #[test]
    fn shift_page_keys_scroll_the_terminals_past() {
        if no_pty_here("shift_page_keys_scroll_the_terminals_past") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        ed.terminal_run(
            vec!["/bin/sh".into()],
            vec![("PS1".into(), "$ ".into()), ("HISTFILE".into(), "/dev/null".into())],
        );
        while ed.session.terminal.is_none() {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_secs(5)) {
                ed.handle_event(ev);
            }
        }
        for c in "i=0; while [ $i -lt 300 ]; do echo past$i; i=$((i+1)); done".chars() {
            ed.handle_key(Key::plain(Code::Char(c)));
        }
        ed.handle_key(Key::plain(Code::Enter));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let done = |ed: &Editor| {
            ed.session.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains("past299"))
        };
        while !done(&ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        assert!(done(&ed), "its output came");
        let back = |ed: &Editor| ed.session.terminal.as_ref().unwrap().parser.screen().scrollback();
        let page = ed.session.terminal.as_ref().unwrap().parser.screen().size().0 as usize;
        ed.handle_key("S-pageup".parse().unwrap());
        ed.handle_key("S-pageup".parse().unwrap());
        assert_eq!(back(&ed), 2 * page, "two pages back");
        ed.handle_key("S-pagedown".parse().unwrap());
        assert_eq!(back(&ed), page, "one forward");
        assert!(ed.session.terminal.as_ref().unwrap().focus, "the keys still the program's");
        ed.handle_key("pageup".parse().unwrap());
        assert_eq!(back(&ed), 0, "a key the program gets: back down");
        // A full-screen program (its alternate screen keeps no past): the key is the program's, as a terminal
        // sends it — the tty's echo shows it arrive (the line cleared first: the pageup above left the shell a `~`)
        ed.handle_key("C-u".parse().unwrap());
        for c in "printf '\\033[?1049h'; cat".chars() {
            ed.handle_key(Key::plain(Code::Char(c)));
        }
        ed.handle_key(Key::plain(Code::Enter));
        let full = |ed: &Editor| ed.session.terminal.as_ref().unwrap().parser.screen().alternate_screen();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !full(&ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        assert!(full(&ed), "on its alternate screen");
        ed.handle_key("S-pageup".parse().unwrap());
        let came =
            |ed: &Editor| ed.session.terminal.as_ref().unwrap().parser.screen().contents().contains("[5;2~");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !came(&ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(200)) {
                ed.handle_event(ev);
            }
        }
        assert!(came(&ed), "{:?}", ed.session.terminal.as_ref().map(|t| t.parser.screen().contents()));
        assert_eq!(back(&ed), 0, "nothing scrolled");
    }

    /// fish's case, for real: while viewed, a question in the batch the reader reads as it waits for the last
    /// one's go-ahead is answered at once — the program (bash, its terminal raw) gets the answer within its 3 s and
    /// says ANSWERED, seen once caught up. Answered only as the batch is handed on, it would say TIMEOUT.
    #[test]
    fn a_question_read_while_the_view_holds_is_answered_at_once() {
        if no_pty_here("a_question_read_while_the_view_holds_is_answered_at_once") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("terminal-question-held");
        let fifo = fifo_in(&scratch);
        let script = format!(
            "exec bash -c 'read -r _ < \"{}\"; stty -icanon -echo; echo first-batch; sleep 0.5; printf \"\\033[c\"; \
             if IFS= read -r -d c -t 3 _; then echo ANSWERED; else echo TIMEOUT; fi; sleep 30'",
            fifo.display()
        );
        let mut ed = run_until(&script, |ed| ed.session.terminal.is_some());
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false, shift: false });
        assert!(crate::view::terminal_viewed(&ed));
        let_go(&fifo);
        // (viewed past bash's 3 s: the first batch held, the question in the reader's next)
        let end = std::time::Instant::now() + std::time::Duration::from_millis(3600);
        while std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(ed.session.terminal.as_ref().unwrap().came(), "output held while viewed");
        ed.handle_key(Key::plain(Code::Esc));
        wait(&mut ed, |ed| shows(ed, "ANSWERED") || shows(ed, "TIMEOUT"));
        let contents = ed.session.terminal.as_ref().map(|t| t.parser.screen().contents());
        assert!(shows(&ed, "ANSWERED"), "answered while held: {contents:?}");
    }

    /// A program ending while viewed with output still in the reader (it read the next batch and waits for the
    /// held one's go-ahead, so it reaches no end): its end waits too — past the drain's 2 s, still not told — and
    /// once the view lets go comes after that output, every line on the screen as it's told. It leaves a child
    /// holding the pty: where the pty outlives its session's leader (Linux) there's no end of file, and the end
    /// comes a drain's time after the view let go; macOS revokes it as the leader exits.
    #[test]
    fn a_program_ending_while_viewed_ends_after_its_output() {
        if no_pty_here("a_program_ending_while_viewed_ends_after_its_output") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("terminal-end-held");
        let fifo = fifo_in(&scratch);
        let script = format!(
            "read -r _ < '{}'; (trap '' HUP; sleep 8) & echo first-words; sleep 0.3; echo last-words; exit 3",
            fifo.display()
        );
        let mut ed = run_until(&script, |ed| ed.session.terminal.is_some());
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false, shift: false });
        let_go(&fifo);
        // (the drain's 2 s, and some: the program long gone)
        let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let t = ed.session.terminal.as_ref().unwrap();
        assert_eq!(t.exited, None, "its end waits with its output");
        assert!(t.came() && !shows(&ed, "first-words"), "its output still held");
        ed.handle_key(Key::plain(Code::Esc));
        wait(&mut ed, |ed| ed.session.terminal.as_ref().is_some_and(|t| t.exited.is_some()));
        assert_eq!(ed.session.terminal.as_ref().unwrap().exited, Some(3), "told once the view let go");
        let contents = ed.session.terminal.as_ref().map(|t| t.parser.screen().contents());
        assert!(
            shows(&ed, "first-words") && shows(&ed, "last-words"),
            "its output before its end: {contents:?}"
        );
    }

    /// A terminal replaced while viewed (dropped — a session ending — and another started): the old one's output
    /// held and its reader waiting go with it; its program (one that ignores the hangup) ends later, once the new
    /// one runs — that end is another run's and touches nothing of the new one, which runs on and shows its own.
    #[test]
    fn a_terminal_replaced_while_viewed_leaves_the_new_one_alone() {
        if no_pty_here("a_terminal_replaced_while_viewed_leaves_the_new_one_alone") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("terminal-replaced-held");
        let fifo = fifo_in(&scratch);
        let script = format!(
            "trap '' HUP; read -r _ < '{}'; (trap '' HUP; sleep 14) & echo old-a; sleep 0.3; echo old-b; sleep 1.5; exit 5",
            fifo.display()
        );
        let mut ed = run_until(&script, |ed| ed.session.terminal.is_some());
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false, shift: false });
        let_go(&fifo);
        wait(&mut ed, |ed| ed.session.terminal.as_ref().is_some_and(|t| t.came()));
        assert!(ed.session.terminal.as_ref().unwrap().came(), "its output held, the next in its reader");
        // (the old run's wait thread holds the other count: it's gone once that thread has sent its end)
        let old = ed.session.terminal.as_ref().unwrap().holds.clone();
        // (replaced: dropped, another started)
        ed.session.terminal = None;
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), "echo fresh-one; sleep 30".into()], Vec::new());
        wait(&mut ed, |ed| shows(ed, "fresh-one"));
        assert!(!crate::view::terminal_viewed(&ed), "the old one's view gone with it");
        // (the old program ends now, the new one running: its end told — to nobody. Its exit 1.8 s after
        // `let_go`, then `DRAIN_AFTER_EXIT` — a drop that left `holds` odd keeps that thread waiting as long as
        // the orphan holds the terminal, 14 s, where it stays open: Linux; macOS revokes it as the shell exits)
        wait_up_to(&mut ed, 10, |_| std::sync::Arc::strong_count(&old) == 1);
        assert_eq!(std::sync::Arc::strong_count(&old), 1, "the old run's end was sent: its view let it go");
        while let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
        let t = ed.session.terminal.as_ref().unwrap();
        assert_eq!(t.exited, None, "the new one runs on");
        assert!(shows(&ed, "fresh-one") && !shows(&ed, "old-b"), "its own screen");
    }

    /// A flooding program (`yes`) while viewed: the reader's go-ahead is kept, so it reads no more — what tarae
    /// holds stays one batch, the pty's buffer fills and the program's write waits there (unread output on the
    /// master, nothing more taken in). `G` sends the go-ahead: the program writes again and the next batch comes
    /// (held again, still viewed); leaving lets it run.
    #[test]
    fn a_flood_waits_on_its_write_while_viewed() {
        if no_pty_here("a_flood_waits_on_its_write_while_viewed") {
            return;
        }
        let mut ed = run_until("yes flood-line", |ed| shows(ed, "flood-line"));
        assert!(shows(&ed, "flood-line"));
        // (the main loop's events — the reader's batches: `n` of them, or `ms` gone)
        let batches = |ed: &mut Editor, n: usize, ms: u64| {
            let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            let mut taken = 0;
            while taken < n && std::time::Instant::now() < end {
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(100)) {
                    ed.handle_event(ev);
                    taken += 1;
                }
            }
        };
        let held_and_full = |ed: &Editor| {
            ed.session
                .terminal
                .as_ref()
                .is_some_and(|t| t.came() && !t.waiting.is_empty() && ready(t.master.as_raw_fd()))
        };
        ed.handle_key(Key { code: Code::Char('\\'), ctrl: true, alt: false, shift: false });
        assert!(crate::view::terminal_viewed(&ed));
        wait(&mut ed, held_and_full);
        let t = ed.session.terminal.as_ref().unwrap();
        let held = t.held_len();
        assert!(t.came() && !t.waiting.is_empty(), "a batch held, its go-ahead kept");
        assert!(held <= BATCH + 64 * 1024, "one batch at most: {held} bytes");
        assert!(ready(t.master.as_raw_fd()), "the program's next output waits unread in the pty");
        // (what doesn't happen is seen only over a while — the one window here; a slow machine can't make it fail)
        batches(&mut ed, 3, 300);
        assert_eq!(
            ed.session.terminal.as_ref().unwrap().held_len(),
            held,
            "nothing more taken in: the program waits"
        );
        ed.handle_key(Key::plain(Code::Char('G')));
        let t = ed.session.terminal.as_ref().unwrap();
        assert!(!t.came() && t.waiting.is_empty(), "caught up: the go-ahead sent");
        wait(&mut ed, |ed| ed.session.terminal.as_ref().is_some_and(|t| t.came()));
        assert!(
            ed.session.terminal.as_ref().unwrap().came(),
            "the program wrote again: the next batch, held"
        );
        ed.handle_key(Key::plain(Code::Esc));
        let left = |ed: &Editor| {
            let t = ed.session.terminal.as_ref().unwrap();
            !crate::view::terminal_viewed(ed) && !t.came() && t.waiting.is_empty()
        };
        assert!(left(&ed), "left: it runs");
        batches(&mut ed, 3, 5000);
        assert!(left(&ed), "its batches go ahead as they come");
    }

    /// #58: a `:preflight` started while the terminal holds the bottom slot shows at once — its tab in the
    /// slot's row, with the running badge — the terminal staying in front.
    #[test]
    fn a_preflight_behind_the_terminal_shows_its_tab_at_once() {
        if no_pty_here("a_preflight_behind_the_terminal_shows_its_tab_at_once") {
            return;
        }
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        let dir = std::env::temp_dir().join(format!("tarae-preflight-behind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ed.session.root = std::fs::canonicalize(&dir).unwrap();
        ed.terminal_run(vec!["/bin/sh".into()], vec![("HISTFILE".into(), "/dev/null".into())]);
        while ed.session.terminal.is_none() {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_secs(5)) {
                ed.handle_event(ev);
            }
        }
        assert_eq!(ed.slot(), Some(crate::sizes::Slot::Terminal));
        let step = |name: &str, cmd: &str| crate::preflight::Step {
            name: name.into(),
            cmd: cmd.into(),
            note: None,
            from: ".tarae.toml".into(),
            full: false,
        };
        ed.config.preflight = vec![step("wait", "sleep 2")];
        ed.preflight_start(crate::preflight::Set::Quick).unwrap();
        // At once — no event handled since
        let tab = ed.slot_tabs().into_iter().find(|t| t.slot == crate::sizes::Slot::Task);
        assert!(
            tab.as_ref().is_some_and(|t| t.label == "preflight" && t.badge == crate::chrome::Badge::Running),
            "{:?}",
            tab.map(|t| (t.label, t.badge))
        );
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 120, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        let row = s.lines().find(|l| l.contains("terminal")).unwrap_or_default();
        assert!(row.contains("preflight"), "its tab beside the terminal's: {s}");
        assert_eq!(ed.slot(), Some(crate::sizes::Slot::Terminal), "the terminal stays in front");
        ed.session.task_run = None;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `C-w j` puts tarae's keys on the terminal (its view mode, not the program's — `C-w z` can be typed there),
    /// `C-w z` fills the screen with it, `i` types in it; `C-\\` then leaves the keys on it (its view mode), and Esc
    /// goes back to the code — which ends it.
    #[test]
    fn the_terminal_fills_the_screen_and_its_escape_stays_on_it() {
        if no_pty_here("the_terminal_fills_the_screen_and_its_escape_stays_on_it") {
            return;
        }
        use crate::sizes::Zoom;
        let (config, _) = crate::config::parse("");
        let mut ed = Editor::new(config);
        ed.terminal_run(
            vec!["/bin/sh".into()],
            vec![("PS1".into(), "$ ".into()), ("HISTFILE".into(), "/dev/null".into())],
        );
        while ed.session.terminal.is_none() {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_secs(5)) {
                ed.handle_event(ev);
            }
        }
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
        };
        let key = |c: char| Key { code: Code::Char(c), ctrl: false, alt: false, shift: false };
        let ctrl = |c: char| Key { code: Code::Char(c), ctrl: true, alt: false, shift: false };
        let focus = |ed: &Editor| ed.session.terminal.as_ref().unwrap().focus;
        draw(&mut ed);
        let small = ed.screen.bottom_h;
        // `C-\\` and Esc: back to the code
        ed.handle_key(ctrl('\\'));
        ed.handle_key(Key::plain(Code::Esc));
        assert!(!focus(&ed) && !ed.bottom_focus);
        for k in [ctrl('w'), key('j'), ctrl('w'), key('z')] {
            ed.handle_key(k);
        }
        assert!(ed.bottom_focus && !focus(&ed), "tarae's keys on it");
        assert!(crate::view::terminal_viewed(&ed), "its view mode");
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
        assert!(!focus(&ed) && ed.bottom_focus, "C-\\: the keys stay on it, its view mode");
        assert_eq!(ed.zoomed(), Some(Zoom::Bottom));
        ed.handle_key(Key::plain(Code::Esc));
        assert!(!ed.bottom_focus);
        assert_eq!(ed.zoom, None, "back to the code: it ended");
        draw(&mut ed);
        assert_eq!(ed.screen.bottom_h, small);
    }
}
