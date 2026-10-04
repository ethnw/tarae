//! A pseudo-terminal whose two ends are close-on-exec from the moment they exist.
//!
//! `openpty` opens them without it, and setting it afterwards leaves a window in which a program another
//! thread starts (a language server, Claude, the next terminal's shell) keeps a copy of the master — and
//! the program on the pty never hangs up while that copy lives, its own included: the terminal panel's
//! shell got the master it was on. Seen 2026-10-04: 439 shells of killed e2e taraes left under launchd, each
//! holding its own master and others', rust-analyzer and marksman holding 36 and 16 — all 511 of the Mac's
//! ptys taken. Shared by the terminal panel, the e2e harness and `cargo xtask shot` (`#[path]`): std and
//! libc only.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A new pty of `rows` × `cols`: (master, slave), both close-on-exec — a child gets the slave only as the
/// stdio it's handed.
pub fn open(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: open with a NUL-terminated path; the descriptor is owned from here.
    let master = unsafe { OwnedFd::from_raw_fd(cvt(libc::open(c"/dev/ptmx".as_ptr(), flags))?) };
    let m = master.as_raw_fd();
    // SAFETY: grantpt and unlockpt on the master just opened.
    unsafe {
        cvt(libc::grantpt(m))?;
        cvt(libc::unlockpt(m))?;
    }
    let name = slave_name(m)?;
    // SAFETY: open with the NUL-terminated name the master gave; owned from here.
    let slave = unsafe { OwnedFd::from_raw_fd(cvt(libc::open(name.as_ptr().cast(), flags))?) };
    // (on the slave, as openpty does: macOS's master says ENOTTY to it before then)
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ reads one winsize from a live local.
    cvt(unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) })?;
    Ok((master, slave))
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// The slave's path, NUL-terminated (`ptsname` keeps it in a buffer every thread shares).
#[cfg(target_os = "macos")]
fn slave_name(master: libc::c_int) -> io::Result<[u8; 128]> {
    let mut name = [0u8; 128];
    // SAFETY: TIOCPTYGNAME fills a 128-byte buffer with a NUL-terminated path.
    cvt(unsafe { libc::ioctl(master, libc::TIOCPTYGNAME as _, name.as_mut_ptr()) })?;
    Ok(name)
}

#[cfg(not(target_os = "macos"))]
fn slave_name(master: libc::c_int) -> io::Result<[u8; 128]> {
    let mut name = [0u8; 128];
    // SAFETY: ptsname_r writes at most the buffer's length, NUL included.
    match unsafe { libc::ptsname_r(master, name.as_mut_ptr().cast(), name.len()) } {
        0 => Ok(name),
        e => Err(io::Error::from_raw_os_error(e)),
    }
}
