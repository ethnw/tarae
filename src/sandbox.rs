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

// (step 7's two sessions side by side on Seatbelt — Linux's are the keeper's)
#[cfg(all(test, target_os = "macos"))]
mod apart;
// (the profile and its hint: plain text, built everywhere — said on Linux)
pub mod apparmor;
#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod channel;
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub mod claude;
// (a keeper's scope from the user's manager: the wire built everywhere — its tests run on macOS too —, asked on Linux)
pub mod dbus;
#[cfg(test)]
pub(crate) mod golden;
// (9a: what a run's sandbox keeps apart and shares — pure, built everywhere: its golden says every tier on any machine;
// Landlock's said on Linux)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod guarantees;
#[cfg(all(test, target_os = "linux"))]
mod handed;
#[cfg(target_os = "linux")]
pub mod keeper;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(test)]
mod loops;
// (6d: the session's marks — which run a connection is — given on Linux; the table built and tested everywhere)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod marks;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod names;
// (step 8: which session a socket is where sessions share a loopback — given on Linux, the lookup tested everywhere)
pub mod owners;
mod policy;
#[cfg(target_os = "linux")]
pub mod ptys;
// (the egress proxy: Linux's runs reach it through their supervisors, macOS's at their own ports)
pub mod proxy;
// (the session's transparent proxy: its ruleset built everywhere — its golden runs on macOS too —, sent on Linux)
pub mod redirect;
pub mod route;
pub mod runenv;
#[cfg(target_os = "linux")]
pub mod supervisor;
// (V2: what the supervisor reads of a run's memory, as the kernel reads it — built and tested everywhere, its loops on
// macOS too —, read on Linux)
#[path = "sandbox/supervisor/wire.rs"]
pub mod wire;
// (6f′: tarae's own copies of a run's sockets, where it can't take them — built and tested everywhere, used on Linux)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod copies;
#[cfg(target_os = "linux")]
pub mod tier;
// (tests: a hang fails within the test's own limit, saying what it waited for)
#[cfg(test)]
pub mod watchdog;

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub use policy::Policy;

#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Set in a sandboxed command's environment (tarae's own tests read it: a sandbox can't start inside one).
pub const MARK: &str = "TARAE_SANDBOXED";

/// Set in a sandboxed command's environment: its tier and Landlock ABI, or `seatbelt` (`Guarantees::status`).
pub const STATUS: &str = "TARAE_SANDBOX_STATUS";

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

/// What a run was let reach outside itself (`Guard::reaches_out`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachOut {
    /// The unix sockets granted it, said — a daemon's: a run granted one is there to talk to it.
    Sockets(String),
    /// The network beyond this machine.
    Network,
    /// No sandbox around it.
    Anything,
}

impl ReachOut {
    /// In words: `~/.colima/default/docker.sock` · `the network` · `anything — no sandbox around it`.
    pub fn said(&self) -> &str {
        match self {
            ReachOut::Sockets(s) => s,
            ReachOut::Network => "the network",
            ReachOut::Anything => "anything — no sandbox around it",
        }
    }
}

/// What a run's sandbox report said (`Guard::report_with`): its text, whether it said something was
/// blocked, and the digests of the rules and the guarantees it would say — `known` for the next report (None: a
/// run without guarantees).
#[derive(Debug)]
pub struct Report {
    pub text: String,
    pub blocked: bool,
    /// What it said was blocked that a grant could open — none kept on purpose, nor another session's port or one
    /// whose can't be told (`tools::also_said` reads it for Claude's `also`).
    pub open: Vec<Denial>,
    pub rules: u64,
    pub guarantees: Option<u64>,
}

/// What Claude was told in its conversation (`Chat.rules_said`, `tools::rules_known`), by digest: the rules (None:
/// say them) and the guarantees (`Guarantees::told` — None: say them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Told {
    pub rules: Option<u64>,
    pub guarantees: Option<u64>,
}

impl Guard {
    /// The run's watch, when it ran in the sandbox.
    pub fn watch(&self) -> Option<&std::sync::Arc<Watch>> {
        match self {
            Guard::In(p) => p.watch.as_ref(),
            _ => None,
        }
    }

    /// What a run under this guard was let reach through which its work may go on elsewhere — the unix
    /// sockets granted it (a daemon's), the network beyond this machine; outside the sandbox, anything. None:
    /// neither. `stuck.rs` says it (frictions15): next to no CPU here doesn't say a run is stuck when it was
    /// granted a daemon's socket — the network or no sandbox alone say nothing of where it is.
    pub fn reaches_out(&self) -> Option<ReachOut> {
        match self {
            Guard::In(p) if !p.sockets.is_empty() => {
                let socks: Vec<String> = p.sockets.iter().map(|s| crate::allow::tilde(s)).collect();
                Some(ReachOut::Sockets(socks.join(", ")))
            }
            Guard::In(p) if p.network => Some(ReachOut::Network),
            Guard::In(_) => None,
            Guard::Without(_) | Guard::Off | Guard::Outside => Some(ReachOut::Anything),
        }
    }

    /// Where a run under this guard ran, from its own record (cardburden 4): the mark its chat row ends with
    /// (`sandbox` — Linux: its tier, `sandbox A+`, and `partial` below Landlock ABI 3 —, `+ Docker socket` or the
    /// socket's name, `+ network`; `outside`, `no sandbox`, `sandbox off`), whether that's past the sandbox (a
    /// Docker socket is as much as outside — `allow::Grant::as_outside`), and in words.
    pub fn where_ran(&self) -> (String, bool, String) {
        match self {
            Guard::In(p) => {
                let (tier, how) = match p.tier_said() {
                    Some(t) if !t.is_empty() => (format!(" {t}"), format!("Landlock, tier {t}")),
                    Some(_) => (String::new(), "Landlock".to_string()),
                    None => (String::new(), "Seatbelt".to_string()),
                };
                let mut mark = format!("sandbox{tier}");
                let mut words = format!("In tarae's sandbox ({how})");
                let mut past = false;
                for s in &p.sockets {
                    let docker = crate::allow::Grant::Socket(s.clone()).as_outside();
                    past |= docker;
                    let name =
                        s.file_name().map_or_else(|| s.display().to_string(), |n| n.to_string_lossy().into());
                    mark.push_str(&if docker {
                        " + Docker socket".to_string()
                    } else {
                        format!(" + {name}")
                    });
                    words.push_str(&format!(", let connect to {}", crate::allow::tilde(s)));
                    if docker {
                        words.push_str(" — Docker's: as much as outside the sandbox");
                    }
                }
                if p.network {
                    mark.push_str(" + network");
                    words.push_str(", the network open (llm.sandbox-network)");
                }
                (mark, past, format!("{words}."))
            }
            Guard::Outside => (
                "outside".into(),
                true,
                "Outside the sandbox, with all your permissions — said yes to.".into(),
            ),
            Guard::Without(why) => {
                ("no sandbox".into(), true, format!("Without a sandbox ({why}) — you agreed."))
            }
            Guard::Off => ("sandbox off".into(), true, "Without a sandbox — llm.sandbox is off.".into()),
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
    /// is said apart, as the output's words (`output_says`): such a line may be the command's own. (Tests:
    /// runs say it through `report_with`.)
    #[cfg(test)]
    pub fn report(&self, output: &[(bool, String)]) -> String {
        self.report_with(output, Told::default()).text
    }

    /// `report`, the rules left out of a quiet one when `known` has their digest — Claude was told them in
    /// this conversation (`tools::rules_known`): ~400 tokens each run, read again by every later request. A
    /// report that says something was blocked, that tarae couldn't watch, or where nothing is watched (a
    /// refusal shows only in the output) always says them. The guarantees (what's kept from other sessions'
    /// commands and what's shared, `Guarantees::told`) come after the rules once a conversation and again when
    /// they change (9b, decision 11; N2: their notes too — 124 to 168 tokens, measured, in every report that says
    /// the rules again), whatever else the report says.
    pub fn report_with(&self, output: &[(bool, String)], known: Told) -> Report {
        self.report_as(output, known, self.guarantees())
    }

    /// `report_with`, its run's guarantees given (tests: each tier's on any machine).
    fn report_as(
        &self,
        output: &[(bool, String)],
        known: Told,
        guarantees: Option<guarantees::Guarantees>,
    ) -> Report {
        let digest = |s: &String| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        };
        let rules = self.rules();
        let rules_digest = digest(&rules);
        let quiet_rules = if known.rules == Some(rules_digest) { String::new() } else { rules.clone() };
        let told = guarantees.map(|g| g.told());
        let guarantees = told.as_ref().map(digest);
        let apart = told.filter(|_| known.guarantees != guarantees).unwrap_or_default();
        let (text, blocked, open) = self.report_text(output, rules, quiet_rules, &apart);
        Report { text, blocked, open, rules: rules_digest, guarantees }
    }

    /// What its runs are kept apart from and share (`Policy::guarantees`) — None: not in the sandbox.
    pub fn guarantees(&self) -> Option<guarantees::Guarantees> {
        match self {
            Guard::In(p) => p.guarantees(),
            _ => None,
        }
    }

    fn report_text(
        &self,
        output: &[(bool, String)],
        rules: String,
        quiet_rules: String,
        apart: &str,
    ) -> (String, bool, Vec<Denial>) {
        let Guard::In(p) = self else { return (rules, false, Vec::new()) };
        // (the guarantees: a paragraph of their own after the rules' — or where they'd be, left out)
        let with_apart = |s: String| match apart.is_empty() {
            true => s.trim_end().to_string(),
            false => format!("{}\n{apart}", s.trim_end()),
        };
        let unwatched = match p.denials_from() {
            Some(from) => format!(
                " What it refused is read {from}: \"Permission denied\" on a file, \"Operation not permitted\" on \
                 a socket, is likely the sandbox."
            ),
            None => " \"Operation not permitted\" is likely the sandbox.".into(),
        };
        let mut b = self.watch().map(|w| w.blocked()).unwrap_or_default();
        let mut said = p.said_in_output(output, &b.seen);
        // (`ps` that never started is said as itself, not as a write to it refused)
        let ps = ps_said(output, Os::HOST);
        if ps.is_some() {
            said.retain(|d| d.target != PS);
        }
        let ps = ps.unwrap_or_default();
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
            // (nothing watched: a refusal shows only in the output — the rules are how Claude reads it)
            return (with_apart(format!("{ps}{rules}{unwatched}")), false, Vec::new());
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
            s.push_str(ps);
            s.push_str(&rules);
            // (step 5g: what the proxy tarae itself goes out by failed is the user's proxy's, not the sandbox's)
            if all_seen.iter().any(|d| d.op == proxy::UPSTREAM_OP) {
                s.push_str(
                    "\nWhat says network-upstream isn't the sandbox: the proxy tarae itself goes out by (its proxy \
                     variables) failed or refused it — tell the user; their proxy, its credentials, or \
                     llm.sandbox-upstream = \"none\" (straight out) is theirs to change.",
                );
            }
            // (step 8: whose a port is couldn't be told — not a rule a grant opens, nor the command's to fix)
            if all_seen.iter().any(|d| d.op == UNTOLD_OP) {
                s.push_str(
                    "\nWhat says network-untold isn't a rule: tarae couldn't read this machine's lists of sockets \
                     (sock_diag, /proc/net) to tell whose that port is, so it refused — tell the user; tarae's \
                     log says why.",
                );
            }
            // (step 8: another session's port, or one whose can't be told — no grant opens it, so nothing to
            // point at)
            let openable = |d: &&Denial| d.op != SESSION_OP && d.op != UNTOLD_OP;
            let open: Vec<Denial> = all_seen.iter().filter(openable).filter(|_| !all).cloned().collect();
            let openable = all_seen.iter().any(|d| openable(&d));
            if (!all && openable) || b.distinct > b.seen.len() || o.distinct > o.seen.len() {
                s.push_str(
                    "\nIf the command needs one of these, tell the user — they can open it from the run's \
                     panel (space m s), or llm.sandbox-write, llm.sandbox-ports, llm.sandbox-localhost, \
                     llm.sandbox-network and llm.sandbox-hosts open more — or ask to run it outside the sandbox \
                     (outside_sandbox, with your reason); don't work around it.",
                );
            }
            return (with_apart(s), true, open);
        }
        let quiet = quiet_said(&b).unwrap_or_default();
        let text = match b.failed {
            None => with_apart(format!("The sandbox blocked nothing. {quiet}{ps}{quiet_rules}")),
            Some(why) => {
                let mut s =
                    format!("tarae couldn't watch what the sandbox blocked ({why}). {ps}{rules}{unwatched}");
                // (the lines read by the rules, right after them; the guarantees last)
                let refused = refused_lines(output);
                if !refused.is_empty() {
                    s.push_str("\nLines that may be the sandbox:\n");
                    for l in refused {
                        s.push_str(&format!("  {l}\n"));
                    }
                }
                with_apart(s)
            }
        };
        (text, false, Vec::new())
    }

    /// What among `seen` the profile keeps on purpose, each said with why and what to do instead — and
    /// whether that's all of them.
    fn kept(&self, seen: &[Denial]) -> (String, bool) {
        let Guard::In(p) = self else { return (String::new(), false) };
        let (mut names, mut git, mut caches, mut all) = (Vec::new(), false, Vec::new(), true);
        let (mut secret, mut services, mut copied) = (Vec::new(), Vec::new(), Vec::new());
        let (mut homes, mut ports, mut keys) = (Vec::new(), Vec::new(), Vec::new());
        let refused_port = &crate::sandboxpack::get().refused_port;
        let (mut entries, mut signals, mut looks, mut attaches) = (false, false, false, false);
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
                    _ if d.op == "process-info-pidinfo" => looks = true,
                    _ if d.op == "mach-priv-task-port" => attaches = true,
                    // (a process the packs say needs this machine's ports refused one: Gradle's JVM)
                    w if w.starts_with("connecting to port")
                        && refused_port.iter().any(|(n, _)| *n == d.process) =>
                    {
                        // (the open network isn't this machine's ports: it goes through tarae's proxy)
                        let closed = !p.localhost;
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
                Ok(rel) if !p.reads_allow(path) && key_named_in(&p.root, path) => {
                    keys.push(rel.display().to_string())
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
        crate::sorting::sort(&mut names);
        names.dedup();
        crate::sorting::sort(&mut secret);
        secret.dedup();
        crate::sorting::sort(&mut keys);
        keys.dedup();
        services.dedup();
        crate::sorting::sort(&mut copied);
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
        if !keys.is_empty() {
            let it = if keys.len() == 1 { "it" } else { "them" };
            s.push_str(&format!(
                "{}: named like a key, so your commands can't read {it} — a key or a token may be in {it}. If a \
                 command needs {it}, tell the user: the run's panel (space m s) lets your commands read that file. \
                 Don't work around it.\n",
                keys.join(", ")
            ));
        }
        if !names.is_empty() {
            let it = if names.len() == 1 { "it" } else { "them" };
            s.push_str(&format!(
                "{}: tarae keeps {it} from your commands — what's in {it} runs later outside the sandbox (the \
                 user's builds, their terminal) or sets how things run. To change {it}, edit {it} with your \
                 edit tool: the user reviews it. Don't work around it (another name, a copy, a link).\n",
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
        // (Linux: Landlock's scope keeps signals in; macOS: Seatbelt's `same-sandbox` keeps signals and looks
        // into processes' details and debuggers' attaching in — `OWN_PROCESSES`, F2)
        let verbs: Vec<&str> =
            [(signals, "signal"), (looks, "look into"), (attaches, "attach a debugger to")]
                .into_iter()
                .filter_map(|(done, verb)| done.then_some(verb))
                .collect();
        let to = match verbs.split_last() {
            None => None,
            Some((last, [])) => Some(last.to_string()),
            Some((last, rest)) => Some(format!("{} or {last}", rest.join(", "))),
        };
        match to {
            Some(_) if cfg!(target_os = "linux") => s.push_str(
                "A command can't signal a process outside its own (Landlock keeps it in) — tarae's, the \
                 user's others. If one needs to, say so to the user.\n",
            ),
            Some(to) => s.push_str(&format!(
                "A command can't {to} a process outside its own — tarae's, another session's commands, the \
                 user's others: macOS's sandbox lets it signal and read the details of (`lsof -p`) — and attach \
                 a debugger to (`lldb -p`) — only the processes it started. If one needs to, say so to the user.\n"
            )),
            None => {}
        }
        (s, all)
    }

    /// What the sandbox allows (or that there was none).
    pub(crate) fn rules(&self) -> String {
        match self {
            Guard::In(p) => {
                let ports = p.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
                let linux = p.linux_said();
                // (Linux: served, the open network is routed out — this machine's ports only as opened; not
                // served, no socket at all, said in its notes. macOS: out through the run's own proxy port)
                let routed = match &linux {
                    Some((no_socket, _)) => no_socket.is_none(),
                    None => cfg!(not(target_os = "linux")),
                };
                let beyond = p.network && (linux.is_none() || routed);
                // (Q3a, macOS: every port of this machine is every other run's way to tarae's proxy too)
                let every = match cfg!(target_os = "linux") {
                    true => "",
                    false => " — llm.sandbox-localhost opens other runs' ports to tarae's proxy too",
                };
                // (Linux, served, but this machine's addresses unread or no socket of tarae's can take a run's
                // place: nothing but loopback — `route` and the supervisor refuse the rest; macOS: no port of
                // the run's own to the proxy)
                let lo = match (p.localhost, ports.is_empty()) {
                    (true, _) => format!("every port of it{every}"),
                    (false, false) => format!("its ports {ports} only (llm.sandbox-ports)"),
                    (false, true) => {
                        "none of its ports (llm.sandbox-ports opens some, llm.sandbox-localhost all)"
                            .to_string()
                    }
                };
                let loopback_only = routed.then(|| p.loopback_only()).flatten();
                // (step 5g: through the proxy tarae itself uses, the names are that proxy's to look up — the
                // address checks don't hold there, said plainly; its variables named, never where it is)
                let never = "never link-local addresses, the cloud metadata servers tarae knows of, multicast or \
                             broadcast";
                let checks = match (p.egress.upstream.said(), p.egress.upstream.through()) {
                    (None, _) => never.to_string(),
                    (Some(ways), true) => format!(
                        "beyond it, the proxy tarae itself uses ({ways}) — which looks the names up, so host names \
                         and TLS server names are checked but not the addresses they lead to: link-local \
                         addresses, the cloud metadata servers and this machine's own addresses aren't refused for \
                         what goes through it (only the metadata servers' own names are — a partial check); \
                         NO_PROXY's hosts go direct, {never}"
                    ),
                    (Some(ways), false) => format!("beyond it: {ways}; {never}"),
                };
                // (that proxy on this machine, at a port opened to the runs: they reach it themselves)
                let reached_itself = p
                    .egress
                    .upstream
                    .on_loopback()
                    .into_iter()
                    .any(|at| p.localhost || p.ports.contains(&at));
                let checks = match reached_itself {
                    true => format!(
                        "{checks} (that proxy is on this machine, at a port open to your commands: they can reach it \
                         themselves, around tarae's host checks, though without tarae's credentials)"
                    ),
                    false => checks,
                };
                let net = match (beyond, p.localhost, ports.is_empty()) {
                    _ if loopback_only.is_some() => format!(
                        ", and connect to nothing but this machine's loopback, {lo} — {}{}",
                        loopback_only.as_deref().unwrap_or_default(),
                        if p.network { ", llm.sandbox-network notwithstanding" } else { "" }
                    ),
                    // (step 5: the open network goes through tarae's proxy — Linux: a direct connect is
                    // `network-direct`; macOS: Seatbelt opens loopback's ports only, the proxy's among them)
                    (true, here, none) => format!(
                        ", and reach the network beyond this machine (its loopback and its own addresses) {}; \
                         {checks} — {}",
                        match p.redirected {
                            // (6e′: the session's network takes every connection and answers every name — no
                            // variable; what isn't TLS or HTTP names no host to check — E4)
                            true => "through tarae's proxy, which takes every connection they make and answers \
                                     their names (no proxy variable is set); one that's neither TLS nor HTTP (ssh, \
                                     a database client) can't go out"
                                .to_string(),
                            false => "only through tarae's proxy (HTTPS_PROXY, set for every run) — a tool that \
                                      ignores it (ssh, a database client, Node's fetch without NODE_USE_ENV_PROXY) \
                                      can't go out"
                                .to_string(),
                        },
                        match (here, none) {
                            (true, _) => format!("and every port of this machine's{every}"),
                            (false, false) => format!("and this machine's ports {ports} only (llm.sandbox-ports)"),
                            (false, true) => "but none of this machine's ports (llm.sandbox-ports opens some, \
                                              llm.sandbox-localhost all)"
                                .to_string(),
                        }
                    ),
                    (false, true, _) => format!(", and connect to nothing over the network but this machine{every}"),
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
                // (7a, 7b — names from the data, `secretnames`; what a file made beside one does on Linux is
                // `linux::unread_said`'s, in the notes)
                let keys = match p.keys.is_some() {
                    true => {
                        " In the project it can't read a file named like a key or a token (but templates and \
                         public keys) — the user can open one from the run's panel."
                    }
                    false => "",
                };
                // (what's shared with other sessions' commands isn't here: the guarantees' paragraph, 9b)
                let (names_where, net, linux) = match linux {
                    Some((said, notes)) => ("where they are now", said.unwrap_or(net), notes),
                    None => ("anywhere in it", net, String::new()),
                };
                format!(
                    "In tarae's sandbox: it could {reads}write only in the project — not {names}, {names_where} \
                     (your edits change those) — temp folders and the toolchain's own{crates}{net}.{granted}{refused}\
                     {keys}{copies}{linux}"
                )
            }
            Guard::Without(why) => {
                format!("Ran without a sandbox ({why}) — the user agreed.{}", outside_userns(refused_here()))
            }
            Guard::Off => format!(
                "Ran without a sandbox — the user turned it off (llm.sandbox).{}",
                outside_userns(refused_here())
            ),
            Guard::Outside => {
                format!(
                    "Ran outside the sandbox — the user chose to, this once.{}",
                    outside_userns(refused_here())
                )
            }
        }
    }
}

/// A refusal's op where sessions share this machine's loopback and the socket there is another session's (step 8,
/// `supervisor::anothers_at`): no grant opens it.
pub const SESSION_OP: &str = "network-session";

/// A refusal's op where sessions share this machine's loopback and whose the socket there is couldn't be told —
/// neither the kernel's list of sockets nor `/proc/net` read (step 8, `supervisor::told`): refused, never said to be
/// another session's.
pub const UNTOLD_OP: &str = "network-untold";

/// What a Seatbelt policy's runs are kept apart from and share (9a, `guarantees.rs`): its shared memory and priority
/// — and, where it opens this machine's ports, what listens on them. Pinned by
/// `apart::two_sessions_cant_reach_each_others_{processes,sockets}`.
fn seatbelt_guarantees(p: &Policy) -> guarantees::Guarantees {
    guarantees::Guarantees::seatbelt(p.localhost || !p.ports.is_empty(), p.network)
}

/// Whether Claude's runs are refused new user namespaces here (`tier::runs_refuse_userns`) — outside the sandbox
/// too (`spawn::Acts`).
fn refused_here() -> bool {
    #[cfg(target_os = "linux")]
    return tier::runs_refuse_userns();
    #[cfg(not(target_os = "linux"))]
    false
}

/// What the report of a run outside the sandbox says of user namespaces (Q4): where they're refused (`refused`),
/// that this one had none either, and where they are — the user's own terminal and tools.
fn outside_userns(refused: bool) -> &'static str {
    match refused {
        true => {
            " It couldn't make a user namespace either (unshare, clone and clone3 are refused): tarae's AppArmor \
             profile would let it, which Ubuntu keeps from other programs — a tool that needs one (rootless \
             podman, bwrap, a browser's own sandbox) fails, and a setuid program (sudo) runs without its \
             privileges; the user's own terminal and tools keep both, as VS Code's do: such a command is theirs \
             to run there."
        }
        false => "",
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
    /// Linux: `/etc/resolv.conf`'s text — the resolvers a run reaches on port 53 with the network open (D-c,
    /// `route::resolvers`).
    #[cfg(target_os = "linux")]
    pub resolv: Option<String>,
    /// Linux: the addresses of tarae's own network (`keeper::addresses`, every interface's) — None where they
    /// couldn't be read.
    #[cfg(target_os = "linux")]
    pub addresses: Option<Vec<std::net::IpAddr>>,
    /// Linux: a keeper of a policy that opens the network takes its runs' connections in the session's network
    /// (`tier::redirects`) — asked on a run's way only (`policy_from`: the probe); `now` and stand-ins: false.
    pub redirects: bool,
    /// The sandbox refuses an IPv6 socket on this machine's loopback reached through a v4-mapped address
    /// (`::ffff:127.0.0.1`, what a JVM's 127.0.0.1 is): macOS's Seatbelt takes it as no `localhost`, connect and
    /// bind alike, below 27 — measured refused on 26.6.2, taken on 27.0.1 (`target/sb/macos26-jvm.md`; older ones
    /// taken as 26). Linux's supervisor reads the address itself: false. The packs' `env` `on = "v4-mapped-refused"`.
    pub v4_mapped_refused: bool,
    /// Tests: every variable asked (`path`, `var`) — what a policy reads of the environment
    /// (`every_variable_the_policy_reads_reaches_the_run`).
    #[cfg(test)]
    pub asked: std::sync::Arc<Mutex<Vec<String>>>,
    /// Tests: where runs' own folders are made instead of tarae's cache (`runs_root`) — `now` gives the test's
    /// own temp folder: a `cargo test` run in tarae's sandbox can't write tarae's cache (no run can: F1). None:
    /// as the variables say.
    #[cfg(test)]
    pub runs: Option<PathBuf>,
}

impl Here {
    pub fn now() -> Here {
        let env = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v)));
        // (tests: no upstream proxy but one a test gives — a `cargo test` run by a sandboxed run with the network
        // open has the outer tarae's proxy in its environment, and every proxy test would chain to it)
        #[cfg(test)]
        let env = env.filter(|(k, _)| !runenv::PROXIES.contains(&k.as_str()));
        Here {
            env: env.collect(),
            tmp: std::fs::canonicalize(std::env::temp_dir()).ok(),
            // SAFETY: getuid has no preconditions and can't fail.
            uid: unsafe { libc::getuid() },
            os: Os::HOST,
            #[cfg(target_os = "linux")]
            resolv: std::fs::read_to_string("/etc/resolv.conf").ok(),
            #[cfg(target_os = "linux")]
            addresses: keeper::addresses()
                .inspect_err(|e| {
                    let said = format!(
                        "this machine's addresses couldn't be read ({e}): Claude's runs reach nothing beyond \
                         loopback until they can"
                    );
                    crate::log::line("sandbox", "warn", &said);
                })
                .ok(),
            redirects: false,
            v4_mapped_refused: macos_major().is_some_and(|m| m < 27),
            #[cfg(test)]
            asked: Default::default(),
            #[cfg(test)]
            runs: Some(profile_path(&std::env::temp_dir())),
        }
    }

    /// Whether `on` holds here.
    pub fn holds(&self, on: crate::sandboxpack::On) -> bool {
        match on {
            crate::sandboxpack::On::V4MappedRefused => self.v4_mapped_refused,
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
    crate::sorting::sort(&mut allow);
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

/// `kept_reads` for `home` as `here` says the variables — tarae's state too where `here`'s `XDG_STATE_HOME` puts
/// it (`config::state_dir`'s rule): what Claude's Read never gets unasked either, wherever it is (cardauto D3:
/// it reads what runs read, and only that). Real paths. On a worker.
pub fn kept_places(home: &Path, here: &Here) -> Vec<PathBuf> {
    let mut v = kept_reads(home, &vars_in(home, here));
    let state =
        here.path("XDG_STATE_HOME").filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".local/state"));
    v.push(profile_path(&state.join("tarae")));
    crate::sorting::sort(&mut v);
    v.dedup();
    v
}

/// `kept_reads` with the packs' `kept` given.
fn kept_in(home: &Path, vars: &crate::sandboxpack::Vars, packs: &[String]) -> Vec<PathBuf> {
    let at = |p: PathBuf| profile_path(&p);
    let floor = crate::allow::SECRETS.iter().chain(&CLAUDES).chain(&KEPT_FLOOR);
    let mut v: Vec<PathBuf> = floor.map(|s| at(home.join(s))).collect();
    v.extend(packs.iter().flat_map(|k| vars.places_both(k)).map(at));
    v.push(at(vars.xdg_config.join("git/credentials")));
    v.extend(crate::config::state_dir().map(at));
    crate::sorting::sort(&mut v);
    v.dedup();
    v
}

/// The checkout's build tool (a built-in pack's `[builds]`), as its named folders are judged: who names them
/// (`builds-said`: `cargo`), its home (real, when it's there), and where a checkout's own build folder sits in
/// that home — so many folders below this one (`[builds] remove`: `build`, 2) — and how it marks a new one
/// (`[builds] cache-tag`, `macos-xattrs`: `own_build_made`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Builder {
    pub who: String,
    pub home: Option<PathBuf>,
    pub own_under: Option<(PathBuf, usize)>,
    pub marks: crate::builds::Marks,
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
            crate::sorting::sort(v);
            v.dedup();
        };
        let mut caches: Vec<_> = vars.caches.clone();
        for c in &mut caches {
            c.at = profile_path(&c.at);
        }
        crate::sorting::sort(&mut caches);
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

/// A run's own temp folder (its `TMPDIR`). Unix sockets anywhere else stay out of reach: in `/private/tmp` and
/// `$TMPDIR` live tmux's, zellij's and ssh-agent's — servers outside the sandbox that run what they're told
/// (reproduced: `tmux -S /private/tmp/tmux-501/default run-shell "touch ~/marker"` from inside made the
/// marker when temp sockets were allowed). Made in tarae's cache (`runs_root`), where no run writes but in its
/// own: in the temp folders, which every run writes, one run could put a file in another's — tools' homes are
/// there (`GRADLE_USER_HOME`: an init script runs in that run's build, with its session's grants — F1).
/// Removed, on a thread of its own, once the run is gone; a gone tarae's at the next start (`prune_runs`).
/// `<base>/<instance>/<n>`: in this tarae's own folder (`Instance`), whose lock says whose they are.
#[derive(Debug, PartialEq, Eq)]
pub struct RunDir(PathBuf);

/// The lock file in an instance's folder.
const INSTANCE_LOCK: &str = "lock";

/// This tarae's folder for its runs' folders in one base — `<pid>-<ms it was made>` (a later ms while that's
/// taken: another tarae with this pid, one in a container, made its folder in the same ms) — and the lock in it
/// (`INSTANCE_LOCK`): an flock held until this tarae ends, the kernel letting go however it ends (`:restart`'s
/// exec too: its runs are ended first, and the next start's prune takes what's left). Whose a run folder is
/// isn't its pid: a gone tarae's pid is often the next start's after a reboot, and a pid means nothing in
/// another pid namespace; a held lock is held for every process on this machine whatever its namespace.
///
/// What it can't see: a home shared over the network between machines. Locks there may not reach the other
/// machine (or reach it as something else), so another machine's tarae can look gone from here and its run
/// folders be pruned under it. tarae's cache is taken to be this machine's.
struct Instance {
    dir: PathBuf,
    lock: std::fs::File,
}

/// Each base's instance (in tarae one, the cache's; tests have their own).
static INSTANCES: Mutex<Vec<(PathBuf, Instance)>> = Mutex::new(Vec::new());

impl Instance {
    /// This tarae's (`pid`) folder in `base`, made new (named for `ms`, or the next free ms) and its lock taken.
    /// A prune can take a lock between its making and its taking — before it's held — and remove the folder:
    /// then it's made again under the next name. Blocks (a worker's).
    fn make(base: &Path, pid: u32, ms: u128) -> io::Result<Instance> {
        use std::os::unix::fs::DirBuilderExt;
        let mut last = None;
        for k in 0..8 {
            let dir = base.join(format!("{pid}-{}", ms + k));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            match lock_taken(&dir.join(INSTANCE_LOCK)) {
                Ok(Some(lock)) => return Ok(Instance { dir, lock }),
                // (a prune holds it: the folder is going)
                Ok(None) => {}
                Err(e) => {
                    let _ = std::fs::remove_file(dir.join(INSTANCE_LOCK));
                    let _ = std::fs::remove_dir(&dir);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("every name it tried for its folder was taken")))
    }

    /// Whether its folder and lock are still where they were (a cache cleared under tarae: not — a run folder
    /// can't be made there, and a lock no longer there protects nothing).
    fn still_there(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let at = std::fs::symlink_metadata(self.dir.join(INSTANCE_LOCK));
        match (self.lock.metadata(), at) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
}

/// The lock at `path` (made where there's none, through no link), taken — None while someone holds it, or
/// when it was removed or replaced before it was taken (a prune let it go): not to use.
fn lock_taken(path: &Path) -> io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let (ours, there) = (f.metadata()?, std::fs::symlink_metadata(path));
    Ok(there.ok().filter(|t| t.dev() == ours.dev() && t.ino() == ours.ino()).map(|_| f))
}

/// Whether `name` is an instance folder's (`<pid>-<ms>`, the pid not 0).
fn instance_name(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.split_once('-')
        .is_some_and(|(pid, ms)| digits(pid) && digits(ms) && pid.parse::<u32>().is_ok_and(|p| p > 0))
}

impl RunDir {
    /// A new folder in this tarae's `Instance` in `base` (`runs_root`, real; made where it isn't, only
    /// tarae's) — the instance made the first time, and again when it's gone — only ours. Short — a socket's
    /// path has 104 bytes at most. Blocks (a worker's).
    pub fn new_in(base: &Path) -> std::io::Result<std::sync::Arc<RunDir>> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(base)?;
        let mut instances = INSTANCES.lock().unwrap_or_else(|e| e.into_inner());
        instances.retain(|(b, i)| b != base || i.still_there());
        let instance = match instances.iter().position(|(b, _)| b == base) {
            Some(i) => &instances[i].1,
            None => {
                let ms =
                    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();
                instances.push((base.to_path_buf(), Instance::make(base, std::process::id(), ms)?));
                &instances[instances.len() - 1].1
            }
        };
        let dir = instance.dir.join(SEQ.fetch_add(1, Ordering::Relaxed).to_string());
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(std::sync::Arc::new(RunDir(dir)))
    }

    /// Tests: a new folder in the test's own temp folder (as `Here::now` says runs' are made).
    #[cfg(test)]
    pub fn new() -> std::io::Result<std::sync::Arc<RunDir>> {
        RunDir::new_in(&profile_path(&std::env::temp_dir()))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The run's tag: `tarae-<instance>-<n>.` — its instance's name and its own, closed with a dot (so `…-1.`
    /// isn't found in `…-10.`); `tarae-<pid>-` first, as `ours` reads it.
    pub fn tag(&self) -> String {
        let name =
            |p: Option<&Path>| p.and_then(Path::file_name).unwrap_or_default().to_string_lossy().into_owned();
        format!("tarae-{}-{}.", name(self.0.parent()), name(Some(&self.0)))
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let dir = std::mem::take(&mut self.0);
        crate::event::thread("tarae-tidy", move || std::fs::remove_dir_all(dir));
    }
}

/// The prune at tarae's start: the run folders a tarae that's gone left in tarae's cache (`runs_pruned`) — a
/// crash's, a kill's; the temp folders were the system's to clear, tarae's cache is tarae's. Said in the log.
/// Blocks (a worker's).
pub fn prune_runs() {
    prune_runs_in(&Here::now());
}

/// `prune_runs` where `here` says tarae's cache is, said in the log: what went, and what was left and why.
fn prune_runs_in(here: &Here) -> Pruned {
    let home = here.path("HOME").and_then(|h| std::fs::canonicalize(h).ok());
    let Some(base) = runs_root(home.as_deref(), here) else { return Pruned::default() };
    let pruned = runs_pruned(&base);
    if !pruned.gone.is_empty() {
        let line =
            format!("removed the run folders of {} gone taraes in {}", pruned.gone.len(), base.display());
        crate::log::line("sandbox", "info", &line);
    }
    for (dir, why) in &pruned.left {
        crate::log::line("sandbox", "warning", &format!("left {}: {why}", dir.display()));
    }
    pruned
}

/// What a prune of runs' folders did: the instances' folders that went, and those left though no tarae holds
/// them, each with why.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pruned {
    gone: Vec<PathBuf>,
    left: Vec<(PathBuf, String)>,
}

/// The instances' folders in `base` (`Instance`) whose tarae is gone — their lock taken here, so nobody holds
/// it — removed with what's in them; `remove_dir_all` follows no link inside, and the folder itself is one
/// (not a link to one). A running tarae's stay, this one's too (a lock taken twice in one process isn't: flock
/// is per open file). Left and said: one with no lock in it (a tarae that ended between making it and its lock,
/// or something not tarae's), an instance's name on what isn't a folder; what isn't an instance's name stays
/// unsaid. Blocks (a worker's).
fn runs_pruned(base: &Path) -> Pruned {
    use std::os::unix::fs::OpenOptionsExt;
    let mut pruned = Pruned::default();
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        if !e.file_name().to_str().is_some_and(instance_name) {
            continue;
        }
        let dir = e.path();
        // (the entry's own type: a link isn't followed)
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            pruned.left.push((dir, "not a folder, so not a tarae's".into()));
            continue;
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(INSTANCE_LOCK));
        let lock = match lock {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                pruned.left.push((dir, "no lock in it, so whose it is can't be told".into()));
                continue;
            }
            Err(e) => {
                pruned.left.push((dir, format!("its lock: {e}")));
                continue;
            }
        };
        match lock.try_lock() {
            Ok(()) => match std::fs::remove_dir_all(&dir) {
                Ok(()) => pruned.gone.push(dir),
                Err(e) => pruned.left.push((dir, format!("removing it: {e}"))),
            },
            // (its tarae is running)
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => pruned.left.push((dir, format!("its lock: {e}"))),
        }
    }
    pruned
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
        // (Linux's supervisor may say why after the address: `remote:1.2.3.4:443 — …`)
        let rest = rest.split_once(" — ").map_or(rest, |(at, _)| at);
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

    /// Whether it's a terminal's open refused (macOS, `TERMINALS`) — reading or writing `/dev/ttysN`. Not its
    /// removal, move or mode (closed too, said as such, and no grant offered for them: no tool needs that).
    pub fn terminal(&self) -> bool {
        let open =
            matches!(self.op.as_str(), "file-read-data" | "file-write-data" | "file-write-create" | EITHER);
        open && is_terminal(Path::new(&self.target))
    }

    /// In plain words: `writing ~/x`, `connecting to port 9`, `opening apps` — Seatbelt's own (`shown`) for
    /// what has none. Its network denials name no address, only the port (`remote:*:9` — measured).
    pub fn plain(&self) -> String {
        let t = self.target_shown();
        let op = self.op.as_str();
        // (a pty the run made, or the user's: Seatbelt can't tell them apart — the grant opens them all)
        if self.terminal() {
            return format!(
                "opening the terminal {t} — every terminal is closed to a run, the ones it makes too (expect, \
                 script, a pty test): macOS's sandbox can't tell them from the user's; the run's panel (space m \
                 s) lets this project's commands open terminals"
            );
        }
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
            // (Linux, 3b: past the ptys tarae opened for the run — `sandbox::ptys`)
            "pty-make" => format!("making more terminals than a run holds at a time ({t})"),
            "signal" if t.is_empty() => "signalling a process outside the run's own processes".into(),
            // (Seatbelt names it `others [name(pid)] signum:N` — F2, measured)
            "signal" => match seatbelt_process(&t) {
                Some((name, pid)) => format!("signalling {name} ({pid}) — outside the run's own processes"),
                None => format!("signalling process {t} — outside the run's own processes"),
            },
            // (no process named: `lsof -p` looks into every one — they're one denial, `Blocked::add`)
            "process-info-pidinfo" => "looking into processes outside the run's own".into(),
            // (a debugger's attach — `lldb -p`: Seatbelt names the process as for a signal, else as it does)
            "mach-priv-task-port" => match seatbelt_process(&t) {
                Some((name, pid)) => format!("attaching to {name} ({pid}) — outside the run's own processes"),
                None => "attaching to a process outside the run's own processes".into(),
            },
            _ if op.starts_with("file-write") => format!("changing {t}"),
            // (a path by what it is, not as shown: under home it's `~/…` — a VM's checkout in home)
            "network-outbound" if self.target.starts_with('/') => format!("connecting to socket {t}"),
            "network-outbound" if t.starts_with("remote:") => match self.port_here() {
                Some(port) => format!("connecting to port {port}"),
                None => match t.strip_prefix("remote:").map(|r| r.split_once(" — ").unwrap_or((r, ""))) {
                    Some((at, why)) if at.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) => {
                        match why.is_empty() {
                            true => format!("connecting to {at}"),
                            false => format!("connecting to {at} — {why}"),
                        }
                    }
                    _ => self.shown(),
                },
            },
            // (Linux's routing: an address never reached, and a socket that can't leave the session, its why
            // after the address — `supervisor::routed_socket`)
            "network-never" => match t.strip_prefix("remote:") {
                Some(at) => format!(
                    "connecting to {at} — link-local addresses, the cloud metadata servers tarae knows of, multicast and \
                     broadcast are never reached"
                ),
                None => self.shown(),
            },
            // (step 5: beyond this machine only through tarae's proxy — a direct connect, or a send naming it)
            "network-direct" => {
                match t.strip_prefix("remote:").map(|r| r.split_once(" — ").unwrap_or((r, ""))) {
                    Some((at, "")) => format!(
                        "connecting to {at} directly — beyond this machine your commands go only through tarae's \
                     proxy (HTTPS_PROXY); a tool that ignores it (ssh, a database client, Node's fetch without \
                     NODE_USE_ENV_PROXY) can't go out"
                    ),
                    Some((at, why)) => format!("connecting to {at} — {why}"),
                    None => self.shown(),
                }
            }
            // (step 5: the egress proxy's own refusals, `host:port: why` — `proxy::tell`)
            "network-proxy" => format!("reaching {t}"),
            // (step 5g: the upstream proxy's failure, not tarae's refusal — `host:port: through …`)
            "network-upstream" => match t.split_once(": ") {
                Some((at, why)) => format!("reaching {at} — {why}"),
                None => format!("reaching {t}"),
            },
            // (a host the user's answer would lift: not among the hosts, no answer, refused — `hosts.rs`)
            "network-host" => match t.split_once(": ") {
                Some((at, why)) => format!("reaching {at} — {why}"),
                None => format!("reaching {t}"),
            },
            // (6e+: raw TCP the user's answer would let through — `host:port: why`)
            "network-raw" => match t.split_once(": ") {
                Some((at, why)) => format!("a raw connection to {at} — {why}"),
                None => format!("a raw connection to {t}"),
            },
            // (6e+: a name's records the user's answer would let it look up — `SRV _x._tcp.name: why`)
            "network-lookup" => match t.split_once(": ") {
                Some((asked, why)) => format!("looking up {asked} — {why}"),
                None => format!("looking up {t}"),
            },
            "network-bound" => match t.strip_prefix("remote:").and_then(|t| t.split_once(' ')) {
                Some((at, why)) => format!("connecting to {at} {why}"),
                None => self.shown(),
            },
            "network-bind" if self.target.starts_with('/') => format!("making socket {t}"),
            // (step 8: where sessions share this machine's loopback — a call, `supervisor::anothers_at`; a bind or a
            // listen joining another session's port, `supervisor::joins_another`)
            SESSION_OP => {
                let addr = |a: &str| a.parse::<std::net::SocketAddr>().ok();
                match (t.strip_prefix("remote:").and_then(addr), t.strip_prefix("local:").and_then(addr)) {
                    (Some(at), _) => format!(
                        "connecting to {at} — port {} is another tarae session's: sessions' commands are kept apart, \
                         on this machine's opened ports too",
                        at.port()
                    ),
                    (_, Some(at)) => format!(
                        "binding {at} — port {} is another tarae session's: sessions' commands are kept apart, \
                         their ports too",
                        at.port()
                    ),
                    _ => self.shown(),
                }
            }
            UNTOLD_OP => {
                let addr = |a: &str| a.parse::<std::net::SocketAddr>().ok();
                let untold = "refused: whose the port is couldn't be told here (this machine's lists of sockets \
                              unread), and sessions' commands are kept apart";
                match (t.strip_prefix("remote:").and_then(addr), t.strip_prefix("local:").and_then(addr)) {
                    (Some(at), _) => format!("connecting to {at} — {untold}"),
                    (_, Some(at)) => format!("binding {at} — {untold}"),
                    _ => self.shown(),
                }
            }
            // (Linux's supervisor: a run's netlink only reads, from the kernel — `supervisor::netlink_reads`)
            "network-netlink" => {
                format!("netlink ({t}) — a command's netlink only reads the network, never changes it")
            }
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

/// macOS's `ps`: setuid (it runs as root to read every process), and Seatbelt starts no setuid program — no
/// rule opens it and nothing reaches the log; the shell says "Operation not permitted" (measured, 27.0).
const PS: &str = "/bin/ps";

/// Whether a line of output is a shell (or `env`) saying it couldn't start `ps`: `sh: /bin/ps: …`, `bash:
/// line 1: /bin/ps: …`, `zsh:1: operation not permitted: ps`, `env: ps: …`.
fn ps_refused(line: &str) -> bool {
    let l = line.trim_end().to_ascii_lowercase();
    let words = "operation not permitted";
    l.ends_with(&format!("{words}: ps"))
        || [format!(" {PS}: {words}"), format!(" ps: {words}")].iter().any(|w| l.contains(w.as_str()))
}

/// The report's line on `ps` when the output says it never started — on macOS only (elsewhere it runs).
fn ps_said(output: &[(bool, String)], os: Os) -> Option<&'static str> {
    (os == Os::Mac && output.iter().any(|(_, l)| ps_refused(l))).then_some(
        "ps never starts in macOS's sandbox: it's setuid (it runs as root to read every process), and the \
         sandbox starts no setuid program — nothing to open. pgrep -l -x name finds processes by name; lsof \
         -p <pid> shows what one of the run's own processes has open.\n",
    )
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
        // (a look into another process's details is one, whichever process: `ps`, `lsof -p`, `pgrep` look into
        // every one — each its own would take every slot from what else was refused)
        let same = |s: &Denial| s.op == d.op && (s.target == d.target || d.op == "process-info-pidinfo");
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

/// A refusal of something run `p` started (Linux's supervisor, the proxy): its own run's watch while that run
/// goes; after it, the newest run of its project still going — what a run left is the session's current run's
/// to say now; none going: kept for the next (`PENDING`).
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

/// Whether `tag` is one of this tarae's runs' (`RunDir::tag`: `tarae-<pid>-<ms>-<n>.` — its instances' since it
/// started, a `:restart`'s too: the same pid).
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

/// Where a run's supervisor (Linux) or tarae's proxy (macOS) says its refusals — a thread saying each in the
/// run's report (`tell`) until the sender is dropped.
pub(crate) fn told(p: std::sync::Arc<Policy>) -> std::sync::mpsc::Sender<Denial> {
    let (tx, rx) = std::sync::mpsc::channel::<Denial>();
    crate::event::thread("tarae-refused", move || {
        for d in rx {
            tell(&p, d);
        }
    });
    tx
}

/// Linux: what the runs of the project at `root` left that came to tarae (their keeper died — `keeper::gone`),
/// told by their tag, ended.
#[cfg(target_os = "linux")]
pub(crate) fn end_left_by_tag(root: &Path) -> Vec<u32> {
    end_left(|r| r.root == root)
}

/// Ends what Claude's runs left running — of the project at `root` (its session ended), or all of it (tarae
/// quits; one from before a `:restart` too). Blocks up to a few seconds — a worker's, or tarae's end.
pub fn end_leftovers(root: Option<&Path>) -> Vec<u32> {
    // Linux: the session's keeper ends its tree and itself (what it's ending told first); the next run makes
    // another. What came to tarae by a dead keeper is ended by its tag, below.
    // (all of them together: a quit with many sessions waits as long as one)
    #[cfg(target_os = "linux")]
    let mut by_keepers: Vec<u32> = {
        let keepers = keeper::of(root);
        let left = keepers.iter().flat_map(|k| k.leftovers()).collect();
        keeper::end_together(&keepers);
        left
    };
    #[cfg(not(target_os = "linux"))]
    let mut by_keepers = Vec::new();
    by_keepers.extend(end_tagged(root));
    by_keepers
}

/// `end_leftovers` of what tarae itself was given by its tag.
fn end_tagged(root: Option<&Path>) -> Vec<u32> {
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
    (q.run, q.tag, q.proxy, q.watch, q.env) = (None, None, None, None, Vec::new());
    // (`supervised` as the proxy URL it gives, below)
    (q.egress, q.run_id, q.supervised) = (Default::default(), 0, false);
    // (the kept homes are the project's and the key's — granted by them, not part of what's compared)
    q.write.retain(|w| !p.kept.iter().any(|k| k.home == *w));
    q.kept = Vec::new();
    #[cfg(target_os = "linux")]
    {
        q.confine = None;
    }
    let mut text = format!("{q:?}");
    // (a proxy URL that's the same for every run is the policy's: a home a daemon lives from never gets runs
    // whose proxy lines differ from those its daemon started with — a per-run one is never in a kept home)
    if let Some(url) = p.proxy_url().filter(|u| !u.per_run) {
        text.push_str(&format!(" proxy {}", url.url()));
    }
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
    crate::sorting::sort(&mut at);
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
    in_runs(protected_names().map(|n| n.split('/').map(any_case).collect::<Vec<_>>().join("/")))
}

/// The names like keys' (`secretnames::patterns`, data) as Seatbelt regex alternatives in any case, `*` any
/// characters but `/` — in runs as `name_regexes`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub(crate) fn key_regexes() -> Vec<String> {
    in_runs(crate::secretnames::patterns().map(glob_regex))
}

/// Their exceptions (`secretnames::exceptions`: `.env.example`, `*.pub`), alike.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub(crate) fn key_exception_regexes() -> Vec<String> {
    in_runs(crate::secretnames::exceptions().map(glob_regex))
}

/// The start of a regex matching below `root` however deep: as many path parts as the root has — no path in it,
/// so a name in the root's own path (a checkout under `.claude/worktrees`) doesn't close the whole project.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub(crate) fn below(root: &Path) -> String {
    let depth = root.components().filter(|c| matches!(c, Component::Normal(_))).count();
    format!("^{}/(.*/)?", "/[^/]+".repeat(depth))
}

/// The folders of tools' output (`outputs`) as Seatbelt regex alternatives.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn outputs_regex() -> String {
    outputs().iter().map(|o| as_written(o)).collect::<Vec<_>>().join("|")
}

/// What's kept from writes in the project at `root` (the profile's `ROOT`), as a Seatbelt filter: the protected
/// names (`name_regexes`) below it however deep, but below a folder of tools' output — runs' profile's rule and
/// claude's own (`claude::Profile`), one rule.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub(crate) fn protected_writes(root: &Path) -> String {
    let (below, outputs) = (below(root), outputs_regex());
    let (mut named, mut in_output) = (String::new(), String::new());
    for names in name_regexes() {
        named.push_str(&format!(" (regex #\"{below}({names})(/|$)\")"));
        in_output.push_str(&format!(" (regex #\"{below}({outputs})/(.*/)?({names})(/|$)\")"));
    }
    format!(
        "(require-all (subpath (param \"ROOT\")) (require-any{named}) (require-not (require-any{in_output})))"
    )
}

/// The files named like keys (`key_regexes`) right after `at` (a regex's start: `below` a project's root, `/`
/// anywhere) — but templates and public keys (`key_exception_regexes`) and what's below tools' output: two
/// filters for a `require-all`. Runs' profile's rule and claude's own (`claude::Profile`).
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub(crate) fn key_files(at: &str) -> String {
    let outputs = outputs_regex();
    let names =
        |v: Vec<String>| -> String { v.iter().map(|r| format!(" (regex #\"{at}({r})$\")")).collect() };
    let (named, except) = (names(key_regexes()), names(key_exception_regexes()));
    format!("(require-any{named}) (require-not (require-any{except} (regex #\"{at}({outputs})/\")))")
}

/// A file name with one `*` at most as a Seatbelt regex in any case: `[^/]*\.[pP][eE][mM]`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn glob_regex(pat: &str) -> String {
    pat.split('*').map(any_case).collect::<Vec<_>>().join("[^/]*")
}

/// Alternatives joined with `|` in runs of at most `NAMES_PER_REGEX` characters.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn in_runs(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
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

/// Whether `path` (real) is a file named like a key (`secretnames`, its exceptions taken out) in the project at
/// `root`, not below a folder of tools' output (`outputs`: a package's test keys in `node_modules`) — what the
/// macOS profile keeps a run from reading (7a) and Linux's rules leave out (`linux::there`, 7b), in Rust:
/// the report names what it denied.
pub fn key_named_in(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return false };
    let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
    let Some((_, folders)) = parts.split_last() else { return false };
    crate::secretnames::named_secret(rel)
        && !folders.iter().any(|p| outputs().iter().any(|o| *p == o.as_str()))
}

/// Device files every program writes to (`/dev/null`, `/dev/ptmx` — a pty test hangs without it: measured), as
/// the profile's filters (`devices_rule`): (filter, its argument). Not the terminals (`TERMINALS`).
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
const DEVICES: [(&str, &str); 6] = [
    ("literal", "/dev/null"),
    ("literal", "/dev/zero"),
    ("literal", "/dev/tty"),
    ("literal", "/dev/ptmx"),
    ("literal", "/dev/dtracehelper"),
    ("subpath", "/dev/fd"),
];

/// macOS's terminals — the user's shells', tarae's own, other sessions' runs' — closed to a run, read and write
/// (3c): Seatbelt matches paths and a pty a run makes is one of these names too (reused, lowest free first),
/// so it can't be told from theirs. `/dev/ptmx` stays: the pty is made, its other end's open refused
/// (`Denial::terminal`). The project's grant opens them all (`allow::Grant::Terminals`). (Runs start without a
/// controlling terminal and with pipes, never a pty of tarae's — `spawn.rs` — so `/dev/tty` reaches nothing.)
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
pub const TERMINALS: &str = "^/dev/ttys[0-9]+$";

/// Whether `path` is one `TERMINALS` matches — the rule in Rust.
pub fn is_terminal(path: &Path) -> bool {
    let n = path.to_str().and_then(|p| p.strip_prefix("/dev/ttys")).unwrap_or_default();
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

/// The profile's rule for `DEVICES`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
fn devices_rule() -> String {
    let filters: String = DEVICES.iter().map(|(f, a)| format!(" ({f} \"{a}\")")).collect();
    format!("(allow file-write*{filters})\n")
}

/// The device places a run writes (`Policy::write_places`): the files and folders the platform's rules name
/// (Seatbelt's `DEVICES`; Landlock's `linux::DEVICES`).
#[cfg(not(target_os = "linux"))]
static DEVICE_PLACES: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| DEVICES.iter().map(|(_, a)| *a).collect());
#[cfg(target_os = "linux")]
static DEVICE_PLACES: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| linux::DEVICES.iter().map(|(d, _)| *d).collect());

/// Ports on this machine where what listens runs what it's asked to, outside the sandbox. The network
/// closed (the default) or open (`llm.sandbox-network`: beyond this machine through tarae's proxy), every port
/// on this machine is closed but `llm.sandbox-ports` — these included. `llm.sandbox-localhost` opens them too:
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

/// The process a Seatbelt denial names — `others [name(pid)] …` — as (name, pid).
fn seatbelt_process(target: &str) -> Option<(&str, &str)> {
    let inner = target.split_once('[')?.1.split_once(']')?.0;
    let (name, pid) = inner.rsplit_once('(')?;
    let pid = pid.strip_suffix(')').filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?;
    Some((name, pid))
}

/// What a run does only to its own processes (`same-sandbox`): signal them, read their details (`lsof`'s
/// `proc_pidinfo`), take their task port — what a debugger attaches by (`lldb -p`: Apple's debugserver may, and
/// with it run anything as tarae or another session's command; `apart::two_sessions_cant_reach_each_others_
/// processes`). Not `process-info*` whole: listing pids has no target, and `lsof` of its own child failed
/// (spike M7.3). Not `mach-task-read`: lldb attached through its deny (measured, 2026-10-10).
#[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
const OWN_PROCESSES: [&str; 3] = ["signal", "process-info-pidinfo", "mach-priv-task-port"];

/// A new run's id — one counter for both systems: the proxy's questions name it, and the main loop finds the run
/// by it (`hosts.rs`).
fn next_run() -> u64 {
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    /// Where runs' own folders are made (`runs_root`, real).
    pub runs: Option<PathBuf>,
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
    /// `llm.sandbox-hosts` (None: any host).
    pub hosts: Option<Vec<proxy::egress::Pattern>>,
    /// The proxy tarae itself goes out by, as its environment names it (`llm.sandbox-upstream`, step 5g).
    pub upstream: proxy::upstream::Upstreams,
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
    /// The resolvers `resolv.conf` names (`route::resolvers`).
    #[cfg(target_os = "linux")]
    pub resolvers: Vec<std::net::SocketAddr>,
    /// This machine's own addresses (`Here::addresses`).
    #[cfg(target_os = "linux")]
    pub own_addresses: Option<Vec<std::net::IpAddr>>,
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

/// Linux: the profile's hint (`tier::hint`) the probe found — None elsewhere, or none. Only read (the probe has run
/// once a run's policy was made: `policy_from` → `confined`).
pub fn profile_hint() -> Option<String> {
    #[cfg(target_os = "linux")]
    return tier::probed().and_then(|p| p.hint.as_ref()).map(|h| h.said.clone());
    #[cfg(not(target_os = "linux"))]
    None
}

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
        runs: runs_root(real_home.as_deref(), here),
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
        hosts: llm.sandbox_hosts.clone(),
        upstream: upstream_of(here, llm.sandbox_upstream),
        tmux_tmpdir: here.path("TMUX_TMPDIR"),
        zellij_dir: here.path("ZELLIJ_SOCKET_DIR"),
        uid: here.uid,
        run_env: run_env_of(here, llm),
        #[cfg(target_os = "linux")]
        resolvers: here.resolv.as_deref().map(route::resolvers).unwrap_or_default(),
        #[cfg(target_os = "linux")]
        own_addresses: here.addresses.clone(),
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
        crate::sorting::sort(&mut env.reads.kept);
        env.reads.kept.dedup();
    }
    (env, above)
}

/// The upstream proxy `here`'s environment names, as `setting` takes it (step 5g) — variables set in both cases
/// with different values said in tarae's log once since tarae started (which one was taken: curl's order).
fn upstream_of(here: &Here, setting: proxy::upstream::Setting) -> proxy::upstream::Upstreams {
    let u = proxy::upstream::of(&here.env, setting);
    static SAID: std::sync::Once = std::sync::Once::new();
    if !u.mixed.is_empty() {
        let pairs: Vec<String> = u.mixed.iter().map(|(t, l)| format!("{t} over {l}")).collect();
        let line = format!("tarae's proxy variables differ in case — taken: {}", pairs.join(", "));
        SAID.call_once(|| crate::log::line("sandbox", "warning", &line));
    }
    u
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

/// Where kept run homes are: tarae's cache folder's `run-homes` (`taraes_cache`), real.
pub(crate) fn kept_root(home: Option<&Path>, here: &Here) -> Option<PathBuf> {
    taraes_cache(home, here).map(|c| profile_path(&c.join("run-homes")))
}

/// Where runs' own folders are made (`RunDir`): tarae's cache folder's `runs` (`taraes_cache`), real — beside
/// `run-homes`, out of every place a run may write (F1; a cache moved into one is refused: `runs_written`).
pub(crate) fn runs_root(home: Option<&Path>, here: &Here) -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(runs) = &here.runs {
        return Some(runs.clone());
    }
    taraes_cache(home, here).map(|c| profile_path(&c.join("runs")))
}

/// The place every run under `p` may write (the project, `write`) that holds runs' folders' `base` or is in it:
/// there one run could put a file in another's folder (F1) — `XDG_CACHE_HOME` in `/tmp`, `$TMPDIR` or the
/// project, `llm.sandbox-write` naming tarae's cache.
fn runs_written<'a>(p: &'a Policy, base: &Path) -> Option<&'a Path> {
    let open = std::iter::once(&p.root).chain(&p.write);
    open.map(PathBuf::as_path).find(|w| base.starts_with(w) || w.starts_with(base))
}

/// tarae's cache folder: `$XDG_CACHE_HOME/tarae`, else `~/.cache/tarae` — as `here` says (`home` real).
fn taraes_cache(home: Option<&Path>, here: &Here) -> Option<PathBuf> {
    let xdg = here.path("XDG_CACHE_HOME").filter(|p| p.is_absolute()).map(PathBuf::into_os_string);
    crate::runtime::cache_dir_of(xdg, home.map(|h| h.as_os_str().to_owned()))
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

/// The policy for the run about to start in the project at `root` with these settings and the places in home
/// the user let Claude read (`reads`, real) — the toolchain's folders found, and what the run needs there
/// before it starts (`run_policy_in`) — or why it can't start. Blocks (a worker's). (Tests; tarae's runs read
/// `Editor.sandbox_here`: `policy_from`.)
#[cfg(test)]
pub fn policy(root: &Path, llm: &crate::llm::LlmConfig, reads: &[PathBuf]) -> Result<Policy, String> {
    let w = crate::tools::Widened { llm: llm.clone(), reads: reads.to_vec(), ..Default::default() };
    policy_from(root, w, &Here::now())
}

/// `policy`, these files named like keys opened as the user's grant from the run's panel opens them
/// (`allow::Grant::Key`, through `tools::with_allowed`).
#[cfg(test)]
pub fn policy_opening(root: &Path, llm: &crate::llm::LlmConfig, keys: &[PathBuf]) -> Result<Policy, String> {
    let open = crate::allow::Allowed { keys: keys.to_vec(), ..Default::default() };
    policy_from(
        root,
        crate::tools::with_allowed(llm, &crate::caps::Caps::answered(&[&open]), None),
        &Here::now(),
    )
}

/// The policy for the run about to start with the settings and grants `w` (`tools::with_allowed`) and what
/// `here` says of tarae's surroundings (`Editor.sandbox_here`: a test's own).
pub fn policy_from(root: &Path, w: crate::tools::Widened, here: &Here) -> Result<Policy, String> {
    // (Linux: whether its keeper takes its connections in the session's network — the probe's, asked here)
    #[cfg(target_os = "linux")]
    let here = &Here { redirects: here.os == Os::Linux && tier::redirects(), ..here.clone() };
    let mut p = run_policy_in(root, &w.llm, &w.reads, here)?;
    // (what no setting carries goes on before Linux's rules are made from the policy: a file named like a key
    // the user opened is left out of what they close — 7b)
    w.onto(&mut p);
    #[cfg(target_os = "linux")]
    let p = confined(p);
    Ok(p)
}

/// `policy_in`, for a run about to start — only here, on a run's way, is anything made for it: its own folder
/// (`RunDir`) in tarae's cache and what's in it (`for_its_run`), and the checkout's own build folder in its
/// tool's home, opened to its runs, where it isn't yet — a run can't make it beside other checkouts'
/// (`own_build_made`). Computing a policy (`policy_computed`, `policy_in`; `write_places`) writes nothing in
/// the user's homes. A run whose own folder can't be made doesn't start: why, and what to do.
pub(crate) fn run_policy_in(
    root: &Path,
    llm: &crate::llm::LlmConfig,
    reads: &[PathBuf],
    here: &Here,
) -> Result<Policy, String> {
    let (mut p, found) = policy_computed(root, llm, reads, here);
    // (a run without its own folder would have the temp folders, which every run writes, as its `TMPDIR` and
    // tools' homes, and no tag its denials are watched by: it doesn't start)
    let log = crate::log::Session::of(root);
    let run = run_folder(&p, here).map_err(|why| {
        log.line("sandbox", "warning", &why);
        format!("The sandbox gives each command a folder of its own, and {why}, then run it again.")
    })?;
    for_its_run(&mut p, Some(run), reads, here, &found);
    if let Some((own, tool)) = found.own {
        match own_build_made(&own, &tool) {
            Ok(true) => log.line("sandbox", "info", &format!("made {} for its first build", own.display())),
            Ok(false) => {}
            Err(e) => log.line("sandbox", "warning", &format!("the checkout's own build folder: {e}")),
        }
    }
    Ok(p)
}

/// `policy` with what `here` says, computed — its Landlock rules not made yet (`confined`), and nothing made
/// for a run (`run_policy_in`): no folder of its own, so no sockets but the project's, `TMPDIR` as it was, no
/// tag to watch by, no tools' homes of its own.
#[cfg(test)]
pub(crate) fn policy_in(root: &Path, llm: &crate::llm::LlmConfig, reads: &[PathBuf], here: &Here) -> Policy {
    let (mut p, found) = policy_computed(root, llm, reads, here);
    for_its_run(&mut p, None, reads, here, &found);
    p
}

/// What computing a policy found that its run is made from (`for_its_run`): the real home, cargo's configs
/// above the project (`env_in`), and the checkout's own build folder in its tool's home when the policy opens
/// it to its runs (the folder, and the tool that keeps it).
struct Found {
    real_home: Option<PathBuf>,
    above: Vec<PathBuf>,
    own: Option<(PathBuf, Builder)>,
}

/// The policy for the project at `root`, computed — nothing made in the user's homes: no run's folder, no kept
/// home stamped, no build folder (those are a run's way's, `run_policy_in`).
fn policy_computed(
    root: &Path,
    llm: &crate::llm::LlmConfig,
    reads: &[PathBuf],
    here: &Here,
) -> (Policy, Found) {
    let (env, above) = env_in(root, llm, reads, here);
    let real_home = env.home.as_deref().and_then(|h| std::fs::canonicalize(h).ok());
    let mut p = Policy::for_project(&env);
    let own = env.own_build.clone().filter(|o| p.write.contains(o)).map(|o| (o, env.builder.clone()));
    // (macOS: the network open goes out through tarae's proxy, at a port of the run's own)
    if p.network && here.os == Os::Mac {
        p.proxy = proxy::RunPort::new(p.run_id)
            .inspect_err(|e| crate::log::line("sandbox", "warning", &format!("a run's proxy port: {e}")))
            .ok();
    }
    // (Linux: the supervisor hands the run's connections over — in the session's network where its keeper gets
    // one, taken there: the open network needs no proxy URL)
    p.supervised = here.os == Os::Linux;
    p.keys = Some(Vec::new());
    p.redirected = p.supervised && p.network && here.redirects;
    with_kept(&mut p, &env);
    (p, Found { real_home, above, own })
}

/// A run's own folder, made in tarae's cache (`p.runs`) — or why there's none: no cache folder, one that
/// can't be made, or one where every run may write (`runs_written`: F1 quietly lost — refused, never warned),
/// each with what to do.
fn run_folder(p: &Policy, here: &Here) -> Result<std::sync::Arc<RunDir>, String> {
    // (tests: a runs folder of the test's own, in its temp folder — `Here::runs`)
    #[cfg(test)]
    let given = here.runs.is_some();
    #[cfg(not(test))]
    let (given, _) = (false, here);
    let Some(base) = p.runs.as_deref() else {
        let why =
            "tarae has no cache folder to make it in (neither XDG_CACHE_HOME nor HOME is set for tarae)";
        return Err(format!("{why}. Start tarae with HOME set"));
    };
    let at = home_shown(&base.display().to_string());
    if let Some(w) = runs_written(p, base).filter(|_| !given) {
        let w = home_shown(&w.display().to_string());
        let why = format!("its own folder would be made in {at}, where every run may write ({w})");
        return Err(format!("{why}. Set XDG_CACHE_HOME for tarae to a folder no run writes"));
    }
    RunDir::new_in(base).map_err(|e| {
        // (#219: a full disk said as one — the run's folder is often the first thing that can't be made)
        if matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) {
            return format!(
                "the disk is full — its own folder couldn't be made in {at} ({e}). Tell the user"
            );
        }
        let why = format!("its own folder couldn't be made in {at} ({e})");
        format!("{why}. Check that {at} can be written — its permissions, the disk's free space")
    })
}

/// What `p`'s run gets in its own folder `run` (none: a policy only, `policy_in`): its tag, the copies of
/// settings that hold a token, its kept homes held, its tools' homes and the packs' variables, its proxy files.
fn for_its_run(
    p: &mut Policy,
    run: Option<std::sync::Arc<RunDir>>,
    reads: &[PathBuf],
    here: &Here,
    found: &Found,
) {
    (p.tag, p.run) = (run.as_ref().map(|r| r.tag()), run);
    let real_home = &found.real_home;
    // Settings that hold a token: copies without it in the run's folder (`tokens.rs`) — while reads are
    // confined; what the user let Claude read stays readable
    if let Some(h) = real_home {
        let sources = crate::tokens::sources_on(h, &|name| here.var(name), here.os);
        let dir = p.run.as_ref().map(|r| r.path());
        p.reads.copies = crate::tokens::copies(&sources, dir, h, reads, &p.reads.kept);
        p.reads.copies.told = crate::tokens::told_holders(&vars_in(h, here), &found.above);
    }
    let url = p.proxy_url();
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
                crate::log::Session::of(&p.root).line(
                    "sandbox",
                    "warning",
                    &format!("kept home {}: {e}", k.home.display()),
                );
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
    let made = p
        .run
        .as_ref()
        .zip(vars.as_ref())
        .map(|(r, v)| run_homes_made(r.path(), v, &p.run_env, &p.kept, url.as_ref()));
    p.env = made.unwrap_or_default();
    p.env.extend(vars.as_ref().map(|v| packs_env(v, here)).unwrap_or_default());
    // What a run with a proxy URL gets of the packs besides (`[sandbox.proxy]`)
    let mut env = std::mem::take(&mut p.env);
    proxy_made(p.run.as_ref().map(|r| r.path()), url.as_ref(), &mut env, &p.run_env);
    p.env = env;
}

/// The packs' `env` for a run, its places filled in (`vars` knows the run's folder): one naming a place that
/// isn't known (`{run}` without a run) is left out — and one waiting for a fact of the system (`on`), where
/// `here` says it holds.
fn packs_env(vars: &crate::sandboxpack::Vars, here: &Here) -> Vec<(String, String)> {
    let filled = |v: &str| match v.starts_with('{') || v.starts_with("~/") {
        true => vars.place(v).map(|p| p.display().to_string()),
        false => Some(v.to_string()),
    };
    let packs = crate::sandboxpack::get();
    let on = packs.env_on.iter().filter(|e| here.holds(e.on)).map(|e| (&e.name, &e.value));
    packs
        .env
        .iter()
        .map(|(k, v)| (k, v))
        .chain(on)
        .filter_map(|(k, v)| Some((k.clone(), filled(v)?)))
        .collect()
}

/// macOS's major version (`kern.osproductversion`: `26.6.2` → 26) — None where it can't be read.
#[cfg(target_os = "macos")]
fn macos_major() -> Option<u32> {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: a NUL-ended name, a buffer of `len` bytes and its length to fill; nothing set.
    let r = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let text = (r == 0).then(|| String::from_utf8_lossy(&buf[..len.min(buf.len())]).into_owned())?;
    text.trim_end_matches('\0').split('.').next()?.parse().ok()
}

/// (not macOS)
#[cfg(not(target_os = "macos"))]
fn macos_major() -> Option<u32> {
    None
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

/// Linux: `p` with its session's keeper found or made, in the namespaces this system and the policy give it
/// (`tier::for_policy`; `keeper::ready`: a keeper of another policy key ends what its runs left — D14), and its
/// Landlock rules made for that keeper's runs (`linux::confine` — it walks the project for protected names,
/// makes first what a build makes beside them: `linux::Made`, and grants the devices on the keeper's own).
/// Either failing, the run doesn't start, saying why.
#[cfg(target_os = "linux")]
pub fn confined(mut p: Policy) -> Policy {
    let ns = tier::for_policy(&p);
    let key = policy_key(&p);
    let made = keeper::ready(&p.root, Some(&key), ns).and_then(|k| {
        let made = crate::landlock::abi().and_then(|abi| {
            linux::confine(&p, abi, linux::audit_readable(abi), true, Some(&k), linux::served_here())
        });
        let mut c = made.map_err(|e| format!("the sandbox couldn't be made: {e}"))?;
        c.keeper = Some(k);
        Ok(c)
    });
    p.confine = Some(made);
    p
}

/// The run's own homes for the tools (the packs' `run-home` — Gradle's: the user's holds what their own builds
/// run later and their daemons' registry; Go's build cache), made in its folder `run` — or the one `kept` for
/// the project (only tarae's: 0700) — the folders each says, links from it into the user's places that are
/// there (read only — what a run writes there is the packs' `write-files`), and the environment that points
/// the tool there — its variable, what's set (when its `if` place is there), what's appended to the user's own
/// value as the run gets it (`run_env`'s: a token-like one is left out, and gets only what's appended — never
/// tarae's own value, which goes in after `from_run_env`; `{home}` in it: the home's folder) — and, a home a
/// daemon lives from, its `daemon`'s folders and appends; a home of the run's own, its `fresh` appends; its
/// `proxy` file with `url`'s values, removed without one (a kept home's, from an earlier run) — and a home with
/// one is the run's own when the URL is (`kept_takes`). Gone with the run's folder, but a kept one. What's made
/// in a home goes through no link (`Unlinked`): a kept one is a folder earlier runs wrote in, and tarae isn't
/// sandboxed. Blocks (a worker's).
fn run_homes_made(
    run: &Path,
    vars: &crate::sandboxpack::Vars,
    run_env: &runenv::RunEnv,
    kept: &[Kept],
    url: Option<&proxy::ProxyUrl>,
) -> Vec<(String, String)> {
    let at = |p: &str| vars.place(p).map(|p| profile_path(&p));
    let mut env: Vec<(String, String)> = Vec::new();
    for h in &crate::sandboxpack::get().run_homes {
        let kept = kept.iter().find(|k| k.name == h.name).filter(|_| kept_takes(h, url));
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
            appended(&mut env, run_env, var, &more.replace("{home}", &home.display().to_string()));
        }
        // Its proxy file: this run's values, or none (a kept home's from an earlier run removed)
        if let (Some(lines), Ok(dir)) = (&h.proxy, &dir) {
            let done = match url {
                Some(url) => dir.write_file(&lines.file, proxy_text(&lines.lines, url).as_bytes()),
                None => dir.remove_file(&lines.file),
            };
            if let Err(e) = done {
                crate::log::line("tarae", "warning", &format!("a run's {} home: {e}", h.name));
            }
        }
    }
    env
}

/// `more` after `var`'s value as the run gets it: what's appended already, else the user's own (`run_env` — a
/// token-like one is left out, and gets only what's appended).
fn appended(env: &mut Vec<(String, String)>, run_env: &runenv::RunEnv, var: &str, more: &str) {
    let i = env.iter().position(|(k, _)| k == var);
    let theirs = match i {
        Some(i) => Some(env.remove(i).1),
        None => run_env.var(var),
    };
    let value = match theirs.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        Some(o) => format!("{o} {more}"),
        None => more.to_string(),
    };
    env.push((var.to_string(), value));
}

/// Whether a kept home may stand for run home `h` with this proxy URL: not when `h` has proxy lines and the
/// URL's values are the run's alone (its own port and token) — another run reads a kept home, and a daemon
/// serves later runs from it (step 5f, the user's decision 2026-10-07: the run's own home instead, the warm
/// daemon given up).
fn kept_takes(h: &crate::sandboxpack::RunHome, url: Option<&proxy::ProxyUrl>) -> bool {
    h.proxy.is_none() || !url.is_some_and(|u| u.per_run)
}

/// `lines` with `url`'s values filled in (`{proxy.<value>}`), as a file's text — a line naming a value that's
/// empty for this run (no user, no password) left out whole.
fn proxy_text(lines: &[String], url: &proxy::ProxyUrl) -> String {
    let mut text = String::new();
    'line: for line in lines {
        let mut l = line.clone();
        for v in proxy::URL_VALUES {
            let named = format!("{{proxy.{v}}}");
            if l.contains(&named) {
                let Some(value) = url.value(v) else { continue 'line };
                l = l.replace(&named, &value);
            }
        }
        text.push_str(&l);
        text.push('\n');
    }
    text
}

/// What the packs' `[sandbox.proxy]` gives a run with a proxy URL (none without one): variables set, appended to
/// the run's own (`env` holds what's set so far — the run homes'), and files written in its own folder `run` —
/// their `append` naming them (`{file}`). Blocks (a worker's).
fn proxy_made(
    run: Option<&Path>,
    url: Option<&proxy::ProxyUrl>,
    env: &mut Vec<(String, String)>,
    run_env: &runenv::RunEnv,
) {
    let Some(url) = url else { return };
    let p = &crate::sandboxpack::get().proxy;
    env.extend(p.env.iter().cloned());
    for (var, more) in &p.append {
        appended(env, run_env, var, more);
    }
    if p.files.is_empty() {
        return;
    }
    let dir = run.ok_or_else(|| "no folder of the run's own".to_string()).and_then(Unlinked::made);
    for f in &p.files {
        let written = dir.as_ref().map_err(Clone::clone).and_then(|d| {
            d.write_file(&f.file, proxy_text(&f.lines, url).as_bytes())?;
            Ok(d.at.join(&f.file))
        });
        match written {
            Ok(at) => {
                for (var, more) in &f.append {
                    appended(env, run_env, var, &more.replace("{file}", &at.display().to_string()));
                }
            }
            Err(e) => crate::log::line("tarae", "warning", &format!("a run's proxy file {}: {e}", f.file)),
        }
    }
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

    /// File `name` (a plain name) in it holding `text`, written whole (0600): a file of its own made through no
    /// link (`O_EXCL | O_NOFOLLOW`), then renamed over the name — a link there is replaced, never gone through.
    fn write_file(&self, name: &str, text: &[u8]) -> Result<(), String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let at = self.at.join(name);
        let said = |e: io::Error| format!("{}: {e}", at.display());
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = c_name(format!(".{name}.tarae-{}-{n}", std::process::id()).as_ref())?;
        let c = c_name(name.as_ref())?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&self.fd);
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: a folder descriptor of ours and a name, live for the call.
        let got = unsafe { libc::openat(fd, temp.as_ptr(), flags, 0o600 as libc::c_uint) };
        if got < 0 {
            return Err(said(io::Error::last_os_error()));
        }
        // SAFETY: just made, ours.
        let mut file: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(got) };
        let written = io::Write::write_all(&mut file, text);
        drop(file);
        // SAFETY: a folder descriptor of ours and two names, live for the call.
        let renamed =
            written.and_then(|()| match unsafe { libc::renameat(fd, temp.as_ptr(), fd, c.as_ptr()) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            });
        if renamed.is_err() {
            // SAFETY: as above.
            unsafe { libc::unlinkat(fd, temp.as_ptr(), 0) };
        }
        renamed.map_err(said)
    }

    /// File `name` in it removed — a link there the link itself; none there is fine.
    fn remove_file(&self, name: &str) -> Result<(), String> {
        let c = c_name(name.as_ref())?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&self.fd);
        // SAFETY: a folder descriptor of ours and a name, live for the call.
        match unsafe { libc::unlinkat(fd, c.as_ptr(), 0) } {
            0 => Ok(()),
            _ => match io::Error::last_os_error() {
                e if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
                e => Err(format!("{}: {e}", self.at.join(name).display())),
            },
        }
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

/// The checkout's own build folder in its tool's home (`own`, real: one its runs may write —
/// `asked_dir_verdict`), made where it isn't yet. The tool makes a new one under a temp name beside it and
/// renames it into place (cargo does), and beside it are other checkouts' build folders, which no run writes —
/// a checkout's first build in the sandbox couldn't start. Made from the tool's home (real) a folder at a time,
/// each opened without following a link (`O_NOFOLLOW`): a link on the way stops it (Err), never removed — the
/// home's folders are the user's. The folder made is marked as the tool marks a new one (`cache_marked`) —
/// or, that failing, taken back with the folders made on the way to it (Err): the tool never marks one that's
/// there already, so one left unmarked would stay out of nothing (backups, file sync) for good. Ok(true):
/// made. Blocks (a worker's).
fn own_build_made(own: &Path, tool: &Builder) -> Result<bool, String> {
    own_build_made_with(own, tool, &|dir| cache_marked(dir, &tool.marks))
}

/// `own_build_made`, the new folder marked by `mark` (tests: one that fails).
fn own_build_made_with(
    own: &Path,
    tool: &Builder,
    mark: &dyn Fn(&Unlinked) -> Result<(), String>,
) -> Result<bool, String> {
    if std::fs::symlink_metadata(own).is_ok() {
        return Ok(false);
    }
    let home = tool.home.as_deref().ok_or("its tool's home isn't known")?;
    let rel = own.strip_prefix(home).map_err(|_| format!("{} isn't in {}", own.display(), home.display()))?;
    let mut dirs = vec![
        open_dir_at(libc::AT_FDCWD, &c_name(home.as_os_str())?)
            .map_err(|e| format!("{}: {e}", home.display()))?,
    ];
    // (each folder this call made, outermost first: its parent's place in `dirs`, its name, its path)
    let (mut at, mut made) = (home.to_path_buf(), Vec::new());
    for part in rel.iter() {
        let c = c_name(part)?;
        at.push(part);
        let fd = std::os::fd::AsRawFd::as_raw_fd(&dirs[dirs.len() - 1]);
        // SAFETY: a folder descriptor of ours and a name, live for the call (one there already: EEXIST). The
        // mode as the tool's own (`create_dir_all`'s), the umask taking its part.
        if unsafe { libc::mkdirat(fd, c.as_ptr(), 0o777) } == 0 {
            made.push((dirs.len() - 1, c.clone(), at.clone()));
        }
        dirs.push(
            open_dir_at(fd, &c).map_err(|e| format!("{}: {e} — a link or no folder there", at.display()))?,
        );
    }
    if !made.last().is_some_and(|(i, ..)| i + 2 == dirs.len()) {
        return Ok(false);
    }
    let new = Unlinked { fd: dirs.pop().expect("the folder made"), at };
    let Err(why) = mark(&new) else { return Ok(true) };
    // What marking put in it, then each folder made on the way, innermost first — empty: nothing else has run
    // in them yet. Removed as folders only (rmdir): one holding something now (another checkout's first build,
    // meanwhile) is that one's, and stays with the ones around it, said.
    let gone = new.remove_file("CACHEDIR.TAG").and_then(|()| {
        for (n, (i, name, at)) in made.iter().rev().enumerate() {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&dirs[*i]);
            // SAFETY: a folder descriptor of ours and a name, live for the call.
            if unsafe { libc::unlinkat(fd, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return match io::Error::last_os_error() {
                    e if n > 0 && e.raw_os_error() == Some(libc::ENOTEMPTY) => {
                        Ok(Some(format!("{} stays: something else is in it now", at.display())))
                    }
                    e => Err(format!("{}: {e}", at.display())),
                };
            }
        }
        Ok(None)
    });
    Err(match gone {
        Ok(None) => format!("{why} — not kept, unmarked"),
        Ok(Some(stays)) => format!("{why} — not kept, unmarked; {stays}"),
        Err(e) => format!("{why} — and it couldn't be taken back: {e}"),
    })
}

/// The cache directory tag's first line, as the standard has it (https://bford.info/cachedir/ — backup tools
/// skip a folder holding a `CACHEDIR.TAG` that starts with it, whatever tool made it).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";

/// Folder `dir`, new, marked as its tool marks a new one — what its pack says (`marks`): the cache directory
/// tag, its signature line then the pack's lines (none: no tag), and on macOS the pack's extended attributes.
fn cache_marked(dir: &Unlinked, marks: &crate::builds::Marks) -> Result<(), String> {
    if let Some(text) = &marks.cache_tag {
        dir.write_file("CACHEDIR.TAG", format!("{CACHEDIR_SIGNATURE}{text}").as_bytes())?;
    }
    #[cfg(target_os = "macos")]
    for (name, value) in &marks.macos_xattrs {
        let c = c_name(name.as_ref())?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&dir.fd);
        // SAFETY: a folder descriptor of ours, a name and a value of its length, live for the call.
        if unsafe { libc::fsetxattr(fd, c.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) } != 0 {
            return Err(format!("{} ({name}): {}", dir.at.display(), io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// `name` as C wants it.
pub(crate) fn c_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString, String> {
    std::ffi::CString::new(name.as_encoded_bytes()).map_err(|_| format!("{name:?}: a NUL in it"))
}

/// Folder `c` in `fd` (or `AT_FDCWD`) opened — Err when it's a link (`O_NOFOLLOW`) or no folder.
pub(crate) fn open_dir_at(fd: libc::c_int, c: &std::ffi::CStr) -> io::Result<std::os::fd::OwnedFd> {
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
        marks: tool.as_ref().map(|t| t.marks().clone()).unwrap_or_default(),
    };
    let fields =
        tool.as_ref().map(|t| t.asked()).map(|(_, t, b)| [t.to_string(), b.unwrap_or(t).to_string()]);
    let said = said.map(|s| [s.target.clone(), s.build.clone()]).or(fields).unwrap_or_default();
    let var = |n: &str| here.var(n).filter(|v| !v.is_empty());
    let own = tool.as_ref().and_then(|t| t.own_setting(&var, tool_home.as_deref()));
    let others: Vec<crate::sandboxpack::Ask> = t.ask.iter().filter(|a| has(&a.when)).cloned().collect();
    let mut set_by = tool.as_ref().map(|t| t.set_by(root, tool_home.as_deref())).unwrap_or_default();
    set_by.extend(others.iter().flat_map(|a| a.when.iter().map(|f| root.join(f))));
    let env = home.map(|h| packs_env(&vars_in(h, here), here)).unwrap_or_default();
    Asks { tool, builder, said, own, others, env, set_by }
}

/// What `toolchain_dirs` found per project (real path): the stamps of the files that set them, the folders.
type Seen = HashMap<PathBuf, (Vec<Option<SystemTime>>, ToolDirs)>;
static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();

/// The project's toolchain folders outside it as last found for a run of Claude's (`toolchain_dirs`) — only
/// those its runs may write (`asked_dir_verdict`, `ask_dir_verdict`; a shared cache too, which they read), judged
/// for the home and temp folder `here` says (`allow::defaults_in`'s); nothing looked up: none before its first run.
pub fn build_dirs(root: &Path, here: &Here) -> Vec<PathBuf> {
    let root = profile_path(root);
    let Some(found) =
        SEEN.get().and_then(|s| s.lock().ok().and_then(|m| m.get(&root).map(|(_, d)| d.clone())))
    else {
        return Vec::new();
    };
    let home = here.path("HOME");
    let tool = asks_for(&root, home.as_deref(), here).builder;
    let (own, tmp) = (found.own_build.as_deref(), here.tmp.as_deref());
    let open = |d: &Path| asked_dir_verdict(d, &root, home.as_deref(), &tool, own, tmp).is_ok();
    let named = found.named.iter().map(|(d, _)| d).filter(|d| open(d));
    let real_home = home.as_deref().and_then(|h| std::fs::canonicalize(h).ok());
    let later = real_home.as_deref().map(|h| RunsLater::new(h, &vars_in(h, here), &path_dirs_in(here)));
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
    // (through the session's keeper — any of its policies': a probe's is none — without a supervisor)
    #[cfg(target_os = "linux")]
    let out = {
        let abi = crate::landlock::abi().ok()?;
        let ns = tier::for_policy(policy);
        let k = keeper::ready(&policy.root, None, ns).ok()?;
        let unserved = Err("a probe's run".into());
        let rules = linux::confine(policy, abi, Err("not watched".into()), false, Some(&k), unserved).ok()?;
        let mut c = Command::new(program);
        from_run_env(&mut c, policy);
        c.args(args).envs(env.iter().copied()).current_dir(&policy.root).env(MARK, "1");
        let s = k.start_confined(&c, &crate::landlock::thread_name(), &rules).ok()?.spawned;
        // (what it says on stderr read meanwhile, unkept: a full pipe would stop it)
        if let Some(mut err) = s.stderr {
            crate::event::thread("tarae-probe", move || io::copy(&mut err, &mut io::sink()));
        }
        let mut stdout = Vec::new();
        io::Read::read_to_end(&mut s.stdout?, &mut stdout).ok()?;
        let status = crate::spawn::wait(s.pid).ok()?;
        std::process::Output { status, stdout, stderr: Vec::new() }
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

/// What tarae sets in every sandboxed run besides the packs' `env` (`inside_env`, `command`'s npm line and its
/// `STATUS`, `task::color_off`, `containers::VAR`) — and `TARAE_UI`, never a run of Claude's: none said as left
/// out of its environment.
const SET_IN_RUNS: [&str; 7] = [
    MARK,
    STATUS,
    "GIT_OPTIONAL_LOCKS",
    "npm_config_update_notifier",
    "NO_COLOR",
    "TARAE_UI",
    crate::containers::VAR,
];

/// What a run in a policy made with these settings gets of `here`'s environment (`runenv::of`): the floor,
/// what the packs' places read, the packs' `pass`, `llm.sandbox-env`.
pub fn run_env_of(here: &Here, llm: &crate::llm::LlmConfig) -> runenv::RunEnv {
    let packs = crate::sandboxpack::get();
    let policy: Vec<&str> = packs.env_names().collect();
    let colors = &crate::languages::get().color_off;
    let mut set: Vec<&str> = SET_IN_RUNS.to_vec();
    set.extend(colors.iter().map(|(k, _)| k.as_str()));
    // (the network open goes through tarae's proxy — its variables take the user's own proxies' place, neither
    // passed nor said; macOS: git's setting for it too)
    if llm.sandbox_network {
        set.extend(runenv::PROXIES);
        if cfg!(not(target_os = "linux")) {
            set.push("GIT_CONFIG_COUNT");
        }
    }
    runenv::of(here, &policy, &packs.pass, &llm.sandbox_env, &set)
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

/// Tests: what runs between a keeper's run's start and its watch hearing its pid (its pid, its name), for the
/// project at the root given — `Started` held there (`linux::tests::a_denial_before_its_start_is_told_is_the_runs`).
#[cfg(all(test, target_os = "linux"))]
pub(crate) type Held = std::sync::Arc<dyn Fn(u32, &str) + Send + Sync>;
#[cfg(all(test, target_os = "linux"))]
pub(crate) static STARTED_HELD: std::sync::Mutex<Option<(PathBuf, Held)>> = std::sync::Mutex::new(None);
#[cfg(all(test, target_os = "linux"))]
fn lock_held() -> std::sync::MutexGuard<'static, Option<(PathBuf, Held)>> {
    STARTED_HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts `cmd` (made by `command`) in a session of its own (`spawn::setsid`) — on Linux forked by the session's
/// keeper (`keeper::Keeper::start`: its rules and the seccomp filter its own, and tarae's supervisor deciding
/// what it reaches by address — the main loop waits for its start only); elsewhere the sandbox is in the
/// command already. A policy whose rules or keeper couldn't be made doesn't start at all.
/// Without a policy (outside the sandbox) its process acts for `acts`: Claude's is kept from new user namespaces
/// where runs are (`spawn::setsid_as`) — a sandboxed one is by its keeper's run.
pub fn spawn_as(
    policy: Option<&Policy>,
    cmd: &Command,
    acts: crate::spawn::Acts,
) -> std::io::Result<crate::spawn::Spawned> {
    #[cfg(target_os = "linux")]
    if let Some(p) = policy {
        let c = match &p.confine {
            Some(Ok(c)) => c,
            Some(Err(why)) => return Err(std::io::Error::other(why.clone())),
            None => return Err(std::io::Error::other("the sandbox wasn't made for this run")),
        };
        let Some(k) = &c.keeper else {
            return Err(std::io::Error::other("the session's keeper wasn't made for this run"));
        };
        // Its watch knows it by its name before it exists: its first denial (and its domain's record with it)
        // can come before `Started` does — then by its pid too
        let comm = crate::landlock::thread_name();
        let owner = |pid| linux::Owner { pid, comm: comm.clone() };
        if let Some(w) = &p.watch {
            w.owned_by(owner(0));
        }
        let started = match c.net {
            tier::RunNet::Served | tier::RunNet::Routed | tier::RunNet::Copied => {
                let serve = std::sync::Arc::new(p.clone());
                k.start_served(cmd, &comm, c, serve.clone(), told(serve))?
            }
            _ => k.start_confined(cmd, &comm, c)?,
        };
        let spawned = started.spawned;
        #[cfg(test)]
        let held = lock_held().as_ref().filter(|(r, _)| *r == p.root).map(|(_, f)| f.clone());
        #[cfg(test)]
        if let Some(held) = held {
            held(spawned.pid, &comm);
        }
        if let Some(w) = &p.watch {
            w.owned_by(owner(spawned.pid));
        }
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
    let spawned = crate::spawn::setsid_as(cmd, acts)?;
    // (macOS, the network open: the run's own port goes to tarae's proxy — its end comes with `after_run`)
    #[cfg(not(target_os = "linux"))]
    if let Some((p, r)) = policy.and_then(|p| Some((p, p.proxy.as_ref()?))) {
        r.started(p, program_of(cmd), told(std::sync::Arc::new(p.clone())));
    }
    #[cfg(target_os = "linux")]
    let _ = policy;
    Ok(spawned)
}

/// Tests: `spawn_as`, the process the user's (what they test is the policy's run).
#[cfg(test)]
pub fn spawn(policy: Option<&Policy>, cmd: &Command) -> std::io::Result<crate::spawn::Spawned> {
    spawn_as(policy, cmd, crate::spawn::Acts::User)
}

/// What a sandboxed command runs (`command`'s: the program after `sandbox-exec`'s own arguments), by its file
/// name — what the proxy's refusals name (macOS: no supervisor knows the process that connected).
#[cfg(not(target_os = "linux"))]
fn program_of(cmd: &Command) -> String {
    let mut args = cmd.get_args();
    let program = match cmd.get_program() == SANDBOX_EXEC {
        true => loop {
            match args.next() {
                Some(a) if a == "-p" || a == "-D" => {
                    args.next();
                }
                other => break other,
            }
        },
        false => Some(cmd.get_program()),
    };
    let name = program.map(|p| Path::new(p).file_name().unwrap_or(p).to_string_lossy().into_owned());
    name.unwrap_or_default()
}

/// What a run in `policy` needs done once it's over, before its end is told (the run's own thread, after it's
/// reaped): on Linux, the folders it could remove inside protected ones made again (`linux::Kept`) — the
/// report says which; on macOS, its port to tarae's proxy closed (`proxy::RunPort::ended`) — what it left
/// running loses its way out, closed.
pub fn after_run(policy: Option<&Policy>) -> impl FnOnce() + Send + 'static {
    #[cfg(target_os = "linux")]
    let kept = match policy.map(|p| &p.confine) {
        Some(Some(Ok(c))) => Some(c.kept.clone()),
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let proxied = policy.and_then(|p| p.proxy.as_ref()).map(|r| r.run);
    move || {
        #[cfg(target_os = "linux")]
        if let Some(k) = kept {
            k.restore();
        }
        #[cfg(not(target_os = "linux"))]
        if let Some(run) = proxied {
            proxy::RunPort::ended(run);
        }
    }
}

/// `program args` inside `policy` — or as it is, without one. On Linux the sandbox comes with the spawn
/// (`spawn`); here only what a sandboxed run's environment says: nothing of tarae's but `Policy::run_env`, then
/// what tarae sets — `MARK`, `GIT_OPTIONAL_LOCKS` (`inside_env`), the policy's own variables, `TMPDIR`, the
/// token-less copies' variables, the proxy's, and last `STATUS` (9c: after the filter, which would drop the one an
/// outer sandbox gave tarae). Without a policy (the user's own runs) nothing is set.
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
            // The network open goes through tarae's egress proxy (step 5), as the run's proxy URL says
            if let Some(url) = p.proxy_url() {
                // (after the settings the run's own environment holds — `llm.sandbox-env` may pass git's)
                let count = p.run_env.var("GIT_CONFIG_COUNT").and_then(|n| n.parse().ok());
                c.envs(proxy::run_env(&url, count.unwrap_or(0)));
            }
            // (a policy without guarantees says none — nor a value from outside, `llm.sandbox-env` passing tarae's)
            match p.guarantees() {
                Some(g) => c.env(STATUS, g.status()),
                None => c.env_remove(STATUS),
            };
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
    skipped_inside_for(test, "Seatbelt can't start inside a sandbox")
}

/// `skipped_inside`, saying `why` the test can't run inside a sandbox — a pty's other end, closed to a run
/// (`TERMINALS`, 3c).
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn skipped_inside_for(test: &str, why: &str) -> bool {
    if std::env::var_os(MARK).is_none() {
        return false;
    }
    let line = format!(
        "{}{test} — {why}; run `cargo test` outside one to check it\n",
        crate::test_results::SELF_SKIP
    );
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    true
}

/// Whether test `test` (its full path), which opens a pty — a terminal panel, `pty::open` — skips itself here:
/// inside macOS's sandbox a run can't open a pty's other end (`TERMINALS`). Elsewhere it runs.
#[cfg(test)]
pub(crate) fn no_pty_here(test: &str) -> bool {
    #[cfg(target_os = "macos")]
    return skipped_inside_for(
        test,
        "a run inside macOS's sandbox can't open a pty's other end (sandbox::TERMINALS)",
    );
    #[cfg(not(target_os = "macos"))]
    {
        let _ = test;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of Claude's outside the sandbox — once, for the conversation, with the sandbox off — tells Claude,
    /// where runs are refused user namespaces, that it had none either, that a setuid program ran without its
    /// privileges (the filter's `no_new_privs`), and that the user's terminal is where such a command runs; where
    /// they aren't, not a word of it. Each guard says what this machine decides (`refused_here`).
    #[test]
    fn a_run_outside_the_sandbox_says_it_had_no_user_namespace_where_refused() {
        let said = outside_userns(true);
        assert!(
            said.starts_with(" It couldn't make a user namespace either (unshare, clone and clone3"),
            "{said}"
        );
        assert!(said.contains("a setuid program (sudo) runs without its privileges"), "{said}");
        assert!(said.contains("the user's own terminal and tools keep both"), "{said}");
        assert_eq!(outside_userns(false), "");
        for g in [Guard::Without("no Landlock".into()), Guard::Off, Guard::Outside] {
            assert!(g.report(&[]).ends_with(outside_userns(refused_here())), "{g:?}");
        }
    }

    /// Where a run may write, as `gitrepo` asks it (`write_places`), is its policy's own (`policy`): the same
    /// list — the project, the toolchain's folders its probe found (a Go module cache here), `llm.sandbox-write`,
    /// runs' own folders, the devices. And a file in that toolchain folder gets no git by discovery
    /// (`gitrepo::for_file_in`), though a repository holds it.
    #[test]
    fn write_places_are_the_policys_own() {
        let pid = std::process::id();
        let scratch = crate::scratch::Scratch::new("write-places");
        let root = scratch.to_path_buf();
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
        // (runs' folders: in a test, its own temp folder — `Here::now`)
        let runs = profile_path(&std::env::temp_dir());
        let named = [&root, &cache, Path::new("/opt/tarae-granted"), &runs];
        for p in named.into_iter().chain(devices.iter().map(Path::new)) {
            assert!(w.iter().any(|x| x == p), "{} in {w:?}", p.display());
        }
        let p = policy(&root, &llm, &[]).unwrap();
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
             (literal \"/dev/ptmx\") (literal \"/dev/dtracehelper\") (subpath \"/dev/fd\"))\n"
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
        let scratch = crate::scratch::Scratch::new("sb-gitlock");
        let dir = scratch.to_path_buf();
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
        let marks = rust.as_ref().unwrap().marks().clone();
        let builder = Builder { who: "cargo".into(), home: Some(home.into()), own_under, marks };
        Env { caches: vec![said], files, builder, ..Env::default() }
    }

    pub(super) fn env(root: &Path) -> Env {
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
        run_homes_made(run, &vars_in(&user.join("no-home"), &here), &run_env, &[], None)
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

    /// For real, with the policy a run gets (cargo asked where the checkout builds): where your setting gives
    /// each checkout a build folder of its own in cargo's home (`{cargo-cache-home}/build/
    /// {workspace-path-hash}`), a checkout's first `cargo test` runs in the sandbox — cargo makes a new one under
    /// a temp name beside it and renames it into place, and beside it are other checkouts' build folders, so
    /// tarae makes it on the run's way (`run_policy_in` — a policy made alone writes nothing in cargo's home:
    /// tests and goldens make policies), marked as cargo marks a new one (the cache tag; on
    /// macOS, kept out of Time Machine and file sync: the same attributes as the other checkout's, which cargo
    /// made). And another checkout's build folder stays as it was: no file made, changed or removed in it, no
    /// folder made beside it. Harmless: a cargo home and two checkouts of the test's own under `target/`
    /// (never the real `~/.cargo` — only its toolchain runs).
    #[test]
    fn a_checkouts_first_cargo_test_runs_and_no_other_build_dir_is_written() {
        let test = "sandbox::tests::a_checkouts_first_cargo_test_runs_and_no_other_build_dir_is_written";
        #[cfg(target_os = "macos")]
        if skipped_inside(test) {
            return;
        }
        #[cfg(target_os = "linux")]
        if !matches!(crate::landlock::abi(), Ok(n) if n >= 2) {
            let why = format!("{}{test} — needs Landlock ABI 2\n", crate::test_results::SELF_SKIP);
            let _ = std::io::Write::write_all(&mut std::io::stderr(), why.as_bytes());
            return;
        }
        let t = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/sb-own-build-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&t).ok();
        let (home, p_root, q_root) = (t.join("cargo"), t.join("p"), t.join("q"));
        for (root, name) in [(&p_root, "p"), (&q_root, "q")] {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(
                root.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
                ),
            )
            .unwrap();
            std::fs::write(root.join("src/lib.rs"), "#[test]\nfn t() {\n    assert_eq!(1 + 1, 2);\n}\n")
                .unwrap();
        }
        std::fs::create_dir_all(&home).unwrap();
        let home = std::fs::canonicalize(&home).unwrap();
        let per_checkout = "{cargo-cache-home}/build/{workspace-path-hash}";
        // (the variable wins over every cargo config — yours above this folder too)
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let outside = |root: &Path, args: &[&str]| {
            let out = Command::new(&cargo)
                .args(args)
                .current_dir(root)
                .env("CARGO_HOME", &home)
                .env("CARGO_BUILD_BUILD_DIR", per_checkout)
                .env("CARGO_TARGET_DIR", root.join("target"))
                .env_remove("RUSTC_WRAPPER")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "cargo {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            out.stdout
        };
        let build_dir = |root: &Path| {
            let json = outside(root, &["metadata", "--no-deps", "--offline", "--format-version", "1"]);
            let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
            profile_path(Path::new(v["build_directory"].as_str().unwrap()))
        };
        // The other checkout built by cargo itself, outside — its build folder made and marked by cargo
        outside(&q_root, &["check", "--offline", "-q"]);
        let (own, theirs) = (build_dir(&p_root), build_dir(&q_root));
        assert!(own.starts_with(home.join("build")) && theirs.starts_with(home.join("build")), "{own:?}");
        assert!(!own.exists(), "this checkout never built");
        std::fs::write(theirs.join("marker"), "kept").unwrap();
        // The policy a run gets, cargo's home and setting as the test's
        let mut here = Here::now();
        here.env.insert("CARGO_HOME".into(), home.clone().into());
        here.env.insert("CARGO_BUILD_BUILD_DIR".into(), per_checkout.into());
        here.env.insert("CARGO_TARGET_DIR".into(), p_root.join("target").into());
        here.env.remove("RUSTC_WRAPPER");
        let llm = crate::llm::LlmConfig::default();
        // Making a policy writes nothing in cargo's home: only a run's way does
        let made = policy_in(&p_root, &llm, &[], &here);
        assert!(made.write.contains(&own) && !own.exists(), "made by a policy alone");
        let p = run_policy_in(&p_root, &llm, &[], &here).unwrap();
        #[cfg(target_os = "linux")]
        let p = confined(p);
        assert!(p.write.contains(&own), "its own build folder opened: {:?} · {:?}", p.write, p.refused);
        let script = r#"
            if "$C" test --offline -q >"$TMPDIR/out" 2>&1; then echo "test ok"; else echo "test failed:"; cat "$TMPDIR/out"; fi
            w() { if ( eval "$2" ) 2>/dev/null; then echo "$1 ok"; else echo "$1 no"; fi; }
            w make 'printf x > "$Q/made"'
            w change 'printf y >> "$Q/marker"'
            w remove 'rm "$Q/marker"'
            w beside 'mkdir "${Q}abcdef"'
        "#;
        let mut cmd = command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.current_dir(&p_root).env("C", &cargo).env("Q", &theirs).stdin(Stdio::null());
        let spawned = spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        after_run(Some(&p))();
        let got: Vec<&str> = out.lines().collect();
        assert_eq!(got, ["test ok", "make no", "change no", "remove no", "beside no"], "{out}");
        // Marked as cargo marks the one it makes: its tag byte for byte (both systems — the pack's text is
        // pinned here, against the tag cargo wrote)
        let tag = |d: &Path| std::fs::read(d.join("CACHEDIR.TAG")).unwrap_or_default();
        let cargos = tag(&theirs);
        assert!(cargos.starts_with(CACHEDIR_SIGNATURE.as_bytes()), "{:?}", String::from_utf8_lossy(&cargos));
        assert_eq!(tag(&own), cargos, "the cache tag");
        // (the rust pack's attributes, each as cargo set it on its own — the pack's bytes are cargo's)
        #[cfg(target_os = "macos")]
        for (name, value) in &cargo_at(&home).builder.marks.macos_xattrs {
            let (mine, cargos) = (xattr_of(&own, name), xattr_of(&theirs, name));
            assert_eq!(cargos.as_ref(), Some(value), "cargo's {name}");
            assert_eq!(mine, cargos, "{name}");
        }
        // The other checkout's as it was
        assert_eq!(std::fs::read_to_string(theirs.join("marker")).unwrap(), "kept");
        assert!(!theirs.join("made").exists());
        let beside = std::fs::read_dir(theirs.parent().unwrap()).unwrap().flatten().map(|e| e.file_name());
        let name = theirs.file_name().unwrap().to_string_lossy().into_owned();
        let beside: Vec<_> =
            beside.filter(|n| n.to_string_lossy().starts_with(&name) && *n != *name).collect();
        assert!(beside.is_empty(), "{beside:?}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// F1: a run writes only its own run folder — never another run's, its session's or another's. tarae points
    /// tools' homes there (`GRADLE_USER_HOME`, Maven's settings, npm's logs): a file put in another run's
    /// `gradle/init.d/` would run inside that run's build, with its session's grants. Two sessions' runs (two
    /// projects' policies, each with its folder where tarae makes them): the first makes, changes or removes
    /// nothing in the second's folder, makes no folder beside it (where a later run's would be) and doesn't
    /// touch the lock that says its tarae runs (`Instance`); its own it writes. (Projects of the test's own.)
    #[test]
    fn a_run_writes_only_its_own_run_folder() {
        let test = "sandbox::tests::a_run_writes_only_its_own_run_folder";
        #[cfg(target_os = "macos")]
        if skipped_inside(test) {
            return;
        }
        #[cfg(target_os = "linux")]
        if crate::landlock::abi().is_err() {
            let why = format!("{}{test} — needs Landlock\n", crate::test_results::SELF_SKIP);
            let _ = std::io::Write::write_all(&mut std::io::stderr(), why.as_bytes());
            return;
        }
        // (not in a temp folder: a policy never opens one holding its project, and a checkout isn't in one)
        let t = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/sb-run-folders-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&t).ok();
        let (p_root, q_root) = (t.join("p"), t.join("q"));
        for root in [&p_root, &q_root] {
            std::fs::create_dir_all(root).unwrap();
        }
        let (p_root, q_root) = (profile_path(&p_root), profile_path(&q_root));
        // Runs' folders where tarae makes them: its cache's `runs`, as the variables say (a cache of the test's
        // own — not the test's temp folder, `Here::now`'s for tests)
        let mut here = Here::now();
        here.runs = None;
        here.env.insert("XDG_CACHE_HOME".into(), t.join("cache").into());
        let llm = crate::llm::LlmConfig::default();
        let run = |root: &Path| run_policy_in(root, &llm, &[], &here).expect("a run's policy");
        let (p, q) = (run(&p_root), run(&q_root));
        #[cfg(target_os = "linux")]
        let p = confined(p);
        let theirs = q.run.as_ref().expect("the other run's folder").path().to_path_buf();
        assert!(theirs.starts_with(t.join("cache/tarae/runs")), "{}", theirs.display());
        std::fs::write(theirs.join("marker"), "kept").unwrap();
        let script = r#"
            w() { if ( eval "$2" ) 2>/dev/null; then echo "$1 ok"; else echo "$1 no"; fi; }
            w own 'printf x > "$TMPDIR/mine"'
            w make 'printf x > "$Q/made"'
            w change 'printf y >> "$Q/marker"'
            w remove 'rm "$Q/marker"'
            w beside 'mkdir "${Q}0"'
            w lock 'printf x >> "${Q%/*}/lock"'
        "#;
        let mut cmd = command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.current_dir(&p_root).env("Q", &theirs).stdin(Stdio::null());
        let spawned = spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        after_run(Some(&p))();
        let beside = PathBuf::from(format!("{}0", theirs.display()));
        let (marker, made, beside_made) =
            (std::fs::read_to_string(theirs.join("marker")), theirs.join("made").exists(), beside.exists());
        std::fs::remove_dir(&beside).ok();
        drop((p, q));
        std::fs::remove_dir_all(&t).ok();
        let got: Vec<&str> = out.lines().collect();
        assert_eq!(got, ["own ok", "make no", "change no", "remove no", "beside no", "lock no"], "{out}");
        assert_eq!((marker.ok().as_deref(), made, beside_made), (Some("kept"), false, false));
    }

    /// F2, macOS: a run signals and looks into only its own processes. Its own child, the process group it
    /// makes (job control) and its grandchild it can signal — a test runner ends its workers, `make -j` its jobs
    /// — and its own child's details it reads (`lsof`: `process-info-pidinfo`); tarae, or another session's run
    /// (the same profile, its own `sandbox-exec`), it can neither signal (signal 0, CONT) nor look into. Each
    /// stand-in in a process group of its own, as tarae and a run are (spike M7.3: one shared with the run's
    /// passes `(target pgrp)`). Stand-ins: shells sleeping a second at a time — the run sends them only signal 0
    /// and CONT; TERM only to its own. (Projects of the test's own; Seatbelt's — Linux keeps signals in with
    /// Landlock's scope, `landlock::tests`.)
    #[cfg(target_os = "macos")]
    #[test]
    fn a_run_signals_and_looks_into_only_its_own_processes() {
        use std::os::unix::process::CommandExt;
        let test = "sandbox::tests::a_run_signals_and_looks_into_only_its_own_processes";
        if skipped_inside(test) {
            return;
        }
        // (not in a temp folder: a policy never opens one holding its project, and a checkout isn't in one)
        let t = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/sb-own-processes-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&t).ok();
        for root in ["p", "q"] {
            std::fs::create_dir_all(t.join(root)).unwrap();
        }
        let llm = crate::llm::LlmConfig::default();
        let run = |root: &str| run_policy_in(&t.join(root), &llm, &[], &Here::now()).expect("a run's policy");
        let (p, q) = (run("p"), run("q"));
        // A shell sleeping a second at a time for `$1` seconds: signalled, it leaves at most a 1 s sleep
        let sleeper = r#"i=0; while [ $i -lt "$1" ]; do sleep 1; i=$((i+1)); done"#;
        let args = |name: &str| vec!["-c".to_string(), sleeper.into(), name.into(), "60".into()];
        // tarae's stand-in, outside the sandbox, in a process group of its own
        let mut taraes = Command::new("sh");
        taraes.args(args("taraes-stand-in")).stdin(Stdio::null()).process_group(0);
        let mut taraes = taraes.spawn().unwrap();
        // Another session's run: its own policy, its own sandbox-exec, its own session (`spawn`)
        let theirs = command(Some(&q), "sh", &args("their-run"));
        let theirs = spawn(Some(&q), &theirs).unwrap();
        let script = r#"
            w() { if eval "$2" >/dev/null 2>&1; then echo "$1 ok"; else echo "$1 no"; fi; }
            looks() { if lsof -a -p "$2" -d cwd -Fn 2>/dev/null | grep -q '^n/'; then echo "$1 seen"; else echo "$1 hidden"; fi; }
            sh -c "$SLEEPER" own 20 & c=$!
            sleep 0.2
            w "own child: 0" "kill -0 $c"
            w "own child: CONT" "kill -CONT $c"
            looks "own child: details" "$c"
            w "own child: TERM" "kill -TERM $c"
            ( set -m; sleep 20 & echo $! > "$TMPDIR/group"; wait ) & sleep 0.3
            w "own group: TERM" "kill -TERM -$(cat "$TMPDIR/group")"
            ( sleep 20 & echo $! > "$TMPDIR/grandchild"; wait ) & sleep 0.3
            w "own grandchild: TERM" "kill -TERM $(cat "$TMPDIR/grandchild")"
            w "tarae: 0" "kill -0 $TARAE"
            w "tarae: CONT" "kill -CONT $TARAE"
            looks "tarae: details" "$TARAE"
            w "their run: 0" "kill -0 $THEIRS"
            w "their run: CONT" "kill -CONT $THEIRS"
            looks "their run: details" "$THEIRS"
        "#;
        let mut cmd = command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.current_dir(t.join("p")).stdin(Stdio::null()).env("SLEEPER", sleeper);
        cmd.env("TARAE", taraes.id().to_string()).env("THEIRS", theirs.pid.to_string());
        // (its denials, as its report says them)
        let w = Watch::start(p.tag.as_deref().expect("a run's tag"));
        std::thread::sleep(WATCH_READY);
        let spawned = spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        after_run(Some(&p))();
        std::thread::sleep(WATCH_LAST * 2);
        let blocked = w.blocked();
        w.finish();
        // (not deduplicated here: one line for every look into others' details is `Blocked::add`'s to make)
        let mut said: Vec<String> = blocked.seen.iter().map(|d| d.plain()).collect();
        said.sort();
        // (the stand-ins: still there — never ended by the run — then ended here)
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        let (tarae_alive, theirs_alive) = (alive(taraes.id()), alive(theirs.pid));
        taraes.kill().ok();
        taraes.wait().ok();
        // SAFETY: the other run's shell, started above and not waited on yet.
        unsafe { libc::kill(theirs.pid as libc::pid_t, libc::SIGTERM) };
        crate::spawn::wait(theirs.pid).ok();
        after_run(Some(&q))();
        drop((p, q));
        std::fs::remove_dir_all(&t).ok();
        let got: Vec<&str> = out.lines().collect();
        let want = [
            "own child: 0 ok",
            "own child: CONT ok",
            "own child: details seen",
            "own child: TERM ok",
            "own group: TERM ok",
            "own grandchild: TERM ok",
            "tarae: 0 no",
            "tarae: CONT no",
            "tarae: details hidden",
            "their run: 0 no",
            "their run: CONT no",
            "their run: details hidden",
        ];
        assert_eq!(got, want, "{out}");
        assert!(tarae_alive && theirs_alive, "the stand-ins outlived the run");
        // Its report: a signal refused names its process — a stand-in, never one of its own (by pid: the name
        // is `/bin/sh`'s); the looks into others' details are one line (`lsof -p` looks into every process:
        // hundreds of refusals, some always logged). Not every refusal reaches the log: signal 0's never, and
        // of the two CONTs one did (measured, twice) — so a signal line isn't required, only checked when there
        // (its words from Seatbelt's measured form: `seatbelt_process`'s cases in `plain`'s test)
        assert_eq!(blocked.failed, None);
        let stand_ins =
            [taraes.id(), theirs.pid].map(|pid| format!(" ({pid}) — outside the run's own processes"));
        let signals: Vec<&String> = said.iter().filter(|l| l.starts_with("signalling ")).collect();
        assert!(signals.iter().all(|l| stand_ins.iter().any(|s| l.ends_with(s))), "{said:?}");
        let looked = said.iter().filter(|l| l.starts_with("looking into "));
        assert_eq!(looked.collect::<Vec<_>>(), ["looking into processes outside the run's own"], "{said:?}");
        assert_eq!(said.len(), 1 + signals.len(), "nothing else refused: {said:?}");
    }

    /// A tarae that's gone (a crash, a kill) leaves its run folders in tarae's cache, which nothing else clears
    /// (the temp folders were the system's): the next start removes them — where tarae's cache is, as the
    /// variables say. Whose they are is the lock in each tarae's own folder (`Instance`), never a pid: one a
    /// process holds stays — another process's (a tarae running beside this one, or in a container: its pid
    /// means nothing here) or this one's own — and goes once that process lets go; a free one's goes, a live
    /// pid's too (one reused after a reboot). Removed through no link: neither one inside it nor one standing
    /// for it is followed. Left and said: a folder with no lock in it (whose can't be told), an instance's name
    /// on a link. What isn't an instance's name stays, unsaid. (A cache of the test's own; the other process is
    /// this test binary again, holding a lock until its stdin ends.)
    #[test]
    fn a_gone_taraes_run_folders_are_pruned() {
        let scratch = crate::scratch::Scratch::new("runs-prune");
        let t = scratch.to_path_buf();
        std::fs::create_dir_all(&t).unwrap();
        let t = profile_path(&t);
        let (base, outside) = (t.join("cache/tarae/runs"), t.join("outside"));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep-me"), "x").unwrap();
        // An instance's folder: its lock, a run folder in it
        let instance = |name: &str| {
            let dir = base.join(name);
            std::fs::create_dir_all(dir.join("0")).unwrap();
            std::fs::write(dir.join(INSTANCE_LOCK), "").unwrap();
            dir
        };
        // Held by another process
        let held = instance("4000001-1");
        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sandbox::tests::holds_a_lock_in_its_own_process", "--ignored", "--nocapture"])
            .env("TARAE_LOCK_HOLD", held.join(INSTANCE_LOCK))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut said = io::BufRead::lines(io::BufReader::new(holder.stdout.take().unwrap()));
        assert!(said.any(|l| l.is_ok_and(|l| l.contains(HELD))), "the other process holds its lock");
        // Gone: its lock free — a link inside, and a run folder that's a link
        let gone = instance("4000002-1");
        std::os::unix::fs::symlink(&outside, gone.join("0/link")).unwrap();
        std::os::unix::fs::symlink(&outside, gone.join("1")).unwrap();
        // A live pid's (this test's), its lock free: gone all the same
        let reused = instance(&format!("{}-1", std::process::id()));
        // This tarae's own, held here
        let mine = RunDir::new_in(&base).unwrap();
        // No lock in it; an instance's name on a link
        let no_lock = base.join("4000003-1");
        std::fs::create_dir_all(no_lock.join("0")).unwrap();
        let link = base.join("4000004-1");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // Not an instance's name
        let others = ["other", "4000005", "0-1", "4000006-x", "tarae-4000002-0"];
        for o in others {
            std::fs::create_dir_all(base.join(o)).unwrap();
        }
        let mut here = Here::now();
        here.runs = None;
        here.env.insert("XDG_CACHE_HOME".into(), t.join("cache").into());
        let sorted = |mut p: Pruned| {
            p.gone.sort();
            p.left.sort();
            p
        };
        let pruned = sorted(prune_runs_in(&here));
        let mut gone_too = vec![gone.clone(), reused.clone()];
        gone_too.sort();
        assert_eq!(pruned.gone, gone_too);
        let left: Vec<_> =
            pruned.left.iter().map(|(p, why)| (p.clone(), why.split(',').next().unwrap())).collect();
        assert_eq!(left, [(no_lock.clone(), "no lock in it"), (link.clone(), "not a folder")]);
        assert!(held.join("0").is_dir() && mine.path().is_dir(), "a held lock's folder stays");
        assert!(no_lock.join("0").is_dir() && std::fs::symlink_metadata(&link).is_ok());
        assert!(outside.join("keep-me").exists(), "gone through a link");
        for o in others {
            assert!(base.join(o).is_dir(), "{o} removed");
        }
        // The other process lets go: its folder goes
        drop(holder.stdin.take());
        assert!(holder.wait().unwrap().success());
        let pruned = sorted(prune_runs_in(&here));
        assert_eq!(pruned.gone, std::slice::from_ref(&held));
        assert_eq!(pruned.left.len(), 2, "{:?}", pruned.left);
        assert!(mine.path().is_dir(), "this tarae's own stays");
        drop(mine);
        std::fs::remove_dir_all(&t).ok();
    }

    /// What `holds_a_lock_in_its_own_process` says once it holds the lock.
    const HELD: &str = "tarae-test-lock-held";

    /// The other process of `a_gone_taraes_run_folders_are_pruned`: holds the lock at `TARAE_LOCK_HOLD`, says
    /// so (`HELD`), and lets go when its stdin ends.
    #[test]
    #[ignore = "run by a_gone_taraes_run_folders_are_pruned in a process of its own"]
    fn holds_a_lock_in_its_own_process() {
        let Some(at) = std::env::var_os("TARAE_LOCK_HOLD") else { return };
        let lock = std::fs::File::open(at).unwrap();
        lock.lock().unwrap();
        let mut out = io::stdout();
        io::Write::write_all(&mut out, format!("{HELD}\n").as_bytes()).unwrap();
        io::Write::flush(&mut out).unwrap();
        io::Read::read_to_end(&mut io::stdin(), &mut Vec::new()).unwrap();
        drop(lock);
    }

    /// A tarae makes its run folders in a folder of its own (`Instance`) whose lock it holds while it runs —
    /// named for its pid and the ms it was made in, so a gone tarae's folders never have the names a new one's
    /// would (a reused pid's after a reboot); another tarae with that pid that made its folder in the same ms (in
    /// another pid namespace) has it, and this one takes the next ms. A run's tag still starts with this
    /// tarae's pid (`ours`). A cache cleared under tarae: the next run folder makes its folder again, its lock
    /// held. (A cache of the test's own.)
    #[test]
    fn a_tarae_makes_its_run_folders_in_a_folder_it_holds() {
        let scratch = crate::scratch::Scratch::new("runs-instance");
        let t = scratch.to_path_buf();
        let base = t.join("runs");
        std::fs::create_dir_all(&base).unwrap();
        let held = |dir: &Path| lock_taken(&dir.join(INSTANCE_LOCK)).unwrap().is_none();
        let first = Instance::make(&base, 777, 5000).unwrap();
        assert_eq!(first.dir, base.join("777-5000"));
        assert!(held(&first.dir), "its lock is held");
        let same_ms = Instance::make(&base, 777, 5000).unwrap();
        assert_eq!(same_ms.dir, base.join("777-5001"), "the next ms");
        assert!(held(&same_ms.dir));
        // A gone one's folder of that name, its lock free: never taken over — its leftovers are the prune's
        let gone = base.join("778-5000");
        std::fs::create_dir_all(gone.join("0")).unwrap();
        std::fs::write(gone.join(INSTANCE_LOCK), "").unwrap();
        let after_gone = Instance::make(&base, 778, 5000).unwrap();
        assert_eq!(after_gone.dir, base.join("778-5001"));
        assert!(gone.join("0").is_dir());
        // This tarae's run folders
        let run = RunDir::new_in(&base).unwrap();
        let own = run.path().parent().unwrap().to_path_buf();
        assert_eq!(own.parent(), Some(base.as_path()));
        assert!(own.file_name().unwrap().to_string_lossy().starts_with(&format!("{}-", std::process::id())));
        assert!(held(&own), "its lock is held");
        let other = RunDir::new_in(&base).unwrap();
        assert_eq!(other.path().parent(), Some(own.as_path()), "one folder of its own for all its runs");
        assert!(ours(&run.tag()) && ours(&other.tag()) && run.tag() != other.tag(), "{}", run.tag());
        assert!(run.tag().ends_with('.'));
        // The cache cleared
        drop((run, other));
        std::fs::remove_dir_all(&base).unwrap();
        let again = RunDir::new_in(&base).unwrap();
        assert!(again.path().is_dir() && held(again.path().parent().unwrap()), "made again, held");
        drop((again, first, same_ms, after_gone));
        std::fs::remove_dir_all(&t).ok();
    }

    /// A run whose own folder can't be made doesn't start: it would have the temp folders, which every run
    /// writes, as its `TMPDIR` and tools' homes (F1), and no tag its denials are watched by. Why is said, and what
    /// to do. (A cache where no folder can be made: a file of the test's own in its way.)
    #[test]
    fn a_run_without_its_own_folder_doesnt_start() {
        let t = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/sb-no-run-folder-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&t).ok();
        std::fs::create_dir_all(t.join("p")).unwrap();
        std::fs::write(t.join("cache"), "a file where the cache's folder would be").unwrap();
        let mut here = Here::now();
        here.runs = None;
        here.env.insert("XDG_CACHE_HOME".into(), t.join("cache").into());
        let llm = crate::llm::LlmConfig::default();
        let why = run_policy_in(&t.join("p"), &llm, &[], &here).map(|_| ()).unwrap_err();
        let at = home_shown(&t.join("cache/tarae/runs").display().to_string());
        assert!(why.contains(&format!("its own folder couldn't be made in {at} (")), "why: {why}");
        assert!(why.contains(&format!("Check that {at} can be written")), "what to do: {why}");
        // No cache folder at all
        let mut nowhere = Here::now();
        nowhere.runs = None;
        nowhere.env.clear();
        let why = run_policy_in(&t.join("p"), &llm, &[], &nowhere).map(|_| ()).unwrap_err();
        assert!(why.contains("no cache folder") && why.contains("Start tarae with HOME set"), "{why}");
        // (a policy alone, for what it would allow: no folder of its own)
        assert!(policy_in(&t.join("p"), &llm, &[], &here).run.is_none());
        std::fs::remove_dir_all(&t).ok();
    }

    /// Runs' folders in a place every run may write are no run's own — one run could put a file in another's
    /// (F1): tarae's cache moved into the project or a temp folder (`XDG_CACHE_HOME`), or a write grant reaching
    /// into it, and the run doesn't start, saying where and what to do; no folder is made there. tarae's cache
    /// out of them, it starts. (Caches of the test's own.)
    #[test]
    fn runs_folders_where_every_run_writes_are_refused() {
        let t = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/sb-runs-written-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&t).ok();
        std::fs::create_dir_all(t.join("p")).unwrap();
        std::fs::create_dir_all(t.join("tmp")).unwrap();
        let start = |cache: &Path, llm: &crate::llm::LlmConfig| {
            let mut here = Here::now();
            (here.runs, here.tmp) = (None, Some(t.join("tmp")));
            here.env.insert("XDG_CACHE_HOME".into(), cache.into());
            run_policy_in(&t.join("p"), llm, &[], &here)
        };
        let plain = crate::llm::LlmConfig::default();
        let into_cache = crate::llm::LlmConfig {
            sandbox_write: vec![t.join("cache/tarae/runs/x").display().to_string()],
            ..Default::default()
        };
        for (cache, llm, written) in [
            (t.join("p/.cache"), &plain, t.join("p")),
            (t.join("tmp/cache"), &plain, t.join("tmp")),
            (t.join("cache"), &into_cache, t.join("cache/tarae/runs/x")),
        ] {
            let why = start(&cache, llm).map(|_| ()).unwrap_err();
            let (at, w) = (cache.join("tarae/runs"), home_shown(&written.display().to_string()));
            let at = home_shown(&at.display().to_string());
            assert!(why.contains(&format!("made in {at}, where every run may write ({w})")), "{why}");
            assert!(why.contains("Set XDG_CACHE_HOME for tarae to a folder no run writes"), "{why}");
            assert!(!cache.join("tarae/runs").exists(), "nothing made there: {}", cache.display());
        }
        let p = start(&t.join("cache"), &plain).expect("tarae's cache out of every write place");
        assert!(runs_written(&p, &t.join("cache/tarae/runs")).is_none());
        assert!(
            p.run.as_ref().is_some_and(|r| r.path().starts_with(t.join("cache/tarae/runs"))),
            "{:?}",
            p.run
        );
        drop(p);
        std::fs::remove_dir_all(&t).ok();
    }

    /// Computing a policy — for what it would allow (`policy_in`), or where runs write (`write_places`' way:
    /// gitescape reads it for every file) — writes nothing in the user's home: no run's folder in tarae's
    /// cache, no kept home stamped, no build folder. Only a run's way makes them (`run_policy_in`), which the
    /// same look sees. (A stand-in home: a Go module for a kept home, a checkout's own cargo build folder.)
    #[test]
    fn computing_a_policy_writes_nothing_in_the_home() {
        let t = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("target/sb-computed-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&t).ok();
        let (home, root) = (t.join("home"), t.join("home/work/p"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(t.join("tmp")).unwrap();
        std::fs::write(root.join("go.mod"), "module p\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        let env = HashMap::from([("HOME".to_string(), std::ffi::OsString::from(&home))]);
        let here = Here { env, tmp: Some(t.join("tmp")), uid: 501, os: Os::HOST, ..Here::default() };
        let own = home.join(".cargo/build/ab/cdef0123");
        let named = vec![(own.clone(), "build-dir".to_string())];
        toolchain_found_in(
            &root,
            ToolDirs { named, own_build: Some(own.clone()), ..ToolDirs::default() },
            &here,
        );
        // Every path in the home, as it is
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                out.push(e.path());
                if e.file_type().is_ok_and(|f| f.is_dir()) {
                    walk(&e.path(), out);
                }
            }
        }
        let in_home = || {
            let mut all = Vec::new();
            walk(&home, &mut all);
            all.sort();
            all
        };
        let before = in_home();
        let llm = crate::llm::LlmConfig::default();
        let p = policy_in(&root, &llm, &[], &here);
        let places = Policy::for_project(&env_in(&root, &llm, &[], &here).0).write_places();
        assert!(p.run.is_none() && p.tag.is_none(), "no run's folder: {:?}", p.run);
        assert!(p.write.contains(&own) && !p.kept.is_empty(), "what it would open: {:?}", p.write);
        assert!(places.iter().any(|w| w.starts_with(home.join(".cache/tarae/runs"))), "{places:?}");
        assert_eq!(in_home(), before, "computing wrote in the home");
        // A run's way makes them, and the look above sees it
        let run = run_policy_in(&root, &llm, &[], &here).expect("a run's policy");
        let made = in_home();
        let runs = run.run.as_ref().expect("its own folder").path().to_path_buf();
        assert!(runs.starts_with(home.join(".cache/tarae/runs")) && made.contains(&runs), "{runs:?}");
        assert!(made.contains(&own), "its build folder made: {made:?}");
        drop((run, p));
        std::fs::remove_dir_all(&t).ok();
    }

    /// The checkout's own build folder is made from its tool's home a folder at a time through no link: a link
    /// where one of them goes (one sending it elsewhere) stops it, and stays — the home's folders are the user's;
    /// a new one is marked as its pack says the tool marks one (a tool that marks none: nothing on it or in it);
    /// one there already is left as it is. One that can't be marked
    /// isn't kept: the tool never marks a folder there already, so it would stay unmarked for good — taken
    /// back with the folders made on the way to it (not one there before, nor one another run wrote in
    /// meanwhile — said), the next run makes it again. (Folders of the test's own.)
    #[test]
    fn the_own_build_folder_is_made_through_no_link() {
        let scratch = crate::scratch::Scratch::new("own-build");
        let t = scratch.to_path_buf();
        std::fs::create_dir_all(t.join("cargo/build")).unwrap();
        std::fs::create_dir_all(t.join("elsewhere")).unwrap();
        let t = std::fs::canonicalize(&t).unwrap();
        let home = t.join("cargo");
        let tool = Builder {
            who: "cargo".into(),
            home: Some(home.clone()),
            own_under: Some(("build".into(), 2)),
            // (marks of the test's own: whatever the pack says is what's put)
            marks: crate::builds::Marks {
                cache_tag: Some("# made by the test.\n".into()),
                macos_xattrs: vec![("com.tarae.test-mark".into(), b"x\0y".to_vec())],
            },
        };
        let own = home.join("build/ab/cd1234");
        assert_eq!(own_build_made(&own, &tool), Ok(true));
        let tag = std::fs::read_to_string(own.join("CACHEDIR.TAG")).unwrap();
        assert_eq!(tag, "Signature: 8a477f597d28d172789f06886806bc55\n# made by the test.\n");
        #[cfg(target_os = "macos")]
        assert_eq!(xattr_of(&own, "com.tarae.test-mark"), Some(b"x\0y".to_vec()));
        // A tool that tags none and marks nothing: made, nothing in it, no mark on it
        let plain = Builder { marks: Default::default(), ..tool.clone() };
        let bare = home.join("build/ab/9999");
        assert_eq!(own_build_made(&bare, &plain), Ok(true));
        assert_eq!(std::fs::read_dir(&bare).unwrap().count(), 0, "a file made in it");
        #[cfg(target_os = "macos")]
        assert_eq!(xattr_of(&bare, "com.tarae.test-mark"), None);
        std::fs::remove_file(own.join("CACHEDIR.TAG")).unwrap();
        assert_eq!(own_build_made(&own, &tool), Ok(false), "there already");
        assert!(!own.join("CACHEDIR.TAG").exists(), "left as it is");
        std::os::unix::fs::symlink(t.join("elsewhere"), home.join("build/ef")).unwrap();
        let made = own_build_made(&home.join("build/ef/0123ab"), &tool);
        assert!(made.as_ref().is_err_and(|e| e.contains("a link or no folder there")), "{made:?}");
        assert_eq!(
            std::fs::read_dir(t.join("elsewhere")).unwrap().count(),
            0,
            "nothing made through the link"
        );
        assert!(home.join("build/ef").is_symlink(), "the link stays");
        // Marking fails partway (the tag written, then an attribute refused) under a hash prefix new too:
        // nothing of it kept — the folder and the prefix made on the way to it
        let next = home.join("build/12/5678ef");
        let fails = |dir: &Unlinked| {
            dir.write_file("CACHEDIR.TAG", b"Signature: 8a477f597d28d172789f06886806bc55\n")?;
            Err("an attribute refused".to_string())
        };
        let made = own_build_made_with(&next, &tool, &fails);
        assert_eq!(made, Err("an attribute refused — not kept, unmarked".into()));
        assert!(!home.join("build/12").exists(), "a folder made on the way left");
        assert!(home.join("build").is_dir(), "a folder there before taken back");
        assert_eq!(own_build_made(&next, &tool), Ok(true), "made again, marked");
        assert!(next.join("CACHEDIR.TAG").is_file());
        // … under a prefix there before: the folder taken back, the prefix stays
        let beside = home.join("build/ab/5678ef");
        assert_eq!(own_build_made_with(&beside, &tool, &fails), made);
        assert!(!beside.exists() && home.join("build/ab").is_dir());
        // … and under a new prefix another run wrote in meanwhile: the folder taken back, the prefix stays, said
        let theirs = home.join("build/34/other");
        let fails_beside = |dir: &Unlinked| {
            std::fs::create_dir(&theirs).unwrap();
            fails(dir)
        };
        let made = own_build_made_with(&home.join("build/34/5678ef"), &tool, &fails_beside);
        assert!(
            made.as_ref().is_err_and(|e| e.contains("build/34 stays: something else is in it")),
            "{made:?}"
        );
        assert!(!home.join("build/34/5678ef").exists() && theirs.is_dir());
        std::fs::remove_dir_all(&t).ok();
    }

    /// tarae makes the checkout's own build folder outside the sandbox, so only where the policy opens it to
    /// the run: a build-folder setting of yours naming a folder that can't be a checkout's own — beside cargo's
    /// crates, too shallow or too deep under `build`, outside cargo's home — makes nothing there, and is said as
    /// kept closed. The one that may be is made, marked (the same setup: the cases differ only in the folder).
    /// (A cargo home and a checkout of the test's own under `target/`; no cargo run — what it names is given.)
    #[test]
    fn a_build_folder_the_policy_refuses_is_never_made() {
        let base = profile_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("target/sb-own-refused-{}", std::process::id())),
        );
        std::fs::remove_dir_all(&base).ok();
        let (home, root) = (base.join("home"), base.join("home/work/p"));
        let cargo = home.join(".cargo");
        std::fs::create_dir_all(cargo.join("build")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        let here = Here { env: [("HOME".to_string(), home.clone().into())].into(), ..Here::default() };
        let llm = crate::llm::LlmConfig::default();
        let run_with = |own: &Path| {
            let named = vec![(own.to_path_buf(), "build-dir".to_string())];
            let found = ToolDirs { named, own_build: Some(own.to_path_buf()), ..ToolDirs::default() };
            toolchain_found_in(&root, found, &here);
            run_policy_in(&root, &llm, &[], &here).unwrap()
        };
        for bad in [
            cargo.join("registry/ab/cd1234"),
            cargo.join("build/ab"),
            cargo.join("build/ab/cd1234/ef"),
            home.join("elsewhere/ab/cd1234"),
        ] {
            let p = run_with(&bad);
            assert!(!p.write.contains(&bad), "{} opened: {:?}", bad.display(), p.write);
            assert!(p.refused.iter().any(|(r, _)| *r == bad), "{} not said: {:?}", bad.display(), p.refused);
            assert!(!bad.exists(), "{} made", bad.display());
        }
        let good = cargo.join("build/ab/cd1234");
        let p = run_with(&good);
        assert!(p.write.contains(&good), "{:?} · {:?}", p.write, p.refused);
        assert!(good.join("CACHEDIR.TAG").is_file(), "its own not made");
        std::fs::remove_dir_all(&base).ok();
    }

    /// An extended attribute of folder `d` (not through a link).
    #[cfg(target_os = "macos")]
    fn xattr_of(d: &Path, name: &str) -> Option<Vec<u8>> {
        let (p, n) = (c_name(d.as_os_str()).unwrap(), c_name(name.as_ref()).unwrap());
        let mut buf = vec![0u8; 4096];
        // SAFETY: two C strings and a buffer of its length, live for the call.
        let got = unsafe {
            libc::getxattr(
                p.as_ptr(),
                n.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        buf.truncate(usize::try_from(got).ok()?);
        Some(buf)
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
            ..Builder::default()
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
        let scratch = crate::scratch::Scratch::in_dir(&tmp, "daemon-home");
        let base = scratch.to_path_buf();
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
            run_policy_in(root, &llm, &[], &here(os)).expect("a run's policy")
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
        let scratch = crate::scratch::Scratch::in_dir(&tmp, "append");
        let base = scratch.to_path_buf();
        std::fs::create_dir_all(base.join("home/work/g")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (home, g) = (base.join("home"), base.join("home/work/g"));
        std::fs::write(g.join("settings.gradle.kts"), "").unwrap();
        let opts_in_run = |opts: &str, os: Os| {
            let env = [("HOME", home.clone().into()), ("GRADLE_OPTS", opts.into())];
            let env = env.map(|(k, v): (&str, std::ffi::OsString)| (k.to_string(), v)).into();
            let here = Here { env, os, ..Here::default() };
            toolchain_found_in(&g, ToolDirs::default(), &here);
            let p = run_policy_in(&g, &crate::llm::LlmConfig::default(), &[], &here).expect("a run's policy");
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
        let scratch = crate::scratch::Scratch::in_dir(&tmp, "gocache");
        let base = scratch.to_path_buf();
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
            run_policy_in(&root, &llm, &[home.join("Library/Caches")], &here).expect("a run's policy")
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
        // (the open network goes out through tarae's proxy: every connect but the opened ones denied — step 5d)
        assert!(text.contains("(deny network*)"), "{text}");
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
            ("/dev/ttys012", false),
        ] {
            assert_eq!(p.reads_allow(Path::new(path)), read, "{path}");
        }
        // (the terminals: the project's grant opens them, to the report's rules too)
        let granted = Policy { terminals: true, ..p.clone() };
        assert!(
            granted.reads_allow(Path::new("/dev/ttys012")) && granted.writes_allow(Path::new("/dev/ttys012"))
        );
        assert!(!p.writes_allow(Path::new("/dev/ttys012")));
        let none = Policy::for_project(&Env { reads: Reads::default(), ..env(Path::new("/Users/u/p")) });
        let text = none.profile();
        // (but the project's own rule: files named like keys in it — `files_named_like_keys_are_closed_…`)
        let keys = "(deny file-read-data file-read-xattr (require-all (subpath (param \"ROOT\"))";
        let reads: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("file-read") && !l.contains(TERMINALS) && !l.starts_with(keys))
            .collect();
        assert!(reads.is_empty() && !text.contains("HOME"), "{text}");
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

    /// A quiet run's report leaves out the rules Claude was told (`known`: their digest); one that says
    /// something was blocked says them all the same; another policy's rules aren't the ones known.
    #[test]
    fn a_quiet_report_leaves_out_the_rules_already_told() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let quiet = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("rules-once.")), ..p.clone() }));
        let rules = quiet.rules();
        let first = quiet.report_with(&[], Told::default());
        assert!(first.text.contains(&rules) && !first.blocked, "{}", first.text);
        let known = |r: &Report| Told { rules: Some(r.rules), guarantees: None };
        let again = quiet.report_with(&[], known(&first));
        assert!(!again.text.contains(&rules), "{}", again.text);
        assert!(again.text.starts_with("The sandbox blocked nothing."), "{}", again.text);
        let w = Watch::quiet("rules-once-blocked.");
        w.record(Denial {
            process: "sh".into(),
            op: "file-write-create".into(),
            target: "/Users/u/elsewhere/x".into(),
        });
        let blocked =
            Guard::In(Box::new(Policy { watch: Some(w), ..p.clone() })).report_with(&[], known(&first));
        assert!(blocked.blocked && blocked.text.contains(&rules), "{}", blocked.text);
        // Nothing watched: a refusal would show only in the output — the rules always come with it
        let unwatched = Guard::In(Box::new(Policy { watch: None, ..p.clone() }));
        let told = unwatched.report_with(&[], known(&unwatched.report_with(&[], Told::default())));
        assert!(!told.blocked && told.text.contains(&unwatched.rules()), "{}", told.text);
        let open = Guard::In(Box::new(Policy { localhost: !p.localhost, ..p }));
        assert_ne!(open.report_with(&[], Told::default()).rules, first.rules);
    }

    /// 9b: the guarantees' paragraph comes after the rules on every path a report takes — quiet, blocked, nothing
    /// watched (the rules always said), couldn't watch (after the lines that may be the sandbox, which stay with the
    /// rules) — and on none once told.
    #[test]
    fn the_guarantees_follow_the_rules_on_every_path() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let g = guarantees::Guarantees::seatbelt(true, false);
        let told = g.told();
        let blocked = Watch::quiet("apart-blocked.");
        blocked.record(denial("file-write-create", "/Users/u/elsewhere/x"));
        let failed = Watch::quiet("apart-failed.");
        failed.fail("log: Cannot run while sandboxed".into());
        let out = [(true, "touch: /Users/u/p/x: Operation not permitted".to_string())];
        let paths = [
            ("quiet", Some(Watch::quiet("apart-quiet.")), &[][..]),
            ("blocked", Some(blocked), &[][..]),
            ("nothing watched", None, &[][..]),
            ("couldn't watch", Some(failed), &out[..]),
        ];
        for (path, watch, output) in paths {
            let guard = Guard::In(Box::new(Policy { watch, ..p.clone() }));
            let rules = guard.rules();
            let first = guard.report_as(output, Told::default(), Some(g.clone()));
            let (at_rules, at_told) = (first.text.find(&rules), first.text.find(&told));
            assert!(at_rules.is_some() && at_told > at_rules, "{path}: {}", first.text);
            if path == "couldn't watch" {
                let lines = first.text.find("Lines that may be the sandbox");
                assert!(lines > at_rules && at_told > lines, "{path}: {}", first.text);
            }
            let known = Told { rules: None, guarantees: first.guarantees };
            let again = guard.report_as(output, known, Some(g.clone()));
            assert!(again.text.contains(&rules) && !again.text.contains(&told), "{path}: {}", again.text);
        }
    }

    /// A policy made for Linux whose Landlock rules weren't made — not yet, or they failed (its run never starts) —
    /// has no guarantees to say, never Seatbelt's; one made for macOS has Seatbelt's, on either system.
    #[test]
    fn a_linux_policy_never_says_seatbelts_words() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let mac = Policy { supervised: false, ..p.clone() };
        let linux = Policy { supervised: true, ..p };
        assert_eq!(mac.guarantees(), Some(seatbelt_guarantees(&mac)));
        assert_eq!(linux.guarantees(), None, "its rules not made yet");
        #[cfg(target_os = "linux")]
        {
            let failed = Policy { confine: Some(Err("the sandbox couldn't be made: x".into())), ..linux };
            assert_eq!(failed.guarantees(), None, "its rules failed");
            let r = Guard::In(Box::new(failed)).report(&[]);
            assert!(!r.contains("Seatbelt") && !r.contains("macOS's sandbox"), "{r}");
        }
    }

    /// Files named like keys in the project (7a, names from the data — `secretnames`): the profile denies their
    /// reads below the root, after the allows, by name in any case and only by parameters — but its exceptions
    /// (a `.env`'s template, a public key), what's below tools' output and the file the user opened; the rule
    /// in Rust (`reads_allow`) agrees; the report names the file and how it opens. Every policy closes them, on
    /// both systems (7b: Linux's rules — `linux::tests::keys_are_found_where_they_are`); one that doesn't (`keys`
    /// None, a test's) says nothing of it.
    #[test]
    fn files_named_like_keys_are_closed_but_the_exceptions() {
        let root = Path::new("/Users/u/p");
        let mac = Policy::for_project(&env(root));
        assert_eq!(mac.keys, Some(Vec::new()), "closed, whatever the system");
        let text = mac.profile();
        let deny = text.lines().find(|l| l.contains("(deny file-read-data file-read-xattr (require-all"));
        let deny = deny.unwrap_or_else(|| panic!("{text}"));
        assert!(deny.contains("\\.[eE][nN][vV]") && deny.contains("[^/]*\\.[pP][eE][mM]"), "{deny}");
        assert!(deny.contains("(require-not (require-any") && deny.contains("node_modules|"), "{deny}");
        assert!(deny.contains("[^/]*\\.[pP][uU][bB])$"), "the exceptions: {deny}");
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s}: {text}"));
        assert!(at(deny) > at("(allow file-read-data file-read-xattr (subpath (param \"ROOT\"))"), "{text}");
        assert!(!text.contains("/Users/u") && !text.contains("Y0"), "paths only as parameters: {text}");
        let reads = |p: &Policy, f: &str| p.reads_allow(&root.join(f));
        for f in [".env", ".env.local", "a/b/Server.PEM", "deep/id_ED25519"] {
            assert!(!reads(&mac, f), "{f}");
        }
        for f in [".env.example", "a/.ENV.sample", "id_rsa.pub", "node_modules/x/test.pem", "src/env.rs"] {
            assert!(reads(&mac, f), "{f}");
        }
        // The file the user opened, that one file: after the deny, by a parameter
        let env_file = root.join(".env");
        let granted = Policy { keys: Some(vec![env_file.clone()]), ..mac.clone() };
        let text = granted.profile();
        let open = "(allow file-read-data file-read-xattr (literal (param \"Y0\")))";
        assert!(text.find(open).is_some_and(|o| o > text.find(deny).unwrap()), "{text}");
        assert!(granted.params().contains(&("Y0".into(), env_file.display().to_string())));
        assert!(reads(&granted, ".env") && !reads(&granted, ".env.local"));
        // The report: the file and how it opens — not where it's open, and nothing where they aren't closed
        let refused =
            [Denial { process: "cat".into(), op: "file-read-data".into(), target: "/Users/u/p/.env".into() }];
        let said = |p: &Policy| Guard::In(Box::new(p.clone())).kept(&refused).0;
        let line = ".env: named like a key, so your commands can't read it — a key or a token may be in it. If a \
                    command needs it, tell the user: the run's panel (space m s) lets your commands read that file.";
        assert!(said(&mac).contains(line), "{}", said(&mac));
        assert!(!said(&granted).contains("named like a key"), "{}", said(&granted));
        let open = Policy { keys: None, ..mac.clone() };
        assert!(reads(&open, ".env") && !said(&open).contains("named like a key"));
        assert!(!open.profile().contains("[pP][eE][mM]"));
        let summary = |p: &Policy| Guard::In(Box::new(p.clone())).report(&[]);
        assert!(summary(&mac).contains("In the project it can't read a file named like a key or a token"));
        assert!(!summary(&open).contains("named like a key"));
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
        let scratch = crate::scratch::Scratch::new("sb-home");
        let home = profile_path(&scratch);
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

    /// Where the sandbox refuses a v4-mapped loopback (macOS below 27), a run's JVMs are IPv4-only: the Java
    /// pack's `JAVA_TOOL_OPTIONS`, tarae's value alone — the user's own (an agent here) never reaches a run, set
    /// there or not; elsewhere (27, Linux) a run has none. The fact as this Mac says it.
    #[test]
    fn runs_jvms_are_ipv4_only_where_the_sandbox_refuses_a_mapped_loopback() {
        let scratch = crate::scratch::Scratch::new("sb-jvm-v4");
        let base = profile_path(&scratch);
        let (home, root) = (base.join("home"), base.join("home/work/j"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("build.gradle"), "").unwrap();
        let options = |os: Os, refused: bool| {
            // (the user's own options variables, each loading an agent: none reaches a run — the launcher's
            // `JDK_JAVA_OPTIONS` and HotSpot's `_JAVA_OPTIONS` would also undo tarae's v4 value)
            let env = [
                ("HOME", home.clone().into()),
                ("JAVA_TOOL_OPTIONS", "-javaagent:/x/agent.jar".into()),
                ("JDK_JAVA_OPTIONS", "-javaagent:/x/a.jar".into()),
                ("_JAVA_OPTIONS", "-javaagent:/x/b.jar".into()),
            ];
            let here = Here {
                env: env.map(|(k, v): (&str, std::ffi::OsString)| (k.to_string(), v)).into(),
                os,
                v4_mapped_refused: refused,
                ..Here::default()
            };
            toolchain_found_in(&root, ToolDirs::default(), &here);
            let p =
                run_policy_in(&root, &crate::llm::LlmConfig::default(), &[], &here).expect("a run's policy");
            let c = command(Some(&p), "java", &[]);
            for other in ["JDK_JAVA_OPTIONS", "_JAVA_OPTIONS"] {
                let given = c.get_envs().any(|(k, v)| k == other && v.is_some());
                assert!(!given, "{os:?}, v4-mapped refused {refused}: the user's {other} reached the run");
            }
            let set = c.get_envs().filter(|(k, _)| *k == "JAVA_TOOL_OPTIONS").filter_map(|(_, v)| v);
            set.map(|v| v.to_string_lossy().into_owned()).collect::<Vec<_>>()
        };
        assert_eq!(options(Os::Mac, true), ["-Djava.net.preferIPv4Stack=true"], "macOS 26: tarae's, alone");
        assert_eq!(options(Os::Mac, false), Vec::<String>::new(), "macOS 27: none, nor the user's");
        assert_eq!(options(Os::Linux, false), Vec::<String>::new(), "Linux: none, nor the user's");
        assert!(
            !crate::readonly::env_allowed("JAVA_TOOL_OPTIONS"),
            "no command line of Claude's sets it unasked"
        );
        // (the other two: no pattern carries them, no pack sets them)
        for n in ["JDK_JAVA_OPTIONS", "_JAVA_OPTIONS"] {
            assert!(!crate::readonly::env_core(n) && !crate::readonly::env_allowed(n), "{n}");
        }
        #[cfg(target_os = "macos")]
        {
            let major = macos_major().expect("this Mac's version");
            assert!(major >= 11, "{major}");
            assert_eq!(Here::now().v4_mapped_refused, major < 27);
        }
        #[cfg(not(target_os = "macos"))]
        assert!(!Here::now().v4_mapped_refused);
    }

    /// The JDK `JAVA_HOME` names in home (sdkman's, asdf's, mise's, a CI tool cache's — the Java pack's `jdk`
    /// home) is read whole, by its real path, and never written; home itself, a kept place, or a folder that isn't
    /// there isn't read; a kept place inside it stays closed. (A stand-in home in the test's own folder.)
    #[test]
    fn the_jdk_java_home_names_is_read_within_bounds() {
        let scratch = crate::scratch::Scratch::new("sb-jdk");
        let home = profile_path(&scratch);
        let jdk = home.join(".sdkman/candidates/java/21.0.4-tem");
        for d in [jdk.join("lib"), jdk.join("bin"), home.join(".ssh"), home.join("Library/Keychains")] {
            std::fs::create_dir_all(d).unwrap();
        }
        // (sdkman's `current`: a link to the version in use)
        let current = home.join(".sdkman/candidates/java/current");
        std::os::unix::fs::symlink(&jdk, &current).unwrap();
        let with = |java_home: &Path| {
            let env = HashMap::from([("JAVA_HOME".to_string(), java_home.into())]);
            let vars = vars_in(&home, &Here { env, ..Here::default() });
            (reads_for(&home, &[], &vars), RunsLater::new(&home, &vars, &[]))
        };
        let (r, later) = with(&current);
        assert!(r.allow.contains(&jdk), "its real path: {:?}", r.allow);
        let policy = Policy::for_project(&Env { root: home.join("proj"), reads: r, ..Env::default() });
        for f in ["lib/libjli.dylib", "bin/java"] {
            assert!(policy.reads_allow(&jdk.join(f)), "{f}");
        }
        assert!(later.refuses(&jdk).is_some_and(|w| w.contains("toolchain's home")), "never written");
        for (java_home, why) in [
            (home.clone(), "home itself"),
            (home.join(".ssh"), "a kept place"),
            (home.join("jdk-never-made"), "not there"),
        ] {
            let (r, _) = with(&java_home);
            assert!(!r.allow.contains(&java_home), "{why}: {:?}", r.allow);
        }
        // (a JAVA_HOME holding a kept place: what's kept stays closed in it)
        let (r, _) = with(&home.join("Library"));
        let policy = Policy::for_project(&Env { root: home.join("proj"), reads: r, ..Env::default() });
        assert!(!policy.reads_allow(&home.join("Library/Keychains/login.keychain-db")));
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
        let scratch = crate::scratch::Scratch::new("sb-gradle");
        let t = profile_path(&scratch);
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
        let scratch = crate::scratch::Scratch::new("sb-unlinked");
        let t = profile_path(&scratch);
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
            let env = run_homes_made(&run, &vars, &Default::default(), &[k], None);
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

    /// A proxy URL as a run gets it: its own port and token (`per_run` — macOS's, Linux C's after step 6), or the
    /// supervisor's address with no credential (Linux's today). Fake tokens only.
    fn stand_in_url(per_run: bool) -> proxy::ProxyUrl {
        proxy::ProxyUrl {
            host: if per_run { "127.0.0.1" } else { "192.0.2.1" }.into(),
            port: if per_run { 40001 } else { 3128 },
            user: per_run.then(|| "run".into()),
            password: per_run.then(|| "tarae-fake-token-5".into()),
            per_run,
        }
    }

    /// What the built-in packs give a run with `url` (or none): its run homes made in `run` — `kept` standing in
    /// where a kept home may — then `[sandbox.proxy]`'s; the user's Gradle home at `t/user/.gradle`.
    fn proxied(t: &Path, run: &Path, kept: &[Kept], url: Option<&proxy::ProxyUrl>) -> Vec<(String, String)> {
        let env = HashMap::from([("GRADLE_USER_HOME".to_string(), t.join("user/.gradle").into())]);
        let vars = vars_in(&t.join("no-home"), &Here { env, ..Here::default() });
        let mut env = run_homes_made(run, &vars, &Default::default(), kept, url);
        proxy_made(Some(run), url, &mut env, &Default::default());
        env
    }

    fn gradle_kept(home: &Path) -> Kept {
        Kept { name: "gradle".into(), home: home.into(), daemon: true, held: Default::default() }
    }

    /// The JVM reads no HTTPS_PROXY (step 5f, M2): the Java pack's lines are written with this run's own values —
    /// Gradle's in the run's Gradle home, Maven's in a settings file of the run's folder that `MAVEN_ARGS` names
    /// (`-s`, the user's decision 2026-10-07) — 0600, no `{proxy.…}` left; where the URL has no user or password
    /// (Linux's supervised runs) their lines are left out. And Node is told to read the proxy variables.
    #[test]
    fn the_java_pack_writes_its_proxy_lines() {
        let scratch = crate::scratch::Scratch::new("sb-proxy-lines");
        let t = profile_path(&scratch);
        let run = t.join("run");
        std::fs::create_dir_all(&run).unwrap();
        let env = proxied(&t, &run, &[], Some(&stand_in_url(true)));
        let get =
            |env: &[(String, String)], k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let gradle = std::fs::read_to_string(run.join("gradle/gradle.properties")).unwrap();
        for line in [
            "systemProp.https.proxyHost=127.0.0.1",
            "systemProp.https.proxyPort=40001",
            "systemProp.https.proxyUser=run",
            "systemProp.https.proxyPassword=tarae-fake-token-5",
            "systemProp.http.proxyPassword=tarae-fake-token-5",
            "systemProp.jdk.http.auth.tunneling.disabledSchemes=",
        ] {
            assert!(gradle.lines().any(|l| l == line), "{line}:\n{gradle}");
        }
        assert!(!gradle.contains("{proxy."), "{gradle}");
        let mode =
            |p: &Path| std::os::unix::fs::PermissionsExt::mode(&p.metadata().unwrap().permissions()) & 0o777;
        assert_eq!(mode(&run.join("gradle/gradle.properties")), 0o600);
        let maven = run.join("maven-settings.xml");
        let xml = std::fs::read_to_string(&maven).unwrap();
        assert!(
            xml.contains("<port>40001</port>") && xml.contains("<password>tarae-fake-token-5</password>"),
            "{xml}"
        );
        assert_eq!(mode(&maven), 0o600);
        assert_eq!(get(&env, "MAVEN_ARGS"), Some(format!("-s {}", maven.display())));
        assert_eq!(get(&env, "NODE_USE_ENV_PROXY").as_deref(), Some("1"));
        // No user, no password: their lines left out, the rest written
        let run2 = t.join("run2");
        std::fs::create_dir_all(&run2).unwrap();
        proxied(&t, &run2, &[], Some(&stand_in_url(false)));
        let gradle = std::fs::read_to_string(run2.join("gradle/gradle.properties")).unwrap();
        assert!(
            gradle.contains("systemProp.https.proxyHost=192.0.2.1\n") && gradle.contains("proxyPort=3128\n")
        );
        assert!(!gradle.contains("proxyUser") && !gradle.contains("proxyPassword"), "{gradle}");
        let xml = std::fs::read_to_string(run2.join("maven-settings.xml")).unwrap();
        assert!(!xml.contains("<username>") && !xml.contains("<password>"), "{xml}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A run with no proxy URL (the network closed — and after step 6, a session whose network redirects) gets
    /// none of the packs' proxy lines: no file, no variable — and a kept home holding a file from an earlier
    /// run with one loses it (the file is tarae's whole; Gradle would read a proxy that isn't there).
    #[test]
    fn a_run_without_a_proxy_gets_no_proxy_lines() {
        let scratch = crate::scratch::Scratch::new("sb-no-proxy");
        let t = profile_path(&scratch);
        let (run, kept) = (t.join("run"), t.join("kept"));
        for d in [&run, &kept] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(kept.join("gradle.properties"), "systemProp.https.proxyPort=40001\n").unwrap();
        let env = proxied(&t, &run, &[gradle_kept(&kept)], None);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!names.contains(&"NODE_USE_ENV_PROXY") && !names.contains(&"MAVEN_ARGS"), "{names:?}");
        assert!(!kept.join("gradle.properties").exists(), "an earlier run's file stays");
        assert!(!run.join("maven-settings.xml").exists() && !run.join("gradle/gradle.properties").exists());
        std::fs::remove_dir_all(&t).ok();
    }

    /// A run whose proxy URL is its own alone (a port and a token of its own) never gets a kept home that has
    /// proxy lines (step 5f, the user's decision 2026-10-07): another run reads that home, and its daemon serves
    /// later runs — the run's own home holds its lines, the kept one is left untouched. A URL the same for every
    /// run keeps the kept home, its lines written there.
    #[test]
    fn per_run_values_never_go_in_a_shared_home() {
        let scratch = crate::scratch::Scratch::new("sb-per-run");
        let t = profile_path(&scratch);
        let (run, kept) = (t.join("run"), t.join("kept"));
        for d in [&run, &kept] {
            std::fs::create_dir_all(d).unwrap();
        }
        let home = |env: &[(String, String)]| {
            env.iter().find(|(k, _)| k == "GRADLE_USER_HOME").map(|(_, v)| PathBuf::from(v))
        };
        let env = proxied(&t, &run, &[gradle_kept(&kept)], Some(&stand_in_url(true)));
        assert_eq!(home(&env), Some(run.join("gradle")), "the run's own");
        let own = std::fs::read_to_string(run.join("gradle/gradle.properties")).unwrap();
        assert!(own.contains("tarae-fake-token-5"), "{own}");
        assert_eq!(std::fs::read_dir(&kept).unwrap().count(), 0, "the kept home untouched");
        let env = proxied(&t, &t.join("run2"), &[gradle_kept(&kept)], Some(&stand_in_url(false)));
        assert_eq!(home(&env), Some(kept.clone()), "the same for every run: kept");
        assert!(
            std::fs::read_to_string(kept.join("gradle.properties")).unwrap().contains("proxyPort=3128\n")
        );
        std::fs::remove_dir_all(&t).ok();
    }

    /// A kept home is a folder earlier runs wrote in, and tarae isn't sandboxed: its proxy file goes in through
    /// no link a run left there — a link planted where the file goes is replaced by the file, and what it pointed
    /// at (a marker in the test's own folder) stays as it was.
    #[test]
    fn a_proxy_file_is_written_through_no_link() {
        let scratch = crate::scratch::Scratch::new("sb-proxy-link");
        let t = profile_path(&scratch);
        let (run, kept) = (t.join("run"), t.join("kept"));
        for d in [&run, &kept] {
            std::fs::create_dir_all(d).unwrap();
        }
        let marker = t.join("marker");
        std::fs::write(&marker, "the marker").unwrap();
        std::os::unix::fs::symlink(&marker, kept.join("gradle.properties")).unwrap();
        proxied(&t, &run, &[gradle_kept(&kept)], Some(&stand_in_url(false)));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "the marker", "written through the link");
        let file = kept.join("gradle.properties");
        assert!(file.symlink_metadata().unwrap().file_type().is_file(), "the link replaced by the file");
        assert!(std::fs::read_to_string(&file).unwrap().contains("proxyPort=3128\n"));
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

    /// A policy made for each system (`policy_in`, a stand-in `Here` and a project in the test's own temp
    /// folder), with the network as `network` says.
    fn made_for(os: Os, network: bool, name: &str) -> (Policy, crate::scratch::Scratch) {
        let t = crate::scratch::Scratch::new(&format!("sb-{name}"));
        std::fs::create_dir_all(t.join("p")).unwrap();
        let here = Here { tmp: Some(profile_path(&t)), uid: 501, os, ..Here::default() };
        let llm = crate::llm::LlmConfig { sandbox_network: network, ..Default::default() };
        (policy_in(&t.join("p"), &llm, &[], &here), t)
    }

    /// Step 5g: the proxy tarae itself goes out by is read from tarae's environment for tarae's proxy alone — a run
    /// never gets it. With the network open the run's proxy variables name tarae's own proxy, and so do the packs'
    /// proxy files; closed, none at all. The upstream's host and credentials are nowhere in the run's environment
    /// or its files, on either system's policy.
    #[test]
    fn runs_never_see_the_upstream() {
        fn files(dir: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                match e.file_type().is_ok_and(|t| t.is_dir()) {
                    true => files(&e.path(), out),
                    false => out.extend(std::fs::read_to_string(e.path())),
                }
            }
        }
        // (one with credentials — left out as token-like anyway — and one without: its host alone mustn't pass)
        let (user, bare) =
            ("http://u:tarae-fake-token-1@up.example.invalid:3128", "http://up.example.invalid:3128");
        for (os, network) in [(Os::Mac, true), (Os::Linux, true), (Os::Mac, false), (Os::Linux, false)] {
            let t = crate::scratch::Scratch::new(&format!("sb-upstream-{os:?}-{network}")).into_path();
            std::fs::create_dir_all(t.join("p")).unwrap();
            std::fs::create_dir_all(t.join("run")).unwrap();
            let vars =
                [("HTTPS_PROXY", user), ("http_proxy", bare), ("ALL_PROXY", user), ("PATH", "/usr/bin:/bin")];
            let env = vars.iter().map(|(k, v)| (k.to_string(), std::ffi::OsString::from(v))).collect();
            let here = Here { env, tmp: Some(t.clone()), uid: 501, os, ..Here::default() };
            let llm = crate::llm::LlmConfig { sandbox_network: network, ..Default::default() };
            let p = policy_in(&t.join("p"), &llm, &[], &here);
            assert_eq!(
                p.egress.upstream.through(),
                network,
                "{os:?}: the proxy's own, from tarae's environment"
            );
            let cmd = command(Some(&p), "true", &[]);
            let https = cmd.get_envs().find(|(k, _)| *k == "HTTPS_PROXY").and_then(|(_, v)| v?.to_str());
            assert_eq!(https.map(str::to_string), p.proxy_url().map(|u| u.url()), "{os:?} {network}");
            let mut seen: Vec<String> = cmd.get_envs().map(|(k, v)| format!("{k:?}={v:?}")).collect();
            let packs = proxied(&t, &t.join("run"), &[], p.proxy_url().as_ref());
            seen.extend(packs.into_iter().map(|(k, v)| format!("{k}={v}")));
            files(&t.join("run"), &mut seen);
            assert!(network == seen.iter().any(|s| s.contains("proxyHost")), "{os:?}: the Java pack's lines");
            for s in &seen {
                assert!(
                    !s.contains("up.example.invalid") && !s.contains("tarae-fake-token-1"),
                    "{os:?}: {s}"
                );
            }
            std::fs::remove_dir_all(&t).ok();
        }
    }

    /// Step 5g (macOS; Linux's in `linux::tests::the_report_says_how_linux_differs`): where tarae itself goes out
    /// through a proxy, the run's report says plainly what that changes — its names go to that proxy unresolved, so
    /// the address checks don't hold for them (only the metadata servers' own names are refused, a partial check),
    /// and NO_PROXY's hosts go direct, checked — naming the variables, never where that proxy is nor its
    /// credentials. Without one, the line as it was; one tarae can't chain to: nothing goes that way, and the
    /// setting that sends runs direct.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_report_says_the_address_checks_are_skipped() {
        let opened = |vars: &[(&str, &str)], ports: Vec<u16>| {
            let named = vars.iter().map(|(k, v)| (k.to_string(), std::ffi::OsString::from(v))).collect();
            let upstream = proxy::upstream::of(&named, proxy::upstream::Setting::Env);
            let p =
                Policy::for_project(&Env { network: true, upstream, ports, ..env(Path::new("/Users/u/p")) });
            let p = Policy { proxy: Some(proxy::RunPort::new(0).unwrap()), ..p };
            let r = Guard::In(Box::new(p)).report(&[]);
            r.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let said = |vars: &[(&str, &str)]| opened(vars, Vec::new());
        let corp = "http://u:tarae-fake-token-4@corp-proxy.example.invalid:3128";
        let r = said(&[("HTTPS_PROXY", corp), ("http_proxy", corp)]);
        assert!(
            r.contains(
                "can't go out; beyond it, the proxy tarae itself uses (https through tarae's own HTTPS_PROXY, plain \
                 http through tarae's own http_proxy) — which looks the names up, so host names and TLS server names \
                 are checked but not the addresses they lead to: link-local addresses, the cloud metadata servers \
                 and this machine's own addresses aren't refused for what goes through it (only the metadata \
                 servers' own names are — a partial check); NO_PROXY's hosts go direct, never link-local \
                 addresses, the cloud metadata servers tarae knows of, multicast or broadcast — but none of this \
                 machine's ports"
            ),
            "{r}"
        );
        assert!(!r.contains("corp-proxy") && !r.contains("tarae-fake-token"), "{r}");
        let r = said(&[]);
        assert!(
            r.contains("can't go out; never link-local addresses") && !r.contains("tarae itself uses"),
            "{r}"
        );
        let r = said(&[("HTTPS_PROXY", "https://corp-proxy.example.invalid:3129")]);
        assert!(
            r.contains(
                "can't go out; beyond it: https nowhere — tarae's own HTTPS_PROXY names an https:// proxy, which \
                 tarae's proxy doesn't chain to yet (llm.sandbox-upstream = \"none\" sends it direct), plain http \
                 direct; never link-local addresses"
            ),
            "{r}"
        );
        // That proxy on this machine, at a port open to the runs: they reach it themselves — said; not open, not
        let itself =
            "that proxy is on this machine, at a port open to your commands: they can reach it themselves";
        let local = [("HTTPS_PROXY", "http://127.0.0.1:3128")];
        assert!(opened(&local, vec![3128]).contains(itself));
        assert!(!opened(&local, vec![3129]).contains(itself) && !said(&local).contains(itself));
    }

    /// A run's proxy URL is the one decision every proxy line hangs on (step 5f), made from the policy alone —
    /// never the system the test runs on: the network closed, none; made for macOS, the run's own port and its
    /// token as the password (`per_run`: never in a kept home); made for Linux, the supervisor's address with no
    /// credential, the same for every run.
    #[test]
    fn a_policys_proxy_url_is_its_own() {
        let (closed, t) = made_for(Os::Mac, false, "url-closed");
        assert_eq!(closed.proxy_url(), None);
        std::fs::remove_dir_all(&t).ok();
        let (mac, t) = made_for(Os::Mac, true, "url-mac");
        let port = mac.proxy.clone().expect("a port of the run's own");
        let url = mac.proxy_url().unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.user.as_deref(), url.password.as_deref(), url.per_run),
            ("127.0.0.1", port.port, Some("run"), Some(port.token()), true)
        );
        std::fs::remove_dir_all(&t).ok();
        let (linux, t) = made_for(Os::Linux, true, "url-linux");
        let url = linux.proxy_url().unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.user, url.password, url.per_run),
            ("192.0.2.1", 3128, None, None, false)
        );
        std::fs::remove_dir_all(&t).ok();
    }

    /// 6e′: where the session's network takes a run's connections (Linux, `Here.redirects` — the probe's, on a
    /// run's way), the run has no proxy URL and none of what hangs on it — no proxy variable, none of the packs'
    /// proxy lines (Node's flag, Maven's settings) — and its key isn't that of a run that has them (a daemon
    /// started with proxy lines isn't reused). Where it doesn't (tier C), as before; the network closed, nothing to
    /// take; macOS keeps the run's own port whatever `redirects` says.
    #[test]
    fn where_the_session_takes_the_connections_a_run_gets_no_proxy_lines() {
        let made = |os: Os, network: bool, redirects: bool| {
            let t = crate::scratch::Scratch::new(&format!("sb-6e2-{os:?}-{network}-{redirects}")).into_path();
            std::fs::create_dir_all(t.join("p")).unwrap();
            let here = Here { tmp: Some(t.clone()), uid: 501, os, redirects, ..Here::default() };
            let llm = crate::llm::LlmConfig { sandbox_network: network, ..Default::default() };
            let p = policy_in(&t.join("p"), &llm, &[], &here);
            let cmd = command(Some(&p), "true", &[]);
            let mut vars: Vec<String> =
                cmd.get_envs().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
            vars.extend(p.env.iter().map(|(k, _)| k.clone()));
            let proxied = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "NODE_USE_ENV_PROXY", "MAVEN_ARGS"];
            vars.retain(|v| proxied.contains(&v.as_str()));
            crate::sorting::sort(&mut vars);
            vars.dedup();
            std::fs::remove_dir_all(&t).ok();
            (p, vars)
        };
        let (taken, vars) = made(Os::Linux, true, true);
        assert!(taken.redirected && taken.supervised, "{taken:?}");
        assert_eq!((taken.proxy_url(), vars), (None, Vec::<String>::new()));
        let (tier_c, vars) = made(Os::Linux, true, false);
        assert!(!tier_c.redirected && tier_c.proxy_url().is_some());
        for v in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "NODE_USE_ENV_PROXY"] {
            assert!(vars.iter().any(|s| s == v), "tier C: {v} — {vars:?}");
        }
        assert_ne!(policy_key(&taken), policy_key(&tier_c), "with the lines or without: two keys");
        // (and the report says which: no variable to name, or the one a tool mustn't ignore — Linux's report: on
        // macOS a Linux policy has no port of its own to the proxy, and reads as loopback only)
        #[cfg(target_os = "linux")]
        {
            let said = |p: &Policy| {
                Guard::In(Box::new(p.clone())).report(&[]).split_whitespace().collect::<Vec<_>>().join(" ")
            };
            let (taken_said, tier_c_said) = (said(&taken), said(&tier_c));
            assert!(
                taken_said.contains(
                    "through tarae's proxy, which takes every connection they make and answers their names (no \
                     proxy variable is set)"
                ) && !taken_said.contains("HTTPS_PROXY"),
                "{taken_said}"
            );
            assert!(
                tier_c_said.contains("only through tarae's proxy (HTTPS_PROXY, set for every run)"),
                "{tier_c_said}"
            );
        }
        let (closed, vars) = made(Os::Linux, false, true);
        assert!(!closed.redirected && closed.proxy_url().is_none() && vars.is_empty(), "{vars:?}");
        let (mac, _) = made(Os::Mac, true, true);
        assert!(!mac.redirected && mac.proxy_url().is_some_and(|u| u.per_run));
    }

    /// A proxy URL that's the same for every run is part of a policy's key (step 5f): a home a daemon lives from
    /// is the key's, so a daemon is never reused by runs whose proxy lines differ from those it started with.
    /// A run's own URL (its port, its token) is not — every run would be a key of its own, and the token would be
    /// in what the key is made from.
    #[test]
    fn a_policys_key_holds_a_shared_proxy_url_never_a_runs_own() {
        let root = Path::new("/w/p");
        let open = Policy::for_project(&Env { network: true, ..env(root) });
        let shared = Policy { proxy: None, supervised: true, ..open.clone() };
        let none = Policy { proxy: None, supervised: false, ..open.clone() };
        assert_ne!(policy_key(&shared), policy_key(&none), "a shared URL or none: two keys");
        let own = |p: &Policy| Policy { proxy: Some(proxy::RunPort::new(p.run_id).unwrap()), ..p.clone() };
        let (a, b) = (own(&none), own(&none));
        assert_ne!(a.proxy_url().unwrap().password, b.proxy_url().unwrap().password);
        assert_eq!(policy_key(&a), policy_key(&b), "a run's own port and token aren't in the key");
    }

    /// A run's rules for the proxy (`Policy.egress`) are the setting's as the network has them — off: no host,
    /// whatever the list; on with `"any"`: any host; on with a list: those, every other asked — and they're not
    /// the policy's key: an answer, or the list changed, mustn't end what an earlier run left (Linux ends a kept
    /// daemon under another key). Every run has an id of its own, the proxy's questions name it.
    #[test]
    fn a_runs_rules_carry_the_setting_and_the_answers() {
        use proxy::egress::Pattern;
        let root = Path::new("/w/p");
        let list = Some(vec![Pattern::parse("github.com").unwrap(), Pattern::parse("*.crates.io").unwrap()]);
        let p =
            |network, hosts: Option<Vec<Pattern>>| Policy::for_project(&Env { network, hosts, ..env(root) });
        let off = p(false, list.clone());
        assert!(!off.egress.any && off.egress.hosts.is_empty(), "the network off: {:?}", off.egress);
        let any = p(true, None);
        assert!(any.egress.any && any.egress.allows("anything.test", 443, 443));
        let listed = p(true, list.clone());
        assert!(!listed.egress.any);
        assert!(
            listed.egress.allows("github.com", 443, 443)
                && listed.egress.allows("static.crates.io", 443, 443)
        );
        assert!(!listed.egress.allows("example.invalid", 443, 443), "another host is asked");
        assert_eq!(policy_key(&listed), policy_key(&p(true, Some(Vec::new()))), "the list isn't the key");
        let mut answered = listed.clone();
        answered.egress.answered.push(("example.invalid".into(), 443));
        assert_eq!(policy_key(&listed), policy_key(&answered), "nor an answer");
        assert_ne!(listed.run_id, p(true, list).run_id, "each run its own id");
    }

    /// What Claude reads about Gradle (Java under P2): its writes in the user's Gradle home — the read-only
    /// dependency cache's index, measured in every build — are expected and said so, not offered to open; a
    /// JVM refused a port on this machine says Gradle needs this machine's ports, only while they're closed — the
    /// network open doesn't open them (it goes through tarae's proxy).
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
        let open = Policy { localhost: true, ..p.clone() };
        assert!(!report(&open, std::slice::from_ref(&port)).contains("Gradle build can't run"));
        let network = Policy { network: true, ..p.clone() };
        assert!(report(&network, std::slice::from_ref(&port)).contains("Gradle build can't run"));
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

    /// What Claude reads when the profile kept something on purpose: which names, and to edit them with its edit
    /// tool (the user reviews it — never "Edit", a tool the chat may not have: `llm.edit-tools`); git's folder, and that git's writes are the user's; cargo's unpacked crates, and why —
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
                && r.contains("To change them, edit them with your edit tool: the user reviews it."),
            "{r}"
        );
        assert!(!r.contains("use Edit"), "{r}");
        assert!(r.contains("  linking /Users/u/p/build.rs (forbidden-link-priv<file-write*>, touch)"), "{r}");
        // Every protected name said, the packs' and the core's
        assert!(protected_names().all(|n| r.contains(&format!(" {n},"))), "{r}");
        assert!(
            r.contains(".tarae.local.toml, .git, anywhere in it (your edits change those)")
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

    /// Connecting on this machine: only the listed ports (`localhost:*` only when asked — its port denies can't
    /// win, measured), the network closed or open — open, the run's own port to tarae's proxy too (macOS, step
    /// 5d: the network goes out through it), and without one of its own none more.
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
        let listed_only = ["(allow network-outbound (remote ip \"localhost:6379\"))"];
        assert_eq!(outbound(&open), listed_only, "no port of its own to the proxy: nothing more");
        let port = proxy::RunPort::new(0).unwrap();
        let proxied = Policy { proxy: Some(port.clone()), ..open.clone() };
        assert_eq!(
            outbound(&proxied),
            [format!(
                "(allow network-outbound (remote ip \"localhost:6379\") (remote ip \"localhost:{}\"))",
                port.port
            )]
        );
        let closed = Policy { network: false, ..proxied };
        assert!(
            outbound(&closed).iter().all(|l| !l.contains(&port.port.to_string())),
            "closed: not the proxy's"
        );
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

    /// F2: one `ps` looks into every process — each refused look its own denial would take every kept slot,
    /// and the `.git` write after it would reach neither the panel nor Claude. They're one, whichever process;
    /// a signal refused stays one per process (it names the one it was sent to).
    #[test]
    fn looks_into_other_processes_are_one_denial() {
        let mut b = Blocked::default();
        for pid in 0..DENIALS_KEPT + 5 {
            b.add(denial("process-info-pidinfo", &format!("others [sleep({pid})]")));
        }
        b.add(denial("signal", "others [sh(1)] signum:19"));
        b.add(denial("signal", "others [sh(2)] signum:19"));
        b.add(denial("file-write-create", "/Users/u/p/.git/index.lock"));
        let ops: Vec<&str> = b.seen.iter().map(|d| d.op.as_str()).collect();
        assert_eq!(ops, ["process-info-pidinfo", "signal", "signal", "file-write-create"]);
        assert_eq!((b.total, b.distinct), (DENIALS_KEPT + 8, 4));
    }

    /// F2, macOS: a signal or a look refused outside the run's own processes is kept on purpose — Claude reads
    /// why, in macOS's words (never Landlock's: that's Linux's), and to ask the user, not "tell the user to open
    /// it". (Seatbelt's denials, as the watch records them; Linux's words: `linux::tests`)
    #[cfg(target_os = "macos")]
    #[test]
    fn the_report_says_other_processes_are_kept_on_purpose() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let said = |denials: &[(&str, &str)]| {
            let w = Watch::quiet("t.");
            for (op, target) in denials {
                w.record(denial(op, target));
            }
            Guard::In(Box::new(Policy { watch: Some(w), ..p.clone() })).report(&[])
        };
        let (signal, look) =
            (("signal", "others [sh(71140)] signum:19"), ("process-info-pidinfo", "others [ps(9)]"));
        let attach = ("mach-priv-task-port", "others [tarae(4242)]");
        for (denials, to) in [
            (&[signal][..], "signal"),
            (&[look], "look into"),
            (&[signal, look], "signal or look into"),
            (&[attach], "attach a debugger to"),
            (&[signal, look, attach], "signal, look into or attach a debugger to"),
        ] {
            let r = said(denials);
            assert!(r.contains(&format!("A command can't {to} a process outside its own")), "{r}");
            assert!(r.contains("macOS's sandbox lets it signal and read the details of"), "{r}");
            assert!(!r.contains("If the command needs one of these"), "all of it kept on purpose: {r}");
            assert!(!r.contains("Landlock"), "{r}");
        }
        // Anything else blocked beside them still asks the user to open it
        let r = said(&[look, ("file-read-data", "/Users/u/other/x")]);
        assert!(r.contains("If the command needs one of these"), "{r}");
    }

    /// macOS: `/bin/ps` is setuid and the sandbox never starts it — the shell says only "Operation not
    /// permitted" (nothing in Seatbelt's log), which read as a write to `/bin/ps` refused and "tell the user to
    /// open it". The report says why in plain words and what to use instead (`pgrep`, `lsof -p`), nothing to
    /// open. Each shell's line as it says it (measured, macOS 27.0); a line naming another program isn't it.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_report_says_why_ps_never_starts() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let g = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("t.")), ..p.clone() }));
        let said = |line: &str| g.report(&[(true, line.to_string())]);
        for line in [
            "sh: /bin/ps: Operation not permitted",
            "bash: line 1: /bin/ps: Operation not permitted",
            "zsh:1: operation not permitted: ps",
            "env: ps: Operation not permitted",
        ] {
            let r = said(line);
            assert!(r.contains("ps never starts in macOS's sandbox: it's setuid"), "{line}: {r}");
            assert!(r.contains("pgrep -l -x name") && r.contains("lsof -p"), "{line}: {r}");
            // (names only: never a way to read other processes' command lines — F2 keeps that unasked-for)
            assert!(!r.contains("pgrep -f") && !r.contains("pgrep -lf"), "{line}: {r}");
            assert!(!r.contains("writing /bin/ps") && !r.contains("If the command needs one"), "{line}: {r}");
        }
        // Beside something blocked: both said, and that one still asks the user
        let w = Watch::quiet("t.");
        w.record(denial("file-write-create", "/Users/u/other/x"));
        let r = Guard::In(Box::new(Policy { watch: Some(w), ..p.clone() }))
            .report(&[(true, "sh: /bin/ps: Operation not permitted".into())]);
        assert!(r.contains("ps never starts") && !r.contains("writing /bin/ps"), "{r}");
        assert!(r.contains("writing /Users/u/other/x") && r.contains("If the command needs one"), "{r}");
        for other in ["sh: /bin/pstree: Operation not permitted", "zsh:1: operation not permitted: top"] {
            assert!(!said(other).contains("setuid"), "{other}");
        }
        // (Linux: `ps` isn't setuid there and runs — never said)
        assert_eq!(ps_said(&[(true, "sh: /bin/ps: Operation not permitted".into())], Os::Linux), None);
    }

    /// A protected name refused by its name relative to where the run is — `sh`'s `cannot touch 'Makefile'`, the
    /// only word of it on Linux without Landlock's audit — is said as what the sandbox blocked, never "names no
    /// cause" (compat, Linux C); a quoted name the rules let it write isn't.
    #[test]
    fn a_protected_name_refused_by_its_relative_name_is_said() {
        let p = Policy::for_project(&env(Path::new("/Users/u/p")));
        let g = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("t.")), ..p.clone() }));
        // (each system's words: Landlock's "Permission denied" is a refusal only on Linux)
        let why = if cfg!(target_os = "linux") { "Permission denied" } else { "Operation not permitted" };
        for (line, target) in [
            (format!("touch: cannot touch 'Makefile': {why}"), "/Users/u/p/Makefile"),
            (format!("sh: 1: cannot create build.rs: {why}"), "/Users/u/p/build.rs"),
            (format!("cp: cannot create regular file ‘sub/.envrc’: {why}"), "/Users/u/p/sub/.envrc"),
        ] {
            let out = [(true, line.clone())];
            let said: Vec<String> = p.said_in_output(&out, &[]).into_iter().map(|d| d.target).collect();
            assert_eq!(said, [target], "{line}");
            assert!(g.report_with(&out, Told::default()).blocked, "{line}");
        }
        let allowed = [(true, format!("touch: cannot touch 'src/a.rs': {why}"))];
        assert!(p.said_in_output(&allowed, &[]).is_empty(), "the rules let it write there");
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
        assert_eq!(crate::cards::denied_line("go vet", &w.blocked()), None);
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
        let (here, vars) = (Here::default(), vars_in(Path::new(&home), &Here::default()));
        let env = packs_env(
            &crate::sandboxpack::Vars { run: Some(run.path().to_path_buf()), ..vars.clone() },
            &here,
        );
        assert!(
            packs_env(&vars, &here).iter().all(|(k, _)| k != "npm_config_logs_dir"),
            "no run: no log folder"
        );
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

    /// Step 5 (Linux): a run with the network open gets tarae's proxy variables — each naming the proxy's address,
    /// this machine's loopback left direct — and never the user's own proxies (theirs would send it past the
    /// proxy, where only `network-direct` refusals wait), nor are those said as left out; with the network
    /// closed, none of them.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_run_under_the_proxy_has_its_variables() {
        let here = Here {
            env: HashMap::from([
                ("HTTPS_PROXY".to_string(), std::ffi::OsString::from("http://users-proxy.example:3128")),
                ("PATH".to_string(), "/usr/bin:/bin".into()),
            ]),
            ..Here::default()
        };
        let env_of = |c: &Command| -> HashMap<String, String> {
            let set =
                c.get_envs().filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())));
            set.collect()
        };
        let open = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
        let run_env = run_env_of(&here, &open);
        assert!(run_env.var("HTTPS_PROXY").is_none() && !run_env.left_out().any(|n| n == "HTTPS_PROXY"));
        let p = Policy { network: true, run_env, ..Policy::for_project(&env(Path::new("/w/p"))) };
        let set = env_of(&command(Some(&p), "true", &[]));
        let url = format!("http://{}", route::PROXY);
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
            assert_eq!(set.get(k), Some(&url), "{k}");
        }
        assert_eq!(set.get("NO_PROXY").map(String::as_str), Some("localhost,127.0.0.1,::1"));
        let closed = Policy { network: false, ..p };
        assert!(!env_of(&command(Some(&closed), "true", &[])).contains_key("HTTPS_PROXY"));
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

    /// macOS with the network open (step 5d): the report says what Linux's does — beyond this machine only
    /// through tarae's proxy, this machine's ports only as opened. With `llm.sandbox-localhost` it says that
    /// other runs' ports to the proxy open too (Q3a); without a port of its own to the proxy, that nothing goes
    /// beyond.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_report_says_macos_goes_out_only_through_the_proxy() {
        let said = |p: Policy| {
            let r = Guard::In(Box::new(p)).report(&[]);
            r.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let base = Policy::for_project(&env(Path::new("/Users/u/p")));
        let open = Policy { network: true, proxy: Some(proxy::RunPort::new(0).unwrap()), ..base.clone() };
        let r = said(Policy { ports: vec![3000], ..open.clone() });
        assert!(
            r.contains(
                "reach the network beyond this machine (its loopback and its own addresses) only through tarae's \
                 proxy (HTTPS_PROXY, set for every run) — a tool that ignores it (ssh, a database client, Node's \
                 fetch without NODE_USE_ENV_PROXY) can't go out; never link-local addresses, the cloud metadata \
                 servers tarae knows of, multicast or broadcast — and this machine's ports 3000 only \
                 (llm.sandbox-ports)"
            ),
            "{r}"
        );
        let other_runs = "llm.sandbox-localhost opens other runs' ports to tarae's proxy too";
        assert!(!r.contains(other_runs), "{r}");
        let r = said(Policy { localhost: true, ..open.clone() });
        assert!(r.contains(&format!("and every port of this machine's — {other_runs}")), "{r}");
        let r = said(Policy { localhost: true, ..base.clone() });
        assert!(
            r.contains(&format!("connect to nothing over the network but this machine — {other_runs}")),
            "{r}"
        );
        let r = said(Policy { proxy: None, ..open });
        assert!(
            r.contains("tarae couldn't open this run a port to its proxy, so nothing goes beyond it"),
            "{r}"
        );
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

    /// A host the proxy held, refused, says why in the report Claude reads — not among the hosts and unanswered,
    /// refused by the user, past the questions a session may have up — with its own op, so Claude can tell it
    /// from the proxy's other refusals; and the report's way out names `llm.sandbox-hosts`.
    #[test]
    fn the_report_says_why_a_host_was_refused() {
        let reported = |target: &str| denial(proxy::HOST_OP, target).reported();
        assert_eq!(
            reported(
                "static.example.invalid:443: not among the hosts it may reach (llm.sandbox-hosts); the user was \
                 asked and didn't answer within 20 s"
            ),
            "reaching static.example.invalid:443 — not among the hosts it may reach (llm.sandbox-hosts); the user \
             was asked and didn't answer within 20 s (network-host, touch)"
        );
        assert_eq!(
            reported("static.example.invalid:443: the user refused it for this session"),
            "reaching static.example.invalid:443 — the user refused it for this session (network-host, touch)"
        );
        assert_eq!(
            reported("static.example.invalid:443: 8 hosts already wait for the user's answer"),
            "reaching static.example.invalid:443 — 8 hosts already wait for the user's answer (network-host, touch)"
        );
        let golden = include_str!("snapshots/sandbox_report.txt");
        assert!(golden.contains("llm.sandbox-network and llm.sandbox-hosts open more"));
    }

    /// 6e+: a raw connection or a name's records refused say so in the report, each in its own words and op — the
    /// host:port or the question first (as the proxy sends them: `serve_raw`'s reset, the held lookup's refusal),
    /// then why; a target without its why said whole.
    #[test]
    fn the_report_says_why_raw_tcp_or_a_lookup_was_refused() {
        let raw = |target: &str| denial(proxy::RAW_OP, target).reported();
        assert_eq!(
            raw("db.test:5432: the user refused it — tarae refused it: the tool sees its connection reset"),
            "a raw connection to db.test:5432 — the user refused it — tarae refused it: the tool sees its connection \
             reset (network-raw, touch)"
        );
        assert_eq!(raw("db.test:5432"), "a raw connection to db.test:5432 (network-raw, touch)");
        let lookup = |target: &str| denial(proxy::LOOKUP_OP, target).reported();
        assert_eq!(
            lookup("SRV _mongodb._tcp.cluster.test: the user refused it for this session"),
            "looking up SRV _mongodb._tcp.cluster.test — the user refused it for this session (network-lookup, touch)"
        );
        assert_eq!(lookup("TXT cluster.test"), "looking up TXT cluster.test (network-lookup, touch)");
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
        // A terminal (3c): why it's closed, the ones a run makes too, and what opens them — read or written
        let tty = plain("file-write-data", "/dev/ttys004");
        assert!(tty.starts_with("opening the terminal /dev/ttys004 — every terminal is closed"), "{tty}");
        assert!(tty.ends_with("the run's panel (space m s) lets this project's commands open terminals"));
        assert_eq!(plain("file-read-data", "/dev/ttys004"), tty);
        assert_eq!(plain("file-write-data", "/dev/ttys"), "writing /dev/ttys", "no number, no terminal");
        assert_eq!(plain("file-write-data", "/dev/ttysx1"), "writing /dev/ttysx1");
        assert_eq!(plain("file-write-create", "/dev/ttys004"), tty, "`>` opens with O_CREAT");
        assert_eq!(plain("file-read-metadata", "/dev/ttys004"), "looking at /dev/ttys004", "only an open");
        assert_eq!(plain("file-write-unlink", "/dev/ttys004"), "deleting /dev/ttys004", "only an open");
        assert_eq!(plain("file-write-mode", "/dev/ttys004"), "changing /dev/ttys004", "only an open");
        assert_eq!(plain("file-write-rename", "/dev/ttys004"), "moving /dev/ttys004", "only an open");
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
        // F2: a signal refused names its process (Seatbelt: `others [name(pid)] signum:N` — measured); a look
        // into another's details names none — `lsof -p` looks into every process: they fold into one line
        assert_eq!(
            plain("signal", "others [bash(71140)] signum:19"),
            "signalling bash (71140) — outside the run's own processes"
        );
        assert_eq!(plain("signal", "4242"), "signalling process 4242 — outside the run's own processes");
        assert_eq!(
            plain("process-info-pidinfo", "others [sleep(71161)]"),
            "looking into processes outside the run's own"
        );
        assert_eq!(
            plain("process-info-pidinfo", "others [log(71142)]"),
            "looking into processes outside the run's own"
        );
        assert_eq!(
            plain("mach-priv-task-port", "others [tarae(4242)]"),
            "attaching to tarae (4242) — outside the run's own processes"
        );
        // As Seatbelt says it, where there are no plainer words
        assert_eq!(plain("mach-lookup", "com.apple.other"), "mach-lookup com.apple.other");
        assert_eq!(plain("network-outbound", "remote:*:x"), "network-outbound remote:*:x");
        // Linux's supervisor names the address: this machine's by port, another as it is
        assert_eq!(plain("network-outbound", "remote:127.0.0.1:9"), "connecting to port 9");
        assert_eq!(plain("network-outbound", "remote:[::ffff:127.0.0.1]:9"), "connecting to port 9");
        assert_eq!(plain("network-outbound", "remote:192.0.2.1:9"), "connecting to 192.0.2.1:9");
        assert_eq!(plain("network-outbound", "remote:[2001:db8::1]:9"), "connecting to [2001:db8::1]:9");
        assert_eq!(plain("network-outbound", "@bus"), "network-outbound @bus");
        // Linux's routing (step 4): never reached, and a socket that can't leave the session
        assert_eq!(
            plain("network-never", "remote:169.254.169.254:80"),
            "connecting to 169.254.169.254:80 — link-local addresses, the cloud metadata servers tarae knows of, multicast \
             and broadcast are never reached"
        );
        let bound =
            "remote:93.184.216.34:443 from a socket it bound itself — a bound socket stays in the session";
        assert_eq!(
            plain("network-bound", bound),
            "connecting to 93.184.216.34:443 from a socket it bound itself — a bound socket stays in the session"
        );
        // Linux's supervisor: a run's netlink reads only
        assert_eq!(
            plain("network-netlink", "message type 20"),
            "netlink (message type 20) — a command's netlink only reads the network, never changes it"
        );
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
    /// the rest of the network doesn't (terminals: `a_run_cant_open_another_terminal`). Inside a sandbox
    /// already (Claude's own `cargo test`) none can start: the probe says so, and that's what's checked.
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
            runs: None,
            tag: None,
            proxy: None,
            supervised: Os::HOST == Os::Linux,
            redirected: false,
            egress: Default::default(),
            run_id: 0,
            session: 0,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            terminals: false,
            keys: None,
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
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS (3c): a terminal of the user's — a pty this test opens, as their shell's — can't be
    /// read nor written by a run, and one the run makes itself can't either (`script`: `/dev/ptmx` opens, its
    /// other end is refused — failing, not hanging); the project's grant (`allow::Grant::Terminals`, kept as
    /// `A` keeps it) opens both, and what the run writes reaches the terminal. Harmless: the pty is the test's
    /// own, and only a marker goes through it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_run_cant_open_another_terminal() {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        if skipped_inside("sandbox::tests::a_run_cant_open_another_terminal") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("sb-tty");
        let t = profile_path(&scratch);
        let root = t.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let (master, _slave) = crate::pty::open(24, 80).unwrap();
        let mut name = [0u8; 128];
        // SAFETY: TIOCPTYGNAME fills a 128-byte buffer with a NUL-terminated path.
        let named = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCPTYGNAME as _, name.as_mut_ptr()) };
        assert_eq!(named, 0);
        let tty = std::ffi::CStr::from_bytes_until_nul(&name).unwrap().to_str().unwrap().to_string();
        assert!(tty.starts_with("/dev/ttys"), "{tty}");
        let store = t.join("state/sandbox.json");
        crate::allow::keep(&store, &root, |a| a.add(&crate::allow::Grant::Terminals)).unwrap();
        let kept = crate::allow::Store::read(Some(&store)).get(&root);
        let none = crate::allow::Allowed::default();
        for (scope, open) in [(&none, false), (&kept, true)] {
            let w = crate::tools::with_allowed(
                &crate::llm::LlmConfig::default(),
                &crate::caps::Caps::answered(&[scope]),
                None,
            );
            let mut p = Policy::for_project(&Env { root: root.clone(), ..Env::default() });
            w.onto(&mut p);
            assert_eq!(p.terminals, open);
            // (each within 10 s, else killed and failed: a pty refused must fail, not hang — its output is a
            // line, so the pipes never fill while it's waited for)
            let sh = |script: &str| {
                let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
                c.current_dir(&root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = c.spawn().unwrap();
                let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while child.try_wait().unwrap().is_none() {
                    if std::time::Instant::now() > end {
                        child.kill().ok();
                        child.wait().ok();
                        panic!("{script}, granted {open}: still going after 10 s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                child.wait_with_output().unwrap()
            };
            let said = |r: &std::process::Output| String::from_utf8_lossy(&r.stderr).into_owned();
            let write = sh(&format!("printf tarae-marker > {tty}"));
            let read = sh(&format!(": < {tty}"));
            let made = sh("/usr/bin/script -q /dev/null /usr/bin/true");
            for (r, what) in [(&write, "writing"), (&read, "reading"), (&made, "a pty of its own")] {
                assert_eq!(r.status.success(), open, "{what}, granted {open}: {}", said(r));
            }
            if !open {
                for (r, what) in [(&write, "writing"), (&read, "reading")] {
                    assert!(said(r).contains("Operation not permitted"), "{what}: {}", said(r));
                }
                continue;
            }
            // (what the run wrote, from the test's end: the marker, as the terminal's line discipline passes it)
            // SAFETY: the master is this test's own, live; nonblocking so a missing marker fails, not hangs.
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
            let mut got = [0u8; 64];
            let n = std::fs::File::from(master.try_clone().unwrap()).read(&mut got).unwrap_or(0);
            assert_eq!(&got[..n], b"tarae-marker");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// Whether the real-sandbox tests of files named like keys run here — else the test says it skips itself and
    /// why: inside a sandbox on macOS (Seatbelt can't start there), without Landlock on Linux. Any ABI: nothing
    /// here needs more than 1 (there every link and rename across folders is refused, which the tests take as
    /// refused too).
    fn keys_tested_here(test: &str) -> bool {
        let test = format!("sandbox::tests::{test}");
        #[cfg(target_os = "macos")]
        return !skipped_inside(&test);
        #[cfg(not(target_os = "macos"))]
        match crate::landlock::abi() {
            Ok(_) => true,
            Err(e) => {
                let line =
                    format!("{}{test} — needs Landlock, this kernel: {e}\n", crate::test_results::SELF_SKIP);
                let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
                false
            }
        }
    }

    /// What a refused open says: Seatbelt's EPERM, Landlock's EACCES.
    const KEY_DENIED: &str =
        if cfg!(target_os = "macos") { "Operation not permitted" } else { "Permission denied" };

    /// The policy a run in the project at `root` gets, its environment as tarae's runs have it.
    fn key_policy(root: &Path) -> Policy {
        Policy::for_project(&Env { root: root.to_path_buf(), run_env: run_env_here(), ..Env::default() })
    }

    /// A project for the real-sandbox tests of files named like keys (7a, 7b), in the test's own temp folder:
    /// each file's whole text a fake token — what a run that reads it prints. (Its folder, its project.)
    fn key_files(tag: &str) -> (PathBuf, PathBuf) {
        let t = profile_path(&crate::scratch::Scratch::new(&format!("sb-keys-{tag}")).into_path());
        let root = t.join("project");
        for f in [
            ".env",
            "deep/er/Server.PEM",
            ".env.example",
            "id_ed25519.pub",
            "node_modules/pkg/test.pem",
            "src/env.rs",
        ] {
            let at = root.join(f);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(&at, "tarae-fake-token").unwrap();
        }
        (t, root)
    }

    /// `script` run in `root` under `p` — on Linux its rules made now, as each run's are (`confined`): whether it
    /// ended well, and its output and errors.
    fn sh_under(p: &Policy, root: &Path, script: &str) -> (bool, String, String) {
        #[cfg(target_os = "macos")]
        {
            let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            let mut c = command(Some(p), "/bin/sh", &["-c".into(), script.into()]);
            let out = c.current_dir(root).stdin(Stdio::null()).output().unwrap();
            (out.status.success(), text(&out.stdout), text(&out.stderr))
        }
        #[cfg(not(target_os = "macos"))]
        {
            sh_confined(&confined(Policy { run: RunDir::new().ok(), ..p.clone() }), root, script)
        }
    }

    /// Linux: `script` run in `root` under `p` whose rules were made already (`confined`) — as `sh_under`.
    #[cfg(not(target_os = "macos"))]
    fn sh_confined(p: &Policy, root: &Path, script: &str) -> (bool, String, String) {
        sh_confined_env(p, root, script, &[])
    }

    /// `sh_confined`, with `env` in its environment too.
    #[cfg(not(target_os = "macos"))]
    fn sh_confined_env(
        p: &Policy,
        root: &Path,
        script: &str,
        env: &[(&str, &str)],
    ) -> (bool, String, String) {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let mut c = command(Some(p), "/bin/sh", &["-c".into(), script.into()]);
        c.current_dir(root).stdin(Stdio::null()).envs(env.iter().copied());
        let s = spawn(Some(p), &c).unwrap_or_else(|e| panic!("{e}: {:?}", p.confine));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut o, mut e) = (s.stdout.expect("its stdout"), s.stderr.expect("its stderr"));
        std::thread::scope(|sc| {
            sc.spawn(|| std::io::Read::read_to_end(&mut e, &mut err));
            let _ = std::io::Read::read_to_end(&mut o, &mut out);
        });
        let ok = crate::spawn::wait(s.pid).is_ok_and(|st| st.success());
        (ok, text(&out), text(&err))
    }

    /// For real (7a, 7b): a run can't read a file in the project named like a key — at the top or deep, in any
    /// case — nor through a link to it or a copy of it; it sees that it's there; the rule in Rust agrees. On
    /// Linux (Landlock's rules hold the file, not its name) not after a rename or a hard link either, in its
    /// folder or to one that reads — where it's hidden by a mount (7d), the mount holds it: it can't be renamed
    /// nor linked, and a folder moved takes it along.
    #[test]
    fn a_run_cant_read_a_key_named_file() {
        if !keys_tested_here("a_run_cant_read_a_key_named_file") {
            return;
        }
        let (t, root) = key_files("closed");
        let p = key_policy(&root);
        for f in [".env", "deep/er/Server.PEM"] {
            let (ok, out, err) = sh_under(&p, &root, &format!("cat {f}"));
            assert!(!ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED), "{f}: {out} {err}");
            assert!(!p.reads_allow(&root.join(f)), "{f}: the rule in Rust");
        }
        let (ok, out, err) = sh_under(&p, &root, "test -f .env && ln -s .env l && cat l; cp .env c; cat c");
        assert!(!ok && !out.contains("tarae-fake-token") && root.join("l").exists(), "{out} {err}");
        #[cfg(not(target_os = "macos"))]
        for script in [
            "ln deep/er/Server.PEM src/h; cat src/h",
            "mv deep/er src/er; cat src/er/Server.PEM",
            "mv .env moved && cat moved",
        ] {
            let (ok, out, err) = sh_under(&p, &root, script);
            assert!(!ok && !out.contains("tarae-fake-token"), "{script}: {out} {err}");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real (7a, 7b): a file named like a key inside a protected name — `.cargo/credentials`, a real token — is
    /// closed as well; what's beside it is read.
    #[test]
    fn a_key_inside_a_protected_folder_is_closed() {
        if !keys_tested_here("a_key_inside_a_protected_folder_is_closed") {
            return;
        }
        let (t, root) = key_files("protected");
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        for f in [".cargo/credentials", ".cargo/config.toml"] {
            std::fs::write(root.join(f), "tarae-fake-token").unwrap();
        }
        let p = key_policy(&root);
        let (ok, out, err) = sh_under(&p, &root, "cat .cargo/credentials");
        assert!(!ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED), "{out} {err}");
        let (ok, out, err) = sh_under(&p, &root, "cat .cargo/config.toml");
        assert!(ok && out == "tarae-fake-token", "beside it: {err}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real (7a, 7b): what's named like a key but isn't one is read — the data's exceptions (a `.env`'s
    /// template, a public key), a package's test key below tools' output, a name that only shares a part.
    #[test]
    fn an_exception_to_the_key_names_is_read() {
        if !keys_tested_here("an_exception_to_the_key_names_is_read") {
            return;
        }
        let (t, root) = key_files("except");
        let p = key_policy(&root);
        for f in [".env.example", "id_ed25519.pub", "node_modules/pkg/test.pem", "src/env.rs"] {
            let (ok, out, err) = sh_under(&p, &root, &format!("cat {f}"));
            assert!(ok && out == "tarae-fake-token", "{f}: {err}");
            assert!(p.reads_allow(&root.join(f)), "{f}: the rule in Rust");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real (7a, 7b): the user's grant from the run's panel (`allow::Grant::Key`, kept as `A` keeps it) opens
    /// that one file — another named like a key stays closed, in its folder too.
    #[test]
    fn a_granted_key_file_is_read() {
        if !keys_tested_here("a_granted_key_file_is_read") {
            return;
        }
        let (t, root) = key_files("granted");
        std::fs::write(root.join("deep/er/other.key"), "tarae-fake-token").unwrap();
        let store = t.join("state/sandbox.json");
        let g = crate::allow::Grant::Key(root.join("deep/er/Server.PEM"));
        crate::allow::keep(&store, &root, |a| a.add(&g)).unwrap();
        let kept = crate::allow::Store::read(Some(&store)).get(&root);
        let w = crate::tools::with_allowed(
            &crate::llm::LlmConfig::default(),
            &crate::caps::Caps::answered(&[&kept]),
            None,
        );
        let mut p = key_policy(&root);
        w.onto(&mut p);
        assert_eq!(p.keys, Some(vec![root.join("deep/er/Server.PEM")]));
        let (ok, out, err) = sh_under(&p, &root, "cat deep/er/Server.PEM");
        assert!(ok && out == "tarae-fake-token", "{err}");
        for f in [".env", "deep/er/other.key"] {
            let (ok, _, err) = sh_under(&p, &root, &format!("cat {f}"));
            assert!(!ok && err.contains(KEY_DENIED), "{f}: {err}");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on Linux (7b): a file a run makes in a folder holding a file named like a key — that folder's
    /// own rule, all but reading files (Landlock grants only: a rule that let the new file be read would let the
    /// key be) — is written but can't be read until the next run, whose rules give it its own; in a folder
    /// holding none it's read at once. So is one it replaces there — a new file under the old name, a save by
    /// rename's — though it was read before; and a file opened to read and write as it's made there (`sed -i`'s
    /// temporary, `mkstemp`) is refused, so `sed -i` changes nothing. The report names those folders, says all
    /// three.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_file_made_or_replaced_beside_a_key_is_read_from_the_next_run() {
        let test = "a_file_made_or_replaced_beside_a_key_is_read_from_the_next_run";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("later");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, false) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        let script = "printf made > new.txt && printf made > src/new.txt && cat src/new.txt && cat new.txt";
        let (ok, out, err) = sh_under(&p, &root, script);
        assert!(!ok && out == "made" && err.contains(KEY_DENIED), "{out} {err}");
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "made", "written");
        let (ok, out, err) = sh_under(&p, &root, "cat new.txt");
        assert!(ok && out == "made", "the next run: {err}");
        // Replaced by a rename over it, read just before: not read again in that run
        let script = "cat .env.example && printf replaced > t && mv t .env.example && cat .env.example";
        let (ok, out, err) = sh_under(&p, &root, script);
        assert!(!ok && out == "tarae-fake-token" && err.contains(KEY_DENIED), "{out} {err}");
        let (ok, out, err) = sh_under(&p, &root, "cat .env.example");
        assert!(ok && out == "replaced", "the next run: {out} {err}");
        // `sed -i`: its temporary file, opened to read and write as it's made, is refused — the file stays as it was
        let (ok, _, err) = sh_under(&p, &root, "sed -i s/fake/replaced/ id_ed25519.pub");
        assert!(!ok && err.contains("temporary file") && err.contains(KEY_DENIED), "{err}");
        let (ok, out, err) = sh_under(&p, &root, "cat id_ed25519.pub");
        assert!(ok && out == "tarae-fake-token", "unchanged: {out} {err}");
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let (_, notes) = c.linux_said().unwrap_or_else(|| panic!("{:?}", c.confine));
        let said = "A file it makes or replaces in the project's top folder, deep/, deep/er/ (a save by rename, mv \
                    over it) can't be read until its next command, and one it opens there to read and write as it \
                    makes it is refused (sed -i's temporary file, mkstemp's): on Linux a folder holding a file \
                    named like a key can't let what's new in it be read";
        assert!(notes.contains(said), "{notes}");
        // (root in a user namespace: why not mounts, said — 7d)
        let namespaced = matches!(&c.confine, Some(Ok(c)) if ["A+", "A", "B+", "B"].contains(&c.tier()));
        // SAFETY: a plain call.
        let root = unsafe { libc::geteuid() } == 0;
        let why = "not hidden by mounts of the run's own: tarae runs as root";
        assert_eq!(notes.contains(why), namespaced && root, "{notes}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// Linux: whether a run under `p` here has the files named like keys hidden by mounts (7d: its keeper in a user
    /// namespace) — where that's not what `test` needs (`mounts`), it says it skips itself and why, and true.
    #[cfg(not(target_os = "macos"))]
    fn skipped_for_mounts(p: &Policy, test: &str, mounts: bool) -> bool {
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let (here, tier) = match &c.confine {
            Some(Ok(c)) => (!c.covers.keys.is_empty(), c.tier()),
            _ => (false, "none"),
        };
        if here != mounts {
            // SAFETY: a plain call.
            let namespaced = ["A+", "A", "B+", "B"].contains(&tier);
            let why = match unsafe { libc::geteuid() } {
                0 if namespaced => {
                    "tarae runs as root, which can't map itself into a run's own user namespace".into()
                }
                _ => tier::get().short.clone().unwrap_or_default(),
            };
            // (a machine said to give namespaces fails instead where it doesn't: keys not hidden there isn't a skip)
            if let Ok(want) = std::env::var("TARAE_EXPECT_TIER")
                && ["A+", "A", "B+", "B"].contains(&want.trim())
                && want.trim() != tier
            {
                panic!("{test}: tier {want} expected here, the runs are {tier} — {why}");
            }
            let how = match here {
                true => "hidden by mounts in the run's own mount namespace (7d)",
                false => "closed by their folders' rules (7b)",
            };
            let skip = crate::test_results::SELF_SKIP;
            let line = format!("{skip}sandbox::tests::{test} — keys here are {how}, tier {tier} {why}\n");
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        }
        here != mounts
    }

    /// Whether `err` says one of `said`.
    #[cfg(not(target_os = "macos"))]
    fn says_one(err: &str, said: &[&str]) -> bool {
        said.iter().any(|s| err.contains(s))
    }

    /// For real, on Linux where a run's keeper is in a user namespace (7d): its folders whole, a file is made,
    /// replaced by a rename and edited in place (`sed -i`'s temporary file) beside a key and read at once, which 7b's
    /// rules can't give (`a_file_made_or_replaced_beside_a_key_is_read_from_the_next_run`). The key, hidden by a
    /// mount, isn't read, written, made readable, renamed, removed, replaced or linked — each refused as the report
    /// says — and stays as it was on disk.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn beside_a_hidden_key_files_are_made_and_read_at_once() {
        let test = "beside_a_hidden_key_files_are_made_and_read_at_once";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("mounted");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, true) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        let script =
            "printf made > new.txt && cat new.txt && printf made > deep/er/new.txt && cat deep/er/new.txt";
        let (ok, out, err) = sh_under(&p, &root, script);
        assert!(ok && out == "mademade", "made beside a key: {out} {err}");
        let script = "cat .env.example && printf replaced > t && mv t .env.example && cat .env.example";
        let (ok, out, err) = sh_under(&p, &root, script);
        assert!(ok && out == "tarae-fake-tokenreplaced", "a save by rename: {out} {err}");
        let (ok, out, err) =
            sh_under(&p, &root, "sed -i s/fake/replaced/ id_ed25519.pub && cat id_ed25519.pub");
        assert!(ok && out == "tarae-replaced-token", "sed -i: {out} {err}");
        let (refused, busy) = (["Permission denied", "Read-only file system"], ["Device or resource busy"]);
        for (script, said) in [
            ("cat .env", &refused[..1]),
            ("cat deep/er/Server.PEM", &refused[..1]),
            ("printf x > .env", &refused[..]),
            ("printf x >> .env", &refused[..]),
            ("chmod 644 .env && cat .env", &refused[1..]),
            ("mv .env moved", &busy[..]),
            ("rm -f .env", &busy[..]),
            ("printf x > t2 && mv t2 .env", &busy[..]),
            ("ln .env src/h", &["Invalid cross-device link"][..]),
        ] {
            let (ok, out, err) = sh_under(&p, &root, script);
            assert!(
                !ok && !out.contains("tarae-fake-token") && says_one(&err, said),
                "{script}: {out} {err}"
            );
        }
        assert_eq!(std::fs::read_to_string(root.join(".env")).unwrap(), "tarae-fake-token", "as it was");
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let (_, notes) = c.linux_said().unwrap_or_else(|| panic!("{:?}", c.confine));
        assert!(notes.contains("hidden by a mount of the run's own"), "{notes}");
        assert!(!notes.contains("can't be read until its next command"), "no folder split: {notes}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, where keys are hidden by mounts (7d): the run can't take a mount back — not by unmounting it
    /// (lazily too), remounting it writable, mounting over it, nor from a user and mount namespace it makes itself
    /// (where it holds every capability): it has none in its own, Landlock refuses a confined process every mount
    /// call, and in a namespace it makes the mounts come locked. After each try the key is still refused.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_run_cant_take_a_keys_mount_back() {
        let test = "a_run_cant_take_a_keys_mount_back";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("unmount");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, true) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        for undo in [
            "umount .env",
            "umount -l .env",
            "mount -o remount,bind,rw .env",
            "mount --bind src/env.rs .env",
            "unshare -Urm sh -c 'umount -l .env && cat .env'",
            "unshare -Urm sh -c 'mount -o remount,bind,rw .env && chmod 644 .env && cat .env'",
        ] {
            let script = format!("{undo} && echo UNDONE; cat .env");
            let (ok, out, err) = sh_under(&p, &root, &script);
            assert!(
                !ok && !out.contains("UNDONE") && !out.contains("tarae-fake-token"),
                "{undo}: {out} {err}"
            );
            assert!(err.contains(KEY_DENIED), "{undo}: still refused after — {err}");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on Linux (7b, 7d): a hard link to a key made before the run — under any name, in a folder holding
    /// no key, in tools' output — is closed as the key is: the walk finds a key's other names by its inode,
    /// everywhere in the project. One outside the project it can't see: the report says so, for that key alone.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_hard_link_to_a_key_made_earlier_is_closed_too() {
        if !keys_tested_here("a_hard_link_to_a_key_made_earlier_is_closed_too") {
            return;
        }
        let (t, root) = key_files("linked");
        std::fs::hard_link(root.join(".env"), root.join("src/notes.txt")).unwrap();
        // (in tools' output too, where names aren't looked at)
        std::fs::hard_link(root.join(".env"), root.join("node_modules/pkg/notes.txt")).unwrap();
        let p = key_policy(&root);
        for f in ["src/notes.txt", "node_modules/pkg/notes.txt"] {
            let (ok, out, err) = sh_under(&p, &root, &format!("cat {f}"));
            assert!(!ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED), "{f}: {out} {err}");
        }
        let (ok, out, err) = sh_under(&p, &root, "cat src/env.rs");
        assert!(ok && out == "tarae-fake-token", "beside it: {err}");
        // A name outside the project: tarae can't see it, and says so
        let said = "deep/er/Server.PEM has another name tarae can't see; it may be readable there.";
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let (_, notes) = c.linux_said().unwrap_or_else(|| panic!("{:?}", c.confine));
        assert!(!notes.contains("another name tarae can't see"), "{notes}");
        std::fs::hard_link(root.join("deep/er/Server.PEM"), t.join("outside.txt")).unwrap();
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let (_, notes) = c.linux_said().unwrap_or_else(|| panic!("{:?}", c.confine));
        assert!(notes.contains(said) && !notes.contains(".env has another name"), "{notes}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, where keys are hidden by mounts (7d): `COVERS_MAX` of them are, each — what that costs a run, said
    /// against the same project closing no key (`keys: None`: each key's checks and its mount); one more and they're
    /// closed by their folders' rules (7b), the report saying so.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn many_keys_are_hidden_at_once() {
        let test = "many_keys_are_hidden_at_once";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("many");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, true) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        // (the fastest of five, rules made and run: the same project closing no key, then hiding every one)
        let timed = |p: &Policy, script: &str| {
            let runs = (0..5).map(|_| {
                let at = std::time::Instant::now();
                let ran = sh_under(p, &root, script);
                (at.elapsed(), ran)
            });
            runs.min_by_key(|(took, _)| *took).unwrap()
        };
        std::fs::create_dir_all(root.join("k")).unwrap();
        // (the project's own two, deep and at the top, are hidden too)
        for i in 0..linux::COVERS_MAX - 2 {
            std::fs::write(root.join(format!("k/{i}.pem")), "tarae-fake-token").unwrap();
        }
        let script = "cat src/env.rs >/dev/null; cat k/0.pem";
        let (none, _) = timed(&Policy { keys: None, ..p.clone() }, script);
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let Some(Ok(c)) = &c.confine else { panic!("{:?}", c.confine) };
        assert_eq!(c.covers.keys.len(), linux::COVERS_MAX);
        let (many, (ok, out, err)) = timed(&p, script);
        assert!(!ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED), "{out} {err}");
        let (_, (ok, out, err)) = timed(&p, &format!("cat k/{}.pem", linux::COVERS_MAX - 3));
        assert!(
            !ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED),
            "the last: {out} {err}"
        );
        let per = many.saturating_sub(none) / linux::COVERS_MAX as u32;
        // (written past libtest's capture: the number is what this test is for)
        let said = format!(
            "{test}: the same run closing no key {none:?}, hiding {} {many:?} — {per:?} a key\n",
            linux::COVERS_MAX
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), said.as_bytes());
        // One past: the folders' rules
        std::fs::write(root.join("k/past.pem"), "tarae-fake-token").unwrap();
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let Some(Ok(c)) = &c.confine else { panic!("{:?}", c.confine) };
        assert!(c.covers.keys.is_empty() && c.unread.contains(&root.join("k")), "{:?}", c.unread);
        let (ok, out, err) = sh_under(&p, &root, "cat k/past.pem");
        assert!(!ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED), "past: {out} {err}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, where keys are hidden by mounts (7d): a key replaced by a rename, or moved away, between the walk
    /// that found it and its run's start doesn't start the run — the child mounts only over the file the walk found
    /// (device, inode), and the run says why; nothing of the new file or the moved one is read.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_key_swapped_after_the_walk_stops_its_run() {
        let test = "a_key_swapped_after_the_walk_stops_its_run";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("swapped");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, true) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        let said = "hiding the files named like keys";
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        std::fs::write(root.join("t"), "tarae-fake-token, new").unwrap();
        std::fs::rename(root.join("t"), root.join(".env")).unwrap();
        let (ok, out, err) = sh_confined(&c, &root, "cat .env");
        let stale = format!("errno {}", libc::ESTALE);
        assert!(!ok && out.is_empty() && err.contains(said) && err.contains(&stale), "replaced: {out} {err}");
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        std::fs::rename(root.join(".env"), root.join("src/moved")).unwrap();
        let (ok, out, err) = sh_confined(&c, &root, "cat src/moved");
        assert!(!ok && out.is_empty() && err.contains(said), "moved away: {out} {err}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, where keys are hidden by mounts (7d): the shut file swapped for a link after tarae checked it (here
    /// to the key itself, which mounted over its own place would read) doesn't start the run — the child opens it
    /// through no link and mounts only the file that was checked. (Its run folder is the test's own: the shut file
    /// is its instance's, which other tests' runs share.)
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_shut_file_swapped_after_its_check_stops_the_run() {
        let test = "a_shut_file_swapped_after_its_check_stops_the_run";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("shut");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, true) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        let c = confined(Policy { run: RunDir::new_in(&t.join("runs")).ok(), ..p.clone() });
        let Some(Ok(conf)) = &c.confine else { panic!("{:?}", c.confine) };
        let shut = conf.covers.shut.clone().expect("a shut file").path;
        assert!(shut.starts_with(&t), "the test's own: {}", shut.display());
        std::fs::remove_file(&shut).unwrap();
        std::os::unix::fs::symlink(root.join(".env"), &shut).unwrap();
        let (ok, out, err) = sh_confined(&c, &root, "cat .env");
        let said = "hiding the files named like keys";
        assert!(!ok && !out.contains("tarae-fake-token") && err.contains(said), "{out} {err}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on Linux (7b, 7d): a key reached through another mount of its filesystem — here a second bind of
    /// the project, made in a user and mount namespace of the test binary's own before its exec, as a desktop's
    /// would be there already — isn't read there: its path through that mount is closed as its own is (a file beside
    /// no key reads there as in the project — Landlock's rules are its folders' inodes'), and the keys are closed by
    /// their folders' rules, not mounts (the project's folder rule would open them through it), said in the report.
    /// The run happens in that namespace (`a_key_reached_through_another_mount_inside`).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_key_reached_through_another_mount_is_closed_there() {
        let test = "a_key_reached_through_another_mount_is_closed_there";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("elsewhere");
        let p = key_policy(&root);
        if skipped_for_mounts(&p, test, true) {
            return std::fs::remove_dir_all(&t).unwrap_or_default();
        }
        let second = t.join("second");
        std::fs::create_dir_all(&second).unwrap();
        let inside = "sandbox::tests::a_key_reached_through_another_mount_inside";
        let env = [("TARAE_TEST_ROOT", root.as_os_str()), ("TARAE_TEST_SECOND", second.as_os_str())];
        let said = with_a_second_bind(inside, Some((&root, &second)), &env).unwrap_or_else(|e| panic!("{e}"));
        assert!(said.contains("1 passed"), "{said}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// Linux: the test `inside` (an ignored one) run by the test binary with `env` — with `bind` (from, to), in a user
    /// and mount namespace of its own where `from` is bound at `to` too before its exec, as a desktop's second mount
    /// would be there already (a test never mounts in its own process). Its output once it passed; Err: the namespace
    /// or the bind refused here (`Refused`, why), or the test failed (its output).
    #[cfg(not(target_os = "macos"))]
    fn with_a_second_bind(
        inside: &str,
        bind: Option<(&Path, &Path)>,
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Result<String, SecondBind> {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", inside, "--ignored", "--test-threads=1", "--nocapture"])
            .envs(env.iter().copied());
        let ran = |out: std::io::Result<std::process::Output>| {
            let out = out.map_err(|e| SecondBind::Refused(e.to_string()))?;
            let said =
                format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            match out.status.success() {
                true => Ok(said),
                false => Err(SecondBind::Failed(said)),
            }
        };
        let Some((from, to)) = bind else { return ran(cmd.output()) };
        let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        let (from, to) = (c(from), c(to));
        // SAFETY: plain calls.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let maps = [
            (c"/proc/self/setgroups", b"deny".to_vec()),
            (c"/proc/self/uid_map", format!("{uid} {uid} 1\n").into_bytes()),
            (c"/proc/self/gid_map", format!("{gid} {gid} 1\n").into_bytes()),
        ];
        // SAFETY: raw calls on the child's own namespaces and strings made before the fork.
        unsafe {
            cmd.pre_exec(move || {
                let fail = || Err(std::io::Error::last_os_error());
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    return fail();
                }
                libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
                for (file, text) in &maps {
                    let fd = libc::open(file.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if fd < 0 || libc::write(fd, text.as_ptr().cast(), text.len()) != text.len() as isize {
                        return fail();
                    }
                    libc::close(fd);
                }
                let null = std::ptr::null();
                if libc::mount(from.as_ptr(), to.as_ptr(), null, libc::MS_BIND, null.cast()) != 0 {
                    return fail();
                }
                Ok(())
            })
        };
        ran(cmd.output())
    }

    /// Why `with_a_second_bind` didn't pass.
    #[cfg(not(target_os = "macos"))]
    #[derive(Debug)]
    enum SecondBind {
        /// The namespace or the bind refused here (a container without the capability, an AppArmor rule).
        Refused(String),
        /// The test inside failed: its output.
        Failed(String),
    }

    #[cfg(not(target_os = "macos"))]
    impl std::fmt::Display for SecondBind {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                SecondBind::Refused(why) => write!(f, "no second bind here: {why}"),
                SecondBind::Failed(said) => write!(f, "{said}"),
            }
        }
    }

    /// For real, on Linux in every tier (7e): home reached through another mount of its filesystem — toolbox's host
    /// root at `/run/host` — isn't read there, as it isn't at its own place: Landlock's rules hold inodes, and what
    /// holds that mount is granted whole by `/`'s rule, so home's path through it is a hole too. What else that mount
    /// and its folder hold still reads, and the project in home reads through either way (its own rule). Two second
    /// mounts, each where it can be made: a bind in a namespace of the test binary's own (`with_a_second_bind`: a
    /// stand-in `run/host/home/u` in the test's own folder) — refused in C/D, whose containers can't make one — and
    /// the checkout's target volume mounted again by `cargo xtask linux` (`TARAE_TEST_SECOND_MOUNT`, any tier). Each
    /// sees its break too: with the other ways not looked at (`TARAE_TEST_NO_HOLES_ELSEWHERE`, in its child) the fake
    /// key is read through the second mount — so a pass is the reads refused, not the report alone.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_home_reached_through_another_mount_is_closed_there() {
        let test = "a_home_reached_through_another_mount_is_closed_there";
        if !keys_tested_here(test) {
            return;
        }
        // (a skip only when neither second mount could be had)
        let skip = |why: &str| {
            let line = format!("{}sandbox::tests::{test} — {why}\n", crate::test_results::SELF_SKIP);
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        };
        let mut refused = None;
        // (a folder of its own in a volume other runs of the checkout may share)
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let ours = format!("tarae-sb-home-{}-{nanos}", std::process::id());
        // A bind in a namespace of the test's own: the fake home at the stand-in for `/run/host/home/u`
        let t = profile_path(&std::env::temp_dir()).join(&ours);
        let (home, second) = home_files(&t.join("home"), &t.join("run/host/home/u"));
        // (the run as it is passes; broken, the key reads through the second mount)
        let check = |home: &Path, second: &Path, bind: Option<(&Path, &Path)>| -> Result<(), SecondBind> {
            let inside = "sandbox::tests::a_home_reached_through_another_mount_inside";
            let env = [("TARAE_TEST_HOME", home.as_os_str()), ("TARAE_TEST_SECOND", second.as_os_str())];
            let said = with_a_second_bind(inside, bind, &env)?;
            assert!(said.contains("1 passed"), "{said}");
            let broken = [&env[..], &[("TARAE_TEST_NO_HOLES_ELSEWHERE", std::ffi::OsStr::new("1"))]].concat();
            match with_a_second_bind(inside, bind, &broken) {
                Err(SecondBind::Failed(said)) => {
                    assert!(said.contains(READ_THROUGH) && said.contains("tarae-fake-token"), "{said}")
                }
                other => panic!("broken, the key isn't read through {}: {other:?}", second.display()),
            }
            Ok(())
        };
        match check(&home, &second, Some((&home, &second))) {
            Ok(()) => {}
            // (where a tier with user namespaces is said to be, a namespace of the test's own can be made: refused is
            // a failure there; C/D's containers refuse it — the second mount below is theirs)
            Err(SecondBind::Refused(why))
                if !std::env::var("TARAE_EXPECT_TIER")
                    .is_ok_and(|t| ["A+", "A", "B+", "B"].contains(&t.trim())) =>
            {
                refused = Some(why)
            }
            Err(e) => panic!("{e}"),
        }
        std::fs::remove_dir_all(&t).ok();
        // The target volume mounted twice by `cargo xtask linux`: the fake home in it, reached at the other mount
        let Some((one, two)) = std::env::var("TARAE_TEST_SECOND_MOUNT").ok().and_then(|v| {
            let (one, two) = v.split_once(':')?;
            Some((PathBuf::from(one), PathBuf::from(two)))
        }) else {
            if let Some(why) = refused {
                skip(&format!(
                    "no bind of its own ({why}), and no second mount given (`cargo xtask linux` gives one)"
                ));
            }
            return;
        };
        let (home, second) = home_files(&one.join(&ours).join("home"), &two.join(&ours).join("home"));
        check(&home, &second, None).unwrap_or_else(|e| panic!("{e}"));
        std::fs::remove_dir_all(one.join(&ours)).ok();
    }

    /// What `home_through_second_mount` says when a run read home's key through the second mount.
    #[cfg(not(target_os = "macos"))]
    const READ_THROUGH: &str = "home's key read through the second mount";

    /// A fake home at `home` for `a_home_reached_through_another_mount_is_closed_there` — a key and a note (each a
    /// fake token) and a project — and beside it in its folder, and in `second`'s folder, files of the mount's
    /// that are no one's home (`run-text`); `second` made, to bind at where none is mounted yet. Real paths.
    #[cfg(not(target_os = "macos"))]
    fn home_files(home: &Path, second: &Path) -> (PathBuf, PathBuf) {
        let beside = |at: &Path| at.parent().unwrap().join("other.txt");
        for (f, text) in [
            (home.join(".ssh/id_ed25519"), "tarae-fake-token"),
            (home.join("notes.txt"), "tarae-fake-token"),
            (home.join("project/src/main.rs"), "project-text"),
            (beside(home), "run-text"),
            (beside(second), "run-text"),
        ] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, text).unwrap();
        }
        std::fs::create_dir_all(second).ok();
        let real = |p: &Path| std::fs::canonicalize(p).unwrap();
        (real(home), real(second.parent().unwrap()).join(second.file_name().unwrap()))
    }

    /// The other half of `a_home_reached_through_another_mount_is_closed_there`: in its namespace, the fake home
    /// `TARAE_TEST_HOME` bound at `TARAE_TEST_SECOND` too.
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore = "run by a_home_reached_through_another_mount_is_closed_there, in a mount namespace of its own"]
    fn a_home_reached_through_another_mount_inside() {
        let (Some(home), Some(second)) =
            (std::env::var_os("TARAE_TEST_HOME"), std::env::var_os("TARAE_TEST_SECOND"))
        else {
            return;
        };
        home_through_second_mount(Path::new(&home), Path::new(&second));
    }

    /// A run whose home is the fake `home` (`home_files`), reached at `second` through another mount: no file of home
    /// read nor its folder listed either way, what's beside it in both mounts read, the project read either way, the
    /// mount said in the report.
    #[cfg(not(target_os = "macos"))]
    fn home_through_second_mount(home: &Path, second: &Path) {
        let root = home.join("project");
        let mut p = key_policy(&root);
        p.reads.home = Some(home.to_path_buf());
        // (the fake home sits where runs may write — the temp folder, the target folder: that grant would read it
        // whatever `/`'s does, as it would a real home there)
        let (a, b) = (home.parent().unwrap(), second.parent().unwrap());
        p.write.retain(|w| !a.starts_with(w) && !b.starts_with(w));
        p.files.retain(|f| !f.starts_with(a) && !f.starts_with(b));
        let c = confined(Policy { run: RunDir::new().ok(), ..p });
        let Some(Ok(conf)) = &c.confine else { panic!("{:?}", c.confine) };
        let (h, s) = (home.display(), second.display());
        // (the reads first: what a run gets is what this test is for — the key through the second mount first)
        let (ok, out, err) = sh_confined(&c, &root, &format!("cat {s}/.ssh/id_ed25519"));
        assert!(
            !ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED),
            "{READ_THROUGH}: {out} {err}"
        );
        for script in [format!("cat {h}/.ssh/id_ed25519"), format!("cat {s}/notes.txt"), format!("ls {s}")] {
            let (ok, out, err) = sh_confined(&c, &root, &script);
            assert!(
                !ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED),
                "{script}: {out} {err}"
            );
        }
        for (script, want) in [
            (format!("cat {}/other.txt", a.display()), "run-text"),
            (format!("cat {}/other.txt", b.display()), "run-text"),
            ("cat src/main.rs".to_string(), "project-text"),
            (format!("cat {s}/project/src/main.rs"), "project-text"),
        ] {
            let (ok, out, err) = sh_confined(&c, &root, &script);
            assert!(ok && out == want, "{script}: {out} {err}");
        }
        assert!(conf.holes_elsewhere.iter().any(|m| second.starts_with(m)), "{:?}", conf.holes_elsewhere);
        let (_, notes) = c.linux_said().unwrap_or_else(|| panic!("{:?}", c.confine));
        assert!(notes.contains("through another mount of its filesystem"), "{notes}");
    }

    /// The other half of `a_key_reached_through_another_mount_is_closed_there`: in its namespace, the project bound
    /// at `TARAE_TEST_SECOND` too.
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore = "run by a_key_reached_through_another_mount_is_closed_there, in a mount namespace of its own"]
    fn a_key_reached_through_another_mount_inside() {
        let (Some(root), Some(second)) =
            (std::env::var_os("TARAE_TEST_ROOT"), std::env::var_os("TARAE_TEST_SECOND"))
        else {
            return;
        };
        let (root, second) = (PathBuf::from(root), PathBuf::from(second));
        let p = key_policy(&root);
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let Some(Ok(conf)) = &c.confine else { panic!("{:?}", c.confine) };
        // (the reads first: what a run gets is what this test is for)
        let s = second.display();
        for script in ["cat .env".to_string(), format!("cat {s}/.env"), format!("cat {s}/deep/er/Server.PEM")]
        {
            let (ok, out, err) = sh_confined(&c, &root, &script);
            assert!(
                !ok && !out.contains("tarae-fake-token") && err.contains(KEY_DENIED),
                "{script}: {out} {err}"
            );
        }
        assert_eq!(conf.elsewhere, std::slice::from_ref(&second), "the second bind closed");
        assert!(conf.covers.keys.is_empty(), "no mounts: {:?}", conf.covers);
        let why = conf.split_why.clone().unwrap_or_default();
        assert!(why.contains(&second.display().to_string()), "{why}");
        // (a file beside no key reads both ways: Landlock's rule is its folder's inode's, whichever path)
        for script in ["cat src/env.rs".to_string(), format!("cat {s}/src/env.rs")] {
            let (ok, out, err) = sh_confined(&c, &root, &script);
            assert!(ok && out == "tarae-fake-token", "{script}: {err}");
        }
        let (_, notes) = c.linux_said().unwrap_or_else(|| panic!("{:?}", c.confine));
        assert!(
            notes.contains("through another mount of their filesystem: they're closed there too"),
            "{notes}"
        );
    }

    /// For real, on Linux with a PID namespace of the session's (A+, B+): a run sees only its session's processes —
    /// not its keeper by tarae's number for it, nor tarae — and no process's `/proc/<pid>/root` (its keeper's mount
    /// namespace holds no mount over a key; another run's has its own) takes it to a key: Landlock keeps a run's
    /// reach into other processes to its own domain, and the keeper isn't dumpable.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_run_reaches_no_key_through_another_process_root() {
        let test = "a_run_reaches_no_key_through_another_process_root";
        if !keys_tested_here(test) {
            return;
        }
        let (t, root) = key_files("roots");
        let p = key_policy(&root);
        let c = confined(Policy { run: RunDir::new().ok(), ..p.clone() });
        let Some(Ok(conf)) = &c.confine else { panic!("{:?}", c.confine) };
        let keeper = conf.keeper.as_ref().map(|k| k.pid);
        let script = format!(
            "for d in /proc/[0-9]*; do echo \"pid ${{d#/proc/}}\"; cat \"$d/root{}/.env\" 2>/dev/null; done; true",
            root.display()
        );
        let (_, out, err) = sh_confined(&c, &root, &script);
        assert!(!out.contains("tarae-fake-token"), "{out} {err}");
        let pids: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("pid ")).collect();
        if conf.keeper.as_ref().is_some_and(|k| k.ns.pid) {
            let (keeper, tarae) = (keeper.unwrap().to_string(), std::process::id().to_string());
            assert!(pids.contains(&"1"), "its keeper, as 1: {pids:?}");
            assert!(
                !pids.contains(&keeper.as_str()) && !pids.contains(&tarae.as_str()),
                "{keeper} {tarae}: {pids:?}"
            );
        } else {
            let line = format!(
                "{}sandbox::tests::{test} — no PID namespace here: only its /proc/<pid>/root reads checked\n",
                crate::test_results::SELF_SKIP
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on Linux: a run holds no descriptor but its stdio — not one its child had open without close-on-exec
    /// (the test build's hook opens the project's folder so, before the child's own mount namespace: through it a
    /// run would reach past the mounts over keys) — `keeper::child`'s `close_range`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_run_holds_no_descriptor_but_its_stdio() {
        if !keys_tested_here("a_run_holds_no_descriptor_but_its_stdio") {
            return;
        }
        let (t, root) = key_files("fds");
        let p = confined(Policy { run: RunDir::new().ok(), ..key_policy(&root) });
        // (the program the run execs lists its own: past its stdio only the folder it lists them from)
        let leak = [("TARAE_TEST_LEAK_FD", "1")];
        let (ok, out, err) = sh_confined_env(&p, &root, "exec ls -l /proc/self/fd/", &leak);
        let fds: Vec<(&str, &str)> = out
            .lines()
            .filter_map(|l| l.split_once(" -> "))
            .filter_map(|(l, to)| Some((l.rsplit(' ').next()?, to)))
            .collect();
        let others: Vec<&(&str, &str)> =
            fds.iter().filter(|(n, to)| !["0", "1", "2"].contains(n) && !to.ends_with("/fd")).collect();
        assert!(ok && fds.len() >= 3 && others.is_empty(), "{fds:?} {err}");
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
                runs: None,
                tag: None,
                proxy: None,
                supervised: Os::HOST == Os::Linux,
                redirected: false,
                egress: Default::default(),
                run_id: 0,
                session: 0,
                watch: None,
                granted: Vec::new(),
                sockets: Vec::new(),
                terminals: false,
                keys: None,
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
    pub(super) fn scratch(name: &str) -> PathBuf {
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
            runs: None,
            tag: Some(tag.to_string()),
            proxy: None,
            supervised: Os::HOST == Os::Linux,
            redirected: false,
            egress: Default::default(),
            run_id: 0,
            session: 0,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            terminals: false,
            keys: None,
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
            runs: None,
            tag: None,
            proxy: None,
            supervised: Os::HOST == Os::Linux,
            redirected: false,
            egress: Default::default(),
            run_id: 0,
            session: 0,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            terminals: false,
            keys: None,
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
        let scratch = crate::scratch::Scratch::new("sb-granted");
        let t = profile_path(&scratch);
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
        let w = crate::tools::with_allowed(
            &crate::llm::LlmConfig::default(),
            &crate::caps::Caps::answered(&[&from_store, &this_chat]),
            None,
        );
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
        let scratch = crate::scratch::Scratch::new("sb-ui");
        let t = profile_path(&scratch);
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
    /// can (cargo links its outputs). Claude reads what was kept and to edit it, which asks the user
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
        // What Claude reads: what was kept, and to edit it with its edit tool
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
        assert!(
            r.contains("build.rs: tarae keeps it from your commands")
                && r.contains("edit it with your edit tool"),
            "{r}"
        );
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
        let p = policy(&root, &llm, &[]).unwrap();
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
    /// - the network open (step 5d: beyond goes through tarae's proxy): as closed — an ordinary port isn't
    ///   reached, a risky one (`RISKY_PORTS`) only when listed.
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
            runs: None,
            tag: None,
            proxy: None,
            supervised: Os::HOST == Os::Linux,
            redirected: false,
            egress: Default::default(),
            run_id: 0,
            session: 0,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            terminals: false,
            keys: None,
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
        assert!(!reached(&open, &ordinary), "network open: an ordinary port");
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
        let scratch = crate::scratch::Scratch::new("runenv-reads");
        let t = scratch.to_path_buf();
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
    /// (`env_in`); a policy made by hand gets nothing of it.
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
        let scratch = crate::scratch::Scratch::new("runenv-e2e");
        let dir = scratch.to_path_buf();
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
        let p = policy(root, &crate::llm::LlmConfig::default(), &[]).unwrap();
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
        std::fs::write(dir.join("said"), p.run_env.said(&mut Vec::new(), Os::HOST).unwrap_or_default())
            .unwrap();
    }

    /// 9c: inside a sandboxed run `TARAE_SANDBOX_STATUS` is its tier — Linux: `<tier>:<ABI>` as the run's guarantees
    /// say; macOS: `seatbelt` — even where tarae itself was started with one (an outer sandbox's: tarae's own tests
    /// run by Claude), which the run's filter drops; a user's run gets nothing of tarae's making (only what tarae
    /// was started with). In a process of its own, the outer value in its environment (`status_in_its_own_process`).
    #[test]
    fn a_run_knows_its_tier() {
        #[cfg(target_os = "macos")]
        if skipped_inside("sandbox::tests::a_run_knows_its_tier") {
            return;
        }
        #[cfg(target_os = "linux")]
        if !crate::landlock::abi().is_ok_and(|n| n >= 2) {
            let line = format!(
                "{}sandbox::tests::a_run_knows_its_tier — needs Landlock ABI 2\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        let scratch = crate::scratch::Scratch::new("status");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sandbox::tests::status_in_its_own_process", "--ignored", "--nocapture"])
            .env("TARAE_STATUS_PROBE", &dir)
            .env(STATUS, "outer")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && said.contains("1 passed"), "the child ran it: {said}");
        let read =
            |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("{f}: {e}\n{said}"));
        let (run, want, user) = (read("run"), read("want"), read("user"));
        #[cfg(target_os = "macos")]
        assert_eq!(want, "seatbelt");
        #[cfg(target_os = "linux")]
        {
            let (tier, abi) = want.split_once(':').unwrap_or_else(|| panic!("<tier>:<ABI>: {want}"));
            assert!(["A+", "A", "B+", "B", "C", "D"].contains(&tier) && abi.parse::<u32>().is_ok(), "{want}");
        }
        assert_eq!(run, want, "the run's own tier, not the outer one nor none");
        assert_eq!(user, "outer", "a user's run: only what tarae was started with");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A run's `TARAE_SANDBOX_STATUS` is tarae's alone: where tarae was started with one (an outer sandbox's), it
    /// isn't said left out of the run's environment — the run gets its own.
    #[test]
    fn a_runs_status_isnt_said_left_out() {
        let here = Here { env: [(STATUS.to_string(), "outer".into())].into(), ..Here::default() };
        let e = run_env_of(&here, &crate::llm::LlmConfig::default());
        assert!(!e.left_out().any(|n| n == STATUS), "{e:?}");
        assert_eq!(e.var(STATUS), None, "not passed either");
    }

    /// A policy without guarantees (a Linux one whose rules weren't made) gives its run no status — not even an
    /// outer one the user's `llm.sandbox-env` passes; one with guarantees gives its own over it.
    #[test]
    fn a_run_without_guarantees_has_no_status() {
        let here = Here { env: [(STATUS.to_string(), "outer".into())].into(), ..Here::default() };
        let llm = crate::llm::LlmConfig { sandbox_env: vec![STATUS.into()], ..Default::default() };
        let run_env = run_env_of(&here, &llm);
        assert_eq!(run_env.var(STATUS).as_deref(), Some("outer"), "passed by the setting");
        let p = Policy { run_env, ..Policy::for_project(&env(Path::new("/Users/u/p"))) };
        let status = |p: &Policy| {
            let c = command(Some(p), "/bin/true", &[]);
            c.get_envs().find(|(k, _)| *k == STATUS).map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let none = Policy { supervised: true, ..p.clone() };
        assert_eq!(none.guarantees(), None);
        assert_eq!(status(&none), Some(None), "removed");
        let seatbelt = Policy { supervised: false, ..p };
        assert_eq!(status(&seatbelt), Some(Some("seatbelt".into())), "its own over the outer one");
    }

    /// `a_run_knows_its_tier`'s child: the policy for this checkout, a run in it and a user's run (no policy) each
    /// saying its `TARAE_SANDBOX_STATUS` (`unset` when none), and what the run's guarantees say — into
    /// `$TARAE_STATUS_PROBE`.
    #[test]
    #[ignore = "run by a_run_knows_its_tier in a process of its own"]
    fn status_in_its_own_process() {
        let Some(dir) = std::env::var_os("TARAE_STATUS_PROBE").map(PathBuf::from) else { return };
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let p = policy(root, &crate::llm::LlmConfig::default(), &[]).unwrap();
        let script = ["-c".to_string(), format!("printf %s \"${{{STATUS}-unset}}\"")];
        let said = |p: Option<&Policy>| {
            let mut cmd = command(p, "/bin/sh", &script);
            cmd.current_dir(root);
            let s = spawn(p, &cmd).expect("the run starts");
            let mut out = String::new();
            io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
            crate::spawn::wait(s.pid).unwrap();
            out
        };
        std::fs::write(dir.join("run"), said(Some(&p))).unwrap();
        std::fs::write(dir.join("user"), said(None)).unwrap();
        let want = p.guarantees().map(|g| g.status()).unwrap_or_else(|| "no guarantees".into());
        std::fs::write(dir.join("want"), want).unwrap();
    }

    /// A toolchain's probe (`output_in`) runs through the session's keeper, confined: its parent is the keeper
    /// `keeper::of` lists for the project, and it can't write the project's `Cargo.toml`. (The project under the
    /// build's `target/`, not in `/tmp`: the policy writes there.)
    #[cfg(target_os = "linux")]
    #[test]
    fn a_probe_runs_through_the_sessions_keeper() {
        if !crate::landlock::abi().is_ok_and(|n| n >= 2) {
            let line = format!(
                "{}sandbox::tests::a_probe_runs_through_the_sessions_keeper — no Landlock here\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        let base =
            std::env::current_dir().unwrap().join(format!("target/tarae-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("Cargo.toml"), "[workspace]\n").unwrap();
        let root = std::fs::canonicalize(&base).unwrap();
        let env = Env {
            root: root.clone(),
            tmp: Some(profile_path(&std::env::temp_dir())),
            run_env: run_env_of(&Here::now(), &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        let p = Policy::for_project(&env);
        let script = r#"if printf x >> "$R/Cargo.toml" 2>/dev/null; then w=yes; else w=no; fi
            printf '{"ppid": %s, "wrote": "%s"}' "$PPID" "$w""#;
        let r = root.to_string_lossy().into_owned();
        let probe = output_in(&p, "/bin/sh", &["-c", script], &[("R", &r)]).expect("the probe answers");
        // (its `$PPID` is the keeper's number as its runs see it: 1 inside a PID namespace)
        let keepers: Vec<u64> =
            keeper::of(Some(&root)).iter().map(|k| u64::from(keeper::seen_by_runs(k))).collect();
        assert_eq!(keepers, [probe["ppid"].as_u64().unwrap()], "forked by the session's keeper: {probe}");
        assert_eq!(probe["wrote"], "no", "confined: {probe}");
        assert_eq!(std::fs::read_to_string(root.join("Cargo.toml")).unwrap(), "[workspace]\n");
        end_leftovers(Some(&root));
        std::fs::remove_dir_all(&root).ok();
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
