//! Where a run's call that names an internet address goes (`docs/design/sandbox-keeper.md` §Sockets and
//! routing) — decided here, pure, from the address, whether the run's sockets are in the session's own network
//! namespace, and what its policy opens; the supervisor makes the call where this says (`supervisor.rs`).
//!
//! In the session's netns its loopback is the session's own: a port nothing opened to this machine stays
//! there, where the session's own listener answers (or nothing does — its `lo` refuses it as any closed port).
//! A port `llm.sandbox-ports`/`-localhost` opened goes to this machine unless the session listens there itself
//! (D2). Without a netns of its own (tier C) loopback is this machine's, reached only where it's opened. This
//! machine's own addresses beyond loopback (its LAN's, Docker's bridge's) are this machine too, in every tier:
//! reached only where the port's opened — a service listening on every address isn't reached through them. Beyond
//! this machine: out with the network open, but never to link-local addresses, the cloud metadata servers tarae
//! knows of (most are link-local; `never` lists the others), multicast or broadcast (D-d); refused with it closed. The resolvers `resolv.conf` names are reached
//! on port 53 only, with the network open (D-c): a loopback one (systemd-resolved's `127.0.0.53`) on this
//! machine, another (GCP's `169.254.169.254`) out.
//!
//! (macOS has no supervisor: only the address checks — `never`, `this_machine` — serve there, its proxy's)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Where a call goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The session's own loopback: tarae's call on the run's own socket.
    Session,
    /// This machine's loopback: a socket of tarae's in its own namespace, bound to `lo`, in place of the run's.
    Machine,
    /// This machine through one of its own addresses beyond loopback, its port opened: a socket of tarae's in
    /// its own namespace, bound to no device (bound to `lo` it reaches loopback's addresses only), in place of
    /// the run's.
    OwnAddress,
    /// Beyond this machine: a socket of tarae's in its own namespace in place of the run's (the egress swap).
    Out,
    /// The egress proxy's reserved address (`PROXY`), the network going through it: a loopback TCP pair of
    /// tarae's, one end in the run's place, the other the proxy's (`supervisor::proxied`).
    Proxy,
    Refused(Refusal),
}

/// Where a run's proxy variables point while its network goes through tarae's proxy (step 5): the session's
/// documentation address (`keeper::SESSION_V4`), never anyone's listener — a connect there is the proxy's.
pub const PROXY: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 3128);

/// Why a call is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The network is closed, or this machine's port isn't opened (no netns of its own).
    Closed,
    /// An address never reached: link-local, the cloud metadata servers, multicast, broadcast (D-d).
    Never,
    /// Beyond this machine directly, while the network goes through the proxy: only the proxy goes out (a tool
    /// that ignores `HTTPS_PROXY` can't).
    Direct,
}

/// What a run's policy opens.
#[derive(Clone, Copy, Debug)]
pub struct Reach<'a> {
    /// Every port on this machine (`llm.sandbox-localhost`).
    pub localhost: bool,
    /// These ports on this machine (`llm.sandbox-ports`).
    pub ports: &'a [u16],
    /// Beyond this machine (`llm.sandbox-network`).
    pub network: bool,
    /// The resolvers `resolv.conf` names, each at its port 53 (`resolvers`).
    pub resolvers: &'a [SocketAddr],
    /// The network goes through the proxy at this address (`PROXY`): a connect there is `Proxy`, and beyond
    /// this machine nothing else goes out.
    pub proxy: Option<SocketAddr>,
}

/// The port a resolver is reached on.
pub const RESOLVER_PORT: u16 = 53;

/// `ip`, an IPv4-mapped IPv6 address as the IPv4 one it is.
fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// This machine: a loopback address, or none (`0.0.0.0` and `::` — a connect there is this machine; a bind
/// there, every address of the namespace's).
pub fn this_machine(ip: IpAddr) -> bool {
    let ip = plain(ip);
    ip.is_loopback() || ip.is_unspecified()
}

/// The cloud metadata servers that aren't link-local, as their providers document them — most (AWS's, Azure's,
/// Google's, Oracle's, Tencent's IPv4 ones) are at `169.254.169.254` or elsewhere in `169.254.0.0/16`, which
/// `never` refuses whole: Alibaba Cloud's, in the shared address space; AWS's IPv6 instance metadata and EKS Pod
/// Identity agent; Google Cloud's IPv6 one.
const METADATA: [IpAddr; 4] = [
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)),
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x23)),
    IpAddr::V6(Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254)),
];

/// An address never reached beyond this machine (D-d): link-local (`169.254.0.0/16` with `169.254.169.254`,
/// `fe80::/10`), the cloud metadata servers that aren't (`METADATA`), multicast, broadcast — and an IPv6 address
/// carrying one of those IPv4 ones to a gateway (`carried`).
pub fn never(ip: IpAddr) -> bool {
    let ip = plain(ip);
    METADATA.contains(&ip)
        || match ip {
            IpAddr::V4(v4) => v4.is_link_local() || v4.is_multicast() || v4.is_broadcast(),
            IpAddr::V6(v6) => {
                v6.is_unicast_link_local()
                    || v6.is_multicast()
                    || carried(v6).is_some_and(|v4| never(IpAddr::V4(v4)))
            }
        }
}

/// The IPv4 address an IPv6 one carries to a gateway that translates it: NAT64's well-known prefix
/// (`64:ff9b::/96`, its last 32 bits) or 6to4 (`2002::/16`, the 32 bits after it).
fn carried(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    match s {
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => Some(v4(hi, lo)),
        [0x2002, hi, lo, ..] => Some(v4(hi, lo)),
        _ => None,
    }
}

/// Where a call naming `to` goes, from a run whose sockets are in the session's own network namespace or
/// not (`netns`), under `reach`. `listens`: whether the session listens at `to`'s port on its loopback —
/// asked only where the answer turns on it (an opened port, a resolver's). `mine`: whether an address beyond
/// loopback is one of this machine's own (its LAN's, a bridge's — Docker's `172.17.0.1`), where a listener on
/// every address answers as on loopback: reached as loopback's opened ports are, never the session's — asked
/// only where it decides; None where it can't be told (the addresses unread): refused, never opened.
pub fn route(
    to: SocketAddr,
    netns: bool,
    reach: &Reach,
    listens: impl FnOnce() -> bool,
    mine: impl FnOnce(IpAddr) -> Option<bool>,
) -> Route {
    let ip = plain(to.ip());
    if reach.proxy.is_some_and(|p| plain(p.ip()) == ip && p.port() == to.port()) {
        return Route::Proxy;
    }
    let resolver = reach.resolvers.iter().any(|r| plain(r.ip()) == ip && r.port() == to.port());
    // (through the proxy names are looked up by the proxy: no tool measured that goes through it looked one up
    // itself — step 5c's Q4 — and an open port 53 would be a way out no rule sees)
    if resolver && reach.proxy.is_some() {
        return Route::Refused(Refusal::Direct);
    }
    let opened = reach.localhost || reach.ports.contains(&to.port()) || (reach.network && resolver);
    if this_machine(ip) {
        return match (netns, opened) {
            (true, false) => Route::Session,
            (true, true) if listens() => Route::Session,
            (_, true) => Route::Machine,
            (false, false) => Route::Refused(Refusal::Closed),
        };
    }
    if (reach.network || opened) && !never(ip) {
        match mine(ip) {
            Some(true) if opened => return Route::OwnAddress,
            Some(true) | None => return Route::Refused(Refusal::Closed),
            Some(false) => {}
        }
    }
    match (reach.network, resolver || !never(ip)) {
        (false, _) => Route::Refused(Refusal::Closed),
        (true, true) if reach.proxy.is_some() => Route::Refused(Refusal::Direct),
        (true, true) => Route::Out,
        (true, false) => Route::Refused(Refusal::Never),
    }
}

/// The resolvers `resolv.conf`'s text names, as glibc reads it: `nameserver <address>` lines, the first three
/// (its `MAXNS`), a link-local one's `%zone` left off; comments (`#`, `;`) and lines it can't read skipped —
/// each at `RESOLVER_PORT`.
pub fn resolvers(conf: &str) -> Vec<SocketAddr> {
    conf.lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            (words.next()? == "nameserver").then_some(())?;
            let at = words.next()?;
            let ip: IpAddr = at.split('%').next()?.parse().ok()?;
            Some(SocketAddr::new(ip, RESOLVER_PORT))
        })
        .take(3)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Every row of step 4's table (`target/sb/step4-plan.md` §3), in the session's netns and without one (C):
    /// loopback stays the session's unless opened and nothing of the session's listens there; opened, it's this
    /// machine's; without a netns it's this machine's where opened, else refused; beyond: out with the network
    /// open, never to link-local, multicast or broadcast, but a resolver's port 53 out or here.
    #[test]
    fn the_route_table() {
        let resolvers: Vec<SocketAddr> = ["127.0.0.53:53", "169.254.169.254:53"].map(at).into();
        let closed =
            Reach { localhost: false, ports: &[], network: false, resolvers: &resolvers, proxy: None };
        let port = Reach { ports: &[3000], ..closed };
        let localhost = Reach { localhost: true, ..closed };
        let open = Reach { network: true, ..closed };
        use Refusal::*;
        use Route::*;
        // (to, reach, the session listens there, in its netns, without one)
        let rows: Vec<(&str, Reach, bool, Route, Route)> = vec![
            // 1: the session's own listener, whatever's opened
            ("127.0.0.1:3000", closed, true, Session, Refused(Closed)),
            ("127.0.0.1:3000", port, true, Session, Machine),
            ("127.0.0.1:3000", localhost, true, Session, Machine),
            // 2: not opened — the session's loopback (nothing there: refused by it), C refused
            ("127.0.0.1:3000", closed, false, Session, Refused(Closed)),
            ("127.0.0.1:9", port, false, Session, Refused(Closed)),
            // 3, 4: opened, the session not listening — this machine (D2)
            ("127.0.0.1:3000", port, false, Machine, Machine),
            ("127.0.0.1:9", localhost, false, Machine, Machine),
            // 5: the other loopback forms
            ("[::1]:3000", port, false, Machine, Machine),
            ("[::1]:3000", closed, false, Session, Refused(Closed)),
            ("[::ffff:127.0.0.1]:3000", port, true, Session, Machine),
            ("0.0.0.0:3000", port, false, Machine, Machine),
            ("[::]:3000", closed, false, Session, Refused(Closed)),
            ("127.1.2.3:3000", port, false, Machine, Machine),
            // 6, 7: a loopback resolver — this machine with the network open, the session's with it closed
            ("127.0.0.53:53", open, false, Machine, Machine),
            ("127.0.0.53:53", open, true, Session, Machine),
            ("127.0.0.53:53", closed, false, Session, Refused(Closed)),
            ("127.0.0.53:80", open, false, Session, Refused(Closed)),
            // 8, 9: GCP's resolver is the metadata server: its port 53 out, no other port of it (D-c, D-d)
            ("169.254.169.254:53", open, false, Out, Out),
            ("169.254.169.254:80", open, false, Refused(Never), Refused(Never)),
            ("169.254.169.254:443", open, false, Refused(Never), Refused(Never)),
            ("169.254.169.254:54", open, false, Refused(Never), Refused(Never)),
            ("169.254.169.254:8080", localhost, false, Refused(Closed), Refused(Closed)),
            ("[fd00:ec2::254]:80", open, false, Refused(Never), Refused(Never)),
            // (the metadata servers that aren't link-local: Alibaba's, EKS Pod Identity's, Google's IPv6 one;
            // Alibaba's neighbours in the shared space go out)
            ("100.100.100.200:80", open, false, Refused(Never), Refused(Never)),
            ("[::ffff:100.100.100.200]:80", open, false, Refused(Never), Refused(Never)),
            ("[64:ff9b::6464:64c8]:80", open, false, Refused(Never), Refused(Never)),
            ("[fd00:ec2::23]:80", open, false, Refused(Never), Refused(Never)),
            ("[fd20:ce::254]:80", open, false, Refused(Never), Refused(Never)),
            ("100.100.100.201:80", open, false, Out, Out),
            ("100.64.0.1:443", open, false, Out, Out),
            ("169.254.1.1:443", open, false, Refused(Never), Refused(Never)),
            ("[fe80::1]:443", open, false, Refused(Never), Refused(Never)),
            ("224.0.0.1:5353", open, false, Refused(Never), Refused(Never)),
            ("[ff02::1]:5353", open, false, Refused(Never), Refused(Never)),
            ("255.255.255.255:67", open, false, Refused(Never), Refused(Never)),
            ("[::ffff:169.254.169.254]:80", open, false, Refused(Never), Refused(Never)),
            // (carried to a gateway: NAT64's well-known prefix, 6to4)
            ("[64:ff9b::a9fe:a9fe]:80", open, false, Refused(Never), Refused(Never)),
            ("[64:ff9b::169.254.169.254]:443", open, false, Refused(Never), Refused(Never)),
            ("[64:ff9b::e000:1]:5353", open, false, Refused(Never), Refused(Never)),
            ("[2002:a9fe:a9fe::1]:80", open, false, Refused(Never), Refused(Never)),
            ("[2002:ffff:ffff::]:67", open, false, Refused(Never), Refused(Never)),
            ("[64:ff9b::5db8:d822]:443", open, false, Out, Out),
            ("[2002:5db8:d822::1]:443", open, false, Out, Out),
            ("[64:ff9b:1::a9fe:a9fe]:80", open, false, Out, Out),
            // 10: beyond, the network open — private ranges too
            ("93.184.216.34:443", open, false, Out, Out),
            ("10.0.0.5:22", open, false, Out, Out),
            ("192.168.1.2:8080", open, false, Out, Out),
            ("[2001:db8::1]:443", open, false, Out, Out),
            // 11: the network closed — opened ports are this machine's only
            ("93.184.216.34:443", closed, false, Refused(Closed), Refused(Closed)),
            ("93.184.216.34:3000", port, false, Refused(Closed), Refused(Closed)),
            ("93.184.216.34:443", localhost, false, Refused(Closed), Refused(Closed)),
            ("169.254.169.254:53", closed, false, Refused(Closed), Refused(Closed)),
            // 12: an IPv4-mapped address is its IPv4 one
            ("[::ffff:93.184.216.34]:443", open, false, Out, Out),
        ];
        let none = |_| Some(false);
        for (to, reach, listening, netns, c) in rows {
            let got = (
                route(at(to), true, &reach, || listening, none),
                route(at(to), false, &reach, || listening, none),
            );
            assert_eq!(got, (netns, c), "{to} under {reach:?}, the session listening: {listening}");
        }
        // (whether the session listens is read only where it decides: an opened port, a resolver's)
        let asked = std::cell::Cell::new(0);
        let listens = || {
            asked.set(asked.get() + 1);
            false
        };
        route(at("127.0.0.1:9"), true, &closed, listens, none);
        route(at("93.184.216.34:443"), true, &open, listens, none);
        assert_eq!(asked.get(), 0, "nothing opened, nothing read");
        route(at("127.0.0.1:3000"), true, &port, listens, none);
        assert_eq!(asked.get(), 1);
    }

    /// With the network through the proxy, the proxy is the only way beyond this machine: its reserved address
    /// (IPv4-mapped too) is `Proxy`, any other address beyond is refused as direct — a tool that ignores
    /// `HTTPS_PROXY` reaches nothing — and so is a resolver's port 53, on this machine or beyond (the proxy looks
    /// names up; Q4: no tool that goes through it looked one up itself), while what isn't beyond stays as it was
    /// (the session's loopback, opened ports, the never-reached). The network closed, the reserved address is
    /// nothing special: refused as any address beyond.
    #[test]
    fn the_proxy_is_the_only_way_out() {
        let resolvers: Vec<SocketAddr> = ["127.0.0.53:53", "169.254.169.254:53"].map(at).into();
        let closed =
            Reach { localhost: false, ports: &[], network: false, resolvers: &resolvers, proxy: None };
        let proxied = Reach { network: true, proxy: Some(PROXY), ..closed };
        use Refusal::*;
        use Route::*;
        let rows: Vec<(&str, Reach, Route, Route)> = vec![
            ("192.0.2.1:3128", proxied, Proxy, Proxy),
            ("[::ffff:192.0.2.1]:3128", proxied, Proxy, Proxy),
            ("192.0.2.1:3129", proxied, Refused(Direct), Refused(Direct)),
            ("93.184.216.34:443", proxied, Refused(Direct), Refused(Direct)),
            ("10.0.0.5:22", proxied, Refused(Direct), Refused(Direct)),
            ("[2001:db8::1]:443", proxied, Refused(Direct), Refused(Direct)),
            ("169.254.169.254:80", proxied, Refused(Never), Refused(Never)),
            ("127.0.0.1:3000", proxied, Session, Refused(Closed)),
            ("127.0.0.53:53", proxied, Refused(Direct), Refused(Direct)),
            ("169.254.169.254:53", proxied, Refused(Direct), Refused(Direct)),
            ("127.0.0.53:80", proxied, Session, Refused(Closed)),
            ("192.0.2.1:3128", closed, Refused(Closed), Refused(Closed)),
        ];
        let none = |_| Some(false);
        for (to, reach, netns, c) in rows {
            let got =
                (route(at(to), true, &reach, || false, none), route(at(to), false, &reach, || false, none));
            assert_eq!(got, (netns, c), "{to} under {reach:?}");
        }
    }

    /// This machine's own addresses beyond loopback (its LAN's, Docker's bridge's) are this machine: reached on
    /// an opened port (from no device: `OwnAddress`), the session listening or not; else refused — never out
    /// with the network open, never the session's. A link-local one is never reached. Whether an address is its own is asked only where it
    /// decides: an address beyond loopback, with the network open or the port opened.
    #[test]
    fn this_machines_own_addresses_are_this_machine() {
        let resolvers: Vec<SocketAddr> = vec![at("192.168.1.5:53")];
        let closed =
            Reach { localhost: false, ports: &[], network: false, resolvers: &resolvers, proxy: None };
        let port = Reach { ports: &[8888], ..closed };
        let open = Reach { network: true, ..closed };
        let open_port = Reach { network: true, ..port };
        let localhost = Reach { localhost: true, ..closed };
        let own = |ip: IpAddr| {
            Some(
                ["192.168.1.5", "172.17.0.1", "2600:1f18::5", "fe80::42"].iter().any(|o| o.parse() == Ok(ip)),
            )
        };
        use Refusal::*;
        use Route::*;
        let rows: Vec<(&str, Reach, Route)> = vec![
            ("192.168.1.5:8888", open, Refused(Closed)),
            ("172.17.0.1:2375", open, Refused(Closed)),
            ("[::ffff:172.17.0.1]:2375", open, Refused(Closed)),
            ("[2600:1f18::5]:8888", open, Refused(Closed)),
            ("192.168.1.5:8888", open_port, OwnAddress),
            ("192.168.1.5:8888", port, OwnAddress),
            ("172.17.0.1:2375", localhost, OwnAddress),
            ("192.168.1.5:8888", closed, Refused(Closed)),
            ("192.168.1.5:53", open, OwnAddress),
            ("[fe80::42]:8888", open_port, Refused(Never)),
            ("192.168.1.6:8888", open, Out),
        ];
        for (to, reach, want) in rows {
            for netns in [true, false] {
                assert_eq!(
                    route(at(to), netns, &reach, || true, own),
                    want,
                    "{to} under {reach:?}, netns {netns}"
                );
            }
        }
        let asked = std::cell::Cell::new(0);
        let mine = |_| {
            asked.set(asked.get() + 1);
            Some(false)
        };
        route(at("127.0.0.1:8888"), true, &open_port, || false, mine);
        route(at("93.184.216.34:443"), true, &closed, || false, mine);
        route(at("[fe80::42]:443"), true, &open, || false, mine);
        assert_eq!(asked.get(), 0, "loopback, everything closed, never reached: not asked");
        route(at("93.184.216.34:443"), true, &open, || false, mine);
        assert_eq!(asked.get(), 1);
    }

    /// This machine's addresses unread (`getifaddrs` failed): an address beyond loopback can't be told from this
    /// machine's own, so it's refused — never opened by a port (`llm.sandbox-ports` would otherwise reach that port
    /// anywhere), never out; loopback goes as ever, and a never-reached address stays `Never`.
    #[test]
    fn unread_own_addresses_only_refuse() {
        let resolvers: Vec<SocketAddr> = vec![at("10.0.0.2:53")];
        let closed =
            Reach { localhost: false, ports: &[], network: false, resolvers: &resolvers, proxy: None };
        let port = Reach { ports: &[3000], ..closed };
        let open = Reach { network: true, ..closed };
        let open_port = Reach { network: true, ..port };
        let localhost = Reach { localhost: true, ..closed };
        let unread = |_| None;
        use Refusal::*;
        use Route::*;
        let rows: Vec<(&str, Reach, Route, Route)> = vec![
            ("93.184.216.34:3000", port, Refused(Closed), Refused(Closed)),
            ("93.184.216.34:3000", open_port, Refused(Closed), Refused(Closed)),
            ("93.184.216.34:443", localhost, Refused(Closed), Refused(Closed)),
            ("93.184.216.34:443", open, Refused(Closed), Refused(Closed)),
            ("93.184.216.34:443", closed, Refused(Closed), Refused(Closed)),
            ("[2600:1f18::5]:443", open, Refused(Closed), Refused(Closed)),
            ("10.0.0.2:53", open, Refused(Closed), Refused(Closed)),
            ("169.254.169.254:80", open, Refused(Never), Refused(Never)),
            ("127.0.0.1:3000", port, Machine, Machine),
            ("127.0.0.1:9", open, Session, Refused(Closed)),
        ];
        for (to, reach, netns, c) in rows {
            let got = (
                route(at(to), true, &reach, || false, unread),
                route(at(to), false, &reach, || false, unread),
            );
            assert_eq!(got, (netns, c), "{to} under {reach:?}");
        }
    }

    /// `resolv.conf` as glibc reads it: `nameserver` lines only, their first three, a zone left off, the rest
    /// skipped.
    #[test]
    fn resolvers_are_read_as_glibc_reads_them() {
        let conf = "# Generated by NetworkManager\nsearch example.internal\n; nameserver 4.4.4.4\n\
                    nameserver 127.0.0.53\nnameservers 1.1.1.1\noptions edns0 trust-ad\n\
                    nameserver   fe80::1%eth0\nnameserver not-an-address\n  nameserver 8.8.8.8 # trailing\n\
                    nameserver 9.9.9.9\nnameserver 1.0.0.1\n";
        let got: Vec<String> = resolvers(conf).iter().map(|r| r.to_string()).collect();
        assert_eq!(got, ["127.0.0.53:53", "[fe80::1]:53", "8.8.8.8:53"]);
        assert!(resolvers("").is_empty());
    }
}
