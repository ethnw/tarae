//! The seccomp filter Claude's runs get on Linux beside Landlock (`landlock.rs`): Landlock keeps files in
//! their place, this keeps the run off sockets. A run makes no socket but netlink's routing one (the
//! interfaces a name lookup asks about — not its others: socket diagnostics lists every socket on the
//! machine) — no internet socket, and no unix one either: through one it could reach the user's
//! D-Bus session bus (and have systemd start a unit, outside the sandbox), `docker.sock` or the ssh-agent,
//! where macOS's profile refuses them (`sandbox.rs`). `socketpair` stays: pipes between a run's own
//! processes. io_uring is refused (its rings make sockets without `socket()`), and so is every call of
//! another ABI than the native one (i386's `socketcall` on x86-64, x32's numbers) — the filter knows only the
//! native numbers.
//!
//! Served (`Mode::Served`), the calls that name an address go to tarae instead (`sandbox/supervisor.rs`):
//! `socket()` for the internet families, `connect`, `bind`, `sendmsg`, `sendmmsg` and a `sendto` with an
//! address — tarae reads the address, decides, and does the call itself; a unix stream socket may be made
//! (what it reaches is tarae's to say, at its `connect`). Not served (no supervisor here), a unix datagram
//! pair is refused too: its `sendto` reaches any datagram socket by path (journald's, `/dev/log`).
//!
//! Where tarae runs under an AppArmor profile with Ubuntu's restriction on (`tier::runs_refuse_userns`), a run
//! makes no user namespace (Q4): `unshare` and `clone` with `CLONE_NEWUSER` refused, `clone3` answered as missing
//! — its flags are in memory the filter can't read, and glibc falls back to `clone` (Docker's default profile
//! does the same) — and joins none: `setns` refused.
//!
//! Like Landlock, the filter is the calling thread's — threads already running stay as they were — and what
//! the thread starts afterwards inherits it, for good (`landlock::spawn` starts a command from a short-lived
//! thread of its own).

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

/// What the kernel says the calls are from (`AUDIT_ARCH_*`, `<linux/audit.h>`); None: no filter here.
#[cfg(target_arch = "x86_64")]
const ARCH: Option<u32> = Some(0xC000_003E);
#[cfg(target_arch = "aarch64")]
const ARCH: Option<u32> = Some(0xC000_00B7);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const ARCH: Option<u32> = None;

/// x32's calls come as x86-64's with this bit set.
#[cfg(target_arch = "x86_64")]
const X32_BIT: u32 = 0x4000_0000;

/// `struct seccomp_data`'s fields the filter reads: the call's number, its ABI, and the low or high half of
/// an argument (both architectures are little-endian; `socket`'s family and type are ints).
const NR: u32 = 0;
const ARCH_AT: u32 = 4;
const fn arg_low(i: u32) -> u32 {
    16 + 8 * i
}
const fn arg_high(i: u32) -> u32 {
    20 + 8 * i
}

/// A socket type's flags — the rest is the type.
const SOCK_FLAGS: u32 = (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u32;

/// Whether the calls that name an address go to tarae (`Served`: the listener `apply` gives back is the
/// supervisor's — tarae makes the internet sockets too), internet sockets are refused outright (`Plain`), or
/// — inside the session's own network namespace, which holds only `lo` (`sandbox::keeper`) — its stream and
/// datagram ones are the run's own: served, its calls that name an address still go to tarae, which routes
/// them (`Routed`: `sandbox::route`); not served, they're its own too (`OwnNetwork`: the namespace is the
/// lock). Raw ones are refused in both, and unix ones not served, which cross the namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Plain,
    Served,
    Routed,
    OwnNetwork,
}

/// Where a check goes: on to the next, over the next few, or to one of the answers at the end.
#[derive(Clone, Copy, Debug, PartialEq)]
enum To {
    Next,
    Skip(usize),
    Allow,
    /// `EPERM` — a socket refused, said as macOS says it ("Operation not permitted").
    Refuse,
    /// `ENOSYS` — as if the kernel had no such call: io_uring's users fall back to plain reads and writes.
    Missing,
    /// To tarae, which answers it (`SECCOMP_RET_USER_NOTIF`).
    Tarae,
}

enum Op {
    /// Load the 32 bits at this offset of `seccomp_data`.
    Load(u32),
    /// Keep only these bits of what was loaded.
    And(u32),
    /// Equal to this?
    Is(u32, To, To),
    /// Any of these bits set?
    Has(u32, To, To),
}

/// The checks, in order — each jumps only forward: to the next, over a block that isn't its call's, or to an
/// answer. The last block is `socket()`'s; any other call is allowed. `no_userns`: no new user namespace.
/// `ptys` (served only): the calls on the way to a pty's other end go to tarae too (`sandbox::ptys`).
fn ops(arch: u32, mode: Mode, no_userns: bool, ptys: bool) -> Vec<Op> {
    let nr = |n: libc::c_long| n as u32;
    let served = matches!(mode, Mode::Served | Mode::Routed);
    let mut ops = native(arch);
    for n in [libc::SYS_io_uring_setup, libc::SYS_io_uring_enter, libc::SYS_io_uring_register] {
        ops.push(Op::Is(nr(n), To::Missing, To::Next));
    }
    if no_userns {
        ops.extend(userns_ops());
    }
    if served {
        // (`listen`: on a socket tarae put out of the session it would listen on this machine's every address)
        for n in [libc::SYS_connect, libc::SYS_bind, libc::SYS_listen, libc::SYS_sendmsg, libc::SYS_sendmmsg]
        {
            ops.push(Op::Is(nr(n), To::Tarae, To::Next));
        }
        // `sendto` with an address (its fifth argument, a pointer); without one it goes where the socket is
        // connected — tarae's doing — or nowhere
        ops.extend([
            Op::Is(nr(libc::SYS_sendto), To::Next, To::Skip(4)),
            Op::Load(arg_low(4)),
            Op::Is(0, To::Next, To::Tarae),
            Op::Load(arg_high(4)),
            Op::Is(0, To::Allow, To::Tarae),
        ]);
        // A pty made from `/dev/ptmx` is unlocked, numbered or its other end opened through these first — tarae
        // puts one its rules grant in its place (an ioctl's request is an `unsigned int`: the low half is all
        // the kernel reads)
        if ptys {
            ops.extend([
                Op::Is(nr(libc::SYS_ioctl), To::Next, To::Skip(4)),
                Op::Load(arg_low(1)),
                Op::Is(libc::TIOCSPTLCK as u32, To::Tarae, To::Next),
                Op::Is(libc::TIOCGPTN as u32, To::Tarae, To::Next),
                Op::Is(libc::TIOCGPTPEER as u32, To::Tarae, To::Next),
                Op::Load(NR), // (back to the call's number for the next block)
            ]);
        }
    } else {
        ops.extend([
            Op::Is(nr(libc::SYS_socketpair), To::Next, To::Skip(3)),
            Op::Load(arg_low(1)),
            Op::And(!SOCK_FLAGS),
            Op::Is(libc::SOCK_DGRAM as u32, To::Refuse, To::Next),
            Op::Load(NR), // (back to the call's number for the next block)
        ]);
    }
    let unix_stream = if served { To::Allow } else { To::Refuse };
    ops.extend([
        Op::Is(nr(libc::SYS_socket), To::Next, To::Allow),
        Op::Load(arg_low(0)),
        // Netlink: its routing protocol only — what a name lookup asks (socket diagnostics would list every
        // socket on the machine, as `ss` does)
        Op::Is(libc::AF_NETLINK as u32, To::Next, To::Skip(2)),
        Op::Load(arg_low(2)),
        Op::Is(libc::NETLINK_ROUTE as u32, To::Allow, To::Refuse),
    ]);
    match mode {
        Mode::Served => ops.extend([
            Op::Is(libc::AF_INET as u32, To::Tarae, To::Next),
            Op::Is(libc::AF_INET6 as u32, To::Tarae, To::Next),
        ]),
        Mode::Plain => ops.extend([
            Op::Is(libc::AF_INET as u32, To::Refuse, To::Next),
            Op::Is(libc::AF_INET6 as u32, To::Refuse, To::Next),
        ]),
        // (an internet socket is the run's as `Served`'s `socket()` makes them: TCP or UDP, by kind and protocol
        // — not raw, ICMP's, SCTP's or MPTCP's, whose own options and messages name addresses tarae never sees)
        Mode::Routed | Mode::OwnNetwork => ops.extend([
            Op::Is(libc::AF_INET as u32, To::Skip(1), To::Next),
            Op::Is(libc::AF_INET6 as u32, To::Next, To::Skip(10)),
            Op::Load(arg_low(1)),
            Op::And(!SOCK_FLAGS),
            Op::Is(libc::SOCK_STREAM as u32, To::Next, To::Skip(3)),
            Op::Load(arg_low(2)),
            Op::Is(0, To::Allow, To::Next),
            Op::Is(libc::IPPROTO_TCP as u32, To::Allow, To::Refuse),
            Op::Is(libc::SOCK_DGRAM as u32, To::Next, To::Refuse),
            Op::Load(arg_low(2)),
            Op::Is(0, To::Allow, To::Next),
            Op::Is(libc::IPPROTO_UDP as u32, To::Allow, To::Refuse),
            Op::Load(arg_low(0)), // (back to the family for the next block)
        ]),
    }
    ops.extend([
        Op::Is(libc::AF_UNIX as u32, To::Next, To::Refuse),
        Op::Load(arg_low(1)),
        Op::And(!SOCK_FLAGS),
        Op::Is(libc::SOCK_STREAM as u32, unix_stream, To::Refuse),
    ]);
    ops
}

/// The first checks of every filter: the call's ABI is this architecture's — another's, and x32's numbers, find
/// no call at all — and its number loaded.
fn native(arch: u32) -> Vec<Op> {
    #[cfg_attr(not(target_arch = "x86_64"), expect(unused_mut, reason = "only x86_64 adds x32's check"))]
    let mut ops = vec![Op::Load(ARCH_AT), Op::Is(arch, To::Next, To::Missing), Op::Load(NR)];
    #[cfg(target_arch = "x86_64")]
    ops.push(Op::Has(X32_BIT, To::Missing, To::Next));
    ops
}

/// No new user namespace: `unshare` and `clone` with `CLONE_NEWUSER` refused, `clone3` missing (its flags are in
/// memory the filter can't read; glibc falls back to `clone`) — and none joined: `setns` refused whatever it
/// names, every namespace type and a pidfd alike. Under tarae's profile the user's own tools make capable user
/// namespaces (the terminal's podman), owned by the same uid: one joined through `/proc/<pid>/ns/user` is as good
/// as one made (Docker's default profile refuses `setns` outright for the same reason). Starts and ends on the
/// call's number.
fn userns_ops() -> Vec<Op> {
    let nr = |n: libc::c_long| n as u32;
    let mut ops = Vec::new();
    // (both take their flags first, on every architecture; `CLONE_NEWUSER` is in the low half)
    for n in [libc::SYS_unshare, libc::SYS_clone] {
        ops.extend([
            Op::Is(nr(n), To::Next, To::Skip(3)),
            Op::Load(arg_low(0)),
            Op::Has(libc::CLONE_NEWUSER as u32, To::Refuse, To::Next),
            Op::Load(NR), // (back to the call's number for the next block)
        ]);
    }
    ops.push(Op::Is(nr(libc::SYS_clone3), To::Missing, To::Next));
    ops.push(Op::Is(nr(libc::SYS_setns), To::Refuse, To::Next));
    ops
}

/// What runs outside the sandbox for Claude or a plugin gets (`spawn::Acts`, Q4): only no new user namespace —
/// every other call as without a filter, sockets and all (another ABI's calls aside, as in every filter here).
fn outside_ops(arch: u32) -> Vec<Op> {
    let mut ops = native(arch);
    ops.extend(userns_ops());
    ops
}

/// `ops` for `mode` as classic BPF (`assemble`).
fn program(arch: u32, mode: Mode, no_userns: bool, ptys: bool) -> Vec<libc::sock_filter> {
    assemble(&ops(arch, mode, no_userns, ptys))
}

/// `ops` as classic BPF, the four answers after them — falling off the last check is allowed.
fn assemble(ops: &[Op]) -> Vec<libc::sock_filter> {
    let n = ops.len();
    let at = |i: usize, to: To| -> u8 {
        let target = match to {
            To::Next => return 0,
            To::Skip(k) => i + 1 + k,
            To::Allow => n,
            To::Refuse => n + 1,
            To::Missing => n + 2,
            To::Tarae => n + 3,
        };
        (target - i - 1) as u8
    };
    let op = |code: u32, k: u32, jt: u8, jf: u8| libc::sock_filter { code: code as u16, jt, jf, k };
    let mut prog: Vec<libc::sock_filter> = ops
        .iter()
        .enumerate()
        .map(|(i, o)| match *o {
            Op::Load(off) => op(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, off, 0, 0),
            Op::And(k) => op(libc::BPF_ALU | libc::BPF_AND | libc::BPF_K, k, 0, 0),
            Op::Is(k, t, f) => op(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, k, at(i, t), at(i, f)),
            Op::Has(k, t, f) => op(libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K, k, at(i, t), at(i, f)),
        })
        .collect();
    let ret = libc::BPF_RET | libc::BPF_K;
    prog.push(op(ret, libc::SECCOMP_RET_ALLOW, 0, 0));
    prog.push(op(ret, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32, 0, 0));
    prog.push(op(ret, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0));
    prog.push(op(ret, libc::SECCOMP_RET_USER_NOTIF, 0, 0));
    prog
}

/// Whether there's a filter for this architecture.
pub fn available() -> bool {
    ARCH.is_some()
}

/// Puts the filter for `mode` on the calling thread (`Filter::new`, then `Filter::apply`).
#[cfg(test)]
pub fn apply(mode: Mode, no_userns: bool) -> io::Result<Option<OwnedFd>> {
    Filter::new(mode, no_userns, false)?.apply()
}

/// A run's filter, made before it's put on — the keeper's run puts it on in the child of a raw `clone3`, which
/// has no fork bookkeeping (glibc's malloc locks aren't reset there): `apply` makes raw calls only and allocates
/// nothing, its errors errno's alone.
pub struct Filter {
    prog: Vec<libc::sock_filter>,
    mode: Mode,
}

impl Filter {
    /// The filter for `mode` — `no_userns`: no new user namespace either (`tier::runs_refuse_userns`); `ptys`:
    /// tarae serves the way to a pty's other end (`tier::Ns::serves_ptys`).
    pub fn new(mode: Mode, no_userns: bool, ptys: bool) -> io::Result<Filter> {
        let arch = ARCH.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "no seccomp filter for this architecture")
        })?;
        Ok(Filter { prog: program(arch, mode, no_userns, ptys), mode })
    }

    /// Puts it on the calling thread — and what it starts from now on — for good. Sets `no_new_privs` first
    /// (the kernel asks it of an unprivileged caller). Served: the listener tarae's supervisor answers on — once
    /// it has read a call, the caller waits for its answer through any signal but a fatal one
    /// (`WAIT_KILLABLE_RECV`, Linux 5.19: a call tarae did isn't started again by a signal; without it, older
    /// kernels). Raw calls only, nothing allocated.
    pub fn apply(&self) -> io::Result<Option<OwnedFd>> {
        let fprog = libc::sock_fprog { len: self.prog.len() as u16, filter: self.prog.as_ptr().cast_mut() };
        let listener = libc::SECCOMP_FILTER_FLAG_NEW_LISTENER;
        let tries: &[libc::c_ulong] = match self.mode {
            Mode::Plain | Mode::OwnNetwork => &[0],
            Mode::Served | Mode::Routed => {
                &[listener | libc::SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV, listener]
            }
        };
        // SAFETY: the program outlives the call (the kernel copies it; it never writes through the pointer); this
        // thread only (no TSYNC).
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let fprog = &raw const fprog;
            let mut last = io::Error::from_raw_os_error(libc::EINVAL);
            for &flags in tries {
                let r = libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, flags, fprog);
                if r >= 0 {
                    // (the listener comes close-on-exec: no run inherits it)
                    let listens = matches!(self.mode, Mode::Served | Mode::Routed);
                    return Ok(listens.then(|| OwnedFd::from_raw_fd(r as i32)));
                }
                last = io::Error::last_os_error();
                if last.raw_os_error() != Some(libc::EINVAL) {
                    break;
                }
            }
            Err(last)
        }
    }
}

/// The outside filter (`outside_ops`), made before it's put on — where it goes on in a fork's child (`pre_exec`),
/// nothing may be allocated: `apply` makes raw calls only.
pub struct NoUserns(Vec<libc::sock_filter>);

impl NoUserns {
    pub fn new() -> io::Result<NoUserns> {
        let arch = ARCH.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "no seccomp filter for this architecture")
        })?;
        Ok(NoUserns(assemble(&outside_ops(arch))))
    }

    /// Puts it on the calling thread, and what it starts from now on, for good — `no_new_privs` first, which
    /// the kernel asks of an unprivileged caller (so a setuid program it starts runs without its privileges).
    /// Raw calls only: fit for a fork's child.
    pub fn apply(&self) -> io::Result<()> {
        put_on(&self.0)
    }
}

/// `prog` on the calling thread, `no_new_privs` first, which the kernel asks of an unprivileged caller (so a
/// setuid program it starts runs without its privileges). Raw calls only: fit for a fork's child.
fn put_on(prog: &[libc::sock_filter]) -> io::Result<()> {
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr().cast_mut() };
    // SAFETY: the program outlives the call (the kernel copies it; it never writes through the pointer).
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            || libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0, &raw const fprog) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `start` on a short-lived thread under the outside filter (`NoUserns`): what it starts (`posix_spawn`)
/// inherits it; tarae's other threads don't — as `landlock::confined` does for a sandboxed run.
pub fn outside<T: Send>(start: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
    let filter = NoUserns::new()?;
    std::thread::scope(|s| {
        let t = std::thread::Builder::new().name("tarae-outside".into()).spawn_scoped(s, || {
            filter.apply()?;
            start()
        })?;
        t.join().unwrap_or_else(|_| Err(io::Error::other("its starting thread panicked")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;

    /// `f` on a thread of its own under the plain filter (the served one is the supervisor's to test).
    fn filtered<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        filtered_with(false, f)
    }

    /// `filtered`, with new user namespaces refused or not.
    fn filtered_with<T: Send + 'static>(no_userns: bool, f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(move || {
            apply(Mode::Plain, no_userns).expect("the filter applies");
            f()
        })
        .join()
        .unwrap()
    }

    /// `socket(family, kind, 0)` straight (closed again): Ok, or the errno.
    fn socket(family: i32, kind: i32) -> Result<(), i32> {
        socket_of(family, kind, 0)
    }

    /// `socket(family, kind, protocol)`, as `socket`.
    fn socket_of(family: i32, kind: i32, protocol: i32) -> Result<(), i32> {
        // SAFETY: a plain call; a descriptor it makes is closed at once.
        let fd = unsafe { libc::socket(family, kind | libc::SOCK_CLOEXEC, protocol) };
        if fd < 0 {
            return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        // SAFETY: ours, just made.
        unsafe { libc::close(fd) };
        Ok(())
    }

    fn temp(test: &str) -> std::path::PathBuf {
        crate::scratch::Scratch::new(&format!("seccomp-{test}")).into_path()
    }

    #[test]
    fn an_internet_socket_is_refused() {
        let (tcp, udp6, inet) = filtered(|| {
            let tcp = std::net::TcpListener::bind("127.0.0.1:0").map(drop);
            let udp6 = std::net::UdpSocket::bind("[::1]:0").map(drop);
            (
                tcp.map_err(|e| e.raw_os_error()),
                udp6.map_err(|e| e.raw_os_error()),
                socket(libc::AF_INET, libc::SOCK_RAW),
            )
        });
        assert_eq!(tcp, Err(Some(libc::EPERM)), "TCP, even on this machine");
        assert_eq!(udp6, Err(Some(libc::EPERM)), "UDP over IPv6");
        assert_eq!(inet, Err(libc::EPERM), "a raw socket");
    }

    /// Pipes between a run's own processes stay, and so does netlink's routing protocol (a name lookup asks
    /// it about the interfaces) — the one family `socket()` may make, and of it only that: socket diagnostics
    /// would list every socket on the machine (`ss`). A datagram pair doesn't: its `sendto` reaches any
    /// datagram socket by path.
    #[test]
    fn socketpair_and_netlink_stay() {
        let (said, netlink, diag, packet, dgram) = filtered(|| {
            let (mut a, mut b) = UnixStream::pair().expect("socketpair");
            io::Write::write_all(&mut a, b"x").unwrap();
            let mut said = [0u8];
            io::Read::read_exact(&mut b, &mut said).unwrap();
            let dgram = std::os::unix::net::UnixDatagram::pair().map(drop).map_err(|e| e.raw_os_error());
            let route = socket_of(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE);
            let diag = socket_of(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_SOCK_DIAG);
            (said, route, diag, socket(libc::AF_PACKET, libc::SOCK_RAW), dgram)
        });
        assert_eq!((said, netlink), (*b"x", Ok(())));
        assert_eq!(diag, Err(libc::EPERM), "netlink's socket diagnostics");
        assert_eq!(packet, Err(libc::EPERM), "any other family");
        assert_eq!(dgram, Err(Some(libc::EPERM)), "a datagram pair");
    }

    /// No unix socket is made — so none is reached (a session bus, docker.sock, an ssh-agent) or listened on.
    #[test]
    fn a_unix_socket_is_refused() {
        let dir = temp("unix");
        let (s, listen, dgram) = (dir.join("s"), dir.join("l"), dir.join("d"));
        let (connect, bind, raw) = filtered(move || {
            let connect = UnixStream::connect(&s).map(drop).map_err(|e| e.raw_os_error());
            let bind = UnixListener::bind(&listen).map(drop).map_err(|e| e.raw_os_error());
            let raw = std::os::unix::net::UnixDatagram::bind(&dgram).map(drop).map_err(|e| e.raw_os_error());
            (connect, bind, raw)
        });
        assert_eq!(connect, Err(Some(libc::EPERM)), "not even the socket to connect with");
        assert_eq!((bind, raw), (Err(Some(libc::EPERM)), Err(Some(libc::EPERM))));
        assert!(!dir.join("l").exists() && !dir.join("d").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn io_uring_setup_is_refused() {
        let got = filtered(|| {
            let mut params = [0u8; 120]; // struct io_uring_params
            // SAFETY: the params are a zeroed buffer of the struct's size; a ring it made is closed.
            let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) };
            if fd >= 0 {
                // SAFETY: ours.
                unsafe { libc::close(fd as i32) };
                return Ok(());
            }
            Err(io::Error::last_os_error().raw_os_error())
        });
        assert_eq!(got, Err(Some(libc::ENOSYS)));
    }

    /// While one thread has the filter, another that applied nothing makes sockets as before — and the
    /// filtered one, asked right after, still can't.
    #[test]
    fn a_thread_without_the_filter_is_unaffected() {
        let dir = temp("unaffected");
        let (ready_tx, ready) = mpsc::channel();
        let (go, go_rx) = mpsc::channel::<()>();
        let filtered = std::thread::spawn(move || {
            ready_tx.send(apply(Mode::Plain, false).map(drop)).unwrap();
            go_rx.recv().unwrap();
            (socket(libc::AF_INET, libc::SOCK_STREAM), socket(libc::AF_UNIX, libc::SOCK_STREAM))
        });
        ready.recv().unwrap().expect("the filter applies");
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("TCP from a thread without the filter");
        let unix = UnixListener::bind(dir.join("s")).expect("a unix socket from a thread without the filter");
        drop((tcp, unix));
        go.send(()).unwrap();
        assert_eq!(filtered.join().unwrap(), (Err(libc::EPERM), Err(libc::EPERM)), "still filtered");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The program for `mode` (new user namespaces refused or not: `no_userns`) run over one call as the kernel
    /// would (classic BPF: loads, a mask, jumps, returns) — the program this architecture's, the call `call_arch`'s.
    fn eval_with(mode: Mode, no_userns: bool, call_arch: u32, nr: i64, args: &[u64]) -> u32 {
        eval_prog(&program(ARCH.unwrap(), mode, no_userns, false), call_arch, nr, args)
    }

    /// 3b: with `ptys` (served) the three calls on the way to a pty's other end go to tarae —
    /// whatever the request's high half, which the kernel doesn't read — and no other ioctl, nor any open; without
    /// it, or not served, they're the run's.
    #[test]
    fn the_way_to_a_ptys_other_end_goes_to_tarae() {
        let arch = ARCH.unwrap();
        let (allow, tarae) = (ALLOW, libc::SECCOMP_RET_USER_NOTIF);
        let ask = |mode, ptys, nr, args: &[u64]| eval_prog(&program(arch, mode, false, ptys), arch, nr, args);
        let mode = Mode::Served;
        for c in [libc::TIOCSPTLCK, libc::TIOCGPTN, libc::TIOCGPTPEER] {
            assert_eq!(ask(mode, true, libc::SYS_ioctl, &[3, c, 0x1000]), tarae, "{c:#x}");
            assert_eq!(ask(mode, true, libc::SYS_ioctl, &[3, c | 1 << 32, 0x1000]), tarae, "high half");
            assert_eq!(ask(mode, false, libc::SYS_ioctl, &[3, c, 0x1000]), allow, "without");
        }
        for other in [libc::TIOCGWINSZ, libc::TCGETS, libc::TIOCSCTTY, libc::FIONREAD] {
            assert_eq!(ask(mode, true, libc::SYS_ioctl, &[3, other]), allow, "{other:#x}");
        }
        assert_eq!(ask(mode, true, libc::SYS_openat, &[3, 0x1000, 2]), allow, "no open");
        // (the blocks after it still see the call's number)
        assert_eq!(ask(mode, true, libc::SYS_connect, &[3]), tarae);
        assert_eq!(ask(Mode::Plain, true, libc::SYS_ioctl, &[3, libc::TIOCGPTN]), allow, "not served");
    }

    /// `prog` run over one call as the kernel would.
    fn eval_prog(prog: &[libc::sock_filter], call_arch: u32, nr: i64, args: &[u64]) -> u32 {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&(nr as i32).to_le_bytes());
        data[4..8].copy_from_slice(&call_arch.to_le_bytes());
        for (i, a) in args.iter().enumerate() {
            data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_le_bytes());
        }
        let (mut acc, mut pc) = (0u32, 0usize);
        loop {
            let i = &prog[pc];
            let code = u32::from(i.code);
            match code {
                c if c == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => {
                    let k = i.k as usize;
                    acc = u32::from_le_bytes(data[k..k + 4].try_into().unwrap());
                    pc += 1;
                }
                c if c == libc::BPF_ALU | libc::BPF_AND | libc::BPF_K => {
                    acc &= i.k;
                    pc += 1;
                }
                c if c == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K => {
                    pc += 1 + usize::from(if acc == i.k { i.jt } else { i.jf });
                }
                c if c == libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K => {
                    pc += 1 + usize::from(if acc & i.k != 0 { i.jt } else { i.jf });
                }
                c if c == libc::BPF_RET | libc::BPF_K => return i.k,
                c => panic!("an instruction the kernel's checker would want to see: {c:#x}"),
            }
        }
    }

    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    const EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    const ENOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;

    /// Where runs lose user namespaces (Q4), the program refuses exactly the calls that make one — `unshare` and
    /// `clone` by their flag word, whatever else is asked with it or set in its high half; `clone3`, whose flags
    /// it can't read, as missing (glibc's fallback is `clone`) — and the one that joins one, `setns`, whatever
    /// it names (a user namespace, any type, a pidfd's set); and leaves the rest: other namespaces, a plain fork,
    /// threads. Without the refusal every one of them goes on, in every mode.
    #[test]
    fn under_the_profile_a_run_is_refused_new_user_namespaces() {
        let arch = ARCH.unwrap();
        let user = libc::CLONE_NEWUSER as u64;
        let fork = libc::SIGCHLD as u64;
        let thread =
            (libc::CLONE_VM | libc::CLONE_FS | libc::CLONE_FILES | libc::CLONE_SIGHAND | libc::CLONE_THREAD)
                as u64;
        for mode in [Mode::Plain, Mode::Served, Mode::Routed, Mode::OwnNetwork] {
            let refused = |nr, args: &[u64]| eval_with(mode, true, arch, nr, args);
            assert_eq!(refused(libc::SYS_unshare, &[user]), EPERM, "{mode:?}");
            assert_eq!(refused(libc::SYS_unshare, &[user | libc::CLONE_NEWNET as u64 | (1 << 32)]), EPERM);
            assert_eq!(refused(libc::SYS_clone, &[user | fork]), EPERM, "{mode:?}");
            assert_eq!(refused(libc::SYS_clone, &[user | thread | (7 << 32)]), EPERM, "{mode:?}");
            assert_eq!(refused(libc::SYS_clone3, &[0x1000, 88]), ENOSYS, "{mode:?}: its flags unread");
            let setns = [[4, user], [4, 0], [4, (libc::CLONE_NEWNET | libc::CLONE_NEWUSER) as u64]];
            for args in setns {
                assert_eq!(refused(libc::SYS_setns, &args), EPERM, "{mode:?}: setns {args:?}");
            }
            assert_eq!(
                refused(libc::SYS_unshare, &[libc::CLONE_NEWNS as u64]),
                ALLOW,
                "{mode:?}: not a user one"
            );
            assert_eq!(refused(libc::SYS_clone, &[fork]), ALLOW, "{mode:?}: a fork");
            assert_eq!(refused(libc::SYS_clone, &[thread]), ALLOW, "{mode:?}: a thread");
            assert_eq!(refused(libc::SYS_read, &[user]), ALLOW, "{mode:?}: another call, the same bit");
            let kept = |nr, args: &[u64]| eval_with(mode, false, arch, nr, args);
            for (nr, args) in [
                (libc::SYS_unshare, [user]),
                (libc::SYS_clone, [user | fork]),
                (libc::SYS_clone3, [0]),
                (libc::SYS_setns, [4]),
            ] {
                assert_eq!(kept(nr, &args), ALLOW, "{mode:?}: {nr} without the refusal");
            }
        }
    }

    /// A new user namespace asked three ways from this thread — `unshare`, `clone`, `clone3` with
    /// `CLONE_NEWUSER` — each `None` (made) or its errno, and whether a plain fork works. Never changes this
    /// process: `unshare` of a user namespace in a process with threads (the test's own, and this one) is the
    /// kernel's `EINVAL` at most, and a child either clone makes ends at once.
    fn userns_calls() -> (Option<i32>, Option<i32>, Option<i32>, bool) {
        let errno = |r: libc::c_long| if r < 0 { io::Error::last_os_error().raw_os_error() } else { None };
        // SAFETY: plain calls; a child one of them makes (stack 0: a copy, as a fork's) ends at once.
        unsafe {
            let unshare = errno(libc::unshare(libc::CLONE_NEWUSER) as libc::c_long);
            let flags = (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong;
            let clone = libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0);
            if clone == 0 {
                libc::_exit(0);
            }
            let clone_errno = errno(clone);
            let mut args = [0u64; 11]; // struct clone_args: its flags and exit signal set
            (args[0], args[4]) = (libc::CLONE_NEWUSER as u64, libc::SIGCHLD as u64);
            let clone3 = libc::syscall(libc::SYS_clone3, args.as_mut_ptr(), std::mem::size_of_val(&args));
            if clone3 == 0 {
                libc::_exit(0);
            }
            let clone3_errno = errno(clone3);
            let fork = libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_ulong, 0, 0, 0, 0);
            if fork == 0 {
                libc::_exit(0);
            }
            for pid in [clone, clone3, fork].into_iter().filter(|p| *p > 0) {
                libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
            }
            (unshare, clone_errno, clone3_errno, fork > 0)
        }
    }

    /// The same live, on a thread under the filter: a new user namespace by any of the three calls is refused
    /// before the kernel looks at it (`EPERM`, not `unshare`'s `EINVAL` in a process with threads), a plain fork
    /// still works. It counts only where the calls reach the kernel without tarae's filter — the control, the
    /// same calls on a thread without it: Docker's default profile answers them `EPERM` and `ENOSYS` itself,
    /// exactly what's asserted, so in a container under it (`cargo xtask linux test` without `--userns`) the test
    /// couldn't fail and says it skips; a machine said to give namespaces (`TARAE_EXPECT_TIER`) fails instead.
    #[test]
    fn a_filtered_thread_makes_no_user_namespace() {
        if !userns_calls_reach_the_kernel("a_filtered_thread_makes_no_user_namespace") {
            return;
        }
        let (unshare, clone, clone3, fork) = filtered_with(true, userns_calls);
        assert_eq!((unshare, clone, clone3), (Some(libc::EPERM), Some(libc::EPERM), Some(libc::ENOSYS)));
        assert!(fork, "a plain fork");
    }

    /// The control of the live tests here: the calls of `userns_calls` on a thread without tarae's filter reach
    /// the kernel (`unshare`'s `EINVAL` in a process with threads, both clones made) — else `test` says it skips,
    /// and why (Docker's default profile answers them `EPERM` and `ENOSYS` itself: exactly what the tests assert),
    /// or fails where a machine is said to give namespaces (`TARAE_EXPECT_TIER`).
    fn userns_calls_reach_the_kernel(test: &str) -> bool {
        let control = std::thread::spawn(userns_calls).join().unwrap();
        if control == (Some(libc::EINVAL), None, None, true) {
            return true;
        }
        let why = format!(
            "the calls don't reach the kernel without tarae's filter here (unshare, clone, clone3, fork: \
             {control:?}) — Docker's default seccomp profile refuses them itself"
        );
        if let Ok(want) = std::env::var("TARAE_EXPECT_TIER")
            && want.trim() == "own"
        {
            panic!("tier {want} expected here: {why}");
        }
        let line = format!("{}seccomp::tests::{test} — {why}\n", crate::test_results::SELF_SKIP);
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        false
    }

    /// What acts for Claude or a plugin outside the sandbox (`NoUserns`, `spawn::Acts`) is refused user namespaces
    /// exactly as a sandboxed run is — none made, none joined (`setns`, whatever it names) — and nothing else:
    /// sockets of every family, other namespaces made, a namespace's file looked at, a pidfd, a fork, a thread go
    /// through as without a filter. Another ABI's calls find none (their numbers aren't these).
    #[test]
    fn the_outside_filter_refuses_only_new_user_namespaces() {
        let arch = ARCH.unwrap();
        let prog = assemble(&outside_ops(arch));
        let run = |nr, args: &[u64]| eval_prog(&prog, arch, nr, args);
        let user = libc::CLONE_NEWUSER as u64;
        assert_eq!(run(libc::SYS_unshare, &[user | (1 << 32)]), EPERM);
        assert_eq!(run(libc::SYS_clone, &[user | libc::SIGCHLD as u64]), EPERM);
        assert_eq!(run(libc::SYS_clone3, &[0x1000, 88]), ENOSYS);
        for nstype in [user, 0, libc::CLONE_NEWNET as u64] {
            assert_eq!(run(libc::SYS_setns, &[4, nstype]), EPERM, "setns {nstype:#x}");
        }
        let raw = (libc::AF_INET as u64, libc::SOCK_RAW as u64);
        for (nr, args) in [
            (libc::SYS_unshare, vec![libc::CLONE_NEWNS as u64]),
            (libc::SYS_unshare, vec![libc::CLONE_NEWNET as u64]),
            (libc::SYS_ioctl, vec![4, 0xb701]), // (NS_GET_USERNS: a namespace's owner looked at)
            (libc::SYS_pidfd_open, vec![1, 0]),
            (libc::SYS_clone, vec![libc::SIGCHLD as u64]),
            (libc::SYS_socket, vec![raw.0, raw.1, 0]),
            (libc::SYS_socket, vec![libc::AF_UNIX as u64, libc::SOCK_DGRAM as u64, 0]),
            (libc::SYS_connect, vec![3]),
            (libc::SYS_io_uring_setup, vec![1]),
            (libc::SYS_read, vec![user]),
        ] {
            assert_eq!(run(nr, &args), ALLOW, "{nr} {args:?}: as without a filter");
        }
        assert_eq!(eval_prog(&prog, 0x4000_0003, libc::SYS_read, &[]), ENOSYS, "i386's calls");
    }

    /// The same live: a thread under the outside filter (`outside`, as Claude's run outside the sandbox starts)
    /// makes no user namespace by any of the three calls; a plain fork still works.
    #[test]
    fn what_starts_outside_for_claude_makes_no_user_namespace() {
        if !userns_calls_reach_the_kernel("what_starts_outside_for_claude_makes_no_user_namespace") {
            return;
        }
        let (unshare, clone, clone3, fork) = outside(|| Ok(userns_calls())).unwrap();
        assert_eq!((unshare, clone, clone3), (Some(libc::EPERM), Some(libc::EPERM), Some(libc::ENOSYS)));
        assert!(fork, "a plain fork");
    }

    /// The program run over calls as the kernel would: another ABI and x32's numbers find no call at all;
    /// plain, as the tests above have it; served, the calls that name an address go to tarae — a `sendto` only
    /// with one — and a unix stream socket may be made.
    #[test]
    fn the_filter_knows_only_the_native_calls() {
        let arch = ARCH.unwrap();
        // (the program is this architecture's; the call says its own)
        let eval = |mode: Mode, call_arch: u32, nr: i64, args: &[u64]| -> u32 {
            eval_with(mode, false, call_arch, nr, args)
        };
        let (allow, eperm, enosys, tarae) = (ALLOW, EPERM, ENOSYS, libc::SECCOMP_RET_USER_NOTIF);
        let (inet, inet6, unix) = (libc::AF_INET as u64, libc::AF_INET6 as u64, libc::AF_UNIX as u64);
        let stream = (libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u64;
        let dgram = (libc::SOCK_DGRAM | libc::SOCK_CLOEXEC) as u64;
        for mode in [Mode::Plain, Mode::Served, Mode::Routed, Mode::OwnNetwork] {
            let run = |nr, args: &[u64]| eval(mode, arch, nr, args);
            assert_eq!(run(libc::SYS_socket, &[libc::AF_NETLINK as u64]), allow, "{mode:?}");
            assert_eq!(run(libc::SYS_socket, &[(1 << 32) | libc::AF_NETLINK as u64]), allow, "an int");
            let (netlink, raw) = (libc::AF_NETLINK as u64, libc::SOCK_RAW as u64);
            let diag = libc::NETLINK_SOCK_DIAG as u64;
            assert_eq!(run(libc::SYS_socket, &[netlink, raw, 0]), allow, "{mode:?}: routing");
            assert_eq!(run(libc::SYS_socket, &[netlink, raw, diag]), eperm, "{mode:?}: socket diagnostics");
            assert_eq!(run(libc::SYS_socket, &[netlink, raw, 1 << 32]), allow, "the protocol is an int");
            assert_eq!(run(libc::SYS_socket, &[libc::AF_PACKET as u64]), eperm, "{mode:?}");
            assert_eq!(run(libc::SYS_socket, &[unix, dgram]), eperm, "{mode:?}: a unix datagram socket");
            assert_eq!(run(libc::SYS_socketpair, &[unix, stream]), allow, "{mode:?}");
            assert_eq!(run(libc::SYS_io_uring_setup, &[0]), enosys);
            assert_eq!(run(libc::SYS_io_uring_enter, &[0]), enosys);
            assert_eq!(run(libc::SYS_read, &[0]), allow);
            assert_eq!(run(libc::SYS_sendto, &[3, 0x1000, 9, 0, 0, 0]), allow, "{mode:?}: no address");
            // i386 (`AUDIT_ARCH_I386`, its `socketcall` is 102) and any other ABI: no call gets through
            for other in [0x4000_0003u32, 0xC000_00B7 ^ 0xC000_003E ^ arch] {
                assert_eq!(eval(mode, other, libc::SYS_read, &[]), enosys, "{other:#x}");
                assert_eq!(eval(mode, other, 102, &[inet]), enosys, "{other:#x}");
            }
            #[cfg(target_arch = "x86_64")]
            assert_eq!(run(i64::from(X32_BIT) | libc::SYS_socket, &[inet]), enosys, "x32");
        }
        let plain = |nr, args: &[u64]| eval(Mode::Plain, arch, nr, args);
        assert_eq!(plain(libc::SYS_socket, &[inet, stream]), eperm);
        assert_eq!(plain(libc::SYS_socket, &[unix, stream]), eperm);
        assert_eq!(plain(libc::SYS_socketpair, &[unix, dgram]), eperm, "a datagram pair");
        assert_eq!(plain(libc::SYS_connect, &[3]), allow, "(no socket to connect)");
        let served = |nr, args: &[u64]| eval(Mode::Served, arch, nr, args);
        assert_eq!(served(libc::SYS_socket, &[inet, stream]), tarae);
        assert_eq!(served(libc::SYS_socket, &[inet6, dgram]), tarae);
        assert_eq!(served(libc::SYS_socket, &[unix, stream]), allow);
        assert_eq!(served(libc::SYS_socketpair, &[unix, dgram]), allow, "its sendto goes to tarae");
        for nr in [libc::SYS_connect, libc::SYS_bind, libc::SYS_listen, libc::SYS_sendmsg, libc::SYS_sendmmsg]
        {
            assert_eq!(served(nr, &[3]), tarae, "{nr}");
        }
        assert_eq!(served(libc::SYS_sendto, &[3, 0x1000, 9, 0, 0x7fff_0000, 16]), tarae, "an address");
        assert_eq!(served(libc::SYS_sendto, &[3, 0x1000, 9, 0, 0x7f_0000_0000, 16]), tarae, "high half");
        // In the session's own network namespace: its stream and datagram internet sockets, its calls on them
        // its own; a raw one, a unix one and a datagram pair still refused
        let own = |nr, args: &[u64]| eval(Mode::OwnNetwork, arch, nr, args);
        let raw = libc::SOCK_RAW as u64;
        assert_eq!(
            [own(libc::SYS_socket, &[inet, stream]), own(libc::SYS_socket, &[inet6, dgram])],
            [allow; 2]
        );
        assert_eq!(own(libc::SYS_socket, &[inet, raw]), eperm, "a raw socket");
        assert_eq!(own(libc::SYS_socket, &[inet6, raw | (1 << 32)]), eperm, "the type is an int");
        assert_eq!(own(libc::SYS_socket, &[unix, stream]), eperm);
        assert_eq!(own(libc::SYS_socketpair, &[unix, dgram]), eperm, "a datagram pair");
        for nr in [libc::SYS_connect, libc::SYS_bind, libc::SYS_listen, libc::SYS_sendmsg, libc::SYS_sendto] {
            assert_eq!(own(nr, &[3, 0x1000, 9, 0, 0x7fff_0000, 16]), allow, "{nr}");
        }
        // TCP and UDP only, by kind and protocol, in both: SCTP, MPTCP, ICMP's and UDP-Lite's sockets refused
        for mode in [Mode::OwnNetwork, Mode::Routed] {
            let socket =
                |family, kind, protocol: u64| eval(mode, arch, libc::SYS_socket, &[family, kind, protocol]);
            let (tcp, udp) = (libc::IPPROTO_TCP as u64, libc::IPPROTO_UDP as u64);
            let ok = [
                socket(inet, stream, 0),
                socket(inet6, stream, tcp),
                socket(inet, dgram, 0),
                socket(inet6, dgram, udp),
            ];
            assert_eq!(ok, [allow; 4], "{mode:?}");
            assert_eq!(socket(inet, stream, (1 << 32) | tcp), allow, "{mode:?}: the protocol is an int");
            for (kind, protocol) in [
                (stream, libc::IPPROTO_SCTP as u64),
                (stream, libc::IPPROTO_MPTCP as u64),
                (stream, udp),
                (dgram, libc::IPPROTO_ICMP as u64),
                (dgram, libc::IPPROTO_UDPLITE as u64),
                (dgram, tcp),
            ] {
                assert_eq!(socket(inet, kind, protocol), eperm, "{mode:?}: {kind:#x} {protocol}");
                assert_eq!(socket(inet6, kind, protocol), eperm, "{mode:?}: IPv6 {kind:#x} {protocol}");
            }
        }
        // Routed: the run's own internet sockets in the session's netns, as above; what names an address goes
        // to tarae, as served; a unix stream socket may be made (tarae says what it reaches)
        let routed = |nr, args: &[u64]| eval(Mode::Routed, arch, nr, args);
        assert_eq!(
            [routed(libc::SYS_socket, &[inet, stream]), routed(libc::SYS_socket, &[inet6, dgram])],
            [allow; 2]
        );
        assert_eq!(routed(libc::SYS_socket, &[inet, raw]), eperm, "a raw socket");
        assert_eq!(routed(libc::SYS_socket, &[inet6, raw | (1 << 32)]), eperm, "the type is an int");
        assert_eq!(routed(libc::SYS_socket, &[unix, stream]), allow);
        assert_eq!(routed(libc::SYS_socket, &[unix, dgram]), eperm);
        for nr in [libc::SYS_connect, libc::SYS_bind, libc::SYS_listen, libc::SYS_sendmsg, libc::SYS_sendmmsg]
        {
            assert_eq!(routed(nr, &[3]), tarae, "{nr}");
        }
        assert_eq!(routed(libc::SYS_sendto, &[3, 0x1000, 9, 0, 0x7fff_0000, 16]), tarae, "an address");
        assert_eq!(routed(libc::SYS_sendto, &[3, 0x1000, 9, 0, 0, 0]), allow, "none");
    }
}
