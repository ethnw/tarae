//! What a run's sandbox keeps apart from other tarae sessions' commands (and the user's other programs) and what it
//! shares (keeper step 9a, `docs/design/sandbox-keeper.md` §Showing it) — one value a run: built where its keeper is
//! known (`linux::confine`, `Guarantees::landlock`) or from its Seatbelt policy (`Guarantees::seatbelt`), never kept
//! by session for a run (a policy that changes gets another keeper, D14) — the session's copy is only shown
//! (`Session.sandbox`). Its words are made here only: the state line
//! (`pairs`), one sentence for Claude (`sentence`), a fallback's reason (`why`) and the report's sentences of what's
//! shared (`shared_said` — the run's report reads them), and `tarae state`'s object (`json`, 9d: the session keeps its
//! last run's — `Session.sandbox`). Pure and built everywhere: its golden (`sandbox_guarantees.txt`) says every tier
//! on any machine.
//!
//! A keeper's namespaces (`Ns`), its tier (`Tier`) and how a run's sockets go (`RunNet`) are here for that — reading
//! them from a process and making them are `tier.rs`'s.

/// A keeper's namespaces: none (tiers host and offline), or the session's — `user` with its ipc and mount namespaces,
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
    /// Without a network namespace of its own (tier host): tarae makes its internet sockets, bound to `lo`, and
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

    /// How the internet sockets of a run of a keeper born with these go, `served` or not — decided once, in
    /// `linux::confine` (`Confine.net`).
    pub fn run_net(self, served: bool) -> RunNet {
        match (served, self.netns()) {
            (true, true) => RunNet::Routed,
            (true, false) => RunNet::Served,
            (false, true) => RunNet::Own,
            (false, false) => RunNet::Plain,
        }
    }

    /// Whether a run of a keeper born with these, its sockets going as `net` says, gets only the ptys it makes
    /// (3b, `sandbox::ptys`): without a `devpts` of the session's own (no `user`) and served — offline runs without
    /// namespaces (`Plain`: no supervisor) keep this system's `/dev/pts`, said. One answer for its rules (`linux::confine`)
    /// and its filter (the keeper's run).
    pub fn serves_ptys(self, net: RunNet) -> bool {
        !self.user && net == RunNet::Served
    }
}

/// Why tarae can't serve its runs' calls where `pidfd_getfd` is refused (`supervisor::decided`) — the way out said
/// with it. Then the session is tier offline: no network, no port on this machine, no unix socket.
pub const PIDFD_REFUSED: &str =
    "pidfd_getfd refused (Docker's default profile) — --cap-add SYS_PTRACE serves the calls";

/// `PIDFD_REFUSED` in the sandbox panel's few words (`Guarantees.brief`): two parts, each a row where it's narrow.
const PIDFD_REFUSED_BRIEF: &str = "not served: pidfd_getfd refused · --cap-add SYS_PTRACE serves the calls";

/// Why tarae can't serve its runs' calls under Yama's `ptrace_scope` 2 where the session has no namespaces of its own
/// (`supervisor::served_from`): Yama lets tarae read only the runs in a user namespace it made — the way out said with
/// it. Then the session is tier offline; with the namespaces, tier own.
pub const YAMA_2_WITHOUT_NS: &str = "Yama's ptrace_scope 2 lets tarae read a run's calls only in namespaces it made \
     for the session, and this session has none — with them its runs are served";

/// `YAMA_2_WITHOUT_NS` in the sandbox panel's few words: two parts, each a row where it's narrow.
const YAMA_2_WITHOUT_NS_BRIEF: &str =
    "not served: Yama's ptrace_scope 2 · the session's namespaces serve the calls";

/// A tier (§Tiers): whether the keeper has the session's namespaces, and whether tarae serves the runs' calls —
/// what a user can tell apart. The rest (a PID namespace, how the network goes) is the rows' and `why`'s to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The session's own namespaces, served.
    Own,
    /// This machine's namespaces, served.
    Host,
    /// Not served: a run reaches no network, with the session's namespaces or without.
    Offline,
}

impl Tier {
    /// The tier of a keeper with `ns`, served or not.
    pub fn of(ns: Ns, served: bool) -> Tier {
        match (served, ns.user) {
            (true, true) => Tier::Own,
            (true, false) => Tier::Host,
            (false, _) => Tier::Offline,
        }
    }

    /// Its name: the state line's, `TARAE_SANDBOX_STATUS`'s, the log's.
    pub fn said(self) -> &'static str {
        match self {
            Tier::Own => "own",
            Tier::Host => "host",
            Tier::Offline => "offline",
        }
    }

    /// The head of Claude's sentence (`Guarantees::sentence`).
    fn head(self) -> &'static str {
        match self {
            Tier::Own => "Own namespaces",
            Tier::Host => "Host namespaces",
            Tier::Offline => "Offline",
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

impl Part {
    /// Its name in `tarae state` (`Guarantees::json`) — the same on every system (`Guarantees::name` is the
    /// system's).
    pub fn key(self) -> &'static str {
        match self {
            Part::Files => "files",
            Part::Processes => "processes",
            Part::Signals => "signals",
            Part::Ipc => "ipc",
            Part::Shm => "shm",
            Part::Ptys => "ptys",
            Part::Loopback => "loopback",
        }
    }
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
    /// To tarae's proxy, by the variable a run gets (macOS: at its own port).
    Proxy,
    /// Nowhere: not served (tier offline), a run makes no socket that leaves its namespace.
    NoSocket,
}

impl Way {
    /// Its name in `tarae state`.
    pub fn key(self) -> &'static str {
        match self {
            Way::Proxy => "proxy",
            Way::NoSocket => "no-socket",
        }
    }
}

/// How a run would get namespaces of its own (9f; 6h's `tier::hint`): where Ubuntu's restriction kept a keeper's
/// namespaces from a tarae under no profile — the whole hint: why, then the step or why there's none
/// (`apparmor::hint`) — and the step alone, the shell line the sandbox panel copies (None for a tarae no profile
/// should name: `apparmor::step`) — and where that tarae is, so a run's own tier can have its words
/// (`Guarantees::landlock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub said: String,
    pub step: Option<String>,
    pub place: super::apparmor::Place,
}

impl Hint {
    /// The hint for runs in `tier`, for the tarae at `place`.
    pub fn of(tier: Tier, place: &super::apparmor::Place) -> Hint {
        let said = super::apparmor::hint(tier.said(), place);
        Hint { said, step: super::apparmor::step(place), place: place.clone() }
    }
}

/// What a run's sandbox guarantees (the module's doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guarantees {
    pub by: By,
    /// Its keeper's namespaces, as read at its birth (`Found.ns`) — None: Seatbelt's (no keeper).
    pub ns: Option<Ns>,
    /// Every part said, in `Part`'s order — a part not here isn't claimed either way.
    pub rows: Vec<Row>,
    /// Whether the policy opens the network, and how it goes.
    pub network: bool,
    /// That network is each host asked on a card — `llm.sandbox-network` off (piece C, `Policy.asks_hosts`).
    pub asks: bool,
    /// That network open, only the hosts `llm.sandbox-hosts` lists reached unasked — by default the packs'
    /// registries (C1b, `!Egress.any`); others asked on a card. Neither this nor `asks`: any host.
    pub listed: bool,
    pub way: Way,
    /// What's missing — the probe's, the supervisor's, Landlock's — None where nothing is.
    pub why: Option<String>,
    /// The same in a few words, ` · ` between (the sandbox panel, 9e, under the tier): `AppArmor blocks user
    /// namespaces · Landlock ABI 2: partial` — `why` is the report's and `tarae state`'s.
    pub brief: Option<String>,
    /// How the tier would go up — the profile's (9f, `Found.hint`); None elsewhere, and on macOS.
    pub hint: Option<Hint>,
    /// The ptys a run holds at a time where it opens only those it makes (`ptys::POOL`).
    pool: Option<usize>,
}

/// What `linux::confine` has of a run's keeper and this system, for `Guarantees::landlock`.
#[derive(Clone, Debug)]
pub struct Found {
    /// The keeper's namespaces (`Keeper.ns`, read from its links at birth).
    pub ns: Ns,
    /// Whether tarae serves its runs' calls — or why not (`supervisor::served`, for this keeper's namespaces).
    pub served: Result<(), String>,
    pub abi: u32,
    /// Where its runs open only the ptys they make (3b, `Ns::serves_ptys`): how many at a time.
    pub ptys: Option<usize>,
    /// The families its `lo` couldn't carry (`Keeper.lo_missing`).
    pub lo_missing: Vec<&'static str>,
    /// The probe's why not all namespaces (`tier::Probed.short`).
    pub short: Option<String>,
    /// The same in a few words (`tier::Probed.brief`).
    pub brief: Option<String>,
    /// The probe's hint (`tier::Probed.hint`) — said again for this run's own tier: the probe's was its own,
    /// and a run can be served where the probe wasn't, or not where it was.
    pub hint: Option<Hint>,
    /// The policy opens the network.
    pub network: bool,
}

/// The loopback of a run served without a network namespace of its own (host): this machine's — another
/// session's socket on it refused (step 8).
const THEIRS_REFUSED: &str = "this machine's, other sessions' sockets refused";

/// The same in the sandbox panel's line (`panel_pairs`).
const THEIRS_SHORT: &str = "this machine's, other sessions' refused";

/// The profile's step in a few words, beside its row in the sandbox panel (9f).
pub const STEP_SAID: &str = "needs sudo, then restart tarae";

/// What the profile would give the runs, in the sandbox panel under the why (`hint_brief`).
const HINT_BRIEF: &str = "tarae's AppArmor profile would give own namespaces";

/// The sandbox panel's row that copies the profile's step (`step_row`).
const STEP_ROW: &str = "copy the step for own namespaces";

/// The terminals of a run that opens only those it makes (3b).
const ONLY_MADE: &str = "only those it makes";

/// What a run of a keeper without a PID namespace of its own sees of other processes, and does (`two_sessions_
/// cant_reach_each_others_processes`).
const PROCESSES_SAID: &str = " Here a command sees this system's processes — another session's commands and the \
     user's others: it can read their command lines and lower their priority, never their environment, nor trace \
     them.";

/// What Seatbelt can't keep apart and the profile opens (as srt's): POSIX shared memory — no rule names who made a
/// name. (System V's and `setpriority` its default refuses, a run's own too: macdeny.) Pinned by
/// `apart::two_sessions_cant_reach_each_others_sockets`.
const MAC_SHARED: &str = " Here a command shares POSIX shared memory with other sessions' commands and the user's \
     other programs — it can read and write what they keep there: macOS's sandbox can't tell whose a name is.";

/// What a Landlock run may change of a file's attributes where served (own, host — linmeta B, D-h): Landlock has no
/// right for them, so tarae serves its mode, times and extended attribute calls (`seccomp::serves_attr`), and allows
/// them only where the run may write, or through a file it holds open for writing (what Landlock let it write) — an
/// owner call too, but only one that changes nothing (sandbox C); a change of owner, of flags, and the `security.` and
/// `trusted.` attributes (a program's capabilities, a security module's labels) are refused in every tier
/// (`seccomp::ATTR_CALLS`, `supervisor::attrs`: D2 B).
const ATTRS_SERVED: &str = " A command changes a file's mode, times and extended attributes (ACLs too) only where it \
     may write, or through a file it holds open for writing; never a file's owner or flags, nor its security or \
     trusted attributes — in the project neither.";

/// What an unserved Landlock run may change of a file's attributes (tier offline stays a full tier with this said:
/// user decision 2026-10-18): nothing serves its calls, so mode and times stay the run's.
const ATTRS_OFFLINE: &str = " Here a command can change the mode and times of any file of the user's it can name — \
     never what's in it: Landlock has no right for them. Never a file's owner, extended attributes, ACLs or flags — \
     in the project neither.";

/// Where a Seatbelt policy opens this machine's ports: what listens on them is reached whoever's it is (step 8, user
/// decision 9: Linux refuses another session's listener there, macOS's sandbox can't tell whose one is).
const MAC_PORTS: &str = " What listens on this machine's ports it opens is reached whoever's it is — another tarae \
     session's commands' too: macOS's sandbox can't tell whose a listener is.";

impl Guarantees {
    /// A Landlock run's, from what its keeper is (`Found`).
    pub fn landlock(f: Found) -> Guarantees {
        let served = f.served.is_ok();
        let net = f.ns.run_net(served);
        let tier = Tier::of(f.ns, served);
        let lo = match f.lo_missing.is_empty() {
            true => String::new(),
            false => format!(", without an {} address", f.lo_missing.join(" or ")),
        };
        let loopback = match net {
            RunNet::Routed | RunNet::Own => format!("its own{lo}"),
            RunNet::Served => THEIRS_REFUSED.to_string(),
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
            RunNet::Routed | RunNet::Served => Way::Proxy,
        };
        let mut why: Vec<String> = f.short.into_iter().collect();
        if let Err(w) = &f.served {
            why.push(format!("tarae can't serve its calls: {w}"));
        }
        if f.abi < 3 {
            why.push(format!("Landlock ABI {}: partial, every run asks", f.abi));
        }
        // (the same, each in a few words — the probe's own first; the tier is said beside it already)
        let mut brief: Vec<String> = f.brief.into_iter().collect();
        match &f.served {
            Err(w) if w == PIDFD_REFUSED => brief.push(PIDFD_REFUSED_BRIEF.into()),
            Err(w) if w == YAMA_2_WITHOUT_NS => brief.push(YAMA_2_WITHOUT_NS_BRIEF.into()),
            Err(w) => brief.push(format!("not served: {w}")),
            Ok(()) => {}
        }
        if f.abi < 3 {
            brief.push(format!("Landlock ABI {}: partial", f.abi));
        }
        Guarantees {
            by: By::Landlock { tier, abi: f.abi, net },
            ns: Some(f.ns),
            rows,
            network: f.network,
            asks: false,
            listed: false,
            way,
            why: (!why.is_empty()).then(|| why.join("; ")),
            brief: (!brief.is_empty()).then(|| brief.join(" · ")),
            hint: f.hint.map(|h| Hint::of(tier, &h.place)),
            pool,
        }
    }

    /// A Seatbelt run's: what it keeps (`OWN_PROCESSES`: signals, details, a debugger's attach — and under the
    /// profile's default, other processes' command lines and starting environments, `KERN_PROCARGS2`; the default
    /// itself: System V's shared memory and every process's priority, `ipc-sysv-*`, `system-sched`, its own too;
    /// macdeny, measured) and shares: POSIX shared memory (`BASE` opens it, as srt — no rule tells whose a name
    /// is: `MAC_SHARED`) and this machine's ports, where `ports` (the policy opens some: llm.sandbox-ports,
    /// -localhost), whoever listens on them. Pinned by `apart::two_sessions_cant_reach_each_others_{processes,
    /// sockets}`.
    pub fn seatbelt(ports: bool, network: bool) -> Guarantees {
        let loopback = if ports { "the ports it opens, whoever's" } else { "none of its ports open" };
        Guarantees {
            by: By::Seatbelt,
            ns: None,
            rows: vec![
                Row::new(Part::Files, true, "but the temp folders"),
                Row::new(Part::Processes, true, ""),
                Row::new(Part::Signals, true, ""),
                Row::new(Part::Ipc, true, ""),
                Row::new(Part::Shm, false, ""),
                Row::new(Part::Loopback, !ports, loopback),
            ],
            network,
            asks: false,
            listed: false,
            way: Way::Proxy,
            why: None,
            brief: None,
            hint: None,
            pool: None,
        }
    }

    /// Its network each host asked (`asks`), as the policy says.
    pub fn asking(self, asks: bool) -> Guarantees {
        Guarantees { asks: asks && self.network, ..self }
    }

    /// Its open network's hosts a list (`listed`) rather than any, as the policy says (`!Egress.any`) — after
    /// `asking`.
    pub fn listing(self, listed: bool) -> Guarantees {
        Guarantees { listed: listed && self.network && !self.asks, ..self }
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
        self.pairs_said(false)
    }

    /// `pairs` for the sandbox panel (9e), where room is short: the loopback of a run served without a network
    /// namespace in fewer words (`THEIRS_SHORT`) — `line` keeps them whole: `tarae state`, `--watch sandbox`, `:procs`.
    pub fn panel_pairs(&self) -> Vec<(&'static str, String)> {
        self.pairs_said(true)
    }

    fn pairs_said(&self, short: bool) -> Vec<(&'static str, String)> {
        let mut v = match self.by {
            By::Landlock { tier, abi, .. } => vec![
                ("tier", format!("{}{}", tier.said(), if abi < 3 { " partial" } else { "" })),
                ("Landlock", format!("ABI {abi}")),
            ],
            By::Seatbelt => vec![("sandbox", "Seatbelt".to_string())],
        };
        // (served without a network namespace, in the panel: `THEIRS_REFUSED` in fewer words)
        let loopback = self.rows.iter().find(|r| r.part == Part::Loopback).map(|r| match self.by {
            By::Landlock { net: RunNet::Served, .. } if short => THEIRS_SHORT.to_string(),
            _ => r.how.clone(),
        });
        v.extend(loopback.map(|l| ("loopback", l)));
        let network = match (self.way, self.network) {
            (Way::NoSocket, _) => "no socket",
            (_, false) => "closed",
            (Way::Proxy, true) => "tarae's proxy",
        };
        let network = match (self.asks, self.listed, self.way) {
            (_, _, Way::NoSocket) | (false, false, _) => network.to_string(),
            (true, _, _) => format!("each host asked, {network}"),
            (false, true, _) => format!("hosts listed, others asked, {network}"),
        };
        v.push(("network", network));
        // (the panel leaves it to the report and the state: a screen element of its own waits for its screenshot)
        if let (false, Some(a)) = (short, self.attrs_brief()) {
            v.push(("attributes", a.to_string()));
        }
        let shared: Vec<&str> = self.shared().into_iter().map(|p| self.name(p)).collect();
        v.push(("shared", if shared.is_empty() { "nothing".into() } else { shared.join(", ") }));
        v
    }

    /// The state line's keys from the least important to the most, where the sandbox panel is narrow (9e,
    /// `term::fold_hints`): Landlock's ABI (the tier says a partial one), the network, the loopback (other sessions'
    /// sockets — step 8's), what's shared. Room goes to the most important first, each part whole where it fits — a
    /// long one that doesn't leaves its room to shorter, less important ones — and what's shared keeps its first
    /// items (`processes, signals +2`) before it goes. The tier (or `sandbox Seatbelt`) is never left out.
    pub const FOLD: &[&str] = &["Landlock", "network", "loopback", "shared"];

    /// The state line: `pairs`, a ` · ` between.
    pub fn line(&self) -> String {
        self.pairs().iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · ")
    }

    /// The state line after a `sandbox` label (the state's text forms): `Seatbelt · …`, not `sandbox Seatbelt · …`.
    pub fn line_after_label(&self) -> String {
        let line = self.line();
        line.strip_prefix("sandbox ").map_or_else(|| line.clone(), String::from)
    }

    /// `tarae state`'s `sandbox` object of a session (9d, pinned by `ui_state.json`): what enforces it — the tier
    /// and Landlock's ABI (null on macOS: `by` `seatbelt`), the keeper's namespaces (`netns`: a network namespace
    /// of the session's own) — how the network goes, each part kept apart
    /// or shared (`Part::key`, `how` where there's more to say), `why` what's missing, `hint` how it would get namespaces
    /// (9f: why, and the profile's step — whole; null where none), and the state `line`.
    pub fn json(&self) -> serde_json::Value {
        use serde_json::json;
        let (by, tier, abi) = match self.by {
            By::Landlock { tier, abi, .. } => ("landlock", Some(tier.said()), Some(abi)),
            By::Seatbelt => ("seatbelt", None, None),
        };
        let parts = |kept: bool| -> Vec<serde_json::Value> {
            let rows = self.rows.iter().filter(|r| r.kept == kept);
            rows.map(|r| match r.how.is_empty() {
                true => json!({ "part": r.part.key() }),
                false => json!({ "part": r.part.key(), "how": r.how }),
            })
            .collect()
        };
        let ns = self
            .ns
            .map(|n| json!({ "user": n.user, "net": n.net, "pid": n.pid, "shm": n.shm, "netns": n.netns() }));
        json!({
            "by": by, "tier": tier, "abi": abi, "partial": abi.is_some_and(|a| a < 3), "namespaces": ns,
            "network": {
                "open": self.network && !self.asks, "asked": self.asks, "listed": self.listed, "way": self.way.key()
            },
            "kept": parts(true), "shared": parts(false), "attributes": self.attrs_json(),
            "why": self.why, "hint": self.hint.as_ref().map(|h| &h.said), "line": self.line(),
        })
    }

    /// The hint in a few words, for the sandbox panel under the why (9f): what the profile would give the runs —
    /// its step is the list's row to copy (`step_row`); where there's none, the one way to one.
    pub fn hint_brief(&self) -> Option<String> {
        let h = self.hint.as_ref()?;
        let made = HINT_BRIEF.to_string();
        Some(match h.step {
            Some(_) => made,
            None => format!("{made} · for a tarae installed at {}", super::apparmor::PACKAGED),
        })
    }

    /// The sandbox panel's row for the profile's step (9f): what Enter does, and what the step is in a few words —
    /// None where there's no step to copy.
    pub fn step_row(&self) -> Option<(String, &'static str)> {
        self.hint.as_ref().filter(|h| h.step.is_some())?;
        Some((STEP_ROW.to_string(), STEP_SAID))
    }

    /// One sentence for Claude: the tier, what's kept apart from other sessions' commands and what's shared.
    pub fn sentence(&self) -> String {
        let head = match self.by {
            By::Landlock { tier, abi, .. } => {
                format!("{}{} (Landlock ABI {abi})", tier.head(), if abi < 3 { ", partial" } else { "" })
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
    /// The run's `TARAE_SANDBOX_STATUS` (9c, D7; decision 12): `<tier>:<Landlock ABI>` — `own:8`, colima's
    /// `host:4`; a partial ABI (below 3) is just its number, `offline:2` — and Seatbelt's `seatbelt`. The tier's name
    /// only: what it lacks is the state's to say.
    pub fn status(&self) -> String {
        match self.by {
            By::Landlock { tier, abi, .. } => format!("{}:{abi}", tier.said()),
            By::Seatbelt => "seatbelt".to_string(),
        }
    }

    /// What Claude's run report says of it, after the rules, once a conversation and again when it changes (9b,
    /// decision 11 and N2): the sentence, then what's shared in the report's words, then what it may change of a
    /// file's attributes.
    pub fn told(&self) -> String {
        format!("{}{}{}", self.sentence(), self.shared_said(), self.attrs_said())
    }

    /// What a run may change of a file's attributes, in the report's words (linmeta B): served (`ATTRS_SERVED`),
    /// offline (`ATTRS_OFFLINE`); nothing for Seatbelt, whose `file-write-mode`, `-times`, `-owner`, `-flags`,
    /// `-xattr` and `-acl` are `file-write*`'s — where it writes, as its files.
    pub fn attrs_said(&self) -> &'static str {
        match self.by {
            By::Landlock { tier: Tier::Offline, .. } => ATTRS_OFFLINE,
            By::Landlock { .. } => ATTRS_SERVED,
            By::Seatbelt => "",
        }
    }

    /// The same on the state line — None for Seatbelt and served, as above.
    fn attrs_brief(&self) -> Option<&'static str> {
        match self.by {
            By::Landlock { tier: Tier::Offline, .. } => Some("mode and times anywhere"),
            _ => None,
        }
    }

    /// `tarae state`'s `attributes`: where a run may change a file's mode and times (`where-writes`, `anywhere`), its
    /// extended attributes and ACLs (`where-writes`, `never`), and its owner and flags (`where-writes`, `never`).
    pub fn attrs_json(&self) -> serde_json::Value {
        let (mode_times, xattrs, owner_flags) = match self.by {
            By::Landlock { tier: Tier::Offline, .. } => ("anywhere", "never", "never"),
            By::Landlock { .. } => ("where-writes", "where-writes", "never"),
            By::Seatbelt => ("where-writes", "where-writes", "where-writes"),
        };
        serde_json::json!({ "mode_times": mode_times, "xattrs": xattrs, "owner_flags": owner_flags })
    }

    /// What its runs share, in the report's words (said after the sentence: `told`): Landlock below
    /// ABI 6, the processes in sight without a PID namespace of the session's own, this system's System V IPC,
    /// `/dev/shm` and terminals without the session's namespaces (served, only the terminals it makes), `/dev/shm`
    /// alone where the session has none of its own; Seatbelt's ports it opens.
    pub fn shared_said(&self) -> String {
        let shared = |p: Part| self.kept(p) == Some(false);
        let abi = match self.by {
            By::Seatbelt => {
                let mem = if shared(Part::Shm) { MAC_SHARED } else { "" };
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

    /// 9f: a run's hint names the run's own tier — never the probe's, which can differ: a probe served, a run not
    /// (offline), or the other way (host) — and what the profile would give it: namespaces, never a tier.
    #[test]
    fn the_hint_names_the_runs_own_tier() {
        let place = super::super::apparmor::Place {
            path: super::super::apparmor::PACKAGED.into(),
            root_owned: Ok(()),
            packaged: true,
        };
        let run = |served: bool, probed: Tier| {
            Guarantees::landlock(Found {
                ns: Ns::NONE,
                served: if served { Ok(()) } else { Err("Yama's ptrace_scope is 3".into()) },
                abi: 4,
                ptys: None,
                lo_missing: Vec::new(),
                short: None,
                brief: None,
                hint: Some(Hint::of(probed, &place)),
                network: false,
            })
        };
        for (served, probed, tier) in
            [(false, Tier::Host, "offline"), (true, Tier::Offline, "host"), (true, Tier::Host, "host")]
        {
            let g = run(served, probed);
            let h = g.hint.as_ref().expect("the hint");
            assert!(h.said.starts_with(&format!("Claude's commands run in tier {tier} here")), "{}", h.said);
            assert!(h.said.contains("To give them namespaces of their own, run once"), "{}", h.said);
            assert_eq!(g.hint_brief().as_deref(), Some(HINT_BRIEF), "probed in {probed:?}");
        }
    }

    /// A refusal with a way out is said whole in `why` and in its own few words in `brief` — Docker's `pidfd_getfd`
    /// and Yama's scope 2 without namespaces; any other refusal in `brief` as it is.
    #[test]
    fn a_refusal_with_a_way_out_says_it_in_few_words() {
        let run = |why: &str| {
            Guarantees::landlock(Found {
                ns: Ns::NONE,
                served: Err(why.into()),
                abi: 6,
                ptys: None,
                lo_missing: Vec::new(),
                short: None,
                brief: None,
                hint: None,
                network: false,
            })
        };
        for (why, brief) in [
            (PIDFD_REFUSED, PIDFD_REFUSED_BRIEF),
            (YAMA_2_WITHOUT_NS, YAMA_2_WITHOUT_NS_BRIEF),
            ("Yama's ptrace_scope 3 keeps tarae from reading its runs' calls", ""),
        ] {
            let g = run(why);
            assert_eq!(g.tier(), Some(Tier::Offline));
            assert_eq!(g.why.as_deref(), Some(format!("tarae can't serve its calls: {why}").as_str()));
            let brief = if brief.is_empty() { format!("not served: {why}") } else { brief.into() };
            assert_eq!(g.brief, Some(brief));
        }
    }

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
