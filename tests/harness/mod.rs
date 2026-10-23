//! The pty harness shared by tests/e2e.rs (offline, every `cargo test`) and tests/live.rs (real tools, on
//! demand). Each uses part of it.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[path = "../../src/pty.rs"]
mod pty;

pub const BIN: &str = env!("CARGO_BIN_EXE_tarae");
/// Generous — a loaded CI runner can be slow; a passing test never waits this long.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Live runs wait on real tools: rust-analyzer indexing, cargo building, Claude thinking.
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(240);
pub const POLL: Duration = Duration::from_millis(20);
/// How long a dropped `Term`'s tarae has to quit on SIGTERM before it's killed.
pub const QUIT_GRACE: Duration = Duration::from_secs(3);
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
    /// The soft descriptor limit tarae starts with (`with_soft_nofile`) — else the test's own.
    soft_nofile: Option<u64>,
    /// The tarae it runs (`with_bin`) — else `TARAE_BIN`, else this build's.
    bin: Option<PathBuf>,
    /// Sockets `socket` gave outside its folder (relative paths), removed with it.
    sockets: Mutex<Vec<PathBuf>>,
}

/// `sun_path`'s room, its closing NUL included: a socket's path must be shorter.
#[cfg(target_os = "macos")]
const SUN_PATH: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH: usize = 108;

/// Where `socket` puts one whose path in the sandbox's folder would pass `sun_path`: relative to the test's
/// working folder — cargo runs a test in its package's root, inside the project.
const SOCKETS: &str = "target/e2e-sockets";

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
        let sb = Sandbox {
            root,
            live: false,
            tools: Vec::new(),
            vars: Vec::new(),
            soft_nofile: None,
            bin: None,
            sockets: Mutex::new(Vec::new()),
        };
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

    /// tarae starts with this soft limit on open descriptors (set in its child, before the exec).
    pub fn with_soft_nofile(mut self, n: u64) -> Self {
        self.soft_nofile = Some(n);
        self
    }

    /// Another build of tarae runs here — one of two measured against each other in one test (`TARAE_BIN`
    /// can name only one).
    pub fn with_bin(mut self, bin: &Path) -> Self {
        self.bin = Some(bin.to_path_buf());
        self
    }

    /// The tarae this sandbox runs.
    fn bin(&self) -> OsString {
        let given = self.bin.clone().map(OsString::from);
        given.or_else(|| std::env::var_os("TARAE_BIN")).unwrap_or_else(|| BIN.into())
    }

    pub fn git_config(&self) -> PathBuf {
        self.root.join("gitconfig")
    }

    /// A file beside the project, not in it — what tarae and Claude mustn't find there (a measurement's
    /// own records).
    pub fn outside(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The project's files = `paths` of this repository as of `commit` (`git archive`) — a tree that stays
    /// the same however the repository grows. False when this clone hasn't that commit.
    pub fn copy_commit(&self, commit: &str, paths: &[&str]) -> bool {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        fs::create_dir_all(self.work()).unwrap();
        let mut archive = Command::new("git")
            .args(["-C", here.to_str().unwrap(), "archive", "--format=tar", commit, "--"])
            .args(paths)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let untarred = Command::new("tar")
            .args(["-x", "-C", self.work().to_str().unwrap()])
            .stdin(archive.stdout.take().unwrap())
            .status()
            .is_ok_and(|s| s.success());
        archive.wait().is_ok_and(|s| s.success()) && untarred
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

    /// A path for a unix socket of the test's own (its listener, a stand-in's): in the sandbox's folder when it
    /// fits `sun_path` — else a short one relative to the test's working folder (`SOCKETS`), removed with the
    /// sandbox. The folder is in `$TMPDIR`, and a sandboxed run's `TMPDIR` is its own folder deep in tarae's
    /// cache: there a socket's path passed 104 bytes (terminals A's measure) — and a sandboxed run binds one
    /// only in the project or that folder, so `/tmp` is no way out. A process given it (`TARAE_UI`) keeps the
    /// test's working folder.
    pub fn socket(&self, name: &str) -> PathBuf {
        let at = self.root.join(name);
        if at.as_os_str().len() < SUN_PATH {
            return at;
        }
        static N: AtomicUsize = AtomicUsize::new(0);
        fs::create_dir_all(SOCKETS).unwrap();
        let n = N.fetch_add(1, Ordering::Relaxed);
        let rel = Path::new(SOCKETS).join(format!("{}-{n}-{name}", std::process::id()));
        self.sockets.lock().unwrap().push(rel.clone());
        rel
    }

    /// tarae's state folder (`$XDG_STATE_HOME/tarae`).
    pub fn state(&self) -> PathBuf {
        self.home().join(".local/state/tarae")
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
        // (in a sandboxed run its pty broker makes the ptys — a tarae under test asks it for its terminal panel's
        // too: `pty::BROKER`, terminals A)
        let broker = std::env::var_os(pty::BROKER).map(|s| (OsString::from(pty::BROKER), s));
        base.into_iter().chain(broker).chain(self.vars.iter().cloned()).collect()
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
        for s in self.sockets.lock().unwrap().drain(..) {
            let _ = fs::remove_file(s);
        }
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
    /// A frame's start has been read and its end not yet (`FRAME_START` … `FRAME_END`): the parsed screen is
    /// half old, half new — not one to judge (#184: under load the chat's first line was read without the
    /// lines under it, all written in one frame).
    in_frame: bool,
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

    /// The status line — the screen's last row (a prompt is a card standing on it).
    pub fn status(&self) -> &str {
        &self.rows[self.rows.len().saturating_sub(1)]
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
    /// The environment it runs in — `tarae state` runs in it too (the same state folder).
    env: Vec<(OsString, OsString)>,
    /// The build it is — `tarae state` is asked of the same one.
    bin: OsString,
}

pub fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

/// A terminal that reads slowly (a remote one, a busy multiplexer): at most `chunk` bytes a read, then a
/// `pause` — what tarae writes faster than that fills the pty, and its writes wait for room.
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    pub chunk: usize,
    pub pause: Duration,
}

impl Term {
    pub fn spawn(sb: &Sandbox, args: &[&str], cols: u16, rows: u16) -> Term {
        Term::spawn_in(sb, &sb.work(), args, cols, rows)
    }

    /// `spawn` on a terminal that reads at `pace`.
    pub fn spawn_paced(sb: &Sandbox, args: &[&str], cols: u16, rows: u16, pace: Pace) -> Term {
        Term::spawn_with(sb, &sb.work(), args, cols, rows, Some(pace))
    }

    /// `spawn`, working in `cwd` rather than the sandbox's work folder (its HOME and state stay the sandbox's).
    pub fn spawn_in(sb: &Sandbox, cwd: &Path, args: &[&str], cols: u16, rows: u16) -> Term {
        Term::spawn_with(sb, cwd, args, cols, rows, None)
    }

    fn spawn_with(sb: &Sandbox, cwd: &Path, args: &[&str], cols: u16, rows: u16, pace: Option<Pace>) -> Term {
        // Tests run in parallel threads: another test's tarae mustn't get this pty — close-on-exec from the
        // start, or its master would outlive this test in that tarae (and in what it starts), and this
        // tarae would never hang up when the test binary dies.
        let (master, slave) = pty::open(rows, cols).expect("pty");
        let master = File::from(master);
        let stdio = || Stdio::from(slave.try_clone().expect("dup slave"));
        // (`TARAE_BIN` or `with_bin`: another build of tarae — a kept copy, to measure it against this one)
        let mut cmd = Command::new(sb.bin());
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(sb.env())
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio());
        let soft = sb.soft_nofile;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                // `setsid` + the pty as its controlling terminal: /dev/tty works, SIGWINCH arrives.
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                if let Some(n) = soft {
                    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                    cvt(libc::getrlimit(libc::RLIMIT_NOFILE, &mut l))?;
                    l.rlim_cur = n;
                    cvt(libc::setrlimit(libc::RLIMIT_NOFILE, &l))?;
                }
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
            in_frame: false,
        }));
        let mut reader = master.try_clone().expect("dup master");
        let sink = Arc::clone(&out);
        // Drains the master continuously (tarae never blocks on a full pty buffer, tests never block on
        // reads) and must never die early: on macOS an exiting process whose tty output nobody reads hangs
        // in the kernel, and so would our `wait`. Ends when the slave side is fully closed (EOF, or EIO on
        // Linux) — i.e. after the drop.
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let chunk = pace.map_or(buf.len(), |p| p.chunk.clamp(1, buf.len()));
            loop {
                if let Some(p) = pace {
                    thread::sleep(p.pause);
                }
                match reader.read(&mut buf[..chunk]) {
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
                        // (with the parser, under the one lock: never one without the other)
                        o.in_frame = in_frame_after(&o.raw[from..], o.in_frame);
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

        let mut term =
            Term { child, master, _slave: slave, out, timeout: sb.timeout(), env: sb.env(), bin: sb.bin() };
        // First frame drawn = raw mode is on — safe to type from here.
        term.wait_for("the first frame (status line with the mode)", |s| s.status().contains("● normal"));
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

    /// The foreground colour the screen shows `needle`'s first character in (None: not on screen).
    pub fn fg_of(&self, needle: &str) -> Option<vt100::Color> {
        use unicode_width::UnicodeWidthStr;
        let o = lock(&self.out);
        let s = o.parser.screen();
        let (_, cols) = s.size();
        let (r, row) = s.rows(0, cols).enumerate().find(|(_, row)| row.contains(needle))?;
        let col = row[..row.find(needle)?].width();
        Some(s.cell(r as u16, col as u16)?.fgcolor())
    }

    pub fn raw(&self) -> Vec<u8> {
        lock(&self.out).raw.clone()
    }

    /// The screen to judge: as of a frame's end, or outside frames — None while one is half read (#184: the
    /// first rows of a frame were judged before the rest were read). A tarae that exited is judged as it left
    /// the screen (one killed mid-frame never ends it).
    fn whole(&mut self) -> Option<Screen> {
        let running = matches!(self.child.try_wait(), Ok(None));
        let o = lock(&self.out);
        if o.in_frame && running {
            return None;
        }
        let s = o.parser.screen();
        let (_, cols) = s.size();
        Some(Screen { rows: s.rows(0, cols).collect() })
    }

    /// Polls the screen — each whole frame — until `done` holds. On timeout, panics with the whole screen.
    pub fn wait_for(&mut self, what: &str, done: impl Fn(&Screen) -> bool) -> Screen {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(s) = self.whole()
                && done(&s)
            {
                return s;
            }
            if Instant::now() > deadline {
                let s = self.screen();
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
            if let Some(s) = self.whole()
                && done(&s)
            {
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

    /// What each of tarae's threads has done so far (`threads`) — two of them apart say which woke (`woke`).
    pub fn threads(&self) -> Vec<Thread> {
        threads(self.child.id())
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

    /// When the latest frame ended — as its bytes were read.
    pub fn last_frame_at(&self) -> Option<Instant> {
        lock(&self.out).last_frame_at
    }

    /// Bytes tarae has written so far.
    pub fn written(&self) -> usize {
        lock(&self.out).raw.len()
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

    /// `tarae state --pid <its pid> args…` in its environment: what it prints (panics if it fails, saying how it
    /// ended — a code, or the signal that ended it, which says nothing of its own — and both its streams).
    pub fn state(&self, args: &[&str]) -> String {
        let out = Command::new(&self.bin)
            .arg("state")
            .args(["--pid", &self.pid().to_string()])
            .args(args)
            .env_clear()
            .envs(self.env.iter().cloned())
            .output()
            .expect("tarae state");
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(
            out.status.success(),
            "tarae state {args:?} ended with {} — stderr: {stderr:?}, stdout: {stdout:?}\n{}",
            out.status,
            self.screen()
        );
        stdout.into_owned()
    }

    /// A shuttle: `/bin/sh -c script sh args…` in its environment, started beside the test, its stdin and stdout
    /// piped — `$TARAE` is this tarae's binary and `$TARAE_PID` its pid, so its `tarae keys` calls run under one
    /// lasting process (`--for $$`). Never this test as the `for`: it started tarae, and what runs tarae never is
    /// one (`shuttle.rs`).
    pub fn shuttle_shell(&self, script: &str, args: &[&str]) -> std::process::Child {
        Command::new("/bin/sh")
            .args(["-c", script, "sh"])
            .args(args)
            .env_clear()
            .envs(self.env.iter().cloned())
            .env("TARAE", &self.bin)
            .env("TARAE_PID", self.pid().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the shuttle's shell")
    }

    /// The state as JSON — once the input so far (`input` keys and clicks) is handled and drawn.
    pub fn state_json(&self, after_input: u64) -> serde_json::Value {
        let s = self.state(&["--json", "--after-input", &after_input.to_string()]);
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("tarae state: {e}: {s}"))
    }

    /// A watcher of its UI state — `tarae state --watch [kinds] --json` running beside it, its event lines
    /// gathered as they come (`Watching::events`); stopped when dropped.
    pub fn watch(&self, kinds: &str) -> Watching {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("state").args(["--pid", &self.pid().to_string(), "--json", "--watch"]);
        if !kinds.is_empty() {
            cmd.arg(kinds);
        }
        let mut child = cmd
            .env_clear()
            .envs(self.env.iter().cloned())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("tarae state --watch");
        let out = child.stdout.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        thread::spawn(move || {
            use std::io::BufRead;
            for l in io::BufReader::new(out).lines().map_while(Result::ok) {
                sink.lock().unwrap_or_else(|e| e.into_inner()).push(l);
            }
        });
        let w = Watching { child, lines };
        // (its first line: it's registered — what changes from here on is told)
        let deadline = Instant::now() + self.timeout;
        while w.events().is_empty() {
            assert!(Instant::now() < deadline, "the watcher never started");
            thread::sleep(POLL);
        }
        w
    }

    /// Polls the UI state until `done` holds — where the keys go, the cards, the runs, the panels, read as
    /// data, not grepped off the screen. On timeout, panics with the last state and the screen.
    pub fn wait_state(&mut self, what: &str, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + self.timeout;
        loop {
            let s = self.state_json(0);
            if done(&s) {
                return s;
            }
            if Instant::now() > deadline {
                panic!(
                    "timed out after {:?} waiting for {what} — the state:\n{s:#}\n{}",
                    self.timeout,
                    self.screen()
                );
            }
            thread::sleep(POLL);
        }
    }

    /// Esc on its own — handled and drawn before anything else is sent (an Esc with a key right behind it
    /// reads as Alt + that key) — then `done` waited for in the state.
    pub fn esc_state(&mut self, what: &str, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let input = self.state_json(0)["input"].as_u64().unwrap_or(0);
        self.send("\x1b");
        self.state_json(input + 1);
        self.wait_state(what, done)
    }

    /// `:cmd` Enter.
    pub fn command(&mut self, cmd: &str) {
        self.send(&format!(":{cmd}\r"));
    }

    /// Leave insert mode and wait until the status line says NORMAL again.
    pub fn normal_mode(&mut self) -> Screen {
        self.esc("NORMAL mode", |s| s.status().contains("● normal"))
    }

    /// Enter insert mode with `i`, type `text`, return to normal mode.
    pub fn insert(&mut self, text: &str) -> Screen {
        self.send("i");
        self.wait_for("INSERT mode", |s| s.status().contains("● insert"));
        self.send(text);
        let last_line = text.lines().last().unwrap_or_default().to_string();
        self.wait_text(&last_line);
        self.normal_mode()
    }
}

impl Drop for Term {
    /// tarae is asked to quit (SIGTERM: as `:q!`, ending what it started — the terminal panel's shell, runs,
    /// servers), killed if it hasn't within `QUIT_GRACE`. A test binary that dies never gets here: its pty
    /// master closes (nothing else holds it — `pty::open`) and the hangup's SIGHUP quits tarae the same way.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // SAFETY: a signal to the tarae this harness started, still running (not yet waited for).
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
            let grace = Instant::now() + QUIT_GRACE;
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < grace {
                thread::sleep(POLL);
            }
        }
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

/// A `tarae state --watch` running: its lines so far.
pub struct Watching {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Watching {
    /// Every line it has printed (the first: `watching`), as JSON.
    pub fn events(&self) -> Vec<serde_json::Value> {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// Waits until an event satisfies `done`; panics with what came.
    pub fn wait(&self, what: &str, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(e) = self.events().into_iter().find(|e| done(e)) {
                return e;
            }
            assert!(Instant::now() < deadline, "no event for {what}: {:#?}", self.events());
            thread::sleep(POLL);
        }
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where a frame ends (`CSI ?2026l`, the synchronized update's end) — and where one starts.
pub const FRAME_END: &[u8] = b"\x1b[?2026l";
pub const FRAME_START: &[u8] = b"\x1b[?2026h";

/// Inside a frame or not after `bytes` (`was`: before them) — whichever of its marks came last; neither:
/// as it was. `bytes` start a mark's length less one before the new ones, so a mark split across reads is
/// seen.
pub fn in_frame_after(bytes: &[u8], was: bool) -> bool {
    let last = |mark: &[u8]| bytes.windows(mark.len()).rposition(|w| w == mark);
    match (last(FRAME_START), last(FRAME_END)) {
        (Some(s), Some(e)) => s > e,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => was,
    }
}

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

/// One thread of a process: its id, its name, and what it's done so far — context switches on Linux, CPU time
/// in µs on macOS (its thread info counts no switches).
#[derive(Clone, Debug)]
pub struct Thread {
    pub id: u64,
    pub name: String,
    pub done: u64,
}

/// What `Thread.done` counts here.
pub const THREAD_DONE: &str = if cfg!(target_os = "macos") { "µs of CPU" } else { "switches" };

/// The threads that did something between `before` and `after` (new ones too), busiest first, a line each —
/// for an idle test's failure: what woke.
pub fn woke(before: &[Thread], after: &[Thread]) -> String {
    let mut did: Vec<(u64, String)> = after
        .iter()
        .filter_map(|t| {
            let was = before.iter().find(|b| b.id == t.id).map_or(0, |b| b.done);
            let n = t.done.saturating_sub(was);
            (n > 0).then(|| (n, format!("{} ({})", t.name, t.id)))
        })
        .collect();
    did.sort_by_key(|d| std::cmp::Reverse(d.0));
    let lines: Vec<String> = did.iter().map(|(n, t)| format!("  {t}: {n} {THREAD_DONE}")).collect();
    match lines.is_empty() {
        true => "  no thread did anything".into(),
        false => lines.join("\n"),
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // (libc's mach_timebase_info)
pub fn threads(pid: u32) -> Vec<Thread> {
    // (sys/proc_info.h's — not in libc)
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    let mut ids = vec![0u64; 1024];
    // SAFETY: a buffer of the size given, which the call only fills in; then plain out-structs the same way.
    unsafe {
        let size = (ids.len() * 8) as libc::c_int;
        let n = libc::proc_pidinfo(pid as i32, PROC_PIDLISTTHREADS, 0, ids.as_mut_ptr().cast(), size);
        ids.truncate(n.max(0) as usize / 8);
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        libc::mach_timebase_info(&mut tb);
        ids.iter()
            .filter_map(|&id| {
                let mut info: libc::proc_threadinfo = std::mem::zeroed();
                let size = std::mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
                let got = libc::proc_pidinfo(
                    pid as i32,
                    libc::PROC_PIDTHREADINFO,
                    id,
                    (&raw mut info).cast(),
                    size,
                );
                (got == size).then(|| {
                    let name =
                        std::ffi::CStr::from_ptr(info.pth_name.as_ptr()).to_string_lossy().into_owned();
                    let ticks = (info.pth_user_time + info.pth_system_time) as u128;
                    let micros = ticks * tb.numer as u128 / tb.denom.max(1) as u128 / 1000;
                    Thread { id, name, done: micros as u64 }
                })
            })
            .collect()
    }
}

#[cfg(not(target_os = "macos"))]
pub fn threads(pid: u32) -> Vec<Thread> {
    let tasks = fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten();
    tasks
        .filter_map(|task| {
            let id = task.file_name().to_str()?.parse().ok()?;
            let name = fs::read_to_string(task.path().join("comm")).ok()?.trim().to_string();
            let status = fs::read_to_string(task.path().join("status")).ok()?;
            let done = status
                .lines()
                .filter(|l| l.contains("ctxt_switches"))
                .filter_map(|l| l.split_whitespace().last()?.parse::<u64>().ok())
                .sum();
            Some(Thread { id, name, done })
        })
        .collect()
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
