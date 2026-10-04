//! The pty harness shared by tests/e2e.rs (offline, every `cargo test`) and tests/live.rs (real tools, on
//! demand). Each uses part of it.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_tarae");
/// Generous — a loaded CI runner can be slow; a passing test never waits this long.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Live runs wait on real tools: rust-analyzer indexing, cargo building, Claude thinking.
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(240);
pub const POLL: Duration = Duration::from_millis(20);
pub const COLS: u16 = 100;
pub const ROWS: u16 = 30;

/// Baseline config for every sandbox: nothing that reaches outside the sandbox or pops up on its own.
/// (PATH is minimal too, so even with `lsp` on no language server would be found.)
pub const BASE_CONFIG: &str = "\
# Written by tests/e2e.rs
[editor]
lsp = false
offer-grammars = false
";

/// Live runs: the real language servers and claude, but still nothing popping up on its own.
pub const LIVE_CONFIG: &str = "\
# Written by tests/live.rs
[editor]
offer-grammars = false
";

/// Variables a live run doesn't pass on from the shell it was started in: the running Claude Code's
/// (a nested claude would take them as its own), multiplexers', and ones that change what tarae shows.
const LIVE_DROP: &[&str] = &["CLAUDE", "TMUX", "ZELLIJ", "TARAE_", "XDG_", "COLORFGBG", "TERM", "NO_COLOR"];

// ── Sandbox ──────────────────────────────────────────────────────────────────

/// A fresh directory standing in for HOME, the XDG dirs, and the working directory. Removed on drop.
pub struct Sandbox {
    root: PathBuf,
    /// Real PATH and HOME (language servers, cargo, claude and its login) — see `live`.
    live: bool,
    /// Folders put in front of the minimal PATH — a tool a test needs as you have it (`with_tools`).
    tools: Vec<PathBuf>,
    /// More variables for the child (`with_env`).
    vars: Vec<(OsString, OsString)>,
}

impl Sandbox {
    pub fn new(name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tarae-e2e-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Real path (macOS: /var → /private/var) — tarae canonicalizes the paths it opens.
        let root = fs::canonicalize(&dir).unwrap();
        let sb = Sandbox { root, live: false, tools: Vec::new(), vars: Vec::new() };
        for d in
            ["home/.config/tarae", "home/.local/state", "home/.local/share", "home/.cache", "claude", "work"]
        {
            fs::create_dir_all(sb.root.join(d)).unwrap();
        }
        sb.write_config(BASE_CONFIG);
        // Project config is searched upward from the working directory — stop the search here so a stray
        // /tmp/.tarae.toml can't leak in.
        sb.write(".tarae.toml", "# tests/e2e.rs\n");
        sb
    }

    /// A sandbox for tests/live.rs: tarae's own config and state stay in the temp dir as always, but the child
    /// sees the real PATH and HOME — rust-analyzer, cargo and a logged-in claude work as they do for you.
    pub fn live(name: &str) -> Self {
        let mut sb = Sandbox::new(&format!("live-{name}"));
        sb.live = true;
        sb.write_config(LIVE_CONFIG);
        // git gets a config of its own — your global one (signing, hooks, aliases) stays out of it
        fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n")
            .unwrap();
        sb
    }

    /// The child finds these programs as you do (a tool a scenario runs) — their folders, from your PATH,
    /// go in front of the minimal one. Panics if one isn't installed.
    pub fn with_tools(mut self, names: &[&str]) -> Self {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for name in names {
            let dir = std::env::split_paths(&path)
                .find(|d| d.join(name).is_file())
                .unwrap_or_else(|| panic!("{name} isn't on PATH — this test needs it"));
            if !self.tools.contains(&dir) {
                self.tools.push(dir);
            }
        }
        self
    }

    /// One more variable in the child's environment (a measurement's hook: `TARAE_TREES_UNSEEN`).
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.vars.push((key.into(), value.into()));
        self
    }

    pub fn git_config(&self) -> PathBuf {
        self.root.join("gitconfig")
    }

    /// Runs git in the work dir with the sandbox's git config; panics if it fails.
    pub fn git(&self, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(self.work())
            .env("GIT_CONFIG_GLOBAL", self.git_config())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(st.success(), "git {args:?}");
    }

    pub fn timeout(&self) -> Duration {
        if self.live { LIVE_TIMEOUT } else { TIMEOUT }
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// The working directory tarae runs in — test files go here.
    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.work().join(name)
    }

    pub fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.file(name);
        fs::write(&p, content).unwrap();
        p
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.file(name)).unwrap_or_else(|e| panic!("reading {name}: {e}"))
    }

    pub fn write_config(&self, content: &str) {
        fs::write(self.home().join(".config/tarae/config.toml"), content).unwrap();
    }

    /// Where Claude Code IDE lock files go (`$CLAUDE_CONFIG_DIR/ide`).
    pub fn claude_ide(&self) -> PathBuf {
        self.root.join("claude/ide")
    }

    /// Claude Code IDE lock files there (`$CLAUDE_CONFIG_DIR/ide/*.lock`).
    pub fn lock_files(&self) -> Vec<PathBuf> {
        let Ok(rd) = fs::read_dir(self.claude_ide()) else { return Vec::new() };
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "lock")).collect()
    }

    /// The whole environment the child sees — everything else is cleared (CLAUDE_*, TMUX, ZELLIJ*,
    /// TARAE_*, COLORFGBG, TERM_PROGRAM … can't leak in from the developer's shell or CI).
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let home = self.home();
        let mut path = self.tools.clone();
        path.extend(["/usr/bin".into(), "/bin".into()]);
        let path = std::env::join_paths(path).unwrap_or_default();
        if self.live {
            let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
                .filter(|(k, _)| !LIVE_DROP.iter().any(|p| k.to_string_lossy().starts_with(p)))
                .collect();
            env.extend([
                ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
                ("XDG_STATE_HOME".into(), home.join(".local/state").into()),
                ("XDG_CACHE_HOME".into(), home.join(".cache").into()),
                ("TERM".into(), "xterm-256color".into()),
                ("TARAE_BACKGROUND".into(), "dark".into()),
                ("GIT_CONFIG_GLOBAL".into(), self.git_config().into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ]);
            env.extend(self.vars.iter().cloned());
            return env;
        }
        let base: Vec<(OsString, OsString)> = [
            ("HOME", home.clone().into()),
            ("XDG_CONFIG_HOME", home.join(".config").into()),
            ("XDG_STATE_HOME", home.join(".local/state").into()),
            ("XDG_DATA_HOME", home.join(".local/share").into()),
            ("XDG_CACHE_HOME", home.join(".cache").into()),
            // tarae writes a Claude Code lock file on startup — never into the real ~/.claude
            ("CLAUDE_CONFIG_DIR", self.root.join("claude").into()),
            // Minimal PATH: no language servers, debuggers or claude from ~/.cargo/bin etc.
            ("PATH", path),
            ("SHELL", "/bin/sh".into()),
            ("TERM", "xterm-256color".into()),
            // The harness doesn't answer the OSC 11 background query — skip it (and its 200 ms timeout)
            ("TARAE_BACKGROUND", "dark".into()),
        ]
        .into_iter()
        .map(|(k, v): (&str, OsString)| (k.into(), v))
        .collect();
        base.into_iter().chain(self.vars.iter().cloned()).collect()
    }

    pub fn spawn(&self, args: &[&str]) -> Term {
        Term::spawn(self, args, COLS, ROWS)
    }

    /// tarae on `args` (`cols`×`rows`) in the middle of a chat turn that's all waiting: a stand-in claude
    /// that answers each message only after `secs` seconds of silence — the message sent, "thinking" on
    /// screen when this returns. Nothing but the waiting animation draws meanwhile (N6).
    pub fn waiting_turn(&self, args: &[&str], cols: u16, rows: u16, secs: u32) -> Term {
        let fake = format!(
            "while read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n  sleep {secs}\n  \
             printf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\ndone\n"
        );
        let script = self.write("fake-claude.sh", &fake);
        self.write_config(&format!(
            "{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n",
            script.display()
        ));
        let mut t = Term::spawn(self, args, cols, rows);
        t.send(" l");
        t.send("wait for it\r");
        t.wait_text("thinking");
        t
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ── Pseudo-terminal harness ──────────────────────────────────────────────────

/// What the reader thread fills in: the parsed screen and every raw byte tarae wrote.
pub struct Output {
    parser: vt100::Parser,
    raw: Vec<u8>,
    /// Frames ended so far (`FRAME_END`), and when the first and the latest came — counted as bytes
    /// arrive, so timing a key to its frame doesn't rescan everything.
    frames: usize,
    first_frame_at: Option<Instant>,
    last_frame_at: Option<Instant>,
    /// Panics of the vt100 parser itself (it overflows on a 1-column screen). The parser is replaced with a
    /// blank one — tarae redraws whole frames, so the next frame fills it again.
    parser_panics: Vec<String>,
}

pub type Shared = Arc<Mutex<Output>>;

/// A panicking test thread must not wedge the others — ignore poisoning.
pub fn lock(out: &Shared) -> std::sync::MutexGuard<'_, Output> {
    out.lock().unwrap_or_else(|e| e.into_inner())
}

/// A screen snapshot — one string per row (wide characters appear once).
pub struct Screen {
    pub rows: Vec<String>,
}

impl Screen {
    pub fn contains(&self, s: &str) -> bool {
        self.rows.iter().any(|r| r.contains(s))
    }

    /// The status line (one above the command line at the bottom).
    pub fn status(&self) -> &str {
        &self.rows[self.rows.len().saturating_sub(2)]
    }
}

impl std::fmt::Display for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let width = self.rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        writeln!(f, "    ┌{}┐", "─".repeat(width))?;
        for (i, r) in self.rows.iter().enumerate() {
            let pad = " ".repeat(width - r.chars().count());
            writeln!(f, "{i:>3} │{r}{pad}│")?;
        }
        write!(f, "    └{}┘", "─".repeat(width))
    }
}

/// tarae running on a pty. The child is killed on drop.
pub struct Term {
    pub child: Child,
    master: File,
    /// The parent keeps a slave fd open for the child's whole life: when tarae exits, its last bytes (the
    /// terminal restore sequence) stay readable on the master instead of racing the slave's close.
    _slave: OwnedFd,
    out: Shared,
    timeout: Duration,
}

pub fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

impl Term {
    pub fn spawn(sb: &Sandbox, args: &[&str], cols: u16, rows: u16) -> Term {
        Term::spawn_in(sb, &sb.work(), args, cols, rows)
    }

    /// `spawn`, working in `cwd` rather than the sandbox's work folder (its HOME and state stay the sandbox's).
    pub fn spawn_in(sb: &Sandbox, cwd: &Path, args: &[&str], cols: u16, rows: u16) -> Term {
        let (mut master, mut slave) = (-1, -1);
        let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: out-pointers to live locals; no name buffer, default termios. The size is a raw pointer
        // because libc declares it `*mut` on macOS and `*const` on Linux.
        cvt(unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut ws)
        })
        .expect("openpty");
        // SAFETY: openpty just returned these fds and nothing else owns them.
        let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // Tests run in parallel threads: don't let another test's child inherit this pty.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: fcntl on an fd we own.
            cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }).unwrap();
        }
        let stdio = || Stdio::from(slave.try_clone().expect("dup slave"));
        // (`TARAE_BIN`: another build of tarae — a kept copy, to measure it against this one)
        let mut cmd = Command::new(std::env::var_os("TARAE_BIN").unwrap_or_else(|| BIN.into()));
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(sb.env())
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // `setsid` + the pty as its controlling terminal: /dev/tty works, SIGWINCH arrives.
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn tarae");

        let out = Arc::new(Mutex::new(Output {
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
            frames: 0,
            first_frame_at: None,
            last_frame_at: None,
            parser_panics: Vec::new(),
        }));
        let mut reader = master.try_clone().expect("dup master");
        let sink = Arc::clone(&out);
        // Drains the master continuously (tarae never blocks on a full pty buffer, tests never block on
        // reads) and must never die early: on macOS an exiting process whose tty output nobody reads hangs
        // in the kernel, and so would our `wait`. Ends when the slave side is fully closed (EOF, or EIO on
        // Linux) — i.e. after the drop.
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let at = Instant::now();
                        let mut o = lock(&sink);
                        let old = o.raw.len();
                        o.raw.extend_from_slice(&buf[..n]);
                        // (every window starting here holds a new byte — one split across reads counts once)
                        let from = old.saturating_sub(FRAME_END.len() - 1);
                        let ended =
                            o.raw[from..].windows(FRAME_END.len()).filter(|w| *w == FRAME_END).count();
                        if ended > 0 {
                            o.frames += ended;
                            o.first_frame_at.get_or_insert(at);
                            o.last_frame_at = Some(at);
                        }
                        let (rows, cols) = o.parser.screen().size();
                        let parser = &mut o.parser;
                        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            parser.process(&buf[..n]);
                        }));
                        if let Err(e) = res {
                            let msg = e
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                            o.parser_panics.push(msg.unwrap_or_else(|| "vt100 panicked".into()));
                            o.parser = vt100::Parser::new(rows, cols, 0);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        let mut term = Term { child, master, _slave: slave, out, timeout: sb.timeout() };
        // First frame drawn = raw mode is on — safe to type from here.
        term.wait_for("the first frame (status line with the mode)", |s| s.status().contains("NORMAL"));
        term
    }

    /// Types keys (UTF-8). Esc = "\x1b" (send it alone), Enter = "\r".
    pub fn send(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).expect("write to pty");
        self.master.flush().unwrap();
    }

    /// Esc on its own, then wait for its visible effect.
    pub fn esc(&mut self, what: &str, done: impl Fn(&Screen) -> bool) -> Screen {
        self.send("\x1b");
        self.wait_for(what, done)
    }

    pub fn screen(&self) -> Screen {
        let o = lock(&self.out);
        let s = o.parser.screen();
        let (_, cols) = s.size();
        Screen { rows: s.rows(0, cols).collect() }
    }

    pub fn raw(&self) -> Vec<u8> {
        lock(&self.out).raw.clone()
    }

    /// Polls the screen until `done` holds. On timeout, panics with the whole screen.
    pub fn wait_for(&mut self, what: &str, done: impl Fn(&Screen) -> bool) -> Screen {
        let deadline = Instant::now() + self.timeout;
        loop {
            let s = self.screen();
            if done(&s) {
                return s;
            }
            if Instant::now() > deadline {
                let state = match self.child.try_wait() {
                    Ok(Some(st)) => format!("tarae EXITED ({st})"),
                    _ => "tarae still running".to_string(),
                };
                let vt = &lock(&self.out).parser_panics;
                let vt = if vt.is_empty() { String::new() } else { format!(" — vt100 panicked: {vt:?}") };
                panic!("timed out after {:?} waiting for {what} — {state}{vt}\n{s}", self.timeout);
            }
            thread::sleep(POLL);
        }
    }

    /// Like `wait_for`, but gives up quietly after `within` — for retrying something the way a person would.
    pub fn poll_for(&mut self, within: Duration, done: impl Fn(&Screen) -> bool) -> Option<Screen> {
        let deadline = Instant::now() + within;
        loop {
            let s = self.screen();
            if done(&s) {
                return Some(s);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(POLL);
        }
    }

    pub fn wait_text(&mut self, text: &str) -> Screen {
        self.wait_for(&format!("{text:?} on screen"), |s| s.contains(text))
    }

    /// Resizes the pty (the kernel sends tarae SIGWINCH) and the parser with it.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        // Lock first: whatever tarae draws after the SIGWINCH is parsed at the new size.
        let mut o = lock(&self.out);
        // (vt100 can't hold a 0-sized screen — keep the old one; tarae still sees the real size)
        if rows > 0 && cols > 0 {
            o.parser.screen_mut().set_size(rows, cols);
        }
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ reads one winsize from a live local.
        cvt(unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) }).expect("TIOCSWINSZ");
    }

    /// What tarae has used so far: CPU time and context switches (every thread's).
    pub fn usage(&self) -> Usage {
        usage(self.child.id())
    }

    /// tarae's process id — for a look at it from outside (`heap`, `vmmap`).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Frames tarae has written so far (each ends its synchronized update).
    pub fn frames(&self) -> usize {
        lock(&self.out).frames
    }

    /// When the first frame ended (startup's end).
    pub fn first_frame_at(&self) -> Option<Instant> {
        lock(&self.out).first_frame_at
    }

    /// `keys` sent, then how long until the next frame has ended — polled finely, the frame's time taken
    /// as its bytes arrived. None if none came within `within`.
    pub fn key_to_frame(&mut self, keys: &str, within: Duration) -> Option<Duration> {
        let before = self.frames();
        let t0 = Instant::now();
        self.send(keys);
        let end = t0 + within;
        while Instant::now() < end {
            {
                let o = lock(&self.out);
                if o.frames > before {
                    return o.last_frame_at.map(|at| at.saturating_duration_since(t0));
                }
            }
            thread::sleep(Duration::from_micros(200));
        }
        None
    }

    pub fn running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }

    pub fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(st) = self.child.try_wait().expect("try_wait") {
                return st;
            }
            if Instant::now() > deadline {
                panic!("tarae didn't exit within {:?}\n{}", self.timeout, self.screen());
            }
            thread::sleep(POLL);
        }
    }

    /// Waits until the raw output (everything tarae ever wrote) satisfies `done`.
    pub fn wait_raw(&mut self, what: &str, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let raw = self.raw();
            if done(&raw) {
                return raw;
            }
            if Instant::now() > deadline {
                let tail = &raw[raw.len().saturating_sub(400)..];
                panic!(
                    "timed out waiting for {what} in the raw output; last bytes: {:?}",
                    String::from_utf8_lossy(tail)
                );
            }
            thread::sleep(POLL);
        }
    }

    /// `:cmd` Enter.
    pub fn command(&mut self, cmd: &str) {
        self.send(&format!(":{cmd}\r"));
    }

    /// Leave insert mode and wait until the status line says NORMAL again.
    pub fn normal_mode(&mut self) -> Screen {
        self.esc("NORMAL mode", |s| s.status().contains("NORMAL"))
    }

    /// Enter insert mode with `i`, type `text`, return to normal mode.
    pub fn insert(&mut self, text: &str) -> Screen {
        self.send("i");
        self.wait_for("INSERT mode", |s| s.status().contains("INSERT"));
        self.send(text);
        let last_line = text.lines().last().unwrap_or_default().to_string();
        self.wait_text(&last_line);
        self.normal_mode()
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        // Bounded: a failing test must fail, not hang the suite (a zombie at worst).
        let deadline = Instant::now() + self.timeout;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(POLL);
        }
    }
}

/// Where a frame ends (`CSI ?2026l`, the synchronized update's end).
pub const FRAME_END: &[u8] = b"\x1b[?2026l";

/// A process's CPU time and context switches so far.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub cpu: Duration,
    pub switches: u64,
    /// Resident memory now, in bytes (not a running total: a difference keeps the later one).
    pub rss: u64,
    /// Physical footprint now (macOS; RSS elsewhere) — what Activity Monitor shows: memory freed back to
    /// malloc doesn't count, as it still does in RSS until the pages are reused.
    pub footprint: u64,
}

impl std::ops::Sub for Usage {
    type Output = Usage;
    fn sub(self, before: Usage) -> Usage {
        Usage {
            cpu: self.cpu.saturating_sub(before.cpu),
            switches: self.switches.saturating_sub(before.switches),
            rss: self.rss,
            footprint: self.footprint,
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // (libc's mach_timebase_info — the one call the mach2 crate would be added for)
pub fn usage(pid: u32) -> Usage {
    // SAFETY: plain out-structs of the sizes given; the calls only fill them in.
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(pid as i32, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size);
        assert_eq!(n, size, "proc_pidinfo({pid})");
        // (CPU times are in Mach time units — not nanoseconds on Apple silicon)
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        libc::mach_timebase_info(&mut tb);
        let ticks = (info.pti_total_user + info.pti_total_system) as u128;
        let nanos = ticks * tb.numer as u128 / tb.denom.max(1) as u128;
        let mut ri: libc::rusage_info_v2 = std::mem::zeroed();
        let ok = libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V2, (&raw mut ri).cast()) == 0;
        Usage {
            cpu: Duration::from_nanos(nanos as u64),
            switches: info.pti_csw as u64,
            rss: info.pti_resident_size,
            footprint: if ok { ri.ri_phys_footprint } else { info.pti_resident_size },
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn usage(pid: u32) -> Usage {
    // utime + stime in clock ticks (fields 14 and 15, after the command's closing parenthesis)
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("/proc stat");
    let rest = &stat[stat.rfind(')').expect("comm") + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // SAFETY: sysconf only reads a constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let ticks: u64 = f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap();
    let mut switches = 0;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let kb = |key: &str| {
        status.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse::<u64>().ok())
    };
    let rss = kb("VmRSS:").unwrap_or(0) * 1024;
    for task in fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
        let status = fs::read_to_string(task.path().join("status")).unwrap_or_default();
        switches += status
            .lines()
            .filter(|l| l.contains("ctxt_switches"))
            .filter_map(|l| l.split_whitespace().last()?.parse::<u64>().ok())
            .sum::<u64>();
    }
    Usage { cpu: Duration::from_millis(ticks * 1000 / hz), switches, rss, footprint: rss }
}

/// Polls a file on disk until it holds `want` (saves are immediate today, but the test shouldn't care).
pub fn wait_file(path: &Path, want: &str) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let got = fs::read_to_string(path).ok();
        if got.as_deref() == Some(want) {
            return;
        }
        if Instant::now() > deadline {
            panic!("{} never held {want:?}; last read: {got:?}", path.display());
        }
        thread::sleep(POLL);
    }
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}
