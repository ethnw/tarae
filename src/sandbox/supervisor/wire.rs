//! What the supervisor (`supervisor.rs`) reads of a run's memory, as the kernel reads it: an address (`struct
//! sockaddr` — `parse`), a netlink batch (`netlink_reads`), a message (`struct msghdr` and all it points to —
//! `read_msg`). Pure over the bytes, and over `Mem` where it follows pointers it has just read — so it's built and
//! tested on every system, its loops on macOS too; on Linux the memory is the caller's (`/proc/<tid>/mem`).
//!
//! The numbers the bytes hold are Linux's (`abi`, its 64-bit uapi — macOS's own differ: `AF_INET6` is 30 there);
//! an errno is the host's libc's, as only Linux's ever reaches a run.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsStr;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use abi::*;

/// Linux's numbers (`include/linux/socket.h`, `uapi/linux/netlink.h`, `uapi/linux/uio.h`), written out so they're
/// Linux's on every system (`the_numbers_are_linuxs` checks them against libc's there).
mod abi {
    pub const AF_UNSPEC: i32 = 0;
    pub const AF_UNIX: i32 = 1;
    pub const AF_INET: i32 = 2;
    pub const AF_INET6: i32 = 10;
    pub const AF_NETLINK: i32 = 16;
    pub const SOL_SOCKET: i32 = 1;
    pub const SCM_RIGHTS: i32 = 1;
    pub const SCM_CREDENTIALS: i32 = 2;
    /// The most descriptors one message passes (`include/net/scm.h`).
    pub const SCM_MAX_FD: usize = 253;
    pub const UIO_MAXIOV: i32 = 1024;
    pub const NLM_F_REQUEST: i32 = 1;
    pub const NLM_F_ACK: i32 = 4;
    pub const NLM_F_DUMP: i32 = 0x300;
}

/// The longest address a call names (`struct sockaddr_storage`).
const ADDR_MAX: usize = 128;
/// The most of a message tarae copies: a datagram longer fails as the kernel's own limit would
/// (`EMSGSIZE`); a stream's is sent in part, as a send may be.
pub(super) const SEND_MAX: usize = 1 << 20;
/// The most control data (`SCM_RIGHTS` …) a message carries.
const CONTROL_MAX: usize = 1 << 16;
/// `struct msghdr` and `struct mmsghdr` on the 64-bit architectures, and where `msg_len` sits in the latter.
const MSGHDR: usize = 56;
const MMSGHDR: usize = 64;
pub(super) const MSG_LEN_AT: u64 = 56;

/// A run's memory as the supervisor reads it (`Caller` on Linux; a fake in the tests).
pub(super) trait Mem {
    /// `len` bytes at `at`, read once — not all there: `EFAULT`.
    fn read(&self, at: u64, len: usize) -> Result<Vec<u8>, i32>;
}

/// An address a call named, as tarae read it.
#[derive(Debug, PartialEq)]
pub(super) enum Addr {
    /// `AF_UNSPEC`: a datagram socket's disconnect, or where it's connected.
    Unspec,
    Ip(SocketAddr),
    /// A unix socket by its path, as named (relative: from the caller's folder).
    Path(PathBuf),
    /// An abstract unix name (none: an unbound socket's own made up).
    Abstract(Vec<u8>),
    /// A unix address with no name at all.
    Unnamed,
    Netlink,
    Other,
}

/// `bytes` (a `struct sockaddr`) read as the kernel reads them; too short for its family: `EINVAL`.
pub(super) fn parse(bytes: &[u8]) -> Result<Addr, i32> {
    if bytes.len() < 2 {
        return Err(libc::EINVAL);
    }
    let b = |at: usize, len: usize| bytes.get(at..at + len).ok_or(libc::EINVAL);
    let port = |at| b(at, 2).map(|p| u16::from_be_bytes([p[0], p[1]]));
    Ok(match i32::from(u16::from_ne_bytes([bytes[0], bytes[1]])) {
        AF_UNSPEC => Addr::Unspec,
        AF_INET => {
            let ip: [u8; 4] = b(4, 4)?.try_into().map_err(|_| libc::EINVAL)?;
            Addr::Ip(SocketAddr::new(IpAddr::from(ip), port(2)?))
        }
        AF_INET6 => {
            let ip: [u8; 16] = b(8, 16)?.try_into().map_err(|_| libc::EINVAL)?;
            Addr::Ip(SocketAddr::new(IpAddr::from(ip), port(2)?))
        }
        AF_UNIX => match &bytes[2..] {
            [] => Addr::Unnamed,
            [0, name @ ..] => Addr::Abstract(name.to_vec()),
            path => {
                let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                Addr::Path(PathBuf::from(OsStr::from_bytes(&path[..end])))
            }
        },
        AF_NETLINK => Addr::Netlink,
        _ => Addr::Other,
    })
}

/// The address a call names (`at`, `len` bytes), read once.
pub(super) fn address(m: &impl Mem, at: u64, len: u64) -> Result<Vec<u8>, i32> {
    match usize::try_from(len) {
        Ok(len) if len <= ADDR_MAX => m.read(at, len),
        _ => Err(libc::EINVAL),
    }
}

/// A unix socket's path as an address (`/proc/self/fd/N…`, by tarae's own descriptor).
pub(super) fn unix_address(path: &[u8]) -> Result<Vec<u8>, i32> {
    if path.len() >= 108 {
        return Err(libc::ENAMETOOLONG);
    }
    let mut a = (AF_UNIX as u16).to_ne_bytes().to_vec();
    a.extend_from_slice(path);
    a.push(0);
    Ok(a)
}

/// The address of `name` in the folder tarae holds open as `dir` — where a bind makes it: the name's own bytes,
/// the ones checked (not UTF-8 either).
pub(super) fn named_in(dir: std::os::fd::RawFd, name: &OsStr) -> Result<Vec<u8>, i32> {
    let mut path = format!("/proc/self/fd/{dir}/").into_bytes();
    path.extend_from_slice(name.as_bytes());
    unix_address(&path)
}

/// A netlink address (`struct sockaddr_nl`: family, padding, port, groups) as (port, groups); too short:
/// `EINVAL`, as the kernel says it.
pub(super) fn netlink_address(addr: &[u8]) -> Result<(u32, u32), i32> {
    let word = |at: usize| addr.get(at..at + 4).map(|w| u32::from_ne_bytes(w.try_into().unwrap_or_default()));
    match (word(4), word(8)) {
        (Some(port), Some(groups)) => Ok((port, groups)),
        _ => Err(libc::EINVAL),
    }
}

/// rtnetlink's message types come in fours from here — new, delete, get, set (`include/uapi/linux/rtnetlink.h`:
/// every `RTM_GET…` is the third of its four, nothing else is) — and the kernel lets a sender without
/// `CAP_NET_ADMIN` only the gets (`rtnetlink_rcv_msg`: `kind != RTNL_KIND_GET` needs it).
const RTM_BASE: u16 = 16;

/// The flags a read carries: a request, a dump, an ack — what a name lookup sends.
const NETLINK_READ_FLAGS: u16 = (NLM_F_REQUEST | NLM_F_DUMP | NLM_F_ACK) as u16;

/// Whether `data`, a batch of rtnetlink messages, only reads: each `struct nlmsghdr` whole (its length at least
/// its own and within what's left, the next at the 4-byte alignment, nothing left over), its type a get, its
/// flags within `NETLINK_READ_FLAGS` — else what's refused, in words.
pub(super) fn netlink_reads(data: &[u8]) -> Result<(), String> {
    let mut rest = data;
    loop {
        if rest.len() < 16 {
            return Err("a message cut short".into());
        }
        let len = u32::from_ne_bytes(rest[..4].try_into().unwrap_or_default()) as usize;
        let kind = u16::from_ne_bytes([rest[4], rest[5]]);
        let flags = u16::from_ne_bytes([rest[6], rest[7]]);
        if len < 16 || len > rest.len() {
            return Err("a message cut short".into());
        }
        if kind < RTM_BASE || (kind - RTM_BASE) % 4 != 2 {
            return Err(format!("message type {kind}"));
        }
        if flags & !NETLINK_READ_FLAGS != 0 {
            return Err(format!("message flags {flags:#x}"));
        }
        rest = &rest[len.next_multiple_of(4).min(rest.len())..];
        if rest.is_empty() {
            return Ok(());
        }
    }
}

/// One `struct msghdr` of the caller's, read once: where it goes (if it names it), what it carries, and its
/// control data — built by tarae: only the descriptors it passes (`SCM_RIGHTS`), taken as tarae's own
/// (`Served::passed`), passing on what the run holds. Every other control message is refused (`Unsent::Control`):
/// credentials (`SCM_CREDENTIALS`) tarae can't speak for, and no other type goes on as the run's bytes.
pub(super) struct Msg {
    pub(super) name: Option<Vec<u8>>,
    pub(super) data: Vec<u8>,
    pub(super) control: Vec<u8>,
    pub(super) fds: Vec<OwnedFd>,
}

/// Why a message isn't sent: the errno the caller gets — or a control message of a type tarae doesn't pass on
/// (every one but `SCM_RIGHTS`: `EPERM`, and the caller says it in the run's report by its `named`).
#[derive(Debug, PartialEq)]
pub(super) enum Unsent {
    Errno(i32),
    Control { level: i32, kind: i32 },
}

impl From<i32> for Unsent {
    fn from(errno: i32) -> Unsent {
        Unsent::Errno(errno)
    }
}

impl Unsent {
    /// Tests: the errno the caller gets.
    #[cfg(any(test, fuzzing))]
    pub(super) fn errno(&self) -> i32 {
        match self {
            Unsent::Errno(e) => *e,
            Unsent::Control { .. } => libc::EPERM,
        }
    }
}

/// A control message as the run's report names it.
pub(super) fn named(level: i32, kind: i32) -> String {
    match (level, kind) {
        (SOL_SOCKET, SCM_CREDENTIALS) => "SCM_CREDENTIALS".into(),
        _ => format!("control message level {level}, type {kind}"),
    }
}

/// The message at `at` in `m`, each descriptor it passes taken by `pass` (the caller's number → tarae's own).
///
/// Its iovecs as the kernel takes them (`import_iovec`): the whole array first, every length checked before any
/// buffer is read — one that isn't an `ssize_t` (its top bit set) `EINVAL` — and a datagram whose total passes
/// `SEND_MAX` `EMSGSIZE`, nothing of it read; then the buffers in order, up to `SEND_MAX`. Its control as the kernel
/// takes it (`____sys_sendmsg`): read wherever `msg_control` points when it has a length, a null pointer too —
/// `EFAULT` where nothing is mapped there (V2-4).
pub(super) fn read_msg<M: Mem>(
    m: &M,
    mut pass: impl FnMut(i32) -> Result<OwnedFd, i32>,
    at: u64,
    datagram: bool,
) -> Result<Msg, Unsent> {
    let h = m.read(at, MSGHDR)?;
    let word = |o: usize| u64::from_ne_bytes(h[o..o + 8].try_into().unwrap_or_default());
    let (name_at, name_len) =
        (word(0), u64::from(u32::from_ne_bytes(h[8..12].try_into().unwrap_or_default())));
    let (iov_at, iov_len, control_at, control_len) = (word(16), word(24), word(32), word(40));
    let name = match (name_at, name_len) {
        (0, _) | (_, 0) => None,
        (a, len) => Some(address(m, a, len)?),
    };
    if iov_len > UIO_MAXIOV as u64 {
        return Err(libc::EMSGSIZE.into());
    }
    let iov = m.read(iov_at, iov_len as usize * 16)?;
    // (`struct iovec`: where, how long)
    let iov: Vec<(u64, u64)> = iov
        .as_chunks::<16>()
        .0
        .iter()
        .map(|v| {
            let (base, len) = v.split_at(8);
            let word = |w: &[u8]| u64::from_ne_bytes(w.try_into().unwrap_or_default());
            (word(base), word(len))
        })
        .collect();
    if iov.iter().any(|&(_, len)| len > isize::MAX as u64) {
        return Err(libc::EINVAL.into());
    }
    let total = iov.iter().fold(0u64, |total, &(_, len)| total.saturating_add(len));
    if datagram && total > SEND_MAX as u64 {
        return Err(libc::EMSGSIZE.into());
    }
    let mut data = Vec::new();
    for (base, len) in iov {
        let room = (SEND_MAX - data.len()) as u64;
        data.extend(m.read(base, len.min(room) as usize)?);
        if data.len() == SEND_MAX {
            break;
        }
    }
    let control_len = usize::try_from(control_len).unwrap_or(usize::MAX);
    if control_len > CONTROL_MAX {
        return Err(libc::ENOBUFS.into());
    }
    let theirs = if control_len == 0 { Vec::new() } else { m.read(control_at, control_len)? };
    // (what goes out is built here: a header of tarae's per `SCM_RIGHTS`, nothing of the run's bytes)
    let (mut control, mut fds) = (Vec::new(), Vec::new());
    // `struct cmsghdr`: its length (with itself), level, type, then the data — each aligned to 8
    let mut o = 0;
    while o + 16 <= theirs.len() {
        let len = usize::try_from(u64::from_ne_bytes(theirs[o..o + 8].try_into().unwrap_or_default()))
            .unwrap_or(usize::MAX);
        if len < 16 || len > theirs.len() - o {
            return Err(libc::EINVAL.into());
        }
        let level = i32::from_ne_bytes(theirs[o + 8..o + 12].try_into().unwrap_or_default());
        let kind = i32::from_ne_bytes(theirs[o + 12..o + 16].try_into().unwrap_or_default());
        if (level, kind) != (SOL_SOCKET, SCM_RIGHTS) {
            return Err(Unsent::Control { level, kind });
        }
        // (a tail under 4 bytes ignored, as the kernel does)
        let n = (len - 16) / 4;
        if fds.len() + n > SCM_MAX_FD {
            return Err(libc::EINVAL.into());
        }
        control.extend((16 + 4 * n as u64).to_ne_bytes());
        control.extend(SOL_SOCKET.to_ne_bytes());
        control.extend(SCM_RIGHTS.to_ne_bytes());
        for at in (o + 16..).step_by(4).take(n) {
            let ours = pass(i32::from_ne_bytes(theirs[at..at + 4].try_into().unwrap_or_default()))?;
            control.extend(ours.as_raw_fd().to_ne_bytes());
            fds.push(ours);
        }
        control.resize(control.len().next_multiple_of(8), 0);
        o += len.div_ceil(8) * 8;
    }
    Ok(Msg { name, data, control, fds })
}

/// Where the `i`th `struct mmsghdr` of an array at `base` is — none where it would run past the end of the
/// address space (its `msg_len` with it, written back at `MSG_LEN_AT`): the kernel's `EFAULT`, never a sum that
/// wraps.
pub(super) fn mmsg_at(base: u64, i: u64) -> Option<u64> {
    let at = i.checked_mul(MMSGHDR as u64)?.checked_add(base)?;
    at.checked_add(MMSGHDR as u64).map(|_| at)
}

/// The properties the loops and the fuzz targets hold these readers to (`fuzz/parsers`: `--cfg fuzzing`).
#[cfg(any(test, fuzzing))]
#[path = "wire/check.rs"]
pub(super) mod check;

#[cfg(test)]
#[path = "wire/tests.rs"]
mod tests;
