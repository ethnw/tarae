//! Where the proxy lets a request go (`target/sb/step5-plan.md` §Egress rules): a run's rules — the hosts its
//! session may reach, a read-only snapshot each message carries (D23) — and the addresses a name resolves to,
//! each checked as `route` checks a run's own connect (this machine, the never-reached, this machine's own
//! addresses beyond loopback; an IPv4-mapped address as its IPv4 one), the connection made to the very address
//! checked: one lookup, never a second. Or, where tarae itself goes out through a proxy (step 5g, `upstream`),
//! through that one: the run's name goes to it unresolved — its addresses are the upstream's to look up, so the
//! address checks don't hold there, and the run's report says so.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use super::upstream::{self, Failed, Proto, Upstream, Way};
use crate::sandbox::route;

/// A host a rule names: one name, or every name under a domain (`*.crates.io` — not `crates.io` itself); at a
/// port, else at the request's own default (443 for a tunnel, 80 for plain http).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Name(String, Option<u16>),
    Under(String, Option<u16>),
}

/// As written (`parse` reads it back).
impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (star, name, port) = match self {
            Pattern::Name(n, p) => ("", n, p),
            Pattern::Under(d, p) => ("*.", d, p),
        };
        match port {
            Some(p) => write!(f, "{star}{name}:{p}"),
            None => write!(f, "{star}{name}"),
        }
    }
}

impl Pattern {
    /// A rule as written (`github.com`, `*.crates.io`, `host:8443`): a name as `parse::name` takes it, never an
    /// address, `*.` only at its start.
    pub fn parse(s: &str) -> Result<Pattern, &'static str> {
        let (h, port) = match s.rsplit_once(':') {
            Some((h, p)) => {
                let digits = !p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit());
                match p.parse::<u16>() {
                    Ok(n) if digits && n > 0 => (h, Some(n)),
                    _ => return Err("a port that isn't 1–65535"),
                }
            }
            None => (s, None),
        };
        let (under, h) = match h.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, h),
        };
        if h.parse::<IpAddr>().is_ok() {
            return Err("an address — a rule names hosts");
        }
        if h.contains('*') {
            return Err("a `*` but as `*.` at its start");
        }
        let name = super::parse::name(h)?;
        Ok(if under { Pattern::Under(name, port) } else { Pattern::Name(name, port) })
    }

    /// Whether it names `host` (as `parse::name` makes it) at `port`, `default` the request's own.
    pub(crate) fn matches(&self, host: &str, port: u16, default: u16) -> bool {
        let (named, at) = match self {
            Pattern::Name(n, at) => (host == n, at),
            Pattern::Under(d, at) => (
                host.len() > d.len()
                    && host.ends_with(d.as_str())
                    && host[..host.len() - d.len()].ends_with('.'),
                at,
            ),
        };
        named && port == at.unwrap_or(default)
    }
}

/// A run's egress rules, as its message carries them: read-only once sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Egress {
    /// Any host (`llm.sandbox-network` on, `llm.sandbox-hosts = "any"`): never asked; the address checks and the
    /// server name still hold.
    pub any: bool,
    /// The hosts reached without asking (`llm.sandbox-hosts`).
    pub hosts: Vec<Pattern>,
    /// The network closed (`llm.sandbox-network` off, piece C): no setting names a host — each is the user's yes
    /// on its card, and a refusal says so (an open network's empty list would send them to `llm.sandbox-hosts`).
    pub closed: bool,
    /// The user's answers when the run's policy was made — what the project keeps (`A`) and a grant for this run
    /// alone (`:sandbox-blocked`'s `y`): each host at its port. An answer the proxy was told since (`Answer`,
    /// `Taken`) goes before them — a host taken back is asked again, a daemon's older rules too.
    pub answered: Vec<(String, u16)>,
    /// This machine's own addresses beyond loopback, as the run's policy read them — None where they couldn't
    /// be read: every address is refused (as `route` refuses it).
    pub own_addresses: Option<Vec<IpAddr>>,
    /// The proxy tarae itself goes out by, as its environment named it when the run's policy was made (step 5g):
    /// a host not in its `NO_PROXY` goes through it — the address checks then are the upstream's, not tarae's.
    pub upstream: super::upstream::Upstreams,
    /// Test-only: loopback addresses taken for beyond this machine — the test's own listeners stand in for the
    /// outside (as `own_addresses: Some([])` stood in for step 4).
    #[cfg(test)]
    pub beyond: Vec<IpAddr>,
}

impl Egress {
    /// Whether the setting lets `host:port` go without asking (`default`: the request's own port).
    pub fn allows(&self, host: &str, port: u16, default: u16) -> bool {
        self.any || self.hosts.iter().any(|p| p.matches(host, port, default))
    }

    /// Whether the user's answers, as the run's policy had them, let `host:port` go — an answer for a registry's
    /// `*.domain` (the card's suggestion, piece C) every name under it.
    pub fn answered(&self, host: &str, port: u16) -> bool {
        self.answered.iter().any(|(h, p)| covers(h, host) && *p == port)
    }

    /// Why `ip` is never reached through the proxy — None: it may be. As `route`: never (link-local, the cloud
    /// metadata servers, multicast, broadcast, those carried in NAT64 or 6to4), this machine (loopback, none),
    /// this machine's own addresses beyond loopback — all when those couldn't be read.
    pub fn refuses(&self, ip: IpAddr) -> Option<&'static str> {
        let ip = ip.to_canonical();
        #[cfg(test)]
        let beyond = self.beyond.contains(&ip);
        #[cfg(not(test))]
        let beyond = false;
        if route::never(ip) {
            return Some("never reached: link-local, a cloud metadata server, multicast or broadcast");
        }
        if route::this_machine(ip) && !beyond {
            return Some("this machine");
        }
        match &self.own_addresses {
            None => Some("this machine's own addresses couldn't be read, so no address is reached"),
            Some(own) if own.contains(&ip) => Some("one of this machine's own addresses"),
            Some(_) => None,
        }
    }
}

/// Whether an answer for `answered` — a name, or `*.domain` for every name under it (not the domain itself, as
/// `Pattern::Under`) — covers `host`.
pub fn covers(answered: &str, host: &str) -> bool {
    match answered.strip_prefix("*.") {
        Some(d) => host.len() > d.len() && host.ends_with(d) && host[..host.len() - d.len()].ends_with('.'),
        None => answered == host,
    }
}

/// How the proxy looks a name up: the system's resolver in tarae, a stand-in in tests.
pub type Resolve = Arc<dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync>;

/// The system's resolver (`getaddrinfo`), on the connection's own thread.
pub fn system() -> Resolve {
    Arc::new(|host, port| Ok(std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?.collect()))
}

/// Why a request wasn't let out: the status it's answered with, and the words its report says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub status: u16,
    pub why: String,
}

impl Refused {
    pub fn new(status: u16, why: impl Into<String>) -> Refused {
        Refused { status, why: why.into() }
    }
}

/// The connection to `name:port`: `name` looked up once, every address checked (`Egress::refuses`), made to the
/// first checked address that answers within `timeout` — the address checked, never a name or a second lookup.
pub fn connect(
    rules: &Egress,
    resolve: &Resolve,
    name: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, Refused> {
    let found = resolve(name, port).map_err(|e| Refused::new(502, format!("its name isn't found ({e})")))?;
    if found.is_empty() {
        return Err(Refused::new(502, "its name has no address"));
    }
    let mut refused = Vec::new();
    let mut ok = Vec::new();
    for a in found {
        let ip = a.ip().to_canonical();
        match rules.refuses(ip) {
            Some(why) => refused.push(format!("{ip} ({why})")),
            None => ok.push(SocketAddr::new(ip, a.port())),
        }
    }
    if ok.is_empty() {
        let shown: Vec<&str> = refused.iter().take(3).map(String::as_str).collect();
        return Err(Refused::new(
            403,
            format!("it resolves only to addresses never reached: {}", shown.join(", ")),
        ));
    }
    let mut last = None;
    for a in ok {
        match dial(a, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    let e = last.map(|e| e.to_string()).unwrap_or_default();
    Err(Refused::new(502, format!("no address of it answered ({e})")))
}

/// Where a request was let out: the connection, and the upstream it goes through (None: direct). A tunnel through
/// an upstream is open to its host already (the upstream answered 2xx).
pub struct Reached {
    pub stream: TcpStream,
    pub through: Option<Upstream>,
}

/// Why a request wasn't let out: tarae's own refusal, or the upstream's failure (its variable, what failed).
#[derive(Debug)]
pub enum NotReached {
    Refused(Refused),
    Upstream(&'static str, Failed),
}

/// The way out for `name:port` (`tunnel`: a CONNECT's; else a plain request's) as the run's rules say it
/// (`Upstreams::way`): direct, `connect`'s — looked up, every address checked; or through the upstream proxy —
/// its own address not checked (it's the user's, named by tarae's environment, never by a run), the run's name
/// never looked up by tarae; a tunnel asked of it (`CONNECT name:port`, the credentials at once) and its answer
/// read within `timeout`. Its failures go to tarae's log with its host:port, never its credentials.
pub fn reach(
    rules: &Egress,
    resolve: &Resolve,
    name: &str,
    port: u16,
    tunnel: bool,
    timeout: Duration,
) -> Result<Reached, NotReached> {
    let up = match rules.upstream.way(name, port, tunnel) {
        Way::Direct => {
            let stream = connect(rules, resolve, name, port, timeout).map_err(NotReached::Refused)?;
            return Ok(Reached { stream, through: None });
        }
        Way::Unusable(u) => return Err(NotReached::Upstream(u.var, Failed::Unusable(u.why))),
        Way::Through(up) => up,
    };
    let failed = |f: Failed| NotReached::Upstream(up.var, upstream_said(up, name, port, f));
    let stream = dial_upstream(up, resolve, timeout).map_err(failed)?;
    // (an http one: a tunnel asked of it, a plain request goes as it is in absolute form; a SOCKS5 one: either
    // asked by name, then what the run sends goes on as it would to the host)
    match up.proto {
        Proto::Http if tunnel => tunnel_through(&stream, up, name, port, timeout).map_err(failed)?,
        Proto::Http => {}
        Proto::Socks5 => socks_through(&stream, up, name, port, timeout).map_err(failed)?,
    }
    Ok(Reached { stream, through: Some(up.clone()) })
}

/// The upstream's failure for `name:port`, said in tarae's log — where it is (host:port, which a report never
/// names), never its credentials.
pub fn upstream_said(up: &Upstream, name: &str, port: u16, f: Failed) -> Failed {
    let line = format!("the upstream proxy {} ({}) for {name}:{port}: {}", up.at(), up.var, f.said(up.var));
    crate::log::line("sandbox", "warning", &line);
    f
}

/// A connection to the upstream itself: its address as written, else its name looked up.
fn dial_upstream(up: &Upstream, resolve: &Resolve, timeout: Duration) -> Result<TcpStream, Failed> {
    let found = match up.host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, up.port)],
        Err(_) => resolve(&up.host, up.port).map_err(|_| Failed::NotFound)?,
    };
    let mut last = Failed::NotFound;
    for a in found {
        match dial(a, timeout) {
            Ok(s) => return Ok(s),
            // (the error's kind only: its text could carry what it was about)
            Err(e) => last = Failed::Unreachable(e.kind().to_string()),
        }
    }
    Err(last)
}

/// A tunnel asked of the upstream on `s`: its CONNECT sent, its answer read whole within `timeout` — 2xx with
/// nothing after it, else why not (a 407 as tarae's credentials meet it).
fn tunnel_through(
    s: &TcpStream,
    up: &Upstream,
    name: &str,
    port: u16,
    timeout: Duration,
) -> Result<(), Failed> {
    use std::io::{Read, Write};
    let silent = Failed::Silent(timeout);
    (&*s).write_all(&up.connect_head(name, port)).map_err(|e| Failed::Unreachable(e.kind().to_string()))?;
    let by = std::time::Instant::now() + timeout;
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
    let a = loop {
        match upstream::answer(&buf) {
            Ok(Some(a)) => break a,
            Ok(None) => {}
            Err(why) => return Err(Failed::Unreadable(why)),
        }
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return Err(silent);
        }
        match (&*s).read(&mut chunk) {
            Ok(0) => return Err(Failed::Cut),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(silent);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Failed::Cut),
        }
    };
    let _ = s.set_read_timeout(None);
    match a.status {
        200..=299 if buf.len() > a.len => Err(Failed::Extra),
        200..=299 => Ok(()),
        407 => Err(Failed::of_407(&a, up.has_credentials())),
        other => Err(Failed::Status(other)),
    }
}

/// A SOCKS5 connection to `name:port` asked of the upstream on `s` (RFC 1928, 1929): its greeting, its sign-in
/// where it asks for tarae's credentials, its CONNECT by name — every answer read whole within `timeout`, byte by
/// byte: nothing past the last is taken from what the host sends next.
fn socks_through(
    s: &TcpStream,
    up: &Upstream,
    name: &str,
    port: u16,
    timeout: Duration,
) -> Result<(), Failed> {
    use std::io::Write;
    let by = std::time::Instant::now() + timeout;
    let send = |b: &[u8]| (&*s).write_all(b).map_err(|e| Failed::Unreachable(e.kind().to_string()));
    let not = Failed::Unreadable("not a SOCKS5 reply");
    send(&up.socks_greeting())?;
    let way = read_exactly(s, 2, by, timeout)?;
    if way[0] != 5 {
        return Err(not);
    }
    match (way[1], up.socks_sign_in()) {
        (0, _) => {}
        (2, Some(sign_in)) => {
            send(&sign_in)?;
            let said = read_exactly(s, 2, by, timeout)?;
            if said[0] != 1 {
                return Err(Failed::Unreadable("not a SOCKS5 sign-in reply"));
            }
            if said[1] != 0 {
                return Err(Failed::SocksWrongCredentials);
            }
        }
        (0xff, None) => return Err(Failed::SocksNoCredentials),
        _ => return Err(Failed::SocksMethod),
    }
    send(&Upstream::socks_connect(name, port))?;
    let mut reply = Vec::new();
    loop {
        match upstream::socks_reply(&reply) {
            Ok(Some((0, _))) => break,
            Ok(Some((rep, _))) => return Err(Failed::SocksRefused(rep)),
            Ok(None) => reply.extend(read_exactly(s, 1, by, timeout)?),
            Err(why) => return Err(Failed::Unreadable(why)),
        }
    }
    let _ = s.set_read_timeout(None);
    Ok(())
}

/// `n` bytes from `s` by `by` — Err: its end came first (`Cut`), or the time (`Silent`, said as `timeout`).
fn read_exactly(
    s: &TcpStream,
    n: usize,
    by: std::time::Instant,
    timeout: Duration,
) -> Result<Vec<u8>, Failed> {
    use std::io::Read;
    let mut got = vec![0u8; n];
    let mut at = 0;
    while at < n {
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return Err(Failed::Silent(timeout));
        }
        match (&*s).read(&mut got[at..]) {
            Ok(0) => return Err(Failed::Cut),
            Ok(k) => at += k,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(Failed::Silent(timeout));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Failed::Cut),
        }
    }
    Ok(got)
}

/// The one place the proxy connects out. In tests only this machine is dialled — its loopback, or one of its own
/// addresses (a container's, standing for the outside in the Linux tests): whatever a broken check above lets
/// through shows as a failed connect, never a packet past this machine.
fn dial(a: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    // (`TARAE_MEASURE_REACHES_NETWORK`: an `#[ignore]`d measurement run by hand on a VM — never `cargo test`)
    #[cfg(test)]
    if !a.ip().is_loopback()
        && !this_machines(a.ip())
        && std::env::var_os("TARAE_MEASURE_REACHES_NETWORK").is_none()
    {
        return Err(io::Error::other("a test's connection past this machine, held back"));
    }
    TcpStream::connect_timeout(&a, timeout)
}

/// Tests: whether `ip` is one of this machine's own addresses (`getifaddrs`).
#[cfg(test)]
fn this_machines(ip: IpAddr) -> bool {
    addresses().is_ok_and(|a| a.contains(&ip))
}

/// The addresses of this machine's network (Linux: tarae's own namespace's), every interface's (`getifaddrs`).
pub fn addresses() -> io::Result<Vec<IpAddr>> {
    let mut addrs = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: the list getifaddrs makes, walked while it lives, read only, freed once after.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut at = list;
        while !at.is_null() {
            let a = (*at).ifa_addr;
            if !a.is_null() {
                match i32::from((*a).sa_family) {
                    libc::AF_INET => addrs
                        .push(IpAddr::from((*a.cast::<libc::sockaddr_in>()).sin_addr.s_addr.to_ne_bytes())),
                    libc::AF_INET6 => {
                        addrs.push(IpAddr::from((*a.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr))
                    }
                    _ => {}
                }
            }
            at = (*at).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    Ok(addrs)
}
