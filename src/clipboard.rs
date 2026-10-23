//! System clipboard = the `+` register. External commands (pbcopy/pbpaste · on WSL the Windows clipboard
//! (win32yank, PowerShell) · wl-copy/wl-paste · xclip/xsel), so
//! per principle 4 they run on a worker thread. Tests use memory so the user's clipboard stays untouched.
//!
//! Over SSH those programs would reach the server's clipboard, not the user's, and a server often has none:
//! a copy goes through the terminal instead (OSC 52, as Helix's termcode provider and Neovim 0.10 do), also
//! where no clipboard program is there. The sequence goes out with tarae's own terminal output, after the
//! next frame (`Editor::term_out`) — never written from a worker. Reading through OSC 52 is refused by most
//! terminals, so a paste there says to use the terminal's own paste (it comes in as a bracketed paste).

use crate::editor::{Editor, ToastKind};
use crate::event::Jobs;

/// Copies `text`; once done, one toast says so, naming it (`what`: "1 selection") and how.
pub fn copy(jobs: &Jobs, text: String, what: String) {
    copy_flashing(jobs, text, what, None)
}

/// `copy`, and a yank's `flash` (`motion.rs`) once the clipboard took it — this copy's own, carried with it (another
/// copy landing first never takes it), saying it where it's drawn.
pub fn copy_flashing(jobs: &Jobs, text: String, what: String, flash: Option<crate::motion::What>) {
    jobs.spawn(move || {
        let r = system::write(&text);
        move |ed: &mut Editor| copied(ed, &what, r, flash)
    });
}

/// "1 selection", "3 selections" — what a copy of a register's values says it copied.
pub fn selections(n: usize) -> String {
    format!("{n} selection{}", if n == 1 { "" } else { "s" })
}

/// How a copy went out.
#[derive(Debug, PartialEq)]
pub enum Wrote {
    /// The clipboard program took it.
    Program,
    /// These bytes, for the terminal (OSC 52), and why that way.
    Terminal(Vec<u8>, Why),
}

/// Why the clipboard is the terminal's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Why {
    /// Over SSH: a program here would reach this machine's clipboard, not the user's.
    Ssh,
    /// No clipboard program here (or nothing for it to talk to: no display).
    NoProgram,
}

/// The result of a copy, on the main loop: the terminal's bytes go out after the next frame.
fn copied(ed: &mut Editor, what: &str, r: Result<Wrote, String>, flash: Option<crate::motion::What>) {
    // (a yank's flash once the copy is in, saying it where it's drawn — none when it failed: the error says so;
    // through the terminal a toast too, for how: the terminal may not take it)
    match r {
        Ok(Wrote::Program) => ed.flash_saying(flash, ToastKind::Success, format!("copied {what}")),
        Ok(Wrote::Terminal(bytes, why)) => {
            if let Some(f) = flash {
                ed.motion_start(f);
            }
            ed.term_out.extend_from_slice(&bytes);
            ed.set_success(match why {
                Why::Ssh => format!("copied {what} — through the terminal (OSC 52)"),
                Why::NoProgram => {
                    format!("copied {what} — through the terminal (OSC 52: no wl-copy, xclip or xsel here)")
                }
            });
        }
        Err(e) => ed.set_error(format!("not copied — {e}")),
    }
}

/// What a read of the clipboard brought.
#[derive(Debug, PartialEq)]
pub enum Read {
    Text(String),
    /// The clipboard is the terminal's: its own paste brings it in.
    Terminal(Why),
}

/// Read the clipboard and pass it to `then` (on the main loop, after the read finishes).
pub fn paste(jobs: &Jobs, then: impl FnOnce(&mut Editor, String) + Send + 'static) {
    jobs.spawn(move || {
        let r = system::read();
        move |ed: &mut Editor| pasted(ed, r, then)
    });
}

/// The result of a read, on the main loop.
fn pasted(ed: &mut Editor, r: Result<Read, String>, then: impl FnOnce(&mut Editor, String)) {
    match r {
        Ok(Read::Text(s)) if s.is_empty() => ed.set_status("clipboard is empty"),
        Ok(Read::Text(s)) => then(ed, s),
        Ok(Read::Terminal(why)) => ed.set_status(paste_hint(why)),
        Err(e) => ed.set_error(format!("clipboard: {e}")),
    }
}

/// What a paste says where the clipboard is the terminal's.
fn paste_hint(why: Why) -> &'static str {
    match why {
        Why::Ssh => "over SSH the clipboard is on your side — paste with your terminal's own paste key",
        Why::NoProgram => "no wl-paste, xclip or xsel here — paste with your terminal's own paste key",
    }
}

/// Text from Windows is CRLF — LF inside the editor (PowerShell also appends a trailing newline).
fn from_powershell(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    match s.strip_suffix('\n') {
        Some(t) => t.to_string(),
        None => s,
    }
}

/// The clipboard programs looked for on PATH (pbcopy/pbpaste are always there on macOS).
const PROGRAMS: &[&str] = &["win32yank.exe", "wl-copy", "wl-paste", "xclip", "xsel", "powershell.exe"];

/// What the choice of a clipboard depends on — read once per copy or paste (`Env::read`), passed in so the
/// choice itself reads nothing.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub ssh: bool,
    pub macos: bool,
    pub wsl: bool,
    /// `WAYLAND_DISPLAY` · `DISPLAY`
    pub wayland: bool,
    pub x11: bool,
    pub mux: Mux,
    /// tmux's pane (`TMUX_PANE`), whose options say what it does with OSC 52.
    pub tmux_pane: Option<String>,
    /// Which of `PROGRAMS` are on PATH.
    pub programs: Vec<&'static str>,
}

/// The terminal multiplexer tarae runs in: an OSC 52 must be wrapped to get through it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mux {
    #[default]
    None,
    Tmux,
    Screen,
}

impl Env {
    /// From the environment's variables (`var`) and what's on PATH (`has`); `macos` and `wsl` are the
    /// caller's.
    pub fn new(var: impl Fn(&str) -> Option<String>, has: impl Fn(&str) -> bool) -> Env {
        let set = |k: &str| var(k).is_some_and(|v| !v.is_empty());
        Env {
            // (SSH_TTY only with a terminal; SSH_CONNECTION also for `ssh host tarae` without one)
            ssh: set("SSH_TTY") || set("SSH_CONNECTION"),
            macos: false,
            wsl: set("WSL_DISTRO_NAME"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            mux: if set("TMUX") {
                Mux::Tmux
            } else if set("STY") {
                Mux::Screen
            } else {
                Mux::None
            },
            tmux_pane: var("TMUX_PANE").filter(|p| !p.is_empty()),
            programs: PROGRAMS.iter().copied().filter(|p| has(p)).collect(),
        }
    }
}

/// Where a copy or paste goes: a program and its arguments, or the terminal.
#[derive(Debug, PartialEq)]
pub enum Way {
    Program(&'static str, &'static [&'static str]),
    Terminal(Why),
}

// PowerShell needs UTF-8 set explicitly or Hangul breaks (clip.exe uses the console code page — breaks)
const PS_SET: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "[Console]::InputEncoding=[Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
];
const PS_GET: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw",
];

/// SSH: the terminal · macOS: pb* · WSL: win32yank → (WSLg) wl-* → PowerShell · Linux: wl-* → xclip → xsel
/// (each only with its display) · else the terminal.
pub fn pick(env: &Env, copy: bool) -> Way {
    if env.ssh {
        return Way::Terminal(Why::Ssh);
    }
    if env.macos {
        return if copy { Way::Program("pbcopy", &[]) } else { Way::Program("pbpaste", &[]) };
    }
    let has = |p: &str| env.programs.contains(&p);
    let wayland = env.wayland && has(if copy { "wl-copy" } else { "wl-paste" });
    let wl = || if copy { Way::Program("wl-copy", &[]) } else { Way::Program("wl-paste", &["-n"]) };
    if env.wsl {
        if has("win32yank.exe") {
            return if copy {
                Way::Program("win32yank.exe", &["-i", "--crlf"])
            } else {
                Way::Program("win32yank.exe", &["-o", "--lf"])
            };
        }
        if wayland {
            return wl();
        }
        if has("powershell.exe") {
            return Way::Program("powershell.exe", if copy { PS_SET } else { PS_GET });
        }
        return Way::Terminal(Why::NoProgram);
    }
    if wayland {
        return wl();
    }
    if env.x11 && has("xclip") {
        return if copy {
            Way::Program("xclip", &["-selection", "clipboard"])
        } else {
            Way::Program("xclip", &["-selection", "clipboard", "-o"])
        };
    }
    if env.x11 && has("xsel") {
        return if copy {
            Way::Program("xsel", &["--clipboard", "--input"])
        } else {
            Way::Program("xsel", &["--clipboard", "--output"])
        };
    }
    Way::Terminal(Why::NoProgram)
}

/// The longest OSC 52 sent: tmux drops a sequence past its input buffer's 1 MB, and terminals cap it too.
const OSC52_MAX: usize = 1 << 20;

/// GNU screen passes a DCS string on only up to its buffer (768 bytes): the payload goes in pieces.
const SCREEN_CHUNK: usize = 76;

/// The OSC 52 that sets the clipboard to `text`, wrapped for the multiplexer it goes through. `tmux` = its
/// `set-clipboard` and `allow-passthrough` as asked (`None`: couldn't ask).
pub fn osc52(text: &str, mux: Mux, tmux: Option<(&str, &str)>) -> Result<Vec<u8>, String> {
    let b64 = base64(text.as_bytes());
    if b64.len() > OSC52_MAX {
        return Err(format!(
            "{} KB is too long to copy through the terminal (OSC 52 takes up to {} KB)",
            text.len() / 1024,
            OSC52_MAX / 4 * 3 / 1024
        ));
    }
    let raw = format!("\x1b]52;c;{b64}\x1b\\");
    // Inside a DCS passthrough the OSC ends on BEL: an ESC there would have to be doubled
    let tmux_wrapped = format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\");
    let out = match (mux, tmux) {
        (Mux::None, _) => raw,
        (Mux::Screen, _) => {
            let pieces: Vec<&str> =
                b64.as_bytes().chunks(SCREEN_CHUNK).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
            format!("\x1bP\x1b]52;c;{}\x07\x1b\\", pieces.join("\x1b\\\x1bP"))
        }
        // `on`: tmux takes it (a buffer of its own too) and sets the terminal's clipboard
        (Mux::Tmux, Some(("on", _))) => raw,
        // Passed through untouched — `on`/`all`, or a tmux before 3.3 that has no such option and passes it
        (Mux::Tmux, Some((_, "on" | "all" | ""))) => tmux_wrapped,
        (Mux::Tmux, Some(_)) => {
            return Err("tmux keeps programs' OSC 52 to itself — `set -g set-clipboard on` in tmux.conf \
                        lets it through"
                .into());
        }
        // Couldn't ask: both — whichever tmux takes sets the same text
        (Mux::Tmux, None) => raw + &tmux_wrapped,
    };
    Ok(out.into_bytes())
}

/// tmux's answer to `display-message -p '#{set-clipboard}|#{allow-passthrough}'`.
fn tmux_options(out: &str) -> Option<(String, String)> {
    let (clip, pass) = out.trim_end_matches(['\n', '\r']).split_once('|')?;
    (!clip.is_empty()).then(|| (clip.to_string(), pass.to_string()))
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(not(test))]
mod system {
    use super::{Env, Mux, Read, Way, Wrote};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn on_path(cmd: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
    }

    /// WSL: the Linux side usually has no clipboard tool — use the Windows clipboard.
    fn wsl() -> bool {
        std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|s| s.to_lowercase().contains("microsoft"))
    }

    fn env() -> Env {
        let mut env = Env::new(|k| std::env::var(k).ok(), on_path);
        env.macos = cfg!(target_os = "macos");
        env.wsl = !env.macos && wsl();
        env
    }

    /// What tmux does with an OSC 52 from a program: its `set-clipboard` and `allow-passthrough` in our pane.
    fn ask_tmux(env: &Env) -> Option<(String, String)> {
        let mut cmd = Command::new("tmux");
        cmd.args(["display-message", "-p"]);
        if let Some(pane) = &env.tmux_pane {
            cmd.args(["-t", pane]);
        }
        let out = cmd.arg("#{set-clipboard}|#{allow-passthrough}").stdin(Stdio::null()).output().ok()?;
        out.status.success().then(|| super::tmux_options(&String::from_utf8_lossy(&out.stdout)))?
    }

    pub fn write(text: &str) -> Result<Wrote, String> {
        let env = env();
        let (cmd, args) = match super::pick(&env, true) {
            Way::Program(cmd, args) => (cmd, args),
            Way::Terminal(why) => {
                let tmux = if env.mux == Mux::Tmux { ask_tmux(&env) } else { None };
                let tmux = tmux.as_ref().map(|(c, p)| (c.as_str(), p.as_str()));
                return Ok(Wrote::Terminal(super::osc52(text, env.mux, tmux)?, why));
            }
        };
        // (stderr not read: xclip and wl-copy stay behind holding the selection, and keep it open)
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{cmd}: {e}"))?;
        child.stdin.take().ok_or("no stdin")?.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("{cmd} failed ({status})"));
        }
        Ok(Wrote::Program)
    }

    pub fn read() -> Result<Read, String> {
        let (cmd, args) = match super::pick(&env(), false) {
            Way::Program(cmd, args) => (cmd, args),
            Way::Terminal(why) => return Ok(Read::Terminal(why)),
        };
        let out = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(match err.lines().rfind(|l| !l.trim().is_empty()) {
                Some(line) => format!("{cmd}: {}", line.trim()),
                None => format!("{cmd} failed ({})", out.status),
            });
        }
        let s = String::from_utf8_lossy(&out.stdout).into_owned();
        Ok(Read::Text(if cmd == "powershell.exe" { super::from_powershell(&s) } else { s }))
    }
}

/// Tests: whether `text` was copied (the tests' clipboard is memory, shared by every test).
#[cfg(test)]
pub fn written(text: &str) -> bool {
    system::written(text)
}

#[cfg(test)]
mod system {
    use super::{Read, Wrote};
    use std::sync::Mutex;

    static CLIP: Mutex<String> = Mutex::new(String::new());
    /// Every copy since the tests started — a test asks whether its own text went (others copy alongside).
    static WRITTEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub fn write(text: &str) -> Result<Wrote, String> {
        *CLIP.lock().unwrap() = text.to_string();
        WRITTEN.lock().unwrap().push(text.to_string());
        Ok(Wrote::Program)
    }

    pub fn written(text: &str) -> bool {
        WRITTEN.lock().unwrap().iter().any(|t| t == text)
    }

    pub fn read() -> Result<Read, String> {
        Ok(Read::Text(CLIP.lock().unwrap().clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_text_becomes_lf() {
        assert_eq!(super::from_powershell("a\r\nb\r\n"), "a\nb");
        assert_eq!(super::from_powershell("a\r\nb"), "a\nb", "no trailing newline — still LF");
    }

    fn env(vars: &[(&str, &str)], programs: &[&str]) -> Env {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Env::new(|k| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()), |p| programs.contains(&p))
    }

    #[test]
    fn the_environment_is_read_into_what_the_choice_needs() {
        assert!(env(&[("SSH_TTY", "/dev/pts/1")], &[]).ssh);
        assert!(env(&[("SSH_CONNECTION", "10.0.0.2 5 10.0.0.1 22")], &[]).ssh, "without a terminal too");
        assert!(!env(&[("SSH_TTY", "")], &[]).ssh, "set but empty isn't SSH");
        assert!(!env(&[], &[]).ssh);
        assert_eq!(env(&[("TMUX", "/tmp/tmux-1/default,1,0"), ("STY", "1.x")], &[]).mux, Mux::Tmux);
        assert_eq!(env(&[("STY", "1.pts-0.host")], &[]).mux, Mux::Screen);
        assert_eq!(env(&[], &[]).mux, Mux::None);
        assert_eq!(env(&[("TMUX_PANE", "%3")], &[]).tmux_pane.as_deref(), Some("%3"));
        let e = env(&[("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-0")], &["xclip", "nope"]);
        assert!(e.x11 && e.wayland);
        assert_eq!(e.programs, ["xclip"], "only the clipboard programs, as found");
    }

    #[test]
    fn a_copy_over_ssh_or_without_a_program_goes_through_the_terminal() {
        let term = |w| Way::Terminal(w);
        // Over SSH: the terminal, whatever is installed (a program would reach the server's clipboard)
        let ssh = env(&[("SSH_TTY", "/dev/pts/1"), ("DISPLAY", ":0")], &["xclip", "wl-copy"]);
        assert_eq!(pick(&ssh, true), term(Why::Ssh));
        assert_eq!(pick(&ssh, false), term(Why::Ssh));
        let mac_ssh = Env { macos: true, ..ssh };
        assert_eq!(pick(&mac_ssh, true), term(Why::Ssh), "a Mac reached over SSH too");
        // macOS here
        let mac = Env { macos: true, ..env(&[], &[]) };
        assert_eq!(pick(&mac, true), Way::Program("pbcopy", &[]));
        assert_eq!(pick(&mac, false), Way::Program("pbpaste", &[]));
        // Linux: wl-* with Wayland, then xclip, then xsel — each only with its display
        let x = env(&[("DISPLAY", ":0")], &["xclip", "xsel"]);
        assert_eq!(pick(&x, true), Way::Program("xclip", &["-selection", "clipboard"]));
        assert_eq!(pick(&x, false), Way::Program("xclip", &["-selection", "clipboard", "-o"]));
        let xsel = env(&[("DISPLAY", ":0")], &["xsel"]);
        assert_eq!(pick(&xsel, true), Way::Program("xsel", &["--clipboard", "--input"]));
        let wl = env(&[("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "w")], &["xclip", "wl-copy", "wl-paste"]);
        assert_eq!(pick(&wl, true), Way::Program("wl-copy", &[]));
        assert_eq!(pick(&wl, false), Way::Program("wl-paste", &["-n"]));
        let wl_only_paste = env(&[("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "w")], &["xclip", "wl-paste"]);
        assert_eq!(pick(&wl_only_paste, true), Way::Program("xclip", &["-selection", "clipboard"]));
        assert_eq!(pick(&env(&[], &["xclip", "xsel"]), true), term(Why::NoProgram), "no display");
        assert_eq!(pick(&env(&[("DISPLAY", ":0")], &[]), true), term(Why::NoProgram), "nothing installed");
        assert_eq!(pick(&env(&[("DISPLAY", ":0")], &[]), false), term(Why::NoProgram));
        // WSL: win32yank → wl-* → PowerShell → the terminal
        let wsl = |progs: &[&str]| Env { wsl: true, ..env(&[("WAYLAND_DISPLAY", "w")], progs) };
        let all = ["win32yank.exe", "wl-copy", "powershell.exe"];
        assert_eq!(pick(&wsl(&all), true), Way::Program("win32yank.exe", &["-i", "--crlf"]));
        assert_eq!(pick(&wsl(&all[1..]), true), Way::Program("wl-copy", &[]));
        assert_eq!(pick(&wsl(&all[2..]), true), Way::Program("powershell.exe", PS_SET));
        assert_eq!(pick(&wsl(&all[2..]), false), Way::Program("powershell.exe", PS_GET));
        assert_eq!(pick(&wsl(&[]), true), term(Why::NoProgram));
    }

    #[test]
    fn base64_is_the_standard_one() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("한글", "7ZWc6riA"),
        ] {
            assert_eq!(base64(plain.as_bytes()), coded, "{plain:?}");
        }
    }

    fn osc(text: &str, mux: Mux, tmux: Option<(&str, &str)>) -> String {
        String::from_utf8(osc52(text, mux, tmux).unwrap()).unwrap()
    }

    #[test]
    fn osc52_is_wrapped_for_the_multiplexer_it_goes_through() {
        let raw = "\x1b]52;c;aGk=\x1b\\";
        let wrapped = "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\";
        assert_eq!(osc("hi", Mux::None, None), raw);
        // tmux: `set-clipboard on` takes it as it is; else passthrough wraps it; neither → said how
        assert_eq!(osc("hi", Mux::Tmux, Some(("on", "off"))), raw);
        assert_eq!(osc("hi", Mux::Tmux, Some(("external", "on"))), wrapped);
        assert_eq!(osc("hi", Mux::Tmux, Some(("external", "all"))), wrapped);
        assert_eq!(osc("hi", Mux::Tmux, Some(("external", ""))), wrapped, "tmux before 3.3 passes it");
        let kept = osc52("hi", Mux::Tmux, Some(("external", "off"))).unwrap_err();
        assert!(kept.contains("set -g set-clipboard on"), "{kept}");
        assert!(osc52("hi", Mux::Tmux, Some(("off", "off"))).is_err());
        assert_eq!(osc("hi", Mux::Tmux, None), format!("{raw}{wrapped}"), "couldn't ask: both");
        // screen: one DCS per piece, each short enough for its buffer
        let text = "x".repeat(200);
        let s = osc(&text, Mux::Screen, None);
        assert!(s.starts_with("\x1bP\x1b]52;c;") && s.ends_with("\x07\x1b\\"), "{s:?}");
        let pieces: Vec<&str> = s.split("\x1b\\\x1bP").collect();
        assert_eq!(pieces.len(), base64(text.as_bytes()).len().div_ceil(SCREEN_CHUNK));
        assert!(pieces.iter().all(|p| p.len() <= SCREEN_CHUNK + 16), "{pieces:?}");
        let joined: String = pieces.concat();
        assert!(joined.contains(&base64(text.as_bytes())), "the payload whole once the pieces are joined");
    }

    #[test]
    fn a_copy_too_long_for_the_terminal_is_said_not_sent() {
        let fits = "a".repeat(OSC52_MAX / 4 * 3);
        assert!(osc52(&fits, Mux::None, None).is_ok());
        let err = osc52(&format!("{fits}a"), Mux::None, None).unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    #[test]
    fn tmux_says_its_options() {
        assert_eq!(tmux_options("external|off\n"), Some(("external".into(), "off".into())));
        assert_eq!(tmux_options("on|\n"), Some(("on".into(), String::new())), "no allow-passthrough");
        assert_eq!(tmux_options("\n"), None);
        assert_eq!(tmux_options("|on"), None, "no set-clipboard: not a tmux that answered");
    }

    fn said(ed: &Editor) -> Vec<(String, crate::editor::ToastKind)> {
        ed.toasts.iter().map(|t| (t.text.clone(), t.kind)).collect()
    }

    #[test]
    fn a_copy_says_once_what_was_done_and_how() {
        use crate::editor::ToastKind as Severity;
        let mut ed = Editor::new(crate::config::Config::default());
        copied(&mut ed, "1 selection", Ok(Wrote::Program), None);
        assert_eq!(said(&ed), [("copied 1 selection".to_string(), Severity::Success)]);
        assert!(ed.term_out.is_empty());

        let mut ed = Editor::new(crate::config::Config::default());
        copied(&mut ed, "1 selection", Ok(Wrote::Terminal(b"\x1b]52;c;YQ==\x1b\\".to_vec(), Why::Ssh)), None);
        assert_eq!(
            said(&ed),
            [("copied 1 selection — through the terminal (OSC 52)".to_string(), Severity::Success)]
        );
        assert_eq!(ed.term_out, b"\x1b]52;c;YQ==\x1b\\", "for the terminal, with the next frame");

        // A failure is only that: no success beside it
        let mut ed = Editor::new(crate::config::Config::default());
        copied(&mut ed, "1 selection", Err("xclip failed (exit status: 1)".into()), None);
        assert_eq!(said(&ed), [("not copied — xclip failed (exit status: 1)".to_string(), Severity::Error)]);
        assert!(ed.term_out.is_empty());
    }

    /// A yank's copy carries its own flash: another copy landing first takes none of it; landed and drawn, its words
    /// a note (in `:messages`, no toast); the buffer moved before it landed — nothing drawn — the toast after the
    /// frame; failed, an error toast and no flash.
    #[test]
    fn a_yanks_copy_flashes_its_own_and_says_it_where_drawn() {
        use crate::editor::ToastKind as Severity;
        use crate::key::{Code, Key};
        let frame = |ed: &mut Editor| crate::term::render(ed, &mut Vec::new(), 80, 12).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.motion_drawn = true;
        ed.docs[0].text = ropey::Rope::from_str("one\n");
        let yank = |ed: &Editor| ed.flash_of(crate::motion::Act::Yank, std::iter::once(0..3).collect());
        let mine = yank(&ed);
        copied(&mut ed, "the message", Ok(Wrote::Program), None);
        assert_eq!(
            (said(&ed), ed.motions.len()),
            (vec![("copied the message".to_string(), Severity::Success)], 0),
            "another copy first: the yank's flash isn't its"
        );
        ed.toasts.clear();
        copied(&mut ed, "1 selection", Ok(Wrote::Program), mine);
        frame(&mut ed);
        assert_eq!((said(&ed), ed.motions.len()), (vec![], 1), "its own: flashed and drawn, no toast");
        assert_eq!(ed.messages.last().map(|m| m.2.as_str()), Some("copied 1 selection"), "in :messages");
        // (the buffer moved before the copy landed: its flash draws nothing — the toast)
        let moved = yank(&ed);
        for k in [Key::plain(Code::Char('i')), Key::plain(Code::Char('a')), "esc".parse().unwrap()] {
            ed.handle_event(crate::event::Event::Key(k));
        }
        copied(&mut ed, "1 selection", Ok(Wrote::Program), moved);
        assert!(said(&ed).is_empty(), "not before the frame");
        frame(&mut ed);
        assert_eq!(said(&ed), [("copied 1 selection".to_string(), Severity::Success)], "not drawn: a toast");
        ed.toasts.clear();
        ed.motions.clear();
        let failed = yank(&ed);
        copied(&mut ed, "1 selection", Err("xclip failed (exit status: 1)".into()), failed);
        assert_eq!(said(&ed), [("not copied — xclip failed (exit status: 1)".to_string(), Severity::Error)]);
        assert!(ed.motions.is_empty(), "nothing flashes");
    }

    #[test]
    fn a_paste_where_the_clipboard_is_the_terminals_says_how_to_paste() {
        use crate::editor::ToastKind;
        for why in [Why::Ssh, Why::NoProgram] {
            let mut ed = Editor::new(crate::config::Config::default());
            pasted(&mut ed, Ok(Read::Terminal(why)), |_, _| panic!("nothing to paste"));
            let said = said(&ed);
            assert_eq!(said.len(), 1, "{said:?}");
            assert_eq!(said[0].1, ToastKind::Info, "not a failure: {said:?}");
            assert!(said[0].0.contains("terminal's own paste key"), "{said:?}");
        }
        let mut ed = Editor::new(crate::config::Config::default());
        let mut got = None;
        pasted(&mut ed, Ok(Read::Text("a".into())), |_, s| got = Some(s));
        assert_eq!(got.as_deref(), Some("a"));
    }
}
