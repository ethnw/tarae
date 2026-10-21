//! The properties `wire`'s readers keep, one function a reader — whatever the bytes, each holds or the function
//! panics: the in-tree loops (`tests.rs`) call them on seeded mutations, the fuzz targets (`fuzz/parsers`, built with
//! `--cfg fuzzing`) on libFuzzer's inputs. With them what they're held to: the kernel's reading of a netlink batch
//! (`kernel_changes`) and of a message (`kernel`), over a run's memory as a test lays it out (`Fake`).

use std::cell::RefCell;

use super::*;

/// A run's memory as a test lays it out: pieces at addresses, nothing between them (`EFAULT`, as `/proc/<tid>/mem`
/// says it) — and every read asked of it, `(at, len)` in order.
#[derive(Default, Clone)]
pub(crate) struct Fake {
    pub(crate) pieces: Vec<(u64, Vec<u8>)>,
    pub(crate) reads: RefCell<Vec<(u64, usize)>>,
}

impl Fake {
    pub(crate) fn at(mut self, at: u64, bytes: impl Into<Vec<u8>>) -> Fake {
        self.pieces.push((at, bytes.into()));
        self
    }

    /// What's there, unrecorded — a read of nothing anywhere (`read_exact_at` asks the kernel nothing).
    pub(crate) fn peek(&self, at: u64, len: usize) -> Result<Vec<u8>, i32> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let within = |(base, b): &(u64, Vec<u8>)| {
            let from = usize::try_from(at.checked_sub(*base)?).ok()?;
            b.get(from..from.checked_add(len)?).map(<[u8]>::to_vec)
        };
        self.pieces.iter().find_map(within).ok_or(libc::EFAULT)
    }

    /// A message's memory as a fuzz input holds it: a byte of flags (1: a datagram), where its header is (8 bytes),
    /// then pieces — each its address (8 bytes), its length (2 bytes) and its bytes; a piece cut short is what's
    /// left. Numbers little-endian. (Where the header is, whether it's a datagram, the memory.)
    pub(crate) fn from_input(b: &[u8]) -> Option<(u64, bool, Fake)> {
        let word = |w: &[u8]| u64::from_le_bytes(w.try_into().unwrap_or_default());
        let (flags, at, mut rest) = (*b.first()?, word(b.get(1..9)?), &b[9..]);
        let mut m = Fake::default();
        while rest.len() >= 10 {
            let len = usize::from(u16::from_le_bytes([rest[8], rest[9]])).min(rest.len() - 10);
            m = m.at(word(&rest[..8]), &rest[10..10 + len]);
            rest = &rest[10 + len..];
        }
        Some((at, flags & 1 == 1, m))
    }

    /// `from_input`'s input for this memory (the fuzz seeds), each piece cut to 64 KiB.
    pub(crate) fn input(&self, at: u64, datagram: bool) -> Vec<u8> {
        let mut b = vec![u8::from(datagram)];
        b.extend(at.to_le_bytes());
        for (base, piece) in &self.pieces {
            let piece = &piece[..piece.len().min(usize::from(u16::MAX))];
            b.extend(base.to_le_bytes());
            b.extend((piece.len() as u16).to_le_bytes());
            b.extend(piece);
        }
        b
    }
}

impl Mem for Fake {
    fn read(&self, at: u64, len: usize) -> Result<Vec<u8>, i32> {
        self.reads.borrow_mut().push((at, len));
        self.peek(at, len)
    }
}

/// A `struct cmsghdr` and its data, padded to 8 as `CMSG_SPACE` pads it.
pub(crate) fn cmsg(level: i32, kind: i32, data: &[u8]) -> Vec<u8> {
    let mut c = (16 + data.len() as u64).to_ne_bytes().to_vec();
    c.extend(level.to_ne_bytes());
    c.extend(kind.to_ne_bytes());
    c.extend(data);
    c.resize(c.len().next_multiple_of(8), 0);
    c
}

pub(crate) fn rights(fds: &[i32]) -> Vec<u8> {
    cmsg(SOL_SOCKET, SCM_RIGHTS, &fds.iter().flat_map(|f| f.to_ne_bytes()).collect::<Vec<u8>>())
}

/// The run's descriptors a test passes (`Served::passed` stands in): `0..8` are, each a new descriptor of tarae's
/// (one of `/dev/null`); any other `EBADF`.
pub(crate) fn passer(numbers: &mut Vec<(i32, i32)>) -> impl FnMut(i32) -> Result<OwnedFd, i32> + '_ {
    let null = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    move |theirs| {
        if !(0..8).contains(&theirs) {
            return Err(libc::EBADF);
        }
        let ours = null.try_clone().map_err(|_| libc::EMFILE)?;
        numbers.push((theirs, ours.as_raw_fd()));
        Ok(ours)
    }
}

/// Whatever an address holds, `parse` reads it as the kernel does: an internet address only from its family with
/// enough bytes for it (8, 24), its port and address where the struct has them; a unix path up to its first NUL,
/// never empty, never holding one; an abstract name the bytes after the leading NUL; no name only with nothing
/// after the family; `EINVAL` only for what's too short for its family. What it read, in a word.
pub(crate) fn address(b: &[u8]) -> &'static str {
    let family = (b.len() >= 2).then(|| i32::from(u16::from_ne_bytes([b[0], b[1]])));
    let port = || u16::from_be_bytes([b[2], b[3]]);
    match (parse(b), family) {
        (Err(e), f) => {
            let short =
                f.is_none() || f == Some(AF_INET) && b.len() < 8 || f == Some(AF_INET6) && b.len() < 24;
            assert!(e == libc::EINVAL && short, "{e} for {b:?}");
            "refused"
        }
        (Ok(Addr::Ip(a)), Some(AF_INET)) => {
            let ip: [u8; 4] = b[4..8].try_into().unwrap();
            assert_eq!(a, SocketAddr::new(IpAddr::from(ip), port()));
            "v4"
        }
        (Ok(Addr::Ip(a)), Some(AF_INET6)) => {
            let ip: [u8; 16] = b[8..24].try_into().unwrap();
            assert_eq!(a, SocketAddr::new(IpAddr::from(ip), port()));
            "v6"
        }
        (Ok(Addr::Path(p)), Some(AF_UNIX)) => {
            let p = p.as_os_str().as_bytes();
            let ends = b.get(2 + p.len()).is_none_or(|&c| c == 0);
            assert!(!p.is_empty() && !p.contains(&0) && b[2..].starts_with(p) && ends, "{p:?} of {b:?}");
            "path"
        }
        (Ok(Addr::Abstract(n)), Some(AF_UNIX)) => {
            assert!(b[2] == 0 && n == b[3..], "{n:?} of {b:?}");
            "abstract"
        }
        (Ok(Addr::Unnamed), Some(AF_UNIX)) => {
            assert_eq!(b.len(), 2);
            "unnamed"
        }
        (Ok(Addr::Unspec), Some(AF_UNSPEC)) => "unspec",
        (Ok(Addr::Netlink), Some(AF_NETLINK)) => "netlink",
        (Ok(Addr::Other), Some(f)) => {
            assert!(![AF_UNSPEC, AF_UNIX, AF_INET, AF_INET6, AF_NETLINK].contains(&f), "{b:?}");
            "other"
        }
        (got, f) => panic!("{got:?} for family {f:?}: {b:?}"),
    }
}

/// The kernel's reading of a netlink batch, as a model (`netlink_rcv_skb`, `rtnetlink_rcv_msg`, 6.1–7.0): the types
/// of the messages it acts on as changes — each whole (its length at least its header's and within what's left;
/// one cut short ends the batch), a request (`NLM_F_REQUEST`; else only acked), not a control message (type < 16),
/// not a get (`(type − 16) & 3 == 2`; a type past `RTM_MAX` counted as a change too). The next at the 4-byte
/// alignment, or the batch's end.
pub(crate) fn kernel_changes(mut b: &[u8]) -> Vec<u16> {
    let mut changes = Vec::new();
    while b.len() >= 16 {
        let len = u32::from_ne_bytes(b[..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes([b[4], b[5]]);
        let flags = u16::from_ne_bytes([b[6], b[7]]);
        if len < 16 || len > b.len() {
            break;
        }
        if flags & NLM_F_REQUEST as u16 != 0 && kind >= 16 && (kind - 16) & 3 != 2 {
            changes.push(kind);
        }
        b = &b[len.next_multiple_of(4).min(b.len())..];
    }
    changes
}

/// A batch `netlink_reads` takes, the kernel reads as gets only (`kernel_changes` finds none). What it said.
pub(crate) fn netlink(b: &[u8]) -> Result<(), String> {
    let said = netlink_reads(b);
    if said.is_ok() {
        assert_eq!(kernel_changes(b), [0u16; 0], "a change the kernel would act on: {b:?}");
    }
    said
}

/// What the kernel takes from a message.
#[derive(Debug, PartialEq)]
pub(crate) struct Read {
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) data: Vec<u8>,
    pub(crate) control: Vec<u8>,
    /// Where in the control each descriptor passed sits.
    pub(crate) slots: Vec<usize>,
    /// How many descriptors each `SCM_RIGHTS` holds, in order.
    pub(crate) rights: Vec<usize>,
}

/// Why the kernel refuses a message: the errno, and the step that says it.
pub(crate) type Refused = (i32, &'static str);

/// What Linux's `sendmsg` makes of the message at `at` in `m` (6.1–7.0: `net/socket.c`, `lib/iov_iter.c`,
/// `net/core/scm.c`), written from its rules — not from `read_msg` — with tarae's own limits where `read_msg` says
/// it has them (1 MiB of data, 64 KiB of control, no control message but `SCM_RIGHTS`): the run's memory it copies,
/// in order, and what comes of it. Step by step:
/// - `copy_msghdr_from_user`: the header (`EFAULT`).
/// - `__copy_msghdr`: `msg_namelen` is an `int` — none without a pointer, below 0 `EINVAL`, past
///   `sockaddr_storage` cut to it — the name copied (`move_addr_to_kernel`, `EFAULT`); past `UIO_MAXIOV` iovecs
///   `EMSGSIZE`.
/// - `import_iovec`: none read for none; else `copy_iovec_from_user` takes each `struct iovec` in turn (`EFAULT`),
///   a length that isn't an `ssize_t` `EINVAL` — every one before any buffer; `__import_iovec` totals them (cut at
///   `MAX_RW_COUNT`, past tarae's 1 MiB whatever the page): a datagram past 1 MiB `EMSGSIZE` (tarae's limit, said
///   before any buffer is read; the kernel's own is the protocol's).
/// - The data, buffer by buffer, up to 1 MiB (`EFAULT`) — before the control, tarae's order: the kernel copies it
///   in the protocol's `sendmsg`, around the control's checks as each protocol has it.
/// - `____sys_sendmsg`: the control copied when it has a length (`EFAULT`; past 64 KiB `ENOBUFS`, tarae's — the
///   kernel's is `optmem_max`).
/// - `__scm_send`: each `cmsghdr` whole (`CMSG_OK`, else `EINVAL`), the next at the 8-byte alignment while a
///   header fits; `SCM_RIGHTS` at most `SCM_MAX_FD` (253) in all (`EINVAL`, before any of that header's is
///   taken), each held (`scm_fp_copy`'s `EBADF` — `passer`'s 0–7); any other level or type tarae's `EPERM` (the
///   kernel's: `SOL_SOCKET`'s credentials taken, its other types `EINVAL`, another level's the protocol's).
pub(crate) fn kernel(m: &Fake, at: u64, datagram: bool) -> (Vec<(u64, usize)>, Result<Read, Refused>) {
    let copies = RefCell::new(Vec::new());
    let copy = |a: u64, len: usize, step: &'static str| {
        copies.borrow_mut().push((a, len));
        m.peek(a, len).map_err(|e| (e, step))
    };
    let long = |b: &[u8], o: usize| u64::from_ne_bytes(b[o..o + 8].try_into().unwrap());
    let int = |b: &[u8], o: usize| i32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
    let came = (|| {
        let h = copy(at, 56, "header")?;
        let namelen = if long(&h, 0) == 0 { 0 } else { int(&h, 8) };
        if namelen < 0 {
            return Err((libc::EINVAL, "name length"));
        }
        let name = match namelen.min(128) as usize {
            0 => None,
            len => Some(copy(long(&h, 0), len, "name")?),
        };
        let (uiov, segs) = (long(&h, 16), long(&h, 24));
        if segs > 1024 {
            return Err((libc::EMSGSIZE, "iovecs"));
        }
        let mut iov = Vec::new();
        if segs > 0 {
            copies.borrow_mut().push((uiov, 16 * segs as usize));
        }
        for k in 0..segs {
            let at = uiov.checked_add(16 * k).ok_or((libc::EFAULT, "iovec"))?;
            let v = m.peek(at, 16).map_err(|e| (e, "iovec"))?;
            let len = long(&v, 8) as i64;
            if len < 0 {
                return Err((libc::EINVAL, "iovec length"));
            }
            iov.push((long(&v, 0), len as usize));
        }
        let total: u128 = iov.iter().map(|&(_, len)| len as u128).sum();
        if datagram && total > SEND_MAX as u128 {
            return Err((libc::EMSGSIZE, "datagram"));
        }
        let mut data = Vec::new();
        for (base, len) in iov {
            if data.len() == SEND_MAX {
                break;
            }
            data.extend(copy(base, len.min(SEND_MAX - data.len()), "data")?);
        }
        let control = match long(&h, 40) {
            0 => Vec::new(),
            len if len > CONTROL_MAX as u64 => return Err((libc::ENOBUFS, "control length")),
            len => copy(long(&h, 32), len as usize, "control")?,
        };
        let (mut slots, mut rights, mut o) = (Vec::new(), Vec::new(), 0);
        while o + 16 <= control.len() {
            let len = long(&control, o);
            if len < 16 || len > (control.len() - o) as u64 {
                return Err((libc::EINVAL, "cmsg length"));
            }
            let len = len as usize;
            match (int(&control, o + 8), int(&control, o + 12)) {
                (SOL_SOCKET, SCM_RIGHTS) => {
                    let fds = (len - 16) / 4;
                    if slots.len() + fds > 253 {
                        return Err((libc::EINVAL, "SCM_MAX_FD"));
                    }
                    for s in (0..fds).map(|i| o + 16 + 4 * i) {
                        if !(0..8).contains(&int(&control, s)) {
                            return Err((libc::EBADF, "a descriptor not held"));
                        }
                        slots.push(s);
                    }
                    rights.push(fds);
                }
                (SOL_SOCKET, SCM_CREDENTIALS) => return Err((libc::EPERM, "credentials")),
                _ => return Err((libc::EPERM, "another control message")),
            }
            o += len.next_multiple_of(8);
        }
        Ok(Read { name, data, control, slots, rights })
    })();
    (copies.into_inner(), came)
}

/// Where `read_msg` reads the message at `at` apart from the kernel — each a difference known and named (the
/// loop counts them; `read_apart_from_the_kernel` pins each); `None`: it must read it as the kernel does.
fn apart(
    m: &Fake,
    at: u64,
    got: &Result<Msg, Unsent>,
    kernel: &Result<Read, Refused>,
) -> Option<&'static str> {
    let h = m.peek(at, MSGHDR).ok()?;
    let long = |o: usize| u64::from_ne_bytes(h[o..o + 8].try_into().unwrap());
    let namelen = i32::from_ne_bytes(h[8..12].try_into().unwrap());
    let refused = got.as_ref().err().map(Unsent::errno);
    match kernel {
        // (F3) a name past `sockaddr_storage`: the kernel cuts it to 128 bytes, tarae refuses it
        _ if long(0) != 0 && namelen > ADDR_MAX as i32 && refused == Some(libc::EINVAL) => {
            Some("F3 name past 128")
        }
        // (F4) a control length with no pointer: the kernel's copy faults, tarae reads no control and goes on
        Err((libc::EFAULT, "control")) if long(32) == 0 => Some("F4 control at 0"),
        // the iovec array not all there: tarae reads it whole (`EFAULT`), the kernel meets a bad length first
        Err((libc::EINVAL, "iovec length"))
            if refused == Some(libc::EFAULT) && m.peek(long(16), 16 * long(24) as usize).is_err() =>
        {
            Some("iovec array cut")
        }
        _ => None,
    }
}

/// Whatever a message holds and wherever its pointers land, `read_msg` never panics and reads it as the kernel does
/// (`kernel`) but where it's known to differ (`apart`): the same memory read, in order — each pointer the run gave
/// once; what tarae decides on is what it read, never fetched again — at most a header, an address, 1024 iovecs,
/// 1 MiB of data and 64 KiB of control asked in all; the kernel's errno, else its address and data, and a control
/// tarae built: a header per `SCM_RIGHTS`, tarae's descriptor for each number the run passed there. What came of
/// it: the kernel's step that refused it, a difference `apart` names, or taken (with descriptors or without).
pub(crate) fn message(m: &Fake, at: u64, datagram: bool) -> &'static str {
    let most = MSGHDR + ADDR_MAX + UIO_MAXIOV as usize * 16 + SEND_MAX + CONTROL_MAX;
    let touched = |r: &[(u64, usize)]| r.iter().copied().filter(|r| r.1 > 0).collect::<Vec<_>>();
    m.reads.borrow_mut().clear();
    let (copies, said) = kernel(m, at, datagram);
    let mut passed = Vec::new();
    let got = read_msg(m, passer(&mut passed), at, datagram);
    let reads = m.reads.borrow().clone();
    assert!(reads.iter().map(|r| r.1).sum::<usize>() <= most, "{reads:?}");
    if let Some(why) = apart(m, at, &got, &said) {
        return why;
    }
    assert_eq!(touched(&reads), touched(&copies), "the memory read, in order");
    match (got, said) {
        (Err(e), Err((w, step))) => {
            assert_eq!(e.errno(), w, "{step}");
            step
        }
        (Ok(msg), Ok(r)) => {
            assert_eq!((&msg.name, &msg.data), (&r.name, &r.data));
            let theirs: Vec<i32> = r
                .slots
                .iter()
                .map(|&s| i32::from_ne_bytes(r.control[s..s + 4].try_into().unwrap()))
                .collect();
            assert_eq!(passed.iter().map(|p| p.0).collect::<Vec<_>>(), theirs, "each number the run passed");
            let ours: Vec<i32> = msg.fds.iter().map(AsRawFd::as_raw_fd).collect();
            assert_eq!(passed.iter().map(|p| p.1).collect::<Vec<_>>(), ours);
            let mut left = &ours[..];
            let built: Vec<u8> = r
                .rights
                .iter()
                .flat_map(|&n| {
                    let (these, rest) = left.split_at(n);
                    left = rest;
                    rights(these)
                })
                .collect();
            assert_eq!(msg.control, built, "tarae's descriptors in tarae's headers, nothing else");
            if r.slots.is_empty() { "taken" } else { "taken with descriptors" }
        }
        (got, kernel) => panic!("{:?} where the kernel says {kernel:?}", got.map(|m| m.data)),
    }
}
