//! What a run reaches by address on Linux, decided by tarae: the seccomp filter's served mode
//! (`seccomp::Mode::Served`) hands tarae every call that names one, and this answers it — as macOS's
//! profile has it (`Policy::profile`): this machine's ports as `llm.sandbox-localhost` and `-ports` say,
//! listening on this machine only, unix sockets in the project and the run's own folder, and the ones the
//! user granted, each that one file.
//!
//! The safe form only: a call's address is read once from the caller's memory (`Caller::read`), decided on
//! tarae's own copy, and the call is done by tarae — on the caller's own socket, taken with `pidfd_getfd` (or tarae's
//! copy of it: `Serving::Copies`) —
//! with that copy; the caller gets the result. The kernel never goes on with a call tarae read (no
//! `SECCOMP_USER_NOTIF_FLAG_CONTINUE`: the caller could change the address in between), so what the run
//! writes there afterwards reaches nothing. A unix socket is opened by its real path without following a
//! link (`openat2`, `RESOLVE_NO_SYMLINKS`) and reached through tarae's own descriptor of it
//! (`/proc/self/fd/N`): a link swapped in after the check reaches nothing either.
//!
//! Internet sockets are tarae's to make (`socket`): bound to the loopback device before the run gets them
//! (`SECCOMP_IOCTL_NOTIF_ADDFD`), so a socket that listens on every address — a bind to `0.0.0.0` or `::`,
//! or a `listen` with no bind — hears only this machine, and a run can't take the device off (that takes
//! `CAP_NET_RAW`). A run sees its connections made by tarae: a unix server's peer credentials name tarae's
//! process, not the run's.
//!
//! Netlink (the routing protocol's, for name lookups) is read only: the kernel judges a netlink call by the
//! capability of whoever makes it — tarae, who holds `CAP_NET_ADMIN` over the session's network namespace (and
//! over this machine's, run as root) — so tarae makes one for a run only where a sender without it may: a
//! batch of reads (`netlink_reads`), to the kernel's address, joining no group that asks the capability.
//!
//! What it needs from the system (`unavailable` says what's missing): a seccomp listener (none inside
//! another one's filter — a sandboxed tarae). Reading a run's memory takes what ptrace takes: under Yama's
//! `ptrace_scope` 1 (the default on Ubuntu) a process that left tarae's tree — reparented away — gets every such
//! call refused; at 2 without `CAP_SYS_PTRACE`, or 3, no run can be served (`yama_allows`). Where `pidfd_getfd` is
//! refused (Docker's default filter, without `CAP_SYS_PTRACE`) it serves by copies (6f′, `Serving::Copies`): every
//! socket the run has is tarae's to make — its unix and netlink ones, a `socketpair`'s, an `accept`'s too — and a
//! copy of each kept (`copies.rs`), found by the inode the run's descriptor names.

use std::io;
use std::mem::MaybeUninit;
#[cfg(test)]
use std::net::IpAddr;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use super::route::{Reach, Refusal, Route};
use super::tier::RunNet;
use super::wire::{
    Addr, MSG_LEN_AT, Mem, Msg, SEND_MAX, Unsent, address, mmsg_at, named, named_in, netlink_address,
    netlink_reads, parse, read_msg, unix_address,
};
use super::{Denial, Policy};

/// A call's answer: what it returns, or the errno it fails with.
type Answer = Result<i64, i32>;

/// How tarae serves its runs' calls here (6f′).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serving {
    /// On the run's own descriptors, taken (`pidfd_getfd`).
    Taken,
    /// `pidfd_getfd` refused (Docker's default filter): on tarae's own copies of every socket the run has
    /// (`RunNet::Copied`).
    Copies,
}

/// How tarae serves its runs' calls here — or why it can't. Asked once (a worker's: it starts a thread).
pub fn serving() -> Result<Serving, String> {
    static HOW: std::sync::OnceLock<Result<Serving, String>> = std::sync::OnceLock::new();
    HOW.get_or_init(|| {
        // (a measurement's other way, on a machine that takes a run's sockets: served by copies — and so the
        // session born without a netns, `tier::probe_with` — as where a filter refuses `pidfd_getfd`; M8.2)
        #[cfg(test)]
        if std::env::var_os("TARAE_TEST_COPIES").is_some() && probe().is_ok() {
            return Ok(Serving::Copies);
        }
        probe()
    })
    .clone()
}

/// Why tarae can't serve its runs' calls here — None when it can.
pub fn unavailable() -> Option<String> {
    serving().err()
}

/// Whether it serves by copies here (`Serving::Copies`).
pub fn copies() -> bool {
    serving() == Ok(Serving::Copies)
}

/// A listener on a filter of a short-lived thread's own (refused under another listener: `EBUSY`), a pidfd, and a
/// descriptor of tarae's own taken through it — refused (a container's filter refuses `pidfd_getfd`): by copies.
fn probe() -> Result<Serving, String> {
    let listener = crate::event::thread("tarae-seccomp", || -> io::Result<()> {
        let mut allow = [libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        }];
        let prog = libc::sock_fprog { len: 1, filter: allow.as_mut_ptr() };
        // SAFETY: this thread's own filter (it ends with the thread), a program that outlives the call.
        let fd = unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = libc::SECCOMP_FILTER_FLAG_NEW_LISTENER;
            libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, flags, &raw const prog)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the listener just made, ours.
        drop(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        Ok(())
    })
    .join()
    .unwrap_or_else(|_| Err(io::Error::other("the probe panicked")));
    // SAFETY: getpid has no preconditions.
    let taken = pidfd_open(unsafe { libc::getpid() })
        .map_err(|e| format!("pidfd_open: {e}"))
        .map(|pidfd| pidfd_getfd(&pidfd, pidfd.as_raw_fd()).map(drop));
    let scope = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok();
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    decided(listener, taken, yama_allows(scope.as_deref(), &status))
}

/// The probe's answer from what it found: a listener of a thread's own (or why not — `EBUSY`: inside another
/// supervisor's filter), a descriptor of tarae's own taken through its pidfd — refused (Docker's default filter
/// refuses `pidfd_getfd`): served by copies, not "can't serve" — and Yama's word on reading a run's memory.
fn decided(
    listener: io::Result<()>,
    taken: Result<io::Result<()>, String>,
    yama: Result<(), String>,
) -> Result<Serving, String> {
    listener.map_err(|e| match e.raw_os_error() {
        Some(libc::EBUSY) => "tarae itself runs under another supervisor's filter".to_string(),
        _ => format!("no seccomp listener: {e}"),
    })?;
    let how = match taken? {
        Ok(()) => Serving::Taken,
        Err(_) => Serving::Copies,
    };
    yama.map(|()| how)
}

/// Whether Yama (`ptrace_scope`, absent: no Yama) lets tarae read its own runs' memory: 0 and 1 do (a run is
/// tarae's child — 1 refuses only what left its tree), 2 only with `CAP_SYS_PTRACE` (in `status`'s `CapEff`),
/// 3 never.
fn yama_allows(scope: Option<&str>, status: &str) -> Result<(), String> {
    const CAP_SYS_PTRACE: u32 = 19;
    let cap = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_SYS_PTRACE) != 0);
    match scope.map(str::trim) {
        Some("2") if !cap => Err("Yama's ptrace_scope 2 keeps tarae from reading its runs' calls".into()),
        Some("3") => Err("Yama's ptrace_scope 3 keeps tarae from reading its runs' calls".into()),
        _ => Ok(()),
    }
}

fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: plain call; a descriptor it makes is ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made (close-on-exec, as pidfds always are).
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// The caller's descriptor `n`, as one of tarae's own (the same open socket — close-on-exec).
pub(super) fn pidfd_getfd(pidfd: &OwnedFd, n: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: plain call; a descriptor it makes is ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), n, 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// The last call's result as an answer.
fn result(r: isize) -> Answer {
    if r < 0 { Err(errno(&io::Error::last_os_error())) } else { Ok(r as i64) }
}

/// What a supervisor serves one run by — all it has of tarae's (`docs/design/sandbox-keeper.md`: the supervisor
/// behind its few messages, so moving it into a process of its own later is mechanical): the run's policy as it
/// was when the run started (nothing comes mid-run), how its sockets go (`net`: `Served`, tarae makes them; or
/// `Routed`, its own in the session's netns), and where its refusals go out (`Refused`). Its own state: the
/// sockets of tarae's it put in the run's place (`swapped`, by inode — where each goes).
pub struct Served {
    policy: Arc<Policy>,
    net: RunNet,
    /// Shared so a mark (6d) can hold it weakly: the run's refusal thread (`sandbox::told`) ends once the supervisor
    /// does — no process of the run left — whatever the session's marks keep.
    refused: Arc<std::sync::mpsc::Sender<Denial>>,
    swapped: std::sync::Mutex<std::collections::HashMap<u64, Route>>,
    /// The sockets the run bound to every address and a port the kernel picks (`0.0.0.0:0`, `[::]:0` — what
    /// Rust's `UdpSocket` and Java's `DatagramSocket` do before sending): by inode. Such a socket goes out as
    /// one it never bound would (`swapped_for`): nothing of its address is its own.
    any_port: std::sync::Mutex<std::collections::HashSet<u64>>,
    /// For the egress proxy (step 5): this run's id, its session's (its project's), and its rules — what each
    /// connection's message carries (D23).
    run: u64,
    session: u64,
    egress: Arc<super::proxy::egress::Egress>,
    /// The session's marks (6d), where its keeper has the session's network (TPROXY): what goes beyond is the run's
    /// own socket marked with its run's (`Route::Redirected`) — None: no such network, beyond is the proxy's pair.
    marks: Option<Arc<super::marks::Marks>>,
    /// By copies (`RunNet::Copied`, 6f′): tarae's copy of every socket the run has — None: taken.
    copies: Option<Arc<super::copies::Copies>>,
    /// The ptys its rules grant (3b, `linux::Confine::ptys`): what it gets in place of those it makes.
    ptys: Option<Arc<super::ptys::Ptys>>,
    /// Its keeper, below which its processes are (what they hold of its ptys: `ptys::holders`).
    tree: u32,
    /// That keeper's serial (`Keeper.serial`): what the run's sockets are known by where sessions share a
    /// loopback (`owners`).
    keeper: u64,
}

impl Served {
    /// What its policy opens, for a call on a socket of `kind` (`SO_TYPE`).
    fn reach(&self, kind: i32) -> Reach<'_> {
        Reach {
            localhost: self.localhost,
            ports: &self.ports,
            network: self.network,
            resolvers: &self.resolvers,
            // (the network open goes through tarae's egress proxy, step 5 — where the session's own network doesn't
            // take it: a TPROXY session's runs are told of no proxy (6e′), and one that names it itself goes as any
            // connect beyond, to the planted socket — 6f)
            proxy: (self.network && !self.taken()).then_some(super::route::PROXY),
            // (6d takes TCP: a datagram beyond goes as before — refused, said — but DNS, the session's own: 6e)
            redirect: self.taken() && kind == libc::SOCK_STREAM,
            names: self.taken(),
        }
    }

    /// Whether the session's own network takes what goes beyond (its keeper has a TPROXY network: the marks).
    fn taken(&self) -> bool {
        self.marks.is_some() && self.netns()
    }

    fn netns(&self) -> bool {
        self.net == RunNet::Routed
    }

    /// Whether tarae made the run's internet sockets, bound to `lo` (tier C: served or by copies).
    fn makes_sockets(&self) -> bool {
        matches!(self.net, RunNet::Served | RunNet::Copied)
    }

    /// Whether a connect's or a send's work of step 8 is done for this run: where sessions share this machine's
    /// loopback (`makes_sockets`) — but, in tests, a session measured without it (`APART_OFF`).
    fn kept_apart(&self) -> bool {
        #[cfg(test)]
        if APART_OFF.load(Relaxed) == self.session {
            return false;
        }
        self.makes_sockets()
    }

    /// The caller's socket `n` as tarae's own (the same open socket): taken (`pidfd_getfd`) — or, by copies,
    /// tarae's copy of it, found by the inode its descriptor names (`/proc/<tid>/fd/<n>`: `socket:[…]`). One tarae has
    /// no copy of — handed in from outside the sandbox — is refused, said.
    fn sock(&self, c: &Caller, n: u64) -> Result<OwnedFd, i32> {
        let Some(copies) = &self.copies else { return c.fd(n) };
        // (the run's doing something: its copies scanned soon again — a close comes with calls around it)
        copies.stir();
        let link = c.link(n)?;
        let ino = super::copies::socket_inode(&link).ok_or(libc::ENOTSOCK)?;
        match copies.get(ino) {
            Some(fd) => Ok(fd),
            None => refuse(self, c, "network-bound", outside_copies()).and(Err(libc::EPERM)),
        }
    }

    /// A descriptor the caller passes (`SCM_RIGHTS`) as tarae's own: taken — or, by copies, a socket's copy, and any
    /// other file opened again through `/proc/<tid>/fd/<n>` with the access its descriptor has (its `fdinfo` flags):
    /// the same file, an open one of its own (its offset isn't the caller's). One that can't be opened again — an
    /// eventfd, an epoll, a pidfd (`anon_inode:`) — is refused, said.
    fn passed(&self, c: &Caller, n: u64) -> Result<OwnedFd, i32> {
        if self.copies.is_none() {
            return c.fd(n);
        }
        let link = c.link(n)?;
        if super::copies::socket_inode(&link).is_some() {
            return self.sock(c, n);
        }
        if link.starts_with("anon_inode:") {
            let why = format!(
                "passing a descriptor ({link}) — by tarae's copies only a file, a pipe or a socket goes"
            );
            return refuse(self, c, "network-bound", why).and(Err(libc::EPERM));
        }
        c.reopen(n)
    }

    /// Whether `ip` (beyond loopback) is one of this machine's own addresses, as read when the run's policy was
    /// made (`Policy::own_addresses`) — None where they couldn't be read: `route` refuses it.
    fn mine(&self, ip: std::net::IpAddr) -> Option<bool> {
        self.own_addresses.as_ref().map(|own| own.contains(&ip))
    }

    /// The sockets a message passed (`SCM_RIGHTS`, tarae's descriptors of them), sent on `via`: by copies, each kept
    /// while the message may wait in the receiver's queue (`Copies::in_flight`) — files and pipes have no copy.
    fn in_flight(&self, passed: &[OwnedFd], via: &OwnedFd) {
        let Some(copies) = &self.copies else { return };
        // (sockets only: a file's inode number is its own filesystem's, and may be a socket's too)
        for fd in passed.iter().filter(|fd| sockopt(fd, libc::SO_TYPE).is_ok()) {
            if let Ok(ino) = super::copies::inode(fd) {
                copies.in_flight(ino, via);
            }
        }
    }

    /// A socket of tarae's the run is about to hold, its copy kept first (by copies; else nothing to keep).
    fn keep(&self, fd: &OwnedFd) -> Result<(), i32> {
        match &self.copies {
            Some(c) => c.keep(fd).map_err(|e| errno(&e)),
            None => Ok(()),
        }
    }

    /// An internet socket the run holds — tarae made it, or a call tarae served just bound it or made it listen —
    /// known as its session's where sessions share this machine's loopback (step 8, `owners`; a bound one again:
    /// a sweep may have let it go before). Elsewhere (the session's own netns) nothing to know.
    fn owned(&self, sock: &OwnedFd) {
        if !self.makes_sockets() {
            return;
        }
        let domain = sockopt(sock, libc::SO_DOMAIN);
        let (Ok(libc::AF_INET | libc::AF_INET6), Ok(ino), Ok(cookie)) = (domain, inode(sock), cookie(sock))
        else {
            return;
        };
        super::owners::OWNERS.made((cookie, ino), self.session, self.keeper, super::owners::live);
    }
}

/// What a socket tarae has no copy of is refused with (by copies).
fn outside_copies() -> String {
    "a socket made outside tarae's sandbox (handed in): with pidfd_getfd refused here, tarae serves only the \
     sockets it made"
        .into()
}

impl std::ops::Deref for Served {
    type Target = Policy;
    fn deref(&self) -> &Policy {
        &self.policy
    }
}

/// The calls a supervisor answers, as its counts name them.
const KINDS: [&str; 10] =
    ["socket", "bind", "connect", "sendto", "sendmsg", "sendmmsg", "listen", "socketpair", "accept", "pty"];

fn kind(nr: i32) -> usize {
    match nr as libc::c_long {
        libc::SYS_socket => 0,
        libc::SYS_bind => 1,
        libc::SYS_connect => 2,
        libc::SYS_sendto => 3,
        libc::SYS_sendmsg => 4,
        libc::SYS_listen => 6,
        libc::SYS_socketpair => 7,
        libc::SYS_accept | libc::SYS_accept4 => 8,
        libc::SYS_ioctl => 9,
        _ => 5,
    }
}

/// What supervisors did, by call: how many, the time from a call's arrival to its answer (summed, and the
/// longest), and the threads made for the calls that may wait — where a run's time in tarae goes (M4). Each
/// run's own goes to the log when its listener ends; every run's add up in `TOTALS`, which a measurement reads
/// before and after (`totals`).
struct Stats {
    calls: [AtomicU64; KINDS.len()],
    nanos: [AtomicU64; KINDS.len()],
    longest: [AtomicU64; KINDS.len()],
    threads: AtomicU64,
}

impl Stats {
    const fn new() -> Stats {
        Stats {
            calls: [const { AtomicU64::new(0) }; KINDS.len()],
            nanos: [const { AtomicU64::new(0) }; KINDS.len()],
            longest: [const { AtomicU64::new(0) }; KINDS.len()],
            threads: AtomicU64::new(0),
        }
    }

    fn add(&self, kind: usize, took: std::time::Duration) {
        let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.calls[kind].fetch_add(1, Relaxed);
        self.nanos[kind].fetch_add(ns, Relaxed);
        self.longest[kind].fetch_max(ns, Relaxed);
    }

    fn now(&self) -> Totals {
        let each = |a: &[AtomicU64; KINDS.len()]| a.each_ref().map(|v| v.load(Relaxed));
        Totals {
            calls: each(&self.calls),
            nanos: each(&self.nanos),
            longest: each(&self.longest),
            threads: self.threads.load(Relaxed),
        }
    }
}

static TOTALS: Stats = Stats::new();

/// What `Stats` held at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub calls: [u64; KINDS.len()],
    pub nanos: [u64; KINDS.len()],
    pub longest: [u64; KINDS.len()],
    pub threads: u64,
}

impl Totals {
    /// What was done since `before` (the longest: of all, not only since — a measurement reads the run's log
    /// line for its own).
    #[cfg(test)]
    pub fn since(self, before: Totals) -> Totals {
        let less =
            |a: [u64; KINDS.len()], b: [u64; KINDS.len()]| std::array::from_fn(|i| a[i].saturating_sub(b[i]));
        Totals {
            calls: less(self.calls, before.calls),
            nanos: less(self.nanos, before.nanos),
            longest: self.longest,
            threads: self.threads.saturating_sub(before.threads),
        }
    }

    /// `connect 3 · sendto 120 — 12.3 ms in tarae (longest 4.1 ms), 3 threads`, the calls it had only.
    pub fn said(&self) -> String {
        let calls: Vec<String> = (0..KINDS.len())
            .filter(|&i| self.calls[i] > 0)
            .map(|i| format!("{} {}", KINDS[i], self.calls[i]))
            .collect();
        let ms = |ns: u64| ns as f64 / 1e6;
        format!(
            "{} — {:.1} ms in tarae (longest {:.1} ms), {} threads",
            if calls.is_empty() { "no calls".into() } else { calls.join(" · ") },
            ms(self.nanos.iter().sum()),
            ms(self.longest.iter().copied().max().unwrap_or(0)),
            self.threads
        )
    }
}

/// Tests (M8.3): the session whose connects and sends skip step 8's work (`Served::kept_apart`) — 0: none.
#[cfg(test)]
pub static APART_OFF: AtomicU64 = AtomicU64::new(0);

/// Tests (M4): every supervisor's calls so far (`Totals::since` for a span).
#[cfg(test)]
pub fn totals() -> Totals {
    TOTALS.now()
}

/// Starts the supervisor of one run, on a thread of its own made from the calling thread (unconfined: a
/// thread a confined one made would be confined too, and its own calls would wait on itself). It waits for
/// the run's listener (taken from the run's process: `sandbox::keeper`), then answers that run's calls until no
/// process of it is left, its refusals out on `refused`. The sender dropped unsent: it ends. `net`: `Served`,
/// `Routed` or `Copied` (`Served`). `marks`: the session's (6d), where its network takes what goes beyond. `tree`:
/// its keeper's pid, below which its processes are (by copies: what's read for the sockets they hold, `copies.rs`),
/// and serial (`owners`). `ptys`: its pool, where its filter hands over the way to a pty's other end (`pty_ioctl`).
pub fn serve(
    policy: Arc<Policy>,
    net: RunNet,
    refused: std::sync::mpsc::Sender<Denial>,
    marks: Option<Arc<super::marks::Marks>>,
    tree: (u32, u64),
    ptys: Option<Arc<super::ptys::Ptys>>,
) -> io::Result<std::sync::mpsc::SyncSender<OwnedFd>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<OwnedFd>(1);
    let session = policy.session;
    // (the policy's hosts and the user's answers as it was made; this machine's own addresses as it read them)
    let egress = Arc::new(super::proxy::egress::Egress {
        own_addresses: policy.own_addresses.clone(),
        ..policy.egress.clone()
    });
    let run = policy.run_id;
    let (tree, keeper) = tree;
    let policy = Arc::new(Served {
        policy,
        net,
        refused: Arc::new(refused),
        swapped: Default::default(),
        any_port: Default::default(),
        run,
        session,
        egress,
        marks,
        copies: (net == RunNet::Copied).then(|| super::copies::Copies::new(tree)),
        ptys,
        tree,
        keeper,
    });
    if let Some(ptys) = &policy.ptys {
        ptys.watch(tree);
    }
    std::thread::Builder::new().name("tarae-serve".into()).spawn(move || {
        quiet_signals();
        let Ok(fd) = rx.recv() else { return };
        let l = Arc::new(Listener(fd));
        let run = Arc::new(Stats::new());
        let took = |run: &Stats, kind: usize, at: std::time::Instant| {
            let t = at.elapsed();
            run.add(kind, t);
            TOTALS.add(kind, t);
        };
        while let Some(n) = l.next() {
            let at = std::time::Instant::now();
            let k = kind(n.data.nr);
            match n.data.nr as libc::c_long {
                // (quick: answered here)
                libc::SYS_socket => l.answer(n.id, socket(&l, &n, &policy)),
                libc::SYS_socketpair => l.answer(n.id, socketpair(&l, &n, &policy)),
                libc::SYS_bind => l.answer(n.id, bind(&l, &n, &policy)),
                libc::SYS_listen => l.answer(n.id, listen(&l, &n, &policy)),
                libc::SYS_ioctl => l.answer(n.id, pty_ioctl(&l, &n, &policy)),
                // (a connect or a send may wait — as long as the caller would have)
                _ => {
                    let (on, p, stats) = (l.clone(), policy.clone(), run.clone());
                    let spawned = std::thread::Builder::new().name("tarae-serve".into()).spawn(move || {
                        on.answer(n.id, call(&on, &n, &p));
                        took(&stats, k, at);
                    });
                    if spawned.is_err() {
                        l.answer(n.id, Err(libc::EAGAIN));
                    } else {
                        run.threads.fetch_add(1, Relaxed);
                        TOTALS.threads.fetch_add(1, Relaxed);
                        // (its time is added when it's answered)
                        continue;
                    }
                }
            }
            took(&run, k, at);
        }
        // (no process of the run left: its ptys go, the watch with them)
        if let Some(ptys) = &policy.ptys {
            ptys.end();
        }
        let said = run.now();
        if said.calls.iter().any(|&c| c > 0) {
            crate::log::line("sandbox", "info", &format!("a run's supervisor ended: {}", said.said()));
        }
    })?;
    Ok(tx)
}

/// No signal interrupts what a supervisor's thread does for a run (a connect cut short by tarae's own
/// signals would end differently than the run's).
fn quiet_signals() {
    // SAFETY: a set filled before use; only this thread's mask changes.
    unsafe {
        let mut all = MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigfillset(all.as_mut_ptr());
        libc::pthread_sigmask(libc::SIG_BLOCK, all.as_ptr(), std::ptr::null_mut());
    }
}

fn call(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    match n.data.nr as libc::c_long {
        libc::SYS_connect => connect(l, n, p),
        libc::SYS_sendto => sendto(l, n, p),
        libc::SYS_sendmsg => sendmsg(l, n, p),
        libc::SYS_sendmmsg => sendmmsg(l, n, p),
        libc::SYS_accept | libc::SYS_accept4 => accept(l, n, p),
        _ => Err(libc::ENOSYS),
    }
}

/// The run's seccomp listener.
struct Listener(OwnedFd);

/// `SECCOMP_IOCTL_NOTIF_ID_VALID` as it was first numbered (`_IOR`, v5.0's uapi header). The header has named it
/// `_IOW` since v5.9; the kernel's handler, which isn't in the headers, kept taking the old number (`_WRONG_DIR`, as
/// the 6f review has it — and every run of tarae's on 6.1, 6.8 and 7.3 sent it). Which kernel's handler first took
/// `_IOW` the uapi headers alone don't say, so the old number stays: it's the one taken from 5.0 on.
const ID_VALID: libc::Ioctl = libc::_IOR::<u64>(b'!' as u32, 2);

impl Listener {
    /// The next call, waiting for it — None once no process of the run is left (or the listener fails).
    fn next(&self) -> Option<libc::seccomp_notif> {
        loop {
            let mut pfd = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one live pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, -1) };
            if r < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return None;
            }
            if pfd.revents & libc::POLLIN == 0 {
                return None; // (POLLHUP: the filter has no process left)
            }
            // SAFETY: an all-integer struct, zeroed as the kernel asks of it.
            let mut n: libc::seccomp_notif = unsafe { std::mem::zeroed() };
            // SAFETY: the struct the request names, live for the call.
            if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_RECV, &mut n) } == 0 {
                return Some(n);
            }
            match io::Error::last_os_error().raw_os_error() {
                // (the caller died meanwhile, or a signal)
                Some(libc::ENOENT | libc::EINTR) => continue,
                _ => return None,
            }
        }
    }

    /// Whether call `id` still waits — its caller is the process that asked (no later one with its pid).
    fn valid(&self, id: u64) -> io::Result<()> {
        let mut id = id;
        // SAFETY: a live u64 for the request.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), ID_VALID, &mut id) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Answers call `id` (a caller gone meanwhile hears nothing — nothing to do).
    fn answer(&self, id: u64, a: Answer) {
        let (val, error) = match a {
            Ok(v) => (v, 0),
            Err(e) => (0, -e),
        };
        let mut resp = libc::seccomp_notif_resp { id, val, error, flags: 0 };
        // SAFETY: the struct the request names, live for the call.
        unsafe { libc::ioctl(self.0.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_SEND, &mut resp) };
    }

    /// Puts `fd` among call `id`'s caller's descriptors — at number `at` in place of what's there (as `dup2`:
    /// `SECCOMP_ADDFD_FLAG_SETFD`, Linux 5.9), else at the lowest free one: the number it got there.
    fn add_fd(&self, id: u64, fd: &OwnedFd, at: Option<u32>, cloexec: bool) -> io::Result<i64> {
        let mut add = libc::seccomp_notif_addfd {
            id,
            flags: if at.is_some() { libc::SECCOMP_ADDFD_FLAG_SETFD as u32 } else { 0 },
            srcfd: fd.as_raw_fd() as u32,
            newfd: at.unwrap_or(0),
            newfd_flags: if cloexec { libc::O_CLOEXEC as u32 } else { 0 },
        };
        // SAFETY: the struct the request names, live for the call.
        let r = unsafe { libc::ioctl(self.0.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_ADDFD, &mut add) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(i64::from(r))
    }
}

/// The thread that made a call, opened for what tarae needs of it: its memory, and its process's
/// descriptors (through a pidfd — threads share them, as a rule).
struct Caller {
    tid: i32,
    tgid: i32,
    mem: std::fs::File,
    pidfd: OwnedFd,
}

impl Caller {
    /// Opened, then checked that the call still waits — so what was opened is the caller's, not a later
    /// process's that took its pid (the kernel's own advice). One it may not read (Yama, a process that made
    /// itself undumpable): refused.
    fn open(l: &Listener, n: &libc::seccomp_notif) -> Result<Caller, i32> {
        let tid = n.pid as i32;
        let status = std::fs::read_to_string(format!("/proc/{tid}/status")).map_err(|_| libc::ESRCH)?;
        let tgid = status
            .lines()
            .find_map(|l| l.strip_prefix("Tgid:"))
            .and_then(|v| v.trim().parse().ok())
            .ok_or(libc::ESRCH)?;
        let mem = std::fs::OpenOptions::new().read(true).write(true).open(format!("/proc/{tid}/mem"));
        let mem =
            mem.map_err(|e| if e.kind() == io::ErrorKind::NotFound { libc::ESRCH } else { libc::EPERM })?;
        let pidfd = pidfd_open(tgid).map_err(|_| libc::ESRCH)?;
        l.valid(n.id).map_err(|_| libc::ESRCH)?;
        Ok(Caller { tid, tgid, mem, pidfd })
    }

    /// `len` bytes at `at` in its memory, read once.
    fn read(&self, at: u64, len: usize) -> Result<Vec<u8>, i32> {
        let mut buf = vec![0u8; len];
        self.mem.read_exact_at(&mut buf, at).map_err(|_| libc::EFAULT)?;
        Ok(buf)
    }

    fn write(&self, at: u64, bytes: &[u8]) -> Result<(), i32> {
        self.mem.write_all_at(bytes, at).map_err(|_| libc::EFAULT)
    }

    /// Its descriptor `n`, as tarae's own.
    fn fd(&self, n: u64) -> Result<OwnedFd, i32> {
        let n = i32::try_from(n).map_err(|_| libc::EBADF)?;
        pidfd_getfd(&self.pidfd, n).map_err(|e| errno(&e))
    }

    /// What its descriptor `n` names (`/proc/<tid>/fd/<n>`: `socket:[…]`, `pipe:[…]`, a path …) — none there: `EBADF`.
    fn link(&self, n: u64) -> Result<String, i32> {
        let n = i32::try_from(n).map_err(|_| libc::EBADF)?;
        let link = std::fs::read_link(format!("/proc/{}/fd/{n}", self.tid)).map_err(|_| libc::EBADF)?;
        Ok(link.to_string_lossy().into_owned())
    }

    /// Its descriptor `n` — a file, a pipe — opened again by tarae through `/proc/<tid>/fd/<n>`, with the access its
    /// descriptor has (`fdinfo`'s flags: read, write, append, `O_PATH`; its `O_NONBLOCK` set after — a pipe's write end
    /// opened without waiting for a reader), close-on-exec, never creating or truncating.
    fn reopen(&self, n: u64) -> Result<OwnedFd, i32> {
        let n = i32::try_from(n).map_err(|_| libc::EBADF)?;
        let info =
            std::fs::read_to_string(format!("/proc/{}/fdinfo/{n}", self.tid)).map_err(|_| libc::EBADF)?;
        let flags = info
            .lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .and_then(|f| i32::from_str_radix(f.trim(), 8).ok())
            .ok_or(libc::EBADF)?;
        let keep = libc::O_ACCMODE | libc::O_APPEND | libc::O_PATH;
        let open = (flags & keep) | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK;
        let path = std::ffi::CString::new(format!("/proc/{}/fd/{n}", self.tid)).map_err(|_| libc::EINVAL)?;
        // SAFETY: a NUL-terminated path of ours; a descriptor it makes is ours.
        let fd = unsafe { libc::open(path.as_ptr(), open) };
        if fd < 0 {
            return Err(errno(&io::Error::last_os_error()));
        }
        // SAFETY: just made.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if flags & libc::O_NONBLOCK == 0 && flags & libc::O_PATH == 0 {
            // SAFETY: plain calls on our own descriptor.
            unsafe {
                let now = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, now & !libc::O_NONBLOCK);
            }
        }
        Ok(fd)
    }

    /// What signal waits for it (`pending`, from its `/proc/<tid>/status`).
    fn pending(&self) -> Pending {
        pending(&std::fs::read_to_string(format!("/proc/{}/status", self.tid)).unwrap_or_default())
    }

    /// The folder it works in (a relative socket path is from there).
    fn cwd(&self) -> Result<PathBuf, i32> {
        std::fs::read_link(format!("/proc/{}/cwd", self.tid)).map_err(|_| libc::ENOENT)
    }

    /// Its program's name, as a denial says it (`java`).
    fn name(&self) -> String {
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", self.tgid)).unwrap_or_default();
        match comm.trim() {
            "" => "the run".into(),
            c => c.into(),
        }
    }

    /// A send that found its stream broken signals the thread, as the kernel would have — unless it said
    /// `MSG_NOSIGNAL` (tarae's own send always does).
    fn sigpipe(&self, flags: i32, a: Answer) -> Answer {
        if a == Err(libc::EPIPE) && flags & libc::MSG_NOSIGNAL == 0 {
            // SAFETY: a signal to one thread of the caller's.
            unsafe { libc::syscall(libc::SYS_tgkill, self.tgid, self.tid, libc::SIGPIPE) };
        }
        a
    }
}

/// What `wire` reads of a run's memory: the caller's own (`Caller::read`).
impl Mem for Caller {
    fn read(&self, at: u64, len: usize) -> Result<Vec<u8>, i32> {
        Caller::read(self, at, len)
    }
}

/// A refusal: `Refused` out (tarae says it in the run's report — its watch, once the run is gone its session's
/// current run's: `sandbox::tell`), `EPERM` to the caller — "Operation not permitted", as macOS says it.
fn refuse(p: &Served, c: &Caller, op: &str, target: String) -> Answer {
    refuse_with(p, c, libc::EPERM, op, target)
}

/// `refuse`, the caller told `errno`.
fn refuse_with(p: &Served, c: &Caller, errno: i32, op: &str, target: String) -> Answer {
    let _ = p.refused.send(Denial { process: c.name(), op: op.into(), target });
    Err(errno)
}

/// An integer socket option of `fd`'s.
pub(super) fn sockopt(fd: &OwnedFd, name: libc::c_int) -> Result<i32, i32> {
    opt(fd, libc::SOL_SOCKET, name)
}

/// An integer option of `fd`'s at `level`.
fn opt(fd: &OwnedFd, level: libc::c_int, name: libc::c_int) -> Result<i32, i32> {
    let (mut v, mut len) = (0i32, std::mem::size_of::<i32>() as libc::socklen_t);
    // SAFETY: an int and its length, live for the call.
    let r = unsafe { libc::getsockopt(fd.as_raw_fd(), level, name, (&raw mut v).cast(), &mut len) };
    if r < 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    Ok(v)
}

/// Where a call of the caller's naming `to` goes (`route::route`), on a socket of `kind` (`SO_TYPE`): whether
/// the session listens there read from the caller's own `/proc/<tid>/net` — its netns's, the session's — and
/// whether an address is this machine's own (`Served::mine`), only where that decides.
fn routed(p: &Served, c: &Caller, to: SocketAddr, kind: i32) -> Route {
    #[cfg(test)]
    if p.resolvers.iter().any(|r| r.ip().to_canonical() == to.ip().to_canonical() && r.port() == to.port()) {
        RESOLVER_CALLS.fetch_add(1, Relaxed);
    }
    // (at port 53, where the session answers names, only a server at that very address — or at every address — is the
    // session's own: tarae's DNS socket is at the session's own address, which any port-53 check would find — 6e)
    let listens = || match to.port() == super::route::RESOLVER_PORT {
        true => session_listens_at(c, to, kind),
        false => session_listens(c, to.port(), kind),
    };
    let route = super::route::route(to, p.netns(), &p.reach(kind), listens, |ip| p.mine(ip));
    // (step 8: where sessions share this machine's loopback, never to another session's socket)
    // (asked only where sessions share this machine's loopback: there's no netns of the session's own)
    super::route::kept_apart(route, !p.kept_apart(), || anothers_at(p, to, kind))
}

/// Whether a call to `to` on a socket of `kind` would land on another session's socket here (step 8, `owners`): the
/// sockets on its port as the kernel lists them (`owners::diag`) — where it can't, tarae's whole tables
/// (`/proc/net`: slower, the same answer but `IPV6_V6ONLY`, which there errs to refusing). Neither readable: Err —
/// whose it is can't be told (`told`), refused as `Refusal::Untold`.
fn anothers_at(p: &Served, to: SocketAddr, kind: i32) -> Result<bool, String> {
    let tcp = kind == libc::SOCK_STREAM;
    let listed = || super::owners::diag::on_port(tcp, to.port());
    anothers_by(&super::owners::OWNERS, to, (tcp, p.session), listed, proc_net)
}

/// `anothers_at` by `owners`, the port's sockets from `listed`, else `table` (tests: each way it falls back).
fn anothers_by(
    owners: &super::owners::Owners,
    to: SocketAddr,
    (tcp, session): (bool, u64),
    listed: impl FnOnce() -> io::Result<Vec<super::owners::Row>>,
    table: impl Fn(&str) -> io::Result<String>,
) -> Result<bool, String> {
    told(owners.another_at(to, (tcp, session), listed, table), to.port())
}

/// Whether a bind or a listen of the caller's at local `at` (`IPV6_V6ONLY` beside it; a socket of `kind`) would join
/// another session's socket on an address that overlaps it (step 8, `owners::Owners::another_on`): a
/// `SO_REUSEPORT` group, an address beside its `SO_REUSEADDR` one — refused, as the kernel refuses a port taken.
/// Err: can't be told (`told`).
fn joins_another(p: &Served, at: (SocketAddr, bool), kind: i32) -> Result<bool, String> {
    let tcp = kind == libc::SOCK_STREAM;
    let listed = || super::owners::diag::bound_on(tcp, at.0.port());
    let answer = super::owners::OWNERS.another_on(at, (tcp, p.session), listed, proc_net);
    told(answer, at.0.port())
}

/// A bind or a listen of the caller's at `at` (with its `IPV6_V6ONLY`), as `joins_another` answers it: onto another
/// session's socket — refused, `EADDRINUSE`, said by its port (`SESSION_OP`); whose can't be told — refused, said so
/// (`UNTOLD_OP`).
fn refuse_joining(p: &Served, c: &Caller, at: (SocketAddr, bool), kind: i32) -> Result<(), i32> {
    let target = format!("local:{}", at.0);
    match joins_another(p, at, kind) {
        Ok(false) => Ok(()),
        Ok(true) => refuse_with(p, c, libc::EADDRINUSE, super::SESSION_OP, target).map(drop),
        Err(_) => refuse_with(p, c, libc::EADDRINUSE, super::UNTOLD_OP, target).map(drop),
    }
}

/// The local address a bind of `addr` on internet socket `sock` asks for — an IPv4 socket's `AF_UNSPEC` too, which
/// the kernel takes as every address at the port it names (`inet_bind`'s compatibility rule); None: no internet
/// address (a unix or netlink socket's, or one the kernel refuses).
fn asked_at(sock: &OwnedFd, addr: &[u8]) -> Result<Option<SocketAddr>, i32> {
    Ok(match (sockopt(sock, libc::SO_DOMAIN)?, parse(addr)) {
        (libc::AF_INET | libc::AF_INET6, Ok(Addr::Ip(a))) => Some(a),
        (libc::AF_INET, Ok(Addr::Unspec)) if addr.len() >= 4 => {
            Some(SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, u16::from_be_bytes([addr[2], addr[3]]))))
        }
        _ => None,
    })
}

/// An IPv6 socket's `IPV6_V6ONLY` (an IPv4 one's: false).
fn v6only(sock: &OwnedFd) -> Result<bool, i32> {
    match sockopt(sock, libc::SO_DOMAIN)? {
        libc::AF_INET6 => Ok(opt(sock, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)? != 0),
        _ => Ok(false),
    }
}

/// `bind` — the kernel picking a UDP socket's port — run with the socket's `SO_REUSEADDR` and `SO_REUSEPORT` off,
/// each put back after (step 8). With either on, the port the kernel picks may be one another socket holds with
/// them too (`udp_lib_lport_inuse` leaves such a port free) — another session's, whose datagrams it would then take:
/// pinned by `owners::tests::a_port_the_kernel_picks_is_another_sockets_only_for_udp_reusing`. With both off it
/// picks one no socket has. (TCP never shares a port it picks: `inet_csk_find_open_port`.) Where they can't be
/// turned off the bind isn't made: `refused` says why (a denial in the run's report) and answers the call; the log too.
pub(super) fn alone(
    sock: &OwnedFd,
    bind: impl FnOnce() -> Answer,
    refused: impl FnOnce(String) -> Answer,
) -> Answer {
    let set = |o: libc::c_int, v: i32| {
        // SAFETY: an int and its size, live for the call, on the caller's socket (ours through `pidfd_getfd`).
        let size = std::mem::size_of::<i32>() as libc::socklen_t;
        match unsafe { libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, o, (&raw const v).cast(), size) }
        {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    };
    alone_with(|o| sockopt(sock, o).unwrap_or(0), set, bind, refused)
}

/// `alone`, the socket's options read by `get` and set by `set` (tests: one that can't be set).
fn alone_with(
    get: impl Fn(libc::c_int) -> i32,
    set: impl Fn(libc::c_int, i32) -> io::Result<()>,
    bind: impl FnOnce() -> Answer,
    refused: impl FnOnce(String) -> Answer,
) -> Answer {
    let on: Vec<libc::c_int> =
        [libc::SO_REUSEADDR, libc::SO_REUSEPORT].into_iter().filter(|o| get(*o) != 0).collect();
    if on.is_empty() {
        return bind();
    }
    if let Some(e) = on.iter().find_map(|o| set(*o, 0).err()) {
        for o in &on {
            let _ = set(*o, 1);
        }
        let why = format!(
            "a UDP socket's SO_REUSEADDR/SO_REUSEPORT couldn't be turned off for the port the kernel picks ({e}): \
             not bound — it could have been given another session's port"
        );
        crate::log::line("sandbox", "warn", &why);
        return refused(why);
    }
    let r = bind();
    for o in on {
        if let Err(e) = set(o, 1) {
            crate::log::line(
                "sandbox",
                "warn",
                &format!("a socket's option {o} not put back after its bind: {e}"),
            );
        }
    }
    r
}

/// `bind` — and, by copies, again where its port was taken: that may be a closed socket's copy, kept until collected
/// (collected now, tried again: `Copies::bind_again`) — a port named, or the only one the kernel may pick.
fn copied_again(p: &Served, bind: impl Fn() -> Answer) -> Answer {
    let r = bind();
    match (&r, &p.copies) {
        (Err(libc::EADDRINUSE), Some(copies)) => {
            copies.bind_again(bind, |e| *e == libc::EADDRINUSE).unwrap_or(r)
        }
        _ => r,
    }
}

/// How many ports the kernel is asked for at most (`picked`) before a UDP bind to port 0 is refused.
const PICKS: usize = 4;

/// `bind` (`alone`) giving the caller's UDP socket `sock` a port the kernel picks, where sessions share this
/// machine's loopback (step 8): a port that a UDP socket of another session's is connected to — its plain sends,
/// which tarae never serves, would come here — is let go again (a disconnect unbinds a socket bound to port 0:
/// `udp_disconnect`) and another asked for, `PICKS` times at most; then refused, said, as a port taken
/// (`EADDRINUSE`). The kernel picks at random over the ephemeral range, so only a socket that narrowed it
/// (`IP_LOCAL_PORT_RANGE`) or a full range is refused.
fn picked(p: &Served, c: &Caller, sock: &OwnedFd, bind: impl Fn() -> Answer) -> Answer {
    if !p.makes_sockets() {
        return bind();
    }
    let mut last = None;
    for _ in 0..PICKS {
        let r = bind()?;
        let Some(at) = local_addr(sock) else { return Ok(r) };
        match joins_another(p, (at, v6only(sock)?), libc::SOCK_DGRAM) {
            Ok(false) => return Ok(r),
            Ok(true) => unbind(sock)?,
            // (whose can't be told: no other port would be told either)
            Err(_) => {
                unbind(sock)?;
                return refuse_with(p, c, libc::EADDRINUSE, super::UNTOLD_OP, format!("local:{at}"));
            }
        }
        last = Some(at);
    }
    let at = last.map_or_else(String::new, |a| format!("local:{a}"));
    refuse_with(p, c, libc::EADDRINUSE, super::SESSION_OP, at)
}

/// A UDP socket bound to port 0 let go of its port (`connect` to `AF_UNSPEC`: `udp_disconnect` unhashes one whose
/// port wasn't named), to be bound again.
fn unbind(sock: &OwnedFd) -> Result<(), i32> {
    // SAFETY: an all-zero sockaddr is valid; its family set, the size its own, live for the call.
    let r = unsafe {
        let mut a: libc::sockaddr = std::mem::zeroed();
        a.sa_family = libc::AF_UNSPEC as libc::sa_family_t;
        libc::connect(
            sock.as_raw_fd(),
            &raw const a,
            std::mem::size_of::<libc::sockaddr>() as libc::socklen_t,
        )
    };
    result(r as isize).map(drop)
}

/// Where sessions share this machine's loopback, a UDP socket still unbound that a connect or a send of the caller's
/// is about to bind (the kernel binds it to every address, at a port of its picking: `inet_autobind`) bound so here
/// first — `alone` and `picked`, as a bind to port 0 is (step 8) —, and known as one the run never bound
/// (`any_port`).
fn bound_alone(p: &Served, c: &Caller, sock: &OwnedFd) -> Result<(), i32> {
    if !p.kept_apart() || local_addr(sock).is_some() {
        return Ok(());
    }
    let every = match sockopt(sock, libc::SO_DOMAIN)? {
        libc::AF_INET => SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0)),
        libc::AF_INET6 => SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
        _ => return Ok(()),
    };
    let (a, len) = super::proxy::raw_addr(every);
    // SAFETY: the socket and an address of ours, live for the call.
    let bind = || result(unsafe { libc::bind(sock.as_raw_fd(), (&raw const a).cast(), len) } as isize);
    let unturned = |why| refuse_with(p, c, libc::EADDRINUSE, "network-bind", why);
    match picked(p, c, sock, || copied_again(p, || alone(sock, bind, unturned))) {
        // (one bound meanwhile — another thread's call — is as bound as this would have made it)
        Ok(_) | Err(libc::EINVAL) => {}
        Err(e) => return Err(e),
    }
    p.any_port.lock().unwrap_or_else(|e| e.into_inner()).insert(inode(sock)?);
    Ok(())
}

/// One of tarae's `/proc/net` tables, by name.
fn proc_net(table: &str) -> io::Result<String> {
    std::fs::read_to_string(format!("/proc/thread-self/net/{table}"))
}

/// An answer of `owners`' — neither the kernel's list nor tarae's tables read (Err): whose the socket on `port` is
/// can't be told, which the caller refuses as such (`UNTOLD_OP`), why said in the log.
fn told(answer: Result<bool, String>, port: u16) -> Result<bool, String> {
    answer.inspect_err(|why| crate::log::line("sandbox", "warn", &format!("{why}: port {port} refused")))
}

/// The local address `sock` is bound to — None: unbound (port 0), or not an internet socket.
fn local_addr(sock: &OwnedFd) -> Option<SocketAddr> {
    let mut me = [0u8; std::mem::size_of::<libc::sockaddr_storage>()];
    let mut len = me.len() as libc::socklen_t;
    // SAFETY: a sockaddr buffer and its length for the call to fill.
    if unsafe { libc::getsockname(sock.as_raw_fd(), me.as_mut_ptr().cast(), &mut len) } != 0 {
        return None;
    }
    match parse(&me[..(len as usize).min(me.len())]) {
        Ok(Addr::Ip(a)) if a.port() != 0 => Some(a),
        _ => None,
    }
}

/// Whether the session has a server at `to` itself (6e, port 53): a TCP listener (`kind` stream) or a bound UDP socket
/// at that address or at every address — not merely one on its port somewhere: tarae's DNS socket is at the session's
/// own address, port 53, and isn't a run's server at loopback.
fn session_listens_at(c: &Caller, to: SocketAddr, kind: i32) -> bool {
    let (tables, listen) = match kind {
        libc::SOCK_STREAM => (["tcp", "tcp6"], Some("0A")),
        _ => (["udp", "udp6"], None),
    };
    tables.iter().any(|t| {
        let rows = std::fs::read_to_string(format!("/proc/{}/net/{t}", c.tid)).unwrap_or_default();
        listening_at(&rows, to, listen)
    })
}

/// Whether a `/proc/net/{tcp,udp}[6]` table holds a row at `to` (its address as the table writes it: each 32-bit word
/// in this machine's order, `%08X`) or at every address, on `to`'s port (in `state`, when given).
fn listening_at(rows: &str, to: SocketAddr, state: Option<&str>) -> bool {
    let hex = |b: &[u8]| -> String {
        b.chunks(4).map(|w| format!("{:08X}", u32::from_ne_bytes(w.try_into().unwrap_or_default()))).collect()
    };
    let (at, any) = match to.ip().to_canonical() {
        std::net::IpAddr::V4(v4) => (hex(&v4.octets()), "00000000".to_string()),
        std::net::IpAddr::V6(v6) => (hex(&v6.octets()), "0".repeat(32)),
    };
    let port = format!("{:04X}", to.port());
    rows.lines().skip(1).any(|row| {
        let mut cols = row.split_whitespace().skip(1);
        let (Some(local), Some(_), Some(st)) = (cols.next(), cols.next(), cols.next()) else { return false };
        let Some((addr, p)) = local.rsplit_once(':') else { return false };
        p == port && (addr == at || addr == any) && state.is_none_or(|s| s == st)
    })
}

/// Tests (Q4's measure): calls naming a resolver's port 53, every run's.
#[cfg(test)]
static RESOLVER_CALLS: AtomicU64 = AtomicU64::new(0);

/// Whether the session has a TCP listener (`kind` stream) or a bound UDP socket (datagram) on `port` — any
/// address of its loopback, IPv4 or IPv6 (`/proc/<tid>/net/{tcp,tcp6}` or `{udp,udp6}`: a row's local address
/// `ADDR:PORT` in hex, its state `0A` LISTEN for TCP).
fn session_listens(c: &Caller, port: u16, kind: i32) -> bool {
    let (tables, listen) = match kind {
        libc::SOCK_STREAM => (["tcp", "tcp6"], Some("0A")),
        _ => (["udp", "udp6"], None),
    };
    tables.iter().any(|t| {
        let rows = std::fs::read_to_string(format!("/proc/{}/net/{t}", c.tid)).unwrap_or_default();
        listening_on(&rows, port, listen)
    })
}

/// Whether a `/proc/net/{tcp,udp}[6]` table holds a row on local `port` (in `state`, when given).
fn listening_on(rows: &str, port: u16, state: Option<&str>) -> bool {
    rows.lines().skip(1).any(|row| {
        let mut cols = row.split_whitespace().skip(1);
        let (Some(local), Some(_), Some(st)) = (cols.next(), cols.next(), cols.next()) else { return false };
        let at = local.rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok());
        at == Some(port) && state.is_none_or(|s| s == st)
    })
}

/// `SIOCGSKNS` (`linux/sockios.h`): a descriptor of the network namespace a socket belongs to.
const SIOCGSKNS: libc::c_ulong = 0x894C;

/// Whether `sock` — a socket of the caller's, in `Routed` — is in the session's network namespace: the caller's
/// own (`/proc/<tid>/ns/net`). One a process outside handed in (over a unix socket in the project) isn't: a call
/// on it is made in its namespace — this machine's loopback, its abstract names. `SIOCGSKNS` needs
/// `CAP_NET_ADMIN` over the socket's namespace, which tarae has over the session's (its user namespace is
/// tarae's) and not over this machine's: refused there, so not the session's — as anything unreadable.
fn in_session(c: &Caller, sock: &OwnedFd) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: an ioctl on our own descriptor; a descriptor it makes is ours.
    let ns = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCGSKNS as _) };
    if ns < 0 {
        return false;
    }
    // SAFETY: just made.
    let ns = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(ns) });
    match (ns.metadata(), std::fs::metadata(format!("/proc/{}/ns/net", c.tid))) {
        (Ok(its), Ok(session)) => (its.dev(), its.ino()) == (session.dev(), session.ino()),
        _ => false,
    }
}

/// The inode of a socket — what it's known by in `Served.swapped`.
fn inode(fd: &OwnedFd) -> Result<u64, i32> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a descriptor of ours and a stat buffer the call fills.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: filled by the successful fstat above.
    Ok(unsafe { st.assume_init() }.st_ino)
}

/// A socket's cookie (`SO_COOKIE`: 64-bit, never reused) — what it's known by in `owners`.
fn cookie(fd: &OwnedFd) -> Result<u64, i32> {
    let (mut v, mut len) = (0u64, std::mem::size_of::<u64>() as libc::socklen_t);
    // SAFETY: a u64 and its length, live for the call.
    let r = unsafe {
        libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_COOKIE, (&raw mut v).cast(), &mut len)
    };
    if r < 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    Ok(v)
}

/// The options a swap carries from the run's socket to tarae's, read back from the one and set on the other
/// where they differ from a fresh socket's: (level, option, the socket kinds it's for — 0: every kind).
/// `SO_RCVBUF`/`SO_SNDBUF` read back doubled (the kernel's bookkeeping): set at half.
const CARRIED: [(libc::c_int, libc::c_int, libc::c_int); 20] = [
    (libc::SOL_SOCKET, libc::SO_REUSEADDR, 0),
    (libc::SOL_SOCKET, libc::SO_REUSEPORT, 0),
    (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 0),
    (libc::SOL_SOCKET, libc::SO_BROADCAST, 0),
    (libc::SOL_SOCKET, libc::SO_OOBINLINE, 0),
    (libc::SOL_SOCKET, libc::SO_PRIORITY, 0),
    (libc::SOL_SOCKET, libc::SO_RCVBUF, 0),
    (libc::SOL_SOCKET, libc::SO_SNDBUF, 0),
    (libc::IPPROTO_TCP, libc::TCP_NODELAY, libc::SOCK_STREAM),
    (libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, libc::SOCK_STREAM),
    (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, libc::SOCK_STREAM),
    (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, libc::SOCK_STREAM),
    (libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, libc::SOCK_STREAM),
    (libc::IPPROTO_TCP, libc::TCP_FASTOPEN_CONNECT, libc::SOCK_STREAM),
    (libc::IPPROTO_IP, libc::IP_TOS, 0),
    (libc::IPPROTO_IP, libc::IP_TTL, 0),
    (libc::IPPROTO_IP, libc::IP_RECVERR, 0),
    (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, 0),
    (libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 0),
    (libc::IPPROTO_IPV6, libc::IPV6_RECVERR, 0),
];

/// The options of a struct's size a swap carries as they are: (level, option, its size).
const CARRIED_STRUCTS: [(libc::c_int, libc::c_int, usize); 3] = [
    (libc::SOL_SOCKET, libc::SO_LINGER, std::mem::size_of::<libc::linger>()),
    (libc::SOL_SOCKET, libc::SO_RCVTIMEO, std::mem::size_of::<libc::timeval>()),
    (libc::SOL_SOCKET, libc::SO_SNDTIMEO, std::mem::size_of::<libc::timeval>()),
];

/// `CARRIED` and `CARRIED_STRUCTS` from `from` onto `to` (a fresh socket of the same domain, type and
/// protocol): each that reads back otherwise than on `to`, set there — one that can't be set fails it all.
/// An option its domain doesn't have (IPv6's on an IPv4 socket) reads back an error on both: skipped.
fn carry(from: &OwnedFd, to: &OwnedFd, domain: i32, kind: i32) -> Result<(), i32> {
    let set = |level, name, v: &[u8]| -> Result<(), i32> {
        // SAFETY: the bytes and their length, live for the call.
        let r = unsafe { libc::setsockopt(to.as_raw_fd(), level, name, v.as_ptr().cast(), v.len() as _) };
        result(r as isize).map(drop)
    };
    for (level, name, only) in CARRIED {
        let ip = (level == libc::IPPROTO_IP && domain != libc::AF_INET)
            || (level == libc::IPPROTO_IPV6 && domain != libc::AF_INET6);
        if (only != 0 && only != kind) || ip {
            continue;
        }
        let (Ok(theirs), Ok(fresh)) = (opt(from, level, name), opt(to, level, name)) else { continue };
        if theirs != fresh {
            let v = if matches!(name, libc::SO_RCVBUF | libc::SO_SNDBUF) && level == libc::SOL_SOCKET {
                theirs / 2
            } else {
                theirs
            };
            set(level, name, &v.to_ne_bytes())?;
        }
    }
    for (level, name, size) in CARRIED_STRUCTS {
        let mut v = vec![0u8; size];
        let mut len = size as libc::socklen_t;
        // SAFETY: a buffer of the option's size and its length, live for the call.
        let r = unsafe { libc::getsockopt(from.as_raw_fd(), level, name, v.as_mut_ptr().cast(), &mut len) };
        if r == 0 && v.iter().any(|&b| b != 0) {
            set(level, name, &v[..len as usize])?;
        }
    }
    Ok(())
}

/// Whether the caller's descriptor `n` is close-on-exec (`/proc/<tgid>/fdinfo/<n>`'s `flags:`, octal).
fn cloexec(c: &Caller, n: u64) -> bool {
    let info = std::fs::read_to_string(format!("/proc/{}/fdinfo/{n}", c.tgid)).unwrap_or_default();
    info.lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .and_then(|f| u32::from_str_radix(f.trim(), 8).ok())
        .is_some_and(|f| f & libc::O_CLOEXEC as u32 != 0)
}

/// The socket a call routed `Machine` or `OwnAddress` (`route`) is made on — this machine, its port opened: the
/// caller's descriptor `n` (`sock`) as it is where it's already that — a socket of tarae's bound alike put there
/// before, or in tier C (`Served`) one tarae made for this machine's loopback — else a socket of tarae's own made
/// in its namespace, carrying what the run set (its `O_NONBLOCK`, close-on-exec, `CARRIED`), bound to the loopback
/// device for `Machine` (to none for `OwnAddress`), put in place of the run's at `n` (`SECCOMP_ADDFD_FLAG_SETFD`)
/// before any call is made on it. (Kept for opened ports until connect leaves the supervisor: 6f's F1.) Refused (`EADDRNOTAVAIL`, said as `network-bound`): a socket the run bound (its source address can't
/// move namespaces), filtered (a BPF filter can't be read back) or bound to a device in the session, or one of
/// tarae's already put there bound otherwise; a connected stream: `EISCONN`, as the kernel says it. A socket it bound to every address
/// and a port the kernel picked (`any_port`) goes as one it never bound: the new one gets its own port.
fn swapped_for(
    l: &Listener,
    n: &libc::seccomp_notif,
    p: &Served,
    c: &Caller,
    (fd, sock): (u64, OwnedFd),
    route: Route,
    to: SocketAddr,
) -> Result<OwnedFd, i32> {
    let ino = inode(&sock)?;
    let before = p.swapped.lock().unwrap_or_else(|e| e.into_inner()).get(&ino).copied();
    let refused =
        |why: &str| refuse_with(p, c, libc::EADDRNOTAVAIL, "network-bound", format!("remote:{to} {why}"));
    // (one bound to `lo` for loopback; one bound to no device for the rest — this machine's own addresses too)
    let on_lo = |r: Route| r == Route::Machine;
    match before {
        Some(r) if on_lo(r) == on_lo(route) => return Ok(sock),
        Some(_) => {
            return refused("on a socket already put out of the session the other way").map(|_| sock);
        }
        // (tier C: tarae made every internet socket, bound to `lo` — this machine's already)
        None if p.makes_sockets() && route == Route::Machine => return Ok(sock),
        None => {}
    }
    let (domain, kind) = (sockopt(&sock, libc::SO_DOMAIN)?, sockopt(&sock, libc::SO_TYPE)?);
    let protocol = sockopt(&sock, libc::SO_PROTOCOL)?;
    // (the filter lets the run make no other — `seccomp::ops`; one whose options or messages name addresses
    // would choose where it goes past tarae)
    if !matches!(
        (kind, protocol),
        (libc::SOCK_STREAM, libc::IPPROTO_TCP) | (libc::SOCK_DGRAM, libc::IPPROTO_UDP)
    ) {
        return Err(libc::EPERM);
    }
    let mut peer = [0u8; std::mem::size_of::<libc::sockaddr_storage>()];
    let mut len = peer.len() as libc::socklen_t;
    // SAFETY: a sockaddr buffer and its length for the call to fill.
    let connected = unsafe { libc::getpeername(sock.as_raw_fd(), peer.as_mut_ptr().cast(), &mut len) } == 0;
    if connected && kind == libc::SOCK_STREAM {
        return Err(libc::EISCONN);
    }
    let mut me = [0u8; std::mem::size_of::<libc::sockaddr_storage>()];
    let mut len = me.len() as libc::socklen_t;
    // SAFETY: as above.
    if unsafe { libc::getsockname(sock.as_raw_fd(), me.as_mut_ptr().cast(), &mut len) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    let me = &me[..(len as usize).min(me.len())];
    let any = p.any_port.lock().unwrap_or_else(|e| e.into_inner()).contains(&ino);
    if matches!(parse(me), Ok(Addr::Ip(a)) if a.port() != 0) && !any {
        refused("from a socket it bound itself — a bound socket stays in the session")?;
    }
    let raw = sock.as_raw_fd();
    let mut filter_len: libc::socklen_t = 0;
    // SAFETY: no buffer: the call says the filter's length in instructions only.
    let r = unsafe {
        libc::getsockopt(raw, libc::SOL_SOCKET, libc::SO_GET_FILTER, std::ptr::null_mut(), &mut filter_len)
    };
    let filtered = r == 0 && filter_len > 0;
    let mut dev = [0u8; libc::IFNAMSIZ];
    let mut dev_len = dev.len() as libc::socklen_t;
    // SAFETY: a buffer of a device name's size and its length, live for the call.
    let r = unsafe {
        libc::getsockopt(raw, libc::SOL_SOCKET, libc::SO_BINDTODEVICE, dev.as_mut_ptr().cast(), &mut dev_len)
    };
    let device = p.net == RunNet::Routed && r == 0 && dev_len > 0 && dev[0] != 0;
    if filtered || device {
        refused("from a socket with a filter or a device of its own — those don't leave the session")?;
    }
    // SAFETY: plain call on our own descriptor.
    let nonblock = unsafe { libc::fcntl(sock.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK != 0;
    let flags = libc::SOCK_CLOEXEC | if nonblock { libc::SOCK_NONBLOCK } else { 0 };
    // SAFETY: plain call; a descriptor it makes is ours.
    let new = unsafe { libc::socket(domain, kind | flags, protocol) };
    if new < 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: just made.
    let new = unsafe { OwnedFd::from_raw_fd(new) };
    if route == Route::Machine {
        let lo = b"lo\0";
        // SAFETY: the device's name and its length, live for the call.
        let r = unsafe {
            libc::setsockopt(new.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, lo.as_ptr().cast(), 3)
        };
        result(r as isize)?;
    }
    carry(&sock, &new, domain, kind)?;
    let at = u32::try_from(fd).map_err(|_| libc::EBADF)?;
    // Known as tarae's before the run holds it: another thread of the run's calling on that number the moment
    // it's there (a connect to loopback, a bind, a listen) finds it put out of the session — never takes it for
    // the session's own, in tarae's namespace
    let new_ino = inode(&new)?;
    p.keep(&new)?;
    p.owned(&new);
    let old = {
        let mut swapped = p.swapped.lock().unwrap_or_else(|e| e.into_inner());
        swapped.insert(new_ino, route);
        swapped.remove(&ino)
    };
    if let Err(e) = l.add_fd(n.id, &new, Some(at), cloexec(c, fd)) {
        let mut swapped = p.swapped.lock().unwrap_or_else(|e| e.into_inner());
        swapped.remove(&new_ino);
        if let Some(r) = old {
            swapped.insert(ino, r);
        }
        return Err(errno(&e));
    }
    #[cfg(test)]
    if let Some(pause) = p.swap_pause {
        std::thread::sleep(pause);
    }
    Ok(new)
}

/// The socket a call naming `to` (routed `route`) goes on — the caller's own (`Session`, and tier C's for
/// this machine), else as `swapped_for` makes it — or its refusal, said.
fn routed_socket(
    l: &Listener,
    n: &libc::seccomp_notif,
    p: &Served,
    c: &Caller,
    (fd, sock): (u64, OwnedFd),
    route: Route,
    to: SocketAddr,
) -> Result<OwnedFd, i32> {
    match route {
        Route::Session | Route::Redirected => {
            // (one of tarae's put in the session's place can't come back to it)
            if p.swapped.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&inode(&sock)?) {
                let why =
                    format!("remote:{to} on a socket already put out of the session — it can't come back");
                return refuse_with(p, c, libc::EADDRNOTAVAIL, "network-bound", why).map(|_| sock);
            }
            // (nor one handed in from outside it: the call would be made in that one's namespace)
            if !in_session(c, &sock) {
                let why = format!("remote:{to} on a socket from outside the session");
                return refuse(p, c, "network-bound", why).map(|_| sock);
            }
            // (beyond: the session's rules take it to a planted socket, which reads whose it is by its mark)
            if route == Route::Redirected {
                marked(p, c, &sock)?;
            }
            Ok(sock)
        }
        Route::Machine | Route::OwnAddress => swapped_for(l, n, p, c, (fd, sock), route, to),
        Route::Refused(Refusal::Closed) => {
            refuse(p, c, "network-outbound", format!("remote:{to}")).map(|_| sock)
        }
        Route::Refused(Refusal::Never) => refuse(p, c, "network-never", format!("remote:{to}")).map(|_| sock),
        // (as where the session's own loopback has nothing there: refused — the report says whose it is)
        Route::Refused(Refusal::Another) => {
            refuse_with(p, c, libc::ECONNREFUSED, super::SESSION_OP, format!("remote:{to}")).map(|_| sock)
        }
        Route::Refused(Refusal::Untold) => {
            refuse_with(p, c, libc::ECONNREFUSED, super::UNTOLD_OP, format!("remote:{to}")).map(|_| sock)
        }
        Route::Refused(Refusal::Direct) => {
            let resolver =
                |r: &SocketAddr| r.ip().to_canonical() == to.ip().to_canonical() && r.port() == to.port();
            let lookup = p.resolvers.iter().any(resolver);
            let why = match lookup {
                true => {
                    " — a name lookup: with the network through tarae's proxy, names are looked up by the proxy"
                }
                false => "",
            };
            refuse(p, c, "network-direct", format!("remote:{to}{why}")).map(|_| sock)
        }
        // (a datagram or a send naming the proxy's address: only a connection goes to it — `connect`)
        Route::Proxy => {
            let why = format!("remote:{to} — a send, not a connection: only a connection goes to the proxy");
            refuse(p, c, "network-direct", why).map(|_| sock)
        }
    }
}

/// The run's socket `sock`, about to go beyond the session (`Route::Redirected`), marked as its run's (6d, `marks.rs`):
/// the caller's run and program's mark, given at its first such call — unless the socket carries one already (it
/// was marked at an earlier call, or another run's process connected it first: a run can't set or clear a mark, so
/// any it carries is tarae's), kept as it is. `SO_MARK` needs `CAP_NET_ADMIN` over the socket's netns — tarae's
/// over the session's. No marks left (past 2³¹ in a session): refused, said.
fn marked(p: &Served, c: &Caller, sock: &OwnedFd) -> Result<(), i32> {
    let Some(marks) = &p.marks else { return Ok(()) };
    if sockopt(sock, libc::SO_MARK)? != 0 {
        return Ok(());
    }
    let owner = super::marks::Owner {
        run: p.run,
        session: p.session,
        process: c.name(),
        rules: p.egress.clone(),
        told: Arc::downgrade(&p.refused),
    };
    let Some(mark) = marks.mark(owner) else {
        return refuse(
            p,
            c,
            "network-outbound",
            "the session's marks ran out — a new session has its own".into(),
        )
        .map(drop);
    };
    // SAFETY: an int, ours for the call, on a socket we hold.
    let r = unsafe {
        libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_MARK, (&raw const mark).cast(), 4)
    };
    result(r as isize).map(drop)
}

/// A connect to the egress proxy's address (`Route::Proxy`): a loopback TCP pair of tarae's (`proxy::pair`) —
/// its run's end carrying what the run set on its own socket (as `swapped_for` does), put in the run's place at
/// the same descriptor (`SECCOMP_ADDFD_FLAG_SETFD`), recorded as tarae's before the run holds it; the proxy's end
/// handed to the proxy in a message with the run's rules (D23). The run's connect answers 0: connected — its
/// `O_NONBLOCK` as it had it. TCP only; a socket the run bound (its source can't move), one connected already,
/// or one of tarae's put there before isn't taken.
fn proxied(
    l: &Listener,
    n: &libc::seccomp_notif,
    p: &Served,
    c: &Caller,
    (fd, sock): (u64, OwnedFd),
    to: SocketAddr,
) -> Answer {
    let (domain, kind) = (sockopt(&sock, libc::SO_DOMAIN)?, sockopt(&sock, libc::SO_TYPE)?);
    if (kind, sockopt(&sock, libc::SO_PROTOCOL)?) != (libc::SOCK_STREAM, libc::IPPROTO_TCP) {
        return refuse(
            p,
            c,
            "network-direct",
            format!("remote:{to} — only a TCP connection goes to the proxy"),
        );
    }
    let ino = inode(&sock)?;
    let mut peer = [0u8; std::mem::size_of::<libc::sockaddr_storage>()];
    let mut len = peer.len() as libc::socklen_t;
    // SAFETY: a sockaddr buffer and its length for the call to fill.
    let connected = unsafe { libc::getpeername(sock.as_raw_fd(), peer.as_mut_ptr().cast(), &mut len) } == 0;
    if connected || p.swapped.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&ino) {
        return Err(libc::EISCONN);
    }
    let mut me = [0u8; std::mem::size_of::<libc::sockaddr_storage>()];
    let mut len = me.len() as libc::socklen_t;
    // SAFETY: as above.
    if unsafe { libc::getsockname(sock.as_raw_fd(), me.as_mut_ptr().cast(), &mut len) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    let any = p.any_port.lock().unwrap_or_else(|e| e.into_inner()).contains(&ino);
    if matches!(parse(&me[..(len as usize).min(me.len())]), Ok(Addr::Ip(a)) if a.port() != 0) && !any {
        let why = format!("remote:{to} from a socket it bound itself — a bound socket stays in the session");
        return refuse_with(p, c, libc::EADDRNOTAVAIL, "network-bound", why);
    }
    // SAFETY: plain call on our own descriptor.
    let nonblock = unsafe { libc::fcntl(sock.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK != 0;
    let ready = |new: &OwnedFd| carry(&sock, new, domain, kind).map_err(io::Error::from_raw_os_error);
    let (run_end, proxy_end) = super::proxy::pair(domain, ready, |_| {}).map_err(|e| errno(&e))?;
    if nonblock {
        // SAFETY: plain calls on our own descriptor.
        unsafe {
            let flags = libc::fcntl(run_end.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(run_end.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let at = u32::try_from(fd).map_err(|_| libc::EBADF)?;
    // Known as tarae's before the run holds it (as `swapped_for`)
    let new_ino = inode(&run_end)?;
    p.keep(&run_end)?;
    p.swapped.lock().unwrap_or_else(|e| e.into_inner()).insert(new_ino, Route::Proxy);
    if let Err(e) = l.add_fd(n.id, &run_end, Some(at), cloexec(c, fd)) {
        p.swapped.lock().unwrap_or_else(|e| e.into_inner()).remove(&new_ino);
        return Err(errno(&e));
    }
    let to_proxy = super::proxy::Connection {
        fd: proxy_end.into(),
        run: p.run,
        session: p.session,
        process: c.name(),
        rules: p.egress.clone(),
        told: (*p.refused).clone(),
    };
    // (the proxy gone, the run's end reads its end at once: nothing goes out)
    super::proxy::get().send(super::proxy::ToProxy::Connection(to_proxy));
    Ok(0)
}

/// A pty call its filter handed over (3b, `sandbox::ptys`: `TIOCSPTLCK`, `TIOCGPTN`, `TIOCGPTPEER`) on the run's
/// descriptor. A master (`/dev/ptmx`'s; `pts/ptmx`'s through a container's link) whose pty isn't one of its pool's
/// is one the run just made: a pty of the pool goes in its place (`SETFD` closes the run's) — tarae's master's open
/// file. Then the call is answered for the pool's pty: its number written where the run asked for it; unlocked
/// (locking again: `EINVAL`); its other end opened by tarae and put among the run's descriptors. Decided on what
/// `/proc` says of the descriptor, read before the call is checked to still wait — nothing in the run's memory
/// decides. Not a master: `ENOTTY`, as the kernel says it (those three are a master's). Every pty of the pool held
/// by the run (`Ptys::take` looks): `EACCES`, said.
fn pty_ioctl(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let args = n.data.args;
    let Some(ptys) = &p.ptys else { return Err(libc::ENOTTY) };
    // (an ioctl's descriptor and request are `unsigned int`s: the low halves are what the kernel reads)
    let (fd, request, arg) = (u64::from(args[0] as u32), args[1] as u32, args[2]);
    let c = Caller::open(l, n)?;
    let master = matches!(c.link(fd)?.as_str(), "/dev/ptmx" | "/dev/pts/ptmx");
    let info = std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", c.tid)).map_err(|_| libc::EBADF)?;
    let index = super::ptys::tty_index(&info).filter(|_| master).ok_or(libc::ENOTTY)?;
    l.valid(n.id).map_err(|_| libc::ESRCH)?;
    let at = match ptys.known(index) {
        true => index,
        false => {
            let Some(at) = ptys.take(p.tree) else {
                return refuse_with(p, &c, libc::EACCES, "pty-make", super::ptys::POOL.to_string());
            };
            let put = ptys.with_master(at, |m| l.add_fd(n.id, m, Some(fd as u32), cloexec(&c, fd)));
            match put {
                Some(Ok(_)) => ptys.handed_over(at),
                put => {
                    ptys.give_back(at);
                    return Err(put.and_then(|r| r.err()).map_or(libc::EIO, |e| errno(&e)));
                }
            }
            at
        }
    };
    if request == libc::TIOCGPTN as u32 {
        c.write(arg, &at.to_ne_bytes()).map(|()| 0)
    } else if request == libc::TIOCSPTLCK as u32 {
        match c.read(arg, 4)?.as_slice() {
            [0, 0, 0, 0] => ptys.unlock(at).map(|()| 0).map_err(|e| errno(&e)),
            _ => Err(libc::EINVAL),
        }
    } else if request == libc::TIOCGPTPEER as u32 {
        let peer = ptys.peer(at, arg as i32).map_err(|e| errno(&e))?;
        l.add_fd(n.id, &peer, None, arg as i32 & libc::O_CLOEXEC != 0).map_err(|e| errno(&e))
    } else {
        Err(libc::ENOTTY)
    }
}

/// `socket()` for an internet family: a stream or datagram socket (TCP, UDP — not raw, ICMP's, SCTP or
/// MPTCP, none of a build's), made by tarae, bound to the loopback device, and put among the caller's
/// descriptors. By copies (6f′) its unix stream and netlink routing sockets too (what the filter lets a run make
/// — `seccomp::Mode::Copied`), each one's copy kept before the run holds it.
fn socket(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let [family, kind, protocol] = [0, 1, 2].map(|i| n.data.args[i] as i32);
    let flags = kind & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC);
    let base = kind & !flags;
    let copied = p.copies.is_some();
    let ok = match (family, base) {
        (libc::AF_INET | libc::AF_INET6, libc::SOCK_STREAM) => matches!(protocol, 0 | libc::IPPROTO_TCP),
        (libc::AF_INET | libc::AF_INET6, libc::SOCK_DGRAM) => matches!(protocol, 0 | libc::IPPROTO_UDP),
        (libc::AF_UNIX, libc::SOCK_STREAM) => copied && protocol == 0,
        (libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_DGRAM) => copied && protocol == libc::NETLINK_ROUTE,
        _ => false,
    };
    if !ok {
        return Err(libc::EPERM);
    }
    let own = base | libc::SOCK_CLOEXEC | (flags & libc::SOCK_NONBLOCK);
    // SAFETY: plain call; a descriptor it makes is ours.
    let fd = unsafe { libc::socket(family, own, protocol) };
    if fd < 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: just made.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if matches!(family, libc::AF_INET | libc::AF_INET6) {
        let lo = b"lo\0";
        // SAFETY: the device's name and its length, live for the call.
        let r = unsafe {
            libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, lo.as_ptr().cast(), 3)
        };
        result(r as isize)?;
    }
    p.keep(&fd)?;
    p.owned(&fd);
    l.add_fd(n.id, &fd, None, flags & libc::SOCK_CLOEXEC != 0).map_err(|e| errno(&e))
}

/// `socketpair(AF_UNIX, kind, 0, sv)` by copies (6f′: the filter hands it to tarae only then): the pair made by
/// tarae, each end's copy kept, both put among the caller's descriptors and their numbers written where it asked.
fn socketpair(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let [family, kind, protocol] = [0, 1, 2].map(|i| n.data.args[i] as i32);
    let flags = kind & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC);
    let base = kind & !flags;
    if family != libc::AF_UNIX
        || !matches!(base, libc::SOCK_STREAM | libc::SOCK_DGRAM | libc::SOCK_SEQPACKET)
        || protocol != 0
    {
        return Err(libc::EAFNOSUPPORT);
    }
    let c = Caller::open(l, n)?;
    let mut fds = [0; 2];
    let own = base | libc::SOCK_CLOEXEC | (flags & libc::SOCK_NONBLOCK);
    // SAFETY: a two-int array the call fills; the descriptors it makes are ours.
    if unsafe { libc::socketpair(family, own, 0, fds.as_mut_ptr()) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: just made.
    let ends = unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] };
    let cloexec = flags & libc::SOCK_CLOEXEC != 0;
    let mut theirs = [0i32; 2];
    for (end, at) in ends.iter().zip(&mut theirs) {
        p.keep(end)?;
        *at = l.add_fd(n.id, end, None, cloexec).map_err(|e| errno(&e))? as i32;
    }
    let mut out = theirs[0].to_ne_bytes().to_vec();
    out.extend(theirs[1].to_ne_bytes());
    c.write(n.data.args[3], &out)?;
    Ok(0)
}

/// How long `accept` waits in one go on a listener that blocks, before it looks for a signal for the caller.
const ACCEPT_SLICE: i32 = 100;

/// The kernel's "interrupted — restart the call if the handler says so" (`include/linux/errno.h`, not uapi): a
/// call that returns it with a signal pending is restarted after a handler with `SA_RESTART` (re-entering the filter:
/// tarae serves it anew), and fails `EINTR` after one without. Only ever answered where the caller is sure to have
/// the signal to take (`Pending::Its`): with none, it would reach the program as error 512.
const ERESTARTSYS: i32 = 512;

/// A signal waiting for a caller tarae waits for (`/proc/<tid>/status`: `SigPnd`, `ShdPnd`, `SigBlk`, `Threads`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    /// One the kernel woke this thread for: sent to it (`SigPnd`), or to its process where it's the only thread.
    Its,
    /// Its process's (`ShdPnd`, these signals) with other threads there — any of them that doesn't block it may be
    /// the one the kernel woke.
    Shared(u64),
}

/// `Pending` from a thread's `/proc/<tid>/status`; signals it blocks don't count.
fn pending(status: &str) -> Pending {
    let field = |key: &str| status.lines().find_map(|l| l.strip_prefix(key)).map(str::trim);
    let mask = |key: &str| field(key).and_then(|v| u64::from_str_radix(v, 16).ok()).unwrap_or(0);
    let open = !mask("SigBlk:");
    let (own, shared) = (mask("SigPnd:") & open, mask("ShdPnd:") & open);
    let alone = field("Threads:").is_some_and(|t| t == "1");
    match (own, shared) {
        (0, 0) => Pending::None,
        (0, s) if !alone => Pending::Shared(s),
        _ => Pending::Its,
    }
}

/// What a wait for the caller ends with when a signal interrupts it, as the kernel's socket waits answer
/// (`sock_intr_errno`): `EINTR` where the socket has a timeout of its own (`SO_RCVTIMEO`: never restarted), else
/// `ERESTARTSYS` — `EINTR` or restarted, as the caller's handler has it.
fn interrupted(timed: bool) -> i32 {
    if timed { libc::EINTR } else { ERESTARTSYS }
}

/// A wait's slice over with `pending` for its caller (`timed`: the socket has its own timeout), the process's
/// signals pending at the last slice's end being `before`: wait on (Ok, the process's signals pending now) or end
/// (the errno). Its own → `interrupted`. The process's, with other threads: one of them may take it — wait a slice;
/// still pending then (the same signal), `EINTR`: tarae can't tell whether the kernel woke this thread for it.
fn after_slice(pending: Pending, before: u64, timed: bool) -> Result<u64, i32> {
    match pending {
        Pending::Its => Err(interrupted(timed)),
        Pending::Shared(s) if s & before != 0 => Err(libc::EINTR),
        Pending::Shared(s) => Ok(s),
        Pending::None => Ok(0),
    }
}

/// A socket's `SO_RCVTIMEO` — None: none set (a blocking call waits for good).
fn receive_timeout(fd: &OwnedFd) -> Result<Option<std::time::Duration>, i32> {
    // SAFETY: an all-integer struct, zeroed, and its length for the call to fill.
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
    // SAFETY: the struct and its length, live for the call.
    let r = unsafe {
        libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw mut tv).cast(), &mut len)
    };
    result(r as isize)?;
    let d = std::time::Duration::from_secs(tv.tv_sec as u64)
        + std::time::Duration::from_micros(tv.tv_usec as u64);
    Ok((!d.is_zero()).then_some(d))
}

/// `accept(fd, addr, len)` / `accept4(…, flags)` by copies (6f′): tarae accepts on its copy of the run's listener —
/// waiting as the caller would (the same open socket's `O_NONBLOCK`, its `SO_RCVTIMEO`), in slices that look for a
/// signal for it (a server waiting on its listener for a shutdown signal, a forking one's `SIGCHLD`) — the
/// connection's copy kept, put among the caller's descriptors (its `SOCK_CLOEXEC`, `SOCK_NONBLOCK`), its peer's
/// address written where it asked.
///
/// A signal ends the wait as the kernel's would where tarae can tell (`Pending::Its`): `ERESTARTSYS`
/// (`interrupted`), so a handler with `SA_RESTART` sees its accept go on and one without sees `EINTR` — the
/// kernel decides, from the handler. Where it can't — a signal sent to the process, with other threads that could
/// be the one woken — it waits one slice more (one of them takes it: the accept goes on, as the kernel's would);
/// still pending then, it ends with `EINTR` whatever the handler says (a spurious `EINTR` after an `SA_RESTART`
/// handler, never error 512 reaching a thread with no signal to take).
fn accept(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let flags = if n.data.nr as libc::c_long == libc::SYS_accept4 { args[3] as i32 } else { 0 };
    if flags & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) != 0 {
        return Err(libc::EINVAL);
    }
    let listener = p.sock(&c, args[0])?;
    // SAFETY: plain call on our own descriptor.
    let blocking = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK == 0;
    // (its `SO_RCVTIMEO`, as the kernel's accept takes it: `EAGAIN` once it's past)
    let by = receive_timeout(&listener)?.map(|d| std::time::Instant::now() + d);
    // SAFETY: a zeroed address of the largest size, filled by the call.
    let mut peer: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // (the process's signals pending at the last slice's end)
    let mut shared = 0;
    let fd = loop {
        if blocking {
            let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one live pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, ACCEPT_SLICE) };
            if r <= 0 {
                // (gone: the caller's call ended — interrupted before 5.19's killable wait; nobody reads this)
                if l.valid(n.id).is_err() {
                    return Err(libc::EINTR);
                }
                shared = after_slice(c.pending(), shared, by.is_some())?;
                if by.is_some_and(|by| std::time::Instant::now() >= by) {
                    return Err(libc::EAGAIN);
                }
                continue;
            }
        }
        let own = libc::SOCK_CLOEXEC | (flags & libc::SOCK_NONBLOCK);
        // SAFETY: our listener, an address buffer and its length for the call to fill.
        let fd = unsafe { libc::accept4(listener.as_raw_fd(), (&raw mut peer).cast(), &mut len, own) };
        match fd {
            fd if fd >= 0 => break fd,
            _ => match errno(&io::Error::last_os_error()) {
                // (another thread's accept took it: wait again)
                libc::EAGAIN | libc::EINTR if blocking => continue,
                e => return Err(e),
            },
        }
    };
    // SAFETY: just made.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if args[1] != 0 && args[2] != 0 {
        let room = u32::from_ne_bytes(c.read(args[2], 4)?.try_into().unwrap_or_default()) as usize;
        let said = (len as usize).min(std::mem::size_of_val(&peer));
        // SAFETY: the address the call filled, `said` bytes of it.
        let bytes = unsafe { std::slice::from_raw_parts((&raw const peer).cast::<u8>(), said) };
        c.write(args[1], &bytes[..said.min(room)])?;
        c.write(args[2], &(said as u32).to_ne_bytes())?;
    }
    p.keep(&fd)?;
    l.add_fd(n.id, &fd, None, flags & libc::SOCK_CLOEXEC != 0).map_err(|e| errno(&e))
}

/// `path` (absolute, real) opened without following any link on the way (`landlock::open_unlinked`) —
/// `O_PATH`, a folder only if `folder`. A link put there since `canonicalize` saw it: `ELOOP`, nothing reached.
fn open_real(path: &Path, folder: bool) -> Result<OwnedFd, i32> {
    let dir = if folder { libc::O_DIRECTORY } else { 0 };
    crate::landlock::open_unlinked(path, libc::O_PATH | dir).map_err(|e| errno(&e))
}

/// A path the caller named, absolute (a relative one from its folder).
fn absolute(c: &Caller, named: &Path) -> Result<PathBuf, i32> {
    Ok(if named.is_absolute() { named.to_path_buf() } else { c.cwd()?.join(named) })
}

/// Whether `real` is where a run's unix sockets may be: the project, the run's own folder.
fn socket_place(p: &Policy, real: &Path) -> bool {
    real.starts_with(&p.root) || p.run.as_ref().is_some_and(|r| real.starts_with(r.path()))
}

/// The unix socket a path the caller named reaches, if it may: one in the project or the run's own folder,
/// or one the user granted (that file, by its real path) — opened (`open_real`), a socket.
/// Refused: said, `EPERM`. The descriptor holds it while its address (`unix_address`) is used.
fn unix_target(p: &Served, c: &Caller, named: &Path) -> Result<(OwnedFd, Vec<u8>), i32> {
    let abs = absolute(c, named)?;
    let real = std::fs::canonicalize(&abs).map_err(|e| errno(&e))?;
    if !socket_place(p, &real) && !p.sockets.contains(&real) {
        refuse(p, c, "network-outbound", real.display().to_string())?;
    }
    let fd = open_real(&real, false)?;
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: an O_PATH descriptor of ours and a stat buffer the call fills.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    // SAFETY: filled by the successful fstat above.
    if unsafe { st.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(libc::ECONNREFUSED); // (what the kernel says of a file that isn't a socket)
    }
    let a = unix_address(format!("/proc/self/fd/{}", fd.as_raw_fd()).as_bytes())?;
    Ok((fd, a))
}

/// `connect(fd, addr, len)`: where `destination` says, waiting as the caller would — an internet socket's
/// connect non-blocking on one put in its place answers `EINPROGRESS` as the run's own would (the same open
/// socket's `O_NONBLOCK`).
fn connect(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let addr = address(&c, args[1], args[2])?;
    let sock = p.sock(&c, args[0])?;
    // (the proxy's address: connected here, through a pair of tarae's — `proxied`)
    if let (libc::AF_INET | libc::AF_INET6, Ok(Addr::Ip(to))) =
        (sockopt(&sock, libc::SO_DOMAIN)?, parse(&addr))
        && routed(p, &c, to, sockopt(&sock, libc::SO_TYPE)?) == Route::Proxy
    {
        return proxied(l, n, p, &c, (args[0], sock), to);
    }
    let (sock, to, _held) = destination(l, n, p, &c, (args[0], sock), addr)?;
    // SAFETY: the socket and an address of ours, live for the call.
    let r = unsafe { libc::connect(sock.as_raw_fd(), to.as_ptr().cast(), to.len() as libc::socklen_t) };
    result(r as isize)
}

/// `bind(fd, addr, len)`: an internet socket on this machine (any port — a test's own server; in the
/// session's netns, the session's), a unix one in the project or the run's own folder where a run may write
/// (`Policy::writes_allow`: not a protected name), netlink as asked. Abstract unix names: the session's own in
/// its netns (`Routed`), else refused. Never a socket of tarae's put in the run's place: it's in tarae's
/// namespace, where a bind would listen on this machine.
fn bind(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let addr = address(&c, args[1], args[2])?;
    let sock = p.sock(&c, args[0])?;
    if p.swapped.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&inode(&sock)?) {
        let at = match parse(&addr) {
            Ok(Addr::Ip(a)) => format!("local:{a}"),
            _ => "a socket tarae put out of the session".into(),
        };
        return refuse_with(p, &c, libc::EINVAL, "network-bind", at);
    }
    // (in the session's netns a bind is the session's — never on a socket handed in from outside it)
    if p.netns() && !in_session(&c, &sock) {
        return refuse(p, &c, "network-bind", "a socket from outside the session".into());
    }
    let (to, _held) = match (sockopt(&sock, libc::SO_DOMAIN)?, parse(&addr)?) {
        // (`AF_UNSPEC` is taken for every address only — the kernel's compatibility rule)
        (libc::AF_INET | libc::AF_INET6, Addr::Unspec) => (addr, None),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) if super::route::this_machine(a.ip()) => (addr, None),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) => {
            return refuse(p, &c, "network-bind", format!("local:{a}"));
        }
        (libc::AF_INET | libc::AF_INET6, _) => return Err(libc::EAFNOSUPPORT),
        (libc::AF_UNIX, Addr::Path(named)) => {
            let abs = absolute(&c, &named)?;
            let name = abs.file_name().ok_or(libc::EINVAL)?.to_os_string();
            let parent = std::fs::canonicalize(abs.parent().ok_or(libc::EINVAL)?).map_err(|e| errno(&e))?;
            let real = parent.join(&name);
            if !socket_place(p, &real) || !p.writes_allow(&real) {
                return refuse(p, &c, "network-bind", real.display().to_string());
            }
            let dir = open_real(&parent, true)?;
            let a = named_in(dir.as_raw_fd(), &name)?;
            (a, Some(dir))
        }
        // (D-f: in the session's netns an abstract name is the session's)
        (libc::AF_UNIX, Addr::Abstract(_)) if p.netns() => (addr, None),
        (libc::AF_UNIX, Addr::Abstract(name)) => {
            return refuse(p, &c, "network-bind", format!("@{}", String::from_utf8_lossy(&name)));
        }
        (libc::AF_UNIX, _) => return refuse(p, &c, "network-bind", "an unnamed unix socket".into()),
        (libc::AF_NETLINK, Addr::Netlink) => {
            netlink_route(p, &c, &sock)?;
            let (_, groups) = netlink_address(&addr)?;
            // (rtnetlink asks these groups' listeners for `CAP_NET_ADMIN` — whoever binds: tarae —
            // `rtnetlink_bind`; a group is bit `group − 1`)
            let gated = (1 << (libc::RTNLGRP_IPV4_MROUTE_R - 1)) | (1 << (libc::RTNLGRP_IPV6_MROUTE_R - 1));
            if groups & gated != 0 {
                return refuse(p, &c, "network-netlink", format!("joining groups {:#x}", groups & gated));
            }
            (addr, None)
        }
        // (as the kernel reads it: `EINVAL`)
        (libc::AF_NETLINK, _) => (addr, None),
        _ => return Err(libc::EPERM),
    };
    // (step 8: where sessions share this machine's loopback, never onto another session's port — one named, checked
    // here; one the kernel picks, picked as no one's: `alone`)
    let kind = sockopt(&sock, libc::SO_TYPE)?;
    let asked = match p.makes_sockets() {
        true => asked_at(&sock, &to)?,
        false => None,
    };
    if let Some(at) = asked
        && at.port() != 0
    {
        refuse_joining(p, &c, (at, v6only(&sock)?), kind)?;
    }
    // SAFETY: the socket and an address of ours, live for the call.
    let bound = || {
        result(
            unsafe { libc::bind(sock.as_raw_fd(), to.as_ptr().cast(), to.len() as libc::socklen_t) } as isize
        )
    };
    let r = match asked {
        Some(at) if at.port() == 0 && kind == libc::SOCK_DGRAM => {
            let unturned = |why| refuse_with(p, &c, libc::EADDRINUSE, "network-bind", why);
            picked(p, &c, &sock, || copied_again(p, || alone(&sock, bound, unturned)))
        }
        _ => copied_again(p, bound),
    };
    if r.is_ok() && matches!(parse(&to), Ok(Addr::Ip(a)) if a.ip().is_unspecified() && a.port() == 0) {
        p.any_port.lock().unwrap_or_else(|e| e.into_inner()).insert(inode(&sock)?);
    }
    if r.is_ok() {
        p.owned(&sock);
    }
    r
}

/// `listen(fd, backlog)`: the caller's own sockets as they are — never one of tarae's put in the run's place.
/// An unbound one (its connect failed, or a datagram one sent nothing yet) the kernel would bind to every address
/// of tarae's namespace — listening on this machine's network, beyond the session — at a port of its picking.
fn listen(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let sock = p.sock(&c, n.data.args[0])?;
    if p.swapped.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&inode(&sock)?) {
        return refuse_with(
            p,
            &c,
            libc::EINVAL,
            "network-bind",
            "a socket tarae put out of the session".into(),
        );
    }
    // (nor one handed in from outside the session's netns: it would listen in that one's)
    if p.netns() && !in_session(&c, &sock) {
        return refuse(p, &c, "network-bind", "a socket from outside the session".into());
    }
    // (step 8: a TCP socket bound beside another session's before either listened joins it here — the kernel lists
    // a bound one only once it listens, so the bind couldn't tell)
    if p.makes_sockets()
        && let Some(at) = local_addr(&sock)
    {
        refuse_joining(p, &c, (at, v6only(&sock)?), sockopt(&sock, libc::SO_TYPE)?)?;
    }
    // SAFETY: plain call on the caller's socket, ours through `pidfd_getfd`.
    let r = result(unsafe { libc::listen(sock.as_raw_fd(), n.data.args[1] as i32) } as isize);
    // (an unbound one bound itself here)
    if r.is_ok() {
        p.owned(&sock);
    }
    r
}

/// Where a connect or a send naming `addr` (read once) goes, from the caller's descriptor `fd` (`sock`): the
/// socket to make the call on — the caller's, or one of tarae's put in its place (`routed_socket`, as
/// `route` says for an internet address) — the address — tarae's copy, or a unix socket's through tarae's own
/// descriptor (held with it: `unix_target`) — or its refusal, said. An abstract unix name is the session's in
/// its netns (D-f), else refused.
fn destination(
    l: &Listener,
    n: &libc::seccomp_notif,
    p: &Served,
    c: &Caller,
    (fd, sock): (u64, OwnedFd),
    addr: Vec<u8>,
) -> Result<(OwnedFd, Vec<u8>, Option<OwnedFd>), i32> {
    let domain = sockopt(&sock, libc::SO_DOMAIN)?;
    // (where sessions share a loopback: a socket still unbound binds itself on this call — a UDP socket's first send
    // or connect, a disconnect's after — so it's touched here, a row again if a sweep took it: `owners`. One bound is
    // in the kernel's list, which a sweep keeps: nothing to touch)
    let inet = matches!(domain, libc::AF_INET | libc::AF_INET6);
    if inet && p.kept_apart() && local_addr(&sock).is_none() {
        p.owned(&sock);
    }
    match (domain, parse(&addr)?) {
        (libc::AF_INET | libc::AF_INET6, Addr::Unspec) => Ok((sock, addr, None)),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) => {
            let kind = sockopt(&sock, libc::SO_TYPE)?;
            let route = routed(p, c, a, kind);
            let on = routed_socket(l, n, p, c, (fd, sock), route, a)?;
            if kind == libc::SOCK_DGRAM {
                bound_alone(p, c, &on)?;
            }
            Ok((on, addr, None))
        }
        (libc::AF_INET | libc::AF_INET6, _) => Err(libc::EAFNOSUPPORT),
        (libc::AF_UNIX, Addr::Path(named)) => {
            unix_target(p, c, &named).map(|(held, a)| (sock, a, Some(held)))
        }
        // (the session's — on a socket of the session's: one handed in from outside reaches this machine's)
        (libc::AF_UNIX, Addr::Abstract(_)) if p.netns() && in_session(c, &sock) => Ok((sock, addr, None)),
        (libc::AF_UNIX, Addr::Abstract(name)) => {
            let target = format!("@{}", String::from_utf8_lossy(&name));
            refuse(p, c, "network-outbound", target).map(|_| (sock, Vec::new(), None))
        }
        (libc::AF_UNIX, Addr::Unspec) => Ok((sock, addr, None)),
        (libc::AF_UNIX, _) => Err(libc::EINVAL),
        // (the kernel's address only: any other port or group is reached with the capability of whoever makes
        // the call — `netlink_allowed`, tarae's — and a connect to one holds for the run's own sends after)
        (libc::AF_NETLINK, Addr::Netlink) => {
            netlink_route(p, c, &sock)?;
            match netlink_address(&addr)? {
                (0, 0) => Ok((sock, addr, None)),
                (port, groups) => refuse(p, c, "network-netlink", format!("port {port}, groups {groups:#x}"))
                    .map(|_| (sock, addr, None)),
            }
        }
        // (a connect's disconnect; a send's `EINVAL`, as the kernel answers it)
        (libc::AF_NETLINK, _) => Ok((sock, addr, None)),
        _ => Err(libc::EPERM),
    }
}

/// A netlink socket tarae calls on for a run is the routing protocol's — the only one a run makes
/// (`seccomp::ops`); another, handed in from outside the sandbox, has rules of its own that `netlink_reads`
/// doesn't know: refused, said.
fn netlink_route(p: &Served, c: &Caller, sock: &OwnedFd) -> Result<(), i32> {
    match sockopt(sock, libc::SO_PROTOCOL)? {
        libc::NETLINK_ROUTE => Ok(()),
        n => refuse(p, c, "network-netlink", format!("protocol {n}")).map(drop),
    }
}

/// A send tarae makes for the run on `sock` carrying `data`: on a netlink socket only reads go
/// (`netlink_reads`) — anything else refused, said, `EPERM` to the run (where the kernel would ack a sender
/// without the capability with `-EPERM`: tarae can't put an ack in the run's socket, so its send fails).
fn netlink_send(p: &Served, c: &Caller, sock: &OwnedFd, data: &[u8]) -> Result<(), i32> {
    if sockopt(sock, libc::SO_DOMAIN)? != libc::AF_NETLINK {
        return Ok(());
    }
    netlink_route(p, c, sock)?;
    netlink_reads(data).or_else(|what| refuse(p, c, "network-netlink", what).map(drop))
}

/// Whether a send on a socket of `kind` (`SO_TYPE`) goes whole or not at all — every kind but a stream: a
/// datagram, a sequenced packet, a netlink batch (raw) — so one past `SEND_MAX` fails (`payload`) rather than
/// going cut short: a netlink batch cut would end mid-message.
fn whole(kind: i32) -> bool {
    kind != libc::SOCK_STREAM
}

/// What a send carries, read once: `len` bytes at `at` — at most `SEND_MAX`; a datagram past it fails.
fn payload(c: &Caller, at: u64, len: u64, datagram: bool) -> Result<Vec<u8>, i32> {
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    if datagram && len > SEND_MAX {
        return Err(libc::EMSGSIZE);
    }
    c.read(at, len.min(SEND_MAX))
}

/// `sendto(fd, buf, len, flags, addr, alen)` with an address (one without isn't tarae's).
fn sendto(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let flags = args[3] as i32;
    let addr = address(&c, args[4], args[5])?;
    let sock = p.sock(&c, args[0])?;
    let datagram = whole(sockopt(&sock, libc::SO_TYPE)?);
    let (sock, to, _held) = destination(l, n, p, &c, (args[0], sock), addr)?;
    let data = payload(&c, args[1], args[2], datagram)?;
    netlink_send(p, &c, &sock, &data)?;
    // SAFETY: the socket, the data and the address are ours, live for the call.
    let r = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            data.as_ptr().cast(),
            data.len(),
            flags | libc::MSG_NOSIGNAL,
            to.as_ptr().cast(),
            to.len() as libc::socklen_t,
        )
    };
    c.sigpipe(flags, result(r))
}

/// One message sent on the caller's descriptor `fd` (`sock`) as it asked — where it names going as a `sendto`
/// there would (`sock` then the socket it went on: one of tarae's put in the run's place, for the next).
fn send_one(
    (l, n): (&Listener, &libc::seccomp_notif),
    p: &Served,
    c: &Caller,
    (fd, sock): (u64, &mut OwnedFd),
    m: Msg,
    flags: i32,
) -> Answer {
    let (to, _held) = match m.name {
        Some(addr) => {
            let theirs = sock.try_clone().map_err(|e| errno(&e))?;
            let (on, to, held) = destination(l, n, p, c, (fd, theirs), addr)?;
            *sock = on;
            (to, held)
        }
        None => (Vec::new(), None),
    };
    netlink_send(p, c, sock, &m.data)?;
    let mut iov = libc::iovec { iov_base: m.data.as_ptr() as *mut libc::c_void, iov_len: m.data.len() };
    // SAFETY: an all-integer/pointer struct, zeroed then set.
    let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
    if !to.is_empty() {
        h.msg_name = to.as_ptr() as *mut libc::c_void;
        h.msg_namelen = to.len() as libc::socklen_t;
    }
    h.msg_iov = &mut iov;
    h.msg_iovlen = 1;
    if !m.control.is_empty() {
        h.msg_control = m.control.as_ptr() as *mut libc::c_void;
        h.msg_controllen = m.control.len() as _;
    }
    // SAFETY: the header and all it points to are ours, live for the call.
    let r = unsafe { libc::sendmsg(sock.as_raw_fd(), &h, flags | libc::MSG_NOSIGNAL) };
    if r >= 0 {
        // (by copies: what it passed is held by no process until the receiver reads it — kept meanwhile)
        p.in_flight(&m.fds, sock);
    }
    c.sigpipe(flags, result(r))
}

/// The caller's message at `at` (`read_msg`), each descriptor it passes taken as tarae's (`Served::passed`) — a
/// control message of any other type refused, said.
fn message(p: &Served, c: &Caller, at: u64, datagram: bool) -> Result<Msg, i32> {
    read_msg(c, |fd| p.passed(c, fd as u64), at, datagram).or_else(|u| match u {
        Unsent::Errno(e) => Err(e),
        Unsent::Control { level, kind } => {
            let why =
                format!("sending {} — a message passes only descriptors (SCM_RIGHTS)", named(level, kind));
            refuse(p, c, "network-bound", why).and(Err(libc::EPERM))
        }
    })
}

/// `sendmsg(fd, msg, flags)` — every one, as only the message says where it goes.
fn sendmsg(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let mut sock = p.sock(&c, args[0])?;
    let m = message(p, &c, args[1], whole(sockopt(&sock, libc::SO_TYPE)?))?;
    send_one((l, n), p, &c, (args[0], &mut sock), m, args[2] as i32)
}

/// `sendmmsg(fd, msgs, n, flags)`: each message as `sendmsg`, its length written back where the caller
/// reads it (`msg_len` — glibc's resolver checks it); the first that fails ends it (failing the call when
/// it's the first; one past the end of the address space is `EFAULT` — `mmsg_at`).
fn sendmmsg(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let mut sock = p.sock(&c, args[0])?;
    let datagram = whole(sockopt(&sock, libc::SO_TYPE)?);
    let count = args[2].min(libc::UIO_MAXIOV as u64);
    let mut sent = 0;
    for i in 0..count {
        let r = mmsg_at(args[1], i).ok_or(libc::EFAULT).and_then(|at| {
            let m = message(p, &c, at, datagram)?;
            Ok((at, send_one((l, n), p, &c, (args[0], &mut sock), m, args[3] as i32)?))
        });
        match r {
            Ok((at, len)) => {
                c.write(at + MSG_LEN_AT, &(len as u32).to_ne_bytes())?;
                sent += 1;
            }
            Err(e) if sent == 0 => return Err(e),
            Err(_) => break,
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 8: a call's lookup where the kernel's list of a port's sockets can't be had falls back to tarae's
    /// `/proc/net` tables — another session's listener (this test's, known by its inode only, as those tables name
    /// it) found there, its own session's not —; and where neither can be read whose the socket is can't be told —
    /// an Err, refused as such (`Refusal::Untold`), its own session's too, never said to be another session's.
    #[test]
    fn a_lookup_falls_back_to_the_tables_then_refuses() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let to = l.local_addr().unwrap();
        let fd = OwnedFd::from(l.try_clone().unwrap());
        let owners = super::super::owners::Owners::with_cap(super::super::owners::KEPT);
        owners.made((cookie(&fd).unwrap(), inode(&fd).unwrap()), 7, 1, || None);
        let unlisted = || Err(io::Error::other("no sock_diag here"));
        let unread = |_: &str| Err(io::Error::other("no /proc/net here"));
        assert_eq!(
            anothers_by(&owners, to, (true, 8), unlisted, proc_net),
            Ok(true),
            "another's, by the tables"
        );
        assert_eq!(
            anothers_by(&owners, to, (true, 7), unlisted, proc_net),
            Ok(false),
            "its own, by the tables"
        );
        let neither = anothers_by(&owners, to, (true, 7), unlisted, unread);
        assert!(
            neither.as_ref().is_err_and(|e| e.contains("no /proc/net here")),
            "neither read: {neither:?}"
        );
    }

    /// A UDP socket's reuse options are off while the kernel picks its port and back after; where they can't be
    /// turned off the bind isn't made — what's refused says why (the report) — and what was turned off is turned
    /// back on.
    #[test]
    fn a_bind_whose_reuse_options_cant_be_turned_off_is_refused_and_said() {
        let opts = std::cell::RefCell::new(std::collections::HashMap::from([
            (libc::SO_REUSEADDR, 1),
            (libc::SO_REUSEPORT, 1),
        ]));
        let get = |o| opts.borrow()[&o];
        let set = |o, v| {
            opts.borrow_mut().insert(o, v);
            Ok(())
        };
        let bound_with = std::cell::Cell::new(None);
        let bind = || {
            bound_with.set(Some((get(libc::SO_REUSEADDR), get(libc::SO_REUSEPORT))));
            Ok(0)
        };
        assert_eq!(alone_with(get, set, bind, |_| panic!("not refused")), Ok(0));
        assert_eq!(bound_with.get(), Some((0, 0)), "bound with both off");
        assert_eq!((get(libc::SO_REUSEADDR), get(libc::SO_REUSEPORT)), (1, 1), "both back after");
        // (SO_REUSEPORT can't be turned off)
        bound_with.set(None);
        let stuck = |o, v| match (o, v) {
            (libc::SO_REUSEPORT, 0) => Err(io::Error::from_raw_os_error(libc::EPERM)),
            _ => set(o, v),
        };
        let said = std::cell::RefCell::new(String::new());
        let refused = |why: String| {
            *said.borrow_mut() = why;
            Err(libc::EADDRINUSE)
        };
        assert_eq!(alone_with(get, stuck, bind, refused), Err(libc::EADDRINUSE));
        assert_eq!(bound_with.get(), None, "not bound");
        assert!(said.borrow().contains("couldn't be turned off"), "said: {}", said.borrow());
        assert_eq!((get(libc::SO_REUSEADDR), get(libc::SO_REUSEPORT)), (1, 1), "turned back on");
    }

    /// The probe (6f′): a listener and `pidfd_getfd` working → served on taken descriptors; `pidfd_getfd` refused
    /// (Docker's default filter) → served by copies, not "can't serve"; no listener (inside another supervisor's
    /// filter), no pidfd at all, or Yama refusing → can't serve, said — by copies too.
    #[test]
    fn the_probe_serves_by_copies_where_pidfd_getfd_is_refused() {
        let refused = || Err(io::Error::from_raw_os_error(libc::EPERM));
        assert_eq!(decided(Ok(()), Ok(Ok(())), Ok(())), Ok(Serving::Taken));
        assert_eq!(decided(Ok(()), Ok(refused()), Ok(())), Ok(Serving::Copies));
        let busy = Err(io::Error::from_raw_os_error(libc::EBUSY));
        assert!(decided(busy, Ok(refused()), Ok(())).is_err_and(|e| e.contains("another supervisor")));
        assert!(decided(Ok(()), Err("pidfd_open: no".into()), Ok(())).is_err());
        let yama = Err("Yama's ptrace_scope 3".to_string());
        assert_eq!(decided(Ok(()), Ok(refused()), yama.clone()), yama.map(|()| Serving::Copies));
    }

    /// What a signal pending for a caller tarae waits for is (`pending`): one sent to its thread, or to its process
    /// where it's the only thread, is surely the one the kernel woke it for (`Its` — `ERESTARTSYS` is safe); one sent
    /// to a process with other threads may be another thread's to take (`Shared`); blocked ones don't count. A wait
    /// on a socket with its own timeout ends `EINTR`, else `ERESTARTSYS` (`sock_intr_errno`).
    #[test]
    fn a_signal_is_the_callers_only_where_the_kernel_surely_woke_it() {
        let status = |own: &str, shared: &str, blocked: &str, threads: u32| {
            format!("Threads:\t{threads}\nSigPnd:\t{own}\nShdPnd:\t{shared}\nSigBlk:\t{blocked}\n")
        };
        let usr1 = "0000000000000200";
        let none = "0000000000000000";
        assert_eq!(pending(&status(none, none, none, 1)), Pending::None);
        assert_eq!(pending(&status(usr1, none, none, 3)), Pending::Its, "sent to the thread");
        assert_eq!(pending(&status(none, usr1, none, 1)), Pending::Its, "to the process, its only thread");
        assert_eq!(
            pending(&status(none, usr1, none, 2)),
            Pending::Shared(0x200),
            "another thread could take it"
        );
        assert_eq!(pending(&status(usr1, usr1, usr1, 1)), Pending::None, "blocked");
        assert_eq!((interrupted(true), interrupted(false)), (libc::EINTR, ERESTARTSYS));
    }

    /// A served wait's slice, ended with a signal pending (`after_slice`): the caller's own ends it as the kernel's
    /// socket wait would (`ERESTARTSYS`, or `EINTR` with a timeout of the socket's own); the process's, with other
    /// threads, waits one slice for one of them to take it — gone by then, the wait goes on (the kernel gave it to
    /// another thread: `by_copies_a_signal_ends_an_accept_as_the_kernels_would` sees that live) — and still pending,
    /// ends with `EINTR`, whatever the handler: the one place tarae can't tell what the kernel would do.
    #[test]
    fn a_process_signal_no_other_thread_takes_ends_the_wait_with_eintr() {
        let usr1 = 0x200;
        assert_eq!(after_slice(Pending::Its, 0, false), Err(ERESTARTSYS));
        assert_eq!(after_slice(Pending::Its, 0, true), Err(libc::EINTR));
        assert_eq!(after_slice(Pending::Shared(usr1), 0, false), Ok(usr1), "a slice for another thread");
        assert_eq!(after_slice(Pending::None, usr1, false), Ok(0), "taken elsewhere: the wait goes on");
        assert_eq!(after_slice(Pending::Shared(usr1), usr1, false), Err(libc::EINTR), "still pending");
        assert_eq!(
            after_slice(Pending::Shared(0x400), usr1, false),
            Ok(0x400),
            "another signal: its own slice"
        );
    }

    /// A run's netlink send goes to the kernel only when every message in it is a read: an `RTM_GET…` (every
    /// one `include/uapi/linux/rtnetlink.h` names; every other type it names refused — those need
    /// `CAP_NET_ADMIN`, and tarae, who sends, has it: 2026-10-07's finding), flags no more than a lookup's, the
    /// batch walked by its headers and refused when one is cut short, runs past the end or leaves bytes over.
    #[test]
    fn only_a_batch_of_reads_goes_to_the_kernel() {
        let (dump, ack) = (libc::NLM_F_REQUEST | libc::NLM_F_DUMP, libc::NLM_F_REQUEST | libc::NLM_F_ACK);
        // (the uapi's types, by their four: new, delete, get, set — 6.1 to 7.0)
        let gets = [
            18, 22, 26, 30, 34, 38, 42, 46, 50, 58, 62, 66, 74, 78, 82, 86, 90, 94, 102, 106, 110, 114, 118,
            122,
        ];
        let changes = [
            16, 17, 19, 20, 21, 24, 25, 28, 29, 32, 33, 36, 37, 40, 41, 44, 45, 48, 49, 52, 56, 57, 60, 61,
            64, 67, 68, 72, 73, 79, 80, 81, 84, 85, 88, 89, 92, 95, 96, 100, 101, 104, 105, 108, 109, 112,
            113, 116, 117, 120, 121,
        ];
        for t in gets {
            assert_eq!(netlink_reads(&nl_msg(t, dump, &[0; 16])), Ok(()), "type {t}: a read");
        }
        for t in changes {
            assert_eq!(
                netlink_reads(&nl_msg(t, ack, &[0; 16])),
                Err(format!("message type {t}")),
                "a change"
            );
        }
        let lookup = nl_msg(libc::RTM_GETADDR, dump, &[0; 8]);
        let links = nl_msg(libc::RTM_GETLINK, ack, &[0; 16]);
        let add = nl_msg(libc::RTM_NEWADDR, ack | libc::NLM_F_CREATE, &[0; 8]);
        let mut short = lookup.clone();
        short[..4].copy_from_slice(&64u32.to_ne_bytes());
        let mut under = lookup.clone();
        under[..4].copy_from_slice(&8u32.to_ne_bytes());
        let cases: [(&str, Vec<u8>, Result<(), &str>); 9] = [
            ("a lookup's dump", lookup.clone(), Ok(())),
            ("two reads in one batch", [links.clone(), lookup.clone()].concat(), Ok(())),
            ("a change after a read", [lookup.clone(), add].concat(), Err("message type 20")),
            (
                "a read's flags and more",
                nl_msg(libc::RTM_GETLINK, dump | libc::NLM_F_ECHO, &[]),
                Err("message flags 0x309"),
            ),
            ("netlink's own types (no-op)", nl_msg(1, libc::NLM_F_REQUEST, &[]), Err("message type 1")),
            ("a header cut short", lookup[..10].to_vec(), Err("a message cut short")),
            ("a length past the end", short, Err("a message cut short")),
            ("a length under a header's", under, Err("a message cut short")),
            ("bytes left over", [links, vec![0; 4]].concat(), Err("a message cut short")),
        ];
        for (what, bytes, want) in cases {
            assert_eq!(netlink_reads(&bytes), want.map_err(String::from), "{what}");
        }
        assert_eq!(netlink_reads(&[]), Err("a message cut short".into()), "nothing");
    }

    /// Addresses as the kernel lays them out — read as it reads them; too short for the family, refused.
    #[test]
    fn addresses_are_read_as_the_kernel_reads_them() {
        let v4 = |ip: [u8; 4], port: u16| {
            let mut a = (libc::AF_INET as u16).to_ne_bytes().to_vec();
            a.extend(port.to_be_bytes());
            a.extend(ip);
            a.extend([0; 8]);
            a
        };
        assert_eq!(parse(&v4([127, 0, 0, 1], 8080)), Ok(Addr::Ip("127.0.0.1:8080".parse().unwrap())));
        assert_eq!(parse(&v4([127, 0, 0, 1], 80)[..6]), Err(libc::EINVAL));
        let mut v6 = (libc::AF_INET6 as u16).to_ne_bytes().to_vec();
        v6.extend(9u16.to_be_bytes());
        v6.extend([0; 4]);
        v6.extend(std::net::Ipv6Addr::LOCALHOST.octets());
        v6.extend([0; 4]);
        assert_eq!(parse(&v6), Ok(Addr::Ip("[::1]:9".parse().unwrap())));
        let unix = |rest: &[u8]| {
            let mut a = (libc::AF_UNIX as u16).to_ne_bytes().to_vec();
            a.extend(rest);
            a
        };
        assert_eq!(parse(&unix(b"/p/s.sock\0junk")), Ok(Addr::Path("/p/s.sock".into())));
        assert_eq!(parse(&unix(b"rel.sock")), Ok(Addr::Path("rel.sock".into())));
        assert_eq!(parse(&unix(b"\0bus")), Ok(Addr::Abstract(b"bus".to_vec())));
        assert_eq!(parse(&unix(b"")), Ok(Addr::Unnamed));
        assert_eq!(parse(&[0, 0]), Ok(Addr::Unspec));
        assert_eq!(parse(&[1]), Err(libc::EINVAL));
    }

    /// Whether the session listens on a port, read from its `/proc/net` tables as the kernel writes them: a
    /// TCP row on that local port in LISTEN (`0A`) — not one connected from it — over IPv4 or IPv6; a UDP row
    /// on it in any state.
    #[test]
    fn the_sessions_listeners_are_read_from_its_tables() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                   \x20  0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1\n\
                   \x20  1: 0100007F:A1B2 0100007F:1F91 01 00000000:00000000 00:00000000 00000000  1000        0 2 1\n";
        assert!(listening_on(tcp, 0x1F90, Some("0A")), "8080, listening");
        assert!(!listening_on(tcp, 0xA1B2, Some("0A")), "a connection's own port isn't a listener");
        assert!(!listening_on(tcp, 0x1F91, Some("0A")), "nor the port it's connected to");
        let tcp6 = "  sl  local_address                         remote_address                        st\n\
                    \x20  0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A\n";
        assert!(listening_on(tcp6, 3000, Some("0A")), "every IPv6 address");
        let udp = "   sl  local_address rem_address   st\n  12: 3500007F:0035 00000000:0000 07\n";
        assert!(listening_on(udp, 53, None) && !listening_on(udp, 54, None));
        assert!(!listening_on("  sl  local_address\n", 53, None), "the header alone");
        assert!(!listening_on("garbage\n 0: x y z\n", 53, None));
    }

    /// 6e: at port 53 the session's own server is one at that very address, or at every address — not tarae's DNS
    /// socket, at the session's own address (`192.0.2.1`), which a port-only check would take for one at loopback.
    /// The rows as a little-endian machine writes them (`127.0.0.1` is `0100007F`; `192.0.2.1` `010200C0`).
    #[test]
    #[cfg(target_endian = "little")]
    fn a_server_at_port_53_is_the_sessions_only_at_its_address() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        let tarae_only = "   sl  local_address rem_address   st\n   1: 010200C0:0035 00000000:0000 07\n";
        assert!(
            !listening_at(tarae_only, at("127.0.0.1:53"), None),
            "tarae's DNS socket isn't a server at loopback"
        );
        assert!(listening_on(tarae_only, 53, None), "(a port-only check would take it for one)");
        let udp = format!("{tarae_only}   2: 0100007F:0035 00000000:0000 07\n");
        assert!(listening_at(&udp, at("127.0.0.1:53"), None), "a run's own at 127.0.0.1");
        assert!(!listening_at(&udp, at("127.0.0.53:53"), None), "not at 127.0.0.53");
        let tcp = "  sl  local_address rem_address   st\n   0: 00000000:0035 00000000:0000 0A\n";
        assert!(listening_at(tcp, at("127.0.0.53:53"), Some("0A")), "at every address: a run's");
        let tcp6 = "  sl  local_address                         remote_address                        st\n\
                    \x20  0: 00000000000000000000000001000000:0035 00000000000000000000000000000000:0000 0A\n";
        assert!(listening_at(tcp6, at("[::1]:53"), Some("0A")), "at ::1");
        assert!(!listening_at(tcp6, at("[::1]:54"), Some("0A")));
    }

    /// Yama: tarae serves its runs at `ptrace_scope` 0 and 1 (and without Yama), at 2 only holding
    /// `CAP_SYS_PTRACE`, never at 3 — said why.
    #[test]
    fn yama_decides_whether_a_runs_calls_can_be_read() {
        let (none, ptrace) = ("Name:\ttarae\nCapEff:\t0000000000000000\n", "CapEff:\t00000000a80c25fb\n");
        for scope in [None, Some("0\n"), Some("1\n")] {
            assert_eq!(yama_allows(scope, none), Ok(()), "{scope:?}");
        }
        assert!(yama_allows(Some("2\n"), none).is_err_and(|why| why.contains("ptrace_scope 2")));
        assert_eq!(yama_allows(Some("2\n"), ptrace), Ok(()), "root in a container with the capability");
        assert!(yama_allows(Some("3"), ptrace).is_err_and(|why| why.contains("ptrace_scope 3")));
    }

    /// A real path is opened as it is: a link anywhere on the way (one swapped in since the path was
    /// resolved) and nothing is opened.
    #[test]
    fn a_real_path_is_opened_through_no_link() {
        let s = Scratch::new("open");
        std::fs::create_dir_all(s.base.join("there")).unwrap();
        let _l = std::os::unix::net::UnixListener::bind(s.base.join("there/s.sock")).unwrap();
        std::os::unix::fs::symlink(s.base.join("there"), s.base.join("link")).unwrap();
        assert!(open_real(&s.base.join("there/s.sock"), false).is_ok());
        assert_eq!(open_real(&s.base.join("link/s.sock"), false).err(), Some(libc::ELOOP));
        assert_eq!(open_real(&s.base.join("link"), true).err(), Some(libc::ELOOP));
    }

    /// Whether tarae serves its runs here — else the test says it skips itself: no Landlock (or ABI 1, which
    /// tarae doesn't use), or no supervisor (inside another one's filter, a container's filter refusing
    /// `pidfd_getfd` …).
    fn served(test: &str) -> bool {
        let why = match (crate::landlock::abi(), unavailable()) {
            (Ok(n), None) if n >= 2 => return true,
            (Ok(n), None) => format!("Landlock ABI {n}, which tarae doesn't use"),
            (Err(e), _) => format!("no Landlock ({e})"),
            (_, Some(why)) => why,
        };
        let line = format!("{}sandbox::supervisor::tests::{test} — {why}\n", crate::test_results::SELF_SKIP);
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        false
    }

    /// A project and a folder beside it under the build's `target/` (not in `/tmp`: a run writes there).
    struct Scratch {
        base: PathBuf,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let base = std::env::current_dir()
                .unwrap()
                .join(format!("target/tarae-serve-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("proj")).unwrap();
            std::fs::create_dir_all(base.join("out")).unwrap();
            let base = std::fs::canonicalize(&base).unwrap();
            Scratch { root: base.join("proj"), base }
        }

        /// A run's policy here: these ports open (or all of this machine's), these sockets granted — its
        /// rules made, and a watch the supervisor says its refusals to.
        fn policy(&self, ports: Vec<u16>, localhost: bool, sockets: Vec<PathBuf>) -> Policy {
            let env = super::super::Env {
                root: self.root.clone(),
                tmp: Some(super::super::profile_path(&std::env::temp_dir())),
                ports,
                localhost,
                // (what a real policy's run gets of this process's environment — `PATH` finds `sh`)
                run_env: super::super::run_env_of(
                    &super::super::Here::now(),
                    &crate::llm::LlmConfig::default(),
                ),
                ..Default::default()
            };
            let run = super::super::RunDir::new().ok();
            let tag = run.as_ref().map(|r| r.tag());
            let p = Policy { run, tag, sockets, ..Policy::for_project(&env) };
            Policy { watch: Some(super::super::Watch::quiet("t.")), ..super::super::confined(p) }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// The test binary run again in the sandbox of `p` as `probe`, from the project: what each of `ops`
    /// came to, in order.
    fn probe_in(p: &Policy, ops: &[String]) -> Vec<String> {
        let exe = std::env::current_exe().unwrap();
        let args =
            ["--exact", "sandbox::supervisor::tests::probe", "--ignored", "--nocapture", "--test-threads=1"];
        let mut cmd = super::super::command(Some(p), &exe.to_string_lossy(), &args.map(String::from));
        cmd.env("TARAE_PROBE", ops.join("\n")).current_dir(&p.root);
        let s = super::super::spawn(Some(p), &cmd).expect("the probe starts");
        let mut out = String::new();
        io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
        let mut err = String::new();
        io::Read::read_to_string(&mut s.stderr.unwrap(), &mut err).unwrap();
        crate::spawn::wait(s.pid).unwrap();
        // (the first comes after libtest's `test … ` on its line)
        let said: Vec<String> =
            out.lines().filter_map(|l| l.split_once("probe: ")).map(|(_, s)| s.to_string()).collect();
        assert_eq!(said.len(), ops.len(), "the probe said each:\n{out}\n{err}");
        said
    }

    /// A stream socket pair, `n` times a byte sent with `sendmsg` and read back on the other end (a receive
    /// waits 5 s at most): `ok`, or where and how it went wrong.
    fn pair_sendmsg(n: usize) -> String {
        let mut fds = [0; 2];
        // SAFETY: a two-slot array the call fills.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            return format!("socketpair {}", io::Error::last_os_error());
        }
        let wait = libc::timeval { tv_sec: 5, tv_usec: 0 };
        // SAFETY: a live struct of the size given.
        unsafe {
            libc::setsockopt(
                fds[1],
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const wait).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        for i in 0..n {
            let mut byte = [(i % 251) as u8];
            let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
            // SAFETY: zeroed, then its one buffer set; no address.
            let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
            (m.msg_iov, m.msg_iovlen) = (&raw mut iov, 1);
            // SAFETY: the message and its buffer live for the call.
            let sent = unsafe { libc::sendmsg(fds[0], &raw const m, 0) };
            if sent != 1 {
                return format!("sendmsg {i}: {sent} {}", io::Error::last_os_error());
            }
            let mut got = [0u8; 1];
            // SAFETY: a one-byte buffer.
            let read = unsafe { libc::recv(fds[1], got.as_mut_ptr().cast(), 1, 0) };
            if read != 1 || got[0] != (i % 251) as u8 {
                return format!("recv {i}: {read} {} {:?}", io::Error::last_os_error(), got);
            }
        }
        // SAFETY: ours.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        "ok".into()
    }

    /// A datagram socket pair, a byte sent with `sendmsg` and one control message of `level` and `kind` —
    /// `SCM_RIGHTS` passing `/dev/null`, any other the run's own credentials (`struct ucred`) — and read back:
    /// `ok` and how many descriptors came, or the send's error.
    fn pair_cmsg(level: i32, kind: i32) -> String {
        let mut fds = [0; 2];
        // SAFETY: a two-slot array the call fills.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
            return format!("socketpair {}", io::Error::last_os_error());
        }
        let null = std::fs::File::open("/dev/null").unwrap();
        let data: Vec<u8> = if (level, kind) == (libc::SOL_SOCKET, libc::SCM_RIGHTS) {
            null.as_raw_fd().to_ne_bytes().to_vec()
        } else {
            // SAFETY: plain calls.
            let ucred = unsafe { [libc::getpid() as u32, libc::getuid(), libc::getgid()] };
            ucred.iter().flat_map(|w| w.to_ne_bytes()).collect()
        };
        let mut control = (16 + data.len() as u64).to_ne_bytes().to_vec();
        control.extend(level.to_ne_bytes());
        control.extend(kind.to_ne_bytes());
        control.extend(&data);
        control.resize(control.len().next_multiple_of(8), 0);
        let mut byte = [1u8];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        // SAFETY: zeroed, then its buffer and control set; no address.
        let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
        (m.msg_iov, m.msg_iovlen) = (&raw mut iov, 1);
        (m.msg_control, m.msg_controllen) = (control.as_mut_ptr().cast(), control.len() as _);
        // SAFETY: the message and all it points to live for the call.
        let result = match unsafe { libc::sendmsg(fds[0], &raw const m, 0) } {
            1 => {
                let mut back = [0u64; 8];
                // SAFETY: as above, the control a buffer of ours to fill.
                let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
                (m.msg_iov, m.msg_iovlen) = (&raw mut iov, 1);
                (m.msg_control, m.msg_controllen) = (back.as_mut_ptr().cast(), size_of_val(&back) as _);
                // SAFETY: as above.
                let read = unsafe { libc::recvmsg(fds[1], &raw mut m, libc::MSG_DONTWAIT) };
                let back: Vec<u8> = back.iter().flat_map(|w| w.to_ne_bytes()).collect();
                let word = |o: usize| i32::from_ne_bytes(back[o..o + 4].try_into().unwrap());
                let len = u64::from_ne_bytes(back[..8].try_into().unwrap()) as usize;
                let came: Vec<i32> = if m.msg_controllen >= 16 && word(12) == libc::SCM_RIGHTS {
                    (16..len).step_by(4).map(word).collect()
                } else {
                    Vec::new()
                };
                for fd in &came {
                    // SAFETY: a descriptor the message gave us.
                    unsafe { libc::close(*fd) };
                }
                if read == 1 { format!("ok {}", came.len()) } else { format!("recvmsg {read}") }
            }
            _ => said::<()>(Err(io::Error::last_os_error())),
        };
        // SAFETY: ours.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        result
    }

    /// A run's message passes descriptors (`SCM_RIGHTS`) and no other control message (`wire::read_msg`): the
    /// descriptor arrives; credentials, `SOL_SOCKET`'s other types and another level's are `EPERM`, each named in
    /// the run's report.
    #[test]
    fn a_runs_message_passes_descriptors_and_no_other_control() {
        if !served("a_runs_message_passes_descriptors_and_no_other_control") {
            return;
        }
        let s = Scratch::new("cmsg");
        let p = s.policy(Vec::new(), false, Vec::new());
        let ops =
            [(1, 1), (1, 2), (1, 99), (41, 77)].map(|(level, kind)| format!("pair-cmsg {level} {kind}"));
        let got = probe_in(&p, &ops);
        assert_eq!(got, ["ok 1".to_string(), refused(), refused(), refused()]);
        let blocked = p.watch.as_ref().unwrap().blocked();
        let said: Vec<&str> = blocked
            .seen
            .iter()
            .filter(|d| d.op == "network-bound")
            .filter_map(|d| d.target.strip_prefix("sending ")?.split_once(" — ").map(|t| t.0))
            .collect();
        assert_eq!(
            said,
            ["SCM_CREDENTIALS", "control message level 1, type 99", "control message level 41, type 77"]
        );
    }

    /// For real (step 7: a sandboxed loop of `sendmsg` and `recv` on a socket pair hung): every message a run
    /// sends on a socket pair with `sendmsg` — tarae making the call on the run's own socket — arrives, once,
    /// in order.
    #[test]
    fn messages_sent_on_a_socket_pair_arrive() {
        if !served("messages_sent_on_a_socket_pair_arrive") {
            return;
        }
        let s = Scratch::new("pair");
        let p = s.policy(Vec::new(), false, Vec::new());
        let got = probe_in(&p, &["pair-sendmsg 1".into(), "pair-sendmsg 2000".into()]);
        assert_eq!(got, ["ok", "ok"]);
    }

    /// Looks with a `reaper::Reaper` at this process's tree every 20 ms, each as if `GRACE` had gone by since
    /// the one before — the most a reaper could take — for good (the scenes' processes are their own).
    fn reaping() {
        std::thread::spawn(|| {
            let (mut s, mut r) = (crate::procs::Sampler::default(), crate::reaper::Reaper::default());
            let mut now = std::time::Instant::now();
            loop {
                now += crate::reaper::GRACE;
                crate::procs::look(&mut s, &mut r, now);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
    }

    /// What tarae does with what a run leaves running (`crate::reaper`), in a process of its own (it changes
    /// the whole process): `TARAE_SCENE=<subreaper> <drop>`: tarae a child subreaper (`reaper::at_start`) or
    /// not, `CAP_SYS_PTRACE` dropped or not (so a container that has it shows what a VM's Yama 1 does), a
    /// reaper taking every zombie child it can (`reaping`); a run starts a child that leaves it (a double
    /// fork) and a `setsid sleep 60`, and ends at once (exit 3, its own waiter's to read); a second later the
    /// child connects to this machine's open port and says whether it could, and who its parent is now; then
    /// what the run left is ended as at its session's end (`sandbox::end_leftovers`). Prints `scene: <ok|no>
    /// <parent is the session's keeper: yes|no> <reaped: yes|no> exit=<the run's, as its waiter read it>
    /// ended=<yes|no>`.
    #[test]
    #[ignore]
    fn detached_scene() {
        let Ok(scene) = std::env::var("TARAE_SCENE") else { return };
        let (on, drop) = scene.split_once(' ').unwrap();
        if drop == "1" {
            drop_ptrace();
        }
        if on == "1" {
            crate::reaper::at_start();
            reaping();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = Scratch::new(&format!("detached-{on}"));
        let p = s.policy(vec![port], false, Vec::new());
        let out = s.root.join("after");
        // (the detached child waits a second after saying its pid: in a PID namespace it's translated to tarae's
        // number while it's there — `keeper::outer_pid`)
        let script = r#"( bash -c 'sleep 1
            if exec 3<>/dev/tcp/127.0.0.1/$PORT; then r=ok; else r=no; fi
            echo "$r $(cut -d" " -f4 /proc/$$/stat) $$" > "$OUT"; sleep 1' & )
            ( setsid sleep 60 & echo $! > "$OUT.long" ); sleep 0.1; exit 3"#;
        let mut cmd = super::super::command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.env("PORT", port.to_string()).env("OUT", &out).current_dir(&p.root);
        let spawned = super::super::spawn(Some(&p), &cmd).unwrap();
        let exit = crate::spawn::wait(spawned.pid).unwrap().code().unwrap_or(-1);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !std::fs::read_to_string(&out).is_ok_and(|t| t.ends_with('\n'))
            && std::time::Instant::now() < end
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let said = std::fs::read_to_string(&out).unwrap_or_default();
        let w: Vec<&str> = said.split_whitespace().collect();
        let (result, parent, child) = (w.first().copied().unwrap_or("none"), w.get(1), w.get(2));
        let k = match &p.confine {
            Some(Ok(c)) => c.keeper.clone(),
            _ => None,
        };
        let keeper = k.as_ref().map(|k| k.pid);
        // (what the child said is in its keeper's numbering — its parent as runs see the keeper; its pid, made
        // tarae's while it's there)
        let mine = k
            .as_ref()
            .is_some_and(|k| parent.is_some_and(|p| *p == super::super::keeper::seen_by_runs(k).to_string()));
        let child: Option<u32> = match (&k, child.and_then(|c| c.parse().ok())) {
            (Some(k), Some(c)) => Some(super::super::keeper::outer_pid(k, c)),
            (_, c) => c,
        };
        // (the orphan, a zombie child of the keeper's once it's done: reaped)
        let gone = || child.is_none_or(|c| !std::path::Path::new(&format!("/proc/{c}")).exists());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while mine && !gone() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let reaped = mine && gone();
        // (the long one, a daemon of sorts: the session's to end — as at its session's end)
        let long: Option<u32> = std::fs::read_to_string(out.with_extension("long"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .map(|l| k.as_ref().map_or(l, |k| super::super::keeper::outer_pid(k, l)));
        let ours = long.and_then(crate::procs::read).is_some_and(|o| keeper == Some(o.ppid));
        let ended = match long {
            Some(l) if ours => {
                super::super::end_leftovers(Some(&p.root)) == [l]
                    && !std::path::Path::new(&format!("/proc/{l}")).exists()
            }
            Some(l) => {
                // SAFETY: a plain kill of the scene's own sleep, which isn't ours to reap.
                unsafe { libc::kill(l as i32, libc::SIGKILL) };
                false
            }
            None => false,
        };
        let yes = |b: bool| if b { "yes" } else { "no" };
        println!("scene: {result} {} {} exit={exit} ended={}", yes(mine), yes(reaped), yes(ended));
    }

    /// (CAP_SYS_PTRACE is capability 19 — libc doesn't name it)
    const CAP_SYS_PTRACE: u32 = 19;

    /// Drops `CAP_SYS_PTRACE` from the calling thread (threads it starts after inherit that) — the scene's
    /// own process only.
    fn drop_ptrace() {
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let mut h = Header { version: 0x2008_0522, pid: 0 };
        let mut d = [Data::default(); 2];
        // SAFETY: capget/capset with version 3's header and its two data blocks.
        unsafe {
            assert_eq!(libc::syscall(libc::SYS_capget, &mut h, d.as_mut_ptr()), 0);
            d[0].effective &= !(1 << CAP_SYS_PTRACE);
            d[0].permitted &= !(1 << CAP_SYS_PTRACE);
            assert_eq!(libc::syscall(libc::SYS_capset, &mut h, d.as_ptr()), 0);
        }
    }

    /// The scene each way (`detached_scene`): a run's detached child is adopted by the session's keeper (a child
    /// subreaper — `sandbox::keeper`), its connect after its run is served (the keeper is tarae's descendant:
    /// Yama 1 lets tarae take its sockets), it's reaped once done, and what's left ends with its session —
    /// whether tarae itself is a subreaper or not, and with `CAP_SYS_PTRACE` dropped too (colima's container
    /// has the capability and its kernel Yama 1, so the capability dropped is a VM's case). Before the keeper,
    /// tarae as subreaper was what kept such a child served.
    #[test]
    fn a_runs_detached_child_is_served_after_its_run() {
        if !served("a_runs_detached_child_is_served_after_its_run") {
            return;
        }
        let scene = |on: &str, drop: bool| {
            let exe = std::env::current_exe().unwrap();
            let out = std::process::Command::new(exe)
                .args(["--exact", "sandbox::supervisor::tests::detached_scene", "--ignored", "--nocapture"])
                .args(["--test-threads=1"])
                .env("TARAE_SCENE", format!("{on} {}", drop as u8))
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines().find_map(|l| l.split_once("scene: ").map(|(_, s)| s.to_string())).unwrap_or(text)
        };
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let scope = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok();
        let yama = scope.as_deref().map(str::trim);
        let cap = status
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:"))
            .and_then(|h| u64::from_str_radix(h.trim(), 16).ok())
            .is_some_and(|c| c & (1 << CAP_SYS_PTRACE) != 0);
        // (Yama 2 or 3: nothing is served without the capability, descendant or not)
        let drops: &[bool] = if cap && yama.is_none_or(|y| y <= "1") { &[false, true] } else { &[false] };
        for &drop in drops {
            let (off, on) = (scene("0", drop), scene("1", drop));
            let line = format!(
                "capability {}: tarae a subreaper off: {off} · on: {on}\n",
                if drop { "dropped" } else { "kept" }
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            for said in [&on, &off] {
                assert_eq!(
                    said, "ok yes yes exit=3 ended=yes",
                    "the keeper a subreaper: adopted, served, reaped; the run's waiter undisturbed; the long one \
                     ended with its session"
                );
            }
        }
    }

    /// A run in `p` of `sh -c script` (`env` set), its watch told it started (as `task.rs` does): its pid.
    fn run_in(p: &Policy, script: &str, env: &[(&str, &Path)]) -> u32 {
        let mut cmd = super::super::command(Some(p), "sh", &["-c".into(), script.into()]);
        cmd.envs(env.iter().copied()).current_dir(&p.root);
        let s = super::super::spawn(Some(p), &cmd).unwrap();
        if let Some(w) = &p.watch {
            w.run_started(s.pid);
        }
        s.pid
    }

    /// Waits (5 s at most) for `done`.
    fn until(done: impl Fn() -> bool) -> bool {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        done()
    }

    /// What a run left running, after its run, in a process of its own (`TARAE_LEFTOVER_SCENE`; tarae a child
    /// subreaper, as it is): run 1 leaves a child that, each time a file says so, connects beyond this machine
    /// with the network closed (a documentation address: refused in every tier). Told while run 2 of the project goes, the refusal is run 2's (its report); told
    /// while none goes, it waits for run 3's. Then: what a run under one policy left is ended when a run of
    /// the project starts under another (it holds the old one) — not by one under the same. Prints `scene:
    /// w1=<n> w2=<n> w3=<n> stale=<ended: yes|no> same=<kept: yes|no>` (n: the refusals of port 9 each run's
    /// watch has).
    #[test]
    #[ignore]
    fn leftover_scene() {
        if std::env::var_os("TARAE_LEFTOVER_SCENE").is_none() {
            return;
        }
        crate::reaper::at_start();
        let s = Scratch::new("leftover");
        let refused = |p: &Policy| {
            let b = p.watch.as_ref().unwrap().blocked();
            b.seen.iter().filter(|d| d.op == "network-outbound" && d.target.ends_with(":9")).count()
        };
        let ended = |p: &Policy| p.watch.as_ref().is_some_and(|w| w.ended());
        let go = s.base.join("out/go");
        let script = r#"( bash -c 'for i in 1 2; do
            while [ ! -e "$GO$i" ]; do sleep 0.05; done
            ( : <>/dev/tcp/192.0.2.1/9 ) 2>/dev/null; done' & ); exit 0"#;
        let p1 = s.policy(Vec::new(), false, Vec::new());
        crate::spawn::wait(run_in(&p1, script, &[("GO", &go)])).unwrap();
        assert!(until(|| ended(&p1)), "run 1's end seen");
        let p2 = s.policy(Vec::new(), false, Vec::new());
        let r2 = run_in(&p2, "sleep 3", &[]);
        std::fs::write(go.with_file_name("go1"), "").unwrap();
        until(|| refused(&p2) > 0);
        crate::spawn::wait(r2).unwrap();
        assert!(until(|| ended(&p2)), "run 2's end seen");
        std::fs::write(go.with_file_name("go2"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let p3 = s.policy(Vec::new(), false, Vec::new());
        crate::spawn::wait(run_in(&p3, "true", &[])).unwrap();
        let (w1, w2, w3) = (refused(&p1), refused(&p2), refused(&p3));
        super::super::end_leftovers(Some(&s.root));
        // A policy since changed: what the old one left ends; under the same, it stays
        let long = |p: &Policy, name: &str| -> u32 {
            let file = s.root.join(name);
            crate::spawn::wait(run_in(p, r#"( setsid sleep 60 & echo $! > "$F" )"#, &[("F", &file)]))
                .unwrap();
            let said: u32 = std::fs::read_to_string(&file).unwrap().trim().parse().unwrap();
            // (in its keeper's numbering: made tarae's)
            match &p.confine {
                Some(Ok(c)) => c.keeper.as_ref().map_or(said, |k| super::super::keeper::outer_pid(k, said)),
                _ => said,
            }
        };
        let there = |pid: u32| std::path::Path::new(&format!("/proc/{pid}")).exists();
        let old = long(&s.policy(Vec::new(), false, Vec::new()), "old");
        let new = long(&s.policy(vec![4242], false, Vec::new()), "new");
        let stale = until(|| !there(old));
        crate::spawn::wait(run_in(&s.policy(vec![4242], false, Vec::new()), "true", &[])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let same = there(new);
        super::super::end_leftovers(Some(&s.root));
        let yes = |b: bool| if b { "yes" } else { "no" };
        println!("scene: w1={w1} w2={w2} w3={w3} stale={} same={}", yes(stale), yes(same));
    }

    /// `leftover_scene`: a refusal of what a run left goes to its project's run going then (its report),
    /// else to the next one's; what a run left under a policy since changed ends with the change.
    #[test]
    fn what_a_run_left_is_told_to_its_projects_current_run() {
        if !served("what_a_run_left_is_told_to_its_projects_current_run") {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--exact", "sandbox::supervisor::tests::leftover_scene", "--ignored", "--nocapture"])
            .args(["--test-threads=1"])
            .env("TARAE_LEFTOVER_SCENE", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let said = text.lines().find_map(|l| l.split_once("scene: ").map(|(_, s)| s.to_string()));
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(said.as_deref(), Some("w1=0 w2=1 w3=1 stale=yes same=yes"), "{text}\n{err}");
    }

    /// 6d: what a run left running connects as its run, after the run is gone — through the session's planted
    /// socket. Its connect still reaches the supervisor of the run that started it (the filter is inherited), which
    /// marks the socket as that run's (`marks.rs`): the proxy judges it by that run's rules, and the refusal goes to
    /// the project's run going then (`sandbox::tell`), as the supervisor's own refusals of a leftover do. Run 1's
    /// rules name only `first-6d.test`, which it reaches itself first (its mark given then), and the user refused
    /// `left-6d.test` for the session; run 2's rules name `left-6d.test`, and run 2 reaches it (its mark the newest).
    /// Run 1 leaves a child that, once run 1 is over and while run 2 goes, asks for `left-6d.test`: it's refused —
    /// run 1's rules, by run 1's older mark, not the session's newest run's — the outside reached twice (the runs'
    /// own), and the refusal is in run 2's report, naming the host, never in run 1's. In a TPROXY session only.
    #[test]
    fn what_a_run_left_connects_as_its_run() {
        let test = "what_a_run_left_connects_as_its_run";
        if !served(test) {
            return;
        }
        let probed = super::super::tier::get();
        if probed.redirect.is_err() || !probed.ns.netns() {
            let why = probed.redirect.clone().err().unwrap_or_else(|| "no network namespace".into());
            assert!(
                !std::env::var("TARAE_EXPECT_NETWORK").is_ok_and(|n| n == "tproxy"),
                "{test}: TPROXY expected here — {why}"
            );
            let line = format!(
                "{}sandbox::supervisor::tests::{test} — no TPROXY here: {why}\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        // (the outside: a listener on this machine's own address, beyond for a policy that knows no own address)
        let ip = {
            let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            s.connect("192.0.2.1:9").unwrap();
            s.local_addr().unwrap().ip()
        };
        let outside = std::net::TcpListener::bind((ip, 0)).unwrap();
        let at = outside.local_addr().unwrap();
        let reached = Arc::new(AtomicU64::new(0));
        let counting = reached.clone();
        std::thread::spawn(move || {
            for s in outside.incoming().flatten() {
                counting.fetch_add(1, Relaxed);
                // (the request's head read whole first: a socket closed with bytes unread — or with them still to
                // come — is reset, and the reply can be lost before the proxy relays it: under load, it was)
                let mut head = io::BufReader::new(&s);
                let mut line = String::new();
                while io::BufRead::read_line(&mut head, &mut line).is_ok_and(|n| n > 0)
                    && !line.trim().is_empty()
                {
                    line.clear();
                }
                let _ = io::Write::write_all(&mut &s, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        for name in ["left-6d.test", "first-6d.test"] {
            super::super::proxy::TEST_NAMES.lock().unwrap().push((name.into(), at));
        }
        let s = Scratch::new("left-6d");
        let with = |hosts: &[&str]| {
            let egress = super::super::proxy::egress::Egress {
                hosts: hosts
                    .iter()
                    .map(|h| super::super::proxy::egress::Pattern::parse(h).unwrap())
                    .collect(),
                ..Default::default()
            };
            Policy {
                network: true,
                own_addresses: Some(Vec::new()),
                egress,
                ..s.policy(Vec::new(), false, Vec::new())
            }
        };
        let (p1, p2) = (with(&["first-6d.test"]), with(&["left-6d.test"]));
        let session = super::super::proxy::session_of(&p1.root);
        let (host, scope) = ("left-6d.test".to_string(), super::super::proxy::Scope::Refused);
        super::super::proxy::get().send(super::super::proxy::ToProxy::Answer {
            session,
            run: p1.run_id,
            kind: super::super::proxy::Kind::Host,
            host,
            port: 80,
            scope,
        });
        // (inside `bash -c '…'`: no single quote of its own)
        let ask_for = |host: &str| {
            format!(
                r#"exec 3<>/dev/tcp/203.0.113.7/80 && printf "GET / HTTP/1.1\r\nHost: {host}\r\n\r\n" >&3 &&
                head -1 <&3"#
            )
        };
        let (ask, first) = (ask_for("left-6d.test"), ask_for("first-6d.test"));
        // (what a run writes, in its project: the only place it may)
        let (go, left, own) = (s.base.join("out/go"), s.root.join("left"), s.root.join("own"));
        // (run 1 reaches a host of its own rules first — its mark given then, before run 2's — and leaves the child)
        let leaves = format!(
            r#"bash -c '{first}' > /dev/null
            ( bash -c 'while [ ! -e "$GO" ]; do sleep 0.05; done; {ask} > "$LEFT"' & ); exit 0"#
        );
        crate::spawn::wait(run_in(&p1, &leaves, &[("GO", &go), ("LEFT", &left)])).unwrap();
        assert!(until(|| p1.watch.as_ref().is_some_and(|w| w.ended())), "run 1's end seen");
        let r2 = run_in(&p2, &format!(r#"bash -c '{ask} > "$OWN"; sleep 3'"#), &[("OWN", &own)]);
        assert!(
            until(|| std::fs::read_to_string(&own).is_ok_and(|t| !t.is_empty())),
            "run 2 reached it — its file {:?}, its report {:?}, the outside reached {}",
            std::fs::read_to_string(&own),
            p2.watch.as_ref().unwrap().blocked().seen,
            reached.load(Relaxed)
        );
        std::fs::write(&go, "").unwrap();
        // (its answer waited for, and said below either way)
        until(|| std::fs::read_to_string(&left).is_ok_and(|t| !t.is_empty()));
        let host_said = |p: &Policy| {
            let b = p.watch.as_ref().unwrap().blocked();
            b.seen
                .iter()
                .filter(|d| d.op == super::super::proxy::HOST_OP && d.target.starts_with("left-6d.test:80: "))
                .count()
        };
        until(|| host_said(&p2) > 0);
        crate::spawn::wait(r2).unwrap();
        let said = (
            std::fs::read_to_string(&own).unwrap().trim().to_string(),
            std::fs::read_to_string(&left).unwrap_or_default().trim().to_string(),
            reached.load(Relaxed),
            host_said(&p1),
            host_said(&p2),
        );
        // (the outside reached twice: run 1's own first connection, run 2's — never the child's)
        let expected = ("HTTP/1.1 200 OK".to_string(), "HTTP/1.1 403 Forbidden".to_string(), 2, 0, 1);
        assert_eq!(
            said, expected,
            "(run 2's reply, the child's, the outside reached, run 1's report, run 2's)"
        );
        super::super::end_leftovers(Some(&s.root));
    }

    /// An op's result as the probe says it: `ok`, or `err` and the errno.
    fn said<T>(r: io::Result<T>) -> String {
        match r {
            Ok(_) => "ok".into(),
            Err(e) => format!("err {}", e.raw_os_error().unwrap_or(0)),
        }
    }

    fn refused() -> String {
        format!("err {}", libc::EPERM)
    }

    /// What a run tries, inside the sandbox (`probe_in`): each op of `TARAE_PROBE` and what it came to.
    #[test]
    #[ignore]
    fn probe() {
        use std::net::{TcpListener, TcpStream, UdpSocket};
        use std::os::unix::net::{UnixListener, UnixStream};
        let Ok(ops) = std::env::var("TARAE_PROBE") else { return };
        for op in ops.lines() {
            let w: Vec<&str> = op.split(' ').collect();
            let result = match w.as_slice() {
                ["connect", a] => said(TcpStream::connect(a)),
                // (and the device it's bound to: the loopback, whatever the address)
                ["bind", a] => match TcpListener::bind(a) {
                    Ok(l) => {
                        let mut dev = [0u8; 16];
                        let mut len = dev.len() as libc::socklen_t;
                        // SAFETY: a buffer and its length for the call.
                        unsafe {
                            libc::getsockopt(
                                l.as_raw_fd(),
                                libc::SOL_SOCKET,
                                libc::SO_BINDTODEVICE,
                                dev.as_mut_ptr().cast(),
                                &mut len,
                            )
                        };
                        let dev =
                            String::from_utf8_lossy(&dev[..len as usize]).trim_end_matches('\0').to_string();
                        format!("ok {dev}")
                    }
                    Err(e) => said::<()>(Err(e)),
                },
                ["sendto", a] => said(UdpSocket::bind("0.0.0.0:0").and_then(|s| s.send_to(b"x", a))),
                ["sendmsg", a] => said(sendmsg_to(a.parse().unwrap())),
                // (an array of two messages where none fits below the end of the address space)
                ["sendmmsg-top"] => said(std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
                    let top = std::ptr::without_provenance_mut::<libc::mmsghdr>(usize::MAX - 63);
                    // SAFETY: an address nothing is read from or written to — the call fails `EFAULT`.
                    match unsafe { libc::sendmmsg(s.as_raw_fd(), top, 2, 0) } {
                        r if r < 0 => Err(io::Error::last_os_error()),
                        r => Ok(r),
                    }
                })),
                ["unix", path] => {
                    said(UnixStream::connect(path).and_then(|mut s| io::Write::write_all(&mut s, b"hi")))
                }
                ["unixbind", path] => said(UnixListener::bind(path)),
                // (a name given in hex: one that isn't UTF-8)
                ["unixbind-hex", hex] => {
                    use std::os::unix::ffi::OsStrExt;
                    let name: Vec<u8> = (0..hex.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                        .collect();
                    said(UnixListener::bind(std::ffi::OsStr::from_bytes(&name)))
                }
                ["unixbind-tmp", name] => {
                    said(UnixListener::bind(PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join(name)))
                }
                ["abstract", name] => {
                    use std::os::linux::net::SocketAddrExt;
                    let a = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
                    said(UnixStream::connect_addr(&a))
                }
                ["race", good, bad, n] => {
                    race(good.parse().unwrap(), bad.parse().unwrap(), n.parse().unwrap())
                }
                ["pair-sendmsg", n] => pair_sendmsg(n.parse().unwrap()),
                ["pair-cmsg", level, kind] => pair_cmsg(level.parse().unwrap(), kind.parse().unwrap()),
                ["lookup", name] => lookup(name),
                ["netlink-route"] => said(netlink_route()),
                ["net-state"] => net_state().unwrap_or_else(|e| format!("err {e}")),
                ["netlink-add-address"] => netlink_add_address(),
                ["netlink-delete-route"] => netlink_delete_route(),
                ["netlink-add-link"] => netlink_add_link(),
                ["netlink-bind-group", g] => netlink_bind_group(g.parse().unwrap()),
                ["netlink-sendto-group"] => netlink_sendto_group(),
                ["netlink-connect-port"] => netlink_connect_port(),
                ["netlink-handed", path] => netlink_handed(path),
                ["netlink-oversized"] => netlink_oversized(),
                ["netlink", protocol] => {
                    // SAFETY: a plain call; a descriptor it makes is closed at once.
                    let fd =
                        unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, protocol.parse().unwrap()) };
                    // SAFETY: ours, when made.
                    said(if fd < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(unsafe { libc::close(fd) })
                    })
                }
                _ => format!("unknown op {op}"),
            };
            println!("probe: {result}");
        }
    }

    /// A name looked up as programs do, asking which addresses this machine has (`AI_ADDRCONFIG` — glibc asks
    /// netlink): `ok` and how many addresses, or the error.
    fn lookup(name: &str) -> String {
        let name = std::ffi::CString::new(name).unwrap();
        // SAFETY: an all-integer/pointer struct, zeroed then set.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_flags = libc::AI_ADDRCONFIG;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        // SAFETY: a C string, the hints and an out pointer, live for the call; the list freed after.
        let r = unsafe { libc::getaddrinfo(name.as_ptr(), std::ptr::null(), &hints, &mut res) };
        if r != 0 {
            return format!("getaddrinfo {r}");
        }
        let mut n = 0;
        let mut at = res;
        while !at.is_null() {
            n += 1;
            // SAFETY: a node of the list getaddrinfo made.
            at = unsafe { (*at).ai_next };
        }
        // SAFETY: the list getaddrinfo made.
        unsafe { libc::freeaddrinfo(res) };
        if n > 0 { "ok".into() } else { "no address".into() }
    }

    /// Netlink's routing protocol asked for the interfaces, as a lookup asks — the request sent to the
    /// kernel's address (`sendto`, tarae's), the answer read back.
    fn netlink_route() -> io::Result<()> {
        // SAFETY: a plain call; the descriptor is closed below.
        let fd = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just made, ours.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // `struct nlmsghdr` (16 bytes) and an empty `struct ifinfomsg` (16): every link, dumped
        let mut req = Vec::new();
        req.extend(32u32.to_ne_bytes());
        req.extend(libc::RTM_GETLINK.to_ne_bytes());
        req.extend(((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
        req.extend(1u32.to_ne_bytes());
        req.extend(0u32.to_ne_bytes());
        req.extend([0u8; 16]);
        // SAFETY: an all-integer struct, zeroed then set.
        let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: the request and the address, live for the call.
        let sent = unsafe {
            libc::sendto(
                fd.as_raw_fd(),
                req.as_ptr().cast(),
                req.len(),
                0,
                (&raw const kernel).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut answer = [0u8; 8192];
        // SAFETY: a buffer of ours and its length.
        let got = unsafe { libc::recv(fd.as_raw_fd(), answer.as_mut_ptr().cast(), answer.len(), 0) };
        let kind = u16::from_ne_bytes([answer[4], answer[5]]);
        match got {
            n if n < 0 => Err(io::Error::last_os_error()),
            n if n >= 16 && kind == libc::RTM_NEWLINK => Ok(()),
            _ => Err(io::Error::other(format!("netlink answered type {kind}"))),
        }
    }

    /// One netlink message: its `struct nlmsghdr` (length, type, flags, sequence 1, port 0), then `body`.
    fn nl_msg(kind: u16, flags: i32, body: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend((16 + body.len() as u32).to_ne_bytes());
        m.extend(kind.to_ne_bytes());
        m.extend((flags as u16).to_ne_bytes());
        m.extend(1u32.to_ne_bytes());
        m.extend(0u32.to_ne_bytes());
        m.extend(body);
        m
    }

    /// An attribute (`struct rtattr`: its length, its type, the data), padded to 4.
    fn nl_attr(kind: u16, data: &[u8]) -> Vec<u8> {
        let mut a = Vec::new();
        a.extend((4 + data.len() as u16).to_ne_bytes());
        a.extend(kind.to_ne_bytes());
        a.extend(data);
        a.resize(a.len().div_ceil(4) * 4, 0);
        a
    }

    /// What netlink answered, message by message (each `nlmsg_len` long, aligned to 4): (type, body).
    fn nl_split(mut buf: &[u8]) -> Vec<(u16, &[u8])> {
        let mut out = Vec::new();
        while buf.len() >= 16 {
            let len = u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
            if len < 16 || len > buf.len() {
                break;
            }
            out.push((u16::from_ne_bytes([buf[4], buf[5]]), &buf[16..len]));
            buf = &buf[(len.div_ceil(4) * 4).min(buf.len())..];
        }
        out
    }

    /// The attribute of type `kind` among `attrs` (`struct rtattr`s, aligned to 4), its data.
    fn nl_find(mut attrs: &[u8], kind: u16) -> Option<&[u8]> {
        while attrs.len() >= 4 {
            let len = u16::from_ne_bytes([attrs[0], attrs[1]]) as usize;
            if len < 4 || len > attrs.len() {
                return None;
            }
            // (the type's top two bits are flags: nested, network byte order)
            if u16::from_ne_bytes([attrs[2], attrs[3]]) & 0x3fff == kind {
                return Some(&attrs[4..len]);
            }
            attrs = &attrs[(len.div_ceil(4) * 4).min(attrs.len())..];
        }
        None
    }

    /// A netlink routing socket of the run's own, its answers awaited at most 10 s.
    fn nl_socket() -> io::Result<OwnedFd> {
        // SAFETY: a plain call; a descriptor it makes is ours.
        let fd = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just made, ours.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let wait = libc::timeval { tv_sec: 10, tv_usec: 0 };
        // SAFETY: a timeval and its size, live for the call.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const wait).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        Ok(fd)
    }

    /// A netlink address: the kernel's is port 0, no groups.
    fn nl_addr(port: u32, groups: u32) -> libc::sockaddr_nl {
        // SAFETY: an all-integer struct, zeroed then set.
        let mut a: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        a.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        a.nl_pid = port;
        a.nl_groups = groups;
        a
    }

    /// `call` (`libc::bind`, `libc::connect`) on `fd` with the netlink address `port`, `groups`.
    fn nl_call(
        call: unsafe extern "C" fn(i32, *const libc::sockaddr, libc::socklen_t) -> i32,
        fd: &OwnedFd,
        (port, groups): (u32, u32),
    ) -> io::Result<()> {
        let a = nl_addr(port, groups);
        let alen = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        // SAFETY: our descriptor and an address of ours, live for the call.
        match unsafe { call(fd.as_raw_fd(), (&raw const a).cast(), alen) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// The `RTM_GETLINK` dump a lookup asks, sent (`sendto`) to netlink port `port` and groups `groups`.
    fn nl_dump_to(fd: &OwnedFd, port: u32, groups: u32) -> io::Result<()> {
        let req = nl_msg(libc::RTM_GETLINK, libc::NLM_F_REQUEST | libc::NLM_F_DUMP, &[0; 16]);
        let to = nl_addr(port, groups);
        // SAFETY: the request and the address, live for the call.
        let sent = unsafe {
            libc::sendto(
                fd.as_raw_fd(),
                req.as_ptr().cast(),
                req.len(),
                0,
                (&raw const to).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// A bind joining rtnetlink's group `group` (bit `group − 1` of `nl_groups`).
    fn netlink_bind_group(group: u32) -> String {
        said(nl_socket().and_then(|fd| nl_call(libc::bind, &fd, (0, 1 << (group - 1)))))
    }

    /// A read sent to rtnetlink's group 1 (`RTNLGRP_LINK`'s listeners) — a group, not the kernel.
    fn netlink_sendto_group() -> String {
        said(nl_socket().and_then(|fd| nl_dump_to(&fd, 0, 1)))
    }

    /// The port another netlink socket is bound to — the run's own, standing in for another process's.
    const OTHER_PORT: u32 = 0x7a7a_0001;

    /// A connect to another netlink socket's port (`OTHER_PORT`, bound first): after it, the run's own sends
    /// without an address would go there.
    fn netlink_connect_port() -> String {
        said((|| {
            let other = nl_socket()?;
            nl_call(libc::bind, &other, (OTHER_PORT, 0))?;
            nl_call(libc::connect, &nl_socket()?, (OTHER_PORT, 0))
        })())
    }

    /// A netlink socket of another protocol than routing, made outside the sandbox and handed to the run over
    /// the unix socket at `path` (`SCM_RIGHTS`) — a run can make none — a read sent on it to the kernel's
    /// address. (Socket diagnostics' type 18 is its oldest read, `TCPDIAG_GETSOCK`: nothing changes either way.)
    fn netlink_handed(path: &str) -> String {
        said((|| {
            let s = std::os::unix::net::UnixStream::connect(path)?;
            let fd = super::super::handed::received_fds(&s)
                .into_iter()
                .next()
                .ok_or_else(|| io::Error::other("no descriptor came"))?;
            nl_send(&fd, &nl_msg(18, libc::NLM_F_REQUEST | libc::NLM_F_DUMP, &[0; 16]), "sendto")
        })())
    }

    /// A batch of reads past `SEND_MAX` (`sendto`), each 20 bytes so that one cut at `SEND_MAX` ends mid-message.
    fn netlink_oversized() -> String {
        let one = nl_msg(libc::RTM_GETLINK, libc::NLM_F_REQUEST | libc::NLM_F_DUMP, &[0; 4]);
        let batch = one.repeat(SEND_MAX / one.len() + 1);
        said(nl_socket().and_then(|fd| nl_send(&fd, &batch, "sendto")))
    }

    /// `req` sent to the kernel's netlink address `how`: `sendto`, `sendmsg` (the address in the message, as
    /// `ip` sends) or `sendmmsg` — each a call tarae makes for the run.
    fn nl_send(fd: &OwnedFd, req: &[u8], how: &str) -> io::Result<()> {
        let mut kernel = nl_addr(0, 0);
        let alen = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        let mut iov = libc::iovec { iov_base: req.as_ptr() as *mut libc::c_void, iov_len: req.len() };
        // SAFETY: an all-integer/pointer struct, zeroed then set.
        let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
        h.msg_name = (&raw mut kernel).cast();
        h.msg_namelen = alen;
        h.msg_iov = &mut iov;
        h.msg_iovlen = 1;
        // SAFETY: the request, the address and the headers pointing at them are live for each call.
        let r = unsafe {
            match how {
                "sendto" => libc::sendto(
                    fd.as_raw_fd(),
                    req.as_ptr().cast(),
                    req.len(),
                    0,
                    (&raw const kernel).cast(),
                    alen,
                ),
                "sendmsg" => libc::sendmsg(fd.as_raw_fd(), &h, 0),
                _ => {
                    let mut m = libc::mmsghdr { msg_hdr: h, msg_len: 0 };
                    libc::sendmmsg(fd.as_raw_fd(), &mut m, 1, 0) as isize
                }
            }
        };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// The messages netlink answers on `fd` up to a dump's end or an error message (an ack is one).
    fn nl_answers(fd: &OwnedFd) -> io::Result<Vec<(u16, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            // SAFETY: a buffer of ours and its length.
            let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n <= 0 {
                return Err(if n < 0 { io::Error::last_os_error() } else { io::Error::other("no answer") });
            }
            for (kind, body) in nl_split(&buf[..n as usize]) {
                out.push((kind, body.to_vec()));
                if [libc::NLMSG_DONE, libc::NLMSG_ERROR].contains(&i32::from(kind)) {
                    return Ok(out);
                }
            }
        }
    }

    /// A change to the network asked of the kernel, with an ack (`NLM_F_ACK` and `flags`), sent `how`: `ok` when
    /// the kernel made it, else the errno — the send's own (tarae refused it) or the ack's (the kernel did).
    fn nl_change(how: &str, kind: u16, flags: i32, body: &[u8]) -> String {
        said(nl_socket().and_then(|fd| {
            nl_send(&fd, &nl_msg(kind, libc::NLM_F_REQUEST | libc::NLM_F_ACK | flags, body), how)?;
            match nl_answers(&fd)?.last() {
                Some((k, ack)) if i32::from(*k) == libc::NLMSG_ERROR && ack.len() >= 4 => {
                    match i32::from_ne_bytes(ack[..4].try_into().unwrap()) {
                        0 => Ok(()),
                        e => Err(io::Error::from_raw_os_error(-e)),
                    }
                }
                _ => Err(io::Error::other("no ack")),
            }
        }))
    }

    /// `ip addr add 198.51.100.9/32 dev lo`, as `ip` sends it (`sendmsg`, the address in the message).
    fn netlink_add_address() -> String {
        // SAFETY: a C string, live for the call.
        let lo = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
        // `struct ifaddrmsg`: family, prefix length, flags, scope, device
        let mut body = vec![libc::AF_INET as u8, 32, 0, 0];
        body.extend(lo.to_ne_bytes());
        body.extend(nl_attr(libc::IFA_LOCAL, &[198, 51, 100, 9]));
        body.extend(nl_attr(libc::IFA_ADDRESS, &[198, 51, 100, 9]));
        nl_change("sendmsg", libc::RTM_NEWADDR, libc::NLM_F_CREATE | libc::NLM_F_EXCL, &body)
    }

    /// `ip route del local 127.0.0.0/8 table local` (`sendto`) — the loopback's route, one every netns with `lo` up
    /// has (the session's has no default route to delete).
    fn netlink_delete_route() -> String {
        // `struct rtmsg`: family, destination length, source length, tos, table, protocol, scope, type, flags
        let mut body = vec![
            libc::AF_INET as u8,
            8,
            0,
            0,
            libc::RT_TABLE_LOCAL,
            0,
            libc::RT_SCOPE_NOWHERE,
            libc::RTN_LOCAL,
        ];
        body.extend(0u32.to_ne_bytes());
        body.extend(nl_attr(libc::RTA_DST, &[127, 0, 0, 0]));
        nl_change("sendto", libc::RTM_DELROUTE, 0, &body)
    }

    /// `ip link add d0 type dummy` (`sendmmsg`).
    fn netlink_add_link() -> String {
        // `struct ifinfomsg`, every field 0
        let mut body = vec![0u8; 16];
        body.extend(nl_attr(libc::IFLA_IFNAME, b"d0\0"));
        body.extend(nl_attr(libc::IFLA_LINKINFO, &nl_attr(libc::IFLA_INFO_KIND, b"dummy\0")));
        nl_change("sendmmsg", libc::RTM_NEWLINK, libc::NLM_F_CREATE | libc::NLM_F_EXCL, &body)
    }

    /// The network the run is in, as netlink dumps it — what a change moves and nothing that moves alone (no
    /// counters): links (index, name, flags, MTU), addresses (family, address/prefix, device), routes (family,
    /// table, type, destination/length, device). One line, sorted.
    fn net_state() -> io::Result<String> {
        let fd = nl_socket()?;
        let word =
            |b: &[u8], at: usize| b.get(at..at + 4).map_or(0, |w| u32::from_ne_bytes(w.try_into().unwrap()));
        let ip = |b: Option<&[u8]>| match b.map(<[u8]>::len) {
            Some(4) => std::net::Ipv4Addr::from(<[u8; 4]>::try_from(b.unwrap()).unwrap()).to_string(),
            Some(16) => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(b.unwrap()).unwrap()).to_string(),
            _ => "-".into(),
        };
        let mut seen = Vec::new();
        // (the requests' bodies: an empty `ifinfomsg` (16), `ifaddrmsg` (8), `rtmsg` (12) — every family)
        for (ask, len) in [(libc::RTM_GETLINK, 16), (libc::RTM_GETADDR, 8), (libc::RTM_GETROUTE, 12)] {
            let req = nl_msg(ask, libc::NLM_F_REQUEST | libc::NLM_F_DUMP, &vec![0u8; len]);
            nl_send(&fd, &req, "sendto")?;
            for (kind, m) in nl_answers(&fd)? {
                let attrs = m.get(len..).unwrap_or_default();
                match kind {
                    libc::RTM_NEWLINK => {
                        let name = nl_find(attrs, libc::IFLA_IFNAME).unwrap_or_default();
                        let name = String::from_utf8_lossy(name).trim_end_matches('\0').to_string();
                        let mtu = word(nl_find(attrs, libc::IFLA_MTU).unwrap_or_default(), 0);
                        seen.push(format!("link {} {name} flags {:x} mtu {mtu}", word(&m, 4), word(&m, 8)));
                    }
                    libc::RTM_NEWADDR => {
                        let at = ip(nl_find(attrs, libc::IFA_ADDRESS));
                        seen.push(format!("addr {} {at}/{} dev {}", m[0], m[1], word(&m, 4)));
                    }
                    libc::RTM_NEWROUTE => {
                        let table = nl_find(attrs, libc::RTA_TABLE).map_or(u32::from(m[4]), |t| word(t, 0));
                        let to = ip(nl_find(attrs, libc::RTA_DST));
                        let dev = word(nl_find(attrs, libc::RTA_OIF).unwrap_or_default(), 0);
                        seen.push(format!(
                            "route {} table {table} type {} {to}/{} dev {dev}",
                            m[0], m[7], m[1]
                        ));
                    }
                    k if i32::from(k) == libc::NLMSG_ERROR => {
                        let e = word(&m, 0) as i32;
                        return Err(io::Error::from_raw_os_error(-e));
                    }
                    _ => {}
                }
            }
        }
        seen.sort();
        Ok(seen.join(", "))
    }

    /// A UDP datagram sent with `sendmsg`, the address in the message.
    fn sendmsg_to(to: SocketAddr) -> io::Result<()> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let SocketAddr::V4(v4) = to else { return Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT)) };
        // SAFETY: an all-integer struct, zeroed then set.
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = v4.port().to_be();
        sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) };
        let data = *b"m";
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: 1 };
        // SAFETY: an all-integer/pointer struct, zeroed then set.
        let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
        h.msg_name = (&raw mut sin).cast();
        h.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        h.msg_iov = &mut iov;
        h.msg_iovlen = 1;
        // SAFETY: the header and what it points to are live for the call.
        if unsafe { libc::sendmsg(s.as_raw_fd(), &h, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `n` connects to one address in memory while another thread keeps changing it between `good` and
    /// `bad` (their ports): how many connected, and how many were refused.
    fn race(good: SocketAddr, bad: SocketAddr, n: usize) -> String {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let word = |a: SocketAddr| {
            let mut w = [0u8; 4];
            w[0..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
            w[2..4].copy_from_slice(&a.port().to_be_bytes());
            u32::from_ne_bytes(w)
        };
        let ip = match good.ip() {
            IpAddr::V4(v4) => u32::from_ne_bytes(v4.octets()),
            IpAddr::V6(_) => panic!("IPv4 only"),
        };
        // (a `sockaddr_in` both threads see: family and port in the first word, the address in the second)
        let addr: Arc<[AtomicU32; 4]> =
            Arc::new([AtomicU32::new(word(good)), AtomicU32::new(ip), AtomicU32::new(0), AtomicU32::new(0)]);
        let done = Arc::new(AtomicBool::new(false));
        let flipper = {
            let (addr, done) = (addr.clone(), done.clone());
            let (g, b) = (word(good), word(bad));
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    addr[0].store(b, Ordering::Relaxed);
                    addr[0].store(g, Ordering::Relaxed);
                }
            })
        };
        let (mut ok, mut no) = (0, 0);
        for _ in 0..n {
            // SAFETY: a socket of ours, connected to the 16 bytes above (a race with the flipper on purpose:
            // the kernel and tarae read them as they are), closed after.
            unsafe {
                let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                assert!(fd >= 0, "a socket: {}", io::Error::last_os_error());
                let r = libc::connect(fd, addr.as_ptr().cast(), 16);
                match r {
                    0 => ok += 1,
                    _ => no += 1,
                }
                libc::close(fd);
            }
        }
        done.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        format!("race {ok} {no}")
    }

    /// This machine's ports as `llm.sandbox-ports` says: a connect and a datagram (`sendto`, `sendmsg`) to
    /// an open one reach it; a closed one, and an address beyond this machine (a documentation address —
    /// nothing is sent anywhere), are refused and said. A bind on this machine works, on every address too —
    /// bound to the loopback device; one on another address is refused.
    #[test]
    fn this_machines_ports_as_the_policy_says() {
        if !served("this_machines_ports_as_the_policy_says") {
            return;
        }
        let s = Scratch::new("ports");
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let udp_closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let [open_at, closed_at, udp_at, udp_closed_at] = [
            open.local_addr().unwrap(),
            closed.local_addr().unwrap(),
            udp.local_addr().unwrap(),
            udp_closed.local_addr().unwrap(),
        ];
        let p = s.policy(vec![open_at.port(), udp_at.port()], false, Vec::new());
        let ops = [
            format!("connect {open_at}"),
            format!("connect {closed_at}"),
            "connect 192.0.2.1:9".to_string(),
            format!("sendto {udp_at}"),
            format!("sendmsg {udp_at}"),
            format!("sendto {udp_closed_at}"),
            "sendmsg 192.0.2.1:9".to_string(),
            "bind 127.0.0.1:0".to_string(),
            "bind 0.0.0.0:0".to_string(),
            "bind 192.0.2.1:0".to_string(),
            // (netlink: a lookup asks it which addresses this machine has — its routing protocol only)
            "lookup localhost".to_string(),
            "netlink-route".to_string(),
            format!("netlink {}", libc::NETLINK_SOCK_DIAG),
        ];
        let got = probe_in(&p, &ops);
        let no = refused();
        // (routed — in the session's netns: a closed port is the session's, whose loopback refuses it and takes
        // a datagram; its own sockets, bound to no device)
        let routed = p.confine.as_ref().is_some_and(|c| c.as_ref().is_ok_and(|c| c.net == RunNet::Routed));
        let (closed_tcp, closed_udp, lo) = match routed {
            true => (format!("err {}", libc::ECONNREFUSED), "ok".to_string(), "ok "),
            false => (no.clone(), no.clone(), "ok lo"),
        };
        let want = ["ok", &closed_tcp, &no, "ok", "ok", &closed_udp, &no, lo, lo, &no, "ok", "ok", &no];
        assert_eq!(got, want, "{ops:?}");
        let mut b = [0u8; 4];
        assert_eq!(udp.recv(&mut b).map(|n| b[..n].to_vec()).unwrap(), b"x");
        assert_eq!(udp.recv(&mut b).map(|n| b[..n].to_vec()).unwrap(), b"m");
        let blocked = p.watch.as_ref().unwrap().blocked();
        let plain: Vec<String> = blocked.seen.iter().map(super::super::Denial::plain).collect();
        let closed_said = plain.contains(&format!("connecting to port {}", closed_at.port()));
        assert_eq!(
            closed_said, !routed,
            "a closed port refused by a rule only without the session's netns: {plain:?}"
        );
        assert!(plain.contains(&"connecting to 192.0.2.1:9".to_string()), "{plain:?}");
        // llm.sandbox-localhost: every port of this machine's
        let all = s.policy(Vec::new(), true, Vec::new());
        assert_eq!(probe_in(&all, &[format!("connect {closed_at}")]), ["ok"]);
    }

    /// A run's netlink reads the session's network and never changes it: an address added, a route deleted, a
    /// link made — each through another of the calls tarae makes for the run (`sendmsg`, `sendto`, `sendmmsg`) —
    /// are refused by tarae (`EPERM`, each said in the run's report by its type), and the links, addresses and
    /// routes after are the ones before; a lookup and a dump still answer. Why: the kernel judges a netlink
    /// message sent *with an address* by the sender's capability alone (`NETLINK_SKB_DST`), and tarae, who sends
    /// it, holds `CAP_NET_ADMIN` over the session's network namespace (2026-10-07,
    /// `target/sb/step6-checks-notes.md` §1b). Only where the run has a network namespace of its own
    /// (`own_netns`): a guard that let a change through then changes a namespace made for this test.
    #[test]
    fn a_runs_netlink_reads_but_never_changes_the_sessions_network() {
        if !served("a_runs_netlink_reads_but_never_changes_the_sessions_network") {
            return;
        }
        let s = Scratch::new("netlink");
        let p = s.policy(Vec::new(), false, Vec::new());
        if !own_netns(&p, "a_runs_netlink_reads_but_never_changes_the_sessions_network") {
            return;
        }
        let ops = [
            "net-state",
            "netlink-add-address",
            "netlink-delete-route",
            "netlink-add-link",
            "net-state",
            "lookup localhost",
            "netlink-route",
        ]
        .map(String::from);
        let got = probe_in(&p, &ops);
        let before = &got[0];
        assert!(
            before.contains("table 255 type 2 127.0.0.0/8"),
            "the route the run deletes is there: {before}"
        );
        let no = refused();
        let want = [no.as_str(), &no, &no, before, "ok", "ok"];
        assert_eq!(got[1..], want, "{ops:?}");
        // (tarae's refusals, not the kernel's: each said, by the message's type — new address, delete route,
        // new link)
        let said = netlink_said(&p);
        assert_eq!(said, ["message type 20", "message type 25", "message type 16"], "{said:?}");
    }

    /// A run's netlink reaches the kernel and nothing else: joining the groups rtnetlink asks the capability of
    /// (multicast-routing reports, 30 and 31), sending to a group (1, the links'), connecting to another
    /// socket's port — after which its own sends, which tarae never sees, would go there — and sending on a
    /// netlink socket of another protocol (socket diagnostics, made outside and handed in: a run can make none)
    /// are each refused by tarae, `EPERM`, said. Why: each is judged by the capability of whoever makes the call
    /// — `netlink_allowed`, `rtnetlink_bind` — and that's tarae (2026-10-07). Where the run has its own netns
    /// (`own_netns`: elsewhere tarae holds no capability to lend, so a broken guard wouldn't show).
    #[test]
    fn a_runs_netlink_reaches_only_the_kernel() {
        use std::os::fd::AsFd;
        if !served("a_runs_netlink_reaches_only_the_kernel") {
            return;
        }
        let s = Scratch::new("netlink-kernel");
        let p = s.policy(Vec::new(), false, Vec::new());
        if !own_netns(&p, "a_runs_netlink_reaches_only_the_kernel") {
            return;
        }
        // SAFETY: a plain call; a descriptor it makes is ours.
        let diag = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG)
        };
        assert!(diag >= 0, "a socket diagnostics socket: {}", io::Error::last_os_error());
        // SAFETY: just made, ours.
        let diag = unsafe { OwnedFd::from_raw_fd(diag) };
        let at = s.root.join("hand.sock");
        let hand = std::os::unix::net::UnixListener::bind(&at).unwrap();
        // (left to itself: a probe that never connects mustn't hold the test)
        std::thread::spawn(move || {
            let (c, _) = hand.accept().unwrap();
            super::super::handed::send_fds(&c, &[diag.as_fd()]);
        });
        let ops = [
            "netlink-bind-group 30".to_string(),
            "netlink-bind-group 31".to_string(),
            "netlink-sendto-group".to_string(),
            "netlink-connect-port".to_string(),
            format!("netlink-handed {}", at.display()),
        ];
        let got = probe_in(&p, &ops);
        assert_eq!(got, vec![refused(); 5], "{ops:?}");
        let said = netlink_said(&p);
        let port = format!("port {OTHER_PORT}, groups 0x0");
        let want = ["joining groups 0x20000000", "joining groups 0x40000000", "port 0, groups 0x1", &port];
        assert_eq!(said, [&want[..], &["protocol 4"]].concat(), "{said:?}");
    }

    /// A netlink batch past `SEND_MAX` fails whole, `EMSGSIZE` (`whole`): cut there, it would end mid-message
    /// and be refused as malformed, said for nothing. (`sendto` cut it before 2026-10-07; `sendmsg` never did.)
    #[test]
    fn a_netlink_batch_past_the_limit_fails_whole() {
        if !served("a_netlink_batch_past_the_limit_fails_whole") {
            return;
        }
        let s = Scratch::new("whole");
        let p = s.policy(Vec::new(), false, Vec::new());
        let got = probe_in(&p, &["netlink-oversized".to_string()]);
        assert_eq!(got, [format!("err {}", libc::EMSGSIZE)]);
    }

    /// Every socket but a stream sends a message whole or not at all — a datagram, a sequenced packet, a netlink
    /// batch (raw) — the same for `sendto` and `sendmsg`. (A unix sequenced packet past `SEND_MAX` fails either
    /// way from a run's side: the kernel refuses one over the socket's buffer, which `net.core.wmem_max` keeps
    /// far under it — measured 2026-10-07, so only this pins it.)
    #[test]
    fn a_message_goes_whole_on_every_socket_but_a_stream() {
        let kinds = [libc::SOCK_STREAM, libc::SOCK_DGRAM, libc::SOCK_SEQPACKET, libc::SOCK_RAW];
        assert_eq!(kinds.map(whole), [false, true, true, true]);
    }

    /// Whether the run of `p` has a network namespace of its own (Routed: `cargo xtask linux --userns`) — else
    /// `test` skips, said.
    fn own_netns(p: &Policy, test: &str) -> bool {
        let net = match &p.confine {
            Some(Ok(c)) => format!("{:?}", c.net),
            Some(Err(e)) => e.clone(),
            None => "no sandbox".into(),
        };
        if net != "Routed" {
            let line = format!(
                "{}sandbox::supervisor::tests::{test} — no network namespace for the run here ({net}; \
                 `cargo xtask linux --userns`)\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        }
        net == "Routed"
    }

    /// What tarae said it refused of the run of `p`'s netlink, in order.
    fn netlink_said(p: &Policy) -> Vec<String> {
        let blocked = p.watch.as_ref().unwrap().blocked();
        blocked.seen.iter().filter(|d| d.op == "network-netlink").map(|d| d.target.clone()).collect()
    }

    /// The address a connect names is read once and decided on tarae's copy: changed by another thread of
    /// the caller's all along, between a port that's open and one that isn't, no connection ever reaches
    /// the one that isn't — and both answers came (the race was on).
    #[test]
    fn a_changed_address_after_the_check_reaches_nothing() {
        if !served("a_changed_address_after_the_check_reaches_nothing") {
            return;
        }
        let s = Scratch::new("race");
        let (good, bad) = (
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        );
        let (good_at, bad_at) = (good.local_addr().unwrap(), bad.local_addr().unwrap());
        let count = |l: std::net::TcpListener, done: Arc<std::sync::atomic::AtomicBool>| {
            l.set_nonblocking(true).unwrap();
            std::thread::spawn(move || {
                let mut n = 0;
                loop {
                    match l.accept() {
                        Ok(_) => n += 1,
                        Err(_) if done.load(std::sync::atomic::Ordering::SeqCst) => return n,
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
                    }
                }
            })
        };
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (goods, bads) = (count(good, done.clone()), count(bad, done.clone()));
        let p = s.policy(vec![good_at.port()], false, Vec::new());
        let got = probe_in(&p, &[format!("race {good_at} {bad_at} 400")]);
        std::thread::sleep(std::time::Duration::from_millis(50));
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let (goods, bads) = (goods.join().unwrap(), bads.join().unwrap());
        let words: Vec<usize> = got[0].split(' ').skip(1).map(|w| w.parse().unwrap()).collect();
        assert_eq!(bads, 0, "a connection reached the port that isn't open ({})", got[0]);
        assert!(words[0] > 0 && words[1] > 0, "both answers came: {}", got[0]);
        assert_eq!(goods, words[0], "each connect that worked reached the open port");
    }

    /// Unix sockets where macOS allows them: in the project (by its path, from the run's folder too) and the
    /// run's own folder — made and reached — and one the user granted, that file; not one beside the project,
    /// nor through a link in the project to it; a socket can't take a protected name. An abstract name: refused
    /// without the session's netns; in it (D-f) the session's own — this machine's listener on that name isn't
    /// reached (the session has none: refused by its namespace).
    #[test]
    fn unix_sockets_where_macos_allows_them() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::UnixListener;
        if !served("unix_sockets_where_macos_allows_them") {
            return;
        }
        let s = Scratch::new("unix");
        std::fs::create_dir_all(s.base.join("granted")).unwrap();
        let (inside, out, granted) =
            (s.root.join("s.sock"), s.base.join("out/o.sock"), s.base.join("granted/g.sock"));
        let mut listeners: Vec<UnixListener> =
            [&inside, &out, &granted].map(|p| UnixListener::bind(p).unwrap()).into();
        let name = format!("tarae-test-{}", std::process::id());
        let abstract_at = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        listeners.push(UnixListener::bind_addr(&abstract_at).unwrap());
        std::os::unix::fs::symlink(&out, s.root.join("link.sock")).unwrap();
        let p = s.policy(Vec::new(), false, vec![granted.clone()]);
        let ops = [
            format!("unix {}", inside.display()),
            "unix s.sock".to_string(),
            format!("unix {}", granted.display()),
            format!("unix {}", out.display()),
            "unix link.sock".to_string(),
            format!("abstract {name}"),
            format!("unixbind {}", s.root.join("made.sock").display()),
            "unixbind-tmp r.sock".to_string(),
            format!("unixbind {}", s.root.join("Cargo.toml").display()),
            format!("unixbind {}", s.base.join("out/x.sock").display()),
        ];
        let got = probe_in(&p, &ops);
        let no = refused();
        let routed = p.confine.as_ref().is_some_and(|c| c.as_ref().is_ok_and(|c| c.net == RunNet::Routed));
        let abstract_said = if routed { format!("err {}", libc::ECONNREFUSED) } else { no.clone() };
        let want = ["ok", "ok", "ok", &no, &no, &abstract_said, "ok", "ok", &no, &no];
        assert_eq!(got, want, "{ops:?}");
        assert!(s.root.join("made.sock").exists() && !s.root.join("Cargo.toml").exists());
        assert!(!s.base.join("out/x.sock").exists());
        let blocked = p.watch.as_ref().unwrap().blocked();
        let plain: Vec<String> = blocked.seen.iter().map(super::super::Denial::plain).collect();
        // (as the words show it: home spelled `~` — a checkout in home, as on the user's VM)
        let out = super::super::home_shown(&out.display().to_string());
        assert!(plain.contains(&format!("connecting to socket {out}")), "{plain:?}");
        drop(listeners);
    }

    /// F1: a `sendmmsg` whose array starts where no message fits below the end of the address space is `EFAULT` to
    /// the run, as the kernel answers it (`wire::mmsg_at`), and the supervisor goes on serving: the next send arrives.
    #[test]
    fn a_sendmmsg_past_the_address_space_is_efault() {
        if !served("a_sendmmsg_past_the_address_space_is_efault") {
            return;
        }
        let s = Scratch::new("mmsg-top");
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let at = udp.local_addr().unwrap();
        let p = s.policy(vec![at.port()], false, Vec::new());
        let got = probe_in(&p, &["sendmmsg-top".to_string(), format!("sendto {at}")]);
        assert_eq!(got, [format!("err {}", libc::EFAULT), "ok".to_string()]);
        let mut buf = [0u8; 8];
        assert_eq!(udp.recv(&mut buf).map(|n| buf[..n].to_vec()).unwrap(), b"x");
    }

    /// F2: a unix socket bound in the project by a name that isn't UTF-8 is made by that name, byte for byte —
    /// the name checked is the name bound (it was made with U+FFFD in its place).
    #[test]
    fn a_unix_socket_is_bound_by_the_bytes_it_was_named() {
        use std::os::unix::ffi::OsStrExt;
        if !served("a_unix_socket_is_bound_by_the_bytes_it_was_named") {
            return;
        }
        let s = Scratch::new("unix-bytes");
        let p = s.policy(Vec::new(), false, Vec::new());
        let name = b"s\xff.sock";
        let hex: String = name.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(probe_in(&p, &[format!("unixbind-hex {hex}")]), ["ok"]);
        let made: Vec<Vec<u8>> = std::fs::read_dir(&s.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().as_bytes().to_vec())
            .filter(|n| n.ends_with(b".sock"))
            .collect();
        assert_eq!(
            made,
            [name.to_vec()],
            "{:?}",
            made.iter().map(|n| String::from_utf8_lossy(n)).collect::<Vec<_>>()
        );
    }

    /// For real: a Gradle build of a small project (no dependencies, so offline) in the sandbox — its client
    /// talks to a daemon of its own over this machine's ports and binds the socket its file locks share. The
    /// daemon is the runs' own: in a Gradle home kept with its daemon where tarae ends what runs leave (the Java
    /// pack's `run-home.gradle` `daemon`; a new project's first build starts it, ended here after), else in the
    /// run's own home with no daemon kept, where the project's other JVM options fork a single-use one. With
    /// `llm.sandbox-localhost` it builds; with this machine's ports closed it can't reach its daemon, and the
    /// report hears Java was refused a port. Skips without `gradle` and `java` on PATH.
    #[test]
    fn gradle_builds_in_the_sandbox() {
        if !served("gradle_builds_in_the_sandbox") {
            return;
        }
        let on_path = |name: &str| {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        };
        let (Some(gradle), Some(_)) = (on_path("gradle"), on_path("java")) else {
            let line = format!(
                "{}sandbox::supervisor::tests::gradle_builds_in_the_sandbox — no gradle and java on PATH\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        };
        let s = Scratch::new("gradle");
        for (f, text) in [
            ("settings.gradle", "rootProject.name = 'fixture'\n"),
            ("build.gradle", "plugins { id 'java' }\n"),
            ("gradle.properties", "org.gradle.jvmargs=-Xmx320m\n"),
            (
                "src/main/java/fx/Main.java",
                "package fx;\npublic class Main { public static void main(String[] a) {} }\n",
            ),
        ] {
            std::fs::create_dir_all(s.root.join(f).parent().unwrap()).unwrap();
            std::fs::write(s.root.join(f), text).unwrap();
        }
        // (a fresh clone: no `.gradle/` nor `build/` — the policy makes them first, from the Java pack's outputs)
        let build = |localhost: bool| {
            let llm = crate::llm::LlmConfig { sandbox_localhost: localhost, ..Default::default() };
            let mut p = super::super::policy(&s.root, &llm, &[]).unwrap();
            p.watch = super::super::watch_for(&p);
            let args = ["--offline", "--console=plain", "build"].map(String::from);
            let mut cmd = super::super::command(Some(&p), &gradle.to_string_lossy(), &args);
            cmd.current_dir(&s.root);
            let spawned = super::super::spawn(Some(&p), &cmd).expect("gradle starts");
            let mut err = spawned.stderr.unwrap();
            let reading = std::thread::spawn(move || {
                let mut e = String::new();
                io::Read::read_to_string(&mut err, &mut e).ok();
                e
            });
            let mut out = String::new();
            io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
            let status = crate::spawn::wait(spawned.pid).unwrap();
            let blocked = p.watch.as_ref().map(|w| w.blocked()).unwrap_or_default();
            // (whether its Gradle home is one kept with its daemon)
            let home = p.env.iter().find(|(k, _)| k == "GRADLE_USER_HOME").map(|(_, v)| PathBuf::from(v));
            let kept = home.is_some_and(|h| p.kept.iter().any(|k| k.daemon && k.home == h));
            (status.success(), format!("{out}\n{}", reading.join().unwrap()), blocked, kept)
        };
        let (built, said, _, kept) = build(true);
        assert!(built, "{said}");
        assert!(s.root.join("build/classes/java/main/fx/Main.class").exists(), "{said}");
        let daemon = if kept { "Starting a Gradle Daemon" } else { "single-use Daemon" };
        let (built, said_closed, blocked, _) = build(false);
        // (the daemons kept with the homes: this project's, ended)
        super::super::end_leftovers(Some(&s.root));
        assert!(said.contains(daemon), "it went through a daemon of its own (kept home: {kept}): {said}");
        let said = said_closed;
        // (routed — the session's netns: its daemon is the session's, reached on the session's loopback)
        if super::super::tier::get().ns.netns() {
            assert!(built, "this machine's ports closed, the daemon the session's own: {said}");
            return;
        }
        assert!(!built, "this machine's ports closed: {said}");
        assert!(
            blocked.seen.iter().any(|d| d.process == "java" && d.plain().starts_with("connecting to port")),
            "{blocked:?}\n{said}"
        );
    }

    /// 6f′: what collecting the copies costs on a long-lived run — a Gradle build (a JDK and Gradle on PATH: the M5
    /// image), twice, its daemon kept (no JVM options of the project's: one daemon both builds use, left running below
    /// the keeper), then 3 s with the daemon idle — the scans' count, time and descriptors read, in each part. By copies
    /// only (`cargo xtask linux --docker-default --image tarae-m5 test --bin tarae measure_copies_scan -- --ignored
    /// --nocapture`): the interval (`copies::INTERVAL`) is picked from it.
    #[test]
    #[ignore = "a measurement (6f′): by hand, by copies, where Gradle is"]
    fn measure_copies_scan() {
        use super::super::copies::SCANS;
        if !served("measure_copies_scan") || !super::copies() {
            return;
        }
        let s = Scratch::new("copies-scan");
        for (f, text) in [
            ("settings.gradle", "rootProject.name = 'fixture'\n"),
            ("build.gradle", "plugins { id 'java' }\n"),
            (
                "src/main/java/fx/Main.java",
                "package fx;\npublic class Main { public static void main(String[] a) {} }\n",
            ),
        ] {
            std::fs::create_dir_all(s.root.join(f).parent().unwrap()).unwrap();
            std::fs::write(s.root.join(f), text).unwrap();
        }
        let now = || {
            let r = |a: &AtomicU64| a.load(Relaxed);
            (r(&SCANS.count), r(&SCANS.nanos), r(&SCANS.descriptors))
        };
        let said = |part: &str, (c0, n0, d0): (u64, u64, u64), (c1, n1, d1): (u64, u64, u64), secs: f64| {
            let count = (c1 - c0).max(1);
            println!(
                "measure: copies scan · {part}: {} scans in {secs:.1} s ({:.1}/s), {:.0} µs each, {:.0} descriptors \
                 each — {:.2}% of a core · longest so far {:.0} µs",
                c1 - c0,
                (c1 - c0) as f64 / secs,
                (n1 - n0) as f64 / count as f64 / 1e3,
                (d1 - d0) as f64 / count as f64,
                (n1 - n0) as f64 / 1e9 / secs * 100.0,
                SCANS.longest.load(Relaxed) as f64 / 1e3,
            );
        };
        println!();
        for build in 1..=2 {
            let (at, before) = (std::time::Instant::now(), now());
            let (ms, ok, out) = sandboxed(&s.root, "gradle --offline --console=plain build");
            assert!(ok, "{out}");
            said(&format!("build {build} ({ms} ms)"), before, now(), at.elapsed().as_secs_f64());
        }
        let before = now();
        std::thread::sleep(std::time::Duration::from_secs(3));
        said("its daemon idle", before, now(), 3.0);
        super::super::end_leftovers(Some(&s.root));
    }

    /// `sh -c script` in the project at `root` as Claude's runs are (`sandbox::policy` → `command` → `spawn`,
    /// `llm.sandbox-localhost` on — Gradle reaches its daemon so): how long to its end in ms, whether it
    /// succeeded, its output's last lines.
    fn sandboxed(root: &Path, script: &str) -> (u128, bool, String) {
        let llm = crate::llm::LlmConfig { sandbox_localhost: true, ..Default::default() };
        sandboxed_under(root, script, &llm)
    }

    /// `sandboxed`, under `llm`'s settings. `$TARAE_SAY_REPORT`: a failed run's report printed too (`report:`,
    /// its first lines) — what Claude would read of it.
    fn sandboxed_under(root: &Path, script: &str, llm: &crate::llm::LlmConfig) -> (u128, bool, String) {
        sandboxed_with(root, script, llm, |_| {})
    }

    /// `sandboxed_under`, the policy `change`d first.
    fn sandboxed_with(
        root: &Path,
        script: &str,
        llm: &crate::llm::LlmConfig,
        change: impl FnOnce(&mut Policy),
    ) -> (u128, bool, String) {
        let mut p = super::super::policy(root, llm, &[]).unwrap();
        change(&mut p);
        p.watch = super::super::watch_for(&p);
        let at = std::time::Instant::now();
        // (every part's errors too, not only the last's)
        let mut cmd = super::super::command(Some(&p), "sh", &["-c".into(), format!("{{ {script}\n}} 2>&1")]);
        cmd.current_dir(root);
        let spawned = super::super::spawn(Some(&p), &cmd).expect("it starts");
        drop(spawned.stderr);
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).ok();
        let ok = crate::spawn::wait(spawned.pid).is_ok_and(|s| s.success());
        let took = at.elapsed().as_millis();
        if !ok && std::env::var_os("TARAE_SAY_REPORT").is_some() {
            let lines: Vec<(bool, String)> = out.lines().map(|l| (true, l.to_string())).collect();
            let report = super::super::Guard::In(Box::new(p.clone())).report(&lines);
            println!("report: {}", report.lines().take(6).collect::<Vec<_>>().join(" ⏎ "));
        }
        let tail: Vec<&str> = out.lines().rev().take(3).collect();
        (took, ok, tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
    }

    /// What the session's keeper of `root` holds after its runs: each process it left (its descendants that
    /// aren't a run going), by what it runs — `gradle-daemon`, `kotlin-daemon`, or its name.
    fn left_by(root: &Path) -> String {
        let Some(k) = super::super::keeper::of(Some(root)).into_iter().find(|k| !k.is_gone()) else {
            return "no keeper".into();
        };
        let mut seen = Vec::new();
        let mut next = k.leftovers();
        while let Some(p) = next.pop() {
            next.extend(crate::procs::children(p));
            let cmdline = std::fs::read(format!("/proc/{p}/cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline);
            let what = if cmdline.contains("GradleDaemon") {
                "gradle-daemon"
            } else if cmdline.contains("KotlinCompileDaemon") {
                "kotlin-daemon"
            } else {
                continue;
            };
            let parent = crate::procs::read(p).map_or(0, |o| o.ppid);
            seen.push(format!(
                "{what} (parent {})",
                if parent == k.pid { "the keeper".into() } else { parent.to_string() }
            ));
        }
        seen.sort();
        format!("{} · {}", k.said(), if seen.is_empty() { "nothing".into() } else { seen.join(", ") })
    }

    /// One of M4's runs: `gradle --offline --console=plain <task> --rerun` in the project at `root` through the
    /// real path (`sandbox::policy` → `command` → `spawn`, `llm.sandbox-localhost` on), with one change as
    /// `variant` says (step 4's plan §8): `v0` none; `v1` Landlock granting everything on `/`; `v2` unserved —
    /// the policy closed, so the keeper has the session's netns and the run its own sockets there (`Own`; None
    /// where there's no netns: unserved, Gradle couldn't reach its daemon); `v3` v1 with the user's own
    /// `~/.gradle` instead of the runs' kept home; `v4` gradle run directly, not in `sh -c '{ …\n} 2>&1'`.
    /// How long to its end in ms, whether it succeeded, its output's last lines, and the supervisors' calls
    /// meanwhile (the client's, and its daemon's — run 1's supervisor serves the daemon it left). Step 4's
    /// (M4 after): `v5` the policy closed (no `llm.sandbox-localhost`: the daemon is the session's own, its
    /// connects routed there without a look at the session's listeners); `v6` the real rules without the
    /// grants on the session's own device places (`session_devices`) — with `TARAE_TEST_NO_PID` (tier A, no
    /// fresh `/proc`), the Landlock cut of step 3's +0.2 s.
    fn m4_run(root: &Path, task: &str, variant: &str) -> Option<(u128, bool, String, Totals)> {
        use super::super::linux;
        let localhost = !matches!(variant, "v2" | "v5");
        let llm = crate::llm::LlmConfig { sandbox_localhost: localhost, ..Default::default() };
        let mut p = super::super::policy(root, &llm, &[]).unwrap();
        let abi = crate::landlock::abi().ok()?;
        match variant {
            "v6" => {
                let c = p.confine.take()?.ok()?;
                let r = linux::ruleset_for(c.net, abi).ok()?;
                let (serve, ptys) = (c.serve.clone(), c.ptys.clone());
                let own = linux::Own { ptys: ptys.as_deref(), ..Default::default() };
                let mut bare =
                    linux::confine_in(r, &p, Err("not watched".into()), true, serve, c.net, own).ok()?;
                bare.keeper = c.keeper;
                bare.ptys = ptys;
                p.confine = Some(Ok(bare));
            }
            "v1" | "v3" => {
                let c = p.confine.take()?.ok()?;
                let mut r = linux::ruleset_for(c.net, abi).ok()?;
                r.allow(Path::new("/"), crate::landlock::ALL).ok()?;
                p.confine = Some(Ok(c.with_rules(r)));
            }
            "v2" => {
                let k = p.confine.take()?.ok()?.keeper?;
                if !k.ns.net {
                    return None;
                }
                let unserved = Err("M4: unserved".into());
                let mut c =
                    linux::confine(&p, abi, Err("not watched".into()), true, Some(&k), unserved).ok()?;
                c.keeper = Some(k);
                p.confine = Some(Ok(c));
            }
            _ => {}
        }
        let args = ["--offline", "--console=plain", task, "--rerun"].map(String::from);
        let mut cmd = match variant {
            "v4" => super::super::command(Some(&p), "gradle", &args),
            _ => {
                let script = format!("{{ gradle {}\n}} 2>&1", args.join(" "));
                super::super::command(Some(&p), "sh", &["-c".into(), script])
            }
        };
        cmd.current_dir(root);
        if variant == "v3" {
            cmd.env("GRADLE_USER_HOME", Path::new(&std::env::var_os("HOME")?).join(".gradle"));
        }
        let before = super::totals();
        let at = std::time::Instant::now();
        let spawned = super::super::spawn(Some(&p), &cmd).ok()?;
        let mut err = spawned.stderr?;
        let reading = std::thread::spawn(move || {
            let mut e = String::new();
            io::Read::read_to_string(&mut err, &mut e).ok();
            e
        });
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout?, &mut out).ok();
        out.push_str(&reading.join().unwrap_or_default());
        let ok = crate::spawn::wait(spawned.pid).is_ok_and(|s| s.success());
        let took = at.elapsed().as_millis();
        let tail: Vec<&str> = out.lines().rev().take(3).collect();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" / ");
        Some((took, ok, tail, super::totals().since(before)))
    }

    /// M4 (`docs/design/sandbox-keeper.md` §Measured): `gradle --offline --console=plain $TARAE_M4_TASK --rerun`
    /// in `$TARAE_M4_PROJECT`, twice in a row through the session's real keeper, three pairs — what each pair's
    /// runs took, the supervisors' calls during run 2, and what the keeper holds after it (its daemons kept, its
    /// own children). `$TARAE_M4_VARIANT`: one change to the path (`m4_run`), `v0` without. Between pairs the
    /// keeper ends (`end_leftovers`): its daemons with it, run 1 cold. `TARAE_TEST_NO_NS`: without namespaces
    /// (tier C), A/B in one machine. Run by `target/sb/m4-keeper.sh` in a container with a JDK and Gradle.
    #[test]
    #[ignore = "a measurement (M4): run by target/sb/m4-keeper.sh"]
    fn measure_gradle_pairs() {
        let (Ok(project), Ok(task)) = (std::env::var("TARAE_M4_PROJECT"), std::env::var("TARAE_M4_TASK"))
        else {
            return;
        };
        let variant = std::env::var("TARAE_M4_VARIANT").unwrap_or_else(|_| "v0".into());
        let root = std::fs::canonicalize(project).unwrap();
        // (libtest's own `test … ...` ends here: each `measure:` starts its line)
        println!();
        for pair in 1..=3 {
            super::super::end_leftovers(Some(&root));
            let Some((one, ok1, said1, _)) = m4_run(&root, &task, &variant) else {
                println!("measure: {task} {variant}: not here (v2 needs the session's netns)");
                break;
            };
            let (two, ok2, said2, calls) = m4_run(&root, &task, &variant).unwrap();
            assert!(ok1 && ok2, "run 1: {said1}\nrun 2: {said2}");
            println!(
                "measure: {task} {variant} pair {pair}: run 1 {one} ms, run 2 {two} ms · run 2's calls: {} · \
                 after run 2: {}",
                calls.said(),
                left_by(&root)
            );
        }
        super::super::end_leftovers(Some(&root));
    }

    /// M8: tools that look at file owners, through the session's keeper (`$TARAE_M8` lines `<project>\t<script>`):
    /// how each ended and its last lines — A/B with `TARAE_TEST_NO_NS`. Inside a user namespace root's files show
    /// as `nobody`. Run by `target/sb/m4-keeper.sh`.
    #[test]
    #[ignore = "a measurement (M8): run by target/sb/m4-keeper.sh"]
    fn measure_tools_that_look_at_owners() {
        let Ok(cases) = std::env::var("TARAE_M8") else { return };
        println!();
        for line in cases.lines().filter(|l| !l.trim().is_empty()) {
            let (project, script) = line.split_once('\t').unwrap();
            let root = std::fs::canonicalize(project).unwrap();
            let (ms, ok, said) = sandboxed(&root, script);
            println!("measure: {script} — {} in {ms} ms: {said}", if ok { "ok" } else { "FAILED" });
            super::super::end_leftovers(Some(&root));
        }
    }

    /// Step 5c's measure (Q4, M4 through the proxy): each `$TARAE_M6` line `<name>\t<project>\t<script>` — a
    /// tool reaching the real network — run through the real path with the network open (so through tarae's
    /// proxy), twice: the resolvers' port 53 as D-c opens it, then closed (no resolver in the policy). How each
    /// ended, how long it took, its last lines, and how many calls named a resolver's port 53 while it was open —
    /// which tools look a name up even with a proxy, and which fail without. `TARAE_TEST_NO_NS`: tier C. Reaches
    /// the real network: run by hand on a VM (`target/sb/step5-notes.md`), never in `cargo test`.
    #[test]
    #[ignore = "a measurement (step 5c, Q4): run by hand on a VM"]
    fn measure_through_the_proxy() {
        let Ok(cases) = std::env::var("TARAE_M6") else { return };
        let llm = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
        println!();
        for line in cases.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.splitn(3, '\t');
            let (Some(name), Some(project), Some(script)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let root = std::fs::canonicalize(project).unwrap();
            let before = RESOLVER_CALLS.load(Relaxed);
            let (ms, ok, said) = sandboxed_with(&root, script, &llm, |_| {});
            let looked = RESOLVER_CALLS.load(Relaxed) - before;
            super::super::end_leftovers(Some(&root));
            let (ms_closed, ok_closed, said_closed) =
                sandboxed_with(&root, script, &llm, |p| p.resolvers.clear());
            super::super::end_leftovers(Some(&root));
            let ended = |ok| if ok { "ok" } else { "FAILED" };
            println!(
                "measure: Q4 {name}: port 53 open {} in {ms} ms ({looked} calls to a resolver): {said} · closed {} \
                 in {ms_closed} ms: {said_closed}",
                ended(ok),
                ended(ok_closed),
            );
        }
    }

    /// M5: name lookups — and a connection after them — with the network open, through the session's keeper
    /// (`$TARAE_M5` lines `<name>\t<project>\t<script>`), each beside the same script run without the sandbox:
    /// how each ended and its last lines, and the supervisors' calls (A/B with `TARAE_TEST_NO_NS`). Inside the
    /// session's netns only `lo` is configured, which `AI_ADDRCONFIG` may read as "no address of either
    /// family". Reaches the real network: run by `target/sb/m5-lookups.sh` on a VM, never in `cargo test`.
    #[test]
    #[ignore = "a measurement (M5): run by target/sb/m5-lookups.sh"]
    fn measure_lookups() {
        let Ok(cases) = std::env::var("TARAE_M5") else { return };
        let llm = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
        println!();
        for line in cases.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.splitn(3, '\t');
            let (Some(name), Some(project), Some(script)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let root = std::fs::canonicalize(project).unwrap();
            let before = totals();
            let (ms, ok, said) = sandboxed_under(&root, script, &llm);
            let calls = totals().since(before);
            let mut bare = std::process::Command::new("sh");
            bare.args(["-c", &format!("{{ {script}\n}} 2>&1")]);
            let bare = bare.current_dir(&root).output().map(|o| {
                let out = String::from_utf8_lossy(&o.stdout).into_owned();
                let tail: Vec<&str> = out.lines().rev().take(2).collect();
                format!("{} {}", if o.status.success() { "ok" } else { "FAILED" }, tail.join(" / "))
            });
            println!(
                "measure: M5 {name}: sandboxed {} in {ms} ms: {said} · calls: {} · unsandboxed {}",
                if ok { "ok" } else { "FAILED" },
                calls.said(),
                bare.unwrap_or_else(|e| e.to_string())
            );
            super::super::end_leftovers(Some(&root));
        }
    }
}
