//! What a run reaches by address on Linux, decided by tarae: the seccomp filter's served mode
//! (`seccomp::Mode::Served`) hands tarae every call that names one, and this answers it — as macOS's
//! profile has it (`Policy::profile`): this machine's ports as `llm.sandbox-localhost` and `-ports` say,
//! listening on this machine only, unix sockets in the project and the run's own folder, and the ones the
//! user granted, each that one file.
//!
//! The safe form only: a call's address is read once from the caller's memory (`Caller::read`), decided on
//! tarae's own copy, and the call is done by tarae — on the caller's own socket, taken with `pidfd_getfd` —
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
//! What it needs from the system (`unavailable` says what's missing): a seccomp listener (none inside
//! another one's filter — a sandboxed tarae) and `pidfd_getfd` (Docker's default filter refuses it without
//! `CAP_SYS_PTRACE`). Reading a run's memory takes what ptrace takes: under Yama's `ptrace_scope` 1 (the
//! default on Ubuntu) a process that left tarae's tree — reparented away — gets every such call refused; at
//! 2 without `CAP_SYS_PTRACE`, or 3, no run can be served (`yama_allows`).

use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Denial, Policy};

/// A call's answer: what it returns, or the errno it fails with.
type Answer = Result<i64, i32>;

/// The longest address a call names (`struct sockaddr_storage`).
const ADDR_MAX: usize = 128;
/// The most of a message tarae copies: a datagram longer fails as the kernel's own limit would
/// (`EMSGSIZE`); a stream's is sent in part, as a send may be.
const SEND_MAX: usize = 1 << 20;
/// The most control data (`SCM_RIGHTS` …) a message carries.
const CONTROL_MAX: usize = 1 << 16;
/// `struct msghdr` and `struct mmsghdr` on the 64-bit architectures, and where `msg_len` sits in the latter.
const MSGHDR: usize = 56;
const MMSGHDR: usize = 64;
const MSG_LEN_AT: u64 = 56;

/// Why tarae can't serve its runs' calls here — None when it can. Asked once (a worker's: it starts a thread).
pub fn unavailable() -> Option<String> {
    static WHY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    WHY.get_or_init(|| probe().err()).clone()
}

/// A listener on a filter of a short-lived thread's own (refused under another listener: `EBUSY`), and a
/// descriptor of tarae's own taken through its pidfd (a container's filter refuses `pidfd_getfd`).
fn probe() -> Result<(), String> {
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
    listener.map_err(|e| match e.raw_os_error() {
        Some(libc::EBUSY) => "tarae itself runs under another supervisor's filter".to_string(),
        _ => format!("no seccomp listener: {e}"),
    })?;
    // SAFETY: getpid has no preconditions.
    let pidfd = pidfd_open(unsafe { libc::getpid() }).map_err(|e| format!("pidfd_open: {e}"))?;
    pidfd_getfd(&pidfd, pidfd.as_raw_fd()).map(drop).map_err(|e| format!("pidfd_getfd: {e}"))?;
    let scope = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok();
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    yama_allows(scope.as_deref(), &status)
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
/// was when the run started (nothing comes mid-run), and where its refusals go out (`Refused`).
pub struct Served {
    policy: Arc<Policy>,
    refused: std::sync::mpsc::Sender<Denial>,
}

impl std::ops::Deref for Served {
    type Target = Policy;
    fn deref(&self) -> &Policy {
        &self.policy
    }
}

/// Starts the supervisor of one run, on a thread of its own made from the calling thread (unconfined: a
/// thread a confined one made would be confined too, and its own calls would wait on itself). It waits for
/// the run's listener (taken from the run's process: `sandbox::keeper`), then answers that run's calls until no
/// process of it is left, its refusals out on `refused`. The sender dropped unsent: it ends.
pub fn serve(
    policy: Arc<Policy>,
    refused: std::sync::mpsc::Sender<Denial>,
) -> io::Result<std::sync::mpsc::SyncSender<OwnedFd>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<OwnedFd>(1);
    let policy = Arc::new(Served { policy, refused });
    std::thread::Builder::new().name("tarae-serve".into()).spawn(move || {
        quiet_signals();
        let Ok(fd) = rx.recv() else { return };
        let l = Arc::new(Listener(fd));
        while let Some(n) = l.next() {
            match n.data.nr as libc::c_long {
                // (quick: answered here)
                libc::SYS_socket => l.answer(n.id, socket(&l, &n)),
                libc::SYS_bind => l.answer(n.id, bind(&l, &n, &policy)),
                // (a connect or a send may wait — as long as the caller would have)
                _ => {
                    let (on, p) = (l.clone(), policy.clone());
                    let spawned = std::thread::Builder::new()
                        .name("tarae-serve".into())
                        .spawn(move || on.answer(n.id, call(&on, &n, &p)));
                    if spawned.is_err() {
                        l.answer(n.id, Err(libc::EAGAIN));
                    }
                }
            }
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
        _ => Err(libc::ENOSYS),
    }
}

/// The run's seccomp listener.
struct Listener(OwnedFd);

/// `SECCOMP_IOCTL_NOTIF_ID_VALID` as kernels before 5.17 numbered it (`_IOR`) — later ones take it too.
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

    /// Puts `fd` among call `id`'s caller's descriptors: the number it got there.
    fn add_fd(&self, id: u64, fd: &OwnedFd, cloexec: bool) -> io::Result<i64> {
        let mut add = libc::seccomp_notif_addfd {
            id,
            flags: 0,
            srcfd: fd.as_raw_fd() as u32,
            newfd: 0,
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

/// An address a call named, as tarae read it.
#[derive(Debug, PartialEq)]
enum Addr {
    /// `AF_UNSPEC`: a datagram socket's disconnect, or where it's connected.
    Unspec,
    Ip(SocketAddr),
    /// A unix socket by its path, as named (relative: from the caller's folder).
    Path(PathBuf),
    /// An abstract unix name (none: an unbound socket's own made up).
    Abstract(Vec<u8>),
    /// A unix address with no name at all.
    Unnamed,
    Netlink,
    Other,
}

/// `bytes` (a `struct sockaddr`) read as the kernel reads them; too short for its family: `EINVAL`.
fn parse(bytes: &[u8]) -> Result<Addr, i32> {
    if bytes.len() < 2 {
        return Err(libc::EINVAL);
    }
    let b = |at: usize, len: usize| bytes.get(at..at + len).ok_or(libc::EINVAL);
    let port = |at| b(at, 2).map(|p| u16::from_be_bytes([p[0], p[1]]));
    Ok(match i32::from(u16::from_ne_bytes([bytes[0], bytes[1]])) {
        libc::AF_UNSPEC => Addr::Unspec,
        libc::AF_INET => {
            let ip: [u8; 4] = b(4, 4)?.try_into().map_err(|_| libc::EINVAL)?;
            Addr::Ip(SocketAddr::new(IpAddr::from(ip), port(2)?))
        }
        libc::AF_INET6 => {
            let ip: [u8; 16] = b(8, 16)?.try_into().map_err(|_| libc::EINVAL)?;
            Addr::Ip(SocketAddr::new(IpAddr::from(ip), port(2)?))
        }
        libc::AF_UNIX => match &bytes[2..] {
            [] => Addr::Unnamed,
            [0, name @ ..] => Addr::Abstract(name.to_vec()),
            path => {
                let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                Addr::Path(PathBuf::from(OsStr::from_bytes(&path[..end])))
            }
        },
        libc::AF_NETLINK => Addr::Netlink,
        _ => Addr::Other,
    })
}

/// This machine: a loopback address (`::ffff:127.0.0.1` too), or none (`0.0.0.0` and `::` — a bind to
/// every address, which on a loopback-bound socket hears this machine only; a connect there is this machine).
fn this_machine(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    ip.is_loopback() || ip.is_unspecified()
}

/// Whether a run may connect or send to `a`: this machine, on a port `llm.sandbox-localhost` or `-ports`
/// opens — macOS's rule. `llm.sandbox-network` opens nothing beyond this machine here.
fn may_reach(p: &Policy, a: SocketAddr) -> bool {
    this_machine(a.ip()) && (p.localhost || p.ports.contains(&a.port()))
}

/// A refusal: `Refused` out (tarae says it in the run's report — its watch, once the run is gone its session's
/// current run's: `sandbox::tell`), `EPERM` to the caller — "Operation not permitted", as macOS says it.
fn refuse(p: &Served, c: &Caller, op: &str, target: String) -> Answer {
    let _ = p.refused.send(Denial { process: c.name(), op: op.into(), target });
    Err(libc::EPERM)
}

/// An integer socket option of `fd`'s.
fn sockopt(fd: &OwnedFd, name: libc::c_int) -> Result<i32, i32> {
    let (mut v, mut len) = (0i32, std::mem::size_of::<i32>() as libc::socklen_t);
    // SAFETY: an int and its length, live for the call.
    let r =
        unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, name, (&raw mut v).cast(), &mut len) };
    if r < 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    Ok(v)
}

/// `socket()` for an internet family: a stream or datagram socket (TCP, UDP — not raw, ICMP's, SCTP or
/// MPTCP, none of a build's), made by tarae, bound to the loopback device, and put among the caller's
/// descriptors.
fn socket(l: &Listener, n: &libc::seccomp_notif) -> Answer {
    let [family, kind, protocol] = [0, 1, 2].map(|i| n.data.args[i] as i32);
    let flags = kind & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC);
    let base = kind & !flags;
    let ok = match base {
        libc::SOCK_STREAM => matches!(protocol, 0 | libc::IPPROTO_TCP),
        libc::SOCK_DGRAM => matches!(protocol, 0 | libc::IPPROTO_UDP),
        _ => false,
    };
    if !matches!(family, libc::AF_INET | libc::AF_INET6) || !ok {
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
    let lo = b"lo\0";
    // SAFETY: the device's name and its length, live for the call.
    let r = unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, lo.as_ptr().cast(), 3)
    };
    result(r as isize)?;
    l.add_fd(n.id, &fd, flags & libc::SOCK_CLOEXEC != 0).map_err(|e| errno(&e))
}

/// The address a call names (`at`, `len` bytes), read once.
fn address(c: &Caller, at: u64, len: u64) -> Result<Vec<u8>, i32> {
    match usize::try_from(len) {
        Ok(len) if len <= ADDR_MAX => c.read(at, len),
        _ => Err(libc::EINVAL),
    }
}

/// A unix socket's path as an address (`/proc/self/fd/N`, tarae's own descriptor of it).
fn unix_address(path: &str) -> Result<Vec<u8>, i32> {
    if path.len() >= 108 {
        return Err(libc::ENAMETOOLONG);
    }
    let mut a = (libc::AF_UNIX as u16).to_ne_bytes().to_vec();
    a.extend_from_slice(path.as_bytes());
    a.push(0);
    Ok(a)
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
    let a = unix_address(&format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    Ok((fd, a))
}

/// `connect(fd, addr, len)`: an internet socket to this machine's open ports, a unix one to a socket it may
/// reach (`unix_target`), netlink as asked — done on the caller's own socket, waiting as the caller would.
fn connect(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let addr = address(&c, args[1], args[2])?;
    let sock = c.fd(args[0])?;
    let (to, _held) = match (sockopt(&sock, libc::SO_DOMAIN)?, parse(&addr)?) {
        (libc::AF_INET | libc::AF_INET6, Addr::Unspec) => (addr, None),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) if may_reach(p, a) => (addr, None),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) => {
            return refuse(p, &c, "network-outbound", format!("remote:{a}"));
        }
        (libc::AF_INET | libc::AF_INET6, _) => return Err(libc::EAFNOSUPPORT),
        (libc::AF_UNIX, Addr::Path(named)) => {
            let (fd, a) = unix_target(p, &c, &named)?;
            (a, Some(fd))
        }
        (libc::AF_UNIX, Addr::Abstract(name)) => {
            return refuse(p, &c, "network-outbound", format!("@{}", String::from_utf8_lossy(&name)));
        }
        (libc::AF_UNIX, Addr::Unspec) => (addr, None),
        (libc::AF_UNIX, _) => return Err(libc::EINVAL),
        (libc::AF_NETLINK, _) => (addr, None),
        _ => return Err(libc::EPERM),
    };
    // SAFETY: the socket and an address of ours, live for the call.
    let r = unsafe { libc::connect(sock.as_raw_fd(), to.as_ptr().cast(), to.len() as libc::socklen_t) };
    result(r as isize)
}

/// `bind(fd, addr, len)`: an internet socket on this machine (any port — a test's own server), a unix one
/// in the project or the run's own folder where a run may write (`Policy::writes_allow`: not a protected
/// name), netlink as asked. Abstract unix names are refused.
fn bind(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let addr = address(&c, args[1], args[2])?;
    let sock = c.fd(args[0])?;
    let (to, _held) = match (sockopt(&sock, libc::SO_DOMAIN)?, parse(&addr)?) {
        // (`AF_UNSPEC` is taken for every address only — the kernel's compatibility rule)
        (libc::AF_INET | libc::AF_INET6, Addr::Unspec) => (addr, None),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) if this_machine(a.ip()) => (addr, None),
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
            let a = unix_address(&format!("/proc/self/fd/{}/{}", dir.as_raw_fd(), name.to_string_lossy()))?;
            (a, Some(dir))
        }
        (libc::AF_UNIX, Addr::Abstract(name)) => {
            return refuse(p, &c, "network-bind", format!("@{}", String::from_utf8_lossy(&name)));
        }
        (libc::AF_UNIX, _) => return refuse(p, &c, "network-bind", "an unnamed unix socket".into()),
        (libc::AF_NETLINK, _) => (addr, None),
        _ => return Err(libc::EPERM),
    };
    // SAFETY: the socket and an address of ours, live for the call.
    let r = unsafe { libc::bind(sock.as_raw_fd(), to.as_ptr().cast(), to.len() as libc::socklen_t) };
    result(r as isize)
}

/// Where a send names (`addr`, read once), on a socket of `domain`: the address to send to — tarae's copy,
/// or a unix socket's through tarae's own descriptor (held with it) — or refused as `connect` would be.
fn destination(
    p: &Served,
    c: &Caller,
    domain: i32,
    addr: Vec<u8>,
) -> Result<(Vec<u8>, Option<OwnedFd>), i32> {
    match (domain, parse(&addr)?) {
        (libc::AF_INET | libc::AF_INET6, Addr::Unspec) => Ok((addr, None)),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) if may_reach(p, a) => Ok((addr, None)),
        (libc::AF_INET | libc::AF_INET6, Addr::Ip(a)) => {
            refuse(p, c, "network-outbound", format!("remote:{a}")).map(|_| (Vec::new(), None))
        }
        (libc::AF_INET | libc::AF_INET6, _) => Err(libc::EAFNOSUPPORT),
        (libc::AF_UNIX, Addr::Path(named)) => unix_target(p, c, &named).map(|(fd, a)| (a, Some(fd))),
        (libc::AF_UNIX, Addr::Abstract(name)) => {
            let target = format!("@{}", String::from_utf8_lossy(&name));
            refuse(p, c, "network-outbound", target).map(|_| (Vec::new(), None))
        }
        (libc::AF_UNIX, _) => Err(libc::EINVAL),
        (libc::AF_NETLINK, _) => Ok((addr, None)),
        _ => Err(libc::EPERM),
    }
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
    let sock = c.fd(args[0])?;
    let datagram = sockopt(&sock, libc::SO_TYPE)? == libc::SOCK_DGRAM;
    let (to, _held) = destination(p, &c, sockopt(&sock, libc::SO_DOMAIN)?, addr)?;
    let data = payload(&c, args[1], args[2], datagram)?;
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

/// One `struct msghdr` of the caller's, read once: where it goes (if it names it), what it carries, and its
/// control data with the descriptors it passes (`SCM_RIGHTS`) taken as tarae's own — passing on what the
/// run holds. Credentials (`SCM_CREDENTIALS`) are refused: tarae can't speak for the run's process.
struct Msg {
    name: Option<Vec<u8>>,
    data: Vec<u8>,
    control: Vec<u8>,
    _fds: Vec<OwnedFd>,
}

fn read_msg(c: &Caller, at: u64, datagram: bool) -> Result<Msg, i32> {
    let h = c.read(at, MSGHDR)?;
    let word = |o: usize| u64::from_ne_bytes(h[o..o + 8].try_into().unwrap_or_default());
    let (name_at, name_len) =
        (word(0), u64::from(u32::from_ne_bytes(h[8..12].try_into().unwrap_or_default())));
    let (iov_at, iov_len, control_at, control_len) = (word(16), word(24), word(32), word(40));
    let name = match (name_at, name_len) {
        (0, _) | (_, 0) => None,
        (a, len) => Some(address(c, a, len)?),
    };
    if iov_len > libc::UIO_MAXIOV as u64 {
        return Err(libc::EMSGSIZE);
    }
    let iov = c.read(iov_at, iov_len as usize * 16)?;
    let mut data = Vec::new();
    // (`struct iovec`: where, how long)
    for v in iov.as_chunks::<16>().0 {
        let (base, len) = v.split_at(8);
        let base = u64::from_ne_bytes(base.try_into().unwrap_or_default());
        let len = u64::from_ne_bytes(len.try_into().unwrap_or_default());
        let room = (SEND_MAX - data.len()) as u64;
        if len > room && datagram {
            return Err(libc::EMSGSIZE);
        }
        data.extend(c.read(base, len.min(room) as usize)?);
        if data.len() == SEND_MAX {
            break;
        }
    }
    let control_len = usize::try_from(control_len).unwrap_or(usize::MAX);
    if control_len > CONTROL_MAX {
        return Err(libc::ENOBUFS);
    }
    let mut control = if control_at == 0 { Vec::new() } else { c.read(control_at, control_len)? };
    let mut fds = Vec::new();
    // `struct cmsghdr`: its length (with itself), level, type, then the data — each aligned to 8
    let mut o = 0;
    while o + 16 <= control.len() {
        let len = usize::try_from(u64::from_ne_bytes(control[o..o + 8].try_into().unwrap_or_default()))
            .unwrap_or(usize::MAX);
        if len < 16 || len > control.len() - o {
            return Err(libc::EINVAL);
        }
        let level = i32::from_ne_bytes(control[o + 8..o + 12].try_into().unwrap_or_default());
        let kind = i32::from_ne_bytes(control[o + 12..o + 16].try_into().unwrap_or_default());
        match (level, kind) {
            (libc::SOL_SOCKET, libc::SCM_RIGHTS) => {
                for at in (o + 16..o + len).step_by(4).filter(|at| at + 4 <= o + len) {
                    let theirs = i32::from_ne_bytes(control[at..at + 4].try_into().unwrap_or_default());
                    let ours = c.fd(theirs as u64)?;
                    control[at..at + 4].copy_from_slice(&ours.as_raw_fd().to_ne_bytes());
                    fds.push(ours);
                }
            }
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS) => return Err(libc::EPERM),
            _ => {}
        }
        o += len.div_ceil(8) * 8;
    }
    Ok(Msg { name, data, control, _fds: fds })
}

/// One message sent on `sock` as the caller asked — where it names refused as a `sendto` there would be.
fn send_one(p: &Served, c: &Caller, sock: &OwnedFd, domain: i32, m: Msg, flags: i32) -> Answer {
    let (to, _held) = match m.name {
        Some(addr) => destination(p, c, domain, addr)?,
        None => (Vec::new(), None),
    };
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
    c.sigpipe(flags, result(r))
}

/// `sendmsg(fd, msg, flags)` — every one, as only the message says where it goes.
fn sendmsg(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let sock = c.fd(args[0])?;
    let domain = sockopt(&sock, libc::SO_DOMAIN)?;
    let m = read_msg(&c, args[1], sockopt(&sock, libc::SO_TYPE)? != libc::SOCK_STREAM)?;
    send_one(p, &c, &sock, domain, m, args[2] as i32)
}

/// `sendmmsg(fd, msgs, n, flags)`: each message as `sendmsg`, its length written back where the caller
/// reads it (`msg_len` — glibc's resolver checks it); the first that fails ends it (failing the call when
/// it's the first).
fn sendmmsg(l: &Listener, n: &libc::seccomp_notif, p: &Served) -> Answer {
    let c = Caller::open(l, n)?;
    let args = n.data.args;
    let sock = c.fd(args[0])?;
    let domain = sockopt(&sock, libc::SO_DOMAIN)?;
    let datagram = sockopt(&sock, libc::SO_TYPE)? != libc::SOCK_STREAM;
    let count = args[2].min(libc::UIO_MAXIOV as u64);
    let mut sent = 0;
    for i in 0..count {
        let at = args[1] + i * MMSGHDR as u64;
        let r = read_msg(&c, at, datagram).and_then(|m| send_one(p, &c, &sock, domain, m, args[3] as i32));
        match r {
            Ok(len) => {
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

    /// This machine is loopback (mapped into IPv6 too) and the every-address binds; a connect there takes a
    /// port `llm.sandbox-ports` names or `llm.sandbox-localhost`; nothing beyond this machine, whatever's open.
    #[test]
    fn only_this_machines_open_ports_are_reached() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        for here in ["127.0.0.1:1", "127.8.9.10:1", "[::1]:1", "[::ffff:127.0.0.1]:1", "0.0.0.0:1", "[::]:1"]
        {
            assert!(this_machine(at(here).ip()), "{here}");
        }
        for away in ["10.0.0.5:1", "8.8.8.8:1", "[fe80::1]:1", "[::ffff:10.0.0.1]:1", "[2001:db8::1]:1"] {
            assert!(!this_machine(at(away).ip()), "{away}");
        }
        let p = Policy { ports: vec![8080], ..Policy::for_project(&super::super::Env::default()) };
        assert!(may_reach(&p, at("127.0.0.1:8080")) && may_reach(&p, at("[::1]:8080")));
        assert!(!may_reach(&p, at("127.0.0.1:8081")), "a port not opened");
        let all = Policy { localhost: true, network: true, ..p };
        assert!(may_reach(&all, at("127.0.0.1:2375")), "localhost opens them all, as on macOS");
        assert!(!may_reach(&all, at("8.8.8.8:53")), "the network beyond stays closed here");
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
        let script = r#"( bash -c 'sleep 1
            if exec 3<>/dev/tcp/127.0.0.1/$PORT; then r=ok; else r=no; fi
            echo "$r $(cut -d" " -f4 /proc/$$/stat) $$" > "$OUT"' & )
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
        let keeper = match &p.confine {
            Some(Ok(c)) => c.keeper.as_ref().map(|k| k.pid),
            _ => None,
        };
        let mine = keeper.is_some_and(|k| parent.is_some_and(|p| *p == k.to_string()));
        // (the orphan, a zombie child of the keeper's once it's done: reaped)
        let gone = || child.is_none_or(|c| !std::path::Path::new(&format!("/proc/{c}")).exists());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while mine && !gone() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let reaped = mine && gone();
        // (the long one, a daemon of sorts: the session's to end — as at its session's end)
        let long: Option<u32> =
            std::fs::read_to_string(out.with_extension("long")).ok().and_then(|t| t.trim().parse().ok());
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
    /// subreaper, as it is): run 1 leaves a child that, each time a file says so, connects to a port the
    /// policy doesn't open. Told while run 2 of the project goes, the refusal is run 2's (its report); told
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
            ( : <>/dev/tcp/127.0.0.1/9 ) 2>/dev/null; done' & ); exit 0"#;
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
            std::fs::read_to_string(&file).unwrap().trim().parse().unwrap()
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
                ["unix", path] => {
                    said(UnixStream::connect(path).and_then(|mut s| io::Write::write_all(&mut s, b"hi")))
                }
                ["unixbind", path] => said(UnixListener::bind(path)),
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
                ["lookup", name] => lookup(name),
                ["netlink-route"] => said(netlink_route()),
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
        let want = ["ok", &no, &no, "ok", "ok", &no, &no, "ok lo", "ok lo", &no, "ok", "ok", &no];
        assert_eq!(got, want, "{ops:?}");
        let mut b = [0u8; 4];
        assert_eq!(udp.recv(&mut b).map(|n| b[..n].to_vec()).unwrap(), b"x");
        assert_eq!(udp.recv(&mut b).map(|n| b[..n].to_vec()).unwrap(), b"m");
        let blocked = p.watch.as_ref().unwrap().blocked();
        let plain: Vec<String> = blocked.seen.iter().map(super::super::Denial::plain).collect();
        assert!(plain.contains(&format!("connecting to port {}", closed_at.port())), "{plain:?}");
        assert!(plain.contains(&"connecting to 192.0.2.1:9".to_string()), "{plain:?}");
        // llm.sandbox-localhost: every port of this machine's
        let all = s.policy(Vec::new(), true, Vec::new());
        assert_eq!(probe_in(&all, &[format!("connect {closed_at}")]), ["ok"]);
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
    /// nor through a link in the project to it, nor an abstract name; a socket can't take a protected name.
    #[test]
    fn unix_sockets_where_macos_allows_them() {
        use std::os::unix::net::UnixListener;
        if !served("unix_sockets_where_macos_allows_them") {
            return;
        }
        let s = Scratch::new("unix");
        std::fs::create_dir_all(s.base.join("granted")).unwrap();
        let (inside, out, granted) =
            (s.root.join("s.sock"), s.base.join("out/o.sock"), s.base.join("granted/g.sock"));
        let listeners: Vec<UnixListener> =
            [&inside, &out, &granted].map(|p| UnixListener::bind(p).unwrap()).into();
        std::os::unix::fs::symlink(&out, s.root.join("link.sock")).unwrap();
        let p = s.policy(Vec::new(), false, vec![granted.clone()]);
        let ops = [
            format!("unix {}", inside.display()),
            "unix s.sock".to_string(),
            format!("unix {}", granted.display()),
            format!("unix {}", out.display()),
            "unix link.sock".to_string(),
            "abstract tarae-test".to_string(),
            format!("unixbind {}", s.root.join("made.sock").display()),
            "unixbind-tmp r.sock".to_string(),
            format!("unixbind {}", s.root.join("Cargo.toml").display()),
            format!("unixbind {}", s.base.join("out/x.sock").display()),
        ];
        let got = probe_in(&p, &ops);
        let no = refused();
        let want = ["ok", "ok", "ok", &no, &no, &no, "ok", "ok", &no, &no];
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

    /// For real: a Gradle build of a small project (no dependencies, so offline) in the sandbox — its client
    /// talks to a daemon of its own over this machine's ports (the project asks other JVM options, so a
    /// single-use daemon is forked) and binds the socket its file locks share. With
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
            let mut p = super::super::policy(&s.root, &llm, &[]);
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
            (status.success(), format!("{out}\n{}", reading.join().unwrap()), blocked)
        };
        let (built, said, _) = build(true);
        assert!(built, "{said}");
        assert!(s.root.join("build/classes/java/main/fx/Main.class").exists(), "{said}");
        assert!(said.contains("single-use Daemon"), "it went through a daemon of its own: {said}");
        let (built, said, blocked) = build(false);
        assert!(!built, "this machine's ports closed: {said}");
        assert!(
            blocked.seen.iter().any(|d| d.process == "java" && d.plain().starts_with("connecting to port")),
            "{blocked:?}\n{said}"
        );
    }
}
