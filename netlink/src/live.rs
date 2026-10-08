//! The crate against a real kernel, in a throwaway user + network namespace: each scene runs in a process of its
//! own (this test binary again, on the ignored `scene` test), made with the namespaces before it runs anything —
//! the test process itself never changes namespace. In it the scene is root of a netns holding only `lo`; nothing it
//! does reaches past it. Where user namespaces can't be made (a container's filter; Ubuntu's restriction, which
//! refuses the scene's own id maps — 24.04, 2026-10-08), or are made with no capability in them (the scene's first
//! step that needs one refused: what the restriction leaves a namespace whose maps were written from outside), the
//! scenes skip, saying why as `sandbox::tier` would — `TARAE_EXPECT_TIER=A+` makes that a failure (`cargo xtask linux
//! --userns`).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::nft::{self, ChainKind, Expr, Hook, Transaction, Verdict};
use crate::tests::{session_routes, session_ruleset};
use crate::{Error, Family, Protocol, Socket, route};

const SCENE: &str = "TARAE_NETLINK_SCENE";
/// Set on a scene's process by `a_capless_namespace_skips_its_scene`: the scene drops its capabilities before its
/// first step — a namespace with none in it, as Ubuntu's restriction leaves one.
const CAPLESS: &str = "TARAE_NETLINK_CAPLESS";
/// A scene's words saying it has nothing to do here, its why after them.
const SKIPPED: &str = "scene skipped: ";
/// Where a namespace is made — the scene's process forked, before its exec.
const NAMESPACE: &str = "the user + network namespace or its id maps";

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// Runs scene `name` in a process of its own, in a new user + network namespace — skipped, said, where no such
/// namespace can be made here or it has no capability. Panics with the scene's output if it failed.
fn in_scene(name: &str) {
    settle(name, &start_scene(name, false));
}

/// `judge`'s word on `scene` with this run's `TARAE_EXPECT_TIER`: a skip said, a failure a panic.
fn settle(name: &str, scene: &Scene) {
    let a_plus = std::env::var("TARAE_EXPECT_TIER").is_ok_and(|t| t == "A+");
    match judge(name, scene, a_plus) {
        Ok(Some(why)) => eprintln!("skipped {name}: {why}"),
        Ok(None) => {}
        Err(failed) => panic!("{failed}"),
    }
}

/// How a scene's process went.
#[derive(Debug)]
enum Scene {
    /// Nothing to do here, and why.
    Skipped { at: Skip, why: String },
    /// It ran, with its output.
    Ended { ok: bool, text: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Skip {
    /// No namespace could be made (`NAMESPACE`).
    Namespace,
    /// One was made with no capability in it: the scene's first step that needs one was refused.
    Capless,
}

/// What a scene's run says of its test: passed (None), skipped and why — or failed, said. `a_plus`
/// (`TARAE_EXPECT_TIER=A+`): this system makes capable namespaces, so a skip is a failure.
fn judge(name: &str, scene: &Scene, a_plus: bool) -> Result<Option<String>, String> {
    match scene {
        Scene::Skipped { why, .. } if a_plus => Err(format!("{name}: {why}, but TARAE_EXPECT_TIER=A+")),
        Scene::Skipped { why, .. } => Ok(Some(why.clone())),
        Scene::Ended { ok: true, text } if text.contains("scene done") => Ok(None),
        Scene::Ended { text, .. } => Err(format!("scene {name} failed:\n{text}")),
    }
}

/// Ubuntu's restriction (23.10 on, and CI's ubuntu-latest), in `sandbox::tier`'s words.
const UBUNTU: &str =
    "Ubuntu's restriction on unprivileged user namespaces (kernel.apparmor_restrict_unprivileged_userns = 1)";

/// Why this system gives a scene no namespace to work in — refused with `errno` at `at` — read from the files
/// `sandbox::tier::why` reads, in its order.
fn why_not(at: &str, errno: i32) -> String {
    let num = |p: &str| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<i64>().ok());
    let refused = matches!(errno, libc::EPERM | libc::EACCES);
    let seccomp = std::fs::read_to_string("/proc/self/status")
        .is_ok_and(|s| s.lines().any(|l| l.strip_prefix("Seccomp:").is_some_and(|v| v.trim() == "2")));
    let said = std::io::Error::from_raw_os_error(errno);
    if at == NAMESPACE && (errno == libc::ENOSPC || num("/proc/sys/user/max_user_namespaces") == Some(0)) {
        "user namespaces are turned off here (user.max_user_namespaces = 0)".into()
    } else if at == NAMESPACE && num("/proc/sys/kernel/unprivileged_userns_clone") == Some(0) {
        "unprivileged user namespaces are turned off here (kernel.unprivileged_userns_clone = 0)".into()
    } else if refused && num("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") == Some(1) {
        format!("{UBUNTU} (refused at {at}: {said})")
    } else if at == NAMESPACE && refused && seccomp {
        format!("a seccomp filter here refuses new namespaces — Docker's default profile does ({said})")
    } else {
        format!("{at}: {said}")
    }
}

/// Scene `name` in a process of its own, in a new user + network namespace. `capless`: the scene drops its
/// capabilities first (`CAPLESS`).
fn start_scene(name: &str, capless: bool) -> Scene {
    // SAFETY: plain calls.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = format!("0 {uid} 1");
    let gid_map = format!("0 {gid} 1");
    let mut cmd = Command::new(std::env::current_exe().expect("the test binary"));
    cmd.args(["live::scene", "--exact", "--ignored", "--nocapture", "--test-threads=1"]).env(SCENE, name);
    if capless {
        cmd.env(CAPLESS, "1");
    }
    // SAFETY: in the forked child before its exec: raw calls on buffers made before the fork, nothing allocated.
    unsafe {
        cmd.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            write_file(c"/proc/self/setgroups", b"deny")?;
            write_file(c"/proc/self/uid_map", uid_map.as_bytes())?;
            write_file(c"/proc/self/gid_map", gid_map.as_bytes())?;
            Ok(())
        });
    }
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let why = why_not(NAMESPACE, e.raw_os_error().unwrap_or(libc::EIO));
            return Scene::Skipped { at: Skip::Namespace, why };
        }
    };
    // (a scene that hangs fails: killed at its limit, its output said)
    let pid = child.id();
    let (done, waited) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let out = match waited.recv_timeout(Duration::from_secs(30)) {
        Ok(out) => out.expect("the scene's output"),
        Err(_) => {
            // SAFETY: a signal to the scene's process, still ours to wait for (the thread holds it unreaped).
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let out = waited.recv().expect("the scene's output").expect("the scene's output");
            panic!("scene {name}: no end within 30 s\n{}", String::from_utf8_lossy(&out.stderr));
        }
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    // (anywhere in its line: the test runner says `test live::scene ... ` before the scene's own output)
    match text.lines().find_map(|l| l.split_once(SKIPPED)).map(|(_, why)| why) {
        Some(why) => Scene::Skipped { at: Skip::Capless, why: why.to_string() },
        None => Scene::Ended { ok: out.status.success(), text },
    }
}

/// A namespace made with no capability in it — what Ubuntu's restriction leaves one whose id maps were written from
/// outside (24.04: the keeper's `lo up` refused, t5) — skips its scene, said, at the scene's first step that needs
/// one; under `TARAE_EXPECT_TIER=A+` that skip is a failure. The scene drops its own capabilities to stand in for the
/// restriction (`CAPLESS`): on 24.04 itself the scenes' own maps are refused first, a skip at `NAMESPACE`.
#[test]
fn a_capless_namespace_skips_its_scene() {
    let scene = start_scene("lands", true);
    match &scene {
        Scene::Skipped { at: Skip::Capless, why } => {
            assert!(why.contains("lo up"), "said where: {why}");
            assert_eq!(judge("lands", &scene, false), Ok(Some(why.clone())), "a skip");
            assert!(judge("lands", &scene, true).is_err(), "a skip under TARAE_EXPECT_TIER=A+ fails");
        }
        // (no namespace here at all: nothing to show — skipped as the other scenes are)
        Scene::Skipped { at: Skip::Namespace, .. } => settle("lands", &scene),
        Scene::Ended { text, .. } => panic!("a capless namespace's scene wasn't skipped:\n{text}"),
    }
}

/// A whole write to `path`, with raw calls only (a forked child's).
fn write_file(path: &std::ffi::CStr, bytes: &[u8]) -> std::io::Result<()> {
    // SAFETY: a NUL-terminated path and a buffer of ours; the descriptor is closed before returning.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        let e = std::io::Error::last_os_error();
        libc::close(fd);
        if n != bytes.len() as isize {
            return Err(e);
        }
    }
    Ok(())
}

/// The scenes' runner, in the scene's own process: does nothing unless the parent named a scene.
#[test]
#[ignore]
fn scene() {
    let Ok(name) = std::env::var(SCENE) else { return };
    if std::env::var_os(CAPLESS).is_some() {
        drop_capabilities();
    }
    if let Err(why) = lo_up() {
        println!("{SKIPPED}{why}");
        return;
    }
    match name.as_str() {
        "lands" => lands(),
        "refused" => refused(),
        "owner" => owner(),
        "goes" => goes(),
        "reaches" => reaches(),
        "routes" => routes(),
        "foreign" => foreign(),
        "capless" => capless(),
        "partial" => partial(),
        "netns" => netns(),
        "adopts" => adopts(),
        other => panic!("no scene {other}"),
    }
    println!("scene done");
}

// ── the scenes' own setup: lo up with the session's addresses (the keeper's ioctls) ─────────────

#[repr(C)]
struct In6Ifreq {
    addr: libc::in6_addr,
    prefixlen: u32,
    ifindex: i32,
}

fn ifreq(name: &str) -> libc::ifreq {
    // SAFETY: all zeros is a valid ifreq.
    let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, s) in r.ifr_name.iter_mut().zip(name.bytes()) {
        *d = s as libc::c_char;
    }
    r
}

fn check(r: i32, what: &str) {
    assert!(r == 0, "{what}: {}", std::io::Error::last_os_error());
}

/// `lo` up, `192.0.2.1/32` on it as `lo:1` and `2001:db8::1/128` — what a session's keeper does at its birth. `lo
/// up` is the scene's first step that needs a capability: refused (EPERM), the namespace has none — why, for a skip.
fn lo_up() -> Result<(), String> {
    // SAFETY: ioctls on sockets of ours with requests of the kernel's layout.
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        let mut r = ifreq("lo");
        check(libc::ioctl(s, libc::SIOCGIFFLAGS, &mut r), "lo's flags");
        r.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        if libc::ioctl(s, libc::SIOCSIFFLAGS, &r) != 0 {
            let e = std::io::Error::last_os_error();
            assert_eq!(e.raw_os_error(), Some(libc::EPERM), "lo up: {e}");
            libc::close(s);
            return Err(why_not("lo up", libc::EPERM));
        }
        let mut a = ifreq("lo:1");
        let sin = &mut *(&raw mut a.ifr_ifru.ifru_addr).cast::<libc::sockaddr_in>();
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_addr.s_addr = u32::from(Ipv4Addr::new(192, 0, 2, 1)).to_be();
        check(libc::ioctl(s, libc::SIOCSIFADDR, &a), "192.0.2.1 on lo");
        let mask = &mut *(&raw mut a.ifr_ifru.ifru_addr).cast::<libc::sockaddr_in>();
        mask.sin_addr.s_addr = u32::MAX;
        check(libc::ioctl(s, libc::SIOCSIFNETMASK, &a), "its /32");
        libc::close(s);
        let s6 = libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        let r6 = In6Ifreq {
            addr: libc::in6_addr { s6_addr: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets() },
            prefixlen: 128,
            ifindex: 1,
        };
        check(libc::ioctl(s6, libc::SIOCSIFADDR, &r6), "2001:db8::1 on lo");
        libc::close(s6);
    }
    Ok(())
}

/// A netlink socket of `p`, as a keeper would hand one over.
fn socket(p: Protocol) -> Socket {
    // SAFETY: plain call; the descriptor is ours.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, p.number()) };
    assert!(fd >= 0, "a netlink socket: {}", std::io::Error::last_os_error());
    // SAFETY: just made.
    Socket::from_fd(unsafe { OwnedFd::from_raw_fd(fd) }, p).expect("a netlink socket")
}

fn install(nf: &mut Socket) {
    let mut rt = socket(Protocol::Route);
    for f in [Family::V4, Family::V6] {
        rt.apply(&session_routes(f), soon()).expect("the routes");
    }
    nf.apply(&session_ruleset("tarae"), soon()).expect("the ruleset");
}

// ── the scenes ──────────────────────────────────────────────────────────

/// The whole ruleset lands as one change: every message acked, the generation moved once, each chain's rules there.
fn lands() {
    let mut nf = socket(Protocol::Netfilter);
    let before = nft::generation(&mut nf, soon()).unwrap();
    install(&mut nf);
    assert_eq!(
        nft::generation(&mut nf, soon()).unwrap(),
        before.wrapping_add(1),
        "one change, one generation"
    );
    let rules = nft::rules(&mut nf, soon()).unwrap();
    let count = |chain: &str| rules.iter().filter(|r| r.table == "tarae" && r.chain == chain).count();
    assert_eq!((count("out"), count("pre")), (7, 13));
}

#[test]
fn the_ruleset_lands_and_moves_the_generation_once() {
    in_scene("lands");
}

/// A batch whose last change can't land changes nothing: the refusal names that message, and the generation and
/// the tables are as before.
fn refused() {
    let mut nf = socket(Protocol::Netfilter);
    let before = nft::generation(&mut nf, soon()).unwrap();
    let mut t = Transaction::new();
    t.table("tarae", true);
    t.chain("tarae", "pre", Hook::Prerouting, -150, ChainKind::Filter);
    t.rule("tarae", "nosuch", &[Expr::Verdict(Verdict::Accept)]);
    match nf.apply(&t.end(), soon()) {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::ENOENT, Some(3))),
        other => panic!("{other:?}"),
    }
    assert_eq!(nft::generation(&mut nf, soon()).unwrap(), before, "nothing committed");
    assert!(nft::tables(&mut nf, soon()).unwrap().is_empty(), "no table left");
}

#[test]
fn a_refused_batch_changes_nothing() {
    in_scene("refused");
}

/// An owned table is its socket's alone: another socket — of the same process, with every capability in the netns —
/// can't add to it.
fn owner() {
    let mut a = socket(Protocol::Netfilter);
    let mut t = Transaction::new();
    t.table("tarae", true);
    a.apply(&t.end(), soon()).unwrap();
    let mut b = socket(Protocol::Netfilter);
    let mut other = Transaction::new();
    other.chain("tarae", "pre", Hook::Prerouting, -150, ChainKind::Filter);
    match b.apply(&other.end(), soon()) {
        Err(Error::Kernel { errno, .. }) => assert_eq!(errno, libc::EPERM),
        other => panic!("another socket changed an owned table: {other:?}"),
    }
}

#[test]
fn only_its_owner_changes_an_owned_table() {
    in_scene("owner");
}

/// An owned table goes when its socket closes.
fn goes() {
    let mut a = socket(Protocol::Netfilter);
    let mut t = Transaction::new();
    t.table("tarae", true);
    a.apply(&t.end(), soon()).unwrap();
    let mut b = socket(Protocol::Netfilter);
    assert_eq!(nft::tables(&mut b, soon()).unwrap(), ["tarae"]);
    drop(a);
    let by = Instant::now() + Duration::from_secs(1);
    while !nft::tables(&mut b, soon()).unwrap().is_empty() {
        assert!(Instant::now() < by, "the owned table outlived its socket");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_owned_table_goes_with_its_socket() {
    in_scene("goes");
}

/// t1's check, with the crate's own bytes: a client with no capability connects to an address beyond the session
/// (`203.0.113.7:443`, `[2001:db8::7]:443`) and lands on the transparent listener, whose accepted socket's own
/// address is the one asked. Every connect, accept, read and write has a time limit: a path that doesn't hold fails
/// the scene, never hangs it.
fn reaches() {
    let limit = Duration::from_secs(3);
    let mut nf = socket(Protocol::Netfilter);
    install(&mut nf);
    for (planted, asked) in [
        (SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 15001), "203.0.113.7:443"),
        (
            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)), 15001),
            "[2001:db8::7]:443",
        ),
    ] {
        let asked: SocketAddr = asked.parse().unwrap();
        let listener = transparent_listener(planted);
        let client = std::thread::spawn(move || {
            drop_capabilities();
            let mut s = TcpStream::connect_timeout(&asked, limit).expect("the connect");
            s.set_read_timeout(Some(limit)).unwrap();
            s.set_write_timeout(Some(limit)).unwrap();
            s.write_all(b"x").unwrap();
            let mut b = [0u8; 1];
            s.read_exact(&mut b).unwrap();
        });
        let by = Instant::now() + limit;
        listener.set_nonblocking(true).unwrap();
        let mut got = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < by => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => panic!("nothing reached the planted socket at {planted} for {asked}: {e}"),
            }
        };
        assert_eq!(got.local_addr().unwrap(), asked, "the original destination, read where it landed");
        got.set_nonblocking(false).unwrap();
        got.set_read_timeout(Some(limit)).unwrap();
        got.set_write_timeout(Some(limit)).unwrap();
        let mut b = [0u8; 1];
        got.read_exact(&mut b).unwrap();
        got.write_all(b"y").unwrap();
        client.join().expect("the client");
    }
}

#[test]
fn a_capless_client_reaches_the_planted_socket() {
    in_scene("reaches");
}

/// A listener at `at`, transparent (`IP_TRANSPARENT` / `IPV6_TRANSPARENT`): what tproxy hands connections to.
fn transparent_listener(at: SocketAddr) -> TcpListener {
    let (domain, level, opt) = match at {
        SocketAddr::V4(_) => (libc::AF_INET, libc::SOL_IP, libc::IP_TRANSPARENT),
        SocketAddr::V6(_) => (libc::AF_INET6, libc::SOL_IPV6, 75),
    };
    // SAFETY: a socket of ours, options and an address of the kernel's layout, live for each call.
    unsafe {
        let fd = libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        assert!(fd >= 0);
        let one: i32 = 1;
        check(libc::setsockopt(fd, level, opt, (&raw const one).cast(), 4), "transparent");
        let (sa, len) = raw(at);
        check(libc::bind(fd, (&raw const sa).cast(), len), "the planted bind");
        check(libc::listen(fd, 8), "listen");
        TcpListener::from(OwnedFd::from_raw_fd(fd))
    }
}

fn raw(a: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: all zeros is a valid sockaddr_storage; each arm writes its family's struct, which fits in it.
    unsafe {
        let mut ss: libc::sockaddr_storage = std::mem::zeroed();
        match a {
            SocketAddr::V4(v4) => {
                let s = &mut *(&raw mut ss).cast::<libc::sockaddr_in>();
                s.sin_family = libc::AF_INET as libc::sa_family_t;
                s.sin_port = v4.port().to_be();
                s.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
                (ss, size_of::<libc::sockaddr_in>() as libc::socklen_t)
            }
            SocketAddr::V6(v6) => {
                let s = &mut *(&raw mut ss).cast::<libc::sockaddr_in6>();
                s.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                s.sin6_port = v6.port().to_be();
                s.sin6_addr.s6_addr = v6.ip().octets();
                (ss, size_of::<libc::sockaddr_in6>() as libc::socklen_t)
            }
        }
    }
}

/// This thread's capabilities, every one dropped (capabilities are a thread's own).
fn drop_capabilities() {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let h = Header { version: 0x2008_0522, pid: 0 };
    let d = [Data { effective: 0, permitted: 0, inheritable: 0 }; 2];
    // SAFETY: capset's header and its two data words, ours for the call; it changes this thread only.
    let r = unsafe { libc::syscall(libc::SYS_capset, &h, d.as_ptr()) };
    assert_eq!(r, 0, "capset: {}", std::io::Error::last_os_error());
}

/// The routes and the policy rule read back as sent.
fn routes() {
    let mut rt = socket(Protocol::Route);
    for f in [Family::V4, Family::V6] {
        rt.apply(&session_routes(f), soon()).expect("the routes");
        let routes = route::routes(&mut rt, f, soon()).unwrap();
        let is = |table: u32, kind: u8| {
            routes.iter().any(|r| r.table == table && r.kind == kind && r.oif == Some(1) && r.dst_len == 0)
        };
        assert!(is(route::MAIN.into(), route::UNICAST), "{f:?} default: {routes:#?}");
        assert!(is(100, route::LOCAL), "{f:?} local default in 100: {routes:#?}");
        let rules = route::rules(&mut rt, f, soon()).unwrap();
        assert!(rules.iter().any(|r| r.table == 100 && r.fwmark == Some(1)), "{f:?}: {rules:#?}");
    }
}

#[test]
fn the_routes_read_back() {
    in_scene("routes");
}

/// A datagram that isn't the kernel's is never taken for its answer: another socket's forged refusal of the next
/// change, queued first, is dropped, and the change lands on the kernel's own acks.
fn foreign() {
    // (the receiving socket bound first, so its port can be named)
    // SAFETY: plain calls on a socket of ours and an address of the kernel's layout.
    let (fd, port) = unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE);
        let mut a: libc::sockaddr_nl = std::mem::zeroed();
        a.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        check(libc::bind(fd, (&raw const a).cast(), len), "bind");
        let mut l = len;
        check(libc::getsockname(fd, (&raw mut a).cast(), &mut l), "its port");
        (OwnedFd::from_raw_fd(fd), a.nl_pid)
    };
    let mut rt = Socket::from_fd(fd, Protocol::Route).unwrap();
    // A refusal (EPERM) of message 1 — the socket's first number — sent from another socket to its port
    let mut forged = Vec::new();
    forged.extend(36u32.to_ne_bytes());
    forged.extend(2u16.to_ne_bytes());
    forged.extend(0x100u16.to_ne_bytes());
    forged.extend(1u32.to_ne_bytes());
    forged.extend(0u32.to_ne_bytes());
    forged.extend((-libc::EPERM).to_ne_bytes());
    forged.extend([16, 0, 0, 0, 24, 0, 5, 6, 1, 0, 0, 0, 0, 0, 0, 0]);
    let other = socket(Protocol::Route);
    // SAFETY: all zeros is a valid sockaddr_nl; a send of our buffer on our socket.
    unsafe {
        let mut to: libc::sockaddr_nl = std::mem::zeroed();
        to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        to.nl_pid = port;
        let n = libc::sendto(
            other.as_fd().as_raw_fd(),
            forged.as_ptr().cast(),
            forged.len(),
            0,
            (&raw const to).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        assert_eq!(n, forged.len() as isize, "the forged reply: {}", std::io::Error::last_os_error());
    }
    rt.apply(&session_routes(Family::V4), soon()).expect("the kernel's acks, not the forged refusal");
    assert_eq!(rt.foreign(), 1, "the forged reply dropped");
}

#[test]
fn only_the_kernel_is_heard() {
    in_scene("foreign");
}

/// The kernel judges the sender, not the opener: a thread without capabilities, sending on a socket a capable one
/// opened, gets one EPERM for `BATCH_BEGIN` — and `apply` says so at once rather than waiting out its deadline for
/// acks that never come. Nothing is committed.
fn capless() {
    let mut nf = socket(Protocol::Netfilter);
    let before = nft::generation(&mut nf, soon()).unwrap();
    let batch = session_ruleset("tarae");
    let got = std::thread::scope(|s| {
        s.spawn(|| {
            drop_capabilities();
            nf.apply(&batch, soon())
        })
        .join()
        .expect("the capless sender")
    });
    match got {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::EPERM, Some(0))),
        other => panic!("a capless sender's batch: {other:?}"),
    }
    assert_eq!(nft::generation(&mut nf, soon()).unwrap(), before, "nothing committed");
    assert!(nft::tables(&mut nf, soon()).unwrap().is_empty(), "no table");
}

#[test]
fn a_capless_sender_is_refused_at_once() {
    in_scene("capless");
}

/// The session's table and chains adopt nothing: with a table of the name there — owned by this very socket, so
/// nothing but `EXCL` refuses it — the ruleset is refused at the table (EEXIST, message 1), and a chain already there
/// is refused too; neither batch commits.
fn adopts() {
    let mut nf = socket(Protocol::Netfilter);
    let mut there = Transaction::new();
    there.table("tarae", true);
    there.chain("tarae", "out", Hook::Output, -150, ChainKind::Route);
    nf.apply(&there.end(), soon()).expect("a table and a chain there first");
    let before = nft::generation(&mut nf, soon()).unwrap();
    match nf.apply(&session_ruleset("tarae"), soon()) {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::EEXIST, Some(1))),
        other => panic!("the table there was taken as it is: {other:?}"),
    }
    let mut chain = Transaction::new();
    chain.chain("tarae", "out", Hook::Output, -150, ChainKind::Route);
    match nf.apply(&chain.end(), soon()) {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::EEXIST, Some(1))),
        other => panic!("the chain there was updated: {other:?}"),
    }
    assert_eq!(nft::generation(&mut nf, soon()).unwrap(), before, "nothing committed");
}

#[test]
fn the_table_and_chains_adopt_nothing() {
    in_scene("adopts");
}

/// Route changes are partial where a transaction isn't: with the main table's default already there, the session's
/// routes are refused at that one (EEXIST), and the policy rule and table 100's route after it land all the same.
fn partial() {
    let mut rt = socket(Protocol::Route);
    let mut first = route::Changes::new();
    first.default_route(Family::V4, route::MAIN, route::RouteKind::Unicast, 1);
    rt.apply(&first.end(), soon()).expect("the default route alone");
    match rt.apply(&session_routes(Family::V4), soon()) {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::EEXIST, Some(0))),
        other => panic!("{other:?}"),
    }
    let routes = route::routes(&mut rt, Family::V4, soon()).unwrap();
    assert!(
        routes.iter().any(|r| r.table == 100 && r.kind == route::LOCAL),
        "the local default landed: {routes:#?}"
    );
    let rules = route::rules(&mut rt, Family::V4, soon()).unwrap();
    assert!(
        rules.iter().any(|r| r.table == 100 && r.fwmark == Some(1)),
        "the policy rule landed: {rules:#?}"
    );
}

#[test]
fn a_refused_route_change_is_partial() {
    in_scene("partial");
}

/// `netns` names the namespace the socket is in — the one `/proc/self/ns/net` names — to a caller with
/// CAP_NET_ADMIN over it, and refuses one without.
fn netns() {
    use std::os::unix::fs::MetadataExt;
    let nf = socket(Protocol::Netfilter);
    let here = std::fs::metadata("/proc/self/ns/net").expect("this process's netns");
    assert_eq!(nf.netns().expect("the socket's netns"), (here.dev(), here.ino()));
    let capless = std::thread::scope(|s| {
        s.spawn(|| {
            drop_capabilities();
            nf.netns()
        })
        .join()
        .expect("the capless caller")
    });
    assert_eq!(capless.map_err(|e| e.raw_os_error()), Err(Some(libc::EPERM)));
}

#[test]
fn netns_is_told_only_to_the_namespaces_admin() {
    in_scene("netns");
}

/// A socket of another protocol than the one asked isn't taken.
#[test]
fn a_socket_of_another_protocol_is_refused() {
    // SAFETY: plain call; the descriptor is ours.
    let fd =
        unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
    assert!(fd >= 0);
    // SAFETY: just made.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    assert!(Socket::from_fd(fd, Protocol::Netfilter).is_err());
}
