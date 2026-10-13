//! tarae's limit on open descriptors (`RLIMIT_NOFILE`), raised at start, and the limit tarae started with given
//! back to what it starts.
//!
//! The soft limit is often 1024 (systemd's default) or 256 (macOS's launchd), and a session with a held served
//! run in tier C holds about 30 of tarae's descriptors, its servers, files and terminal more: the 20th session's
//! run couldn't be made (`docs/design/sandbox-keeper.md` §Many sessions). So `raise` lifts the soft limit to the
//! hard one in `main` once it knows it's the editor — a subcommand that ends at once (`state`, `--version`) raises
//! and pins nothing — before any thread runs; on macOS to `kern.maxfilesperproc` at most (`setrlimit` refuses
//! more); never lower, and never an error when it can't.
//!
//! What tarae starts that runs arbitrary programs gets the limit it started with back, as Go's runtime does since
//! 1.19: a program using `select()` meets a descriptor numbered 1024 or more where under the old limit it got
//! `EMFILE` — glibc's `FD_SET` writes past the set without `_FORTIFY_SOURCE`; macOS's skips it and `select`
//! refuses it, but a program built with an SDK older than macOS 11 writes past it too (`tests`). A child made by
//! a fork (`pre_exec`: the terminal's shell, debug adapters, plugins' programs) or born of the keeper (whose runs
//! inherit its limit) sets it itself (`restore`). `posix_spawn` has no attribute for it, so a child it makes that
//! runs the project's code or the user's commands — tasks, tests, Claude's runs, `:sh`, shell pipes, language
//! servers, a debug build — is started through tarae's own binary, which sets it and execs the program (`shim`,
//! `--nofile N <path> <argv0> args…`, first thing in `main`; the program sees the `argv[0]` it was given): +0.3
//! ms per start on Linux, ~2 ms on macOS, for children started rarely. On macOS that binary is a hard link made
//! at start (`pin`) — a rebuild can't change which shim runs — and it's looked at before each start (`same_exe`). Left raised: git
//! (started often; it sizes its open packs by the limit), claude (measured: node and Bun raise their own at
//! start), and tarae's own short queries of fixed tools (a pack's `ask`, the container engine's CLI, the
//! clipboard, the log readers) — they open a handful of descriptors and end.

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The shim's flag: `tarae --nofile N <absolute path> <argv0> args…`.
pub const FLAG: &str = "--nofile";

/// The soft limit tarae started with, where it raised it (else `UNCHANGED`) — written before any thread runs,
/// read in a fork's child (an atomic load: no lock, no allocation).
static ORIGINAL: AtomicU64 = AtomicU64::new(UNCHANGED);
/// The hard limit, as it was (unchanged by `raise`).
static HARD: AtomicU64 = AtomicU64::new(0);
const UNCHANGED: u64 = u64::MAX;

/// tarae's binary for the shim: on Linux `/proc/self/exe`, which a posix_spawn child resolves before its exec —
/// tarae's even once a build replaced the file; on macOS a hard link to it made at start (`pin`).
static EXE: OnceLock<Option<CString>> = OnceLock::new();

/// The soft limit to raise to from `cur`, with hard limit `max` and the system's cap — None where it's no higher
/// (never lowered), or where it would be unlimited (`setrlimit` refuses `RLIM_INFINITY` for descriptors).
pub fn target(cur: u64, max: u64, cap: Option<u64>) -> Option<u64> {
    let to = cap.map_or(max, |c| max.min(c));
    (to != libc::RLIM_INFINITY && to > cur).then_some(to)
}

/// The most descriptors this system lets a process have: macOS's `kern.maxfilesperproc`, Linux's
/// `fs.nr_open` (the hard limit is no higher, but read: a hard limit of `RLIM_INFINITY` isn't a number to set).
fn cap() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut n: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: an int for the call to fill, its size given.
        let r = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut n).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && n > 0).then_some(n as u64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/sys/fs/nr_open").ok()?.trim().parse().ok()
    }
}

fn limit() -> libc::rlimit {
    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: reads this process's limit into a struct of ours.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) };
    l
}

/// Raises the soft limit (`target`) — the editor's alone (`main`, past the arguments), before any thread runs;
/// on macOS it pins tarae's binary for the shim too (`pin`). What to say in the log
/// once it's up (from → to, or why not); None when there was nothing to raise.
pub fn raise() -> Option<String> {
    let l = limit();
    let to = target(l.rlim_cur, l.rlim_max, cap())?;
    let new = libc::rlimit { rlim_cur: to, rlim_max: l.rlim_max };
    // SAFETY: a limit of ours, no higher than the hard one.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } != 0 {
        let e = std::io::Error::last_os_error();
        return Some(format!(
            "open descriptors: the soft limit stays {} (couldn't raise it to {to}: {e})",
            l.rlim_cur
        ));
    }
    HARD.store(l.rlim_max, Ordering::SeqCst);
    ORIGINAL.store(l.rlim_cur, Ordering::SeqCst);
    let unpinned = remember_exe();
    let spawned = match unpinned {
        None => format!("what tarae starts gets {} back", l.rlim_cur),
        Some(why) => format!("what tarae starts gets {} back — {why}", l.rlim_cur),
    };
    Some(format!("open descriptors: the soft limit raised {} → {to}; {spawned}", l.rlim_cur))
}

/// The shim's binary, remembered (`EXE`) — Some(why) where there's none.
#[cfg(target_os = "linux")]
fn remember_exe() -> Option<String> {
    EXE.get_or_init(|| Some(c"/proc/self/exe".to_owned()));
    EXE_ID.get_or_init(|| None);
    None
}

/// Tests: `exe` as the shim's binary, remembered as it is now (`same_exe` checks it).
#[cfg(test)]
fn remember_exe_for_test(exe: &Path) {
    EXE.set(Some(CString::new(exe.as_os_str().as_bytes()).unwrap())).unwrap();
    EXE_ID.set(identity(exe)).unwrap();
}

/// The shim's binary, remembered (`EXE`, `EXE_ID`) — Some(what to say) where it isn't pinned.
#[cfg(not(target_os = "linux"))]
fn remember_exe() -> Option<String> {
    use std::os::unix::ffi::OsStringExt as _;
    let mut said =
        Some("but where tarae's binary is can't be told: what it posix_spawns keeps the raised limit".into());
    EXE.get_or_init(|| {
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok()?;
        let pinned = pins_dir()
            .ok_or_else(|| std::io::Error::other("no state folder"))
            .and_then(|dir| pin(&exe, &dir, &std::process::id().to_string()));
        let (at, id, note) = shim_exe(&exe, pinned);
        EXE_ID.get_or_init(|| id);
        said = note;
        CString::new(at.into_os_string().into_vec()).ok()
    });
    said
}

/// Where the shim's binary is: `pinned` (the pin and its identity), or — no pin (another volume: no hard link
/// across; no state folder) — `exe` itself, its identity checked before each start (`same_exe`), so a rebuild
/// turns the shim off rather than run another build's: then what posix_spawn starts keeps the raised limit. What
/// to say once (None: pinned).
#[cfg(any(not(target_os = "linux"), test))]
fn shim_exe(
    exe: &Path,
    pinned: std::io::Result<(std::path::PathBuf, (u64, u64))>,
) -> (std::path::PathBuf, Option<(u64, u64)>, Option<String>) {
    match pinned {
        Ok((at, id)) => (at, Some(id), None),
        Err(e) => {
            let note = format!(
                "tarae's binary not pinned for the shim ({e}): {} checked before each start instead — after a \
                 rebuild what tarae posix_spawns keeps the raised limit",
                exe.display()
            );
            (exe.to_path_buf(), identity(exe), Some(note))
        }
    }
}

/// Where tarae's binary is pinned: `shims` in tarae's state folder — no sandboxed run reads or writes there
/// (`sandbox::kept_reads`; outside every `sandbox::write_places`, `tests`), unlike tarae's cache, whose run homes
/// runs write. So between the check before a start (`same_exe`) and the shim's exec, nothing inside the sandbox
/// can swap the pin.
#[cfg(any(not(target_os = "linux"), test))]
fn pins_dir() -> Option<std::path::PathBuf> {
    crate::config::state_dir().map(|d| d.join("shims"))
}

/// The lock this tarae holds on its pin (`pin`) while it runs — a pin whose lock is free is a gone tarae's
/// (`prune_pins_in`), as recovery tells one. Close-on-exec: a `:restart`'s exec lets it go and takes its own.
#[cfg(any(not(target_os = "linux"), test))]
static PIN_LOCK: std::sync::Mutex<Option<std::os::fd::OwnedFd>> = std::sync::Mutex::new(None);

/// `dir` opened without following a link at its last step, made (0700) where it isn't — its parents as they come
/// (tarae's own folders) — and refused unless it's a folder of this user's that no one else may enter.
#[cfg(any(not(target_os = "linux"), test))]
fn pins_folder(dir: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let (parent, name) = (dir.parent().unwrap_or(Path::new("/")), dir.file_name().unwrap_or_default());
    std::fs::create_dir_all(parent)?;
    let c = |s: &OsStr| {
        CString::new(s.as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (parent_c, name_c) = (c(parent.as_os_str())?, c(name)?);
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: plain calls with NUL-terminated names held across them; what's opened is ours.
    unsafe {
        let p = libc::open(parent_c.as_ptr(), flags);
        if p < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let p = std::os::fd::OwnedFd::from_raw_fd(p);
        libc::mkdirat(p.as_raw_fd(), name_c.as_ptr(), 0o700);
        let d = libc::openat(p.as_raw_fd(), name_c.as_ptr(), flags);
        if d < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let d = std::os::fd::OwnedFd::from_raw_fd(d);
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(d.as_raw_fd(), &mut st) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if st.st_uid != libc::geteuid() || st.st_mode & 0o077 != 0 {
            return Err(std::io::Error::other(format!("{} isn't this user's alone", dir.display())));
        }
        Ok(d)
    }
}

/// tarae's binary pinned for the shim (macOS: `posix_spawn` takes a path, never a descriptor): a hard link to
/// `exe` (real path), `dir/name`, and its identity. A build that replaces that file (cargo writes a new one and
/// renames it over), a link under way, or a `cargo clean` leave this one as it was — the same inode, this very
/// build — so the shim is always this tarae's. Made through no link: `dir` opened so (`pins_folder`), the lock
/// first (`name.lock`, held while tarae runs), then `name.new` linked in it — a link never followed there — and
/// renamed over `name` (a `:restart` keeps its pid). Fails across volumes (no hard link there). (Linux execs
/// `/proc/self/exe`: no pin.)
#[cfg(any(not(target_os = "linux"), test))]
pub fn pin(exe: &Path, dir: &Path, name: &str) -> std::io::Result<(std::path::PathBuf, (u64, u64))> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let d = pins_folder(dir)?;
    let c = |s: String| CString::new(s).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let (lock, new, at) = (c(format!("{name}.lock"))?, c(format!("{name}.new"))?, c(name.to_string())?);
    let exe_c = CString::new(exe.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fail = || Err(std::io::Error::last_os_error());
    // (a lock this process holds already is its own: let go first)
    PIN_LOCK.lock().unwrap_or_else(|e| e.into_inner()).take();
    // SAFETY: plain calls on a folder descriptor of ours and NUL-terminated names held across them.
    unsafe {
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let l = libc::openat(d.as_raw_fd(), lock.as_ptr(), flags, 0o600 as libc::c_uint);
        if l < 0 {
            return fail();
        }
        let l = std::os::fd::OwnedFd::from_raw_fd(l);
        if libc::flock(l.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return fail();
        }
        libc::unlinkat(d.as_raw_fd(), new.as_ptr(), 0);
        if libc::linkat(libc::AT_FDCWD, exe_c.as_ptr(), d.as_raw_fd(), new.as_ptr(), 0) != 0 {
            return fail();
        }
        if libc::renameat(d.as_raw_fd(), new.as_ptr(), d.as_raw_fd(), at.as_ptr()) != 0 {
            return fail();
        }
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstatat(d.as_raw_fd(), at.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) != 0 {
            return fail();
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(std::io::Error::other("the pin isn't a file"));
        }
        *PIN_LOCK.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
        Ok((dir.join(name), (st.st_dev as u64, st.st_ino as u64)))
    }
}

/// This tarae's pin gone (it quits); a `:restart`'s exec makes its own.
pub fn unpin() {
    #[cfg(not(target_os = "linux"))]
    if let Some(dir) = pins_dir() {
        let pid = std::process::id();
        let _ = std::fs::remove_file(dir.join(pid.to_string()));
        let _ = std::fs::remove_file(dir.join(format!("{pid}.lock")));
        PIN_LOCK.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// Pins whose tarae is gone (killed, crashed: `unpin` never ran — each keeps an old build's binary alive) —
/// removed, on a worker at start: those whose lock (`pin`) no tarae holds, with their lock and any `.new`; this
/// tarae's own (`own`) and what isn't a pin are left alone. How many tarae's went.
#[cfg(any(not(target_os = "linux"), test))]
pub fn prune_pins_in(dir: &Path, own: u32) -> usize {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let stem = n.strip_suffix(".lock").or_else(|| n.strip_suffix(".new")).unwrap_or(&n);
            stem.parse().ok()
        })
        .filter(|&p| p != own)
        .collect();
    crate::sorting::sort(&mut pids);
    pids.dedup();
    let mut gone = 0;
    for pid in pids {
        let lock = dir.join(format!("{pid}.lock"));
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock);
        // (no lock at all: a pin without one is no running tarae's — `pin` takes it first)
        // SAFETY: flock on a descriptor we just opened.
        let free = opened
            .as_ref()
            .map_or(true, |f| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0);
        if free {
            for n in [pid.to_string(), format!("{pid}.new"), format!("{pid}.lock")] {
                let _ = std::fs::remove_file(dir.join(n));
            }
            gone += 1;
        }
    }
    gone
}

/// `prune_pins_in` tarae's own folder of pins (macOS; none elsewhere).
pub fn prune_pins() {
    #[cfg(not(target_os = "linux"))]
    if let Some(dir) = pins_dir() {
        prune_pins_in(&dir, std::process::id());
    }
}

/// The soft limit tarae started with, where it raised it.
pub fn original() -> Option<u64> {
    match ORIGINAL.load(Ordering::SeqCst) {
        UNCHANGED => None,
        n => Some(n),
    }
}

/// The limit tarae started with, set on this process — in a fork's child between fork and exec, or in the
/// keeper's raw child: async-signal-safe (atomic loads and one raw call). Nothing where tarae raised nothing.
pub fn restore() {
    let soft = ORIGINAL.load(Ordering::SeqCst);
    if soft == UNCHANGED {
        return;
    }
    let l = libc::rlimit { rlim_cur: soft, rlim_max: HARD.load(Ordering::SeqCst) };
    // SAFETY: a plain call with a struct of ours; lowering the soft limit can't fail but on a bad value.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &l) };
}

/// For an exec of this process itself (`:restart`): the limit tarae started with, set now — the new tarae raises
/// it again from there — and put back by the guard's `undo` if the exec failed.
pub fn for_exec() -> Raised {
    let now = limit();
    restore();
    Raised(now)
}

pub struct Raised(libc::rlimit);

impl Raised {
    pub fn undo(self) {
        // SAFETY: putting back the limit read before, as it was.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
    }
}

/// How a posix_spawn child gets the limit back: started as `exe` with these arguments before the program's own
/// (`--nofile N <program> <argv0>`: the program then sees the `argv[0]` it would without the shim) — None where
/// nothing was raised, `program` isn't an absolute path, or `exe` is no longer the binary remembered (`same_exe`).
pub fn shim(program: &Path, argv0: &OsStr) -> Option<(CString, [CString; 4])> {
    let n = original()?;
    let exe = EXE.get()?.clone()?;
    if !program.is_absolute() || !same_exe(&exe) {
        return None;
    }
    let c = |s: &OsStr| CString::new(s.as_bytes()).ok();
    Some((exe, [c(OsStr::new(FLAG))?, CString::new(n.to_string()).ok()?, c(program.as_os_str())?, c(argv0)?]))
}

/// The shim's binary as it was when remembered: its (device, inode) — None where its path is always the running
/// binary (`/proc/self/exe`) and needs no look.
static EXE_ID: OnceLock<Option<(u64, u64)>> = OnceLock::new();

/// (device, inode) of the file at `p`.
fn identity(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

/// Whether `exe` still holds the binary remembered (one look, at each shimmed start): a pin replaced or removed
/// behind tarae's back — not by tarae, whose pin no build touches (`pin`) — is no shim to trust; then what
/// posix_spawn starts keeps the raised limit, said in the log once.
fn same_exe(exe: &CString) -> bool {
    let Some(Some(was)) = EXE_ID.get() else { return true };
    let now = identity(Path::new(OsStr::from_bytes(exe.as_bytes())));
    if now == Some(*was) {
        return true;
    }
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, Ordering::SeqCst) {
        let said = format!(
            "open descriptors: the shim's binary {} is no longer the one remembered — what tarae posix_spawns \
             keeps the raised limit from now on",
            exe.to_string_lossy()
        );
        crate::log::line("tarae", "warning", &said);
    }
    false
}

/// A `Command` for `program` run in `dir` (its `current_dir`, set here; None: tarae's own), that std starts with
/// posix_spawn — through the shim where tarae raised the limit. `program` is found now as std would find it in the
/// child: a path with a `/` from `dir`, a name on tarae's `PATH` (its relative entries from `dir` too); a caller
/// that gives the child another `PATH` uses `command_on`. One not found, or nothing raised: the plain `Command`
/// (its spawn fails as before).
pub fn command(program: impl AsRef<OsStr>, dir: Option<&Path>) -> std::process::Command {
    command_on(program, dir, std::env::var_os("PATH").as_deref())
}

/// `command`, the program found on `path` (None: the child has no `PATH`).
pub fn command_on(
    program: impl AsRef<OsStr>,
    dir: Option<&Path>,
    path: Option<&OsStr>,
) -> std::process::Command {
    let program = program.as_ref();
    let found = original().and_then(|_| {
        let dir = match dir {
            Some(d) => std::path::absolute(d).ok()?,
            None => std::env::current_dir().ok()?,
        };
        // (one that isn't there: the plain `Command`, whose spawn says so — not the shim's exit 127)
        crate::spawn::resolve(program, Some(&dir), path).ok().filter(|p| crate::spawn::executable(p))
    });
    let mut c = match found.as_deref().and_then(|p| shim(p, program)) {
        Some((exe, args)) => {
            let mut c = std::process::Command::new(OsStr::from_bytes(exe.as_bytes()));
            c.args(args.iter().map(|a| OsStr::from_bytes(a.as_bytes())));
            c
        }
        None => std::process::Command::new(program),
    };
    if let Some(d) = dir {
        c.current_dir(d);
    }
    c
}

/// What the shim was asked: the soft limit, the program's path, its `argv[0]` and its arguments after — None for
/// anything else of that shape: a limit that isn't a number, a path that isn't absolute, no `argv[0]`.
pub fn parse(args: &[OsString]) -> Option<(u64, &OsStr, &OsStr, &[OsString])> {
    let [flag, n, program, argv0, rest @ ..] = args else { return None };
    if flag != FLAG {
        return None;
    }
    let n: u64 = n.to_str()?.parse().ok()?;
    Path::new(program).is_absolute().then_some((n, program.as_os_str(), argv0.as_os_str(), rest))
}

/// The shim, first thing in `main`: `tarae --nofile N <absolute path> <argv0> args…` sets the soft limit to N (no
/// higher than the hard one) and execs the program as `argv0` — this process becomes it. Anything else of that
/// shape is refused: an exit, never run as the editor. Returns only when the first argument isn't the flag.
pub fn shim_main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|a| a != FLAG) {
        return;
    }
    let Some((n, program, argv0, rest)) = parse(&args) else {
        eprintln!("tarae: usage: tarae {FLAG} N <absolute path> <argv0> [args…]");
        std::process::exit(2);
    };
    let hard = limit().rlim_max;
    let l = libc::rlimit { rlim_cur: n.min(hard), rlim_max: hard };
    // SAFETY: a limit of ours, no higher than the hard one.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &l) };
    let c = |s: &OsStr| CString::new(s.as_bytes()).ok();
    let argv: Option<Vec<CString>> =
        std::iter::once(argv0).chain(rest.iter().map(|a| a.as_os_str())).map(c).collect();
    let (Some(path), Some(argv)) = (c(program), argv) else {
        eprintln!("tarae: {FLAG}: an argument holds a NUL");
        std::process::exit(2);
    };
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    // SAFETY: NUL-terminated strings and a NULL-terminated array of them, held across the call.
    unsafe { libc::execv(path.as_ptr(), argv_p.as_ptr()) };
    let e = std::io::Error::last_os_error();
    eprintln!("tarae: {}: {e}", program.to_string_lossy());
    std::process::exit(if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 });
}

#[cfg(test)]
mod tests;
