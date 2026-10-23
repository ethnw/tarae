//! `cargo xtask shot out.png [keys …]` — the real screen as a PNG: tarae on a pty, read back through `vt100`
//! (as `tests/e2e.rs` does), drawn cell by cell the way a terminal draws them. The tarae is the `shot`
//! profile's, built first (release's behavior; incremental, so seconds after an edit).
//!
//! `cargo xtask shot --live out.png [--pid N]` — instead, the frame a running tarae drew last (`live`).
//!
//! Keys are sent in order, 0.35 s apart (`\r` = Enter, `\t` = Tab, `\x1b` = Esc, `\xHH` = any byte; everything
//! else, Korean too, goes out as UTF-8); `@3` waits 3 s taking screen updates; `@?ready` waits until `ready` is
//! on screen (a language server indexing …) — at most 120 s, or `@30?ready` 30; `@!ready` (`@30!ready`) until it's
//! gone from it. `@~300` sends the key after it and keeps every frame drawn in the 300 ms after it: `out-01.png` (the
//! screen before the key), `out-02.png` … and `out-sheet.png`, all side by side — a motion's contact sheet
//! (`TARAE_MOTION_AT=<ms>` instead pins every motion at that age, for one picture of a step). A wait that times out
//! fails the shot (the picture is still saved, of the screen as it was then):
//! a shot of the wrong frame never passes as one that worked.
//!
//! Environment:
//! - `TARAE_SHOT_XDG` — config home (`tarae/config.toml` under it; default `/tmp/tarae-shot/xdg`). Point it at
//!   an empty folder so your own settings don't leak in. Folders given here must be absolute (and the config
//!   and start folders there already) — else the shot refuses.
//! - `TARAE_SHOT_STATE` — state home (files to restore, recent files, undo, chats); default a fresh temp dir, removed
//!   afterwards — never the user's.
//! - `TARAE_BACKGROUND=dark|light` — the theme (the virtual terminal doesn't answer the background query).
//! - `TARAE_SHOT_FILES` (default `src/transaction.rs src/selection.rs`), `TARAE_SHOT_CWD` (default the repo),
//!   `TARAE_SHOT_COLS`/`TARAE_SHOT_ROWS` (default 112×30).
//! - `TARAE_SHOT_REPLAY=<file>` — draw terminal bytes recorded elsewhere instead of running tarae.
//! - `TARAE_SHOT_BIN` — another tarae binary (absolute; a saved "before" build for a before/after), not built.
//! - `TARAE_SHOT_ZELLIJ=1` — the tarae runs as inside zellij (`ZELLIJ=0`, as zellij sets it: the hints say the twins
//!   of the keys zellij takes — `src/zellij.rs`). Without it `ZELLIJ` is taken out, so a shot taken from inside
//!   zellij shows the default hints.
//! - `TARAE_SHOT_SCALE` — the picture's scale, a whole number 1–4 (default 1; `--live` too): at 2 the font is
//!   30 px and every measure of the 1× picture — cells, the margin, lines, blocks, the cursor — doubles, so it's
//!   the same layout at twice the pixels, box drawing and half-blocks on whole pixels (for a high-density screen).
//!
//! Set by the shot for the tarae it runs, never yours to set: `TARAE_SHOT_NO_DISK=1` — the free space is the
//! machine's the shot is taken on, not the picture's, so the status line's disk mark and the disk card stay off
//! (`procs::disk_check`); also a color terminal, its own config and state (`as_a_terminal`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::{Result, root};

const SIZE: f32 = 15.0;
/// The margin around the cells, in 1× pixels (`docshots.rs` finds a picture's cells by it).
pub(crate) const PAD: usize = 14;
const BG: Rgb = [30, 31, 40];
const FG: Rgb = [248, 248, 242];
/// tarae wraps each frame in a synchronized update (`CSI ?2026h … ?2026l`) — only whole frames are fed, so a
/// capture never lands mid-frame (cards half overdrawn while an animation ticks).
const FRAME_END: &[u8] = b"\x1b[?2026l";

type Rgb = [u8; 3];

pub fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "--live") {
        return live(&args[1..]);
    }
    let (out, keys) =
        args.split_first().ok_or("usage: cargo xtask shot out.png [keys …] | shot --live out.png")?;
    let num = |k: &str, d: u16| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (cols, rows) = (num("TARAE_SHOT_COLS", 112), num("TARAE_SHOT_ROWS", 30));
    let scale = scale(std::env::var("TARAE_SHOT_SCALE").ok().as_deref())?;
    let mut vt = Vt {
        parser: vt100::Parser::new(rows, cols, 0),
        held: Vec::new(),
        capturing: None,
        captured: Vec::new(),
    };

    if let Some(replay) = std::env::var_os("TARAE_SHOT_REPLAY") {
        let bytes = std::fs::read(&replay).map_err(|e| format!("{}: {e}", Path::new(&replay).display()))?;
        vt.feed(&bytes);
    } else {
        let given = folder("TARAE_SHOT_STATE", std::env::var_os("TARAE_SHOT_STATE"), false)?;
        let state = given
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join(format!("tarae-shot-state-{}", std::process::id())));
        let res = Setup::from_env().and_then(|setup| drive(&mut vt, &setup, keys, &state, &[], cols, rows));
        if given.is_none() {
            let _ = std::fs::remove_dir_all(&state);
        }
        let missed = res?;
        contact_sheet(&vt, scale, Path::new(out))?;
        return picture_of(&vt, scale, Path::new(out), &missed);
    }
    picture(&vt, scale, Path::new(out))
}

/// What a shot runs: the tarae, the files it opens and where, its config home, the theme's background
/// (`TARAE_BACKGROUND`; None: the shot's own environment's), and variables of the shot's own for tarae (a
/// scene's terminal shell — `scenes.rs`; Some(""): set empty, None: taken out).
pub struct Setup {
    pub bin: PathBuf,
    pub files: Vec<String>,
    pub cwd: PathBuf,
    pub config: PathBuf,
    pub background: Option<&'static str>,
    pub env: Vec<(&'static str, Option<&'static str>)>,
}

impl Setup {
    /// `cargo xtask shot`'s: from its variables (the top of this file), the shot profile's tarae built unless
    /// `TARAE_SHOT_BIN` names one.
    fn from_env() -> Result<Setup> {
        let root = root();
        let bin = match folder("TARAE_SHOT_BIN", std::env::var_os("TARAE_SHOT_BIN"), false)? {
            Some(bin) => bin,
            None => build_shot(&root)?,
        };
        let files = std::env::var("TARAE_SHOT_FILES")
            .unwrap_or_else(|_| "src/transaction.rs src/selection.rs".into())
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let cwd = folder("TARAE_SHOT_CWD", std::env::var_os("TARAE_SHOT_CWD"), true)?.unwrap_or(root);
        let config = folder("TARAE_SHOT_XDG", std::env::var_os("TARAE_SHOT_XDG"), true)?
            .unwrap_or_else(|| "/tmp/tarae-shot/xdg".into());
        let env = shot_env(std::env::var_os("TARAE_SHOT_ZELLIJ"));
        Ok(Setup { bin, files, cwd, config, background: None, env })
    }
}

/// The shot's own variables for tarae, `TARAE_SHOT_ZELLIJ` as given: `ZELLIJ` taken out — a shot taken from inside
/// zellij shows the default hints — unless it's `1`, then `ZELLIJ=0` as zellij sets it (the hints say the twins of
/// the keys zellij takes — `src/zellij.rs`).
fn shot_env(zellij: Option<std::ffi::OsString>) -> Vec<(&'static str, Option<&'static str>)> {
    vec![("ZELLIJ", zellij.is_some_and(|v| v == "1").then_some("0"))]
}

/// One shot of `setup`'s tarae on a `cols`×`rows` pty, `keys` sent as `cargo xtask shot` sends them, its state
/// in `state` (left there), the screen saved to `out`.
pub fn take(setup: &Setup, keys: &[String], size: (u16, u16), state: &Path, out: &Path) -> Result<()> {
    let (cols, rows) = size;
    let mut vt = Vt {
        parser: vt100::Parser::new(rows, cols, 0),
        held: Vec::new(),
        capturing: None,
        captured: Vec::new(),
    };
    let missed = drive(&mut vt, setup, keys, state, &[], cols, rows)?;
    picture_of(&vt, 1, out, &missed)
}

/// `take` for a picture the docs show (`scenes --docs`): at `scale`, and with tarae's homes in `homes` rather than
/// yours (`XDG_DATA_HOME` — grammars, packs, sessions' checkouts —, `XDG_CACHE_HOME`, `HOME` …): nothing
/// downloaded, cached or kept on this machine shows in it. What it returns: the screen it drew, each cell's text
/// row by row (a wide character's second cell empty) — a docs row checks its `varies` there.
pub fn take_at(
    setup: &Setup,
    keys: &[String],
    size: (u16, u16),
    scale: usize,
    state: &Path,
    homes: &[(&str, std::ffi::OsString)],
    out: &Path,
) -> Result<Vec<Vec<String>>> {
    let (cols, rows) = size;
    let mut vt = Vt {
        parser: vt100::Parser::new(rows, cols, 0),
        held: Vec::new(),
        capturing: None,
        captured: Vec::new(),
    };
    let missed = drive(&mut vt, setup, keys, state, homes, cols, rows)?;
    picture_of(&vt, scale, out, &missed)?;
    let screen = vt.parser.screen();
    let text = |r, c| screen.cell(r, c).map(|c| c.contents().to_string()).unwrap_or_default();
    Ok((0..rows).map(|r| (0..cols).map(|c| text(r, c)).collect()).collect())
}

/// The picture of a driven screen, saved either way — and an error when a wait in its keys timed out (`missed`):
/// it's of the frame as it was then, likely not the one meant, and must not pass as a shot that worked.
fn picture_of(vt: &Vt, scale: usize, out: &Path, missed: &[String]) -> Result<()> {
    picture(vt, scale, out)?;
    match missed {
        [] => Ok(()),
        _ => Err(format!("{} — the shot is of the screen as it was then", missed.join("; "))),
    }
}

/// The screen as a PNG at `out`.
fn picture(vt: &Vt, scale: usize, out: &Path) -> Result<()> {
    let fonts = Fonts::load(scale)?;
    let (w, h, px) = render(vt.parser.screen(), &fonts, scale);
    save_png(out, w, h, &px)?;
    println!("saved {} ({w}, {h})", out.display());
    Ok(())
}

/// `TARAE_SHOT_SCALE`'s value: 1 when unset, else a whole number from 1 to 4.
fn scale(given: Option<&str>) -> Result<usize> {
    match given.map(|v| v.trim().parse::<usize>()) {
        None => Ok(1),
        Some(Ok(s @ 1..=4)) => Ok(s),
        Some(_) => Err(format!("TARAE_SHOT_SCALE={}: a whole number from 1 to 4", given.unwrap_or_default())),
    }
}

// ── A running tarae ──────────────────────────────────────────────────────────

/// `shot --live out.png [--pid N]` — the frame a running tarae drew last, as a picture (#209: a driver saw the
/// live tarae only as text). Asked of its UI state socket (`{"get":"frame"}`, `tarae state --frame`'s request):
/// tarae `N`'s, else the one `TARAE_UI` names (its terminal panel and your tasks have it), else the only one
/// running. No tarae binary is run (see `linux::tarae_sessions`). The fonts and the ground are the shot's.
fn live(args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: cargo xtask shot --live out.png [--pid N]";
    let (mut out, mut pid) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pid" => pid = Some(it.next().and_then(|p| p.parse::<u32>().ok()).ok_or(USAGE)?),
            o if out.is_none() && !o.starts_with('-') => out = Some(o.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let out = out.ok_or(USAGE)?;
    let sock = live_socket(pid, std::env::var_os("TARAE_UI"), ui_dir().as_deref())?;
    let v = ui_ask(&sock, &serde_json::json!({ "get": "frame", "timeout_ms": 5000 }))?;
    let f = &v["frame"];
    let size = |k: &str| f[k].as_u64().and_then(|n| u16::try_from(n).ok()).filter(|&n| n > 0);
    let (Some(cols), Some(rows)) = (size("cols"), size("rows")) else {
        return Err(format!("{}: no frame in its answer", sock.display()));
    };
    let mut vt = Vt {
        parser: vt100::Parser::new(rows, cols, 0),
        held: Vec::new(),
        capturing: None,
        captured: Vec::new(),
    };
    vt.feed(f["bytes"].as_str().unwrap_or_default().as_bytes());
    // (the frame as tarae drew it, before the terminal's update markers: ended here)
    vt.feed(FRAME_END);
    let scale = scale(std::env::var("TARAE_SHOT_SCALE").ok().as_deref())?;
    let fonts = Fonts::load(scale)?;
    let (w, h, px) = render(vt.parser.screen(), &fonts, scale);
    save_png(Path::new(&out), w, h, &px)?;
    println!("saved {out} ({w}, {h}) — frame {} of {}", f["frames"], sock.display());
    Ok(())
}

/// tarae's sockets' folder: `$XDG_STATE_HOME/tarae/ui` (default `~/.local/state/tarae/ui`), as tarae has it.
fn ui_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("tarae").join("ui"))
}

/// The socket to ask: tarae `pid`'s, else `TARAE_UI`'s (`ui`), else the only one in `dir`. Err says which there
/// are when it can't tell.
fn live_socket(pid: Option<u32>, ui: Option<std::ffi::OsString>, dir: Option<&Path>) -> Result<PathBuf> {
    let dir_or = || {
        dir.map(Path::to_path_buf)
            .ok_or_else(|| "no state folder (neither XDG_STATE_HOME nor HOME)".to_string())
    };
    if let Some(p) = pid {
        return Ok(dir_or()?.join(format!("{p}.sock")));
    }
    if let Some(ui) = ui {
        return Ok(PathBuf::from(ui));
    }
    let dir = dir_or()?;
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .collect();
    found.sort();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!("no tarae running ({} has no socket)", dir.display())),
        _ => {
            let pids: Vec<String> = found
                .iter()
                .filter_map(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            Err(format!("several tarae — --pid one of {}", pids.join(", ")))
        }
    }
}

/// One request to a tarae's UI state socket, its answer (its `error` as Err).
fn ui_ask(sock: &Path, req: &serde_json::Value) -> Result<serde_json::Value> {
    use std::io::BufRead;
    let said = |e: io::Error| format!("{}: {e}", sock.display());
    let mut s = std::os::unix::net::UnixStream::connect(sock).map_err(said)?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(said)?;
    writeln!(s, "{req}").map_err(said)?;
    let mut line = String::new();
    io::BufReader::new(&s).read_line(&mut line).map_err(said)?;
    let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| format!("its answer: {e}"))?;
    match v["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

// ── The terminal ─────────────────────────────────────────────────────────────

struct Vt {
    parser: vt100::Parser,
    held: Vec<u8>,
    /// `@~N`: since when every whole frame is kept (`captured`), for N ms after the key it went with.
    capturing: Option<Instant>,
    /// The frames `@~N` kept: the screen before its key, then each frame drawn, with when it came.
    captured: Vec<(Duration, vt100::Screen)>,
}

impl Vt {
    fn feed(&mut self, data: &[u8]) {
        self.held.extend_from_slice(data);
        let Some(cut) = self.held.windows(FRAME_END.len()).rposition(|w| w == FRAME_END) else { return };
        let frames: Vec<u8> = self.held.drain(..cut + FRAME_END.len()).collect();
        let Some(since) = self.capturing else { return self.process(&frames) };
        // (one frame at a time, each kept as drawn)
        let mut rest = &frames[..];
        while let Some(end) = rest.windows(FRAME_END.len()).position(|w| w == FRAME_END) {
            self.process(&rest[..end + FRAME_END.len()]);
            self.captured.push((since.elapsed(), self.parser.screen().clone()));
            rest = &rest[end + FRAME_END.len()..];
        }
    }

    fn process(&mut self, frames: &[u8]) {
        let parser = &mut self.parser;
        // vt100 can panic on odd input — keep the screen so far rather than lose the shot
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(frames)));
    }
}

/// `@~300`: the next key is sent and every frame drawn in the 300 ms after it kept — a motion's frames as they come.
fn capture_key(key: &str) -> Option<f64> {
    key.strip_prefix("@~")?.parse::<f64>().ok().filter(|ms| *ms > 0.0).map(|ms| ms / 1000.0)
}

/// `@~`'s frames beside `out`: `out-01.png` (the screen before its key), `out-02.png` …, and all of them side by
/// side, a gap between, in `out-sheet.png` — the contact sheet a motion is judged on.
fn contact_sheet(vt: &Vt, scale: usize, out: &Path) -> Result<()> {
    if vt.captured.is_empty() {
        return Ok(());
    }
    let fonts = Fonts::load(scale)?;
    let named = |tail: &str| {
        let stem = out.file_stem().unwrap_or_default().to_string_lossy();
        out.with_file_name(format!("{stem}-{tail}.png"))
    };
    let pictures: Vec<(usize, usize, Vec<u8>)> =
        vt.captured.iter().map(|(_, screen)| render(screen, &fonts, scale)).collect();
    for (i, ((t, _), (w, h, px))) in vt.captured.iter().zip(&pictures).enumerate() {
        let path = named(&format!("{:02}", i + 1));
        save_png(&path, *w, *h, px)?;
        let when =
            if i == 0 { "before its key".to_string() } else { format!("{} ms after it", t.as_millis()) };
        println!("saved {} ({when})", path.display());
    }
    let gap = PAD * scale;
    let (fw, fh) = (pictures[0].0, pictures.iter().map(|p| p.1).max().unwrap_or(0));
    let n = pictures.len();
    let (w, h) = (n * fw + (n - 1) * gap, fh);
    // (the gaps a mid grey: apart from a dark and a light theme alike)
    let mut sheet = vec![0x80u8; w * h * 3];
    for (k, (pw, ph, px)) in pictures.iter().enumerate() {
        let x0 = k * (fw + gap);
        for y in 0..*ph {
            let row = &px[y * pw * 3..(y + 1) * pw * 3];
            let at = (y * w + x0) * 3;
            sheet[at..at + row.len()].copy_from_slice(row);
        }
    }
    let path = named("sheet");
    save_png(&path, w, h, &sheet)?;
    println!("saved {} ({n} frames)", path.display());
    Ok(())
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

/// `homes`: folders set for tarae last, over the rest (`take_at`; none: the shot's own environment's). What it
/// returns: each wait in `keys` that timed out, said (the keys after it are still sent).
fn drive(
    vt: &mut Vt,
    setup: &Setup,
    keys: &[String],
    state: &Path,
    homes: &[(&str, std::ffi::OsString)],
    cols: u16,
    rows: u16,
) -> Result<Vec<String>> {
    if !setup.bin.is_file() {
        return Err(format!("{} not found", setup.bin.display()));
    }
    let (master, slave) = crate::pty::open(rows, cols).map_err(|e| format!("pty: {e}"))?;
    let mut master = File::from(master);
    let stdio = || slave.try_clone().map(Stdio::from).map_err(|e| e.to_string());
    let mut cmd = Command::new(&setup.bin);
    cmd.args(&setup.files).current_dir(&setup.cwd);
    if let Some(bg) = setup.background {
        cmd.env("TARAE_BACKGROUND", bg);
    }
    for (k, v) in &setup.env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    as_a_terminal(&mut cmd, setup.config.as_os_str(), state)
        .stdin(stdio()?)
        .stdout(stdio()?)
        .stderr(stdio()?);
    for (k, v) in homes {
        cmd.env(k, v);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            // `setsid` + the pty as its controlling terminal: /dev/tty works, SIGWINCH arrives.
            cvt(libc::setsid())?;
            cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", setup.bin.display()))?;
    drop(slave);

    // Drains the master until EOF — on macOS an exiting process whose tty output nobody reads hangs.
    let (tx, rx) = mpsc::channel();
    let mut reader = master.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    let pump = |vt: &mut Vt, secs: f64| {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(chunk) => vt.feed(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    };
    // Until `text` is on screen (whole frames — `Vt::feed`) — or, `gone`, no longer is — or `max` seconds pass
    let pump_until = |vt: &mut Vt, text: &str, max: f64, gone: bool| -> bool {
        let end = Instant::now() + Duration::from_secs_f64(max);
        loop {
            if vt.parser.screen().contents().contains(text) != gone {
                return true;
            }
            let Some(left) = end.checked_duration_since(Instant::now()) else { return false };
            match rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(chunk) => vt.feed(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    };
    pump(vt, 1.2);
    let mut missed = Vec::new();
    let mut capture = None;
    for k in keys {
        if let Some(secs) = capture_key(k) {
            capture = Some(secs);
            continue;
        }
        if let Some((max, text)) = wait_key(k) {
            let t = Instant::now();
            if pump_until(vt, text, max, false) {
                eprintln!("shot: {text:?} on screen after {:.1}s", t.elapsed().as_secs_f64());
            } else {
                eprintln!("shot: {text:?} not on screen after {max}s — going on, the shot will fail");
                missed.push(format!("waited {max} s for {text:?}: never on screen"));
            }
            continue;
        }
        if let Some((max, text)) = gone_key(k) {
            let t = Instant::now();
            if pump_until(vt, text, max, true) {
                eprintln!("shot: {text:?} gone from the screen after {:.1}s", t.elapsed().as_secs_f64());
            } else {
                eprintln!("shot: {text:?} still on screen after {max}s — going on, the shot will fail");
                missed.push(format!("waited {max} s for {text:?} to go: still on screen"));
            }
            continue;
        }
        if let Some(secs) = k.strip_prefix('@').and_then(|s| s.parse::<f64>().ok()) {
            pump(vt, secs);
            continue;
        }
        if let Some(secs) = capture.take() {
            let since = Instant::now();
            vt.captured.push((Duration::ZERO, vt.parser.screen().clone()));
            vt.capturing = Some(since);
            let _ = master.write_all(&unescape(k));
            pump(vt, secs);
            vt.capturing = None;
            continue;
        }
        let _ = master.write_all(&unescape(k));
        pump(vt, 0.35);
    }
    pump(vt, 0.6);
    quit(&mut master, &mut child, &rx);
    Ok(missed)
}

/// The longest `@?text` waits unless it says (`@30?text`).
const WAIT_MAX: f64 = 120.0;

/// `@?text` / `@30?text`: wait until `text` is on screen — at most `WAIT_MAX` seconds, or the ones given.
fn wait_key(key: &str) -> Option<(f64, &str)> {
    let (secs, text) = key.strip_prefix('@')?.split_once('?')?;
    let max = if secs.is_empty() { WAIT_MAX } else { secs.parse().ok()? };
    (!text.is_empty()).then_some((max, text))
}

/// `@!text` / `@30!text`: wait until `text` is gone from the screen — as `wait_key`'s limits.
fn gone_key(key: &str) -> Option<(f64, &str)> {
    let (secs, text) = key.strip_prefix('@')?.split_once('!')?;
    let max = if secs.is_empty() { WAIT_MAX } else { secs.parse().ok()? };
    (!text.is_empty()).then_some((max, text))
}

/// The `shot` profile's tarae, built now (`Cargo.toml`: release's behavior, built incrementally without
/// LTO — seconds after an edit where `--release` takes over a minute).
pub fn build_shot(root: &Path) -> Result<PathBuf> {
    build(root, "shot", &[])
}

/// tarae in cargo `profile` with `features`, built if it needs to be: the binary.
pub fn build(root: &Path, profile: &str, features: &[&str]) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--quiet", "--profile", profile, "--bin", "tarae"]);
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }
    let st = cmd.current_dir(root).status().map_err(|e| format!("cargo: {e}"))?;
    if !st.success() {
        return Err(format!("building the {profile} profile failed"));
    }
    Ok(root.join("target").join(profile).join("tarae"))
}

/// tarae's environment in the shot: a color terminal, its own config and state. An inherited `NO_COLOR`
/// goes — it's meant for the output of whatever runs it (a task panel sets it), and the shot renders tarae
/// itself: with it, every shot taken from a panel came out gray. `TARAE_SHOT_NO_DISK=1`: the machine's free
/// space isn't the picture's — tarae's disk mark and card stay off (`procs::disk_check`).
/// A folder given in `var`: absolute (tarae starts in `TARAE_SHOT_CWD`, so a relative one would mean another
/// folder there), and there already when `must_exist` — a config folder that isn't found leaves tarae on its
/// defaults, and the default `llm.command` is the real `claude` (a slip that started it once).
fn folder(var: &str, value: Option<std::ffi::OsString>, must_exist: bool) -> Result<Option<PathBuf>> {
    let Some(p) = value.map(PathBuf::from) else { return Ok(None) };
    if !p.is_absolute() {
        return Err(format!("{var}={} — give an absolute path", p.display()));
    }
    if must_exist && !p.is_dir() {
        return Err(format!("{var}={} — no such folder", p.display()));
    }
    Ok(Some(p))
}

fn as_a_terminal<'c>(cmd: &'c mut Command, config: &std::ffi::OsStr, state: &Path) -> &'c mut Command {
    cmd.env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("TARAE_UNDERCURL", "0")
        .env("TARAE_SHOT_NO_DISK", "1")
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
        // (tarae's cache — runs' folders, the reviewer's reading copies — the shot's own, never yours)
        .env("XDG_CACHE_HOME", state.join("cache"))
        .env_remove("NO_COLOR")
}

/// `:q!`, then a kill if tarae is still there a second later. Esc goes out alone — `ESC :` in one read is Alt-:.
fn quit(master: &mut File, child: &mut Child, rx: &Receiver<Vec<u8>>) {
    let _ = master.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(60));
    let _ = master.write_all(b":q!\r");
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        while rx.try_recv().is_ok() {}
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `\r` `\t` `\xHH` → their bytes; everything else as UTF-8.
fn unescape(k: &str) -> Vec<u8> {
    let (b, mut out, mut i) = (k.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        let hex = k.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok());
        let (byte, len) = match (b[i], b.get(i + 1), hex) {
            (b'\\', Some(b'r'), _) => (b'\r', 2),
            (b'\\', Some(b't'), _) => (b'\t', 2),
            (b'\\', Some(b'x'), Some(h)) => (h, 4),
            (c, _, _) => (c, 1),
        };
        out.push(byte);
        i += len;
    }
    out
}

// ── Fonts ────────────────────────────────────────────────────────────────────

struct Fonts {
    regular: fontdue::Font,
    bold: fontdue::Font,
    italic: fontdue::Font,
    bold_italic: fontdue::Font,
    /// Hangul and anything else past U+2E80 (CJK)
    wide: Option<fontdue::Font>,
    /// The wide font's bold, for a bold cell's: a bold cell looks bold whatever its script (none found: `wide`)
    wide_bold: Option<fontdue::Font>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Face {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Wide,
    WideBold,
}

/// The wide fonts looked for, in order: each with its bold — (file, collection index), the bold's file and index.
/// (Apple SD Gothic Neo's collection: Regular 0, Medium 2, Bold 6 — by the ink each puts down, measured
/// 2026-10-20: Bold 148 % of Regular's.)
const WIDE_FONTS: [(&str, u32, &str, u32); 3] = [
    ("AppleSDGothicNeo.ttc", 0, "AppleSDGothicNeo.ttc", 6),
    ("NotoSansCJK-Regular.ttc", 0, "NotoSansCJK-Bold.ttc", 0),
    ("NanumGothic.ttf", 0, "NanumGothicBold.ttf", 0),
];

impl Fonts {
    /// The fonts, tuned for `SIZE` at `scale`.
    fn load(scale: usize) -> Result<Fonts> {
        let size = SIZE * scale as f32;
        let open_at = |name: &str, index: u32| -> Option<fontdue::Font> {
            let bytes = std::fs::read(find_font(name)?).ok()?;
            let settings =
                fontdue::FontSettings { scale: size, collection_index: index, ..Default::default() };
            fontdue::Font::from_bytes(bytes, settings).ok()
        };
        let open = |name: &str| open_at(name, 0);
        let regular = open("JetBrainsMono-Regular.ttf").ok_or("JetBrainsMono-Regular.ttf not found")?;
        let or_regular = |name| open(name).unwrap_or_else(|| regular.clone());
        let (bold, italic) = (or_regular("JetBrainsMono-Bold.ttf"), or_regular("JetBrainsMono-Italic.ttf"));
        let bold_italic = open("JetBrainsMono-BoldItalic.ttf").unwrap_or_else(|| bold.clone());
        let (wide, wide_bold) = WIDE_FONTS
            .iter()
            .find_map(|&(f, i, b, j)| Some((open_at(f, i)?, open_at(b, j))))
            .map_or((None, None), |(w, b)| (Some(w), b));
        Ok(Fonts { regular, bold, italic, bold_italic, wide, wide_bold })
    }

    fn face(&self, f: Face) -> &fontdue::Font {
        match f {
            Face::Regular => &self.regular,
            Face::Bold => &self.bold,
            Face::Italic => &self.italic,
            Face::BoldItalic => &self.bold_italic,
            Face::Wide => self.wide.as_ref().unwrap_or(&self.regular),
            Face::WideBold => self.wide_bold.as_ref().unwrap_or(self.face(Face::Wide)),
        }
    }
}

fn find_font(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dirs = [
        home.join("Library/Fonts"),
        "/Library/Fonts".into(),
        "/System/Library/Fonts".into(),
        home.join(".local/share/fonts"),
        home.join(".fonts"),
        "/usr/share/fonts".into(),
        "/usr/local/share/fonts".into(),
    ];
    dirs.iter().find_map(|d| search(d, name, 3))
}

fn search(dir: &Path, name: &str, depth: u8) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .find_map(|e| search(&e.path(), name, depth - 1))
}

// ── Drawing ──────────────────────────────────────────────────────────────────

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    /// Fills the half-open rectangle [x0, x1) × [y0, y1), clipped.
    fn rect(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, c: Rgb) {
        for y in y0..y1.min(self.h) {
            for x in x0..x1.min(self.w) {
                let i = (y * self.w + x) * 3;
                self.px[i..i + 3].copy_from_slice(&c);
            }
        }
    }

    fn blend(&mut self, x: i64, y: i64, c: Rgb, a: u8) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a == 0 {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 3;
        for (d, s) in self.px[i..i + 3].iter_mut().zip(c) {
            *d = ((*d as u32 * (255 - a as u32) + s as u32 * a as u32 + 127) / 255) as u8;
        }
    }
}

/// Line chars as lines through the cell center, filling the cell so neighbors connect (as a terminal draws them).
fn box_arms(c: &str) -> Option<&'static str> {
    Some(match c {
        "─" => "lr",
        "│" => "ud",
        "╭" | "┌" => "rd",
        "╮" | "┐" => "ld",
        "╰" | "└" => "ru",
        "╯" | "┘" => "lu",
        "├" => "udr",
        "┤" => "udl",
        "┬" => "lrd",
        "┴" => "lru",
        "┼" => "udlr",
        _ => return None,
    })
}

/// Block chars as rectangles exactly filling their part of the cell — font glyphs leave gaps.
fn block(c: &str) -> Option<(f32, f32, f32, f32)> {
    Some(match c {
        "▀" => (0.0, 0.0, 1.0, 0.5),
        "▄" => (0.0, 0.5, 1.0, 1.0),
        "▌" => (0.0, 0.0, 0.5, 1.0),
        "▐" => (0.5, 0.0, 1.0, 1.0),
        "█" => (0.0, 0.0, 1.0, 1.0),
        "▎" => (0.0, 0.0, 0.25, 1.0),
        "▏" => (0.0, 0.0, 0.125, 1.0),
        _ => return None,
    })
}

fn color(c: vt100::Color, default: Rgb) -> Rgb {
    const BASE: [Rgb; 8] = [
        [33, 34, 44],
        [255, 85, 85],
        [80, 250, 123],
        [241, 250, 140],
        [189, 147, 249],
        [255, 121, 198],
        [139, 233, 253],
        [248, 248, 242],
    ];
    match c {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        vt100::Color::Idx(i @ 0..16) => BASE[i as usize % 8],
        vt100::Color::Idx(i @ 16..232) => {
            let v = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
            let i = i - 16;
            [v(i / 36), v(i / 6 % 6), v(i % 6)]
        }
        vt100::Color::Idx(i) => [8 + (i - 232) * 10; 3],
    }
}

/// The screen as RGB pixels at `s` times the size: glyphs drawn at `SIZE` × `s`, every other measure the 1×
/// picture's times `s` — so 2× is the 1× layout at twice the pixels, lines and blocks on whole pixels.
fn render(screen: &vt100::Screen, fonts: &Fonts, s: usize) -> (usize, usize, Vec<u8>) {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows as usize, cols as usize);
    let size = SIZE * s as f32;
    // The 1× cell
    let (cw1, ch1) = (fonts.regular.metrics('M', SIZE).advance_width as usize, (SIZE * 1.45) as usize);
    let (cw, ch, pad) = (cw1 * s, ch1 * s, PAD * s);
    let baseline1 = baseline(&fonts.regular, ch1);
    let cell = |y: usize, x: usize| screen.cell(y as u16, x as u16);
    // The outer margin takes the edit view's background (the command line's last cell — no card covers it)
    let bg = cell(rows - 1, cols - 1).map_or(BG, |c| color(c.bgcolor(), BG));
    let (w, h) = (cols * cw + pad * 2, rows * ch + pad * 2);
    let mut cv = Canvas { w, h, px: bg.repeat(w * h) };

    let colors = |c: &vt100::Cell| {
        let (fg, b) = (color(c.fgcolor(), FG), color(c.bgcolor(), bg));
        if c.inverse() { (b, fg) } else { (fg, b) }
    };
    // 1) All backgrounds first — so wide glyphs aren't half covered by the next cell's background
    for y in 0..rows {
        for x in 0..cols {
            let Some(c) = cell(y, x) else { continue };
            let (_, b) = colors(c);
            if b != bg {
                let (px, py) = (pad + x * cw, pad + y * ch);
                cv.rect(px, py, px + cw * if c.is_wide() { 2 } else { 1 }, py + ch, b);
            }
        }
    }
    // 2) Glyphs
    let mut glyphs: HashMap<(Face, char), (fontdue::Metrics, Vec<u8>)> = HashMap::new();
    for y in 0..rows {
        for x in 0..cols {
            let Some(c) = cell(y, x) else { continue };
            let text = c.contents();
            if text.trim().is_empty() {
                continue;
            }
            let (fg, _) = colors(c);
            let (px, py) = (pad + x * cw, pad + y * ch);
            if let Some((a0, b0, a1, b1)) = block(text) {
                let at = |t: f32, n: usize| (t * n as f32).round() as usize * s;
                cv.rect(px + at(a0, cw1), py + at(b0, ch1), px + at(a1, cw1), py + at(b1, ch1), fg);
                continue;
            }
            if let Some(arms) = box_arms(text) {
                // (a line is `s` pixels thick)
                let (mx, my) = (px + cw1 / 2 * s, py + ch1 / 2 * s);
                for a in arms.chars() {
                    match a {
                        'l' => cv.rect(px, my, mx + s, my + s, fg),
                        'r' => cv.rect(mx, my, px + cw + s, my + s, fg),
                        'u' => cv.rect(mx, py, mx + s, my + s, fg),
                        _ => cv.rect(mx, my, mx + s, py + ch + s, fg),
                    }
                }
                continue;
            }
            let mut pen = px as f32;
            // (a wide character alone in its cell: at the font's own size, centred across its two cells. Drawn from
            // the first cell's edge `실타래` read `실 타 래` (frictions29 8); drawn bigger, to fill both cells, the
            // Hangul stood too big next to the English (the user's look, 2026-10-20) — so the size is the font's, and
            // the centring is what stays. A bold cell's in the wide font's Bold: bold whatever its script)
            let centred = c.is_wide() && text.chars().count() == 1;
            for chr in text.chars() {
                let face = match (c.bold(), c.italic()) {
                    (true, _) if chr as u32 > 0x2e80 && fonts.wide_bold.is_some() => Face::WideBold,
                    _ if chr as u32 > 0x2e80 && fonts.wide.is_some() => Face::Wide,
                    (true, true) => Face::BoldItalic,
                    (true, false) => Face::Bold,
                    (false, true) => Face::Italic,
                    _ => Face::Regular,
                };
                // A glyph the mono font lacks: try the wide font
                let face = if !matches!(face, Face::Wide | Face::WideBold)
                    && fonts.face(face).lookup_glyph_index(chr) == 0
                    && fonts.wide.is_some()
                {
                    // (a bold cell's in the wide font's bold, whatever its script)
                    if c.bold() && fonts.wide_bold.is_some() { Face::WideBold } else { Face::Wide }
                } else {
                    face
                };
                let font = fonts.face(face);
                let (m, bitmap) = glyphs.entry((face, chr)).or_insert_with(|| font.rasterize(chr, size));
                // (the 1× baseline's, times the scale)
                let baseline = (py + baseline1 * s) as i64;
                let top = baseline - m.ymin as i64 - m.height as i64;
                let left = match centred {
                    true => (px + (2 * cw).saturating_sub(m.width) / 2) as i64,
                    false => pen.round() as i64 + m.xmin as i64,
                };
                for (i, a) in bitmap.iter().enumerate() {
                    cv.blend(left + (i % m.width.max(1)) as i64, top + (i / m.width.max(1)) as i64, fg, *a);
                }
                pen += m.advance_width;
            }
            if c.underline() {
                let uw = cw * if c.is_wide() { 2 } else { 1 };
                cv.rect(px, py + ch - 3 * s, px + uw, py + ch - 2 * s, fg);
            }
        }
    }
    // 3) The cursor: a thin bar in the cell's text color
    if !screen.hide_cursor() {
        let (cy, cx) = screen.cursor_position();
        let (cy, cx) = (cy as usize, cx as usize);
        let fg = if cx < cols { cell(cy, cx).map_or(FG, |c| color(c.fgcolor(), FG)) } else { FG };
        let (px, py) = (pad + cx * cw, pad + cy * ch);
        cv.rect(px, py, px + 2 * s, py + ch, fg);
    }
    (w, h, cv.px)
}

/// The 1× baseline, in pixels from the top of a cell `ch1` tall: `font`'s line (ascent, descent, line gap)
/// centred in the cell, as a terminal places it (Ghostty, kitty) — one for every face of a row, a fallback's
/// glyphs on it too. (frictions28: `3 + ascent` put it about 3 px low in a 21 px cell.)
fn baseline(font: &fontdue::Font, ch1: usize) -> usize {
    let at = font.horizontal_line_metrics(SIZE).map_or(SIZE, |l| {
        let line = l.ascent - l.descent + l.line_gap;
        (ch1 as f32 - line) / 2.0 + l.line_gap / 2.0 + l.ascent
    });
    at.round() as usize
}

fn save_png(path: &Path, w: usize, h: usize, px: &[u8]) -> Result<()> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = File::create(path).map_err(|e| err(&e))?;
    let mut enc = png::Encoder::new(io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| err(&e))?;
    writer.write_image_data(px).map_err(|e| err(&e))?;
    writer.finish().map_err(|e| err(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_key_is_its_ms_after_a_tilde() {
        assert_eq!(capture_key("@~300"), Some(0.3));
        assert_eq!(capture_key("@~0"), None);
        assert_eq!(capture_key("@3"), None);
        assert_eq!(capture_key("@?ready"), None);
    }

    #[test]
    fn unescapes_keys() {
        assert_eq!(unescape(r"/foo\r"), b"/foo\r");
        assert_eq!(unescape(r"\x1b:q\t"), b"\x1b:q\t");
        assert_eq!(unescape("가\\xZZ"), "가\\xZZ".as_bytes());
    }

    #[test]
    fn only_whole_frames_are_drawn() {
        assert_eq!(wait_key("@?ready"), Some((WAIT_MAX, "ready")));
        assert_eq!(wait_key("@30?a b"), Some((30.0, "a b")));
        assert_eq!(
            (wait_key("@3"), wait_key("@?"), wait_key("@x?y"), wait_key("?y")),
            (None, None, None, None)
        );
        assert_eq!((gone_key("@!lit"), gone_key("@5!a b")), (Some((WAIT_MAX, "lit")), Some((5.0, "a b"))));
        assert_eq!((gone_key("@?x"), gone_key("@!")), (None, None));
        let mut vt = Vt {
            parser: vt100::Parser::new(2, 10, 0),
            held: Vec::new(),
            capturing: None,
            captured: Vec::new(),
        };
        vt.feed(b"\x1b[?2026hab\x1b[?2026l\x1b[?2026hcd");
        assert_eq!(vt.parser.screen().contents(), "ab");
        vt.feed(b"\x1b[?2026l");
        assert_eq!(vt.parser.screen().contents(), "abcd");
    }

    /// A wait that times out is said and fails the shot — the picture still saved, of the screen as it was —
    /// and one that's met isn't; the keys after a missed one are still sent. (A stand-in on the pty draws one whole
    /// frame, then echoes nothing.)
    #[test]
    fn a_wait_that_times_out_fails_the_shot() {
        // (the picture is drawn either way: without JetBrains Mono there's none to draw)
        if Fonts::load(1).is_err() {
            return;
        }
        let dir = crate::scenes::Fresh::new("tarae-shot-missed").unwrap();
        let setup = Setup {
            bin: "/bin/sh".into(),
            files: vec!["-c".into(), r"printf '\033[?2026hready\033[?2026l'; exec cat >/dev/null".into()],
            cwd: dir.0.clone(),
            config: dir.0.clone(),
            background: None,
            env: Vec::new(),
        };
        let keys = |ks: &[&str]| ks.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        let out = dir.0.join("met.png");
        assert_eq!(take(&setup, &keys(&["@3?ready", "@0.3!nothere"]), (20, 3), &dir.0, &out), Ok(()));
        let out = dir.0.join("missed.png");
        let e = take(&setup, &keys(&["@0.3?nothere", "@0.3!ready", "@3?ready"]), (20, 3), &dir.0, &out)
            .unwrap_err();
        assert!(e.contains(r#"waited 0.3 s for "nothere": never on screen"#), "{e}");
        assert!(e.contains(r#"waited 0.3 s for "ready" to go: still on screen"#), "{e}");
        assert!(out.is_file(), "the picture is saved to look at");
    }

    /// A shot's homes (`take_at`'s, a docs shot's own) are what its program sees, over the environment it's
    /// taken in, its setup's own variables and the terminal's (`as_a_terminal`'s `XDG_STATE_HOME`): `drive` sets
    /// them last, and that order is what makes them win. (A stand-in on the pty draws `ok` only when `HOME`,
    /// `XDG_CACHE_HOME` and `XDG_STATE_HOME` are the homes given.)
    #[test]
    fn a_shot_runs_tarae_outside_zellij_unless_asked() {
        assert_eq!(super::shot_env(None), [("ZELLIJ", None)], "taken out: the default hints");
        assert_eq!(super::shot_env(Some("0".into())), [("ZELLIJ", None)], "only `1` asks");
        assert_eq!(super::shot_env(Some("1".into())), [("ZELLIJ", Some("0"))], "as zellij sets it");
    }

    #[test]
    fn a_shots_homes_win_over_what_else_sets_them() {
        if Fonts::load(1).is_err() {
            return;
        }
        let dir = crate::scenes::Fresh::new("tarae-shot-homes").unwrap();
        let check = r#"v="$HOME|$XDG_CACHE_HOME|$XDG_STATE_HOME"; [ "$v" = "/t/home|/t/cache|/t/state" ] && m=ok || m="no:$v"
            printf '\033[?2026h%s\033[?2026l' "$m"; exec cat >/dev/null"#;
        let setup = Setup {
            bin: "/bin/sh".into(),
            files: vec!["-c".into(), check.into()],
            cwd: dir.0.clone(),
            config: dir.0.clone(),
            background: None,
            // (the setup's own, set before the homes)
            env: vec![("HOME", Some("/yours")), ("XDG_CACHE_HOME", Some("/yours/.cache"))],
        };
        let homes = [
            ("HOME", "/t/home".into()),
            ("XDG_CACHE_HOME", "/t/cache".into()),
            ("XDG_STATE_HOME", "/t/state".into()),
        ];
        let out = dir.0.join("homes.png");
        let keys = ["@3?ok".to_string()];
        assert!(take_at(&setup, &keys, (80, 2), 1, &dir.0, &homes, &out).is_ok(), "the homes given");
    }

    /// A bold cell's character the mono font lacks, below the wide range, is drawn in the wide font's bold too —
    /// "bold whatever its script". (Skipped where no such character is found, or no bold wide face.)
    #[test]
    fn a_bold_cells_fallback_glyph_is_the_wide_fonts_bold() {
        let Ok(f) = Fonts::load(1) else { return };
        let (Some(wide), Some(bold)) = (f.wide.as_ref(), f.wide_bold.as_ref()) else { return };
        let ink =
            |font: &fontdue::Font, c| font.rasterize(c, SIZE).1.iter().map(|a| u64::from(*a)).sum::<u64>();
        // (one the mono font lacks whose bold the wide font draws heavier — some punctuation is one shape in both)
        let lacked = (0x2000..0x2e80u32).filter_map(char::from_u32).find(|&c| {
            f.regular.lookup_glyph_index(c) == 0
                && wide.lookup_glyph_index(c) != 0
                && ink(bold, c) > ink(wide, c) + 50
        });
        let Some(c) = lacked else { return };
        let drawn = |text: &str| {
            let mut vt = vt100::Parser::new(1, 8, 0);
            vt.process(format!("\x1b[?25l{text}").as_bytes());
            let (_, _, px) = render(vt.screen(), &f, 1);
            px.chunks(3).flat_map(|p| p.iter().zip(BG).map(|(a, b)| u64::from(a.abs_diff(b)))).sum::<u64>()
        };
        let (regular, bold) = (drawn(&c.to_string()), drawn(&format!("\x1b[1m{c}")));
        assert!(
            bold > regular,
            "{c:?} (U+{:04X}) in a bold cell: ink {bold} against Regular's {regular}",
            c as u32
        );
    }

    /// A shot taken where `NO_COLOR=1` is set (a task panel) is still in color: tarae doesn't see it. (A
    /// stand-in prints what it was given; the variable set the way an inherited one would be.)
    #[test]
    fn an_inherited_no_color_doesnt_reach_tarae() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", r#"printf '%s|%s|%s' "${NO_COLOR-unset}" "$TERM" "$TARAE_SHOT_NO_DISK""#])
            .env("NO_COLOR", "1");
        let out = as_a_terminal(&mut cmd, "/nonexistent-config".as_ref(), Path::new("/nonexistent-state"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|xterm-256color|1", "(and no host disk mark)");
    }

    /// Folders from the environment: a relative one is refused (it'd be read from where tarae starts), and so
    /// is a config or start folder that isn't there; an absolute one that is goes through, nothing set is fine.
    #[test]
    fn shot_folders_are_absolute_and_there() {
        let tmp = std::env::temp_dir();
        let given = |p: &str| Some(std::ffi::OsString::from(p));
        let e = folder("TARAE_SHOT_XDG", given("target/sb/shot-xdg"), true).unwrap_err();
        assert!(e.contains("TARAE_SHOT_XDG=target/sb/shot-xdg") && e.contains("absolute"), "{e}");
        assert!(folder("TARAE_SHOT_CWD", given("."), true).is_err());
        assert!(folder("TARAE_SHOT_STATE", given("state"), false).is_err());
        let missing = tmp.join(format!("tarae-shot-missing-{}", std::process::id()));
        let e = folder("TARAE_SHOT_XDG", Some(missing.clone().into()), true).unwrap_err();
        assert!(e.contains("no such folder"), "{e}");
        assert_eq!(folder("TARAE_SHOT_STATE", Some(missing.clone().into()), false), Ok(Some(missing)));
        assert_eq!(folder("TARAE_SHOT_XDG", Some(tmp.clone().into()), true), Ok(Some(tmp)));
        assert_eq!(folder("TARAE_SHOT_XDG", None, true), Ok(None));
    }

    /// `--live` asks tarae `N`'s socket, else `TARAE_UI`'s, else the only one running — several or none say so.
    #[test]
    fn a_live_shot_asks_the_tarae_named_else_the_only_one() {
        let dir = std::env::temp_dir().join(format!("tarae-shot-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let e = live_socket(None, None, Some(&dir)).unwrap_err();
        assert!(e.starts_with("no tarae running"), "{e}");
        std::fs::write(dir.join("41.sock"), "").unwrap();
        assert_eq!(live_socket(None, None, Some(&dir)), Ok(dir.join("41.sock")));
        std::fs::write(dir.join("42.sock"), "").unwrap();
        let e = live_socket(None, None, Some(&dir)).unwrap_err();
        assert!(e.contains("--pid one of 41, 42"), "{e}");
        assert_eq!(live_socket(Some(42), Some("/x.sock".into()), Some(&dir)), Ok(dir.join("42.sock")));
        assert_eq!(live_socket(None, Some("/x.sock".into()), Some(&dir)), Ok(PathBuf::from("/x.sock")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The frame a tarae answers with (a stand-in socket here, answering as `{"get":"frame"}` does) is drawn
    /// whole: its bytes come without the terminal's update markers, and still reach the screen.
    #[test]
    fn a_live_frame_is_drawn_whole() {
        let dir = std::env::temp_dir().join(format!("tarae-shot-frame-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("7.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            use std::io::BufRead;
            let (s, _) = listener.accept().unwrap();
            let mut req = String::new();
            io::BufReader::new(&s).read_line(&mut req).unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            let answer = match v["get"].as_str() {
                Some("frame") => serde_json::json!({ "v": 1, "frame": {
                    "cols": 10, "rows": 2, "frames": 3, "input": 0, "bytes": "\x1b[1;1Hab\x1b[2;1Hcd" } }),
                _ => serde_json::json!({ "error": "unknown request" }),
            };
            writeln!(&s, "{answer}").unwrap();
        });
        let v = ui_ask(&sock, &serde_json::json!({ "get": "frame" })).unwrap();
        server.join().unwrap();
        let f = &v["frame"];
        let mut vt = Vt {
            parser: vt100::Parser::new(2, 10, 0),
            held: Vec::new(),
            capturing: None,
            captured: Vec::new(),
        };
        vt.feed(f["bytes"].as_str().unwrap().as_bytes());
        assert_eq!(vt.parser.screen().contents(), "", "held until a frame's end");
        vt.feed(FRAME_END);
        assert_eq!(vt.parser.screen().contents(), "ab\ncd");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A screen with every kind of cell `render` draws: colors, faces, underline, inverse, box drawing, blocks,
    /// wide characters, the cursor.
    fn every_kind_of_cell() -> vt100::Parser {
        let mut vt = vt100::Parser::new(5, 60, 0);
        vt.process(
            "\x1b[48;2;40;42;54m\x1b[38;2;248;248;242m╭──┬──╮ plain \x1b[1mbold\x1b[22m \x1b[3mitalic\x1b[23m \
             \x1b[1;3mboth\x1b[0m\r\n\x1b[38;2;189;147;249m│ab├cd┤ \x1b[4munder\x1b[24m 가나 ▀▄▌▐█▎▏ ●▲›◦ —…×\r\n\
             \x1b[48;2;68;71;90m╰──┴──╯ ┼ ┌┐└┘ fn main() { let x: u8 = 0; }\x1b[0m\r\n\
             \x1b[7minverse\x1b[0m \x1b[38;5;214midx\x1b[0m \x1b[38;5;245mgray\x1b[0m\x1b[4;5H"
                .as_bytes(),
        );
        vt
    }

    /// At 2× the picture is the 1× one at twice the pixels: twice as wide and tall, and everything but the
    /// glyphs — backgrounds, box drawing, blocks, the cursor — exactly the 1× pixels doubled, on whole pixels.
    /// (Skipped without JetBrains Mono, which the cell's width comes from.)
    #[test]
    fn a_2x_shot_is_the_1x_layout_at_twice_the_pixels() {
        let (Ok(f1), Ok(f2)) = (Fonts::load(1), Fonts::load(2)) else { return };
        let (w1, h1, _) = render(every_kind_of_cell().screen(), &f1, 1);
        let (w2, h2, _) = render(every_kind_of_cell().screen(), &f2, 2);
        assert_eq!((w2, h2), (w1 * 2, h1 * 2));
        let mut vt = vt100::Parser::new(3, 12, 0);
        vt.process(
            "\x1b[48;2;40;42;54m\x1b[38;2;189;147;249m╭─┬─╮▀▄▌▐█▎▏\r\n│ ├┼┤\x1b[0m ╰┴╯┌┐\r\n\x1b[7m \x1b[0m└┘\x1b[3;7H"
                .as_bytes(),
        );
        let (one, two) = (render(vt.screen(), &f1, 1), render(vt.screen(), &f2, 2));
        let (w, h) = (one.0, one.1);
        for y in 0..h * 2 {
            for x in 0..w * 2 {
                let (i, j) = ((y * w * 2 + x) * 3, ((y / 2) * w + x / 2) * 3);
                assert_eq!(two.2[i..i + 3], one.2[j..j + 3], "at ({x}, {y})");
            }
        }
    }

    /// A wide character is drawn at the font's own size — scale 1.0, as the user chose on 2026-10-20 (filling its
    /// two cells, it stood too big next to the English) — in the middle of its two cells (frictions29 8: drawn from
    /// the first cell's edge, `실타래` read `실 타 래`); a bold cell's in the wide font's Bold. (Skipped without
    /// JetBrains Mono or a CJK face; the bold part without a bold CJK face.)
    #[test]
    fn a_wide_character_is_at_the_fonts_own_size_centred_and_bold_in_a_bold_cell() {
        let Ok(f) = Fonts::load(1) else { return };
        let Some(wide) = f.wide.as_ref() else { return };
        let cw = f.regular.metrics('M', SIZE).advance_width as usize;
        let drawn = |text: &str| {
            let mut vt = vt100::Parser::new(1, 8, 0);
            vt.process(format!("\x1b[?25l{text}").as_bytes());
            render(vt.screen(), &f, 1)
        };
        let (w, h, px) = drawn("실타래");
        let lit = |x: usize, y: usize| px[(y * w + x) * 3..(y * w + x) * 3 + 3] != BG;
        for (i, c) in ['실', '타', '래'].into_iter().enumerate() {
            let (x0, x1) = (PAD + 2 * i * cw, PAD + 2 * (i + 1) * cw);
            let cols: Vec<usize> = (x0..x1).filter(|&x| (0..h).any(|y| lit(x, y))).collect();
            let rows = (0..h).filter(|&y| (x0..x1).any(|x| lit(x, y))).count();
            let (Some(&a), Some(&b)) = (cols.first(), cols.last()) else { panic!("{c}: no ink") };
            // (scale 1.0: its ink the font's own glyph's at the shot's size — a pixel's give for antialiasing)
            let m = wide.metrics(c, SIZE);
            assert!((b - a + 1).abs_diff(m.width) <= 1, "{c}: {} px wide, the font's {}", b - a + 1, m.width);
            assert!(rows.abs_diff(m.height) <= 1, "{c}: {rows} px tall, the font's {}", m.height);
            let (left, right) = (a - x0, x1 - 1 - b);
            assert!(left.abs_diff(right) <= 1, "{c}: {left} px of air on its left, {right} on its right");
        }
        // (a bold cell's: the wide font's Bold — Apple SD Gothic Neo's puts down about half again Regular's ink)
        if f.wide_bold.is_some() {
            // (the ink's strength, how far each pixel is from the ground: antialiasing lights about as many either way)
            let ink = |(_, _, px): (usize, usize, Vec<u8>)| -> u64 {
                px.chunks(3).flat_map(|p| p.iter().zip(BG).map(|(a, b)| u64::from(a.abs_diff(b)))).sum()
            };
            let (regular, bold) = (ink(drawn("한")), ink(drawn("\x1b[1m한")));
            assert!(bold * 100 > regular * 125, "a bold cell's 한: ink {bold} against Regular's {regular}");
        }
    }

    /// The font's line sits in the middle of its cell: as much room above its ascent as below its descent,
    /// within the pixel the baseline rounds to (frictions28: letters sat low). (Skipped without JetBrains Mono.)
    #[test]
    fn the_fonts_line_is_centred_in_the_cell() {
        let Ok(f) = Fonts::load(1) else { return };
        let ch1 = (SIZE * 1.45) as usize;
        let l = f.regular.horizontal_line_metrics(SIZE).unwrap();
        let b = baseline(&f.regular, ch1) as f32;
        let (above, below) = (b - l.ascent, ch1 as f32 - b + l.descent);
        assert!((above - below).abs() <= 1.0, "above {above}, below {below} (baseline {b} of {ch1})");
    }

    #[test]
    fn a_scale_is_a_whole_number_from_1_to_4() {
        assert_eq!((scale(None), scale(Some("2")), scale(Some("4"))), (Ok(1), Ok(2), Ok(4)));
        for bad in ["0", "5", "1.5", "two", ""] {
            let e = scale(Some(bad)).unwrap_err();
            assert_eq!(e, format!("TARAE_SHOT_SCALE={bad}: a whole number from 1 to 4"));
        }
    }

    #[test]
    fn palette() {
        assert_eq!(color(vt100::Color::Idx(16), FG), [0, 0, 0]);
        assert_eq!(color(vt100::Color::Idx(231), FG), [255, 255, 255]);
        assert_eq!(color(vt100::Color::Idx(255), FG), [238, 238, 238]);
    }
}
