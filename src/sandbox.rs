//! Claude's commands in a sandbox. What Claude runs — `run` and `run_tests` (`tools.rs`) — goes through
//! macOS's Seatbelt (`/usr/bin/sandbox-exec`) with a profile made here (`Policy::profile`): it may write
//! in the project (but `protected_names` — `.git`, `build.rs`, `Cargo.toml`, `.cargo`… wherever they sit —
//! and `llm.sandbox-protect`), the temp folders and the toolchain's own folders (cargo's registry and git
//! caches only with the network open — `Policy::caches` —, its target and build dirs when they're the checkout's own —
//! `asked_dir_verdict`, #20: a cargo config in the checkout names them, and Claude can write it; Go's caches), connect to nothing
//! over the network but the ports on this machine the user opened (`llm.sandbox-ports`, or all with
//! `llm.sandbox-localhost` — `RISKY_PORTS` says why not all by default), and make or reach unix sockets
//! only in the project and its own temp folder (`RunDir`, its `TMPDIR`). In home it reads only the project,
//! where it writes and what the toolchains were measured reading, besides what's granted (`Reads` — card model
//! P2) — never keys or Claude's and tarae's own (`kept_reads`), nor the clipboard or the keychain. What the
//! user runs themselves (`space m m`, `space x x`, installs, `:restart`) isn't sandboxed.
//!
//! Measured on macOS 27 (2026-10-01, `target/sb/` scripts): the later of two rules on a path wins and
//! `require-not` works · a sandboxed process can't apply a different profile (exit 71, `sandbox_apply:
//! Operation not permitted` — so `probe` says so inside one) · `launchctl submit` is refused, Apple Events
//! and LaunchServices are blocked by the rules below · localhost connects (by name too), other addresses
//! fail at once, names don't resolve · tarae's own `cargo test` passes inside, its build dir being
//! `~/.cargo/build/shared` (`build.build-dir`) — which is why the toolchain's dirs come from `cargo
//! metadata`, not a guess. Paths are matched as real paths (`/tmp` = `/private/tmp`): all are canonical.
//!
//! On Linux the same policy is enforced by Landlock and seccomp instead of a profile (`linux.rs`): fully
//! from Landlock ABI 3; at 1–2 as far as it goes, every run asking.
//!
//! Where there's no sandbox (no Landlock, another system, or tarae itself in a Seatbelt one) Claude's
//! commands don't run quietly without one: each asks, or the user agrees for the conversation, and the chat's
//! title says so.

#[cfg(test)]
mod golden;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod runenv;
#[cfg(target_os = "linux")]
pub mod supervisor;

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Set in a sandboxed command's environment (tarae's own tests read it: a sandbox can't start inside one).
pub const MARK: &str = "TARAE_SANDBOXED";

/// The shared temp folder, as a real path.
const TMP: &str = if cfg!(target_os = "macos") { "/private/tmp" } else { "/tmp" };

/// Whether this system can sandbox a command.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Availability {
    /// Not looked yet (`probe` runs on the first command's worker).
    #[default]
    Unknown,
    Ready,
    /// A sandbox that goes only so far (why — Linux's Landlock below ABI 3): runs go in it, each asking first.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Partial(String),
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

    /// What the run got of tarae's environment, when it ran in the sandbox.
    pub fn run_env(&self) -> Option<&runenv::RunEnv> {
        match self {
            Guard::In(p) => Some(&p.run_env),
            _ => None,
        }
    }

    /// What Claude reads with the command's result: what the sandbox blocked (or that it blocked nothing),
    /// then what the sandbox allows. Blocked = what tarae has its own record of (`Watch`: Seatbelt's log,
    /// Landlock's audit log, the supervisor's refusals). `output` = the run's lines: what they say was refused
    /// where the profile denies it (`Policy::said_in_output` — the kernel's own report of a denial gets lost)
    /// is said apart, as the output's words (`output_says`): such a line may be the command's own.
    pub fn report(&self, output: &[(bool, String)]) -> String {
        let rules = self.rules();
        let Guard::In(p) = self else { return rules };
        let unwatched = match p.denials_from() {
            Some(from) => format!(
                " What it refused is read {from}: \"Permission denied\" on a file, \"Operation not permitted\" on \
                 a socket, is likely the sandbox."
            ),
            None => " \"Operation not permitted\" is likely the sandbox.".into(),
        };
        let mut b = self.watch().map(|w| w.blocked()).unwrap_or_default();
        let said = p.said_in_output(output, &b.seen);
        let mut o = Blocked::default();
        for d in p.merge_made(&mut b.seen, said) {
            o.add(d);
        }
        for q in std::mem::take(&mut o.quiet) {
            if !b.quiet.iter().any(|d| d.op == q.op && d.target == q.target) {
                b.quiet.push(q);
            }
        }
        if (self.watch().is_none() || p.files_unwatched()) && b.total == 0 && o.total == 0 {
            return format!("{rules}{unwatched}");
        }
        if b.total > 0 || o.total > 0 {
            let mut s = match (&b.failed, b.total) {
                (Some(why), 0) => format!("tarae couldn't watch what the sandbox blocked ({why}). "),
                (Some(why), _) => {
                    format!("tarae couldn't watch what the sandbox blocked ({why}) — its output says:\n")
                }
                (None, 0) => "The sandbox blocked nothing tarae saw. ".into(),
                (None, _) => String::new(),
            };
            if b.total > 0 {
                s.push_str(&format!(
                    "The sandbox blocked {} — {} time{}:\n",
                    if b.distinct == 1 { "one thing".to_string() } else { format!("{} things", b.distinct) },
                    b.total,
                    if b.total == 1 { "" } else { "s" }
                ));
                for d in &b.seen {
                    s.push_str(&format!("  {}\n", d.reported()));
                }
                if b.distinct > b.seen.len() {
                    s.push_str(&format!("  … and {} more\n", b.distinct - b.seen.len()));
                }
            }
            // (Linux: what came from Landlock's audit log, and where that is here)
            if let Some(log) = p.audit_log().filter(|_| b.seen.iter().any(|d| d.process == SEEN_IN_AUDIT_LOG))
            {
                s.push_str(&format!("(Read from Landlock's audit log — {log} here.)\n"));
            }
            s.push_str(&p.output_says(&o, b.total > 0));
            let all_seen: Vec<Denial> = b.seen.iter().chain(&o.seen).cloned().collect();
            let (kept, all) = self.kept(&all_seen);
            s.push_str(&kept);
            s.push_str(&quiet_said(&b).unwrap_or_default());
            s.push_str(&rules);
            if !all || b.distinct > b.seen.len() || o.distinct > o.seen.len() {
                s.push_str(
                    "\nIf the command needs one of these, tell the user — they can open it from the run's \
                     panel (space m s), or llm.sandbox-write, llm.sandbox-ports, llm.sandbox-localhost and \
                     llm.sandbox-network open more — or ask to run it outside the sandbox (outside_sandbox, \
                     with your reason); don't work around it.",
                );
            }
            return s;
        }
        let quiet = quiet_said(&b).unwrap_or_default();
        match b.failed {
            None => format!("The sandbox blocked nothing. {quiet}{rules}"),
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

    /// What among `seen` the profile keeps on purpose, each said with why and what to do instead — and
    /// whether that's all of them.
    fn kept(&self, seen: &[Denial]) -> (String, bool) {
        let Guard::In(p) = self else { return (String::new(), false) };
        let (mut names, mut git, mut caches, mut all) = (Vec::new(), false, Vec::new(), true);
        let (mut secret, mut services, mut copied) = (Vec::new(), Vec::new(), Vec::new());
        let (mut homes, mut ports) = (Vec::new(), Vec::new());
        let refused_port = &crate::sandboxpack::get().refused_port;
        let (mut entries, mut signals) = (false, false);
        for d in seen {
            if d.op.starts_with("file-entry-") {
                entries = true;
                continue;
            }
            let Some(path) = d.file() else {
                match d.plain().as_str() {
                    "the clipboard" => services.push("the clipboard"),
                    "the keychain" => services.push("the keychain"),
                    _ if d.op == "signal" => signals = true,
                    // (a process the packs say needs this machine's ports refused one: Gradle's JVM)
                    w if w.starts_with("connecting to port")
                        && refused_port.iter().any(|(n, _)| *n == d.process) =>
                    {
                        // (Linux: the open network isn't this machine's ports)
                        let closed = !p.localhost && !(p.network && p.linux_said().is_none());
                        let of = refused_port.iter().position(|(n, _)| *n == d.process);
                        ports.extend(of.filter(|i| closed && !ports.contains(i)));
                        all = false;
                    }
                    _ => all = false,
                }
                continue;
            };
            match path.strip_prefix(&p.root) {
                Ok(rel) if protected_in(&p.root, path) => {
                    match rel.iter().any(|c| c.eq_ignore_ascii_case(".git")) {
                        true => git = true,
                        false => names.push(rel.display().to_string()),
                    }
                }
                _ if p.caches.iter().any(|c| path.starts_with(c)) => {
                    let of = p.caches_said.iter().position(|s| s.dirs.iter().any(|c| path.starts_with(c)));
                    caches.extend(of.filter(|i| !caches.contains(i)));
                }
                _ if p.reads.kept_holds(&p.root, path) => secret.push(crate::allow::tilde(path)),
                _ if p.reads.copies.instead_of(path).is_some() => {
                    let var = p.reads.copies.instead_of(path).unwrap_or_default();
                    copied.push(format!("{} (${var})", crate::allow::tilde(path)))
                }
                _ if d.op.starts_with("file-write")
                    && p.run_homes.iter().any(|h| path.starts_with(&h.user)) =>
                {
                    let of = p.run_homes.iter().position(|h| path.starts_with(&h.user));
                    homes.extend(of.filter(|i| !homes.contains(i)));
                }
                _ => all = false,
            }
        }
        names.sort();
        names.dedup();
        secret.sort();
        secret.dedup();
        services.dedup();
        copied.sort();
        copied.dedup();
        let mut s = String::new();
        if !copied.is_empty() {
            s.push_str(&format!(
                "{}: it holds a token, so your commands read a copy without those lines, at the variable named — \
                 the tool finds it on its own; the file itself stays closed. If a command needs the token, say so \
                 to the user and let them run it.\n",
                copied.join(", ")
            ));
        }
        if !secret.is_empty() || !services.is_empty() {
            let what: Vec<String> =
                secret.into_iter().chain(services.into_iter().map(String::from)).collect();
            s.push_str(&format!(
                "{}: tarae keeps these from every command of yours — keys, tokens, Claude's and tarae's own \
                 state, the clipboard, the keychain. Nothing opens them for a command; if one needs it, say so \
                 to the user and let them run it.\n",
                what.join(", ")
            ));
        }
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
        for i in caches {
            s.push_str(&p.caches_said[i].kept());
        }
        for i in homes {
            let h = &p.run_homes[i];
            s.push_str(&format!("{}\n", h.said.replace("{home}", &crate::allow::tilde(&h.user))));
        }
        for i in ports {
            s.push_str(&format!("{}\n", refused_port[i].1));
        }
        if entries {
            s.push_str(
                "On Linux a folder that holds a protected name, or one below it, takes no new files and keeps \
                 the ones it has (Landlock's rules are on what's there; what a build makes there is made \
                 first). For a file of your own, make it in a folder without one — the run's own folder \
                 ($TMPDIR) always is; for one the build needs there, tell the user.\n",
            );
        }
        if signals {
            s.push_str(
                "A command can't signal a process outside its own (Landlock keeps it in) — tarae's, the \
                 user's others. If one needs to, say so to the user.\n",
            );
        }
        (s, all)
    }

    /// What the sandbox allows (or that there was none).
    fn rules(&self) -> String {
        match self {
            Guard::In(p) => {
                let ports = p.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
                let linux = p.linux_said();
                // (Linux's supervisor opens nothing beyond this machine — said in its notes)
                let beyond = p.network && linux.is_none();
                let net = match (beyond, p.localhost, ports.is_empty()) {
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
                            " It kept closed what the checkout's {} config named: {} — a build writing \
                             there fails; the user can open a folder with llm.sandbox-write.",
                            p.named_by,
                            each.join("; ")
                        )
                    }
                };
                // (what the toolchain's settings named for its caches and stays closed)
                let refused = match p.refused_asked.is_empty() {
                    true => refused,
                    false => {
                        let each: Vec<String> = p
                            .refused_asked
                            .iter()
                            .map(|(d, why)| format!("{} ({why})", crate::allow::tilde(d)))
                            .collect();
                        format!(
                            "{refused} It kept closed what the toolchain's settings named: {} — a build writing \
                             there fails; the user can open a folder with llm.sandbox-write.",
                            each.join("; ")
                        )
                    }
                };
                let names = protected_names().collect::<Vec<_>>().join(", ");
                let rules: Vec<&str> = p.caches_said.iter().map(|c| c.rule.as_str()).collect();
                let crates = match p.caches.is_empty() {
                    false => format!(" (not {} — while the network is closed)", rules.join("; ")),
                    true => String::new(),
                };
                let reads = match p.reads.home.is_some() {
                    true => {
                        "read in your home folder only the project, the repository's other checkouts and the \
                         toolchains' own folders — never keys, tokens, Claude's or tarae's state, the clipboard \
                         or the keychain — "
                    }
                    false => "",
                };
                // (said only where a token was there: nothing more to read on a machine without one)
                let copies = match p.reads.copies.said() {
                    Some((files, vars)) => {
                        format!(" The tools read copies of {files} without their token lines ({vars}).")
                    }
                    None => String::new(),
                };
                let (names_where, net, linux) = match linux {
                    Some((said, notes)) => ("where they are now", said.unwrap_or(net), notes),
                    None => ("anywhere in it", net, String::new()),
                };
                format!(
                    "In tarae's sandbox: it could {reads}write only in the project — not {names}, {names_where} \
                     (Edit changes those) — temp folders and the toolchain's own{crates}{net}.{granted}{refused}\
                     {copies}{linux}"
                )
            }
            Guard::Without(why) => format!("Ran without a sandbox ({why}) — the user agreed."),
            Guard::Off => "Ran without a sandbox — the user turned it off (llm.sandbox).".into(),
            Guard::Outside => "Ran outside the sandbox — the user chose to, this once.".into(),
        }
    }
}

/// A tool's shared caches as a run is told of them (the packs' `caches`): its folders, how the rules say them,
/// and what Claude reads when a write there was kept — why, and what to do instead (`{caches}`: the folders).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachesSaid {
    pub dirs: Vec<PathBuf>,
    pub rule: String,
    pub said: String,
    /// The extended attribute the tool sets on these folders themselves at every run (Seatbelt's: Landlock has
    /// no such rule) — cargo's for file sync, the one write to its caches an offline build makes.
    pub xattr: Option<String>,
}

impl CachesSaid {
    fn kept(&self) -> String {
        let dirs: Vec<String> = self.dirs.iter().map(|c| crate::allow::tilde(c)).collect();
        format!("{}\n", self.said.replace("{caches}", &dirs.join(", ")))
    }
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
    /// Single files writable — the caches' own files (cargo's locks and last-use database), the network closed
    /// even those inside `caches`, and the packs' `write-files`.
    pub files: Vec<PathBuf>,
    /// Folders inside writable ones kept as they are: the terminal multiplexers' sockets (`mux_dirs`) — a
    /// command could otherwise delete or replace the multiplexer the user works in.
    pub keep: Vec<PathBuf>,
    /// The tools' shared caches (the packs' `caches` — cargo's crates and git checkouts) kept as they are while
    /// the network is closed: every project's build outside the sandbox uses what's there, and with no
    /// download a build has nothing new to put there. Only `files` are written in them. What a build needs
    /// that isn't there fails here, and the report says why (`caches_said`).
    pub caches: Vec<PathBuf>,
    /// How each tool's caches are said, while they're kept, and the attribute it sets on them.
    pub caches_said: Vec<CachesSaid>,
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
    /// The folders the checkout's build tool named that stay closed, and why (`asked_dir_verdict`) — said with
    /// the result and on the panel — and who named them (`builds-said`: `cargo`).
    pub refused: Vec<(PathBuf, String)>,
    pub named_by: String,
    /// The folders the packs' `ask` named that stay closed, and why (`ask_dir_verdict` — Go's caches).
    pub refused_asked: Vec<(PathBuf, String)>,
    /// Where it may read (card model P2).
    pub reads: Reads,
    /// What the run's environment gets from the packs: the variables that point a tool at a home of the run's
    /// own (`run_homes_made` — Gradle's), and their `env` (npm's log folder in the run's).
    pub env: Vec<(String, String)>,
    /// What the run gets of tarae's own environment (`runenv.rs`) — it starts from nothing else — and the names
    /// of what it doesn't.
    pub run_env: runenv::RunEnv,
    /// The users' own homes of the tools that get one of the run's (kept as they are), as Claude is told.
    pub run_homes: Vec<RunHomeSaid>,
    /// The kept run homes its runs get (`with_kept`; held — `policy_in`).
    pub kept: Vec<Kept>,
    /// Linux: the policy as Landlock's rules (`linux::confine`, made on the worker) — Err why it couldn't be
    /// (the run then doesn't start: `spawn`); None before it's made (`policy` makes it).
    #[cfg(target_os = "linux")]
    pub confine: Option<Result<linux::Confine, String>>,
}

/// A tool that gets a home of the run's own (the packs' `run-home`): its name, the user's own home of it (real,
/// there), and what Claude reads when a write there was kept (`{home}`: that home).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHomeSaid {
    pub name: String,
    pub user: PathBuf,
    pub said: String,
}

/// Where a run reads (card model P2): anywhere outside home; in home the project, where it may write, and
/// `allow` — never `kept`, whatever else holds it. Real paths. A file's metadata (`stat`) stays open in home:
/// every program looks at home and the folders above the project, thousands of times a run (measured,
/// `target/sb/p2-measured.md`); what's in a file or a folder's list is what's confined.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    /// Home — None: reads aren't confined (no home known; tests that don't ask).
    pub home: Option<PathBuf>,
    /// Places in home it may read, files or folders with everything in them (`reads_for`).
    pub allow: Vec<PathBuf>,
    /// Never read, denied after every allow (`kept_reads`) — but `log`.
    pub kept: Vec<PathBuf>,
    /// tarae's log, though it's in tarae's state: Claude reads it unasked (#45).
    pub log: Option<PathBuf>,
    /// Settings that hold a token: the copies the tools read instead, in the run's own folder, and the files
    /// themselves, not read (`tokens.rs`).
    pub copies: crate::tokens::Copies,
}

impl Reads {
    /// Whether `real` is in a place kept from every run — the profile's rule, in Rust: one that holds the
    /// project (a checkout under `~/.claude/worktrees`) is kept but for the project.
    pub fn kept_holds(&self, root: &Path, real: &Path) -> bool {
        self.log.as_deref() != Some(real)
            && self
                .kept
                .iter()
                .any(|k| real.starts_with(k) && !(root.starts_with(k) && real.starts_with(root)))
    }
}

/// What every run reads in home besides what the toolchains read (the packs' `reads` — `sandboxpack.rs`),
/// measured (`target/sb/p2-measured.md`, every read in home reported): git's settings, the text encoding
/// CoreFoundation reads in every process, the fonts a shot draws with.
const HOME_READS: [&str; 4] = [".gitconfig", ".config/git", ".CFUserTextEncoding", "Library/Fonts"];

/// What a run never reads at a toolchain's default home, whatever is granted and whatever the packs say — the
/// floor under their `kept` (which adds where a variable moved the home), as `allow::SECRETS` is: cargo's
/// credentials, and the user's Gradle daemons' registry — their ports and the token each takes (measured,
/// `target/sb/java-checkpoint.md` c0: with it readable and this machine's ports open, a sandboxed `./gradlew`
/// hands its build to a daemon started outside, and the build runs outside the sandbox).
pub const KEPT_FLOOR: [&str; 3] = [".cargo/credentials", ".cargo/credentials.toml", ".gradle/daemon"];

/// The packs' places (`sandboxpack::Vars`) for `home` as `here` says the variables — read from the process
/// once, where a policy is made, and passed in: what's worked out from it is the same in a test as in a run (a
/// sandboxed run has a Gradle home of its own).
pub fn vars_in(home: &Path, here: &Here) -> crate::sandboxpack::Vars {
    crate::sandboxpack::Vars::new(crate::sandboxpack::get(), home, &|name| here.path(name), here.os)
}

/// The system a policy is made for — what the folders it names are on it (`tokens::user_config`). The one tarae
/// runs on, but in tests: each system's policy is made on any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Os {
    #[default]
    Mac,
    Linux,
}

impl Os {
    pub const HOST: Os = if cfg!(target_os = "macos") { Os::Mac } else { Os::Linux };
}

/// What the process's surroundings say where a policy is made — its variables, `$TMPDIR` (real), the user's
/// id, the system — read once (`now`) and passed in: what's worked out from it is the same in a test as in a
/// run.
#[derive(Clone, Debug, Default)]
pub struct Here {
    pub env: HashMap<String, std::ffi::OsString>,
    pub tmp: Option<PathBuf>,
    pub uid: u32,
    pub os: Os,
    /// Tests: every variable asked (`path`, `var`) — what a policy reads of the environment
    /// (`every_variable_the_policy_reads_reaches_the_run`).
    #[cfg(test)]
    pub asked: std::sync::Arc<Mutex<Vec<String>>>,
}

impl Here {
    pub fn now() -> Here {
        Here {
            env: std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))).collect(),
            tmp: std::fs::canonicalize(std::env::temp_dir()).ok(),
            // SAFETY: getuid has no preconditions and can't fail.
            uid: unsafe { libc::getuid() },
            os: Os::HOST,
            #[cfg(test)]
            asked: Default::default(),
        }
    }

    /// A variable naming a place, when it's set and not empty.
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        self.asked(name);
        self.env.get(name).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    /// A variable as it's set (empty too), in UTF-8 — what `tokens::sources` reads (`std::env::var`'s).
    pub fn var(&self, name: &str) -> Option<String> {
        self.asked(name);
        self.env.get(name).and_then(|v| v.clone().into_string().ok())
    }

    #[cfg(test)]
    fn asked(&self, name: &str) {
        self.asked.lock().unwrap().push(name.to_string());
    }

    #[cfg(not(test))]
    fn asked(&self, _: &str) {}
}

/// Claude's own, in home: its settings, conversations and token cache — kept from its runs like keys.
const CLAUDES: [&str; 3] = [".claude", ".claude.json", ".claude.json.backup"];

/// Where a run reads in `home` (real; `vars` its places): `HOME_READS`, the packs' `reads` (a home alone only
/// where it's there — its variable may name a folder never made), tarae's grammars (its own tests and shots
/// load them), the folders on PATH (what's there runs), and `also` (the repository's other checkouts, what the
/// user let Claude read) — none that's home itself or in a kept place.
pub fn reads_for(home: &Path, also: &[PathBuf], vars: &crate::sandboxpack::Vars) -> Reads {
    let packs = crate::sandboxpack::get().reads.iter();
    let packs = packs.filter_map(|r| vars.place(r).filter(|p| !vars.is_home(r) || p.exists()));
    let mut allow: Vec<PathBuf> = HOME_READS.iter().map(|r| home.join(r)).chain(packs).collect();
    allow = allow.iter().map(|p| profile_path(p)).collect();
    allow.extend(crate::grammar::grammars_dir().map(|d| profile_path(&d)));
    allow.extend(crate::blocked::path_dirs());
    allow.extend(also.iter().cloned());
    let kept = kept_reads(home, vars);
    allow.retain(|p| p.starts_with(home) && p != home && !kept.iter().any(|k| p.starts_with(k)));
    allow.sort();
    allow.dedup();
    let log = crate::log::path().map(|l| profile_path(&l));
    Reads { home: Some(home.to_path_buf()), allow, kept, log, copies: Default::default() }
}

/// What a toolchain reads above the project (`root`, real) in `home`: the packs' `reads-above` files in every
/// folder between them — those files, not the folder (cargo's `credentials.toml` sits beside its config).
pub fn reads_above(root: &Path, home: &Path) -> Vec<PathBuf> {
    let files = &crate::sandboxpack::get().reads_above;
    root.ancestors()
        .skip(1)
        .take_while(|a| a.starts_with(home) && *a != home)
        .flat_map(|a| files.iter().map(|f| a.join(f)))
        .collect()
}

/// What a run never reads, whatever is granted: the places that hold keys (`allow::SECRETS`, and git's
/// credential store where `$XDG_CONFIG_HOME` puts it), Claude's own (`CLAUDES`), `KEPT_FLOOR`, the packs'
/// `kept` at both a home's places, and tarae's state (its conversations, what was allowed — its log aside).
pub fn kept_reads(home: &Path, vars: &crate::sandboxpack::Vars) -> Vec<PathBuf> {
    kept_in(home, vars, &crate::sandboxpack::get().kept)
}

/// `kept_reads` with the packs' `kept` given.
fn kept_in(home: &Path, vars: &crate::sandboxpack::Vars, packs: &[String]) -> Vec<PathBuf> {
    let at = |p: PathBuf| profile_path(&p);
    let floor = crate::allow::SECRETS.iter().chain(&CLAUDES).chain(&KEPT_FLOOR);
    let mut v: Vec<PathBuf> = floor.map(|s| at(home.join(s))).collect();
    v.extend(packs.iter().flat_map(|k| vars.places_both(k)).map(at));
    v.push(at(vars.xdg_config.join("git/credentials")));
    v.extend(crate::config::state_dir().map(at));
    v.sort();
    v.dedup();
    v
}

/// The checkout's build tool (a built-in pack's `[builds]`), as its named folders are judged: who names them
/// (`builds-said`: `cargo`), its home (real, when it's there), and where a checkout's own build folder sits in
/// that home — so many folders below this one (`[builds] remove`: `build`, 2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Builder {
    pub who: String,
    pub home: Option<PathBuf>,
    pub own_under: Option<(PathBuf, usize)>,
}

/// Whether a folder the checkout's build tool names for the checkout at `root` (`d`, a real path) may be
/// written by its runs (#20 — a config in the checkout names it, and Claude can write that): `Ok(true)` it
/// needs a rule of its own (this checkout's own per-checkout build dir, `own`, where `tool` keeps them),
/// `Ok(false)` it's writable already (inside the checkout, a temp folder); `Err` why it stays closed — home or
/// above it, a folder holding the checkout, the tool's home or above it, or anything else.
pub fn asked_dir_verdict(
    d: &Path,
    root: &Path,
    home: Option<&Path>,
    tool: &Builder,
    own: Option<&Path>,
    tmp: Option<&Path>,
) -> Result<bool, String> {
    if let Some(why) = never_written(d, root, home) {
        return Err(why);
    }
    let who = &tool.who;
    if tool.home.as_ref().is_some_and(|c| c.starts_with(d)) {
        return Err(format!("it's {who}'s home"));
    }
    if d.starts_with(root) {
        return Ok(false);
    }
    let per_checkout = |o: &Path| {
        let (Some(h), Some((under, depth))) = (&tool.home, &tool.own_under) else { return false };
        o.strip_prefix(h.join(under)).is_ok_and(|b| b.components().count() == *depth)
    };
    if own.is_some_and(|o| o == d && per_checkout(o)) {
        return Ok(true);
    }
    let temps = [Some(Path::new(TMP)), tmp];
    if temps.iter().flatten().any(|t| d.starts_with(t) && d != *t) {
        return Ok(false);
    }
    Err(format!("it isn't in the checkout, its own build dir under {who}'s home, or a temp folder"))
}

/// Why a folder a toolchain names (`d`, real) is never one its runs write, whatever names it: home or above
/// it, a folder holding the checkout `root` (it would open `.git`).
fn never_written(d: &Path, root: &Path, home: Option<&Path>) -> Option<String> {
    if home.is_some_and(|h| h.starts_with(d)) {
        return Some(if home == Some(d) { "it's your home".into() } else { "it holds your home".into() });
    }
    (root.starts_with(d) && root != d).then(|| "it holds the checkout".into())
}

/// Whether a folder the packs' `ask` names (Go's module cache — `d`, real) may be written by the project's runs
/// (a cache: with the network open): it's the tool's own, wherever its settings put it — but never home or
/// above it, a folder holding the checkout, nor in or around where keys are, Claude's and tarae's own, or what
/// runs later (`allow::SECRETS`, `KEPT_FLOOR`, `allow::NEVER_WRITE`, `later`: programs' folders, shared caches,
/// toolchains' homes and what's in them). Err why not.
pub fn ask_dir_verdict(d: &Path, root: &Path, home: Option<&Path>, later: &RunsLater) -> Result<(), String> {
    if let Some(why) = never_written(d, root, home).or_else(|| later.refuses(d)) {
        return Err(why);
    }
    let Some(home) = home else { return Ok(()) };
    let keys = crate::allow::SECRETS.iter().chain(&CLAUDES).chain(&KEPT_FLOOR).map(|k| home.join(k));
    let mut keys = keys.chain(crate::config::state_dir().map(|s| profile_path(&s)));
    if keys.any(|k| k.starts_with(d) || d.starts_with(&k)) {
        return Err("it's where keys, Claude's or tarae's own are".into());
    }
    let later = crate::allow::NEVER_WRITE.iter().map(|w| home.join(w));
    if later.into_iter().any(|w| w.starts_with(d) || d.starts_with(&w)) {
        return Err("what's there runs later outside the sandbox".into());
    }
    Ok(())
}

/// Where programs land by convention in home, whatever the toolchain (install scripts', pipx's).
pub const USER_BIN: &str = ".local/bin";

/// What no folder opened for writing is, holds or is in — what's there runs outside the sandbox later, as the
/// user: the folders of programs they run (`bins`: those on `PATH`, `USER_BIN`, the packs' `bins`), the
/// toolchains' shared caches (`caches`: the packs' `caches` at their defaults and where a home or variable moved
/// them — every project's build uses what's there; written only through the caches rule, with the network
/// open), and the toolchains' homes (the packs' `homes`, where they are and at their defaults) and all in them —
/// compiled build scripts, the distributions a wrapper runs, the toolchains a proxy runs; what the packs let a
/// run write there goes through its own rule (a cache's, `write-files`), never this. Real paths. The asked
/// folders (`ask_dir_verdict`) and the write card (`allow::write_choices`) read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunsLater {
    pub bins: Vec<PathBuf>,
    pub caches: Vec<crate::sandboxpack::CachePlace>,
    pub homes: Vec<PathBuf>,
    /// The places in a home a cache being judged has there (`but_cache`) — a folder in one isn't refused for
    /// being in that home.
    pub own: Vec<PathBuf>,
}

impl RunsLater {
    /// For `home` (real) and the packs' places there (`vars`), with the folders on `PATH` (`path`, real).
    pub fn new(home: &Path, vars: &crate::sandboxpack::Vars, path: &[PathBuf]) -> RunsLater {
        let packs = crate::sandboxpack::get().bins.iter().flat_map(|b| vars.places_both(b));
        let bins = path.iter().cloned().chain([home.join(USER_BIN)]).chain(packs);
        let homes = vars.homes.iter().flat_map(|(_, at, default)| [at.clone(), default.clone()]);
        let homes = homes.chain(vars.listed.iter().cloned());
        let real = |v: &mut Vec<PathBuf>| {
            *v = v.iter().map(|p| profile_path(p)).collect();
            v.sort();
            v.dedup();
        };
        let mut caches: Vec<_> = vars.caches.clone();
        for c in &mut caches {
            c.at = profile_path(&c.at);
        }
        caches.sort();
        caches.dedup();
        let mut r = RunsLater { bins: bins.collect(), caches, homes: homes.collect(), own: Vec::new() };
        real(&mut r.bins);
        real(&mut r.homes);
        r
    }

    /// How a cache a toolchain names (its `asked` key) is judged: as any folder, but for that cache's own places
    /// (it's kept by the caches rule) — another toolchain's cache, or home, still refuses it; in a home only
    /// where the pack gives that cache a place there.
    pub fn but_cache(&self, asked: &str) -> RunsLater {
        let (own, caches): (Vec<_>, Vec<_>) =
            self.caches.iter().cloned().partition(|c| c.asked.as_deref() == Some(asked));
        let own = own.into_iter().filter(|c| c.given).map(|c| c.at).collect();
        RunsLater { caches, own, ..self.clone() }
    }

    /// Why folder `d` (real) is never written — None: not for this.
    pub fn refuses(&self, d: &Path) -> Option<String> {
        if let Some(b) = self.bins.iter().find(|b| b.starts_with(d) || d.starts_with(b)) {
            let how = if d.starts_with(b) { "it's where" } else { "it holds where" };
            return Some(format!("{how} programs you run are ({})", crate::allow::tilde(b)));
        }
        if let Some(c) = self.caches.iter().map(|c| &c.at).find(|c| c.starts_with(d) || d.starts_with(c)) {
            let how = if d.starts_with(c) { "it's in" } else { "it holds" };
            return Some(format!(
                "{how} a toolchain's shared cache ({}) — written only with the network open",
                crate::allow::tilde(c)
            ));
        }
        let own = |h: &Path| self.own.iter().any(|o| o.starts_with(h) && o != h && d.starts_with(o));
        let h = self.homes.iter().find(|h| h.starts_with(d) || (d.starts_with(h) && !own(h)))?;
        let how = if *h == d {
            "it's"
        } else if h.starts_with(d) {
            "it holds"
        } else {
            "it's in"
        };
        Some(format!("{how} a toolchain's home ({})", crate::allow::tilde(h)))
    }
}

/// The folders on `PATH` as `here` says it (real paths, those that are there).
pub fn path_dirs_in(here: &Here) -> Vec<PathBuf> {
    let path = here.path("PATH");
    path.map(|p| std::env::split_paths(&p).filter_map(|d| std::fs::canonicalize(d).ok()).collect())
        .unwrap_or_default()
}

/// A run's own temp folder. Unix sockets anywhere else stay out of reach: in `/private/tmp` and `$TMPDIR`
/// live tmux's, zellij's and ssh-agent's — servers outside the sandbox that run what they're told
/// (reproduced: `tmux -S /private/tmp/tmux-501/default run-shell "touch ~/marker"` from inside made the
/// marker when temp sockets were allowed). Removed, on a thread of its own, once the run is gone.
#[derive(Debug, PartialEq, Eq)]
pub struct RunDir(PathBuf);

impl RunDir {
    /// Where runs' own folders are made: `$TMPDIR`, real.
    pub fn base() -> PathBuf {
        profile_path(&std::env::temp_dir())
    }

    /// A new folder under `$TMPDIR` (real path; short — a socket's path has 104 bytes at most), only ours.
    pub fn new() -> std::io::Result<std::sync::Arc<RunDir>> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = RunDir::base();
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
        crate::event::thread("tarae-tidy", move || std::fs::remove_dir_all(dir));
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

/// A hard link refused because its source isn't writable or readable — Seatbelt's
/// `forbidden-link-priv<file-write*>` / `<file-read*>` `<source> <link>` (measured).
const LINK_PRIV: &str = "forbidden-link-priv";

/// A denial read from the run's output (`Policy::said_in_output`): its process, and its operation when the
/// profile denies both reading and writing the path — the output doesn't say which was tried.
const SEEN_IN_OUTPUT: &str = "said in the output";
/// A denial read from Landlock's audit log (`linux::Audit`): its process — the record names only its domain.
pub(crate) const SEEN_IN_AUDIT_LOG: &str = "the run";
pub(crate) const EITHER: &str = "file-read* file-write*";

/// `target` (a path, or what a denial names) with home spelled `~` — how a denial's words show it.
pub(crate) fn home_shown(target: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match target.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => target.to_string(),
    }
}

impl Denial {
    /// The port on this machine a connection was refused on — Seatbelt names no address (`remote:*:9`, so
    /// any port counts), Linux's supervisor does (`remote:127.0.0.1:9`, `remote:[::1]:9`). None for an
    /// address beyond this machine.
    pub fn port_here(&self) -> Option<u16> {
        let rest = self.target.strip_prefix("remote:").filter(|_| self.op == "network-outbound")?;
        let (host, port) = rest.rsplit_once(':')?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let here = host == "*"
            || host.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
                std::net::IpAddr::V6(v6) => {
                    v6.to_ipv4_mapped().map_or(v6.is_loopback(), |v4| v4.is_loopback())
                }
                v4 => v4.is_loopback(),
            });
        port.parse().ok().filter(|_| here)
    }

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

    /// The target, home spelled `~` (`home_shown`).
    fn target_shown(&self) -> String {
        home_shown(&self.target)
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
            "file-read-data" | "file-read-xattr" => format!("reading {t}"),
            "file-read-metadata" => format!("looking at {t}"),
            "file-link" => format!("linking {t}"),
            EITHER => format!("reading or writing {t}"),
            "file-write-create" | "file-write-data" | "file-write*" => format!("writing {t}"),
            "file-write-unlink" => format!("deleting {t}"),
            "file-write-rename" => format!("moving {t}"),
            _ if op.starts_with(LINK_PRIV) => {
                format!("linking {}", t.rsplit_once(" /").map_or(t.as_str(), |(s, _)| s))
            }
            // (Linux: Landlock's own rule, and its signal scope — `landlock_said_in`)
            "file-entry-make" => {
                format!("making {t} — a folder holding protected names takes no new files on Linux")
            }
            "file-entry-remove" => {
                format!("removing {t} — a folder holding protected names keeps its files on Linux")
            }
            "file-entry-move" => {
                format!("moving {t} — a folder holding protected names keeps its files on Linux")
            }
            "file-entry-make-in" => {
                format!(
                    "making a new file in {t} — a folder holding protected names takes no new files on Linux"
                )
            }
            "file-make-in" => format!("making a new file in {t}"),
            "signal" if t.is_empty() => "signalling a process outside the run's own processes".into(),
            "signal" => format!("signalling process {t} — outside the run's own processes"),
            _ if op.starts_with("file-write") => format!("changing {t}"),
            // (a path by what it is, not as shown: under home it's `~/…` — a VM's checkout in home)
            "network-outbound" if self.target.starts_with('/') => format!("connecting to socket {t}"),
            "network-outbound" if t.starts_with("remote:") => match self.port_here() {
                Some(port) => format!("connecting to port {port}"),
                None => match t.strip_prefix("remote:") {
                    Some(at) if at.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) => {
                        format!("connecting to {at}")
                    }
                    _ => self.shown(),
                },
            },
            "network-bind" if self.target.starts_with('/') => format!("making socket {t}"),
            "appleevent-send" if !t.is_empty() => format!("Apple Event to {t}"),
            "mach-lookup"
                if t.starts_with("com.apple.lsd") || t == "com.apple.coreservices.launchservicesd" =>
            {
                "opening apps".into()
            }
            "mach-lookup" if t.starts_with("com.apple.pasteboard.") => "the clipboard".into(),
            "mach-lookup" if KEYCHAIN.contains(&t.as_str()) => "the keychain".into(),
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
    /// Denials expected, kept apart (`quiet`) — not counted as blocked, no panel line; each kind once.
    pub quiet: Vec<Denial>,
}

pub const DENIALS_KEPT: usize = 10;

/// Where a denied write is expected and harmless, so no report or panel names it — the packs' `quiet-writes`
/// (Go's telemetry counters) at their places for this home on this system (`vars`): macOS's `~/Library/
/// Application Support/go/telemetry`, Linux's where `$XDG_CONFIG_HOME` puts it, else `~/.config/…`. Only
/// there: the same tail anywhere else (a project's own `go/telemetry/`) is named as ever. As the variables say
/// them — a place reached through a link isn't matched, and its denial is named.
pub fn quiet_places(vars: &crate::sandboxpack::Vars) -> Vec<PathBuf> {
    crate::sandboxpack::get().quiet_writes.iter().filter_map(|q| vars.place(q)).collect()
}

/// `quiet_places` for the process's home on this system, read once (variables only — no disk).
fn quiet_writes() -> &'static [PathBuf] {
    static AT: OnceLock<Vec<PathBuf>> = OnceLock::new();
    AT.get_or_init(|| {
        let var = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
        let Some(home) = var("HOME").filter(|h| h.is_absolute()) else { return Vec::new() };
        quiet_places(&crate::sandboxpack::Vars::new(crate::sandboxpack::get(), &home, &var, Os::HOST))
    })
}

/// A shell's own startup files, right in home — closed with the rest of home on purpose (they hold tokens,
/// and set what a command runs: a run is what its line says). zsh reads `~/.zshenv` for every `zsh -c` and
/// starts without it, saying nothing; the report says so calmly, apart from what was blocked (`quiet_said`).
const SHELL_SETUP: [&str; 8] =
    [".zshenv", ".zprofile", ".zshrc", ".zlogin", ".profile", ".bashrc", ".bash_profile", ".bash_login"];

/// Whether `d` is a read of one of `SHELL_SETUP` in `home`.
fn shell_setup(d: &Denial, home: Option<&Path>) -> bool {
    let p = Path::new(&d.target);
    d.op.starts_with("file-read")
        && home.is_some_and(|h| p.parent() == Some(h))
        && p.file_name().is_some_and(|n| SHELL_SETUP.iter().any(|s| n == *s))
}

/// Whether `d` is expected — the packs' quiet writes (`quiet_writes`), a shell's startup files
/// (`SHELL_SETUP`), a JVM's own `/proc/<pid>/coredump_filter` (it goes on without), a `connect` to port 0
/// (getaddrinfo sorting its answers by the route to each — a UDP socket, nothing sent; Rocky's `getent ahosts`,
/// step 7).
pub fn quiet(d: &Denial) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    quiet_in(d, home.as_deref(), quiet_writes())
}

/// `quiet` for `home`, its quiet writes at `places` (`quiet_places`).
fn quiet_in(d: &Denial, home: Option<&Path>, places: &[PathBuf]) -> bool {
    let write = d.op.starts_with("file-write") || d.op == EITHER;
    (write && places.iter().any(|w| Path::new(&d.target).starts_with(w)))
        || (write && d.target.starts_with("/proc/") && d.target.ends_with("/coredump_filter"))
        || (d.op == "network-outbound" && d.target.starts_with("remote:") && d.target.ends_with(":0"))
        || shell_setup(d, home)
}

/// The report's calm line on what was kept apart: the shell's setup, not read — None for Go's telemetry
/// (nothing to say: go carries on as it would).
fn quiet_said(b: &Blocked) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut shell: Vec<String> = b
        .quiet
        .iter()
        .filter(|d| shell_setup(d, home.as_deref()))
        .map(|d| crate::allow::tilde(Path::new(&d.target)))
        .collect();
    shell.dedup();
    (!shell.is_empty()).then(|| {
        format!(
            "{}: the shell's own setup isn't read in the sandbox, as always (it holds tokens, and sets what \
             runs) — the shell started without it, nothing failed for it. A command that needs something set \
             there (a PATH entry, a variable) sets it itself.\n",
            shell.join(", ")
        )
    })
}

impl Blocked {
    pub fn add(&mut self, d: Denial) {
        if quiet(&d) {
            if self.quiet.len() < DENIALS_KEPT
                && !self.quiet.iter().any(|q| q.op == d.op && q.target == d.target)
            {
                self.quiet.push(d);
            }
            return;
        }
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
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
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

/// How long a run in `p` waits for its watch to be up before it starts (`tools::guard_then_go`, on a worker):
/// `WATCH_READY` when the watch reads `log stream` (macOS — it reads from when it's up). Linux's audit
/// stream reads from a mark taken when it was asked for (`linux::AuditSource::stream`): no wait.
pub fn watch_wait(p: &Policy) -> std::time::Duration {
    match !cfg!(target_os = "linux") && p.watch.as_ref().is_some_and(|w| w.pid().is_some()) {
        true => WATCH_READY,
        false => std::time::Duration::ZERO,
    }
}

/// How long a watch waits for its run to start (`Watch::run_started`) before it ends itself — the run's
/// saves come first (`tools::go`), a few ms as a rule. Seen: a `log stream` living on 18 minutes after its
/// run, held by nothing `:procs` knew; a watch no longer depends on its holders letting go.
pub const WATCH_UNCLAIMED: std::time::Duration = std::time::Duration::from_secs(60);
/// How often a watch looks whether its run's process is still there.
const WATCH_LOOKS: std::time::Duration = std::time::Duration::from_millis(250);

/// Every watch alive, wherever it's held — `:procs` knows each (`live_watches`).
static LIVE: Mutex<Vec<std::sync::Weak<Watch>>> = Mutex::new(Vec::new());

/// The watches alive now.
pub fn live_watches() -> Vec<std::sync::Arc<Watch>> {
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.retain(|w| w.strong_count() > 0);
    live.iter().filter_map(std::sync::Weak::upgrade).collect()
}

/// The kernel's denials for one run (macOS: `log stream`, only the lines with its tag; Linux: Landlock's
/// audit records of its domain — `linux::Audit`), gathered on a thread of its own; stopped when the run is
/// done (`finish`), when its process is gone (`run_started`), when it never starts (`WATCH_UNCLAIMED`), or
/// when the last holder lets go.
pub struct Watch {
    tag: String,
    child: Mutex<Option<std::process::Child>>,
    blocked: std::sync::Arc<Mutex<Blocked>>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Its run started (`run_started`).
    claimed: std::sync::atomic::AtomicBool,
    /// Its run's own process is gone: what the run left running tells the project's next run since (`tell`).
    ended: std::sync::atomic::AtomicBool,
    /// Linux: the thread that confined its run (`owned_by`) — its audit records name it.
    #[cfg(target_os = "linux")]
    owner: std::sync::Arc<Mutex<Option<linux::Owner>>>,
    /// Linux: the threads that confined its project's earlier runs — what those left running is told here
    /// (`also_owned_by`).
    #[cfg(target_os = "linux")]
    also: std::sync::Arc<Mutex<Vec<linux::Owner>>>,
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
        let w = std::sync::Arc::new(Watch {
            tag: tag.to_string(),
            child: Mutex::new(None),
            blocked: Default::default(),
            stopped: Default::default(),
            claimed: Default::default(),
            ended: Default::default(),
            #[cfg(target_os = "linux")]
            owner: Default::default(),
            #[cfg(target_os = "linux")]
            also: Default::default(),
        });
        LIVE.lock().unwrap_or_else(|e| e.into_inner()).push(std::sync::Arc::downgrade(&w));
        w
    }

    /// Linux: its run was confined by thread `owner` (`spawn`).
    #[cfg(target_os = "linux")]
    pub fn owned_by(&self, owner: linux::Owner) {
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(owner);
    }

    /// Linux: what the runs confined by `owners` (its project's earlier ones) left running is told here too —
    /// Landlock's records name the thread that confined the run that started it.
    #[cfg(target_os = "linux")]
    pub fn also_owned_by(&self, owners: Vec<linux::Owner>) {
        *self.also.lock().unwrap_or_else(|e| e.into_inner()) = owners;
    }

    /// It ends itself unless a run has started under it (`run_started`) `within` from now.
    pub fn unless_claimed_within(self: &std::sync::Arc<Self>, within: std::time::Duration) {
        let w = std::sync::Arc::downgrade(self);
        crate::event::thread("tarae-watch", move || {
            std::thread::sleep(within);
            if let Some(w) = w.upgrade().filter(|w| !w.claimed.load(std::sync::atomic::Ordering::SeqCst)) {
                w.fail(format!("its run didn't start within {}s", within.as_secs()));
                w.finish();
            }
        });
    }

    /// Its run started as `pid` (the run's own process — `spawn::setsid`'s): watched until that's gone and
    /// reaped (by the run's thread), its last denials given `WATCH_LAST` — however the run ends, and
    /// whoever still holds the watch.
    pub fn run_started(self: &std::sync::Arc<Self>, pid: u32) {
        self.claimed.store(true, std::sync::atomic::Ordering::SeqCst);
        let w = std::sync::Arc::downgrade(self);
        crate::event::thread("tarae-watch", move || {
            // SAFETY: signal 0 only asks whether the process is there.
            let there = || unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
            let stopped = |w: &std::sync::Weak<Watch>| {
                w.upgrade().is_none_or(|w| w.stopped.load(std::sync::atomic::Ordering::SeqCst))
            };
            while there() {
                std::thread::sleep(WATCH_LOOKS);
                if stopped(&w) {
                    return;
                }
            }
            if let Some(w) = w.upgrade() {
                w.ended.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            std::thread::sleep(WATCH_LAST);
            if let Some(w) = w.upgrade() {
                w.finish();
            }
        });
    }

    /// Starts `log stream` for `tag` (on a worker: it returns at once, but give it `WATCH_READY` before the
    /// run starts). When it can't run or ends early, `blocked().failed` says why.
    #[cfg(not(target_os = "linux"))]
    pub fn start(tag: &str) -> std::sync::Arc<Watch> {
        let w = Watch::quiet(tag);
        let predicate = format!("sender == \"Sandbox\" AND eventMessage CONTAINS \"{tag}\"");
        let mut c = Command::new("/usr/bin/log");
        c.args(["stream", "--style", "ndjson", "--predicate", &predicate]);
        let tag = tag.to_string();
        w.read(c, move |line| parse_denial(line, &tag).into_iter().collect());
        w
    }

    /// Linux: Landlock's audit records, read from `source` (where `linux::audit_readable` found them), its
    /// run's domain told by the thread that confined it (`owned_by`). As `start`.
    #[cfg(target_os = "linux")]
    pub fn start_audit(tag: &str, source: &linux::AuditSource) -> std::sync::Arc<Watch> {
        Watch::start_audit_from(tag, source.stream())
    }

    /// `start_audit` reading `stream` (a stand-in for `journalctl` in tests).
    #[cfg(target_os = "linux")]
    pub fn start_audit_from(tag: &str, stream: Command) -> std::sync::Arc<Watch> {
        let w = Watch::quiet(tag);
        let (owner, also, mut audit) = (w.owner.clone(), w.also.clone(), linux::Audit::default());
        w.read(stream, move |line| {
            audit.owner = owner.lock().unwrap_or_else(|e| e.into_inner()).clone();
            audit.also(also.lock().unwrap_or_else(|e| e.into_inner()).clone());
            audit.line(line)
        });
        w
    }

    /// Reads what `cmd` prints, line by line, on a thread of its own: each line's denials (`parse`) are the
    /// run's. Its child is the watch's; one that can't start or ends early says why in `blocked().failed`.
    fn read(
        self: &std::sync::Arc<Self>,
        mut cmd: Command,
        mut parse: impl FnMut(&str) -> Vec<Denial> + Send + 'static,
    ) {
        let name = cmd.get_program().to_string_lossy().into_owned();
        let spawned = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return self.fail(format!("{name}: {e}")),
        };
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let (blocked, stopped) = (self.blocked.clone(), self.stopped.clone());
        crate::event::thread("tarae-deny-read", move || {
            use std::io::{BufRead, Read};
            for line in std::io::BufReader::new(stdout.expect("piped")).lines().map_while(Result::ok) {
                for d in parse(&line) {
                    blocked.lock().unwrap_or_else(|e| e.into_inner()).add(d);
                }
            }
            // Ended without being stopped: why, from what it said
            if !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                let mut said = String::new();
                if let Some(mut e) = stderr {
                    let _ = e.read_to_string(&mut said);
                }
                let why = said.lines().find(|l| !l.trim().is_empty()).unwrap_or("it ended").trim();
                blocked.lock().unwrap_or_else(|e| e.into_inner()).failed = Some(format!("{name}: {why}"));
            }
        });
        self.unless_claimed_within(WATCH_UNCLAIMED);
    }

    /// A watch whose child is `program args` — a stand-in for `log stream` (tests; killed with it).
    #[cfg(test)]
    pub fn spawned_for_test(program: &str, arg: &str) -> std::sync::Arc<Watch> {
        let w = Watch::quiet("test.");
        let child = Command::new(program).arg(arg).stdin(Stdio::null()).spawn().unwrap();
        *w.child.lock().unwrap() = Some(child);
        w
    }

    /// A denial told by what refused it — Linux's supervisor (`supervisor::refuse`), tests.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

    /// Whether its run's process is gone (or it was told to stop).
    pub fn ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
            || self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The run is done: no more watching (its last denials have had `WATCH_LAST`).
    pub fn finish(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            crate::event::thread("tarae-reap", move || c.wait());
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.finish();
    }
}

// ── What runs leave running (Linux — `reaper`) ──────────────────────────

/// A run of Claude's, kept after it's gone for what it left running: whose it was (its project — a session's
/// checkout), its policy's key (`policy_key`), its own process, its watch, and (Linux) the thread that
/// confined it — Landlock's records of what it left name that thread.
struct Ran {
    tag: String,
    root: PathBuf,
    key: String,
    pid: u32,
    watch: Option<std::sync::Weak<Watch>>,
    #[cfg(target_os = "linux")]
    owner: Option<linux::Owner>,
}

/// The runs since tarae started, oldest first — `RAN_KEPT` at most.
static RAN: Mutex<Vec<Ran>> = Mutex::new(Vec::new());
const RAN_KEPT: usize = 256;

/// What the supervisor refused what a project's runs left, told while none of its runs was going: for the next
/// one's watch — `PENDING_KEPT` a project.
static PENDING: Mutex<Vec<(PathBuf, Denial)>> = Mutex::new(Vec::new());
const PENDING_KEPT: usize = 32;

/// Run `pid` in `p` started: kept by its tag. What its project's earlier runs left and was refused while none
/// went is its watch's now; what they left under another policy (a grant since, a setting) is ended — it
/// holds the old one (on a thread of its own).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn ran(p: &Policy, pid: u32) {
    let Some(tag) = p.tag.clone() else { return };
    let key = policy_key(p);
    if let Some(w) = &p.watch {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).retain(|(root, d)| match *root == p.root {
            true => {
                w.record(d.clone());
                false
            }
            false => true,
        });
    }
    let mut ran = RAN.lock().unwrap_or_else(|e| e.into_inner());
    ran.push(Ran {
        tag,
        root: p.root.clone(),
        key: key.clone(),
        pid,
        watch: p.watch.as_ref().map(std::sync::Arc::downgrade),
        #[cfg(target_os = "linux")]
        owner: p.watch.as_ref().and_then(|w| w.owner.lock().unwrap_or_else(|e| e.into_inner()).clone()),
    });
    let n = ran.len();
    ran.drain(..n.saturating_sub(RAN_KEPT));
    drop(ran);
    let root = p.root.clone();
    crate::event::thread("tarae-end", move || {
        end_left(|r| r.root == root && r.key != key);
    });
}

/// The threads that confined the earlier runs of `root`'s project (Linux) — what they left is a new run's
/// watch's to tell too (`Watch::also_owned_by`).
#[cfg(target_os = "linux")]
fn earlier_owners(root: &Path) -> Vec<linux::Owner> {
    let ran = RAN.lock().unwrap_or_else(|e| e.into_inner());
    ran.iter().filter(|r| r.root == root).filter_map(|r| r.owner.clone()).collect()
}

/// A refusal of something run `p` started (Linux's supervisor): its own run's watch while that run goes;
/// after it, the newest run of its project still going — what a run left is the session's current run's to
/// say now; none going: kept for the next (`PENDING`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn tell(p: &Policy, d: Denial) {
    if let Some(w) = p.watch.as_ref().filter(|w| !w.ended()) {
        return w.record(d);
    }
    let current = RAN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .filter(|r| r.root == p.root)
        .find_map(|r| r.watch.as_ref()?.upgrade().filter(|w| !w.ended()));
    match current {
        Some(w) => w.record(d),
        None => {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            if pending.iter().filter(|(r, _)| *r == p.root).count() < PENDING_KEPT {
                pending.push((p.root.clone(), d));
            }
        }
    }
}

/// A process a run of this tarae's left running: tarae's child now (`reaper`), its tag (`MARK`) naming the
/// run — and that run's project, when tarae still knows it (not one from before a `:restart`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Left {
    pub pid: u32,
    pub tag: String,
    pub root: Option<PathBuf>,
}

/// Whether `tag` is one of this tarae's runs' (`RunDir::tag`: `tarae-<pid>-<n>.`).
fn ours(tag: &str) -> bool {
    tag.strip_prefix(&format!("tarae-{}-", std::process::id())).is_some_and(|n| n.ends_with('.'))
}

/// The project of the run tagged `tag` — None: not one tarae knows (`:procs` names what a run left by it).
pub fn ran_root(tag: &str) -> Option<PathBuf> {
    let ran = RAN.lock().unwrap_or_else(|e| e.into_inner());
    ran.iter().find(|r| r.tag == tag).map(|r| r.root.clone())
}

/// The run of this tarae's whose tag process `pid`'s environment carries: (the tag, its project when known) —
/// None: no run's. Reads `/proc` (a worker's).
pub fn left_by(pid: u32) -> Option<(String, Option<PathBuf>)> {
    let tag = crate::reaper::tag_of(pid).filter(|t| ours(t))?;
    let root = ran_root(&tag);
    Some((tag, root))
}

/// What this tarae's runs left running: its children carrying one of its runs' tags, but those runs' own
/// processes (a run going, or just ended and not yet reaped). Reads `/proc` (a worker's).
pub fn leftovers() -> Vec<Left> {
    let me = std::process::id();
    let own: Vec<u32> = RAN.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|r| r.pid).collect();
    crate::procs::children(me)
        .into_iter()
        .filter(|pid| !own.contains(pid))
        .filter(|&pid| crate::procs::read(pid).is_some_and(|o| !o.zombie))
        .filter_map(|pid| left_by(pid).map(|(tag, root)| Left { pid, tag, root }))
        .collect()
}

/// How long what's ended gets for its TERM before KILL.
const LEFT_TERM: std::time::Duration = std::time::Duration::from_secs(1);

/// Ends what the runs `which` picks left running (`reaper::end`) — looked for again a few times: a process
/// it started under another group comes to tarae once its parent is gone. Blocks up to a few seconds. The
/// pids ended.
fn end_left(which: impl Fn(&Ran) -> bool) -> Vec<u32> {
    let mut ended = Vec::new();
    for _ in 0..3 {
        let left = leftovers();
        let picked: Vec<u32> = {
            let ran = RAN.lock().unwrap_or_else(|e| e.into_inner());
            let pick = |l: &Left| ran.iter().find(|r| r.tag == l.tag).is_some_and(&which);
            left.into_iter().filter(|l| pick(l)).map(|l| l.pid).collect()
        };
        if picked.is_empty() {
            break;
        }
        ended.extend(crate::reaper::end(&picked, LEFT_TERM));
    }
    if !ended.is_empty() {
        crate::log::line("sandbox", "info", &format!("ended what runs left running: {ended:?}"));
    }
    ended
}

/// Ends what Claude's runs left running — of the project at `root` (its session ended), or all of it (tarae
/// quits; one from before a `:restart` too). Blocks up to a few seconds — a worker's, or tarae's end.
pub fn end_leftovers(root: Option<&Path>) -> Vec<u32> {
    match root {
        Some(root) => end_left(|r| r.root == root),
        None => {
            let mut ended = end_left(|_| true);
            // (a run tarae doesn't know any more — before a `:restart`)
            let unknown: Vec<u32> =
                leftovers().into_iter().filter(|l| l.root.is_none()).map(|l| l.pid).collect();
            ended.extend(crate::reaper::end(&unknown, LEFT_TERM));
            ended
        }
    }
}

/// What a policy allows, as a key: the same project and the same grants and settings give the same key —
/// what's the run's own (its folder, tag, watch, rules, the environment made from it) left out. What a run
/// left under one key holds that policy: a run under another ends what the old one left (`ran`), and a home a
/// daemon lives from is the key's own (`with_kept`).
pub fn policy_key(p: &Policy) -> String {
    let mut q = p.clone();
    (q.run, q.tag, q.watch, q.env) = (None, None, None, Vec::new());
    // (the kept homes are the project's and the key's — granted by them, not part of what's compared)
    q.write.retain(|w| !p.kept.iter().any(|k| k.home == *w));
    q.kept = Vec::new();
    #[cfg(target_os = "linux")]
    {
        q.confine = None;
    }
    let mut text = format!("{q:?}");
    if let Some(r) = &p.run {
        text = text.replace(&r.path().display().to_string(), "$RUN");
    }
    crate::sha256::hex(text.as_bytes())[..16].to_string()
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
    // (nothing denied yet: what the checkout's build tool config named and stays closed — #20)
    let Some(first) = b.seen.first() else {
        let Guard::In(p) = g else { return None };
        let all = p.refused.len() + p.refused_asked.len();
        let more = if all > 1 { format!("  +{}", all - 1) } else { String::new() };
        if let Some((d, _)) = p.refused.first() {
            let (d, by) = (crate::allow::tilde(d), &p.named_by);
            return Some((format!("sandbox kept {d} closed — {by}'s config named it{more}"), None));
        }
        let (d, which) = p.refused_asked.first()?;
        let (d, which) = (crate::allow::tilde(d), which.split(" — ").next().unwrap_or(which));
        return Some((format!("sandbox kept {d} closed — {which} named it{more}"), None));
    };
    let more = if b.distinct > 1 { format!("  +{}", b.distinct - 1) } else { String::new() };
    let quiet = (running && last_output.elapsed() >= QUIET).then(|| {
        format!("quiet {}s — it may be waiting on what was blocked", last_output.elapsed().as_secs())
    });
    Some((format!("sandbox blocked {}{more}", first.plain()), quiet))
}

/// The paths a line of output names — words that start `/`, `~/` (home), `./` or `../` (from `root`), with
/// quotes, brackets and a closing `:` or `.` taken off — as the profile sees them, worked out without the
/// disk (the main thread asks): `.` and `..` folded, `/tmp`, `/var` and `/etc` in `/private`. Not the
/// devices (every run writes `/dev/null`).
fn paths_in(line: &str, root: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let word = |w: &str| -> Option<PathBuf> {
        match w {
            // (not `/` alone, nor `//` — a comment in a quoted line of code)
            w if w.starts_with('/') && !w.trim_matches('/').is_empty() => Some(PathBuf::from(w)),
            w if w.starts_with("./") || w.starts_with("../") => Some(root.join(w)),
            w => Some(home.as_ref()?.join(w.strip_prefix("~/")?)),
        }
    };
    let folded = |p: PathBuf| {
        let mut out = PathBuf::from("/");
        for c in p.components() {
            match c {
                Component::Normal(n) => out.push(n),
                Component::ParentDir => {
                    out.pop();
                }
                _ => {}
            }
        }
        let private = ["tmp", "var", "etc"].iter().any(|d| out.starts_with(Path::new("/").join(d)));
        match private && cfg!(target_os = "macos") {
            true => Path::new("/private").join(out.strip_prefix("/").unwrap_or(&out)),
            false => out,
        }
    };
    line.split(|c: char| c.is_whitespace() || "`'\"()[]{}<>,;=".contains(c))
        .filter_map(|w| word(w.trim_end_matches([':', '.'])))
        .map(folded)
        .filter(|p| !p.starts_with("/dev"))
        .collect()
}

/// The relative names a line of output gives files by — quoted (`'src/x'`, `‘x’`, `"x"`), or `sh`'s `cannot
/// create x:` — as paths from `root` (where the run is, as a rule), folded. No disk.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // (Landlock's — `landlock_said_in`)
fn named_in(line: &str, root: &Path) -> Vec<PathBuf> {
    let mut names: Vec<&str> = Vec::new();
    for (open, close) in [('\'', '\''), ('‘', '’'), ('"', '"')] {
        let mut rest = line;
        while let Some(a) = rest.find(open) {
            let after = &rest[a + open.len_utf8()..];
            let Some(b) = after.find(close) else { break };
            names.push(&after[..b]);
            rest = &after[b + close.len_utf8()..];
        }
    }
    if let Some((name, _)) = line.split_once("cannot create ").and_then(|(_, after)| after.split_once(": ")) {
        names.push(name);
    }
    let relative = |n: &&str| !n.is_empty() && !n.starts_with(['/', '~']) && !n.contains(char::is_whitespace);
    names
        .into_iter()
        .filter(relative)
        .map(|n| {
            let mut out = PathBuf::new();
            for c in root.join(n).components() {
                match c {
                    Component::ParentDir => {
                        out.pop();
                    }
                    Component::CurDir => {}
                    other => out.push(other),
                }
            }
            out
        })
        .collect()
}

/// Lines of output that say a permission was refused — what's left to go by when nothing watched.
pub fn refused_lines(output: &[(bool, String)]) -> Vec<String> {
    output.iter().filter(|(_, l)| refusal(l)).take(5).map(|(_, l)| l.clone()).collect()
}

/// Whether a line says the sandbox may have refused something: "Operation not permitted" (Seatbelt's, and
/// seccomp's on Linux), and on Linux "Permission denied" (Landlock's).
fn refusal(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("operation not permitted") || (cfg!(target_os = "linux") && l.contains("permission denied"))
}

/// The names every project keeps, whatever its languages: tarae's project config (its trusted settings and
/// steps), direnv's, Claude's, the CI's and VS Code's settings and tasks, `.tarae.local.toml` (read as the
/// user's own config, its preflight steps run unsandboxed: `config::personal`) and `.git`. The ecosystems'
/// own — `build.rs`, `package.json`, `pom.xml` … — are their packs' (`packs::protect`).
const CORE_NAMES: [&str; 7] =
    [".tarae.toml", ".envrc", ".claude", ".github", ".vscode", ".tarae.local.toml", ".git"];

/// What Claude's runs never write in the project, wherever the name sits in it (a crate's `build.rs`, a
/// folder's `.cargo`) — what runs later outside the sandbox (the user's builds, `:preflight`, the terminal,
/// a language server importing the project) or sets how things run: every pack's `protect` and the core's
/// (`CORE_NAMES`), whatever the project's languages, read once from the binary — nothing configured takes
/// one out. Claude changes them with Edit, which always asks (`edits::listed`). Names, not paths — static in
/// the profile — in any case: APFS opens `Build.rs` for `build.rs`, and so does cargo. One of several parts
/// (`gradle/wrapper`) is those names in a row.
pub fn protected_names() -> impl Iterator<Item = &'static str> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let mut v = crate::packs::protect();
        v.extend(CORE_NAMES.iter().map(|n| n.to_string()));
        let mut seen = std::collections::HashSet::new();
        v.retain(|n| seen.insert(n.to_ascii_lowercase()));
        v
    });
    names.iter().map(String::as_str)
}

/// Where a protected name (`protected_names`) starts among `parts` (a path's, from the project's root) —
/// each name's parts matched in a row, in any case.
pub fn protected_at(parts: &[&std::ffi::OsStr]) -> Vec<usize> {
    let mut at = Vec::new();
    for name in protected_names() {
        let want: Vec<&str> = name.split('/').collect();
        for (i, w) in parts.windows(want.len()).enumerate() {
            if w.iter().zip(&want).all(|(p, n)| p.eq_ignore_ascii_case(n)) {
                at.push(i);
            }
        }
    }
    at.sort_unstable();
    at.dedup();
    at
}

/// Folders of tools' output (the built-in packs' `outputs`), where those names are what the tool writes:
/// cmake's `Makefile`s and `cargo package`'s `Cargo.toml`s under `target/`, each package's `package.json`
/// under `node_modules/`, a virtualenv's own. What's there runs later anyway (build scripts, packages), so
/// keeping the names closes nothing there — a name below one of these is let through
/// (`.git/modules/target/hooks` isn't: the name comes first).
pub fn outputs() -> &'static [String] {
    &crate::languages::get().outputs
}

/// A name as a Seatbelt regex, as it's written: `\.venv`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn as_written(name: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    name.chars().map(|c| if plain(c) { c.to_string() } else { format!("\\{c}") }).collect()
}

/// The longest run of names (`a|b|…`) in one regex of the profile. Seatbelt refuses a string literal past
/// about a kilobyte — "Error reading string", exit 65 (measured: 1032 characters refused, 578 taken) — and
/// every pack's names in any case are past it, so they go in several regexes of this much at most.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
const NAMES_PER_REGEX: usize = 400;

/// The protected names (`protected_names`) as Seatbelt regex alternatives in any case, a name of several
/// parts joined with `/`, split in runs of at most `NAMES_PER_REGEX` characters.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn name_regexes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in protected_names() {
        let n = n.split('/').map(any_case).collect::<Vec<_>>().join("/");
        match out.last_mut() {
            Some(last) if last.len() + 1 + n.len() <= NAMES_PER_REGEX => {
                last.push('|');
                last.push_str(&n);
            }
            _ => out.push(n),
        }
    }
    out
}

/// A name as a Seatbelt regex in any case: `[bB][uU][iI][lL][dD]\.[rR][sS]`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
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
/// (`protected_names`, `outputs`) — the profile's rule, in Rust: the report names what it denied.
pub fn protected_in(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return false };
    let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
    let at = protected_at(&parts);
    let output = parts.iter().position(|p| outputs().iter().any(|o| *p == o.as_str()));
    !at.is_empty() && !output.is_some_and(|o| at.iter().any(|&i| i > o))
}

/// Device files every program writes to (`/dev/null`, terminals — a pty test hangs without them: measured), as
/// the profile's filters (`devices_rule`): (filter, its argument).
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
const DEVICES: [(&str, &str); 7] = [
    ("literal", "/dev/null"),
    ("literal", "/dev/zero"),
    ("literal", "/dev/tty"),
    ("literal", "/dev/ptmx"),
    ("literal", "/dev/dtracehelper"),
    ("regex", "^/dev/ttys[0-9]+$"),
    ("subpath", "/dev/fd"),
];

/// The profile's rule for `DEVICES`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn devices_rule() -> String {
    let filters: String = DEVICES
        .iter()
        .map(|(f, a)| match *f {
            "regex" => format!(" (regex #\"{a}\")"),
            f => format!(" ({f} \"{a}\")"),
        })
        .collect();
    format!("(allow file-write*{filters})\n")
}

/// The device places a run writes (`Policy::write_places`): the files and folders the platform's rules name
/// (Seatbelt's `DEVICES` but its patterns — terminals' files in `/dev`; Landlock's `linux::DEVICES`).
#[cfg(not(target_os = "linux"))]
static DEVICE_PLACES: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| DEVICES.iter().filter(|(f, _)| *f != "regex").map(|(_, a)| *a).collect());
#[cfg(target_os = "linux")]
static DEVICE_PLACES: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| linux::DEVICES.iter().map(|(d, _)| *d).collect());

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

/// The keychain's servers: the only lookup `security find-generic-password` makes (measured) — the keychain
/// files themselves are kept (`allow::SECRETS`).
const KEYCHAIN: [&str; 2] = ["com.apple.SecurityServer", "com.apple.securityd"];

/// What a run never reaches: the clipboard (`pbpaste` reads it — measured, exit 0 open, 1 denied) and the
/// keychain's servers. No toolchain looks either up (measured: preflight, Go, Node, a shot, e2e).
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn kept_services() -> String {
    let keychain: String = KEYCHAIN.iter().map(|k| format!(" (global-name \"{k}\")")).collect();
    format!("mach-lookup (global-name-regex #\"^com\\.apple\\.pasteboard\\.\"){keychain}")
}

/// Ways to get something run outside: Apple Events to other apps, LaunchServices (`open`).
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
const NO_WAY_OUT: [&str; 2] = [
    "appleevent-send",
    "mach-lookup (global-name \"com.apple.coreservices.launchservicesd\") (global-name-regex #\"^com\\.apple\\.lsd\\.\")",
];

impl Policy {
    /// The Seatbelt profile. Paths aren't in it: they're `-D` parameters (`params`), so no path can change
    /// what it says. Every deny carries the run's tag (`tag`) — the only text from outside, and tarae's own.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
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
        // Reads in home (card model P2): the project, where it writes, and what's allowed (`Reads`). A hard
        // link's source must be readable too — Seatbelt's own `forbidden-link-priv<file-read-data>`, measured
        // 2026-10-05 with `link(2)` itself, `target/sb/p2-links2.sh` — so nothing a run can't read comes into
        // the project by a link, though its folder be writable: a `file-link` rule would add nothing (its
        // places would be these write rules') and costs every build a denial per object file rustc links
        if self.reads.home.is_some() {
            let files: String =
                (0..self.files.len()).map(|i| format!(" (literal (param \"F{i}\"))")).collect();
            let reads: String =
                (0..self.reads.allow.len()).map(|i| format!(" (subpath (param \"R{i}\"))")).collect();
            p.push_str(&deny("file-read-data file-read-xattr (subpath (param \"HOME\"))"));
            p.push_str(&format!(
                "(allow file-read-data file-read-xattr (subpath (param \"ROOT\")){run}{dirs}{files}{reads})\n"
            ));
        }
        // After the allows, so these win. The names a run keeps off, below the project's root however deep
        // — the regex starts with as many path parts as the root has (no path in it), so a name in the
        // root's own path (a checkout under `.claude/worktrees`) doesn't close the whole project
        let depth = self.root.components().filter(|c| matches!(c, Component::Normal(_))).count();
        let below = format!("^{}/(.*/)?", "/[^/]+".repeat(depth));
        let outputs = outputs().iter().map(|o| as_written(o)).collect::<Vec<_>>().join("|");
        let (mut named, mut in_output) = (String::new(), String::new());
        for names in name_regexes() {
            named.push_str(&format!(" (regex #\"{below}({names})(/|$)\")"));
            in_output.push_str(&format!(" (regex #\"{below}({outputs})/(.*/)?({names})(/|$)\")"));
        }
        p.push_str(&deny(&format!(
            "file-write* (require-all (subpath (param \"ROOT\")) (require-any{named}) \
             (require-not (require-any{in_output})))"
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
        // …and the mark cargo puts on its caches at every run, on its own folders themselves
        let mut xattrs: Vec<(&str, String)> = Vec::new();
        for c in &self.caches_said {
            let Some(x) = c.xattr.as_deref() else { continue };
            let at = (0..self.caches.len()).filter(|&i| c.dirs.contains(&self.caches[i]));
            let at: String = at.map(|i| format!(" (literal (param \"C{i}\"))")).collect();
            match xattrs.iter_mut().find(|(y, _)| *y == x) {
                Some((_, all)) => all.push_str(&at),
                None => xattrs.push((x, at)),
            }
        }
        for (x, caches) in xattrs.iter().filter(|(_, c)| !c.is_empty()) {
            p.push_str(&format!(
                "(allow file-write-xattr (require-all (require-any{caches}) (xattr \"{x}\")))\n"
            ));
        }
        p.push_str(&devices_rule());
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
        p.push_str(&deny(&kept_services()));
        // The settings that hold a token, each that file: the tools read their copies (`tokens.rs`)
        let hidden: String =
            (0..self.reads.copies.hidden.len()).map(|i| format!(" (literal (param \"T{i}\"))")).collect();
        if !hidden.is_empty() {
            p.push_str(&deny(&format!("file-read-data file-read-xattr{hidden}")));
        }
        // Last, so nothing granted opens them: what no run reads — but the project, when one holds it (a
        // checkout under `~/.claude/worktrees`), and tarae's log. (Their metadata stays open, as everywhere:
        // tarae's own tests look at its state folder — measured; a hard link needs the data's read anyway)
        let (around, other) = self.kept_split();
        let each = |v: &[usize]| v.iter().map(|i| format!(" (subpath (param \"X{i}\"))")).collect::<String>();
        if !other.is_empty() {
            p.push_str(&deny(&format!("file-read-data file-read-xattr{}", each(&other))));
        }
        if !around.is_empty() {
            p.push_str(&deny(&format!(
                "file-read-data file-read-xattr (require-all (require-any{}) (require-not (subpath (param \
                 \"ROOT\"))))",
                each(&around)
            )));
        }
        if self.reads.log.is_some() && !self.reads.kept.is_empty() {
            p.push_str("(allow file-read-data file-read-xattr (literal (param \"LOG\")))\n");
        }
        p
    }

    /// Linux: how the report says the network where it differs from macOS's words — tarae's supervisor
    /// serving: as macOS has it, but nothing beyond this machine (`None`); not serving here: no socket at all,
    /// this machine's ports and the project's unix sockets included, and why — and what Landlock can't keep
    /// (`linux.rs`). None elsewhere.
    pub fn linux_said(&self) -> Option<(Option<String>, String)> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            let asked = self.network || self.localhost || !self.ports.is_empty() || !self.sockets.is_empty();
            let net = c.serve.as_ref().err().map(|why| {
                format!(
                    ", and make no socket: no network, not even this machine's ports, and no unix socket — not \
                     even in the project or its own folder, which macOS allows (tarae can't serve them here: \
                     {why}){}",
                    if asked {
                        " — what llm.sandbox-network, -localhost, -ports or a socket grant open, too"
                    } else {
                        ""
                    }
                )
            });
            let beyond = match c.serve.is_ok() && self.network {
                true => " On Linux llm.sandbox-network doesn't open the network beyond this machine yet.",
                false => "",
            };
            let notes = format!(
                " On Linux a protected name it makes where none is yet isn't refused (the user is asked about \
                 it before your next run), and the terminal multiplexers' folders in /tmp stay writable.\
                 {beyond}{}{}{}",
                linux::notes(c.abi),
                linux::made_said(&c.made, &self.root),
                linux::kept_said(&c.kept, &self.root)
            );
            return Some((net, notes));
        }
        None
    }

    /// Linux without Landlock's audit log: what a run was refused in files is read from its own error lines
    /// only, though its watch hears what the supervisor refused.
    fn files_unwatched(&self) -> bool {
        #[cfg(target_os = "linux")]
        return matches!(&self.confine, Some(Ok(c)) if c.audit.is_err());
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// Linux: why what a run refused was read from its own error lines — why there's no audit log, or that
    /// the log didn't have it (Landlock's own rule for new entries isn't the policy's; a signal).
    fn output_read_why(&self) -> String {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return match &c.audit {
                Ok(_) => "Landlock's audit log didn't have it".into(),
                Err(why) => why.clone(),
            };
        }
        String::new()
    }

    /// Linux: where Landlock's audit log is read here (`linux::AuditSource::said`) — None without one, and
    /// elsewhere.
    fn audit_log(&self) -> Option<&'static str> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return c.audit.as_ref().ok().map(linux::AuditSource::said);
        }
        None
    }

    /// The report's words on what only the run's output says was refused (`o`, from `said_in_output`): tarae
    /// has no record of it, so it's the output's — the sandbox's report of a denial gets lost now and then, but
    /// such a line may be the command's own (a hit quoting the phrase, a refusal inside a container or a VM).
    /// `also`: after what tarae saw blocked. Empty when the output says nothing.
    fn output_says(&self, o: &Blocked, also: bool) -> String {
        if o.total == 0 {
            return String::new();
        }
        let (what, was, it, these) = match o.distinct {
            1 => ("one thing".to_string(), "was", "it", "that's"),
            n => (format!("{n} things"), "were", "them", "those are"),
        };
        let mut s = format!(
            "Its output {}says {what} {was} refused where the sandbox's rules keep a command out — tarae has no \
             record of {it}, so {these} the output's words, not a refusal seen: ",
            if also { "also " } else { "" },
        );
        s.push_str(match self.denials_from() {
            // (Linux without the audit log: the error lines are all there is to go by)
            Some(_) => "likely the sandbox's, though a line may be the command's own:\n",
            None => {
                "the sandbox's report of a denial gets lost now and then, but the line may be the command's \
                 own (a hit quoting the phrase, a refusal inside a container or VM):\n"
            }
        });
        for d in &o.seen {
            s.push_str(&format!("  {}\n", d.reported()));
        }
        if o.distinct > o.seen.len() {
            s.push_str(&format!("  … and {} more\n", o.distinct - o.seen.len()));
        }
        if self.denials_from().is_some() {
            s.push_str(&format!(
                "(Read from its own error lines — {}, judged by the sandbox's rules.)\n",
                self.output_read_why()
            ));
        }
        s
    }

    /// Linux: where what a run refused is told from — Landlock's audit log, or the run's own error lines and
    /// why. None elsewhere (macOS's log, always).
    pub fn denials_from(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return Some(match &c.audit {
                Ok(_) => "from Landlock's audit log".into(),
                Err(why) => format!("from its own error lines — {why}"),
            });
        }
        None
    }

    /// The kept places (`Reads::kept`, by index) that hold the project, and the others.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
    fn kept_split(&self) -> (Vec<usize>, Vec<usize>) {
        (0..self.reads.kept.len()).partition(|&i| self.root.starts_with(&self.reads.kept[i]))
    }

    /// Whether the profile lets a run read `real` — its rules, in Rust.
    pub fn reads_allow(&self, real: &Path) -> bool {
        let r = &self.reads;
        // (the log: opened after everything, when anything is kept)
        if r.log.as_deref() == Some(real) && !r.kept.is_empty() {
            return true;
        }
        if r.kept_holds(&self.root, real) || r.copies.instead_of(real).is_some() {
            return false;
        }
        match &r.home {
            Some(home) if real.starts_with(home) => {
                real.starts_with(&self.root)
                    || self.run.as_ref().is_some_and(|d| real.starts_with(d.path()))
                    || self.write.iter().chain(&r.allow).any(|w| real.starts_with(w))
                    || self.files.iter().any(|f| f == real)
            }
            _ => true,
        }
    }

    /// Every place a run under this policy may write, folders and files — what its rules open, from the fields
    /// the profile (`args`) and Landlock's rules (`linux::confine`) are made from: the project, `write`, `files`,
    /// a run's own folder (any run's: where `RunDir` makes them), the devices (`DEVICE_PLACES`). What's kept
    /// inside them (`protect`, `keep`, `caches`) counts as written too. Real paths.
    pub fn write_places(&self) -> Vec<PathBuf> {
        let run = self.run.as_ref().map(|r| r.path().to_path_buf());
        let mut all: Vec<PathBuf> = std::iter::once(self.root.clone())
            .chain(self.write.iter().cloned())
            .chain(self.files.iter().cloned())
            .chain([RunDir::base()])
            .chain(run)
            .chain(DEVICE_PLACES.iter().map(PathBuf::from))
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// Whether the profile lets a run write `real` — its rules, in Rust (the devices aside).
    pub fn writes_allow(&self, real: &Path) -> bool {
        if self.files.iter().any(|f| f == real) {
            return true;
        }
        if real.starts_with(&self.root) {
            return !protected_in(&self.root, real) && !self.protect.iter().any(|p| real.starts_with(p));
        }
        let open = self.run.as_ref().is_some_and(|d| real.starts_with(d.path()))
            || self.write.iter().any(|w| real.starts_with(w));
        open && !self.keep.iter().chain(&self.caches).any(|k| real.starts_with(k))
    }

    /// What the run's output says was refused where the profile denies it — the kernel's own report of a
    /// denial is lost now and then, at random (1 in 10, measured, `target/sb/p2-measured.md` §5): a path on
    /// a line that failed, at or just before a line saying `Operation not permitted` (cargo says the path,
    /// then `Caused by:`, then the error), that this profile doesn't let a run read or write. Those `seen`
    /// already said aren't again. Relative paths are from the project (`paths_in`: no disk).
    pub fn said_in_output(&self, output: &[(bool, String)], seen: &[Denial]) -> Vec<Denial> {
        const BEFORE: usize = 3;
        let refused = |l: &str| l.contains("ermi") && refusal(l);
        let failed = |l: &str| {
            let l = l.to_ascii_lowercase();
            let words = [
                "not permitted",
                "permission denied",
                "fail",
                "error",
                "cannot",
                "can't",
                "couldn't",
                "unable",
            ];
            words.iter().any(|w| l.contains(w))
        };
        let mut near = vec![false; output.len()];
        // (Landlock's own: "Permission denied" at or just after it — cargo says the path, then why)
        let mut denied = vec![false; output.len()];
        for (i, (_, l)) in output.iter().enumerate().filter(|(_, (_, l))| refused(l)) {
            near[i.saturating_sub(BEFORE)..=i].fill(true);
            if l.to_ascii_lowercase().contains("permission denied") {
                denied[i.saturating_sub(BEFORE)..=i].fill(true);
            }
        }
        let mut out: Vec<Denial> = Vec::new();
        let lines = output.iter().zip(near.iter().zip(&denied)).filter(|(_, (n, _))| **n);
        for (l, denied) in lines.map(|((_, l), (_, d))| (l, *d)).filter(|(l, _)| failed(l)) {
            for real in paths_in(l, &self.root) {
                let op = match (self.reads_allow(&real), self.writes_allow(&real)) {
                    (true, true) => continue,
                    (true, false) => "file-write*",
                    (false, _) => EITHER,
                };
                if seen.iter().chain(&out).any(|d| d.file() == Some(real.as_path())) {
                    continue;
                }
                let target = real.display().to_string();
                out.push(Denial { process: SEEN_IN_OUTPUT.into(), op: op.into(), target });
            }
            for d in self.landlock_said_in(l, denied) {
                if !seen.iter().chain(&out).any(|s| s.op == d.op && s.target == d.target) {
                    out.push(d);
                }
            }
        }
        out.truncate(DENIALS_KEPT);
        out
    }

    /// `said` (what the output says, `said_in_output`) put with `seen` (the kernel's): Landlock's record of a
    /// refused new file names only its folder (`file-make-in`) — the file the output names in that folder
    /// takes its place, the kernel's line kept; without one, the folder is said with Landlock's own rule
    /// where that's it (a folder holding protected names: `file-entry-make-in`), else as a write there. What
    /// of `said` is left.
    pub fn merge_made(&self, seen: &mut [Denial], mut said: Vec<Denial>) -> Vec<Denial> {
        for d in seen.iter_mut().filter(|d| d.op == "file-make-in") {
            let folder = PathBuf::from(&d.target);
            let named = said.iter().position(|s| {
                ["file-entry-make", "file-write*", EITHER].contains(&s.op.as_str())
                    && Path::new(&s.target).parent() == Some(folder.as_path())
            });
            match named {
                Some(i) => {
                    let s = said.remove(i);
                    (d.op, d.target) = (s.op, s.target);
                }
                None if self.closed_holds(&folder) => d.op = "file-entry-make-in".into(),
                None => d.op = "file-write*".into(),
            }
        }
        said
    }

    /// Linux: whether `folder` is one of the project's that take no new entry (`linux::Confine::closed`).
    fn closed_holds(&self, folder: &Path) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return c.closed.iter().any(|f| f == folder);
        }
        let _ = folder;
        false
    }

    /// Linux: what a line of output says Landlock refused though the rules above let it write — making,
    /// removing or moving an entry of a folder that holds a protected name, or one below it
    /// (`linux::Confine::closed`; "Permission denied" on it or, `denied`, just after it as cargo says it —
    /// by a path, or a name from where the run is, as `sh` says it); from ABI 6, a signal to a process
    /// outside the run's own ("Operation not permitted" from `kill`). None elsewhere.
    fn landlock_said_in(&self, line: &str, denied: bool) -> Vec<Denial> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            let low = line.to_ascii_lowercase();
            let mut out: Vec<Denial> = Vec::new();
            if denied || low.contains("permission denied") {
                let op = match () {
                    _ if low.contains("remove") || low.contains("unlink") => "file-entry-remove",
                    _ if low.contains("move") || low.contains("rename") => "file-entry-move",
                    _ => "file-entry-make",
                };
                for real in paths_in(line, &self.root).into_iter().chain(named_in(line, &self.root)) {
                    let closed = real.parent().is_some_and(|d| c.closed.iter().any(|f| f == d));
                    let target = real.display().to_string();
                    if closed && self.writes_allow(&real) && !out.iter().any(|d| d.target == target) {
                        out.push(Denial { process: SEEN_IN_OUTPUT.into(), op: op.into(), target });
                    }
                }
            }
            if c.abi >= 6 && low.contains("kill") && low.contains("not permitted") {
                let pid = line
                    .split(['(', ')'])
                    .nth(1)
                    .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
                out.push(Denial {
                    process: SEEN_IN_OUTPUT.into(),
                    op: "signal".into(),
                    target: pid.unwrap_or_default().into(),
                });
            }
            return out;
        }
        let _ = (line, denied);
        Vec::new()
    }

    /// The profile's parameters (name, value).
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
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
        let r = &self.reads;
        v.extend(r.home.iter().map(|h| ("HOME".to_string(), s(h))));
        v.extend(
            r.home.iter().flat_map(|_| r.allow.iter().enumerate().map(|(i, p)| (format!("R{i}"), s(p)))),
        );
        v.extend(r.kept.iter().enumerate().map(|(i, p)| (format!("X{i}"), s(p))));
        v.extend(r.copies.hidden.iter().enumerate().map(|(i, (p, _))| (format!("T{i}"), s(p))));
        v.extend(r.log.iter().filter(|_| !r.kept.is_empty()).map(|l| ("LOG".to_string(), s(l))));
        v
    }

    /// `sandbox-exec`'s arguments before the command.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
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
        let mut write = vec![PathBuf::from(TMP)];
        if let Some(t) = &env.tmp {
            write.push(user_temp(t));
        }
        // The folders the packs' `ask` names: the tool's own — but never where nothing is written, a shared cache
        // or a toolchain's home among it; its caches (Go's module cache) as the others, read — each judged but
        // for its own places, which the caches rule keeps (another toolchain's cache or home still refuses it)
        let mut refused_asked = Vec::new();
        let home = env.home.as_deref();
        let open = |d: &Path, which: &str, later: &RunsLater, refused: &mut Vec<(PathBuf, String)>| {
            let verdict = ask_dir_verdict(d, &root, home, later);
            verdict.map_err(|why| refused.push((d.to_path_buf(), format!("{which} — {why}")))).is_ok()
        };
        for (d, which) in &env.dirs {
            if open(d, which, &env.runs_later, &mut refused_asked) {
                write.push(d.clone());
            }
        }
        let mut reads = env.reads.clone();
        let mut caches_said = env.caches.clone();
        for (key, which, c) in &env.asked_caches {
            let but = env.runs_later.but_cache(key);
            if c.dirs.iter().all(|d| open(d, which, &but, &mut refused_asked)) {
                reads.allow.extend(
                    c.dirs.iter().filter(|d| reads.home.as_ref().is_some_and(|h| d.starts_with(h))).cloned(),
                );
                caches_said.push(c.clone());
            }
        }
        // The tools' caches: written as they download and unpack — with the network closed, kept (`caches`)
        let caches: Vec<PathBuf> = caches_said.iter().flat_map(|c| c.dirs.iter().cloned()).collect();
        if env.network {
            write.extend(caches.iter().cloned());
        }
        // The tools' homes kept for the project's runs (Go's build cache)
        write.extend(env.kept_homes.iter().map(|(_, h)| h.clone()));
        // The folders the checkout's build tool names as its config says: only its own (#20)
        let mut refused = Vec::new();
        for (d, which) in &env.asked {
            let (home, own) = (env.home.as_deref(), env.own_build.as_deref());
            match asked_dir_verdict(d, &root, home, &env.builder, own, env.tmp.as_deref()) {
                Ok(true) => write.push(d.clone()),
                Ok(false) => {}
                Err(why) if !refused.iter().any(|(r, _)| r == d) => {
                    refused.push((d.clone(), format!("{}'s {which} — {why}", env.builder.who)))
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
        let mut ports: Vec<u16> = env.ports.clone();
        ports.sort_unstable();
        ports.dedup();
        ports.truncate(PORTS_MAX);
        Policy {
            root,
            protect,
            write,
            files: env.files.clone(),
            keep: mux_dirs(env),
            caches: if env.network { Vec::new() } else { caches },
            caches_said: if env.network { Vec::new() } else { caches_said },
            network: env.network,
            ports,
            localhost: env.localhost,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused,
            refused_asked,
            named_by: env.builder.who.clone(),
            reads,
            env: Vec::new(),
            run_env: env.run_env.clone(),
            run_homes: env.run_homes.clone(),
            kept: Vec::new(),
            #[cfg(target_os = "linux")]
            confine: None,
        }
    }
}

/// Where tmux and zellij keep their sockets: tmux in `$TMUX_TMPDIR` (else `/tmp`) `/tmux-<uid>`, zellij in
/// `$ZELLIJ_SOCKET_DIR` (else `$TMPDIR/zellij-<uid>`). Real paths.
fn mux_dirs(env: &Env) -> Vec<PathBuf> {
    let tmux_base = env.tmux_tmpdir.as_deref().unwrap_or(Path::new(TMP));
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
    /// The tools' shared caches (the packs' `caches`: their folders in their homes, real, when there).
    pub caches: Vec<CachesSaid>,
    /// Single files a run writes: the caches' own files, the packs' `write-files` (`sandboxpack::filled`).
    pub files: Vec<PathBuf>,
    /// The toolchain's own folders (`toolchain_dirs`) — what the packs' `ask` names for writing (real paths, as
    /// said: `go's GOCACHE`), opened as `ask_dir_verdict` says; the build tool's apart.
    pub dirs: Vec<(PathBuf, String)>,
    /// The toolchain's shared caches it names (`ask`'s `caches` — Go's module cache): by key (`GOMODCACHE`), as
    /// said (`go's GOMODCACHE`) and as the pack says them — kept as `caches` are, once `ask_dir_verdict` lets
    /// them be.
    pub asked_caches: Vec<(String, String, CachesSaid)>,
    /// What no folder opened for writing is or holds (`ask_dir_verdict`).
    pub runs_later: RunsLater,
    /// The tools' homes kept across the project's runs (the packs' `run-home` with `keep` — `kept_homes`): the
    /// tool's name, the folder (real; made by the run that needs it).
    pub kept_homes: Vec<(String, PathBuf)>,
    /// The homes a tool's daemon lives from (`run-home`'s `daemon`), kept per project and policy: the tool's
    /// name, the folder but the policy's key (`with_kept` puts it after a `-`).
    pub daemon_homes: Vec<(String, PathBuf)>,
    /// The folders the checkout's build tool names (real paths, as said: `target-dir` · `build-dir`) — a config
    /// in the checkout sets them, so they open only as `asked_dir_verdict` says (#20) — and the tool.
    pub asked: Vec<(PathBuf, String)>,
    pub builder: Builder,
    /// This checkout's own per-checkout build dir: what the build tool names with your own setting (cargo's
    /// `$CARGO_BUILD_BUILD_DIR`, else its home config) put over the checkout's — `cargo-home/build/xx/yyyy`.
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
    /// Where it reads (`reads_for`; the default: reads not confined).
    pub reads: Reads,
    /// The users' own homes of the tools that get one of the run's (`run_homes_said`).
    pub run_homes: Vec<RunHomeSaid>,
    /// What a run gets of tarae's environment (`run_env_of`; the default: nothing).
    pub run_env: runenv::RunEnv,
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

/// Linux: from the kernel's Landlock ABI, asked (backports count — never the version string).
#[cfg(target_os = "linux")]
fn probe_now() -> Availability {
    linux::availability(crate::landlock::abi(), crate::seccomp::available())
}

#[cfg(not(target_os = "linux"))]
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

const INSIDE: &str = "tarae itself runs in a sandbox, and another can't start inside it";

/// Whether tarae itself runs inside a sandbox — started by a sandboxed run (`MARK`), or `sandbox-exec`
/// can't start one inside it (`probe`: blocks the first time — a worker's). What it starts is inside too
/// (#140: the terminal panel).
pub fn inside() -> bool {
    std::env::var_os(MARK).is_some() || matches!(probe(), Availability::Missing(w) if w == INSIDE)
}

/// Why `sandbox-exec` couldn't start one, from what it said.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub fn missing_why(stderr: &str) -> String {
    if stderr.contains("sandbox_apply") { INSIDE.into() } else { format!("sandbox-exec: {}", stderr.trim()) }
}

/// Where a sandboxed run in the project at `root` may write with these settings (grants put in as
/// `tools::with_allowed` does): its policy's own (`Policy::write_places`), made as `policy` makes it — the same
/// `env_for`, the toolchain's folders asked as for a run (blocks the first time: a worker's). For what tarae
/// reads from places no run can write (`gitrepo::for_file_in`).
pub fn write_places(root: &Path, llm: &crate::llm::LlmConfig) -> Vec<PathBuf> {
    let env = env_in(root, llm, &[], &Here::now()).0;
    let mut p = Policy::for_project(&env);
    // (the homes a daemon lives from are per policy key too — every one of the tool's, as any run may have
    // one: what a run may write is never trusted)
    p.write.extend(env.daemon_homes.iter().filter_map(|(_, base)| base.parent().map(Path::to_path_buf)));
    p.write_places()
}

/// What the policy for the project at `root` is made from (`policy`, `write_places`): these settings, the
/// places in home the user let Claude read (`reads`, real), the toolchain's folders found (asked inside the
/// sandbox they're for — blocks), what `here` says. With cargo's configs above the project (`tokens.rs` reads
/// them).
pub(crate) fn env_in(
    root: &Path,
    llm: &crate::llm::LlmConfig,
    reads: &[PathBuf],
    here: &Here,
) -> (Env, Vec<PathBuf>) {
    let home = here.path("HOME");
    let tmp = here.tmp.clone();
    let real_home = home.as_deref().and_then(|h| std::fs::canonicalize(h).ok());
    let vars = real_home.as_deref().map(|h| vars_in(h, here));
    let (caches, files) = vars.as_ref().map(caches_and_files).unwrap_or_default();
    let asks = asks_for(root, real_home.as_deref(), here);
    // (the repository's other checkouts — never home or above it, nor where keys are; configs above)
    let mut also: Vec<PathBuf> = crate::allow::checkouts(root)
        .into_iter()
        .filter(|c| crate::allow::keepable(c, real_home.as_deref()))
        .chain(reads.iter().cloned())
        .collect();
    let above: Vec<PathBuf> = real_home.iter().flat_map(|h| reads_above(&profile_path(root), h)).collect();
    also.extend(above.iter().cloned());
    let reads = real_home.as_deref().zip(vars.as_ref()).map(|(h, v)| reads_for(h, &also, v));
    let runs_later =
        real_home.as_deref().zip(vars.as_ref()).map(|(h, v)| RunsLater::new(h, v, &path_dirs_in(here)));
    let mut env = Env {
        asked_caches: Vec::new(),
        runs_later: runs_later.unwrap_or_default(),
        kept_homes: kept_homes(root, real_home.as_deref(), here, false),
        daemon_homes: kept_homes(root, real_home.as_deref(), here, true),
        run_homes: vars.as_ref().map(run_homes_said).unwrap_or_default(),
        reads: reads.unwrap_or_default(),
        root: profile_path(root),
        home,
        tmp,
        caches,
        files,
        dirs: Vec::new(),
        asked: Vec::new(),
        builder: asks.builder.clone(),
        own_build: None,
        write: llm.sandbox_write.clone(),
        protect: llm.sandbox_protect.clone(),
        network: llm.sandbox_network,
        ports: llm.sandbox_ports.clone(),
        localhost: llm.sandbox_localhost,
        tmux_tmpdir: here.path("TMUX_TMPDIR"),
        zellij_dir: here.path("ZELLIJ_SOCKET_DIR"),
        uid: here.uid,
        run_env: run_env_of(here, llm),
    };
    // The toolchain is asked inside the sandbox it's for (it reads only)
    let found = toolchain_dirs(&Policy::for_project(&env), &asks);
    (env.dirs, env.asked, env.own_build) = (found.other, found.named, found.own_build);
    // Its caches, as the packs say them; what it names that no run reads — in home, not home itself nor what
    // holds the project
    let packs = &crate::sandboxpack::get().caches;
    for (d, key, which) in found.caches {
        if let Some(c) = packs.iter().find(|c| c.asked.as_ref() == Some(&key)) {
            env.asked_caches.push((key, which, caches_in(c, &d).0));
        }
    }
    if let Some(h) = &env.reads.home {
        let keep = found.kept.into_iter().filter(|d| d.starts_with(h) && d != h && !env.root.starts_with(d));
        env.reads.kept.extend(keep);
        env.reads.kept.sort();
        env.reads.kept.dedup();
    }
    (env, above)
}

/// The tools' homes kept across the project's runs (`run-home`s with `keep`, in a project with one of their
/// `when`): in tarae's cache folder (`$XDG_CACHE_HOME`, else `~/.cache` — as `here` says), `run-homes/<name>/
/// <the project's key>` — the key, the project's real path's. Not made here (a run makes its own). Those a
/// daemon lives from (`daemon`) apart (`daemon`: true) — only where tarae serves and ends what runs leave
/// (Linux: `reaper.rs`); elsewhere none: the run's own.
fn kept_homes(root: &Path, home: Option<&Path>, here: &Here, daemon: bool) -> Vec<(String, PathBuf)> {
    if daemon && here.os != Os::Linux {
        return Vec::new();
    }
    let homes = crate::sandboxpack::get().run_homes.iter();
    let kept = homes.filter(|h| {
        h.keep
            && h.daemon.is_some() == daemon
            && (h.when.is_empty() || h.when.iter().any(|f| root.join(f).exists()))
    });
    let Some(cache) = kept_root(home, here) else { return Vec::new() };
    let key = project_key(root);
    kept.map(|h| (h.name.clone(), cache.join(&h.name).join(&key))).collect()
}

/// Where kept run homes are: tarae's cache folder's `run-homes` (`$XDG_CACHE_HOME`, else `~/.cache` — as `here`
/// says), real.
pub(crate) fn kept_root(home: Option<&Path>, here: &Here) -> Option<PathBuf> {
    let xdg = here.path("XDG_CACHE_HOME").filter(|p| p.is_absolute()).map(PathBuf::into_os_string);
    let cache = crate::runtime::cache_dir_of(xdg, home.map(|h| h.as_os_str().to_owned()));
    cache.map(|c| profile_path(&c.join("run-homes")))
}

/// `p`'s kept homes (`Policy::kept`, not held yet): the project's (`env.kept_homes`, written already), then
/// those a daemon lives from (`env.daemon_homes`) under `p`'s key — opened to its runs. The key is taken with
/// the project's in and the daemon's out, as `policy_key` takes it of the whole policy later (`ran`).
fn with_kept(p: &mut Policy, env: &Env) {
    let kept = |(name, home): &(String, PathBuf), daemon| Kept {
        name: name.clone(),
        home: home.clone(),
        daemon,
        held: Default::default(),
    };
    p.kept = env.kept_homes.iter().map(|h| kept(h, false)).collect();
    let key = policy_key(p);
    for (name, base) in &env.daemon_homes {
        let home = PathBuf::from(format!("{}-{key}", base.display()));
        p.write.push(home.clone());
        p.kept.push(kept(&(name.clone(), home), true));
    }
}

/// A kept run home a run gets (the packs' `run-home` with `keep`): the tool's name, its folder (real), whether a
/// daemon lives from it (`daemon`: kept per policy too), and its stamp, held while the policy lives — a kept
/// home in use is never pruned (`runhomes.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub name: String,
    pub home: PathBuf,
    pub daemon: bool,
    pub held: crate::runhomes::Held,
}

/// A project's key among the kept homes: its real path's (sha256, 16 hex).
pub(crate) fn project_key(root: &Path) -> String {
    crate::sha256::hex(profile_path(root).as_os_str().as_encoded_bytes())[..16].to_string()
}

/// The tools' shared caches in their homes (real, when there) as the packs say them — with the attribute each
/// tool sets on them — and the single files a run writes (the caches' own, then the packs' `write-files`
/// filled with the folders that are there — blocks). (Those the toolchain names come with its answer.)
fn caches_and_files(vars: &crate::sandboxpack::Vars) -> (Vec<CachesSaid>, Vec<PathBuf>) {
    let t = crate::sandboxpack::get();
    let (mut caches, mut files) = (Vec::new(), Vec::new());
    for c in t.caches.iter().filter(|c| c.asked.is_none()) {
        let Some(home) = vars.home_of(&c.home).and_then(|(at, _)| std::fs::canonicalize(at).ok()) else {
            continue;
        };
        let (said, own) = caches_in(c, &home);
        caches.push(said);
        files.extend(own);
    }
    for w in &t.write_files {
        files
            .extend(vars.place(w).map(|p| crate::sandboxpack::filled(&profile_path(&p))).unwrap_or_default());
    }
    (caches, files)
}

/// A tool's caches in its `home` (or the folder its toolchain named, `asked`): how they're said, and their
/// own files.
fn caches_in(c: &crate::sandboxpack::Caches, home: &Path) -> (CachesSaid, Vec<PathBuf>) {
    let dirs = match c.asked {
        Some(_) => vec![home.to_path_buf()],
        None => c.dirs.iter().map(|d| home.join(d)).collect(),
    };
    let files = c.files.iter().map(|f| home.join(f)).collect();
    (CachesSaid { dirs, rule: c.rule.clone(), said: c.said.clone(), xattr: c.xattr.clone() }, files)
}

/// The policy for the project at `root` with these settings and the places in home the user let Claude read
/// (`reads`, real) — the toolchain's folders found (blocks).
pub fn policy(root: &Path, llm: &crate::llm::LlmConfig, reads: &[PathBuf]) -> Policy {
    let p = policy_in(root, llm, reads, &Here::now());
    #[cfg(target_os = "linux")]
    let p = confined(p);
    p
}

/// `policy` with what `here` says — its Landlock rules not made yet (`confined`).
pub(crate) fn policy_in(root: &Path, llm: &crate::llm::LlmConfig, reads: &[PathBuf], here: &Here) -> Policy {
    let (mut env, above) = env_in(root, llm, reads, here);
    let real_home = env.home.as_deref().and_then(|h| std::fs::canonicalize(h).ok());
    // (no folder of its own: no sockets but the project's, `TMPDIR` as it was, and no tag to watch by)
    let run = RunDir::new().ok();
    let tag = run.as_ref().map(|r| r.tag());
    // Settings that hold a token: copies without it in the run's folder (`tokens.rs`) — while reads are
    // confined; what the user let Claude read stays readable
    if let Some(h) = &real_home {
        let sources = crate::tokens::sources_on(h, &|name| here.var(name), here.os);
        let dir = run.as_ref().map(|r| r.path());
        env.reads.copies = crate::tokens::copies(&sources, dir, h, reads, &env.reads.kept);
        env.reads.copies.told = crate::tokens::told_holders(&vars_in(h, here), &above);
    }
    let mut p = Policy { run, tag, ..Policy::for_project(&env) };
    with_kept(&mut p, &env);
    // The kept homes held (their stamps, before they're made: a prune can't take one between) — one that can't
    // be is no kept home for this run: the run's own, and its folder not written (`policy_key` leaves the
    // kept homes out of `write`: gone from both, the key is the same — the same policy's daemon isn't ended)
    if p.run.is_some() {
        let mut dropped = Vec::new();
        p.kept.retain_mut(|k| match crate::runhomes::stamp(&k.home, &p.root) {
            Ok(held) => {
                k.held = held;
                true
            }
            Err(e) => {
                crate::log::line("sandbox", "warning", &format!("kept home {}: {e}", k.home.display()));
                dropped.push(k.home.clone());
                false
            }
        });
        p.write.retain(|w| !dropped.contains(w));
    }
    // The run's own homes for the tools, and the packs' variables (`{run}`: its folder)
    let vars = real_home.as_deref().map(|h| crate::sandboxpack::Vars {
        run: p.run.as_ref().map(|r| r.path().to_path_buf()),
        ..vars_in(h, here)
    });
    let made =
        p.run.as_ref().zip(vars.as_ref()).map(|(r, v)| run_homes_made(r.path(), v, &p.run_env, &p.kept));
    p.env = made.unwrap_or_default();
    p.env.extend(vars.as_ref().map(packs_env).unwrap_or_default());
    p
}

/// The packs' `env` for a run, its places filled in (`vars` knows the run's folder): one naming a place that
/// isn't known (`{run}` without a run) is left out.
fn packs_env(vars: &crate::sandboxpack::Vars) -> Vec<(String, String)> {
    let filled = |v: &str| match v.starts_with('{') || v.starts_with("~/") {
        true => vars.place(v).map(|p| p.display().to_string()),
        false => Some(v.to_string()),
    };
    let env = &crate::sandboxpack::get().env;
    env.iter().filter_map(|(k, v)| Some((k.clone(), filled(v)?))).collect()
}

/// The tools that get a home of the run's own, with the user's own home of each that's there (real).
fn run_homes_said(vars: &crate::sandboxpack::Vars) -> Vec<RunHomeSaid> {
    let homes = crate::sandboxpack::get().run_homes.iter();
    homes
        .filter_map(|h| {
            let user = vars.home_of(&h.name).map(|(at, _)| profile_path(at)).filter(|u| u.is_dir())?;
            Some(RunHomeSaid { name: h.name.clone(), user, said: h.said.clone() })
        })
        .collect()
}

/// Linux: `p` with its Landlock rules made (`linux::confine` — it walks the project for protected names, and
/// makes first what a build makes beside them: `linux::Made`).
#[cfg(target_os = "linux")]
pub fn confined(mut p: Policy) -> Policy {
    let made =
        crate::landlock::abi().and_then(|abi| linux::confine(&p, abi, linux::audit_readable(abi), true));
    p.confine = Some(made.map_err(|e| format!("the sandbox couldn't be made: {e}")));
    p
}

/// The run's own homes for the tools (the packs' `run-home` — Gradle's: the user's holds what their own builds
/// run later and their daemons' registry; Go's build cache), made in its folder `run` — or the one `kept` for
/// the project (only tarae's: 0700) — the folders each says, links from it into the user's places that are
/// there (read only — what a run writes there is the packs' `write-files`), and the environment that points
/// the tool there — its variable, what's set (when its `if` place is there), what's appended to the user's own
/// value as the run gets it (`run_env`'s: a token-like one is left out, and gets only what's appended — never
/// tarae's own value, which goes in after `from_run_env`; `{home}` in it: the home's folder) — and, a home a
/// daemon lives from, its `daemon`'s folders and appends; a home of the run's own, its `fresh` appends. Gone
/// with the run's folder, but a kept one. What's made in a home goes through no link (`Unlinked`): a kept one
/// is a folder earlier runs wrote in, and tarae isn't sandboxed. Blocks (a worker's).
fn run_homes_made(
    run: &Path,
    vars: &crate::sandboxpack::Vars,
    run_env: &runenv::RunEnv,
    kept: &[Kept],
) -> Vec<(String, String)> {
    let at = |p: &str| vars.place(p).map(|p| profile_path(&p));
    let mut env: Vec<(String, String)> = Vec::new();
    for h in &crate::sandboxpack::get().run_homes {
        let kept = kept.iter().find(|k| k.name == h.name);
        // (no kept one, or one that's a link: the run's own, as before)
        let made = kept.and_then(|k| Some((k.home.clone(), k.daemon, Unlinked::made(&k.home).ok()?)));
        let fresh = made.is_none();
        let (home, daemon, dir) = match made {
            Some((home, daemon, dir)) => (home, daemon.then_some(h.daemon.as_ref()).flatten(), Ok(dir)),
            None => (run.join(&h.name), None, Unlinked::made(&run.join(&h.name))),
        };
        env.push((h.env.clone(), home.display().to_string()));
        let mut said = Vec::new();
        match &dir {
            Ok(dir) => {
                for m in h.make.iter().chain(daemon.iter().flat_map(|d| &d.make)) {
                    if let Err(e) = dir.folder(Path::new(m), &mut said) {
                        said.push(e);
                    }
                }
                for (inside, to) in &h.link {
                    if let Some(to) = at(to).filter(|t| t.is_dir())
                        && let Err(e) = dir.link(Path::new(inside), &to, &mut said)
                    {
                        said.push(e);
                    }
                }
            }
            Err(e) => said.push(e.clone()),
        }
        for s in said {
            crate::log::line("tarae", "warning", &format!("a run's {} home: {s}", h.name));
        }
        for (var, value, when) in &h.set {
            if when.as_deref().is_none_or(|w| at(w).is_some_and(|w| w.exists()))
                && let Some(value) = at(value)
            {
                env.push((var.clone(), value.display().to_string()));
            }
        }
        let fresh = fresh.then_some(&h.fresh).into_iter().flatten();
        for (var, more) in h.append.iter().chain(daemon.iter().flat_map(|d| &d.append)).chain(fresh) {
            let more = more.replace("{home}", &home.display().to_string());
            // (after what's appended already — the user's own value first, as the run gets it)
            let i = env.iter().position(|(k, _)| k == var);
            let theirs = match i {
                Some(i) => Some(env.remove(i).1),
                None => run_env.var(var),
            };
            let value = match theirs.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
                Some(o) => format!("{o} {more}"),
                None => more,
            };
            env.push((var.clone(), value));
        }
    }
    env
}

/// A run home's folder, opened through no link — what's made in it goes from it a step at a time, each
/// folder opened from the one before without following a link (`O_NOFOLLOW`), so a link a run left there, or
/// swaps in while another run is under way, is never gone through. A link in the way is removed (said); a file
/// or folder where a link goes is left as it is (Err).
struct Unlinked {
    fd: std::os::fd::OwnedFd,
    at: PathBuf,
}

impl Unlinked {
    /// Folder `home` (absolute), made where it isn't (0700, its parents as they come — tarae's own folders),
    /// opened — Err when it's a link or no folder.
    fn made(home: &Path) -> Result<Unlinked, String> {
        use std::os::unix::fs::DirBuilderExt;
        let said = |e: io::Error| format!("{}: {e}", home.display());
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(home).map_err(said)?;
        let c = c_name(home.as_os_str())?;
        let at = home.to_path_buf();
        Ok(Unlinked { fd: open_dir_at(libc::AT_FDCWD, &c).map_err(said)?, at })
    }

    /// Folder `rel` (plain names) in it, made where it isn't — each step through no link (`gone`: links
    /// removed on the way).
    fn folder(&self, rel: &Path, gone: &mut Vec<String>) -> Result<Unlinked, String> {
        let mut parts = rel.iter();
        let first = parts.next().ok_or("no folder named")?;
        let mut dir = self.step(first, gone)?;
        for p in parts {
            dir = dir.step(p, gone)?;
        }
        Ok(dir)
    }

    /// One folder in it, made where it isn't (0700) and opened — a link there removed and the folder made.
    fn step(&self, name: &std::ffi::OsStr, gone: &mut Vec<String>) -> Result<Unlinked, String> {
        let c = c_name(name)?;
        let at = self.at.join(name);
        let fd = std::os::fd::AsRawFd::as_raw_fd(&self.fd);
        for _ in 0..2 {
            // SAFETY: a folder descriptor of ours and a name, live for the call (one there already: EEXIST).
            unsafe { libc::mkdirat(fd, c.as_ptr(), 0o700) };
            match open_dir_at(fd, &c) {
                Ok(fd) => return Ok(Unlinked { fd, at }),
                Err(_) if self.unlinked(&c, &at, gone) => {}
                Err(e) => return Err(format!("{}: {e} (no folder there)", at.display())),
            }
        }
        Err(format!("{}: a link came back", at.display()))
    }

    /// A link `rel` (plain names) in it to `to` — its folders made through no link; one there already to `to`
    /// kept, to elsewhere removed first.
    fn link(&self, rel: &Path, to: &Path, gone: &mut Vec<String>) -> Result<(), String> {
        let name = rel.file_name().ok_or("no link named")?;
        let dir = match rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => self.folder(p, gone)?,
            None => Unlinked { fd: self.fd.try_clone().map_err(|e| e.to_string())?, at: self.at.clone() },
        };
        let (c, target) = (c_name(name)?, c_name(to.as_os_str())?);
        let at = dir.at.join(name);
        let fd = std::os::fd::AsRawFd::as_raw_fd(&dir.fd);
        for _ in 0..2 {
            // SAFETY: a folder descriptor of ours and two names, live for the call.
            if unsafe { libc::symlinkat(target.as_ptr(), fd, c.as_ptr()) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(format!("{}: {e}", at.display()));
            }
            if link_target(fd, &c).is_some_and(|t| t == to.as_os_str()) {
                return Ok(());
            }
            if !dir.unlinked(&c, &at, gone) {
                return Err(format!("{}: not a link, where the link goes — left as it is", at.display()));
            }
        }
        Err(format!("{}: a link came back", at.display()))
    }

    /// Whether `c` in it was a link, now removed (said in `gone`).
    fn unlinked(&self, c: &std::ffi::CStr, at: &Path, gone: &mut Vec<String>) -> bool {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&self.fd);
        // SAFETY: a folder descriptor of ours and a name, live for the call.
        let removed = link_target(fd, c).is_some() && unsafe { libc::unlinkat(fd, c.as_ptr(), 0) } == 0;
        if removed {
            gone.push(format!("{}: a link there, removed", at.display()));
        }
        removed
    }
}

/// `name` as C wants it.
fn c_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString, String> {
    std::ffi::CString::new(name.as_encoded_bytes()).map_err(|_| format!("{name:?}: a NUL in it"))
}

/// Folder `c` in `fd` (or `AT_FDCWD`) opened — Err when it's a link (`O_NOFOLLOW`) or no folder.
fn open_dir_at(fd: libc::c_int, c: &std::ffi::CStr) -> io::Result<std::os::fd::OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: a folder descriptor of ours (or the cwd's) and a name, live for the call.
    let got = unsafe { libc::openat(fd, c.as_ptr(), flags) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made, ours.
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(got) })
}

/// Where link `c` in `fd` points — None: no link there.
fn link_target(fd: libc::c_int, c: &std::ffi::CStr) -> Option<std::ffi::OsString> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: a folder descriptor of ours, a name and a buffer of its length, live for the call.
    let n = unsafe { libc::readlinkat(fd, c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    let n = usize::try_from(n).ok()?;
    buf.truncate(n);
    Some(std::os::unix::ffi::OsStringExt::from_vec(buf))
}

/// What the project's toolchain names: what the packs' `ask` gives — folders written (`other`), its shared
/// caches (`caches`: with their key, as said — Go's module cache), what no run reads (`kept` — the user's Go
/// build cache) — the checkout's build tool's target and build dirs (`named`, real paths, as said — cargo's),
/// and this checkout's own per-checkout build dir (`own_build` — `Env`'s).
#[derive(Clone, Debug, Default)]
pub struct ToolDirs {
    pub other: Vec<(PathBuf, String)>,
    pub caches: Vec<(PathBuf, String, String)>,
    pub kept: Vec<PathBuf>,
    pub named: Vec<(PathBuf, String)>,
    pub own_build: Option<PathBuf>,
}

/// What the project is asked where it writes outside it, and how the answers are judged: the checkout's
/// build tool (the first built-in `[builds]` whose `when` it has), the packs' `ask` whose `when` it has, and
/// the files that set what they answer (their times keep the answer — `toolchain_dirs`).
struct Asks {
    tool: Option<crate::builds::Tool>,
    builder: Builder,
    /// How the tool's answer's fields are said (`builds-said`: target, build).
    said: [String; 2],
    /// Your own build-folder setting: the variable that puts it over the checkout's, its value.
    own: Option<(String, String)>,
    others: Vec<crate::sandboxpack::Ask>,
    /// What they're asked with: the packs' `env` a run gets (but what names its own folder) — Go's
    /// `GOTOOLCHAIN=local`.
    env: Vec<(String, String)>,
    set_by: Vec<PathBuf>,
}

/// What the project at `root` (`home`: real) is asked, with what `here` says.
fn asks_for(root: &Path, home: Option<&Path>, here: &Here) -> Asks {
    let t = crate::sandboxpack::get();
    let has = |when: &[String]| when.iter().any(|f| root.join(f).is_file());
    let entries = crate::builds::Builds::of(crate::packs::builtin_manifests());
    let (pack, tool) = match entries.into_iter().find(|(_, when, _)| has(when)) {
        Some((pack, _, Ok(tool))) => (pack, Some(tool)),
        _ => (String::new(), None),
    };
    let said = t.builds_said.iter().find(|s| s.pack == pack);
    let tool_home = tool.as_ref().and_then(|t| t.home_in(&|n| here.path(n), home));
    let tool_home = tool_home.and_then(|h| std::fs::canonicalize(h).ok());
    let builder = Builder {
        who: said.map_or_else(
            || tool.as_ref().map(|t| t.name.to_lowercase()).unwrap_or_default(),
            |s| s.who.clone(),
        ),
        home: tool_home.clone(),
        own_under: tool.as_ref().and_then(|t| t.own_under()).map(|(u, d)| (u.to_path_buf(), d)),
    };
    let fields =
        tool.as_ref().map(|t| t.asked()).map(|(_, t, b)| [t.to_string(), b.unwrap_or(t).to_string()]);
    let said = said.map(|s| [s.target.clone(), s.build.clone()]).or(fields).unwrap_or_default();
    let var = |n: &str| here.var(n).filter(|v| !v.is_empty());
    let own = tool.as_ref().and_then(|t| t.own_setting(&var, tool_home.as_deref()));
    let others: Vec<crate::sandboxpack::Ask> = t.ask.iter().filter(|a| has(&a.when)).cloned().collect();
    let mut set_by = tool.as_ref().map(|t| t.set_by(root, tool_home.as_deref())).unwrap_or_default();
    set_by.extend(others.iter().flat_map(|a| a.when.iter().map(|f| root.join(f))));
    let env = home.map(|h| packs_env(&vars_in(h, here))).unwrap_or_default();
    Asks { tool, builder, said, own, others, env, set_by }
}

/// What `toolchain_dirs` found per project (real path): the stamps of the files that set them, the folders.
type Seen = HashMap<PathBuf, (Vec<Option<SystemTime>>, ToolDirs)>;
static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();

/// The project's toolchain folders outside it as last found for a run of Claude's (`toolchain_dirs`) — only
/// those its runs may write (`asked_dir_verdict`, `ask_dir_verdict`; a shared cache too, which they read);
/// nothing looked up: none before its first run.
pub fn build_dirs(root: &Path) -> Vec<PathBuf> {
    let root = profile_path(root);
    let Some(found) =
        SEEN.get().and_then(|s| s.lock().ok().and_then(|m| m.get(&root).map(|(_, d)| d.clone())))
    else {
        return Vec::new();
    };
    let here = Here::now();
    let home = here.path("HOME");
    let tool = asks_for(&root, home.as_deref(), &here).builder;
    let (own, tmp) = (found.own_build.as_deref(), here.tmp.as_deref());
    let open = |d: &Path| asked_dir_verdict(d, &root, home.as_deref(), &tool, own, tmp).is_ok();
    let named = found.named.iter().map(|(d, _)| d).filter(|d| open(d));
    let real_home = home.as_deref().and_then(|h| std::fs::canonicalize(h).ok());
    let later = real_home.as_deref().map(|h| RunsLater::new(h, &vars_in(h, &here), &path_dirs_in(&here)));
    let later = later.unwrap_or_default();
    let open = |d: &&PathBuf, later: &RunsLater| ask_dir_verdict(d, &root, home.as_deref(), later).is_ok();
    let written = found.other.iter().map(|(d, _)| d).filter(|d| open(d, &later));
    let caches = found.caches.iter().filter(|(d, key, _)| open(&d, &later.but_cache(key))).map(|(d, ..)| d);
    let other = written.chain(caches);
    other.chain(named).filter(|d| !d.starts_with(&root)).cloned().collect()
}

/// Folders the project's toolchain writes outside it: the checkout's build tool's target and build dirs
/// (cargo's `cargo metadata` — they can be anywhere: `CARGO_TARGET_DIR`, `build.build-dir` in a cargo config)
/// and what the packs' `ask` names (Go's caches) — and the build dir the tool names with your own setting put
/// over the checkout's (`ToolDirs::own_build`). Kept per project until a file that sets them changes.
fn toolchain_dirs(base: &Policy, asks: &Asks) -> ToolDirs {
    let root = &base.root;
    let stamp = stamp_of(&asks.set_by);
    let seen = SEEN.get_or_init(Default::default);
    if let Some((s, dirs)) = seen.lock().ok().and_then(|m| m.get(root).cloned())
        && s == stamp
    {
        return dirs;
    }
    let mut dirs = ToolDirs::default();
    let json_of = |run: &[String], env: &[(&str, &str)]| {
        let args: Vec<&str> = run.iter().skip(1).map(String::as_str).collect();
        output_in(base, run.first()?, &args, env)
    };
    if let Some(tool) = &asks.tool {
        let (run, target, build) = tool.asked();
        if let Some(json) = json_of(run, &[]) {
            for (key, said) in [Some(target), build].into_iter().zip(&asks.said) {
                if let Some(d) = key.and_then(|k| json[k].as_str()) {
                    dirs.named.push((profile_path(Path::new(d)), said.clone()));
                }
            }
            // (your own build folder over the checkout's — the variable wins over every config file)
            if let Some((var, own)) = &asks.own
                && let Some(json) = json_of(run, &[(var, own)])
            {
                dirs.own_build = build.and_then(|b| json[b].as_str()).map(|d| profile_path(Path::new(d)));
            }
        }
    }
    let env: Vec<(&str, &str)> = asks.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    for a in &asks.others {
        if let Some(json) = json_of(&a.run, &env) {
            let who = a.run.first().map_or("", String::as_str);
            let at =
                |key: &str| json[key].as_str().filter(|d| !d.is_empty()).map(|d| profile_path(Path::new(d)));
            for key in &a.json {
                dirs.other.extend(at(key).map(|d| (d, format!("{who}'s {key}"))));
            }
            for key in &a.caches {
                dirs.caches.extend(at(key).map(|d| (d, key.clone(), format!("{who}'s {key}"))));
            }
            dirs.kept.extend(a.kept.iter().filter_map(|k| at(k)));
        }
    }
    if let Ok(mut m) = seen.lock() {
        m.insert(root.clone(), (stamp, dirs.clone()));
    }
    dirs
}

/// The times of the files that set the toolchain's folders (`Asks::set_by`) — what `toolchain_dirs` keeps its
/// answer by.
fn stamp_of(files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    files.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect()
}

/// Tests: what `toolchain_dirs` answers for the project at `root` — as a probe of its toolchain would have
/// found it (no cargo or go asked inside a sandbox, which can't start inside the one tests may run in).
#[cfg(test)]
pub(crate) fn toolchain_found(root: &Path, dirs: ToolDirs) {
    toolchain_found_in(root, dirs, &Here::now());
}

/// `toolchain_found`, with what `here` says (a stand-in home).
#[cfg(test)]
pub(crate) fn toolchain_found_in(root: &Path, dirs: ToolDirs, here: &Here) {
    let root = profile_path(root);
    let home = here.path("HOME").and_then(|h| std::fs::canonicalize(h).ok());
    let stamp = stamp_of(&asks_for(&root, home.as_deref(), here).set_by);
    SEEN.get_or_init(Default::default).lock().unwrap().insert(root, (stamp, dirs));
}

/// A program's JSON output, run in the project inside `policy`, with `env` set.
fn output_in(
    policy: &Policy,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Option<serde_json::Value> {
    #[cfg(target_os = "linux")]
    let out = {
        let abi = crate::landlock::abi().ok()?;
        let rules = linux::confine(policy, abi, Err("not watched".into()), false).ok()?;
        let mut c = Command::new(program);
        from_run_env(&mut c, policy);
        c.args(args).envs(env.iter().copied()).current_dir(&policy.root).env(MARK, "1");
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let child = linux::confined(&rules, None, |_| {}, || c.spawn()).ok()?;
        child.wait_with_output().ok()?
    };
    #[cfg(not(target_os = "linux"))]
    let out = from_run_env(&mut Command::new(SANDBOX_EXEC), policy)
        .args(policy.args())
        .arg(program)
        .args(args)
        .envs(env.iter().copied())
        .current_dir(&policy.root)
        .env(MARK, "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let out = Some(out).filter(|o| o.status.success())?;
    serde_json::from_slice(&out.stdout).ok()
}

/// What tarae sets in every sandboxed run besides the packs' `env` (`inside_env`, `command`'s npm line,
/// `task::color_off`) — and `TARAE_UI`, never a run of Claude's: none said as left out of its environment.
const SET_IN_RUNS: [&str; 5] =
    [MARK, "GIT_OPTIONAL_LOCKS", "npm_config_update_notifier", "NO_COLOR", "TARAE_UI"];

/// What a run in a policy made with these settings gets of `here`'s environment (`runenv::of`): the floor,
/// the proxies with the network open, what the packs' places read, the packs' `pass`, `llm.sandbox-env`.
pub fn run_env_of(here: &Here, llm: &crate::llm::LlmConfig) -> runenv::RunEnv {
    let packs = crate::sandboxpack::get();
    let policy: Vec<&str> = packs.env_names().collect();
    let colors = &crate::languages::get().color_off;
    let mut set: Vec<&str> = SET_IN_RUNS.to_vec();
    set.extend(colors.iter().map(|(k, _)| k.as_str()));
    runenv::of(here, llm.sandbox_network, &policy, &packs.pass, &llm.sandbox_env, &set)
}

/// A sandboxed command's environment: tarae's own isn't inherited — only what `run_env` passes of it. Every
/// other name of tarae's is removed by name, not with `env_clear`: `spawn::setsid` (every run's) starts from
/// tarae's environment and applies `get_envs()`, which doesn't say it was cleared (`get_env_clear` is
/// unstable) — and after `env_clear` std doesn't record a removal at all. Measured both ways on Linux: the
/// fake token reached the run (`a_fake_token_never_reaches_a_sandboxed_run`).
fn from_run_env<'c>(c: &'c mut Command, p: &Policy) -> &'c mut Command {
    for (k, _) in std::env::vars_os() {
        if !p.run_env.vars.iter().any(|(name, ..)| k == name.as_str()) {
            c.env_remove(k);
        }
    }
    c.envs(p.run_env.vars.iter().map(|(k, v, _)| (k, v)))
}

/// What a sandboxed command's environment says: that it's sandboxed (`MARK` — its value the run's tag: what
/// the run leaves running carries it, `reaper`), and `GIT_OPTIONAL_LOCKS=0` — git's read-only commands
/// (status, diff) otherwise refresh the index on the side, a write into `.git` the sandbox denies: noise in
/// every report (measured: 6 denials in one `cargo test`).
fn inside_env<'c>(c: &'c mut Command, tag: Option<&str>) -> &'c mut Command {
    c.env(MARK, tag.unwrap_or("1")).env("GIT_OPTIONAL_LOCKS", "0")
}

/// The watch for a run in `p` (on a worker; give one with a stream — `Watch::pid` — `WATCH_READY` before the
/// run starts) — None where nothing can watch: no tag, or on Linux neither an audit log to read
/// (`linux::Confine::audit` — the files' denials then come from the run's output alone) nor a supervisor
/// to say what it refused (a quiet watch it records in).
pub fn watch_for(p: &Policy) -> Option<std::sync::Arc<Watch>> {
    let tag = p.tag.as_ref()?;
    #[cfg(target_os = "linux")]
    return match &p.confine {
        Some(Ok(c)) => match &c.audit {
            Ok(source) => {
                let w = Watch::start_audit(tag, source);
                w.also_owned_by(earlier_owners(&p.root));
                Some(w)
            }
            Err(_) if c.serve.is_ok() => Some(Watch::quiet(tag)),
            Err(_) => None,
        },
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    Some(Watch::start(tag))
}

/// Starts `cmd` (made by `command`) in a session of its own (`spawn::setsid`) — on Linux from a thread
/// confined to `policy` (`linux::confined`: its rules, the seccomp filter, and tarae's supervisor deciding
/// what it reaches by address); elsewhere the sandbox is in the command already. A policy whose rules
/// couldn't be made doesn't start at all.
pub fn spawn(policy: Option<&Policy>, cmd: &Command) -> std::io::Result<crate::spawn::Spawned> {
    #[cfg(target_os = "linux")]
    if let Some(p) = policy {
        let c = match &p.confine {
            Some(Ok(c)) => c,
            Some(Err(why)) => return Err(std::io::Error::other(why.clone())),
            None => return Err(std::io::Error::other("the sandbox wasn't made for this run")),
        };
        let watch = p.watch.clone();
        let owner = move |o| {
            if let Some(w) = &watch {
                w.owned_by(o);
            }
        };
        let serve = std::sync::Arc::new(p.clone());
        let spawned = linux::confined(c, Some(serve), owner, || crate::spawn::setsid(cmd))?;
        ran(p, spawned.pid);
        // (what was made first, still empty once the run is gone, goes — the run's own thread reaps it)
        let (made, pid) = (c.made.clone(), spawned.pid);
        if !made.paths.is_empty() {
            crate::event::thread("tarae-tidy", move || {
                // SAFETY: signal 0 only asks whether the process is there.
                while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
                    std::thread::sleep(WATCH_LOOKS);
                }
                made.tidy();
            });
        }
        return Ok(spawned);
    }
    let _ = policy;
    crate::spawn::setsid(cmd)
}

/// What a run in `policy` needs done once it's over, before its end is told (the run's own thread, after it's
/// reaped): on Linux, the folders it could remove inside protected ones made again (`linux::Kept`) — the
/// report says which. Nothing elsewhere.
pub fn after_run(policy: Option<&Policy>) -> impl FnOnce() + Send + 'static {
    #[cfg(target_os = "linux")]
    let kept = match policy.map(|p| &p.confine) {
        Some(Some(Ok(c))) => Some(c.kept.clone()),
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let _ = policy;
    move || {
        #[cfg(target_os = "linux")]
        if let Some(k) = kept {
            k.restore();
        }
    }
}

/// `program args` inside `policy` — or as it is, without one. On Linux the sandbox comes with the spawn
/// (`spawn`); here only what a sandboxed run's environment says: nothing of tarae's but `Policy::run_env`, then
/// what tarae sets.
pub fn command(policy: Option<&Policy>, program: &str, args: &[String]) -> Command {
    match policy {
        Some(p) => {
            #[cfg(target_os = "linux")]
            let mut c = Command::new(program);
            #[cfg(not(target_os = "linux"))]
            let mut c = {
                let mut c = Command::new(SANDBOX_EXEC);
                c.args(p.args()).arg(program);
                c
            };
            c.args(args);
            from_run_env(&mut c, p);
            inside_env(&mut c, p.tag.as_deref()).envs(p.env.iter().map(|(k, v)| (k, v)));
            if let Some(run) = &p.run {
                c.env("TMPDIR", run.path());
            }
            // (npm's daily check for its own update writes the date in `~/.npm`, closed — and asks the network:
            // reported blocked in every npm run on the VMs, step 7)
            c.env("npm_config_update_notifier", "false");
            for (var, copy) in &p.reads.copies.env {
                c.env(var, copy);
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

    /// Where a run may write, as `gitrepo` asks it (`write_places`), is its policy's own (`policy`): the same
    /// list — the project, the toolchain's folders its probe found (a Go module cache here), `llm.sandbox-write`,
    /// runs' own folders, the devices. And a file in that toolchain folder gets no git by discovery
    /// (`gitrepo::for_file_in`), though a repository holds it.
    #[test]
    fn write_places_are_the_policys_own() {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("tarae-write-places-{pid}"));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        // (under this crate's `target/`: outside the temp folders; a repository of its own, so a discovery
        // there never climbs into tarae's)
        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("target/tarae-gomodcache-{pid}"));
        std::fs::remove_dir_all(&cache).ok();
        std::fs::create_dir_all(cache.join("mod")).unwrap();
        let cache = std::fs::canonicalize(&cache).unwrap();
        let ok = Command::new("git").args(["init", "-q"]).current_dir(&cache).output().unwrap();
        assert!(ok.status.success());
        toolchain_found(
            &root,
            ToolDirs { other: vec![(cache.clone(), "go's GOMODCACHE".into())], ..Default::default() },
        );
        let llm =
            crate::llm::LlmConfig { sandbox_write: vec!["/opt/tarae-granted".into()], ..Default::default() };
        let w = write_places(&root, &llm);
        let devices =
            if cfg!(target_os = "linux") { ["/dev/shm", "/dev/pts"] } else { ["/dev/fd", "/dev/null"] };
        let named = [&root, &cache, Path::new("/opt/tarae-granted"), &RunDir::base()];
        for p in named.into_iter().chain(devices.iter().map(Path::new)) {
            assert!(w.iter().any(|x| x == p), "{} in {w:?}", p.display());
        }
        let p = policy(&root, &llm, &[]);
        let run = p.run.as_ref().map(|r| r.path().to_path_buf());
        let of_policy: Vec<PathBuf> =
            p.write_places().into_iter().filter(|x| Some(x) != run.as_ref()).collect();
        assert_eq!(of_policy, w, "the policy's own list");
        let file = cache.join("mod/x.go");
        std::fs::write(&file, "package x\n").unwrap();
        let writes = w.clone();
        let places = crate::gitrepo::Places {
            roots: vec![root.clone()],
            writes: std::sync::Arc::new(move || Some(writes.clone())),
        };
        assert_eq!(
            crate::gitrepo::for_file_in(&places, &file).err(),
            Some(crate::gitrepo::NoGit::Writable { looks: true })
        );
        // (the profile's device rule as it was written by hand)
        assert_eq!(
            devices_rule(),
            "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
             (literal \"/dev/ptmx\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/ttys[0-9]+$\") (subpath \
             \"/dev/fd\"))\n"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&cache).ok();
    }

    /// Whether `pid` is there at all — a zombie counts (signal 0 only asks).
    fn there(pid: u32) -> bool {
        // SAFETY: signal 0 sends nothing.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Waits (5 s at most) for `pid` to be gone — killed and reaped.
    fn gone(pid: u32) -> bool {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while there(pid) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        !there(pid)
    }

    /// A watch ends with its run's process, though something still holds it (seen: a `log stream` 18
    /// minutes past its run) — killed and reaped. (Stand-ins: `sleep` for `log stream`, `true` for the run.)
    #[test]
    fn a_watch_ends_with_its_run_whoever_still_holds_it() {
        let held = Watch::spawned_for_test("sleep", "60");
        let watching = held.pid().unwrap();
        let run = crate::spawn::setsid(&Command::new("true")).unwrap();
        held.run_started(run.pid);
        crate::spawn::wait(run.pid).unwrap();
        assert!(gone(watching), "the watch ({watching}) outlived its run");
        assert_eq!(held.pid(), None);
    }

    /// A run waits for its watch's stream before it starts only on macOS: `log stream` reads from when it's
    /// up; Linux's audit stream reads from a mark taken when it was asked for (5c), so a run there doesn't
    /// wait. No stream, no wait. (A stand-in stream: `sleep`.)
    #[test]
    fn only_macos_runs_wait_for_their_watch() {
        let mut p = Policy::for_project(&Env { root: PathBuf::from("/w/p"), ..Default::default() });
        assert_eq!(watch_wait(&p), std::time::Duration::ZERO, "no watch");
        let w = Watch::spawned_for_test("sleep", "5");
        p.watch = Some(w.clone());
        let want = if cfg!(target_os = "linux") { std::time::Duration::ZERO } else { WATCH_READY };
        assert_eq!(watch_wait(&p), want);
        w.finish();
    }

    /// A watch whose run never starts (the turn ended meanwhile, the run refused …) ends on its own, saying
    /// why — and one whose run started isn't ended by that.
    #[test]
    fn a_watch_no_run_takes_up_ends_on_its_own() {
        let held = Watch::spawned_for_test("sleep", "60");
        let watching = held.pid().unwrap();
        held.unless_claimed_within(std::time::Duration::from_millis(200));
        let claimed = Watch::spawned_for_test("sleep", "60");
        let runs = claimed.pid().unwrap();
        let run = crate::spawn::setsid(Command::new("sleep").arg("2")).unwrap();
        claimed.unless_claimed_within(std::time::Duration::from_millis(200));
        claimed.run_started(run.pid);
        assert!(gone(watching), "the watch ({watching}) no run took up lives on");
        assert!(held.blocked().failed.is_some_and(|w| w.contains("didn't start")), "{:?}", held.blocked());
        assert!(there(runs), "a watch whose run started watches on");
        claimed.finish();
        crate::spawn::wait(run.pid).unwrap();
        assert!(gone(runs));
    }

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
        let out = inside_env(&mut status, None).output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before, "the index was rewritten");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Cargo as the rust pack says it, its home at `home` (that needn't be there): `Env` with its caches, their
    /// own files and attribute, and the build tool that names its folders.
    fn cargo_at(home: &Path) -> Env {
        let t = crate::sandboxpack::get();
        let c = t.caches.iter().find(|c| c.home == "cargo").expect("the rust pack's caches");
        let (said, files) = caches_in(c, home);
        let builds = crate::builds::Builds::of(crate::packs::builtin_manifests());
        let (_, _, rust) = builds.iter().find(|(p, ..)| p == "rust").expect("its [builds]");
        let own_under = rust.as_ref().unwrap().own_under().map(|(u, d)| (u.to_path_buf(), d));
        let builder = Builder { who: "cargo".into(), home: Some(home.into()), own_under };
        Env { caches: vec![said], files, builder, ..Env::default() }
    }

    fn env(root: &Path) -> Env {
        let cargo = cargo_at(Path::new("/Users/u/.cargo"));
        let lock = "/Users/u/.gradle/wrapper/dists/gradle-9.7.1-bin/h1/gradle-9.7.1-bin.zip.lck";
        Env {
            root: root.to_path_buf(),
            home: Some("/Users/u".into()),
            tmp: Some("/private/var/folders/ab/cd/T".into()),
            files: cargo.files.iter().cloned().chain([lock.into()]).collect(),
            dirs: Vec::new(),
            asked: vec![
                ("/Users/u/.cargo/build/ab/cd12".into(), "build-dir".into()),
                (root.join("target"), "target-dir".into()),
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
            reads: Reads {
                home: Some("/Users/u".into()),
                allow: vec!["/Users/u/.cargo".into(), "/Users/u/.gitconfig".into()],
                kept: vec![
                    "/Users/u/.ssh".into(),
                    "/Users/u/.claude".into(),
                    "/Users/u/.local/state/tarae".into(),
                ],
                log: Some("/Users/u/.local/state/tarae/tarae.log".into()),
                copies: Default::default(),
            },
            run_homes: vec![gradle_at(Path::new("/Users/u/.gradle"))],
            ..cargo
        }
    }

    /// Gradle's run home as the java pack says it, the user's Gradle home at `user` (that needn't be there).
    fn gradle_at(user: &Path) -> RunHomeSaid {
        let homes = &crate::sandboxpack::get().run_homes;
        let h = homes.iter().find(|h| h.name == "gradle").expect("the java pack's run-home");
        RunHomeSaid { name: h.name.clone(), user: user.into(), said: h.said.clone() }
    }

    /// The run's own homes made in `run` with the user's Gradle home at `user` and their `GRADLE_OPTS`.
    fn made_with(run: &Path, user: &Path, opts: Option<&str>) -> Vec<(String, String)> {
        let mut env = HashMap::from([("GRADLE_USER_HOME".to_string(), user.into())]);
        env.extend(opts.map(|o| ("GRADLE_OPTS".to_string(), o.into())));
        let here = Here { env, ..Here::default() };
        let run_env = run_env_of(&here, &crate::llm::LlmConfig::default());
        run_homes_made(run, &vars_in(&user.join("no-home"), &here), &run_env, &[])
    }

    /// The multiplexers' socket folders, where they put them by default and where the environment says.
    #[test]
    fn the_multiplexers_socket_folders() {
        let root = Path::new("/Users/u/p");
        let keep = Policy::for_project(&env(root)).keep;
        assert_eq!(
            keep,
            [profile_path(Path::new(TMP)).join("tmux-501"), "/private/var/folders/ab/cd/T/zellij-501".into()]
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
                TMP,
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
        // (not in a temp folder: what's there is writable anyway — Linux's temp_dir is `/tmp`)
        let target = profile_path(&std::env::current_dir().unwrap().join("target"));
        let base = target.join(format!("tarae-sb-dirs-{}", std::process::id()));
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
        let named: Vec<(PathBuf, String)> = bad
            .iter()
            .cloned()
            .chain([root.join("target"), own.clone()])
            .map(|d| (d, "build-dir".into()))
            .collect();
        let env = Env {
            root: root.clone(),
            home: Some(home.clone()),
            asked: named,
            own_build: Some(own.clone()),
            ..cargo_at(&cargo_home)
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
        assert!(said.contains(&format!("{} (cargo's build-dir", crate::allow::tilde(&home))), "{said}");
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
        let builds = crate::builds::Builds::of(crate::packs::builtin_manifests());
        let (_, _, rust) = builds.iter().find(|(p, ..)| p == "rust").unwrap();
        let rust = rust.as_ref().unwrap();
        let at = |var: Option<&str>| {
            let var = |_: &str| var.map(String::from);
            rust.own_setting(&var, Some(&cargo_home)).map(|(v, own)| format!("{v}={own}"))
        };
        let var = "CARGO_BUILD_BUILD_DIR";
        assert_eq!(at(None), Some(format!("{var}={{cargo-cache-home}}/build/{{workspace-path-hash}}")));
        assert_eq!(at(Some("/x/{workspace-path-hash}")), Some(format!("{var}=/x/{{workspace-path-hash}}")));
        std::fs::remove_file(cargo_home.join("config.toml")).unwrap();
        assert_eq!(at(None), None, "none of yours: no per-checkout dir to trust");
        std::fs::remove_dir_all(&base).ok();
    }

    /// The folders a checkout's build tool names are judged the same for any tool a pack brings: a tool of the
    /// test's own naming home, `/`, the checkout's parent and its own home — all closed, each said in its words;
    /// its own build folder (where its `[builds] remove` puts one) and the checkout's own open. (Names only.)
    #[test]
    fn asked_dirs_are_judged_for_every_tool() {
        let (home, root) = (PathBuf::from("/Users/u"), PathBuf::from("/Users/u/work/p"));
        let tool_home = home.join(".mytool");
        let own = tool_home.join("cache/one");
        let builder = Builder {
            who: "mytool".into(),
            home: Some(tool_home.clone()),
            own_under: Some(("cache".into(), 1)),
        };
        let bad =
            [home.clone(), "/".into(), home.join("work"), tool_home.clone(), tool_home.join("cache/one/x")];
        let asked =
            bad.iter().chain([&own, &root.join("out")]).map(|d| (d.clone(), "out-dir".into())).collect();
        let env = Env {
            root: root.clone(),
            home: Some(home),
            asked,
            builder,
            own_build: Some(own.clone()),
            ..Env::default()
        };
        let p = Policy::for_project(&env);
        for d in &bad {
            assert!(!p.write.contains(d), "{} opened: {:?}", d.display(), p.write);
            let (_, why) =
                p.refused.iter().find(|(r, _)| r == d).unwrap_or_else(|| panic!("{} not said", d.display()));
            assert!(why.starts_with("mytool's out-dir — "), "{why}");
        }
        assert!(p.refused.iter().any(|(_, why)| why.ends_with("it's mytool's home")), "{:?}", p.refused);
        assert!(p.write.contains(&own) && p.refused.len() == bad.len(), "{:?} {:?}", p.write, p.refused);
        assert_eq!(p.named_by, "mytool");
    }

    /// The folders a pack's `ask` names (Go's caches) are the tool's own wherever its settings put them — but
    /// never home or above it, what holds the checkout, nor in or around keys or what runs later: those stay
    /// closed, said where Claude reads it and on the panel. (Names only.)
    #[test]
    fn ask_dirs_are_judged_too() {
        let (home, root) = (PathBuf::from("/Users/u"), PathBuf::from("/Users/u/work/p"));
        let good = [PathBuf::from("/opt/out"), home.join("Library/Caches/mytool"), home.join("go/pkg/mod")];
        let bad = [
            home.clone(),
            "/".into(),
            home.join("work"),
            home.join(".ssh/cache"),
            home.join(".gradle/daemon"),
            home.join(".cargo/registry/x"),
            home.join(".config/fish"),
        ];
        let dirs = good.iter().chain(&bad).map(|d| (d.clone(), "mytool's OUT".to_string())).collect();
        let p = Policy::for_project(&Env {
            root: root.clone(),
            home: Some(home.clone()),
            dirs,
            ..Env::default()
        });
        assert!(good.iter().all(|d| p.write.contains(d)), "{:?}", p.write);
        for d in &bad {
            assert!(!p.write.contains(d), "{} opened", d.display());
            assert!(
                p.refused_asked.iter().any(|(r, why)| r == d && why.starts_with("mytool's OUT — ")),
                "{}",
                d.display()
            );
        }
        assert!(p.refused.is_empty());
        let g = Guard::In(Box::new(p));
        let said = g.report(&[]);
        let h = crate::allow::tilde(&home);
        assert!(
            said.contains(&format!(
                "It kept closed what the toolchain's settings named: {h} (mytool's OUT — it's your home)"
            )),
            "{said}"
        );
        let (line, _) = panel_line(Some(&g), false, std::time::Instant::now()).expect("the panel says it");
        assert_eq!(line, format!("sandbox kept {h} closed — mytool's OUT named it  +{}", bad.len() - 1));
    }

    /// What runs later as the user is never a folder the packs' `ask` names, nor one a write card offers
    /// (`RunsLater`): a folder on `PATH`, `~/.local/bin`, cargo's `bin`, Go's `GOBIN` (moved) and `GOPATH`'s
    /// `bin`, in them or holding them; a toolchain's shared cache (Go's module cache, cargo's registry and git —
    /// at their defaults and moved by `GOPATH`, `GOMODCACHE`, `CARGO_HOME`), in or holding one; and a toolchain's
    /// home (cargo's, rustup's, Gradle's, `GOPATH`, npm's), what holds one and all in one — Gradle's compiled
    /// scripts and wrapper distributions, rustup's toolchains, what `npx` runs from npm's `_npx` (at its default
    /// and moved by `npm_config_cache` in either case). A cache the toolchain names is judged but for its own
    /// places: another's cache or home still refuses it. Each said why. (Names only.)
    #[test]
    fn what_runs_later_is_never_an_asked_folder() {
        let (home, root) = (PathBuf::from("/Users/u"), PathBuf::from("/Users/u/work/p"));
        let env = HashMap::from([
            ("GOBIN".to_string(), "/opt/gobin".into()),
            ("GRADLE_USER_HOME".to_string(), "/opt/g/gradle".into()),
            ("GOPATH".to_string(), "/opt/gopath".into()),
            ("GOMODCACHE".to_string(), "/opt/modcache".into()),
            ("CARGO_HOME".to_string(), "/opt/cargo".into()),
            ("npm_config_cache".to_string(), "/opt/npm".into()),
            ("NPM_CONFIG_CACHE".to_string(), "/opt/npm-upper".into()),
        ]);
        let vars = vars_in(&home, &Here { env, ..Here::default() });
        let later = RunsLater::new(&home, &vars, &[PathBuf::from("/opt/tools/bin")]);
        let cache = "it's in a toolchain's shared cache";
        let in_home = "it's in a toolchain's home";
        let cases: [(PathBuf, &str); 32] = [
            // (the shared caches: at their defaults, and where a home or variable moved them)
            (home.join("go/pkg/mod"), cache),
            (home.join("go/pkg/mod/cache/download"), cache),
            ("/opt/gopath/pkg/mod/x".into(), cache),
            ("/opt/modcache".into(), cache),
            ("/opt/cargo/registry/src".into(), cache),
            ("/opt/cargo/git".into(), cache),
            (home.join("go/pkg"), "it holds a toolchain's shared cache"),
            ("/opt/gopath/bin".into(), "it's where programs you run are"),
            ("/opt/g/gradle".into(), "it's a toolchain's home"),
            ("/opt/g".into(), "it holds a toolchain's home"),
            ("/opt/tools/bin".into(), "it's where programs you run are"),
            ("/opt/tools".into(), "it holds where programs you run are"),
            (home.join(".local/bin/sub"), "it's where programs you run are"),
            (home.join(".local"), "it holds where programs you run are"),
            (home.join(".cargo/bin"), "it's where programs you run are"),
            ("/opt/gobin".into(), "it's where programs you run are"),
            (home.join("go/bin"), "it's where programs you run are"),
            (home.join("go"), "it holds where programs you run are"),
            (home.join(".cargo"), "it holds where programs you run are"),
            (home.join(".rustup"), "it's a toolchain's home"),
            (home.join(".gradle"), "it's a toolchain's home"),
            // (in a home: what the user's own builds and proxies run — at its default and where it's moved)
            (home.join(".gradle/caches"), in_home),
            (home.join(".gradle/wrapper/dists"), in_home),
            ("/opt/g/gradle/caches/x".into(), in_home),
            (home.join(".rustup/toolchains/x/bin"), in_home),
            ("/opt/cargo/bin".into(), "it's where programs you run are"),
            ("/opt/gopath/src".into(), in_home),
            (home.join(".npm"), "it's a toolchain's home"),
            (home.join(".npm/_npx/0a1b/node_modules"), in_home),
            ("/opt/npm/_npx".into(), in_home),
            ("/opt/npm-upper/_cacache".into(), in_home),
            (home.join("tools"), ""),
        ];
        // (every case's verdict, the wrong ones said together)
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(d, why)| {
                let got = ask_dir_verdict(d, &root, Some(&home), &later);
                let right = match why.is_empty() {
                    true => got.is_ok(),
                    false => got.as_ref().is_err_and(|w| w.starts_with(why)),
                };
                (!right).then(|| format!("{}: {got:?}", d.display()))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
        // (a home's place, not only its default: GOBIN moved — and the default too)
        assert!(
            later.refuses(&home.join("go/bin")).is_some() && later.refuses(Path::new("/opt/gobin")).is_some()
        );
        // (a cache the toolchain names is judged but for its own places — kept by the caches rule — in Go's home
        // only where the pack gives it one, wherever `GOMODCACHE` points; never another's cache or home)
        let verdict = |d: &Path, later: &RunsLater| ask_dir_verdict(d, &root, Some(&home), later);
        let but = later.but_cache("GOMODCACHE");
        for d in [home.join("go/pkg/mod"), "/opt/gopath/pkg/mod".into(), "/opt/modcache".into()] {
            assert_eq!(verdict(&d, &but), Ok(()), "{}", d.display());
        }
        let refused = |d: &Path, later: &RunsLater| verdict(d, later).unwrap_err();
        assert!(refused(Path::new("/opt/gopath/src"), &but).starts_with(in_home));
        assert!(refused(Path::new("/opt/cargo/registry"), &but).starts_with(cache));
        assert!(refused(&home.join(".gradle/caches"), &but).starts_with(in_home));
        assert!(refused(&home.join("go/bin"), &but).starts_with("it's where programs you run are"));
        // (Go's module cache moved into a moved cargo home's registry: cargo's cache, not Go's to write)
        let env = HashMap::from([
            ("CARGO_HOME".to_string(), "/opt/cargo".into()),
            ("GOMODCACHE".to_string(), "/opt/cargo/registry".into()),
        ]);
        let vars = vars_in(&home, &Here { env, ..Here::default() });
        let but = RunsLater::new(&home, &vars, &[]).but_cache("GOMODCACHE");
        assert_eq!(
            refused(Path::new("/opt/cargo/registry"), &but),
            "it's in a toolchain's shared cache (/opt/cargo/registry) — written only with the network open"
        );
        // (… or anywhere else in cargo's home: not a place the pack gives Go's cache)
        let env = HashMap::from([
            ("CARGO_HOME".to_string(), "/opt/cargo".into()),
            ("GOMODCACHE".to_string(), "/opt/cargo/mods".into()),
        ]);
        let vars = vars_in(&home, &Here { env, ..Here::default() });
        let but = RunsLater::new(&home, &vars, &[]).but_cache("GOMODCACHE");
        assert!(refused(Path::new("/opt/cargo/mods"), &but).starts_with(in_home));
        // (a `GOPATH` that's a list: the module cache and `bin` in its first entry, every entry a home)
        let env = HashMap::from([("GOPATH".to_string(), "/a:/b".into())]);
        let later = RunsLater::new(&home, &vars_in(&home, &Here { env, ..Here::default() }), &[]);
        let cases = [
            ("/a/pkg/mod/x", cache),
            ("/a/bin", "it's where programs you run are"),
            ("/a/src", in_home),
            ("/a", "it holds where programs you run are"),
            ("/b", "it's a toolchain's home"),
            ("/b/pkg/mod", in_home),
            ("/b/bin", in_home),
        ];
        for (d, why) in cases {
            assert!(
                refused(Path::new(d), &later).starts_with(why),
                "{d}: {:?}",
                refused(Path::new(d), &later)
            );
        }
        let but = later.but_cache("GOMODCACHE");
        assert_eq!(verdict(Path::new("/a/pkg/mod"), &but), Ok(()));
        assert!(refused(Path::new("/b/pkg/mod"), &but).starts_with(in_home));
    }

    /// A toolchain's shared cache its `ask` names (Go's module cache) is kept as the packs' `caches` are: read,
    /// written only with the network open, said when a write there was kept — and judged as any folder it
    /// names (one that's a home stays closed). (Names only.)
    #[test]
    fn an_asked_cache_is_kept_as_caches_are() {
        let (home, root) = (PathBuf::from("/Users/u"), PathBuf::from("/Users/u/work/p"));
        let modcache = home.join("go/pkg/mod");
        let said = |d: &Path| CachesSaid {
            dirs: vec![d.into()],
            rule: "the cache".into(),
            said: "kept {caches}".into(),
            xattr: None,
        };
        let place = crate::sandboxpack::CachePlace {
            at: modcache.clone(),
            asked: Some("GOMODCACHE".into()),
            given: true,
        };
        let later = RunsLater { homes: vec![home.join("go")], caches: vec![place], ..Default::default() };
        let env = |network: bool| Env {
            root: root.clone(),
            home: Some(home.clone()),
            reads: Reads { home: Some(home.clone()), ..Reads::default() },
            asked_caches: vec![
                ("GOMODCACHE".into(), "go's GOMODCACHE".into(), said(&modcache)),
                ("GOMODCACHE".into(), "go's GOMODCACHE".into(), said(&home.join("go"))),
            ],
            runs_later: later.clone(),
            network,
            ..Env::default()
        };
        let closed = Policy::for_project(&env(false));
        assert_eq!(closed.caches, std::slice::from_ref(&modcache));
        assert!(!closed.write.contains(&modcache) && closed.reads.allow.contains(&modcache), "{closed:?}");
        assert!(closed.reads_allow(&modcache.join("x@v1/x.go")) && !closed.writes_allow(&modcache.join("x")));
        let (go, why) = &closed.refused_asked[0];
        assert_eq!(
            (go, why.as_str()),
            (&home.join("go"), "go's GOMODCACHE — it's a toolchain's home (/Users/u/go)")
        );
        let d = Denial {
            process: "go".into(),
            op: "file-write-create".into(),
            target: modcache.join("x").display().to_string(),
        };
        let w = Watch::quiet("asked-cache.");
        w.record(d);
        let report = Guard::In(Box::new(Policy { watch: Some(w), ..closed })).report(&[]);
        assert!(report.contains(&format!("kept {}", crate::allow::tilde(&modcache))), "{report}");
        assert!(report.contains("(not the cache — while the network is closed)"), "{report}");
        // (cargo's file-sync mark: on cargo's caches only — C0, C1 — never another tool's)
        let both = Policy::for_project(&Env { caches: cargo_at(&home.join(".cargo")).caches, ..env(false) });
        assert_eq!(both.caches[2], modcache);
        let profile = both.profile();
        let marks: Vec<&str> = profile.lines().filter(|l| l.contains("file-write-xattr")).collect();
        assert_eq!(
            marks,
            [
                "(allow file-write-xattr (require-all (require-any (literal (param \"C0\")) (literal (param \"C1\"))) \
              (xattr \"com.apple.fileprovider.ignore#P\")))"
            ]
        );
        let open = Policy::for_project(&env(true));
        assert!(
            open.caches.is_empty()
                && open.write.contains(&modcache)
                && open.writes_allow(&modcache.join("x"))
        );
        assert!(!open.write.contains(&home.join("go")), "{:?}", open.write);
    }

    /// A home a tool's daemon lives from (the java pack's Gradle row, `daemon`): on Linux, a Gradle project's runs
    /// get one kept per project and policy — its name ends with the policy's key as `ran` takes it, made only
    /// tarae's, written by those runs, the daemon left on (the user's `GRADLE_OPTS`, then the row's `daemon`
    /// appends: Kotlin's registry in it as `{home}`, both daemons' idle ends — no `org.gradle.daemon=false`), its
    /// own folders made; the same project and policy find it again, another policy (a port opened) another. Its
    /// stamp is held while the policy lives — a prune leaves it — and once that's gone a prune takes it. On
    /// macOS, or in a project with none of Gradle's files: the run's own, the daemon off (`fresh`), nothing of
    /// Kotlin's. (A stand-in home in the test's own folder; no Gradle is run.)
    #[test]
    fn a_daemons_home_is_kept_per_project_and_policy() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;
        let tmp = std::env::temp_dir();
        let tmp = if tmp.starts_with(TMP) { PathBuf::from("/var/tmp") } else { tmp };
        let base = tmp.join(format!("tarae-daemon-home-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        for d in ["home/work/g", "home/work/plain"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let base = std::fs::canonicalize(&base).unwrap();
        let home = base.join("home");
        let (g, plain) = (home.join("work/g"), home.join("work/plain"));
        std::fs::write(g.join("settings.gradle.kts"), "").unwrap();
        let here = |os: Os| {
            let env = [("HOME", home.clone().into()), ("GRADLE_OPTS", "-Xmx1g".into())];
            Here {
                env: env.map(|(k, v): (&str, std::ffi::OsString)| (k.to_string(), v)).into(),
                os,
                ..Here::default()
            }
        };
        let policy = |root: &Path, os: Os, ports: Vec<u16>| {
            toolchain_found_in(root, ToolDirs::default(), &here(os));
            let llm = crate::llm::LlmConfig { sandbox_ports: ports, ..Default::default() };
            policy_in(root, &llm, &[], &here(os))
        };
        let var = |p: &Policy, k: &str| p.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let kept = home.join(".cache/tarae/run-homes/gradle");
        let a = policy(&g, Os::Linux, Vec::new());
        let key = policy_key(&a);
        let ours = PathBuf::from(var(&a, "GRADLE_USER_HOME").unwrap());
        assert_eq!(ours, kept.join(format!("{}-{}", project_key(&g), policy_key(&a))), "the key `ran` takes");
        assert_eq!(std::fs::metadata(&ours).unwrap().permissions().mode() & 0o777, 0o700, "only tarae's");
        assert!(a.write.contains(&ours) && a.writes_allow(&ours.join("caches/x")));
        let opts = format!(
            "-Xmx1g -Dkotlin.daemon.options=runFilesPath={}/kotlin-daemon,autoshutdownIdleSeconds=1800 \
             -Dorg.gradle.daemon.idletimeout=1800000",
            ours.display()
        );
        assert_eq!(var(&a, "GRADLE_OPTS"), Some(opts), "the daemons on, ending themselves");
        assert!(ours.join("kotlin-daemon").is_dir() && ours.join("wrapper").is_dir());
        let again = policy(&g, Os::Linux, Vec::new());
        assert_eq!(
            var(&again, "GRADLE_USER_HOME"),
            var(&a, "GRADLE_USER_HOME"),
            "the same project and policy"
        );
        let other = PathBuf::from(var(&policy(&g, Os::Linux, vec![8080]), "GRADLE_USER_HOME").unwrap());
        assert!(other.starts_with(&kept) && other != ours, "another policy: {other:?}");
        // Held while a policy lives: a prune leaves it; let go, a prune of homes unused that long takes it
        let later = std::time::SystemTime::now() + Duration::from_secs(400 * 24 * 3600);
        let root = kept.parent().unwrap();
        let day = Duration::from_secs(24 * 3600);
        assert!(crate::runhomes::prune(root, later, day, |_| false).iter().all(|(h, ..)| *h != ours));
        assert!(ours.join("kotlin-daemon").is_dir(), "kept while held");
        drop((a, again));
        // (a process another test forks shares the stamp's lock until its `exec` closes it: looked for again
        // while that's so — a lock never let go fails all the same)
        let went = (0..100).find_map(|_| {
            let went = crate::runhomes::prune(root, later, day, |_| false);
            let gone = went.iter().any(|(h, ..)| *h == ours);
            gone.then_some(went).or_else(|| {
                std::thread::sleep(Duration::from_millis(20));
                None
            })
        });
        assert!(went.is_some() && !ours.exists(), "never let go");
        // A stamp that can't be made (a folder where it goes): no kept home for that run — the run's own, the
        // kept one not written — and the same key, so `ran` doesn't end the same policy's daemon
        let stamp = ours.with_file_name(format!("{}.stamp", ours.file_name().unwrap().to_string_lossy()));
        std::fs::create_dir_all(&stamp).unwrap();
        let p = policy(&g, Os::Linux, Vec::new());
        let run = p.run.as_ref().unwrap().path().join("gradle");
        assert_eq!(var(&p, "GRADLE_USER_HOME"), Some(run.display().to_string()));
        assert!(p.kept.is_empty() && !p.write.contains(&ours) && !p.writes_allow(&ours.join("caches/x")));
        assert_eq!(policy_key(&p), key, "the same policy");
        drop(p);
        std::fs::remove_dir(&stamp).unwrap();
        // macOS, and a project with none of Gradle's files: the run's own, the daemon off, nothing of Kotlin's
        for (root, os) in [(&g, Os::Mac), (&plain, Os::Linux)] {
            let p = policy(root, os, Vec::new());
            let run = p.run.as_ref().unwrap().path().join("gradle");
            assert_eq!(var(&p, "GRADLE_USER_HOME"), Some(run.display().to_string()), "{os:?}");
            assert_eq!(var(&p, "GRADLE_OPTS").as_deref(), Some("-Xmx1g -Dorg.gradle.daemon=false"), "{os:?}");
            assert!(!p.write.iter().any(|w| w.starts_with(&kept)) && p.kept.is_empty(), "{os:?}");
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// What a run home appends to (the java pack's `GRADLE_OPTS`) is the user's value as the run gets it: one
    /// holding a fake password (`-D…Password=`, left out as token-like) gives the run only tarae's append — the
    /// fake never comes back through it, on either system's appends; a harmless one still comes first.
    #[test]
    fn a_run_homes_append_starts_from_the_runs_own_value() {
        let tmp = std::env::temp_dir();
        let tmp = if tmp.starts_with(TMP) { PathBuf::from("/var/tmp") } else { tmp };
        let base = tmp.join(format!("tarae-append-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("home/work/g")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (home, g) = (base.join("home"), base.join("home/work/g"));
        std::fs::write(g.join("settings.gradle.kts"), "").unwrap();
        let opts_in_run = |opts: &str, os: Os| {
            let env = [("HOME", home.clone().into()), ("GRADLE_OPTS", opts.into())];
            let env = env.map(|(k, v): (&str, std::ffi::OsString)| (k.to_string(), v)).into();
            let here = Here { env, os, ..Here::default() };
            toolchain_found_in(&g, ToolDirs::default(), &here);
            let p = policy_in(&g, &crate::llm::LlmConfig::default(), &[], &here);
            let c = command(Some(&p), "/bin/sh", &["-c".into(), "true".into()]);
            let opts = c.get_envs().find(|(k, _)| *k == "GRADLE_OPTS").and_then(|(_, v)| v);
            opts.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default()
        };
        let fake = format!("-Xmx2g -Dorg.gradle.project.repoPassword={FAKE}");
        let mac = opts_in_run(&fake, Os::Mac);
        assert_eq!(mac, "-Dorg.gradle.daemon=false", "tarae's append only");
        let linux = opts_in_run(&fake, Os::Linux);
        assert!(linux.starts_with("-Dkotlin.daemon.options=") && !linux.contains("Xmx2g"), "{linux}");
        assert!(!mac.contains(FAKE) && !linux.contains(FAKE));
        assert_eq!(opts_in_run("-Xmx2g", Os::Mac), "-Xmx2g -Dorg.gradle.daemon=false", "the user's first");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Go's build cache for runs is the project's own, kept across its runs in tarae's cache folder — made only
    /// tarae's — pointed at by `GOCACHE` and written by those runs only; the user's own (where `go env` says it
    /// is) is never read; another project gets another; a project without `go.mod` gets the run's own as
    /// before; and the toolchain is asked, and runs, with `GOTOOLCHAIN=local`. (A stand-in home in the test's
    /// own folder; no go is run — what it would say is given.)
    #[test]
    fn gos_build_cache_is_the_projects_own() {
        use std::os::unix::fs::PermissionsExt;
        // (never in the shared temp folder: on Linux that's `/tmp`, which every run writes)
        let tmp = std::env::temp_dir();
        let tmp = if tmp.starts_with(TMP) { PathBuf::from("/var/tmp") } else { tmp };
        let base = tmp.join(format!("tarae-gocache-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let made = [
            "work/a",
            "work/b",
            "work/plain",
            "go/gocache",
            "go/pkg/mod",
            "Library/Caches/go-build",
            "Library/Caches/x",
        ];
        for d in made {
            std::fs::create_dir_all(base.join("home").join(d)).unwrap();
        }
        let base = std::fs::canonicalize(&base).unwrap();
        let home = base.join("home");
        for p in ["a", "b"] {
            std::fs::write(home.join("work").join(p).join("go.mod"), "module p\n").unwrap();
        }
        let here =
            Here { env: HashMap::from([("HOME".to_string(), home.clone().into())]), ..Here::default() };
        let llm = crate::llm::LlmConfig::default();
        let policy = |p: &str| {
            let root = home.join("work").join(p);
            let found = ToolDirs {
                caches: vec![(home.join("go/pkg/mod"), "GOMODCACHE".into(), "go's GOMODCACHE".into())],
                kept: vec![home.join("go/gocache")],
                ..ToolDirs::default()
            };
            toolchain_found_in(&root, found, &here);
            // (the user let Claude read their caches folder: the build cache in it stays kept)
            policy_in(&root, &llm, &[home.join("Library/Caches")], &here)
        };
        let var = |p: &Policy, k: &str| p.env.iter().find(|(n, _)| n == k).map(|(_, v)| PathBuf::from(v));
        let (a, b) = (policy("a"), policy("b"));
        // (the module cache Go named, at its default place: a cache of the project's — not refused as one)
        assert_eq!((&a.caches[..], &a.refused_asked[..]), (&[home.join("go/pkg/mod")][..], &[][..]));
        let kept = home.join(".cache/tarae/run-homes/go-build");
        let ours = kept.join(project_key(&home.join("work/a")));
        assert_eq!(var(&a, "GOCACHE"), Some(ours.clone()));
        let mode = std::fs::metadata(&ours).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "only tarae's");
        assert!(
            a.write.contains(&ours) && a.writes_allow(&ours.join("ab/x-d")) && a.reads_allow(&ours.join("x"))
        );
        let theirs = var(&b, "GOCACHE").unwrap();
        assert!(
            theirs.starts_with(&kept) && theirs != ours && !b.writes_allow(&ours.join("x")),
            "{theirs:?}"
        );
        // (the user's own build cache: neither read nor written, at its default or where `go env` says — though
        // what holds each is read)
        for user in [home.join("Library/Caches/go-build/ab/x"), home.join("go/gocache/ab/x")] {
            assert!(!a.reads_allow(&user) && !a.writes_allow(&user), "{}", user.display());
        }
        assert!(
            a.reads_allow(&home.join("Library/Caches/x/y"))
                && a.reads_allow(&home.join("go/pkg/mod/m@v1/x.go"))
        );
        let plain = policy("plain");
        let run = plain.run.as_ref().unwrap().path().join("go-build");
        assert_eq!(var(&plain, "GOCACHE"), Some(run), "no go.mod: the run's own");
        assert!(!plain.write.iter().any(|w| w.starts_with(&kept)));
        assert!(a.env.contains(&("GOTOOLCHAIN".into(), "local".into())), "{:?}", a.env);
        let asks = asks_for(&home.join("work/a"), Some(&home), &here);
        assert!(asks.env.contains(&("GOTOOLCHAIN".into(), "local".into())), "{:?}", asks.env);
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

    /// Reads in the profile (card model P2), by parameters only: home's contents denied, then the project,
    /// where it writes and the allowed places opened (a file's metadata stays open); the clipboard and the
    /// keychain's servers never; and last of all, so nothing before opens them, the kept places — but the
    /// project where one holds it, and tarae's log after that. Without a home, no read rules.
    #[test]
    fn reads_in_the_profile() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let text = p.profile();
        assert!(!text.contains("/Users/u"), "{text}");
        let at = |rule: &str| text.find(rule).unwrap_or_else(|| panic!("no {rule} in {text}"));
        let home = at("(deny file-read-data file-read-xattr (subpath (param \"HOME\")))");
        let open =
            at("(allow file-read-data file-read-xattr (subpath (param \"ROOT\")) (subpath (param \"W0\"))");
        assert!(
            home < open
                && text[open..].contains("(literal (param \"F0\")) ")
                && text.contains("(subpath (param \"R1\"))")
        );
        assert!(!text.contains("file-read-metadata"), "metadata stays open: {text}");
        let link = at("(allow file-write* (subpath (param \"W0\"))");
        at(
            "(deny mach-lookup (global-name-regex #\"^com\\.apple\\.pasteboard\\.\") (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd\"))",
        );
        let kept =
            at("(deny file-read-data file-read-xattr (subpath (param \"X0\")) (subpath (param \"X1\")) \
                       (subpath (param \"X2\")))");
        let log = at("(allow file-read-data file-read-xattr (literal (param \"LOG\")))");
        assert!(kept > at("(deny network-outbound (remote unix-socket))") && kept > link && log > kept);
        assert!(text[kept..].lines().count() == 2, "the kept places last, then the log: {text}");
        let params = p.params();
        for (k, v) in [
            ("HOME", "/Users/u"),
            ("R0", "/Users/u/.cargo"),
            ("X0", "/Users/u/.ssh"),
            ("X2", "/Users/u/.local/state/tarae"),
            ("LOG", "/Users/u/.local/state/tarae/tarae.log"),
        ] {
            assert!(params.contains(&(k.into(), v.into())), "{k}: {params:?}");
        }
        // A checkout inside a kept place: kept but for the project
        let inside = Policy::for_project(&env(Path::new("/Users/u/.claude/worktrees/p")));
        let text = inside.profile();
        assert!(
            text.contains(
                "(deny file-read-data file-read-xattr (subpath (param \"X0\")) (subpath (param \"X2\")))"
            ) && text.contains(
                "(deny file-read-data file-read-xattr (require-all (require-any (subpath (param \"X1\"))) \
                     (require-not (subpath (param \"ROOT\")))))"
            ),
            "{text}"
        );
        assert!(inside.reads_allow(Path::new("/Users/u/.claude/worktrees/p/src/a.rs")));
        assert!(!inside.reads_allow(Path::new("/Users/u/.claude/settings.json")));
        // The rules in Rust, as the report judges the output by them
        for (path, read) in [
            ("/Users/u/p/src/a.rs", true),
            ("/Users/u/.cargo/registry/index/x", true),
            ("/Users/u/.gitconfig", true),
            ("/Users/u/.cargo/build/ab/cd12/x", true),
            ("/Users/u/Library/Caches/thing/x", true),
            ("/opt/homebrew/bin/go", true),
            ("/Users/u/.local/state/tarae/tarae.log", true),
            ("/Users/u/other/x", false),
            ("/Users/u/.ssh/id_ed25519", false),
            ("/Users/u/.local/state/tarae/chats/x.json", false),
        ] {
            assert_eq!(p.reads_allow(Path::new(path)), read, "{path}");
        }
        let none = Policy::for_project(&Env { reads: Reads::default(), ..env(Path::new("/Users/u/p")) });
        let text = none.profile();
        assert!(!text.contains("file-read") && !text.contains("HOME"), "{text}");
        assert!(none.reads_allow(Path::new("/Users/u/.ssh/id")), "not confined without a home");
    }

    /// Settings that hold a token (`tokens.rs`): each file denied by itself after the allows (`~/.gitconfig`
    /// is one), named only by a parameter; the copy's variable in the run's environment; the report says
    /// which were copied, and a read of one refused says the tool reads a copy — nothing said where none was.
    #[test]
    fn settings_holding_a_token_are_read_from_copies() {
        let copy = PathBuf::from("/private/var/folders/ab/cd/T/tarae-1-0/settings/gitconfig");
        let gitconfig = PathBuf::from("/Users/u/.gitconfig");
        let mut e = env(Path::new("/Users/u/p"));
        e.reads.copies = crate::tokens::Copies {
            env: vec![("GIT_CONFIG_GLOBAL".into(), copy.clone())],
            hidden: vec![(gitconfig.clone(), "GIT_CONFIG_GLOBAL".into())],
            left_out: vec![(gitconfig.clone(), "GIT_CONFIG_GLOBAL".into())],
            told: vec![],
        };
        let p = Policy::for_project(&e);
        let text = p.profile();
        let deny = "(deny file-read-data file-read-xattr (literal (param \"T0\")))";
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s}: {text}"));
        assert!(at(deny) > at("(allow file-read-data file-read-xattr (subpath (param \"ROOT\"))"), "{text}");
        assert!(p.params().contains(&("T0".into(), "/Users/u/.gitconfig".into())));
        assert!(!text.contains("/Users/u"), "paths only as parameters");
        assert!(!p.reads_allow(&gitconfig) && p.reads_allow(Path::new("/Users/u/.cargo/config.toml")));
        let c = command(Some(&p), "git", &[]);
        let set: Vec<_> = c.get_envs().filter(|(k, _)| k.to_str() == Some("GIT_CONFIG_GLOBAL")).collect();
        assert_eq!(set, [(std::ffi::OsStr::new("GIT_CONFIG_GLOBAL"), Some(copy.as_os_str()))]);
        // Its report: what was copied, and a refused read of the file itself
        let refused = format!("fatal: unable to access '{}': Operation not permitted", gitconfig.display());
        let r = Guard::In(Box::new(p.clone())).report(&[(true, refused)]);
        let shown = crate::allow::tilde(&gitconfig);
        assert!(r.contains(&format!(
            "{shown} ($GIT_CONFIG_GLOBAL): it holds a token, so your commands read a copy"
        )));
        assert!(r.contains(&format!(
            "The tools read copies of {shown} without their token lines (GIT_CONFIG_GLOBAL)."
        )));
        assert!(!r.contains("If the command needs one of these"), "nothing left to open: {r}");
        let none = Guard::In(Box::new(Policy::for_project(&env(Path::new("/Users/u/p"))))).report(&[]);
        assert!(!none.contains("copies of") && !none.contains("T0"), "{none}");
    }

    /// What a run never reads holds without any pack — `KEPT_FLOOR` beside `allow::SECRETS`, Claude's own,
    /// tarae's state — and the packs' `kept` adds where a variable moved a home: `GRADLE_USER_HOME` elsewhere,
    /// both daemons' registries are the profile's denies (the measured escape stays closed).
    #[test]
    fn the_kept_floor_holds_without_packs() {
        let home = Path::new("/Users/u");
        let kept = kept_in(home, &vars_in(home, &Here::default()), &[]);
        for s in crate::allow::SECRETS.iter().chain(&CLAUDES).chain(&KEPT_FLOOR) {
            assert!(kept.contains(&home.join(s)), "{s}: {kept:?}");
        }
        assert!(crate::config::state_dir().is_none_or(|s| kept.contains(&profile_path(&s))), "{kept:?}");
        let moved = Path::new("/opt/tarae-gradle-home");
        let env = HashMap::from([("GRADLE_USER_HOME".to_string(), moved.into())]);
        let vars = vars_in(home, &Here { env, ..Here::default() });
        let reads = Reads { home: Some(home.into()), kept: kept_reads(home, &vars), ..Reads::default() };
        let p = Policy::for_project(&Env { root: home.join("p"), reads, ..Env::default() });
        let denied: Vec<String> =
            p.params().into_iter().filter(|(k, _)| k.starts_with('X')).map(|(_, v)| v).collect();
        for d in [home.join(".gradle/daemon"), moved.join("daemon")] {
            assert!(denied.contains(&d.display().to_string()), "{}: {denied:?}", d.display());
        }
    }

    /// Where a run reads in home: what the toolchains were measured reading, the repository's other checkouts
    /// and grants (`also`) — never home itself, a place outside it, nor one in a kept place, granted or not.
    /// (A stand-in home in the test's own temp folder.)
    #[test]
    fn reads_for_a_home() {
        let home = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-home-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        for d in [".cargo", ".ssh", "Library/Fonts", "src/other", ".claude/x"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let also = [
            home.join("src/other"),
            home.clone(),
            home.join(".ssh/x"),
            home.join(".claude/x"),
            "/opt/x".into(),
        ];
        // (nothing from this process's environment: the same inside a sandboxed run, which has a Gradle home of
        // its own, as outside)
        let vars = vars_in(&home, &Here::default());
        let r = reads_for(&home, &also, &vars);
        assert_eq!(r.home.as_deref(), Some(home.as_path()));
        for p in [
            ".cargo",
            ".rustup",
            ".gitconfig",
            ".config/git",
            ".CFUserTextEncoding",
            "go",
            "Library/Fonts",
            ".npm",
            ".pip/pip.conf",
            ".config/pip/pip.conf",
            "Library/Application Support/pip/pip.conf",
            "src/other",
            "Library/Java/JavaVirtualMachines",
            ".m2/repository",
            ".m2/wrapper",
        ] {
            assert!(r.allow.contains(&home.join(p)), "{p}: {:?}", r.allow);
        }
        // Java's: Gradle's downloaded dependencies and wrapper distributions, never its daemons' registry nor
        // either tool's settings (repository passwords)
        let gradle = vars.home_of("gradle").unwrap().0.to_path_buf();
        assert_eq!(gradle, home.join(".gradle"));
        for p in ["caches/modules-2", "wrapper/dists"] {
            assert!(r.allow.contains(&gradle.join(p)), "{p}: {:?}", r.allow);
        }
        assert!(r.kept.contains(&gradle.join("daemon")));
        // `GRADLE_USER_HOME` elsewhere: its reads, and its daemons kept as well as `~/.gradle`'s
        let moved = home.join("gradle-elsewhere");
        std::fs::create_dir_all(&moved).unwrap();
        let env = HashMap::from([("GRADLE_USER_HOME".to_string(), moved.clone().into())]);
        let r2 = reads_for(&home, &also, &vars_in(&home, &Here { env, ..Here::default() }));
        assert!(r2.allow.contains(&moved.join("caches/modules-2")), "{:?}", r2.allow);
        assert!(!r2.allow.contains(&home.join(".gradle/caches/modules-2")));
        assert!(r2.kept.contains(&home.join(".gradle/daemon")) && r2.kept.contains(&moved.join("daemon")));
        let policy =
            Policy::for_project(&Env { root: home.join("proj"), reads: r.clone(), ..Env::default() });
        for (path, read) in [
            (gradle.join("caches/modules-2/files-2.1/a.jar"), true),
            (gradle.join("daemon/9.7.1/registry.bin"), false),
            (gradle.join("gradle.properties"), false),
            (gradle.join("caches/9.7.1/scripts/x"), false),
            (home.join(".m2/settings.xml"), false),
        ] {
            assert_eq!(policy.reads_allow(&path), read, "{}", path.display());
        }
        assert!(
            crate::allow::read_choices(&gradle.join("daemon/9.7.1/registry.bin"), &home, &r.kept).is_empty()
        );
        for p in [home.clone(), home.join(".ssh/x"), home.join(".claude/x"), "/opt/x".into()] {
            assert!(!r.allow.contains(&p), "{}: {:?}", p.display(), r.allow);
        }
        for p in [
            ".ssh",
            ".aws",
            ".config/gh",
            ".config/git/credentials",
            ".git-credentials",
            "Library/Keychains",
            ".claude",
            ".claude.json",
            ".cargo/credentials.toml",
        ] {
            assert!(r.kept.contains(&home.join(p)), "{p}: {:?}", r.kept);
        }
        assert!(r.kept.iter().any(|k| crate::config::state_dir().is_some_and(|s| *k == profile_path(&s))));
        // Cargo's configs above a project, up to home: those files only
        let above = reads_above(&home.join("src/other/p"), &home);
        let want = [
            "src/other/.cargo/config.toml",
            "src/other/.cargo/config",
            "src/.cargo/config.toml",
            "src/.cargo/config",
        ];
        assert_eq!(above, want.map(|w| home.join(w)));
        assert!(reads_above(Path::new("/opt/p"), &home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// The files the packs' `write-files` give with the user's Gradle home at `gradle` (`GRADLE_USER_HOME`; no
    /// cargo home beside it).
    fn locks_in(gradle: &Path) -> Vec<PathBuf> {
        let env = HashMap::from([("GRADLE_USER_HOME".to_string(), gradle.into())]);
        let home = gradle.join("no-home");
        caches_and_files(&vars_in(&home, &Here { env, ..Here::default() })).1
    }

    /// A run's own Gradle home (Java under P2): `GRADLE_USER_HOME` in the run's folder, the user's wrapper
    /// distributions linked in — their lock files the only writes in the user's Gradle home — the user's
    /// dependencies read in place, no daemon after the build, after the user's own `GRADLE_OPTS`. Removing the
    /// run's folder takes the link, never what it points at. (Stand-ins in the test's own temp folder.)
    #[test]
    fn a_runs_own_gradle_home() {
        let t = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-gradle-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let (user, run) = (t.join("user/.gradle"), t.join("run"));
        let dist = user.join("wrapper/dists/gradle-9.7.1-bin/h1");
        for d in [dist.join("gradle-9.7.1-bin"), user.join("wrapper/dists/gradle-8.9-bin/h2"), run.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(user.join("caches/modules-2")).unwrap();
        std::fs::write(dist.join("gradle-9.7.1-bin/marker"), "kept").unwrap();
        assert_eq!(
            locks_in(&user),
            [
                user.join("wrapper/dists/gradle-8.9-bin/h2/gradle-8.9-bin.zip.lck"),
                dist.join("gradle-9.7.1-bin.zip.lck")
            ]
        );
        let vars = made_with(&run, &user, Some(" -Xmx1g "));
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("GRADLE_USER_HOME"), Some(run.join("gradle").display().to_string()));
        assert_eq!(get("GRADLE_RO_DEP_CACHE"), Some(user.join("caches").display().to_string()));
        assert_eq!(get("GRADLE_OPTS").as_deref(), Some("-Xmx1g -Dorg.gradle.daemon=false"));
        let linked = run.join("gradle/wrapper/dists/gradle-9.7.1-bin/h1/gradle-9.7.1-bin/marker");
        assert_eq!(std::fs::read_to_string(linked).unwrap(), "kept");
        std::fs::remove_dir_all(&run).unwrap();
        assert!(dist.join("gradle-9.7.1-bin/marker").exists(), "the run's folder took the distribution");
        // No Gradle home of the user's: the run's own all the same, nothing to link or read in place
        let bare = made_with(&t.join("run2"), &t.join("no-gradle"), Some("  "));
        // (Go's build cache is a run home too: the run's own here, no project being Go's)
        let bare: Vec<&(String, String)> = bare.iter().filter(|(k, _)| k.starts_with("GRADLE")).collect();
        let keys: Vec<&str> = bare.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["GRADLE_USER_HOME", "GRADLE_OPTS"]);
        assert_eq!(bare[1].1, "-Dorg.gradle.daemon=false");
        assert!(t.join("run2/gradle/wrapper").is_dir() && !t.join("run2/gradle/wrapper/dists").exists());
        // In the policy: the lock files are writable files, the environment goes with every command
        let e =
            Env { run_homes: vec![gradle_at(&user)], files: locks_in(&user), ..env(Path::new("/Users/u/p")) };
        let p = Policy { env: vars, ..Policy::for_project(&e) };
        assert!(p.files.contains(&dist.join("gradle-9.7.1-bin.zip.lck")), "{:?}", p.files);
        let c = command(Some(&p), "true", &[]);
        let set: Vec<_> = c.get_envs().filter_map(|(k, v)| Some((k.to_str()?, v?.to_str()?))).collect();
        assert!(set.contains(&("GRADLE_OPTS", "-Xmx1g -Dorg.gradle.daemon=false")), "{set:?}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A kept run home is a folder earlier runs wrote in: what tarae (unsandboxed) makes there goes through no
    /// link a run left — where a made folder goes, where the wrapper link goes (pointing elsewhere), the home
    /// itself — and a folder where the link goes is left as it is. Nothing is made where a link points.
    /// (Gradle's home as a kept one, stand-ins in the test's own temp folder.)
    #[test]
    fn a_kept_home_is_made_through_no_link() {
        let t = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-unlinked-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let (user, run, kept, away) = (t.join("user/.gradle"), t.join("run"), t.join("kept"), t.join("away"));
        let dists = user.join("wrapper/dists");
        for d in [&dists, &run, &kept, &away] {
            std::fs::create_dir_all(d).unwrap();
        }
        let env = HashMap::from([("GRADLE_USER_HOME".to_string(), user.clone().into())]);
        let here = Here { env, ..Here::default() };
        let vars = vars_in(&user.join("no-home"), &here);
        let made = |kept: &Path| {
            // (as a home a daemon lives from: its own folders too)
            let k = Kept { name: "gradle".into(), home: kept.into(), daemon: true, held: Default::default() };
            let env = run_homes_made(&run, &vars, &Default::default(), &[k]);
            env.into_iter().find(|(k, _)| k == "GRADLE_USER_HOME").map(|(_, v)| PathBuf::from(v))
        };
        let symlink = |to: &Path, at: &Path| std::os::unix::fs::symlink(to, at).unwrap();
        let is_link = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
        // A link where the made folder goes: removed, the folder made, the wrapper's link in it — and where the
        // daemon's own folder goes too
        symlink(&away, &kept.join("wrapper"));
        symlink(&away, &kept.join("kotlin-daemon"));
        assert_eq!(made(&kept), Some(kept.clone()));
        assert!(!is_link(&kept.join("wrapper")) && kept.join("wrapper").is_dir());
        assert!(!is_link(&kept.join("kotlin-daemon")) && kept.join("kotlin-daemon").is_dir());
        assert_eq!(std::fs::read_link(kept.join("wrapper/dists")).unwrap(), dists);
        // A link where the wrapper's link goes, pointing elsewhere: replaced
        std::fs::remove_file(kept.join("wrapper/dists")).unwrap();
        symlink(&away, &kept.join("wrapper/dists"));
        made(&kept);
        assert_eq!(std::fs::read_link(kept.join("wrapper/dists")).unwrap(), dists);
        // (the same again: kept as it is)
        made(&kept);
        assert_eq!(std::fs::read_link(kept.join("wrapper/dists")).unwrap(), dists);
        // A folder where the link goes: left as it is, nothing linked in it
        std::fs::remove_file(kept.join("wrapper/dists")).unwrap();
        std::fs::create_dir(kept.join("wrapper/dists")).unwrap();
        made(&kept);
        assert!(!is_link(&kept.join("wrapper/dists")));
        assert_eq!(std::fs::read_dir(kept.join("wrapper/dists")).unwrap().count(), 0);
        // The kept home itself a link: the run's own instead, nothing made where it points
        let linked = t.join("linked");
        symlink(&away, &linked);
        assert_eq!(made(&linked), Some(run.join("gradle")));
        assert!(run.join("gradle/wrapper/dists").is_symlink());
        assert_eq!(std::fs::read_dir(&away).unwrap().count(), 0, "nothing made through a link");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A policy's key (`policy_key`): the same project, grants and settings give the same key, whatever is a
    /// run's own (its folder, its tag); a port opened since gives another.
    #[test]
    fn a_policys_key_is_what_it_allows() {
        let p = |root: &Path, ports: Vec<u16>| {
            let run = RunDir::new().ok();
            let e = Env { ports, ..env(root) };
            Policy { tag: run.as_ref().map(|r| r.tag()), run, ..Policy::for_project(&e) }
        };
        let root = Path::new("/w/p");
        let (a, b) = (p(root, Vec::new()), p(root, Vec::new()));
        assert_ne!(a.run, b.run);
        assert_eq!(policy_key(&a), policy_key(&b), "a run's own isn't in it");
        assert_ne!(policy_key(&a), policy_key(&p(root, vec![8080])), "a port opened");
        assert_ne!(policy_key(&a), policy_key(&p(Path::new("/w/q"), Vec::new())), "another project");
    }

    /// What Claude reads about Gradle (Java under P2): its writes in the user's Gradle home — the read-only
    /// dependency cache's index, measured in every build — are expected and said so, not offered to open; a
    /// JVM refused a port on this machine says Gradle needs this machine's ports, only while they're closed.
    #[test]
    fn what_a_run_is_told_about_gradle() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let report = |p: &Policy, d: &[Denial]| {
            let w = Watch::quiet("g.");
            for d in d {
                w.record(d.clone());
            }
            Guard::In(Box::new(Policy { watch: Some(w), ..p.clone() })).report(&[])
        };
        let java =
            |op: &str, target: &str| Denial { process: "java".into(), op: op.into(), target: target.into() };
        let index =
            java("file-write-data", "/Users/u/.gradle/caches/modules-2/metadata-2.107/module-artifact.bin");
        let r = report(&p, std::slice::from_ref(&index));
        assert!(r.contains("/Users/u/.gradle: the user's Gradle home stays as it is"), "{r}");
        assert!(
            !r.contains("If the command needs one of these") && !r.contains("Gradle build can't run"),
            "{r}"
        );
        // (a read there isn't Gradle's own doing: the run reads its own home)
        let read = java("file-read-data", "/Users/u/.gradle/gradle.properties");
        assert!(!report(&p, &[read]).contains("Gradle home stays"));
        let port = java("network-outbound", "remote:*:55417");
        let r = report(&p, &[port.clone(), index]);
        assert!(
            r.contains("a Gradle build can't run while they're closed")
                && r.contains("llm.sandbox-localhost"),
            "{r}"
        );
        for open in [Policy { localhost: true, ..p.clone() }, Policy { network: true, ..p.clone() }] {
            assert!(!report(&open, std::slice::from_ref(&port)).contains("Gradle build can't run"));
        }
        let other = Denial { process: "curl".into(), ..port };
        assert!(!report(&p, &[other]).contains("Gradle build can't run"));
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
            // the Java pack's, here as anywhere — a name of several parts in a row
            "build.gradle.kts",
            "app/Build.Gradle",
            "settings.gradle",
            "gradle.properties",
            "gradlew",
            "gradle/wrapper/gradle-wrapper.properties",
            "buildSrc/src/main/kotlin/x.kt",
            "pom.xml",
            "mvnw",
            ".mvn/jvm.config",
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
            "gradle/libs.versions.toml",
            "src/main/java/org/gradle/Api.java",
            "wrapper/x",
            "target/classes/META-INF/maven/p/pom.xml",
            ".venv/src/p/Makefile",
        ] {
            assert!(!kept(rel), "{rel}");
        }
        assert!(kept("xvenv/Makefile") && kept("a.venv/Makefile"), "only `.venv` itself");
        assert!(!protected_in(root, Path::new("/Users/u/elsewhere/build.rs")));
        let text = Policy::for_project(&env(root)).profile();
        assert!(!text.contains("/Users") && !text.contains("worktrees"), "{text}");
        let skip = "/[^/]+".repeat(6);
        // Every name in any case, a name of several parts joined with `/`, in regexes Seatbelt reads: none past
        // its string limit (a profile with one refuses every run — measured at 1032 characters)
        let regexes = name_regexes();
        assert!(regexes.len() > 1, "every pack's names fit one regex no more: {regexes:?}");
        let mut each = String::new();
        for names in &regexes {
            each.push_str(&format!(" (regex #\"^{skip}/(.*/)?({names})(/|$)\")"));
        }
        let rule = format!(
            "(deny file-write* (require-all (subpath (param \"ROOT\")) (require-any{each}) (require-not"
        );
        assert!(text.contains(&rule), "{text}");
        // (the packs' `outputs`, as written: `.venv`'s dot isn't any character)
        let outputs = "node_modules|\\.venv|__pycache__|target";
        assert!(
            text.contains(&format!("(regex #\"^{skip}/(.*/)?({outputs})/(.*/)?({})", regexes[0])),
            "{text}"
        );
        for n in protected_names() {
            let n = n.split('/').map(any_case).collect::<Vec<_>>().join("/");
            assert_eq!(regexes.iter().filter(|r| r.split('|').any(|a| a == n)).count(), 1, "{n}");
        }
        assert!(text.contains("[gG][rR][aA][dD][lL][eE]/[wW][rR][aA][pP][pP][eE][rR]"), "{text}");
        for literal in text.split("#\"").skip(1).map(|s| s.split('"').next().unwrap_or("")) {
            assert!(literal.len() < 600, "a regex of {} characters: {literal}", literal.len());
        }
        // Every name is plain enough for a regex in quotes (`any_case` escapes the rest), its parts too
        for n in protected_names() {
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)), "{n}");
            assert!(n.split('/').all(|p| !p.is_empty() && p != "." && p != ".."), "{n}");
        }
    }

    /// The protected names (card model P1, now the packs'): every pack's `protect` with the core's — today's
    /// names (what `llm.review-ask` held, `.tarae.local.toml` and `.git`) and the Java pack's, nothing more or
    /// less; the core holds none of an ecosystem's.
    #[test]
    fn the_protected_names_are_the_packs_and_the_cores() {
        let mut names: Vec<String> = protected_names().map(str::to_string).collect();
        let today = [
            "build.rs",
            "Cargo.toml",
            "package.json",
            "Makefile",
            "GNUmakefile",
            "justfile",
            ".justfile",
            ".envrc",
            ".tarae.toml",
            ".cargo",
            ".github",
            ".claude",
            ".vscode",
            ".tarae.local.toml",
            ".git",
        ];
        let java = [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "gradle.properties",
            "gradlew",
            "gradle/wrapper",
            "buildSrc",
            "pom.xml",
            "mvnw",
            ".mvn",
        ];
        let mut want: Vec<String> = today.iter().chain(&java).map(|s| s.to_string()).collect();
        names.sort();
        want.sort();
        assert_eq!(names, want);
        for ecosystem in
            ["build.rs", "Cargo.toml", ".cargo", "package.json", "Makefile", "justfile", "pom.xml"]
        {
            assert!(!CORE_NAMES.contains(&ecosystem), "{ecosystem}: the core's");
        }
    }

    /// A pack's names are kept whatever the project's languages: a Java build file in a Rust project, a
    /// Rust one in a Java project, an npm one in either — the files that mark the project don't matter.
    /// (Stand-in projects in the test's own temp folder.)
    #[test]
    fn a_packs_names_are_kept_in_a_project_of_another_language() {
        let t = profile_path(&std::env::temp_dir()).join(format!("tarae-sb-langs-{}", std::process::id()));
        let (rust, java) = (t.join("rust"), t.join("java"));
        for (dir, marker) in [(&rust, "Cargo.toml"), (&java, "settings.gradle.kts")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(marker), "").unwrap();
        }
        for (root, other) in
            [(&rust, "pom.xml"), (&rust, "build.gradle"), (&java, "build.rs"), (&java, "package.json")]
        {
            assert!(protected_in(root, &root.join(other)), "{other} in {}", root.display());
            assert!(crate::edits::listed(Path::new(other), &[]), "{other}: an Edit wouldn't ask");
            let profile = Policy::for_project(&Env { root: root.clone(), ..Env::default() }).profile();
            let first = other.split('/').next().map(any_case).unwrap();
            assert!(profile.contains(&first), "{other}: not in {}'s profile", root.display());
        }
        std::fs::remove_dir_all(&t).ok();
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
        // Every protected name said, the packs' and the core's
        assert!(protected_names().all(|n| r.contains(&format!(" {n},"))), "{r}");
        assert!(
            r.contains(".tarae.local.toml, .git, anywhere in it (Edit changes those)")
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
        // The kernel's report of the denial didn't come: cargo's own error says it — the path two lines above
        // the error — and that's what's said, as the output's words: tarae has no record of it
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
            r.starts_with(
                "The sandbox blocked nothing tarae saw. Its output says one thing was refused where the \
                 sandbox's rules keep a command out — tarae has no record of it, so that's the output's words, \
                 not a refusal seen: "
            ) && r.contains("\n  writing /Users/u/.cargo/registry/src/i/serde-1.0.0 (file-write*, said in the output)\n")
                && !r.contains("The sandbox blocked one thing")
                && r.contains("Cargo's caches (/Users/u/.cargo/registry, /Users/u/.cargo/git) stay as they are")
                && !r.contains("If the command needs one of these"),
            "{r}"
        );
        // …only when the output names it on a line that failed, and says a permission was refused
        assert!(unreported.report(&cargo_said[..2]).starts_with("The sandbox blocked nothing."));
        let listed = vec![(false, "/Users/u/.cargo/registry/src/i/a.rs: fn x".into()), cargo_said[3].clone()];
        assert!(unreported.report(&listed).starts_with("The sandbox blocked nothing."), "a grep's hit");
        let quoted = [(true, "/Users/u/p/src/a.rs:3: // Operation not permitted".to_string())];
        assert!(unreported.report(&quoted).starts_with("The sandbox blocked nothing."), "a comment's `//`");
        // Anywhere the profile keeps it from: a place in home not read by default, in Go's and Node's words
        // too — and that's not all kept on purpose: the user can open it
        for said in [
            "failed: /Users/u/other/x\nOperation not permitted",
            "open /Users/u/other/x: operation not permitted",
            "Error: EPERM: operation not permitted, open '/Users/u/other/x'",
        ] {
            let out: Vec<(bool, String)> = said.lines().map(|l| (true, l.to_string())).collect();
            let r = unreported.report(&out);
            assert!(
                r.contains(
                    "  reading or writing /Users/u/other/x (file-read* file-write*, said in the output)\n"
                ) && r.contains("If the command needs one of these"),
                "{said}: {r}"
            );
        }
        // A key: kept from every command, said so
        let r = unreported.report(&[(true, "cat: /Users/u/.ssh/id: Operation not permitted".into())]);
        assert!(r.contains("/Users/u/.ssh/id: tarae keeps these from every command of yours"), "{r}");
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

    /// P2 leftovers: Go's telemetry counters (written in home by every go command) are denied but never named —
    /// a Go run's report says nothing was blocked; any other write next to them is named as ever. A shell's
    /// setup (`~/.zshenv`, read by every `zsh -c`) is said calmly, apart — not a warning. And npm keeps its
    /// debug log in the run's own folder (its cleanup lists only that one), not `~/.npm/_logs`.
    #[test]
    fn expected_denials_stay_quiet_and_npm_logs_in_the_run() {
        let home = std::env::var("HOME").unwrap();
        // (this system's own form: the process's home is what `quiet` reads)
        let config = match Os::HOST {
            Os::Mac => format!("{home}/Library/Application Support"),
            Os::Linux => std::env::var("XDG_CONFIG_HOME")
                .ok()
                .filter(|x| x.starts_with('/'))
                .unwrap_or(format!("{home}/.config")),
        };
        let count =
            format!("{config}/go/telemetry/local/go@go1.27.1-go1.27.1-darwin-arm64-2026-10-05.v1.count");
        let p = Policy { tag: Some("t.".into()), ..Policy::for_project(&env(Path::new("/Users/u/p"))) };
        let w = Watch::quiet("t.");
        w.record(Denial { process: "go".into(), op: "file-write-data".into(), target: count.clone() });
        w.record(Denial { process: "go".into(), op: "file-write-create".into(), target: count });
        let b = w.blocked();
        assert_eq!((b.total, b.distinct, b.quiet.len()), (0, 0, 2));
        let g = Guard::In(Box::new(Policy { watch: Some(w.clone()), ..p.clone() }));
        let r = g.report(&[]);
        assert!(r.starts_with("The sandbox blocked nothing. In tarae's sandbox"), "{r}");
        assert_eq!(panel_line(Some(&g), true, std::time::Instant::now()), None, "nothing on the panel");
        assert_eq!(crate::cards::denied_line("s", "go vet", &w.blocked()), None);
        // (a read there isn't quieted: only Go's own writes)
        assert!(!quiet(&denial("file-read-data", &format!("{home}/Library/Application Support/go/env"))));
        // A shell's setup in home, closed on purpose: said calmly, not as blocked — no "tell the user"
        let zsh = Watch::quiet("t.");
        zsh.record(Denial {
            process: "zsh".into(),
            op: "file-read-data".into(),
            target: format!("{home}/.zshenv"),
        });
        let g = Guard::In(Box::new(Policy { watch: Some(zsh.clone()), ..p.clone() }));
        let r = g.report(&[]);
        assert!(
            r.starts_with(
                "The sandbox blocked nothing. ~/.zshenv: the shell's own setup isn't read in the sandbox"
            ) && !r.contains("don't work around it"),
            "{r}"
        );
        assert_eq!(panel_line(Some(&g), true, std::time::Instant::now()), None, "no ▲ for it");
        // (writing it, or one deeper in home: named as ever)
        assert!(!quiet(&denial("file-write-data", &format!("{home}/.zshenv"))));
        assert!(!quiet(&denial("file-read-data", &format!("{home}/proj/.zshenv"))));
        // Beside something blocked: that one named and counted, the shell's line calm after it
        zsh.record(denial("file-write-create", &format!("{home}/marker")));
        let r = g.report(&[]);
        assert!(r.starts_with("The sandbox blocked one thing — 1 time:\n  writing ~/marker"), "{r}");
        assert!(r.contains("~/.zshenv: the shell's own setup") && !r.contains("reading ~/.zshenv"), "{r}");
        let g = Guard::In(Box::new(Policy { watch: Some(w.clone()), ..p.clone() }));
        w.record(denial("file-write-create", &format!("{home}/marker")));
        let r = g.report(&[]);
        assert!(r.starts_with("The sandbox blocked one thing — 1 time:\n  writing ~/marker"), "{r}");
        // npm's log folder: the run's own (the packs' `env`, `{run}` filled in)
        let run = RunDir::new().unwrap();
        let vars = vars_in(Path::new(&home), &Here::default());
        let env =
            packs_env(&crate::sandboxpack::Vars { run: Some(run.path().to_path_buf()), ..vars.clone() });
        assert!(packs_env(&vars).iter().all(|(k, _)| k != "npm_config_logs_dir"), "no run: no log folder");
        let p = Policy { run: Some(run.clone()), env, ..p };
        let c = command(Some(&p), "npm", &["test".into()]);
        let logs = c.get_envs().find(|(k, _)| *k == "npm_config_logs_dir").and_then(|(_, v)| v);
        assert_eq!(logs, Some(run.path().as_os_str()));
        let unsandboxed = command(None, "npm", &["test".into()]);
        assert!(
            unsandboxed.get_envs().all(|(k, _)| k != "npm_config_logs_dir"),
            "the user's own runs as they are"
        );
    }

    /// A quiet write is quiet only at its place for the home on this system — macOS's `~/Library/Application
    /// Support/go/telemetry`, Linux's where `XDG_CONFIG_HOME` puts it (else `~/.config`): the same tail in the
    /// project, elsewhere in home or in the other system's form is a denial like any, named in the report.
    #[test]
    fn a_quiet_write_is_quiet_only_at_its_place() {
        let home = Path::new("/Users/u");
        let tail = "go/telemetry/local/x.count";
        let places = |os: Os, env: &[(&str, &str)]| {
            let env = env.iter().map(|(k, v)| (k.to_string(), v.into())).collect();
            quiet_places(&vars_in(home, &Here { env, os, ..Here::default() }))
        };
        let quiet_at = |places: &[PathBuf], rel: &str| {
            quiet_in(&denial("file-write-create", &home.join(rel).display().to_string()), Some(home), places)
        };
        let mac = places(Os::Mac, &[("XDG_CONFIG_HOME", "/Users/u/conf")]);
        assert!(quiet_at(&mac, &format!("Library/Application Support/{tail}")));
        assert!(quiet_at(&mac, "Library/Application Support/go/telemetry"), "the folder itself");
        for rel in [
            format!("work/p/Library/Application Support/{tail}"),
            format!("work/p/.config/{tail}"),
            format!("work/p/{tail}"),
            format!(".config/{tail}"),
            format!("conf/{tail}"),
            format!("Library/Application Support/go/telemetry-x/{tail}"),
        ] {
            assert!(!quiet_at(&mac, &rel), "macOS: {rel}");
        }
        // (a read there is no quiet write)
        let read = denial(
            "file-read-data",
            &home.join(format!("Library/Application Support/{tail}")).display().to_string(),
        );
        assert!(!quiet_in(&read, Some(home), &mac));
        let linux = places(Os::Linux, &[]);
        assert!(quiet_at(&linux, &format!(".config/{tail}")));
        for rel in [format!("Library/Application Support/{tail}"), format!("work/p/.config/{tail}")] {
            assert!(!quiet_at(&linux, &rel), "Linux: {rel}");
        }
        let xdg = places(Os::Linux, &[("XDG_CONFIG_HOME", "/Users/u/conf")]);
        assert!(quiet_at(&xdg, &format!("conf/{tail}")), "where XDG_CONFIG_HOME puts it");
        assert!(!quiet_at(&xdg, &format!(".config/{tail}")), "not the default once it's moved");
        // In a report: a write in the project with the same tail as this system's is named and counted
        let real = std::env::var("HOME").unwrap();
        let root = Path::new(&real).join("work/p");
        let w = Watch::quiet("t-tail.");
        for form in ["Library/Application Support", ".config"] {
            w.record(denial("file-write-create", &root.join(form).join(tail).display().to_string()));
        }
        let b = w.blocked();
        assert_eq!((b.total, b.distinct, b.quiet.len()), (2, 2, 0));
        let p = Policy { tag: Some("t-tail.".into()), ..Policy::for_project(&env(&root)) };
        let r = Guard::In(Box::new(Policy { watch: Some(w), ..p })).report(&[]);
        let at = crate::allow::tilde(&root.join(".config").join(tail));
        assert!(r.starts_with("The sandbox blocked 2 things — 2 times:\n") && r.contains(&at), "{r}");
    }

    /// Step 7 on the VMs: what was reported as blocked though nothing needed it. npm checks for its own update
    /// once a day, writing the date in `~/.npm` — a run is told not to (the user's own runs as they are); the
    /// JVM writes its own `/proc/<pid>/coredump_filter` and goes on; getaddrinfo `connect`s a UDP socket to
    /// port 0 of each address to sort them (nothing is sent). Those last two quiet — a write or a connect
    /// beside them still named.
    #[test]
    fn what_runs_try_harmlessly_stays_quiet() {
        let p = Policy { run: RunDir::new().ok(), ..Policy::for_project(&env(Path::new("/Users/u/p"))) };
        let notifier = |c: &Command| {
            c.get_envs()
                .find(|(k, _)| *k == "npm_config_update_notifier")
                .and_then(|(_, v)| v)
                .map(|v| v.to_owned())
        };
        assert_eq!(notifier(&command(Some(&p), "npm", &["ci".into()])), Some("false".into()));
        assert_eq!(notifier(&command(None, "npm", &["ci".into()])), None, "the user's own runs as they are");
        let out = |target: &str| Denial {
            process: "getent".into(),
            op: "network-outbound".into(),
            target: target.into(),
        };
        assert!(quiet(&denial("file-write*", "/proc/198585/coredump_filter")));
        assert!(quiet(&out("remote:127.0.0.1:0")) && quiet(&out("remote:[::1]:0")));
        assert!(!quiet(&denial("file-write*", "/proc/198585/environ")));
        assert!(!quiet(&denial("file-write*", "/home/u/proc/1/coredump_filter")));
        assert!(!quiet(&out("remote:127.0.0.1:10")) && !quiet(&out("remote:169.254.169.254:53")));
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
        // (a path the profile lets it write: not the sandbox's for sure — the line is shown as it is)
        let out =
            [(true, "touch: /Users/u/p/x: Operation not permitted".to_string()), (false, "fine".to_string())];
        let g = Guard::In(Box::new(Policy { watch: Some(failed), ..p }));
        let r = g.report(&out);
        assert!(
            r.starts_with("tarae couldn't watch what the sandbox blocked (log: Cannot run while sandboxed)"),
            "{r}"
        );
        assert!(
            r.contains("likely the sandbox")
                && r.contains("  touch: /Users/u/p/x: Operation not permitted")
                && !r.contains("fine"),
            "{r}"
        );
        // One the profile keeps from it: said as the output's, not as blocked
        let r = g.report(&[(true, "touch: /x: Operation not permitted".to_string())]);
        assert!(
            r.starts_with("tarae couldn't watch what the sandbox blocked (log: Cannot run while sandboxed). Its output says one thing was refused")
                && r.contains("\n  writing /x (file-write*, said in the output)\n")
                && !r.contains("The sandbox blocked one thing"),
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
        // (a socket in home is still a socket — once said as Seatbelt's `network-outbound ~/…`)
        assert_eq!(plain("network-outbound", &format!("{home}/s.sock")), "connecting to socket ~/s.sock");
        assert_eq!(plain("network-bind", &format!("{home}/s.sock")), "making socket ~/s.sock");
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
        // Linux's supervisor names the address: this machine's by port, another as it is
        assert_eq!(plain("network-outbound", "remote:127.0.0.1:9"), "connecting to port 9");
        assert_eq!(plain("network-outbound", "remote:[::ffff:127.0.0.1]:9"), "connecting to port 9");
        assert_eq!(plain("network-outbound", "remote:192.0.2.1:9"), "connecting to 192.0.2.1:9");
        assert_eq!(plain("network-outbound", "remote:[2001:db8::1]:9"), "connecting to [2001:db8::1]:9");
        assert_eq!(plain("network-outbound", "@bus"), "network-outbound @bus");
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
            caches_said: Vec::new(),
            network: false,
            ports: vec![port],
            localhost: false,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
            named_by: String::new(),
            refused_asked: Vec::new(),
            reads: Reads::default(),
            env: Vec::new(),
            run_env: Default::default(),
            run_homes: Vec::new(),
            kept: Vec::new(),
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
                caches_said: Vec::new(),
                network,
                ports: Vec::new(),
                localhost: false,
                run: Some(run.clone()),
                tag: None,
                watch: None,
                granted: Vec::new(),
                sockets: Vec::new(),
                refused: Vec::new(),
                named_by: String::new(),
                refused_asked: Vec::new(),
                reads: Reads::default(),
                env: Vec::new(),
                run_env: Default::default(),
                run_homes: Vec::new(),
                kept: Vec::new(),
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
            caches_said: Vec::new(),
            network: false,
            ports: Vec::new(),
            localhost: false,
            run: None,
            tag: Some(tag.to_string()),
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
            named_by: String::new(),
            refused_asked: Vec::new(),
            reads: Reads::default(),
            env: Vec::new(),
            run_env: Default::default(),
            run_homes: Vec::new(),
            kept: Vec::new(),
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
            caches_said: Vec::new(),
            network: false,
            ports: Vec::new(),
            localhost: false,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
            named_by: String::new(),
            refused_asked: Vec::new(),
            reads: Reads::default(),
            env: Vec::new(),
            run_env: Default::default(),
            run_homes: Vec::new(),
            kept: Vec::new(),
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
    /// their sibling isn't; a place in home kept for the project is read, the one beside it isn't (card
    /// model P2); a granted port is reached, another isn't. The policy is made as a run's is
    /// (`tools::with_allowed` → `llm.sandbox-write`/`-ports`, `reads_for` → `Policy::for_project`), but
    /// without the usual temp folders — the test's own folders are in one. Harmless: markers in its temp
    /// folder (a stand-in home there), listeners of its own on 127.0.0.1 (only `nc -z` knocks).
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
        let (home, readable, unread) = (t.join("home"), t.join("home/readable"), t.join("home/unread"));
        for d in [&root, &kept, &chat, &sibling, &readable, &unread] {
            std::fs::create_dir_all(d).unwrap();
        }
        for d in [&readable, &unread] {
            std::fs::write(d.join("marker"), "marker\n").unwrap();
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
            a.add(&Grant::Read(readable.clone()));
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
        let w =
            crate::tools::with_allowed(&crate::llm::LlmConfig::default(), &[&from_store, &this_chat], None);
        assert_eq!(w.granted.len(), 5, "{:?}", w.granted);
        let env = Env {
            root: root.clone(),
            write: w.llm.sandbox_write.clone(),
            ports: w.llm.sandbox_ports,
            reads: reads_for(&home, &w.reads, &vars_in(&home, &Here::now())),
            ..Env::default()
        };
        let p = Policy { sockets: w.sockets, ..Policy::for_project(&env) };
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
        let r = sh(&format!("cat '{}'", readable.join("marker").display()));
        assert_eq!(String::from_utf8_lossy(&r.stdout), "marker\n", "the place granted for reading: {r:?}");
        let r = sh(&format!("cat '{}'", unread.join("marker").display()));
        assert!(!r.status.success() && r.stdout.is_empty(), "the place beside it was read: {r:?}");
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
            std::fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
            std::fs::write(root.join(name), "kept\n").unwrap();
            let upper = name.to_uppercase();
            // (a name of several parts, `gradle/wrapper`: the folders it sits in made first, so every try is one)
            let parents: Vec<String> = [
                format!("a/{name}"),
                format!("b/{name}"),
                format!("c/{upper}"),
                format!("moved{name}"),
                format!("{o}/evil{name}"),
                format!("{o}/second{name}"),
                format!("target/x/{name}"),
                format!("node_modules/p/{name}"),
            ]
            .iter()
            .filter_map(|p| Path::new(p).parent().filter(|d| !d.as_os_str().is_empty()))
            .map(|d| format!("'{}'", d.display()))
            .collect();
            let parents = parents.join(" ");
            let ran = sh(
                &p,
                &format!(
                    "mkdir -p {parents}; echo x > 'a/{name}'; mkdir 'b/{name}'; \
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
            assert!(crate::edits::listed(Path::new(name), &[]), "{name}: an Edit wouldn't ask");
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

    /// For real, on macOS (card model P2) — reads under the real profile: in a stand-in home only the project,
    /// the allowed places and tarae's log are read; a place beside them isn't (nor listed — though a file's
    /// metadata stays open), nor a kept place, though an allowed — even writable — folder holds it; outside
    /// home everything is. A checkout inside a kept place reads its own files and nothing else there. Nothing
    /// unreadable can be hard-linked into the project — a key in a writable folder, a file in home not read —
    /// nor what it reads but can't write; the project's own files link as before. The clipboard isn't
    /// reached. Harmless: plain-text stand-ins in a folder of the test's own under `target/`; `pbpaste` goes to
    /// /dev/null.
    #[cfg(target_os = "macos")]
    #[test]
    fn reads_stay_in_their_places_under_the_real_profile() {
        if skipped_inside("sandbox::tests::reads_stay_in_their_places_under_the_real_profile") {
            return;
        }
        let t = scratch("sb-reads");
        let home = t.join("home");
        let (root, claudes) = (home.join("work/proj"), home.join(".claude/worktrees/p"));
        for d in [&root, &claudes, &t.join("outside")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let (read, unread) = (
            [root.join("own"), home.join("allowed/m"), home.join("state/tarae.log"), t.join("outside/m")],
            [
                home.join("other/m"),
                home.join("dir/.ssh/key"),
                home.join("state/chat.json"),
                home.join(".claude/settings.json"),
            ],
        );
        for f in read.iter().chain(&unread).chain([&claudes.join("own")]) {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "marker\n").unwrap();
        }
        let reads = Reads {
            home: Some(home.clone()),
            allow: vec![home.join("allowed"), home.join("dir")],
            kept: vec![home.join("dir/.ssh"), home.join("state"), home.join(".claude")],
            log: Some(home.join("state/tarae.log")),
            copies: Default::default(),
        };
        // (`dir` writable too: then only the read rules keep its key from a link)
        let policy = |root: &Path| Policy {
            reads: reads.clone(),
            write: vec![home.join("dir")],
            ..Policy::for_project(&Env { root: root.to_path_buf(), ..Env::default() })
        };
        let p = policy(&root);
        let sh = |p: &Policy, script: &str| {
            let mut c = command(Some(p), "/bin/sh", &["-c".into(), script.into()]);
            let out = c.current_dir(&p.root).stdin(Stdio::null()).output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        // Hard links: nothing unreadable comes in, nor what it can read but not write
        let o = |f: &str| home.join(f).display().to_string();
        sh(
            &p,
            &format!(
                "ln '{}' a; ln '{}' b; ln '{}' c; echo x > own2; ln own2 own3",
                o("other/m"),
                o("dir/.ssh/key"),
                o("allowed/m")
            ),
        );
        for (name, what) in [
            ("a", "a file in home not read"),
            ("b", "a key in a writable folder"),
            ("c", "a file read, not written"),
        ] {
            assert!(!root.join(name).exists(), "{what}: linked into the project");
        }
        assert!(root.join("own3").exists(), "the project's own file: not linked");
        let cat = |p: &Policy, f: &Path| sh(p, &format!("cat '{}' 2>/dev/null", f.display())) == "marker\n";
        for f in &read {
            assert!(cat(&p, f), "{}: not read", f.display());
        }
        for f in &unread {
            assert!(!cat(&p, f), "{}: read", f.display());
        }
        for (dir, file) in [("other", "other/m"), ("dir/.ssh", "dir/.ssh/key")] {
            let (dir, file) = (home.join(dir).display().to_string(), home.join(file).display().to_string());
            let listed = sh(&p, &format!("ls '{dir}' 2>/dev/null; test -e '{file}' && echo there"));
            assert_eq!(listed, "there\n", "{dir} listed, or its file's metadata closed");
        }
        // A checkout in a kept place
        let inside = policy(&claudes);
        assert!(cat(&inside, &claudes.join("own")) && !cat(&inside, &home.join(".claude/settings.json")));
        assert_eq!(sh(&p, "/usr/bin/pbpaste > /dev/null 2>&1; echo $?"), "1\n", "the clipboard reached");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS (card model P2): git's credential store — plain-text passwords in
    /// `~/.config/git/credentials`, inside `~/.config/git`, which runs read, and `~/.git-credentials` — isn't
    /// read, while git's ignore file beside it is; nor offered for reading, nor kept for good. The reads are a
    /// run's (`reads_for`) in a stand-in home. Harmless: plain-text markers under `target/`.
    #[cfg(target_os = "macos")]
    #[test]
    fn gits_stored_passwords_stay_unread_under_the_real_profile() {
        if skipped_inside("sandbox::tests::gits_stored_passwords_stay_unread_under_the_real_profile") {
            return;
        }
        let t = scratch("sb-gitcreds");
        let home = t.join("home");
        let root = home.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        let (ignore, stored, older) = (
            home.join(".config/git/ignore"),
            home.join(".config/git/credentials"),
            home.join(".git-credentials"),
        );
        for f in [&ignore, &stored, &older] {
            std::fs::write(f, "marker\n").unwrap();
        }
        let env = Env {
            root: root.clone(),
            reads: reads_for(&home, &[], &vars_in(&home, &Here::now())),
            ..Env::default()
        };
        let p = Policy::for_project(&env);
        let cat = |f: &Path| {
            let mut c =
                command(Some(&p), "/bin/sh", &["-c".into(), format!("cat '{}' 2>/dev/null", f.display())]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap().stdout == b"marker\n"
        };
        assert!(cat(&ignore), "git's ignore file: not read");
        for f in [&stored, &older] {
            assert!(!cat(f), "{}: read", f.display());
            assert!(
                crate::allow::read_choices(f, &home, &p.reads.kept).is_empty(),
                "{}: offered",
                f.display()
            );
            assert!(!crate::allow::keepable(f, Some(&home)), "{}: kept for good", f.display());
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS: settings that hold a token (`tokens.rs`), fake ones in a stand-in home — git's two
    /// files (an include in one), Go's env file, npm's and pip's, cargo's. The tools work from their copies:
    /// git's alias, its include's and its other file's settings; Go's flags, npm's registry, pip's timeout
    /// (each when installed). And nothing a run can read holds a token — not the files, not the copies — but
    /// cargo's config, left readable on purpose (cargo fails without it). Harmless: fake values in plain files
    /// under the test's own folder under `target/`; no network (only settings are read).
    #[cfg(target_os = "macos")]
    #[test]
    fn tokens_stay_out_of_runs_under_the_real_profile() {
        if skipped_inside("sandbox::tests::tokens_stay_out_of_runs_under_the_real_profile") {
            return;
        }
        const FAKE: &str = "tarae-fake-token";
        let t = scratch("sb-tokens");
        let (home, root) = (t.join("home"), t.join("home/proj"));
        let files = [
            (
                ".gitconfig",
                "[alias]\n\tco = checkout\n[include]\n\tpath = .config/git/extra.inc\n[http]\n\textraHeader = \
                 Authorization: Bearer tarae-fake-token\n[url \"https://x:tarae-fake-token@github.com/\"]\n\
                 \tinsteadOf = https://github.com/\n",
            ),
            (".config/git/extra.inc", "[alias]\n\tst = status\n"),
            (".config/git/config", "[core]\n\tabbrev = 9\n"),
            (
                "Library/Application Support/go/env",
                "GOFLAGS=-mod=mod\nGOPROXY=https://u:tarae-fake-token@proxy.example.com,direct\n",
            ),
            (".npmrc", "registry=https://r.example.com/\n//r.example.com/:_authToken=tarae-fake-token\n"),
            (
                ".config/pip/pip.conf",
                "[global]\nindex-url = https://u:tarae-fake-token@p.example.com/s\ntimeout = 7\n",
            ),
            (".cargo/config.toml", "[registry]\ntoken = \"tarae-fake-token\"\n"),
            ("proj/README", "marker\n"),
        ];
        for (file, text) in files {
            std::fs::create_dir_all(home.join(file).parent().unwrap()).unwrap();
            std::fs::write(home.join(file), text).unwrap();
        }
        let run = RunDir::new().unwrap();
        let mut reads = reads_for(&home, &[], &vars_in(&home, &Here::now()));
        let sources = crate::tokens::sources(&home, &|_| None);
        reads.copies = crate::tokens::copies(&sources, Some(run.path()), &home, &[], &reads.kept);
        let tmp = std::fs::canonicalize(std::env::temp_dir()).ok();
        let run_env = run_env_here();
        let env = Env { root: root.clone(), home: Some(home.clone()), tmp, reads, run_env, ..Env::default() };
        let p = Policy { run: Some(run), ..Policy::for_project(&env) };
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            for v in [
                "XDG_CONFIG_HOME",
                "GOFLAGS",
                "GOPROXY",
                "npm_config_registry",
                "PIP_TIMEOUT",
                "PIP_INDEX_URL",
            ] {
                c.env_remove(v);
            }
            let out = c
                .current_dir(&root)
                .env("HOME", &home)
                .env("GOCACHE", root.join("gocache"))
                .env("GOTOOLCHAIN", "local")
                .env("npm_config_cache", root.join("npmcache"))
                .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
                .env("GIT_CEILING_DIRECTORIES", &t)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let said =
                format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(!said.contains(FAKE), "a token reached the run: {script}: {said}");
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let git = "git config --get alias.co && git config --get alias.st && git config --get core.abbrev && \
                   ! git config --get-regexp '^(http|url)\\.'";
        assert_eq!(sh(git), (true, "checkout\nstatus\n9\n".into()), "git from its copy");
        // Nothing it can read holds one — cargo's config aside: each file by name (home's folders aren't
        // listed), and everything in the run's own folder, the copies there
        let named: Vec<String> = files.iter().map(|(f, _)| format!("'{}'", home.join(f).display())).collect();
        let script = format!("grep -l {FAKE} {} 2>/dev/null; grep -rl {FAKE} \"$TMPDIR\"", named.join(" "));
        let found = sh(&script).1;
        assert_eq!(found, format!("{}\n", home.join(".cargo/config.toml").display()), "read: {found}");
        assert!(!sh("cat \"$HOME/.gitconfig\" 2>/dev/null").0, "the file itself read");
        let installed = |tool: &str| {
            Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if installed("go") {
            let (ok, out) = sh("go env GOFLAGS GOPROXY");
            assert!(ok && out.starts_with("-mod=mod\n"), "go: {out}");
        }
        if installed("npm") {
            assert_eq!(sh("npm config get registry"), (true, "https://r.example.com/\n".into()), "npm");
        }
        if installed("pip3") {
            let (ok, out) = sh("pip3 config list");
            assert!(ok && out.contains("global.timeout='7'"), "pip: {out}");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS (Java under P2): Java's places in a stand-in home as a run gets them. Read: the JDKs,
    /// Maven's repository, Gradle's downloaded dependencies and wrapper distributions — through the run's own
    /// Gradle home too, which it writes. Written in the user's Gradle home: the wrapper's lock file, nothing
    /// else. Unread: the Gradle daemons' registry (measured: a sandboxed build that read it ran in a daemon
    /// started outside) and both tools' settings (repository passwords). Harmless: markers under `target/`.
    #[cfg(target_os = "macos")]
    #[test]
    fn javas_places_under_the_real_profile() {
        if skipped_inside("sandbox::tests::javas_places_under_the_real_profile") {
            return;
        }
        // (the stand-in Gradle home is where `GRADLE_USER_HOME` would point instead: not one to write markers in)
        if std::env::var_os("GRADLE_USER_HOME").is_some() {
            eprintln!("javas_places_under_the_real_profile: GRADLE_USER_HOME is set — skipped");
            return;
        }
        let t = scratch("sb-java");
        let (home, run_dir) = (t.join("home"), RunDir::new().unwrap());
        let (root, g) = (home.join("proj"), home.join(".gradle"));
        let dist = g.join("wrapper/dists/gradle-9.7.1-bin/h1");
        let read = [
            home.join("Library/Java/JavaVirtualMachines/jdk/release"),
            home.join(".m2/repository/junit/junit.jar"),
            g.join("caches/modules-2/files-2.1/junit/junit.jar"),
            dist.join("gradle-9.7.1-bin/lib/gradle.jar"),
        ];
        let unread =
            [g.join("daemon/9.7.1/registry.bin"), g.join("gradle.properties"), home.join(".m2/settings.xml")];
        for f in read.iter().chain(&unread) {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "marker\n").unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        let env = Env {
            root: root.clone(),
            reads: reads_for(&home, &[], &vars_in(&home, &Here::now())),
            run_homes: vec![gradle_at(&g)],
            files: locks_in(&g),
            ..Env::default()
        };
        let vars = made_with(run_dir.path(), &g, None);
        let p = Policy { run: Some(run_dir.clone()), env: vars, ..Policy::for_project(&env) };
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap().status.success()
        };
        let cat = |f: &Path| sh(&format!("[ \"$(cat '{}' 2>/dev/null)\" = marker ]", f.display()));
        for f in &read {
            assert!(cat(f), "{}: not read", f.display());
        }
        for f in &unread {
            assert!(!cat(f), "{}: read", f.display());
        }
        let jar = "wrapper/dists/gradle-9.7.1-bin/h1/gradle-9.7.1-bin/lib/gradle.jar";
        assert!(
            sh(&format!("[ \"$(cat \"$GRADLE_USER_HOME/{jar}\")\" = marker ]")),
            "the distribution, linked"
        );
        assert!(sh("mkdir -p \"$GRADLE_USER_HOME/native\" && touch \"$GRADLE_USER_HOME/caches.lock\""));
        let touch = |f: &Path| sh(&format!("touch '{}' 2>/dev/null", f.display()));
        assert!(touch(&dist.join("gradle-9.7.1-bin.zip.lck")), "the wrapper's lock");
        for f in [dist.join("gradle-9.7.1-bin.zip.ok"), g.join("caches/modules-2/x"), g.join("init.gradle")] {
            assert!(!touch(&f) && !f.exists(), "{}: written", f.display());
        }
        // In the project: the Java pack's names can't be made or changed (they run in the user's builds and
        // when jdtls imports the project); the code beside them is written as usual
        std::fs::write(root.join("build.gradle"), "kept\n").unwrap();
        for f in [
            "settings.gradle.kts",
            "app/pom.xml",
            "gradle/wrapper/gradle-wrapper.properties",
            ".mvn/jvm.config",
        ] {
            let dir = Path::new(f).parent().filter(|d| !d.as_os_str().is_empty());
            let mk = dir.map_or(String::new(), |d| format!("mkdir -p '{}' 2>/dev/null; ", d.display()));
            assert!(
                !sh(&format!("{mk}echo evil > '{f}' 2>/dev/null")) && !root.join(f).exists(),
                "{f}: made"
            );
        }
        assert!(!sh("echo evil >> build.gradle 2>/dev/null"), "build.gradle: appended to");
        assert_eq!(std::fs::read_to_string(root.join("build.gradle")).unwrap(), "kept\n");
        assert!(
            sh("mkdir -p src/main/java && echo 'class A {}' > src/main/java/A.java"),
            "the code: not written"
        );
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS (card model P2): the toolchains work with reads confined in the real home as a run
    /// gets it (`reads_for` — the measured list — and cargo's configs above the project: this checkout has
    /// one, and the test's crate is below it): git, rustc and cargo (a tiny crate built offline and run, its
    /// target and build dirs in the test's folder), go and node when they're installed — and none of them is
    /// denied a read (as far as the kernel's reports come). Harmless: they read their own settings;
    /// what they write is under the test's own folder under `target/`.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_toolchains_work_with_reads_confined() {
        if skipped_inside("sandbox::tests::the_toolchains_work_with_reads_confined") {
            return;
        }
        let t = scratch("sb-tools");
        let root = t.join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("go")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() { println!(\"built\"); }\n").unwrap();
        std::fs::write(root.join("go/go.mod"), "module p\n\ngo 1.21\n").unwrap();
        std::fs::write(
            root.join("go/main.go"),
            "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"built\") }\n",
        )
        .unwrap();
        let home = std::fs::canonicalize(std::env::var_os("HOME").unwrap()).unwrap();
        let vars = vars_in(&home, &Here::now());
        let (caches, files) = caches_and_files(&vars);
        let tag = format!("tarae-test-{}-tools.", std::process::id());
        let w = Watch::start(&tag);
        std::thread::sleep(WATCH_READY);
        let env = Env {
            root: root.clone(),
            home: Some(home.clone()),
            tmp: std::fs::canonicalize(std::env::temp_dir()).ok(),
            caches,
            files,
            reads: reads_for(&home, &reads_above(&root, &home), &vars),
            run_env: run_env_here(),
            ..Env::default()
        };
        let p = Policy { tag: Some(tag), watch: Some(w.clone()), ..Policy::for_project(&env) };
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            let out = c
                .current_dir(&root)
                .env("CARGO_BUILD_BUILD_DIR", root.join("build"))
                .env("CARGO_TARGET_DIR", root.join("target"))
                .env("GOCACHE", root.join("gocache"))
                .env("GIT_CEILING_DIRECTORIES", &t)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            (
                out.status.success(),
                format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
            )
        };
        // (git reads its settings — not the checkout this test's folder is in)
        let (ok, said) = sh(
            "git config --list >/dev/null && rustc --version && cargo build --offline -q && target/debug/p",
        );
        assert!(ok && said.ends_with("built\n"), "{said}");
        let here =
            |tool: &str| Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success());
        if here("go") {
            let (ok, said) = sh("cd go && go build -o p . && ./p");
            assert!(ok && said.ends_with("built\n"), "go: {said}");
        }
        if here("node") && here("npm") {
            let (ok, said) = sh(
                "node -e 'console.log(require(\"os\").homedir().length > 1)' && npm --version >/dev/null && echo built",
            );
            assert!(ok && said.ends_with("true\nbuilt\n"), "node: {said}");
        }
        std::thread::sleep(WATCH_LAST * 2);
        w.finish();
        let reads: Vec<String> =
            w.blocked().seen.iter().filter(|d| d.op.starts_with("file-read")).map(Denial::shown).collect();
        assert!(reads.is_empty(), "denied: {reads:?}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// Not a check: a measuring aid (card model P2's confirm pass, `target/sb/p2-confirm.sh`) — prints the
    /// `sandbox-exec` arguments a run of Claude's gets in `$TARAE_PROFILE_ROOT` (else this checkout), with
    /// that project's settings (`$TARAE_PROFILE_LOCALHOST`: its `sandbox-localhost`, as a session that trusts
    /// it has), as a shell `set --` line (`PROFILE:`), and its tag (`TAG:`). Run outside a sandbox: the
    /// toolchain's folders are asked inside one.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "a measuring aid: cargo test -- --ignored --nocapture sandbox::tests::the_profile_a_run_gets"]
    fn the_profile_a_run_gets() {
        let root = std::env::var_os("TARAE_PROFILE_ROOT")
            .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
        // (as a real run's: your config and trust answers — run by hand, never in the suite)
        let mut llm = crate::config::load(&root, &crate::config::Sources::own()).0.editor.llm;
        llm.sandbox_localhost |= std::env::var_os("TARAE_PROFILE_LOCALHOST").is_some();
        let p = policy(&root, &llm, &[]);
        let quote = |a: &str| format!("'{}'", a.replace('\n', " ").replace('\'', "'\\''"));
        let args: Vec<String> = p.args().iter().map(|a| quote(a)).collect();
        println!("TAG: {}", p.tag.clone().unwrap_or_default());
        println!("PROFILE: set -- {}", args.join(" "));
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
    /// last-use files (the rust pack's `caches.files`) and trying nothing it's denied. Harmless and offline: cargo's home, a
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
        let (tmp, run_env) = (Some(profile_path(&std::env::temp_dir())), run_env_here());
        let env = Env { root: root.clone(), tmp, run_env, ..cargo_at(&home) };
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
        // (said as blocked, or as the output's words when the kernel's report didn't come — never "nothing")
        assert!(!report.contains("The sandbox blocked nothing."), "{report}");
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
            caches_said: Vec::new(),
            network,
            ports,
            localhost,
            run: None,
            tag: None,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused: Vec::new(),
            named_by: String::new(),
            refused_asked: Vec::new(),
            reads: Reads::default(),
            env: Vec::new(),
            run_env: Default::default(),
            run_homes: Vec::new(),
            kept: Vec::new(),
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

    /// Every variable the policy reads to place things (a toolchain's home, a setting's file, the XDG folders,
    /// PATH) reaches the run — a run that saw another `CARGO_HOME` than the policy would look where nothing was
    /// opened. What the policy reads is recorded as it asks (`Here::asked`), each set in turn until no new one
    /// is asked; the multiplexers' socket folders aside (the policy keeps them from runs — a run has no use
    /// for them).
    #[test]
    fn every_variable_the_policy_reads_reaches_the_run() {
        let t = std::env::temp_dir().join(format!("tarae-runenv-reads-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let (home, root) = (t.join("home"), t.join("home/work/p"));
        std::fs::create_dir_all(&root).unwrap();
        for f in ["Cargo.toml", "go.mod", "build.gradle", "package.json"] {
            std::fs::write(root.join(f), "").unwrap();
        }
        let (home, root) = (std::fs::canonicalize(&home).unwrap(), std::fs::canonicalize(&root).unwrap());
        let llm = crate::llm::LlmConfig::default();
        let mut env: HashMap<String, std::ffi::OsString> =
            HashMap::from([("HOME".into(), home.clone().into())]);
        let mut asked: Vec<String> = Vec::new();
        for _ in 0..4 {
            let here = Here { env: env.clone(), ..Here::default() };
            toolchain_found_in(&root, ToolDirs::default(), &here);
            let p = policy_in(&root, &llm, &[], &here);
            let now: Vec<String> = here.asked.lock().unwrap().clone();
            let names: Vec<&str> = p.run_env.vars.iter().map(|(k, ..)| k.as_str()).collect();
            for n in &now {
                if !["TMUX_TMPDIR", "ZELLIJ_SOCKET_DIR"].contains(&n.as_str()) && env.contains_key(n) {
                    assert!(names.contains(&n.as_str()), "{n}: the policy reads it, the run doesn't get it");
                }
            }
            let new: Vec<String> = now.into_iter().filter(|n| !asked.contains(n)).collect();
            if new.is_empty() {
                break;
            }
            for n in &new {
                env.entry(n.clone()).or_insert_with(|| t.join("stand-in").join(n).into());
            }
            asked.extend(new);
        }
        for n in ["XDG_CONFIG_HOME", "CARGO_HOME", "GRADLE_USER_HOME", "GIT_CONFIG_GLOBAL", "PIP_CONFIG_FILE"]
        {
            assert!(asked.iter().any(|a| a == n), "{n} isn't asked: {asked:?}");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    const FAKE: &str = "tarae-fake-token";

    /// What a run gets of this process's environment with the default settings — as a real policy's
    /// (`env_in`); a policy made by hand gets nothing of it. (Seatbelt's tests'; Linux's have their own.)
    #[cfg(target_os = "macos")]
    fn run_env_here() -> runenv::RunEnv {
        run_env_of(&Here::now(), &crate::llm::LlmConfig::default())
    }

    /// lsbenv end to end, the real path: tarae's environment holding fake tokens (one inside a pack's family:
    /// `CARGO_REGISTRY_TOKEN`), a pack's variable (`RUSTFLAGS`, the rust pack's `RUST*`) and one of nobody's —
    /// the policy made from it (`policy` → `Here::now`) — a sandboxed run's environment holds the pack's and the
    /// floor, not the fakes nor the other; the toolchain's probe (`output_in`) sees the same; the report names
    /// what was left out, never a value. In a process of its own, the variables in its environment
    /// (`env_in_its_own_process`): this one's isn't touched. Seatbelt can't start inside a sandbox; Linux needs
    /// Landlock (`cargo xtask linux`).
    #[test]
    fn a_fake_token_never_reaches_a_sandboxed_run() {
        #[cfg(target_os = "macos")]
        if skipped_inside("sandbox::tests::a_fake_token_never_reaches_a_sandboxed_run") {
            return;
        }
        #[cfg(target_os = "linux")]
        if !crate::landlock::abi().is_ok_and(|n| n >= 2) {
            let line = format!(
                "{}sandbox::tests::a_fake_token_never_reaches_a_sandboxed_run — needs Landlock ABI 2\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-runenv-e2e-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sandbox::tests::env_in_its_own_process", "--ignored", "--nocapture"])
            .env("TARAE_ENV_PROBE", &dir)
            .env("GITHUB_TOKEN", FAKE)
            .env("AWS_SECRET_ACCESS_KEY", FAKE)
            .env("CARGO_REGISTRY_TOKEN", FAKE)
            .env("RUSTFLAGS", "--cfg tarae_fake")
            .env("TARAE_LSBENV_OTHER", "x")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && said.contains("1 passed"), "the child ran it: {said}");
        let read =
            |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("{f}: {e}\n{said}"));
        let names = read("names");
        let names: Vec<&str> = names.lines().collect();
        for n in ["PATH", "HOME", "RUSTFLAGS", MARK] {
            assert!(names.contains(&n), "{n} didn't reach the run: {names:?}");
        }
        for n in ["GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "CARGO_REGISTRY_TOKEN", "TARAE_LSBENV_OTHER"] {
            assert!(!names.contains(&n), "{n} reached the run");
        }
        assert_eq!(read("probe"), "token=;path=set", "the toolchain's probe");
        let report = read("said");
        assert!(report.contains("token-like: ") && report.contains("GITHUB_TOKEN"), "{report}");
        assert!(report.contains("CARGO_REGISTRY_TOKEN") && report.contains("TARAE_"), "{report}");
        assert!(!report.contains(FAKE), "a value in the report: {report}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `a_fake_token_never_reaches_a_sandboxed_run`'s child: the policy for this checkout from what this
    /// process's environment says, a run in it writing the names it got (`names_of_its_environment`), the
    /// toolchain's probe saying whether two variables are set, and the report — into `$TARAE_ENV_PROBE`.
    #[test]
    #[ignore = "run by a_fake_token_never_reaches_a_sandboxed_run in a process of its own"]
    fn env_in_its_own_process() {
        let Some(dir) = std::env::var_os("TARAE_ENV_PROBE").map(PathBuf::from) else { return };
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let p = policy(root, &crate::llm::LlmConfig::default(), &[]);
        let exe = std::env::current_exe().unwrap();
        let args = ["--exact", "sandbox::tests::names_of_its_environment", "--ignored", "--nocapture"];
        let mut cmd = command(Some(&p), &exe.to_string_lossy(), &args.map(String::from));
        cmd.env("TARAE_ENV_NAMES", dir.join("names")).current_dir(root);
        let s = spawn(Some(&p), &cmd).expect("the run starts");
        let mut out = String::new();
        io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(s.pid).unwrap();
        // (whether each is set — never what it holds)
        let script = r#"printf '{"token": "%s", "path": "%s"}' "${GITHUB_TOKEN:+set}" "${PATH:+set}""#;
        let probe = output_in(&p, "/bin/sh", &["-c", script], &[]).expect("the probe answers");
        let said =
            format!("token={};path={}", probe["token"].as_str().unwrap(), probe["path"].as_str().unwrap());
        std::fs::write(dir.join("probe"), said).unwrap();
        std::fs::write(dir.join("said"), p.run_env.said(&mut Vec::new()).unwrap_or_default()).unwrap();
    }

    /// A sandboxed run's side: the names of its environment, one a line, into `$TARAE_ENV_NAMES` — never a
    /// value.
    #[test]
    #[ignore = "run in a sandbox by env_in_its_own_process"]
    fn names_of_its_environment() {
        let Some(file) = std::env::var_os("TARAE_ENV_NAMES") else { return };
        let mut names: Vec<String> = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).collect();
        names.sort();
        std::fs::write(file, names.join("\n")).unwrap();
    }
}
