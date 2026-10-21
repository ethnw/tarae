//! Which session each internet socket of a run's is, where sessions share this machine's loopback (step 8, D18,
//! `docs/design/sandbox-keeper.md` §Where sessions share a loopback). With no network namespace of the session's
//! own — tier host — tarae makes every internet socket a run has
//! (`supervisor::socket`), so it knows each from the moment it exists — by its cookie (`SO_COOKIE`: 64-bit, never
//! reused, the same number the kernel's own list gives each socket, `diag`). A connect or an addressed send
//! to an opened loopback port looks up the sockets the kernel would deliver it to (`listener_at`, from the sockets
//! on that port in tarae's network namespace: `diag::on_port`) and is refused when one of them is another
//! session's (`route::Refusal::Another`). A socket of no session's — the user's own listener, tarae's — is reached
//! as the port's opening says. A bind or a listen that would join another session's socket on its port — at an
//! address that overlaps (`overlap`), through `SO_REUSEPORT` or `SO_REUSEADDR` — is refused too (`another_on`).
//!
//! Rows go when their keeper ends (`ended`: what its runs left ends with it). A session that makes sockets without
//! end is swept past `KEPT`: a row stays while its socket is in tarae's tables (bound, listening, connected) or was
//! touched since the sweep before — made, or named by a call tarae served (`bind`, `listen`, a connect or a send:
//! where an unbound socket binds itself, a UDP socket's first send), which makes it a row again if a sweep took it.
//!
//! Why the cookie, not the inode: inode numbers of sockets come from one 32-bit counter the machine's pipes and
//! other pseudo files draw from too (`get_next_ino`), so a busy machine wraps it in hours or days, and a row whose
//! socket closed long ago would name a newer socket. A cookie names one socket ever. The inode is kept beside it
//! only for where the kernel's list can't be had (`/proc/net` has no cookie): there a reused number can name a socket
//! of no session's as another session's — refused where it would be reached; never another session's as its own (a
//! session's socket rewrites its number's row when made).
//!
//! Accepted, and said: the check and the call are two moments — a listener can go and another come between them
//! (microseconds; user decision 10).
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{LazyLock, Mutex};

/// A session's rows past which it's swept (then again at twice what stayed).
pub const KEPT: usize = 65536;

/// How long a dead keeper's rows are asked after (`Owners::let_go`). What its runs left that still holds a socket an
/// hour on is a daemon that outlived it for good; a thread asking every 30 s per dead keeper till tarae's end buys
/// nothing more. Its rows stay as they are: a cookie names one socket ever, so a row left can't name another — it
/// costs its memory, and only `/proc/net`'s fallback, which finds rows by inode, can err (to refusing) — and the
/// session's next sweep past `KEPT` drops it once its socket is gone.
pub const LET_GO_FOR: std::time::Duration = std::time::Duration::from_secs(3600);

/// Every session's sockets, tarae-wide: a supervisor serves one run, a session's runs are many.
pub static OWNERS: LazyLock<Owners> = LazyLock::new(|| Owners::with_cap(KEPT));

/// Whose a socket is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    /// Its session (`Policy.session`).
    pub session: u64,
    /// The keeper its run is below (`Keeper.serial`): its rows go when that one ends — a session's keeper may be
    /// superseded by another while its last runs finish.
    pub keeper: u64,
    /// Its session's sweeps before it was last touched: a sweep keeps what was touched since the one before.
    round: u64,
    /// Its inode: found by it only where the kernel's list can't be had.
    inode: u64,
}

/// The sockets of sessions sharing this machine's loopback, by cookie.
pub struct Owners {
    inner: Mutex<Inner>,
    cap: usize,
}

#[derive(Default)]
struct Inner {
    by_cookie: HashMap<u64, Owner>,
    /// inode → the cookie of the last socket made with that number (`/proc/net`'s lookups).
    by_ino: HashMap<u64, u64>,
    sessions: HashMap<u64, Rows>,
}

/// A session's rows: how many, the count at which it's swept next, its sweeps so far, whether a sweep is reading the
/// kernel's list now.
#[derive(Clone, Copy)]
struct Rows {
    n: usize,
    at: usize,
    round: u64,
    sweeping: bool,
}

impl Inner {
    /// `n` rows of `session`'s gone.
    fn less(&mut self, session: u64, n: usize) {
        if let Some(r) = self.sessions.get_mut(&session) {
            r.n = r.n.saturating_sub(n);
        }
    }

    /// `session` swept: what the kernel still lists (`live`, cookies) stays, and what was touched since the sweep
    /// before — a row made a moment ago may be a socket about to bind itself.
    fn sweep(&mut self, session: u64, live: &HashSet<u64>, cap: usize) {
        let Some(r) = self.sessions.get(&session).copied() else { return };
        self.by_cookie.retain(|c, o| o.session != session || o.round == r.round || live.contains(c));
        self.prune_inodes();
        let n = self.by_cookie.values().filter(|o| o.session == session).count();
        self.sessions.insert(session, Rows { n, at: (2 * n).max(cap), round: r.round + 1, sweeping: false });
    }

    /// The inodes whose socket's row is gone go too.
    fn prune_inodes(&mut self) {
        let rows = &self.by_cookie;
        self.by_ino.retain(|i, c| rows.get(c).is_some_and(|o| o.inode == *i));
    }

    /// Whose the socket of `row` is: by its cookie — by its inode only where the list gave none (`/proc/net`).
    fn owner(&self, row: &Row) -> Option<&Owner> {
        let cookie = row.cookie.or_else(|| self.by_ino.get(&row.inode).copied())?;
        self.by_cookie.get(&cookie)
    }
}

impl Owners {
    pub fn with_cap(cap: usize) -> Owners {
        Owners { inner: Mutex::new(Inner::default()), cap }
    }

    /// A socket (`cookie`, inode `ino`) of a run below `keeper` of `session`'s, touched: made by tarae, or named by
    /// a call it served — a row again if a sweep took it. `live`: the cookies the kernel lists now — read only when
    /// the session is swept (once per `KEPT` sockets at most), with the table let go: a dump of every socket can take
    /// a while, and every session's calls wait on this table; one sweep of a session at a time. None: unread, no
    /// sweep.
    pub fn made(
        &self,
        (cookie, ino): (u64, u64),
        session: u64,
        keeper: u64,
        live: impl FnOnce() -> Option<HashSet<u64>>,
    ) {
        let cap = self.cap;
        let lock = || self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut t = lock();
        let fresh = Rows { n: 0, at: cap, round: 0, sweeping: false };
        let rows = *t.sessions.entry(session).or_insert(fresh);
        t.by_ino.insert(ino, cookie);
        let before = t.by_cookie.insert(cookie, Owner { session, keeper, round: rows.round, inode: ino });
        match before {
            Some(b) if b.session == session => return,
            Some(b) => t.less(b.session, 1),
            None => {}
        }
        let r = t.sessions.get_mut(&session).expect("made above");
        r.n += 1;
        if r.n < r.at || r.sweeping {
            return;
        }
        r.sweeping = true;
        drop(t);
        let read = live();
        let mut t = lock();
        match read {
            Some(live) => t.sweep(session, &live, cap),
            // (unread: no sweep — the next try at twice the count)
            None => {
                if let Some(r) = t.sessions.get_mut(&session) {
                    (r.at, r.sweeping) = (2 * r.n, false);
                }
            }
        }
    }

    /// `session` swept now, as past its cap (tests).
    #[cfg(test)]
    pub fn swept(&self, session: u64, live: &HashSet<u64>) {
        let cap = self.cap;
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sweep(session, live, cap);
    }

    /// `keeper` ended, and with it everything its runs left: their rows go.
    pub fn ended(&self, keeper: u64) {
        let mut t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut gone = HashMap::<u64, usize>::new();
        t.by_cookie.retain(|_, o| {
            if o.keeper == keeper {
                *gone.entry(o.session).or_default() += 1;
            }
            o.keeper != keeper
        });
        t.prune_inodes();
        for (session, n) in gone {
            t.less(session, n);
        }
    }

    /// `keeper` died, or was killed for not ending in time: what its runs left may still hold their sockets. Its
    /// rows go as their sockets do — the kernel's list (`live`) asked again and again, `wait` between (from 100 ms,
    /// doubling to 30 s); a socket still listed (a process it left still holds it, wherever that went) stays its
    /// session's, and an unread list keeps every row. Returns once none is left — or after `LET_GO_FOR`, what's left
    /// kept as it is, the count of it returned (a thread's).
    pub fn let_go(
        &self,
        keeper: u64,
        live: impl Fn() -> Option<HashSet<u64>>,
        wait: impl Fn(std::time::Duration),
    ) -> usize {
        let mut every = std::time::Duration::from_millis(100);
        let mut waited = std::time::Duration::ZERO;
        loop {
            if let Some(live) = live() {
                let mut t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                let mut gone = HashMap::<u64, usize>::new();
                t.by_cookie.retain(|c, o| {
                    let goes = o.keeper == keeper && !live.contains(c);
                    if goes {
                        *gone.entry(o.session).or_default() += 1;
                    }
                    !goes
                });
                t.prune_inodes();
                for (session, n) in gone {
                    t.less(session, n);
                }
                let left = t.by_cookie.values().filter(|o| o.keeper == keeper).count();
                if left == 0 || waited >= LET_GO_FOR {
                    return left;
                }
            } else if waited >= LET_GO_FOR {
                let t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                return t.by_cookie.values().filter(|o| o.keeper == keeper).count();
            }
            wait(every);
            waited += every;
            every = (every * 2).min(std::time::Duration::from_secs(30));
        }
    }

    /// Whose the socket of `cookie` is (tests).
    #[cfg(test)]
    pub fn get(&self, cookie: u64) -> Option<Owner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).by_cookie.get(&cookie).copied()
    }

    /// How many rows `session` has (tests).
    #[cfg(test)]
    pub fn of(&self, session: u64) -> usize {
        let t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        t.by_cookie.values().filter(|o| o.session == session).count()
    }

    /// The other session a call of `session`'s to `to` would reach, by `rows` (the sockets of its kind on `to`'s
    /// port): one of the sockets the kernel would deliver it to that's another session's — any one: with
    /// `SO_REUSEPORT` the kernel picks among them by a hash of the caller's address.
    pub fn anothers(&self, rows: &[Row], to: SocketAddr, session: u64) -> Option<u64> {
        let t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        listener_at(rows, to).iter().filter_map(|r| t.owner(r)).map(|o| o.session).find(|s| *s != session)
    }

    /// Whether a call of `session`'s to `to` (on a socket of `tcp`, else UDP) would land on another session's
    /// socket: by the port's sockets as the kernel lists them (`listed`), else by tarae's whole tables (`table`:
    /// `/proc/net`'s by name — slower; the same answer but `IPV6_V6ONLY`, which there errs to refusing). Err:
    /// neither could be read, whose it is can't be told — the caller refuses.
    pub fn another_at(
        &self,
        to: SocketAddr,
        (tcp, session): (bool, u64),
        listed: impl FnOnce() -> io::Result<Vec<Row>>,
        table: impl Fn(&str) -> io::Result<String>,
    ) -> Result<bool, String> {
        let rows = rows_from(tcp, listed, table)?;
        Ok(self.anothers(&rows, to, session).is_some())
    }

    /// Whether a bind or a listen of `session`'s at local `at` (`v6only`: the socket's `IPV6_V6ONLY`) would join
    /// another session's socket — a `SO_REUSEPORT` group (the same user: the kernel lets it), an address beside its
    /// `SO_REUSEADDR` one: one of the other session's on that port (`listed`: its TCP listeners — the kernel lists a
    /// bound TCP socket only once it listens, so a listen asks too —, its UDP sockets in any state) whose address
    /// overlaps `at` (`overlap`) — or a UDP socket of the other session's connected to that port at an address `at`
    /// takes: its plain sends, which tarae never serves, would come to this one. As `another_at`, else by `table`
    /// (where v6-only isn't told: errs to refusing); Err: neither.
    pub fn another_on(
        &self,
        (at, v6only): (SocketAddr, bool),
        (tcp, session): (bool, u64),
        listed: impl FnOnce() -> io::Result<Vec<Row>>,
        table: impl Fn(&str) -> io::Result<String>,
    ) -> Result<bool, String> {
        let rows = rows_from(tcp, listed, table)?;
        let t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let on = rows.iter().filter(|r| {
            let beside = r.local.port() == at.port()
                && (r.open || !tcp)
                && overlap((at.ip(), v6only), (r.local.ip(), r.v6only));
            let aimed = !tcp
                && r.peer
                    .is_some_and(|p| p.port() == at.port() && overlap((at.ip(), v6only), (p.ip(), false)));
            beside || aimed
        });
        Ok(on.filter_map(|r| t.owner(r)).any(|o| o.session != session))
    }
}

/// The sockets `listed` gives, else every one of tarae's `table`s of the kind (`tcp`, else UDP; an IPv6 one missing
/// where IPv6 is off) — Err where neither can be read.
fn rows_from(
    tcp: bool,
    listed: impl FnOnce() -> io::Result<Vec<Row>>,
    table: impl Fn(&str) -> io::Result<String>,
) -> Result<Vec<Row>, String> {
    let listed = match listed() {
        Ok(rows) => return Ok(rows),
        Err(e) => e,
    };
    let names = if tcp { ["tcp", "tcp6"] } else { ["udp", "udp6"] };
    let v4 = table(names[0])
        .map_err(|e| format!("sock_diag ({listed}) and /proc/net/{} ({e}) unread", names[0]))?;
    let mut found = rows(&v4, tcp);
    found.extend(rows(&table(names[1]).unwrap_or_default(), tcp));
    Ok(found)
}

/// A socket as the kernel lists it: its local address, its inode and cookie (`/proc/net` gives none), whether a
/// call to that address may land on it — a TCP listener; a UDP socket connected to no one (a connected one takes
/// only its peer's) —, an IPv6 one, whether it's `IPV6_V6ONLY` (`/proc/net` doesn't say: false there), and the
/// address it's connected to, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub local: SocketAddr,
    pub inode: u64,
    pub cookie: Option<u64>,
    pub open: bool,
    pub v6only: bool,
    pub peer: Option<SocketAddr>,
}

/// The rows of a `/proc/net/{tcp,udp}[6]` table — `tcp`: a TCP one (else UDP). Each address word as the kernel
/// prints it: a 32-bit word in this machine's order, `%08X`; the port `%04X`. (Where the kernel's list can't be had:
/// a call's lookup asks for its port's alone, `diag::on_port`.)
pub fn rows(table: &str, tcp: bool) -> Vec<Row> {
    let word = |h: &str| u32::from_str_radix(h, 16).ok().map(u32::to_ne_bytes);
    let ip = |hex: &str| -> Option<IpAddr> {
        match hex.len() {
            8 => Some(IpAddr::V4(Ipv4Addr::from(word(hex)?))),
            32 => {
                let mut b = [0u8; 16];
                for (i, chunk) in b.chunks_mut(4).enumerate() {
                    chunk.copy_from_slice(&word(hex.get(i * 8..i * 8 + 8)?)?);
                }
                Some(IpAddr::V6(Ipv6Addr::from(b)))
            }
            _ => None,
        }
    };
    let addr = |a: &str| -> Option<SocketAddr> {
        let (h, p) = a.rsplit_once(':')?;
        Some(SocketAddr::new(ip(h)?, u16::from_str_radix(p, 16).ok()?))
    };
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            // (sl, local, remote, st, tx:rx, tr:when, retrnsmt, uid, timeout, inode, …)
            let cols: Vec<&str> = line.split_whitespace().collect();
            let (local, remote) = (addr(cols.get(1)?)?, addr(cols.get(2)?)?);
            let inode = cols.get(9)?.parse().ok()?;
            let open = match tcp {
                true => *cols.get(3)? == "0A",
                false => remote.ip().is_unspecified() && remote.port() == 0,
            };
            let peer = (remote.port() != 0).then_some(remote);
            Some(Row { local, inode, cookie: None, open, v6only: false, peer })
        })
        .collect()
}

/// The sockets among `rows` a call to `to` lands on, as the kernel looks them up (`__inet_lookup_listener`,
/// `__udp4_lib_lookup`): on `to`'s port, those bound to its very address — else those bound to every address. An
/// IPv4 call lands on an IPv4 socket, or on an IPv6 one bound to the address's mapped form or to `::` unless it's
/// `v6only`; an IPv6 call only on IPv6 sockets. Every one of them, not the one the kernel picks by score: what's
/// asked of them is whether any is another session's. A call to `0.0.0.0` or `::` the kernel sends to the calling
/// socket's own address, else to loopback (`ip_route_output_key_hash_rcu`, `tcp_v6_connect`) — which address isn't
/// told here: it lands on every socket of its family on the port.
pub fn listener_at(rows: &[Row], to: SocketAddr) -> Vec<Row> {
    let dst = to.ip().to_canonical();
    let on: Vec<&Row> = rows.iter().filter(|r| r.open && r.local.port() == to.port()).collect();
    if dst.is_unspecified() {
        let of_family = |r: &&&Row| match (dst, r.local.ip()) {
            (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => true,
            (IpAddr::V4(_), IpAddr::V6(a)) => {
                !r.v6only && (a.is_unspecified() || a.to_ipv4_mapped().is_some())
            }
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        };
        return on.iter().filter(of_family).map(|r| **r).collect();
    }
    let lands = |r: &Row, any: bool| {
        let at = |a: IpAddr| if any { a.is_unspecified() } else { a == dst };
        match (dst, r.local.ip()) {
            (IpAddr::V4(_), IpAddr::V4(a)) => at(IpAddr::V4(a)),
            (IpAddr::V4(_), IpAddr::V6(a)) if any => a.is_unspecified() && !r.v6only,
            (IpAddr::V4(_), IpAddr::V6(a)) => {
                a.to_ipv4_mapped().is_some_and(|m| at(IpAddr::V4(m))) && !r.v6only
            }
            (IpAddr::V6(_), IpAddr::V6(a)) => at(IpAddr::V6(a)),
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    };
    let found = |any| on.iter().filter(|r| lands(r, any)).map(|r| **r).collect::<Vec<Row>>();
    let exact = found(false);
    if exact.is_empty() { found(true) } else { exact }
}

/// Whether sockets bound at `a` and at `b` on one port (each with its `IPV6_V6ONLY`) share an address — as the
/// kernel's bind conflict compares two (`inet_rcv_saddr_equal`), either way round: the same address; one bound to
/// every address of its family; an IPv6 `::` to every IPv4 address too, unless v6-only; an IPv4-mapped IPv6 address
/// as its IPv4 one.
pub fn overlap(a: (IpAddr, bool), b: (IpAddr, bool)) -> bool {
    #[derive(Clone, Copy)]
    enum Reach<T> {
        No,
        One(T),
        Every,
    }
    use Reach::*;
    // (the IPv4 addresses, the IPv6 ones, a socket at `ip` is reached on)
    let reach = |(ip, v6only): (IpAddr, bool)| -> (Reach<Ipv4Addr>, Reach<Ipv6Addr>) {
        match ip {
            IpAddr::V4(a) if a.is_unspecified() => (Every, No),
            IpAddr::V4(a) => (One(a), No),
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(m) if m.is_unspecified() => (Every, No),
                Some(m) => (One(m), No),
                None if a.is_unspecified() => (if v6only { No } else { Every }, Every),
                None => (No, One(a)),
            },
        }
    };
    fn meet<T: PartialEq>(a: Reach<T>, b: Reach<T>) -> bool {
        match (a, b) {
            (No, _) | (_, No) => false,
            (Every, _) | (_, Every) => true,
            (One(x), One(y)) => x == y,
        }
    }
    let ((a4, a6), (b4, b6)) = (reach(a), reach(b));
    meet(a4, b4) || meet(a6, b6)
}

/// The cookies of every socket the kernel lists in tarae's own netns (bound, listening or connected) — what a sweep
/// keeps; None where its list can't be had (no sweep then: rows wait for their keeper).
#[cfg(target_os = "linux")]
pub fn live() -> Option<HashSet<u64>> {
    diag::every().ok()
}

#[cfg(target_os = "linux")]
pub mod diag;

#[cfg(test)]
pub mod tests;
