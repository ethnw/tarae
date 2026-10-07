//! Where the proxy lets a request go (`target/sb/step5-plan.md` §Egress rules): a run's rules — the hosts its
//! session may reach, a read-only snapshot each message carries (D23) — and the addresses a name resolves to,
//! each checked as `route` checks a run's own connect (this machine, the never-reached, this machine's own
//! addresses beyond loopback; an IPv4-mapped address as its IPv4 one), the connection made to the very address
//! checked: one lookup, never a second.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::sandbox::route;

/// A host a rule names: one name, or every name under a domain (`*.crates.io` — not `crates.io` itself); at a
/// port, else at the request's own default (443 for a tunnel, 80 for plain http).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))] // (the setting's list of hosts comes with step 5e)
pub enum Pattern {
    Name(String, Option<u16>),
    Under(String, Option<u16>),
}

#[cfg_attr(not(test), allow(dead_code))] // (as `Pattern`)
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
    fn matches(&self, host: &str, port: u16, default: u16) -> bool {
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
#[derive(Clone, Debug, Default)]
pub struct Egress {
    /// Any host (`llm.sandbox-network = true`): never asked; the address checks and the server name still hold.
    pub any: bool,
    /// The hosts reached without asking.
    pub hosts: Vec<Pattern>,
    /// This machine's own addresses beyond loopback, as the run's policy read them — None where they couldn't
    /// be read: every address is refused (as `route` refuses it).
    pub own_addresses: Option<Vec<IpAddr>>,
    /// Test-only: loopback addresses taken for beyond this machine — the test's own listeners stand in for the
    /// outside (as `own_addresses: Some([])` stood in for step 4).
    #[cfg(test)]
    pub beyond: Vec<IpAddr>,
}

impl Egress {
    /// Whether a rule lets `host:port` go without asking (`default`: the request's own port).
    pub fn allows(&self, host: &str, port: u16, default: u16) -> bool {
        self.any || self.hosts.iter().any(|p| p.matches(host, port, default))
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
