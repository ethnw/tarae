//! What a session answers its runs' names with (step 6e, `target/sb/step6e-design.md`): every question a run asks
//! (to any address's port 53, UDP or TCP) is the session's to answer, and none leaves the machine. A name gets a
//! synthetic address from the session's map (`names.rs`), and a connection to it names its host before any byte.
//!
//! - E1 (b): a name the run's rules name (or the user said yes to) is looked up for real by tarae first — the
//!   absolute name, in tarae's own namespace: the lookup its connection makes anyway — and answered NXDOMAIN or NODATA
//!   where it doesn't exist; any other name gets its address unlooked-up (the card asks at connect). Through an
//!   upstream proxy tarae resolves nothing: every name synthetic.
//! - E6 (b): a name that is two or more labels and then one of the runs' search domains, not named by the rules, is
//!   NXDOMAIN without a lookup — the search list's candidate for a name as written, which the client then asks for.
//! - Q3: A/AAAA from the map (AAAA only where the session carries IPv6, else NODATA); PTR of a pool address from the
//!   map, any other NXDOMAIN; HTTPS/SVCB an empty NOERROR; the rest REFUSED, said with the name. A metadata server's
//!   own name NXDOMAIN, said. `localhost` and below it: loopback.
//!
//! Pure here: `plan` decides, `after` finishes what needed a lookup; the proxy holds the user's answers, looks up,
//! and sends.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::dns::{self, Answer, Question};
use super::parse;
use crate::sandbox::names::Names;
use crate::sandbox::route;

/// How long a name's real lookup (E1) is taken as it came — a burst of questions for one name (A and AAAA, a
/// client's retries) is one lookup.
const LOOKED_FOR: Duration = Duration::from_secs(60);
/// The most names whose lookups are kept (past it they're all forgotten: looked up again).
const LOOKED_MAX: usize = 4096;
/// The TTL of an answer: the map lives as long as the session, so an answer never goes stale within it.
pub const TTL: u32 = 300;

/// A session's answering: its map, its runs' search list, whether it carries IPv6, and E1's lookups so far.
pub struct Session {
    pub names: Names,
    /// The search list its runs' resolver uses (`resolv.conf`'s, `route::search`).
    pub search: Vec<String>,
    /// Its `lo` carries IPv6: AAAA answered, else NODATA.
    pub v6: bool,
    looked: Mutex<HashMap<String, (Instant, Exists)>>,
}

impl Session {
    pub fn new(search: Vec<String>, v6: bool) -> Session {
        Session { names: Names::new(), search, v6, looked: Default::default() }
    }

    /// What `name`'s real lookup came to, within `LOOKED_FOR`.
    pub fn looked(&self, name: &str) -> Option<Exists> {
        let l = self.looked.lock().unwrap_or_else(|e| e.into_inner());
        l.get(name).filter(|(at, _)| at.elapsed() < LOOKED_FOR).map(|(_, e)| *e)
    }

    pub fn remember(&self, name: &str, e: Exists) {
        let mut l = self.looked.lock().unwrap_or_else(|e| e.into_inner());
        if l.len() >= LOOKED_MAX {
            l.clear();
        }
        l.insert(name.to_string(), (Instant::now(), e));
    }
}

/// Whose a question is — by the mark it came with (E2): a run's (its rules, where its refusals are said), or, on a
/// kernel that reads no datagram's mark (before 5.19, D5), the session's: no run's rules, nothing looked up.
pub enum Asker {
    Run {
        run: u64,
        session: u64,
        process: String,
        rules: Arc<super::Egress>,
        told: std::sync::mpsc::Sender<crate::sandbox::Denial>,
    },
    Session,
}

/// What a name the rules allow came to when tarae looked it up for real (E1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exists {
    /// It has addresses — of these families.
    Yes { v4: bool, v6: bool },
    /// No such name.
    No,
    /// The lookup failed otherwise (no network, a resolver that didn't answer): SERVFAIL, the client tries again.
    Unknown,
}

/// How tarae looks a name up for real (E1): the system's resolver, the absolute name; a stand-in in tests.
pub type Lookup = Arc<dyn Fn(&str) -> Exists + Send + Sync>;

/// The system's: `getaddrinfo` of the absolute name (`name.` — its own search list never expands it), any family.
pub fn system() -> Lookup {
    Arc::new(|name: &str| {
        let Ok(c) = std::ffi::CString::new(format!("{name}.")) else { return Exists::No };
        // SAFETY: zeroed hints of the C layout; the list the call gives is read, then freed once.
        unsafe {
            let mut hints: libc::addrinfo = std::mem::zeroed();
            hints.ai_family = libc::AF_UNSPEC;
            hints.ai_socktype = libc::SOCK_STREAM;
            let mut res: *mut libc::addrinfo = std::ptr::null_mut();
            match libc::getaddrinfo(c.as_ptr(), std::ptr::null(), &hints, &mut res) {
                0 => {
                    let (mut v4, mut v6) = (false, false);
                    let mut p = res;
                    while !p.is_null() {
                        v4 |= (*p).ai_family == libc::AF_INET;
                        v6 |= (*p).ai_family == libc::AF_INET6;
                        p = (*p).ai_next;
                    }
                    libc::freeaddrinfo(res);
                    Exists::Yes { v4, v6 }
                }
                libc::EAI_NONAME => Exists::No,
                _ => Exists::Unknown,
            }
        }
    })
}

/// What a question gets, before any lookup.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Now(Answer),
    /// The name looked up for real first (E1), then `after`.
    LookUp(String),
}

/// What a question gets (`plan`), and what the run's report says of it (None: nothing — an answer like any other).
pub type Planned = (Plan, Option<String>);

/// `q`'s answer in session `s`: `named`, whether the asker's rules (or the user's answers) name a host; `through`,
/// whether a name would go out through the upstream proxy (then tarae resolves nothing).
pub fn plan(
    q: &Question,
    s: &Session,
    named: impl Fn(&str) -> bool,
    through: impl Fn(&str) -> bool,
) -> Planned {
    let now = |a| (Plan::Now(a), None);
    let refused = |why: String| (Plan::Now(Answer::Rcode(dns::REFUSED)), Some(why));
    if q.qclass != dns::CLASS_IN {
        return refused(format!(
            "{} (class {}): only the Internet class is answered",
            shown(&q.name),
            q.qclass
        ));
    }
    if q.qtype == dns::TYPE_PTR {
        return now(ptr(&q.name, &s.names));
    }
    let Ok(name) = parse::name(&q.name) else {
        return match q.name.is_empty() {
            true => refused("the root (`.`): no name to answer".into()),
            false => now(Answer::Rcode(dns::NXDOMAIN)),
        };
    };
    if route::never_named(&name) {
        let why = format!("{name}: a cloud metadata server's own name — never answered");
        return (Plan::Now(Answer::Rcode(dns::NXDOMAIN)), Some(why));
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return now(match q.qtype {
            dns::TYPE_A => Answer::Addresses(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
            dns::TYPE_AAAA if s.v6 => Answer::Addresses(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]),
            _ => Answer::Empty,
        });
    }
    match q.qtype {
        dns::TYPE_A | dns::TYPE_AAAA => {}
        dns::TYPE_HTTPS | dns::TYPE_SVCB => return now(Answer::Empty),
        t => {
            return refused(format!(
                "{name} ({}): tarae answers a name's addresses only (A, AAAA) — its other records aren't asked for \
                 in the session",
                type_said(t)
            ));
        }
    }
    let named = named(&name);
    // (E6: the search list's candidate for a name with dots — the client asks for that name next)
    if !named && candidate(&name, &s.search) {
        return now(Answer::Rcode(dns::NXDOMAIN));
    }
    if named && !through(&name) {
        return match s.looked(&name) {
            Some(e) => now(after(q, s, e)),
            None => (Plan::LookUp(name), None),
        };
    }
    now(synthetic(q.qtype, &name, s))
}

/// `q`'s answer once its name's real lookup came to `e` (E1): its families as they are, each from the map.
pub fn after(q: &Question, s: &Session, e: Exists) -> Answer {
    match e {
        Exists::No => Answer::Rcode(dns::NXDOMAIN),
        Exists::Unknown => Answer::Rcode(dns::SERVFAIL),
        Exists::Yes { v4, v6 } => {
            let has = if q.qtype == dns::TYPE_A { v4 } else { v6 };
            match (has, parse::name(&q.name)) {
                (true, Ok(name)) => synthetic(q.qtype, &name, s),
                _ => Answer::Empty,
            }
        }
    }
}

/// `name`'s address of `qtype`'s family from the map — AAAA none where the session carries no IPv6 (NODATA).
fn synthetic(qtype: u16, name: &str, s: &Session) -> Answer {
    match qtype {
        dns::TYPE_AAAA if !s.v6 => Answer::Empty,
        dns::TYPE_AAAA => Answer::Addresses(vec![IpAddr::V6(s.names.give(name).v6)]),
        _ => Answer::Addresses(vec![IpAddr::V4(s.names.give(name).v4)]),
    }
}

/// Whether `name` is two or more labels and then one of `search`'s domains (E6).
fn candidate(name: &str, search: &[String]) -> bool {
    search.iter().any(|d| {
        name.strip_suffix(d.as_str())
            .and_then(|stem| stem.strip_suffix('.'))
            .is_some_and(|stem| stem.contains('.'))
    })
}

/// A PTR question's answer: a pool address's name from the map, any other NXDOMAIN.
fn ptr(name: &str, names: &Names) -> Answer {
    let ip = reversed(name);
    match ip.and_then(|ip| names.name_of(ip)) {
        Some(n) => Answer::Ptr(n),
        None => Answer::Rcode(dns::NXDOMAIN),
    }
}

/// The address a reverse name stands for (`4.3.2.1.in-addr.arpa`, 32 nibbles of `ip6.arpa`).
fn reversed(name: &str) -> Option<IpAddr> {
    if let Some(v4) = name.strip_suffix(".in-addr.arpa") {
        let o: Vec<u8> = v4.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        let [d, c, b, a] = o[..] else { return None };
        return Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
    }
    let v6 = name.strip_suffix(".ip6.arpa")?;
    let n: Vec<u8> = v6
        .split('.')
        .map(|p| u8::from_str_radix(p, 16).ok().filter(|_| p.len() == 1))
        .collect::<Option<_>>()?;
    if n.len() != 32 {
        return None;
    }
    let mut b = [0u8; 16];
    for (i, pair) in n.rchunks(2).enumerate() {
        b[i] = pair[1] << 4 | pair[0];
    }
    Some(IpAddr::V6(Ipv6Addr::from(b)))
}

/// A name as a report says it (`.` for the root).
fn shown(name: &str) -> &str {
    if name.is_empty() { "." } else { name }
}

fn type_said(t: u16) -> String {
    match t {
        2 => "NS".into(),
        5 => "CNAME".into(),
        6 => "SOA".into(),
        15 => "MX".into(),
        16 => "TXT".into(),
        33 => "SRV".into(),
        255 => "ANY".into(),
        t => format!("type {t}"),
    }
}

#[cfg(test)]
mod tests;
