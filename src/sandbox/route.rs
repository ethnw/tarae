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
    /// Beyond this machine where the session's network takes it (6d, TPROXY): the run's own socket, marked as its
    /// run's (`marks.rs`), the call made in the session — whose rules hand it to a planted socket, tarae's egress
    /// proxy then deciding its host as it does a CONNECT's.
    Redirected,
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
    /// The session's own network takes what goes beyond (6d: its keeper has a TPROXY network — `redirect.rs` — and
    /// the call is a TCP socket's: the supervisor's to say): there, beyond this machine is `Redirected` rather than
    /// refused as direct — every way out is the proxy's to decide. Datagrams but DNS stay as they were (refused).
    pub redirect: bool,
    /// The session answers names itself (6e: its keeper has a TPROXY network): a question to any address's port 53 —
    /// UDP or TCP, a loopback resolver's too, a resolver `resolv.conf` names or not — is `Redirected`, with the
    /// network open: the session's rules take it to a planted socket, where tarae answers it. Nothing of it leaves.
    pub names: bool,
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

/// The cloud metadata servers' own names, as their providers document them (Google Cloud's three, AWS's two):
/// what the egress proxy refuses by name (step 5g) — through an upstream proxy, which looks names up itself,
/// tarae never sees their addresses. A partial check: another name leading there (a wildcard DNS's) isn't caught.
const METADATA_NAMES: [&str; 5] =
    ["metadata.google.internal", "metadata.goog", "metadata", "instance-data", "instance-data.ec2.internal"];

/// Whether `name` (as `proxy::parse::name` makes it: lowercase, no trailing dot) is a metadata server's own.
pub fn never_named(name: &str) -> bool {
    METADATA_NAMES.contains(&name)
}

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
    // (6e: a name asked is the session's to answer, wherever the question is addressed — but the session's own
    // server and what the user opened come first: a server of a run's listening at that very loopback address and
    // port 53 is the session's (`listens`, which at port 53 is the caller's word for that address alone: tarae's DNS
    // socket is at every address); port 53 opened (`llm.sandbox-ports`, `-localhost`) is this machine's own server,
    // on loopback or one of its own addresses, as before 6e)
    if netns && reach.names && reach.network && to.port() == RESOLVER_PORT {
        let opened = reach.localhost || reach.ports.contains(&to.port());
        if this_machine(ip) && listens() {
            return Route::Session;
        }
        if opened && this_machine(ip) {
            return Route::Machine;
        }
        if opened && !never(ip) && mine(ip) == Some(true) {
            return Route::OwnAddress;
        }
        return Route::Redirected;
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
        (true, true) if netns && reach.redirect && !resolver => Route::Redirected,
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

/// The search list `resolv.conf`'s text gives, as glibc reads it: the last `search` or `domain` line's names (each
/// line replaces the one before), lowercase, no trailing dot, at most 6 (its `MAXDNSRCH`); `.` alone is none.
pub fn search(conf: &str) -> Vec<String> {
    let mut list = Vec::new();
    for l in conf.lines() {
        let mut words = l.split_whitespace();
        if matches!(words.next(), Some("search" | "domain")) {
            list = words
                .take_while(|w| !w.starts_with('#') && !w.starts_with(';'))
                .map(|w| w.trim_end_matches('.').to_ascii_lowercase())
                .filter(|w| !w.is_empty())
                .take(6)
                .collect();
        }
    }
    list
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
        let closed = Reach {
            localhost: false,
            ports: &[],
            network: false,
            resolvers: &resolvers,
            proxy: None,
            redirect: false,
            names: false,
        };
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
        let closed = Reach {
            localhost: false,
            ports: &[],
            network: false,
            resolvers: &resolvers,
            proxy: None,
            redirect: false,
            names: false,
        };
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

    /// Where the session's network takes what goes beyond (6d: a TPROXY keeper), beyond this machine is `Redirected`
    /// — the run's own socket, marked, to the planted socket — with the proxy's variable set or not: a tool that
    /// ignores `HTTPS_PROXY` is no longer refused as direct, its host the proxy's to decide. What isn't beyond stays as
    /// it was: the session's loopback, opened ports, this machine's own addresses, the never-reached, a resolver's
    /// port 53 (6e's), the network closed. Without a netns (C) there's no session's network: as before.
    #[test]
    fn the_sessions_network_takes_what_goes_beyond() {
        let resolvers: Vec<SocketAddr> =
            ["127.0.0.53:53", "169.254.169.254:53", "10.0.0.2:53"].map(at).into();
        let closed = Reach {
            localhost: false,
            ports: &[],
            network: false,
            resolvers: &resolvers,
            proxy: None,
            redirect: true,
            names: false,
        };
        let proxied = Reach { network: true, proxy: Some(PROXY), ..closed };
        let open = Reach { network: true, ..closed };
        let own = |ip: IpAddr| Some(ip == "192.168.1.5".parse::<IpAddr>().unwrap());
        use Refusal::*;
        use Route::*;
        // (to, reach, in the session's netns, in C)
        let rows: Vec<(&str, Reach, Route, Route)> = vec![
            ("93.184.216.34:443", proxied, Redirected, Refused(Direct)),
            ("[2606:2800:220:1::1]:443", proxied, Redirected, Refused(Direct)),
            ("10.0.0.5:22", proxied, Redirected, Refused(Direct)),
            ("93.184.216.34:443", open, Redirected, Out),
            ("[::ffff:93.184.216.34]:80", open, Redirected, Out),
            ("192.0.2.1:3128", proxied, Proxy, Proxy),
            ("127.0.0.1:3000", proxied, Session, Refused(Closed)),
            ("192.168.1.5:8888", proxied, Refused(Closed), Refused(Closed)),
            ("169.254.169.254:80", proxied, Refused(Never), Refused(Never)),
            ("[fe80::1]:443", open, Refused(Never), Refused(Never)),
            ("10.0.0.2:53", proxied, Refused(Direct), Refused(Direct)),
            ("10.0.0.2:53", open, Out, Out),
            ("93.184.216.34:443", closed, Refused(Closed), Refused(Closed)),
        ];
        for (to, reach, netns, c) in rows {
            let got =
                (route(at(to), true, &reach, || false, own), route(at(to), false, &reach, || false, own));
            assert_eq!(got, (netns, c), "{to} under {reach:?}");
        }
    }

    /// Where the session answers names (6e), a question to any address's port 53 is the session's — `Redirected`, to
    /// a planted socket where tarae answers it — whoever it's addressed to: a resolver `resolv.conf` names (the
    /// loopback stub, GCP's link-local one, a LAN's), one it doesn't, loopback's `::1`; a datagram to another port
    /// stays refused. With the network closed it's as before (the session's own loopback: its question reaches the
    /// planted socket unmarked, and is refused there), and without a netns (C) there's nothing to answer it.
    #[test]
    fn where_the_session_answers_names_every_question_is_its_own() {
        let resolvers: Vec<SocketAddr> =
            ["127.0.0.53:53", "169.254.169.254:53", "10.0.0.2:53"].map(at).into();
        let closed = Reach {
            localhost: false,
            ports: &[],
            network: false,
            resolvers: &resolvers,
            proxy: None,
            redirect: true,
            names: true,
        };
        let open = Reach { network: true, proxy: Some(PROXY), ..closed };
        let datagram = Reach { redirect: false, ..open };
        // (port 53 opened by the user: this machine's own server comes first)
        let port53 = Reach { ports: &[53], ..datagram };
        let every_port = Reach { localhost: true, ..datagram };
        let own = |ip: IpAddr| Some(ip == "192.168.1.5".parse::<IpAddr>().unwrap());
        use Refusal::*;
        use Route::*;
        // (to, reach, in the session's netns, in C)
        let rows: Vec<(&str, Reach, Route, Route)> = vec![
            ("127.0.0.53:53", open, Redirected, Refused(Direct)),
            ("169.254.169.254:53", open, Redirected, Refused(Direct)),
            ("10.0.0.2:53", datagram, Redirected, Refused(Direct)),
            ("8.8.8.8:53", datagram, Redirected, Refused(Direct)),
            ("[::1]:53", datagram, Redirected, Refused(Closed)),
            ("[2001:4860:4860::8888]:53", open, Redirected, Refused(Direct)),
            ("93.184.216.34:443", datagram, Refused(Direct), Refused(Direct)),
            ("93.184.216.34:443", open, Redirected, Refused(Direct)),
            ("127.0.0.53:80", open, Session, Refused(Closed)),
            ("127.0.0.53:53", closed, Session, Refused(Closed)),
            ("10.0.0.2:53", closed, Refused(Closed), Refused(Closed)),
            // (opened by the user: this machine's own server — loopback's, one of its own addresses' — not the
            // session's answer; beyond this machine an opened port means nothing)
            ("127.0.0.53:53", port53, Machine, Refused(Direct)),
            ("127.0.0.1:53", port53, Machine, Machine),
            ("[::1]:53", every_port, Machine, Machine),
            ("192.168.1.5:53", port53, OwnAddress, OwnAddress),
            ("192.168.1.5:53", datagram, Redirected, Refused(Closed)),
            ("8.8.8.8:53", port53, Redirected, Refused(Direct)),
        ];
        for (to, reach, netns, c) in rows {
            let got =
                (route(at(to), true, &reach, || false, own), route(at(to), false, &reach, || false, own));
            assert_eq!(got, (netns, c), "{to} under {reach:?}");
        }
        // (a run's own server listening at that very loopback address comes first — before an opened port too; for
        // an address beyond this machine whether the session listens isn't asked)
        let asked = std::cell::Cell::new(0);
        let listens = || {
            asked.set(asked.get() + 1);
            true
        };
        assert_eq!(route(at("127.0.0.1:53"), true, &open, listens, own), Session, "its own server");
        assert_eq!(route(at("127.0.0.1:53"), true, &port53, listens, own), Session, "before an opened port");
        assert_eq!(route(at("[::1]:53"), true, &datagram, listens, own), Session);
        assert_eq!(
            route(at("127.0.0.1:53"), false, &open, listens, own),
            Refused(Closed),
            "no netns: as before"
        );
        let asked_before = asked.get();
        assert_eq!(route(at("10.0.0.2:53"), true, &open, listens, own), Redirected);
        assert_eq!(asked.get(), asked_before, "beyond this machine: not asked");
    }

    /// This machine's own addresses beyond loopback (its LAN's, Docker's bridge's) are this machine: reached on
    /// an opened port (from no device: `OwnAddress`), the session listening or not; else refused — never out
    /// with the network open, never the session's. A link-local one is never reached. Whether an address is its own is asked only where it
    /// decides: an address beyond loopback, with the network open or the port opened.
    #[test]
    fn this_machines_own_addresses_are_this_machine() {
        let resolvers: Vec<SocketAddr> = vec![at("192.168.1.5:53")];
        let closed = Reach {
            localhost: false,
            ports: &[],
            network: false,
            resolvers: &resolvers,
            proxy: None,
            redirect: false,
            names: false,
        };
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
        let closed = Reach {
            localhost: false,
            ports: &[],
            network: false,
            resolvers: &resolvers,
            proxy: None,
            redirect: false,
            names: false,
        };
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

    /// The search list as glibc reads it: the last `search` or `domain` line wins whole, its names lowercase and
    /// without their trailing dot, at most six, a comment ending the line; systemd-resolved's `search .` is none.
    #[test]
    fn the_search_list_is_read_as_glibc_reads_it() {
        let gce = "nameserver 169.254.169.254\nsearch asia-northeast3-a.c.p.internal c.p.internal google.internal\n";
        assert_eq!(search(gce), ["asia-northeast3-a.c.p.internal", "c.p.internal", "google.internal"]);
        let replaced = "search old.example\ndomain Corp.Example.\noptions ndots:5\n";
        assert_eq!(search(replaced), ["corp.example"]);
        assert!(search("nameserver 127.0.0.53\nsearch .\n").is_empty(), "the stub's `.`");
        assert_eq!(search("search a b c d e f g h # the rest\n").len(), 6);
        assert_eq!(search("search a.example # comment\n"), ["a.example"]);
        assert!(search("").is_empty());
    }
}
