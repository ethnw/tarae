use std::collections::BTreeMap;

use super::check::{self, Fake, cmsg, kernel, passer, rights};
use super::*;

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

fn sockaddr(family: i32, rest: &[u8]) -> Vec<u8> {
    let mut a = (family as u16).to_ne_bytes().to_vec();
    a.extend(rest);
    a
}

fn v4(ip: [u8; 4], port: u16) -> Vec<u8> {
    sockaddr(AF_INET, &[&port.to_be_bytes()[..], &ip, &[0; 8]].concat())
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

/// The addresses a run names: each family's, a unix path with junk after its NUL, a relative one, an abstract name,
/// no name.
fn addresses() -> Vec<Vec<u8>> {
    let mut v6 = sockaddr(AF_INET6, &9u16.to_be_bytes());
    v6.extend([0; 4].iter().chain(&std::net::Ipv6Addr::LOCALHOST.octets()).chain(&[0; 4]));
    vec![
        v4([127, 0, 0, 1], 8080),
        v6,
        sockaddr(AF_UNIX, b"/p/s.sock\0junk"),
        sockaddr(AF_UNIX, b"rel.sock"),
        sockaddr(AF_UNIX, b"\0bus"),
        sockaddr(AF_UNIX, b""),
        sockaddr(AF_NETLINK, &[0; 10]),
        sockaddr(AF_UNSPEC, &[0; 14]),
    ]
}

/// Whatever an address holds, `parse` reads it as the kernel does (`check::address`). Seeded addresses mutated
/// 100,000 times; each way of reading one seen.
#[test]
fn an_address_is_read_as_the_kernel_reads_it() {
    let seeds = addresses();
    let mut rng = Rng(0x5eed_0add_7e55_0002);
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..100_000 {
        let mut b = seeds[rng.below(seeds.len())].clone();
        mutate(&mut rng, &mut b, &[]);
        *seen.entry(check::address(&b)).or_default() += 1;
    }
    assert!(seen.len() == 9 && seen.values().all(|&n| n > 500), "{seen:?}");
}

/// A batch `netlink_reads` takes, the kernel reads as gets only (`check::netlink`), and a batch whole of gets with
/// a read's flags is taken. Batches of one to three gets, changes and control messages, half of them mutated,
/// 100,000 times.
#[test]
fn a_batch_taken_is_only_reads_as_the_kernel_reads_it() {
    let mut rng = Rng(0x5eed_0add_7e55_0003);
    let (mut taken, mut refused, mut mutated_taken) = (0, 0, 0);
    for _ in 0..100_000 {
        let (mut b, reads) = batch(&mut rng);
        let mutated = rng.below(2) == 0;
        if mutated {
            mutate(&mut rng, &mut b, &[]);
        }
        match check::netlink(&b) {
            Ok(()) => {
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

/// A batch of one to three rtnetlink messages — gets, changes, control messages, a read's flags and others — and
/// whether it's all reads.
fn batch(rng: &mut Rng) -> (Vec<u8>, bool) {
    const FLAGS: [i32; 7] = [
        NLM_F_REQUEST | NLM_F_DUMP,
        NLM_F_REQUEST | NLM_F_ACK,
        NLM_F_REQUEST,
        NLM_F_DUMP,
        NLM_F_REQUEST | 0x400, // (NLM_F_CREATE)
        NLM_F_REQUEST | NLM_F_DUMP | 0x800,
        0x10,
    ];
    let (mut b, mut reads) = (Vec::new(), true);
    for _ in 0..1 + rng.below(3) {
        let kind = match rng.below(4) {
            0 => 18 + 4 * rng.below(28) as u16,
            1 => rng.below(16) as u16,
            _ => 16 + rng.below(112) as u16,
        };
        let flags = FLAGS[rng.below(FLAGS.len())];
        reads &= kind >= 16 && (kind - 16) % 4 == 2 && flags & !(NLM_F_REQUEST | NLM_F_DUMP | NLM_F_ACK) == 0;
        let payload = rng.below(25);
        let at = b.len();
        b.extend(((16 + payload) as u32).to_ne_bytes());
        b.extend(kind.to_ne_bytes());
        b.extend((flags as u16).to_ne_bytes());
        b.extend((0..8 + payload).map(|_| rng.next() as u8));
        b.resize(at + (16 + payload).next_multiple_of(4), 0);
    }
    (b, reads)
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

/// Whatever a message holds and wherever its pointers land, `read_msg` never panics and reads it as the kernel does
/// but where it's known to differ (`check::message`). Laid out messages mutated 100,000 times, their pointers now
/// and then sent to another piece or their own; each of the kernel's refusals and each difference seen.
#[test]
fn a_message_is_read_as_the_kernel_reads_it() {
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
        *seen.entry(check::message(&m, at, datagram)).or_default() += 1;
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
    ];
    let each = steps.iter().all(|s| seen.get(s).is_some_and(|&n| n > 20));
    assert!(each && seen["taken"] > 5000 && seen["taken with descriptors"] > 5000, "{seen:?}");
}

/// Where `read_msg` reads a message apart from the kernel, each as `apart` names it: (F3) a name past 128 bytes —
/// the kernel cuts it, tarae refuses it; an iovec array not all there — tarae `EFAULT` where the kernel meets a bad
/// length first. (F4, a control length with a null pointer, was one until V2-4: `read_msg` reads it as the kernel does.)
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
    let cut = Fake::default().at(HEAD, msghdr((0, 0), (IOV, 2), (0, 0))).at(IOV, iovecs(&[(BUFS, 1 << 63)]));
    assert_eq!(both(&cut), (Err(libc::EFAULT), Err((libc::EINVAL, "iovec length"))));
}

/// A message's memory reads back from the fuzz input made of it (`Fake::input`, `Fake::from_input`): where its
/// header is, whether it's a datagram, every piece at its address; an input too short for its header's place is
/// none, a piece cut short is what's left of it.
#[test]
fn a_messages_fuzz_input_reads_back_as_it_was_laid_out() {
    let mut rng = Rng(0x5eed_0add_7e55_0005);
    for k in 0..100 {
        let m = laid_out(&mut rng);
        let (at, datagram, back) = Fake::from_input(&m.input(HEAD + k, k % 2 == 0)).unwrap();
        assert_eq!((at, datagram, back.pieces), (HEAD + k, k % 2 == 0, m.pieces));
    }
    assert!(Fake::from_input(&[1; 8]).is_none());
    let cut = [&[0][..], &HEAD.to_le_bytes(), &NAME.to_le_bytes(), &[9, 0], b"abc"].concat();
    assert_eq!(Fake::from_input(&cut).unwrap().2.pieces, [(NAME, b"abc".to_vec())]);
}

/// The fuzz targets' seeds (`fuzz/parsers/seeds/`): `sockaddr` the addresses above, `netlink` batches of gets and
/// changes, `msghdr` messages laid out as a run lays them out.
#[test]
#[ignore = "writes fuzz/parsers/seeds: by hand"]
fn writes_fuzz_seeds() {
    use crate::mutate::fuzz_seeds;
    fuzz_seeds("sockaddr", addresses().into_iter().enumerate().map(|(k, a)| (format!("address-{k}"), a)));
    let mut rng = Rng(0x5eed_0add_7e55_0006);
    let batches = (0..8).map(|k| {
        let (b, reads) = batch(&mut rng);
        (format!("{}-{k}", if reads { "reads" } else { "changes" }), b)
    });
    fuzz_seeds("netlink", batches);
    let messages = (0..8).map(|k| (format!("message-{k}"), laid_out(&mut rng).input(HEAD, k % 2 == 0)));
    fuzz_seeds("msghdr", messages);
}

/// V2-4 (fuzz `msghdr`, 2026-10-16): a control length with a null pointer is read where the pointer points, as the
/// kernel reads it (`____sys_sendmsg` copies `ctl_len` bytes from `msg_control` whatever it is): with memory at
/// address 0, its descriptors passed; with none there (every run's case — `vm.mmap_min_addr`), `EFAULT`. tarae read
/// no control at a null pointer and sent the message without it. The fuzz input, minimized: a stream's header at
/// `HEAD` with no name, no iovecs and 24 bytes of control at 0 — and, at 0, `SCM_RIGHTS` passing descriptor 3.
#[test]
fn a_control_at_address_0_is_read_as_the_kernel_reads_it() {
    let header = msghdr((0, 0), (IOV, 0), (0, 24));
    let at_0 = rights(&[3]);
    let input = [&[0][..], &HEAD.to_le_bytes(), &HEAD.to_le_bytes(), &[56, 0], &header].concat();
    let mapped = [&input[..], &0u64.to_le_bytes(), &[24, 0], &at_0].concat();
    let (at, datagram, m) = Fake::from_input(&mapped).unwrap();
    assert_eq!(check::message(&m, at, datagram), "taken with descriptors");
    let (at, datagram, m) = Fake::from_input(&input).unwrap();
    assert_eq!(check::message(&m, at, datagram), "control", "nothing at 0: the kernel's EFAULT");
}

/// V2-5 (fuzz `msghdr`, 2026-10-16): the stand-in memory is one — an address holds one byte, whatever the size of
/// the read that reaches it (where pieces are laid over each other, the first laid), and a read across pieces that
/// meet is whole, as the kernel's copies and `/proc/<tid>/mem` read across mappings. It held the first piece that
/// held a read whole: the model's 16-byte iovec reads saw one piece, `read_msg`'s one read of the array another.
/// The fuzz input, minimized: a stream's header at `HEAD` naming three iovecs at `IOV`; at `IOV` first one iovec
/// (`BUFS`, 2), then three (`BUFS`, 4); four bytes at `BUFS`.
#[test]
fn a_fake_memory_holds_one_byte_an_address() {
    let piece = |at: u64, b: &[u8]| [&at.to_le_bytes()[..], &(b.len() as u16).to_le_bytes(), b].concat();
    let input = [
        &[0][..],
        &HEAD.to_le_bytes(),
        &piece(HEAD, &msghdr((0, 0), (IOV, 3), (0, 0))),
        &piece(IOV, &iovecs(&[(BUFS, 2)])),
        &piece(IOV, &iovecs(&[(BUFS, 4); 3])),
        &piece(BUFS, b"abcd"),
    ]
    .concat();
    let (at, datagram, m) = Fake::from_input(&input).unwrap();
    assert_eq!(check::message(&m, at, datagram), "taken");
    assert_eq!(m.peek(IOV, 48).unwrap()[..16], m.peek(IOV, 16).unwrap(), "the first piece laid there");
    let msg = read_msg(&m, passer(&mut Vec::new()), at, datagram).ok().unwrap();
    assert_eq!(msg.data, b"ababcdabcd");
    let meeting = Fake::default().at(BUFS, *b"ab").at(BUFS + 2, *b"cd").at(u64::MAX - 1, *b"yz");
    assert_eq!(meeting.peek(BUFS, 4), Ok(b"abcd".to_vec()), "across pieces that meet");
    assert_eq!(meeting.peek(BUFS + 1, 4), Err(libc::EFAULT), "past them");
    assert_eq!(meeting.peek(u64::MAX - 1, 2), Ok(b"yz".to_vec()));
    assert_eq!(meeting.peek(u64::MAX, 2), Err(libc::EFAULT), "past the address space's end");
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
