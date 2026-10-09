//! tarae's own lookup of a name's SRV or TXT records (6e+, `target/sb/step6eplus-design.md`): where a run's question
//! for them is answered — a name its rules allow (F1), or one the user said yes to on the lookup card. Asked as a stub
//! asks, in tarae's own namespace: the absolute name, of `resolv.conf`'s nameservers in turn, over UDP, again over TCP
//! where the answer was cut short (`dns::query`, `dns::response`). No resolver library: glibc's `res_query` is
//! versioned and not on every libc, and getaddrinfo looks up only addresses.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::dns::{self, Found};

/// How tarae looks up a name's records of one type (`dns::TYPE_SRV`, `TYPE_TXT`): `system`, a stand-in in tests.
pub type Records = Arc<dyn Fn(&str, u16) -> Found + Send + Sync>;

/// A nameserver's answer waited for at most, a try (UDP, or TCP's connect and its answer). Two rounds of each
/// nameserver: within glibc's 5 s hold of the run's own question (M13) where the first answers.
const TRY: Duration = Duration::from_millis(1500);
/// Rounds over the nameservers.
const ROUNDS: usize = 2;
/// Nameservers asked at most (glibc's `MAXNS`).
const SERVERS_MAX: usize = 3;

/// The system's: `/etc/resolv.conf`'s nameservers, read at each lookup (a laptop's change between networks).
#[cfg_attr(test, allow(dead_code))] // (tests: `proxy::TEST_RECORDS` — never the network)
pub fn system() -> Records {
    Arc::new(|name: &str, qtype| {
        let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        look_up(&servers(&conf), name, qtype, TRY)
    })
}

/// `resolv.conf`'s nameservers (`nameserver <address>` lines, at most `SERVERS_MAX`), port 53 — none: this
/// machine's (resolv.conf(5)). A scoped IPv6 one (`fe80::1%eth0`) isn't taken.
pub fn servers(conf: &str) -> Vec<SocketAddr> {
    let mut found: Vec<SocketAddr> = conf
        .lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            (words.next() == Some("nameserver")).then(|| words.next()).flatten()
        })
        .filter_map(|a| a.parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .take(SERVERS_MAX)
        .collect();
    if found.is_empty() {
        found.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53));
    }
    found
}

/// `name`'s records of `qtype`, asked of `servers` in turn, `ROUNDS` times — each over UDP within `wait`, over TCP
/// where its answer was cut short. The first that answers (records, none, no such name) is what came; none did:
/// `Found::Failed`.
pub fn look_up(servers: &[SocketAddr], name: &str, qtype: u16, wait: Duration) -> Found {
    let Some(query) = dns::query(id(), name, qtype) else { return Found::No };
    for _ in 0..ROUNDS {
        for &server in servers {
            let found = match udp(server, &query, wait) {
                Some(Found::Truncated) => tcp(server, &query, wait),
                f => f,
            };
            match found {
                Some(Found::Failed | Found::Truncated) | None => continue,
                Some(f) => return f,
            }
        }
    }
    Found::Failed
}

/// A question's ID: random (a reply forged from off the path has to guess it).
fn id() -> u16 {
    let mut b = [0u8; 2];
    // SAFETY: a buffer of ours, its length.
    if unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) } != 0 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        return nanos.subsec_nanos() as u16;
    }
    u16::from_ne_bytes(b)
}

/// `query` to `server` over UDP: its answer within `wait` — a datagram that isn't (another ID, another question)
/// isn't, and the wait goes on. None: none came.
fn udp(server: SocketAddr, query: &[u8], wait: Duration) -> Option<Found> {
    let here: SocketAddr = match server {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let s = UdpSocket::bind(here).ok()?;
    // (connected: only the server's datagrams come)
    s.connect(server).ok()?;
    s.send(query).ok()?;
    let by = Instant::now() + wait;
    let mut buf = vec![0u8; 65_535];
    loop {
        let left = by.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return None;
        }
        let n = match s.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if let Ok(f) = dns::response(&buf[..n], query) {
            return Some(f);
        }
    }
}

/// `query` to `server` over TCP (its length first, RFC 1035 §4.2.2): its answer within `wait`. None: none came,
/// or not one to it.
fn tcp(server: SocketAddr, query: &[u8], wait: Duration) -> Option<Found> {
    let by = Instant::now() + wait;
    let mut s = TcpStream::connect_timeout(&server, wait).ok()?;
    let left = || Some(by.saturating_duration_since(Instant::now())).filter(|d| !d.is_zero());
    s.set_write_timeout(left()).ok()?;
    s.write_all(&dns::framed(query)).ok()?;
    s.set_read_timeout(left()).ok()?;
    let mut len = [0u8; 2];
    s.read_exact(&mut len).ok()?;
    let mut msg = vec![0u8; usize::from(u16::from_be_bytes(len))];
    s.set_read_timeout(left()).ok()?;
    s.read_exact(&mut msg).ok()?;
    dns::response(&msg, query).ok()
}

#[cfg(test)]
mod tests;
