//! What a process is connected to — read unprivileged, nothing spawned: macOS's libproc (`proc_pidinfo`'s
//! descriptor list, then `proc_pidfdinfo` for each socket), Linux's `/proc/<pid>/fd` matched to
//! `/proc/<pid>/net/tcp{,6}` (TCP only: a unix socket's peer there takes `sock_diag`, a later step).
//!
//! `stuck.rs` asks it of a quiet run's processes (frictions15 #3): a client whose work goes on elsewhere — a
//! container engine's daemon in a VM or on another host, a build server — prints nothing and uses no CPU
//! here, and isn't stuck. What the run's tree is connected to outside itself (`outside`) is said on the card.

use std::net::SocketAddr;
use std::path::PathBuf;

/// One connected socket of a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sock {
    /// A TCP connection, established: its own end and the other.
    Tcp { local: SocketAddr, peer: SocketAddr },
    /// A connected unix socket: the kernel's id of it and of its peer (the same number in every process for
    /// one socket — how two ends in one tree are told), and the path the peer is bound to, if any.
    Unix { id: u64, peer: u64, path: Option<PathBuf> },
}

/// What the sockets of a tree of processes (`socks`, every process's together) are connected to outside that
/// tree, as said: a peer's path (`~/…`), `10.0.0.5:2376`, or `another program's socket` for an unnamed
/// one — each once, in the order found. A connection between two of the tree's own processes isn't one.
pub fn outside(socks: &[Sock]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in socks {
        let said = match s {
            Sock::Tcp { peer, .. }
                if socks.iter().any(|o| matches!(o, Sock::Tcp { local, .. } if local == peer)) =>
            {
                continue;
            }
            Sock::Tcp { peer, .. } => peer.to_string(),
            Sock::Unix { peer, .. }
                if socks.iter().any(|o| matches!(o, Sock::Unix { id, .. } if id == peer)) =>
            {
                continue;
            }
            Sock::Unix { path: Some(p), .. } => crate::allow::tilde(p),
            Sock::Unix { path: None, .. } => "another program's socket".into(),
        };
        if !out.contains(&said) {
            out.push(said);
        }
    }
    out
}

/// The connected sockets of process `pid` — empty when it's gone, not ours to read, or has none.
#[cfg(target_os = "macos")]
pub fn of(pid: u32) -> Vec<Sock> {
    // (libproc's flavour for a socket's info — not in the libc crate)
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    let pid = pid as i32;
    // SAFETY: a size query — no buffer.
    let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    // (room for a few opened meanwhile)
    let mut fds = vec![libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }; bytes as usize / FD_INFO + 16];
    let room = (fds.len() * FD_INFO) as libc::c_int;
    // SAFETY: the buffer is `room` bytes; the call writes whole `proc_fdinfo`s into it.
    let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), room) };
    fds.truncate((got.max(0) as usize / FD_INFO).min(fds.len()));
    let mut out = Vec::new();
    for fd in fds.iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32) {
        let mut info = SocketFdInfo::zeroed();
        let size = std::mem::size_of::<SocketFdInfo>() as libc::c_int;
        // SAFETY: a plain out-struct of the size given (`sys/proc_info.h`'s `socket_fdinfo`); the call only
        // fills it in.
        let n = unsafe {
            libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDSOCKETINFO, (&raw mut info).cast(), size)
        };
        if n == size
            && let Some(s) = info.sock()
        {
            out.push(s);
        }
    }
    out
}

#[cfg(target_os = "macos")]
const FD_INFO: usize = std::mem::size_of::<libc::proc_fdinfo>();

/// `sys/proc_info.h`'s `struct socket_fdinfo` (792 bytes): `proc_fileinfo` (24), then `socket_info` — its
/// `vinfo_stat` (136), the fields read here, and the per-kind union (`soi_proto`, 528) kept as bytes and read
/// at the header's offsets (`sock`). The own-process test checks the layout against real sockets.
#[cfg(target_os = "macos")]
#[repr(C)]
struct SocketFdInfo {
    pfi: [u8; 24],
    soi_stat: [u8; 136],
    soi_so: u64,
    soi_pcb: u64,
    soi_type: i32,
    soi_protocol: i32,
    soi_family: i32,
    soi_shorts: [i16; 8],
    soi_oobmark: u32,
    soi_rcv: [u8; 24],
    soi_snd: [u8; 24],
    soi_kind: i32,
    rfu_1: u32,
    soi_proto: [u64; 66],
}

#[cfg(target_os = "macos")]
const _: () = assert!(std::mem::size_of::<SocketFdInfo>() == 792);

#[cfg(target_os = "macos")]
impl SocketFdInfo {
    fn zeroed() -> SocketFdInfo {
        // SAFETY: plain integers and byte arrays — all zeroes is a value of each.
        unsafe { std::mem::zeroed() }
    }

    /// The union's bytes.
    fn proto(&self) -> &[u8] {
        // SAFETY: `soi_proto` is 528 initialised bytes; a byte view of it lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.soi_proto.as_ptr().cast::<u8>(), 66 * 8) }
    }

    /// The socket, when it's a connected one this reads: TCP established, unix connected.
    fn sock(&self) -> Option<Sock> {
        // (`SOCKINFO_TCP`, `SOCKINFO_UN`; `TSI_S_ESTABLISHED`)
        const TCP: i32 = 2;
        const UN: i32 = 3;
        const ESTABLISHED: i32 = 4;
        let p = self.proto();
        let u32_at = |at: usize| u32::from_ne_bytes(p[at..at + 4].try_into().expect("4 bytes"));
        let u64_at = |at: usize| u64::from_ne_bytes(p[at..at + 8].try_into().expect("8 bytes"));
        match self.soi_kind {
            TCP => {
                // `tcp_sockinfo`: its `in_sockinfo` (80 bytes) first, then `tcpsi_state`
                if u32_at(80) as i32 != ESTABLISHED {
                    return None;
                }
                // (ports in network order, in an int; `insi_vflag` 1 = IPv4, 2 = IPv6; the addresses at 32
                // (foreign) and 48 (local), 16 bytes each — an IPv4 one in the last 4)
                let port = |at: usize| u16::from_be_bytes([p[at], p[at + 1]]);
                let addr = |at: usize| -> Option<std::net::IpAddr> {
                    match p[24] {
                        1 => Some(
                            std::net::Ipv4Addr::new(p[at + 12], p[at + 13], p[at + 14], p[at + 15]).into(),
                        ),
                        2 => {
                            Some(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&p[at..at + 16]).ok()?).into())
                        }
                        _ => None,
                    }
                };
                Some(Sock::Tcp {
                    local: SocketAddr::new(addr(48)?, port(4)),
                    peer: SocketAddr::new(addr(32)?, port(0)),
                })
            }
            UN => {
                // `un_sockinfo`: `unsi_conn_so`, `unsi_conn_pcb`, then two `sockaddr_un`s of 255 bytes — its
                // own address, then its peer's (`sun_len`, `sun_family`, `sun_path`)
                let peer = u64_at(0);
                if peer == 0 {
                    return None;
                }
                let caddr = &p[16 + 255..16 + 255 + 255];
                let path = caddr[2..].split(|&b| b == 0).next().filter(|s| !s.is_empty());
                let path = path.map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()));
                Some(Sock::Unix { id: self.soi_so, peer, path })
            }
            _ => None,
        }
    }
}

/// The connected sockets of process `pid` — TCP only, from `/proc` (a unix socket's peer needs `sock_diag`).
#[cfg(target_os = "linux")]
pub fn of(pid: u32) -> Vec<Sock> {
    let inodes: Vec<u64> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let link = std::fs::read_link(e.path()).ok()?;
            link.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
        })
        .collect();
    if inodes.is_empty() {
        return Vec::new();
    }
    ["tcp", "tcp6"]
        .iter()
        .filter_map(|t| std::fs::read_to_string(format!("/proc/{pid}/net/{t}")).ok())
        .flat_map(|table| tcp_table(&table, &inodes))
        .collect()
}

/// The established connections in a `/proc/net/tcp{,6}` table whose inode is one of `inodes`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn tcp_table(table: &str, inodes: &[u64]) -> Vec<Sock> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // (sl local rem st tx:rx tr:when retrnsmt uid timeout inode …; st 01 = established)
            let inode: u64 = f.get(9)?.parse().ok()?;
            (*f.get(3)? == "01" && inodes.contains(&inode)).then_some(())?;
            Some(Sock::Tcp { local: hex_addr(f.get(1)?)?, peer: hex_addr(f.get(2)?)? })
        })
        .collect()
}

/// `0100007F:1F90` (IPv4: one 32-bit word in the kernel's byte order) or 32 hex digits (IPv6: four of them),
/// then the port.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn hex_addr(s: &str) -> Option<SocketAddr> {
    let (addr, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| -> Option<[u8; 4]> {
        Some(u32::from_str_radix(addr.get(i * 8..i * 8 + 8)?, 16).ok()?.to_ne_bytes())
    };
    let ip: std::net::IpAddr = match addr.len() {
        8 => std::net::Ipv4Addr::from(word(0)?).into(),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            std::net::Ipv6Addr::from(b).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn of(_pid: u32) -> Vec<Sock> {
    Vec::new()
}

#[cfg(test)]
mod tests;
