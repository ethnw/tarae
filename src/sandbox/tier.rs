//! A session keeper's namespaces (`docs/design/sandbox-keeper.md` §Tiers): what this system lets a keeper be
//! born with — found once, by making one for real and running `true` through it (`probe`) — and, where it
//! can't, why, in a sentence (`why`: a pure function of what was seen, `Seen`).
//!
//! With user namespaces a keeper is PID 1 of the session's user, ipc, pid and mount namespaces — and its
//! network one when its policy opens nothing (`for_policy`: until step 4's routing, an opened port or the
//! network would reach the session's empty loopback) — tier A+; where a fresh `/proc` can't be mounted
//! (Docker's masked `/proc`), the same without the PID namespace (D12) — A; without user namespaces none —
//! C. Each with tarae serving its runs' calls; not serving (`supervisor::unavailable`), B+, B and D.

use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

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

    /// As `clone3`'s flags.
    pub fn clone_flags(self) -> u64 {
        let mut f = 0;
        if self.user {
            f |= (libc::CLONE_NEWUSER | libc::CLONE_NEWIPC | libc::CLONE_NEWNS) as u64;
        }
        if self.user && self.net {
            f |= libc::CLONE_NEWNET as u64;
        }
        if self.user && self.pid {
            f |= libc::CLONE_NEWPID as u64;
        }
        f
    }

    /// Whether these make a network namespace (`net` without `user` makes none: `clone_flags`).
    pub fn netns(self) -> bool {
        self.clone_flags() & libc::CLONE_NEWNET as u64 != 0
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

    /// In the log's words: `user net ipc pid mnt`, or `none`.
    pub fn said(self) -> String {
        if !self.user {
            return "none".into();
        }
        let net = if self.net { " net" } else { "" };
        let pid = if self.pid { " pid" } else { "" };
        let shm = if self.shm { "" } else { "; /dev/shm shared: the project or a place it writes is in it" };
        format!("user{net} ipc{pid} mnt{shm}")
    }
}

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

/// What a keeper's birth failed at, before its exec — on its birth pipe as (code, errno), as a run's failure
/// pipe has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BirthStep {
    Clone,
    Map,
    Go,
    Propagation,
    Proc,
    Devpts,
    Ptmx,
    Shm,
    Lo,
    Caps,
    Exec,
    // tarae's side, after `Ready`: the session's network (`redirect::make`) — never on the birth pipe
    /// A socket the keeper handed isn't in the session's netns (P5).
    Netns,
    /// The keeper's sockets for it: made, handed, the planted ones made transparent.
    Planted,
    Routes,
    Rules,
    /// 6d: the session's netns keeps a connection's mark on what accepts it (`tcp_fwmark_accept`) — written by the
    /// birth's child before its capability drop (on the birth pipe, the one network step there): without it the
    /// planted socket can't tell whose a connection is. A read-only `/proc/sys` (a container's) refuses it.
    Fwmark,
}

impl BirthStep {
    const ALL: [BirthStep; 16] = [
        BirthStep::Clone,
        BirthStep::Map,
        BirthStep::Go,
        BirthStep::Propagation,
        BirthStep::Proc,
        BirthStep::Devpts,
        BirthStep::Ptmx,
        BirthStep::Shm,
        BirthStep::Lo,
        BirthStep::Caps,
        BirthStep::Exec,
        BirthStep::Netns,
        BirthStep::Planted,
        BirthStep::Routes,
        BirthStep::Rules,
        BirthStep::Fwmark,
    ];

    pub fn code(self) -> u8 {
        BirthStep::ALL.iter().position(|s| *s == self).unwrap_or(0) as u8
    }

    pub fn from_code(code: u8) -> Option<BirthStep> {
        BirthStep::ALL.get(code as usize).copied()
    }

    pub fn said(self) -> &'static str {
        match self {
            BirthStep::Clone => "making its namespaces",
            BirthStep::Map => "writing its id maps",
            BirthStep::Go => "its start",
            BirthStep::Propagation => "making its mounts its own",
            BirthStep::Proc => "mounting a fresh /proc",
            BirthStep::Devpts => "mounting its own /dev/pts",
            BirthStep::Ptmx => "putting its /dev/ptmx in place",
            BirthStep::Shm => "mounting its own /dev/shm",
            BirthStep::Lo => "bringing its loopback up",
            BirthStep::Caps => "dropping every capability",
            BirthStep::Exec => "running tarae's binary",
            BirthStep::Netns => "checking its sockets are the session's",
            BirthStep::Planted => "making its planted sockets",
            BirthStep::Routes => "sending its routes",
            BirthStep::Rules => "sending its rules",
            BirthStep::Fwmark => "keeping a connection's mark where it's accepted (tcp_fwmark_accept)",
        }
    }

    /// A step of the session's network — tarae's after `Ready`, and `Fwmark` the child's — not of its namespaces.
    pub fn network(self) -> bool {
        matches!(
            self,
            BirthStep::Netns | BirthStep::Planted | BirthStep::Routes | BirthStep::Rules | BirthStep::Fwmark
        )
    }

    /// A mount of the birth's.
    fn mounts(self) -> bool {
        matches!(
            self,
            BirthStep::Propagation | BirthStep::Proc | BirthStep::Devpts | BirthStep::Ptmx | BirthStep::Shm
        )
    }
}

/// A keeper's birth failed at `step` with `errno` — carried inside the `io::Error` `Keeper::born` returns
/// (`of`), so the probe can tell which step it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BirthError {
    pub step: BirthStep,
    pub errno: i32,
}

impl std::fmt::Display for BirthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step.said(), io::Error::from_raw_os_error(self.errno))
    }
}

impl std::error::Error for BirthError {}

impl From<BirthError> for io::Error {
    fn from(e: BirthError) -> io::Error {
        io::Error::other(e)
    }
}

impl BirthError {
    /// The birth error inside `e`, if that's what it is.
    pub fn of(e: &io::Error) -> Option<BirthError> {
        e.get_ref().and_then(|i| i.downcast_ref::<BirthError>()).copied()
    }
}

/// What the probe saw: the step a birth failed at and its errno, and what this system says about user
/// namespaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub failed: BirthError,
    /// `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu 23.10 on).
    pub restrict_userns: Option<u32>,
    /// `user.max_user_namespaces`.
    pub max_userns: Option<u64>,
    /// `kernel.unprivileged_userns_clone` (Debian's older kernels).
    pub userns_clone: Option<u32>,
    /// tarae itself runs under a seccomp filter (`Seccomp: 2`).
    pub seccomp: bool,
    /// tarae's AppArmor confinement (`unconfined`, `docker-default (enforce)` …).
    pub apparmor: Option<String>,
    /// WSL 1 (`/proc/version`).
    pub wsl1: bool,
    /// tarae is dumpable (`PR_GET_DUMPABLE`): non-dumpable — its ids changed since it started — the `/proc`
    /// files of what it makes are root's, a keeper's id maps among them.
    pub dumpable: bool,
}

fn read(p: &str) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn num<T: std::str::FromStr>(p: &str) -> Option<T> {
    read(p).and_then(|s| s.parse().ok())
}

/// `kernel.apparmor_restrict_unprivileged_userns`.
fn restriction() -> Option<u32> {
    num("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
}

/// tarae's AppArmor label: `unconfined`, `/usr/bin/tarae (unconfined)`, `docker-default (enforce)` …
pub fn label() -> Option<String> {
    read("/proc/self/attr/apparmor/current").or_else(|| read("/proc/self/attr/current"))
}

/// Whether `label` is a profile's — tarae runs under one, whatever its mode (`flags=(unconfined)` too).
fn confined(label: Option<&str>) -> bool {
    label.is_some_and(|l| !l.is_empty() && l != "unconfined")
}

/// Whether `label` is a profile in a mode that refuses — every mode but `complain` (logs only) and `unconfined`:
/// `enforce` (`docker-default (enforce)`), `kill`, `prompt`, and any a later AppArmor adds. tarae's own
/// (`/usr/bin/tarae (unconfined)`) refuses none, so a mount refused under it is refused for another reason. A
/// label without a mode (`unconfined`) is no profile.
fn refusing(label: Option<&str>) -> bool {
    let mode = label.and_then(|l| l.strip_suffix(')')).and_then(|l| l.rsplit_once(" (")).map(|(_, m)| m);
    mode.is_some_and(|m| !matches!(m, "complain" | "unconfined"))
}

/// Whether a run is refused new user namespaces (Q4: `unshare`, `clone` with `CLONE_NEWUSER`, `clone3` — the
/// keeper's `seccomp` filter): where Ubuntu's restriction is on and tarae runs under an AppArmor profile — any. An
/// exec keeps the label (t5), so that profile's word on `userns` is every run's too, and the restriction, which
/// covers unconfined programs only, no longer holds for them: tarae's own profile (`apparmor.rs`) would hand a run
/// what Ubuntu keeps from every other program. The filter puts the restriction back for runs. Elsewhere a run keeps
/// what this system gives every program (Debian lets it make one). Read once.
pub fn runs_refuse_userns() -> bool {
    static REFUSED: OnceLock<bool> = OnceLock::new();
    *REFUSED.get_or_init(|| refuses_userns(restriction(), label().as_deref()))
}

/// Tests: whether tarae runs here under a profile that lends user namespaces with Ubuntu's restriction on
/// (`profile_lends`) — else `test` (its whole path) says it skips, and why; a machine said to hold the profile
/// (`TARAE_EXPECT_PROFILE=1`: the VM's run under it) fails instead.
#[cfg(test)]
pub(crate) fn profiled(test: &str) -> bool {
    let label = label();
    let Err(why) = profile_lends(restriction(), label.as_deref()) else { return true };
    if std::env::var("TARAE_EXPECT_PROFILE").is_ok_and(|v| v == "1") {
        panic!("{test}: the profile expected here — {why}");
    }
    let line = format!("{}{test} — {why}\n", crate::test_results::SELF_SKIP);
    let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    false
}

/// Tests: the profile tests' ground, from the restriction's value and tarae's label — or why it isn't here. Runs
/// are refused user namespaces (`refuses_userns`) *and* the profile lends them, so the user's kind makes one as the
/// tests' control: tarae's own (`flags=(unconfined)` with `userns,`). Under a refusing one (`docker-default
/// (enforce)`, Docker's defaults, whose seccomp refuses them to every process) runs are refused too, but nothing
/// can make one — the tests' controls would fail for Docker, not for tarae.
#[cfg(test)]
fn profile_lends(restrict: Option<u32>, label: Option<&str>) -> Result<(), String> {
    let l = label.unwrap_or_default();
    if !refuses_userns(restrict, label) {
        return Err(format!("no profile with Ubuntu's restriction here (tarae runs as `{l}`)"));
    }
    if refusing(label) {
        return Err(format!(
            "tarae runs under `{l}`, a profile that refuses user namespaces to everything — these need tarae's \
             own (`cargo xtask linux --userns test --installed` on a VM that has it)"
        ));
    }
    Ok(())
}

/// `runs_refuse_userns` from what it reads: the restriction's value, tarae's label.
fn refuses_userns(restrict: Option<u32>, label: Option<&str>) -> bool {
    restrict == Some(1) && confined(label)
}

impl Seen {
    /// `failed`, with this system's answers read now.
    pub fn now(failed: BirthError) -> Seen {
        let status = read("/proc/self/status").unwrap_or_default();
        let seccomp = status.lines().any(|l| l.strip_prefix("Seccomp:").is_some_and(|v| v.trim() == "2"));
        let apparmor = label();
        let version = read("/proc/version").unwrap_or_default();
        Seen {
            failed,
            restrict_userns: restriction(),
            max_userns: num("/proc/sys/user/max_user_namespaces"),
            userns_clone: num("/proc/sys/kernel/unprivileged_userns_clone"),
            seccomp,
            apparmor,
            wsl1: version.contains("Microsoft") && !version.contains("WSL2"),
            // SAFETY: a plain prctl that only reads.
            dumpable: unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 1,
        }
    }
}

/// Ubuntu's restriction, and its remedy (said once, at the probe — tarae ships no profile).
const UBUNTU: &str = "Ubuntu's restriction on unprivileged user namespaces \
                      (kernel.apparmor_restrict_unprivileged_userns = 1) — an AppArmor profile granting userns to \
                      tarae's binary lifts it for tarae alone; else that setting at 0, which lifts it for every \
                      program";

/// What a keeper's birth failed for (§Tiers' table), in `why`'s order: the first that fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    Wsl1,
    MaxZero,
    CloneOff,
    Seccomp,
    NotDumpable,
    /// Ubuntu's restriction: the one `hint` speaks for.
    Restriction,
    ProfileMounts,
    MaskedProc,
    /// None of these: the step and errno as they are.
    Other,
}

fn cause(s: &Seen) -> Cause {
    let BirthError { step, errno } = s.failed;
    let refused = matches!(errno, libc::EPERM | libc::EACCES);
    if s.wsl1 {
        return Cause::Wsl1;
    }
    if step == BirthStep::Clone {
        if errno == libc::ENOSPC || s.max_userns == Some(0) {
            return Cause::MaxZero;
        }
        if s.userns_clone == Some(0) {
            return Cause::CloneOff;
        }
        if matches!(errno, libc::EPERM | libc::ENOSYS) && s.seccomp {
            return Cause::Seccomp;
        }
    }
    if refused && step == BirthStep::Map && !s.dumpable {
        return Cause::NotDumpable;
    }
    if refused && s.restrict_userns == Some(1) && step != BirthStep::Exec && !step.network() {
        return Cause::Restriction;
    }
    if refused && step.mounts() && refusing(s.apparmor.as_deref()) {
        return Cause::ProfileMounts;
    }
    if refused && step == BirthStep::Proc {
        return Cause::MaskedProc;
    }
    Cause::Other
}

/// Why a keeper couldn't be born with its namespaces, from what was seen (§Tiers' table).
pub fn why(s: &Seen) -> String {
    match cause(s) {
        Cause::Wsl1 => "WSL 1 has no user namespaces".into(),
        Cause::MaxZero => {
            "user namespaces are turned off on this system (user.max_user_namespaces = 0)".into()
        }
        Cause::CloneOff => "unprivileged user namespaces are turned off on this system \
                            (kernel.unprivileged_userns_clone = 0)"
            .into(),
        Cause::Seccomp => {
            "a seccomp filter tarae runs under refuses new namespaces — Docker's default profile does".into()
        }
        Cause::NotDumpable => {
            "tarae runs non-dumpable (its ids changed after it started), so the id maps of what \
                               it makes are root's to write"
                .into()
        }
        Cause::Restriction => format!("{UBUNTU} (refused at {})", s.failed.step.said()),
        Cause::ProfileMounts => format!(
            "an AppArmor profile tarae runs under ({}) refuses mounting",
            s.apparmor.as_deref().unwrap_or_default()
        ),
        Cause::MaskedProc => {
            "a fresh /proc can't be mounted here — this /proc has paths masked over it (Docker's \
                              default does), so no PID namespace"
                .into()
        }
        Cause::Other => s.failed.to_string(),
    }
}

/// The hint (6h; a toast once, and the tier's line): where Ubuntu's restriction is why a keeper got no namespaces
/// and tarae runs under no profile — runs in `tier`, why, and the step that makes them A (`apparmor::hint`, by the
/// binary's `place`). Under a profile that still didn't let it, none: that profile isn't the one tarae ships.
pub fn hint(s: &Seen, tier: Tier, place: &super::apparmor::Place) -> Option<String> {
    let ours_missing = cause(s) == Cause::Restriction && !confined(s.apparmor.as_deref());
    ours_missing.then(|| super::apparmor::hint(tier.said(), place))
}

/// What the probe found.
#[derive(Clone, Debug)]
pub struct Probed {
    /// The namespaces a session's keeper is born with — `net` among them (a policy that opens something goes
    /// without: `for_policy`).
    pub ns: Ns,
    /// Why not all of them (no `pid`: D12's reason; none: why not) — None: all.
    pub short: Option<String>,
    /// Whether tarae serves its runs' calls here (`supervisor::unavailable`).
    pub served: Result<(), String>,
    /// Whether a keeper of this tarae is born with the session's transparent proxy (`redirect`): the probe's own
    /// keeper got its routes and owned table, or why not (no netns; a module that can't load, a kernel without
    /// `NFT_TPROXY` or the owner flag). Decided once (D-b): a session's own birth whose network fails after this
    /// passed fails its runs, said — no silent fallback. Without it a session's runs go out by the proxy's variable
    /// and the pair, their own loopback kept (6f, F3).
    pub redirect: Result<(), String>,
    /// 6g: whether a keeper with that network gets a scope of its own, each run a cgroup of its own below it — where
    /// the scope comes from — or why not (`cgroup::conditions`; without `redirect`, nothing would read it). Decided
    /// once (D-b); a keeper whose own scope fails says so with it, its runs in its cgroup.
    pub cgroups: Result<super::cgroup::Source, String>,
    /// Ubuntu's restriction kept the namespaces from a tarae under no profile: why, and the step (`hint`).
    pub hint: Option<String>,
}

impl Probed {
    pub fn tier(&self) -> Tier {
        Tier::of(self.ns, self.served.is_ok())
    }
}

/// How long the probe's keeper has, from its birth to its `true` reaped: past it, no namespaces — never
/// success.
const PROBE_WITHIN: Duration = Duration::from_secs(4);

static PROBED: OnceLock<Probed> = OnceLock::new();

/// What the probe found, if it has run — never probes (the main loop's).
pub fn probed() -> Option<&'static Probed> {
    PROBED.get()
}

/// What the probe found here — probed the first time (a worker's: it makes a keeper or two and waits on them).
pub fn get() -> &'static Probed {
    PROBED.get_or_init(|| {
        // (a measurement's other half, in the same process: no namespaces asked — `measure_gradle_pairs`)
        #[cfg(test)]
        if std::env::var_os("TARAE_TEST_NO_NS").is_some() {
            let served = super::supervisor::unavailable().map_or(Ok(()), Err);
            return Probed {
                ns: Ns::NONE,
                short: Some("none asked (TARAE_TEST_NO_NS)".into()),
                served,
                redirect: Err("no network namespace".into()),
                cgroups: Err("no TPROXY network".into()),
                hint: None,
            };
        }
        #[allow(unused_mut)]
        let mut p = probe();
        // (M4's cut: the session's namespaces without the PID one — tier A, no fresh `/proc` — on a machine
        // that gives A+)
        #[cfg(test)]
        if std::env::var_os("TARAE_TEST_NO_PID").is_some() && p.ns.pid {
            p.ns.pid = false;
            p.short = Some("no PID namespace asked (TARAE_TEST_NO_PID)".into());
        }
        let said = match &p.short {
            None => format!("namespaces: {} ({})", p.tier().said(), p.ns.said()),
            Some(why) => format!("namespaces: {} ({}) — {why}", p.tier().said(), p.ns.said()),
        };
        let said = match &p.hint {
            Some(hint) => format!("{said} — {hint}"),
            None => said,
        };
        let said = match p.served.is_ok() && super::supervisor::copies() {
            true => format!(
                "{said} — its runs served by tarae's own copies of their sockets (pidfd_getfd refused: Docker's \
                 default profile; --cap-add SYS_PTRACE gives the full way)"
            ),
            false => said,
        };
        crate::log::line("sandbox", "info", &said);
        // (Q5: the network's way and why, beside the tier)
        let way = match &p.redirect {
            Ok(()) => "network: TPROXY (the session's routes and owned table)".to_string(),
            Err(why) => format!("network: the session's proxy, by its variable — no TPROXY: {why}"),
        };
        crate::log::line("sandbox", "info", &way);
        // (6g, D4: said here only — nothing is lost without it but what 6d would add)
        if p.redirect.is_ok() {
            let runs = match &p.cgroups {
                Ok(source) => format!(
                    "runs: a cgroup each, in their keeper's own scope ({}) — a rule's level: a run's depth \
                     below the root",
                    source.said()
                ),
                Err(why) => format!("runs: in their keeper's cgroup — {why}"),
            };
            crate::log::line("sandbox", "info", &runs);
        }
        p
    })
}

/// The namespaces the keeper of a policy is born with: the probe's — without a `/dev/shm` of its own where the
/// project or a place it writes is in this system's (`in_shm`: a private one would hide them). (Whatever it opens:
/// a socket of tarae's can always take a run's place — `SETFD`, in the v5.9 header, is older than the 5.14 floor — so
/// what it opens is reached from the session's netns too. 6f took D-a's "no netns for a policy that opens
/// something" out with the probe for it.)
pub fn for_policy(p: &super::Policy) -> Ns {
    for_policy_in(get(), p)
}

/// `for_policy` where the probe found `probed`.
fn for_policy_in(probed: &Probed, p: &super::Policy) -> Ns {
    ns_for(probed, in_shm(p))
}

/// `for_policy` from what it reads of a policy: whether it's in `/dev/shm`.
fn ns_for(probed: &Probed, in_shm: bool) -> Ns {
    Ns { shm: probed.ns.shm && !in_shm, ..probed.ns }
}

/// Whether the keeper of a policy that opens the network takes its runs' connections in the session's network
/// (TPROXY, A+/A) — its namespaces as `for_policy` gives them such a policy, and the keeper's own rule
/// (`keeper::network_for`). Then its runs get no proxy URL (6e′, `Policy.redirected`). Probes the first time (a
/// worker's). Decided before there's a keeper: it must be what the keeper is born with
/// (`the_proxy_url_is_decided_as_the_keeper_is_born`).
pub fn redirects() -> bool {
    redirects_in(get())
}

/// `redirects` where the probe found `probed`.
fn redirects_in(probed: &Probed) -> bool {
    super::keeper::network_for(ns_for(probed, false), &probed.redirect).is_some()
}

/// Whether the project, or a place or a file `p` writes, is in `/dev/shm` (real paths, as a policy has them).
pub fn in_shm(p: &super::Policy) -> bool {
    let shm = Path::new("/dev/shm");
    std::iter::once(&p.root).chain(&p.write).chain(&p.files).any(|w| w.starts_with(shm))
}

/// A keeper made with every namespace, and a `true` through it; one whose `/proc` didn't mount (D12), again
/// without the PID namespace; any other failure, none — and why.
fn probe() -> Probed {
    let served = match super::supervisor::unavailable() {
        None => Ok(()),
        Some(why) => Err(why),
    };
    probe_with(served, super::supervisor::copies(), tried)
}

/// `probe` where serving is `served`, by `copies` or not, and a keeper's birth and run is `tried` (tests stand in).
fn probe_with(
    served: Result<(), String>,
    copies: bool,
    mut tried: impl FnMut(Ns, bool) -> Result<(), Failed>,
) -> Probed {
    // (6f′: where tarae serves by its own copies, every socket is tarae's — a session netns would hold none of them)
    let net = !copies;
    let all = Ns::session(net, true);
    let none = || Err("no network namespace".into());
    // (no namespaces: why, and the hint where Ubuntu's restriction is why — for the binary running)
    let without = |e: Failed| {
        let tier = Tier::of(Ns::NONE, served.is_ok());
        let place = || super::apparmor::place_of(&std::env::current_exe().unwrap_or_default());
        let hint = match &e {
            Failed::Birth(b) => hint(&Seen::now(*b), tier, &place()),
            _ => None,
        };
        (Ns::NONE, Some(e.said()), none(), hint)
    };
    let (ns, short, redirect, hint) = match with_network(all, &mut tried) {
        (Ok(()), network) => (all, None, network, None),
        (Err(Failed::Birth(e)), _) if e.step == BirthStep::Proc => {
            let no_pid = why(&Seen::now(e));
            match with_network(Ns::session(net, false), &mut tried) {
                (Ok(()), network) => (Ns::session(net, false), Some(no_pid), network, None),
                (Err(e), _) => without(e),
            }
        }
        (Err(e), _) => without(e),
    };
    let cgroups = match &redirect {
        Ok(()) => super::cgroup::conditions(&super::cgroup::facts()),
        Err(_) => Err("no TPROXY network".into()),
    };
    Probed { ns, short, served, redirect, cgroups, hint }
}

/// A keeper born with `ns` and, where it has a netns, the session's network: whether the namespaces work, and
/// whether the network does. One keeper where both do (measured on deb12: 47 ms with the network, 43 without —
/// `measure_the_probe`; a second keeper for the network alone was the whole 47 more, on the first run's path).
/// A network step failing (`BirthStep::network`), the namespaces are tried again alone: a network that can't be
/// made doesn't cost the session its namespaces. Anything else — a namespace step, no answer in time, the run — is
/// the namespaces' answer as it is, and the network wasn't tried. The keeper's try is `tried` (the tests pass a
/// stand-in).
fn with_network(
    ns: Ns,
    mut tried: impl FnMut(Ns, bool) -> Result<(), Failed>,
) -> (Result<(), Failed>, Result<(), String>) {
    if !ns.netns() {
        return (tried(ns, false), Err("no network namespace".into()));
    }
    match tried(ns, true) {
        Ok(()) => (Ok(()), Ok(())),
        Err(Failed::Birth(e)) if e.step.network() => (tried(ns, false), Err(Failed::Birth(e).said())),
        Err(namespaces) => (Err(namespaces), Err("not tried: the namespaces failed first".into())),
    }
}

/// How a probe's keeper failed.
enum Failed {
    /// Its birth, at a step.
    Birth(BirthError),
    /// Its `true`.
    Run(String),
    /// Nothing within `PROBE_WITHIN`.
    Late,
}

impl Failed {
    fn said(self) -> String {
        match self {
            Failed::Birth(e) => why(&Seen::now(e)),
            Failed::Run(why) => format!("a run through a keeper in namespaces failed: {why}"),
            Failed::Late => {
                format!("a keeper in namespaces didn't answer within {} s", PROBE_WITHIN.as_secs())
            }
        }
    }
}

/// A keeper born with `ns` — with the session's network, `redirect` — and a `true` run through it, all within
/// `PROBE_WITHIN` (on a thread: what hangs is left to it, said as late).
fn tried(ns: Ns, redirect: bool) -> Result<(), Failed> {
    let (tx, rx) = std::sync::mpsc::channel();
    crate::event::thread("tarae-nsprobe", move || {
        let _ = tx.send(true_through(ns, redirect));
    });
    rx.recv_timeout(PROBE_WITHIN).unwrap_or(Err(Failed::Late))
}

/// `true` run by a keeper born with `ns` (and the session's network, `redirect`) for a root no session has, which
/// ends after.
fn true_through(ns: Ns, redirect: bool) -> Result<(), Failed> {
    let root = std::env::temp_dir().join(format!("tarae-nsprobe-{}", std::process::id()));
    let k = super::keeper::born_for_probe(&root, ns, redirect).map_err(|e| match BirthError::of(&e) {
        Some(b) => Failed::Birth(b),
        None => Failed::Run(e.to_string()),
    })?;
    let ran = match crate::landlock::abi() {
        // (no Landlock here: the birth is what's probed — the sandbox itself is missing anyway)
        Err(_) => Ok(()),
        Ok(abi) => (|| {
            let net = ns.run_net(false, false);
            let mut rules = match net {
                RunNet::Own => crate::landlock::Ruleset::new_own_network(abi)?,
                _ => crate::landlock::Ruleset::new(abi)?,
            };
            rules.allow(Path::new("/"), crate::landlock::ALL)?;
            let cmd = std::process::Command::new("true");
            let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), net)?;
            let status = crate::spawn::wait(s.spawned.pid)?;
            match status.success() {
                true => Ok(()),
                false => Err(io::Error::other(format!("`true` ended {status}"))),
            }
        })()
        .map_err(|e: io::Error| Failed::Run(e.to_string())),
    };
    k.end();
    ran
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(step: BirthStep, errno: i32) -> Seen {
        Seen {
            failed: BirthError { step, errno },
            restrict_userns: None,
            max_userns: Some(63_000),
            userns_clone: None,
            seccomp: false,
            apparmor: Some("unconfined".into()),
            wsl1: false,
            dumpable: true,
        }
    }

    /// 6e′: a run's proxy URL is decided before its keeper is born (`redirects`, through `Here.redirects`), and it
    /// must be what the keeper is born with — else a run has neither the variable nor the session's network, and
    /// its report says the proxy takes its connections. For every outcome of the probe (namespaces: the session's
    /// with or without `pid`, without `net`, none; served or not; the session's network landed or not) and every
    /// policy that matters (the network closed or open, a port, this machine's
    /// ports, both), the policy as computed (`policy_in`) has no proxy URL exactly where its network is open and
    /// its keeper gets the session's network (`keeper::network_for` over `for_policy`'s namespaces).
    #[test]
    fn the_proxy_url_is_decided_as_the_keeper_is_born() {
        let t = std::env::temp_dir().join(format!("tarae-tier-6e2-{}", std::process::id()));
        std::fs::create_dir_all(t.join("p")).unwrap();
        let t = std::fs::canonicalize(&t).unwrap();
        let llm = |network: bool, ports: Vec<u16>, localhost: bool| crate::llm::LlmConfig {
            sandbox_network: network,
            sandbox_ports: ports,
            sandbox_localhost: localhost,
            ..Default::default()
        };
        let llms = [
            llm(false, vec![], false),
            llm(true, vec![], false),
            llm(true, vec![8080], false),
            llm(true, vec![], true),
            llm(false, vec![8080], true),
        ];
        let nses = [Ns::session(true, true), Ns::session(true, false), Ns::session(false, true), Ns::NONE];
        let ok = |yes: bool| if yes { Ok(()) } else { Err("not here".to_string()) };
        let (mut taken, mut cases) = (0, 0);
        for ns in nses {
            for (served, redirect) in [(false, false), (false, true), (true, false), (true, true)] {
                let probed = Probed {
                    ns,
                    short: None,
                    served: ok(served),
                    redirect: ok(redirect),
                    cgroups: Err("not asked".into()),
                    hint: None,
                };
                let here = super::super::Here {
                    tmp: Some(t.clone()),
                    uid: 501,
                    os: super::super::Os::Linux,
                    redirects: redirects_in(&probed),
                    ..Default::default()
                };
                for l in &llms {
                    let p = super::super::policy_in(&t.join("p"), l, &[], &here);
                    let born =
                        super::super::keeper::network_for(for_policy_in(&probed, &p), &probed.redirect);
                    let keepers = p.network && born.is_some();
                    let case = format!(
                        "{probed:?} network={} ports={:?} localhost={}",
                        p.network, p.ports, p.localhost
                    );
                    assert_eq!(p.redirected, keepers, "{case}");
                    assert_eq!(p.proxy_url().is_none(), !p.network || keepers, "{case}");
                    (taken, cases) = (taken + usize::from(keepers), cases + 1);
                }
            }
        }
        // (both answers seen, or the table proves nothing)
        assert!(taken > 0 && taken < cases, "{taken} of {cases}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A refused mount is blamed on the AppArmor profile tarae runs under only where that profile's mode refuses —
    /// `enforce`, `kill`, `prompt` (and a mode a later AppArmor adds) — never `complain` (it only logs) nor
    /// `unconfined` (tarae's own profile: then the reason is another, here a masked `/proc`); a label with no mode
    /// is no profile.
    #[test]
    fn a_refused_mount_is_the_profiles_only_in_a_mode_that_refuses() {
        for (label, theirs) in [
            ("docker-default (enforce)", true),
            ("tarae-test (kill)", true),
            ("snap.app.app (prompt)", true),
            ("some-profile//&other (enforce)", true),
            ("/usr/bin/tarae (unconfined)", false),
            ("tarae-test (complain)", false),
            ("unconfined", false),
            ("", false),
        ] {
            let s = Seen { apparmor: Some(label.into()), ..seen(BirthStep::Propagation, libc::EACCES) };
            assert_eq!(why(&s).contains("refuses mounting"), theirs, "{label}: {}", why(&s));
        }
        assert!(!refusing(None));
    }

    /// A run's internet sockets are its own only not served and in a network namespace its keeper made — `net`
    /// without `user` makes none (`clone_flags`).
    #[test]
    fn a_run_owns_its_network_only_in_a_namespace_made_for_it() {
        assert_eq!(Ns::session(true, true).run_net(false, false), RunNet::Own);
        assert_eq!(Ns::session(true, false).run_net(true, false), RunNet::Routed);
        assert_eq!(Ns::session(false, true).run_net(false, false), RunNet::Plain, "no network namespace");
        assert_eq!(Ns::session(false, true).run_net(true, false), RunNet::Served);
        assert_eq!(
            Ns { net: true, ..Ns::NONE }.run_net(false, false),
            RunNet::Plain,
            "net without user: none made"
        );
        assert_eq!(Ns { net: true, ..Ns::NONE }.run_net(true, false), RunNet::Served);
        assert_eq!(Ns::NONE.run_net(true, false), RunNet::Served);
        // (6f′: by copies wherever it's served — every socket tarae's, a netns or not; not served, as before)
        assert_eq!(Ns::NONE.run_net(true, true), RunNet::Copied);
        assert_eq!(Ns::session(true, true).run_net(true, true), RunNet::Copied);
        assert_eq!(Ns::NONE.run_net(false, true), RunNet::Plain);
    }

    /// Served by copies (6f′), the probe asks for no network namespace at all — every socket a run has is tarae's,
    /// a session netns would hold none of them — and the session gets none (nor the TPROXY network); served the
    /// usual way, its first keeper is asked with one. (Keepers stood in for: what each was asked.)
    #[test]
    fn served_by_copies_the_probe_asks_no_network_namespace() {
        let asked = |copies: bool| {
            let mut asked = Vec::new();
            let p = probe_with(Ok(()), copies, |ns: Ns, redirect| {
                asked.push((ns.netns(), redirect));
                // (the network's step refused: what the session's network would be isn't this test's)
                match redirect {
                    true => Err(Failed::Birth(BirthError { step: BirthStep::Netns, errno: libc::EPERM })),
                    false => Ok(()),
                }
            });
            (asked, p)
        };
        let (by_copies, p) = asked(true);
        assert_eq!(by_copies, [(false, false)], "one keeper, no netns, no network tried");
        assert!(!p.ns.netns() && p.redirect.is_err(), "{:?}", p.ns);
        let (taken, p) = asked(false);
        assert_eq!(taken[0], (true, true), "the usual way: a netns and its network first");
        assert!(p.ns.netns());
    }

    /// Each of §Tiers' rows from what a stand-in system would show: the probe says why in the words of
    /// what's there, and only what's left over as the bare step and errno.
    #[test]
    fn the_probe_says_why() {
        let cases: Vec<(Seen, &str)> = vec![
            (
                Seen { restrict_userns: Some(1), ..seen(BirthStep::Propagation, libc::EPERM) },
                "Ubuntu's restriction",
            ),
            (Seen { restrict_userns: Some(1), ..seen(BirthStep::Map, libc::EPERM) }, "Ubuntu's restriction"),
            (Seen { seccomp: true, ..seen(BirthStep::Clone, libc::EPERM) }, "Docker's default profile"),
            (Seen { seccomp: true, ..seen(BirthStep::Clone, libc::ENOSYS) }, "Docker's default profile"),
            (seen(BirthStep::Clone, libc::ENOSPC), "user.max_user_namespaces = 0"),
            (Seen { max_userns: Some(0), ..seen(BirthStep::Clone, libc::EPERM) }, "max_user_namespaces = 0"),
            (
                Seen { userns_clone: Some(0), ..seen(BirthStep::Clone, libc::EPERM) },
                "kernel.unprivileged_userns_clone = 0",
            ),
            (Seen { wsl1: true, ..seen(BirthStep::Clone, libc::EINVAL) }, "WSL 1"),
            (seen(BirthStep::Proc, libc::EPERM), "masked over it"),
            (
                Seen {
                    apparmor: Some("docker-default (enforce)".into()),
                    ..seen(BirthStep::Propagation, libc::EACCES)
                },
                "AppArmor profile tarae runs under (docker-default (enforce)) refuses mounting",
            ),
            // (tarae's own profile is `flags=(unconfined)`: it refuses no mount — a masked /proc is still why)
            (
                Seen {
                    apparmor: Some("/usr/bin/tarae (unconfined)".into()),
                    ..seen(BirthStep::Proc, libc::EPERM)
                },
                "masked over it",
            ),
            (Seen { dumpable: false, ..seen(BirthStep::Map, libc::EACCES) }, "tarae runs non-dumpable"),
            (seen(BirthStep::Map, libc::EACCES), "writing its id maps: Permission denied"),
            (seen(BirthStep::Lo, libc::EPERM), "bringing its loopback up: Operation not permitted"),
            (seen(BirthStep::Clone, libc::EPERM), "making its namespaces: Operation not permitted"),
        ];
        for (s, words) in cases {
            let said = why(&s);
            assert!(said.contains(words), "{:?} {} → {said}", s.failed.step, s.failed.errno);
        }
        // (the session's network is tarae's own sends, past the namespaces: the restriction isn't why it failed)
        let rules = why(&Seen { restrict_userns: Some(1), ..seen(BirthStep::Rules, libc::EPERM) });
        assert_eq!(rules, "sending its rules: Operation not permitted (os error 1)");
        // (the restriction's remedy: a profile for tarae first, the setting second)
        let ubuntu = why(&Seen { restrict_userns: Some(1), ..seen(BirthStep::Devpts, libc::EPERM) });
        assert!(ubuntu.find("AppArmor profile") < ubuntu.find("that setting at 0"), "{ubuntu}");
    }

    /// The hint speaks only where Ubuntu's restriction is why a keeper got nothing and tarae runs under no profile
    /// — that's what the profile would change. Not where the restriction is off or something else failed first
    /// (Docker's filter refuses the clone before the restriction is reached), not for a failure of the session's
    /// network (tarae's own sends, past the namespaces), and not under a profile already: one that didn't help
    /// isn't tarae's to replace. It says the tier the runs are in.
    #[test]
    fn the_hint_comes_only_where_the_restriction_is_why() {
        let place = crate::sandbox::apparmor::Place {
            path: "/usr/bin/tarae".into(),
            root_owned: Ok(()),
            packaged: true,
        };
        let restricted = |step, label: Option<&str>| Seen {
            restrict_userns: Some(1),
            apparmor: label.map(str::to_string),
            ..seen(step, libc::EPERM)
        };
        let hint = |s: &Seen, tier| super::hint(s, tier, &place);
        let unconfined = restricted(BirthStep::Map, Some("unconfined"));
        let said = hint(&unconfined, Tier::C).expect("restricted, no profile: the hint");
        assert!(said.contains("tier C") && said.contains("sudo apparmor_parser -r"), "{said}");
        assert!(hint(&unconfined, Tier::D).is_some_and(|h| h.contains("tier D")), "the tier the runs are in");
        assert!(hint(&restricted(BirthStep::Devpts, None), Tier::C).is_some(), "no label read: no profile");
        let none = [
            (restricted(BirthStep::Map, Some("/usr/bin/tarae (unconfined)")), "under a profile already"),
            (Seen { restrict_userns: Some(0), ..unconfined.clone() }, "the restriction off"),
            (Seen { restrict_userns: None, ..unconfined.clone() }, "no such setting"),
            (
                Seen { seccomp: true, ..restricted(BirthStep::Clone, Some("unconfined")) },
                "Docker's filter first",
            ),
            (restricted(BirthStep::Rules, Some("unconfined")), "the session's network"),
            (
                Seen { failed: BirthError { step: BirthStep::Map, errno: libc::EINVAL }, ..unconfined },
                "not refused",
            ),
        ];
        for (s, what) in none {
            assert_eq!(hint(&s, Tier::C), None, "{what}");
        }
    }

    /// Runs are refused new user namespaces exactly where tarae's label would hand them one Ubuntu's restriction
    /// keeps from other programs: the restriction on and tarae under a profile. Off, or tarae unconfined (the
    /// restriction covers its runs itself), a run keeps what this system gives every program.
    #[test]
    fn runs_lose_user_namespaces_only_where_a_profile_would_lend_them() {
        assert!(refuses_userns(Some(1), Some("/usr/bin/tarae (unconfined)")));
        assert!(
            refuses_userns(Some(1), Some("docker-default (enforce)")),
            "any profile decides for its runs"
        );
        assert!(!refuses_userns(Some(1), Some("unconfined")), "the restriction holds for them itself");
        assert!(!refuses_userns(Some(1), None));
        assert!(
            !refuses_userns(Some(0), Some("/usr/bin/tarae (unconfined)")),
            "the restriction off: as Debian"
        );
        assert!(!refuses_userns(None, Some("/usr/bin/tarae (unconfined)")));
    }

    /// The profile tests (`profiled`) run where tarae's own profile lends what its runs are refused: the
    /// restriction on and tarae under it (`--installed` on a VM with the profile). Not under Docker's defaults:
    /// `docker-default (enforce)` refuses runs too, but its seccomp lets nothing make a user namespace — the tests'
    /// controls can't be. Nor where runs aren't refused (unconfined, Debian's kernel without the restriction).
    #[test]
    fn the_profile_tests_run_only_under_a_profile_that_lends() {
        assert_eq!(profile_lends(Some(1), Some("/usr/bin/tarae (unconfined)")), Ok(()));
        assert_eq!(profile_lends(Some(1), Some("/opt/tarae/bin/tarae (unconfined)")), Ok(()));
        let docker = profile_lends(Some(1), Some("docker-default (enforce)"));
        assert!(
            docker.as_ref().is_err_and(|w| w.contains("refuses user namespaces to everything")),
            "{docker:?}"
        );
        for (restrict, label) in [(Some(1), Some("unconfined")), (None, Some("/usr/bin/tarae (unconfined)"))]
        {
            let off = profile_lends(restrict, label);
            assert!(
                off.as_ref().is_err_and(|w| w.starts_with("no profile")),
                "{restrict:?} {label:?}: {off:?}"
            );
        }
    }

    /// What the probe finds on this system, said on stderr (each machine's tier, in the test output): no
    /// namespaces always with a reason; the PID namespace only with the rest; the reason for no PID namespace
    /// is D12's own. A machine's expected tier, when said (`TARAE_EXPECT_TIER`, `cargo xtask linux --env …`), is
    /// the one found; its expected network the same (`TARAE_EXPECT_NETWORK=tproxy`) — else no TPROXY is said, not
    /// failed.
    #[test]
    fn the_probe_here() {
        let p = get();
        let line = format!(
            "tier here: {} ({}){}; network: {}\n",
            p.tier().said(),
            p.ns.said(),
            p.short.as_deref().map(|w| format!(" — {w}")).unwrap_or_default(),
            p.redirect.as_ref().map_or_else(|why| format!("no TPROXY — {why}"), |()| "TPROXY".into())
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        // (6h: the label tarae runs under, whether its runs lose user namespaces, the hint — the profile's run on a
        // VM reads them here; `TARAE_EXPECT_HINT` 1 or 0 says whether one is expected)
        let profile = format!(
            "label: {} · runs refuse user namespaces: {} · hint: {}\n",
            label().unwrap_or_default(),
            runs_refuse_userns(),
            p.hint.as_deref().unwrap_or("none")
        );
        let _ = io::Write::write_all(&mut io::stderr(), profile.as_bytes());
        match std::env::var("TARAE_EXPECT_HINT").as_deref() {
            Ok("1") => assert!(p.hint.is_some(), "a hint expected: {line}{profile}"),
            Ok("0") => assert!(p.hint.is_none(), "no hint expected: {profile}"),
            _ => {}
        }
        if let Ok(want) = std::env::var("TARAE_EXPECT_TIER") {
            assert_eq!(p.tier().said(), want, "{line}");
        }
        // (the network's way is an expectation of its own — `TARAE_EXPECT_NETWORK=tproxy`, set by the VM runs: a
        // container without `nft_tproxy` gives the tier and not the redirect, and says why)
        match (std::env::var("TARAE_EXPECT_NETWORK").as_deref(), &p.redirect) {
            (Ok("tproxy"), r) => assert!(r.is_ok(), "TARAE_EXPECT_NETWORK=tproxy: {line}"),
            (Ok(other), _) => panic!("TARAE_EXPECT_NETWORK={other}: only `tproxy` is known"),
            (Err(_), Err(why)) => {
                let said = format!("the_probe_here: no TPROXY here, not expected — {why}\n");
                let _ = io::Write::write_all(&mut io::stderr(), said.as_bytes());
            }
            (Err(_), Ok(())) => {}
        }
        assert!(p.ns.netns() || p.redirect.is_err(), "no TPROXY without a netns: {line}");
        assert_eq!(p.short.is_none(), p.ns == Ns::session(true, true), "{line}");
        assert!(p.ns.user || p.ns == Ns::NONE, "{line}");
        if p.ns.user && !p.ns.pid {
            assert!(p.short.as_deref().is_some_and(|w| w.contains("/proc")), "{line}");
        }
    }

    /// The probe's keeper is born with the network where it can be — one keeper telling both, the common case. A
    /// network step failing doesn't cost the session its namespaces: they're tried again alone (two births). A
    /// namespace step failing, or no answer in time, is the namespaces' answer as it is — one birth, the network said
    /// not tried. Without a netns, no network is tried. (A stand-in for the keeper's try: the failure it gives with
    /// the network, success alone; each birth counted.)
    #[test]
    fn one_keeper_probes_both_and_a_failed_network_keeps_the_namespaces() {
        fn birth(step: BirthStep) -> Failed {
            Failed::Birth(BirthError { step, errno: libc::ENOENT })
        }
        let run = |with: Option<fn() -> Failed>, ns: Ns| {
            let mut births = Vec::new();
            let (namespaces, network) = with_network(ns, |_, net| {
                births.push(net);
                match (net, with) {
                    (true, Some(fail)) => Err(fail()),
                    _ => Ok(()),
                }
            });
            (namespaces.is_ok(), network, births)
        };
        assert_eq!(run(None, Ns::session(true, true)), (true, Ok(()), vec![true]), "one keeper, both told");
        let (namespaces, network, births) = run(Some(|| birth(BirthStep::Rules)), Ns::session(true, true));
        assert!(namespaces, "a network step: the namespaces kept");
        assert!(network.is_err_and(|w| w.starts_with("sending its rules")), "the network's why");
        assert_eq!(births, [true, false], "then the namespaces alone");
        for (what, with) in
            [("a namespace step", (|| birth(BirthStep::Proc)) as fn() -> Failed), ("late", || Failed::Late)]
        {
            let (namespaces, network, births) = run(Some(with), Ns::session(true, true));
            assert!(!namespaces, "{what}: the namespaces' answer as it is");
            assert!(network.is_err_and(|w| w.starts_with("not tried")), "{what}: the network not tried");
            assert_eq!(births, [true], "{what}: one birth");
        }
        assert_eq!(run(None, Ns::NONE).2, [false], "no netns: no network tried");
    }

    /// A measurement (6c review): what the probe's network part costs — a probe keeper's birth, its `true` and its
    /// end, without the network and with it (sockets planted, checked, routes and batch sent), alternating, 7 rounds
    /// each: the medians and every round, in ms.
    #[test]
    #[ignore = "a measurement: run by hand on a VM (6c review, the probe's cost)"]
    fn measure_the_probe() {
        let ns = Ns::session(true, true);
        let (mut without, mut with) = (Vec::new(), Vec::new());
        for _ in 0..7 {
            for network in [false, true] {
                let t0 = std::time::Instant::now();
                assert!(tried(ns, network).is_ok(), "the probe's keeper, network {network}");
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                if network { with.push(ms) } else { without.push(ms) }
            }
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (a, b) = (median(&mut without), median(&mut with));
        println!("the probe's keeper: without the network {a:.1} ms, with it {b:.1} ms (medians of 7)");
        println!("  without: {without:.1?}\n  with:    {with:.1?}");
    }

    /// A step goes through its code and back; the flags are each namespace's, and none without a user one.
    #[test]
    fn steps_and_flags() {
        for s in BirthStep::ALL {
            assert_eq!(BirthStep::from_code(s.code()), Some(s));
        }
        assert_eq!(BirthStep::from_code(99), None);
        assert_eq!(Ns::NONE.clone_flags(), 0);
        let all = Ns::session(true, true).clone_flags();
        let each = libc::CLONE_NEWUSER
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWNS;
        assert_eq!(all, each as u64);
        assert_eq!(
            Ns::session(false, false).clone_flags() & (libc::CLONE_NEWNET | libc::CLONE_NEWPID) as u64,
            0
        );
        assert_eq!(Ns::session(true, true).said(), "user net ipc pid mnt");
        assert_eq!(Ns::session(false, true).said(), "user ipc pid mnt");
        assert_eq!(
            [(Ns::session(true, true), true), (Ns::session(true, false), false), (Ns::NONE, true)]
                .map(|(n, s)| Tier::of(n, s).said()),
            ["A+", "B", "C"]
        );
    }
}
