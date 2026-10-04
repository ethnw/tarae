//! Claude's commands in a sandbox. What Claude runs — `run` and `run_tests` (`tools.rs`) — goes through
//! macOS's Seatbelt (`/usr/bin/sandbox-exec`) with a profile made here (`Policy::profile`): it may write
//! in the project (but `protected_names` — `.git`, `build.rs`, `Cargo.toml`, `.cargo`… wherever they sit —
//! and `llm.sandbox-protect`), the temp folders and the toolchain's own folders (cargo's registry and git
//! caches only with the network open — `Policy::caches` —, its target and build dirs when they're the checkout's own —
//! `cargo_dir_verdict`, #20: a cargo config in the checkout names them, and Claude can write it; Go's caches), connect to nothing
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
    In(Box<Policy>),
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
        let in_caches = self.refused_in_caches(output);
        if b.total == 0 && b.failed.is_none() && !in_caches.is_empty() {
            let mut s =
                "The sandbox's own report of what it blocked didn't come, but the output says a write \
                         in cargo's caches was refused:\n"
                    .to_string();
            for l in &in_caches {
                s.push_str(&format!("  {}\n", l.trim()));
            }
            if let Guard::In(p) = self {
                s.push_str(&caches_kept(p));
            }
            s.push_str(&rules);
            return s;
        }
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
            let (kept, all) = self.kept(&b.seen, !in_caches.is_empty());
            s.push_str(&kept);
            s.push_str(&rules);
            if !all || b.distinct > b.seen.len() {
                s.push_str(
                    "\nIf the command needs one of these, tell the user — they can open it from the run's \
                     panel (space m s), or llm.sandbox-write, llm.sandbox-ports, llm.sandbox-localhost and \
                     llm.sandbox-network open more — or ask to run it outside the sandbox (outside_sandbox, \
                     with your reason); don't work around it.",
                );
            }
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
                if let (false, Guard::In(p)) = (in_caches.is_empty(), self) {
                    s.push_str(&caches_kept(p));
                }
                s
            }
        }
    }

    /// The output's lines that name a path in cargo's caches as what failed, when it says a write was
    /// refused — what's left to go by when the kernel's report of the denial doesn't come (measured
    /// 2026-10-04: a cargo that couldn't unpack a crate offline went unreported in 7 of 9 runs, even after
    /// 10 s, while every `/bin/mkdir` in a row was reported).
    fn refused_in_caches<'a>(&self, output: &'a [(bool, String)]) -> Vec<&'a str> {
        let Guard::In(p) = self else { return Vec::new() };
        if p.caches.is_empty() || !output.iter().any(|(_, l)| l.contains("Operation not permitted")) {
            return Vec::new();
        }
        let caches: Vec<String> = p.caches.iter().map(|c| format!("{}/", c.display())).collect();
        let failed = |l: &str| l.contains("failed") || l.contains("Operation not permitted");
        output
            .iter()
            .map(|(_, l)| l.as_str())
            .filter(|l| failed(l) && caches.iter().any(|c| l.contains(c.as_str())))
            .take(3)
            .collect()
    }

    /// What among `seen` the profile keeps on purpose, each said with why and what to do instead — and
    /// whether that's all of them. `caches`: the output says a write in cargo's caches was refused.
    fn kept(&self, seen: &[Denial], caches: bool) -> (String, bool) {
        let Guard::In(p) = self else { return (String::new(), false) };
        let (mut names, mut git, mut crates, mut all) = (Vec::new(), false, caches, true);
        for d in seen {
            let Some(path) = d.file() else {
                all = false;
                continue;
            };
            match path.strip_prefix(&p.root) {
                Ok(rel) if protected_in(&p.root, path) => {
                    match rel.iter().any(|c| c.eq_ignore_ascii_case(".git")) {
                        true => git = true,
                        false => names.push(rel.display().to_string()),
                    }
                }
                _ if p.caches.iter().any(|c| path.starts_with(c)) => crates = true,
                _ => all = false,
            }
        }
        names.sort();
        names.dedup();
        let mut s = String::new();
        if !names.is_empty() {
            let it = if names.len() == 1 { "it" } else { "them" };
            s.push_str(&format!(
                "{}: tarae keeps {it} from your commands — what's in {it} runs later outside the sandbox (the \
                 user's builds, their terminal) or sets how things run. To change {it}, use Edit: the user \
                 reviews it. Don't work around it (another name, a copy, a link).\n",
                names.join(", ")
            ));
        }
        if git {
            s.push_str(
                "Git's own folder is kept from your commands: committing, staging, switching branches and the \
                 like are the user's (their git tab) — ask them, or ask to run it outside the sandbox \
                 (outside_sandbox, with your reason).\n",
            );
        }
        if crates {
            s.push_str(&caches_kept(p));
        }
        (s, all)
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
                // (what the checkout's cargo config named and stays closed — #20)
                let refused = match p.refused.is_empty() {
                    true => String::new(),
                    false => {
                        let each: Vec<String> = p
                            .refused
                            .iter()
                            .map(|(d, why)| format!("{} ({why})", crate::allow::tilde(d)))
                            .collect();
                        format!(
                            " It kept closed what the checkout's cargo config named: {} — a build writing \
                             there fails; the user can open a folder with llm.sandbox-write.",
                            each.join("; ")
                        )
                    }
                };
                let names = protected_names().collect::<Vec<_>>().join(", ");
                let crates = match p.caches.is_empty() {
                    false => {
                        " (not cargo's caches — its crates and git checkouts — while the network is closed)"
                    }
                    true => "",
                };
                format!(
                    "In tarae's sandbox: it could write only in the project — not {names}, anywhere in it (Edit \
                     changes those) — temp folders and the toolchain's own{crates}{net}.{granted}{refused}"
                )
            }
            Guard::Without(why) => format!("Ran without a sandbox ({why}) — the user agreed."),
            Guard::Off => "Ran without a sandbox — the user turned it off (llm.sandbox).".into(),
            Guard::Outside => "Ran outside the sandbox — the user chose to, this once.".into(),
        }
    }
}

/// What Claude reads when a write in cargo's caches was kept (`Policy::caches`): why, and what to do instead.
fn caches_kept(p: &Policy) -> String {
    let caches: Vec<String> = p.caches.iter().map(|c| crate::allow::tilde(c)).collect();
    format!(
        "Cargo's caches ({}) stay as they are while the network is closed: every project's build outside \
         the sandbox uses the crates and git checkouts there. A build that needs a crate unpacked or a git \
         dependency checked out fails here — that's not something to fix in the code: tell the user. A build \
         or `cargo fetch` of theirs puts it there, then run it again; llm.sandbox-network opens the caches to \
         your commands.\n",
        caches.join(", ")
    )
}

/// What a sandboxed command may do. Every path is real (canonical).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The project: writable but for `protect` and the names `protected_names` gives.
    pub root: PathBuf,
    /// Inside `root`, kept as they are (`llm.sandbox-protect`).
    pub protect: Vec<PathBuf>,
    /// Folders writable besides the project (temp, the toolchain's, `llm.sandbox-write`).
    pub write: Vec<PathBuf>,
    /// Single files writable — cargo's lock and last-use files (`CARGO_FILES`), the network closed even
    /// those inside `caches`.
    pub files: Vec<PathBuf>,
    /// Folders inside writable ones kept as they are: the terminal multiplexers' sockets (`mux_dirs`) — a
    /// command could otherwise delete or replace the multiplexer the user works in.
    pub keep: Vec<PathBuf>,
    /// Cargo's shared caches — `registry/` (the index, the downloaded `.crate`s, the unpacked crates) and
    /// `git/` (git dependencies' databases and checkouts) — kept as they are while the network is closed:
    /// every project's build outside the sandbox uses what's there (cargo doesn't check an unpacked crate or a
    /// checkout again), and with no download a build has nothing new to put there. Only `files` are written
    /// in them. A crate downloaded and never unpacked, a git dependency never checked out (or what cargo's
    /// cleanup removed) fails here, and the report says why.
    pub caches: Vec<PathBuf>,
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
    /// Unix sockets outside the project it may connect to, each that one file (`allow::Grant::Socket`).
    pub sockets: Vec<PathBuf>,
    /// The folders cargo named that stay closed, and why (`cargo_dir_verdict`) — said with the result and on
    /// the panel.
    pub refused: Vec<(PathBuf, String)>,
}

/// Whether a folder cargo names for the checkout at `root` (`d`, a real path) may be written by its runs
/// (#20 — the checkout's cargo config names it, and Claude can write that): `Ok(true)` it needs a rule of
/// its own (this checkout's own per-checkout build dir, `own`, under cargo's home as `build/xx/yyyy`),
/// `Ok(false)` it's writable already (inside the checkout, a temp folder); `Err` why it stays closed — home or
/// above it, a folder holding the checkout, cargo's home or above it, or anything else.
pub fn cargo_dir_verdict(
    d: &Path,
    root: &Path,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
    own: Option<&Path>,
    tmp: Option<&Path>,
) -> Result<bool, String> {
    if home.is_some_and(|h| h.starts_with(d)) {
        return Err(if home == Some(d) { "it's your home".into() } else { "it holds your home".into() });
    }
    if root.starts_with(d) && root != d {
        return Err("it holds the checkout".into());
    }
    if cargo_home.is_some_and(|c| c.starts_with(d)) {
        return Err("it's cargo's home".into());
    }
    if d.starts_with(root) {
        return Ok(false);
    }
    let per_checkout = |o: &Path| {
        let below = cargo_home.and_then(|c| o.strip_prefix(c.join("build")).ok());
        below.is_some_and(|b| b.components().count() == 2)
    };
    if own.is_some_and(|o| o == d && per_checkout(o)) {
        return Ok(true);
    }
    let temps = [Some(Path::new("/private/tmp")), tmp];
    if temps.iter().flatten().any(|t| d.starts_with(t) && d != *t) {
        return Ok(false);
    }
    Err("it isn't in the checkout, its own build dir under cargo's home, or a temp folder".into())
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
        let base = profile_path(&std::env::temp_dir());
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

/// A hard link refused because its source isn't writable — Seatbelt's `forbidden-link-priv<file-write*>
/// <source> <link>` (measured).
const LINK_PRIV: &str = "forbidden-link-priv";

impl Denial {
    /// The file it was about: the target — for a refused link, the file linked.
    fn file(&self) -> Option<&Path> {
        match self.op.as_str() {
            op if op.starts_with(LINK_PRIV) => {
                Some(Path::new(self.target.rsplit_once(" /").map_or(self.target.as_str(), |(s, _)| s)))
            }
            op if op.starts_with("file-") => Some(Path::new(&self.target)),
            _ => None,
        }
    }

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
            _ if op.starts_with(LINK_PRIV) => {
                format!("linking {}", t.rsplit_once(" /").map_or(t.as_str(), |(s, _)| s))
            }
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

    /// A watch whose child is `program args` — a stand-in for `log stream` (tests; killed with it).
    #[cfg(test)]
    pub fn spawned_for_test(program: &str, arg: &str) -> std::sync::Arc<Watch> {
        let w = Watch::quiet("test.");
        let child = Command::new(program).arg(arg).stdin(Stdio::null()).spawn().unwrap();
        *w.child.lock().unwrap() = Some(child);
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

    /// Its `log stream`'s pid while it watches — `:procs` knows it as the run's.
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(std::process::Child::id)
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
    let g = guard?;
    let b = g.watch().map(|w| w.blocked()).unwrap_or_default();
    // (nothing denied yet: what the checkout's cargo config named and stays closed — #20)
    let Some(first) = b.seen.first() else {
        let Guard::In(p) = g else { return None };
        let (d, _) = p.refused.first()?;
        let more = if p.refused.len() > 1 { format!("  +{}", p.refused.len() - 1) } else { String::new() };
        return Some((
            format!("sandbox kept {} closed — cargo's config named it{more}", crate::allow::tilde(d)),
            None,
        ));
    };
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

/// What Claude's runs never write in the project, wherever the name sits in it (a crate's `build.rs`, a
/// folder's `.cargo`): `llm.review-ask`'s defaults — what runs later outside the sandbox (the user's builds,
/// `:preflight`, the terminal) or sets how things run — plus `.tarae.local.toml` (read as the user's own
/// config, its preflight steps run unsandboxed: `config::personal`) and `.git`. Claude changes them with
/// Edit, which the user reviews (`edits::listed`). Names, not paths — static in the profile — in any case:
/// APFS opens `Build.rs` for `build.rs`, and so does cargo.
pub fn protected_names() -> impl Iterator<Item = &'static str> {
    crate::llm::REVIEW_ASK.iter().copied().chain([".tarae.local.toml", ".git"])
}

/// Folders of tools' output, where those names are what the tool writes: cmake's `Makefile`s and `cargo
/// package`'s `Cargo.toml`s under `target/`, each package's `package.json` under `node_modules/`. What's
/// there runs later anyway (build scripts, packages), so keeping the names closes nothing there — a name
/// below one of these is let through (`.git/modules/target/hooks` isn't: the name comes first).
const OUTPUTS: [&str; 2] = ["target", "node_modules"];

/// A name as a Seatbelt regex in any case: `[bB][uU][iI][lL][dD]\.[rR][sS]`.
fn any_case(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            c if c.is_ascii_alphabetic() => format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase()),
            c if c.is_ascii_digit() || c == '_' || c == '-' => c.to_string(),
            c => format!("\\{c}"),
        })
        .collect()
}

/// Whether the profile keeps `path` (real) from a run in the project at `root` by its name
/// (`protected_names`, `OUTPUTS`) — the profile's rule, in Rust: the report names what it denied.
pub fn protected_in(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return false };
    let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
    let named = |p: &&std::ffi::OsStr| {
        p.to_str().is_some_and(|p| protected_names().any(|n| n.eq_ignore_ascii_case(p)))
    };
    let output = parts.iter().position(|p| OUTPUTS.iter().any(|o| p == o));
    parts.iter().any(named) && !output.is_some_and(|o| parts[o + 1..].iter().any(named))
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
    (9222, "Chrome DevTools — drives the browser, signed in as you"),
    (9229, "Node inspector — runs code in the inspected program"),
    (6379, "Redis — its CONFIG and MODULE commands write files and load code"),
    (2379, "etcd — a cluster's state, its secrets included"),
    (6443, "Kubernetes API server — starts pods"),
    (10250, "kubelet — runs commands in the node's pods"),
];

/// `llm.sandbox-ports` at most: one rule a port, and they cost every command (measured: 1000 rules 20 ms,
/// 5000 0.4 s, 20000 don't compile).
pub const PORTS_MAX: usize = 200;

/// Cargo's shared caches in its home, kept while the network is closed (`Policy::caches`).
const CARGO_CACHES: [&str; 2] = ["registry", "git"];

/// The files in cargo's home a sandboxed cargo writes, the network closed or open: its locks, its
/// last-use database (SQLite, with its journal or write-ahead log), and the tag that tells backup tools a
/// cache is one — written again when it's gone (measured, cargo 1.99, 2026-10-04).
const CARGO_FILES: [&str; 8] = [
    ".package-cache",
    ".package-cache-mutate",
    ".global-cache",
    ".global-cache-journal",
    ".global-cache-wal",
    ".global-cache-shm",
    "registry/CACHEDIR.TAG",
    "git/CACHEDIR.TAG",
];

/// The extended attribute cargo sets on `registry` and `git` at every run, so file sync (iCloud, File
/// Provider) leaves them alone — the one write to the caches an offline build makes (measured, cargo 1.99,
/// 2026-10-04): denied, the build goes on, but every run's report would carry it.
const CARGO_XATTR: &str = "com.apple.fileprovider.ignore#P";

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
        // (A hard link needs these write rules to allow its source too — Seatbelt's own
        // `forbidden-link-priv<file-write*>`, measured 2026-10-04, `target/sb/p1-links2.sh` — so a run can't
        // link a file from outside into the project, nor give a kept one a second name to write through: no
        // `file-link` rule of its own is needed)
        p.push_str(&deny("file-write*"));
        let protect: String =
            (0..self.protect.len()).map(|i| format!(" (require-not (subpath (param \"P{i}\")))")).collect();
        p.push_str(&format!("(allow file-write* (require-all (subpath (param \"ROOT\")){protect}))\n"));
        let run = if self.run.is_some() { " (subpath (param \"RUN\"))" } else { "" };
        let dirs: String = (0..self.write.len()).map(|i| format!(" (subpath (param \"W{i}\"))")).collect();
        if !run.is_empty() || !dirs.is_empty() {
            p.push_str(&format!("(allow file-write*{run}{dirs})\n"));
        }
        // After the allows, so these win. The names a run keeps off, below the project's root however deep
        // — the regex starts with as many path parts as the root has (no path in it), so a name in the
        // root's own path (a checkout under `.claude/worktrees`) doesn't close the whole project
        let depth = self.root.components().filter(|c| matches!(c, Component::Normal(_))).count();
        let below = format!("^{}/(.*/)?", "/[^/]+".repeat(depth));
        let names = protected_names().map(any_case).collect::<Vec<_>>().join("|");
        p.push_str(&deny(&format!(
            "file-write* (require-all (subpath (param \"ROOT\")) (regex #\"{below}({names})(/|$)\") \
             (require-not (regex #\"{below}({})/(.*/)?({names})(/|$)\")))",
            OUTPUTS.join("|")
        )));
        // …the multiplexers' socket folders (they sit inside writable temp ones), cargo's caches
        let keep: String = (0..self.keep.len()).map(|i| format!(" (subpath (param \"K{i}\"))")).collect();
        let caches: String = (0..self.caches.len()).map(|i| format!(" (subpath (param \"C{i}\"))")).collect();
        if !keep.is_empty() || !caches.is_empty() {
            p.push_str(&deny(&format!("file-write*{keep}{caches}")));
        }
        // …but cargo's own files, each that one file (its backup tags sit in the caches)
        let files: String = (0..self.files.len()).map(|i| format!(" (literal (param \"F{i}\"))")).collect();
        if !files.is_empty() {
            p.push_str(&format!("(allow file-write*{files})\n"));
        }
        // …and the mark cargo puts on its caches at every run, on those folders themselves
        let caches: String = (0..self.caches.len()).map(|i| format!(" (literal (param \"C{i}\"))")).collect();
        if !caches.is_empty() {
            p.push_str(&format!(
                "(allow file-write-xattr (require-all (require-any{caches}) (xattr \"{CARGO_XATTR}\")))\n"
            ));
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
        // …and the sockets the user granted: each that one file, connecting only (#119)
        let sockets: String =
            (0..self.sockets.len()).map(|i| format!(" (literal (param \"S{i}\"))")).collect();
        if !sockets.is_empty() {
            p.push_str(&format!(
                "(allow network-outbound (require-all (remote unix-socket) (require-any{sockets})))\n"
            ));
        }
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
        v.extend(self.caches.iter().enumerate().map(|(i, p)| (format!("C{i}"), s(p))));
        v.extend(self.sockets.iter().enumerate().map(|(i, p)| (format!("S{i}"), s(p))));
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
        // (`.git`, `.tarae.toml` and the rest are names the profile keeps itself: `protected_names`)
        let mut protect = Vec::new();
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
        // Cargo's caches: written as it downloads and unpacks — with the network closed, kept (`caches`)
        let cargo_caches: Vec<PathBuf> =
            env.cargo_home.iter().flat_map(|ch| CARGO_CACHES.map(|c| ch.join(c))).collect();
        if env.network {
            write.extend(cargo_caches.iter().cloned());
        }
        write.extend(env.dirs.iter().cloned());
        // Cargo's folders as the checkout's config names them: only its own (#20)
        let mut refused = Vec::new();
        for (d, which) in &env.cargo_dirs {
            let (home, ch, own) = (env.home.as_deref(), env.cargo_home.as_deref(), env.own_build.as_deref());
            match cargo_dir_verdict(d, &root, home, ch, own, env.tmp.as_deref()) {
                Ok(true) => write.push(d.clone()),
                Ok(false) => {}
                Err(why) if !refused.iter().any(|(r, _)| r == d) => {
                    refused.push((d.clone(), format!("cargo's {which} — {why}")))
                }
                Err(_) => {}
            }
        }
        for w in &env.write {
            let p = match (w.strip_prefix("~/"), &env.home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => root.join(w),
            };
            write.push(profile_path(&p));
        }
        // Never the project or what holds it (that would open .git again); inside it, the project's rule
        write.retain(|w| !root.starts_with(w) && !w.starts_with(&root));
        write.dedup();
        let files = env.cargo_home.iter().flat_map(|ch| CARGO_FILES.map(|f| ch.join(f))).collect();
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
            caches: if env.network { Vec::new() } else { cargo_caches },
            network: env.network,
            ports,
            localhost: env.localhost,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused,
        }
    }
}

/// Where tmux and zellij keep their sockets: tmux in `$TMUX_TMPDIR` (else `/tmp`) `/tmux-<uid>`, zellij in
/// `$ZELLIJ_SOCKET_DIR` (else `$TMPDIR/zellij-<uid>`). Real paths.
fn mux_dirs(env: &Env) -> Vec<PathBuf> {
    let tmux_base = env.tmux_tmpdir.as_deref().unwrap_or(Path::new("/private/tmp"));
    let mut v = vec![profile_path(tmux_base).join(format!("tmux-{}", env.uid))];
    match (&env.zellij_dir, &env.tmp) {
        (Some(d), _) => v.push(profile_path(d)),
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
    /// The toolchain's own folders (`toolchain_dirs`) — Go's caches; cargo's apart, below.
    pub dirs: Vec<PathBuf>,
    /// The folders cargo names for the checkout (real paths, which: `target-dir` · `build-dir`) — a cargo
    /// config in the checkout sets them, so they open only as `cargo_dir_verdict` says (#20).
    pub cargo_dirs: Vec<(PathBuf, &'static str)>,
    /// This checkout's own per-checkout build dir: what cargo names with your own `build-dir` (`$CARGO_BUILD_
    /// BUILD_DIR`, else cargo's home config) put over the checkout's — `cargo-home/build/xx/yyyy`.
    pub own_build: Option<PathBuf>,
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

/// A path as the profile writes it: the real path; for one that isn't there yet, its folder's real path
/// joined with what's below as it is (`..` too — Seatbelt resolves it; for an "is it inside" check use
/// `paths::inside`, which refuses a `..` below what's there).
pub fn profile_path(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) if dir != p => profile_path(dir).join(name),
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
    let cargo_home = cargo_home_of(home.as_deref());
    let tmp = std::fs::canonicalize(std::env::temp_dir()).ok();
    let mut env = Env {
        root: profile_path(root),
        home,
        tmp,
        cargo_home,
        dirs: Vec::new(),
        cargo_dirs: Vec::new(),
        own_build: None,
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
    let found = toolchain_dirs(&Policy::for_project(&env), env.cargo_home.as_deref());
    (env.dirs, env.cargo_dirs, env.own_build) = (found.other, found.cargo, found.own_build);
    // (no folder of its own: no sockets but the project's, `TMPDIR` as it was, and no tag to watch by)
    let run = RunDir::new().ok();
    let tag = run.as_ref().map(|r| r.tag());
    Policy { run, tag, ..Policy::for_project(&env) }
}

/// What the project's toolchain names: Go's caches (`other`), cargo's target and build dirs (`cargo`, real
/// paths, which), and this checkout's own per-checkout build dir (`own_build` — `Env`'s).
#[derive(Clone, Debug, Default)]
pub struct ToolDirs {
    pub other: Vec<PathBuf>,
    pub cargo: Vec<(PathBuf, &'static str)>,
    pub own_build: Option<PathBuf>,
}

/// What `toolchain_dirs` found per project (real path): the stamps of the files that set them, the folders.
type Seen = HashMap<PathBuf, (Vec<Option<SystemTime>>, ToolDirs)>;
static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();

/// The project's toolchain folders outside it as last found for a run of Claude's (`toolchain_dirs`) — only
/// those its runs may write (`cargo_dir_verdict`); nothing looked up: none before its first run.
pub fn build_dirs(root: &Path) -> Vec<PathBuf> {
    let root = profile_path(root);
    let Some(found) =
        SEEN.get().and_then(|s| s.lock().ok().and_then(|m| m.get(&root).map(|(_, d)| d.clone())))
    else {
        return Vec::new();
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_home = cargo_home_of(home.as_deref());
    let tmp = std::fs::canonicalize(std::env::temp_dir()).ok();
    let open = |d: &Path| {
        cargo_dir_verdict(
            d,
            &root,
            home.as_deref(),
            cargo_home.as_deref(),
            found.own_build.as_deref(),
            tmp.as_deref(),
        )
        .is_ok()
    };
    let cargo = found.cargo.iter().map(|(d, _)| d).filter(|d| open(d));
    found.other.iter().chain(cargo).filter(|d| !d.starts_with(&root)).cloned().collect()
}

/// `$CARGO_HOME` (else `~/.cargo`), real — when it exists.
fn cargo_home_of(home: Option<&Path>) -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".cargo")))
        .and_then(|p| std::fs::canonicalize(p).ok())
}

/// Your own `build.build-dir` — `$CARGO_BUILD_BUILD_DIR`, else cargo's home config's — never the
/// checkout's: what its per-checkout build dir is worked out from (#20).
fn own_build_setting(cargo_home: Option<&Path>, var: Option<String>) -> Option<String> {
    if let Some(v) = var.filter(|v| !v.is_empty()) {
        return Some(v);
    }
    ["config.toml", "config"].iter().find_map(|f| {
        let text = std::fs::read_to_string(cargo_home?.join(f)).ok()?;
        let v: toml::Value = toml::from_str(&text).ok()?;
        v.get("build")?.get("build-dir")?.as_str().map(str::to_string)
    })
}

/// Folders the project's toolchain writes outside it: cargo's target and build dirs (`cargo metadata` —
/// they can be anywhere: `CARGO_TARGET_DIR`, `build.build-dir` in a cargo config), Go's caches — and the
/// build dir cargo names with your own setting put over the checkout's (`ToolDirs::own_build`). Kept per
/// project until a file that sets them changes.
fn toolchain_dirs(base: &Policy, cargo_home: Option<&Path>) -> ToolDirs {
    let root = &base.root;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let cargo_config = cargo_home.map_or_else(|| home.join(".cargo"), Path::to_path_buf);
    let stamp_of = [
        root.join("Cargo.toml"),
        root.join(".cargo/config.toml"),
        root.join(".cargo/config"),
        cargo_config.join("config.toml"),
        cargo_config.join("config"),
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
    let mut dirs = ToolDirs::default();
    let metadata = ["metadata", "--no-deps", "--offline", "--format-version", "1"];
    if root.join("Cargo.toml").is_file()
        && let Some(json) = output_in(base, "cargo", &metadata, &[])
    {
        for (key, which) in [("target_directory", "target-dir"), ("build_directory", "build-dir")] {
            if let Some(d) = json[key].as_str() {
                dirs.cargo.push((profile_path(Path::new(d)), which));
            }
        }
        // (your own build-dir over the checkout's — the variable wins over every config file)
        if let Some(own) = own_build_setting(cargo_home, std::env::var("CARGO_BUILD_BUILD_DIR").ok())
            && let Some(json) = output_in(base, "cargo", &metadata, &[("CARGO_BUILD_BUILD_DIR", &own)])
        {
            dirs.own_build = json["build_directory"].as_str().map(|d| profile_path(Path::new(d)));
        }
    }
    if root.join("go.mod").is_file()
        && let Some(json) = output_in(base, "go", &["env", "-json", "GOCACHE", "GOMODCACHE"], &[])
    {
        for key in ["GOCACHE", "GOMODCACHE"] {
            if let Some(d) = json[key].as_str().filter(|d| !d.is_empty()) {
                dirs.other.push(profile_path(Path::new(d)));
            }
        }
    }
    if let Ok(mut m) = seen.lock() {
        m.insert(root.clone(), (stamp, dirs.clone()));
    }
    dirs
}

/// A program's JSON output, run in the project inside `policy`, with `env` set.
fn output_in(
    policy: &Policy,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Option<serde_json::Value> {
    let out = Command::new(SANDBOX_EXEC)
        .args(policy.args())
        .arg(program)
        .args(args)
        .envs(env.iter().copied())
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
/// (macOS only, as the Seatbelt tests that ask — elsewhere it'd be dead code, an error under `-D warnings`)
#[cfg(all(test, target_os = "macos"))]
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
            dirs: Vec::new(),
            cargo_dirs: vec![
                ("/Users/u/.cargo/build/ab/cd12".into(), "build-dir"),
                (root.join("target"), "target-dir"),
            ],
            own_build: Some("/Users/u/.cargo/build/ab/cd12".into()),
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
                profile_path(Path::new("/private/tmp")).join("tmux-501"),
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
        assert_eq!(p.protect, [root.join("secrets")], "only paths inside the project");
        // Cargo's caches stay as they are while the network is closed — writable with it open
        let caches = [PathBuf::from("/Users/u/.cargo/registry"), "/Users/u/.cargo/git".into()];
        assert_eq!(p.caches, caches);
        let w: Vec<&str> = p.write.iter().map(|w| w.to_str().unwrap()).collect();
        assert_eq!(
            w,
            [
                "/private/tmp",
                "/private/var/folders/ab/cd",
                "/Users/u/.cargo/build/ab/cd12",
                "/Users/u/Library/Caches/thing",
            ]
        );
        let open = Policy::for_project(&Env { network: true, ..env(root) });
        assert!(open.caches.is_empty(), "{:?}", open.caches);
        assert!(caches.iter().all(|c| open.write.contains(c)), "{:?}", open.write);
        for p in [&p, &open] {
            assert!(p.files.contains(&PathBuf::from("/Users/u/.cargo/.package-cache")));
            assert!(p.files.contains(&PathBuf::from("/Users/u/.cargo/.global-cache-wal")));
            assert!(!p.write.iter().chain(&p.files).any(|w| w.starts_with("/Users/u/.cargo/bin")));
        }
    }

    /// #20: a cargo config in the checkout names the build or target dir, and Claude can write it: only the
    /// checkout's own (inside it, or its per-checkout build dir under cargo's home) or a temp folder becomes
    /// writable — never home or above it, a folder holding the checkout, a link to home, cargo's home, another
    /// checkout's build dir, or a folder beside them (`~/.config/fish`). (Folders of the test's own.)
    #[test]
    fn a_cargo_dir_from_the_checkout_opens_only_its_own() {
        let base = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-dirs-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let (home, root) = (base.join("home"), base.join("work/proj"));
        let cargo_home = home.join(".cargo");
        for d in [
            &root,
            &home.join(".config/fish"),
            &cargo_home.join("build/ab/cd1234"),
            &cargo_home.join("build/ab/ffff"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(&home, root.join("homelink")).unwrap();
        let own = cargo_home.join("build/ab/cd1234");
        let bad = [
            home.clone(),
            PathBuf::from("/"),
            base.join("work"),
            profile_path(&root.join("homelink")),
            home.join(".config/fish"),
            cargo_home.clone(),
            cargo_home.join("build/ab/ffff"),
        ];
        let named: Vec<(PathBuf, &str)> =
            bad.iter().cloned().chain([root.join("target"), own.clone()]).map(|d| (d, "build-dir")).collect();
        let env = Env {
            root: root.clone(),
            home: Some(home.clone()),
            cargo_home: Some(cargo_home.clone()),
            cargo_dirs: named,
            own_build: Some(own.clone()),
            ..Env::default()
        };
        let p = Policy::for_project(&env);
        for d in &bad {
            assert!(!p.write.iter().any(|w| w == d), "{} became writable: {:?}", d.display(), p.write);
            assert!(p.refused.iter().any(|(r, _)| r == d), "{} not said", d.display());
        }
        assert!(p.write.contains(&own), "its own per-checkout build dir: {:?}", p.write);
        assert!(!p.refused.iter().any(|(r, _)| *r == root.join("target") || *r == own), "{:?}", p.refused);
        // Said where Claude reads it, and on the panel
        let g = Guard::In(Box::new(p));
        let said = g.report(&[]);
        assert!(said.contains(&format!("{} (cargo's build-dir", home.display())), "{said}");
        let (line, _) = panel_line(Some(&g), false, std::time::Instant::now()).expect("the panel says it");
        assert!(
            line.starts_with("sandbox kept ") && line.contains(" closed — cargo's config named it"),
            "{line}"
        );
        // Its own build dir is worked out from your setting — the variable, else cargo's home config — never
        // the checkout's
        std::fs::write(
            cargo_home.join("config.toml"),
            "[build]\nbuild-dir = \"{cargo-cache-home}/build/{workspace-path-hash}\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(root.join(".cargo/config.toml"), "[build]\nbuild-dir = \"/\"\n").unwrap();
        let at = |var: Option<&str>| own_build_setting(Some(&cargo_home), var.map(String::from));
        assert_eq!(at(None).as_deref(), Some("{cargo-cache-home}/build/{workspace-path-hash}"));
        assert_eq!(at(Some("/x/{workspace-path-hash}")).as_deref(), Some("/x/{workspace-path-hash}"));
        std::fs::remove_file(cargo_home.join("config.toml")).unwrap();
        assert_eq!(at(None), None, "none of yours: no per-checkout dir to trust");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Paths are parameters, never text in the profile — a path can't change what it says.
    #[test]
    fn the_profile_names_paths_only_through_parameters() {
        let root = Path::new("/Users/u/a\"b (c)");
        let p = Policy::for_project(&env(root));
        let text = p.profile();
        assert!(!text.contains("/Users/u"), "{text}");
        assert!(text.contains("(require-not (subpath (param \"P0\")))"));
        assert!(text.contains("(subpath (param \"W3\"))") && !text.contains("W4"));
        assert!(text.contains("(deny network*)") && text.contains("(deny appleevent-send)"));
        // The caches kept after the allows, and cargo's own files in them let through after that
        let kept = "(deny file-write* (subpath (param \"K0\")) (subpath (param \"K1\")) (subpath (param \"C0\")) \
                    (subpath (param \"C1\")))";
        let files = "(allow file-write* (literal (param \"F0\"))";
        assert!(text.find(kept).is_some_and(|k| text.find(files).is_some_and(|f| f > k)), "{text}");
        assert!(
            text.find("(allow file-write* (subpath (param \"W0\"))")
                .is_some_and(|w| w < text.find(kept).unwrap())
        );
        let params = p.params();
        assert_eq!(params[0], ("ROOT".into(), "/Users/u/a\"b (c)".into()));
        assert!(params.iter().any(|(k, v)| k == "P0" && v.ends_with("/secrets")));
        assert!(params.contains(&("C0".into(), "/Users/u/.cargo/registry".into())));
        assert!(params.contains(&("C1".into(), "/Users/u/.cargo/git".into())));
        let open = Policy::for_project(&Env { network: true, ..env(root) });
        let text = open.profile();
        assert!(!text.contains("(param \"C0\")"), "{text}");
        assert!(!text.contains("(deny network*)"), "{text}");
        assert!(
            text.contains("(deny network-outbound (remote unix-socket))"),
            "sockets stay guarded: {text}"
        );
        assert!(!text.contains("RUN"), "no folder of its own: only the project's sockets");
        assert!(text.contains("(param \"K0\")"), "the socket folders stay kept, the network open or not");
        // #119: a granted socket is that file — a literal, connecting only, after the sockets' deny
        assert!(!text.contains("S0"));
        let sock = Policy { sockets: vec!["/Users/u/.colima/t/docker.sock".into()], ..p.clone() };
        let text = sock.profile();
        let rule = "(allow network-outbound (require-all (remote unix-socket) (require-any (literal (param \"S0\")))))";
        let deny = text.find("(deny network-outbound (remote unix-socket))").unwrap();
        assert!(text.find(rule).is_some_and(|at| at > deny), "{text}");
        assert!(
            !text.contains("docker.sock")
                && !text.contains("S0\")) (")
                && !text.contains("subpath (param \"S")
        );
        assert!(sock.params().contains(&("S0".into(), "/Users/u/.colima/t/docker.sock".into())));
    }

    /// The names a run keeps off (card model P1), in Rust as in the profile: anywhere below the root, in any
    /// case, but not below a tool's output folder (unless the name comes first: `.git/modules/target`); the
    /// root's own path doesn't count (here it holds `.claude` and `target`). In the profile: names, never
    /// the path — a regex that skips as many parts as the root has.
    #[test]
    fn what_a_run_keeps_off_by_name() {
        let root = Path::new("/Users/u/.claude/worktrees/target/p");
        let kept = |rel: &str| protected_in(root, &root.join(rel));
        for rel in [
            "build.rs",
            "crates/a/build.rs",
            "BUILD.RS",
            "Cargo.toml",
            "web/package.json",
            "Makefile",
            "justfile",
            ".envrc",
            ".tarae.toml",
            ".tarae.local.toml",
            ".cargo/config.toml",
            "crates/a/.cargo",
            ".github/workflows/ci.yml",
            ".claude/settings.json",
            ".vscode/tasks.json",
            ".git",
            ".git/hooks/pre-commit",
            ".git/modules/target/hooks/pre-commit",
        ] {
            assert!(kept(rel), "{rel}");
        }
        for rel in [
            "",
            "src/main.rs",
            "build.rs.bak",
            "Cargo.lock",
            ".gitignore",
            "target/debug/build/x/out/Makefile",
            "target/package/a-0.1.0/Cargo.toml",
            "node_modules/a/package.json",
            "web/node_modules/a/package.json",
        ] {
            assert!(!kept(rel), "{rel}");
        }
        assert!(!protected_in(root, Path::new("/Users/u/elsewhere/build.rs")));
        let text = Policy::for_project(&env(root)).profile();
        assert!(!text.contains("/Users") && !text.contains("worktrees"), "{text}");
        let skip = "/[^/]+".repeat(6);
        let rule = format!(
            "(deny file-write* (require-all (subpath (param \"ROOT\")) (regex #\"^{skip}/(.*/)?\
             ([bB][uU][iI][lL][dD]\\.[rR][sS]|[cC][aA][rR][gG][oO]\\.[tT][oO][mM][lL]|"
        );
        assert!(text.contains(&rule), "{text}");
        assert!(
            text.contains(&format!(
                "|\\.[gG][iI][tT])(/|$)\") (require-not (regex #\"^{skip}/(.*/)?(target|node_modules)/(.*/)?("
            )),
            "{text}"
        );
        // Every name is plain enough for a regex in quotes (`any_case` escapes the rest)
        for n in protected_names() {
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)), "{n}");
        }
    }

    /// What Claude reads when the profile kept something on purpose: which names, and to use Edit (the user
    /// reviews it); git's folder, and that git's writes are the user's; cargo's unpacked crates, and why —
    /// with nothing else blocked, no "tell the user to open it" (they can't).
    #[test]
    fn the_report_says_what_was_kept_and_what_to_do_instead() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let w = Watch::quiet("t.");
        w.record(denial("file-write-data", "/Users/u/p/build.rs"));
        w.record(denial("file-write-create", "/Users/u/p/crates/a/.cargo"));
        w.record(denial("forbidden-link-priv<file-write*>", "/Users/u/p/build.rs /Users/u/p/out/b"));
        let g = Guard::In(Box::new(Policy { watch: Some(w.clone()), ..p.clone() }));
        let r = g.report(&[]);
        assert!(
            r.contains("build.rs, crates/a/.cargo: tarae keeps them from your commands")
                && r.contains("To change them, use Edit: the user reviews it."),
            "{r}"
        );
        assert!(r.contains("  linking /Users/u/p/build.rs (forbidden-link-priv<file-write*>, touch)"), "{r}");
        assert!(
            r.contains("not build.rs, Cargo.toml, package.json")
                && r.contains(".tarae.local.toml, .git, anywhere in it (Edit changes those)")
                && r.contains(
                    "(not cargo's caches — its crates and git checkouts — while the network is closed)"
                ),
            "{r}"
        );
        assert!(!r.contains("If the command needs one of these"), "all of it kept on purpose: {r}");
        w.record(denial("file-write-create", "/Users/u/p/.git/index.lock"));
        let r = g.report(&[]);
        assert!(r.contains("Git's own folder is kept from your commands"), "{r}");
        assert!(!r.contains("Cargo's caches"), "{r}");
        // Each of cargo's caches: what's kept, why, and that it's the user's to put there
        for (i, at) in [
            "/Users/u/.cargo/registry/src/i/serde-1.0.0/build.rs",
            "/Users/u/.cargo/registry/cache/i/serde-1.0.0.crate",
            "/Users/u/.cargo/registry/index/i/.cache/se/rd/serde",
            "/Users/u/.cargo/git/db/dep-1234/HEAD",
            "/Users/u/.cargo/git/checkouts/dep-1234/abcdef/.cargo-ok",
        ]
        .iter()
        .enumerate()
        {
            let w = Watch::quiet(&format!("t{i}."));
            w.record(denial("file-write-create", at));
            let r = Guard::In(Box::new(Policy { watch: Some(w), ..p.clone() })).report(&[]);
            assert!(
                r.contains(
                    "Cargo's caches (/Users/u/.cargo/registry, /Users/u/.cargo/git) stay as they are while the \
                     network is closed"
                ) && r.contains("tell the user. A build or `cargo fetch` of theirs puts it there"),
                "{at}: {r}"
            );
            assert!(!r.contains("If the command needs one of these"), "{at}: {r}");
        }
        // The kernel's report of the denial didn't come: cargo's own error says it, and that's what's said
        let cargo_said: Vec<(bool, String)> = [
            "error: failed to download `serde v1.0.0`",
            "  failed to create directory `/Users/u/.cargo/registry/src/i/serde-1.0.0`",
            "Caused by:",
            "  Operation not permitted (os error 1)",
        ]
        .iter()
        .map(|l| (true, l.to_string()))
        .collect();
        let unreported = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("u.")), ..p.clone() }));
        let r = unreported.report(&cargo_said);
        assert!(
            r.starts_with("The sandbox's own report of what it blocked didn't come, but the output says a write \
                           in cargo's caches was refused:\n  failed to create directory `/Users/u/.cargo/registry/src/i/serde-1.0.0`\n")
                && r.contains("Cargo's caches (/Users/u/.cargo/registry, /Users/u/.cargo/git) stay as they are")
                && !r.contains("blocked nothing"),
            "{r}"
        );
        // …only when the output names a path in the caches as what failed, and says a write was refused
        assert!(unreported.report(&cargo_said[..2]).starts_with("The sandbox blocked nothing."));
        let elsewhere: Vec<(bool, String)> =
            vec![(true, "failed: /Users/u/other/x".into()), (true, "Operation not permitted".into())];
        assert!(unreported.report(&elsewhere).starts_with("The sandbox blocked nothing."));
        let listed = vec![(false, "/Users/u/.cargo/registry/src/i/a.rs: fn x".into()), cargo_said[3].clone()];
        assert!(unreported.report(&listed).starts_with("The sandbox blocked nothing."), "a grep's hit");
        w.record(denial("file-write-create", "/Users/u/marker"));
        assert!(g.report(&[]).contains("If the command needs one of these"), "something else blocked");
        // The network open: the caches aren't kept, and nothing says they are
        let open = Policy::for_project(&Env { network: true, ..env(Path::new("/Users/u/p")) });
        let open = Guard::In(Box::new(Policy { watch: Some(w), ..open }));
        assert!(!open.report(&[]).contains("Cargo's caches") && !open.report(&[]).contains("cargo's caches"));
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
        let g = Guard::In(Box::new(Policy { watch: Some(w), ..p.clone() }));
        let r = g.report(&[]);
        assert!(r.starts_with("The sandbox blocked 2 things — 3 times:\n"), "{r}");
        // Plain words first, Seatbelt's after
        assert!(r.contains("  writing ~/marker (file-write-create, touch)\n"), "{r}");
        assert!(r.contains("  connecting to port 9 (network-outbound, touch)\n"), "{r}");
        assert!(r.contains("In tarae's sandbox") && r.contains("don't work around it"), "{r}");
        let none = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("t.")), ..p.clone() })).report(&[]);
        assert!(
            none.starts_with("The sandbox blocked nothing.") && !none.contains("likely the sandbox"),
            "{none}"
        );
        let failed = Watch::quiet("t.");
        failed.fail("log: Cannot run while sandboxed".into());
        let out = [(true, "touch: /x: Operation not permitted".to_string()), (false, "fine".to_string())];
        let r = Guard::In(Box::new(Policy { watch: Some(failed), ..p })).report(&out);
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
        // A link refused for its source (measured: the source, then the link)
        assert_eq!(
            plain("forbidden-link-priv<file-write*>", &format!("{home}/.ssh/k /p/k")),
            "linking ~/.ssh/k"
        );
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
        let g = Guard::In(Box::new(Policy { watch: Some(w.clone()), ..p }));
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
        let (root, out) = (profile_path(&root), profile_path(&out));
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
            caches: Vec::new(),
            network: false,
            ports: vec![port],
            localhost: false,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
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
        let t = profile_path(&std::env::temp_dir()).join(format!("tarae-sock-{pid}"));
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
                write: vec!["/private/tmp".into(), user_temp(&profile_path(&std::env::temp_dir()))],
                files: Vec::new(),
                keep: Vec::new(),
                caches: Vec::new(),
                network,
                ports: Vec::new(),
                localhost: false,
                run: Some(run.clone()),
                tag: None,
                watch: None,
                granted: Vec::new(),
                sockets: Vec::new(),
                refused: Vec::new(),
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
        profile_path(&t)
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
            caches: Vec::new(),
            network: false,
            ports: Vec::new(),
            localhost: false,
            run: None,
            tag: Some(tag.to_string()),
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
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
            caches: Vec::new(),
            network: false,
            ports: Vec::new(),
            localhost: false,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
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
        let t = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-granted-{}", std::process::id()));
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
        // #119: a socket granted for this conversation is reached — that file only, not its sibling
        // (a socket's path has 104 bytes at most: short names)
        let socks = t.join("s");
        std::fs::create_dir_all(&socks).unwrap();
        let sock = std::os::unix::net::UnixListener::bind(socks.join("granted.sock")).unwrap();
        let sock_other = std::os::unix::net::UnixListener::bind(socks.join("other.sock")).unwrap();
        for l in [&sock, &sock_other] {
            l.set_nonblocking(true).unwrap();
        }
        let mut this_chat = Allowed::default();
        this_chat.add(&Grant::Write(chat.clone()));
        this_chat.add(&Grant::Socket(socks.join("granted.sock")));
        let (llm, said, sockets) =
            crate::tools::with_allowed(&crate::llm::LlmConfig::default(), &[&from_store, &this_chat], None);
        assert_eq!(said.len(), 4, "{said:?}");
        let env = Env {
            root: root.clone(),
            write: llm.sandbox_write.clone(),
            ports: llm.sandbox_ports,
            ..Env::default()
        };
        let p = Policy { sockets, ..Policy::for_project(&env) };
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
        let knocked = |l: &std::os::unix::net::UnixListener, name: &str| {
            sh(&format!("/usr/bin/nc -U -w 2 '{}' < /dev/null", socks.join(name).display()));
            l.accept().is_ok()
        };
        assert!(knocked(&sock, "granted.sock"), "the granted socket");
        assert!(!knocked(&sock_other, "other.sock"), "the socket beside it");
        std::fs::remove_dir_all(&t).ok();
    }

    /// tarae's UI state socket (`uisocket.rs`, `$state/ui/<pid>.sock`) can't be reached from Claude's runs:
    /// under the real profile — the network closed or open — `nc -U` doesn't get through, while the same
    /// knock from outside the sandbox does. Harmless: the socket is this test's own, nothing is sent.
    #[cfg(target_os = "macos")]
    #[test]
    fn claudes_runs_cant_reach_the_ui_state_socket() {
        if skipped_inside("sandbox::tests::claudes_runs_cant_reach_the_ui_state_socket") {
            return;
        }
        let t = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-ui-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let root = t.join("project");
        std::fs::create_dir_all(&root).unwrap();
        // (as tarae binds it: the state folder's `ui/`, 0700, the socket 0600)
        let ui = t.join("state/tarae/ui");
        let listener = crate::uisocket::bind_in(&ui, 4242).unwrap();
        listener.set_nonblocking(true).unwrap();
        let sock = ui.join("4242.sock");
        let knock = |p: Option<&Policy>| {
            let script = format!("/usr/bin/nc -U -w 2 '{}' < /dev/null", sock.display());
            let mut c = command(p, "/bin/sh", &["-c".into(), script]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap();
            listener.accept().is_ok()
        };
        assert!(knock(None), "from outside the sandbox: reached (the knock works)");
        let env = Env { root: root.clone(), ..Env::default() };
        let closed = Policy::for_project(&env);
        assert!(!knock(Some(&closed)), "a run of Claude's (network closed) reached the UI state");
        let open = Policy { network: true, ..Policy::for_project(&env) };
        assert!(!knock(Some(&open)), "a run of Claude's (network open) reached the UI state");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS (card model P1) — what runs later outside the sandbox stays as it is: every name
    /// `protected_names` gives, anywhere in the project and in any case, can't be made (a file, a folder),
    /// written, appended to, moved onto, moved or removed, nor given a second name to write through; the
    /// files beside them are written as before, and so are those names inside tools' output (`OUTPUTS`); a
    /// checkout whose own path holds such names (`.claude/worktrees`, `target`) stays writable. A file from
    /// outside (a stand-in key in a stand-in home) can't be linked into the project; the project's own files
    /// can (cargo links its outputs). Claude reads what was kept and to use Edit, which asks the user
    /// (`edits::listed`). Harmless: plain-text stand-ins in a folder of the test's own under `target/`.
    #[cfg(target_os = "macos")]
    #[test]
    fn what_runs_later_stays_as_it_is_under_the_real_profile() {
        if skipped_inside("sandbox::tests::what_runs_later_stays_as_it_is_under_the_real_profile") {
            return;
        }
        // (not `sb-kept`: editor.rs' sandbox test removes that folder when it ends)
        let t = scratch("sb-names");
        let (root, home, out) = (t.join(".claude/worktrees/p"), t.join("home"), t.join("out"));
        for d in [&root, &home.join(".ssh"), &out] {
            std::fs::create_dir_all(d).unwrap();
        }
        let key = home.join(".ssh/id_stand_in");
        std::fs::write(&key, "not a key\n").unwrap();
        let p = Policy {
            write: vec![out.clone()],
            ..Policy::for_project(&Env { root: root.clone(), ..Env::default() })
        };
        let sh = |p: &Policy, script: &str| {
            let mut c = command(Some(p), "/bin/sh", &["-c".into(), script.into()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap()
        };
        let o = out.display();
        for name in protected_names() {
            std::fs::write(root.join(name), "kept\n").unwrap();
            let upper = name.to_uppercase();
            let ran = sh(
                &p,
                &format!(
                    "mkdir -p a b c target/x node_modules/p; echo x > 'a/{name}'; mkdir 'b/{name}'; \
                     echo x > 'b/{name}/x'; echo x > 'c/{upper}'; echo pwned >> '{name}'; mv '{name}' 'moved{name}'; \
                     echo evil > '{o}/evil{name}'; mv '{o}/evil{name}' '{name}'; \
                     ln '{name}' '{o}/second{name}' && echo pwned >> '{o}/second{name}'; rm -f '{name}'; \
                     echo x > 'a/{name}.bak'; echo x > 'target/x/{name}'; echo x > 'node_modules/p/{name}'"
                ),
            );
            let said = String::from_utf8_lossy(&ran.stderr);
            for made in
                [format!("a/{name}"), format!("b/{name}"), format!("c/{upper}"), format!("moved{name}")]
            {
                assert!(!root.join(&made).exists(), "{made}: made — {said}");
            }
            assert_eq!(
                std::fs::read_to_string(root.join(name)).unwrap(),
                "kept\n",
                "{name}: changed — {said}"
            );
            assert!(!out.join(format!("second{name}")).exists(), "{name}: linked to a second name — {said}");
            for beside in
                [format!("a/{name}.bak"), format!("target/x/{name}"), format!("node_modules/p/{name}")]
            {
                assert!(root.join(&beside).exists(), "{beside}: not written — {said}");
            }
            // An Edit of it asks the user — where the report sends Claude
            let defaults: Vec<String> = crate::llm::REVIEW_ASK.iter().map(|s| s.to_string()).collect();
            assert!(crate::edits::listed(Path::new(name), &defaults), "{name}: an Edit wouldn't ask");
        }
        // Links: nothing from outside comes in; the project's own files link as before
        sh(&p, &format!("ln '{}' stolen; echo x > own; ln own own2", key.display()));
        assert!(!root.join("stolen").exists(), "a file from outside linked into the project");
        assert!(root.join("own2").exists(), "a link of the project's own file");
        // What Claude reads: what was kept, and to use Edit
        let tag = format!("tarae-test-{}-kept.", std::process::id());
        let w = Watch::start(&tag);
        std::thread::sleep(WATCH_READY);
        let watched = Policy { tag: Some(tag), watch: Some(w.clone()), ..p.clone() };
        std::fs::write(root.join("build.rs"), "kept\n").unwrap();
        sh(&watched, "echo pwned >> build.rs; mkdir -p d/.git");
        std::thread::sleep(WATCH_LAST * 2);
        w.finish();
        assert_eq!(w.blocked().failed, None);
        let r = Guard::In(Box::new(watched)).report(&[]);
        assert!(r.contains("build.rs: tarae keeps it from your commands") && r.contains("use Edit"), "{r}");
        assert!(r.contains("Git's own folder is kept"), "{r}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A folder's files and folders below it, each with its size, change time and inode — what a run wrote
    /// in it shows as a difference.
    #[cfg(target_os = "macos")]
    fn listing(dir: &Path) -> std::collections::BTreeMap<PathBuf, (u64, i64, i64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let mut out = std::collections::BTreeMap::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let m = e.path().symlink_metadata().unwrap();
                if m.is_dir() {
                    todo.push(e.path());
                }
                out.insert(e.path(), (m.len(), m.mtime(), m.mtime_nsec(), m.ino()));
            }
        }
        out
    }

    /// For real, on macOS (card model P1b) — cargo's caches under the real profile. The network closed:
    /// nothing under `registry/` (index, cache, src) or `git/` (db, checkouts) can be made, changed or
    /// removed; a crate downloaded but never unpacked, a git dependency never checked out, can't be — the build
    /// fails, and Claude reads why and that the user puts it there. The network open, all of it is written
    /// as before. And a project with a registry and a git dependency, a build script and a test still
    /// builds, tests and formats offline once they're in place, writing in cargo's home only its lock and
    /// last-use files (`CARGO_FILES`) and trying nothing it's denied. Harmless and offline: cargo's home, a
    /// registry (a git index of one crate, its `.crate` fetched over `file://`), a git dependency and the
    /// project are folders of the test's own under `target/` (never the real `~/.cargo`); the caches' writes
    /// are tried on marker files the test planted.
    #[cfg(target_os = "macos")]
    #[test]
    fn cargo_works_offline_and_its_caches_stay_under_the_real_profile() {
        if skipped_inside("sandbox::tests::cargo_works_offline_and_its_caches_stay_under_the_real_profile") {
            return;
        }
        let t = scratch("sb-cargo");
        let (home, index, dl, gdep, root) =
            (t.join("cargo"), t.join("index"), t.join("dl"), t.join("gdep"), t.join("p"));
        let write = |p: &Path, text: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        // git in the test's own repositories only (never tarae's), with no config or hooks of yours
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(["-c", "core.hooksPath=/dev/null", "-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(dir)
                .env("GIT_DIR", dir.join(".git"))
                .env("GIT_WORK_TREE", dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let commit = |dir: &Path| {
            git(dir, &["init", "-q"]);
            git(dir, &["add", "-A"]);
            git(dir, &["commit", "-qm", "one"]);
        };
        // A registry with one crate: a git index (its line, and `dl` naming where the .crate is) and the
        // .crate, a gzipped tar
        let stage = t.join("stage/tarae-dep-0.1.0");
        write(
            &stage.join("Cargo.toml"),
            "[package]\nname = \"tarae-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&stage.join("src/lib.rs"), "pub fn one() -> u8 {\n    1\n}\n");
        std::fs::create_dir_all(&dl).unwrap();
        let krate = dl.join("tarae-dep-0.1.0.crate");
        let tar = Command::new("/usr/bin/tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&krate)
            .arg("-C")
            .arg(t.join("stage"))
            .arg("tarae-dep-0.1.0")
            .status()
            .unwrap();
        assert!(tar.success());
        let sum = Command::new("/usr/bin/shasum").args(["-a", "256"]).arg(&krate).output().unwrap();
        let sum = String::from_utf8_lossy(&sum.stdout).split_whitespace().next().unwrap().to_string();
        write(
            &index.join("ta/ra/tarae-dep"),
            &format!(
                "{{\"name\":\"tarae-dep\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"
            ),
        );
        write(
            &index.join("config.json"),
            &format!("{{\"dl\":\"file://{}/{{crate}}-{{version}}.crate\"}}\n", dl.display()),
        );
        commit(&index);
        write(
            &home.join("config.toml"),
            &format!(
                "[source.crates-io]\nreplace-with = \"here\"\n[source.here]\nregistry = \"file://{}\"\n",
                index.display()
            ),
        );
        // A git dependency
        write(
            &gdep.join("Cargo.toml"),
            "[package]\nname = \"gdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&gdep.join("src/lib.rs"), "pub fn two() -> u8 {\n    2\n}\n");
        commit(&gdep);
        // The project: both dependencies, a build script, a test, a file `cargo fmt` changes
        write(
            &root.join("Cargo.toml"),
            &format!(
                "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n\
                 tarae-dep = \"0.1\"\ngdep = {{ git = \"file://{}\" }}\n\n[workspace]\n",
                gdep.display()
            ),
        );
        write(
            &root.join("build.rs"),
            "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
             std::fs::write(format!(\"{out}/made\"), \"x\").unwrap();\n}\n",
        );
        let unformatted =
            "pub fn f()->u8{tarae_dep::one()+gdep::two()}\n#[test]\nfn t(){assert_eq!(f(),3);}\n";
        write(&root.join("src/lib.rs"), unformatted);
        let env = Env {
            root: root.clone(),
            cargo_home: Some(home.clone()),
            tmp: Some(profile_path(&std::env::temp_dir())),
            ..Env::default()
        };
        let (closed, open) = (Policy::for_project(&env), Policy::for_project(&Env { network: true, ..env }));
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let run = |p: &Policy, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let mut c = command(Some(p), &cargo, &args);
            c.current_dir(&root)
                .env("CARGO_HOME", &home)
                .env_remove("CARGO_NET_OFFLINE")
                .env_remove("CARGO_TARGET_DIR")
                // (cargo also reads the cargo configs above this folder — yours, whose `build-dir` would put
                // the build under this cargo home: a run's real policy asks `cargo metadata` for that dir,
                // this one doesn't, so the build stays in the project)
                .env("CARGO_BUILD_BUILD_DIR", root.join("target"))
                .env_remove("RUSTC_WRAPPER")
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let said = |r: &std::process::Output| String::from_utf8_lossy(&r.stderr).into_owned();
        // A run of Claude's, watched: what it printed, and what Claude reads
        let watched = |name: &str, args: &[&str]| {
            let tag = format!("tarae-test-{}-{name}.", std::process::id());
            let w = Watch::start(&tag);
            std::thread::sleep(WATCH_READY);
            let p = Policy { tag: Some(tag), watch: Some(w.clone()), ..closed.clone() };
            let r = run(&p, args);
            std::thread::sleep(WATCH_LAST * 2);
            w.finish();
            assert_eq!(w.blocked().failed, None);
            let output: Vec<(bool, String)> = [(false, &r.stdout), (true, &r.stderr)]
                .into_iter()
                .flat_map(|(err, o)| {
                    String::from_utf8_lossy(o).lines().map(|l| (err, l.to_string())).collect::<Vec<_>>()
                })
                .collect();
            let report = Guard::In(Box::new(p)).report(&output);
            (r, w.blocked(), report)
        };
        // The one folder below `dir` (cargo names them by a hash)
        let only = |dir: &Path| {
            let all: Vec<PathBuf> =
                std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
            assert_eq!(all.len(), 1, "{}: {all:?}", dir.display());
            all[0].clone()
        };
        // The network open: the registry's index fetched, the crate downloaded and unpacked, the git
        // dependency fetched and checked out — as before
        let r = run(&open, &["build"]);
        assert!(r.status.success(), "{}", said(&r));
        let src = only(&home.join("registry/src")).join("tarae-dep-0.1.0");
        let checkout = only(&only(&home.join("git/checkouts")));
        assert!(src.join("src/lib.rs").exists() && checkout.join("src/lib.rs").exists());
        // The network closed, all of it in place: a build, its tests and fmt work offline — writing in cargo's
        // home only its own files, denied nothing
        std::fs::remove_dir_all(root.join("target")).unwrap();
        // (the caches' backup tags gone, as in a cargo home an older cargo made: written again)
        let tags = [home.join("registry/CACHEDIR.TAG"), home.join("git/CACHEDIR.TAG")];
        for tag in &tags {
            std::fs::remove_file(tag).unwrap();
        }
        let before = listing(&home);
        for (i, args) in
            [&["build", "--offline"][..], &["test", "--offline"], &["fmt"]].into_iter().enumerate()
        {
            let (r, blocked, _) = watched(&format!("warm{i}"), args);
            assert!(r.status.success(), "cargo {args:?}: {}", said(&r));
            assert_eq!(blocked.total, 0, "cargo {args:?} was denied: {:?}", blocked.seen);
        }
        assert_ne!(std::fs::read_to_string(root.join("src/lib.rs")).unwrap(), unformatted, "fmt wrote");
        let after = listing(&home);
        let wrote: Vec<&PathBuf> = before
            .keys()
            .chain(after.keys())
            .filter(|p| before.get(*p) != after.get(*p))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        eprintln!("cargo's home, written by an offline build, test and fmt: {wrote:?}");
        // (a folder's own time moves when a file is made in it)
        let own =
            |p: &PathBuf| closed.files.contains(p) || closed.files.iter().any(|f| f.parent() == Some(p));
        assert!(wrote.iter().all(|p| own(p)), "beyond cargo's own files: {wrote:?}");
        assert!(tags.iter().all(|t| t.exists()), "the backup tags not written again");
        // Downloaded, never unpacked — the network closed, it can't be: the build fails, and Claude reads why
        // and what to do
        std::fs::remove_dir_all(root.join("target")).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
        let (r, blocked, report) = watched("cold-registry", &["build", "--offline"]);
        assert!(!r.status.success() && !src.exists(), "unpacked offline: {}", said(&r));
        // (the kernel's report of the denial doesn't always come — the report then goes by cargo's error)
        let under = |d: &str| home.join(d).display().to_string();
        let denied = |b: &Blocked, d: &str| {
            eprintln!("{d}: the kernel reported {} denials", b.total);
            b.seen.iter().all(|s| s.target.starts_with(&under(d)))
        };
        assert!(denied(&blocked, "registry/src"), "{:?} — {}", blocked.seen, said(&r));
        for words in
            ["registry/src/", "Cargo's caches (", "tell the user. A build or `cargo fetch` of theirs"]
        {
            assert!(report.contains(words), "{words}: {report}");
        }
        assert!(!report.contains("If the command needs one of these"), "{report}");
        assert!(!report.contains("blocked nothing"), "{report}");
        let r = run(&open, &["build", "--offline"]);
        assert!(r.status.success() && src.exists(), "unpacked with the network open: {}", said(&r));
        // A git dependency fetched, never checked out — the same
        std::fs::remove_dir_all(root.join("target")).unwrap();
        std::fs::remove_dir_all(&checkout).unwrap();
        let (r, blocked, report) = watched("cold-git", &["build", "--offline"]);
        assert!(!r.status.success() && !checkout.exists(), "checked out offline: {}", said(&r));
        assert!(denied(&blocked, "git/checkouts"), "{:?} — {}", blocked.seen, said(&r));
        for words in
            ["git/checkouts/", "Cargo's caches (", "tell the user. A build or `cargo fetch` of theirs"]
        {
            assert!(report.contains(words), "{words}: {report}");
        }
        let r = run(&open, &["build", "--offline"]);
        assert!(r.status.success() && checkout.exists(), "checked out with the network open: {}", said(&r));
        // Nothing in the caches can be made, changed or removed with the network closed; the network open, it can
        let dirs = [
            home.join("registry/index"),
            home.join("registry/cache"),
            home.join("registry/src"),
            src.clone(),
            home.join("git/db"),
            home.join("git/checkouts"),
            checkout.clone(),
            // (the caches themselves, where cargo's one mark is let through — no other)
            home.join("registry"),
            home.join("git"),
        ];
        let tries = |p: &Policy| {
            for d in &dirs {
                write(&d.join("planted"), "planted\n");
                let d = d.display();
                let script = format!(
                    "echo x > '{d}/made'; mkdir '{d}/folder'; echo more >> '{d}/planted'; \
                     mv '{d}/planted' '{d}/moved' || rm '{d}/planted'; /usr/bin/xattr -w tarae.marker x '{d}'"
                );
                let mut c = command(Some(p), "/bin/sh", &["-c".into(), script]);
                c.current_dir(&root).stdin(Stdio::null()).output().unwrap();
            }
        };
        let marked = |d: &Path| {
            let out = Command::new("/usr/bin/xattr").args(["-p", "tarae.marker"]).arg(d).output().unwrap();
            out.status.success()
        };
        tries(&closed);
        for d in &dirs {
            let at = d.display();
            assert!(!d.join("made").exists() && !d.join("folder").exists(), "{at}: made offline");
            assert!(!marked(d), "{at}: an attribute set offline");
            assert_eq!(
                std::fs::read_to_string(d.join("planted")).unwrap(),
                "planted\n",
                "{at}: changed offline"
            );
            assert!(!d.join("moved").exists(), "{at}: moved offline");
            std::fs::remove_file(d.join("planted")).unwrap();
        }
        tries(&open);
        for d in &dirs {
            let at = d.display();
            assert!(d.join("made").exists() && d.join("folder").is_dir(), "{at}: not made, network open");
            assert_eq!(std::fs::read_to_string(d.join("moved")).unwrap(), "planted\nmore\n", "{at}");
            assert!(!d.join("planted").exists(), "{at}: not moved, network open");
            assert!(marked(d), "{at}: no attribute set, network open");
            for f in ["made", "moved"] {
                std::fs::remove_file(d.join(f)).unwrap();
            }
            std::fs::remove_dir(d.join("folder")).unwrap();
        }
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
            caches: Vec::new(),
            network,
            ports,
            localhost,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
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
