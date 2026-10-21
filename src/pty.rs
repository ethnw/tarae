//! A pseudo-terminal whose two ends are close-on-exec from the moment they exist.
//!
//! `openpty` opens them without it, and setting it afterwards leaves a window in which a program another
//! thread starts (a language server, Claude, the next terminal's shell) keeps a copy of the master — and
//! the program on the pty never hangs up while that copy lives, its own included: the terminal panel's
//! shell got the master it was on. Seen 2026-10-04: 439 shells of killed e2e taraes left under launchd, each
//! holding its own master and others', rust-analyzer and marksman holding 36 and 16 — all 511 of the Mac's
//! ptys taken. Shared by the terminal panel, the e2e harness and `cargo xtask shot` (`#[path]`): std and
//! libc only.
//!
//! Inside a sandboxed run on macOS no pty can be opened (every `/dev/ttys*` is closed to it — the user's are
//! those names too): the run's broker makes it (`sandbox::ptybroker`), asked on the socket `BROKER` names, and
//! hands both ends in (`SCM_RIGHTS`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// The variable naming the socket of the run's pty broker — set for a sandboxed run on macOS only.
pub const BROKER: &str = "TARAE_PTY_SOCK";

/// A new pty of `rows` × `cols`: (master, slave), both close-on-exec — a child gets the slave only as the
/// stdio it's handed. In a sandboxed run (`BROKER` set): asked of its broker.
pub fn open(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    match std::env::var_os(BROKER) {
        Some(socket) => asked(std::path::Path::new(&socket), rows, cols),
        None => made(rows, cols),
    }
}

/// A pty asked of the broker at `socket`: the size sent, both ends received.
pub fn asked(socket: &std::path::Path, rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    asked_with_node(socket, rows, cols).map(|(m, s, _)| (m, s))
}

/// The slave of a pair handed in, as the broker that made it named it: its path in /dev and its inode.
pub type Node = (std::path::PathBuf, u64);

/// `asked`, with the `Node` the broker sent beside the pair (`node_bytes`) — None from one that sent none.
#[allow(dead_code, reason = "tarae's broker's: the e2e harness and xtask share this file (`#[path]`)")]
pub fn asked_with_node(
    socket: &std::path::Path,
    rows: u16,
    cols: u16,
) -> io::Result<(OwnedFd, OwnedFd, Option<Node>)> {
    use std::io::Write as _;
    let mut s = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| io::Error::new(e.kind(), format!("the run's pty broker ({}): {e}", socket.display())))?;
    let mut ask = [0u8; 4];
    ask[..2].copy_from_slice(&rows.to_le_bytes());
    ask[2..].copy_from_slice(&cols.to_le_bytes());
    s.write_all(&ask)?;
    let (fds, data) = recv_fds(s.as_raw_fd())?;
    match <[OwnedFd; 2]>::try_from(fds) {
        Ok([master, slave]) => Ok((master, slave, node_of(&data))),
        Err(_) => Err(io::Error::other("the run's pty broker sent no pty")),
    }
}

/// A `Node` as the broker sends it with the pair: its inode (8 bytes, little-endian), then its path.
#[allow(dead_code, reason = "tarae's broker's: the e2e harness and xtask share this file (`#[path]`)")]
pub fn node_bytes((path, ino): &Node) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = ino.to_le_bytes().to_vec();
    out.extend(path.as_os_str().as_bytes());
    out
}

/// `node_bytes` read back — None for one byte (a broker that names none) or what isn't one.
fn node_of(data: &[u8]) -> Option<Node> {
    use std::os::unix::ffi::OsStrExt as _;
    let (ino, path) = (data.get(..8)?, data.get(8..).filter(|p| p.starts_with(b"/dev/"))?);
    let ino = u64::from_le_bytes(ino.try_into().ok()?);
    Some((std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path)), ino))
}

/// A new pty made here — `/dev/ptmx` opened (what `open` does outside a sandbox, and the broker for a run).
pub fn made(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
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

/// `fds` sent on the unix socket `sock` with `data` (at least one byte, at most `DATA_MAX`; `SCM_RIGHTS`) — the
/// broker's side.
#[allow(dead_code, reason = "tarae's broker's: the e2e harness and xtask share this file (`#[path]`)")]
pub fn send_fds(sock: RawFd, fds: &[RawFd], data: &[u8]) -> io::Result<()> {
    let bytes = std::mem::size_of_val(fds) as u32;
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
    let mut control = vec![0u8; space];
    let mut data = if data.is_empty() { vec![0u8] } else { data[..data.len().min(DATA_MAX)].to_vec() };
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
    // SAFETY: a zeroed msghdr pointing at live locals; one control message of `fds.len()` descriptors fills
    // the buffer CMSG_SPACE sized for it.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), fds.len());
        if libc::sendmsg(sock, &msg, 0) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The most bytes a message beside the descriptors carries (`send_fds`).
const DATA_MAX: usize = 1024;

/// The descriptors one message on `sock` carries (at most 2), each made close-on-exec at once (macOS has no
/// `MSG_CMSG_CLOEXEC`: until then a program another thread starts could get a copy — a moment, in a run) —
/// and the bytes beside them.
fn recv_fds(sock: RawFd) -> io::Result<(Vec<OwnedFd>, Vec<u8>)> {
    let bytes = (2 * std::mem::size_of::<RawFd>()) as u32;
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
    let mut control = vec![0u8; space];
    let mut data = vec![0u8; DATA_MAX];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
    let mut out = Vec::new();
    // SAFETY: a zeroed msghdr pointing at live locals; the control messages walked as the kernel filled them,
    // each descriptor owned from here.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let got = libc::recvmsg(sock, &mut msg, flags);
        if got == -1 {
            return Err(io::Error::last_os_error());
        }
        data.truncate(got as usize);
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let fds = libc::CMSG_DATA(c).cast::<RawFd>();
                let n = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..n {
                    let fd = std::ptr::read_unaligned(fds.add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    out.push(OwnedFd::from_raw_fd(fd));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((out, data))
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
