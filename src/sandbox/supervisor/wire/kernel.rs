//! The kernel's own reading of an address against `parse`'s (V2 step 4, `target/sb/v2-fuzz-plan.md` §3): the bytes
//! `parse` read handed to the kernel as the supervisor hands it tarae's copy — a UDP socket's `connect` (no packet
//! goes anywhere) and `getpeername`; a unix socket's `bind` in the test's own scratch folder and the file it makes;
//! an abstract or made-up name's `getsockname`. The kernel reads what tarae read, or refuses. Linux only: the
//! numbers and the reading are Linux's.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileTypeExt;

use super::*;
use crate::sandbox::route::this_machine;
use crate::scratch::Scratch;

/// Cases each test draws.
const CASES: usize = 10_000;
/// Where a case's address lies in the stand-in memory `address` reads it from.
const AT: u64 = 0x1000;
/// Disagreements listed at most, after the loop (all of them counted).
const LISTED: usize = 20;

fn socket(family: i32, kind: i32) -> Result<OwnedFd, i32> {
    // SAFETY: plain arguments; the descriptor is owned at once.
    let fd = unsafe { libc::socket(family, kind | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(errno());
    }
    // SAFETY: just made, ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `connect(s, b, len b)`, the address as a run's call names it.
fn connect(s: &OwnedFd, b: &[u8]) -> Result<(), i32> {
    // SAFETY: `b` lives for the call; the kernel reads at most `b.len()` bytes of it.
    match unsafe { libc::connect(s.as_raw_fd(), b.as_ptr().cast(), b.len() as libc::socklen_t) } {
        0 => Ok(()),
        _ => Err(errno()),
    }
}

/// `bind(s, b, len b)`.
fn bind(s: &OwnedFd, b: &[u8]) -> Result<(), i32> {
    // SAFETY: as `connect`'s.
    match unsafe { libc::bind(s.as_raw_fd(), b.as_ptr().cast(), b.len() as libc::socklen_t) } {
        0 => Ok(()),
        _ => Err(errno()),
    }
}

/// The address the kernel says the socket is connected to (`peer`) or bound at, as it wrote it.
fn name(s: &OwnedFd, peer: bool) -> Result<Vec<u8>, i32> {
    let mut a = [0u8; ADDR_MAX];
    let mut len = a.len() as libc::socklen_t;
    // SAFETY: room for `len` bytes, the most the kernel writes.
    let r = unsafe {
        match peer {
            true => libc::getpeername(s.as_raw_fd(), a.as_mut_ptr().cast(), &mut len),
            false => libc::getsockname(s.as_raw_fd(), a.as_mut_ptr().cast(), &mut len),
        }
    };
    match r {
        0 => Ok(a[..(len as usize).min(a.len())].to_vec()),
        _ => Err(errno()),
    }
}

/// An internet address as the kernel writes one (`struct sockaddr_in`, `sockaddr_in6`), read here by hand — not by
/// `parse`, which is what's checked.
fn kernels(a: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes(a.get(2..4)?.try_into().ok()?);
    let ip: IpAddr = match i32::from(u16::from_ne_bytes(a.get(..2)?.try_into().ok()?)) {
        libc::AF_INET => Ipv4Addr::from(<[u8; 4]>::try_from(a.get(4..8)?).ok()?).into(),
        libc::AF_INET6 => Ipv6Addr::from(<[u8; 16]>::try_from(a.get(8..24)?).ok()?).into(),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// What tarae reads of `b` as a call's address: read once (`address`, at most 128 bytes), then `parse`.
fn tarae_reads(b: &[u8]) -> Result<Addr, i32> {
    address(&Fake::default().at(AT, b.to_vec()), AT, b.len() as u64).and_then(|a| parse(&a))
}

/// A `struct sockaddr_in6`: port, no flow label, the address, `scope` (its last four bytes).
fn v6(ip: Ipv6Addr, port: u16, scope: u32) -> Vec<u8> {
    let rest = [&port.to_be_bytes()[..], &[0; 4], &ip.octets(), &scope.to_ne_bytes()].concat();
    sockaddr(AF_INET6, &rest)
}

/// The counts by kind and the disagreements, the first `LISTED` of them in words.
#[derive(Default)]
struct Tally {
    seen: BTreeMap<String, usize>,
    wrong: Vec<String>,
    wrongs: usize,
}

impl Tally {
    fn wrong(&mut self, what: String) {
        self.wrongs += 1;
        if self.wrong.len() < LISTED {
            self.wrong.push(what);
        }
    }

    /// No disagreement, and each of `kinds` seen at least `floor` times — so the loop reached what it checks.
    fn check(&self, kinds: &[&str], floor: usize) {
        assert!(
            self.wrongs == 0,
            "{} disagreements, the first:\n{}\nseen: {:?}",
            self.wrongs,
            self.wrong.join("\n"),
            self.seen
        );
        let short: Vec<&&str> =
            kinds.iter().filter(|k| self.seen.get(**k).is_none_or(|&n| n < floor)).collect();
        assert!(short.is_empty(), "under {floor} of {short:?}: {:?}", self.seen);
    }
}

/// Random addresses as a run's `connect` names them on a UDP socket of each family (nothing is sent): where the
/// kernel connects is what tarae read — the same port and address, an IPv4 one on an IPv6 socket as the mapped one
/// it is (`route::plain`), `0.0.0.0` and `::` (mapped too) as loopback, which `this_machine` takes them for — or
/// it connects nowhere (`getpeername`'s `ENOTCONN`) where tarae read `AF_UNSPEC` or port 0, or it refuses. What
/// tarae refuses (too short for its family, past 128 bytes) the kernel refuses too. Seeded, 10,000 addresses on
/// each socket; IPv6 where the kernel has it (else said).
#[test]
fn the_kernel_connects_where_tarae_read_the_address() {
    let seeds = [
        v4([127, 0, 0, 1], 9),
        v4([127, 0, 0, 1], 0),
        v4([127, 1, 2, 3], 53),
        v4([0, 0, 0, 0], 9),
        v4([224, 0, 0, 1], 5353),
        v4([192, 0, 2, 1], 443),
        [v4([127, 0, 0, 1], 9), vec![0; 110]].concat(),
        v6(Ipv6Addr::LOCALHOST, 9, 0),
        v6(Ipv6Addr::LOCALHOST, 0, 0),
        v6(Ipv6Addr::UNSPECIFIED, 9, 0),
        v6(Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 9, 0),
        v6(Ipv4Addr::UNSPECIFIED.to_ipv6_mapped(), 9, 0),
        v6("fe80::1".parse().unwrap(), 9, 1),
        v6("2001:db8::1".parse().unwrap(), 9, 0),
        v6(Ipv6Addr::LOCALHOST, 9, 0)[..24].to_vec(),
        sockaddr(AF_UNSPEC, &[0; 14]),
        sockaddr(AF_UNIX, b"/x\0"),
        sockaddr(AF_NETLINK, &[0; 10]),
    ];
    let mut families = vec![(libc::AF_INET, "v4")];
    match socket(libc::AF_INET6, libc::SOCK_DGRAM).and_then(|s| connect(&s, &v6(Ipv6Addr::LOCALHOST, 9, 0))) {
        Ok(()) => families.push((libc::AF_INET6, "v6")),
        Err(e) => eprintln!("no IPv6 loopback here (errno {e}): IPv6 sockets left out"),
    }
    let mut rng = Rng(0x5eed_0add_7e55_0004);
    let mut t = Tally::default();
    for _ in 0..CASES {
        let mut b = seeds[rng.below(seeds.len())].clone();
        mutate(&mut rng, &mut b, &[]);
        let read = tarae_reads(&b);
        for &(family, on) in &families {
            let s = socket(family, libc::SOCK_DGRAM).unwrap();
            let what = match (connect(&s, &b), &read) {
                (Err(_), Err(_)) => "both refuse",
                (Err(_), Ok(_)) => "the kernel refuses",
                (Ok(()), Err(e)) => {
                    t.wrong(format!("{on}: tarae refused ({e}) what the kernel took: {b:?}"));
                    continue;
                }
                (Ok(()), Ok(ours)) => match (ours, name(&s, true)) {
                    (Addr::Unspec, Err(libc::ENOTCONN)) => "disconnected",
                    (Addr::Ip(a), Err(libc::ENOTCONN)) if a.port() == 0 => "port 0",
                    (Addr::Ip(a), Ok(peer)) => {
                        let Some(peer) = kernels(&peer) else {
                            t.wrong(format!("{on}: tarae read {a}, the kernel's peer {peer:?}: {b:?}"));
                            continue;
                        };
                        let (mine, its) = (a.ip().to_canonical(), peer.ip().to_canonical());
                        match a.port() == peer.port() {
                            true if mine == its => "the same",
                            true if mine.is_unspecified() && its.is_loopback() && this_machine(a.ip()) => {
                                "this machine"
                            }
                            _ => {
                                t.wrong(format!(
                                    "{on}: tarae read {a}, the kernel connected to {peer}: {b:?}"
                                ));
                                continue;
                            }
                        }
                    }
                    (ours, peer) => {
                        t.wrong(format!(
                            "{on}: tarae read {ours:?}, the kernel connected, its peer {peer:?}: {b:?}"
                        ));
                        continue;
                    }
                },
            };
            *t.seen.entry(format!("{what} {on}")).or_default() += 1;
        }
    }
    let kinds = ["both refuse", "the kernel refuses", "disconnected", "port 0", "the same", "this machine"];
    let kinds: Vec<String> = families.iter().flat_map(|(_, on)| kinds.map(|k| format!("{k} {on}"))).collect();
    t.check(&kinds.iter().map(String::as_str).collect::<Vec<_>>(), 50);
}

/// Random unix addresses as a run's `bind` names them: a path is where the kernel makes the socket — the bytes up
/// to the first NUL, in the test's own scratch folder —, an abstract name (under a prefix of the test's own) is the
/// one `getsockname` says, the family alone is the kernel's own made-up name (a NUL and five hex digits); else the
/// kernel refuses and makes nothing. Seeded, 10,000 addresses. Only a name's bytes are mutated, never the folder's
/// or the prefix, and a `/` in them becomes `_`: nothing is made outside the folder, nor at a path relative to the
/// test's.
#[test]
fn the_kernel_binds_a_unix_socket_where_tarae_read_it() {
    let dir = Scratch::new("wire-kernel");
    let folder = [dir.as_os_str().as_bytes(), b"/"].concat();
    assert!(folder.len() <= 60, "{} leaves too little of a unix name's 108 bytes", dir.display());
    let prefix = format!("\0tarae-wire-{}-", std::process::id()).into_bytes();
    let names: [&[u8]; 6] = [b"s.sock", b"s.sock\0junk", b"\xff\xfe.sock", b"a", &[b'n'; 40], &[b'n'; 107]];
    let mut rng = Rng(0x5eed_0add_7e55_0005);
    let mut t = Tally::default();
    for _ in 0..CASES {
        let mut n = names[rng.below(names.len())].to_vec();
        mutate(&mut rng, &mut n, &[]);
        n.iter_mut().filter(|c| **c == b'/').for_each(|c| *c = b'_');
        let b = match rng.below(5) {
            0 => sockaddr(AF_UNIX, b""),
            1 | 2 => sockaddr(AF_UNIX, &[&prefix[..], &n].concat()),
            _ => sockaddr(AF_UNIX, &[&folder[..], &n].concat()),
        };
        let s = socket(libc::AF_UNIX, libc::SOCK_STREAM).unwrap();
        let bound = bind(&s, &b);
        let made: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
        let what = match (bound, tarae_reads(&b)) {
            (Err(_), _) if made.is_empty() => "refused",
            (Err(e), _) => {
                t.wrong(format!("the kernel refused ({e}) and made {made:?}: {b:?}"));
                ""
            }
            (Ok(()), Ok(Addr::Path(p))) => match &made[..] {
                [one]
                    if *one == p
                        && std::fs::symlink_metadata(one).is_ok_and(|m| m.file_type().is_socket()) =>
                {
                    "path"
                }
                _ => {
                    t.wrong(format!("tarae read {p:?}, the kernel made {made:?}: {b:?}"));
                    ""
                }
            },
            (Ok(()), Ok(Addr::Abstract(ours))) => match name(&s, false) {
                Ok(its) if made.is_empty() && its.get(2) == Some(&0) && its[3..] == ours => "abstract",
                its => {
                    t.wrong(format!("tarae read @{ours:?}, the kernel bound {its:?}, made {made:?}: {b:?}"));
                    ""
                }
            },
            (Ok(()), Ok(Addr::Unnamed)) => match name(&s, false) {
                Ok(its)
                    if its.len() == 8 && its[2] == 0 && its[3..].iter().all(|c| c.is_ascii_hexdigit()) =>
                {
                    "made up"
                }
                its => {
                    t.wrong(format!("tarae read no name, the kernel bound {its:?}: {b:?}"));
                    ""
                }
            },
            (Ok(()), ours) => {
                t.wrong(format!("tarae read {ours:?}, the kernel bound, made {made:?}: {b:?}"));
                ""
            }
        };
        drop(s);
        // (each case starts from an empty folder: what the kernel made is the test's own, in it)
        made.iter().for_each(|m| std::fs::remove_file(m).unwrap());
        if !what.is_empty() {
            *t.seen.entry(what.into()).or_default() += 1;
        }
    }
    t.check(&["refused", "path", "abstract", "made up"], 300);
}
