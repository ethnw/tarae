//! The sandbox on Linux: the `Policy` macOS's profile is made from, enforced by Landlock (files —
//! `landlock.rs`) and seccomp (sockets — `seccomp.rs`) on the short-lived thread that starts the run
//! (`spawn`). What differs is only how.
//!
//! Landlock grants; Seatbelt denies after it allows. So what the profile keeps inside a place it allows is a
//! hole here, and the place is granted entry by entry around it (`grant`) — its own entries then can't be
//! made, removed or renamed (the user's decision): reads everywhere but home (home a hole in `/`), in home
//! the project and its places (`Reads`) around what's kept and the token files the tools read copies of;
//! writes in the project around every protected name that's there (`protected_there`) and
//! `llm.sandbox-protect`, in the run's folder, the temp and toolchain folders, the single files cargo and
//! Gradle's wrapper write (those there now), and the devices programs use (`DEVICES`, measured).
//!
//! What Landlock can't keep, said in the report (`linux_notes`): a protected name a run makes anew where
//! nothing is protected yet (a path that isn't there has no rule); the terminal multiplexers' folders in
//! `/tmp` (splitting `/tmp` would refuse every new file there, and Java writes its temp files in `/tmp`
//! whatever `TMPDIR` says); a single file not there yet (cargo's SQLite journal). The network: what a run
//! reaches by address tarae decides, as macOS's profile does (`supervisor.rs`) — where it can't here,
//! seccomp refuses every socket but netlink, this machine's ports and the project's unix sockets too.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Availability, Denial, Policy};
use crate::landlock::{self, Ruleset};

/// Full support from this Landlock ABI (≈ Linux 6.2): it controls truncating (3) and moving across folders
/// (2). Below it the sandbox goes as far as it can and every run asks (the user's decision, 2026-10-05).
pub const FULL: u32 = 3;

/// Reading, listing, running.
const READ: u64 = landlock::EXECUTE | landlock::READ_FILE | landlock::READ_DIR;
/// Writing one file (not making or removing it).
const WRITE_ONE: u64 = landlock::READ_FILE | landlock::WRITE_FILE | landlock::TRUNCATE | landlock::IOCTL_DEV;

/// Devices programs use, and how (measured, `target/sb/measure-devices.sh`: cargo, go, npm, Maven, Gradle, a
/// pty, Python's multiprocessing): `/dev/null` read, written, created-and-truncated and asked `isatty`;
/// terminals for a pty test (`/dev/ptmx`, then its `/dev/pts/N`, with their ioctls); POSIX semaphores and
/// shared memory as files in `/dev/shm` (made and removed — macOS's profile allows them too). Reading the
/// rest of `/dev` (`/dev/urandom`) is the reads' (outside home).
const DEVICES: [(&str, u64); 7] = [
    ("/dev/null", WRITE_ONE),
    ("/dev/zero", WRITE_ONE),
    ("/dev/full", WRITE_ONE),
    ("/dev/tty", WRITE_ONE),
    ("/dev/ptmx", WRITE_ONE),
    ("/dev/pts", landlock::ALL),
    ("/dev/shm", landlock::ALL),
];

/// Whether this system can sandbox Claude's runs, from the kernel's Landlock ABI (`landlock::abi`) and
/// whether seccomp has a filter for this architecture: full from `FULL`, partial (every run asks, saying
/// why) at 1–2, none without Landlock.
pub fn availability(abi: io::Result<u32>, seccomp: bool) -> Availability {
    if !seccomp {
        return Availability::Missing("tarae has no seccomp filter for this architecture".into());
    }
    match abi {
        Ok(n) if n >= FULL => Availability::Ready,
        Ok(1) => Availability::Partial(
            "this kernel's Landlock (ABI 1) can't control truncating files, and refuses every move across \
             folders"
                .into(),
        ),
        Ok(n) => Availability::Partial(format!(
            "this kernel's Landlock (ABI {n}) can't control truncating files — a command could empty any \
             file of yours"
        )),
        Err(e) => Availability::Missing(match e.raw_os_error() {
            Some(libc::ENOSYS) => "this kernel has no Landlock (it came with Linux 5.13)".into(),
            Some(libc::EOPNOTSUPP) => "Landlock is turned off on this system (its boot's lsm= list)".into(),
            _ => format!("Landlock: {e}"),
        }),
    }
}

/// A policy made into Landlock's rules for this kernel's ABI — applied, with the seccomp filter, by the
/// thread that starts each run (`spawn`). `audit`: the denials come from Landlock's audit log (`audit_readable`
/// — seen working here); else why not, and they come from the run's own error lines. `serve`: what a run
/// reaches by address is tarae's to decide (`supervisor.rs`) — or why it can't be here, and every socket but
/// netlink is refused. `made`: what was made first, beside protected names, for the run to write in. `closed`:
/// the project's folders a run can't make, remove or rename an entry in — each holds a protected name, or one
/// below it (`grant`).
#[derive(Clone)]
pub struct Confine {
    pub abi: u32,
    pub audit: Result<(), String>,
    pub serve: Result<(), String>,
    pub made: Arc<Made>,
    pub closed: Vec<PathBuf>,
    rules: Arc<Ruleset>,
}

impl std::fmt::Debug for Confine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Confine(ABI {}, audit {:?}, serve {:?}, made {:?})",
            self.abi, self.audit, self.serve, self.made.paths
        )
    }
}

impl PartialEq for Confine {
    fn eq(&self, other: &Self) -> bool {
        self.abi == other.abi
            && self.audit == other.audit
            && self.serve == other.serve
            && self.closed == other.closed
            && Arc::ptr_eq(&self.made, &other.made)
            && Arc::ptr_eq(&self.rules, &other.rules)
    }
}

/// What a first build makes beside a protected name, made before a run: a folder holding one is closed to new
/// entries (`grant`), so a fresh clone's `cargo build` couldn't make `target/` beside `Cargo.toml`, nor Gradle
/// `build/` beside `build.gradle`. Each pack's `first-made` folders in every folder holding one of its protected
/// names, its files (written in place: `Cargo.lock`) in the outermost of those only — a workspace's
/// members get none — each when it isn't there (`to_make`). Empty, made through no link, and taken away again
/// when the run is over if still empty (`Made::tidy`): a run that built nothing leaves nothing behind.
pub struct Made {
    /// What was made — (path, a folder) — in the order made.
    pub paths: Vec<(PathBuf, bool)>,
    tidied: std::sync::atomic::AtomicBool,
}

impl Made {
    pub fn none() -> Made {
        Made { paths: Vec::new(), tidied: Default::default() }
    }

    /// What's still empty of it, taken away — once (a folder something was put in, a file written, stay). On a
    /// thread of its own: a run's end is said on the main loop.
    pub fn tidy(self: &Arc<Self>) {
        if self.paths.is_empty() || self.tidied.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let paths = self.paths.clone();
        std::thread::spawn(move || tidy(&paths));
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if !self.paths.is_empty() && !self.tidied.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let paths = std::mem::take(&mut self.paths);
            std::thread::spawn(move || tidy(&paths));
        }
    }
}

/// `paths` (made first) that are still empty, taken away, the last made first — each through its folder,
/// opened through no link: an empty folder (`rmdir` leaves one that isn't), a file of no bytes. Blocks.
fn tidy(paths: &[(PathBuf, bool)]) {
    for (path, folder) in paths.iter().rev() {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { continue };
        let Ok(dir) = landlock::open_unlinked(dir, libc::O_PATH | libc::O_DIRECTORY) else { continue };
        let Ok(name) = std::ffi::CString::new(name.as_encoded_bytes()) else { continue };
        let fd = std::os::fd::AsRawFd::as_raw_fd(&dir);
        // SAFETY: a folder descriptor of ours and a name, live for the calls; a stat buffer the call fills.
        unsafe {
            if *folder {
                libc::unlinkat(fd, name.as_ptr(), libc::AT_REMOVEDIR);
                continue;
            }
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            if libc::fstatat(fd, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) == 0 {
                let st = st.assume_init();
                if st.st_mode & libc::S_IFMT == libc::S_IFREG && st.st_size == 0 {
                    libc::unlinkat(fd, name.as_ptr(), 0);
                }
            }
        }
    }
}

/// Whether a pack may have `name` made first: one plain name, not a protected one (`protected_at`), nor the
/// first of a protected name of several (`gradle` would hold `gradle/wrapper` — a protected name made where
/// none is, which Landlock can't keep).
pub fn makeable(name: &str) -> bool {
    let plain =
        !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0');
    plain
        && super::protected_at(&[std::ffi::OsStr::new(name)]).is_empty()
        && !super::protected_names()
            .any(|p| p.split_once('/').is_some_and(|(first, _)| first.eq_ignore_ascii_case(name)))
}

/// What to make first in the project at `root` whose protected names are `found` (`protected_there`), by
/// `packs`: (path, a folder) — each pack's `first-made` folders in every folder holding one of its names (a name
/// of several parts: the folder holding its first), its files in the outermost such folders, those that
/// aren't there (nor a link) and are `makeable`. Asks the disk only whether each is there.
pub fn to_make(root: &Path, found: &[PathBuf], packs: &[crate::packs::MadeBeside]) -> Vec<(PathBuf, bool)> {
    // The folders holding each pack's names: (pack, folder)
    let mut anchors: Vec<(usize, PathBuf)> = Vec::new();
    for f in found {
        let Ok(rel) = f.strip_prefix(root) else { continue };
        let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
        for (i, m) in packs.iter().enumerate() {
            for name in &m.protect {
                let want: Vec<&str> = name.split('/').collect();
                let Some(from) = parts.len().checked_sub(want.len()) else { continue };
                if parts[from..].iter().zip(&want).all(|(p, n)| p.eq_ignore_ascii_case(n)) {
                    let at = root.join(parts[..from].iter().collect::<PathBuf>());
                    if !anchors.contains(&(i, at.clone())) {
                        anchors.push((i, at));
                    }
                }
            }
        }
    }
    let missing = |p: &Path| p.symlink_metadata().is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    for (i, at) in &anchors {
        let m = &packs[*i];
        let outermost = !anchors.iter().any(|(j, other)| j == i && other != at && at.starts_with(other));
        let folders = m.folders.iter().map(|o| (o, true));
        let files = m.files.iter().filter(|_| outermost).map(|l| (l, false));
        for (name, folder) in folders.chain(files).filter(|(n, _)| makeable(n)) {
            let path = at.join(name);
            if missing(&path) && !out.iter().any(|(p, _)| *p == path) {
                out.push((path, folder));
            }
        }
    }
    out.sort();
    out
}

/// `plan` made: each through its folder opened through no link — `mkdirat`, or a file made empty
/// (`O_CREAT|O_EXCL|O_NOFOLLOW`: one there now, or a link, is left alone). What was made.
fn make(plan: Vec<(PathBuf, bool)>) -> Made {
    let mut paths = Vec::new();
    for (path, folder) in plan {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { continue };
        let Ok(dir) = landlock::open_unlinked(dir, libc::O_PATH | libc::O_DIRECTORY) else { continue };
        let Ok(c) = std::ffi::CString::new(name.as_encoded_bytes()) else { continue };
        let fd = std::os::fd::AsRawFd::as_raw_fd(&dir);
        // SAFETY: a folder descriptor of ours and a name, live for the calls; a file made is closed at once.
        let made = unsafe {
            match folder {
                true => libc::mkdirat(fd, c.as_ptr(), 0o777) == 0,
                false => {
                    let flags =
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
                    let f = libc::openat(fd, c.as_ptr(), flags, 0o666);
                    f >= 0 && libc::close(f) == 0
                }
            }
        };
        if made {
            paths.push((path, folder));
        }
    }
    Made { paths, tidied: Default::default() }
}

impl Eq for Confine {}

/// `access` beneath `path`, around `holes`: none at or under it → one rule; `path` itself one (or in one) →
/// nothing; one deeper → each of its entries on its own, and nothing for the folder itself (its own entries
/// can't be made, removed or renamed). A path not there is skipped; a symlink gets no rule (`allow`).
fn grant(r: &mut Ruleset, path: &Path, access: u64, holes: &[PathBuf]) -> io::Result<()> {
    if holes.iter().any(|h| path.starts_with(h)) {
        return Ok(());
    }
    if !holes.iter().any(|h| h.starts_with(path)) {
        return match r.allow(path, access) {
            Ok(_) => Ok(()),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => Ok(()),
            Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        };
    }
    let Ok(entries) = std::fs::read_dir(path) else { return Ok(()) };
    for e in entries.flatten() {
        grant(r, &e.path(), access, holes)?;
    }
    Ok(())
}

/// The protected names (`protected_in`) that are there in the project at `root`, each the path where one
/// starts — not looked for below one, nor in tools' output (`OUTPUTS`, where those names are let through) —
/// and `extra` (`llm.sandbox-protect`) that are there. Links aren't followed.
pub fn protected_there(root: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if super::protected_in(root, &path) {
                out.push(path);
                continue;
            }
            let folder = e.file_type().is_ok_and(|t| t.is_dir());
            if folder && !super::OUTPUTS.iter().any(|o| e.file_name() == *o) {
                walk(root, &path, out);
            }
        }
    }
    let mut out: Vec<PathBuf> = extra.iter().filter(|p| p.symlink_metadata().is_ok()).cloned().collect();
    walk(root, root, &mut out);
    out
}

/// The folders in the project at `root` that `grant` splits around `found` (`protected_there`): each one's
/// folders up to the root, the root too.
fn closed_folders(root: &Path, found: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for f in found.iter().filter(|f| f.starts_with(root)) {
        for dir in f.ancestors().skip(1) {
            if !dir.starts_with(root) {
                break;
            }
            if !out.iter().any(|o| o == dir) {
                out.push(dir.to_path_buf());
            }
        }
    }
    out
}

/// `p` as Landlock's rules for ABI `abi`. Reads everywhere but home, and in it where `p.reads` says around what
/// it keeps (none of that when reads aren't confined); writes as `writes_allow` has them; the devices. With
/// `make_first`, what a first build makes beside a protected name is made before (`Made`) — and so granted.
pub fn confine(p: &Policy, abi: u32, audit: Result<(), String>, make_first: bool) -> io::Result<Confine> {
    let mut r = Ruleset::new(abi)?;
    let hidden: Vec<PathBuf> = p.reads.copies.hidden.iter().map(|(f, _)| f.clone()).collect();
    match &p.reads.home {
        Some(home) => {
            grant(&mut r, Path::new("/"), READ, std::slice::from_ref(home))?;
            // In home: the project, where it writes, what's allowed — none inside a kept place (but the
            // project, when one holds it), and around what's kept or read from a copy below them
            let kept = &p.reads.kept;
            let run = p.run.as_ref().map(|d| d.path().to_path_buf());
            let places = std::iter::once(&p.root)
                .chain(run.as_ref())
                .chain(&p.write)
                .chain(&p.files)
                .chain(&p.reads.allow);
            for place in places.filter(|w| w.starts_with(home)) {
                let in_kept = kept.iter().any(|k| place.starts_with(k));
                if in_kept && !place.starts_with(&p.root) {
                    continue;
                }
                let holes: Vec<PathBuf> =
                    kept.iter().chain(&hidden).filter(|h| h.starts_with(place)).cloned().collect();
                grant(&mut r, place, READ, &holes)?;
            }
            if let Some(log) = p.reads.log.as_ref().filter(|_| !kept.is_empty()) {
                grant(&mut r, log, landlock::READ_FILE, &[])?;
            }
        }
        None => grant(&mut r, Path::new("/"), READ, &hidden)?,
    }
    let abs: Vec<PathBuf> = p.protect.clone();
    let found = protected_there(&p.root, &abs);
    let made = match make_first {
        true => make(to_make(&p.root, &found, &crate::packs::made_beside())),
        false => Made::none(),
    };
    grant(&mut r, &p.root, landlock::ALL, &found)?;
    if let Some(run) = &p.run {
        grant(&mut r, run.path(), landlock::ALL, &[])?;
    }
    for w in &p.write {
        let holes: Vec<PathBuf> = p.caches.iter().filter(|c| c.starts_with(w)).cloned().collect();
        grant(&mut r, w, landlock::ALL, &holes)?;
    }
    for f in &p.files {
        grant(&mut r, f, WRITE_ONE, &[])?;
    }
    for (d, access) in DEVICES {
        grant(&mut r, Path::new(d), access, &[])?;
    }
    let serve = match super::supervisor::unavailable() {
        None => Ok(()),
        Some(why) => Err(why),
    };
    let closed = closed_folders(&p.root, &found);
    Ok(Confine { abi, audit, serve, made: Arc::new(made), closed, rules: Arc::new(r) })
}

/// `start` (a spawn) on a short-lived thread confined to `c` and under the seccomp filter — what it starts is
/// in the sandbox, the caller isn't (`landlock::confined`). `serve`: the run's policy, when tarae decides
/// what it reaches by address (`supervisor::serve`, started from here: an unconfined thread) — else the
/// plain filter. `owner` hears the thread's id before (a watch reading Landlock's audit log tells its run's
/// domain by it).
pub fn confined<T: Send>(
    c: &Confine,
    serve: Option<Arc<Policy>>,
    owner: impl FnOnce(u32) + Send,
    start: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    use crate::seccomp::Mode;
    let to = match serve.filter(|_| c.serve.is_ok()) {
        Some(p) => Some(super::supervisor::serve(p)?),
        None => None,
    };
    let mode = if to.is_some() { Mode::Served } else { Mode::Plain };
    landlock::confined(
        &c.rules,
        || crate::seccomp::apply(mode),
        |listener| {
            if let (Some(to), Some(l)) = (to, listener) {
                to.send(l).map_err(|_| io::Error::other("tarae's supervisor ended before its run"))?;
            }
            // SAFETY: gettid has no preconditions.
            owner(unsafe { libc::gettid() } as u32);
            start()
        },
    )
}

/// What the report says of what was made first (`Made`) — nothing when nothing was.
pub fn made_said(made: &Made, root: &Path) -> String {
    const SAID: usize = 6;
    if made.paths.is_empty() {
        return String::new();
    }
    let mut names: Vec<String> = made
        .paths
        .iter()
        .take(SAID)
        .map(|(p, folder)| {
            let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
            if *folder { format!("{rel}/") } else { rel }
        })
        .collect();
    if made.paths.len() > SAID {
        names.push(format!("{} more", made.paths.len() - SAID));
    }
    format!(
        " Before it ran, tarae made {} — empty: on Linux a folder holding a protected name takes no new entry, so \
         a build couldn't make them itself. What's still empty when it's over goes again.",
        names.join(", ")
    )
}

/// What a run of ABI `abi` can't be kept from, for the report: no signal scope below 6.
pub fn notes(abi: u32) -> String {
    match abi {
        n if n < 6 => format!(
            " This kernel's Landlock (ABI {n}) can't keep a command's signals to its own processes (6 does)."
        ),
        _ => String::new(),
    }
}

/// Where Landlock's audit records are read: the journal (the kernel's messages), or auditd's own log when
/// auditd runs — it takes them, and none reach the journal (the user's GCP VM: `/var/log/audit/audit.log`,
/// root:adm 0640).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditSource {
    Journal,
    Auditd(PathBuf),
}

impl AuditSource {
    /// The command that prints its records as they come (from its end).
    pub fn stream(&self) -> std::process::Command {
        match self {
            AuditSource::Journal => {
                let mut c = std::process::Command::new("journalctl");
                let args = [
                    "-f",
                    "-n",
                    "0",
                    "-q",
                    "-o",
                    "cat",
                    "--no-pager",
                    "_TRANSPORT=kernel",
                    "_TRANSPORT=audit",
                ];
                c.args(args);
                c
            }
            AuditSource::Auditd(log) => {
                let mut c = std::process::Command::new("tail");
                c.args(["-n", "0", "-F"]).arg(log);
                c
            }
        }
    }
}

/// Where the records go here, from whether auditd runs (`auditd`: its log, or why this user can't read it) and
/// whether the journal shows the kernel's messages to this user — or why there's nowhere to read them.
fn audit_source(auditd: Option<Result<PathBuf, String>>, journal: bool) -> Result<AuditSource, String> {
    match auditd {
        Some(Ok(log)) => Ok(AuditSource::Auditd(log)),
        Some(Err(why)) => Err(why),
        None if journal => Ok(AuditSource::Journal),
        None => Err("tarae can't read the kernel's log here".into()),
    }
}

/// Whether auditd runs (a process of that name), and then its log — `log_file` in its settings, else the
/// default — or why this user can't read it. Blocks (a worker's).
fn auditd_log() -> Option<Result<PathBuf, String>> {
    let running = std::fs::read_dir("/proc").ok()?.flatten().any(|e| {
        let pid = e.file_name();
        pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit())
            && std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "auditd")
    });
    if !running {
        return None;
    }
    let conf = std::fs::read_to_string("/etc/audit/auditd.conf").unwrap_or_default();
    let set = conf.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "log_file").then(|| PathBuf::from(v.trim()))
    });
    let log = set.unwrap_or_else(|| PathBuf::from("/var/log/audit/audit.log"));
    Some(match std::fs::File::open(&log) {
        Ok(_) => Ok(log),
        Err(_) => Err("audit goes to auditd's log, which this user can't read".into()),
    })
}

/// Where this tarae reads Landlock's records (`audit_readable` finds it; the journal until then).
static AUDIT_SOURCE: std::sync::OnceLock<AuditSource> = std::sync::OnceLock::new();

/// Whether a run's denials come from Landlock's audit records here — Err why not (said with the result): below
/// ABI 7 none are made; they must be readable where they go (`audit_source`: auditd's log when it runs, else
/// the journal); and the kernel's audit must be on (`auditd`, or `audit=1` at boot) — off, as on a stock
/// Ubuntu 24.04, Landlock emits nothing, which no setting says: the result said "blocked nothing" for two real
/// denials (the user, kernel 7.0). So it's tried for real there (`audit_probe`). Blocks up to `AUDIT_PROBE`
/// (a worker's), once.
pub fn audit_readable(abi: u32) -> Result<(), String> {
    if abi < 7 {
        return Err(format!("this kernel's Landlock (ABI {abi}) doesn't log what it refuses"));
    }
    static FOUND: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let journal = std::process::Command::new("journalctl")
                .args(["-k", "-n", "1", "-q", "-o", "cat", "--no-pager"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
            let source = audit_source(auditd_log(), journal)?;
            let found = audit_probe(abi, |_, _| source.stream(), AUDIT_PROBE);
            let _ = AUDIT_SOURCE.set(source);
            found
        })
        .clone()
}

/// How long `audit_probe` waits for its record.
const AUDIT_PROBE: std::time::Duration = std::time::Duration::from_millis(500);

/// A denial of a throwaway thread's own — confined to nothing, it tries to make a file in a temp folder of the
/// probe's, again every 100 ms — and its record looked for in `stream` (made with that thread's id and that
/// folder: a refused new file's record names its folder, measured) for up to `wait`. Ok: it came, and `Audit`
/// tells it as that thread's run's; Err why not.
fn audit_probe(
    abi: u32,
    stream: impl FnOnce(u32, &Path) -> std::process::Command,
    wait: std::time::Duration,
) -> Result<(), String> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let folder = std::env::temp_dir().join(format!("tarae-audit-probe-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // The thread that's denied: confined to nothing (it ends with the probe)
    let ((tid_tx, tid_rx), (stop, stopped)) = (channel(), channel::<()>());
    let tried = folder.join("x");
    std::thread::spawn(move || {
        if Ruleset::new(abi).and_then(|r| r.apply()).is_err() {
            return;
        }
        // SAFETY: gettid has no preconditions.
        let _ = tid_tx.send(unsafe { libc::gettid() } as u32);
        loop {
            let _ = std::fs::OpenOptions::new().write(true).create_new(true).open(&tried);
            if stopped.recv_timeout(std::time::Duration::from_millis(100)) != Err(RecvTimeoutError::Timeout) {
                return;
            }
        }
    });
    let tid =
        tid_rx.recv_timeout(std::time::Duration::from_secs(1)).map_err(|_| "Landlock confined no thread")?;
    let mut cmd = stream(tid, &folder);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("tarae can't read the kernel's log here ({e})"))?;
    let (lines_tx, lines) = channel::<String>();
    let out = child.stdout.take();
    std::thread::spawn(move || {
        let Some(out) = out else { return };
        for l in io::BufRead::lines(io::BufReader::new(out)).map_while(Result::ok) {
            if lines_tx.send(l).is_err() {
                return;
            }
        }
    });
    let (mut audit, mut seen, mut told) = (Audit { owner: Some(tid), ..Default::default() }, false, false);
    let end = std::time::Instant::now() + wait;
    while !told {
        let Ok(line) = lines.recv_timeout(end.saturating_duration_since(std::time::Instant::now())) else {
            break;
        };
        // (the folder — as a refused new file's record names it — or the file in it)
        let ours = |path: &str| {
            let p = Path::new(path);
            p.ends_with(&name) || p.parent().is_some_and(|d| d.ends_with(&name))
        };
        seen |=
            audit_value(&line, "blockers").is_some() && audit_value(&line, "path").is_some_and(|p| ours(&p));
        told = audit.line(&line).iter().any(|d| ours(&d.target));
    }
    let _ = stop.send(());
    let _ = child.kill();
    std::thread::spawn(move || child.wait());
    let _ = std::fs::remove_dir_all(&folder);
    match (told, seen) {
        (true, _) => Ok(()),
        (false, true) => {
            Err("Landlock's audit records here don't name the thread that confined a run".into())
        }
        (false, false) => Err("kernel audit is off (no auditd or audit=1)".into()),
    }
}

/// The command a watch reads Landlock's audit records from, as they come: where `audit_readable` found them
/// (auditd's log, or the journal).
pub fn audit_stream() -> std::process::Command {
    AUDIT_SOURCE.get().unwrap_or(&AuditSource::Journal).stream()
}

/// A value in an audit record's `key=value` words: quoted, or hex (how audit writes a string with spaces).
fn audit_value(line: &str, key: &str) -> Option<String> {
    let at = line.split(' ').position(|w| w.starts_with(&format!("{key}=")))?;
    let rest: String = line.split(' ').skip(at).collect::<Vec<_>>().join(" ");
    let v = &rest[key.len() + 1..];
    if let Some(q) = v.strip_prefix('"') {
        return q.split_once('"').map(|(s, _)| s.to_string());
    }
    let word = v.split(' ').next().unwrap_or_default();
    let hex = word.len().is_multiple_of(2) && !word.is_empty() && word.chars().all(|c| c.is_ascii_hexdigit());
    if hex && key == "path" {
        let bytes: Option<Vec<u8>> =
            (0..word.len()).step_by(2).map(|i| u8::from_str_radix(&word[i..i + 2], 16).ok()).collect();
        return bytes.map(|b| String::from_utf8_lossy(&b).into_owned());
    }
    Some(word.to_string())
}

/// What one run's Landlock audit records say, read line by line: its domain is the one whose record names
/// the thread that confined it (`owner`); access records of a domain not told yet wait for it.
#[derive(Debug, Default)]
pub struct Audit {
    pub owner: Option<u32>,
    ours: Vec<String>,
    others: Vec<String>,
    waiting: Vec<(String, Denial)>,
}

/// Access records kept waiting for their domain's record, at most.
const AUDIT_WAITING: usize = 64;

impl Audit {
    /// The denials `line` adds: an access record of the run's domain (`LANDLOCK_ACCESS`: `domain=… blockers=…
    /// path=…`), or those waiting for a domain record that turned out the run's (`LANDLOCK_DOMAIN`:
    /// `domain=… status=allocated … pid=…`). Said in the profile's words (`file-write*`, `file-read-data`).
    pub fn line(&mut self, line: &str) -> Vec<Denial> {
        let Some(domain) = audit_value(line, "domain") else { return Vec::new() };
        if let Some(blockers) = audit_value(line, "blockers") {
            let target = audit_value(line, "path").unwrap_or_default();
            let d = Denial { process: "the run".into(), op: audit_op(&blockers).into(), target };
            if self.ours.contains(&domain) {
                return vec![d];
            }
            if !self.others.contains(&domain) && self.waiting.len() < AUDIT_WAITING {
                self.waiting.push((domain, d));
            }
            return Vec::new();
        }
        let pid = audit_value(line, "pid").and_then(|p| p.parse::<u32>().ok());
        if audit_value(line, "status").is_none() || pid.is_none() {
            return Vec::new();
        }
        let mine = self.owner.is_some() && pid == self.owner;
        let (keep, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.waiting).into_iter().partition(|(d, _)| *d == domain);
        self.waiting = rest;
        match mine {
            true => {
                self.ours.push(domain);
                keep.into_iter().map(|(_, d)| d).collect()
            }
            false => {
                self.others.push(domain);
                Vec::new()
            }
        }
    }
}

/// An audit record's blockers (`fs.write_file,fs.make_reg`, `scope.signal`) in the profile's words.
fn audit_op(blockers: &str) -> &'static str {
    let any = |names: &[&str]| blockers.split(',').any(|b| names.contains(&b));
    if any(&["fs.read_file", "fs.read_dir", "fs.execute"]) && !blockers.contains("fs.write") {
        return "file-read-data";
    }
    // (a new file, folder or link refused: the record names the folder it was refused in — `merge_made`)
    if blockers.split(',').all(|b| b.starts_with("fs.make_")) {
        return "file-make-in";
    }
    if blockers.starts_with("fs.") {
        return "file-write*";
    }
    if any(&["scope.signal"]) {
        return "signal";
    }
    "network-outbound"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Env, Guard, Reads, RunDir};

    /// The kernel's ABI, or None after saying the test skips itself (no Landlock here).
    fn kernel_abi(test: &str) -> Option<u32> {
        match landlock::abi() {
            Ok(n) => Some(n),
            Err(e) => {
                let line = format!(
                    "{}sandbox::linux::tests::{test} — this kernel has no Landlock ({e})\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                None
            }
        }
    }

    /// The floor: full from ABI 3, partial (saying why) at 1–2, none without Landlock or a seccomp filter.
    #[test]
    fn availability_follows_the_kernels_abi() {
        let err = |n| Err(io::Error::from_raw_os_error(n));
        assert_eq!(availability(Ok(3), true), Availability::Ready);
        assert_eq!(availability(Ok(8), true), Availability::Ready);
        let partial = |a| match availability(Ok(a), true) {
            Availability::Partial(why) => why,
            other => panic!("ABI {a}: {other:?}"),
        };
        assert!(partial(2).contains("can't control truncating files"), "{}", partial(2));
        assert!(partial(1).contains("refuses every move across folders"), "{}", partial(1));
        let missing = |a: io::Result<u32>, seccomp| match availability(a, seccomp) {
            Availability::Missing(why) => why,
            other => panic!("{other:?}"),
        };
        assert!(missing(err(libc::ENOSYS), true).contains("no Landlock (it came with Linux 5.13)"));
        assert!(missing(err(libc::EOPNOTSUPP), true).contains("turned off"));
        assert!(missing(Ok(8), false).contains("seccomp"));
        // (this kernel, asked as the probe asks)
        if let Ok(n) = landlock::abi() {
            assert_eq!(crate::sandbox::probe_now() == Availability::Ready, n >= FULL);
        }
    }

    /// The protected names that are there, each where it starts — not below one, nor in tools' output — and
    /// `llm.sandbox-protect`'s; a link isn't followed.
    #[test]
    fn protected_names_are_found_where_they_are() {
        let dir = std::env::temp_dir().join(format!("tarae-protected-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in [
            "Cargo.toml",
            ".git/HEAD",
            "src/main.rs",
            "sub/build.rs",
            "app/gradle/wrapper/gradle-wrapper.properties",
            "target/package/x/Cargo.toml",
            "node_modules/p/package.json",
            "kept/x",
        ] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        let outside = dir.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Makefile"), "").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let mut found = protected_there(&dir, &[dir.join("kept"), dir.join("not-there")]);
        found.sort();
        let want: Vec<PathBuf> = [".git", "Cargo.toml", "app/gradle/wrapper", "kept", "sub/build.rs"]
            .iter()
            .map(|f| dir.join(f))
            .collect();
        assert_eq!(found, want);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// A scratch project and home for a real policy, under the build's `target/` (not in `/tmp`: the
    /// policy writes there, and leaves out what holds the project).
    struct Scratch {
        base: PathBuf,
        root: PathBuf,
        home: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let base = std::env::current_dir()
                .unwrap()
                .join(format!("target/tarae-confine-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            for f in [
                "proj/Cargo.toml",
                "proj/.git/HEAD",
                "proj/src/main.rs",
                "proj/sub/build.rs",
                "proj/sub/lib.rs",
                "proj/kept/x",
                "home/.cargo/config.toml",
                "home/.cargo/credentials.toml",
                "home/.ssh/id",
                "home/notes",
            ] {
                std::fs::create_dir_all(base.join(f).parent().unwrap()).unwrap();
                std::fs::write(base.join(f), "x").unwrap();
            }
            let base = std::fs::canonicalize(&base).unwrap();
            Scratch { root: base.join("proj"), home: base.join("home"), base }
        }

        /// The policy a run of Claude's gets here: `kept` protected (`llm.sandbox-protect`), reads in home
        /// only `.cargo` but its credentials and `.ssh`, a folder of its own — made into the rules.
        fn policy(&self) -> Policy {
            let h = &self.home;
            let env = Env {
                root: self.root.clone(),
                home: Some(h.clone()),
                tmp: Some(crate::sandbox::profile_path(&std::env::temp_dir())),
                protect: vec!["kept".into()],
                reads: Reads {
                    home: Some(h.clone()),
                    allow: vec![h.join(".cargo")],
                    kept: vec![h.join(".ssh"), h.join(".cargo/credentials.toml")],
                    ..Default::default()
                },
                ..Default::default()
            };
            let p = Policy { run: RunDir::new().ok(), ..Policy::for_project(&env) };
            crate::sandbox::confined(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// For real: a run started through `sandbox::spawn` keeps to its policy — writes in the project but its
    /// protected names (where they are, and every entry of a folder holding one), `llm.sandbox-protect`, git's
    /// folder; in its own folder, `/tmp`, `/dev/null`; nowhere else. Reads in home only what's allowed, never
    /// what's kept inside it; outside home anything. No port of this machine's that wasn't opened. The thread
    /// that started it writes anywhere.
    #[test]
    fn a_run_keeps_to_its_policy() {
        let Some(_) = kernel_abi("a_run_keeps_to_its_policy") else { return };
        let s = Scratch::new("run");
        let p = s.policy();
        assert!(matches!(p.confine, Some(Ok(_))), "{:?}", p.confine);
        let run = p.run.as_ref().unwrap().path().to_path_buf();
        let script = r#"
            w() { if ( : > "$2" ) 2>/dev/null; then echo "$1 ok"; else echo "$1 no"; fi; }
            r() { if cat "$2" >/dev/null 2>&1; then echo "$1 ok"; else echo "$1 no"; fi; }
            w project "$R/src/new.rs"; w protected "$R/Cargo.toml"; w protected-deep "$R/sub/build.rs"
            w new-at-root "$R/new.txt"; w beside-a-protected-name "$R/sub/new.rs"; w git "$R/.git/HEAD"
            w llm-protect "$R/kept/x"; w run "$RUN/x"; w tmp "/tmp/tarae-confine-$$"; w devnull /dev/null
            w outside "$B/outside"
            r home-allowed "$H/.cargo/config.toml"; r home-kept "$H/.cargo/credentials.toml"
            r home-ssh "$H/.ssh/id"; r home-other "$H/notes"; r outside-home /etc/hostname
            if mv "$R/src/main.rs" "$R/src/moved.rs" 2>/dev/null; then echo move ok; else echo move no; fi
            if rm "$R/sub/build.rs" 2>/dev/null; then echo remove-protected ok; else echo remove-protected no; fi
            if bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT" 2>/dev/null; then echo socket ok; else echo socket no; fi
            rm -f "/tmp/tarae-confine-$$"
        "#;
        // (a port that answers — the test's own listener: unconfined, the run would connect)
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let mut cmd = crate::sandbox::command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.env("R", &s.root).env("RUN", &run).env("H", &s.home).env("B", &s.base).env("PORT", port);
        let spawned = crate::sandbox::spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        let got: Vec<&str> = out.lines().collect();
        let want = [
            "project ok",
            "protected no",
            "protected-deep no",
            "new-at-root no",
            "beside-a-protected-name no",
            "git no",
            "llm-protect no",
            "run ok",
            "tmp ok",
            "devnull ok",
            "outside no",
            "home-allowed ok",
            "home-kept no",
            "home-ssh no",
            "home-other no",
            "outside-home ok",
            "move ok",
            "remove-protected no",
            "socket no",
        ];
        assert_eq!(got, want);
        std::fs::write(s.base.join("outside"), "x").expect("the thread that started it writes anywhere");
    }

    /// What a first build makes beside a protected name, by the built-in packs: their folders in every folder
    /// holding one of their names — a name of several parts (`gradle/wrapper`) counting for the folder holding
    /// its first — their files in the outermost only (a workspace member gets none); nothing beside a
    /// core name (`.envrc`), nothing that's there, nor where a link is. (A tree in the test's own folder.)
    #[test]
    fn what_a_first_build_makes_is_made_first_where_it_goes() {
        let dir = std::env::temp_dir().join(format!("tarae-made-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in [
            "Cargo.toml",
            ".git/HEAD",
            "sub/Cargo.toml",
            "sub/build.rs",
            "web/package.json",
            "app/build.gradle",
            "settings.gradle",
            "gradle/wrapper/gradle-wrapper.properties",
            "docs/.envrc",
            "done/Cargo.toml",
            "done/target/x",
            "link/Cargo.toml",
        ] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::os::unix::fs::symlink(dir.join("docs"), dir.join("link/target")).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let found = protected_there(&root, &[]);
        let mut plan: Vec<(String, bool)> = to_make(&root, &found, &crate::packs::made_beside())
            .into_iter()
            .map(|(p, folder)| (p.strip_prefix(&root).unwrap().display().to_string(), folder))
            .collect();
        plan.sort();
        let want: Vec<(String, bool)> = [
            (".gradle", true),
            ("Cargo.lock", false),
            ("app/.gradle", true),
            ("app/build", true),
            ("app/target", true),
            ("build", true),
            ("sub/target", true),
            ("target", true),
            ("web/node_modules", true),
            ("web/package-lock.json", false),
        ]
        .iter()
        .map(|(p, f)| (p.to_string(), *f))
        .collect();
        assert_eq!(plan, want);
        // Made: through no link, each once; the next plan has nothing left
        let made = make(to_make(&root, &found, &crate::packs::made_beside()));
        assert_eq!(made.paths.len(), want.len());
        assert!(root.join("target").is_dir() && root.join("Cargo.lock").metadata().unwrap().len() == 0);
        assert!(to_make(&root, &found, &crate::packs::made_beside()).is_empty(), "all there now");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Nothing made first is, or can hold, a protected name: the built-in packs' `first-made` names are
    /// each one plain name, none a protected one in any case, nor the first of one of several.
    #[test]
    fn nothing_made_first_is_a_protected_name() {
        let packs = crate::packs::made_beside();
        let names: Vec<&String> = packs.iter().flat_map(|m| m.folders.iter().chain(&m.files)).collect();
        for want in ["target", "Cargo.lock", "node_modules", "package-lock.json", ".gradle", "build"] {
            assert!(names.iter().any(|n| *n == want), "{want}: {names:?}");
        }
        for n in &names {
            assert!(makeable(n), "{n}");
        }
        for bad in
            ["build.rs", "CARGO.TOML", ".cargo", ".git", ".envrc", "gradle", "Gradle", "a/b", "..", ".", ""]
        {
            assert!(!makeable(bad), "{bad}");
        }
        // (a pack listing one: left out of the plan)
        let root = std::env::temp_dir().join(format!("tarae-made-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let bad = crate::packs::MadeBeside {
            pack: "bad".into(),
            protect: vec!["Cargo.toml".into()],
            folders: vec![".cargo".into(), "gradle".into(), "out".into()],
            files: vec!["build.rs".into()],
        };
        let plan = to_make(&root, &[root.join("Cargo.toml")], &[bad]);
        assert_eq!(plan, [(root.join("out"), true)]);
        std::fs::remove_dir_all(&root).ok();
    }

    /// For real, a fresh clone: its first builds write what they make beside their manifests — made first,
    /// empty — and nothing else new there (a protected name least of all). When the run is over, what it
    /// left empty goes; what it wrote stays.
    #[test]
    fn a_fresh_clones_first_build_works() {
        let Some(_) = kernel_abi("a_fresh_clones_first_build_works") else { return };
        let s = Scratch::new("fresh");
        for f in ["proj/web/package.json", "proj/app/build.gradle"] {
            std::fs::create_dir_all(s.base.join(f).parent().unwrap()).unwrap();
            std::fs::write(s.base.join(f), "x").unwrap();
        }
        let p = s.policy();
        let r = &s.root;
        assert!(
            r.join("target").is_dir() && r.join("sub/target").is_dir() && !r.join("sub/Cargo.lock").exists()
        );
        let script = r#"
            m() { if mkdir -p "$2" 2>/dev/null; then echo "$1 ok"; else echo "$1 no"; fi; }
            w() { if ( echo x > "$2" ) 2>/dev/null; then echo "$1 ok"; else echo "$1 no"; fi; }
            m target "$R/target/debug/deps"; w lock "$R/Cargo.lock"; m npm "$R/web/node_modules/p"
            w npm-lock "$R/web/package-lock.json"; m gradle "$R/app/build/classes"
            m cargo-config "$R/.cargo"; w build-rs "$R/build.rs"; m wrapper "$R/gradle/wrapper"; w other "$R/web/x.json"
        "#;
        let mut cmd = crate::sandbox::command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.env("R", r);
        let spawned = crate::sandbox::spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        let got: Vec<&str> = out.lines().collect();
        let want = [
            "target ok",
            "lock ok",
            "npm ok",
            "npm-lock ok",
            "gradle ok",
            "cargo-config no",
            "build-rs no",
            "wrapper no",
            "other no",
        ];
        assert_eq!(got, want);
        // (the run is gone: what it left empty goes, on a thread of its own)
        let left_empty = ["sub/target", "app/.gradle", "app/target"];
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while left_empty.iter().any(|e| r.join(e).exists()) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for e in left_empty {
            assert!(!r.join(e).exists(), "{e} left empty: gone");
        }
        for kept in
            ["target/debug", "Cargo.lock", "web/node_modules/p", "web/package-lock.json", "app/build/classes"]
        {
            assert!(r.join(kept).exists(), "{kept} written: kept");
        }
        drop(p);
    }

    /// What Claude reads of a Linux run: the network as macOS says it where tarae serves it (nothing beyond
    /// this machine, `llm.sandbox-network` or not), else no socket at all and why; the protected names where
    /// they are, what Landlock can't keep, and where what it refused is read from — this kernel's ABI said.
    #[test]
    fn the_report_says_how_linux_differs() {
        let Some(abi) = kernel_abi("the_report_says_how_linux_differs") else { return };
        let s = Scratch::new("report");
        let said = |p: Policy| {
            let r = Guard::In(Box::new(p)).report(&[]);
            r.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let r = said(s.policy());
        match super::super::supervisor::unavailable() {
            None => {
                assert!(
                    r.contains("connect to nothing over the network, this machine's ports included"),
                    "{r}"
                );
                let open = said(Policy { network: true, ..s.policy() });
                assert!(!open.contains("reach the network"), "{open}");
                assert!(
                    open.contains("llm.sandbox-network doesn't open the network beyond this machine"),
                    "{open}"
                );
            }
            Some(why) => {
                assert!(
                    r.contains(
                        "make no socket: no network, not even this machine's ports, and no unix socket"
                    ),
                    "{r}"
                );
                assert!(r.contains("which macOS allows") && r.contains(&why), "{r}");
            }
        }
        assert!(r.contains("where they are now"), "{r}");
        assert!(
            r.contains("Before it ran, tarae made Cargo.lock, ") && r.contains(" target/"),
            "made first: {r}"
        );
        assert!(r.contains("a protected name it makes where none is yet isn't refused"), "{r}");
        assert_eq!(r.contains("can't keep a command's signals"), abi < 6, "{r}");
        if abi < 7 {
            assert!(
                r.contains(&format!("this kernel's Landlock (ABI {abi}) doesn't log what it refuses")),
                "{r}"
            );
        }
    }

    /// Whether Landlock's records come is tried for real: a denial of a thread's own, its record looked for. A
    /// stand-in for `journalctl`: silent (the kernel's audit off — a stock Ubuntu's) is said so, within the
    /// wait; the probe's own record, its domain naming the thread that confined it, is the log working; one
    /// whose domain names another (the record's `pid=` not the confining thread's) can't be told as a run's.
    #[test]
    fn the_audit_log_is_tried_for_real() {
        let Some(abi) = kernel_abi("the_audit_log_is_tried_for_real") else { return };
        let sh = |script: String| {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(script);
            c
        };
        // (auditd's form, as its log has them: the new file refused named by its folder, `UID=` glued on)
        let records = |pid: String, path: &Path| {
            format!(
                "echo 'type=UNKNOWN[1424] msg=audit(1:2): domain=7a status=allocated mode=enforcing pid={pid} \
                 uid=1001 exe=\"/t/tarae\" comm=\"tarae-landlock\"UID=\"wq\"'; \
                 echo 'type=UNKNOWN[1423] msg=audit(1:2): domain=7a blockers=fs.make_reg path=\"{}\" dev=\"sda1\" \
                 ino=1'; exec sleep 5",
                path.display()
            )
        };
        let wait = std::time::Duration::from_millis(500);
        let t = std::time::Instant::now();
        let off = audit_probe(abi, |_, _| sh("exec sleep 5".into()), wait);
        assert_eq!(off, Err("kernel audit is off (no auditd or audit=1)".into()));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}: within its wait", t.elapsed());
        assert_eq!(audit_probe(abi, |tid, path| sh(records(tid.to_string(), path)), wait), Ok(()));
        let other = audit_probe(abi, |_, path| sh(records("1".into(), path)), wait);
        assert!(other.as_ref().is_err_and(|w| w.contains("don't name the thread")), "{other:?}");
    }

    /// What a Linux run's output says the sandbox refused, though the policy lets it write there: making,
    /// removing or moving an entry of a folder holding a protected name (Landlock's own rule — by a path, or a
    /// name from where the run is, as `sh` says it), and from ABI 6 a signal to a process outside the run's
    /// own (Landlock's scope). In plain words, and in the report — not "blocked nothing" — said as read from
    /// the output even where the audit log works (it didn't have them).
    #[test]
    fn what_a_linux_runs_output_says_was_refused() {
        let Some(_) = kernel_abi("what_a_linux_runs_output_says_was_refused") else { return };
        let s = Scratch::new("said");
        let mut p = s.policy();
        if let Some(Ok(c)) = &mut p.confine {
            (c.abi, c.audit) = (6, Ok(()));
        }
        p.watch = Some(crate::sandbox::Watch::quiet("t."));
        let lib = s.root.join("sub/lib.rs");
        let out = [
            (true, "sh: 1: cannot create Cargo.toml.bak: Permission denied".to_string()),
            (true, format!("rm: cannot remove '{}': Permission denied", lib.display())),
            (true, "bash: kill: (4242) - Operation not permitted".to_string()),
            (true, "rm: cannot remove 'src/old.rs': No such file or directory".to_string()),
        ];
        let plain = |p: &Policy| p.said_in_output(&out, &[]).iter().map(Denial::plain).collect::<Vec<_>>();
        let said = plain(&p);
        assert_eq!(
            said,
            [
                format!(
                    "making {} — a folder holding protected names takes no new files on Linux",
                    s.root.join("Cargo.toml.bak").display()
                ),
                format!(
                    "removing {} — a folder holding protected names keeps its files on Linux",
                    lib.display()
                ),
                "signalling process 4242 — outside the run's own processes".to_string(),
            ]
        );
        let r = Guard::In(Box::new(p.clone())).report(&out);
        let r = r.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(r.contains("The sandbox blocked 3 things"), "{r}");
        assert!(
            r.contains("(Read from its own error lines") && !r.contains("from Landlock's audit log,"),
            "{r}"
        );
        assert!(r.contains("make it in a folder without one") && r.contains("can't signal"), "{r}");
        assert!(!r.contains("open it from the run's panel"), "nothing to open for these: {r}");
        // Below ABI 6 a refused signal isn't the sandbox's
        if let Some(Ok(c)) = &mut p.confine {
            c.abi = 5;
        }
        assert_eq!(plain(&p).len(), 2);
    }

    /// Without an audit log, what a run refused is read from its error lines: Landlock's "Permission
    /// denied" on a path the policy keeps, as the profile's words — paths as they are (no `/private`).
    #[test]
    fn the_output_says_what_was_refused() {
        let root = PathBuf::from("/w/p");
        let p = Policy::for_project(&Env { root: root.clone(), ..Default::default() });
        let out = [
            (true, "touch: cannot touch '/w/p/Cargo.toml': Permission denied".to_string()),
            (true, "cat: /tmp/x: No such file or directory".to_string()),
        ];
        let said = p.said_in_output(&out, &[]);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!((said[0].op.as_str(), said[0].target.as_str()), ("file-write*", "/w/p/Cargo.toml"));
        assert_eq!(crate::sandbox::paths_in("cat /tmp/x", &root), [PathBuf::from("/tmp/x")]);
        assert_eq!(crate::sandbox::refused_lines(&out), [out[0].1.clone()]);
    }

    /// Landlock's audit records (the kernel's documented form): the run's domain is the one whose record
    /// names the thread that confined it; access records waiting for it come with it, others' are dropped;
    /// a path in hex is read.
    #[test]
    fn audit_records_are_read_for_their_run() {
        let access = |d: &str, b: &str, path: &str| {
            format!(
                "audit: type=1423 audit(1729738800.268:30): domain={d} blockers={b} path={path} dev=\"vda2\" ino=9"
            )
        };
        let domain = |d: &str, pid: u32| {
            format!(
                "audit: type=1424 audit(1729738800.268:30): domain={d} status=allocated mode=enforcing pid={pid} \
                 uid=0 exe=\"/usr/bin/tarae\" comm=\"tarae-landlock\""
            )
        };
        let mut a = Audit { owner: Some(300), ..Default::default() };
        assert!(
            a.line(&access("1a", "fs.write_file", "\"/w/p/Cargo.toml\"")).is_empty(),
            "waits for its domain"
        );
        let d = a.line(&domain("1a", 300));
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].op.as_str(), d[0].target.as_str()), ("file-write*", "/w/p/Cargo.toml"));
        let d = a.line(&access("1a", "fs.read_file", "2F686F6D652F752F612062"));
        assert_eq!((d[0].op.as_str(), d[0].target.as_str()), ("file-read-data", "/home/u/a b"));
        assert_eq!(
            a.line(&access("1a", "scope.signal", "\"\"")).first().map(|d| d.op.clone()),
            Some("signal".into())
        );
        // Another run's domain
        assert!(a.line(&access("2b", "fs.make_reg", "\"/x\"")).is_empty());
        assert!(a.line(&domain("2b", 301)).is_empty(), "not this run's");
        assert!(a.line(&access("2b", "fs.make_reg", "\"/x\"")).is_empty());
        assert!(a.line("audit: type=1400 apparmor=\"DENIED\"").is_empty());
    }

    /// auditd's records, verbatim from its log on a kernel 7.0 (the user's VM): its enrichment glued on
    /// (`comm="…"UID="wq"`, no space) changes nothing; a refused new file names its folder (`file-make-in`),
    /// whichever record comes first. Where they're read: auditd's log when it runs (why not, when this user
    /// can't read it), else the journal — followed from its end.
    #[test]
    fn auditds_records_are_read_where_they_go() {
        let domain = r#"type=UNKNOWN[1424] msg=audit(1791187720.409:280): domain=107163519 status=allocated mode=enforcing pid=127026 uid=1001 exe="/home/wq/tarae/target-ll/release/tarae" comm="tarae-landlock"UID="wq""#;
        let access = r#"type=UNKNOWN[1423] msg=audit(1791187720.409:280): domain=107163519 blockers=fs.make_reg path="/home/wq/proj" dev="sda1" ino=264783"#;
        let made =
            Denial { process: "the run".into(), op: "file-make-in".into(), target: "/home/wq/proj".into() };
        let mut a = Audit { owner: Some(127026), ..Default::default() };
        assert!(a.line(domain).is_empty());
        assert_eq!(a.line(access), std::slice::from_ref(&made));
        let mut a = Audit { owner: Some(127026), ..Default::default() };
        assert!(a.line(access).is_empty(), "waits for its domain");
        assert_eq!(a.line(domain), [made]);
        let log = PathBuf::from("/var/log/audit/audit.log");
        let unreadable = "audit goes to auditd's log, which this user can't read".to_string();
        assert_eq!(audit_source(Some(Ok(log.clone())), true), Ok(AuditSource::Auditd(log.clone())));
        assert_eq!(audit_source(Some(Err(unreadable.clone())), true), Err(unreadable));
        assert_eq!(audit_source(None, true), Ok(AuditSource::Journal));
        assert!(audit_source(None, false).is_err_and(|w| w.contains("can't read the kernel's log")));
        let tail = AuditSource::Auditd(log).stream();
        let args: Vec<_> = tail.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            (tail.get_program().to_string_lossy(), args.join(" ")),
            ("tail".into(), "-n 0 -F /var/log/audit/audit.log".into())
        );
    }

    /// A refused new file's record names only its folder: the file the output names there takes its place
    /// (one line, the kernel's); without one the folder is said — the rule said too where it's Landlock's own
    /// (a folder holding protected names), else as a write there (what `space m s` can open).
    #[test]
    fn a_new_files_record_takes_the_files_name_from_the_output() {
        let Some(_) = kernel_abi("a_new_files_record_takes_the_files_name_from_the_output") else { return };
        let s = Scratch::new("made-in");
        let p = s.policy();
        let in_root = Denial {
            process: "the run".into(),
            op: "file-make-in".into(),
            target: s.root.display().to_string(),
        };
        let mut seen = vec![in_root.clone()];
        let out = [(true, "sh: 1: cannot create Cargo.toml.bak: Permission denied".to_string())];
        let said = p.said_in_output(&out, &seen);
        let rest = p.merge_made(&mut seen, said);
        assert!(rest.is_empty(), "{rest:?}");
        let bak = s.root.join("Cargo.toml.bak").display().to_string();
        assert_eq!(
            seen[0].plain(),
            format!("making {bak} — a folder holding protected names takes no new files on Linux")
        );
        assert_eq!(seen[0].process, "the run");
        let mut alone = vec![in_root];
        assert!(p.merge_made(&mut alone, Vec::new()).is_empty());
        let root = s.root.display();
        assert_eq!(
            alone[0].plain(),
            format!(
                "making a new file in {root} — a folder holding protected names takes no new files on Linux"
            )
        );
        let mut outside = vec![Denial {
            process: "the run".into(),
            op: "file-make-in".into(),
            target: s.base.display().to_string(),
        }];
        p.merge_made(&mut outside, Vec::new());
        assert_eq!(
            (outside[0].op.as_str(), outside[0].plain()),
            ("file-write*", format!("writing {}", s.base.display()))
        );
    }

    /// A watch reads its run's records as they come (a stand-in for `journalctl`: a script that prints the
    /// kernel's lines once the run's thread is told), and stops with it.
    #[test]
    fn a_watch_reads_its_runs_audit_records() {
        let dir = std::env::temp_dir().join(format!("tarae-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join("go");
        let mut stream = std::process::Command::new("sh");
        stream.arg("-c").arg(format!(
            "until [ -e '{}' ]; do sleep 0.01; done; \
             echo 'audit: type=1424 audit(1:2): domain=9f status=allocated mode=enforcing pid=4242 uid=1'; \
             echo 'audit: type=1423 audit(1:2): domain=9f blockers=fs.write_file path=\"/w/p/build.rs\" ino=1'; \
             exec sleep 30",
            go.display()
        ));
        let w = crate::sandbox::Watch::start_audit_from("t.", stream);
        w.owned_by(4242);
        std::fs::write(&go, "").unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while w.blocked().seen.is_empty() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let b = w.blocked();
        assert_eq!(b.seen.len(), 1, "{b:?}");
        assert_eq!((b.seen[0].op.as_str(), b.seen[0].target.as_str()), ("file-write*", "/w/p/build.rs"));
        let pid = w.pid().unwrap();
        w.finish();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // SAFETY: signal 0 only asks.
        while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // SAFETY: as above.
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0, "the stream stopped with it");
        std::fs::remove_dir_all(&dir).ok();
    }
}
