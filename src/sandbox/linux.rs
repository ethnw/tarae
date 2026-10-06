//! The sandbox on Linux: the `Policy` macOS's profile is made from, enforced by Landlock (files —
//! `landlock.rs`) and seccomp (sockets — `seccomp.rs`) on the short-lived thread that starts the run
//! (`spawn`). What differs is only how.
//!
//! Landlock grants; Seatbelt denies after it allows. So what the profile keeps inside a place it allows is a
//! hole here, and the place is granted entry by entry around it (`grant`) — its own entries then can't be
//! made, removed or renamed (the user's decision), but a folder there may go once emptied (`cargo clean`;
//! the user's decision A, 2026-10-05 — new files there would take write rights over the protected ones,
//! and making folders there would let `.git` be renamed away: measured, `what_a_folder_holding_a_protected_
//! name_may_be_given`): reads everywhere but home (home a hole in `/`), in home
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
/// (2). At 2 the sandbox goes as far as it can and every run asks; at 1 there's none (the user's decisions,
/// 2026-10-05).
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
pub(super) const DEVICES: [(&str, u64); 7] = [
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
/// why) at 2, none at 1 (saying why) or without Landlock.
pub fn availability(abi: io::Result<u32>, seccomp: bool) -> Availability {
    if !seccomp {
        return Availability::Missing("tarae has no seccomp filter for this architecture".into());
    }
    match abi {
        Ok(n) if n >= FULL => Availability::Ready,
        Ok(n) if n < 2 => Availability::Missing(format!(
            "this kernel's Landlock (ABI {n}) refuses every move across folders, so tarae doesn't use it"
        )),
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
/// thread that starts each run (`spawn`). `audit`: the denials come from Landlock's audit log, read where
/// `audit_readable` saw it work here; else why not, and they come from the run's own error lines. `serve`: what a run
/// reaches by address is tarae's to decide (`supervisor.rs`) — or why it can't be here, and every socket but
/// netlink is refused. `made`: what was made first, beside protected names, for the run to write in. `closed`:
/// the project's folders a run can't make a file in, nor remove or rename one (a folder in them it may remove,
/// once emptied) — each holds a protected name, or one
/// below it (`grant`).
#[derive(Clone)]
pub struct Confine {
    pub abi: u32,
    pub audit: Result<AuditSource, String>,
    pub serve: Result<(), String>,
    pub made: Arc<Made>,
    pub closed: Vec<PathBuf>,
    /// The fileless folders under protected ones, made again after a run (`Kept`).
    pub kept: Arc<Kept>,
    rules: Arc<Ruleset>,
}

impl Confine {
    /// Tests: the rules made — (path, the rights granted) — in the order added.
    #[cfg(test)]
    pub fn granted(&self) -> &[(PathBuf, u64)] {
        &self.rules.added
    }
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
            && Arc::ptr_eq(&self.kept, &other.kept)
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
        crate::event::thread("tarae-tidy", move || tidy(&paths));
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if !self.paths.is_empty() && !self.tidied.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let paths = std::mem::take(&mut self.paths);
            crate::event::thread("tarae-tidy", move || tidy(&paths));
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

/// Folders a run can remove where it mustn't: the `REMOVE_DIR` a folder holding a protected name gets
/// (`confine`) reaches beneath it, into the protected folders too — a rule can't leave one out. Measured
/// (`empty_folders_inside_git_stay_after_a_run`): with its refs packed, `.git/refs/heads` and `refs/tags`
/// are empty, a run removed them and `refs/`, and git took `.git` for no repository (its search went on
/// upward). So every folder under a protected folder that holds no file — nothing, or only such folders —
/// is noted before a run with its mode (`of`), and made again once the run is over (`restore`, on the run's
/// own thread before its end is told — `sandbox::after_run`); the report says which (`kept_said`). During
/// the run itself they can be gone.
pub struct Kept {
    /// Parents before their folders.
    dirs: Vec<(PathBuf, u32)>,
    restored: std::sync::Mutex<Vec<PathBuf>>,
}

/// Entries a `Kept` walk reads at most (a huge `llm.sandbox-protect` folder): past it, the rest isn't noted.
const KEPT_WALK_MAX: usize = 200_000;

impl Kept {
    pub fn none() -> Kept {
        Kept { dirs: Vec::new(), restored: Default::default() }
    }

    /// The fileless folders at and under each folder of `found` (`protected_there`). Links aren't followed.
    /// Blocks (a worker's).
    pub fn of(found: &[PathBuf]) -> Kept {
        use std::os::unix::fs::PermissionsExt;
        // (whether `dir` holds no file; its fileless folders, parents first, pushed to `out`)
        fn walk(dir: &Path, mode: u32, budget: &mut usize, out: &mut Vec<(PathBuf, u32)>) -> bool {
            let Ok(entries) = std::fs::read_dir(dir) else { return false };
            let (mut fileless, mut below) = (true, Vec::new());
            for e in entries.flatten() {
                if *budget == 0 {
                    return false;
                }
                *budget -= 1;
                match e.file_type() {
                    Ok(t) if t.is_dir() => {
                        let mode = e.metadata().map(|m| m.permissions().mode()).unwrap_or(0o755);
                        fileless &= walk(&e.path(), mode, budget, &mut below);
                    }
                    _ => fileless = false,
                }
            }
            if fileless {
                out.push((dir.to_path_buf(), mode));
            }
            out.append(&mut below);
            fileless
        }
        let mut dirs = Vec::new();
        let mut budget = KEPT_WALK_MAX;
        for f in found {
            let Ok(m) = f.symlink_metadata() else { continue };
            if m.is_dir() {
                walk(f, m.permissions().mode(), &mut budget, &mut dirs);
            }
        }
        // (in order, not the file system's listing order — it differs between file systems, and the report
        // names them in this order; a parent still comes before its folders)
        dirs.sort();
        Kept { dirs, restored: Default::default() }
    }

    /// The noted folders a run removed, made again (parents first, each through its folder opened through no
    /// link, with its mode) — and noted as restored. Blocks: the run's own thread, once it's over.
    pub fn restore(&self) {
        let mut restored = Vec::new();
        for (path, mode) in &self.dirs {
            if !path.symlink_metadata().is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
                continue;
            }
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { continue };
            let Ok(dir) = landlock::open_unlinked(parent, libc::O_PATH | libc::O_DIRECTORY) else { continue };
            let Ok(c) = std::ffi::CString::new(name.as_encoded_bytes()) else { continue };
            let fd = std::os::fd::AsRawFd::as_raw_fd(&dir);
            // SAFETY: a folder descriptor of ours and a name, live for the calls.
            let made = unsafe {
                libc::mkdirat(fd, c.as_ptr(), 0o700) == 0
                    && libc::fchmodat(fd, c.as_ptr(), (*mode & 0o7777) as libc::mode_t, 0) == 0
            };
            if made {
                restored.push(path.clone());
            }
        }
        self.restored.lock().unwrap_or_else(|e| e.into_inner()).extend(restored);
    }

    /// What `restore` made again.
    pub fn restored(&self) -> Vec<PathBuf> {
        self.restored.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What the report says of folders made again after the run (`Kept`) — the outermost of each — or nothing.
pub fn kept_said(kept: &Kept, root: &Path) -> String {
    let restored = kept.restored();
    let top: Vec<String> = restored
        .iter()
        .filter(|p| !restored.iter().any(|q| q != *p && p.starts_with(q)))
        .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
        .collect();
    match top.is_empty() {
        true => String::new(),
        false => format!(
            " It removed empty folders inside protected ones — {} — which tarae made again once it was over: \
             they're what makes them what they are (git takes .git for a repository by its refs/).",
            top.join(", ")
        ),
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
/// can't be made, removed or renamed — `confine` lets a folder there go, once emptied). A path not there is
/// skipped; a symlink gets no rule (`allow`).
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
/// starts — not looked for below one, nor in tools' output (`outputs`, where those names are let through) —
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
            if folder && !super::outputs().iter().any(|o| e.file_name() == o.as_str()) {
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
pub fn confine(
    p: &Policy,
    abi: u32,
    audit: Result<AuditSource, String>,
    make_first: bool,
) -> io::Result<Confine> {
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
    // A folder holding a protected name: a folder in it may go once emptied (`cargo clean`, `rm -rf target` —
    // the user's decision, 2026-10-05). Nothing else: no file there is removed (no `REMOVE_FILE`), no entry
    // made (a rename there is a remove and a make — `.git` renamed away), and what's written is granted
    // entry by entry. An empty protected folder can go too: a rule can't leave a name out.
    let closed = closed_folders(&p.root, &found);
    for dir in &closed {
        match r.allow(dir, landlock::REMOVE_DIR) {
            Err(e) if !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => {
                return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display())));
            }
            _ => {}
        }
    }
    // Every other place a run writes in leaves the project's protected names out where it holds them — a
    // project in `/dev/shm` (measured: its Cargo.toml written and `.git` renamed away through that grant);
    // `p.write` drops what holds the project already (`Policy::for_project`). None there: one rule.
    let protected_in =
        |place: &Path| found.iter().filter(|f| f.starts_with(place)).cloned().collect::<Vec<_>>();
    if let Some(run) = &p.run {
        grant(&mut r, run.path(), landlock::ALL, &protected_in(run.path()))?;
    }
    for w in &p.write {
        let mut holes: Vec<PathBuf> = p.caches.iter().filter(|c| c.starts_with(w)).cloned().collect();
        holes.extend(protected_in(w));
        grant(&mut r, w, landlock::ALL, &holes)?;
    }
    for f in &p.files {
        grant(&mut r, f, WRITE_ONE, &protected_in(f))?;
    }
    for (d, access) in DEVICES {
        grant(&mut r, Path::new(d), access, &protected_in(Path::new(d)))?;
    }
    let serve = match super::supervisor::unavailable() {
        None => Ok(()),
        Some(why) => Err(why),
    };
    // (`REMOVE_DIR` reaches into the protected folders too: what it could take there is noted, made again
    // after the run — `Kept`)
    let kept = match closed.is_empty() {
        true => Kept::none(),
        false => Kept::of(&found),
    };
    Ok(Confine { abi, audit, serve, made: Arc::new(made), closed, kept: Arc::new(kept), rules: Arc::new(r) })
}

/// `start` (a spawn) on a short-lived thread confined to `c` and under the seccomp filter — what it starts is
/// in the sandbox, the caller isn't (`landlock::confined`). `serve`: the run's policy, when tarae decides
/// what it reaches by address (`supervisor::serve`, started from here: an unconfined thread) — else the
/// plain filter. `owner` hears the thread before (a watch reading Landlock's audit log tells its run's domain
/// by it).
pub fn confined<T: Send>(
    c: &Confine,
    serve: Option<Arc<Policy>>,
    owner: impl FnOnce(Owner) + Send,
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
            owner(Owner::this_thread());
            start()
        },
    )
}

/// The thread that confined a run, as Landlock's domain record names it: tarae's process (`pid=` is the
/// process's id, not the thread's) and the thread's own name (`comm=`, one per confining thread —
/// `landlock::thread_name`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    pub comm: String,
}

impl Owner {
    /// The calling thread.
    pub fn this_thread() -> Owner {
        Owner { pid: std::process::id(), comm: landlock::comm().unwrap_or_default() }
    }
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
    /// Where it is, in a few words, for a run's report.
    pub fn said(&self) -> &'static str {
        match self {
            AuditSource::Journal => "the journal",
            AuditSource::Auditd(_) => "auditd's log",
        }
    }

    /// The command that prints its records as they come, from now — where its end is as this is called, not
    /// when the command gets to open it: a domain's record is written once, with its first denial, and a
    /// run (or the probe's thread) can deny before its reader has started (the user's VM: the probe missed
    /// its own). The journal from this second on (a record of the second before can come too — told apart
    /// by its domain's record like any other); auditd's log from its length now (`tail -c +N -F`, which
    /// follows it through a rotation too).
    pub fn stream(&self) -> std::process::Command {
        match self {
            AuditSource::Journal => {
                let mut c = std::process::Command::new("journalctl");
                let now =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                c.args(["-f", "-n", "all", "-q", "-o", "cat", "--no-pager"])
                    .arg(format!("--since=@{}", now.as_secs()))
                    .args(["_TRANSPORT=kernel", "_TRANSPORT=audit"]);
                c
            }
            AuditSource::Auditd(log) => {
                let len = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
                let mut c = std::process::Command::new("tail");
                c.args(["-c", &format!("+{}", len + 1), "-F"]).arg(log);
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

/// Where a run's denials come from Landlock's audit records here — Err why not (said with the result): below
/// ABI 7 none are made; they must be readable where they go (`audit_source`: auditd's log when it runs, else
/// the journal); and the kernel's audit must be on (`auditd`, or `audit=1` at boot) — off, as on a stock
/// Ubuntu 24.04, Landlock emits nothing, which no setting says: the result said "blocked nothing" for two real
/// denials (the user, kernel 7.0). So it's tried for real there (`audit_probe`). Blocks up to `AUDIT_PROBE`
/// (a worker's), once.
pub fn audit_readable(abi: u32) -> Result<AuditSource, String> {
    if abi < 7 {
        return Err(format!("this kernel's Landlock (ABI {abi}) doesn't log what it refuses"));
    }
    static FOUND: std::sync::OnceLock<Result<AuditSource, String>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let journal = std::process::Command::new("journalctl")
                .args(["-k", "-n", "1", "-q", "-o", "cat", "--no-pager"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
            let source = audit_source(auditd_log(), journal)?;
            audit_probe(abi, |_, _| source.stream(), AUDIT_PROBE).map(|()| source)
        })
        .clone()
}

/// How long `audit_probe` waits for its record.
const AUDIT_PROBE: std::time::Duration = std::time::Duration::from_millis(500);

/// A denial of a throwaway thread's own — named as a run's confining thread is (`landlock::thread_name`),
/// confined to nothing, it tries to make a file in a temp folder of the probe's, again every 100 ms — and its
/// record looked for in `stream` (made with that thread's process and name, and that folder: a refused new
/// file's record names its folder, measured) for up to `wait`. The reader is asked for before the thread
/// starts: its domain's record comes once, with its first denial (the user's VM: a probe whose thread denied
/// first missed it). Ok: it came, and `Audit` tells it as that thread's run's; Err why not.
fn audit_probe(
    abi: u32,
    stream: impl FnOnce(&Owner, &Path) -> std::process::Command,
    wait: std::time::Duration,
) -> Result<(), String> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let folder = std::env::temp_dir().join(format!("tarae-audit-probe-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let owner = Owner { pid: std::process::id(), comm: landlock::thread_name() };
    // Its records' reader first, from now: the thread's domain record comes once, with its first denial
    let mut cmd = stream(&owner, &folder);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let spawned = cmd.spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&folder);
            return Err(format!("tarae can't read the kernel's log here ({e})"));
        }
    };
    // The thread that's denied: confined to nothing (it ends with the probe)
    let ((ready, confined), (stop, stopped)) = (channel(), channel::<()>());
    let tried = folder.join("x");
    let spawned = std::thread::Builder::new().name(owner.comm.clone()).spawn(move || {
        if Ruleset::new(abi).and_then(|r| r.apply()).is_err() {
            return;
        }
        let _ = ready.send(Owner::this_thread());
        loop {
            let _ = std::fs::OpenOptions::new().write(true).create_new(true).open(&tried);
            if stopped.recv_timeout(std::time::Duration::from_millis(100)) != Err(RecvTimeoutError::Timeout) {
                return;
            }
        }
    });
    let confined = spawned.ok().and_then(|_| confined.recv_timeout(std::time::Duration::from_secs(1)).ok());
    if confined.as_ref() != Some(&owner) {
        let _ = child.kill();
        crate::event::thread("tarae-reap", move || child.wait());
        let _ = std::fs::remove_dir_all(&folder);
        return Err(match confined {
            None => "Landlock confined no thread".into(),
            Some(o) => {
                format!("the probe's thread is named {:?} to the kernel, not {:?}", o.comm, owner.comm)
            }
        });
    }
    let (lines_tx, lines) = channel::<String>();
    let out = child.stdout.take();
    crate::event::thread("tarae-audit", move || {
        let Some(out) = out else { return };
        for l in io::BufRead::lines(io::BufReader::new(out)).map_while(Result::ok) {
            if lines_tx.send(l).is_err() {
                return;
            }
        }
    });
    let (mut audit, mut seen, mut told) = (Audit { owner: Some(owner), ..Default::default() }, false, false);
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
    crate::event::thread("tarae-reap", move || child.wait());
    let _ = std::fs::remove_dir_all(&folder);
    match (told, seen) {
        (true, _) => Ok(()),
        (false, true) => {
            Err("Landlock's audit records here don't name the thread that confined a run".into())
        }
        (false, false) => Err("kernel audit is off (no auditd or audit=1)".into()),
    }
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
/// the thread that confined it (`owner`: tarae's process and the thread's name); access records of a domain
/// not told yet wait for it.
#[derive(Debug, Default)]
pub struct Audit {
    pub owner: Option<Owner>,
    /// The threads that confined the project's earlier runs (`also`): what those left running is this run's to
    /// tell now (`sandbox::tell`).
    also: Vec<Owner>,
    ours: Vec<String>,
    others: Vec<String>,
    waiting: Vec<(String, Denial)>,
}

/// Access records kept waiting for their domain's record, at most.
const AUDIT_WAITING: usize = 64;

/// The domains told so far, by the thread that confined each (a domain's record comes once, with its first
/// denial — a later run's watch knows an earlier run's domain from here). `DOMAINS_KEPT` at most.
static DOMAINS: std::sync::Mutex<Vec<(Owner, String)>> = std::sync::Mutex::new(Vec::new());
const DOMAINS_KEPT: usize = 256;

impl Audit {
    /// The project's earlier runs are `owners` too: their domains known so far are this run's from now.
    pub fn also(&mut self, owners: Vec<Owner>) {
        if owners == self.also {
            return;
        }
        let known = DOMAINS.lock().unwrap_or_else(|e| e.into_inner());
        for (o, d) in known.iter() {
            if owners.contains(o) && !self.ours.contains(d) {
                self.ours.push(d.clone());
                self.others.retain(|x| x != d);
            }
        }
        self.also = owners;
    }

    /// The denials `line` adds: an access record of the run's domain (`LANDLOCK_ACCESS`: `domain=… blockers=…
    /// path=…`), or those waiting for a domain record that turned out the run's (`LANDLOCK_DOMAIN`:
    /// `domain=… status=allocated … pid=… comm=…`). Said in the profile's words (`file-write*`,
    /// `file-read-data`).
    pub fn line(&mut self, line: &str) -> Vec<Denial> {
        let Some(domain) = audit_value(line, "domain") else { return Vec::new() };
        if let Some(blockers) = audit_value(line, "blockers") {
            let target = audit_value(line, "path").unwrap_or_default();
            let d =
                Denial { process: super::SEEN_IN_AUDIT_LOG.into(), op: audit_op(&blockers).into(), target };
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
        let comm = audit_value(line, "comm");
        let named =
            |o: &&Owner| !o.comm.is_empty() && pid == Some(o.pid) && comm.as_deref() == Some(o.comm.as_str());
        let mine = self.owner.iter().chain(&self.also).find(named).cloned();
        let (keep, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.waiting).into_iter().partition(|(d, _)| *d == domain);
        self.waiting = rest;
        if let Some(o) = &mine {
            let mut known = DOMAINS.lock().unwrap_or_else(|e| e.into_inner());
            if !known.iter().any(|(_, d)| *d == domain) {
                known.push((o.clone(), domain.clone()));
                let n = known.len();
                known.drain(..n.saturating_sub(DOMAINS_KEPT));
            }
        }
        match mine.is_some() {
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

    /// What a run gets of this process's environment with the default settings — as a real policy's
    /// (`sandbox::env_in`); a policy made by hand gets nothing of it.
    fn run_env_here() -> crate::sandbox::runenv::RunEnv {
        crate::sandbox::run_env_of(&crate::sandbox::Here::now(), &crate::llm::LlmConfig::default())
    }

    /// The kernel's ABI, or None after saying the test skips itself (no Landlock here, or ABI 1 — tarae
    /// doesn't use it).
    fn kernel_abi(test: &str) -> Option<u32> {
        match landlock::abi() {
            Ok(n) if n >= 2 => Some(n),
            got => {
                let line = format!(
                    "{}sandbox::linux::tests::{test} — needs Landlock ABI 2, this kernel: {got:?}\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                None
            }
        }
    }

    /// The tiers (the user's, 2026-10-05): full from ABI 3; partial (saying why) at 2; none at 1 — saying why
    /// — as without Landlock or a seccomp filter.
    #[test]
    fn availability_follows_the_kernels_abi() {
        let err = |n| Err(io::Error::from_raw_os_error(n));
        assert_eq!(availability(Ok(3), true), Availability::Ready);
        assert_eq!(availability(Ok(8), true), Availability::Ready);
        match availability(Ok(2), true) {
            Availability::Partial(why) => assert!(why.contains("can't control truncating files"), "{why}"),
            other => panic!("ABI 2: {other:?}"),
        }
        let missing = |a: io::Result<u32>, seccomp| match availability(a, seccomp) {
            Availability::Missing(why) => why,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            missing(Ok(1), true),
            "this kernel's Landlock (ABI 1) refuses every move across folders, so tarae doesn't use it"
        );
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

    /// What rights on a folder holding a protected name give, the kernel asked (its entries granted one by
    /// one as `grant` does, the protected ones not; the folder's own rule varied): with none (today) a run
    /// rewrites what's there and nothing else; `REMOVE_DIR` lets it remove a folder it emptied (`rm -rf
    /// target`, `cargo clean`) — no protected one, nor any rename; `MAKE_REG | MAKE_DIR` besides lets it
    /// make entries it then can't write (write rights go by the folder, Cargo.toml's too) — and rename a
    /// protected folder away (a rename in one folder is a remove and a make there).
    #[test]
    fn what_a_folder_holding_a_protected_name_may_be_given() {
        let Some(abi) = kernel_abi("what_a_folder_holding_a_protected_name_may_be_given") else { return };
        let base = std::env::temp_dir().join(format!("tarae-own-rights-{}", std::process::id()));
        // Each try in a fresh project: (what, done)
        let tries = |own: u64| -> Vec<(&'static str, bool)> {
            let f = base.join(format!("p{own}"));
            let _ = std::fs::remove_dir_all(&f);
            for (p, text) in [("Cargo.toml", "kept\n"), (".git/HEAD", "ref\n"), ("target/debug/x", "o\n")] {
                std::fs::create_dir_all(f.join(p).parent().unwrap()).unwrap();
                std::fs::write(f.join(p), text).unwrap();
            }
            std::fs::create_dir_all(f.join(".github")).unwrap();
            let mut r = Ruleset::new(abi).unwrap();
            grant(&mut r, Path::new("/"), READ, &[]).unwrap();
            let holes = [f.join("Cargo.toml"), f.join(".git"), f.join(".github")];
            grant(&mut r, &f, landlock::ALL, &holes).unwrap();
            if own != 0 {
                r.allow(&f, own).unwrap();
            }
            let f2 = f.clone();
            let done = landlock::confined(
                &r,
                || Ok(()),
                |()| {
                    let ok = |r: io::Result<()>| r.is_ok();
                    Ok(vec![
                        ("remove target/ whole", ok(std::fs::remove_dir_all(f2.join("target")))),
                        ("write a new file", ok(std::fs::write(f2.join("new.txt"), "x"))),
                        ("make dist/ and write in it", {
                            ok(std::fs::create_dir(f2.join("dist"))
                                .and_then(|_| std::fs::write(f2.join("dist/a"), "x")))
                        }),
                        ("write Cargo.toml", ok(std::fs::write(f2.join("Cargo.toml"), "changed"))),
                        ("remove Cargo.toml", ok(std::fs::remove_file(f2.join("Cargo.toml")))),
                        ("rename .git away", ok(std::fs::rename(f2.join(".git"), f2.join("git-old")))),
                        ("remove .git whole", ok(std::fs::remove_dir_all(f2.join(".git")))),
                        ("remove the empty .github", ok(std::fs::remove_dir(f2.join(".github")))),
                    ])
                },
            )
            .unwrap();
            assert_eq!(std::fs::read_to_string(f.join("Cargo.toml")).unwrap(), "kept\n", "{own:#x}");
            done
        };
        let only = |done: Vec<(&'static str, bool)>| {
            done.into_iter().filter(|d| d.1).map(|d| d.0).collect::<Vec<_>>()
        };
        assert_eq!(only(tries(0)), Vec::<&str>::new(), "today");
        assert_eq!(
            only(tries(landlock::REMOVE_DIR)),
            ["remove target/ whole", "remove the empty .github"],
            "A"
        );
        let b = landlock::REMOVE_DIR | landlock::MAKE_REG | landlock::MAKE_DIR;
        assert_eq!(
            only(tries(b)),
            ["remove target/ whole", "rename .git away", "remove the empty .github"],
            "A + B"
        );
        std::fs::remove_dir_all(&base).ok();
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
                // (what a run gets of this process's environment, as `env_in` gives it)
                run_env: run_env_here(),
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

    /// For real (the user's decision A, 2026-10-05): in a folder holding a protected name a run may remove a
    /// folder it emptied — `rm -rf target`, and cargo's own `cargo clean` — but `Cargo.toml`, `package.json`
    /// and `.git` stay as they are: not written, truncated, removed, renamed over or away; no new file there.
    #[test]
    fn a_run_removes_folders_beside_protected_names_and_nothing_protected() {
        let Some(_) = kernel_abi("a_run_removes_folders_beside_protected_names_and_nothing_protected") else {
            return;
        };
        let s = Scratch::new("remove-dir");
        // (a workspace of its own: under tarae's tree, cargo would take tarae's for its)
        let manifest = "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";
        std::fs::write(s.root.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(s.root.join("package.json"), "{}\n").unwrap();
        for f in ["target/debug/x", "sub/target/y", "build/z"] {
            std::fs::create_dir_all(s.root.join(f).parent().unwrap()).unwrap();
            std::fs::write(s.root.join(f), "o").unwrap();
        }
        let p = s.policy();
        let script = r#"
            t() { if ( eval "$2" ) >/dev/null 2>&1; then echo "$1 ok"; else echo "$1 no"; fi; }
            t rm-target 'rm -rf "$R/target"'
            t rm-sub-target 'rm -rf "$R/sub/target"'
            t cargo-clean 'mkdir -p "$R/build/t" && CARGO_TARGET_DIR="$R/build" cargo clean --offline'
            t write-manifest 'printf x >> "$R/Cargo.toml"'
            t truncate-manifest 'truncate -s 0 "$R/Cargo.toml"'
            t remove-manifest 'rm -f "$R/Cargo.toml"'
            t rename-over-manifest 'printf "[x]\n" > "$R/src/c" && mv -f "$R/src/c" "$R/Cargo.toml"'
            t write-package-json 'printf x >> "$R/package.json"'
            t remove-package-json 'rm -f "$R/package.json"'
            t rename-git-away 'mv "$R/.git" "$R/git-old"'
            t remove-git 'rm -rf "$R/.git"'
            t new-file-at-root 'printf x > "$R/new.txt"'
            t new-build-rs-at-root 'printf "fn main() {}\n" > "$R/build.rs"'
        "#;
        let mut cmd = crate::sandbox::command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.env("R", &s.root).env_remove("CARGO_TARGET_DIR");
        let spawned = crate::sandbox::spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        let got: Vec<&str> = out.lines().collect();
        let cargo = match std::process::Command::new("cargo").arg("--version").output() {
            Ok(o) if o.status.success() => "cargo-clean ok",
            _ => "cargo-clean no",
        };
        let want = [
            "rm-target ok",
            "rm-sub-target ok",
            cargo,
            "write-manifest no",
            "truncate-manifest no",
            "remove-manifest no",
            "rename-over-manifest no",
            "write-package-json no",
            "remove-package-json no",
            "rename-git-away no",
            "remove-git no",
            "new-file-at-root no",
            "new-build-rs-at-root no",
        ];
        assert_eq!(got, want);
        assert!(!s.root.join("target").exists() && !s.root.join("build").exists(), "removed");
        assert_eq!(std::fs::read_to_string(s.root.join("Cargo.toml")).unwrap(), manifest);
        assert_eq!(std::fs::read_to_string(s.root.join("package.json")).unwrap(), "{}\n");
        assert!(s.root.join(".git/HEAD").exists() && !s.root.join("build.rs").exists());
    }

    /// For real: a project inside a place a run may write anything in keeps its protected names, as on macOS
    /// — the temp folder, a place `llm.sandbox-write` opens, `/dev/shm` (a device folder runs write in). Its
    /// own files are written; `Cargo.toml` and `.git` aren't, however they're tried.
    #[test]
    fn a_project_inside_a_writable_place_keeps_its_protected_names() {
        let Some(_) = kernel_abi("a_project_inside_a_writable_place_keeps_its_protected_names") else {
            return;
        };
        let pid = std::process::id();
        let under_target = std::env::current_dir().unwrap().join(format!("target/tarae-in-write-{pid}"));
        let places = [
            ("temp", std::env::temp_dir().join(format!("tarae-in-tmp-{pid}"))),
            ("sandbox-write", under_target),
            ("dev-shm", PathBuf::from(format!("/dev/shm/tarae-in-shm-{pid}"))),
        ];
        let script = r#"
            t() { if ( eval "$2" ) >/dev/null 2>&1; then echo "$1 ok"; else echo "$1 no"; fi; }
            t own-file 'printf x > "$R/src/new.rs"'
            t write-manifest 'printf x >> "$R/Cargo.toml"'
            t truncate-manifest 'truncate -s 0 "$R/Cargo.toml"'
            t remove-manifest 'rm -f "$R/Cargo.toml"'
            t write-git 'printf x >> "$R/.git/HEAD"'
            t rename-git-away 'mv "$R/.git" "$R/git-old"'
        "#;
        let want = [
            "own-file ok",
            "write-manifest no",
            "truncate-manifest no",
            "remove-manifest no",
            "write-git no",
            "rename-git-away no",
        ];
        for (name, base) in places {
            if !base.parent().is_some_and(Path::is_dir) {
                continue;
            }
            let _ = std::fs::remove_dir_all(&base);
            for (f, text) in
                [("proj/Cargo.toml", "kept\n"), ("proj/.git/HEAD", "ref\n"), ("proj/src/main.rs", "x")]
            {
                std::fs::create_dir_all(base.join(f).parent().unwrap()).unwrap();
                std::fs::write(base.join(f), text).unwrap();
            }
            let base = std::fs::canonicalize(&base).unwrap();
            let root = base.join("proj");
            let env = Env {
                root: root.clone(),
                tmp: Some(crate::sandbox::profile_path(&std::env::temp_dir())),
                write: if name == "sandbox-write" { vec![base.display().to_string()] } else { Vec::new() },
                run_env: run_env_here(),
                ..Default::default()
            };
            let p = crate::sandbox::confined(Policy { run: RunDir::new().ok(), ..Policy::for_project(&env) });
            let mut cmd = crate::sandbox::command(Some(&p), "sh", &["-c".into(), script.into()]);
            cmd.env("R", &root);
            let spawned = crate::sandbox::spawn(Some(&p), &cmd).unwrap();
            let mut out = String::new();
            io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
            crate::spawn::wait(spawned.pid).unwrap();
            let got: Vec<&str> = out.lines().collect();
            let kept = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
            let _ = std::fs::remove_dir_all(&base);
            assert_eq!(got, want, "a project in {name}");
            assert_eq!(kept, "kept\n", "a project in {name}");
        }
    }

    /// For real: `REMOVE_DIR` on the project's folders reaches beneath them, into the protected folders too
    /// (a rule can't leave one out) — in a repository whose refs are all packed, `refs/heads`, `refs/tags`,
    /// then `refs/` are empty, and without `refs/` git no longer takes `.git` for a repository. A run's
    /// `rmdir` of those (and of `objects/info`, `branches`) leaves them as they were once it's over, and
    /// git still finds its repository there. (git's search upward stops at the test's own folder.)
    #[test]
    fn empty_folders_inside_git_stay_after_a_run() {
        let Some(_) = kernel_abi("empty_folders_inside_git_stay_after_a_run") else { return };
        let s = Scratch::new("git-empty");
        std::fs::remove_dir_all(s.root.join(".git")).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&s.root)
                .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
                .args(["-c", "user.email=t@example.invalid", "-c", "user.name=t"])
                .args(args)
                .env("GIT_CEILING_DIRECTORIES", &s.base)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        std::fs::create_dir_all(s.root.join(".git/branches")).unwrap();
        assert!(git(&["add", "-A"]).status.success());
        assert!(git(&["commit", "-q", "-m", "base"]).status.success());
        assert!(git(&["tag", "t1"]).status.success());
        assert!(git(&["pack-refs", "--all"]).status.success());
        let empty = ["refs/heads", "refs/tags", "objects/info", "branches"];
        for d in empty {
            let n = std::fs::read_dir(s.root.join(".git").join(d)).unwrap().count();
            assert_eq!(n, 0, ".git/{d} is empty once the refs are packed");
        }
        let p = s.policy();
        let script = r#"
            for d in refs/heads refs/tags refs objects/info branches; do
                if rmdir "$R/.git/$d" 2>/dev/null; then echo "$d removed"; else echo "$d kept"; fi
            done
        "#;
        let mut cmd = crate::sandbox::command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.env("R", &s.root);
        let after_run = crate::sandbox::after_run(Some(&p));
        let spawned = crate::sandbox::spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        // (as a run's own thread does once it's reaped — `task.rs`, `testing.rs`)
        after_run();
        // The run removed them all (`REMOVE_DIR` reaches there) …
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "refs/heads removed",
                "refs/tags removed",
                "refs removed",
                "objects/info removed",
                "branches removed"
            ]
        );
        // … and the report says what was made again — in order, whatever order the file system lists them in
        // (the VMs' ext4 lists them otherwise than colima's overlay: the report's words changed with it)
        let r = crate::sandbox::Guard::In(Box::new(p.clone())).report(&[]);
        let r = r.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            r.contains("It removed empty folders inside protected ones — .git/branches, .git/objects/info, .git/refs — which tarae made again"),
            "{r}"
        );
        let gone: Vec<&str> = ["refs/heads", "refs/tags", "refs", "objects/info", "branches"]
            .into_iter()
            .filter(|d| !s.root.join(".git").join(d).is_dir())
            .collect();
        let found = git(&["rev-parse", "--git-dir"]);
        assert!(found.status.success(), "git: {} (gone: {gone:?})", String::from_utf8_lossy(&found.stderr));
        assert_eq!(gone, Vec::<&str>::new(), "folders inside .git gone after the run");
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
        // (outside home, a file of the test's own — not a system file: GCP's Ubuntu image has no /etc/hostname)
        std::fs::write(s.base.join("readable"), "x").unwrap();
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
            r home-ssh "$H/.ssh/id"; r home-other "$H/notes"; r outside-home "$B/readable"
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
    /// wait; the probe's own record is the log working — its domain named as the kernel names it: tarae's
    /// process id (`pid=`, the process's, not the thread's) and the confining thread's name (`comm=`); one
    /// naming another thread of tarae's (the name every confining thread had once — the user's VM), or the
    /// thread's name in another process, can't be told as a run's.
    #[test]
    fn the_audit_log_is_tried_for_real() {
        let Some(abi) = kernel_abi("the_audit_log_is_tried_for_real") else { return };
        let sh = |script: String| {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(script);
            c
        };
        // (auditd's form, as its log has them: the new file refused named by its folder, `UID=` glued on)
        let records = |pid: u32, comm: &str, path: &Path| {
            format!(
                "echo 'type=UNKNOWN[1424] msg=audit(1:2): domain=7a status=allocated mode=enforcing pid={pid} \
                 uid=1001 exe=\"/t/tarae\" comm=\"{comm}\"UID=\"wq\"'; \
                 echo 'type=UNKNOWN[1423] msg=audit(1:2): domain=7a blockers=fs.make_reg path=\"{}\" dev=\"sda1\" \
                 ino=1'; exec sleep 5",
                path.display()
            )
        };
        // (the thread's name the probe gives: the kernel's too, or the probe says so)
        let pid = std::process::id();
        assert_eq!(pid, Owner::this_thread().pid);
        let wait = std::time::Duration::from_millis(500);
        let t = std::time::Instant::now();
        let off = audit_probe(abi, |_, _| sh("exec sleep 5".into()), wait);
        assert_eq!(off, Err("kernel audit is off (no auditd or audit=1)".into()));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}: within its wait", t.elapsed());
        assert_eq!(audit_probe(abi, |o, path| sh(records(pid, &o.comm, path)), wait), Ok(()));
        for (pid, name) in [(pid, "tarae-landlock".to_string()), (1, String::new())] {
            let other = audit_probe(
                abi,
                |o, path| sh(records(pid, if name.is_empty() { &o.comm } else { &name }, path)),
                wait,
            );
            assert!(
                other.as_ref().is_err_and(|w| w.contains("don't name the thread")),
                "{pid} {name}: {other:?}"
            );
        }
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
            (c.abi, c.audit) = (6, Ok(AuditSource::Journal));
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
        // (paths as the words show them: home spelled `~` — a checkout in home, as on the user's VM)
        let shown = |p: &Path| crate::sandbox::home_shown(&p.display().to_string());
        assert_eq!(
            said,
            [
                format!(
                    "making {} — a folder holding protected names takes no new files on Linux",
                    shown(&s.root.join("Cargo.toml.bak"))
                ),
                format!(
                    "removing {} — a folder holding protected names keeps its files on Linux",
                    shown(&lib)
                ),
                "signalling process 4242 — outside the run's own processes".to_string(),
            ]
        );
        let r = Guard::In(Box::new(p.clone())).report(&out);
        let r = r.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(r.contains("The sandbox blocked 3 things"), "{r}");
        assert!(
            r.contains("(Read from its own error lines") && !r.contains("(Read from Landlock's audit log"),
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

    /// Where a run's refusals were read is said beside them, each source in its own words: Landlock's audit
    /// log — auditd's log or the journal, wherever it was found — or the run's own error lines, and why.
    #[test]
    fn the_report_says_where_it_read_the_refusals() {
        let Some(_) = kernel_abi("the_report_says_where_it_read_the_refusals") else { return };
        let s = Scratch::new("read-from");
        let report = |audit, seen: &[Denial], out: &[(bool, String)]| {
            let mut p = s.policy();
            if let Some(Ok(c)) = &mut p.confine {
                c.audit = audit;
            }
            let w = crate::sandbox::Watch::quiet("t.");
            for d in seen {
                w.record(d.clone());
            }
            p.watch = Some(w);
            let r = Guard::In(Box::new(p)).report(out);
            r.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let kept = s.root.join("Cargo.toml").display().to_string();
        let logged = Denial { process: "the run".into(), op: "file-write*".into(), target: kept.clone() };
        let auditd = AuditSource::Auditd("/var/log/audit/audit.log".into());
        for (source, said) in [(auditd, "auditd's log"), (AuditSource::Journal, "the journal")] {
            let r = report(Ok(source), std::slice::from_ref(&logged), &[]);
            assert!(r.contains(&format!("(Read from Landlock's audit log — {said} here.)")), "{r}");
            assert!(!r.contains("error lines"), "{r}");
        }
        // From its own error lines: why — and not the log
        let out = [(true, format!("touch: cannot touch '{kept}': Permission denied"))];
        let r = report(Err("kernel audit is off (no auditd or audit=1)".into()), &[], &out);
        assert!(
            r.contains(
                "(Read from its own error lines — kernel audit is off (no auditd or audit=1), judged by the \
                 sandbox's rules.)"
            ),
            "{r}"
        );
        assert!(!r.contains("Read from Landlock's audit log"), "{r}");
    }

    /// cargo says what it couldn't do on one line and why below (`Caused by:` … `Permission denied`, the
    /// same form `cargo clean` failed in, step 7): a file of a folder holding protected names it couldn't
    /// remove is said as Landlock's own rule — not "blocked nothing".
    #[test]
    fn cargos_two_line_refusal_is_read() {
        let Some(_) = kernel_abi("cargos_two_line_refusal_is_read") else { return };
        let s = Scratch::new("cargo-said");
        let p = s.policy();
        let lock = s.root.join("Cargo.lock");
        let out = [
            (true, format!("error: failed to remove file `{}`", lock.display())),
            (true, String::new()),
            (true, "Caused by:".to_string()),
            (true, "  Permission denied (os error 13)".to_string()),
        ];
        let said: Vec<String> = p.said_in_output(&out, &[]).iter().map(Denial::plain).collect();
        let lock = crate::sandbox::home_shown(&lock.display().to_string());
        assert_eq!(
            said,
            [format!("removing {lock} — a folder holding protected names keeps its files on Linux")]
        );
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
        let domain = |d: &str, comm: &str| {
            format!(
                "audit: type=1424 audit(1729738800.268:30): domain={d} status=allocated mode=enforcing pid=300 \
                 uid=0 exe=\"/usr/bin/tarae\" comm=\"{comm}\""
            )
        };
        let mut a = Audit { owner: Some(Owner { pid: 300, comm: "tl-1".into() }), ..Default::default() };
        assert!(
            a.line(&access("1a", "fs.write_file", "\"/w/p/Cargo.toml\"")).is_empty(),
            "waits for its domain"
        );
        let d = a.line(&domain("1a", "tl-1"));
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].op.as_str(), d[0].target.as_str()), ("file-write*", "/w/p/Cargo.toml"));
        let d = a.line(&access("1a", "fs.read_file", "2F686F6D652F752F612062"));
        assert_eq!((d[0].op.as_str(), d[0].target.as_str()), ("file-read-data", "/home/u/a b"));
        assert_eq!(
            a.line(&access("1a", "scope.signal", "\"\"")).first().map(|d| d.op.clone()),
            Some("signal".into())
        );
        // Another run's domain: tarae's process, another thread's name
        assert!(a.line(&access("2b", "fs.make_reg", "\"/x\"")).is_empty());
        assert!(a.line(&domain("2b", "tl-11")).is_empty(), "not this run's");
        assert!(a.line(&access("2b", "fs.make_reg", "\"/x\"")).is_empty());
        assert!(a.line("audit: type=1400 apparmor=\"DENIED\"").is_empty());
    }

    /// What an earlier run of the project left running is denied in that run's domain: a later run's watch
    /// told the earlier ones' threads (`also`) reads it — by the domain's record when it comes now, or, its
    /// record told already (to the earlier run's watch), by the domain known from then. Another thread's
    /// domain stays out.
    #[test]
    fn what_an_earlier_run_left_is_read_by_the_current_runs_watch() {
        let access = |d: &str, path: &str| {
            format!(
                "audit: type=1423 audit(1:3): domain={d} blockers=fs.write_file path=\"{path}\" dev=\"vda2\" ino=9"
            )
        };
        let domain = |d: &str, comm: &str| {
            format!(
                "audit: type=1424 audit(1:3): domain={d} status=allocated mode=enforcing pid=301 uid=0 \
                 exe=\"/usr/bin/tarae\" comm=\"{comm}\""
            )
        };
        let o = |comm: &str| Owner { pid: 301, comm: comm.into() };
        // Run 1 (thread tl-e1) sees its domain e1 told; run 2 hears of run 1 after its record went by
        let mut first = Audit { owner: Some(o("tl-e1")), ..Default::default() };
        assert_eq!(first.line(&domain("e1", "tl-e1")).len(), 0);
        let mut now = Audit { owner: Some(o("tl-e2")), ..Default::default() };
        now.also(vec![o("tl-e1"), o("tl-e3")]);
        let d = now.line(&access("e1", "/w/p/Cargo.toml"));
        assert_eq!(
            d.iter().map(|d| d.target.as_str()).collect::<Vec<_>>(),
            ["/w/p/Cargo.toml"],
            "known from then"
        );
        // Run 3's domain first told now: its record names its thread
        assert!(now.line(&access("e3", "/w/p/build.rs")).is_empty(), "waits for its domain");
        assert_eq!(now.line(&domain("e3", "tl-e3")).len(), 1);
        // Another thread's
        assert!(now.line(&access("e9", "/x")).is_empty());
        assert!(now.line(&domain("e9", "tl-e9")).is_empty());
    }

    /// auditd's records, verbatim from its log on a kernel 7.0 (the user's VM): the domain is told by tarae's
    /// process id (`pid=`, the process's — not the confining thread's) and the thread's name (`comm=`), its
    /// enrichment glued on (`comm="…"UID="wq"`, no space); another thread of the same tarae, or the name in
    /// another process, isn't the run. A refused new file names its folder (`file-make-in`), whichever record
    /// comes first. Where they're read: auditd's log when it runs (why not, when this user can't read it),
    /// else the journal — followed from its end.
    #[test]
    fn auditds_records_are_read_where_they_go() {
        let domain = r#"type=UNKNOWN[1424] msg=audit(1791187720.409:280): domain=107163519 status=allocated mode=enforcing pid=127026 uid=1001 exe="/home/wq/tarae/target-ll/release/tarae" comm="tarae-landlock"UID="wq""#;
        let access = r#"type=UNKNOWN[1423] msg=audit(1791187720.409:280): domain=107163519 blockers=fs.make_reg path="/home/wq/proj" dev="sda1" ino=264783"#;
        let made =
            Denial { process: "the run".into(), op: "file-make-in".into(), target: "/home/wq/proj".into() };
        let owned = |pid: u32, comm: &str| Audit {
            owner: Some(Owner { pid, comm: comm.into() }),
            ..Default::default()
        };
        let mut a = owned(127026, "tarae-landlock");
        assert!(a.line(domain).is_empty());
        assert_eq!(a.line(access), std::slice::from_ref(&made));
        let mut a = owned(127026, "tarae-landlock");
        assert!(a.line(access).is_empty(), "waits for its domain");
        assert_eq!(a.line(domain), std::slice::from_ref(&made));
        for (pid, comm) in [(127026, "tl-3"), (1, "tarae-landlock")] {
            let mut a = owned(pid, comm);
            assert!(a.line(access).is_empty() && a.line(domain).is_empty(), "{pid} {comm}: not the run");
            assert!(a.line(access).is_empty(), "{pid} {comm}: nor what comes after");
        }
        let log = PathBuf::from("/var/log/audit/audit.log");
        let unreadable = "audit goes to auditd's log, which this user can't read".to_string();
        assert_eq!(audit_source(Some(Ok(log.clone())), true), Ok(AuditSource::Auditd(log.clone())));
        assert_eq!(audit_source(Some(Err(unreadable.clone())), true), Err(unreadable));
        assert_eq!(audit_source(None, true), Ok(AuditSource::Journal));
        assert!(audit_source(None, false).is_err_and(|w| w.contains("can't read the kernel's log")));
        // (followed from where it ends as the reader is asked for: auditd's log from its length then, the
        // journal from that second)
        let words = |c: &std::process::Command| {
            let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
            (c.get_program().to_string_lossy().into_owned(), args.join(" "))
        };
        let log = auditd_log("args");
        let len = std::fs::metadata(&log).unwrap().len();
        assert_eq!(
            words(&AuditSource::Auditd(log.clone()).stream()),
            ("tail".into(), format!("-c +{} -F {}", len + 1, log.display()))
        );
        std::fs::remove_dir_all(log.parent().unwrap()).ok();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let (program, args) = words(&AuditSource::Journal.stream());
        assert_eq!(program, "journalctl");
        assert!(
            args.starts_with("-f -n all ") && args.ends_with("_TRANSPORT=kernel _TRANSPORT=audit"),
            "{args}"
        );
        let since: u64 =
            args.split("--since=@").nth(1).and_then(|s| s.split(' ').next()?.parse().ok()).unwrap();
        assert!(since.abs_diff(now) <= 1, "{since} {now}");
    }

    /// A stand-in for auditd's log: a file of its own, with a line already in it.
    fn auditd_log(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tarae-auditd-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("audit.log");
        std::fs::write(&log, "type=DAEMON_START msg=audit(1791193000.000:1): op=start ver=3.1.2\n").unwrap();
        log
    }

    /// `lines` appended to `log`, as auditd writes them.
    fn append(log: &Path, lines: &[String]) {
        let mut f = std::fs::OpenOptions::new().append(true).open(log).unwrap();
        io::Write::write_all(&mut f, lines.iter().map(|l| format!("{l}\n")).collect::<String>().as_bytes())
            .unwrap();
    }

    /// The user's VM's records (verbatim but for the process, the thread's name and the path): a domain's
    /// record, one of its access records, its end.
    fn vm_records(pid: u32, comm: &str, path: &Path) -> [String; 3] {
        [
            format!(
                "type=UNKNOWN[1424] msg=audit(1791193258.514:942): domain=107163549 status=allocated \
                 mode=enforcing pid={pid} uid=1001 exe=\"/home/wq/tarae/target-ll/release/tarae\" \
                 comm=\"{comm}\"UID=\"wq\""
            ),
            format!(
                "type=UNKNOWN[1423] msg=audit(1791193258.614:943): domain=107163549 blockers=fs.make_reg \
                 path=\"{}\" dev=\"sda1\" ino=266488",
                path.display()
            ),
            "type=UNKNOWN[1424] msg=audit(1791193259.032:947): domain=107163549 status=deallocated denials=5"
                .to_string(),
        ]
    }

    /// The probe reading auditd's log as tarae does (`AuditSource::Auditd`), the kernel's records written as
    /// on the user's VM: the domain's comes once, with the first denial — which can be before the reader has
    /// the file open — then an access record at each retry. Both orders of the first two: the probe's there
    /// (domain first) and a run's (its access record first, the same instant). The reader is asked for before
    /// the thread that's denied is there. (What said "don't name the thread" on the VM, the records right.)
    #[test]
    fn the_audit_probe_reads_records_logged_before_its_reader_opens() {
        let Some(abi) = kernel_abi("the_audit_probe_reads_records_logged_before_its_reader_opens") else {
            return;
        };
        let threads = || -> Vec<String> {
            let tasks = std::fs::read_dir("/proc/self/task").unwrap().flatten();
            tasks
                .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
                .map(|c| c.trim_end().into())
                .collect()
        };
        for domain_first in [true, false] {
            let log = auditd_log(&format!("probe-{domain_first}"));
            let source = AuditSource::Auditd(log.clone());
            let found = audit_probe(
                abi,
                |o, folder| {
                    assert!(!threads().contains(&o.comm), "{} is there already: it may have denied", o.comm);
                    let stream = source.stream();
                    let [domain, access, _] = vm_records(o.pid, &o.comm, folder);
                    // (the first denial's records: before the reader starts)
                    let first = match domain_first {
                        true => [domain, access.clone()],
                        false => [access.clone(), domain],
                    };
                    append(&log, &first);
                    // (each retry's, after)
                    let (log, access) = (log.clone(), access);
                    std::thread::spawn(move || {
                        for _ in 0..4 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            append(&log, std::slice::from_ref(&access));
                        }
                    });
                    stream
                },
                std::time::Duration::from_secs(2),
            );
            assert_eq!(found, Ok(()), "domain record first: {domain_first}");
            std::fs::remove_dir_all(log.parent().unwrap()).ok();
        }
    }

    /// Whether the watch's reader (`tail -F` on auditd's log) has `log` open within `within` — the file among
    /// its descriptors.
    fn opened(w: &crate::sandbox::Watch, log: &Path, within: std::time::Duration) -> bool {
        let end = std::time::Instant::now() + within;
        while std::time::Instant::now() < end {
            let fds = w.pid().and_then(|p| std::fs::read_dir(format!("/proc/{p}/fd")).ok());
            if fds
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| std::fs::read_link(e.path()).is_ok_and(|t| t == log))
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// The run's denial, `within` — or what the watch had.
    fn the_runs(w: &crate::sandbox::Watch, within: std::time::Duration) -> Vec<Denial> {
        let end = std::time::Instant::now() + within;
        while w.blocked().seen.is_empty() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        w.blocked().seen
    }

    /// A run's watch on auditd's log, its reader open before the records come (as the user asked: lines
    /// appended after it opened), in both orders — the run's on the VM (access first) and the other.
    #[test]
    fn a_watch_reads_records_logged_after_its_reader_opened() {
        let made = Denial { process: "the run".into(), op: "file-make-in".into(), target: "/w/proj".into() };
        for domain_first in [true, false] {
            let log = std::fs::canonicalize(auditd_log(&format!("after-{domain_first}"))).unwrap();
            let w = crate::sandbox::Watch::start_audit_from("t.", AuditSource::Auditd(log.clone()).stream());
            w.owned_by(Owner { pid: 134330, comm: "tl-2".into() });
            assert!(opened(&w, &log, std::time::Duration::from_secs(5)), "tail opened the log");
            let [domain, access, end] = vm_records(134330, "tl-2", Path::new("/w/proj"));
            append(&log, &if domain_first { [domain, access, end] } else { [access, domain, end] });
            assert_eq!(
                the_runs(&w, std::time::Duration::from_secs(5)),
                std::slice::from_ref(&made),
                "{domain_first}"
            );
            w.finish();
            std::fs::remove_dir_all(log.parent().unwrap()).ok();
        }
    }

    /// A run's watch on auditd's log: what's logged after the watch was asked for is its run's, though its
    /// reader opens the file later (a run starting at once — the probe's case too).
    #[test]
    fn a_watch_reads_records_logged_before_its_reader_opens() {
        let made = Denial { process: "the run".into(), op: "file-make-in".into(), target: "/w/proj".into() };
        for domain_first in [true, false] {
            let log = std::fs::canonicalize(auditd_log(&format!("before-{domain_first}"))).unwrap();
            let stream = AuditSource::Auditd(log.clone()).stream();
            let [domain, access, end] = vm_records(134330, "tl-2", Path::new("/w/proj"));
            append(&log, &if domain_first { [domain, access, end] } else { [access, domain, end] });
            // (the same command, held until the watch knows its run — as a real run's is before it starts)
            let go = log.with_file_name("go");
            let mut gated = std::process::Command::new("sh");
            gated.args(["-c", "until [ -e \"$0\" ]; do sleep 0.01; done; exec \"$@\""]).arg(&go);
            gated.arg(stream.get_program()).args(stream.get_args());
            let w = crate::sandbox::Watch::start_audit_from("t.", gated);
            w.owned_by(Owner { pid: 134330, comm: "tl-2".into() });
            std::fs::write(&go, "").unwrap();
            assert_eq!(
                the_runs(&w, std::time::Duration::from_secs(2)),
                std::slice::from_ref(&made),
                "{domain_first}"
            );
            w.finish();
            std::fs::remove_dir_all(log.parent().unwrap()).ok();
        }
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
        // (paths as the words show them: home spelled `~` — a checkout in home, as on the user's VM)
        let shown = |p: &Path| crate::sandbox::home_shown(&p.display().to_string());
        let bak = shown(&s.root.join("Cargo.toml.bak"));
        assert_eq!(
            seen[0].plain(),
            format!("making {bak} — a folder holding protected names takes no new files on Linux")
        );
        assert_eq!(seen[0].process, "the run");
        let mut alone = vec![in_root];
        assert!(p.merge_made(&mut alone, Vec::new()).is_empty());
        let root = shown(&s.root);
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
            ("file-write*", format!("writing {}", shown(&s.base)))
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
             echo 'audit: type=1424 audit(1:2): domain=9f status=allocated mode=enforcing pid=4242 uid=1 \
             exe=\"/t/tarae\" comm=\"tl-9\"'; \
             echo 'audit: type=1423 audit(1:2): domain=9f blockers=fs.write_file path=\"/w/p/build.rs\" ino=1'; \
             exec sleep 30",
            go.display()
        ));
        let w = crate::sandbox::Watch::start_audit_from("t.", stream);
        w.owned_by(Owner { pid: 4242, comm: "tl-9".into() });
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

    /// The toolchains in the sandbox, measured (`target/sb/landlock-measured.md`, step 7): each command
    /// `TARAE_MEASURE` lists — a file, a line each: `name<TAB>project<TAB>localhost 0|1<TAB>command`, projects
    /// with warm caches (`target/sb/measure-toolchains.sh` makes them) — run as Claude's runs go: the
    /// project's own policy (`sandbox::policy`: the toolchains' folders asked, what a first build makes made
    /// first, the protected names walked), the run's environment (`sandbox::command`), its spawn
    /// (`sandbox::spawn`: Landlock, seccomp, tarae's supervisor) — and plain, `TARAE_MEASURE_N` times each (3),
    /// taking turns. Prints wall times, exits, the policy's own time and what the report says it blocked.
    /// Skips without the file.
    #[test]
    fn perf_toolchains_in_the_sandbox() {
        if crate::test_results::unoptimized("sandbox::linux::tests::perf_toolchains_in_the_sandbox") {
            return;
        }
        use std::time::{Duration, Instant};
        let Some(list) = std::env::var_os("TARAE_MEASURE") else {
            let line = format!(
                "{}sandbox::linux::tests::perf_toolchains_in_the_sandbox — no TARAE_MEASURE list\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        };
        let n: usize = std::env::var("TARAE_MEASURE_N").ok().and_then(|n| n.parse().ok()).unwrap_or(3);
        // (the test binary as tarae is: what a run leaves running stays its — `reaper`; the list is measured
        // alone, with `--exact`: that's the whole process's)
        crate::reaper::at_start();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let list = std::fs::read_to_string(list).unwrap();
        let mut out = vec![
            "| command | plain (ms, median) | sandboxed (ms, median) | + | policy (ms) | exits plain / sandboxed | \
             the report |"
                .to_string(),
            "|---|---|---|---|---|---|---|".to_string(),
        ];
        for line in list.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
            let [name, dir, localhost, cmd] = line.splitn(4, '\t').collect::<Vec<_>>()[..] else {
                panic!("not name<TAB>project<TAB>0|1<TAB>command: {line:?}")
            };
            let root = std::fs::canonicalize(dir).unwrap();
            let llm = crate::llm::LlmConfig { sandbox_localhost: localhost == "1", ..Default::default() };
            // One run: (wall ms, exit code, policy ms, the report's first line — sandboxed only)
            let run = |sandboxed: bool| {
                let t = Instant::now();
                let p = sandboxed.then(|| {
                    let mut p = crate::sandbox::policy(&root, &llm, &[]);
                    p.watch = crate::sandbox::watch_for(&p);
                    p
                });
                let policy_ms = ms(t.elapsed());
                let mut c = crate::sandbox::command(p.as_ref(), "sh", &["-c".into(), cmd.to_string()]);
                c.current_dir(&root);
                let t = Instant::now();
                let s = crate::sandbox::spawn(p.as_ref(), &c).unwrap();
                if let Some(w) = p.as_ref().and_then(|p| p.watch.as_ref()) {
                    w.run_started(s.pid);
                }
                let read = |r: Option<std::io::PipeReader>, err: bool| {
                    std::thread::spawn(move || {
                        let mut text = String::new();
                        if let Some(mut r) = r {
                            let _ = io::Read::read_to_string(&mut r, &mut text);
                        }
                        text.lines().map(|l| (err, l.to_string())).collect::<Vec<_>>()
                    })
                };
                let (o, e) = (read(s.stdout, false), read(s.stderr, true));
                let status = crate::spawn::wait(s.pid).unwrap();
                let wall = ms(t.elapsed());
                let lines: Vec<(bool, String)> =
                    o.join().unwrap().into_iter().chain(e.join().unwrap()).collect();
                let report = p.map(|p| {
                    let r = crate::sandbox::Guard::In(Box::new(p)).report(&lines);
                    let first =
                        r.lines().take_while(|l| !l.starts_with("In tarae's sandbox")).collect::<Vec<_>>();
                    first.join(" / ")
                });
                if !status.success() {
                    let tail: Vec<&str> = lines.iter().rev().take(15).map(|(_, l)| l.as_str()).collect();
                    let _ = io::Write::write_all(
                        &mut io::stderr(),
                        format!(
                            "{name} ({}): exit {status}\n  {}\n",
                            if sandboxed { "sandboxed" } else { "plain" },
                            tail.into_iter().rev().collect::<Vec<_>>().join("\n  ")
                        )
                        .as_bytes(),
                    );
                }
                (wall, status.code().unwrap_or(-1), policy_ms, report)
            };
            // (said as it goes — a long list is never silent for minutes)
            let say = |s: String| {
                let _ = io::Write::write_all(&mut io::stderr(), format!("{s}\n").as_bytes());
            };
            say(format!("measuring {name} …"));
            run(false); // (warm: the first run of a command isn't counted)
            let (mut plain, mut sandboxed, mut policy) = (Vec::new(), Vec::new(), Vec::new());
            let (mut exits, mut said) = ((Vec::new(), Vec::new()), String::new());
            for i in 0..n {
                let (w, code, _, _) = run(false);
                plain.push(w);
                exits.0.push(code);
                let (ws, code, p, r) = run(true);
                sandboxed.push(ws);
                policy.push(p);
                exits.1.push(code);
                said = r.unwrap_or_default();
                say(format!("  {name} {}/{n}: plain {w:.0} ms, sandboxed {ws:.0} ms", i + 1));
            }
            let (pl, sb) = (median(plain), median(sandboxed));
            out.push(format!(
                "| `{name}` | {pl:.0} | {sb:.0} | {:+.0}% | {:.0} | {:?} / {:?} | {} |",
                (sb / pl - 1.0) * 100.0,
                median(policy),
                exits.0,
                exits.1,
                said.replace('|', "\\|")
            ));
        }
        let _ = io::Write::write_all(&mut io::stderr(), format!("{}\n", out.join("\n")).as_bytes());
        // What the runs left running (a build tool's daemon), and what it holds now — then ended, as at quit
        let rss = |pid: u32| {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            status
                .lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        for l in crate::sandbox::leftovers() {
            // (with what it started — a Gradle daemon's Kotlin daemon, its workers)
            for o in crate::procs::tree(l.pid) {
                let line =
                    format!("left running: {} (pid {}) · RSS {} · {}\n", o.name, o.pid, rss(o.pid), l.tag);
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            }
        }
        crate::sandbox::end_leftovers(None);
    }
}
