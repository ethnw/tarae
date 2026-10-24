use super::*;

/// Tables as the kernel prints them (`tcp4_seq_show`, `udp4_format_sock` and their v6 twins) on a little-endian
/// machine: on port 0x1F90 (8080) a UDP socket at 127.0.0.1 (inode 1) and one at every address (2) — two that can
/// share a port only with `SO_REUSEADDR`, which is how the exact-before-wildcard order shows; on 0x1F91 a TCP
/// listener at `::` (3); on 0x1F92 one at `::ffff:127.0.0.1` (4); on 0x1F93 one at `::1` (6); and an established
/// connection at 127.0.0.1:0x1F93 (5), never a call's target.
const UDP: &str = "\
   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  125: 0100007F:1F90 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1 2 0000000000000000 0
  125: 00000000:1F90 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 2 2 0000000000000000 0
";
const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F93 0100007F:A2C4 01 00000000:00000000 00:00000000 00000000  1000        0 5 1 0000000000000000 20 4 30 10 -1
";
const TCP6: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1F91 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000100007F:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4 1 0000000000000000 100 0 0 10 0
   2: 00000000000000000000000001000000:1F93 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 6 1 0000000000000000 100 0 0 10 0
";

fn at(a: &str) -> SocketAddr {
    a.parse().unwrap()
}

/// The sockets a call lands on are the kernel's: on the port, an exact address before a wildcard one; an IPv4
/// call on an IPv6 `::` socket that isn't v6-only, and on one bound to the address's mapped form; an IPv6 call never
/// on an IPv4 socket; `127.0.0.2` not on a socket at `127.0.0.1`; an established connection on nothing.
#[test]
fn a_listener_is_found_as_the_kernel_finds_it() {
    if cfg!(target_endian = "big") {
        return;
    }
    let udp = rows(UDP, false);
    let mut tcp = rows(TCP, true);
    tcp.extend(rows(TCP6, true));
    assert_eq!(udp.len(), 2, "{udp:?}");
    assert_eq!(tcp.len(), 4, "{tcp:?}");
    // (the `::` listener v6-only, as the kernel's own list would say it)
    let only: Vec<Row> = tcp.iter().map(|r| Row { v6only: r.inode == 3, ..*r }).collect();
    let cases: [(&[Row], &str, &[u64]); 15] = [
        // (to every address: the kernel sends it to the caller's own address or loopback — every socket of its
        // family on the port; a v6-only one not for IPv4, an IPv4 one never for IPv6)
        (&udp, "0.0.0.0:8080", &[1, 2]),
        (&tcp, "0.0.0.0:8081", &[3]),
        (&only, "0.0.0.0:8081", &[]),
        (&tcp, "0.0.0.0:8082", &[4]),
        (&tcp, "[::]:8083", &[6]),
        (&udp, "127.0.0.1:8080", &[1]),
        (&udp, "127.0.0.2:8080", &[2]),
        (&udp, "[::1]:8080", &[]),
        (&tcp, "127.0.0.1:8081", &[3]),
        (&only, "127.0.0.1:8081", &[]),
        (&only, "[::1]:8081", &[3]),
        (&tcp, "127.0.0.1:8082", &[4]),
        (&tcp, "[::ffff:127.0.0.1]:8082", &[4]),
        (&tcp, "127.0.0.1:8083", &[]),
        (&tcp, "[::1]:8083", &[6]),
    ];
    for (rows, to, want) in cases {
        assert_eq!(inodes(&listener_at(rows, at(to))), want, "{to}");
    }
}

fn inodes(rows: &[Row]) -> Vec<u64> {
    rows.iter().map(|r| r.inode).collect()
}

/// A socket's `SO_COOKIE`.
#[cfg(target_os = "linux")]
fn cookie_of(fd: &dyn std::os::fd::AsRawFd) -> u64 {
    let (mut v, mut len) = (0u64, std::mem::size_of::<u64>() as libc::socklen_t);
    // SAFETY: a u64 and its length, live for the call, on a descriptor of ours.
    let r = unsafe {
        libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_COOKIE, (&raw mut v).cast(), &mut len)
    };
    assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
    v
}

/// The kernel's own list of a port's sockets (`diag::on_port`) holds what a call to that port may land on and
/// nothing else, each by the inode and the cookie (`SO_COOKIE`) its descriptor has: on one port a TCP listener at
/// 127.0.0.1 and a v6-only one
/// at `::`; on another a UDP socket connected to no one at `::` (dual-stack) and a connected one beside it — not
/// listed, nor a TCP listener on a third port. All this test's own, nothing sent.
#[cfg(target_os = "linux")]
#[test]
fn the_kernel_lists_a_ports_sockets_with_their_v6only() {
    use std::os::fd::AsRawFd;
    let ino = |fd: &dyn AsRawFd| {
        // SAFETY: a plain out-struct for the call to fill, on a descriptor of ours.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) }, 0);
        st.st_ino as u64
    };
    // (the port the kernel gave the IPv4 one is free at 127.0.0.1 only: another test's IPv6 socket may hold it at
    // `[::1]` — frictions33, `EADDRINUSE` now and then in a full suite. A pair that fits is found again.)
    let (v4, port, v6) = (0..50)
        .find_map(|_| {
            let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = v4.local_addr().unwrap().port();
            match v6only_listener(port) {
                Ok(v6) => Some((v4, port, v6)),
                Err(e) if e.raw_os_error() == Some(libc::EADDRINUSE) => None,
                Err(e) => panic!("a v6-only listener: {e}"),
            }
        })
        .expect("a port free in both families within 50 tries");
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("[::]:0").unwrap();
    let uport = udp.local_addr().unwrap().port();
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let connected = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    connected.connect(peer.local_addr().unwrap()).unwrap();
    let mut tcp = diag::on_port(true, port).unwrap();
    tcp.sort_by_key(|r| r.inode);
    let row = |local: String, fd: &dyn AsRawFd, v6only| Row {
        local: at(&local),
        inode: ino(fd),
        cookie: Some(cookie_of(fd)),
        open: true,
        v6only,
        peer: None,
    };
    let mut want = vec![row(format!("127.0.0.1:{port}"), &v4, false), row(format!("[::]:{port}"), &v6, true)];
    want.sort_by_key(|r| r.inode);
    assert_eq!(tcp, want);
    let udp_rows = diag::on_port(false, uport).unwrap();
    assert_eq!(udp_rows, [row(format!("[::]:{uport}"), &udp, false)]);
    // (every socket's cookie: these among them, the connected one too)
    let every = diag::every().unwrap();
    assert!([&v4 as &dyn AsRawFd, &v6, &udp, &connected].iter().all(|fd| every.contains(&cookie_of(*fd))));
    // A connect to every address lands on loopback's listener (the kernel's own rule), which the lookup finds there
    assert!(std::net::TcpStream::connect((std::net::Ipv4Addr::UNSPECIFIED, port)).is_ok());
    assert_eq!(inodes(&listener_at(&tcp, at(&format!("0.0.0.0:{port}")))), [ino(&v4)]);
    let cport = connected.local_addr().unwrap().port();
    assert_eq!(diag::on_port(false, cport).unwrap(), [], "a connected one");
    // (what a bind on the peer's port could take from: the peer there, and the one connected to it — with its peer)
    let to_peer = peer.local_addr().unwrap();
    let mut aimed: Vec<(u64, Option<SocketAddr>)> =
        diag::bound_on(false, to_peer.port()).unwrap().iter().map(|r| (r.inode, r.peer)).collect();
    aimed.sort();
    let mut want = vec![(ino(&peer), None), (ino(&connected), Some(to_peer))];
    want.sort();
    assert_eq!(aimed, want, "a bind on the peer's port");
    assert!(
        diag::on_port(true, other.local_addr().unwrap().port())
            .unwrap()
            .iter()
            .all(|r| r.inode == ino(&other))
    );
}

/// A TCP listener at `[::]:port`, `IPV6_V6ONLY` (beside an IPv4 one on that port).
#[cfg(target_os = "linux")]
fn v6only_listener(port: u16) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: plain calls on a socket of ours, an address and an int of the sizes given.
    unsafe {
        let fd = std::os::fd::OwnedFd::from_raw_fd(libc::socket(libc::AF_INET6, libc::SOCK_STREAM, 0));
        let one: libc::c_int = 1;
        let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = libc::setsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            (&raw const one).cast(),
            size,
        );
        assert_eq!(r, 0);
        let mut a: libc::sockaddr_in6 = std::mem::zeroed();
        a.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        a.sin6_port = port.to_be();
        let len = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
        if libc::bind(fd.as_raw_fd(), (&raw const a).cast(), len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        assert_eq!(libc::listen(fd.as_raw_fd(), 1), 0);
        Ok(fd)
    }
}

/// A socket of `tcp` (else UDP) at `at`, with `SO_REUSEADDR` and `SO_REUSEPORT` where `reuse`, its port picked
/// from `range` alone where given (`IP_LOCAL_PORT_RANGE`, 6.3: low and high, the port's both) — a TCP one listening.
#[cfg(target_os = "linux")]
pub fn bound(tcp: bool, at: &str, reuse: bool, range: Option<u16>) -> std::io::Result<std::os::fd::OwnedFd> {
    let fd = bound_only(tcp, at, reuse, range)?;
    if tcp {
        listening(&fd)?;
    }
    Ok(fd)
}

/// A TCP socket of ours listening.
#[cfg(target_os = "linux")]
pub fn listening(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain call on a socket of ours.
    match unsafe { libc::listen(fd.as_raw_fd(), 1) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// `bound`, a TCP one not listening.
#[cfg(target_os = "linux")]
pub fn bound_only(
    tcp: bool,
    at: &str,
    reuse: bool,
    range: Option<u16>,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let kind = if tcp { libc::SOCK_STREAM } else { libc::SOCK_DGRAM };
    let last = || Err(std::io::Error::last_os_error());
    // SAFETY: plain calls on a socket of ours, an address and ints of the sizes given.
    unsafe {
        let mut sa: libc::sockaddr_in6 = std::mem::zeroed();
        let (family, len) = match self::at(at) {
            SocketAddr::V4(v4) => {
                let a = &mut *(&raw mut sa).cast::<libc::sockaddr_in>();
                a.sin_family = libc::AF_INET as libc::sa_family_t;
                a.sin_port = v4.port().to_be();
                a.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
                (libc::AF_INET, std::mem::size_of::<libc::sockaddr_in>())
            }
            SocketAddr::V6(v6) => {
                sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sa.sin6_port = v6.port().to_be();
                sa.sin6_addr.s6_addr = v6.ip().octets();
                (libc::AF_INET6, std::mem::size_of::<libc::sockaddr_in6>())
            }
        };
        let fd = libc::socket(family, kind | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return last();
        }
        let fd = std::os::fd::OwnedFd::from_raw_fd(fd);
        let set = |level, opt, v: u32| {
            let size = std::mem::size_of::<u32>() as libc::socklen_t;
            libc::setsockopt(fd.as_raw_fd(), level, opt, (&raw const v).cast(), size)
        };
        if reuse
            && (set(libc::SOL_SOCKET, libc::SO_REUSEADDR, 1) != 0
                || set(libc::SOL_SOCKET, libc::SO_REUSEPORT, 1) != 0)
        {
            return last();
        }
        // (`IP_LOCAL_PORT_RANGE`, at `SOL_IP` for both families)
        if let Some(p) = range
            && set(libc::SOL_IP, 51, u32::from(p) | (u32::from(p) << 16)) != 0
        {
            return last();
        }
        if libc::bind(fd.as_raw_fd(), (&raw const sa).cast(), len as libc::socklen_t) != 0 {
            return last();
        }
        Ok(fd)
    }
}

/// The port a socket of ours is bound to.
#[cfg(target_os = "linux")]
pub fn port_of(fd: &std::os::fd::OwnedFd) -> u16 {
    use std::os::fd::AsRawFd;
    let mut a: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    // SAFETY: an address buffer and its length, on a descriptor of ours.
    assert_eq!(unsafe { libc::getsockname(fd.as_raw_fd(), (&raw mut a).cast(), &mut len) }, 0);
    u16::from_be(a.sin6_port)
}

/// Why a UDP socket's port the kernel picks is picked with its reuse options off (`supervisor::alone`), and a TCP
/// one's needs nothing — the held socket's port the only one the kernel may pick (`IP_LOCAL_PORT_RANGE`, 6.3+):
/// beside a socket there with `SO_REUSEADDR` and `SO_REUSEPORT` (a TCP one listening), a bind naming the port with
/// both joins it (the control: they're what lets it); a TCP bind to port 0 with both is refused it (the kernel never
/// shares a TCP port it picks), a UDP one is given it — another session's port, its datagrams then shared — unless
/// made through `alone`, which leaves the options as they were.
#[cfg(target_os = "linux")]
#[test]
fn a_port_the_kernel_picks_is_another_sockets_only_for_udp_reusing() {
    let in_use = |r: &std::io::Result<std::os::fd::OwnedFd>| r.as_ref().err().and_then(|e| e.raw_os_error());
    for tcp in [true, false] {
        let held = bound(tcp, "127.0.0.1:0", true, None).unwrap();
        let port = port_of(&held);
        let picked = bound(tcp, "127.0.0.1:0", true, Some(port));
        if in_use(&picked) == Some(libc::ENOPROTOOPT) {
            let line = format!(
                "{}sandbox::owners::tests::a_port_the_kernel_picks_is_another_sockets_only_for_udp_reusing — no \
                 IP_LOCAL_PORT_RANGE here\n",
                crate::test_results::SELF_SKIP
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            return;
        }
        let named = bound(tcp, &format!("127.0.0.1:{port}"), true, None);
        assert_eq!(named.as_ref().map(port_of).ok(), Some(port), "tcp {tcp}: named");
        match tcp {
            true => assert_eq!(in_use(&picked), Some(libc::EADDRINUSE), "TCP picked"),
            false => assert_eq!(picked.as_ref().map(port_of).ok(), Some(port), "UDP picked"),
        }
    }
    // `alone`: the port isn't given, and both options are back after
    use std::os::fd::{AsRawFd, FromRawFd};
    let held = bound(false, "127.0.0.1:0", true, None).unwrap();
    let port = port_of(&held);
    // SAFETY: plain call; the descriptor is ours from here.
    let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0)) };
    let set = |(level, opt): (i32, i32), v: u32| {
        let size = std::mem::size_of::<u32>() as libc::socklen_t;
        // SAFETY: an int of the size given, live for the call, on a socket of ours.
        let r = unsafe { libc::setsockopt(sock.as_raw_fd(), level, opt, (&raw const v).cast(), size) };
        assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
    };
    set((libc::SOL_SOCKET, libc::SO_REUSEADDR), 1);
    set((libc::SOL_SOCKET, libc::SO_REUSEPORT), 1);
    set((libc::SOL_IP, 51), u32::from(port) | (u32::from(port) << 16));
    let (a, len) = super::super::proxy::raw_addr(at("127.0.0.1:0"));
    // SAFETY: the socket and an address of ours, live for the call.
    let bind = || match unsafe { libc::bind(sock.as_raw_fd(), (&raw const a).cast(), len) } {
        0 => Ok(0),
        _ => Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)),
    };
    let refused = |why| panic!("refused: {why}");
    assert_eq!(super::super::supervisor::alone(&sock, bind, refused), Err(libc::EADDRINUSE), "alone");
    let opt = |o| super::super::supervisor::sockopt(&sock, o);
    assert_eq!((opt(libc::SO_REUSEADDR), opt(libc::SO_REUSEPORT)), (Ok(1), Ok(1)), "put back");
}

/// M8.1's stand-in: what a lookup costs a call to an opened loopback port — the port's sockets asked, `anothers`
/// — median and 90th of 101, with 100, 1 000 and 10 000 TCP listeners, then as many UDP sockets, in this test's
/// network namespace: through `NETLINK_SOCK_DIAG` (`diag::on_port`, what's built), and `/proc/net/{tcp,udp}[6]`
/// read and parsed whole (what the plan began with, linear in the namespace's sockets — measured 1.4, 2.2 and
/// 12 ms for TCP in colima's VM, 2026-10-14: over the plan's ≈ 1 ms). The sockets are held by this binary re-run on
/// `loopback_listeners`, `PER_HOLDER` each (a process's descriptors are limited; this one's is never changed).
#[cfg(target_os = "linux")]
#[test]
fn perf_a_loopback_lookup() {
    if crate::test_results::unoptimized("sandbox::owners::tests::perf_a_loopback_lookup") {
        return;
    }
    let mut said = Vec::new();
    for (tcp, proto) in [(true, "tcp"), (false, "udp")] {
        let mut holders = Vec::new();
        for n in [100, 1000, 10_000] {
            said.push(format!("{proto} {}", lookup_timed(tcp, n, &mut holders)));
        }
    }
    let line = format!("M8.1 stand-in, a loopback lookup — {}\n", said.join(" · "));
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
}

/// `perf_a_loopback_lookup` with `n` sockets of `tcp` (else UDP) held — `holders` grown to that —, in words.
#[cfg(target_os = "linux")]
fn lookup_timed(tcp: bool, n: usize, holders: &mut Vec<Holder>) -> String {
    let held = |h: &[Holder]| h.iter().map(|h| h.n).sum::<usize>();
    while held(holders) < n {
        holders.push(Holder::new((n - held(holders)).min(PER_HOLDER), tcp));
    }
    let to = holders[0].first;
    let o = Owners::with_cap(KEPT);
    let names = if tcp { ["tcp", "tcp6"] } else { ["udp", "udp6"] };
    let proc = || {
        let read = |t: &str| std::fs::read_to_string(format!("/proc/thread-self/net/{t}")).unwrap();
        let mut all = rows(&read(names[0]), tcp);
        all.extend(rows(&read(names[1]), tcp));
        all
    };
    let timed = |ask: &dyn Fn() -> Vec<Row>| {
        let mut times: Vec<std::time::Duration> = (0..101)
            .map(|_| {
                let at = std::time::Instant::now();
                std::hint::black_box(o.anothers(&ask(), to, 1));
                at.elapsed()
            })
            .collect();
        times.sort();
        format!("{:.3} ms, 90th {:.3}", ms(times[50]), ms(times[90]))
    };
    let diag = || diag::on_port(tcp, to.port()).unwrap();
    assert_eq!(diag().len(), 1, "the port's socket alone");
    let rows = proc().len();
    format!("{n} ({rows} rows): sock_diag {}, /proc {}", timed(&diag), timed(&proc))
}

#[cfg(target_os = "linux")]
const PER_HOLDER: usize = 900;

/// A process of this binary's holding TCP listeners or UDP sockets (`loopback_listeners`): the first one's address;
/// ended, and waited for, when dropped.
#[cfg(target_os = "linux")]
struct Holder {
    child: std::process::Child,
    /// Its output, read to its end when it's dropped (closed early, the holder's last lines would meet EPIPE).
    out: std::io::BufReader<std::process::ChildStdout>,
    n: usize,
    first: SocketAddr,
}

#[cfg(target_os = "linux")]
impl Holder {
    fn new(n: usize, tcp: bool) -> Holder {
        use std::io::BufRead;
        let name = "sandbox::owners::tests::loopback_listeners";
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env("TARAE_LISTENERS", format!("{n}{}", if tcp { "" } else { " udp" }))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        // (anywhere in its line: libtest may have begun one)
        let first = (&mut out)
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.split_once("listening ").map(|(_, a)| a.trim().parse().unwrap()))
            .expect("the holder said nothing");
        Holder { child, out, n, first }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Holder {
    fn drop(&mut self) {
        // (its stdin closed: it ends)
        drop(self.child.stdin.take());
        std::io::Read::read_to_end(&mut self.out, &mut Vec::new()).ok();
        self.child.wait().ok();
    }
}

/// `$TARAE_LISTENERS` listeners on loopback (`<n> udp`: UDP sockets), the first's address said, held until stdin
/// ends — a minute at most.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "run by perf_a_loopback_lookup in a process of its own"]
fn loopback_listeners() {
    let Ok(asked) = std::env::var("TARAE_LISTENERS") else { return };
    let (n, udp) = match asked.split_once(' ') {
        Some((n, _)) => (n.parse::<usize>().unwrap(), true),
        None => (asked.parse::<usize>().unwrap(), false),
    };
    let held: Vec<(SocketAddr, Box<dyn std::any::Any>)> = (0..n)
        .map(|_| -> (SocketAddr, Box<dyn std::any::Any>) {
            match udp {
                true => {
                    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                    (s.local_addr().unwrap(), Box::new(s))
                }
                false => {
                    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                    (l.local_addr().unwrap(), Box::new(l))
                }
            }
        })
        .collect();
    println!("listening {}", held[0].0);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new()).ok();
        tx.send(()).ok();
    });
    rx.recv_timeout(crate::test_results::PATIENCE).ok();
}

#[cfg(target_os = "linux")]
fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// A call is refused only for another session's socket: its own session's and nobody's (the user's) aren't —
/// here by `/proc/net`'s rows, which name a socket by its inode alone.
#[test]
fn only_another_sessions_socket_is_found() {
    let o = Owners::with_cap(KEPT);
    let none = || None;
    let udp = rows(UDP, false);
    o.made((101, 1), 7, 1, none);
    assert_eq!(o.anothers(&udp, at("127.0.0.1:8080"), 7), None, "its own");
    assert_eq!(o.anothers(&udp, at("127.0.0.1:8080"), 8), Some(7), "another's");
    assert_eq!(o.anothers(&udp, at("127.0.0.2:8080"), 8), None, "nobody's (2)");
}

/// Where the kernel's list can't be had, tarae's tables answer (by the inode; IPv6's may be missing); where neither
/// can be read, whose the socket is can't be told — an Err, which the caller refuses (`supervisor::unless_unread`).
#[test]
fn the_kernels_list_else_taraes_tables_else_refused() {
    let o = Owners::with_cap(KEPT);
    o.made((101, 1), 7, 1, || None);
    let to = at("127.0.0.1:8080");
    let listed =
        || Ok(vec![Row { local: to, inode: 1, cookie: Some(101), open: true, v6only: false, peer: None }]);
    let unlisted = || Err(std::io::Error::other("no sock_diag here"));
    let tables = |t: &str| match t {
        "udp" => Ok(UDP.to_string()),
        _ => Err(std::io::Error::other("no IPv6 here")),
    };
    let unread = |_: &str| Err(std::io::Error::other("unread"));
    assert_eq!(o.another_at(to, (false, 8), listed, unread), Ok(true), "the kernel's list");
    assert_eq!(o.another_at(to, (false, 8), unlisted, tables), Ok(true), "tarae's tables");
    assert_eq!(o.another_at(to, (false, 7), unlisted, tables), Ok(false), "its own, by the tables");
    let neither = o.another_at(to, (false, 8), unlisted, unread);
    assert!(neither.as_ref().is_err_and(|e| e.contains("no sock_diag here")), "{neither:?}");
}

/// A bind or a listen onto a port where another session has a socket at an address that overlaps it joins it (a
/// `SO_REUSEPORT` group, an address beside a `SO_REUSEADDR` one); its own session's doesn't count, nor another
/// port, nor an address of its own (`127.0.0.3` beside `127.0.0.2`). Of TCP only a listener counts (the kernel lists
/// no other bound TCP socket); of UDP any — and one connected to the port at that address: its plain sends would
/// come to the bind.
#[test]
fn a_bind_onto_another_sessions_port_is_found() {
    let o = Owners::with_cap(KEPT);
    o.made((101, 1), 7, 1, || None);
    let row = |open| Row {
        local: at("127.0.0.2:8080"),
        inode: 1,
        cookie: Some(101),
        open,
        v6only: false,
        peer: None,
    };
    let on = |to: &str, tcp, session, open| {
        o.another_on((at(to), false), (tcp, session), || Ok(vec![row(open)]), |_| Ok(String::new()))
    };
    // (a UDP socket of 7's on another port, connected to 127.0.0.1:9000: what's bound there takes its sends)
    let aimed = Row { local: at("127.0.0.1:41000"), peer: Some(at("127.0.0.1:9000")), ..row(false) };
    let at_aimed = |to: &str, tcp, session| {
        o.another_on((at(to), false), (tcp, session), || Ok(vec![aimed]), |_| Ok(String::new()))
    };
    assert_eq!(at_aimed("127.0.0.1:9000", false, 8), Ok(true), "UDP, where another's is connected to");
    assert_eq!(at_aimed("0.0.0.0:9000", false, 8), Ok(true), "UDP, every address there");
    assert_eq!(at_aimed("127.0.0.2:9000", false, 8), Ok(false), "UDP, another address");
    assert_eq!(at_aimed("127.0.0.1:9000", false, 7), Ok(false), "its own session's");
    assert_eq!(at_aimed("127.0.0.1:9000", true, 8), Ok(false), "TCP: a connection isn't taken by a bind");
    assert_eq!(on("127.0.0.2:8080", false, 8, false), Ok(true), "UDP, connected or not");
    assert_eq!(on("0.0.0.0:8080", false, 8, false), Ok(true), "UDP, every address");
    assert_eq!(on("[::ffff:127.0.0.2]:8080", false, 8, false), Ok(true), "UDP, its mapped form");
    assert_eq!(on("127.0.0.3:8080", false, 8, false), Ok(false), "another address");
    assert_eq!(on("127.0.0.2:8080", true, 8, false), Ok(false), "TCP, not listening");
    assert_eq!(on("[::]:8080", true, 8, true), Ok(true), "TCP, listening, IPv6's every address");
    assert_eq!(on("127.0.0.2:8080", true, 7, true), Ok(false), "its own");
    assert_eq!(on("127.0.0.2:9090", false, 8, true), Ok(false), "another port");
}

/// Two sockets on one port share an address as the kernel's bind conflict says, either way round: the same one; one
/// at every address of its family; IPv6's `::` at every IPv4 address too — not when v6-only; a mapped address as
/// its IPv4 one; never an IPv4 address and a plain IPv6 one.
#[test]
fn addresses_overlap_as_the_kernel_compares_them() {
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    let cases = [
        ("127.0.0.1", false, "127.0.0.1", false, true),
        ("127.0.0.1", false, "127.0.0.2", false, false),
        ("0.0.0.0", false, "127.0.0.1", false, true),
        ("::", false, "127.0.0.1", false, true),
        ("::", true, "127.0.0.1", false, false),
        ("::", true, "::1", false, true),
        ("::", false, "0.0.0.0", false, true),
        ("::ffff:127.0.0.1", false, "127.0.0.1", false, true),
        ("::ffff:127.0.0.1", false, "0.0.0.0", false, true),
        ("::ffff:127.0.0.1", false, "::", false, true),
        ("::ffff:127.0.0.1", false, "::", true, false),
        ("::ffff:127.0.0.1", false, "::ffff:127.0.0.2", false, false),
        ("::1", false, "127.0.0.1", false, false),
        ("::1", false, "0.0.0.0", false, false),
        ("::1", false, "::1", true, true),
        ("::1", false, "::2", false, false),
    ];
    for (a, a6, b, b6, want) in cases {
        assert_eq!(overlap((ip(a), a6), (ip(b), b6)), want, "{a} ({a6}) and {b} ({b6})");
        assert_eq!(overlap((ip(b), b6), (ip(a), a6)), want, "{b} ({b6}) and {a} ({a6})");
    }
}

/// A dead keeper's rows go as their sockets do: one the kernel still lists (a process it left holds it) stays its
/// session's, asked again after a wait that doubles; an unread list keeps every row; another keeper's rows stay.
#[test]
fn a_dead_keepers_rows_go_as_their_sockets_do() {
    let o = Owners::with_cap(KEPT);
    for c in [1, 2] {
        o.made((c, c), 7, 1, || None);
    }
    o.made((3, 3), 8, 2, || None);
    let asked = std::cell::Cell::new(0);
    let lists = |n: usize| -> Option<HashSet<u64>> {
        match n {
            0 => Some([1, 2].into()),
            1 => None,
            2 => Some([2].into()),
            _ => Some(HashSet::new()),
        }
    };
    let waits = std::cell::RefCell::new(Vec::new());
    let rows_seen = std::cell::RefCell::new(Vec::new());
    o.let_go(
        1,
        || {
            rows_seen.borrow_mut().push(o.of(7));
            let l = lists(asked.get());
            asked.set(asked.get() + 1);
            l
        },
        |d| waits.borrow_mut().push(d.as_millis()),
    );
    assert_eq!(*rows_seen.borrow(), [2, 2, 2, 1], "both while listed, both while unread, then 2's");
    assert_eq!(*waits.borrow(), [100, 200, 400]);
    assert_eq!((o.of(7), o.of(8)), (0, 1));
}

/// A dead keeper's rows are asked after for `LET_GO_FOR`, no longer — a socket still listed then (a daemon that
/// outlived it for good) stays its session's, said by the count returned; an unread list stops there too.
#[test]
fn a_dead_keepers_rows_are_asked_after_for_an_hour() {
    for listed in [true, false] {
        let o = Owners::with_cap(KEPT);
        o.made((1, 1), 7, 1, || None);
        let waited = std::cell::Cell::new(std::time::Duration::ZERO);
        let live = || listed.then(|| [1].into());
        let left = o.let_go(1, live, |d| waited.set(waited.get() + d));
        assert_eq!((left, o.of(7)), (1, 1), "listed {listed}: kept, said");
        let w = waited.get();
        assert!(
            w >= LET_GO_FOR && w < LET_GO_FOR + std::time::Duration::from_secs(30),
            "listed {listed}: {w:?}"
        );
    }
}

/// A row names one socket ever, by its cookie: a newer socket the kernel gave the same inode number (the counter
/// pipes and pseudo files share wraps) isn't the closed one's session's — only `/proc/net`'s rows, which have no
/// cookie, are found by the number, and that errs to refusing.
#[test]
fn a_stale_row_never_names_a_newer_socket() {
    let o = Owners::with_cap(KEPT);
    o.made((101, 1), 7, 1, || None);
    let row =
        |cookie| Row { local: at("127.0.0.1:8080"), inode: 1, cookie, open: true, v6only: false, peer: None };
    let to = at("127.0.0.1:8080");
    assert_eq!(o.anothers(&[row(Some(101))], to, 8), Some(7), "the socket itself");
    assert_eq!(o.anothers(&[row(Some(202))], to, 8), None, "a newer socket with its number: nobody's");
    assert_eq!(o.anothers(&[row(None)], to, 8), Some(7), "/proc/net's: by the number");
    // (its keeper gone, its number goes with it)
    o.ended(1);
    assert_eq!(o.anothers(&[row(None)], to, 8), None);
}

/// Rows go with the keeper their runs were below, no other's — a superseded keeper of the same session takes only
/// its own. Past the cap a sweep keeps what's in tarae's tables and what was touched since the sweep before (a
/// socket made a moment ago may be about to bind itself): the first keeps all, the second drops the untouched
/// that aren't there; the next waits for twice what stayed.
#[test]
fn rows_go_with_their_keeper_and_past_the_cap() {
    let o = Owners::with_cap(4);
    let live: HashSet<u64> = [1, 2].into();
    let reads = std::cell::Cell::new(0);
    let sweep = || {
        reads.set(reads.get() + 1);
        Some(live.clone())
    };
    // (each socket's cookie its inode here: what's swept is by cookie)
    for c in 1..=3 {
        o.made((c, c), 7, 1, sweep);
    }
    o.made((100, 100), 8, 3, sweep);
    assert_eq!(reads.get(), 0, "under the cap: no read");
    o.made((4, 4), 7, 2, sweep);
    assert_eq!((reads.get(), o.of(7)), (1, 4), "all touched since: all kept");
    // (4 stayed: the next sweep at 8)
    for c in 5..=7 {
        o.made((c, c), 7, 2, sweep);
    }
    // (1 touched again — a send named it: no new row)
    o.made((1, 1), 7, 1, sweep);
    assert_eq!(reads.get(), 1);
    o.made((8, 8), 7, 2, sweep);
    assert_eq!(reads.get(), 2);
    let kept: Vec<bool> = (1..=8).map(|i| o.get(i).is_some()).collect();
    assert_eq!(kept, [true, true, false, false, true, true, true, true], "3 and 4: untouched, not there");
    assert_eq!(o.of(8), 1, "another session's rows aren't swept");
    // (session 7's first keeper ends: its rows go, the one that took its place keeps its own)
    o.ended(1);
    assert_eq!((o.of(7), o.of(8)), (4, 1));
    o.ended(2);
    assert_eq!((o.of(7), o.of(8)), (0, 1));
}

/// A sweep reads the kernel's list with the table let go — every session's calls wait on it, and a dump of every
/// socket can take a while —: another session's socket is made meanwhile, and so is one of the session's own,
/// which starts no second sweep.
#[test]
fn a_sweep_reads_the_kernels_list_with_the_table_let_go() {
    let o = Owners::with_cap(2);
    let reads = std::cell::Cell::new(0);
    o.made((1, 1), 7, 1, || None);
    o.made((2, 2), 7, 1, || {
        reads.set(reads.get() + 1);
        assert!(o.inner.try_lock().is_ok(), "the table held while the list is read");
        o.made((100, 100), 8, 2, || None);
        o.made((3, 3), 7, 1, || {
            reads.set(reads.get() + 1);
            None
        });
        Some([1, 2, 3].into())
    });
    assert_eq!(reads.get(), 1, "one sweep at a time");
    assert_eq!((o.of(7), o.of(8)), (3, 1));
}

/// An address as `/proc/net` prints it: each 32-bit word of it in this machine's order, `%08X`; the port `%04X`.
fn printed(a: SocketAddr) -> String {
    let octets = match a.ip() {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    let words: String =
        octets.chunks(4).map(|w| format!("{:08X}", u32::from_ne_bytes(w.try_into().unwrap()))).collect();
    format!("{words}:{:04X}", a.port())
}

/// A row as the kernel prints it (`udp4_format_sock`, `tcp4_seq_show` and their v6 twins), in state `st`.
fn row_line(local: SocketAddr, remote: SocketAddr, st: &str, inode: u64) -> String {
    format!(
        "  12: {} {} {st} 00000000:00000000 00:00000000 00000000  1000        0 {inode} 2 0000000000000000 0",
        printed(local),
        printed(remote)
    )
}

/// Rows as the kernel prints them read back as they were — the addresses, the port, the inode, whether a call may
/// land on it — for 2,000 random sockets of both families; and any table, mutated 20,000 times, is read without a
/// panic.
#[test]
fn a_tables_rows_read_back_as_the_kernel_printed_them() {
    use crate::mutate::{self, Rng};
    let mut rng = Rng(0x5eed_0add_7e55_0109);
    let any = |rng: &mut Rng, v6: bool| -> SocketAddr {
        let ip = match v6 {
            true => IpAddr::V6(Ipv6Addr::from(u128::from(rng.next()) << 64 | u128::from(rng.next()))),
            false => IpAddr::V4(Ipv4Addr::from(rng.next() as u32)),
        };
        SocketAddr::new(ip, rng.next() as u16)
    };
    for i in 0..2_000 {
        let (v6, tcp) = (i % 2 == 0, i % 3 == 0);
        let (local, inode) = (any(&mut rng, v6), rng.next() >> 1);
        let remote = if rng.below(2) == 0 { any(&mut rng, v6) } else { SocketAddr::new(local.ip(), 0) };
        let st = if tcp { ["0A", "01"][rng.below(2)] } else { "07" };
        let table = format!("  sl  local_address rem_address   st\n{}\n", row_line(local, remote, st, inode));
        let got = rows(&table, tcp);
        let open = match tcp {
            true => st == "0A",
            false => remote.ip().is_unspecified() && remote.port() == 0,
        };
        let peer = (remote.port() != 0).then_some(remote);
        assert_eq!(got, [Row { local, inode, cookie: None, open, v6only: false, peer }], "{table}");
    }
    let words = [":", " ", "\n", "0A", "FFFFFFFF", "+", "éééé"];
    for _ in 0..20_000 {
        let mut t = [UDP, TCP, TCP6][rng.below(3)].to_string();
        mutate::text(&mut rng, &mut t, &words);
        for r in rows(&t, rng.below(2) == 0) {
            assert!(t.contains(&r.inode.to_string()), "{r:?} of {t:?}");
        }
    }
}

/// The kernel's own table holds a UDP socket of the test's own as it's bound: its address, its inode, open to calls.
#[cfg(target_os = "linux")]
#[test]
fn the_kernels_table_reads_as_the_socket_is() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let ino = std::fs::metadata(format!("/proc/self/fd/{}", s.as_raw_fd())).unwrap().ino();
    let found = rows(&std::fs::read_to_string("/proc/net/udp").unwrap(), false);
    let ours: Vec<&Row> = found.iter().filter(|r| r.inode == ino).collect();
    assert!(
        matches!(&ours[..], [r] if r.local == s.local_addr().unwrap() && r.open && r.peer.is_none()),
        "{ours:?} for {:?}",
        s.local_addr()
    );
}
