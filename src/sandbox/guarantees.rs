//! What a run's sandbox keeps apart from other tarae sessions' commands (and the user's other programs) and what it
//! shares (keeper step 9a, `docs/design/sandbox-keeper.md` §Showing it) — one value a run: built where its keeper is
//! known (`linux::confine`, `Guarantees::landlock`) or from its Seatbelt policy (`Guarantees::seatbelt`), never kept
//! by session (a policy that changes gets another keeper, D14). Its words are made here only: the state line
//! (`pairs`), one sentence for Claude (`sentence`), a fallback's reason (`why`) and the report's sentences of what's
//! shared (`shared_said` — the run's report reads them). Pure and built everywhere: its golden
//! (`sandbox_guarantees.txt`) says every tier on any machine.
//!
//! A keeper's namespaces (`Ns`), its tier (`Tier`) and how a run's sockets go (`RunNet`) are here for that — reading
//! them from a process and making them are `tier.rs`'s.

/// A keeper's namespaces: none (tiers C/D), or the session's — `user` with its ipc and mount namespaces,
/// `pid` (and a fresh `/proc`) where it mounts, `net` (only `lo`, up) where the policy opens nothing, and a
/// `/dev/shm` of its own (`shm`) but where the project or a place it writes is in this system's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ns {
    pub user: bool,
    pub net: bool,
    pub pid: bool,
    pub shm: bool,
}

/// How a run's internet sockets go (`Ns::run_net`): one answer its Landlock rules (`linux::confine`) and its
/// seccomp mode (the keeper's run) both take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunNet {
    /// Without a network namespace of its own (tier C): tarae makes its internet sockets, bound to `lo`, and
    /// every call naming an address (`seccomp::Mode::Served`); Landlock handles TCP and UDP.
    Served,
    /// In the network namespace its keeper made: its internet sockets its own, every call naming an address
    /// tarae's, routed (`Mode::Routed`, `sandbox::route`); Landlock handles TCP and UDP (tarae's calls on its
    /// sockets aren't the run's).
    Routed,
    /// Not served, in that network namespace, holding only `lo` (`Mode::OwnNetwork`); Landlock leaves TCP and UDP
    /// to it (`Ruleset::new_own_network`).
    Own,
    /// None (`Mode::Plain`); Landlock handles TCP and UDP.
    Plain,
    /// Served where tarae can't take a run's descriptors (`pidfd_getfd` refused — Docker's default profile; 6f′):
    /// tarae makes every socket the run has, its unix and netlink ones too, and keeps a copy of each
    /// (`seccomp::Mode::Copied`, `supervisor::Copies`) — never in a network namespace (its sockets are tarae's). Landlock
    /// handles TCP and UDP.
    Copied,
}

impl Ns {
    pub const NONE: Ns = Ns { user: false, net: false, pid: false, shm: false };

    /// The session's namespaces: user, ipc and mount, a `/dev/shm` of its own — `net` and `pid` as said.
    pub fn session(net: bool, pid: bool) -> Ns {
        Ns { user: true, net, pid, shm: true }
    }

    /// Whether these make a network namespace (`net` without `user` makes none: `tier::Ns::clone_flags`).
    pub fn netns(self) -> bool {
        self.user && self.net
    }

    /// How the internet sockets of a run of a keeper born with these go, `served` or not — by copies where tarae
    /// can't take a run's descriptors (`copies`: every socket tarae's, whatever the namespaces) — decided once, in
    /// `linux::confine` (`Confine.net`).
    pub fn run_net(self, served: bool, copies: bool) -> RunNet {
        match (served, self.netns()) {
            (true, _) if copies => RunNet::Copied,
            (true, true) => RunNet::Routed,
            (true, false) => RunNet::Served,
            (false, true) => RunNet::Own,
            (false, false) => RunNet::Plain,
        }
    }

    /// Whether a run of a keeper born with these, its sockets going as `net` says, gets only the ptys it makes
    /// (3b, `sandbox::ptys`): without a `devpts` of the session's own (no `user`: C/D) and served — `D`'s runs
    /// (`Plain`: no supervisor) keep this system's `/dev/pts`, said. One answer for its rules (`linux::confine`)
    /// and its filter (the keeper's run).
    pub fn serves_ptys(self, net: RunNet) -> bool {
        !self.user && matches!(net, RunNet::Served | RunNet::Copied)
    }
}

/// What the session's namespaces without a network one are (N1, 2026-10-14; only a keeper served by copies is born
/// so): the keeper's own words, nothing of its runs — whether a run's loopback is kept from other sessions' is the
/// run's (served or not), its guarantees' to say (the `Loopback` row).
pub const NO_NETNS: &str = "no network namespace of its own: its loopback is this machine's";

/// A tier (§Tiers): the namespaces, and whether tarae serves the runs' calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    APlus,
    A,
    BPlus,
    B,
    C,
    D,
}

impl Tier {
    /// The tier of a keeper with `ns` — `net` aside: a policy that opens something has none until routing —
    /// served or not.
    pub fn of(ns: Ns, served: bool) -> Tier {
        match (ns.user, ns.pid, served) {
            (true, true, true) => Tier::APlus,
            (true, false, true) => Tier::A,
            (true, true, false) => Tier::BPlus,
            (true, false, false) => Tier::B,
            (false, _, true) => Tier::C,
            (false, _, false) => Tier::D,
        }
    }

    pub fn said(self) -> &'static str {
        match self {
            Tier::APlus => "A+",
            Tier::A => "A",
            Tier::BPlus => "B+",
            Tier::B => "B",
            Tier::C => "C",
            Tier::D => "D",
        }
    }
}

/// A part of what a run could reach of another session's commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// What it writes (the temp folders are every session's, D16).
    Files,
    /// The other processes in sight: their command lines read, their priority lowered.
    Processes,
    /// Signals to processes outside its own (Landlock's scope from ABI 6; Seatbelt's `same-sandbox`).
    Signals,
    /// System V IPC (macOS: its shared memory).
    Ipc,
    /// POSIX shared memory — Linux's `/dev/shm`.
    Shm,
    /// The terminals (`/dev/pts`).
    Ptys,
    /// What listens on the loopback it has.
    Loopback,
}

/// One part, kept apart or shared, and how in a few words (empty: nothing to add).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub part: Part,
    pub kept: bool,
    pub how: String,
}

impl Row {
    fn new(part: Part, kept: bool, how: &str) -> Row {
        Row { part, kept, how: how.to_string() }
    }
}

/// What enforces it: Seatbelt, or Landlock at a tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Seatbelt,
    Landlock { tier: Tier, abi: u32, net: RunNet },
}

/// How what the policy opens beyond this machine goes (Q5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    /// Taken in the session's network namespace by its routes and owned table (TPROXY, step 6).
    Tproxy,
    /// To tarae's proxy, by the variable a run gets (macOS: at its own port).
    Proxy,
    /// To tarae's proxy, every socket the run has made by tarae (6f′: `pidfd_getfd` refused).
    Copies,
    /// Nowhere: not served, a run makes no socket that leaves its namespace (B+, B, D).
    NoSocket,
}

/// What a run's sandbox guarantees (the module's doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guarantees {
    pub by: By,
    /// Every part said, in `Part`'s order — a part not here isn't claimed either way.
    pub rows: Vec<Row>,
    /// Whether the policy opens the network, and how it goes.
    pub network: bool,
    pub way: Way,
    /// Why not the top tier — the probe's, the supervisor's, no TPROXY's, Landlock's — None at the top.
    pub why: Option<String>,
    /// The ptys a run holds at a time where it opens only those it makes (`ptys::POOL`).
    pool: Option<usize>,
}

/// What `linux::confine` has of a run's keeper and this system, for `Guarantees::landlock`.
#[derive(Clone, Debug)]
pub struct Found {
    /// The keeper's namespaces (`Keeper.ns`, read from its links at birth).
    pub ns: Ns,
    /// Whether tarae serves its runs' calls — or why not (`supervisor::unavailable`).
    pub served: Result<(), String>,
    /// Served by tarae's copies of a run's sockets (`supervisor::copies`).
    pub copies: bool,
    pub abi: u32,
    /// Where its runs open only the ptys they make (3b, `Ns::serves_ptys`): how many at a time.
    pub ptys: Option<usize>,
    /// The keeper takes its runs' connections in the session's network (TPROXY) — or why not.
    pub tproxy: Result<(), String>,
    /// The families its `lo` couldn't carry (`Keeper.lo_missing`).
    pub lo_missing: Vec<&'static str>,
    /// The probe's why not all namespaces (`tier::Probed.short`).
    pub short: Option<String>,
    /// The policy opens the network.
    pub network: bool,
}

/// The loopback of a run served without a network namespace of its own (C, by copies): this machine's — another
/// session's socket on it refused (step 8).
const THEIRS_REFUSED: &str = "this machine's, other sessions' sockets refused";

/// The terminals of a run that opens only those it makes (3b).
const ONLY_MADE: &str = "only those it makes";

/// What a run of a keeper without a PID namespace of its own sees of other processes, and does (`two_sessions_
/// cant_reach_each_others_processes`).
const PROCESSES_SAID: &str = " Here a command sees this system's processes — another session's commands and the \
     user's others: it can read their command lines and lower their priority, never their environment, nor trace \
     them.";

/// What Seatbelt can't keep apart (step 7, the user's decision 2026-10-10: shared, and said): shared memory, POSIX and
/// System V — no rule names who made a segment — and `setpriority`, which no operation guards
/// (`process-info-setcontrol` doesn't: measured). The command line and starting environment are said once a
/// conversation (`runenv::OTHERS_READ`). Pinned by `apart::two_sessions_cant_reach_each_others_{processes,sockets}`.
const MAC_SHARED: &str = " Here a command shares System V and POSIX shared memory with other sessions' commands \
     and the user's other programs — it can read and write what they keep there — and can lower their priority \
     (renice): macOS's sandbox has no rule for either.";

/// Where a Seatbelt policy opens this machine's ports: what listens on them is reached whoever's it is (step 8, user
/// decision 9: Linux refuses another session's listener there, macOS's sandbox can't tell whose one is).
const MAC_PORTS: &str = " What listens on this machine's ports it opens is reached whoever's it is — another tarae \
     session's commands' too: macOS's sandbox can't tell whose a listener is.";

impl Guarantees {
    /// A Landlock run's, from what its keeper is (`Found`).
    pub fn landlock(f: Found) -> Guarantees {
        let served = f.served.is_ok();
        let net = f.ns.run_net(served, f.copies);
        let tier = Tier::of(f.ns, served);
        let lo = match f.lo_missing.is_empty() {
            true => String::new(),
            false => format!(", without an {} address", f.lo_missing.join(" or ")),
        };
        let loopback = match net {
            RunNet::Routed | RunNet::Own => format!("its own{lo}"),
            RunNet::Served | RunNet::Copied => THEIRS_REFUSED.to_string(),
            RunNet::Plain => "no socket".to_string(),
        };
        let pool = (!f.ns.user).then_some(f.ptys).flatten();
        let pid = f.ns.user && f.ns.pid;
        // What Landlock gains by ABI, row by row — only two rows turn on it:
        // - files: truncating is controlled from 3 (`linux::availability`'s partial): below it any file of the user's can
        //   be emptied, another session's too;
        // - signals: scoped to the run's own domain from 6 — below it, in the session's own PID namespace other
        //   sessions are out of sight (its own other commands not); without one, shared.
        // The rest are tarae's at any ABI: abstract unix names (scoped from 6) the supervisor refuses outside the
        // session's network namespace, and a run not served makes no unix socket (`seccomp`); TCP (4), unix sockets by
        // path (9) and UDP (10) are the supervisor's to decide, every call; device ioctls (5) reach only devices the run
        // can open — the terminals are their own row, decided by what it may open.
        let files = match f.abi >= 3 {
            true => Row::new(Part::Files, true, "but the temp folders"),
            false => Row::new(Part::Files, false, "any can be emptied: no truncating rule below ABI 3"),
        };
        let signals = match (f.abi >= 6, pid) {
            (true, _) => Row::new(Part::Signals, true, ""),
            (false, true) => {
                Row::new(Part::Signals, true, "out of sight — within its session, reached below ABI 6")
            }
            (false, false) => Row::new(Part::Signals, false, "no signal scope below ABI 6"),
        };
        let rows = vec![
            files,
            Row::new(Part::Processes, pid, if pid { "" } else { "command lines, priority" }),
            signals,
            Row::new(Part::Ipc, f.ns.user, ""),
            Row::new(Part::Shm, f.ns.user && f.ns.shm, ""),
            Row::new(Part::Ptys, f.ns.user || pool.is_some(), if pool.is_some() { ONLY_MADE } else { "" }),
            Row { part: Part::Loopback, kept: true, how: loopback },
        ];
        let way = match net {
            RunNet::Plain | RunNet::Own => Way::NoSocket,
            RunNet::Copied => Way::Copies,
            RunNet::Routed | RunNet::Served if f.tproxy.is_ok() => Way::Tproxy,
            RunNet::Routed | RunNet::Served => Way::Proxy,
        };
        let mut why: Vec<String> = f.short.into_iter().collect();
        if let Err(w) = &f.served {
            why.push(format!("tarae can't serve its calls: {w}"));
        }
        if net == RunNet::Copied {
            why.push("served by tarae's copies of its sockets: pidfd_getfd refused".into());
        }
        if let (Way::Proxy, Err(w), true) = (way, &f.tproxy, f.network) {
            why.push(format!("no TPROXY: {w}"));
        }
        if f.abi < 3 {
            why.push(format!("Landlock ABI {}: partial, every run asks", f.abi));
        }
        Guarantees {
            by: By::Landlock { tier, abi: f.abi, net },
            rows,
            network: f.network,
            way,
            why: (!why.is_empty()).then(|| why.join("; ")),
            pool,
        }
    }

    /// A Seatbelt run's: what it keeps (`OWN_PROCESSES`: signals, details, a debugger's attach) and shares
    /// (`MAC_SHARED`) — and this machine's ports, where `ports` (the policy opens some: llm.sandbox-ports,
    /// -localhost), whoever listens on them.
    pub fn seatbelt(ports: bool, network: bool) -> Guarantees {
        let loopback = if ports { "the ports it opens, whoever's" } else { "none of its ports open" };
        Guarantees {
            by: By::Seatbelt,
            rows: vec![
                Row::new(Part::Files, true, "but the temp folders"),
                Row::new(Part::Processes, false, "command lines, priority"),
                Row::new(Part::Signals, true, ""),
                Row::new(Part::Ipc, false, ""),
                Row::new(Part::Shm, false, ""),
                Row::new(Part::Loopback, !ports, loopback),
            ],
            network,
            way: Way::Proxy,
            why: None,
            pool: None,
        }
    }

    /// Its tier — None: Seatbelt's (no tiers).
    pub fn tier(&self) -> Option<Tier> {
        match self.by {
            By::Landlock { tier, .. } => Some(tier),
            By::Seatbelt => None,
        }
    }

    /// Whether `part` is kept apart — None: not said.
    pub fn kept(&self, part: Part) -> Option<bool> {
        self.rows.iter().find(|r| r.part == part).map(|r| r.kept)
    }
}

/// Its words for the places that show it — Claude's report (9b), `tarae state` (9d), the panel and `:procs` (9e).
#[cfg_attr(all(target_os = "linux", not(test)), expect(dead_code, reason = "the state line: 9d"))]
impl Guarantees {
    /// The parts it shares, in order.
    pub fn shared(&self) -> Vec<Part> {
        self.rows.iter().filter(|r| !r.kept).map(|r| r.part).collect()
    }

    /// A part's name, as this system has it.
    pub fn name(&self, part: Part) -> &'static str {
        match (part, self.by) {
            (Part::Files, _) => "files",
            (Part::Processes, _) => "processes",
            (Part::Signals, _) => "signals",
            (Part::Ipc, By::Seatbelt) => "System V shared memory",
            (Part::Ipc, _) => "System V IPC",
            (Part::Shm, By::Seatbelt) => "POSIX shared memory",
            (Part::Shm, _) => "/dev/shm",
            (Part::Ptys, _) => "terminals",
            (Part::Loopback, _) => "loopback",
        }
    }

    /// The state line's parts, key and meaning: the tier and Landlock's ABI (or Seatbelt), the loopback, the
    /// network, and what's shared.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut v = match self.by {
            By::Landlock { tier, abi, .. } => vec![
                ("tier", format!("{}{}", tier.said(), if abi < 3 { " partial" } else { "" })),
                ("Landlock", format!("ABI {abi}")),
            ],
            By::Seatbelt => vec![("sandbox", "Seatbelt".to_string())],
        };
        let loopback = self.rows.iter().find(|r| r.part == Part::Loopback).map(|r| r.how.clone());
        v.extend(loopback.map(|l| ("loopback", l)));
        let network = match (self.way, self.network) {
            (Way::NoSocket, _) => "no socket",
            (_, false) => "closed",
            (Way::Tproxy, true) => "TPROXY",
            (Way::Proxy, true) => "tarae's proxy",
            (Way::Copies, true) => "tarae's proxy, by copies",
        };
        v.push(("network", network.to_string()));
        let shared: Vec<&str> = self.shared().into_iter().map(|p| self.name(p)).collect();
        v.push(("shared", if shared.is_empty() { "nothing".into() } else { shared.join(", ") }));
        v
    }

    /// The state line: `pairs`, a ` · ` between.
    pub fn line(&self) -> String {
        self.pairs().iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · ")
    }

    /// One sentence for Claude: the tier, what's kept apart from other sessions' commands and what's shared.
    pub fn sentence(&self) -> String {
        let head = match self.by {
            By::Landlock { tier, abi, .. } => {
                format!("Tier {}{} (Landlock ABI {abi})", tier.said(), if abi < 3 { ", partial" } else { "" })
            }
            By::Seatbelt => "Seatbelt".to_string(),
        };
        let list = |kept: bool| -> String {
            let v: Vec<String> = self
                .rows
                .iter()
                .filter(|r| r.kept == kept)
                .map(|r| match r.how.is_empty() {
                    true => self.name(r.part).to_string(),
                    false => format!("{} ({})", self.name(r.part), r.how),
                })
                .collect();
            if v.is_empty() { "nothing".into() } else { v.join(", ") }
        };
        format!(
            "{head}: kept apart from other tarae sessions' commands — {}; shared with them — {}.",
            list(true),
            list(false)
        )
    }
}

impl Guarantees {
    /// What Claude's run report says of it, after the rules, once a conversation and again when it changes (9b,
    /// decision 11 and N2): the sentence, then what's shared in the report's words.
    pub fn told(&self) -> String {
        format!("{}{}", self.sentence(), self.shared_said())
    }

    /// What its runs share, in the report's words (said after the sentence: `told`): Landlock below
    /// ABI 6, the processes in sight without a PID namespace of the session's own, this system's System V IPC,
    /// `/dev/shm` and terminals without the session's namespaces (served, only the terminals it makes), `/dev/shm`
    /// alone where the session has none of its own; Seatbelt's shared memory and priority, and the ports it opens.
    pub fn shared_said(&self) -> String {
        let shared = |p: Part| self.kept(p) == Some(false);
        let abi = match self.by {
            By::Seatbelt => {
                let mem = if shared(Part::Ipc) || shared(Part::Shm) { MAC_SHARED } else { "" };
                let ports = if shared(Part::Loopback) { MAC_PORTS } else { "" };
                return format!("{mem}{ports}");
            }
            By::Landlock { abi, .. } => abi,
        };
        // (said below 6 whatever the row: its session's own other commands are reached in any tier)
        let mut s = match abi < 6 {
            true => format!(
                " This kernel's Landlock (ABI {abi}) can't keep a command's signals to its own processes (6 does)."
            ),
            false => String::new(),
        };
        if shared(Part::Files) {
            s.push_str(&format!(
                " This kernel's Landlock (ABI {abi}) can't control truncating files: a command could empty any file of \
                 the user's — another session's too."
            ));
        }
        if shared(Part::Processes) {
            s.push_str(PROCESSES_SAID);
        }
        match (shared(Part::Ipc), shared(Part::Ptys)) {
            (true, true) => s.push_str(
                " Here a command shares this system's System V IPC, /dev/shm and terminals (/dev/pts) with other \
                 sessions' commands and the user's other programs: it can read and write the shared memory they \
                 keep there and open any of those terminals — the user's own too, not only other sessions'.",
            ),
            (true, false) => {
                s.push_str(
                    " Here a command shares this system's System V IPC and /dev/shm with other sessions' commands \
                     and the user's other programs: it can read and write the shared memory they keep there.",
                );
                if let Some(n) = self.pool {
                    s.push_str(&format!(
                        " Of the terminals (/dev/pts) it opens only those it makes, up to {n} at a time."
                    ));
                }
            }
            (false, _) if shared(Part::Shm) => s.push_str(
                " Here a command shares this system's /dev/shm with other sessions' commands and the user's other \
                 programs: it can read and write the files they keep there.",
            ),
            (false, _) => {}
        }
        s
    }
}

/// Tests: a part tried more than one way (the processes: a command line read, a priority lowered) — reached where any
/// try reached it, refused where every try named was refused, out of sight (None) only where none was named.
#[cfg(test)]
pub fn reached_by_any(tries: &[Option<bool>]) -> Option<bool> {
    tries.iter().flatten().copied().reduce(|a, b| a || b)
}

/// Tests: what is said shared is what is found shared (9a) — for each part a two-session test tried, `found`: whether
/// it reached the other session's (None: out of sight — kept). Each part's row must say what was found. What's
/// wrong, in words.
#[cfg(test)]
pub fn said_as_found(g: &Guarantees, found: &[(Part, Option<bool>)]) -> Vec<String> {
    let mut wrong = Vec::new();
    for &(part, reached) in found {
        let said_shared = g.kept(part) == Some(false);
        if said_shared != reached.unwrap_or(false) {
            let found = match reached {
                Some(true) => "reached",
                Some(false) => "refused",
                None => "out of sight",
            };
            wrong.push(format!(
                "{part:?}: {found}, and the guarantees say it's {}",
                if said_shared { "shared" } else { "kept apart (or say nothing)" }
            ));
        }
    }
    wrong
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A part tried two ways is reached when either try reached it — one out of sight doesn't hide the other's
    /// reach (the processes: a command line read where a priority couldn't be named) — refused when every named try
    /// was, and out of sight only when neither was named.
    #[test]
    fn a_part_is_reached_by_any_try() {
        let (reached, refused, unseen) = (Some(true), Some(false), None);
        assert_eq!(reached_by_any(&[reached, unseen]), reached, "one try unnamed, the other reached");
        assert_eq!(reached_by_any(&[unseen, reached]), reached);
        assert_eq!(reached_by_any(&[refused, reached]), reached);
        assert_eq!(reached_by_any(&[refused, unseen]), refused);
        assert_eq!(reached_by_any(&[refused, refused]), refused);
        assert_eq!(reached_by_any(&[unseen, unseen]), unseen, "out of sight only when neither was named");
    }
}
