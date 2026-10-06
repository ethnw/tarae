//! Landlock (Linux ≥ 5.13): a thread gives up file-system rights for itself and every process it starts —
//! what Seatbelt is to Claude's runs on macOS (`sandbox.rs`). The shape of the kernel's own example
//! (`Documentation/userspace-api/landlock.rst`): ask the kernel for its ABI version, make a ruleset handling
//! the file rights that version has, grant some of them beneath paths (`Ruleset::allow`), and apply it to the
//! calling thread (`Ruleset::apply`) — from then on whatever the ruleset handles is refused unless a rule
//! grants it. Only the calling thread: threads already running stay as they were; threads and processes it
//! starts afterwards inherit it, and nothing takes it off again — so a command is started confined from a
//! short-lived thread of its own (`confined`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
#[cfg(test)]
use std::process::{Child, Command};

/// File-system rights, as `<linux/landlock.h>` numbers them.
pub const EXECUTE: u64 = 1 << 0;
pub const WRITE_FILE: u64 = 1 << 1;
pub const READ_FILE: u64 = 1 << 2;
pub const READ_DIR: u64 = 1 << 3;
pub const REMOVE_DIR: u64 = 1 << 4;
pub const REMOVE_FILE: u64 = 1 << 5;
pub const MAKE_CHAR: u64 = 1 << 6;
pub const MAKE_DIR: u64 = 1 << 7;
pub const MAKE_REG: u64 = 1 << 8;
pub const MAKE_SOCK: u64 = 1 << 9;
pub const MAKE_FIFO: u64 = 1 << 10;
pub const MAKE_BLOCK: u64 = 1 << 11;
pub const MAKE_SYM: u64 = 1 << 12;
/// Linking or renaming a file into another folder (ABI 2; before it, never across folders).
pub const REFER: u64 = 1 << 13;
/// Truncating a file (ABI 3; before it, always).
pub const TRUNCATE: u64 = 1 << 14;
/// `ioctl` on a device file (ABI 5; before it, always).
pub const IOCTL_DEV: u64 = 1 << 15;
/// Connecting or sending to a unix socket by its path (ABI 9; before it, always — tarae's supervisor decides
/// each one whatever the ABI, `sandbox/supervisor.rs`). Left out of `FILE` until a kernel that has it shows a rule on the socket file
/// itself takes it: the kernel refuses a file rule with a right it doesn't take there.
pub const RESOLVE_UNIX: u64 = 1 << 16;

/// Every right above.
pub const ALL: u64 = EXECUTE
    | WRITE_FILE
    | READ_FILE
    | READ_DIR
    | REMOVE_DIR
    | REMOVE_FILE
    | MAKE_CHAR
    | MAKE_DIR
    | MAKE_REG
    | MAKE_SOCK
    | MAKE_FIFO
    | MAKE_BLOCK
    | MAKE_SYM
    | REFER
    | TRUNCATE
    | IOCTL_DEV
    | RESOLVE_UNIX;
/// The rights a rule on a single file can hold — the rest are about a folder's entries.
pub const FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

/// Scopes (ABI 6): what a confined process may reach of processes outside its domain — tarae's, the user's
/// others. Connecting to an abstract unix socket made outside it (tarae's supervisor refuses abstract names
/// anyway), and sending a signal outside it.
pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
pub const SCOPE_SIGNAL: u64 = 1 << 1;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;
/// `landlock_restrict_self`'s flag (ABI 7): log the denials of what the confined thread runs — by default a
/// domain logs only the program that made it, and that's tarae's thread, not the command.
const RESTRICT_SELF_LOG_NEW_EXEC_ON: u32 = 1 << 1;

/// `struct landlock_ruleset_attr`: file rights, network rights (none handled here — `seccomp.rs` keeps runs
/// off sockets), scopes. A kernel older than a field takes the struct while the fields it lacks are 0.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// `struct landlock_path_beneath_attr` (packed in the header too).
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI version this kernel speaks (1 for 5.13's …). Err when it has none: `ENOSYS` (not built
/// in) or `EOPNOTSUPP` (built in, not enabled at boot — `lsm=` leaves it out).
pub fn abi() -> io::Result<u32> {
    // SAFETY: no attribute (NULL, size 0) with the version flag only asks; nothing is written.
    let n = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as u32)
}

/// The rights a ruleset made for ABI `abi` (2 or later — tarae doesn't use 1: `sandbox::linux::availability`)
/// handles: `ALL` but those that version doesn't know (it'd refuse the ruleset).
pub fn handled(abi: u32) -> u64 {
    let mut fs = ALL;
    if abi < 3 {
        fs &= !TRUNCATE;
    }
    if abi < 5 {
        fs &= !IOCTL_DEV;
    }
    if abi < 9 {
        fs &= !RESOLVE_UNIX;
    }
    fs
}

/// The scopes a ruleset made for ABI `abi` has: both from 6, none before.
pub fn scopes(abi: u32) -> u64 {
    if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 }
}

/// A ruleset being made: what it handles is refused once applied, but what a rule grants.
#[derive(Debug)]
pub struct Ruleset {
    fd: OwnedFd,
    handled: u64,
    abi: u32,
    /// Tests: every rule added — (path, the rights granted) — in order.
    #[cfg(test)]
    pub added: Vec<(std::path::PathBuf, u64)>,
}

impl Ruleset {
    /// A ruleset handling every file right ABI `abi` has (`handled`), and its scopes (`scopes`).
    pub fn new(abi: u32) -> io::Result<Ruleset> {
        let attr =
            RulesetAttr { handled_access_fs: handled(abi), handled_access_net: 0, scoped: scopes(abi) };
        // SAFETY: the attribute is a live struct of the size given; the kernel only reads it.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new descriptor of our own, closed with the ruleset.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        Ok(Ruleset {
            fd,
            handled: attr.handled_access_fs,
            abi,
            #[cfg(test)]
            added: Vec::new(),
        })
    }

    /// Grants `access` beneath `path` (a real path): in a folder and everything under it, or on one file — of
    /// `access`, the rights a file can hold (`FILE`). Rights the ruleset doesn't handle are left out (they stay
    /// allowed anyway); a rule left with none isn't added. `path` must exist. No link is followed anywhere on
    /// its way (`open_unlinked`) — the rule is left out (false): a link planted where a run writes, as the
    /// path itself or a folder above it, would otherwise carry the grant to where it points.
    pub fn allow(&mut self, path: &Path, access: u64) -> io::Result<bool> {
        let at = match open_unlinked(path, libc::O_PATH) {
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(false),
            at => std::fs::File::from(at?),
        };
        let kind = at.metadata()?.file_type();
        let mut access = access & self.handled;
        if !kind.is_dir() {
            access &= FILE;
        }
        if access == 0 {
            return Ok(false);
        }
        let rule = PathBeneathAttr { allowed_access: access, parent_fd: at.as_raw_fd() };
        // SAFETY: both descriptors are open for the call; the kernel only reads the rule.
        let r = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(test)]
        self.added.push((path.to_path_buf(), access));
        Ok(true)
    }

    /// Confines the calling thread — and what it starts from now on — to the ruleset, for good (the ruleset
    /// stays, for the next thread). Sets `no_new_privs` first (the kernel asks it of an unprivileged caller): a
    /// setuid program it starts gains nothing. From ABI 7 the denials of what the thread runs are logged
    /// (`RESTRICT_SELF_LOG_NEW_EXEC_ON`).
    pub fn apply(&self) -> io::Result<()> {
        let flags = if self.abi >= 7 { RESTRICT_SELF_LOG_NEW_EXEC_ON } else { 0 };
        // SAFETY: plain prctl/syscall on this thread with integer arguments.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, self.fd.as_raw_fd(), flags) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// `path` (absolute) opened with `flags` (and close-on-exec) through no link at all — any component one, the
/// last too: `ELOOP` (`openat2`'s `RESOLVE_NO_SYMLINKS`, Linux 5.6; `/proc`'s magic links neither).
pub fn open_unlinked(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: an all-integer struct, zeroed then set.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
    // SAFETY: the path and the struct, live for the call; a descriptor it makes is ours.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            c.as_ptr(),
            &raw const how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// A name of its own for a thread about to confine itself (`confined`, the audit probe): Landlock's audit
/// record of a domain names tarae's process (`pid=` is the process's id, not the thread's — the user's VM)
/// and the thread's name as it was then (`comm=`), so the name is what tells one run's records from another's.
/// Within the kernel's 15 bytes.
pub fn thread_name() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("tl-{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// The calling thread's name as the kernel has it (`comm`) — what Landlock's audit records name.
pub fn comm() -> io::Result<String> {
    let mut buf = [0u8; 16];
    // SAFETY: PR_GET_NAME writes at most 16 bytes, NUL included, into the buffer given.
    if unsafe { libc::prctl(libc::PR_GET_NAME, buf.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// `start` on a short-lived thread of its own confined to `rules` — `also` applied after them (the seccomp
/// filter), what it gives handed to `start` — leaving the caller as it was: the confinement ends with the
/// thread, which has a name of its own (`thread_name`). Waits for `start` only.
///
/// Why a process `start` starts is confined: Landlock's domain (and `no_new_privs`) belong to a thread's
/// credentials, and a new process takes its credentials from the thread that starts it. glibc's
/// `posix_spawn` (std's `Command::spawn`, tarae's `spawn::setsid`) clones the calling thread (`CLONE_VM |
/// CLONE_VFORK`), and the `fork` std falls back to (`pre_exec`, a uid …) copies only the calling thread too.
/// So the child, and all it starts, has this thread's ruleset; a process started from any other thread at
/// the same time has that thread's own. What the command opens to start — its program, libraries,
/// `/dev/null` for its stdin — is opened under the ruleset already: it must grant them.
pub fn confined<A, T: Send>(
    rules: &Ruleset,
    also: impl FnOnce() -> io::Result<A> + Send,
    start: impl FnOnce(A) -> io::Result<T> + Send,
) -> io::Result<T> {
    std::thread::scope(|s| {
        let confined = std::thread::Builder::new().name(thread_name()).spawn_scoped(s, || {
            rules.apply()?;
            start(also()?)
        })?;
        confined.join().unwrap_or_else(|_| Err(io::Error::other("the confined thread panicked")))
    })
}

/// Starts `cmd` confined to `rules` with std's spawn (`confined`) — the tests' way; tarae's runs go through
/// `sandbox::spawn`.
#[cfg(test)]
pub fn spawn(rules: &Ruleset, cmd: &mut Command) -> io::Result<Child> {
    confined(rules, || Ok(()), |()| cmd.spawn())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::sync::mpsc;

    /// The kernel's ABI, or None after saying the test skips itself (a kernel without Landlock, or with ABI 1,
    /// which tarae doesn't use — any other error is a failure, not a skip).
    fn kernel_abi(test: &str) -> Option<u32> {
        match abi() {
            Ok(n) if n >= 2 => Some(n),
            Ok(n) => {
                let line = format!(
                    "{}landlock::tests::{test} — this kernel's Landlock is ABI {n}, which tarae doesn't use\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                None
            }
            Err(e) if ![libc::ENOSYS, libc::EOPNOTSUPP].contains(&e.raw_os_error().unwrap_or(0)) => {
                panic!("asking the ABI: {e}")
            }
            Err(e) => {
                let line = format!(
                    "{}landlock::tests::{test} — this kernel has no Landlock ({e})\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                None
            }
        }
    }

    /// A fresh temp folder of the test's own with `granted/` and `sibling/` in it.
    fn folders(test: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-landlock-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (granted, sibling) = (dir.join("granted"), dir.join("sibling"));
        std::fs::create_dir_all(&granted).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        (dir, granted, sibling)
    }

    /// Confines the calling thread to `path` with `access`.
    fn confine(abi: u32, path: &Path, access: u64) -> io::Result<()> {
        let mut rules = Ruleset::new(abi)?;
        rules.allow(path, access)?;
        rules.apply()
    }

    #[test]
    fn a_confined_thread_writes_only_where_granted() {
        let Some(abi) = kernel_abi("a_confined_thread_writes_only_where_granted") else { return };
        let (dir, granted, sibling) = folders("confined");
        let (a, b) = (granted.clone(), sibling.clone());
        let (inside, outside) = std::thread::spawn(move || {
            confine(abi, &a, ALL).expect("the ruleset applies");
            (std::fs::write(a.join("marker"), "in"), std::fs::write(b.join("marker"), "out"))
        })
        .join()
        .unwrap();
        inside.expect("a marker in the granted folder");
        assert_eq!(outside.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied), "the sibling");
        assert!(granted.join("marker").exists() && !sibling.join("marker").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// While one thread is confined, another that applied nothing writes as before — and the confined one,
    /// asked right after, still can't.
    #[test]
    fn a_thread_that_applied_nothing_is_unaffected() {
        let Some(abi) = kernel_abi("a_thread_that_applied_nothing_is_unaffected") else { return };
        let (dir, granted, sibling) = folders("unaffected");
        let (ready_tx, ready) = mpsc::channel();
        let (go, go_rx) = mpsc::channel::<()>();
        let (a, b) = (granted.clone(), sibling.clone());
        let confined = std::thread::spawn(move || {
            ready_tx.send(confine(abi, &a, ALL)).unwrap();
            go_rx.recv().unwrap();
            std::fs::write(b.join("confined"), "out")
        });
        ready.recv().unwrap().expect("the ruleset applies");
        std::fs::write(sibling.join("free"), "x").expect("a thread that applied nothing writes there");
        go.send(()).unwrap();
        let after = confined.join().unwrap();
        assert_eq!(after.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied), "still confined");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A rule on one file grants that file (the folder's rights in `ALL` left out, else the kernel refuses
    /// the rule) — not its neighbours, nor making new files beside it.
    #[test]
    fn a_file_rule_grants_that_file_only() {
        let Some(abi) = kernel_abi("a_file_rule_grants_that_file_only") else { return };
        let (dir, _, sibling) = folders("file");
        let (one, other) = (sibling.join("one"), sibling.join("other"));
        std::fs::write(&one, "").unwrap();
        std::fs::write(&other, "").unwrap();
        let (o, n, s) = (one.clone(), other.clone(), sibling.clone());
        let (that, neighbour, new) = std::thread::spawn(move || {
            confine(abi, &o, ALL).expect("the ruleset applies");
            (std::fs::write(&o, "in"), std::fs::write(&n, "out"), std::fs::write(s.join("new"), "out"))
        })
        .join()
        .unwrap();
        that.expect("the granted file");
        assert_eq!(neighbour.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied));
        assert_eq!(new.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied));
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "in");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each confining thread has a name of its own as the kernel has it (`comm` — what Landlock's audit record
    /// of a domain names, beside tarae's process id, not the thread's): two runs' records are told apart by
    /// it. Within the kernel's 15 bytes.
    #[test]
    fn each_confining_thread_has_a_name_of_its_own() {
        let Some(abi) = kernel_abi("each_confining_thread_has_a_name_of_its_own") else { return };
        let rules = Ruleset::new(abi).unwrap();
        // (asked, not read from `/proc`: the ruleset grants no file)
        let name = || confined(&rules, || Ok(()), |()| comm()).unwrap();
        let (a, b) = (name(), name());
        assert_ne!(a, b);
        assert!(a.starts_with("tl-") && b.starts_with("tl-"), "{a} {b}");
    }

    /// The kernel says its version, and a ruleset leaves out the rights older versions don't know.
    #[test]
    fn the_abi_is_read_and_sets_the_rights() {
        let Some(abi) = kernel_abi("the_abi_is_read_and_sets_the_rights") else { return };
        assert!(abi >= 1, "ABI {abi}");
        let folder = READ_DIR | REMOVE_DIR | REMOVE_FILE | MAKE_DIR | MAKE_REG | MAKE_SYM | REFER;
        let specials = MAKE_CHAR | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | RESOLVE_UNIX;
        assert_eq!(FILE | folder | specials, ALL, "every right is in ALL");
        assert_eq!(FILE & (folder | specials), 0);
        assert_eq!(handled(2), ALL & !(TRUNCATE | IOCTL_DEV | RESOLVE_UNIX));
        assert_eq!((handled(3), handled(4)), (handled(2) | TRUNCATE, handled(2) | TRUNCATE));
        assert_eq!((handled(5), handled(8)), (handled(3) | IOCTL_DEV, handled(3) | IOCTL_DEV));
        assert_eq!((handled(9), handled(99)), (ALL, ALL));
        assert_eq!((scopes(5), scopes(6)), (0, SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL));
        Ruleset::new(abi).expect("this kernel takes a ruleset of its own version's rights");
    }

    /// A symlink carries no grant: a rule asked for a link (to a sibling folder, as a run could plant one) is
    /// left out, and the folder it points to stays refused — the ruleset holds for a second thread too.
    #[test]
    fn a_link_carries_no_grant_and_a_ruleset_serves_twice() {
        let Some(abi) = kernel_abi("a_link_carries_no_grant_and_a_ruleset_serves_twice") else { return };
        let (dir, granted, sibling) = folders("link");
        let link = granted.join("to-sibling");
        std::os::unix::fs::symlink(&sibling, &link).unwrap();
        let mut rules = Ruleset::new(abi).unwrap();
        assert!(!rules.allow(&link, ALL).unwrap(), "a link's rule is left out");
        assert!(rules.allow(&granted, ALL).unwrap());
        let rules = std::sync::Arc::new(rules);
        for i in 0..2 {
            let (r, s, g) = (rules.clone(), sibling.clone(), granted.clone());
            let (through, inside) = std::thread::spawn(move || {
                r.apply().unwrap();
                (std::fs::write(s.join(format!("m{i}")), "x"), std::fs::write(g.join(format!("m{i}")), "x"))
            })
            .join()
            .unwrap();
            assert_eq!(through.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied), "thread {i}");
            inside.expect("the granted folder");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A link anywhere on a granted path's way — a parent a run swapped for one — carries no grant either: the
    /// rule is left out, and the folder the link points to stays refused.
    #[test]
    fn a_link_on_the_way_carries_no_grant() {
        let Some(abi) = kernel_abi("a_link_on_the_way_carries_no_grant") else { return };
        let (dir, granted, sibling) = folders("parent-link");
        std::fs::create_dir_all(sibling.join("inner")).unwrap();
        let via = granted.join("via");
        std::os::unix::fs::symlink(&sibling, &via).unwrap();
        let mut rules = Ruleset::new(abi).unwrap();
        assert!(!rules.allow(&via.join("inner"), ALL).unwrap(), "the rule is left out");
        let inner = sibling.join("inner");
        let wrote = std::thread::spawn(move || {
            rules.apply().unwrap();
            std::fs::write(inner.join("marker"), "x")
        })
        .join()
        .unwrap();
        assert_eq!(wrote.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied));
        assert!(!sibling.join("inner/marker").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// From ABI 6 a confined thread's processes can't signal one outside their domain — a stand-in process
    /// of the test's own, which the test can signal itself.
    #[test]
    fn a_confined_run_signals_nothing_outside_it() {
        let Some(abi) = kernel_abi("a_confined_run_signals_nothing_outside_it") else { return };
        if abi < 6 {
            let line = format!(
                "{}landlock::tests::a_confined_run_signals_nothing_outside_it — this kernel's Landlock is ABI \
                 {abi}: signal scopes came with 6\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        let mut target = Command::new("sleep").arg("30").spawn().unwrap();
        let (dir, granted, _) = folders("signal");
        let mut kill = Command::new("kill");
        kill.args(["-0", &target.id().to_string()]).stderr(std::process::Stdio::piped());
        let out = spawn(&run_rules(abi, &granted), &mut kill).unwrap().wait_with_output().unwrap();
        assert!(!out.status.success(), "refused: {}", String::from_utf8_lossy(&out.stderr));
        // SAFETY: signal 0 only asks; the test's own child.
        assert_eq!(unsafe { libc::kill(target.id() as i32, 0) }, 0, "the test itself still can");
        target.kill().ok();
        target.wait().ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reading and running everywhere (the shell, its libraries), everything in `granted`.
    fn run_rules(abi: u32, granted: &Path) -> Ruleset {
        let mut rules = Ruleset::new(abi).unwrap();
        rules.allow(Path::new("/"), EXECUTE | READ_FILE | READ_DIR).unwrap();
        rules.allow(granted, ALL).unwrap();
        rules
    }

    /// `sh -c script sh marker`, its stdin and stderr piped (no `/dev/null` to open).
    fn sh(script: &str, marker: &Path) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script, "sh"]).arg(marker);
        c.stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        c
    }

    /// A process started through `spawn` writes where the ruleset grants and nowhere else; the thread that
    /// called `spawn` still writes anywhere.
    #[test]
    fn a_process_started_confined_writes_only_where_granted() {
        let Some(abi) = kernel_abi("a_process_started_confined_writes_only_where_granted") else { return };
        let (dir, granted, sibling) = folders("spawn");
        let write = "echo x > \"$1\"";
        let inside = spawn(&run_rules(abi, &granted), &mut sh(write, &granted.join("marker"))).unwrap();
        let out = inside.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let outside = spawn(&run_rules(abi, &granted), &mut sh(write, &sibling.join("marker"))).unwrap();
        let out = outside.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("Permission denied"), "{:?}: {err}", out.status);
        assert!(granted.join("marker").exists() && !sibling.join("marker").exists());
        std::fs::write(sibling.join("caller"), "x").expect("spawn leaves its caller as it was");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Both ways at once: while `spawn`'s thread is confined and still starting its process, a process the
    /// test's own thread starts writes outside the grant — the confined one can't. The confined command is
    /// held in `pre_exec` (after its fork, before its exec: `spawn`'s thread waits in `Command::spawn`
    /// meanwhile); `pre_exec` makes std fork, where the test above went through `posix_spawn`.
    #[test]
    fn a_process_started_from_another_thread_meanwhile_is_free() {
        let Some(abi) = kernel_abi("a_process_started_from_another_thread_meanwhile_is_free") else { return };
        let (dir, granted, sibling) = folders("meanwhile");
        let (held, free) = (sibling.join("held"), sibling.join("free"));
        let ((mut ready, ready_w), (go_r, mut go)) = (io::pipe().unwrap(), io::pipe().unwrap());
        let (ready_fd, go_fd, go_w_fd) = (ready_w.as_raw_fd(), go_r.as_raw_fd(), go.as_raw_fd());
        let mut cmd = sh("echo x > \"$1\"", &held);
        // SAFETY: close/write/read only (async-signal-safe), on descriptors the forked child has.
        unsafe {
            cmd.pre_exec(move || {
                libc::close(go_w_fd); // (the child's copy: the test's end gone, the read below ends)
                libc::write(ready_fd, b"r".as_ptr().cast(), 1);
                let mut b = 0u8;
                libc::read(go_fd, (&raw mut b).cast(), 1);
                Ok(())
            });
        }
        let starting = std::thread::spawn(move || {
            let child = spawn(&run_rules(abi, &granted), &mut cmd);
            drop(ready_w); // (a spawn that failed before its fork: the read below ends)
            child
        });
        let mut b = [0u8];
        io::Read::read_exact(&mut ready, &mut b).expect("the confined process is held before its exec");
        let out = sh("echo x > \"$1\"", &free).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(free.exists());
        io::Write::write_all(&mut go, b"g").unwrap();
        let out = starting.join().unwrap().unwrap().wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("Permission denied"), "{:?}: {err}", out.status);
        assert!(!held.exists());
        drop(go_r);
        std::fs::remove_dir_all(&dir).ok();
    }
}
