//! The wire protocol between tarae and a process of the sandbox's (`docs/design/sandbox-keeper.md`): a session's
//! keeper (`keeper.rs`) — and the supervisor and the egress proxy only if they ever leave tarae's process (D21; in
//! it they talk over a Rust channel, D23). One `SOCK_SEQPACKET` record is one
//! message (the kernel keeps the boundaries): a kind byte, then its fields — integers little-endian, a byte
//! string as a `u32` length and its bytes, a list as a `u32` count and its items — and the descriptors its kind
//! takes as `SCM_RIGHTS`, exactly those.
//!
//! The parser is bounded: a record past `MSG_MAX`, more than `FDS_MAX` descriptors, a list past `LIST_MAX`, a
//! length past what's left of the record, bytes left over, a truncated record or control data, descriptors
//! another kind than the message's takes — each a refusal (`Refused`), never a panic, and nothing allocated by a
//! claimed length before it's checked against what's there.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use super::tier::RunNet;

/// The longest record.
pub const MSG_MAX: usize = 1 << 20;
/// The most descriptors a record carries.
pub const FDS_MAX: usize = 8;
/// The most items a list holds.
pub const LIST_MAX: usize = 4096;
/// The longest name a run gives itself (`PR_SET_NAME`: the kernel's 15 bytes).
pub const COMM_MAX: usize = 15;

/// What a run's child failed at before its exec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The request itself (malformed, or its descriptors).
    Request,
    Fork,
    Chdir,
    Landlock,
    Seccomp,
    Handoff,
    Exec,
}

impl Step {
    const ALL: [Step; 7] =
        [Step::Request, Step::Fork, Step::Chdir, Step::Landlock, Step::Seccomp, Step::Handoff, Step::Exec];

    /// As a byte (the record's, and a run's failure pipe's).
    pub fn code(self) -> u8 {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0) as u8
    }

    pub fn from_code(code: u8) -> Option<Step> {
        Step::ALL.get(code as usize).copied()
    }

    /// In a report's words.
    pub fn said(self) -> &'static str {
        match self {
            Step::Request => "the request",
            Step::Fork => "starting it",
            Step::Chdir => "its folder",
            Step::Landlock => "its file rules",
            Step::Seccomp => "its socket filter",
            Step::Handoff => "handing its calls to tarae",
            Step::Exec => "running the program",
        }
    }
}

/// A run asked of a keeper. Its descriptors: the Landlock ruleset, stdin, stdout, stderr — and, served (`net`
/// `Served` or `Routed`) or `held`, the handoff socket: served, its listener's number goes back on it and a
/// byte comes once tarae has it; held, a byte comes once tarae knows its pid (a keeper in a PID namespace says
/// its own number — tarae reads its own from the pidfd, before the run can end and be reaped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub id: u64,
    /// The program, a real path (tarae resolves it, as `spawn::resolve`).
    pub program: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    /// `NAME=value`, each.
    pub envp: Vec<Vec<u8>>,
    pub cwd: Vec<u8>,
    /// The name the run's process gives itself before its rules (Landlock's audit names it), ≤ `COMM_MAX`.
    pub comm: Vec<u8>,
    /// How its sockets go: its seccomp mode (`keeper::mode`), decided with its rules (`linux::confine`).
    pub net: RunNet,
    pub held: bool,
    /// It makes no user namespace (`tier::runs_refuse_userns`, Q4): its seccomp filter refuses one.
    pub no_userns: bool,
}

impl Run {
    /// Served: its calls that name an address go to tarae, over its handoff socket's listener.
    pub fn served(&self) -> bool {
        matches!(self.net, RunNet::Served | RunNet::Routed)
    }
}

/// `RunNet` on the wire, each its byte.
const NETS: [RunNet; 4] = [RunNet::Plain, RunNet::Served, RunNet::Routed, RunNet::Own];

/// A message, either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    // tarae → keeper
    Run(Run),
    /// Signal the run's process group (cancel) — only while the keeper hasn't reaped it.
    Signal {
        id: u64,
        sig: i32,
    },
    /// What the keeper itself uses (it's non-dumpable: nobody reads its `/proc`).
    AskUsage,
    /// End every run and what they left, then exit (as EOF does).
    End,
    /// Make the sockets the session's network is made with (`redirect::Kind::wanted`): its `lo` carries `v4`'s
    /// and `v6`'s addresses. Asked once, by the birth, before any run.
    Plant {
        v4: bool,
        v6: bool,
    },
    // keeper → tarae
    /// It's there; with an `O_PATH` handle of each device place it has (`places`, the names — the fds in their
    /// order), opened in its own mount namespace before any run (`keeper::DEVICE_HANDLES`).
    Ready {
        pid: u32,
        places: Vec<Vec<u8>>,
    },
    /// The run's process is there; its pidfd comes with it. `pid` is the keeper's number for it — in a PID
    /// namespace not tarae's (tarae reads its own from the pidfd).
    Started {
        id: u64,
        pid: u32,
    },
    Failed {
        id: u64,
        errno: i32,
        step: Step,
    },
    /// The run reaped: its `wait` status.
    Exited {
        id: u64,
        status: i32,
    },
    /// Orphans reaped since the last one said.
    Reaped {
        n: u32,
    },
    Usage {
        rss_kb: u64,
        pss_kb: u64,
    },
    /// `Plant`'s answer: the sockets made, each its kind (`redirect::Kind::code`; each once) and its descriptor in
    /// that order — and `errno`, 0 or what failed the next one (then those made so far).
    Planted {
        kinds: Vec<u8>,
        errno: i32,
    },
}

impl Msg {
    fn kind(&self) -> u8 {
        match self {
            Msg::Run(_) => 1,
            Msg::Signal { .. } => 2,
            Msg::AskUsage => 3,
            Msg::End => 4,
            Msg::Plant { .. } => 5,
            Msg::Ready { .. } => 64,
            Msg::Started { .. } => 65,
            Msg::Failed { .. } => 66,
            Msg::Exited { .. } => 67,
            Msg::Reaped { .. } => 68,
            Msg::Usage { .. } => 69,
            Msg::Planted { .. } => 70,
        }
    }

    /// The descriptors the message takes, exactly.
    pub fn fds(&self) -> usize {
        match self {
            Msg::Run(r) => 4 + (r.served() || r.held) as usize,
            Msg::Ready { places, .. } => places.len(),
            Msg::Started { .. } => 1,
            Msg::Planted { kinds, .. } => kinds.len(),
            _ => 0,
        }
    }
}

/// Why a record isn't a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a malformed message: {}", self.0)
    }
}

/// `msg` as its record. Err: past a limit (a request tarae can't send).
pub fn encode(msg: &Msg) -> Result<Vec<u8>, Refused> {
    let mut b = vec![msg.kind()];
    match msg {
        Msg::Run(r) => {
            b.extend(r.id.to_le_bytes());
            bytes(&mut b, &r.program);
            list(&mut b, &r.argv)?;
            list(&mut b, &r.envp)?;
            bytes(&mut b, &r.cwd);
            bytes(&mut b, &r.comm);
            let net = NETS.iter().position(|n| *n == r.net).unwrap_or(0) as u8;
            b.extend([net, r.held as u8, r.no_userns as u8]);
            valid_run(r)?;
        }
        Msg::Signal { id, sig } => {
            b.extend(id.to_le_bytes());
            b.extend(sig.to_le_bytes());
        }
        Msg::AskUsage | Msg::End => {}
        Msg::Plant { v4, v6 } => b.extend([*v4 as u8, *v6 as u8]),
        Msg::Ready { pid, places } => {
            if places.len() > FDS_MAX {
                return Err(Refused("more places than a message takes descriptors"));
            }
            b.extend(pid.to_le_bytes());
            list(&mut b, places)?;
        }
        Msg::Started { id, pid } => {
            b.extend(id.to_le_bytes());
            b.extend(pid.to_le_bytes());
        }
        Msg::Failed { id, errno, step } => {
            b.extend(id.to_le_bytes());
            b.extend(errno.to_le_bytes());
            b.push(step.code());
        }
        Msg::Exited { id, status } => {
            b.extend(id.to_le_bytes());
            b.extend(status.to_le_bytes());
        }
        Msg::Reaped { n } => b.extend(n.to_le_bytes()),
        Msg::Usage { rss_kb, pss_kb } => {
            b.extend(rss_kb.to_le_bytes());
            b.extend(pss_kb.to_le_bytes());
        }
        Msg::Planted { kinds, errno } => {
            valid_kinds(kinds)?;
            bytes(&mut b, kinds);
            b.extend(errno.to_le_bytes());
        }
    }
    if b.len() > MSG_MAX {
        return Err(Refused("past the longest record"));
    }
    Ok(b)
}

fn bytes(b: &mut Vec<u8>, s: &[u8]) {
    b.extend((s.len() as u32).to_le_bytes());
    b.extend(s);
}

fn list(b: &mut Vec<u8>, items: &[Vec<u8>]) -> Result<(), Refused> {
    if items.len() > LIST_MAX {
        return Err(Refused("a list past its limit"));
    }
    b.extend((items.len() as u32).to_le_bytes());
    for s in items {
        bytes(b, s);
    }
    Ok(())
}

/// What makes `Planted`'s kinds ones tarae takes: at most `FDS_MAX`, each a kind (`redirect::Kind`), none twice.
fn valid_kinds(kinds: &[u8]) -> Result<(), Refused> {
    if kinds.len() > FDS_MAX {
        return Err(Refused("more sockets than a message takes descriptors"));
    }
    for (i, k) in kinds.iter().enumerate() {
        if super::redirect::Kind::from_code(*k).is_none() {
            return Err(Refused("a socket kind that isn't one"));
        }
        if kinds[..i].contains(k) {
            return Err(Refused("a socket kind twice"));
        }
    }
    Ok(())
}

/// What makes a run's strings ones `execve` takes: no NUL anywhere, a program, a folder and a name.
fn valid_run(r: &Run) -> Result<(), Refused> {
    let nul = |s: &[u8]| s.contains(&0);
    if r.program.is_empty() || r.cwd.is_empty() || r.argv.is_empty() {
        return Err(Refused("a run without its program, folder or arguments"));
    }
    if r.comm.is_empty() || r.comm.len() > COMM_MAX {
        return Err(Refused("a run's name past the kernel's 15 bytes"));
    }
    if [&r.program, &r.cwd, &r.comm].into_iter().any(|s| nul(s))
        || r.argv.iter().chain(&r.envp).any(|s| nul(s))
    {
        return Err(Refused("a NUL inside a string"));
    }
    if r.envp.iter().any(|e| !e.contains(&b'=')) {
        return Err(Refused("an environment entry without '='"));
    }
    Ok(())
}

/// The record read so far.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Refused> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or(Refused("cut short"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Refused> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8, Refused> {
        Ok(self.array::<1>()?[0])
    }

    fn flag(&mut self) -> Result<bool, Refused> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Refused("a flag that isn't 0 or 1")),
        }
    }

    fn u32(&mut self) -> Result<u32, Refused> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, Refused> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, Refused> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, Refused> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn list(&mut self) -> Result<Vec<Vec<u8>>, Refused> {
        let n = self.u32()? as usize;
        // (each item is at least its length: a count past what's left can't be true)
        if n > LIST_MAX || n > (self.b.len() - self.at) / 4 {
            return Err(Refused("a list past its limit"));
        }
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(self.bytes()?);
        }
        Ok(items)
    }

    fn done(&self) -> Result<(), Refused> {
        if self.at == self.b.len() { Ok(()) } else { Err(Refused("bytes left over")) }
    }
}

/// The message in record `b`, which came with `fds` descriptors.
pub fn decode(b: &[u8], fds: usize) -> Result<Msg, Refused> {
    if b.len() > MSG_MAX {
        return Err(Refused("past the longest record"));
    }
    let mut r = Reader { b, at: 0 };
    let msg = match r.u8()? {
        1 => {
            let run = Run {
                id: r.u64()?,
                program: r.bytes()?,
                argv: r.list()?,
                envp: r.list()?,
                cwd: r.bytes()?,
                comm: r.bytes()?,
                net: *NETS.get(usize::from(r.u8()?)).ok_or(Refused("a network mode it doesn't know"))?,
                held: r.flag()?,
                no_userns: r.flag()?,
            };
            valid_run(&run)?;
            Msg::Run(run)
        }
        2 => Msg::Signal { id: r.u64()?, sig: r.i32()? },
        3 => Msg::AskUsage,
        4 => Msg::End,
        5 => Msg::Plant { v4: r.flag()?, v6: r.flag()? },
        64 => {
            let pid = r.u32()?;
            let places = r.list()?;
            if places.len() > FDS_MAX {
                return Err(Refused("more places than a message takes descriptors"));
            }
            Msg::Ready { pid, places }
        }
        65 => Msg::Started { id: r.u64()?, pid: r.u32()? },
        66 => Msg::Failed {
            id: r.u64()?,
            errno: r.i32()?,
            step: Step::from_code(r.u8()?).ok_or(Refused("a step that isn't one"))?,
        },
        67 => Msg::Exited { id: r.u64()?, status: r.i32()? },
        68 => Msg::Reaped { n: r.u32()? },
        69 => Msg::Usage { rss_kb: r.u64()?, pss_kb: r.u64()? },
        70 => {
            let kinds = r.bytes()?;
            valid_kinds(&kinds)?;
            Msg::Planted { kinds, errno: r.i32()? }
        }
        _ => return Err(Refused("a kind that isn't one")),
    };
    r.done()?;
    if fds != msg.fds() {
        return Err(Refused("descriptors its kind doesn't take"));
    }
    Ok(msg)
}

/// Sends `msg` with `fds` (exactly as many as its kind takes) on `sock`, one record.
pub fn send(sock: BorrowedFd, msg: &Msg, fds: &[BorrowedFd]) -> io::Result<()> {
    if fds.len() != msg.fds() || fds.len() > FDS_MAX {
        return Err(io::Error::other("descriptors its kind doesn't take"));
    }
    let b = encode(msg).map_err(|e| io::Error::other(e.to_string()))?;
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let mut iov = libc::iovec { iov_base: b.as_ptr().cast_mut().cast(), iov_len: b.len() };
    let mut control = [0u64; control_words()];
    // SAFETY: a zeroed msghdr, then pointed at the iovec and control buffer above, live for the call; the
    // header written in place, within the buffer (`CMSG_SPACE` of the descriptors, ≤ its size).
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &raw mut iov;
        m.msg_iovlen = 1;
        if !raw.is_empty() {
            let len = std::mem::size_of_val(raw.as_slice()) as u32;
            m.msg_control = control.as_mut_ptr().cast();
            m.msg_controllen = libc::CMSG_SPACE(len) as _;
            let c = libc::CMSG_FIRSTHDR(&m);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(len) as _;
            std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), raw.len());
        }
        loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &m, libc::MSG_NOSIGNAL);
            if n >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// The control buffer's size in `u64`s: room for `FDS_MAX` descriptors (aligned as a `cmsghdr` must be).
const fn control_words() -> usize {
    // (CMSG_SPACE isn't const: a header of 16 bytes, the descriptors rounded up to 8)
    (16 + (FDS_MAX * 4).div_ceil(8) * 8) / 8
}

/// A record as `recv` read it: the message — or why it isn't one — and the descriptors that came with it.
pub type Received = (Result<Msg, Refused>, Vec<OwnedFd>);

/// The next record on `sock`: None at its end (the other side gone), else the message — or why it isn't one —
/// with the descriptors that came (close-on-exec, owned: dropped with a refusal). `buf` is reused between calls.
pub fn recv(sock: BorrowedFd, buf: &mut Vec<u8>) -> io::Result<Option<Received>> {
    buf.resize(MSG_MAX + 1, 0);
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut control = [0u64; control_words()];
    // SAFETY: a zeroed msghdr pointed at the buffers above, live for the call; the control data read only
    // within what the kernel says it wrote, each descriptor taken once.
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &raw mut iov;
        m.msg_iovlen = 1;
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &mut m, libc::MSG_CMSG_CLOEXEC);
            if n >= 0 {
                break n as usize;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        };
        let mut fds = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&m);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as usize - c as usize;
                let count = ((*c).cmsg_len as usize).saturating_sub(data) / std::mem::size_of::<RawFd>();
                let at = libc::CMSG_DATA(c).cast::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(at.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&m, c);
        }
        if n == 0 && fds.is_empty() && m.msg_flags & libc::MSG_TRUNC == 0 {
            return Ok(None);
        }
        let msg = if m.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            Err(Refused("a record or its descriptors cut short"))
        } else if fds.len() > FDS_MAX {
            Err(Refused("more descriptors than a message takes"))
        } else {
            decode(&buf[..n], fds.len())
        };
        if msg.is_err() {
            fds.clear();
        }
        Ok(Some((msg, fds)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    /// One of every kind, with fields worth mutating.
    fn every_kind() -> Vec<Msg> {
        let run = |net, held| {
            Msg::Run(Run {
                id: 7,
                program: b"/bin/sh".to_vec(),
                argv: vec![b"sh".to_vec(), b"-c".to_vec(), b"exit 3".to_vec()],
                envp: vec![b"PATH=/usr/bin:/bin".to_vec(), b"TARAE_SANDBOXED=tarae-1-2.".to_vec()],
                cwd: b"/tmp/project".to_vec(),
                comm: b"tl-12".to_vec(),
                net,
                held,
                // (one way and the other, by the net mode: both values on the wire)
                no_userns: net == RunNet::Routed,
            })
        };
        vec![
            run(RunNet::Plain, false),
            run(RunNet::Served, false),
            run(RunNet::Own, true),
            run(RunNet::Served, true),
            run(RunNet::Routed, true),
            Msg::Signal { id: 7, sig: libc::SIGTERM },
            Msg::AskUsage,
            Msg::End,
            Msg::Ready { pid: 4242, places: Vec::new() },
            Msg::Ready {
                pid: 1,
                places: vec![b"/dev/pts".to_vec(), b"/dev/pts/ptmx".to_vec(), b"/dev/shm".to_vec()],
            },
            Msg::Started { id: 7, pid: 4243 },
            Msg::Failed { id: 7, errno: libc::ENOENT, step: Step::Exec },
            Msg::Exited { id: 7, status: 3 << 8 },
            Msg::Reaped { n: 2 },
            Msg::Usage { rss_kb: 3100, pss_kb: 900 },
            Msg::Plant { v4: true, v6: false },
            Msg::Plant { v4: true, v6: true },
            Msg::Planted { kinds: vec![0, 1, 2, 4], errno: 0 },
            Msg::Planted { kinds: vec![0, 1, 2, 3, 4, 5, 6], errno: 0 },
            Msg::Planted { kinds: vec![0], errno: libc::EMFILE },
        ]
    }

    /// `Planted` names each socket by a kind tarae knows, once, and carries exactly as many descriptors: a kind
    /// that isn't one, one twice, or more than a message takes is a refusal — tarae never guesses what a descriptor
    /// is.
    #[test]
    fn planted_takes_only_known_kinds_once() {
        let record = |kinds: &[u8]| {
            let mut b = vec![70u8];
            b.extend((kinds.len() as u32).to_le_bytes());
            b.extend(kinds);
            b.extend(0i32.to_le_bytes());
            b
        };
        assert_eq!(decode(&record(&[0, 1, 5]), 3), Ok(Msg::Planted { kinds: vec![0, 1, 5], errno: 0 }));
        assert_eq!(decode(&record(&[0, 1, 5]), 2), Err(Refused("descriptors its kind doesn't take")));
        assert_eq!(decode(&record(&[0, 7]), 2), Err(Refused("a socket kind that isn't one")));
        assert_eq!(decode(&record(&[0, 2, 2]), 3), Err(Refused("a socket kind twice")));
        let nine = [0u8; 9];
        assert!(decode(&record(&nine), 9).is_err(), "past FDS_MAX");
        assert!(encode(&Msg::Planted { kinds: vec![7], errno: 0 }).is_err(), "tarae's side refuses it too");
    }

    /// A small fixed-seed generator: the loop is the same every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    /// Whatever a record holds, the parser answers a message or a refusal — never a panic — and a message it
    /// answers is that record exactly (it encodes back to the same bytes) with the descriptors its kind takes.
    /// Each kind's record mutated: bytes flipped, cut, extended, a 4-byte field set past the limits, and any
    /// count of descriptors.
    #[test]
    fn the_channel_parser_never_panics() {
        let records: Vec<(Vec<u8>, usize)> =
            every_kind().iter().map(|m| (encode(m).unwrap(), m.fds())).collect();
        for (b, fds) in &records {
            assert_eq!(encode(&decode(b, *fds).unwrap()).unwrap(), *b, "each kind round-trips");
        }
        let mut rng = Rng(0x7a7a_e5ee_d1ce_0001);
        let (mut messages, mut refusals) = (0, 0);
        for _ in 0..200_000 {
            let (base, base_fds) = &records[rng.below(records.len())];
            let mut b = base.clone();
            for _ in 0..1 + rng.below(3) {
                match rng.below(5) {
                    0 if !b.is_empty() => {
                        let at = rng.below(b.len());
                        b[at] ^= 1 << rng.below(8);
                    }
                    1 => b.truncate(rng.below(b.len() + 1)),
                    2 => b.extend((0..1 + rng.below(16)).map(|_| rng.next() as u8)),
                    3 if b.len() >= 4 => {
                        let at = rng.below(b.len() - 3);
                        let big = [u32::MAX, LIST_MAX as u32 + 1, MSG_MAX as u32, 1 << 31, 5][rng.below(5)];
                        b[at..at + 4].copy_from_slice(&big.to_le_bytes());
                    }
                    _ => {}
                }
            }
            let fds = if rng.below(4) == 0 { rng.below(FDS_MAX + 2) } else { *base_fds };
            match decode(&b, fds) {
                Ok(m) => {
                    messages += 1;
                    assert_eq!(encode(&m).as_deref(), Ok(&b[..]), "a message is its record exactly");
                    assert_eq!(m.fds(), fds);
                }
                Err(_) => refusals += 1,
            }
        }
        assert!(messages > 1000 && refusals > 10_000, "both kinds of answer: {messages} / {refusals}");
        // Past the longest record, whatever it holds
        assert!(decode(&vec![4u8; MSG_MAX + 1], 0).is_err());
    }

    /// A message and its descriptors cross a `SOCK_SEQPACKET` pair as they went in; the descriptors come
    /// close-on-exec; the other end gone, `recv` says so (None).
    #[test]
    fn a_message_and_its_descriptors_cross_the_socket() {
        let (a, b) = pair();
        let (r, _w) = io::pipe().unwrap();
        let started = Msg::Started { id: 9, pid: 77 };
        send(a.as_fd(), &started, &[r.as_fd()]).unwrap();
        assert!(send(a.as_fd(), &started, &[]).is_err(), "too few descriptors aren't sent");
        let mut buf = Vec::new();
        let (msg, fds) = recv(b.as_fd(), &mut buf).unwrap().unwrap();
        assert_eq!((msg, fds.len()), (Ok(started), 1));
        // SAFETY: a flag read on a descriptor we own.
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "close-on-exec");
        drop(a);
        assert!(recv(b.as_fd(), &mut buf).unwrap().is_none(), "the other end gone");
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: a two-int array for the call to fill.
        let r = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr())
        };
        assert_eq!(r, 0);
        // SAFETY: both just made, ours.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }
}
