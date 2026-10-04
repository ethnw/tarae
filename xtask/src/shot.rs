//! `cargo xtask shot out.png [keys …]` — the real screen as a PNG: tarae on a pty, read back through `vt100`
//! (as `tests/e2e.rs` does), drawn cell by cell the way a terminal draws them. The tarae is the `shot`
//! profile's, built first (release's behavior; incremental, so seconds after an edit).
//!
//! Keys are sent in order, 0.35 s apart (`\r` = Enter, `\t` = Tab, `\x1b` = Esc, `\xHH` = any byte; everything
//! else, Korean too, goes out as UTF-8); `@3` waits 3 s taking screen updates; `@?ready` waits until `ready` is
//! on screen (a language server indexing …) — at most 120 s, or `@30?ready` 30.
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
const PAD: usize = 14;
const BG: Rgb = [30, 31, 40];
const FG: Rgb = [248, 248, 242];
/// tarae wraps each frame in a synchronized update (`CSI ?2026h … ?2026l`) — only whole frames are fed, so a
/// capture never lands mid-frame (cards half overdrawn while an animation ticks).
const FRAME_END: &[u8] = b"\x1b[?2026l";

type Rgb = [u8; 3];

pub fn run(args: &[String]) -> Result<()> {
    let (out, keys) = args.split_first().ok_or("usage: cargo xtask shot out.png [keys …]")?;
    let num = |k: &str, d: u16| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (cols, rows) = (num("TARAE_SHOT_COLS", 112), num("TARAE_SHOT_ROWS", 30));
    let mut vt = Vt { parser: vt100::Parser::new(rows, cols, 0), held: Vec::new() };

    if let Some(replay) = std::env::var_os("TARAE_SHOT_REPLAY") {
        let bytes = std::fs::read(&replay).map_err(|e| format!("{}: {e}", Path::new(&replay).display()))?;
        vt.feed(&bytes);
    } else {
        let given = folder("TARAE_SHOT_STATE", std::env::var_os("TARAE_SHOT_STATE"), false)?;
        let state = given
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join(format!("tarae-shot-state-{}", std::process::id())));
        let res = drive(&mut vt, keys, &state, cols, rows);
        if given.is_none() {
            let _ = std::fs::remove_dir_all(&state);
        }
        res?;
    }

    let fonts = Fonts::load()?;
    let (w, h, px) = render(vt.parser.screen(), &fonts);
    save_png(Path::new(out), w, h, &px)?;
    println!("saved {out} ({w}, {h})");
    Ok(())
}

// ── The terminal ─────────────────────────────────────────────────────────────

struct Vt {
    parser: vt100::Parser,
    held: Vec<u8>,
}

impl Vt {
    fn feed(&mut self, data: &[u8]) {
        self.held.extend_from_slice(data);
        let Some(cut) = self.held.windows(FRAME_END.len()).rposition(|w| w == FRAME_END) else { return };
        let frames: Vec<u8> = self.held.drain(..cut + FRAME_END.len()).collect();
        let parser = &mut self.parser;
        // vt100 can panic on odd input — keep the screen so far rather than lose the shot
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.process(&frames)));
    }
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

fn drive(vt: &mut Vt, keys: &[String], state: &Path, cols: u16, rows: u16) -> Result<()> {
    let root = root();
    let bin = match folder("TARAE_SHOT_BIN", std::env::var_os("TARAE_SHOT_BIN"), false)? {
        Some(bin) => bin,
        None => build_shot(&root)?,
    };
    if !bin.is_file() {
        return Err(format!("{} not found", bin.display()));
    }
    let files =
        std::env::var("TARAE_SHOT_FILES").unwrap_or_else(|_| "src/transaction.rs src/selection.rs".into());
    let cwd =
        folder("TARAE_SHOT_CWD", std::env::var_os("TARAE_SHOT_CWD"), true)?.unwrap_or_else(|| root.clone());
    let config = folder("TARAE_SHOT_XDG", std::env::var_os("TARAE_SHOT_XDG"), true)?
        .map_or_else(|| "/tmp/tarae-shot/xdg".into(), PathBuf::into_os_string);

    let (master, slave) = crate::pty::open(rows, cols).map_err(|e| format!("pty: {e}"))?;
    let mut master = File::from(master);
    let stdio = || slave.try_clone().map(Stdio::from).map_err(|e| e.to_string());
    let mut cmd = Command::new(&bin);
    cmd.args(files.split_whitespace()).current_dir(cwd);
    as_a_terminal(&mut cmd, &config, state).stdin(stdio()?).stdout(stdio()?).stderr(stdio()?);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            // `setsid` + the pty as its controlling terminal: /dev/tty works, SIGWINCH arrives.
            cvt(libc::setsid())?;
            cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", bin.display()))?;
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
    // Until `text` is on screen (whole frames — `Vt::feed`) or `max` seconds pass
    let pump_until = |vt: &mut Vt, text: &str, max: f64| -> bool {
        let end = Instant::now() + Duration::from_secs_f64(max);
        loop {
            if vt.parser.screen().contents().contains(text) {
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
    for k in keys {
        if let Some((max, text)) = wait_key(k) {
            let t = Instant::now();
            if pump_until(vt, text, max) {
                eprintln!("shot: {text:?} on screen after {:.1}s", t.elapsed().as_secs_f64());
            } else {
                eprintln!("shot: {text:?} not on screen after {max}s — taking the shot as it is");
            }
            continue;
        }
        if let Some(secs) = k.strip_prefix('@').and_then(|s| s.parse::<f64>().ok()) {
            pump(vt, secs);
            continue;
        }
        let _ = master.write_all(&unescape(k));
        pump(vt, 0.35);
    }
    pump(vt, 0.6);
    quit(&mut master, &mut child, &rx);
    Ok(())
}

/// The longest `@?text` waits unless it says (`@30?text`).
const WAIT_MAX: f64 = 120.0;

/// `@?text` / `@30?text`: wait until `text` is on screen — at most `WAIT_MAX` seconds, or the ones given.
fn wait_key(key: &str) -> Option<(f64, &str)> {
    let (secs, text) = key.strip_prefix('@')?.split_once('?')?;
    let max = if secs.is_empty() { WAIT_MAX } else { secs.parse().ok()? };
    (!text.is_empty()).then_some((max, text))
}

/// The `shot` profile's tarae, built now (`Cargo.toml`: release's behavior, built incrementally without
/// LTO — seconds after an edit where `--release` takes over a minute).
fn build_shot(root: &Path) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--quiet", "--profile", "shot", "--bin", "tarae"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if !st.success() {
        return Err("building the shot profile failed".into());
    }
    Ok(root.join("target/shot/tarae"))
}

/// tarae's environment in the shot: a color terminal, its own config and state. An inherited `NO_COLOR`
/// goes — it's meant for the output of whatever runs it (a task panel sets it), and the shot renders tarae
/// itself: with it, every shot taken from a panel came out gray.
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
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
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
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Face {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Wide,
}

impl Fonts {
    fn load() -> Result<Fonts> {
        let open = |name: &str| -> Option<fontdue::Font> {
            let bytes = std::fs::read(find_font(name)?).ok()?;
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings { scale: SIZE, ..Default::default() }).ok()
        };
        let regular = open("JetBrainsMono-Regular.ttf").ok_or("JetBrainsMono-Regular.ttf not found")?;
        let or_regular = |name| open(name).unwrap_or_else(|| regular.clone());
        let (bold, italic) = (or_regular("JetBrainsMono-Bold.ttf"), or_regular("JetBrainsMono-Italic.ttf"));
        let bold_italic = open("JetBrainsMono-BoldItalic.ttf").unwrap_or_else(|| bold.clone());
        let wide =
            ["AppleSDGothicNeo.ttc", "NotoSansCJK-Regular.ttc", "NanumGothic.ttf"].into_iter().find_map(open);
        Ok(Fonts { regular, bold, italic, bold_italic, wide })
    }

    fn face(&self, f: Face) -> &fontdue::Font {
        match f {
            Face::Regular => &self.regular,
            Face::Bold => &self.bold,
            Face::Italic => &self.italic,
            Face::BoldItalic => &self.bold_italic,
            Face::Wide => self.wide.as_ref().unwrap_or(&self.regular),
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

fn render(screen: &vt100::Screen, fonts: &Fonts) -> (usize, usize, Vec<u8>) {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows as usize, cols as usize);
    let cw = fonts.regular.metrics('M', SIZE).advance_width as usize;
    let ch = (SIZE * 1.45) as usize;
    let cell = |y: usize, x: usize| screen.cell(y as u16, x as u16);
    // The outer margin takes the edit view's background (the command line's last cell — no card covers it)
    let bg = cell(rows - 1, cols - 1).map_or(BG, |c| color(c.bgcolor(), BG));
    let (w, h) = (cols * cw + PAD * 2, rows * ch + PAD * 2);
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
                let (px, py) = (PAD + x * cw, PAD + y * ch);
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
            let (px, py) = (PAD + x * cw, PAD + y * ch);
            if let Some((a0, b0, a1, b1)) = block(text) {
                let at = |t: f32, n: usize| (t * n as f32).round() as usize;
                cv.rect(px + at(a0, cw), py + at(b0, ch), px + at(a1, cw), py + at(b1, ch), fg);
                continue;
            }
            if let Some(arms) = box_arms(text) {
                let (mx, my) = (px + cw / 2, py + ch / 2);
                for a in arms.chars() {
                    match a {
                        'l' => cv.rect(px, my, mx + 1, my + 1, fg),
                        'r' => cv.rect(mx, my, px + cw + 1, my + 1, fg),
                        'u' => cv.rect(mx, py, mx + 1, my + 1, fg),
                        _ => cv.rect(mx, my, mx + 1, py + ch + 1, fg),
                    }
                }
                continue;
            }
            let mut pen = px as f32;
            for chr in text.chars() {
                let face = match (c.bold(), c.italic()) {
                    _ if chr as u32 > 0x2e80 && fonts.wide.is_some() => Face::Wide,
                    (true, true) => Face::BoldItalic,
                    (true, false) => Face::Bold,
                    (false, true) => Face::Italic,
                    _ => Face::Regular,
                };
                // A glyph the mono font lacks: try the wide font
                let face = if face != Face::Wide
                    && fonts.face(face).lookup_glyph_index(chr) == 0
                    && fonts.wide.is_some()
                {
                    Face::Wide
                } else {
                    face
                };
                let font = fonts.face(face);
                let (m, bitmap) = glyphs.entry((face, chr)).or_insert_with(|| font.rasterize(chr, SIZE));
                // ceil, as FreeType rounds a scaled ascender (PIL, the reference renders, used it)
                let ascent = font.horizontal_line_metrics(SIZE).map_or(SIZE, |l| l.ascent).ceil() as i64;
                let baseline = (py + 3) as i64 + ascent;
                let top = baseline - m.ymin as i64 - m.height as i64;
                let left = pen.round() as i64 + m.xmin as i64;
                for (i, a) in bitmap.iter().enumerate() {
                    cv.blend(left + (i % m.width.max(1)) as i64, top + (i / m.width.max(1)) as i64, fg, *a);
                }
                pen += m.advance_width;
            }
            if c.underline() {
                let uw = cw * if c.is_wide() { 2 } else { 1 };
                cv.rect(px, py + ch - 3, px + uw, py + ch - 2, fg);
            }
        }
    }
    // 3) The cursor: a thin bar in the cell's text color
    if !screen.hide_cursor() {
        let (cy, cx) = screen.cursor_position();
        let (cy, cx) = (cy as usize, cx as usize);
        let fg = if cx < cols { cell(cy, cx).map_or(FG, |c| color(c.fgcolor(), FG)) } else { FG };
        let (px, py) = (PAD + cx * cw, PAD + cy * ch);
        cv.rect(px, py, px + 2, py + ch, fg);
    }
    (w, h, cv.px)
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
        let mut vt = Vt { parser: vt100::Parser::new(2, 10, 0), held: Vec::new() };
        vt.feed(b"\x1b[?2026hab\x1b[?2026l\x1b[?2026hcd");
        assert_eq!(vt.parser.screen().contents(), "ab");
        vt.feed(b"\x1b[?2026l");
        assert_eq!(vt.parser.screen().contents(), "abcd");
    }

    /// A shot taken where `NO_COLOR=1` is set (a task panel) is still in color: tarae doesn't see it. (A
    /// stand-in prints what it was given; the variable set the way an inherited one would be.)
    #[test]
    fn an_inherited_no_color_doesnt_reach_tarae() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", r#"printf '%s|%s' "${NO_COLOR-unset}" "$TERM""#]).env("NO_COLOR", "1");
        let out = as_a_terminal(&mut cmd, "/nonexistent-config".as_ref(), Path::new("/nonexistent-state"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|xterm-256color");
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

    #[test]
    fn palette() {
        assert_eq!(color(vt100::Color::Idx(16), FG), [0, 0, 0]);
        assert_eq!(color(vt100::Color::Idx(231), FG), [255, 255, 255]);
        assert_eq!(color(vt100::Color::Idx(255), FG), [238, 238, 238]);
    }
}
