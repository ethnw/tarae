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
    /// tarae's supervisor makes every call naming an address (`seccomp::Mode::Served`); Landlock handles TCP and
    /// UDP.
    Served,
    /// Its own, in the network namespace its keeper made, holding only `lo` (`Mode::OwnNetwork`); Landlock
    /// leaves TCP and UDP to it (`Ruleset::new_own_network`).
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

    /// How the internet sockets of a run of a keeper born with these go, `served` or not — decided once, in
    /// `linux::confine` (`Confine.net`).
    pub fn run_net(self, served: bool) -> RunNet {
        match (served, self.clone_flags() & libc::CLONE_NEWNET as u64 != 0) {
            (true, _) => RunNet::Served,
            (false, true) => RunNet::Own,
            (false, false) => RunNet::Plain,
        }
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

impl Seen {
    /// `failed`, with this system's answers read now.
    pub fn now(failed: BirthError) -> Seen {
        fn read(p: &str) -> Option<String> {
            std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
        }
        fn num<T: std::str::FromStr>(p: &str) -> Option<T> {
            read(p).and_then(|s| s.parse().ok())
        }
        let status = read("/proc/self/status").unwrap_or_default();
        let seccomp = status.lines().any(|l| l.strip_prefix("Seccomp:").is_some_and(|v| v.trim() == "2"));
        let apparmor = read("/proc/self/attr/apparmor/current").or_else(|| read("/proc/self/attr/current"));
        let version = read("/proc/version").unwrap_or_default();
        Seen {
            failed,
            restrict_userns: num("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
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

/// Why a keeper couldn't be born with its namespaces, from what was seen (§Tiers' table).
pub fn why(s: &Seen) -> String {
    let BirthError { step, errno } = s.failed;
    let refused = matches!(errno, libc::EPERM | libc::EACCES);
    if s.wsl1 {
        return "WSL 1 has no user namespaces".into();
    }
    if step == BirthStep::Clone {
        if errno == libc::ENOSPC || s.max_userns == Some(0) {
            return "user namespaces are turned off on this system (user.max_user_namespaces = 0)".into();
        }
        if s.userns_clone == Some(0) {
            return "unprivileged user namespaces are turned off on this system \
                    (kernel.unprivileged_userns_clone = 0)"
                .into();
        }
        if matches!(errno, libc::EPERM | libc::ENOSYS) && s.seccomp {
            return "a seccomp filter tarae runs under refuses new namespaces — Docker's default profile does"
                .into();
        }
    }
    if refused && step == BirthStep::Map && !s.dumpable {
        return "tarae runs non-dumpable (its ids changed after it started), so the id maps of what it makes are \
                root's to write"
            .into();
    }
    if refused && s.restrict_userns == Some(1) && step != BirthStep::Exec {
        return format!("{UBUNTU} (refused at {})", step.said());
    }
    if refused
        && step.mounts()
        && let Some(profile) = s.apparmor.as_deref().filter(|a| !a.is_empty() && *a != "unconfined")
    {
        return format!("an AppArmor profile tarae runs under ({profile}) refuses mounting");
    }
    if refused && step == BirthStep::Proc {
        return "a fresh /proc can't be mounted here — this /proc has paths masked over it (Docker's default \
                does), so no PID namespace"
            .into();
    }
    s.failed.to_string()
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
}

impl Probed {
    pub fn tier(&self) -> Tier {
        Tier::of(self.ns, self.served.is_ok())
    }
}

/// How long the probe's keeper has, from its birth to its `true` reaped: past it, no namespaces — never
/// success.
const PROBE_WITHIN: Duration = Duration::from_secs(4);

/// What the probe found here — probed the first time (a worker's: it makes a keeper or two and waits on them).
pub fn get() -> &'static Probed {
    static PROBED: OnceLock<Probed> = OnceLock::new();
    PROBED.get_or_init(|| {
        // (a measurement's other half, in the same process: no namespaces asked — `measure_gradle_pairs`)
        #[cfg(test)]
        if std::env::var_os("TARAE_TEST_NO_NS").is_some() {
            let served = super::supervisor::unavailable().map_or(Ok(()), Err);
            return Probed { ns: Ns::NONE, short: Some("none asked (TARAE_TEST_NO_NS)".into()), served };
        }
        let p = probe();
        let said = match &p.short {
            None => format!("namespaces: {} ({})", p.tier().said(), p.ns.said()),
            Some(why) => format!("namespaces: {} ({}) — {why}", p.tier().said(), p.ns.said()),
        };
        crate::log::line("sandbox", "info", &said);
        p
    })
}

/// The namespaces the keeper of a policy is born with: the probe's, without `net` while the policy opens a
/// port, this machine's ports or the network (D-a: until step 4's routing, its runs' sockets would be the
/// session's, where nothing listens and nothing goes out), and without a `/dev/shm` of its own where the
/// project or a place it writes is in this system's (`in_shm`: a private one would hide them).
pub fn for_policy(p: &super::Policy) -> Ns {
    let ns = get().ns;
    Ns { net: ns.net && !opens(p), shm: ns.shm && !in_shm(p), ..ns }
}

/// Whether the project, or a place or a file `p` writes, is in `/dev/shm` (real paths, as a policy has them).
pub fn in_shm(p: &super::Policy) -> bool {
    let shm = Path::new("/dev/shm");
    std::iter::once(&p.root).chain(&p.write).chain(&p.files).any(|w| w.starts_with(shm))
}

/// Whether `p` opens anything a network namespace would close: the network, this machine's ports, one port.
pub fn opens(p: &super::Policy) -> bool {
    p.network || p.localhost || !p.ports.is_empty()
}

/// A keeper made with every namespace, and a `true` through it; one whose `/proc` didn't mount (D12), again
/// without the PID namespace; any other failure, none — and why.
fn probe() -> Probed {
    let served = match super::supervisor::unavailable() {
        None => Ok(()),
        Some(why) => Err(why),
    };
    let all = Ns::session(true, true);
    let (ns, short) = match tried(all) {
        Ok(()) => (all, None),
        Err(Failed::Birth(e)) if e.step == BirthStep::Proc => {
            let no_pid = why(&Seen::now(e));
            match tried(Ns::session(true, false)) {
                Ok(()) => (Ns::session(true, false), Some(no_pid)),
                Err(e) => (Ns::NONE, Some(e.said())),
            }
        }
        Err(e) => (Ns::NONE, Some(e.said())),
    };
    Probed { ns, short, served }
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

/// A keeper born with `ns` and a `true` run through it, all within `PROBE_WITHIN` (on a thread: what hangs is
/// left to it, said as late).
fn tried(ns: Ns) -> Result<(), Failed> {
    let (tx, rx) = std::sync::mpsc::channel();
    crate::event::thread("tarae-nsprobe", move || {
        let _ = tx.send(true_through(ns));
    });
    rx.recv_timeout(PROBE_WITHIN).unwrap_or(Err(Failed::Late))
}

/// `true` run by a keeper born with `ns` for a root no session has, which ends after.
fn true_through(ns: Ns) -> Result<(), Failed> {
    let root = std::env::temp_dir().join(format!("tarae-nsprobe-{}", std::process::id()));
    let k = super::keeper::born_for_probe(&root, ns).map_err(|e| match BirthError::of(&e) {
        Some(b) => Failed::Birth(b),
        None => Failed::Run(e.to_string()),
    })?;
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

    /// A run's internet sockets are its own only not served and in a network namespace its keeper made — `net`
    /// without `user` makes none (`clone_flags`).
    #[test]
    fn a_run_owns_its_network_only_in_a_namespace_made_for_it() {
        assert_eq!(Ns::session(true, true).run_net(false), RunNet::Own);
        assert_eq!(Ns::session(true, true).run_net(true), RunNet::Served);
        assert_eq!(Ns::session(false, true).run_net(false), RunNet::Plain, "no network namespace");
        assert_eq!(Ns { net: true, ..Ns::NONE }.run_net(false), RunNet::Plain, "net without user: none made");
        assert_eq!(Ns::NONE.run_net(true), RunNet::Served);
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
            p.short.as_deref().map(|w| format!(" — {w}")).unwrap_or_default()
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
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
        assert_eq!(Ns::session(false, true).said(), "user ipc pid mnt");
        assert_eq!(
            [(Ns::session(true, true), true), (Ns::session(true, false), false), (Ns::NONE, true)]
                .map(|(n, s)| Tier::of(n, s).said()),
            ["A+", "B", "C"]
        );
    }
}
