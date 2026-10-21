use std::cell::RefCell;
use std::collections::BTreeMap;

use super::*;

/// A run's memory as a test lays it out: pieces at addresses, nothing between them (`EFAULT`, as `/proc/<tid>/mem`
/// says it) — and every read asked of it, `(at, len)` in order.
#[derive(Default, Clone)]
struct Fake {
    pieces: Vec<(u64, Vec<u8>)>,
    reads: RefCell<Vec<(u64, usize)>>,
}

impl Fake {
    fn at(mut self, at: u64, bytes: impl Into<Vec<u8>>) -> Fake {
        self.pieces.push((at, bytes.into()));
        self
    }

    /// What's there, unrecorded — a read of nothing anywhere (`read_exact_at` asks the kernel nothing).
    fn peek(&self, at: u64, len: usize) -> Result<Vec<u8>, i32> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let within = |(base, b): &(u64, Vec<u8>)| {
            let from = usize::try_from(at.checked_sub(*base)?).ok()?;
            b.get(from..from.checked_add(len)?).map(<[u8]>::to_vec)
        };
        self.pieces.iter().find_map(within).ok_or(libc::EFAULT)
    }
}

impl Mem for Fake {
    fn read(&self, at: u64, len: usize) -> Result<Vec<u8>, i32> {
        self.reads.borrow_mut().push((at, len));
        self.peek(at, len)
    }
}

/// Where a test lays a message out: the header, its name, its iovec array, its control, its buffers (`BUFS + k * 0x1000`).
const HEAD: u64 = 0x1_0000;
const NAME: u64 = 0x2_0000;
const IOV: u64 = 0x3_0000;
const CONTROL: u64 = 0x4_0000;
const BUFS: u64 = 0x10_0000;

/// A `struct msghdr`: its name (where, how long), iovec array (where, how many), control (where, how long).
fn msghdr(name: (u64, u32), iov: (u64, u64), control: (u64, u64)) -> Vec<u8> {
    let mut h = vec![0u8; MSGHDR];
    h[0..8].copy_from_slice(&name.0.to_ne_bytes());
    h[8..12].copy_from_slice(&name.1.to_ne_bytes());
    for (at, w) in [(16, iov.0), (24, iov.1), (32, control.0), (40, control.1)] {
        h[at..at + 8].copy_from_slice(&w.to_ne_bytes());
    }
    h
}

fn iovecs(v: &[(u64, u64)]) -> Vec<u8> {
    v.iter().flat_map(|(at, len)| at.to_ne_bytes().into_iter().chain(len.to_ne_bytes())).collect()
}

/// A `struct cmsghdr` and its data, padded to 8 as `CMSG_SPACE` pads it.
fn cmsg(level: i32, kind: i32, data: &[u8]) -> Vec<u8> {
    let mut c = (16 + data.len() as u64).to_ne_bytes().to_vec();
    c.extend(level.to_ne_bytes());
    c.extend(kind.to_ne_bytes());
    c.extend(data);
    c.resize(c.len().next_multiple_of(8), 0);
    c
}

fn rights(fds: &[i32]) -> Vec<u8> {
    cmsg(SOL_SOCKET, SCM_RIGHTS, &fds.iter().flat_map(|f| f.to_ne_bytes()).collect::<Vec<u8>>())
}

fn sockaddr(family: i32, rest: &[u8]) -> Vec<u8> {
    let mut a = (family as u16).to_ne_bytes().to_vec();
    a.extend(rest);
    a
}

fn v4(ip: [u8; 4], port: u16) -> Vec<u8> {
    sockaddr(AF_INET, &[&port.to_be_bytes()[..], &ip, &[0; 8]].concat())
}

/// The run's descriptors a test passes (`Served::passed` stands in): `0..8` are, each a new descriptor of tarae's
/// (one of `/dev/null`); any other `EBADF`.
fn passer(numbers: &mut Vec<(i32, i32)>) -> impl FnMut(i32) -> Result<OwnedFd, i32> + '_ {
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

/// Linux's numbers are libc's there.
#[cfg(target_os = "linux")]
#[test]
fn the_numbers_are_linuxs() {
    let ours = [
        AF_UNSPEC,
        AF_UNIX,
        AF_INET,
        AF_INET6,
        AF_NETLINK,
        SOL_SOCKET,
        SCM_RIGHTS,
        SCM_CREDENTIALS,
        UIO_MAXIOV,
    ];
    let libcs = [
        libc::AF_UNSPEC,
        libc::AF_UNIX,
        libc::AF_INET,
        libc::AF_INET6,
        libc::AF_NETLINK,
        libc::SOL_SOCKET,
        libc::SCM_RIGHTS,
        libc::SCM_CREDENTIALS,
        libc::UIO_MAXIOV,
    ];
    assert_eq!(ours, libcs);
    assert_eq!(
        [NLM_F_REQUEST, NLM_F_ACK, NLM_F_DUMP],
        [libc::NLM_F_REQUEST, libc::NLM_F_ACK, libc::NLM_F_DUMP]
    );
    assert_eq!((MSGHDR, MMSGHDR), (size_of::<libc::msghdr>(), size_of::<libc::mmsghdr>()));
    assert_eq!(MSG_LEN_AT as usize, std::mem::offset_of!(libc::mmsghdr, msg_len));
    // (`SCM_MAX_FD`, which libc doesn't name: the kernel's own answer on a socket pair — the most, and one more)
    let null = std::fs::File::open("/dev/null").unwrap();
    let mut pair = [0; 2];
    // SAFETY: a two-slot array the call fills.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, pair.as_mut_ptr()) }, 0);
    let send = |n: usize| {
        let control = rights(&vec![null.as_raw_fd(); n]);
        let mut byte = [0u8];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        // SAFETY: zeroed, then set to buffers that live for the call.
        let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
        (h.msg_iov, h.msg_iovlen) = (&raw mut iov, 1);
        (h.msg_control, h.msg_controllen) = (control.as_ptr() as *mut libc::c_void, control.len() as _);
        // SAFETY: as above.
        match unsafe { libc::sendmsg(pair[0], &h, 0) } {
            r if r < 0 => Err(std::io::Error::last_os_error().raw_os_error()),
            r => Ok(r),
        }
    };
    assert_eq!((send(SCM_MAX_FD), send(SCM_MAX_FD + 1)), (Ok(1), Err(Some(libc::EINVAL))));
    // SAFETY: ours.
    unsafe {
        libc::close(pair[0]);
        libc::close(pair[1]);
    }
}

/// A message as glibc's `sendmsg` lays it out — an address, two buffers, two descriptors passed — is read whole:
/// the address and the buffers as they are, each descriptor swapped for tarae's own where it sat; each piece read
/// once, in order.
#[test]
fn a_message_is_read_whole_each_piece_once() {
    let to = v4([127, 0, 0, 1], 53);
    let control = rights(&[3, 5]);
    let m = Fake::default()
        .at(HEAD, msghdr((NAME, 16), (IOV, 2), (CONTROL, control.len() as u64)))
        .at(NAME, to.clone())
        .at(IOV, iovecs(&[(BUFS, 3), (BUFS + 0x1000, 2)]))
        .at(BUFS, *b"abc")
        .at(BUFS + 0x1000, *b"de")
        .at(CONTROL, control.clone());
    let mut passed = Vec::new();
    let msg = read_msg(&m, passer(&mut passed), HEAD, true).unwrap();
    assert_eq!((msg.name, msg.data), (Some(to), b"abcde".to_vec()));
    let ours: Vec<i32> = msg.fds.iter().map(AsRawFd::as_raw_fd).collect();
    assert_eq!(passed.iter().map(|p| p.0).collect::<Vec<_>>(), [3, 5]);
    assert_eq!(passed.iter().map(|p| p.1).collect::<Vec<_>>(), ours);
    let mut want = control.clone();
    want[16..20].copy_from_slice(&ours[0].to_ne_bytes());
    want[20..24].copy_from_slice(&ours[1].to_ne_bytes());
    assert_eq!(msg.control, want, "only the descriptors changed");
    let reads = m.reads.borrow().clone();
    let control_len = control.len();
    assert_eq!(
        reads,
        [(HEAD, MSGHDR), (NAME, 16), (IOV, 32), (BUFS, 3), (BUFS + 0x1000, 2), (CONTROL, control_len)]
    );
}

/// What's refused, as the kernel refuses it: credentials (`EPERM` — tarae can't speak for the run), a datagram past
/// 1 MiB (`EMSGSIZE`, no buffer read; a stream's is cut there), a buffer's length past `ssize_t` (`EINVAL`, no
/// buffer read — every length checked first, as `import_iovec` does), more than 1024 buffers (`EMSGSIZE`), control past 64 KiB
/// (`ENOBUFS`), an address past 128 bytes or a control message running past its control (`EINVAL`), memory not
/// there (`EFAULT`).
#[test]
fn what_a_message_may_not_hold() {
    let read = |m: &Fake, datagram: bool| {
        read_msg(m, passer(&mut Vec::new()), HEAD, datagram).map(|m| m.data.len()).map_err(|u| u.errno())
    };
    let with_control = |c: Vec<u8>| {
        let h = msghdr((0, 0), (IOV, 0), (CONTROL, c.len() as u64));
        Fake::default().at(HEAD, h).at(CONTROL, c)
    };
    let creds = cmsg(SOL_SOCKET, SCM_CREDENTIALS, &[0; 12]);
    assert_eq!(read(&with_control(creds.clone()), true), Err(libc::EPERM));
    let mut long = rights(&[1]);
    long[0] = 40;
    assert_eq!(read(&with_control(long), true), Err(libc::EINVAL), "a cmsg past its control");
    let big = Fake::default()
        .at(HEAD, msghdr((0, 0), (IOV, 2), (0, 0)))
        .at(IOV, iovecs(&[(BUFS, 1), (BUFS + 1, SEND_MAX as u64)]))
        .at(BUFS, vec![1; SEND_MAX + 1]);
    assert_eq!(read(&big, true), Err(libc::EMSGSIZE), "a datagram past 1 MiB");
    assert_eq!(*big.reads.borrow(), [(HEAD, MSGHDR), (IOV, 32)], "no buffer read");
    let past = Fake::default()
        .at(HEAD, msghdr((0, 0), (IOV, 2), (0, 0)))
        .at(IOV, iovecs(&[(BUFS, 3), (BUFS, 1 << 63)]))
        .at(BUFS, *b"abc");
    assert_eq!(read(&past, false), Err(libc::EINVAL), "a length past ssize_t");
    assert_eq!(
        *past.reads.borrow(),
        [(HEAD, MSGHDR), (IOV, 32)],
        "no buffer read before every length is checked"
    );
    assert_eq!(read(&big, false), Ok(SEND_MAX), "a stream's cut at 1 MiB");
    let many = Fake::default().at(HEAD, msghdr((0, 0), (IOV, 1025), (0, 0)));
    assert_eq!(read(&many, false), Err(libc::EMSGSIZE));
    let control = Fake::default().at(HEAD, msghdr((0, 0), (IOV, 0), (CONTROL, CONTROL_MAX as u64 + 1)));
    assert_eq!(read(&control, false), Err(libc::ENOBUFS));
    let name = Fake::default().at(HEAD, msghdr((NAME, 129), (IOV, 0), (0, 0)));
    assert_eq!(read(&name, false), Err(libc::EINVAL));
    assert_eq!(name.reads.borrow().len(), 1, "an address too long isn't read");
    assert_eq!(read(&Fake::default(), false), Err(libc::EFAULT));
}

/// A message laid out at `HEAD` with only `control`.
fn with_control(control: Vec<u8>) -> Fake {
    Fake::default().at(HEAD, msghdr((0, 0), (IOV, 0), (CONTROL, control.len() as u64))).at(CONTROL, control)
}

/// Only descriptors go (`SCM_RIGHTS`): every other control message — credentials, `SOL_SOCKET`'s other types,
/// another level's — is refused (`EPERM`), handed back by its level and type for the run's report to name it.
#[test]
fn a_control_message_but_descriptors_is_refused_by_its_type() {
    for (level, kind, name) in [
        (SOL_SOCKET, SCM_CREDENTIALS, "SCM_CREDENTIALS"),
        (SOL_SOCKET, 99, "control message level 1, type 99"),
        (41, 77, "control message level 41, type 77"),
        (0, SCM_RIGHTS, "control message level 0, type 1"),
    ] {
        let m = with_control([rights(&[3]), cmsg(level, kind, &[7; 12])].concat());
        let got = read_msg(&m, passer(&mut Vec::new()), HEAD, false).err();
        assert_eq!(got, Some(Unsent::Control { level, kind }), "{name}");
        assert_eq!(got.map(|u| u.errno()), Some(libc::EPERM));
        assert_eq!(named(level, kind), name);
    }
}

/// At most `SCM_MAX_FD` (253) descriptors in a message: 253 taken; one more `EINVAL`, said before the header holding
/// it passes any — none past the 253rd taken, in one header or across two.
#[test]
fn past_scm_max_fd_descriptors_none_more_is_taken() {
    let read = |control: Vec<u8>| {
        let mut passed = Vec::new();
        let got = read_msg(&with_control(control), passer(&mut passed), HEAD, false).map(|msg| msg.fds.len());
        (got, passed.len())
    };
    assert_eq!(read(rights(&[3; 253])), (Ok(253), 253));
    assert_eq!(read(rights(&[3; 254])), (Err(Unsent::Errno(libc::EINVAL)), 0));
    let two = [rights(&[3; 200]), rights(&[4; 54])].concat();
    assert_eq!(read(two), (Err(Unsent::Errno(libc::EINVAL)), 200));
}

/// What goes to `sendmsg` is a buffer tarae built: a header per `SCM_RIGHTS` of the run's, as long as its
/// descriptors (a tail under 4 bytes left out, as the kernel ignores it), tarae's descriptors, padding zeroed —
/// nothing of the run's bytes, a tail past the last header neither.
#[test]
fn only_a_buffer_tarae_built_goes_out() {
    // (two descriptors and 3 bytes more, then the run's bytes as padding)
    let mut first = rights(&[3, 5]);
    first[..8].copy_from_slice(&27u64.to_ne_bytes());
    first.extend([0xaa; 8]);
    let mut second = rights(&[6]);
    second[20..24].copy_from_slice(&[0xbb; 4]);
    let m = with_control([first, second, vec![0xcc; 10]].concat());
    let mut passed = Vec::new();
    let msg = read_msg(&m, passer(&mut passed), HEAD, false).unwrap();
    let ours: Vec<i32> = msg.fds.iter().map(AsRawFd::as_raw_fd).collect();
    assert_eq!(passed.iter().map(|p| p.0).collect::<Vec<_>>(), [3, 5, 6]);
    assert_eq!(msg.control, [rights(&ours[..2]), rights(&ours[2..])].concat());
}

#[cfg(target_os = "linux")]
#[path = "kernel.rs"]
mod kernel;

/// A small fixed-seed generator: the loops are the same every run.
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

/// Words that mean something to a reader of these structs: lengths at and past each limit, an offset past
/// `i64::MAX` (where `pread` takes none), the end of the address space.
const WORDS: [u64; 22] = [
    0,
    1,
    2,
    7,
    8,
    15,
    16,
    17,
    20,
    56,
    128,
    129,
    1024,
    1025,
    SEND_MAX as u64 - 1,
    SEND_MAX as u64,
    SEND_MAX as u64 + 1,
    CONTROL_MAX as u64,
    CONTROL_MAX as u64 + 1,
    i64::MAX as u64 + 1,
    u64::MAX - 7,
    u64::MAX,
];

/// One to three of: a bit flipped, a byte set, cut, extended, an aligned word (4 or 8 bytes) set to one of `WORDS`
/// or to an address at or just past one of `near` — so a pointer lands on another piece, or its own.
fn mutate(rng: &mut Rng, b: &mut Vec<u8>, near: &[u64]) {
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(b.len());
        match rng.below(6) {
            0 if !b.is_empty() => b[at] ^= 1 << rng.below(8),
            1 if !b.is_empty() => b[at] = rng.next() as u8,
            2 => b.truncate(rng.below(b.len() + 1)),
            3 => b.extend((0..1 + rng.below(16)).map(|_| rng.next() as u8)),
            _ => {
                let w = if rng.below(2) == 0 { 4 } else { 8 };
                if b.len() < w {
                    continue;
                }
                let at = rng.below(b.len() / w) * w;
                let v = match near.len() {
                    n if n > 0 && rng.below(3) == 0 => near[rng.below(n)] + rng.below(24) as u64,
                    _ => WORDS[rng.below(WORDS.len())],
                };
                b[at..at + w].copy_from_slice(&v.to_ne_bytes()[..w]);
            }
        }
    }
}

/// Whatever an address holds, `parse` reads it as the kernel does: an internet address only from its family with
/// enough bytes for it (8, 24), its port and address where the struct has them; a unix path up to its first NUL,
/// never empty, never holding one; an abstract name the bytes after the leading NUL; no name only with nothing
/// after the family; `EINVAL` only for what's too short for its family. Seeded addresses mutated 100,000 times.
#[test]
fn an_address_is_read_as_the_kernel_reads_it() {
    let mut v6 = sockaddr(AF_INET6, &9u16.to_be_bytes());
    v6.extend([0; 4].iter().chain(&std::net::Ipv6Addr::LOCALHOST.octets()).chain(&[0; 4]));
    let seeds = [
        v4([127, 0, 0, 1], 8080),
        v6,
        sockaddr(AF_UNIX, b"/p/s.sock\0junk"),
        sockaddr(AF_UNIX, b"rel.sock"),
        sockaddr(AF_UNIX, b"\0bus"),
        sockaddr(AF_UNIX, b""),
        sockaddr(AF_NETLINK, &[0; 10]),
        sockaddr(AF_UNSPEC, &[0; 14]),
    ];
    let mut rng = Rng(0x5eed_0add_7e55_0002);
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..100_000 {
        let mut b = seeds[rng.below(seeds.len())].clone();
        mutate(&mut rng, &mut b, &[]);
        let family = (b.len() >= 2).then(|| i32::from(u16::from_ne_bytes([b[0], b[1]])));
        let port = || u16::from_be_bytes([b[2], b[3]]);
        let what = match (parse(&b), family) {
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
        };
        *seen.entry(what).or_default() += 1;
    }
    assert!(seen.len() == 9 && seen.values().all(|&n| n > 500), "{seen:?}");
}

/// The kernel's reading of a netlink batch, as a model (`netlink_rcv_skb`, `rtnetlink_rcv_msg`, 6.1–7.0): the types
/// of the messages it acts on as changes — each whole (its length at least its header's and within what's left;
/// one cut short ends the batch), a request (`NLM_F_REQUEST`; else only acked), not a control message (type < 16),
/// not a get (`(type − 16) & 3 == 2`; a type past `RTM_MAX` counted as a change too). The next at the 4-byte
/// alignment, or the batch's end.
fn kernel_changes(mut b: &[u8]) -> Vec<u16> {
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

/// A batch `netlink_reads` takes, the kernel reads as gets only (`kernel_changes` finds none), and a batch whole
/// of gets with a read's flags is taken. Batches of one to three gets, changes and control messages, half of them
/// mutated, 100,000 times.
#[test]
fn a_batch_taken_is_only_reads_as_the_kernel_reads_it() {
    const FLAGS: [i32; 7] = [
        NLM_F_REQUEST | NLM_F_DUMP,
        NLM_F_REQUEST | NLM_F_ACK,
        NLM_F_REQUEST,
        NLM_F_DUMP,
        NLM_F_REQUEST | 0x400, // (NLM_F_CREATE)
        NLM_F_REQUEST | NLM_F_DUMP | 0x800,
        0x10,
    ];
    let mut rng = Rng(0x5eed_0add_7e55_0003);
    let (mut taken, mut refused, mut mutated_taken) = (0, 0, 0);
    for _ in 0..100_000 {
        let (mut b, mut reads) = (Vec::new(), true);
        for _ in 0..1 + rng.below(3) {
            let kind = match rng.below(4) {
                0 => 18 + 4 * rng.below(28) as u16,
                1 => rng.below(16) as u16,
                _ => 16 + rng.below(112) as u16,
            };
            let flags = FLAGS[rng.below(FLAGS.len())];
            reads &=
                kind >= 16 && (kind - 16) % 4 == 2 && flags & !(NLM_F_REQUEST | NLM_F_DUMP | NLM_F_ACK) == 0;
            let payload = rng.below(25);
            let at = b.len();
            b.extend(((16 + payload) as u32).to_ne_bytes());
            b.extend(kind.to_ne_bytes());
            b.extend((flags as u16).to_ne_bytes());
            b.extend((0..8 + payload).map(|_| rng.next() as u8));
            b.resize(at + (16 + payload).next_multiple_of(4), 0);
        }
        let mutated = rng.below(2) == 0;
        if mutated {
            mutate(&mut rng, &mut b, &[]);
        }
        match netlink_reads(&b) {
            Ok(()) => {
                assert_eq!(kernel_changes(&b), [0u16; 0], "a change the kernel would act on: {b:?}");
                taken += 1;
                mutated_taken += usize::from(mutated);
            }
            Err(why) => {
                assert!(mutated || !reads, "a batch of reads refused ({why}): {b:?}");
                refused += 1;
            }
        }
    }
    assert!(
        taken > 5000 && mutated_taken > 1000 && refused > 50_000,
        "{taken} ({mutated_taken}) / {refused}"
    );
}

/// A message as a run lays it out at `HEAD`: maybe an address (internet or unix), up to three buffers of up to 64
/// bytes, maybe control — descriptors passed (some numbers the run doesn't hold: `passer`; now and then past
/// `SCM_MAX_FD`), another level's or `SOL_SOCKET`'s unknown type, credentials.
fn laid_out(rng: &mut Rng) -> Fake {
    let name = match rng.below(3) {
        0 => Vec::new(),
        1 => v4([10, 0, 0, rng.next() as u8], 53),
        _ => sockaddr(AF_UNIX, b"/p/s.sock\0"),
    };
    let bufs: Vec<Vec<u8>> =
        (0..rng.below(4)).map(|_| (0..rng.below(65)).map(|_| rng.next() as u8).collect()).collect();
    let mut control = Vec::new();
    for _ in 0..rng.below(3) {
        control.extend(match rng.below(4) {
            0 if rng.below(100) == 0 => rights(&vec![rng.below(8) as i32; 250 + rng.below(8)]),
            0 | 1 => rights(&(0..1 + rng.below(3)).map(|_| rng.below(10) as i32).collect::<Vec<_>>()),
            2 => cmsg(if rng.below(4) == 0 { SOL_SOCKET } else { 41 }, 77, &[1; 5]),
            _ => cmsg(SOL_SOCKET, SCM_CREDENTIALS, &[0; 12]),
        });
    }
    let at = |k: usize| BUFS + 0x1000 * k as u64;
    let iov: Vec<(u64, u64)> = bufs.iter().enumerate().map(|(k, b)| (at(k), b.len() as u64)).collect();
    let name_at = if name.is_empty() { (0, 0) } else { (NAME, name.len() as u32) };
    let control_at =
        if control.is_empty() && rng.below(2) == 0 { (0, 0) } else { (CONTROL, control.len() as u64) };
    let mut m = Fake::default()
        .at(HEAD, msghdr(name_at, (IOV, iov.len() as u64), control_at))
        .at(NAME, name)
        .at(IOV, iovecs(&iov))
        .at(CONTROL, control);
    for (k, b) in bufs.into_iter().enumerate() {
        m = m.at(at(k), b);
    }
    m
}

/// What the kernel takes from a message.
#[derive(Debug, PartialEq)]
struct Read {
    name: Option<Vec<u8>>,
    data: Vec<u8>,
    control: Vec<u8>,
    /// Where in the control each descriptor passed sits.
    slots: Vec<usize>,
    /// How many descriptors each `SCM_RIGHTS` holds, in order.
    rights: Vec<usize>,
}

/// Why the kernel refuses a message: the errno, and the step that says it.
type Refused = (i32, &'static str);

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
fn kernel(m: &Fake, at: u64, datagram: bool) -> (Vec<(u64, usize)>, Result<Read, Refused>) {
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
/// tarae built: a header per `SCM_RIGHTS`, tarae's descriptor for each number the run passed there. Laid out
/// messages mutated 100,000 times, their pointers now and then sent to another piece or their own.
#[test]
fn a_message_is_read_as_the_kernel_reads_it() {
    let most = MSGHDR + ADDR_MAX + UIO_MAXIOV as usize * 16 + SEND_MAX + CONTROL_MAX;
    let touched = |r: &[(u64, usize)]| r.iter().copied().filter(|r| r.1 > 0).collect::<Vec<_>>();
    let mut rng = Rng(0x5eed_0add_7e55_0004);
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..100_000 {
        let mut m = laid_out(&mut rng);
        let near: Vec<u64> = m.pieces.iter().map(|p| p.0).collect();
        for _ in 0..1 + rng.below(2) {
            let k = rng.below(m.pieces.len());
            mutate(&mut rng, &mut m.pieces[k].1, &near);
        }
        let at = match rng.below(50) {
            0 => WORDS[rng.below(WORDS.len())],
            1 => near[rng.below(near.len())] + rng.below(24) as u64,
            _ => HEAD,
        };
        let datagram = rng.below(2) == 0;
        let (copies, said) = kernel(&m, at, datagram);
        let mut passed = Vec::new();
        let got = read_msg(&m, passer(&mut passed), at, datagram);
        let reads = m.reads.borrow().clone();
        assert!(reads.iter().map(|r| r.1).sum::<usize>() <= most, "{reads:?}");
        if let Some(why) = apart(&m, at, &got, &said) {
            *seen.entry(why).or_default() += 1;
            continue;
        }
        assert_eq!(touched(&reads), touched(&copies), "the memory read, in order");
        let what = match (got, said) {
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
                assert_eq!(
                    passed.iter().map(|p| p.0).collect::<Vec<_>>(),
                    theirs,
                    "each number the run passed"
                );
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
        };
        *seen.entry(what).or_default() += 1;
    }
    let steps = [
        "header",
        "name",
        "name length",
        "iovecs",
        "iovec",
        "iovec length",
        "datagram",
        "data",
        "control length",
        "control",
        "cmsg length",
        "a descriptor not held",
        "SCM_MAX_FD",
        "credentials",
        "another control message",
        "F3 name past 128",
        "F4 control at 0",
    ];
    let each = steps.iter().all(|s| seen.get(s).is_some_and(|&n| n > 20));
    assert!(each && seen["taken"] > 5000 && seen["taken with descriptors"] > 5000, "{seen:?}");
}

/// Where `read_msg` reads a message apart from the kernel, each as `apart` names it: (F3) a name past 128 bytes —
/// the kernel cuts it, tarae refuses it; (F4) a control length with no pointer — the kernel's copy faults, tarae
/// goes on without control; an iovec array not all there — tarae `EFAULT` where the kernel meets a bad length first.
#[test]
fn read_apart_from_the_kernel() {
    let both = |m: &Fake| {
        let got = read_msg(m, passer(&mut Vec::new()), HEAD, false)
            .map(|msg| (msg.name, msg.control.len()))
            .map_err(|u| u.errno());
        (got, kernel(m, HEAD, false).1.map(|r| (r.name, r.control.len())))
    };
    let long_name = Fake::default().at(HEAD, msghdr((NAME, 200), (IOV, 0), (0, 0))).at(NAME, [7; 200]);
    assert_eq!(both(&long_name), (Err(libc::EINVAL), Ok((Some(vec![7; 128]), 0))));
    let no_control = Fake::default().at(HEAD, msghdr((0, 0), (IOV, 0), (0, 16)));
    assert_eq!(both(&no_control), (Ok((None, 0)), Err((libc::EFAULT, "control"))));
    let cut = Fake::default().at(HEAD, msghdr((0, 0), (IOV, 2), (0, 0))).at(IOV, iovecs(&[(BUFS, 1 << 63)]));
    assert_eq!(both(&cut), (Err(libc::EFAULT), Err((libc::EINVAL, "iovec length"))));
}

/// F1: the `i`th message of a `sendmmsg` array is found by checked sums — one whose header (and its `msg_len`,
/// written back) would run past the end of the address space is none, the kernel's `EFAULT` — never a sum that
/// wraps (a release build) or panics (a test build).
#[test]
fn an_mmsghdr_past_the_address_space_is_none() {
    assert_eq!(mmsg_at(0x1000, 0), Some(0x1000));
    assert_eq!(mmsg_at(0x1000, 3), Some(0x10c0));
    assert_eq!(mmsg_at(u64::MAX - 64, 0), Some(u64::MAX - 64));
    assert_eq!(mmsg_at(u64::MAX - 63, 0), None, "its msg_len would end past the address space");
    assert_eq!(mmsg_at(u64::MAX - 64, 1), None);
    assert_eq!(mmsg_at(u64::MAX - 200, 1023), None);
    assert_eq!(mmsg_at(0, u64::MAX / 2), None);
}

/// F2: a unix socket is bound by the name checked, byte for byte — a name that isn't UTF-8 too (it was bound with
/// U+FFFD in its place: checked on one name, made on another).
#[test]
fn a_bind_names_the_bytes_it_checked() {
    let name = OsStr::from_bytes(b"s\xff.sock");
    let mut want = (AF_UNIX as u16).to_ne_bytes().to_vec();
    want.extend(b"/proc/self/fd/7/s\xff.sock\0");
    assert_eq!(named_in(7, name), Ok(want));
    assert_eq!(named_in(7, OsStr::new(&"n".repeat(100))), Err(libc::ENAMETOOLONG));
}
