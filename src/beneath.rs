//! Writes in the project through no link (toolcalls, 2026-10-13): what tarae writes for Claude's change sets
//! (`refactor::write_file` — edit, rename, replace) goes through descriptors walked from the project root's
//! real path one name at a time, each folder opened with `O_DIRECTORY | O_NOFOLLOW` from the one before, the
//! file itself `O_NOFOLLOW` — and every write after the check uses those descriptors, never a path walked again.
//! A sandboxed run writes the project and tarae's writes aren't sandboxed: a folder swapped for a link to
//! outside — before the walk, or after it while the change waits — is never gone through (a link in the way
//! is refused and left as it is; a folder swapped away after its descriptor was taken keeps the write).
//! The read side has the same rule (`paths::Opened`). Portable `openat` steps on macOS and Linux alike.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

/// A folder in the project, opened through no link.
pub struct Dir {
    fd: OwnedFd,
    /// Where it was when opened (for what's said).
    pub at: PathBuf,
}

fn c(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a name"))
}

fn open_at(fd: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a folder descriptor of ours (or the cwd's) and a name, live for the call.
    let got = unsafe { libc::openat(fd, name.as_ptr(), flags | libc::O_CLOEXEC, 0o666 as libc::c_uint) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made, ours.
    Ok(unsafe { OwnedFd::from_raw_fd(got) })
}

/// Whether `name` in `fd` is a link (not followed).
fn is_link(fd: libc::c_int, name: &CStr) -> bool {
    // SAFETY: zeroed out-struct, filled by the call; a descriptor of ours and a name, live for the call.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    r == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

/// An error at `name` in `fd` said as what it is: a link there is "a link — not gone through".
fn said(e: io::Error, fd: libc::c_int, name: &CStr, at: &Path) -> io::Error {
    if is_link(fd, name) {
        return io::Error::other(format!("{} is a link — not gone through", at.display()));
    }
    io::Error::new(e.kind(), format!("{}: {e}", at.display()))
}

/// A file's stamp as `disk::stamp` takes it — from its descriptor.
fn stamp_of(f: &std::fs::File) -> io::Result<crate::disk::Stamp> {
    let m = f.metadata()?;
    Ok((m.modified()?, m.len()))
}

impl Dir {
    /// The project's root by its real path — the root itself not a link.
    pub fn root(root: &Path) -> io::Result<Dir> {
        let real = std::fs::canonicalize(root)?;
        let fd = open_at(
            libc::AT_FDCWD,
            &c(real.as_os_str())?,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )?;
        Ok(Dir { fd, at: real })
    }

    /// Folder `rel` below it (plain names only), each step opened through no link from the one before;
    /// `make`: the missing ones made on the way (`mkdirat`, as the umask has it).
    pub fn folder(&self, rel: &Path, make: bool) -> io::Result<Dir> {
        let mut dir = Dir { fd: self.fd.try_clone()?, at: self.at.clone() };
        for part in rel.components() {
            let Component::Normal(name) = part else {
                return Err(io::Error::other(format!("{}: not a plain name in the project", rel.display())));
            };
            let cn = c(name)?;
            let at = dir.at.join(name);
            let fd = dir.fd.as_raw_fd();
            if make {
                // SAFETY: a folder descriptor of ours and a name, live for the call (one there already: EEXIST).
                unsafe { libc::mkdirat(fd, cn.as_ptr(), 0o777) };
            }
            let opened = open_at(fd, &cn, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .map_err(|e| said(e, fd, &cn, &at))?;
            dir = Dir { fd: opened, at };
        }
        Ok(dir)
    }

    /// File `name`'s bytes — None: nothing there; Err for a link (never followed) or what isn't a plain file.
    pub fn read(&self, name: &OsStr) -> io::Result<Option<Vec<u8>>> {
        let (cn, at, fd) = (c(name)?, self.at.join(name), self.fd.as_raw_fd());
        let file = match open_at(fd, &cn, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK) {
            Ok(f) => std::fs::File::from(f),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(said(e, fd, &cn, &at)),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(format!("{} isn't a plain file", at.display())));
        }
        let mut bytes = Vec::new();
        (&file).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    /// A new file `name` holding `bytes`, made here only — `O_CREAT | O_EXCL | O_NOFOLLOW`: never over anything
    /// there, never through a link. Its stamp.
    pub fn create(&self, name: &OsStr, bytes: &[u8]) -> io::Result<crate::disk::Stamp> {
        let (cn, at, fd) = (c(name)?, self.at.join(name), self.fd.as_raw_fd());
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        let mut file = std::fs::File::from(open_at(fd, &cn, flags).map_err(|e| said(e, fd, &cn, &at))?);
        file.write_all(bytes)?;
        file.sync_all()?;
        stamp_of(&file)
    }

    /// File `name` replaced whole by `bytes`, as a save is: a file of its own made beside it (`O_EXCL |
    /// O_NOFOLLOW`) with the old one's permissions, then renamed over the name in this folder — the old one read
    /// through no link first, so a link at the name is refused, never replaced nor gone through. Its stamp.
    pub fn replace(&self, name: &OsStr, bytes: &[u8]) -> io::Result<crate::disk::Stamp> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (cn, at, fd) = (c(name)?, self.at.join(name), self.fd.as_raw_fd());
        let old = open_at(fd, &cn, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .map_err(|e| said(e, fd, &cn, &at))?;
        let old = std::fs::File::from(old);
        let meta = old.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::other(format!("{} isn't a plain file", at.display())));
        }
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".tarae-{}-{n}", std::process::id()));
        let temp = c(&temp_name)?;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        let mut file = std::fs::File::from(open_at(fd, &temp, flags)?);
        let written = (|| {
            file.set_permissions(meta.permissions())?;
            file.write_all(bytes)?;
            file.sync_all()?;
            let stamp = stamp_of(&file)?;
            // SAFETY: a folder descriptor of ours and two names, live for the call.
            match unsafe { libc::renameat(fd, temp.as_ptr(), fd, cn.as_ptr()) } {
                0 => Ok(stamp),
                _ => Err(io::Error::last_os_error()),
            }
        })();
        if written.is_err() {
            // SAFETY: as above — the file of our own, made here.
            unsafe { libc::unlinkat(fd, temp.as_ptr(), 0) };
        }
        written.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", at.display())))
    }

    /// File `name` removed from this folder (`unlinkat` — a link there would go itself, nothing it leads to).
    pub fn remove(&self, name: &OsStr) -> io::Result<()> {
        let (cn, fd) = (c(name)?, self.fd.as_raw_fd());
        // SAFETY: a folder descriptor of ours and a name, live for the call.
        match unsafe { libc::unlinkat(fd, cn.as_ptr(), 0) } {
            0 => Ok(()),
            _ => {
                let e = io::Error::last_os_error();
                Err(io::Error::new(e.kind(), format!("{}: {e}", self.at.join(name).display())))
            }
        }
    }

    /// File `name` here moved to `to_name` in `to`: linked there first (`linkat` — never over anything there,
    /// and a link at `name` is linked as itself, not followed), then unlinked here.
    pub fn move_to(&self, name: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
        let (cn, tn) = (c(name)?, c(to_name)?);
        // SAFETY: two folder descriptors of ours and two names, live for the call (flags 0: no following).
        if unsafe { libc::linkat(self.fd.as_raw_fd(), cn.as_ptr(), to.fd.as_raw_fd(), tn.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.remove(name)
    }

    /// File `name`'s stamp, through no link.
    pub fn stamp(&self, name: &OsStr) -> io::Result<crate::disk::Stamp> {
        let (cn, at, fd) = (c(name)?, self.at.join(name), self.fd.as_raw_fd());
        let f = open_at(fd, &cn, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .map_err(|e| said(e, fd, &cn, &at))?;
        stamp_of(&std::fs::File::from(f))
    }
}
