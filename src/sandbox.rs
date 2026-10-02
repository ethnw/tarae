//! Claude's commands in a sandbox. What Claude runs — `run` and `run_tests` (`tools.rs`) — goes through
//! macOS's Seatbelt (`/usr/bin/sandbox-exec`) with a profile made here (`Policy::profile`): it may write
//! in the project (but `.git`, `.tarae.toml` and `llm.sandbox-protect`), the temp folders and the
//! toolchain's own folders (cargo's target and build dirs, its registry; Go's caches), connect to nothing
//! over the network but the ports on this machine the user opened (`llm.sandbox-ports`, or all with
//! `llm.sandbox-localhost` — `RISKY_PORTS` says why not all by default), and make or reach unix sockets
//! only in the project and its own temp folder (`RunDir`, its `TMPDIR`). What the user runs themselves (`space m m`, `space x x`, installs,
//! `:restart`) isn't sandboxed.
//!
//! Measured on macOS 27 (2026-10-01, `target/sb/` scripts): the later of two rules on a path wins and
//! `require-not` works · a sandboxed process can't apply a different profile (exit 71, `sandbox_apply:
//! Operation not permitted` — so `probe` says so inside one) · `launchctl submit` is refused, Apple Events
//! and LaunchServices are blocked by the rules below · localhost connects (by name too), other addresses
//! fail at once, names don't resolve · tarae's own `cargo test` passes inside, its build dir being
//! `~/.cargo/build/shared` (`build.build-dir`) — which is why the toolchain's dirs come from `cargo
//! metadata`, not a guess. Paths are matched as real paths (`/tmp` = `/private/tmp`): all are canonical.
//!
//! Where there's no sandbox (not macOS, or tarae itself runs inside one) Claude's commands don't run
//! quietly without one: each asks, or the user agrees for the conversation, and the chat's title says so.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Set in a sandboxed command's environment (tarae's own tests read it: a sandbox can't start inside one).
pub const MARK: &str = "TARAE_SANDBOXED";

/// Whether this system can sandbox a command.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Availability {
    /// Not looked yet (`probe` runs on the first command's worker).
    #[default]
    Unknown,
    Ready,
    /// Why there's none.
    Missing(String),
    /// Tests only: Claude's commands run as before — the sandbox's own tests set the others.
    Bypass,
}

/// How a command of Claude's runs.
#[derive(Clone, Debug)]
pub enum Guard {
    In(Policy),
    /// No sandbox here (why) — the user agreed.
    Without(String),
    /// `llm.sandbox` is off.
    Off,
    /// The user ran it outside the sandbox, this once (`blocked.rs`).
    Outside,
}

impl Guard {
    /// The run's watch, when it ran in the sandbox.
    pub fn watch(&self) -> Option<&std::sync::Arc<Watch>> {
        match self {
            Guard::In(p) => p.watch.as_ref(),
            _ => None,
        }
    }

    /// What Claude reads with the command's result: what the sandbox blocked (or that it blocked nothing),
    /// then what the sandbox allows. `output` = the run's lines — the fallback when nothing could watch.
    pub fn report(&self, output: &[(bool, String)]) -> String {
        let rules = self.rules();
        let unwatched = " \"Operation not permitted\" is likely the sandbox.";
        let Some(w) = self.watch() else {
            return if matches!(self, Guard::In(_)) { format!("{rules}{unwatched}") } else { rules };
        };
        let b = w.blocked();
        if b.total > 0 {
            let mut s = format!(
                "The sandbox blocked {} — {} time{}:\n",
                if b.distinct == 1 { "one thing".to_string() } else { format!("{} things", b.distinct) },
                b.total,
                if b.total == 1 { "" } else { "s" }
            );
            for d in &b.seen {
                s.push_str(&format!("  {}\n", d.reported()));
            }
            if b.distinct > b.seen.len() {
                s.push_str(&format!("  … and {} more\n", b.distinct - b.seen.len()));
            }
            s.push_str(&format!(
                "{rules}\nIf the command needs one of these, tell the user — they can open it from the run's \
                 panel (space m s), or llm.sandbox-write, llm.sandbox-ports, llm.sandbox-localhost and \
                 llm.sandbox-network open more — or ask to run it outside the sandbox (outside_sandbox, with \
                 your reason); don't work around it."
            ));
            return s;
        }
        match b.failed {
            None => format!("The sandbox blocked nothing. {rules}"),
            Some(why) => {
                let mut s =
                    format!("tarae couldn't watch what the sandbox blocked ({why}). {rules}{unwatched}");
                let refused = refused_lines(output);
                if !refused.is_empty() {
                    s.push_str("\nLines that may be the sandbox:\n");
                    for l in refused {
                        s.push_str(&format!("  {l}\n"));
                    }
                }
                s
            }
        }
    }

    /// What the sandbox allows (or that there was none).
    fn rules(&self) -> String {
        match self {
            Guard::In(p) => {
                let ports = p.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
                let net = match (p.network, p.localhost, ports.is_empty()) {
                    (true, ..) => ", and reach the network — but not this machine's risky ports (Docker, Jupyter, \
                                   debuggers…)"
                        .to_string(),
                    (false, true, _) => ", and connect to nothing over the network but this machine".into(),
                    (false, false, true) => ", and connect to nothing over the network, this machine's ports \
                                             included (llm.sandbox-ports opens some, llm.sandbox-localhost all)"
                        .into(),
                    (false, false, false) => format!(
                        ", and connect to nothing over the network but this machine's ports {ports} \
                         (llm.sandbox-ports)"
                    ),
                };
                let granted = match p.granted.is_empty() {
                    true => String::new(),
                    false => format!(" The user also let it {}.", p.granted.join(", and ")),
                };
                format!(
                    "In tarae's sandbox: it could write only in the project (not .git or .tarae.toml), temp \
                     folders and the toolchain's own{net}.{granted}"
                )
            }
            Guard::Without(why) => format!("Ran without a sandbox ({why}) — the user agreed."),
            Guard::Off => "Ran without a sandbox — the user turned it off (llm.sandbox).".into(),
            Guard::Outside => "Ran outside the sandbox — the user chose to, this once.".into(),
        }
    }
}

/// What a sandboxed command may do. Every path is real (canonical).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The project: writable but for `protect`.
    pub root: PathBuf,
    /// Inside `root`, kept as they are (`.git`, `.tarae.toml`, `llm.sandbox-protect`).
    pub protect: Vec<PathBuf>,
    /// Folders writable besides the project (temp, the toolchain's, `llm.sandbox-write`).
    pub write: Vec<PathBuf>,
    /// Single files writable (cargo's lock and cache databases).
    pub files: Vec<PathBuf>,
    /// Folders inside writable ones kept as they are: the terminal multiplexers' sockets (`mux_dirs`) — a
    /// command could otherwise delete or replace the session the user works in.
    pub keep: Vec<PathBuf>,
    /// The network beyond this machine (`llm.sandbox-network`).
    pub network: bool,
    /// Ports on this machine it may connect to (`llm.sandbox-ports`): the network closed, the only ones;
    /// open, the risky ones listed here stay reachable.
    pub ports: Vec<u16>,
    /// Every port on this machine, the risky ones too (`llm.sandbox-localhost`).
    pub localhost: bool,
    /// This run's own temp folder — its `TMPDIR`, and with the project the only place it may make or reach
    /// a unix socket (`RunDir`).
    pub run: Option<std::sync::Arc<RunDir>>,
    /// This run's tag: every deny carries it (`(with message …)`), so the kernel's report of a denial says
    /// whose it is — by the time it arrives the process is mostly gone (measured), so its pid can't.
    pub tag: Option<String>,
    /// What the sandbox denied this run, watched while it goes (`Watch`).
    pub watch: Option<std::sync::Arc<Watch>>,
    /// What the user let Claude's commands do besides (`allow.rs`) — as the result says it.
    pub granted: Vec<String>,
}

/// A run's own temp folder. Unix sockets anywhere else stay out of reach: in `/private/tmp` and `$TMPDIR`
/// live tmux's, zellij's and ssh-agent's — servers outside the sandbox that run what they're told
/// (reproduced: `tmux -S /private/tmp/tmux-501/default run-shell "touch ~/marker"` from inside made the
/// marker when temp sockets were allowed). Removed, on a thread of its own, once the run is gone.
#[derive(Debug, PartialEq, Eq)]
pub struct RunDir(PathBuf);

impl RunDir {
    /// A new folder under `$TMPDIR` (real path; short — a socket's path has 104 bytes at most), only ours.
    pub fn new() -> std::io::Result<std::sync::Arc<RunDir>> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = real(&std::env::temp_dir());
        let dir = base.join(format!("tarae-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(std::sync::Arc::new(RunDir(dir)))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The run's tag: its folder's name, closed with a dot (so `…-1.` isn't found in `…-10.`).
    pub fn tag(&self) -> String {
        format!("{}.", self.0.file_name().unwrap_or_default().to_string_lossy())
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let dir = std::mem::take(&mut self.0);
        std::thread::spawn(move || std::fs::remove_dir_all(dir));
    }
}

/// One denial the kernel reported: the process, what it tried (`file-write-create`, `network-outbound`…)
/// and on what (a path, an address).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    pub process: String,
    pub op: String,
    pub target: String,
}

impl Denial {
    /// The target, home spelled `~`.
    fn target_shown(&self) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        match self.target.strip_prefix(&home) {
            Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
                format!("~{rest}")
            }
            _ => self.target.clone(),
        }
    }

    /// As Seatbelt says it: `file-write-create ~/x`.
    pub fn shown(&self) -> String {
        format!("{} {}", self.op, self.target_shown()).trim_end().to_string()
    }

    /// In plain words: `writing ~/x`, `connecting to port 9`, `opening apps` — Seatbelt's own (`shown`) for
    /// what has none. Its network denials name no address, only the port (`remote:*:9` — measured).
    pub fn plain(&self) -> String {
        let t = self.target_shown();
        let op = self.op.as_str();
        match op {
            "file-write-create" | "file-write-data" => format!("writing {t}"),
            "file-write-unlink" => format!("deleting {t}"),
            "file-write-rename" => format!("moving {t}"),
            _ if op.starts_with("file-write") => format!("changing {t}"),
            "network-outbound" if t.starts_with('/') => format!("connecting to socket {t}"),
            "network-outbound" if t.starts_with("remote:") => match t.rsplit_once(':') {
                Some((_, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                    format!("connecting to port {port}")
                }
                _ => self.shown(),
            },
            "network-bind" if t.starts_with('/') => format!("making socket {t}"),
            "appleevent-send" if !t.is_empty() => format!("Apple Event to {t}"),
            "mach-lookup"
                if t.starts_with("com.apple.lsd") || t == "com.apple.coreservices.launchservicesd" =>
            {
                "opening apps".into()
            }
            _ => self.shown(),
        }
    }

    /// For Claude: the plain words, then Seatbelt's (`writing ~/x (file-write-create, touch)`).
    pub fn reported(&self) -> String {
        match self.plain() {
            p if p == self.shown() => format!("{p} ({})", self.process),
            p => format!("{p} ({}, {})", self.op, self.process),
        }
    }
}

/// What the sandbox denied a run: the distinct ones (at most `DENIALS_KEPT`), how many in all, and why
/// watching didn't work if it didn't.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blocked {
    pub seen: Vec<Denial>,
    pub distinct: usize,
    pub total: usize,
    pub failed: Option<String>,
}

pub const DENIALS_KEPT: usize = 10;

impl Blocked {
    pub fn add(&mut self, d: Denial) {
        self.total += 1;
        let same = |s: &Denial| s.op == d.op && s.target == d.target;
        if self.seen.iter().any(same) {
            return;
        }
        self.distinct += 1;
        if self.seen.len() < DENIALS_KEPT {
            self.seen.push(d);
        }
    }
}

/// A denial from one line of `log stream --style ndjson`, if it carries `tag` — read from the JSON's
/// `eventMessage`: `Sandbox: touch(80432) deny(1) file-write-create /path` with the tag after it (on a line
/// of its own, measured — or after a space). A line of repeats (`3 duplicate reports for Sandbox: …`) counts
/// once.
pub fn parse_denial(json: &str, tag: &str) -> Option<Denial> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let msg = v["eventMessage"].as_str()?;
    let (first, rest) = msg.split_once('\n').unwrap_or((msg, ""));
    let first = first.trim_end();
    let first = match first.strip_suffix(tag) {
        Some(head) if head.ends_with(' ') => head.trim_end(),
        _ if rest.lines().any(|l| l.trim() == tag) => first,
        _ => return None,
    };
    let after = &first[first.find("Sandbox: ")? + "Sandbox: ".len()..];
    let (process, after) = after.split_once('(')?;
    let (_, after) = after.split_once(") deny(")?;
    let (_, after) = after.split_once(") ")?;
    let (op, target) = after.split_once(' ').unwrap_or((after, ""));
    Some(Denial { process: process.into(), op: op.into(), target: target.trim().into() })
}

/// How long `log stream` takes to be up before a run starts (its first denials arrived from 50 ms on, and
/// 10 ms after they happened — measured), and how long the end of a watched run waits for the last ones.
pub const WATCH_READY: std::time::Duration = std::time::Duration::from_millis(150);
pub const WATCH_LAST: std::time::Duration = std::time::Duration::from_millis(150);

/// The kernel's denials for one run (`log stream`, only the lines with its tag), gathered on a thread of its
/// own; stopped when the run is gone or done (`finish`).
pub struct Watch {
    tag: String,
    child: Mutex<Option<std::process::Child>>,
    blocked: std::sync::Arc<Mutex<Blocked>>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Watch({})", self.tag)
    }
}

impl PartialEq for Watch {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag
    }
}

impl Eq for Watch {}

impl Watch {
    /// A watch with nothing behind it — the denials come from `record` (tests), or never.
    pub fn quiet(tag: &str) -> std::sync::Arc<Watch> {
        std::sync::Arc::new(Watch {
            tag: tag.to_string(),
            child: Mutex::new(None),
            blocked: Default::default(),
            stopped: Default::default(),
        })
    }

    /// Starts `log stream` for `tag` (on a worker: it returns at once, but give it `WATCH_READY` before the
    /// run starts). When it can't run or ends early, `blocked().failed` says why.
    pub fn start(tag: &str) -> std::sync::Arc<Watch> {
        let w = Watch::quiet(tag);
        let predicate = format!("sender == \"Sandbox\" AND eventMessage CONTAINS \"{tag}\"");
        let spawned = Command::new("/usr/bin/log")
            .args(["stream", "--style", "ndjson", "--predicate", &predicate])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                w.fail(format!("/usr/bin/log: {e}"));
                return w;
            }
        };
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        *w.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let (blocked, stopped, tag) = (w.blocked.clone(), w.stopped.clone(), tag.to_string());
        std::thread::spawn(move || {
            use std::io::{BufRead, Read};
            for line in std::io::BufReader::new(stdout.expect("piped")).lines().map_while(Result::ok) {
                if let Some(d) = parse_denial(&line, &tag) {
                    blocked.lock().unwrap_or_else(|e| e.into_inner()).add(d);
                }
            }
            // Ended without being stopped: why, from what it said
            if !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                let mut said = String::new();
                if let Some(mut e) = stderr {
                    let _ = e.read_to_string(&mut said);
                }
                let why = said.lines().find(|l| !l.trim().is_empty()).unwrap_or("log stream ended").trim();
                blocked.lock().unwrap_or_else(|e| e.into_inner()).failed = Some(why.to_string());
            }
        });
        w
    }

    /// A denial, as if reported (tests).
    #[cfg(test)]
    pub fn record(&self, d: Denial) {
        self.blocked.lock().unwrap_or_else(|e| e.into_inner()).add(d);
    }

    fn fail(&self, why: String) {
        self.blocked.lock().unwrap_or_else(|e| e.into_inner()).failed = Some(why);
    }

    pub fn blocked(&self) -> Blocked {
        self.blocked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The run is done: no more watching (its last denials have had `WATCH_LAST`).
    pub fn finish(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            std::thread::spawn(move || c.wait());
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A sandboxed run that has printed nothing this long since something was denied may be waiting on it —
/// its panel says so (the worst case had no error at all: a pty test without `/dev/ptmx` just waited).
pub const QUIET: std::time::Duration = std::time::Duration::from_secs(15);

/// A run's sandbox line for its panel: (what was blocked, in plain words — `sandbox blocked writing ~/x  +2`,
/// the quiet hint when it applies). None when nothing was.
pub fn panel_line(
    guard: Option<&Guard>,
    running: bool,
    last_output: std::time::Instant,
) -> Option<(String, Option<String>)> {
    let b = guard?.watch()?.blocked();
    let first = b.seen.first()?;
    let more = if b.distinct > 1 { format!("  +{}", b.distinct - 1) } else { String::new() };
    let quiet = (running && last_output.elapsed() >= QUIET).then(|| {
        format!("quiet {}s — it may be waiting on what was blocked", last_output.elapsed().as_secs())
    });
    Some((format!("sandbox blocked {}{more}", first.plain()), quiet))
}

/// Lines of output that say a permission was refused — what's left to go by when nothing watched.
pub fn refused_lines(output: &[(bool, String)]) -> Vec<String> {
    output
        .iter()
        .filter(|(_, l)| l.contains("Operation not permitted"))
        .take(5)
        .map(|(_, l)| l.clone())
        .collect()
}

/// Device files every program writes to (`/dev/null`, terminals — a pty test hangs without them: measured).
const DEVICES: &str = "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
(literal \"/dev/ptmx\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/ttys[0-9]+$\") (subpath \"/dev/fd\"))\n";

/// Ports on this machine where what listens runs what it's asked to, outside the sandbox. The network
/// closed (the default), every port on this machine is closed but `llm.sandbox-ports` — these included;
/// open (`llm.sandbox-network`), these stay closed unless listed. `llm.sandbox-localhost` opens them too:
/// once `localhost:*` is allowed no port's deny wins over it, before or after, by any filter (measured by
/// the user, 2026-10-02) — so "localhost but these" can't be written. `docs/claude-integration.md` lists
/// them (a test checks).
pub const RISKY_PORTS: [(u16, &str); 10] = [
    (2375, "Docker API — starts containers that mount any folder"),
    (2376, "Docker API over TLS — the same"),
    (4243, "Docker API on its old port — the same"),
    (8888, "Jupyter — runs code in its kernels"),
    (9222, "Chrome DevTools — drives the browser, signed-in sessions included"),
    (9229, "Node inspector — runs code in the inspected program"),
    (6379, "Redis — its CONFIG and MODULE commands write files and load code"),
    (2379, "etcd — a cluster's state, its secrets included"),
    (6443, "Kubernetes API server — starts pods"),
    (10250, "kubelet — runs commands in the node's pods"),
];

/// `llm.sandbox-ports` at most: one rule a port, and they cost every command (measured: 1000 rules 20 ms,
/// 5000 0.4 s, 20000 don't compile).
pub const PORTS_MAX: usize = 200;

/// Ways to get something run outside: Apple Events to other apps, LaunchServices (`open`).
const NO_WAY_OUT: [&str; 2] = [
    "appleevent-send",
    "mach-lookup (global-name \"com.apple.coreservices.launchservicesd\") (global-name-regex #\"^com\\.apple\\.lsd\\.\")",
];

impl Policy {
    /// The Seatbelt profile. Paths aren't in it: they're `-D` parameters (`params`), so no path can change
    /// what it says. Every deny carries the run's tag (`tag`) — the only text from outside, and tarae's own.
    pub fn profile(&self) -> String {
        let with = self.tag.as_ref().map(|t| format!(" (with message \"{t}\")")).unwrap_or_default();
        let deny = |rule: &str| format!("(deny {rule}{with})\n");
        let mut p = String::from("(version 1)\n(allow default)\n");
        p.push_str(&deny("file-write*"));
        let protect: String =
            (0..self.protect.len()).map(|i| format!(" (require-not (subpath (param \"P{i}\")))")).collect();
        p.push_str(&format!("(allow file-write* (require-all (subpath (param \"ROOT\")){protect}))\n"));
        let run = if self.run.is_some() { " (subpath (param \"RUN\"))" } else { "" };
        let dirs: String = (0..self.write.len()).map(|i| format!(" (subpath (param \"W{i}\"))")).collect();
        let files: String = (0..self.files.len()).map(|i| format!(" (literal (param \"F{i}\"))")).collect();
        if !run.is_empty() || !dirs.is_empty() || !files.is_empty() {
            p.push_str(&format!("(allow file-write*{run}{dirs}{files})\n"));
        }
        // After the allows, so these win: the multiplexers' socket folders sit inside writable temp ones
        let keep: String = (0..self.keep.len()).map(|i| format!(" (subpath (param \"K{i}\"))")).collect();
        if !keep.is_empty() {
            p.push_str(&deny(&format!("file-write*{keep}")));
        }
        p.push_str(DEVICES);
        let open: String = self.ports.iter().map(|n| format!(" (remote ip \"localhost:{n}\")")).collect();
        if !self.network {
            // Listening stays open (a test's own server); connecting, only where it's allowed
            p.push_str(&deny("network*"));
            p.push_str("(allow network-bind network-inbound (local ip \"localhost:*\"))\n");
            if self.localhost {
                p.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
            } else if !open.is_empty() {
                p.push_str(&format!("(allow network-outbound{open})\n"));
            }
        } else {
            // Under the open network a port's deny does win (measured)
            let risky: String = RISKY_PORTS
                .iter()
                .filter(|(n, _)| !self.ports.contains(n))
                .map(|(n, _)| format!(" (remote ip \"localhost:{n}\")"))
                .collect();
            if !risky.is_empty() {
                p.push_str(&deny(&format!("network-outbound{risky}")));
            }
        }
        // Unix sockets — made or reached — only in the project and the run's own folder (`RunDir`), the
        // network open or not: the ones elsewhere are servers outside (tmux, zellij, ssh-agent). Last, so
        // these win (the later rule does — measured)
        let here = format!("(require-any (subpath (param \"ROOT\")){run})");
        p.push_str(&deny("network-bind network-inbound (local unix-socket)"));
        p.push_str(&deny("network-outbound (remote unix-socket)"));
        p.push_str(&format!(
            "(allow network-bind network-inbound (require-all (local unix-socket) {here}))\n\
             (allow network-outbound (require-all (remote unix-socket) {here}))\n"
        ));
        for rule in NO_WAY_OUT {
            p.push_str(&deny(rule));
        }
        p
    }

    /// The profile's parameters (name, value).
    pub fn params(&self) -> Vec<(String, String)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut v = vec![("ROOT".to_string(), s(&self.root))];
        v.extend(self.run.iter().map(|r| ("RUN".to_string(), s(r.path()))));
        v.extend(self.protect.iter().enumerate().map(|(i, p)| (format!("P{i}"), s(p))));
        v.extend(self.write.iter().enumerate().map(|(i, p)| (format!("W{i}"), s(p))));
        v.extend(self.files.iter().enumerate().map(|(i, p)| (format!("F{i}"), s(p))));
        v.extend(self.keep.iter().enumerate().map(|(i, p)| (format!("K{i}"), s(p))));
        v
    }

    /// `sandbox-exec`'s arguments before the command.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["-p".to_string(), self.profile()];
        for (k, v) in self.params() {
            a.push("-D".into());
            a.push(format!("{k}={v}"));
        }
        a
    }

    /// The policy for a project, from what's around it (`Env`, gathered on a worker — or made up in tests).
    pub fn for_project(env: &Env) -> Policy {
        let root = env.root.clone();
        let mut protect = vec![root.join(".git"), root.join(".tarae.toml")];
        for p in &env.protect {
            let rel = Path::new(p);
            // Inside the project only (`..` would reach out of it)
            if rel.is_relative() && rel.components().all(|c| matches!(c, Component::Normal(_))) {
                protect.push(root.join(rel));
            }
        }
        let mut write = vec![PathBuf::from("/private/tmp")];
        if let Some(t) = &env.tmp {
            write.push(user_temp(t));
        }
        if let Some(ch) = &env.cargo_home {
            write.extend([ch.join("registry"), ch.join("git")]);
        }
        write.extend(env.dirs.iter().cloned());
        for w in &env.write {
            let p = match (w.strip_prefix("~/"), &env.home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => root.join(w),
            };
            write.push(real(&p));
        }
        // Never the project or what holds it (that would open .git again); inside it, the project's rule
        write.retain(|w| !root.starts_with(w) && !w.starts_with(&root));
        write.dedup();
        let files = match &env.cargo_home {
            Some(ch) => [".package-cache", ".package-cache-mutate", ".global-cache"]
                .iter()
                .flat_map(|f| {
                    let db = *f == ".global-cache";
                    let extra: &[&str] = if db { &["", "-journal", "-wal", "-shm"] } else { &[""] };
                    extra.iter().map(move |e| ch.join(format!("{f}{e}"))).collect::<Vec<_>>()
                })
                .collect(),
            None => Vec::new(),
        };
        let mut ports: Vec<u16> = env.ports.clone();
        ports.sort_unstable();
        ports.dedup();
        ports.truncate(PORTS_MAX);
        Policy {
            root,
            protect,
            write,
            files,
            keep: mux_dirs(env),
            network: env.network,
            ports,
            localhost: env.localhost,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
        }
    }
}

/// Where tmux and zellij keep their sockets: tmux in `$TMUX_TMPDIR` (else `/tmp`) `/tmux-<uid>`, zellij in
/// `$ZELLIJ_SOCKET_DIR` (else `$TMPDIR/zellij-<uid>`). Real paths.
fn mux_dirs(env: &Env) -> Vec<PathBuf> {
    let tmux_base = env.tmux_tmpdir.as_deref().unwrap_or(Path::new("/private/tmp"));
    let mut v = vec![real(tmux_base).join(format!("tmux-{}", env.uid))];
    match (&env.zellij_dir, &env.tmp) {
        (Some(d), _) => v.push(real(d)),
        (None, Some(t)) => v.push(t.join(format!("zellij-{}", env.uid))),
        (None, None) => {}
    }
    v
}

/// What a project's policy is made from.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub root: PathBuf,
    pub home: Option<PathBuf>,
    /// `$TMPDIR`, real.
    pub tmp: Option<PathBuf>,
    /// `$CARGO_HOME` (or `~/.cargo`), real — when it exists.
    pub cargo_home: Option<PathBuf>,
    /// The toolchain's own folders (`toolchain_dirs`).
    pub dirs: Vec<PathBuf>,
    /// `llm.sandbox-write` · `-protect` · `-network` · `-ports` · `-localhost`.
    pub write: Vec<String>,
    pub protect: Vec<String>,
    pub network: bool,
    pub ports: Vec<u16>,
    pub localhost: bool,
    /// `$TMUX_TMPDIR` · `$ZELLIJ_SOCKET_DIR` · the user's id (their socket folders — `mux_dirs`).
    pub tmux_tmpdir: Option<PathBuf>,
    pub zellij_dir: Option<PathBuf>,
    pub uid: u32,
}

/// The real path; for one that isn't there yet, its folder's real path joined with its name.
pub fn real(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) if dir != p => real(dir).join(name),
        _ => p.to_path_buf(),
    }
}

/// The user's own temp space: `$TMPDIR` is `/private/var/folders/xx/yyy/T` — its parent holds `C` (caches
/// some tools use) too.
fn user_temp(tmp: &Path) -> PathBuf {
    match tmp.parent() {
        Some(parent) if tmp.starts_with("/private/var/folders") && tmp.file_name() == Some("T".as_ref()) => {
            parent.to_path_buf()
        }
        _ => tmp.to_path_buf(),
    }
}

// ── On a worker ─────────────────────────────────────────────────────────────

/// Whether commands can be sandboxed here — looked once per run of tarae (blocks: a worker's).
pub fn probe() -> Availability {
    static FOUND: OnceLock<Availability> = OnceLock::new();
    FOUND.get_or_init(probe_now).clone()
}

fn probe_now() -> Availability {
    if !cfg!(target_os = "macos") {
        return Availability::Missing("this system has no sandbox tarae can use yet".into());
    }
    let out = Command::new(SANDBOX_EXEC)
        .args([
            "-p",
            "(version 1)(allow default)(deny file-write* (literal \"/tarae-probe\"))",
            "/usr/bin/true",
        ])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Availability::Ready,
        Ok(o) => Availability::Missing(missing_why(&String::from_utf8_lossy(&o.stderr))),
        Err(e) => Availability::Missing(format!("{SANDBOX_EXEC}: {e}")),
    }
}

/// Why `sandbox-exec` couldn't start one, from what it said.
pub fn missing_why(stderr: &str) -> String {
    if stderr.contains("sandbox_apply") {
        "tarae itself runs in a sandbox, and another can't start inside it".into()
    } else {
        format!("sandbox-exec: {}", stderr.trim())
    }
}

/// The policy for the project at `root` with these settings — the toolchain's folders found (blocks).
pub fn policy(root: &Path, llm: &crate::llm::LlmConfig) -> Policy {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")))
        .and_then(|p| std::fs::canonicalize(p).ok());
    let tmp = std::fs::canonicalize(std::env::temp_dir()).ok();
    let mut env = Env {
        root: real(root),
        home,
        tmp,
        cargo_home,
        dirs: Vec::new(),
        write: llm.sandbox_write.clone(),
        protect: llm.sandbox_protect.clone(),
        network: llm.sandbox_network,
        ports: llm.sandbox_ports.clone(),
        localhost: llm.sandbox_localhost,
        tmux_tmpdir: std::env::var_os("TMUX_TMPDIR").filter(|v| !v.is_empty()).map(PathBuf::from),
        zellij_dir: std::env::var_os("ZELLIJ_SOCKET_DIR").filter(|v| !v.is_empty()).map(PathBuf::from),
        // SAFETY: getuid has no preconditions and can't fail.
        uid: unsafe { libc::getuid() },
    };
    // The toolchain is asked inside the sandbox it's for (it reads only)
    env.dirs = toolchain_dirs(&Policy::for_project(&env));
    // (no folder of its own: no sockets but the project's, `TMPDIR` as it was, and no tag to watch by)
    let run = RunDir::new().ok();
    let tag = run.as_ref().map(|r| r.tag());
    Policy { run, tag, ..Policy::for_project(&env) }
}

/// Folders the project's toolchain writes outside it: cargo's target and build dirs (`cargo metadata` —
/// they can be anywhere: `CARGO_TARGET_DIR`, `build.build-dir` in a cargo config), Go's caches. Kept per
/// project until a file that sets them changes.
fn toolchain_dirs(base: &Policy) -> Vec<PathBuf> {
    type Seen = HashMap<PathBuf, (Vec<Option<SystemTime>>, Vec<PathBuf>)>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let root = &base.root;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let stamp_of = [
        root.join("Cargo.toml"),
        root.join(".cargo/config.toml"),
        home.join(".cargo/config.toml"),
        root.join("go.mod"),
    ];
    let stamp: Vec<Option<SystemTime>> =
        stamp_of.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect();
    let seen = SEEN.get_or_init(Default::default);
    if let Some((s, dirs)) = seen.lock().ok().and_then(|m| m.get(root).cloned())
        && s == stamp
    {
        return dirs;
    }
    let mut dirs = Vec::new();
    if root.join("Cargo.toml").is_file()
        && let Some(json) =
            output_in(base, "cargo", &["metadata", "--no-deps", "--offline", "--format-version", "1"])
    {
        for key in ["target_directory", "build_directory"] {
            if let Some(d) = json[key].as_str() {
                dirs.push(real(Path::new(d)));
            }
        }
    }
    if root.join("go.mod").is_file()
        && let Some(json) = output_in(base, "go", &["env", "-json", "GOCACHE", "GOMODCACHE"])
    {
        for key in ["GOCACHE", "GOMODCACHE"] {
            if let Some(d) = json[key].as_str().filter(|d| !d.is_empty()) {
                dirs.push(real(Path::new(d)));
            }
        }
    }
    if let Ok(mut m) = seen.lock() {
        m.insert(root.clone(), (stamp, dirs.clone()));
    }
    dirs
}

/// A program's JSON output, run in the project inside `policy`.
fn output_in(policy: &Policy, program: &str, args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new(SANDBOX_EXEC)
        .args(policy.args())
        .arg(program)
        .args(args)
        .current_dir(&policy.root)
        .env(MARK, "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    serde_json::from_slice(&out.stdout).ok()
}

/// What a sandboxed command's environment says: that it's sandboxed (`MARK`), and `GIT_OPTIONAL_LOCKS=0` —
/// git's read-only commands (status, diff) otherwise refresh the index on the side, a write into `.git` the
/// sandbox denies: noise in every report (measured: 6 denials in one `cargo test`).
fn inside_env(c: &mut Command) -> &mut Command {
    c.env(MARK, "1").env("GIT_OPTIONAL_LOCKS", "0")
}

/// `program args` inside `policy` — or as it is, without one.
pub fn command(policy: Option<&Policy>, program: &str, args: &[String]) -> Command {
    match policy {
        Some(p) => {
            let mut c = Command::new(SANDBOX_EXEC);
            c.args(p.args()).arg(program).args(args);
            inside_env(&mut c);
            if let Some(run) = &p.run {
                c.env("TMPDIR", run.path());
            }
            c
        }
        None => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
    }
}

/// Tests only: true inside a sandbox already (Claude's `cargo test`), where Seatbelt can't start — and then
/// says so past libtest's capture (`test_results::SELF_SKIP`), so the run reads it as a skip, not a pass.
#[cfg(test)]
pub(crate) fn skipped_inside(test: &str) -> bool {
    if std::env::var_os(MARK).is_none() {
        return false;
    }
    let line = format!(
        "{}{test} — Seatbelt can't start inside a sandbox; run `cargo test` outside one to check it\n",
        crate::test_results::SELF_SKIP
    );
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git's read-only commands don't write in `.git` in a sandboxed run's environment: `git status` over a
    /// file whose stat changed would refresh the index (a write the sandbox denies, reported as noise) —
    /// with `GIT_OPTIONAL_LOCKS=0` it leaves it. (A repository of the test's own, in its temp folder.)
    #[test]
    fn git_status_leaves_the_index_in_a_sandboxed_run() {
        let dir = std::env::temp_dir().join(format!("tarae-sb-gitlock-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git").args(args).current_dir(&dir).output().unwrap().status.success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["init", "-q"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "a.txt"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "x",
        ]);
        // The same text, an hour older: its stat no longer matches the index's (git counts whole seconds)
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(dir.join("a.txt")).unwrap().set_modified(hour_ago).unwrap();
        let before = std::fs::read(dir.join(".git/index")).unwrap();
        let mut status = Command::new("git");
        status.args(["status", "--porcelain"]).current_dir(&dir);
        let out = inside_env(&mut status).output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before, "the index was rewritten");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn env(root: &Path) -> Env {
        Env {
            root: root.to_path_buf(),
            home: Some("/Users/u".into()),
            tmp: Some("/private/var/folders/ab/cd/T".into()),
            cargo_home: Some("/Users/u/.cargo".into()),
            dirs: vec!["/Users/u/.cargo/build/shared".into(), root.join("target")],
            write: vec!["~/Library/Caches/thing".into(), "/".into(), "/Users".into()],
            protect: vec!["secrets".into(), "../outside".into(), "/etc".into()],
            network: false,
            ports: Vec::new(),
            localhost: false,
            tmux_tmpdir: None,
            zellij_dir: None,
            uid: 501,
        }
    }

    /// The multiplexers' socket folders, where they put them by default and where the environment says.
    #[test]
    fn the_multiplexers_socket_folders() {
        let root = Path::new("/Users/u/p");
        let keep = Policy::for_project(&env(root)).keep;
        assert_eq!(
            keep,
            [
                real(Path::new("/private/tmp")).join("tmux-501"),
                "/private/var/folders/ab/cd/T/zellij-501".into()
            ]
        );
        let moved = Env {
            tmux_tmpdir: Some("/Users/u/tm".into()),
            zellij_dir: Some("/Users/u/zj".into()),
            ..env(root)
        };
        assert_eq!(
            Policy::for_project(&moved).keep,
            [PathBuf::from("/Users/u/tm/tmux-501"), "/Users/u/zj".into()]
        );
    }

    /// The docs say which local ports are closed and why — every one of them.
    #[test]
    fn the_docs_name_every_risky_port() {
        let doc = include_str!("../docs/claude-integration.md");
        let start = doc.find("### The sandbox").expect("the sandbox section");
        let section = &doc[start..start + doc[start + 1..].find("\n### ").unwrap_or(doc.len() - start - 1)];
        for (port, why) in RISKY_PORTS {
            assert!(
                section.contains(&port.to_string()),
                "docs/claude-integration.md doesn't name port {port} ({why})"
            );
        }
    }

    /// The project but its own guarded files, temp, cargo's caches and dirs — not cargo's `bin` (on PATH:
    /// what lands there runs later outside), not the project's ancestors (they'd open `.git` again).
    #[test]
    fn a_projects_policy() {
        let root = Path::new("/Users/u/p");
        let p = Policy::for_project(&env(root));
        assert_eq!(
            p.protect,
            [root.join(".git"), root.join(".tarae.toml"), root.join("secrets")],
            "only paths inside the project"
        );
        let w: Vec<&str> = p.write.iter().map(|w| w.to_str().unwrap()).collect();
        assert_eq!(
            w,
            [
                "/private/tmp",
                "/private/var/folders/ab/cd",
                "/Users/u/.cargo/registry",
                "/Users/u/.cargo/git",
                "/Users/u/.cargo/build/shared",
                "/Users/u/Library/Caches/thing",
            ]
        );
        assert!(p.files.contains(&PathBuf::from("/Users/u/.cargo/.package-cache")));
        assert!(p.files.contains(&PathBuf::from("/Users/u/.cargo/.global-cache-wal")));
        assert!(!p.write.iter().chain(&p.files).any(|w| w.starts_with("/Users/u/.cargo/bin")));
    }

    /// Paths are parameters, never text in the profile — a path can't change what it says.
    #[test]
    fn the_profile_names_paths_only_through_parameters() {
        let root = Path::new("/Users/u/a\"b (c)");
        let p = Policy::for_project(&env(root));
        let text = p.profile();
        assert!(!text.contains("/Users/u"), "{text}");
        assert!(text.contains("(require-not (subpath (param \"P0\")))"));
        assert!(text.contains("(subpath (param \"W5\"))") && !text.contains("W6"));
        assert!(text.contains("(deny network*)") && text.contains("(deny appleevent-send)"));
        assert!(
            text.contains("(deny file-write* (subpath (param \"K0\")) (subpath (param \"K1\")))"),
            "{text}"
        );
        let params = p.params();
        assert_eq!(params[0], ("ROOT".into(), "/Users/u/a\"b (c)".into()));
        assert!(params.iter().any(|(k, v)| k == "P0" && v.ends_with("/.git")));
        let open = Policy { network: true, ..p.clone() };
        let text = open.profile();
        assert!(!text.contains("(deny network*)"), "{text}");
        assert!(
            text.contains("(deny network-outbound (remote unix-socket))"),
            "sockets stay guarded: {text}"
        );
        assert!(!text.contains("RUN"), "no folder of its own: only the project's sockets");
        assert!(text.contains("(param \"K0\")"), "the socket folders stay kept, the network open or not");
    }

    /// Connecting on this machine: the network closed, only the listed ports (`localhost:*` only when
    /// asked — its port denies can't win, measured); open, all but the risky ports not listed.
    #[test]
    fn local_ports_in_the_profile() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let outbound = |p: &Policy| -> Vec<String> {
            p.profile()
                .lines()
                .filter(|l| l.contains("network-outbound (remote ip"))
                .map(str::to_string)
                .collect()
        };
        assert!(outbound(&p).is_empty(), "closed, nothing listed: no connect allowed: {:?}", outbound(&p));
        assert!(p.profile().contains("(allow network-bind network-inbound (local ip \"localhost:*\"))"));
        let listed = Policy { ports: vec![3000, 5432], ..p.clone() };
        assert_eq!(
            outbound(&listed),
            ["(allow network-outbound (remote ip \"localhost:3000\") (remote ip \"localhost:5432\"))"]
        );
        let all = Policy { localhost: true, ..listed.clone() };
        assert_eq!(outbound(&all), ["(allow network-outbound (remote ip \"localhost:*\"))"]);
        let open = Policy { network: true, ports: vec![6379], ..p.clone() };
        let deny = outbound(&open);
        assert_eq!(deny.len(), 1);
        assert!(
            deny[0].starts_with("(deny network-outbound") && deny[0].contains("localhost:2375"),
            "{deny:?}"
        );
        assert!(!deny[0].contains("localhost:6379"), "a listed risky port stays reachable: {deny:?}");
        // Ports in the env are sorted, once each, and at most PORTS_MAX
        let many = Env { ports: (1..=500).rev().chain([7, 7]).collect(), ..env(Path::new("/Users/u/p")) };
        let ports = Policy::for_project(&many).ports;
        assert_eq!((ports.len(), ports[0], ports[1]), (PORTS_MAX, 1, 2));
        assert!(
            crate::settings::Kind::Ports { max: PORTS_MAX }
                .check(&toml::Value::Array((1..=201).map(toml::Value::from).collect()))
                .is_err_and(|e| e.contains("201 ports, at most 200") && e.contains("llm.sandbox-localhost"))
        );
    }

    /// A line of `log stream --style ndjson` (its other fields cut), the message as given.
    fn ndjson(msg: &str) -> String {
        serde_json::json!({ "eventMessage": msg, "processID": 0, "senderImagePath": "/System/Library/Extensions/Sandbox.kext/Contents/MacOS/Sandbox" }).to_string()
    }

    /// The denial out of the JSON (not the raw line — its tail would ride along: measured), only with this
    /// run's tag; `…-1.` isn't `…-10.`; repeats count; a path keeps its spaces.
    #[test]
    fn denials_are_read_by_their_tag() {
        let tag = "tarae-7-1.";
        let d = parse_denial(
            &ndjson(&format!("Sandbox: touch(80432) deny(1) file-write-create /Users/u/a b\n{tag}")),
            tag,
        );
        assert_eq!(
            d,
            Some(Denial {
                process: "touch".into(),
                op: "file-write-create".into(),
                target: "/Users/u/a b".into()
            })
        );
        let after_space =
            parse_denial(&ndjson(&format!("Sandbox: nc(9) deny(1) network-outbound 192.0.2.1:9 {tag}")), tag);
        assert_eq!(
            after_space.map(|d| (d.op, d.target)),
            Some(("network-outbound".into(), "192.0.2.1:9".into()))
        );
        let repeats = parse_denial(
            &ndjson(&format!("3 duplicate reports for Sandbox: sh(5) deny(1) file-write-data /x\n{tag}")),
            tag,
        );
        assert_eq!(repeats.map(|d| d.process), Some("sh".into()));
        assert_eq!(
            parse_denial(&ndjson("Sandbox: touch(1) deny(1) file-write-create /x\ntarae-7-10."), tag),
            None
        );
        assert_eq!(parse_denial(&ndjson("Sandbox: mds(1) deny(1) system-info vfs.disk-space"), tag), None);
        assert_eq!(parse_denial("not json", tag), None);
        // Escaped slashes, as log stream writes them
        let raw = format!(
            r#"{{"eventMessage":"Sandbox: touch(2) deny(1) file-write-create \/private\/tmp\/x\n{tag}","processID":0}}"#
        );
        assert_eq!(parse_denial(&raw, tag).map(|d| d.target), Some("/private/tmp/x".into()));
    }

    /// Every deny in the profile carries the run's tag — the network closed or open (the ones it has
    /// differ) — and without a tag none do.
    #[test]
    fn every_deny_carries_the_tag() {
        let p =
            Policy { tag: Some("tarae-7-1.".into()), ..Policy::for_project(&env(Path::new("/Users/u/p"))) };
        for p in [p.clone(), Policy { network: true, ..p.clone() }] {
            let text = p.profile();
            let denies: Vec<&str> = text.lines().filter(|l| l.starts_with("(deny")).collect();
            assert!(denies.len() >= 7, "{text}");
            for d in denies {
                assert!(d.ends_with(" (with message \"tarae-7-1.\"))"), "untagged: {d}");
            }
        }
        let untagged = Policy::for_project(&env(Path::new("/Users/u/p"))).profile();
        assert!(!untagged.contains("with message"), "{untagged}");
    }

    fn denial(op: &str, target: &str) -> Denial {
        Denial { process: "touch".into(), op: op.into(), target: target.into() }
    }

    /// Kept once each, at most `DENIALS_KEPT`, every one counted.
    #[test]
    fn blocked_keeps_each_once() {
        let mut b = Blocked::default();
        b.add(denial("file-write-create", "/a"));
        b.add(denial("file-write-create", "/a"));
        for i in 0..20 {
            b.add(denial("file-write-data", &format!("/f{i}")));
        }
        assert_eq!((b.total, b.distinct, b.seen.len()), (22, 21, DENIALS_KEPT));
    }

    /// What Claude reads: what was blocked (deduplicated, with who tried); that nothing was; or that nothing
    /// could watch — then the output's refusals.
    #[test]
    fn the_report_says_what_was_blocked() {
        let p = Policy { tag: Some("t.".into()), ..Policy::for_project(&env(Path::new("/Users/u/p"))) };
        let home = std::env::var("HOME").unwrap();
        let w = Watch::quiet("t.");
        w.record(denial("file-write-create", &format!("{home}/marker")));
        w.record(denial("file-write-create", &format!("{home}/marker")));
        w.record(denial("network-outbound", "remote:*:9"));
        let g = Guard::In(Policy { watch: Some(w), ..p.clone() });
        let r = g.report(&[]);
        assert!(r.starts_with("The sandbox blocked 2 things — 3 times:\n"), "{r}");
        // Plain words first, Seatbelt's after
        assert!(r.contains("  writing ~/marker (file-write-create, touch)\n"), "{r}");
        assert!(r.contains("  connecting to port 9 (network-outbound, touch)\n"), "{r}");
        assert!(r.contains("In tarae's sandbox") && r.contains("don't work around it"), "{r}");
        let none = Guard::In(Policy { watch: Some(Watch::quiet("t.")), ..p.clone() }).report(&[]);
        assert!(
            none.starts_with("The sandbox blocked nothing.") && !none.contains("likely the sandbox"),
            "{none}"
        );
        let failed = Watch::quiet("t.");
        failed.fail("log: Cannot run while sandboxed".into());
        let out = [(true, "touch: /x: Operation not permitted".to_string()), (false, "fine".to_string())];
        let r = Guard::In(Policy { watch: Some(failed), ..p }).report(&out);
        assert!(
            r.starts_with("tarae couldn't watch what the sandbox blocked (log: Cannot run while sandboxed)"),
            "{r}"
        );
        assert!(
            r.contains("likely the sandbox")
                && r.contains("  touch: /x: Operation not permitted")
                && !r.contains("fine"),
            "{r}"
        );
    }

    /// Seatbelt's words in plain ones (its network denials name only the port: measured) — and its own
    /// where there are none; for Claude, both.
    #[test]
    fn denials_in_plain_words() {
        let home = std::env::var("HOME").unwrap();
        let plain = |op: &str, target: &str| denial(op, target).plain();
        assert_eq!(plain("file-write-create", &format!("{home}/x")), "writing ~/x");
        assert_eq!(plain("file-write-data", "/dev/ttys004"), "writing /dev/ttys004");
        assert_eq!(plain("file-write-unlink", "/a"), "deleting /a");
        assert_eq!(plain("file-write-rename", "/a"), "moving /a");
        assert_eq!(plain("file-write-mode", "/a"), "changing /a");
        assert_eq!(plain("network-outbound", "remote:*:9"), "connecting to port 9");
        assert_eq!(plain("network-outbound", "remote:*:40147"), "connecting to port 40147");
        assert_eq!(
            plain("network-outbound", "/private/tmp/tmux-501/default"),
            "connecting to socket /private/tmp/tmux-501/default"
        );
        assert_eq!(plain("network-bind", "/private/tmp/x.sock"), "making socket /private/tmp/x.sock");
        assert_eq!(plain("appleevent-send", "com.apple.finder"), "Apple Event to com.apple.finder");
        assert_eq!(plain("mach-lookup", "com.apple.coreservices.launchservicesd"), "opening apps");
        assert_eq!(plain("mach-lookup", "com.apple.lsd.mapdb"), "opening apps");
        // As Seatbelt says it, where there are no plainer words
        assert_eq!(plain("mach-lookup", "com.apple.other"), "mach-lookup com.apple.other");
        assert_eq!(plain("network-outbound", "remote:*:x"), "network-outbound remote:*:x");
        assert_eq!(plain("system-info", ""), "system-info");
        assert_eq!(denial("file-write-unlink", "/a").reported(), "deleting /a (file-write-unlink, touch)");
        assert_eq!(denial("system-info", "x").reported(), "system-info x (touch)");
    }

    /// The panel's line: the first thing blocked and how many more; the quiet hint only while it runs and
    /// has printed nothing for `QUIET`.
    #[test]
    fn the_panel_line() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let w = Watch::quiet("t.");
        let g = Guard::In(Policy { watch: Some(w.clone()), ..p });
        let now = std::time::Instant::now();
        assert_eq!(panel_line(Some(&g), true, now), None, "nothing blocked: no line");
        w.record(denial("file-write-create", "/a"));
        w.record(denial("network-outbound", "remote:*:9"));
        let (line, quiet) = panel_line(Some(&g), true, now).unwrap();
        assert_eq!((line.as_str(), quiet), ("sandbox blocked writing /a  +1", None));
        let long_ago = now - QUIET - std::time::Duration::from_secs(1);
        assert!(
            panel_line(Some(&g), true, long_ago).unwrap().1.is_some_and(|q| q.contains("may be waiting"))
        );
        assert_eq!(panel_line(Some(&g), false, long_ago).unwrap().1, None, "done: no longer waiting");
        assert_eq!(panel_line(Some(&Guard::Off), true, long_ago), None);
    }

    /// What `sandbox-exec` says inside another sandbox (measured) reads as that.
    #[test]
    fn a_sandbox_inside_one_says_so() {
        assert!(
            missing_why("sandbox-exec: sandbox_apply: Operation not permitted\n")
                .contains("runs in a sandbox")
        );
    }

    /// For real, on macOS — with harmless markers in a temp folder: writes land in the project and nowhere
    /// else, `.git` and `.tarae.toml` stay as they are, a symlink out doesn't help, localhost answers and
    /// the rest of the network doesn't, and a pseudo-terminal opens. Inside a sandbox already (Claude's
    /// own `cargo test`) none can start: the probe says so, and that's what's checked.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_keeps_writes_in_the_project() {
        if skipped_inside("sandbox::tests::seatbelt_keeps_writes_in_the_project") {
            assert!(matches!(probe_now(), Availability::Missing(w) if w.contains("runs in a sandbox")));
            return;
        }
        assert_eq!(probe_now(), Availability::Ready);
        let t = std::env::temp_dir().join(format!("tarae-sb-{}", std::process::id()));
        let (root, out) = (t.join("project"), t.join("outside"));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let (root, out) = (real(&root), real(&out));
        std::os::unix::fs::symlink(&out, root.join("link")).ok();
        // A server of this test's own, its port opened for the sandbox
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Not the usual temp folders: here, the outside folder is in one
        let p = Policy {
            root: root.clone(),
            protect: vec![root.join(".git"), root.join(".tarae.toml")],
            write: Vec::new(),
            files: Vec::new(),
            keep: Vec::new(),
            network: false,
            ports: vec![port],
            localhost: false,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
        };
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap()
        };
        assert!(sh("touch inside").status.success());
        assert!(root.join("inside").exists());
        for (target, what) in [
            (out.join("marker"), "outside"),
            (root.join(".git/marker"), ".git"),
            (root.join(".tarae.toml"), ".tarae.toml"),
            (root.join("link/through"), "a symlink out"),
        ] {
            let r = sh(&format!("touch '{}'", target.display()));
            assert!(!r.status.success() && !target.exists(), "{what}: written");
            assert!(String::from_utf8_lossy(&r.stderr).contains("Operation not permitted"), "{what}");
        }
        // Its open port on this machine answers; TEST-NET-1 (never routed) is refused at once, not timed out
        assert!(sh(&format!("/usr/bin/nc -z -w 2 127.0.0.1 {port}")).status.success());
        drop(listener);
        let started = std::time::Instant::now();
        assert!(!sh("/usr/bin/nc -z -w 5 192.0.2.1 9").status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "refused, not timed out");
        // A pseudo-terminal opens (without /dev/ptmx a pty test hangs instead of failing: measured)
        assert!(sh("/usr/bin/script -q /dev/null /usr/bin/true").status.success());
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS: a server outside the run listening on a unix socket — in `/private/tmp` or
    /// `$TMPDIR`, where tmux's and zellij's live — can't be reached from inside, the network closed or
    /// open (a tmux server there ran a command for a sandboxed one: reproduced). The run's own folder is
    /// its `TMPDIR`, and a socket made there is reached; the project's are too. Harmless: the listeners
    /// are this test's own, and nothing is sent but "hi".
    #[cfg(target_os = "macos")]
    #[test]
    fn unix_sockets_only_in_the_run_and_the_project() {
        use std::os::unix::net::UnixListener;
        if skipped_inside("sandbox::tests::unix_sockets_only_in_the_run_and_the_project") {
            return;
        }
        let pid = std::process::id();
        let t = real(&std::env::temp_dir()).join(format!("tarae-sock-{pid}"));
        let root = t.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let in_tmp = PathBuf::from(format!("/private/tmp/tarae-sock-{pid}"));
        std::fs::create_dir_all(&in_tmp).unwrap();
        let outside = [in_tmp.join("s"), t.join("s")];
        for network in [false, true] {
            let run = RunDir::new().unwrap();
            let p = Policy {
                root: root.clone(),
                protect: vec![root.join(".git")],
                write: vec!["/private/tmp".into(), user_temp(&real(&std::env::temp_dir()))],
                files: Vec::new(),
                keep: Vec::new(),
                network,
                ports: Vec::new(),
                localhost: false,
                run: Some(run.clone()),
                tag: None,
                watch: None,
                granted: Vec::new(),
            };
            let sh = |script: &str| {
                let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
                let out = c.current_dir(&root).stdin(Stdio::null()).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            let reached = |path: &Path| {
                std::fs::remove_file(path).ok();
                let l = UnixListener::bind(path).unwrap();
                l.set_nonblocking(true).unwrap();
                sh(&format!("echo hi | /usr/bin/nc -w 2 -U '{}'", path.display()));
                l.accept().is_ok()
            };
            for path in &outside {
                assert!(!reached(path), "{}: reached from inside (network open: {network})", path.display());
            }
            assert!(reached(&root.join("s")), "the project's socket (network open: {network})");
            // Its own folder: its TMPDIR, where a socket is made and reached from inside
            assert_eq!(sh("echo \"$TMPDIR\""), run.path().display().to_string());
            let got = sh("/usr/bin/nc -w 2 -lU \"$TMPDIR/s\" > \"$TMPDIR/got\" & sleep 0.5; \
                 echo hi | /usr/bin/nc -w 2 -U \"$TMPDIR/s\"; wait; cat \"$TMPDIR/got\"");
            assert_eq!(got, "hi", "a socket in its own folder (network open: {network})");
        }
        std::fs::remove_dir_all(&t).ok();
        std::fs::remove_dir_all(&in_tmp).ok();
    }

    /// A folder of this test's own under `target/` (real path), made fresh.
    #[cfg(target_os = "macos")]
    fn scratch(name: &str) -> PathBuf {
        let t = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("target/{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        std::fs::create_dir_all(&t).unwrap();
        real(&t)
    }

    /// For real, on macOS: a watch sees its own run's denials, and not another's sandboxed at the same time
    /// under a tag it begins with (`…-1.` / `…-10.`). Harmless: the denied writes are touches into a folder
    /// of this test's own under `target/`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_watch_sees_its_own_runs_denials() {
        if skipped_inside("sandbox::tests::a_watch_sees_its_own_runs_denials") {
            return;
        }
        let t = scratch("sb-watch");
        let (root, out) = (t.join("project"), t.join("out"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let policy = |tag: &str| Policy {
            root: root.clone(),
            protect: Vec::new(),
            write: Vec::new(),
            files: Vec::new(),
            keep: Vec::new(),
            network: false,
            ports: Vec::new(),
            localhost: false,
            run: None,
            tag: Some(tag.to_string()),
            watch: None,
            granted: Vec::new(),
        };
        let pid = std::process::id();
        let (mine, other) = (format!("tarae-test-{pid}-1."), format!("tarae-test-{pid}-10."));
        let w = Watch::start(&mine);
        std::thread::sleep(WATCH_READY);
        for (tag, name) in [(&mine, "mine"), (&other, "other")] {
            let mut c =
                command(Some(&policy(tag)), "/usr/bin/touch", &[out.join(name).display().to_string()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap();
        }
        std::thread::sleep(WATCH_LAST * 2);
        let b = w.blocked();
        w.finish();
        assert_eq!(b.failed, None);
        let seen: Vec<String> = b.seen.iter().map(|d| format!("{} {}", d.op, d.target)).collect();
        assert_eq!(seen, [format!("file-write-create {}", out.join("mine").display())]);
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS: the multiplexers' socket folders stay as they are though they sit in a writable
    /// temp folder — nothing made in them, what's there not moved away. Harmless: stand-in folders under
    /// `target/`, a plain file standing in for the socket.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_multiplexers_socket_folders_stay_as_they_are() {
        if skipped_inside("sandbox::tests::the_multiplexers_socket_folders_stay_as_they_are") {
            return;
        }
        let t = scratch("sb-mux");
        let (root, tmp) = (t.join("project"), t.join("tmp"));
        let kept = [tmp.join("tmux-501"), tmp.join("zellij-501")];
        std::fs::create_dir_all(&root).unwrap();
        for dir in &kept {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("default"), "").unwrap();
        }
        let p = Policy {
            root: root.clone(),
            protect: Vec::new(),
            write: vec![tmp.clone()],
            files: Vec::new(),
            keep: kept.to_vec(),
            network: false,
            ports: Vec::new(),
            localhost: false,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
        };
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap()
        };
        sh(&format!("touch '{}'", tmp.join("free").display()));
        assert!(tmp.join("free").exists(), "the temp folder around them stays writable");
        for dir in &kept {
            sh(&format!("touch '{}'", dir.join("marker").display()));
            assert!(!dir.join("marker").exists(), "{}: written in", dir.display());
            sh(&format!("mv '{}' '{}'", dir.join("default").display(), dir.join("gone").display()));
            assert!(
                dir.join("default").exists() && !dir.join("gone").exists(),
                "{}: moved away",
                dir.display()
            );
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS — what the user granted (`blocked.rs`) opens under the real profile, and only that:
    /// a folder kept for the project (through the store file) and one for this conversation are written in,
    /// their sibling isn't; a granted port is reached, another isn't. The policy is made as a run's is
    /// (`tools::with_allowed` → `llm.sandbox-write`/`-ports` → `Policy::for_project`), but without the usual
    /// temp folders — the test's own folders are in one. Harmless: markers in its temp folder, listeners of
    /// its own on 127.0.0.1 (only `nc -z` knocks).
    #[cfg(target_os = "macos")]
    #[test]
    fn what_the_user_granted_opens_under_the_real_profile() {
        use crate::allow::{Allowed, Grant};
        use std::net::TcpListener;
        if skipped_inside("sandbox::tests::what_the_user_granted_opens_under_the_real_profile") {
            return;
        }
        let t = real(&std::env::temp_dir()).join(format!("tarae-sb-granted-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let (root, kept, chat, sibling) =
            (t.join("project"), t.join("kept"), t.join("chat"), t.join("sibling"));
        for d in [&root, &kept, &chat, &sibling] {
            std::fs::create_dir_all(d).unwrap();
        }
        let granted = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = |l: &TcpListener| l.local_addr().unwrap().port();
        for l in [&granted, &other] {
            l.set_nonblocking(true).unwrap();
        }
        // Kept for the project, as `A` keeps it; this conversation's, as `a` adds it
        let store = t.join("state/sandbox.json");
        crate::allow::keep(&store, &root, |a| {
            a.add(&Grant::Write(kept.clone()));
            a.add(&Grant::Port(port(&granted)));
        })
        .unwrap();
        let from_store = crate::allow::Store::read(Some(&store)).get(&root);
        let mut this_chat = Allowed::default();
        this_chat.add(&Grant::Write(chat.clone()));
        let (llm, said) =
            crate::tools::with_allowed(&crate::llm::LlmConfig::default(), &[&from_store, &this_chat], None);
        assert_eq!(said.len(), 3, "{said:?}");
        let env = Env {
            root: root.clone(),
            write: llm.sandbox_write.clone(),
            ports: llm.sandbox_ports,
            ..Env::default()
        };
        let p = Policy::for_project(&env);
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap()
        };
        for (dir, what) in [(&kept, "kept for the project"), (&chat, "this conversation's")] {
            let r = sh(&format!("touch '{}'", dir.join("marker").display()));
            assert!(r.status.success() && dir.join("marker").exists(), "{what}: not written — {r:?}");
        }
        let r = sh(&format!("touch '{}'", sibling.join("marker").display()));
        assert!(!r.status.success() && !sibling.join("marker").exists(), "the sibling: written");
        assert!(String::from_utf8_lossy(&r.stderr).contains("Operation not permitted"));
        let reached = |l: &TcpListener| {
            sh(&format!("/usr/bin/nc -z -w 2 127.0.0.1 {}", port(l)));
            l.accept().is_ok()
        };
        assert!(reached(&granted), "the granted port");
        assert!(!reached(&other), "another port");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS — connecting to this machine's ports from inside:
    /// - the network closed (the default): an ordinary port isn't reached, a listed one
    ///   (`llm.sandbox-ports`) is;
    /// - `llm.sandbox-localhost`: both are, and a risky one too (it can't be left out — measured);
    /// - the network open: an ordinary port is reached, a risky one (`RISKY_PORTS`) only when listed.
    ///
    /// Harmless: the listeners are this test's own, on 127.0.0.1, and only `nc -z` knocks.
    #[cfg(target_os = "macos")]
    #[test]
    fn local_ports_open_only_where_asked() {
        use std::net::TcpListener;
        if skipped_inside("sandbox::tests::local_ports_open_only_where_asked") {
            return;
        }
        // One of them free here (Redis or Jupyter may be running)
        let risky = RISKY_PORTS
            .iter()
            .find_map(|(port, _)| TcpListener::bind(("127.0.0.1", *port)).ok())
            .expect("a risky port free to listen on");
        let ordinary = TcpListener::bind("127.0.0.1:0").unwrap();
        let listed = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = |l: &TcpListener| l.local_addr().unwrap().port();
        for l in [&risky, &ordinary, &listed] {
            l.set_nonblocking(true).unwrap();
        }
        let root = scratch("sb-ports");
        let policy = |network: bool, localhost: bool, ports: Vec<u16>| Policy {
            root: root.clone(),
            protect: Vec::new(),
            write: Vec::new(),
            files: Vec::new(),
            keep: Vec::new(),
            network,
            ports,
            localhost,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
        };
        let reached = |p: &Policy, l: &TcpListener| {
            let script = format!("/usr/bin/nc -z -w 2 127.0.0.1 {}", port(l));
            let mut c = command(Some(p), "/bin/sh", &["-c".into(), script]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap();
            l.accept().is_ok()
        };
        let closed = policy(false, false, vec![port(&listed)]);
        assert!(!reached(&closed, &ordinary), "closed: an ordinary port");
        assert!(reached(&closed, &listed), "closed: a listed port");
        assert!(!reached(&closed, &risky), "closed: port {}", port(&risky));
        let localhost = policy(false, true, Vec::new());
        assert!(reached(&localhost, &ordinary), "sandbox-localhost: an ordinary port");
        assert!(reached(&localhost, &listed), "sandbox-localhost: the listed one");
        assert!(reached(&localhost, &risky), "sandbox-localhost: port {} (opens them all)", port(&risky));
        let open = policy(true, false, Vec::new());
        assert!(reached(&open, &ordinary), "network open: an ordinary port");
        assert!(!reached(&open, &risky), "network open: port {}, not listed", port(&risky));
        let open_listed = policy(true, false, vec![port(&risky)]);
        assert!(reached(&open_listed, &risky), "network open: port {}, listed", port(&risky));
        std::fs::remove_dir_all(&root).ok();
    }
}
