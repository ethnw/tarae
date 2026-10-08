//! 6g: a cgroup per run (`target/sb/step6g-design.md`). Where a session's keeper can have a delegated cgroup of its
//! own — a scope — each of its runs starts in a cgroup of its own below it (`clone3(CLONE_INTO_CGROUP)`, the keeper's),
//! so that a rule of the session's ruleset can tell runs apart by `socket cgroupv2` (6d; nothing reads it before).
//! Here: whether this tarae can (`conditions`, decided once, with the probe — D-b), a new keeper put in a scope of its
//! own (`place`), and the level a rule names a run's cgroup at (`Scope.level`).
//!
//! The level: `socket cgroupv2 level L` counts from the sender's cgroup namespace's root on some kernels and from the
//! machine's root on others (spike6 Q2: Debian's 6.1 and 7.3 the one, Ubuntu's 6.8.0-1013 the other). The two agree
//! only where tarae's cgroup namespace is the machine's — a condition here (the user, 2026-10-08): L is then a run's
//! depth below `/`, on every kernel, with nothing to learn.
//!
//! Only sessions with the TPROXY network ask (`tier::Probed.cgroups`): nothing else would read a run's cgroup.

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The initial cgroup namespace as `/proc/self/ns/cgroup` names it: its inode is fixed (`PROC_CGROUP_INIT_INO`),
/// whatever the machine.
const INIT_NS: &str = "cgroup:[4026531835]";
/// The initial PID namespace, as `/proc/self/ns/pid` names it (`PROC_PID_INIT_INO`, as fixed).
const INIT_PID_NS: &str = "pid:[4026531836]";
/// `statfs`'s type of a cgroup v2 hierarchy (`CGROUP2_SUPER_MAGIC`).
const CGROUP2: u32 = 0x6367_7270;
/// Where the hierarchy is: a cgroup's path is a folder below it.
const ROOT: &str = "/sys/fs/cgroup";

/// Where a keeper's scope comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The user's systemd manager, over the user bus at this socket (`StartTransientUnit`, `Delegate=yes`).
    UserBus(PathBuf),
    /// Tests: a folder made as systemd's `Delegate=yes` makes one (`cargo xtask linux --cgroup`), each keeper put in a
    /// folder of its own there with a `cgroup.procs` write — which the root of a test's container may make.
    #[cfg(test)]
    Given(PathBuf),
}

impl Source {
    /// In the tier's line.
    pub fn said(&self) -> String {
        match self {
            Source::UserBus(bus) => format!("a scope from the user's systemd manager ({})", bus.display()),
            #[cfg(test)]
            Source::Given(folder) => format!("a folder in {} (TARAE_TEST_CGROUP)", folder.display()),
        }
    }
}

/// What decides whether runs get cgroups of their own here, as read (`facts`).
#[derive(Clone, Debug)]
pub struct Facts {
    /// `/sys/fs/cgroup` is a cgroup v2 hierarchy (the only one: v1's controllers aren't mounted there then).
    pub cgroup2: bool,
    /// What `/proc/self/ns/cgroup` names.
    pub ns: Option<String>,
    /// What `/proc/self/ns/pid` names.
    pub pid_ns: Option<String>,
    /// `clone3` is there and not refused (a container's seccomp answers it `ENOSYS`).
    pub clone3: bool,
    pub source: Result<Source, String>,
}

/// Whether runs get a cgroup of their own here — where a keeper's scope comes from — or the first thing missing, in
/// the tier's words. Without `clone3` there's no per-run cgroup at all rather than a `cgroup.procs` write per run (its
/// 10–50 ms tail, spike6 Q4). The user's manager takes the keeper by its number (`PIDs`), read in the manager's PID
/// namespace — the machine's: tarae must number processes as it does, so in the initial one too (else it would name
/// another process). Checked rather than sending a pidfd (`PIDFDs`, systemd ≥ 253): the keeper's number can't be
/// another's while tarae holds it unreaped — only another namespace's numbering can make it so — and a pidfd would
/// take descriptors passed on the bus and `PIDs` beside it for older managers, for a case (tarae in a PID namespace
/// of its own, the user's bus in reach) only a container makes. The tests' folder (`Given`) is placed by tarae in its
/// own numbering: no PID namespace asked there.
pub fn conditions(f: &Facts) -> Result<Source, String> {
    if !f.cgroup2 {
        return Err(format!("{ROOT} isn't a cgroup v2 hierarchy"));
    }
    if f.ns.as_deref() != Some(INIT_NS) {
        return Err(
            "tarae runs in a cgroup namespace of its own (a container?): the rules' level can't be told"
                .into(),
        );
    }
    if !f.clone3 {
        return Err("clone3 is missing or refused here (a container's seccomp?)".into());
    }
    let source = f.source.clone()?;
    if matches!(source, Source::UserBus(_)) && f.pid_ns.as_deref() != Some(INIT_PID_NS) {
        return Err(
            "tarae runs in a PID namespace of its own (a container?): the user manager would take the \
                    keeper's number for another process"
                .into(),
        );
    }
    Ok(source)
}

/// This machine's `Facts`.
pub fn facts() -> Facts {
    Facts {
        cgroup2: crate::fswatch::statfs(Path::new(ROOT)).is_some_and(|st| st.f_type as u32 == CGROUP2),
        ns: std::fs::read_link("/proc/self/ns/cgroup").ok().map(|p| p.to_string_lossy().into_owned()),
        pid_ns: std::fs::read_link("/proc/self/ns/pid").ok().map(|p| p.to_string_lossy().into_owned()),
        clone3: clone3_allowed(),
        source: source(),
    }
}

/// Whether `clone3` gets to the kernel: asked with no arguments at all, which the kernel refuses (`EINVAL`) before
/// making anything; a filter's refusal, or a kernel before 5.3, is `ENOSYS`.
fn clone3_allowed() -> bool {
    // SAFETY: a size of 0 is refused before the kernel reads anything (`copy_clone_args_from_user`): no process made.
    let r = unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0usize) };
    r == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS)
}

/// Where this tarae's keepers' scopes come from. Tests never ask the user's manager: only what
/// `TARAE_TEST_CGROUP` gives (`cargo xtask linux --cgroup`).
fn source() -> Result<Source, String> {
    #[cfg(test)]
    return match std::env::var_os("TARAE_TEST_CGROUP") {
        Some(folder) => Ok(Source::Given(folder.into())),
        None => Err("no folder given to the tests (TARAE_TEST_CGROUP: cargo xtask linux --cgroup)".into()),
    };
    #[cfg(not(test))]
    user_bus(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).as_deref(),
        |p| p.exists(),
    )
    .map(Source::UserBus)
}

/// The user bus's socket: the first `unix:path=` of `address` (`DBUS_SESSION_BUS_ADDRESS`: `;`-separated, its values
/// `%`-escaped), else `$XDG_RUNTIME_DIR/bus` where it's there (`exists`) — what systemd's own tools fall back to.
pub fn user_bus(
    address: Option<&str>,
    runtime: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let named = address.into_iter().flat_map(|a| a.split(';')).find_map(|one| {
        let keys = one.strip_prefix("unix:")?;
        keys.split(',').find_map(|kv| kv.strip_prefix("path=")).and_then(unescaped)
    });
    if let Some(path) = named {
        return Ok(path.into());
    }
    match runtime.map(|r| r.join("bus")) {
        Some(bus) if exists(&bus) => Ok(bus),
        _ => Err("no systemd user manager to delegate a cgroup (no user bus)".into()),
    }
}

/// A D-Bus address value with its `%xx` escapes undone — None: an escape that isn't one.
fn unescaped(v: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut b = v.bytes();
    while let Some(c) = b.next() {
        match c {
            b'%' => {
                let hex = [b.next()?, b.next()?];
                out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
            }
            c => out.push(c),
        }
    }
    String::from_utf8(out).ok()
}

/// A keeper's scope: the folder of the cgroup it was moved into (for the keeper: `Msg::Cgroup`), its path below
/// the hierarchy's root, and the level of a run's own cgroup below it in a `socket cgroupv2` rule.
pub struct Scope {
    pub dir: OwnedFd,
    pub path: String,
    pub level: u32,
}

/// Puts keeper `pid` in a scope of its own from `source`, and opens it.
pub fn place(source: &Source, pid: u32) -> Result<Scope, String> {
    moved(source, pid)?;
    let path = cgroup_of(pid)?;
    let dir = open_dir(&Path::new(ROOT).join(path.trim_start_matches('/')))?;
    let level = run_level(&path);
    Ok(Scope { dir, path, level })
}

/// How long a keeper's move into its scope may take, the call and the move (M11 on 7.3: the call ≈ 22 ms, the move
/// ≈ 35 ms from the call's start).
const PLACED_WITHIN: Duration = Duration::from_secs(2);

/// The scopes this tarae asked for, counted: `tarae-keeper-<tarae's pid>-<n>.scope`, never one name twice.
static SCOPES: AtomicU32 = AtomicU32::new(0);

/// Keeper `pid` moved into a cgroup of its own from `source`.
fn moved(source: &Source, pid: u32) -> Result<(), String> {
    match source {
        // (the user's manager moves it: tarae can't — the common ancestor of its cgroup and the keeper's new one is
        // root's `user-N.slice`; the keeper's pid is tarae's number, the manager's too, both in the machine's PID
        // namespace)
        Source::UserBus(bus) => {
            let unit = format!(
                "tarae-keeper-{}-{}.scope",
                std::process::id(),
                SCOPES.fetch_add(1, Ordering::Relaxed)
            );
            let deadline = Instant::now() + PLACED_WITHIN;
            super::dbus::start_scope(bus, &unit, pid, deadline)
                .map_err(|e| format!("the user manager didn't make a scope: {e}"))?;
            let in_it = format!("/{unit}");
            while !cgroup_of(pid)?.ends_with(&in_it) {
                if Instant::now() > deadline {
                    return Err(format!(
                        "the user manager didn't move the keeper within {} s",
                        PLACED_WITHIN.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        }
        #[cfg(test)]
        Source::Given(folder) => {
            let own = folder.join(format!("keeper-{pid}"));
            std::fs::create_dir(&own).map_err(|e| format!("{}: {e}", own.display()))?;
            std::fs::write(own.join("cgroup.procs"), pid.to_string())
                .map_err(|e| format!("moving the keeper into {}: {e}", own.display()))
        }
    }
}

/// The cgroup `pid` is in, as tarae's cgroup namespace names it (`/proc/<pid>/cgroup`'s v2 line).
pub fn cgroup_of(pid: u32) -> Result<String, String> {
    let said =
        std::fs::read_to_string(format!("/proc/{pid}/cgroup")).map_err(|e| format!("its cgroup: {e}"))?;
    said.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
        .ok_or_else(|| "its cgroup: no cgroup v2 line".into())
}

/// The level of a run's own cgroup — a folder right below the scope at `scope` (as `cgroup_of` names it) — in a
/// `socket cgroupv2` rule: its depth below the root, where tarae's cgroup namespace is the machine's (`conditions`).
pub fn run_level(scope: &str) -> u32 {
    scope.split('/').filter(|c| !c.is_empty()).count() as u32 + 1
}

fn open_dir(path: &Path) -> Result<OwnedFd, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a NUL-terminated path; the descriptor is ours if made.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!("{}: {}", path.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: just opened, ours.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What's left of a keeper's scope once it's gone: the test stand-in's folder (and run folders a killed keeper
/// couldn't remove), removed — systemd collects its own scopes.
pub fn let_go(source: &Source, path: &str) {
    #[cfg(test)]
    if let Source::Given(_) = source {
        let own = Path::new(ROOT).join(path.trim_start_matches('/'));
        for e in std::fs::read_dir(&own).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = std::fs::remove_dir(e.path());
            }
        }
        let _ = std::fs::remove_dir(own);
    }
    let _ = (source, path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_with(f: impl FnOnce(&mut Facts)) -> Facts {
        let mut facts = Facts {
            cgroup2: true,
            ns: Some(INIT_NS.into()),
            pid_ns: Some(INIT_PID_NS.into()),
            clone3: true,
            source: Ok(Source::UserBus("/run/user/1001/bus".into())),
        };
        f(&mut facts);
        facts
    }

    /// Runs get a cgroup of their own only where all of it holds, and the tier's line says the first thing missing:
    /// a v2 hierarchy, tarae in the machine's cgroup namespace (else the rules' level means one thing on 6.1 and 7.3
    /// and another on Ubuntu's 6.8 — spike6 Q2 — and can't be told from inside), `clone3` (else only a slower
    /// `cgroup.procs` write per run), and somewhere a keeper's scope comes from.
    #[test]
    fn the_conditions_say_why() {
        assert_eq!(conditions(&facts_with(|_| {})), Ok(Source::UserBus("/run/user/1001/bus".into())));
        let why = |f: Facts| conditions(&f).unwrap_err();
        assert!(why(facts_with(|f| f.cgroup2 = false)).contains("isn't a cgroup v2 hierarchy"));
        // a container's own namespace, and one whose name couldn't be read
        for ns in [Some("cgroup:[4026532561]".to_string()), None] {
            let e = why(facts_with(|f| f.ns = ns.clone()));
            assert!(e.contains("a cgroup namespace of its own") && e.contains("level"), "{e}");
        }
        assert!(why(facts_with(|f| f.clone3 = false)).contains("clone3"));
        // The review's sixth finding: the user's manager reads the keeper's number in the machine's PID namespace —
        // tarae in one of its own (or one whose name couldn't be read) would name another process: none asked. The
        // tests' folder is placed by tarae in its own numbering: no PID namespace needed there.
        for pid_ns in [Some("pid:[4026532198]".to_string()), None] {
            let e = why(facts_with(|f| f.pid_ns = pid_ns.clone()));
            assert!(e.contains("a PID namespace of its own") && e.contains("another process"), "{e}");
            let given = facts_with(|f| {
                f.pid_ns = pid_ns.clone();
                f.source = Ok(Source::Given("/sys/fs/cgroup/tests".into()));
            });
            assert_eq!(conditions(&given), Ok(Source::Given("/sys/fs/cgroup/tests".into())));
        }
        let none = "no systemd user manager to delegate a cgroup (no user bus)";
        assert_eq!(why(facts_with(|f| f.source = Err(none.into()))), none);
        // (the first missing is said: the hierarchy before the rest)
        let all = facts_with(|f| {
            f.cgroup2 = false;
            f.ns = None;
            f.clone3 = false;
        });
        assert!(why(all).contains("cgroup v2"));
    }

    /// The user bus is where `DBUS_SESSION_BUS_ADDRESS` says — its first `unix:path=`, escapes undone — else
    /// `$XDG_RUNTIME_DIR/bus` where that's there; neither (`su`, `sudo -u`, a container, no systemd): none, said.
    #[test]
    fn the_user_bus_is_found_where_systemd_looks() {
        let there = |p: &Path| p == Path::new("/run/user/1001/bus");
        let rt = Some(Path::new("/run/user/1001"));
        assert_eq!(
            user_bus(Some("unix:path=/run/user/1001/bus"), None, there),
            Ok("/run/user/1001/bus".into())
        );
        assert_eq!(
            user_bus(Some("tcp:host=x,port=1;unix:guid=ab,path=/tmp/b%20us"), None, there),
            Ok("/tmp/b us".into()),
            "the first unix:path, its escape undone"
        );
        assert_eq!(user_bus(Some("unix:abstract=/tmp/dbus-x"), rt, there), Ok("/run/user/1001/bus".into()));
        assert_eq!(user_bus(None, rt, there), Ok("/run/user/1001/bus".into()));
        assert!(user_bus(None, Some(Path::new("/run/user/7")), there).unwrap_err().contains("no user bus"));
        assert!(user_bus(None, None, there).is_err());
        assert!(user_bus(Some("unix:path=/tmp/%zz"), None, there).is_err(), "an escape that isn't one");
    }

    /// A run's own cgroup is one folder below its keeper's scope: its level is the scope's depth below the root and
    /// one — counted from `/` as tarae, in the machine's namespace, reads the scope's path.
    #[test]
    fn a_runs_level_is_its_depth_below_the_root() {
        let scope = "/user.slice/user-1001.slice/user@1001.service/app.slice/tarae-keeper-41-1.scope";
        assert_eq!(run_level(scope), 6);
        assert_eq!(run_level("/system.slice/docker-ab.scope/deleg/keeper-7"), 5);
        assert_eq!(run_level("/"), 1);
    }
}
