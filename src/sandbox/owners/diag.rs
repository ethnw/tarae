//! The sockets on one local port of tarae's network namespace, as the kernel lists them (`NETLINK_SOCK_DIAG`,
//! `inet_diag`): asked per call that may land on another session's (step 8). The port is filtered in the kernel
//! (`INET_DIAG_REQ_BYTECODE`), and a TCP ask lists listeners only — the kernel walks its listening table, not every
//! connection (M8.1: `/proc/net/tcp` walks them all — 1.4 ms with 100 sockets in the namespace, 12 ms with
//! 10 000). Each socket comes with its cookie (`idiag_cookie`, its `SO_COOKIE`: 64-bit, never reused — what
//! `owners` knows it by) and, an IPv6 one, its `IPV6_V6ONLY` (`INET_DIAG_SKV6ONLY`); `/proc/net` says neither.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::Row;

/// `SOCK_DIAG_BY_FAMILY` (`linux/sock_diag.h`).
const BY_FAMILY: u16 = 20;
/// `INET_DIAG_REQ_BYTECODE`, `INET_DIAG_SKV6ONLY` (`linux/inet_diag.h`).
const REQ_BYTECODE: u16 = 1;
const SKV6ONLY: u16 = 11;
/// `INET_DIAG_BC_S_GE`, `INET_DIAG_BC_S_LE`: the local port at least, at most the next op's `no`; `D_`: the
/// remote port.
const BC_S_GE: u8 = 2;
const BC_S_LE: u8 = 3;
const BC_D_GE: u8 = 4;
const BC_D_LE: u8 = 5;
/// The states asked: a TCP listener (`TCP_LISTEN`); a UDP socket connected to no one (`TCP_CLOSE`).
const LISTEN: u32 = 1 << 10;
const UNCONNECTED: u32 = 1 << 7;
/// `struct inet_diag_msg`'s size: its attributes follow.
const MSG: usize = 72;

/// The sockets on local `port` of tarae's network namespace a call may land on — TCP listeners (`tcp`), or UDP
/// sockets connected to no one — IPv4 and IPv6. An answer that doesn't come within a second is an error.
pub fn on_port(tcp: bool, port: u16) -> io::Result<Vec<Row>> {
    dump(tcp, Some((port, Local)), if tcp { LISTEN } else { UNCONNECTED })
}

/// The sockets a bind or a listen on local `port` could take from (`Owners::another_on`): TCP listeners there — the
/// kernel lists a bound TCP socket only once it listens —; UDP sockets there in any state, and UDP sockets
/// connected to that port (`Row.peer`): their plain sends, never served, land on what's bound there.
pub fn bound_on(tcp: bool, port: u16) -> io::Result<Vec<Row>> {
    match tcp {
        true => on_port(true, port),
        false => {
            let mut rows = dump(false, Some((port, Local)), !0)?;
            rows.extend(dump(false, Some((port, Remote)), !0)?);
            Ok(rows)
        }
    }
}

/// Which end of a socket a port filter asks about.
#[derive(Clone, Copy)]
enum End {
    Local,
    Remote,
}
use End::*;

/// The cookies of every TCP and UDP socket of tarae's network namespace the kernel lists — bound, listening or
/// connected, in any state (an unbound one isn't listed): what a sweep keeps.
pub fn every() -> io::Result<std::collections::HashSet<u64>> {
    let mut all = std::collections::HashSet::new();
    for tcp in [true, false] {
        all.extend(dump(tcp, None, !0)?.into_iter().filter_map(|r| r.cookie));
    }
    Ok(all)
}

/// The sockets of the protocol (`tcp`, else UDP) in `states`, IPv4 and IPv6, with `port` at that end where given.
fn dump(tcp: bool, port: Option<(u16, End)>, states: u32) -> io::Result<Vec<Row>> {
    // SAFETY: plain call; a descriptor it makes is ours.
    let fd = unsafe {
        libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let second = libc::timeval { tv_sec: 1, tv_usec: 0 };
    // SAFETY: a timeval and its size, live for the call.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const second).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut rows = Vec::new();
    for (seq, family) in [(1, libc::AF_INET), (2, libc::AF_INET6)] {
        let req = request(family as u8, tcp, port, states, seq);
        // SAFETY: the request's bytes, live for the call.
        if unsafe { libc::send(fd.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        dumped(&fd, seq, &mut rows)?;
    }
    Ok(rows)
}

/// A dump of `family`'s sockets of the protocol (`tcp`, else UDP) in `states` — with `port` at that end where given.
fn request(family: u8, tcp: bool, port: Option<(u16, End)>, states: u32, seq: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(92);
    let len: u32 = 16 + 56 + if port.is_some() { 4 + 16 } else { 0 };
    // nlmsghdr
    b.extend(len.to_ne_bytes());
    b.extend(BY_FAMILY.to_ne_bytes());
    b.extend(((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    b.extend(seq.to_ne_bytes());
    b.extend(0u32.to_ne_bytes());
    // inet_diag_req_v2: family, protocol, ext, pad, states, then an empty inet_diag_sockid (48 bytes)
    let protocol = if tcp { libc::IPPROTO_TCP } else { libc::IPPROTO_UDP } as u8;
    b.extend([family, protocol, 0, 0]);
    b.extend(states.to_ne_bytes());
    b.extend([0u8; 48]);
    let Some((port, end)) = port else { return b };
    // The filter: the port ≥ `port` (else past the end: rejected), then ≤ it (else rejected) — each op
    // `{code, yes, no}`, the port in the next op's `no`; a socket is listed when the program ends exactly at its end
    b.extend((4u16 + 16).to_ne_bytes());
    b.extend(REQ_BYTECODE.to_ne_bytes());
    let (ge, le) = match end {
        Local => (BC_S_GE, BC_S_LE),
        Remote => (BC_D_GE, BC_D_LE),
    };
    for (code, no) in [(ge, 16u16 + 4), (le, 8 + 4)] {
        b.extend([code, 8]);
        b.extend(no.to_ne_bytes());
        b.extend([0, 0]);
        b.extend(port.to_ne_bytes());
    }
    debug_assert_eq!(b.len(), len as usize);
    b
}

/// The answers to request `seq` until its `NLMSG_DONE`, each socket a row (`open`: asked in the states a call lands
/// on, for a lookup).
fn dumped(fd: &OwnedFd, seq: u32, rows: &mut Vec<Row>) -> io::Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        // SAFETY: a buffer of ours and its length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut at = &buf[..n as usize];
        while at.len() >= 16 {
            let u32_at = |b: &[u8], i: usize| u32::from_ne_bytes(b[i..i + 4].try_into().unwrap());
            let len = u32_at(at, 0) as usize;
            let kind = u16::from_ne_bytes([at[4], at[5]]);
            if len < 16 || len > at.len() {
                return Err(io::Error::other("sock_diag: a message cut short"));
            }
            if u32_at(at, 8) == seq {
                match kind as i32 {
                    libc::NLMSG_DONE => return Ok(()),
                    libc::NLMSG_ERROR => {
                        let code = i32::from_ne_bytes(at[16..20].try_into().unwrap_or_default());
                        return Err(io::Error::from_raw_os_error(-code));
                    }
                    _ if kind == BY_FAMILY => rows.extend(row(&at[16..len])),
                    _ => {}
                }
            }
            at = &at[len.next_multiple_of(4).min(at.len())..];
        }
    }
}

/// One `inet_diag_msg` and its attributes as a row.
fn row(m: &[u8]) -> Option<Row> {
    if m.len() < MSG {
        return None;
    }
    // (family, state, timer, retrans; the sockid: sport, dport — network order —, src, dst, if, cookie; … inode)
    let (port, peer_port) = (u16::from_be_bytes([m[4], m[5]]), u16::from_be_bytes([m[6], m[7]]));
    let ip = |at: usize| match m[0] as i32 {
        libc::AF_INET => Some(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&m[at..at + 4]).ok()?))),
        libc::AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&m[at..at + 16]).ok()?))),
        _ => None,
    };
    let (ip, peer) = (ip(8)?, ip(24)?);
    let word = |i: usize| m[i..i + 4].try_into().ok().map(u32::from_ne_bytes);
    // (`idiag_cookie`: the socket's `SO_COOKIE`, low word first — `sock_diag_save_cookie`)
    let cookie = u64::from(word(44)?) | (u64::from(word(48)?) << 32);
    let inode = word(68)?;
    let mut v6only = false;
    let mut a = &m[MSG..];
    while a.len() >= 4 {
        let len = u16::from_ne_bytes([a[0], a[1]]) as usize;
        let kind = u16::from_ne_bytes([a[2], a[3]]);
        if len < 4 || len > a.len() {
            break;
        }
        if kind == SKV6ONLY && len >= 5 {
            v6only = a[4] != 0;
        }
        a = &a[len.next_multiple_of(4).min(a.len())..];
    }
    let (local, inode, cookie) = (SocketAddr::new(ip, port), u64::from(inode), Some(cookie));
    let peer = (peer_port != 0).then(|| SocketAddr::new(peer, peer_port));
    Some(Row { local, inode, cookie, open: true, v6only, peer })
}
