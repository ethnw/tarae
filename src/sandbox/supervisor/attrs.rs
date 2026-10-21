//! A file's mode and times, for a run (linmeta, `target/sb/linmeta-design.md`): Landlock has no right for them,
//! so the served filter hands tarae every call that changes them (`seccomp::serves_attr`), and they're let only where
//! the run may write (`Policy::writes_allow`: its write places, never a protected name) — as macOS's profile has
//! `file-write-mode` and `file-write-times`. An owner call is let only where it changes nothing — each id `-1` or
//! the file's own (`rsync -a`, `cp -a`) — and there as a mode is; made by tarae with `-1`, `-1`, so the
//! kernel's own side effects (its change time, a set-ID bit cleared) are the run's call's.
//!
//! The safe form, as the socket calls': a path is read once from the caller's memory (`wire::c_path`) and opened by
//! the kernel, never walked by tarae — an absolute one from the run's root (`/proc/<tid>/root`, `RESOLVE_IN_ROOT`),
//! a relative one from the run's own folder (its cwd, or the dirfd: tarae's copy, `pidfd_getfd`) — into an `O_PATH`
//! descriptor of tarae's, decided on what that is, and the call made on it (through `/proc/self/fd/M` for a mode:
//! Linux 5.14 has no `fchmodat2`): a link swapped in after the check reaches nothing. What the run sees at the
//! file's name (from its own root) must be that file: a key's cover in tier own is another one, a B9 seal read-only
//! (`EROFS`, as the kernel answers it there), a name gone (` (deleted)`) none. Only what builds name is served: a
//! path through `/proc` or a descriptor's name in `/dev` (`/dev/fd/N/…`, `/dev/stdout`), or a magic link on the way,
//! is refused, said, never followed for the run — but the whole path `/proc/self/fd/N`, which is its descriptor N.
//! A descriptor form is decided on what tarae's copy names, and the run's call is made again on the copy: one that
//! names no file — a pipe, a socket, a file no folder holds any more — is the run's to change, and one open for
//! writing is let wherever its name still leads the run to it (Landlock let the run write that file). tarae acts
//! with its own credentials, the run's user's.

use std::ffi::{CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::super::wire::{Times, c_path, no_name, self_fd, through_proc, times};
use super::{Answer, Caller, Listener, Served, errno, refuse, refuse_with, result};

/// What a call changes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Change {
    Mode(libc::mode_t),
    /// Access and modification times as `utimensat` takes them; None: both now.
    Times(Option<[i64; 4]>),
    /// Owner and group as the call gives them (`u32::MAX`: as it is) — let only where neither changes (`refused`).
    Owner(u32, u32),
}

impl Change {
    /// How a refusal says it (Seatbelt's words, which the report says as "changing …").
    fn op(self) -> &'static str {
        match self {
            Change::Mode(_) => "file-write-mode",
            Change::Times(_) => "file-write-times",
            Change::Owner(..) => "file-write-owner",
        }
    }
}

/// How the run's call on a descriptor of its own is made again on tarae's copy of it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Again {
    /// `fchmod(fd, mode)`.
    Fchmod,
    /// `fchmodat2(fd, "", mode, flags)` (`AT_EMPTY_PATH`).
    Fchmodat2(i32),
    /// `utimensat(fd, NULL or "", times, flags)` — `futimens` (and `futimesat(fd, NULL, …)`), or `AT_EMPTY_PATH`.
    Utimensat { empty: bool, flags: i32 },
    /// The path `/proc/self/fd/N`: the same path through tarae's own (`/proc/self/fd/M`).
    Proc { follow: bool },
    /// `fchown(fd, …)`, `fchownat(fd, "", …, AT_EMPTY_PATH)`: made as every owner call is (`chown_unchanged`).
    Owner,
}

/// Where a call's file is: a path from a folder of the caller's (`AT_FDCWD`: where it works), or a descriptor.
enum At {
    Path { dirfd: u64, path: Vec<u8>, follow: bool },
    Fd(u64, Again),
}

const NOFOLLOW: i32 = libc::AT_SYMLINK_NOFOLLOW;
const EMPTY: i32 = libc::AT_EMPTY_PATH;

/// A descriptor argument as the kernel reads it: an `int`, its low half.
fn low(v: u64) -> u64 {
    u64::from(v as u32)
}

/// Whether a folder argument is `AT_FDCWD`.
fn cwd(dirfd: u64) -> bool {
    dirfd as i32 == libc::AT_FDCWD
}

/// A call that changes a file's mode, times or owner (`seccomp::serves_attr`), answered.
pub(super) fn call(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let a = n.data.args;
    let path = |at: u64| c_path(&c, at);
    let (at, change) = match n.data.nr as libc::c_long {
        libc::SYS_fchmod => (At::Fd(low(a[0]), Again::Fchmod), Change::Mode(a[1] as libc::mode_t)),
        libc::SYS_fchmodat => {
            (At::Path { dirfd: a[0], path: path(a[1])?, follow: true }, Change::Mode(a[2] as libc::mode_t))
        }
        libc::SYS_fchmodat2 => {
            let flags = a[3] as i32;
            if flags & !(NOFOLLOW | EMPTY) != 0 {
                return Err(libc::EINVAL);
            }
            let at = with_flags(a[0], path(a[1])?, flags, Again::Fchmodat2(flags))?;
            (at, Change::Mode(a[2] as libc::mode_t))
        }
        libc::SYS_fchown => (At::Fd(low(a[0]), Again::Owner), Change::Owner(a[1] as u32, a[2] as u32)),
        libc::SYS_fchownat => {
            let flags = a[4] as i32;
            if flags & !(NOFOLLOW | EMPTY) != 0 {
                return Err(libc::EINVAL);
            }
            (with_flags(a[0], path(a[1])?, flags, Again::Owner)?, Change::Owner(a[2] as u32, a[3] as u32))
        }
        #[cfg(target_arch = "x86_64")]
        libc::SYS_chown | libc::SYS_lchown => {
            let follow = n.data.nr as libc::c_long == libc::SYS_chown;
            (
                At::Path { dirfd: libc::AT_FDCWD as u64, path: path(a[0])?, follow },
                Change::Owner(a[1] as u32, a[2] as u32),
            )
        }
        libc::SYS_utimensat => {
            let t = times(&c, a[2], Times::Timespecs)?;
            // (both times `UTIME_OMIT`: nothing to do — the kernel looks at neither the path nor the flags)
            if t.is_some_and(|t| t[1] == libc::UTIME_OMIT && t[3] == libc::UTIME_OMIT) {
                return Ok(0);
            }
            let flags = a[3] as i32;
            if flags & !(NOFOLLOW | EMPTY) != 0 {
                return Err(libc::EINVAL);
            }
            let t = Change::Times(t);
            match a[1] {
                // (a path of none: `futimens` — dfd must not be AT_FDCWD and flags must be 0)
                0 if cwd(a[0]) => return Err(libc::EFAULT),
                0 if flags != 0 => return Err(libc::EINVAL),
                0 => (At::Fd(low(a[0]), Again::Utimensat { empty: false, flags: 0 }), t),
                at => (with_flags(a[0], path(at)?, flags, Again::Utimensat { empty: true, flags })?, t),
            }
        }
        #[cfg(target_arch = "x86_64")]
        libc::SYS_chmod => (
            At::Path { dirfd: libc::AT_FDCWD as u64, path: path(a[0])?, follow: true },
            Change::Mode(a[1] as libc::mode_t),
        ),
        #[cfg(target_arch = "x86_64")]
        libc::SYS_utime => (
            At::Path { dirfd: libc::AT_FDCWD as u64, path: path(a[0])?, follow: true },
            Change::Times(times(&c, a[1], Times::Utimbuf)?),
        ),
        #[cfg(target_arch = "x86_64")]
        libc::SYS_utimes => (
            At::Path { dirfd: libc::AT_FDCWD as u64, path: path(a[0])?, follow: true },
            Change::Times(times(&c, a[1], Times::Timevals)?),
        ),
        #[cfg(target_arch = "x86_64")]
        libc::SYS_futimesat => {
            let t = Change::Times(times(&c, a[2], Times::Timevals)?);
            match a[1] {
                0 if cwd(a[0]) => return Err(libc::EFAULT),
                0 => (At::Fd(low(a[0]), Again::Utimensat { empty: false, flags: 0 }), t),
                at => (At::Path { dirfd: a[0], path: path(at)?, follow: true }, t),
            }
        }
        _ => return Err(libc::ENOSYS),
    };
    let (dirfd, path, follow) = match at {
        At::Fd(fd, again) => return on_fd(p, &c, fd, change, again),
        // (an empty path names nothing — `ENOENT`, as the kernel says it; `AT_EMPTY_PATH`'s became `.` or a descriptor)
        At::Path { path, .. } if path.is_empty() => return Err(libc::ENOENT),
        At::Path { dirfd, path, follow } => (dirfd, path, follow),
    };
    if let Some(fd) = self_fd(&path) {
        return on_fd(p, &c, fd, change, Again::Proc { follow });
    }
    let said = || String::from_utf8_lossy(&path).into_owned();
    if through_proc(&path) {
        return refuse(p, &c, change.op(), said());
    }
    match open_in_run(&c, dirfd, &path, follow, true) {
        // (a magic link on the way — the same open without the guard gets past it: refused, never followed for the
        // run; a loop of links is the kernel's own ELOOP)
        Err(libc::ELOOP) if open_in_run(&c, dirfd, &path, follow, false).err() != Some(libc::ELOOP) => {
            refuse(p, &c, change.op(), said())
        }
        file => on_path(p, &c, &file?, change),
    }
}

/// Where a call with `AT_SYMLINK_NOFOLLOW`/`AT_EMPTY_PATH` (`flags`) on `path` from `dirfd` is: an empty path with
/// `AT_EMPTY_PATH` is the folder argument itself (`AT_FDCWD`: where the caller works) — made `again` on a
/// descriptor; without it, `ENOENT`, as the kernel says it.
fn with_flags(dirfd: u64, path: Vec<u8>, flags: i32, again: Again) -> Result<At, i32> {
    let follow = flags & NOFOLLOW == 0;
    Ok(match (path.is_empty(), flags & EMPTY != 0) {
        (false, _) => At::Path { dirfd, path, follow },
        (true, false) => return Err(libc::ENOENT),
        (true, true) if cwd(dirfd) => At::Path { dirfd, path: b".".to_vec(), follow },
        (true, true) => At::Fd(low(dirfd), again),
    })
}

/// `path` from `dirfd` opened `O_PATH` by the kernel as the run would reach it: an absolute one from the run's root
/// (`/proc/<tid>/root`, `RESOLVE_IN_ROOT`); a relative one from where the run works by that folder's name from its
/// root too, while the name leads the run to the folder it works in — else, as from tarae's copy of `dirfd`
/// (`EBADF` where it has none), from the folder itself, where an absolute link met on the way resolves from tarae's
/// root, which in tier own isn't the run's: what it opens is decided only if its name leads the run to it
/// (`refused`). A magic link on the way, `guarded`: `ELOOP` (`RESOLVE_NO_MAGICLINKS`).
fn open_in_run(c: &Caller, dirfd: u64, path: &[u8], follow: bool, guarded: bool) -> Result<OwnedFd, i32> {
    let (from, resolve) = match path.first() {
        Some(b'/') => {
            let root = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
                .open(format!("/proc/{}/root", c.tid));
            (OwnedFd::from(root.map_err(|_| libc::ESRCH)?), libc::RESOLVE_IN_ROOT)
        }
        _ if cwd(dirfd) => {
            let here = c.open_cwd()?;
            let named = |w: &Path| w.is_absolute() && stat(&here).is_ok_and(|st| named_there(c, w, &st));
            if let Ok(w) = c.cwd()
                && named(&w)
            {
                let at = w.join(OsStr::from_bytes(path));
                return open_in_run(c, libc::AT_FDCWD as u64, at.as_os_str().as_bytes(), follow, guarded);
            }
            (here, 0)
        }
        _ => (c.fd(low(dirfd))?, 0),
    };
    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    // SAFETY: an all-zero `open_how` is its empty value.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW }) as u64;
    how.resolve = resolve | if guarded { libc::RESOLVE_NO_MAGICLINKS } else { 0 };
    let size = std::mem::size_of::<libc::open_how>();
    // SAFETY: a descriptor of ours, a path and an `open_how` that live through the call.
    let fd =
        unsafe { libc::syscall(libc::SYS_openat2, from.as_raw_fd(), path.as_ptr(), &raw const how, size) };
    if fd < 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: a descriptor the call just made, ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// A call on `file` (opened in the run's view, its last part's link followed or not): refused where the run may not
/// write, or where its name doesn't lead the run to it; else made by tarae on its own descriptor of it.
fn on_path(p: &Served, c: &Caller, file: &OwnedFd, change: Change) -> Answer {
    let st = stat(file)?;
    let real = name_of(file)?;
    if changes_owner(change, &st) {
        return owner_refused(p, c, file, &real, &st, change);
    }
    if let Some(no) = refused(p, c, file, &real, &st, change, false) {
        return no;
    }
    match change {
        Change::Mode(_) if st.st_mode & libc::S_IFMT == libc::S_IFLNK => Err(libc::EOPNOTSUPP),
        Change::Mode(mode) => chmod_proc(file, mode),
        // (an empty path: the descriptor's own file, a link itself too)
        Change::Times(t) => utimensat(file.as_raw_fd(), c"".as_ptr(), t, EMPTY),
        Change::Owner(..) => chown_unchanged(file, change, &st),
    }
}

/// A call on the caller's descriptor `fd`: decided on what it names — a file by its name (`/proc/self/fd/M`'s link:
/// where it is now), never one the name no longer leads to; no file of the user's (`wire::no_name`: no name left, a
/// pipe, a socket), the run's — and made `again` on tarae's copy.
fn on_fd(p: &Served, c: &Caller, fd: u64, change: Change, again: Again) -> Answer {
    let file = c.fd(fd)?;
    let st = stat(&file)?;
    let proc = proc_path(&file);
    let name = name_of(&file)?;
    if changes_owner(change, &st) {
        return owner_refused(p, c, &file, &name, &st, change);
    }
    let named = || named_there(c, &name, &st);
    if !no_name(name.as_os_str().as_bytes(), st.st_nlink == 0, named)
        && let Some(no) = refused(p, c, &file, &name, &st, change, writable(&file))
    {
        return no;
    }
    let f = file.as_raw_fd();
    match (again, change) {
        (_, Change::Owner(..)) => chown_unchanged(&file, change, &st),
        // SAFETY: plain calls on tarae's own descriptor.
        (Again::Fchmod, Change::Mode(mode)) => result(unsafe { libc::fchmod(f, mode) } as isize),
        (Again::Fchmodat2(flags), Change::Mode(mode)) => {
            // SAFETY: tarae's descriptor and an empty path that lives through the call.
            result(unsafe { libc::syscall(libc::SYS_fchmodat2, f, c"".as_ptr(), mode, flags) } as isize)
        }
        (Again::Utimensat { empty, flags }, Change::Times(t)) => {
            utimensat(f, if empty { c"".as_ptr() } else { std::ptr::null() }, t, flags)
        }
        (Again::Proc { follow: false }, Change::Mode(_)) => Err(libc::EOPNOTSUPP),
        (Again::Proc { follow: true }, Change::Mode(mode)) => chmod_proc(&file, mode),
        (Again::Proc { follow }, Change::Times(t)) => {
            utimensat(libc::AT_FDCWD, proc.as_ptr(), t, if follow { 0 } else { NOFOLLOW })
        }
        _ => Err(libc::EINVAL),
    }
}

/// The refusal of a call on `file`, named `name` (`st` its stat): where the run may not write, or where `name`
/// doesn't lead the run to that file — from its own root, through no link (a cover's, one moved or put there since,
/// a name the kernel says is gone). `EROFS` on a read-only mount (a B9 seal), as the kernel answers it there, else
/// `EPERM`. A descriptor the run holds open for writing (`written`) is let wherever: Landlock let the run write that
/// file when it opened it, so its mode and times follow what it may write in it (a protected name the run made where
/// none was too: the snapshot's card asks about it, G23) — its name must still lead the run to it.
fn refused(
    p: &Served,
    c: &Caller,
    file: &OwnedFd,
    name: &Path,
    st: &libc::stat,
    change: Change,
    written: bool,
) -> Option<Answer> {
    if (written || p.writes_allow(name)) && named_there(c, name, st) {
        return None;
    }
    let errno = if read_only(file) { libc::EROFS } else { libc::EPERM };
    Some(refuse_with(p, c, errno, change.op(), name.display().to_string()))
}

/// Whether `name` leads the run, from its own root and through no link, to the file `st` is.
fn named_there(c: &Caller, name: &Path, st: &libc::stat) -> bool {
    let there = open_in_run(c, libc::AT_FDCWD as u64, name.as_os_str().as_bytes(), false, true);
    there.and_then(|t| stat(&t)).is_ok_and(|t| (t.st_dev, t.st_ino) == (st.st_dev, st.st_ino))
}

/// Whether an owner call changes the owner or the group — each asked for (not `-1`) and not what the file has: never
/// let, in no place (`rsync -a`'s and `cp -a`'s ask for what the file has, and are).
fn changes_owner(change: Change, st: &libc::stat) -> bool {
    matches!(change, Change::Owner(uid, gid)
        if ![u32::MAX, st.st_uid].contains(&uid) || ![u32::MAX, st.st_gid].contains(&gid))
}

/// A real change of owner or group, refused: `EROFS` on a read-only mount (a B9 seal), as the kernel answers it there,
/// else `EPERM` — said, but where the kernel refuses that call anyway (`kernel_lets`): its own `EPERM`, unsaid, as
/// outside (a user's `cp -a` of another's file asks for that owner and goes on without it).
fn owner_refused(
    p: &Served,
    c: &Caller,
    file: &OwnedFd,
    name: &Path,
    st: &libc::stat,
    change: Change,
) -> Answer {
    let errno = if read_only(file) { libc::EROFS } else { libc::EPERM };
    if errno == libc::EPERM && !kernel_lets(change, st) {
        return Err(libc::EPERM);
    }
    refuse_with(p, c, errno, change.op(), name.display().to_string())
}

/// Whether the kernel lets tarae's credentials — the run's user's — make an owner call on `st`'s file (`chown_ok`,
/// `chgrp_ok`): root any (`CAP_CHOWN`; a root without it is said); else only the file's owner, its owner kept and
/// its group one of the caller's.
fn kernel_lets(change: Change, st: &libc::stat) -> bool {
    // SAFETY: a plain call.
    lets(unsafe { libc::geteuid() }, change, st, in_group)
}

/// `kernel_lets` for a caller of `euid` in the groups `in_group` says.
fn lets(euid: u32, change: Change, st: &libc::stat, in_group: impl Fn(u32) -> bool) -> bool {
    let Change::Owner(uid, gid) = change else { return true };
    let owner = euid == st.st_uid;
    euid == 0
        || (uid == u32::MAX || owner && uid == st.st_uid)
            && (gid == u32::MAX || owner && (gid == st.st_gid || in_group(gid)))
}

/// Whether `gid` is tarae's effective group or one of its supplementary groups (`in_group_p`).
pub(in crate::sandbox) fn in_group(gid: u32) -> bool {
    // SAFETY: plain calls; the buffer holds what the first call counted, and only what the second filled is read.
    unsafe {
        if libc::getegid() == gid {
            return true;
        }
        let n = libc::getgroups(0, std::ptr::null_mut()).max(0);
        let mut groups = vec![0; n as usize];
        let filled = libc::getgroups(n, groups.as_mut_ptr()).max(0) as usize;
        groups[..filled].contains(&gid)
    }
}

/// An owner call that changes nothing, made on tarae's own descriptor of the file it decided on (`-1`, `-1`, through no
/// link): the kernel's own side effects — its change time, a set-user-ID or set-group-ID bit cleared — as the run's
/// call would have had them. But the kernel's own `EPERM` where it refuses that call (`kernel_lets`: an id given
/// as the file's own, on a file not the caller's) — `-1`, `-1` alone it lets anyone make.
fn chown_unchanged(fd: &OwnedFd, change: Change, st: &libc::stat) -> Answer {
    if !kernel_lets(change, st) {
        return Err(libc::EPERM);
    }
    // SAFETY: tarae's descriptor and an empty path that lives through the call.
    result(unsafe { libc::fchownat(fd.as_raw_fd(), c"".as_ptr(), u32::MAX, u32::MAX, EMPTY) } as isize)
}

/// Whether tarae's copy of a run's descriptor is open for writing (`O_WRONLY`, `O_RDWR`; never `O_PATH`).
fn writable(fd: &OwnedFd) -> bool {
    // SAFETY: a descriptor of ours.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    flags >= 0
        && flags & libc::O_PATH == 0
        && matches!(flags & libc::O_ACCMODE, libc::O_WRONLY | libc::O_RDWR)
}

fn stat(fd: &OwnedFd) -> Result<libc::stat, i32> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a descriptor of ours and a buffer the call fills.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: filled by the successful fstat above.
    Ok(unsafe { st.assume_init() })
}

/// Whether `fd`'s mount is read-only (`statvfs`'s flags hold the mount's own, a read-only bind's too).
fn read_only(fd: &OwnedFd) -> bool {
    let mut s = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: a descriptor of ours and a buffer the call fills; read only once filled.
    unsafe {
        libc::fstatvfs(fd.as_raw_fd(), s.as_mut_ptr()) == 0 && s.assume_init().f_flag & libc::ST_RDONLY != 0
    }
}

/// What tarae's own descriptor `fd` names now (its `/proc/self/fd/M` link): a path, ` (deleted)` after one gone, or
/// `pipe:[…]`, `socket:[…]`.
fn name_of(fd: &OwnedFd) -> Result<std::path::PathBuf, i32> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(|e| errno(&e))
}

/// `/proc/self/fd/M`: tarae's own descriptor `fd` as a path — what it reaches is that file, whatever's at its name.
fn proc_path(fd: &OwnedFd) -> CString {
    CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap_or_default()
}

/// The mode of tarae's own descriptor's file, through its `/proc/self/fd/M` (an `O_PATH` one's too, which `fchmod`
/// refuses — Linux 5.14 has no `fchmodat2` for it).
fn chmod_proc(fd: &OwnedFd, mode: libc::mode_t) -> Answer {
    let proc = proc_path(fd);
    // SAFETY: a path that lives through the call.
    result(unsafe { libc::syscall(libc::SYS_fchmodat, libc::AT_FDCWD, proc.as_ptr(), mode) } as isize)
}

/// `utimensat(dirfd, path, times, flags)` raw (glibc refuses a path of none), the times tarae's own copy.
fn utimensat(dirfd: i32, path: *const libc::c_char, t: Option<[i64; 4]>, flags: i32) -> Answer {
    let ts = t.map(|t| {
        [libc::timespec { tv_sec: t[0], tv_nsec: t[1] }, libc::timespec { tv_sec: t[2], tv_nsec: t[3] }]
    });
    let at = ts.as_ref().map_or(std::ptr::null(), |ts| ts.as_ptr());
    // SAFETY: the path (or none) and the times (or none) live through the call.
    result(unsafe { libc::syscall(libc::SYS_utimensat, dirfd, path, at, flags) } as isize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_call_is_let_as_the_kernel_lets_it() {
        // SAFETY: an all-zero `stat` is a valid value; only its ids are read.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (st.st_uid, st.st_gid) = (1000, 1000);
        let none = u32::MAX;
        // (the caller: 1000 in groups 1000 and 20, or 1001, or root)
        let groups = |g| [1000, 20].contains(&g);
        for (euid, uid, gid, want) in [
            (1000, none, none, true),
            (1000, 1000, 1000, true),
            (1000, none, 20, true),
            (1000, none, 30, false),
            (1000, 1001, none, false),
            // (not the file's: its own ids given are refused, `-1`, `-1` alone let)
            (1001, 1000, none, false),
            (1001, none, 1000, false),
            (1001, none, none, true),
            (0, 1001, 30, true),
        ] {
            assert_eq!(lets(euid, Change::Owner(uid, gid), &st, groups), want, "{euid}: {uid} {gid}");
        }
    }
}
