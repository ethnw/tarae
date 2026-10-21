//! The session's transparent proxy, as its keeper's birth makes it (step 6c, `target/sb/step6-plan.md` §6c): where
//! the keeper has a network namespace of its own (tier own) and the probe's batch landed (`tier::Probed.redirect`),
//! tarae — not the keeper — sends the session's routes and one owned nf_tables table (`tarae-netlink`) over netlink
//! sockets the keeper opened in that netns before any run, and makes the keeper's planted sockets transparent. From
//! then on a TCP connection or UDP datagram leaving the session for anything but loopback and the session's own
//! addresses is taken by prerouting's tproxy rules to a planted socket; what's neither TCP nor UDP is dropped there.
//!
//! The ruleset is tarae's — the crate knows nf_tables, tarae knows the session (`ruleset`, `routes`), held against
//! what nft and ip sent for the same.
//!
//! From 6d, a served run's connect beyond is made on its own socket, marked by its supervisor as its run's
//! (`marks.rs`; `route::Route::Redirected`). The rules keep that mark — output adds the route rule's bit to it,
//! prerouting sets none — and the planted socket reads it back (`tcp_fwmark_accept`): `Taken` hands a connection whose
//! mark tarae gave to the egress proxy (`proxy::Planted`), which decides its host as a CONNECT's. Anything else that
//! lands there is refused and said in the log: a connect to the planted address itself, a mark tarae didn't give.
//!
//! What's still refused there, until 6e (UDP, DNS): datagrams, and the connections of a run tarae doesn't serve — a
//! run without the supervisor (offline, in the session's namespaces), or a test's own-sockets run — which used to fail at once beyond loopback (no
//! route: `ENETUNREACH`). `Taken` makes the refusal as quick and as plain as a closed port's: a connection is accepted
//! and reset (`ECONNRESET` on its first read or write), a datagram answered with ICMP port unreachable from the raw
//! socket the birth made in the session's netns (`Kind::Icmp4`) — a connected UDP socket, glibc's resolver's among
//! them, gets `ECONNREFUSED` at once rather than waiting out its timeout. Pinned by `an_unserved_runs_way_out_fails_fast`
//! (keeper.rs) — whose lookup half 6e flips: names answered there by tarae. (IPv6 datagrams aren't answered yet: no
//! ICMPv6 socket — these machines' sessions carry no IPv6 to test it on.)
//!
//! Any refusal while making it ends the session's start — never a retry (review, 2026-10-08): route changes are
//! partial and none replaces what's there, so a retry in that netns would meet its own half; a half-made network is
//! never a session's. Its keeper and netns are thrown away (`Keeper::born`).
// (elsewhere only the tests build it)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tarae_netlink::nft::{self, Base, ChainKind, Expr, Hook, MetaKey, Reg, SocketKey, Transaction, Verdict};
use tarae_netlink::route::{self, Changes, RouteKind};
use tarae_netlink::{Batch, Family};

/// The address the session's `lo` carries beside `127.0.0.1` where tarae's network has IPv4 (`keeper::Families`; as
/// `lo:1`, a /32): a documentation address, local to the session's netns. Without an IPv4 address other than
/// loopback, glibc's `AI_ADDRCONFIG` reads "no IPv4 here" and an IPv4-only lookup comes back empty (M5,
/// `target/sb/step4-notes.md`). The planted sockets are at it (`Kind::at`).
pub const SESSION_V4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// `SESSION_V4`'s IPv6 twin (a /128 on `lo`), where tarae's network has IPv6 beyond loopback and link-local.
pub const SESSION_V6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

/// The owned table's name.
pub const TABLE: &str = "tarae";
/// Where the planted TCP sockets listen and the planted UDP sockets are bound, at the session's own addresses.
pub const TCP_PORT: u16 = 15001;
pub const UDP_PORT: u16 = 15053;
/// The mark's bit the output chain adds to what leaves (`keep_mark`) — what sends it to table 100 — and that table.
/// A socket's own mark (6d: tarae's, its run's — `marks.rs`) never carries it: a socket marked with it would meet the
/// route rule at its first lookup, before the output chain, and IPv4 would take the destination itself as its source
/// (M12, `target/sb/step6d-design.md`).
const MARK: u32 = 1;
const MARKED: u8 = 100;
/// `lo`'s index in every network namespace (the kernel's `LOOPBACK_IFINDEX`): no `RTM_GETLINK` needed.
const LO: u32 = 1;

const TCP: u8 = 6;
const UDP: u8 = 17;

/// A socket the keeper makes for the session's network (`Msg::Planted`'s kinds, each its byte): the two netlink
/// sockets tarae sends on, the planted ones, a DNS one a family (6e: unbound — tarae binds it at port 53, a port below
/// what a capless keeper may bind) — and a raw ICMP one the birth made before the keeper's capability drop (it needs
/// `CAP_NET_RAW` to be made), for `Taken`'s answers; where the birth couldn't make it, none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Route,
    Netfilter,
    Tcp4,
    Tcp6,
    Udp4,
    Udp6,
    Icmp4,
    Dns4,
    Dns6,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Route,
        Kind::Netfilter,
        Kind::Tcp4,
        Kind::Tcp6,
        Kind::Udp4,
        Kind::Udp6,
        Kind::Icmp4,
        Kind::Dns4,
        Kind::Dns6,
    ];

    pub fn code(self) -> u8 {
        Kind::ALL.iter().position(|k| *k == self).unwrap_or(0) as u8
    }

    pub fn from_code(code: u8) -> Option<Kind> {
        Kind::ALL.get(usize::from(code)).copied()
    }

    /// Where a planted one is: the session's own address of its family, at its protocol's port — None: a netlink one.
    pub fn at(self) -> Option<SocketAddr> {
        let (ip, port) = match self {
            Kind::Route | Kind::Netfilter | Kind::Icmp4 | Kind::Dns4 | Kind::Dns6 => return None,
            Kind::Tcp4 => (IpAddr::V4(SESSION_V4), TCP_PORT),
            Kind::Tcp6 => (IpAddr::V6(SESSION_V6), TCP_PORT),
            Kind::Udp4 => (IpAddr::V4(SESSION_V4), UDP_PORT),
            Kind::Udp6 => (IpAddr::V6(SESSION_V6), UDP_PORT),
        };
        Some(SocketAddr::new(ip, port))
    }

    /// The kinds the keeper makes where its `lo` carries `v4` and `v6`'s addresses: both netlink sockets, and the
    /// planted and DNS ones of those families (`Icmp4` comes as well where the birth made it).
    pub fn wanted(v4: bool, v6: bool) -> Vec<Kind> {
        let mut k = vec![Kind::Route, Kind::Netfilter];
        if v4 {
            k.extend([Kind::Tcp4, Kind::Udp4, Kind::Dns4]);
        }
        if v6 {
            k.extend([Kind::Tcp6, Kind::Udp6, Kind::Dns6]);
        }
        k
    }

    /// Where a DNS one is bound (by tarae): the session's own address of its family, port 53 — not every address:
    /// that would hold port 53 at loopback too, where a run's own server may bind (6e review). Questions to other
    /// addresses reach the planted UDP socket; the answer goes out on this one, its source the address asked
    /// (`IP_PKTINFO`) and its port 53. None: not a DNS one.
    pub fn dns_at(self) -> Option<SocketAddr> {
        let port = crate::sandbox::route::RESOLVER_PORT;
        match self {
            Kind::Dns4 => Some(SocketAddr::new(IpAddr::V4(SESSION_V4), port)),
            Kind::Dns6 => Some(SocketAddr::new(IpAddr::V6(SESSION_V6), port)),
            _ => None,
        }
    }
}

/// What tarae sends to make the session's network: the routes (v4, then v6) and the ruleset.
pub struct Network {
    pub routes: Vec<Batch>,
    pub rules: Batch,
    /// The search list the session's runs' resolver uses (`/etc/resolv.conf`'s, read at the birth — the runs see
    /// this machine's): what the session's answering reads a name's candidates by (6e, E6).
    pub search: Vec<String>,
    /// Tests: the planted UDP sockets made as on a kernel before 5.19 — no `SO_RCVMARK` (D5; E5's stand-in on a
    /// kernel that has it).
    #[cfg(test)]
    pub no_rcvmark: bool,
}

impl Network {
    /// The session's own, for the families its `lo` carries (`v4`, `v6`: `keeper::Families`, less what it couldn't
    /// take): a family not carried gets no routes and no rules — no planted socket of its family would take what
    /// they sent there, so its connects beyond would wait out their time limits where without a route they fail at
    /// once (`ENETUNREACH`), as before.
    pub fn session(v4: bool, v6: bool) -> Network {
        let carried = [(Family::V4, v4), (Family::V6, v6)];
        let routes = carried.iter().filter(|(_, c)| *c).map(|(f, _)| routes(*f)).collect();
        let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        Network {
            routes,
            rules: ruleset(v4, v6),
            search: crate::sandbox::route::search(&conf),
            #[cfg(test)]
            no_rcvmark: false,
        }
    }
}

/// The routes TPROXY needs for `family`: everything leaving by `lo` (main), and a marked packet taken as this
/// machine's own (`fwmark 0x1/0x1 lookup 100`, `local default dev lo table 100`). The rule looks at `MARK`'s bit
/// alone (6d): the output chain adds it to a run's mark, whose other bits are its own (M12 measured this rule).
pub fn routes(family: Family) -> Batch {
    let mut c = Changes::new();
    c.default_route(family, route::MAIN, RouteKind::Unicast, LO);
    c.fwmark_rule(family, MARK, Some(MARK), MARKED);
    c.default_route(family, MARKED, RouteKind::Local, LO);
    c.end()
}

// ── the ruleset's expressions, as nft writes them ──────────────────────────────

fn then(mut e: Vec<Expr>, more: impl IntoIterator<Item = Expr>) -> Vec<Expr> {
    e.extend(more);
    e
}

fn nfproto(f: Family) -> Vec<Expr> {
    vec![
        Expr::Meta { key: MetaKey::NfProto, dreg: Reg::R1 },
        Expr::Cmp { sreg: Reg::R1, data: vec![f.number()] },
    ]
}

fn l4proto(p: u8) -> Vec<Expr> {
    vec![Expr::Meta { key: MetaKey::L4Proto, dreg: Reg::R1 }, Expr::Cmp { sreg: Reg::R1, data: vec![p] }]
}

fn udp_dport(p: u16) -> Vec<Expr> {
    dport(UDP, p)
}

fn dport(proto: u8, p: u16) -> Vec<Expr> {
    then(
        l4proto(proto),
        [
            Expr::Payload { base: Base::Transport, offset: 2, len: 2, dreg: Reg::R1 },
            Expr::Cmp { sreg: Reg::R1, data: nft::port(p) },
        ],
    )
}

/// The destination's first `bytes` bytes are `a`'s (nft writes a whole-byte prefix as a shorter load).
fn daddr(a: IpAddr, bytes: u32) -> Vec<Expr> {
    let (family, offset) = match a {
        IpAddr::V4(_) => (Family::V4, 16),
        IpAddr::V6(_) => (Family::V6, 24),
    };
    then(
        nfproto(family),
        [
            Expr::Payload { base: Base::Network, offset, len: bytes, dreg: Reg::R1 },
            Expr::Cmp { sreg: Reg::R1, data: nft::address(a)[..bytes as usize].to_vec() },
        ],
    )
}

/// The packet's mark kept, `MARK` added (`meta mark set meta mark or 0x1`): what a run's socket carries (6d) stays,
/// and an unmarked one leaves with `MARK` alone.
fn keep_mark() -> Vec<Expr> {
    vec![
        Expr::Meta { key: MetaKey::Mark, dreg: Reg::R1 },
        Expr::Bitwise { sreg: Reg::R1, dreg: Reg::R1, mask: nft::mark(!MARK), xor: nft::mark(MARK) },
        Expr::MetaSet { key: MetaKey::Mark, sreg: Reg::R1 },
    ]
}

fn transparent(is: u8) -> Vec<Expr> {
    vec![
        Expr::Socket { key: SocketKey::Transparent, dreg: Reg::R1 },
        Expr::Cmp { sreg: Reg::R1, data: vec![is] },
    ]
}

/// Handed to the transparent socket at `to`:`port`, accepted, its mark as it came — for `family`'s packets matching
/// `first`.
fn tproxied(family: Family, first: Vec<Expr>, to: IpAddr, port: u16) -> Vec<Expr> {
    let tproxy = [
        Expr::Immediate { dreg: Reg::R1, data: nft::address(to) },
        Expr::Immediate { dreg: Reg::R2, data: nft::port(port) },
        Expr::Tproxy { family, addr: Reg::R1, port: Reg::R2 },
    ];
    then(then(then(nfproto(family), first), tproxy), [Expr::Verdict(Verdict::Accept)])
}

/// The session's whole ruleset, one transaction: the owned table (it goes with tarae's socket — P4), the output
/// `route` chain and the prerouting `filter` chain, t1's rules with the sets split into one rule per element, and a
/// last rule in prerouting that drops what no rule took. Every table and chain as nft's `create` (`EXCL`): one there
/// already is refused, never adopted. A family's own rules (its addresses, its tproxy) only where it's carried
/// (`v4`, `v6`); with both, it's the batch nft sent for it, byte for byte (r6e2's `create-batch-6e2`).
///
/// The mark (6d): output keeps the one a packet's socket carries — its run's, tarae's to give — and adds `MARK`;
/// prerouting sets none, so the planted socket reads the run's (`tcp_fwmark_accept`, a datagram's `SO_RCVMARK`).
///
/// DNS (6e): a question to any address's port 53 — UDP or TCP, the loopback resolvers' too (`127.0.0.53`) — is the
/// session's to answer: taken before the loopback's return, to tarae's DNS socket or, over TCP, the planted TCP one
/// (whose original port says it's DNS) — but a question to an address and port 53 that a socket of the session's own
/// is bound to (not transparent: not tarae's) is delivered to it, a run's own DNS server reached as before 6e.
pub fn ruleset(v4: bool, v6: bool) -> Batch {
    let (s4, s6) = (IpAddr::V4(SESSION_V4), IpAddr::V6(SESSION_V6));
    let (lo4, lo6) = (IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST));
    let ret = || Expr::Verdict(Verdict::Return);
    let accept = || Expr::Verdict(Verdict::Accept);
    let (both, f4, f6) = (None, Some(Family::V4), Some(Family::V6));
    let mut t = Transaction::new();
    t.table(TABLE, true);
    t.chain(TABLE, "out", Hook::Output, -150, ChainKind::Route);
    t.chain(TABLE, "pre", Hook::Prerouting, -150, ChainKind::Filter);
    // (each rule with the family it's for — None: both)
    // What leaves: DNS (UDP and TCP) marked and let go as it is (its own tproxy rules take it back in); loopback and the
    // session's addresses left alone; the rest of TCP and UDP marked — routed through `lo` into prerouting
    let out = [
        (both, then(then(udp_dport(53), keep_mark()), [ret()])),
        (both, then(then(dport(TCP, 53), keep_mark()), [ret()])),
        (f4, then(daddr(lo4, 1), [ret()])),
        (f4, then(daddr(s4, 4), [ret()])),
        (f6, then(daddr(lo6, 16), [ret()])),
        (f6, then(daddr(s6, 16), [ret()])),
        (both, then(l4proto(TCP), keep_mark())),
        (both, then(l4proto(UDP), keep_mark())),
    ];
    // What comes in: a packet of a connection a transparent socket already holds stays with it; DNS to the
    // planted UDP sockets, over TCP to the planted TCP ones; loopback and the session's own addresses left alone; the rest of TCP and UDP to the
    // planted sockets; and what no rule took — neither TCP nor UDP — dropped. Each with the mark it came with
    let pre = [
        (both, then(then(l4proto(TCP), transparent(1)), [accept()])),
        (both, then(then(l4proto(UDP), transparent(1)), [accept()])),
        // (a question to port 53 that a socket of the session's own — not transparent: not tarae's DNS one — is
        // bound to is that server's: a run's own DNS server, reached as before 6e)
        (both, then(then(udp_dport(53), transparent(0)), [accept()])),
        (both, then(then(dport(TCP, 53), transparent(0)), [accept()])),
        (f4, tproxied(Family::V4, udp_dport(53), s4, UDP_PORT)),
        (f6, tproxied(Family::V6, udp_dport(53), s6, UDP_PORT)),
        (f4, tproxied(Family::V4, dport(TCP, 53), s4, TCP_PORT)),
        (f6, tproxied(Family::V6, dport(TCP, 53), s6, TCP_PORT)),
        (f4, then(daddr(lo4, 1), [ret()])),
        (f4, then(daddr(s4, 4), [ret()])),
        (f6, then(daddr(lo6, 16), [ret()])),
        (f6, then(daddr(s6, 16), [ret()])),
        (f4, tproxied(Family::V4, l4proto(TCP), s4, TCP_PORT)),
        (f6, tproxied(Family::V6, l4proto(TCP), s6, TCP_PORT)),
        (f4, tproxied(Family::V4, l4proto(UDP), s4, UDP_PORT)),
        (f6, tproxied(Family::V6, l4proto(UDP), s6, UDP_PORT)),
        (both, vec![Expr::Verdict(Verdict::Drop)]),
    ];
    let carried = |f: &Option<Family>| match f {
        None => true,
        Some(Family::V4) => v4,
        Some(Family::V6) => v6,
    };
    for (_, r) in out.iter().filter(|(f, _)| carried(f)) {
        t.rule(TABLE, "out", r);
    }
    for (_, r) in pre.iter().filter(|(f, _)| carried(f)) {
        t.rule(TABLE, "pre", r);
    }
    t.end()
}

#[cfg(target_os = "linux")]
pub use linux::{Made, make, netns_of_pid, open};
#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::{address as socket_address, checksum, icmp_filter};

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tarae_netlink::{Error, Protocol, Socket};

    use super::{Kind, Network};
    use crate::sandbox::marks::Marks;
    use crate::sandbox::proxy::{self, answering, dns};
    use crate::sandbox::tier::{BirthError, BirthStep};

    /// How long tarae waits for the kernel's answers to each change while making the session's network.
    const ANSWERED_WITHIN: Duration = Duration::from_secs(2);
    /// `SIOCGSKNS` (`linux/sockios.h`): a descriptor of the network namespace a socket is in.
    const SIOCGSKNS: libc::c_ulong = 0x894C;
    /// `linux/in6.h`'s, not in every libc: the IPv6 twins of `IP_TRANSPARENT` and `IP_RECVORIGDSTADDR` — each
    /// `RECVORIGDSTADDR` also the type of the control message that carries a datagram's original destination.
    const IPV6_TRANSPARENT: i32 = 75;
    const IPV6_RECVORIGDSTADDR: i32 = 74;
    const IP_RECVORIGDSTADDR: i32 = 20;

    /// The session's network, made: tarae's two netlink sockets into its netns — the owned table goes with `nf`
    /// (P4), so both are kept for the session's life and let go with its keeper (`Keeper::gone`) — what serves what
    /// lands on the planted sockets (`Taken`: a connection tarae marked to the egress proxy, the rest refused), and
    /// the session's marks its runs' supervisors give (6d).
    pub struct Made {
        #[cfg_attr(not(test), allow(dead_code))]
        pub route: Socket,
        #[cfg_attr(not(test), allow(dead_code))]
        pub nf: Socket,
        #[cfg_attr(not(test), allow(dead_code))]
        pub taken: Taken,
        /// Which run a connection is, by the mark its socket carries (6d): given by the runs' supervisors, read by
        /// `Taken`.
        pub marks: Arc<Marks>,
        /// The session's names (6e): its map, its runs' search list — what the egress proxy answers its questions
        /// with and reads a connection's host from (`Taken`'s hooks hold them; the tests read them here).
        #[cfg_attr(not(test), allow(dead_code))]
        pub names: Arc<answering::Session>,
        /// The raw ICMP socket the birth made in the session's netns (`Kind::Icmp4`), where it could: `Taken`
        /// answers refused datagrams on it, and the tests send on it (`what_isnt_tcp_or_udp_meets_the_final_drop`).
        #[cfg_attr(not(test), allow(dead_code))]
        pub icmp: Option<Arc<OwnedFd>>,
    }

    /// A socket of `kind` for the session's network, made in this netns (the keeper's, before any run —
    /// `Msg::Plant`): a netlink one, or a planted one — a TCP listener or a bound UDP socket at `kind.at()`, an
    /// IPv6 one IPv6 only — or a DNS one, a UDP socket left unbound. None of it needs a capability: what does (tarae's
    /// sends, `IP_TRANSPARENT`, binding port 53) is tarae's, with its own over this netns.
    pub fn open(kind: Kind) -> io::Result<OwnedFd> {
        let (domain, ty, proto) = match kind {
            Kind::Route => (libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE),
            Kind::Netfilter => (libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_NETFILTER),
            Kind::Tcp4 => (libc::AF_INET, libc::SOCK_STREAM, 0),
            Kind::Tcp6 => (libc::AF_INET6, libc::SOCK_STREAM, 0),
            Kind::Udp4 | Kind::Dns4 => (libc::AF_INET, libc::SOCK_DGRAM, 0),
            Kind::Udp6 | Kind::Dns6 => (libc::AF_INET6, libc::SOCK_DGRAM, 0),
            // (it takes `CAP_NET_RAW`, which the keeper dropped: only its birth makes it — `Birth::child`)
            Kind::Icmp4 => return Err(io::Error::from_raw_os_error(libc::EPERM)),
        };
        // SAFETY: plain calls; a descriptor made is ours.
        let fd = unsafe { libc::socket(domain, ty | libc::SOCK_CLOEXEC, proto) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just made.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let Some(at) = kind.at() else { return Ok(fd) };
        // SAFETY: an int and an address of the kernel's layout, ours for each call, on our socket.
        unsafe {
            let one: libc::c_int = 1;
            if domain == libc::AF_INET6
                && libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&raw const one).cast(),
                    4,
                ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            let (sa, len) = address(at);
            if libc::bind(fd.as_raw_fd(), (&raw const sa).cast(), len) != 0 {
                return Err(io::Error::last_os_error());
            }
            if ty == libc::SOCK_STREAM && libc::listen(fd.as_raw_fd(), 128) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(fd)
    }

    /// `a` as the kernel takes it, and its length.
    pub(crate) fn address(a: std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
        // SAFETY: all zeros is a valid sockaddr_storage; each arm writes its family's struct, which fits in it.
        unsafe {
            let mut ss: libc::sockaddr_storage = std::mem::zeroed();
            match a {
                std::net::SocketAddr::V4(v4) => {
                    let s = &mut *(&raw mut ss).cast::<libc::sockaddr_in>();
                    s.sin_family = libc::AF_INET as libc::sa_family_t;
                    s.sin_port = v4.port().to_be();
                    s.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
                    (ss, size_of::<libc::sockaddr_in>() as libc::socklen_t)
                }
                std::net::SocketAddr::V6(v6) => {
                    let s = &mut *(&raw mut ss).cast::<libc::sockaddr_in6>();
                    s.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                    s.sin6_port = v6.port().to_be();
                    s.sin6_addr.s6_addr = v6.ip().octets();
                    (ss, size_of::<libc::sockaddr_in6>() as libc::socklen_t)
                }
            }
        }
    }

    /// The network namespace process `pid` is in, as (device, inode) — tarae reads its keeper's: non-dumpable, but
    /// its user namespace is tarae's, so tarae may (`ptrace_may_access`'s capability over its mm's user namespace).
    pub fn netns_of_pid(pid: u32) -> io::Result<(u64, u64)> {
        netns_at(Path::new(&format!("/proc/{pid}/ns/net")))
    }

    fn netns_at(p: &Path) -> io::Result<(u64, u64)> {
        let m = std::fs::metadata(p)?;
        Ok((m.dev(), m.ino()))
    }

    /// The network namespace socket `fd` is in (`SIOCGSKNS`), as (device, inode) — `EPERM` without
    /// `CAP_NET_ADMIN` over it.
    fn netns_of(fd: BorrowedFd) -> io::Result<(u64, u64)> {
        // SAFETY: an ioctl on a descriptor we hold; the descriptor it makes is ours.
        let ns = unsafe { libc::ioctl(fd.as_raw_fd(), SIOCGSKNS as _) };
        if ns < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just made, ours.
        let m = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(ns) }).metadata()?;
        Ok((m.dev(), m.ino()))
    }

    fn errno(e: &io::Error) -> i32 {
        e.raw_os_error().unwrap_or(libc::EIO)
    }

    fn errno_of(e: &Error) -> i32 {
        match e {
            Error::Kernel { errno, .. } => *errno,
            Error::Io(e) => errno(e),
            Error::Malformed(_) => libc::EPROTO,
            Error::Timeout => libc::ETIMEDOUT,
        }
    }

    /// `step` failed with `errno`: said in the log with what the kernel or tarae saw (`detail`), as the birth's error.
    fn failed(step: BirthStep, errno: i32, detail: &str) -> BirthError {
        crate::log::line("sandbox", "warn", &format!("the session's network: {}: {detail}", step.said()));
        BirthError { step, errno }
    }

    /// The session's network made from the sockets its keeper handed (`Keeper::plant`), each checked to be in
    /// `session` — the netns its runs are in — before anything is sent (P5); then the planted sockets made
    /// transparent, the routes and the ruleset sent (`network`), each answered or refused within
    /// `ANSWERED_WITHIN`. Any refusal is the birth's error, with its step: the caller throws the keeper and its
    /// netns away — never a retry.
    pub fn make(
        session: (u64, u64),
        handed: Vec<(Kind, OwnedFd)>,
        network: &Network,
    ) -> Result<Made, BirthError> {
        let own = netns_at(Path::new("/proc/self/ns/net"))
            .map_err(|e| failed(BirthStep::Netns, errno(&e), &format!("tarae's own netns: {e}")))?;
        if session == own {
            return Err(failed(BirthStep::Netns, libc::EPERM, "the session's netns is tarae's own"));
        }
        let (mut route, mut nf, mut planted, mut icmp) = (None, None, Vec::new(), None);
        for (kind, fd) in handed {
            let ns = match kind {
                Kind::Icmp4 => {
                    let ns = netns_of(fd.as_fd());
                    icmp = Some(Arc::new(fd));
                    ns
                }
                Kind::Route | Kind::Netfilter => {
                    let protocol = if kind == Kind::Route { Protocol::Route } else { Protocol::Netfilter };
                    let s = Socket::from_fd(fd, protocol).map_err(|e| {
                        failed(BirthStep::Planted, errno(&e), &format!("its {kind:?} socket: {e}"))
                    })?;
                    let ns = s.netns();
                    match kind {
                        Kind::Route => route = Some(s),
                        _ => nf = Some(s),
                    }
                    ns
                }
                _ => {
                    let ns = netns_of(fd.as_fd());
                    planted.push((kind, fd));
                    ns
                }
            };
            // (tarae has CAP_NET_ADMIN over the session's netns — its user namespace is tarae's — so the lookup's
            // EPERM means another's)
            match ns {
                Ok(ns) if ns == session => {}
                Ok(ns) => {
                    let said =
                        format!("its {kind:?} socket is in netns {ns:?}, the session's is {session:?}");
                    return Err(failed(BirthStep::Netns, libc::EPERM, &said));
                }
                Err(e) => {
                    return Err(failed(BirthStep::Netns, errno(&e), &format!("its {kind:?} socket: {e}")));
                }
            }
        }
        let (Some(mut route), Some(mut nf)) = (route, nf) else {
            return Err(failed(BirthStep::Planted, libc::EPROTO, "no netlink socket to send on"));
        };
        for (kind, fd) in &planted {
            transparent(*kind, fd.as_fd())
                .and_then(|()| match kind.dns_at() {
                    Some(at) => bound(fd.as_fd(), at),
                    None => Ok(()),
                })
                .map_err(|e| failed(BirthStep::Planted, errno(&e), &format!("its {kind:?} socket: {e}")))?;
        }
        #[cfg(test)]
        let refused = network.no_rcvmark;
        #[cfg(not(test))]
        let refused = false;
        let udp =
            planted.iter().filter(|(k, _)| matches!(k, Kind::Udp4 | Kind::Udp6 | Kind::Dns4 | Kind::Dns6));
        if let Some(e) = udp.filter_map(|(_, fd)| asks_marks(fd.as_fd(), refused).err()).next() {
            crate::log::line(
                "sandbox",
                "info",
                &format!(
                    "the session's planted UDP sockets read no datagram's mark ({e}: before Linux 5.19) — every \
                     datagram is the session's, none a run's (D5)"
                ),
            );
        }
        if let Some(icmp) = &icmp {
            sends_only(icmp.as_fd())
                .map_err(|e| failed(BirthStep::Planted, errno(&e), &format!("its raw ICMP socket: {e}")))?;
        }
        for batch in &network.routes {
            route
                .apply(batch, Instant::now() + ANSWERED_WITHIN)
                .map_err(|e| failed(BirthStep::Routes, errno_of(&e), &e.to_string()))?;
        }
        nf.apply(&network.rules, Instant::now() + ANSWERED_WITHIN)
            .map_err(|e| failed(BirthStep::Rules, errno_of(&e), &e.to_string()))?;
        let marks = Arc::new(Marks::new());
        // (6e: the session's names, answered by the egress proxy — each reply from the address asked, on the DNS
        // socket of its family)
        let v6 = planted.iter().any(|(k, _)| *k == Kind::Dns6);
        let names = Arc::new(answering::Session::new(network.search.clone(), v6));
        let reply_on = |want: Kind| {
            let fd = planted.iter().find(|(k, _)| *k == want)?;
            fd.1.try_clone().ok().map(Arc::new)
        };
        let replies = [reply_on(Kind::Dns4), reply_on(Kind::Dns6)];
        let to_proxy: Handoff = {
            let (marks, names) = (marks.clone(), names.clone());
            Arc::new(move |c, original| to_proxy(&marks, &names, c, original))
        };
        let datagrams: Datagrams = {
            let (marks, names) = (marks.clone(), names.clone());
            Arc::new(move |d, payload| question(&marks, &names, &replies, d, payload))
        };
        let handled = (Some(to_proxy), Some(datagrams));
        let taken =
            Taken::start(planted, icmp.clone(), Arc::new(accept_one), handled, SAID_EVERY).map_err(|e| {
                failed(BirthStep::Planted, errno(&e), &format!("its planted sockets' thread: {e}"))
            })?;
        Ok(Made { route, nf, taken, marks, names, icmp })
    }

    /// What a datagram arriving at a planted UDP socket or a DNS one gets — given the payload it carries; Err:
    /// refused, with what the log says of it (answered ICMP port unreachable).
    pub(super) type Datagrams = Arc<dyn Fn(&Datagram, &[u8]) -> Result<(), String> + Send + Sync>;

    /// A datagram `d` (carrying `payload`) that arrived for a port 53 — a question (6e): to the egress proxy, which
    /// answers it as its asker's (by its mark: E2, the run's rules — or, on a kernel that reads no mark, D5, the
    /// session's), the answer sent from the address asked on the DNS socket of its family (`replies`: v4, v6). Any
    /// other port, refused as before; a question with a mark tarae didn't give (or a run's with nothing of it served
    /// any more), answered REFUSED at once and said.
    fn question(
        marks: &Marks,
        names: &Arc<answering::Session>,
        replies: &[Option<Arc<OwnedFd>>; 2],
        d: &Datagram,
        payload: &[u8],
    ) -> Result<(), String> {
        let (Some(from), Some(orig)) = (d.from, d.orig) else { return Err(whose(marks, d.mark)) };
        if orig.port() != crate::sandbox::route::RESOLVER_PORT {
            return Err(whose(marks, d.mark));
        }
        let Some(sock) = replies[usize::from(orig.is_ipv6())].clone() else {
            return Err(format!("{}: no DNS socket of its family", whose(marks, d.mark)));
        };
        let reply: proxy::Reply = Box::new(move |answer: Vec<u8>| {
            let _ = reply_from(sock.as_fd(), orig, from, &answer);
        });
        let asker = match d.mark {
            None => answering::Asker::Session,
            Some(m) => match marks.owner(m).and_then(|o| Some((o.told.upgrade()?, o))) {
                Some((told, o)) => answering::Asker::Run {
                    run: o.run,
                    session: o.session,
                    process: o.process,
                    rules: o.rules,
                    told: (*told).clone(),
                },
                // (E2: no run's — answered REFUSED at once, so its lookup ends as fast as a refused one; said in the
                // log, the first and then at each power of two: an unserved run asking in a loop can't fill it)
                None => {
                    match dns::question(payload) {
                        Ok(q) => reply(dns::answer(&q, &dns::Answer::Rcode(dns::REFUSED), 0)),
                        Err(dns::Refused::Answered { id, flags, .. }) => {
                            reply(dns::refusal(id, flags, dns::REFUSED))
                        }
                        Err(dns::Refused::Dropped) => {}
                    }
                    static UNMARKED: AtomicU64 = AtomicU64::new(0);
                    let n = UNMARKED.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        let line = format!(
                            "the session's names: a question for {orig} from {from} — {}; answered REFUSED ({n} so far)",
                            whose(marks, d.mark)
                        );
                        crate::log::line("sandbox", "info", &line);
                    }
                    return Ok(());
                }
            },
        };
        let q =
            proxy::NameQuestion { payload: payload.to_vec(), asker, names: names.clone(), reply, tcp: false };
        proxy::get().send(proxy::ToProxy::Question(q));
        Ok(())
    }

    /// `answer` sent on the DNS socket `sock` to `to`, from `src` — the address and port the question was sent to:
    /// `IP_PKTINFO`'s address (a transparent socket may send from one that isn't its machine's), the socket's own
    /// port (53). A connected client takes nothing from another address or port.
    fn reply_from(
        sock: BorrowedFd,
        src: std::net::SocketAddr,
        to: std::net::SocketAddr,
        answer: &[u8],
    ) -> io::Result<()> {
        let (dest, dest_len) = address(to);
        let mut control = [0u64; 8];
        let mut iov = libc::iovec { iov_base: answer.as_ptr().cast_mut().cast(), iov_len: answer.len() };
        // SAFETY: a zeroed msghdr pointed at buffers of ours, live for the call; the one control message written
        // within the buffer, at the header's own data offset.
        unsafe {
            let mut m: libc::msghdr = std::mem::zeroed();
            m.msg_name = (&raw const dest).cast_mut().cast();
            m.msg_namelen = dest_len;
            m.msg_iov = &raw mut iov;
            m.msg_iovlen = 1;
            m.msg_control = control.as_mut_ptr().cast();
            let (level, kind, len) = match src.ip() {
                std::net::IpAddr::V4(_) => (libc::SOL_IP, libc::IP_PKTINFO, size_of::<libc::in_pktinfo>()),
                std::net::IpAddr::V6(_) => {
                    (libc::SOL_IPV6, libc::IPV6_PKTINFO, size_of::<libc::in6_pktinfo>())
                }
            };
            m.msg_controllen = libc::CMSG_SPACE(len as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&m);
            (*c).cmsg_level = level;
            (*c).cmsg_type = kind;
            (*c).cmsg_len = libc::CMSG_LEN(len as u32) as _;
            match src.ip() {
                std::net::IpAddr::V4(v4) => {
                    let info = libc::in_pktinfo {
                        ipi_ifindex: 0,
                        ipi_spec_dst: libc::in_addr { s_addr: u32::from_ne_bytes(v4.octets()) },
                        ipi_addr: libc::in_addr { s_addr: 0 },
                    };
                    std::ptr::write_unaligned(libc::CMSG_DATA(c).cast(), info);
                }
                std::net::IpAddr::V6(v6) => {
                    let info = libc::in6_pktinfo {
                        ipi6_addr: libc::in6_addr { s6_addr: v6.octets() },
                        ipi6_ifindex: 0,
                    };
                    std::ptr::write_unaligned(libc::CMSG_DATA(c).cast(), info);
                }
            }
            if libc::sendmsg(sock.as_raw_fd(), &m, libc::MSG_DONTWAIT) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// A DNS socket bound at `at` (`Kind::dns_at`: the session's own address of its family, port 53) — by tarae, whose
    /// capabilities over the session's netns let it bind a port below 1024; an IPv6 one IPv6 only.
    fn bound(fd: BorrowedFd, at: std::net::SocketAddr) -> io::Result<()> {
        let one: libc::c_int = 1;
        // SAFETY: an int and an address of the kernel's layout, ours for each call, on our socket.
        unsafe {
            if at.is_ipv6()
                && libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&raw const one).cast(),
                    4,
                ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            let (sa, len) = address(at);
            if libc::bind(fd.as_raw_fd(), (&raw const sa).cast(), len) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Whose a datagram is, by the mark it was read with (`SO_RCVMARK`): a run's — its program's — or, with a mark
    /// tarae didn't give (none, the output chain's alone), no one's; on a kernel that reads none (before 5.19, D5),
    /// the session's.
    fn whose(marks: &Marks, mark: Option<u32>) -> String {
        match mark {
            None => "the session's: this kernel reads no datagram's mark".into(),
            Some(m) => match marks.owner(m) {
                Some(o) => format!("run {}'s {}", o.run, o.process),
                None => format!("its mark {m:#x} isn't one tarae gave"),
            },
        }
    }

    /// `asm-generic/socket.h`'s (5.19): each datagram read with its mark, as a control message (`SO_MARK`'s type).
    const SO_RCVMARK: i32 = 75;

    /// A planted UDP socket asked to read each datagram's mark (`SO_RCVMARK`) — whose run it is. A kernel before
    /// 5.19 refuses it (`ENOPROTOOPT`); so does `refused`, the tests' stand-in for one (E5).
    fn asks_marks(fd: BorrowedFd, refused: bool) -> io::Result<()> {
        if refused {
            return Err(io::Error::from_raw_os_error(libc::ENOPROTOOPT));
        }
        let one: libc::c_int = 1;
        // SAFETY: an int, ours for the call, on a socket we hold.
        let r = unsafe {
            libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, SO_RCVMARK, (&raw const one).cast(), 4)
        };
        if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Where a connection the planted socket accepted goes (6d): to the egress proxy, with its run's rules and
    /// sender, when it carries a mark tarae gave (`Marks::owner` — the supervisor's, at the run's connect) — else
    /// back, with why it's refused: a connect to the planted address itself, or a mark tarae didn't give (a socket
    /// that never went through the supervisor: it shouldn't exist).
    pub(super) type Handoff =
        Arc<dyn Fn(OwnedFd, std::net::SocketAddr) -> Result<(), (OwnedFd, String)> + Send + Sync>;

    /// `Handoff`'s, for the session's `marks` and `names`: connection `c`, going to `original`.
    fn to_proxy(
        marks: &Marks,
        names: &Arc<answering::Session>,
        c: OwnedFd,
        original: std::net::SocketAddr,
    ) -> Result<(), (OwnedFd, String)> {
        let planted = [Kind::Tcp4, Kind::Tcp6].iter().any(|k| k.at() == Some(original));
        if planted {
            return Err((c, "a connection to the planted socket itself".into()));
        }
        let mark = mark_of(c.as_fd());
        let Some(o) = marks.owner(mark) else {
            let why =
                format!("its mark {mark:#x} isn't one tarae gave — it didn't go through tarae's supervisor");
            return Err((c, why));
        };
        // (a run's sender lives with its supervisor, as long as a process of the run — or one it left — is served:
        // a connection arriving after that has no one to tell, nor anyone of the run to connect)
        let Some(told) = o.told.upgrade() else {
            let why = format!("run {}'s {}: the run is over and nothing of it is served", o.run, o.process);
            return Err((c, why));
        };
        let p = crate::sandbox::proxy::ToProxy::Planted(crate::sandbox::proxy::Planted {
            fd: c,
            original,
            run: o.run,
            session: o.session,
            process: o.process,
            rules: o.rules,
            told: (*told).clone(),
            names: names.clone(),
        });
        // (the proxy gone, the connection goes with the message: its run reads its end)
        crate::sandbox::proxy::get().send(p);
        Ok(())
    }

    /// A socket's mark (`SO_MARK`) — 0 where it can't be read.
    fn mark_of(fd: BorrowedFd) -> u32 {
        let (mut v, mut len) = (0u32, size_of::<u32>() as libc::socklen_t);
        // SAFETY: an int and its length, ours for the call, on a socket we hold.
        let r = unsafe {
            libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_MARK, (&raw mut v).cast(), &mut len)
        };
        if r == 0 { v } else { 0 }
    }

    /// `linux/icmp.h`'s `ICMP_FILTER`, at `SOL_RAW`: a bit per ICMP type a raw socket isn't given.
    const SOL_RAW: i32 = 255;
    const ICMP_FILTER: i32 = 1;

    /// The ICMP types raw socket `fd` isn't given: a bit per type, 0–31 (the filter reaches no further).
    pub(crate) fn icmp_filter(fd: BorrowedFd, filtered: u32) -> io::Result<()> {
        // SAFETY: the filter's word, ours for the call, on a socket we hold.
        let r = unsafe {
            libc::setsockopt(fd.as_raw_fd(), SOL_RAW, ICMP_FILTER, (&raw const filtered).cast(), 4)
        };
        if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// The raw ICMP socket made to send only: a raw socket gets a copy of every ICMP packet delivered in its netns,
    /// and nothing reads this one, so its queue would fill and stay full — every type it can be given is filtered.
    /// (A type past 31 can't be: rare, and its receive buffer bounds what it keeps.)
    fn sends_only(fd: BorrowedFd) -> io::Result<()> {
        icmp_filter(fd, u32::MAX)
    }

    /// A planted socket made transparent — tproxy delivers to it what was addressed elsewhere — and, a UDP one,
    /// each datagram's original destination read with it (`RECVORIGDSTADDR`). `IP_TRANSPARENT` needs `CAP_NET_ADMIN`
    /// over the socket's netns: tarae's. (Whose run a datagram is — its mark, `SO_RCVMARK` — is 6e's, with what
    /// reads it.)
    fn transparent(kind: Kind, fd: BorrowedFd) -> io::Result<()> {
        let (level, transparent, origdst) = match kind {
            Kind::Tcp4 | Kind::Udp4 | Kind::Dns4 => (libc::SOL_IP, libc::IP_TRANSPARENT, IP_RECVORIGDSTADDR),
            _ => (libc::SOL_IPV6, IPV6_TRANSPARENT, IPV6_RECVORIGDSTADDR),
        };
        let set = |name: i32| {
            let one: libc::c_int = 1;
            // SAFETY: an int, ours for the call, on a socket we hold.
            let r = unsafe { libc::setsockopt(fd.as_raw_fd(), level, name, (&raw const one).cast(), 4) };
            if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
        };
        set(transparent)?;
        if matches!(kind, Kind::Udp4 | Kind::Udp6 | Kind::Dns4 | Kind::Dns6) {
            set(origdst)?;
        }
        Ok(())
    }

    /// What lands on the planted sockets — until 6d (TCP) and 6e (UDP) refused as quickly and as plainly as a
    /// closed port refuses it: a connection accepted and reset, a datagram read and answered with ICMP port
    /// unreachable (on the birth's raw socket, where there is one) — each with where it was going (a connection's own
    /// address, a datagram's `ORIGDSTADDR`) counted and said in the log. Its thread ends when this is dropped (with
    /// the session's network: its keeper gone), and the planted sockets with it.
    pub struct Taken {
        _stop: OwnedFd,
        #[cfg_attr(not(test), allow(dead_code))]
        seen: Arc<Seen>,
    }

    /// What `Taken` refused: how many, the last one's original destination, and how many spells of failures it
    /// waited out (`STALLED_FOR`), each said once.
    #[derive(Default)]
    struct Seen {
        count: AtomicU64,
        last: std::sync::Mutex<Option<std::net::SocketAddr>>,
        spells: AtomicU64,
    }

    /// How a connection is taken off a planted listener — `accept_one`; the tests pass one that fails.
    pub(super) type Accept = Arc<dyn Fn(BorrowedFd) -> io::Result<OwnedFd> + Send + Sync>;

    /// How long the loop waits after an accept or a read failed for a reason that doesn't go by itself (out of
    /// descriptors, out of memory): the listener stays readable — the connection is still queued — so trying at once
    /// would spin.
    const STALLED_FOR: Duration = Duration::from_millis(100);

    /// A connection off listener `fd`, close-on-exec.
    fn accept_one(fd: BorrowedFd) -> io::Result<OwnedFd> {
        // SAFETY: an accept on a non-blocking socket we hold; the connection is ours.
        let c = unsafe {
            libc::accept4(fd.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC)
        };
        if c < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just accepted, ours.
        Ok(unsafe { OwnedFd::from_raw_fd(c) })
    }

    /// How often the refusals at one planted socket are said after its first — as a count, with where the last was
    /// going: a run retrying in a loop, or a burst of datagrams, grows tarae's log (it has no cap) by a line this
    /// often, not a line a refusal. Kept per planted socket — at most four a session, so what's kept is fixed; per
    /// destination it would grow with what a run chose to send to, the very thing being bounded.
    const SAID_EVERY: Duration = Duration::from_secs(5);

    /// What one planted socket has said of its refusals: whether its first yet, how many since it last said, when
    /// that was, and where the last was going.
    struct Telling {
        at: String,
        tcp: bool,
        said: bool,
        unsaid: u64,
        since: Instant,
        last: Option<std::net::SocketAddr>,
    }

    impl Telling {
        fn new(kind: Kind, fd: BorrowedFd) -> Telling {
            let at = own_address(fd).map_or_else(|| format!("{kind:?}"), |a| a.to_string());
            let tcp = matches!(kind, Kind::Tcp4 | Kind::Tcp6);
            Telling { at, tcp, said: false, unsaid: 0, since: Instant::now(), last: None }
        }

        /// One refused, going to `to` — `why`, where something said (a connection the egress proxy wasn't given:
        /// `Handoff`): said if it's the socket's first, else counted.
        fn refused(&mut self, to: Option<std::net::SocketAddr>, why: Option<&str>, every: Duration) {
            self.last = to;
            if !self.said {
                self.said = true;
                self.since = Instant::now();
                let (what, how) = match (self.tcp, why) {
                    (true, Some(why)) => ("a connection", format!("{why}; reset")),
                    (true, None) => ("a connection", "reset".into()),
                    (false, Some(why)) => ("a datagram", format!("{why}; answered port unreachable")),
                    (false, None) => ("a datagram", "answered port unreachable".into()),
                };
                let to = to.map_or_else(|| "somewhere it didn't say".into(), |a| a.to_string());
                crate::log::line(
                    "sandbox",
                    "warn",
                    &format!(
                        "the session's planted socket at {} refused {what} for {to} ({how}) — more are counted, said \
                         every {:.1} s",
                        self.at,
                        every.as_secs_f32()
                    ),
                );
                return;
            }
            self.unsaid += 1;
        }

        /// How long until its count is due — None: nothing to say.
        fn due(&self, every: Duration) -> Option<Duration> {
            (self.unsaid > 0).then(|| (self.since + every).saturating_duration_since(Instant::now()))
        }

        /// Its count said, where one's due — or, `now`, wherever there's one.
        fn tell(&mut self, every: Duration, now: bool) {
            if self.unsaid == 0 || (!now && self.since.elapsed() < every) {
                return;
            }
            let what = if self.tcp { "connections" } else { "datagrams" };
            let to = self.last.map_or_else(|| "somewhere it didn't say".into(), |a| a.to_string());
            crate::log::line(
                "sandbox",
                "warn",
                &format!(
                    "the session's planted socket at {} refused {} more {what} in {:.1} s, the last for {to}",
                    self.at,
                    self.unsaid,
                    self.since.elapsed().as_secs_f32()
                ),
            );
            self.unsaid = 0;
            self.since = Instant::now();
        }
    }

    impl Taken {
        /// The thread that serves what lands on `planted`: taking connections by `accept`, each handed on
        /// (`handoff`, 6d — None: none, every one refused) or refused; each datagram given to `datagrams` (6e — None:
        /// every one refused), a refused one answered on `icmp`; saying the refusals at most `every` (`SAID_EVERY`)
        /// per socket after its first.
        fn start(
            planted: Vec<(Kind, OwnedFd)>,
            icmp: Option<Arc<OwnedFd>>,
            accept: Accept,
            (handoff, datagrams): (Option<Handoff>, Option<Datagrams>),
            every: Duration,
        ) -> io::Result<Taken> {
            let (stop_r, stop_w) = io::pipe()?;
            for (_, fd) in &planted {
                // SAFETY: flags read and set on a descriptor we hold.
                unsafe {
                    let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
            let seen = Arc::new(Seen::default());
            let seeing = seen.clone();
            let stop = OwnedFd::from(stop_r);
            crate::event::thread("tarae-planted", move || {
                refuse(planted, stop, seeing, icmp, accept, (handoff, datagrams), every)
            });
            Ok(Taken { _stop: stop_w.into(), seen })
        }

        /// How many spells of failures it waited out.
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn spells(&self) -> u64 {
            self.seen.spells.load(Ordering::Relaxed)
        }

        /// How many it refused (the log says the first and then counts; the tests read this).
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn count(&self) -> u64 {
            self.seen.count.load(Ordering::Relaxed)
        }

        /// Where the last one it refused was going.
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn last(&self) -> Option<std::net::SocketAddr> {
            *self.seen.last.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    /// An address the kernel wrote (`sockaddr_in`, `sockaddr_in6`) — None another family's.
    fn read_address(ss: &libc::sockaddr_storage) -> Option<std::net::SocketAddr> {
        // SAFETY: each family's struct read from the storage it fits in, by the family the kernel wrote.
        unsafe {
            match i32::from(ss.ss_family) {
                libc::AF_INET => {
                    let s = &*(ss as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>();
                    let ip = std::net::Ipv4Addr::from(s.sin_addr.s_addr.to_ne_bytes());
                    Some(std::net::SocketAddr::new(ip.into(), u16::from_be(s.sin_port)))
                }
                libc::AF_INET6 => {
                    let s = &*(ss as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>();
                    let ip = std::net::Ipv6Addr::from(s.sin6_addr.s6_addr);
                    Some(std::net::SocketAddr::new(ip.into(), u16::from_be(s.sin6_port)))
                }
                _ => None,
            }
        }
    }

    /// A connection's own address — on a planted socket its original destination: tproxy keeps it.
    fn own_address(c: BorrowedFd) -> Option<std::net::SocketAddr> {
        // SAFETY: an address buffer of ours for the call, on a socket we hold.
        unsafe {
            let mut ss: libc::sockaddr_storage = std::mem::zeroed();
            let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            if libc::getsockname(c.as_raw_fd(), (&raw mut ss).cast(), &mut len) != 0 {
                return None;
            }
            read_address(&ss)
        }
    }

    /// A datagram as `received` read it: who sent it, where it was going (`ORIGDSTADDR`), how long it was, and the
    /// mark it came with (`SO_RCVMARK` — None where the socket reads none: before 5.19).
    pub(super) struct Datagram {
        pub from: Option<std::net::SocketAddr>,
        pub orig: Option<std::net::SocketAddr>,
        pub len: usize,
        pub mark: Option<u32>,
    }

    /// A datagram read from `fd` into `buf`, its sender, and from its control messages its original destination
    /// (`IP_ORIGDSTADDR`, `IPV6_ORIGDSTADDR` — `RECVORIGDSTADDR` asked) and its mark (`SO_MARK`'s — `SO_RCVMARK`).
    fn received(fd: BorrowedFd, buf: &mut [u8]) -> io::Result<Datagram> {
        let mut control = [0u64; 16];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        // SAFETY: a zeroed msghdr pointed at buffers of ours, live for the call; the control data read only within
        // what the kernel says it wrote.
        unsafe {
            let mut name: libc::sockaddr_storage = std::mem::zeroed();
            let mut m: libc::msghdr = std::mem::zeroed();
            m.msg_name = (&raw mut name).cast();
            m.msg_namelen = size_of_val(&name) as libc::socklen_t;
            m.msg_iov = &raw mut iov;
            m.msg_iovlen = 1;
            m.msg_control = control.as_mut_ptr().cast();
            m.msg_controllen = size_of_val(&control) as _;
            let n = libc::recvmsg(fd.as_raw_fd(), &mut m, libc::MSG_DONTWAIT);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut got = Datagram { from: read_address(&name), orig: None, len: n as usize, mark: None };
            let mut c = libc::CMSG_FIRSTHDR(&m);
            while !c.is_null() {
                let data = libc::CMSG_DATA(c);
                let len = ((*c).cmsg_len as usize).saturating_sub(data as usize - c as usize);
                match ((*c).cmsg_level, (*c).cmsg_type) {
                    (libc::SOL_IP, IP_RECVORIGDSTADDR) | (libc::SOL_IPV6, IPV6_RECVORIGDSTADDR) => {
                        let mut ss: libc::sockaddr_storage = std::mem::zeroed();
                        std::ptr::copy_nonoverlapping(data, (&raw mut ss).cast(), len.min(size_of_val(&ss)));
                        got.orig = read_address(&ss);
                    }
                    (libc::SOL_SOCKET, libc::SO_MARK) if len >= 4 => {
                        let mut v = [0u8; 4];
                        std::ptr::copy_nonoverlapping(data, v.as_mut_ptr(), 4);
                        got.mark = Some(u32::from_ne_bytes(v));
                    }
                    _ => {}
                }
                c = libc::CMSG_NXTHDR(&m, c);
            }
            Ok(got)
        }
    }

    /// The Internet checksum (RFC 1071) of `b`.
    pub(crate) fn checksum(b: &[u8]) -> u16 {
        let mut sum: u32 =
            b.chunks(2).map(|w| u32::from(u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)]))).sum();
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    /// What a closed port answers a UDP datagram `len` bytes long from `from` to `orig` (IPv4): ICMP port
    /// unreachable, quoting the datagram's own IP header and its UDP header — the kernel finds the sender's socket by
    /// them, and a connected one gets `ECONNREFUSED` — sent to `from` on `icmp`, in the session's netns.
    fn unreachable(
        icmp: BorrowedFd,
        from: std::net::SocketAddrV4,
        orig: std::net::SocketAddrV4,
        len: usize,
    ) -> io::Result<()> {
        let udp_len = u16::try_from(8 + len).unwrap_or(u16::MAX);
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&u16::try_from(28 + len).unwrap_or(u16::MAX).to_be_bytes());
        (ip[8], ip[9]) = (64, 17);
        ip[12..16].copy_from_slice(&from.ip().octets());
        ip[16..20].copy_from_slice(&orig.ip().octets());
        let ip_sum = checksum(&ip);
        ip[10..12].copy_from_slice(&ip_sum.to_be_bytes());
        let mut p = vec![3u8, 3, 0, 0, 0, 0, 0, 0];
        p.extend(ip);
        p.extend(from.port().to_be_bytes());
        p.extend(orig.port().to_be_bytes());
        p.extend(udp_len.to_be_bytes());
        p.extend([0, 0]);
        let sum = checksum(&p);
        p[2..4].copy_from_slice(&sum.to_be_bytes());
        let (to, to_len) = address(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(*from.ip(), 0)));
        // SAFETY: a send of our buffer to an address of ours, on a socket we hold.
        let n = unsafe {
            libc::sendto(icmp.as_raw_fd(), p.as_ptr().cast(), p.len(), 0, (&raw const to).cast(), to_len)
        };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// A connection reset as it's closed (`SO_LINGER` 0): its peer's next read or write fails at once
    /// (`ECONNRESET`), as against a port refused — not an end of data it might wait past.
    fn reset(c: OwnedFd) {
        let linger = libc::linger { l_onoff: 1, l_linger: 0 };
        // SAFETY: a linger struct of ours for the call, on a socket we hold; then it's closed.
        unsafe {
            libc::setsockopt(
                c.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&raw const linger).cast(),
                size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        drop(c);
    }

    /// Whether `e` goes by itself (nothing waited after all, a signal, a peer gone before its accept) — else the
    /// loop waits `STALLED_FOR` before it tries again.
    fn passing(e: &io::Error) -> bool {
        matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
        )
    }

    fn refuse(
        planted: Vec<(Kind, OwnedFd)>,
        stop: OwnedFd,
        seen: Arc<Seen>,
        icmp: Option<Arc<OwnedFd>>,
        accept: Accept,
        (handoff, datagrams): (Option<Handoff>, Option<Datagrams>),
        every: Duration,
    ) {
        let mut buf = vec![0u8; 64 * 1024];
        // (in a spell of failures: said once, at its start)
        let mut stalled = false;
        let mut telling: Vec<Telling> =
            planted.iter().map(|(kind, fd)| Telling::new(*kind, fd.as_fd())).collect();
        loop {
            let mut polled: Vec<libc::pollfd> = std::iter::once(stop.as_raw_fd())
                .chain(planted.iter().map(|(_, fd)| fd.as_raw_fd()))
                .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
                .collect();
            // (woken when a count is due, rounded up: never early, so never spun on)
            let wait = telling.iter().filter_map(|t| t.due(every)).min();
            let ms = wait.map_or(-1, |d| d.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32);
            // SAFETY: the pollfds, live for the call.
            let r = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, ms) };
            if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // (its writer gone — the session's network let go — or poll failing: what's counted is said, and it ends)
            if r < 0 || polled[0].revents != 0 || polled.iter().any(|p| p.revents & libc::POLLNVAL != 0) {
                for t in &mut telling {
                    t.tell(every, true);
                }
                return;
            }
            for t in &mut telling {
                t.tell(every, false);
            }
            for (((kind, fd), p), telling) in planted.iter().zip(&polled[1..]).zip(&mut telling) {
                if p.revents == 0 {
                    continue;
                }
                let tcp = matches!(kind, Kind::Tcp4 | Kind::Tcp6);
                // (recorded before the connection's close is seen: whoever sees it closed sees it counted)
                let record = |orig: Option<std::net::SocketAddr>| {
                    *seen.last.lock().unwrap_or_else(|e| e.into_inner()) = orig;
                    seen.count.fetch_add(1, Ordering::Relaxed) + 1
                };
                let got = match tcp {
                    true => accept(fd.as_fd()).map(|c| (own_address(c.as_fd()), Some(c), None)),
                    false => received(fd.as_fd(), &mut buf).map(|d| (d.orig, None, Some(d))),
                };
                let (orig, connection, datagram) = match got {
                    Ok(got) => {
                        stalled = false;
                        got
                    }
                    Err(e) if passing(&e) => continue,
                    // (the connection still queued, the socket still readable: waited out, not spun on)
                    Err(e) => {
                        if !stalled {
                            stalled = true;
                            seen.spells.fetch_add(1, Ordering::Relaxed);
                            let at = kind.at().map(|a| a.to_string()).unwrap_or_default();
                            crate::log::line(
                                "sandbox",
                                "warn",
                                &format!(
                                    "the session's planted socket at {at} can't be read ({e}): tried again every \
                                     {} ms until it can",
                                    STALLED_FOR.as_millis()
                                ),
                            );
                        }
                        std::thread::sleep(STALLED_FOR);
                        continue;
                    }
                };
                // (6d: a connection whose mark tarae gave goes to the egress proxy; what comes back is refused)
                let mut why = None;
                if let Some(c) = connection {
                    match (&handoff, orig) {
                        (Some(to_proxy), Some(at)) => match to_proxy(c, at) {
                            Ok(()) => continue,
                            Err((c, said)) => {
                                why = Some(said);
                                record(orig);
                                reset(c);
                            }
                        },
                        _ => {
                            record(orig);
                            reset(c);
                        }
                    }
                } else if let Some(d) = &datagram {
                    // (6e: a datagram given to the session's answering — Err: refused, with why)
                    if let Some(given) = &datagrams {
                        match given(d, &buf[..d.len.min(buf.len())]) {
                            Ok(()) => continue,
                            Err(said) => why = Some(said),
                        }
                    }
                    record(orig);
                }
                if let (Some(icmp), Some(d)) = (&icmp, datagram)
                    && let (Some(std::net::SocketAddr::V4(from)), Some(std::net::SocketAddr::V4(orig))) =
                        (d.from, d.orig)
                {
                    let _ = unreachable(icmp.as_fd(), from, orig, d.len);
                }
                telling.refused(orig, why.as_deref(), every);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A failed accept that doesn't go by itself (`EMFILE`: tarae out of descriptors) leaves the connection
        /// queued and the listener readable: the loop waits `STALLED_FOR` between tries instead of spinning, and
        /// says the spell once. A listener of this process's with one connection waiting, taken by an accept that
        /// always fails, for half a second.
        #[test]
        fn a_failing_accept_backs_off_instead_of_spinning() {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let _waiting = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let tries = Arc::new(AtomicU64::new(0));
            let counted = tries.clone();
            let accept: Accept = Arc::new(move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::from_raw_os_error(libc::EMFILE))
            });
            let taken =
                Taken::start(vec![(Kind::Tcp4, OwnedFd::from(l))], None, accept, (None, None), SAID_EVERY)
                    .unwrap();
            std::thread::sleep(Duration::from_millis(500));
            let n = tries.load(Ordering::Relaxed);
            assert!(
                (2..=10).contains(&n),
                "{n} tries in half a second, one per {} ms",
                STALLED_FOR.as_millis()
            );
            assert_eq!(taken.spells(), 1, "the spell said once");
            assert_eq!(taken.count(), 0, "nothing taken");
        }

        /// A burst of refusals is said in a bounded number of lines — tarae's log has no cap, and a run can send a
        /// burst or retry in a loop: the first said, then a count at most every interval (200 ms here, `SAID_EVERY`
        /// for real), the last when it stops. A burst of 300 datagrams on a planted UDP socket of this process's:
        /// 2–4 lines that between them say every refusal `Taken` counted. (On a busy machine the socket's buffer may
        /// lose some of the 300 before they're read — never refused, so never said: what's compared is what was
        /// counted, not what was sent.)
        #[test]
        fn a_burst_of_refusals_is_said_in_a_bounded_number_of_lines() {
            const SENT: u64 = 300;
            let planted = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let at = planted.local_addr().unwrap();
            let captured = crate::log::capture();
            let every = Duration::from_millis(200);
            let taken = Taken::start(
                vec![(Kind::Udp4, OwnedFd::from(planted))],
                None,
                Arc::new(accept_one),
                (None, None),
                every,
            )
            .unwrap();
            let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            for _ in 0..SENT {
                sender.send_to(b"x", at).unwrap();
            }
            // (read to the end: the count unchanged for 100 ms, within 2 s)
            let by = Instant::now() + Duration::from_secs(2);
            let mut was = u64::MAX;
            while taken.count() != was && Instant::now() < by {
                was = taken.count();
                std::thread::sleep(Duration::from_millis(100));
            }
            // (one interval more: the last count is due)
            std::thread::sleep(every * 2);
            let ours = format!("planted socket at {at} ");
            let lines: Vec<String> = captured.lines().into_iter().filter(|l| l.contains(&ours)).collect();
            let counted = taken.count();
            assert!((2..=SENT).contains(&counted), "a burst landed: {counted} of {SENT}");
            assert!((2..=4).contains(&lines.len()), "{} lines: {lines:#?}", lines.len());
            let more = |l: &String| l.split(" refused ").nth(1)?.split(' ').next()?.parse::<u64>().ok();
            let said = 1 + lines.iter().filter_map(more).sum::<u64>();
            assert_eq!(said, counted, "the first, and the counts: {lines:#?}");
        }

        /// The quote an ICMP error carries is checksummed as the kernel checks it: a header summed with its own
        /// checksum sums to zero (RFC 1071's example words, and an odd length padded).
        #[test]
        fn the_checksum_is_rfc_1071s() {
            let words = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
            assert_eq!(checksum(&words), !0xddf2);
            let mut with = words.to_vec();
            with.extend(checksum(&words).to_be_bytes());
            assert_eq!(checksum(&with), 0);
            assert_eq!(checksum(&[0xff]), !0xff00);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tarae_netlink::fixture::{self, without_acks, without_seqs};

    fn assert_bytes(ours: &[u8], theirs: &[u8], what: &str) {
        if ours != theirs {
            let at =
                ours.iter().zip(theirs).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()));
            panic!(
                "{what}: the bytes differ from byte {at} (ours {} long, theirs {})\n ours:   {:02x?}\n theirs: {:02x?}",
                ours.len(),
                theirs.len(),
                &ours[at.saturating_sub(8)..(at + 24).min(ours.len())],
                &theirs[at.saturating_sub(8)..(at + 24).min(theirs.len())],
            );
        }
    }

    /// The session's network is, message for message, what nft and ip sent the kernel for it: the ruleset the batch
    /// nft sent for M1's `split.nft` as 6d changed it — output keeping a mark it finds, prerouting setting none — and
    /// 6e (TCP to port 53 kept for the session; a question a session's own server is bound for delivered to it),
    /// written as `create`s and `add rule`s (`create-batch-6e2`; the acks
    /// tarae asks aside), each family's routes the
    /// three changes ip sent (`route-default`, `rule-fwmark…-bit` — bit 0 under its mask, 6d — and `route-local`; ip
    /// numbers its by the clock). Both families carried: that's what was captured.
    #[test]
    fn the_sessions_network_is_what_nft_and_ip_sent() {
        let n = Network::session(true, true);
        assert_bytes(
            &without_acks(n.rules.bytes()),
            &fixture::last_sent("create-batch-6e2"),
            "create-batch-6e2",
        );
        for (batch, f) in n.routes.iter().zip(["4", "6"]) {
            let theirs: Vec<u8> = ["route-default{f}", "rule-fwmark{f}-bit", "route-local{f}"]
                .iter()
                .flat_map(|c| without_seqs(&fixture::last_sent(&c.replace("{f}", f))))
                .collect();
            assert_bytes(&without_seqs(batch.bytes()), &theirs, &format!("routes v{f}"));
        }
    }

    /// `b`'s messages, each with its sequence number set to 0.
    fn messages(b: &[u8]) -> Vec<Vec<u8>> {
        let b = without_seqs(b);
        let (mut out, mut at) = (Vec::new(), 0);
        while at + 16 <= b.len() {
            let len = (u32::from_ne_bytes(b[at..at + 4].try_into().unwrap()) as usize).max(16);
            out.push(b[at..(at + len).min(b.len())].to_vec());
            at += len;
        }
        out
    }

    /// A family the session's `lo` doesn't carry gets no routes and none of its own rules — the rest are the captured
    /// ruleset's, byte for byte and in its order (no capture holds the one-family sets themselves: they're the
    /// two-family batch less one family's 8 rules — 2 leaving, 6 coming in); without a family, no routes at all.
    #[test]
    fn a_family_not_carried_gets_no_rules_and_no_routes() {
        let both = messages(ruleset(true, true).bytes());
        for (v4, v6) in [(true, false), (false, true)] {
            let one = messages(ruleset(v4, v6).bytes());
            assert_eq!(one.len() + 8, both.len(), "v4 {v4}, v6 {v6}");
            let mut rest = both.iter();
            assert!(
                one.iter().all(|m| rest.any(|b| b == m)),
                "the captured rules, in order: v4 {v4}, v6 {v6}"
            );
            let n = Network::session(v4, v6);
            let only = if v4 { Family::V4 } else { Family::V6 };
            assert_eq!(
                n.routes.iter().map(|b| b.bytes().to_vec()).collect::<Vec<_>>(),
                [routes(only).bytes()]
            );
        }
        assert!(Network::session(false, false).routes.is_empty(), "no family, no routes");
    }

    /// A kind goes through its byte and back; a planted one is at the session's address of its family, at its
    /// protocol's port; the keeper makes the netlink ones always, the planted ones only of the families asked.
    #[test]
    fn kinds_and_where_they_are() {
        for k in Kind::ALL {
            assert_eq!(Kind::from_code(k.code()), Some(k));
        }
        assert_eq!(Kind::from_code(9), None);
        assert_eq!(Kind::Icmp4.at(), None, "the raw socket is at no address");
        assert_eq!(Kind::Tcp4.at(), Some("192.0.2.1:15001".parse().unwrap()));
        assert_eq!(Kind::Udp6.at(), Some("[2001:db8::1]:15053".parse().unwrap()));
        assert_eq!(Kind::Route.at(), None);
        assert_eq!((Kind::Dns4.at(), Kind::Dns6.at()), (None, None), "made unbound: tarae binds them");
        assert_eq!(
            Kind::Dns4.dns_at(),
            Some("192.0.2.1:53".parse().unwrap()),
            "the session's own, not every address"
        );
        assert_eq!(Kind::Dns6.dns_at(), Some("[2001:db8::1]:53".parse().unwrap()));
        assert_eq!(Kind::wanted(false, false), [Kind::Route, Kind::Netfilter]);
        assert_eq!(Kind::wanted(true, true).len(), 8, "and the raw one: 9, one message (`channel`'s test)");
        assert_eq!(
            Kind::wanted(false, true),
            [Kind::Route, Kind::Netfilter, Kind::Tcp6, Kind::Udp6, Kind::Dns6]
        );
    }
}
