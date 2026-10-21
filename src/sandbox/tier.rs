//! A session keeper's namespaces (`docs/design/sandbox-keeper.md` §Tiers): what this system lets a keeper be
//! born with — found once, by making one for real and running `true` through it (`probe`) — and, where it
//! can't, why, in a sentence (`why`: a pure function of what was seen, `Seen`).
//!
//! With user namespaces a keeper is PID 1 of the session's user, ipc, pid and mount namespaces — and its
//! network one when its policy opens nothing (`for_policy`: until step 4's routing, an opened port or the
//! network would reach the session's empty loopback); where a fresh `/proc` can't be mounted (Docker's masked
//! `/proc`), the same without the PID namespace (D12); without user namespaces none. Served, the first two are
//! tier own, the last host; not serving (`supervisor::served`), any of them is offline. Where Yama's `ptrace_scope`
//! 2 decides, the probe's keeper also holds a run while tarae tries the supervisor's two calls on it
//! (`supervisor::reaches`): served in the session's namespaces is what that found.

use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

// (what a keeper's namespaces are, its tier and how a run's sockets go: pure, `guarantees.rs`'s — built everywhere)
pub use super::guarantees::{Ns, RunNet, Tier};

impl Ns {
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

    /// What process `pid` is in apart from tarae's own namespaces — each of its `/proc/<pid>/ns/` links against
    /// `/proc/self/ns/`'s (`of_apart`) — with a `/dev/shm` of its own where `shm` and it has a mount namespace of its
    /// own (its birth mounts one there, or fails). What a keeper is in, not the flags its birth asked.
    pub fn of_pid(pid: u32, shm: bool) -> io::Result<Ns> {
        let link = |of: &str, name: &str| {
            std::fs::read_link(format!("/proc/{of}/ns/{name}"))
                .map_err(|e| io::Error::new(e.kind(), format!("/proc/{of}/ns/{name}: {e}")))
        };
        let mut apart = [false; 5];
        for (i, name) in NS_LINKS.iter().enumerate() {
            apart[i] = link(&pid.to_string(), name)? != link("self", name)?;
        }
        Ns::of_apart(apart, shm)
    }

    /// `of_pid` from which of `NS_LINKS` are apart from tarae's: the session's user namespace comes with its ipc,
    /// mount and network ones (`clone_flags`; the probe always asks the network one), and a PID one only with it —
    /// anything else is no keeper's.
    fn of_apart([user, ipc, mnt, net, pid]: [bool; 5], shm: bool) -> io::Result<Ns> {
        if user != ipc || user != mnt || user != net || (!user && pid) {
            return Err(io::Error::other(format!(
                "in namespaces no keeper is born with: apart from tarae's in {}",
                apart_in([user, ipc, mnt, net, pid])
            )));
        }
        Ok(Ns { user, net, pid, shm: user && shm })
    }

    /// The links of `NS_LINKS` these say apart from tarae's, field by field: `user ipc mnt net pid`, or `none` — for
    /// a birth that isn't what it asked (`net` without `user` asks what `clone_flags` makes none of).
    pub fn apart_in(self) -> String {
        apart_in([self.user, self.user, self.user, self.net, self.pid])
    }
}

/// Which of `NS_LINKS` are apart, by name — `none` when none is.
fn apart_in(apart: [bool; 5]) -> String {
    let names: Vec<&str> = NS_LINKS.iter().zip(apart).filter_map(|(n, a)| a.then_some(*n)).collect();
    if names.is_empty() { "none".to_string() } else { names.join(" ") }
}

/// The links of `/proc/<pid>/ns/` an `Ns` stands for (`Ns::of_pid`), in `of_apart`'s order.
const NS_LINKS: [&str; 5] = ["user", "ipc", "mnt", "net", "pid"];

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
}

impl BirthStep {
    const ALL: [BirthStep; 11] = [
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
        }
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
    if refused && s.restrict_userns == Some(1) && step != BirthStep::Exec {
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

/// `why` in a few words, people's — the sandbox panel's state line (9e); `why`'s whole words stay in the log, the
/// report and `tarae state`.
pub fn brief(s: &Seen) -> String {
    match cause(s) {
        Cause::Wsl1 => "WSL 1 has no user namespaces".into(),
        Cause::MaxZero | Cause::CloneOff => "user namespaces are turned off".into(),
        Cause::Seccomp => "a seccomp filter blocks namespaces".into(),
        Cause::NotDumpable => "tarae isn't dumpable".into(),
        Cause::Restriction => "AppArmor blocks user namespaces".into(),
        Cause::ProfileMounts => "an AppArmor profile blocks mounts".into(),
        Cause::MaskedProc => "/proc is masked, no PID namespace".into(),
        Cause::Other => format!("namespaces failed at {}", s.failed.step.said()),
    }
}

/// The hint (6h; a toast once, and the tier's line): where Ubuntu's restriction is why a keeper got no namespaces
/// and tarae runs under no profile — runs in `tier`, why, and the step that gives them namespaces of their own
/// (`apparmor::hint`, by the binary's `place`; 9f, `Hint`). Under a profile that still didn't let it, none: that
/// profile isn't the one tarae ships.
pub fn hint(s: &Seen, tier: Tier, place: &super::apparmor::Place) -> Option<super::guarantees::Hint> {
    let ours_missing = cause(s) == Cause::Restriction && !confined(s.apparmor.as_deref());
    ours_missing.then(|| super::guarantees::Hint::of(tier, place))
}

/// What the probe found.
#[derive(Clone, Debug)]
pub struct Probed {
    /// The namespaces a session's keeper is born with — `net` among them (a policy that opens something goes
    /// without: `for_policy`).
    pub ns: Ns,
    /// Why not all of them (no `pid`: D12's reason; none: why not) — None: all.
    pub short: Option<String>,
    /// The same in a few words (`brief`; the sandbox panel's, 9e) — without why it isn't served, which the guarantees
    /// say themselves.
    pub brief: Option<String>,
    /// Whether tarae serves the calls of runs in the namespaces found (`supervisor::served`).
    pub served: Result<(), String>,
    /// Ubuntu's restriction kept the namespaces from a tarae under no profile: why, and the step (`hint`).
    pub hint: Option<super::guarantees::Hint>,
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
            let served = super::supervisor::served(false);
            return Probed {
                ns: Ns::NONE,
                short: Some("none asked (TARAE_TEST_NO_NS)".into()),
                brief: Some("none asked".into()),
                served,
                hint: None,
            };
        }
        #[allow(unused_mut)]
        let mut p = probe();
        // (M4's cut: the session's namespaces without the PID one — no fresh `/proc` — on a machine that gives
        // all of them)
        #[cfg(test)]
        if std::env::var_os("TARAE_TEST_NO_PID").is_some() && p.ns.pid {
            p.ns.pid = false;
            p.short = Some("no PID namespace asked (TARAE_TEST_NO_PID)".into());
            p.brief = Some("no PID namespace asked".into());
        }
        let said = match &p.short {
            None => format!("namespaces: {} ({})", p.tier().said(), p.ns.said()),
            Some(why) => format!("namespaces: {} ({}) — {why}", p.tier().said(), p.ns.said()),
        };
        let said = match &p.hint {
            Some(hint) => format!("{said} — {}", hint.said),
            None => said,
        };
        let said = match &p.served {
            Ok(()) => said,
            Err(why) => format!("{said} — not served: {why}"),
        };
        crate::log::line("sandbox", "info", &said);
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

/// Whether the project, or a place or a file `p` writes, is in `/dev/shm` (real paths, as a policy has them).
pub fn in_shm(p: &super::Policy) -> bool {
    let shm = Path::new("/dev/shm");
    std::iter::once(&p.root).chain(&p.write).chain(&p.files).any(|w| w.starts_with(shm))
}

/// A keeper made with every namespace, and a `true` through it; one whose `/proc` didn't mount (D12), again
/// without the PID namespace; any other failure, none — and why. Served as `supervisor::served` says for what was
/// found — where Yama's scope 2 decides, after the keeper's run was tried (`supervisor::asks_session`).
fn probe() -> Probed {
    let asks = super::supervisor::asks_session();
    probe_with(super::supervisor::served, |ns| tried(ns, asks))
}

/// `probe` where serving a keeper with the session's user namespace or without is `served` (asked once a birth
/// was tried: a keeper's run tried there decides it) and a keeper's birth and run is `tried` (tests stand in).
fn probe_with(
    served: impl Fn(bool) -> Result<(), String>,
    mut tried: impl FnMut(Ns) -> Result<(), Failed>,
) -> Probed {
    let all = Ns::session(true, true);
    // (no namespaces: why, and the hint where Ubuntu's restriction is why — for the binary running)
    let without = |e: Failed| {
        let tier = Tier::of(Ns::NONE, served(false).is_ok());
        let place = || super::apparmor::place_of(&std::env::current_exe().unwrap_or_default());
        let hint = match &e {
            Failed::Birth(b) => hint(&Seen::now(*b), tier, &place()),
            _ => None,
        };
        let short = e.brief();
        (Ns::NONE, Some(e.said()), Some(short), hint)
    };
    let (ns, short, brief_said, hint) = match tried(all) {
        Ok(()) => (all, None, None, None),
        Err(Failed::Birth(e)) if e.step == BirthStep::Proc => {
            let seen = Seen::now(e);
            let (no_pid, few) = (why(&seen), brief(&seen));
            match tried(Ns::session(true, false)) {
                Ok(()) => (Ns::session(true, false), Some(no_pid), Some(few), None),
                Err(e) => without(e),
            }
        }
        Err(e) => without(e),
    };
    Probed { ns, short, brief: brief_said, served: served(ns.user), hint }
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
    /// `said` in a few words (`brief`).
    fn brief(&self) -> String {
        match self {
            Failed::Birth(e) => brief(&Seen::now(*e)),
            Failed::Run(_) => "a run in namespaces failed".into(),
            Failed::Late => "namespaces didn't answer in time".into(),
        }
    }

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

/// A keeper born with `ns` and a `true` run through it, all within `PROBE_WITHIN` (on a thread: what hangs is left
/// to it, said as late) — and where `asks` (Yama's scope 2), a held run tried as the supervisor would read it.
fn tried(ns: Ns, asks: bool) -> Result<(), Failed> {
    let (tx, rx) = std::sync::mpsc::channel();
    crate::event::thread("tarae-nsprobe", move || {
        let _ = tx.send(true_through(ns, asks));
    });
    rx.recv_timeout(PROBE_WITHIN).unwrap_or(Err(Failed::Late))
}

/// `true` run by a keeper born with `ns` for a root no session has, which ends after. Where `asks` and `ns` has the
/// session's user namespace, first a `sleep` it holds, tried with the supervisor's two calls and ended
/// (`held_read`): what was found kept (`supervisor::tried_in_session`).
fn true_through(ns: Ns, asks: bool) -> Result<(), Failed> {
    let (ran, read) = true_through_holding(ns, (asks && ns.user).then_some("sleep"));
    if let Some(read) = read {
        crate::log::line("sandbox", "info", &format!("a run in the session's namespaces, read: {read:?}"));
        super::supervisor::tried_in_session(read);
    }
    ran
}

/// `true_through`, the run held and read first being `held` (tests name one that doesn't start) — what was read
/// beside how `true` went, kept by nobody: a held run that didn't start is an answer of its own, never the
/// namespaces failing (review C2: a `sleep` not found, `true` there, gave the session none).
fn true_through_holding(ns: Ns, held: Option<&str>) -> (Result<(), Failed>, Option<Result<(), String>>) {
    let mut read = None;
    let root = std::env::temp_dir().join(format!("tarae-nsprobe-{}", std::process::id()));
    let k = match super::keeper::born_for_probe(&root, ns) {
        Ok(k) => k,
        Err(e) => match BirthError::of(&e) {
            Some(b) => return (Err(Failed::Birth(b)), None),
            None => return (Err(Failed::Run(e.to_string())), None),
        },
    };
    let ran = match crate::landlock::abi() {
        // (no Landlock here: the birth is what's probed — the sandbox itself is missing anyway)
        Err(_) => Ok(()),
        Ok(abi) => (|| {
            let net = ns.run_net(false);
            let mut rules = match net {
                RunNet::Own => crate::landlock::Ruleset::new_own_network(abi)?,
                _ => crate::landlock::Ruleset::new(abi)?,
            };
            rules.allow(Path::new("/"), crate::landlock::ALL)?;
            if let Some(program) = held {
                read = Some(held_read(&k, program, rules.fd(), net));
            }
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
    (ran, read)
}

/// `program` held by keeper `k` (`program 30`) and tried with the supervisor's two calls (`supervisor::reaches`, by
/// its pidfd), then ended by its keeper, which reaps it (`keeper::signal`) — never by its number, which a run that
/// ended before its exec may have given to another process (review C1/C3). One that didn't start is said so.
fn held_read(
    k: &super::keeper::Keeper,
    program: &str,
    rules: std::os::fd::BorrowedFd<'_>,
    net: RunNet,
) -> Result<(), String> {
    let mut held = std::process::Command::new(program);
    held.arg("30");
    let s = k
        .start(&held, &crate::landlock::thread_name(), rules, net)
        .map_err(|e| format!("a run held to be read didn't start: {e}"))?;
    let pid = s.spawned.pid;
    let read = match super::keeper::pidfd(pid) {
        Some(pidfd) => super::supervisor::reaches(pid, std::os::fd::AsFd::as_fd(&*pidfd), program),
        None => Err("its pidfd isn't known".into()),
    };
    super::keeper::signal(pid, libc::SIGKILL);
    let _ = crate::spawn::wait(pid);
    read
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
        assert_eq!(Ns::session(true, true).run_net(false), RunNet::Own);
        assert_eq!(Ns::session(true, false).run_net(true), RunNet::Routed);
        assert_eq!(Ns { net: true, ..Ns::NONE }.run_net(false), RunNet::Plain, "net without user: none made");
        assert_eq!(Ns { net: true, ..Ns::NONE }.run_net(true), RunNet::Served);
        assert_eq!(Ns::NONE.run_net(true), RunNet::Served);
        assert_eq!(Ns::NONE.run_net(false), RunNet::Plain);
    }

    /// The probe asks one keeper, with the session's netns — never without it — and the session keeps it. Not
    /// served, the same namespaces: tier offline. (Keepers stood in for: what each was asked.)
    #[test]
    fn the_probe_always_asks_the_sessions_network_namespace() {
        let asked = |served: Result<(), String>| {
            let mut asked = Vec::new();
            let p = probe_with(
                |_| served.clone(),
                |ns: Ns| {
                    asked.push(ns.netns());
                    Ok(())
                },
            );
            (asked, p)
        };
        let (taken, p) = asked(Ok(()));
        assert_eq!(taken, [true], "one keeper, in a netns");
        assert!(p.ns.netns(), "{:?}", p.ns);
        assert_eq!(p.tier(), Tier::Own);
        let (taken, p) = asked(Err(super::super::guarantees::PIDFD_REFUSED.into()));
        assert_eq!(taken, [true]);
        assert_eq!(p.tier(), Tier::Offline);
    }

    /// Served is asked for the namespaces found: where only a keeper with the session's user namespace is served
    /// (Yama's scope 2), the probe that gets them is tier own, the one that falls back to none offline — in
    /// `YAMA_2_WITHOUT_NS`'s words. (Keepers stood in for.)
    #[test]
    fn served_is_asked_for_the_namespaces_found() {
        let without = super::super::guarantees::YAMA_2_WITHOUT_NS;
        let served = |user: bool| if user { Ok(()) } else { Err(without.to_string()) };
        let p = probe_with(served, |_| Ok(()));
        assert_eq!((p.tier(), p.served.clone()), (Tier::Own, Ok(())));
        let clone = BirthError { step: BirthStep::Clone, errno: libc::EPERM };
        let p = probe_with(served, |ns| if ns.user { Err(Failed::Birth(clone)) } else { Ok(()) });
        assert_eq!((p.tier(), p.served), (Tier::Offline, Err(without.into())));
    }

    /// A held run that doesn't start (a program not found where `true` is) is the read's answer — said, kept as not
    /// read — and the probe goes on to `true`: never the namespaces failing, which gave the session none (review C2).
    /// A real keeper in the session's namespaces, where this system gives them and Landlock is there.
    #[test]
    fn a_held_run_that_doesnt_start_isnt_the_namespaces_failing() {
        let ns = get().ns;
        if !ns.user || crate::landlock::abi().is_err() {
            let why = if ns.user { "no Landlock" } else { "no user namespaces here" };
            let test = "sandbox::tier::tests::a_held_run_that_doesnt_start_isnt_the_namespaces_failing";
            let line = format!("{}{test} — {why}\n", crate::test_results::SELF_SKIP);
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            return;
        }
        let (ran, read) = true_through_holding(ns, Some("tarae-no-such-program"));
        if let Err(e) = ran {
            panic!("`true` didn't run: {}", e.said());
        }
        let read = read.expect("a read was asked").unwrap_err();
        assert!(read.starts_with("a run held to be read didn't start"), "{read}");
    }

    /// On the scope 2 VM (`TARAE_EXPECT_YAMA=2`, `TARAE_EXPECT_TIER=own`: CONTRIBUTING's colima recipe) the probe reads its
    /// keeper's run and the session is served — tier own, never offline for a run tried before its exec. Elsewhere
    /// nothing to check: the scope decides nothing, or the tier is another.
    #[test]
    fn at_yama_2_the_probe_serves_the_sessions_namespaces() {
        let at = |name: &str, value: &str| std::env::var(name).is_ok_and(|v| v == value);
        if !(at("TARAE_EXPECT_YAMA", "2") && at("TARAE_EXPECT_TIER", "own")) {
            return;
        }
        let p = get();
        assert_eq!((p.tier(), &p.served), (Tier::Own, &Ok(())), "{:?}", p.short);
        assert_eq!(
            super::super::supervisor::served(false),
            Err(super::super::guarantees::YAMA_2_WITHOUT_NS.into())
        );
    }

    /// A keeper is what its `/proc/<pid>/ns/` links say (`Ns::of_pid`), not what its birth asked: the session's user
    /// namespace with its ipc and mount ones, a network and a PID one each its own, `/dev/shm` its own only with them
    /// — and links no birth makes (a user namespace without its mount one, a network one without the user one) no
    /// keeper at all.
    #[test]
    fn a_keeper_is_what_its_links_say() {
        let of = |apart: [bool; 5], shm: bool| Ns::of_apart(apart, shm).ok();
        assert_eq!(of([true; 5], true), Some(Ns::session(true, true)));
        assert_eq!(of([true, true, true, true, false], true), Some(Ns::session(true, false)), "A");
        assert_eq!(
            of([true; 5], false),
            Some(Ns { shm: false, ..Ns::session(true, true) }),
            "/dev/shm shared"
        );
        assert_eq!(of([false; 5], true), Some(Ns::NONE), "none: no /dev/shm of its own either");
        for apart in [
            [true, true, true, false, true],
            [true, false, true, true, true],
            [true, true, false, true, true],
            [false, true, true, false, false],
            [false, false, false, true, false],
            [false, false, false, false, true],
        ] {
            assert_eq!(of(apart, true), None, "{apart:?}");
        }
    }

    /// 9e: the panel's few words for each cause `why` tells apart — the cause named (AppArmor, seccomp, WSL, a masked
    /// /proc), short enough for a 60-column panel's row with the next reason after it (`… · Landlock ABI 2: partial`), never its whole
    /// reason.
    #[test]
    fn the_panel_says_why_in_a_few_words() {
        let cases: Vec<(Seen, &str)> = vec![
            (
                Seen { restrict_userns: Some(1), ..seen(BirthStep::Propagation, libc::EPERM) },
                "AppArmor blocks user namespaces",
            ),
            (
                Seen { seccomp: true, ..seen(BirthStep::Clone, libc::EPERM) },
                "a seccomp filter blocks namespaces",
            ),
            (seen(BirthStep::Clone, libc::ENOSPC), "user namespaces are turned off"),
            (
                Seen { userns_clone: Some(0), ..seen(BirthStep::Clone, libc::EPERM) },
                "user namespaces are turned off",
            ),
            (Seen { wsl1: true, ..seen(BirthStep::Clone, libc::EINVAL) }, "WSL 1 has no user namespaces"),
            (seen(BirthStep::Proc, libc::EPERM), "/proc is masked, no PID namespace"),
            (
                Seen {
                    apparmor: Some("docker-default (enforce)".into()),
                    ..seen(BirthStep::Propagation, libc::EACCES)
                },
                "an AppArmor profile blocks mounts",
            ),
            (Seen { dumpable: false, ..seen(BirthStep::Map, libc::EACCES) }, "tarae isn't dumpable"),
            (seen(BirthStep::Clone, libc::EPERM), "namespaces failed at making its namespaces"),
        ];
        for (s, words) in cases {
            let few = brief(&s);
            assert_eq!(few, words, "{:?} {} — why: {}", s.failed.step, s.failed.errno, why(&s));
            assert!(few.len() + " · ".len() <= 51, "a 60-column panel's row: {few}");
        }
    }

    /// 9e: the probe's few words go where its whole reason goes, from the same failure — none with every namespace;
    /// without the PID namespace and without any, `brief` of what was seen beside `why` of it (never the one in the
    /// other's place); a run that failed or a keeper that didn't answer in their own few words. (Keepers stood in for.)
    #[test]
    fn the_probe_says_in_a_few_words_what_it_says_whole() {
        let probe = |fail: &dyn Fn(Ns) -> Result<(), Failed>| probe_with(|_| Ok(()), fail);
        let p = probe(&|_| Ok(()));
        assert_eq!((p.short, p.brief), (None, None), "every namespace: nothing to say");
        let proc = BirthError { step: BirthStep::Proc, errno: libc::EPERM };
        let p = probe(&|ns| if ns.pid { Err(Failed::Birth(proc)) } else { Ok(()) });
        assert_eq!(p.ns, Ns::session(true, false), "no PID namespace");
        let seen = Seen::now(proc);
        assert_eq!((p.short, p.brief), (Some(why(&seen)), Some(brief(&seen))));
        let clone = BirthError { step: BirthStep::Clone, errno: libc::EPERM };
        let p = probe(&|ns| if ns.user { Err(Failed::Birth(clone)) } else { Ok(()) });
        assert_eq!(p.ns, Ns::NONE, "no namespaces");
        let seen = Seen::now(clone);
        assert_eq!((p.short, p.brief), (Some(why(&seen)), Some(brief(&seen))));
        let p = probe(&|ns| if ns.user { Err(Failed::Late) } else { Ok(()) });
        assert!(p.short.as_deref().is_some_and(|w| w.contains("didn't answer within")), "{:?}", p.short);
        assert_eq!(p.brief.as_deref(), Some("namespaces didn't answer in time"));
        let p = probe(&|ns| if ns.user { Err(Failed::Run("`true` ended 1".into())) } else { Ok(()) });
        assert!(p.short.as_deref().is_some_and(|w| w.ends_with("`true` ended 1")), "{:?}", p.short);
        assert_eq!(p.brief.as_deref(), Some("a run in namespaces failed"));
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
        // (the restriction's remedy: a profile for tarae first, the setting second)
        let ubuntu = why(&Seen { restrict_userns: Some(1), ..seen(BirthStep::Devpts, libc::EPERM) });
        assert!(ubuntu.find("AppArmor profile") < ubuntu.find("that setting at 0"), "{ubuntu}");
    }

    /// The hint speaks only where Ubuntu's restriction is why a keeper got nothing and tarae runs under no profile
    /// — that's what the profile would change. Not where the restriction is off or something else failed first
    /// (Docker's filter refuses the clone before the restriction is reached), and not under a profile already: one that didn't help
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
        let hint = |s: &Seen, tier| super::hint(s, tier, &place).map(|h| h.said);
        let unconfined = restricted(BirthStep::Map, Some("unconfined"));
        let said = hint(&unconfined, Tier::Host).expect("restricted, no profile: the hint");
        assert!(said.contains("tier host") && said.contains("sudo apparmor_parser -r"), "{said}");
        assert!(
            hint(&unconfined, Tier::Offline).is_some_and(|h| h.contains("tier offline")),
            "the tier the runs are in"
        );
        assert!(
            hint(&restricted(BirthStep::Devpts, None), Tier::Host).is_some(),
            "no label read: no profile"
        );
        let none = [
            (restricted(BirthStep::Map, Some("/usr/bin/tarae (unconfined)")), "under a profile already"),
            (Seen { restrict_userns: Some(0), ..unconfined.clone() }, "the restriction off"),
            (Seen { restrict_userns: None, ..unconfined.clone() }, "no such setting"),
            (
                Seen { seccomp: true, ..restricted(BirthStep::Clone, Some("unconfined")) },
                "Docker's filter first",
            ),
            (
                Seen { failed: BirthError { step: BirthStep::Map, errno: libc::EINVAL }, ..unconfined },
                "not refused",
            ),
        ];
        for (s, what) in none {
            assert_eq!(hint(&s, Tier::Host), None, "{what}");
        }
    }

    /// 9f: a run's guarantees carry the hint where the probe's would be said — Ubuntu's restriction on and no
    /// profile on tarae — saying what the profile gives (the session's namespaces, served or not) in the panel's
    /// words, the whole hint in `tarae state`'s. A profile attached, or a system without the restriction
    /// (Debian), none; nor Seatbelt's, which has no tiers.
    #[test]
    fn the_profile_hint_says_how_to_raise_the_tier() {
        use crate::sandbox::guarantees::{Found, Guarantees};
        let place = crate::sandbox::apparmor::Place {
            path: "/usr/bin/tarae".into(),
            root_owned: Ok(()),
            packaged: true,
        };
        let ubuntu = |label: &str| Seen {
            restrict_userns: Some(1),
            apparmor: Some(label.to_string()),
            ..seen(BirthStep::Map, libc::EPERM)
        };
        // (as `linux::confine` builds a run's: the probe's hint, a keeper without namespaces, served or not)
        let run = |s: &Seen, served: bool| {
            let tier = Tier::of(Ns::NONE, served);
            Guarantees::landlock(Found {
                ns: Ns::NONE,
                served: if served { Ok(()) } else { Err("Yama's ptrace_scope is 3".into()) },
                abi: 4,
                ptys: Some(4),
                lo_missing: Vec::new(),
                short: Some(why(s)),
                brief: Some(brief(s)),
                hint: super::hint(s, tier, &place),
                network: true,
            })
        };
        let c = run(&ubuntu("unconfined"), true);
        let said = c.hint_brief().expect("restricted, no profile: the hint");
        assert_eq!(said, "tarae's AppArmor profile would give own namespaces");
        let step = c.step_row().map(|r| r.0);
        assert_eq!(step.as_deref(), Some("copy the step for own namespaces"), "the step's row");
        let whole = c.json()["hint"].as_str().unwrap_or_default().to_string();
        assert!(
            whole.contains("To give them namespaces of their own")
                && whole.contains("sudo apparmor_parser -r"),
            "{whole}"
        );
        let offline = run(&ubuntu("unconfined"), false);
        assert!(
            offline.hint.as_ref().is_some_and(|h| h.said.contains("tier offline")),
            "not served: offline"
        );
        assert_eq!(offline.hint_brief(), Some(said), "not served, the profile still gives namespaces");
        let debian = Seen { restrict_userns: None, ..ubuntu("unconfined") };
        let none = [
            (run(&ubuntu("/usr/bin/tarae (unconfined)"), true), "a profile attached"),
            (run(&debian, true), "Debian: no restriction"),
            (Guarantees::seatbelt(true, true), "Seatbelt"),
        ];
        for (g, what) in none {
            assert_eq!((g.hint_brief(), &g.json()["hint"]), (None, &serde_json::Value::Null), "{what}");
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
    /// the one found.
    #[test]
    fn the_probe_here() {
        let p = get();
        let line = format!(
            "tier here: {} ({}){}\n",
            p.tier().said(),
            p.ns.said(),
            p.short.as_deref().map(|w| format!(" — {w}")).unwrap_or_default(),
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        // (6h: the label tarae runs under, whether its runs lose user namespaces, the hint — the profile's run on a
        // VM reads them here; `TARAE_EXPECT_HINT` 1 or 0 says whether one is expected)
        let profile = format!(
            "label: {} · runs refuse user namespaces: {} · hint: {}\n",
            label().unwrap_or_default(),
            runs_refuse_userns(),
            p.hint.as_ref().map_or("none", |h| h.said.as_str())
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
        assert_eq!(p.short.is_none(), p.ns == Ns::session(true, true), "{line}");
        assert!(p.ns.user || p.ns == Ns::NONE, "{line}");
        if p.ns.user && !p.ns.pid {
            assert!(p.short.as_deref().is_some_and(|w| w.contains("/proc")), "{line}");
        }
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
        assert_eq!(Ns::session(true, false).said(), "user net ipc mnt");
        assert_eq!(
            [
                (Ns::session(true, true), true),
                (Ns::session(true, false), true),
                (Ns::session(true, false), false),
                (Ns::NONE, true),
                (Ns::NONE, false)
            ]
            .map(|(n, s)| Tier::of(n, s).said()),
            ["own", "own", "offline", "host", "offline"]
        );
    }
}
