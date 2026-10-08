//! A netlink socket tarae was handed: sends to the kernel's own address, hears only the kernel, waits by a deadline.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Instant;

use crate::answer::{Message, Owed, Reading};
use crate::{Batch, DUMP, Error, HEADER, Malformed, Protocol};

/// `SOL_NETLINK`'s options: the request echoed as its header only in an error, and the extended ack's attributes.
const SOL_NETLINK: i32 = 270;
const NETLINK_CAP_ACK: i32 = 10;
const NETLINK_EXT_ACK: i32 = 11;
/// `SIOCGSKNS` (`linux/sockios.h`): a descriptor of the network namespace a socket is in.
const SIOCGSKNS: libc::c_ulong = 0x894C;

/// A netlink socket of `protocol`, handed over.
pub struct Socket {
    fd: OwnedFd,
    protocol: Protocol,
    /// The next sequence number (from 1: the kernel's own notifications carry 0).
    seq: u32,
    buf: Vec<u8>,
    /// Datagrams dropped because they weren't the kernel's.
    foreign: u64,
}

fn getsockopt(fd: &OwnedFd, level: i32, name: i32) -> io::Result<i32> {
    let mut v: i32 = 0;
    let mut len = size_of::<i32>() as libc::socklen_t;
    // SAFETY: an int and its length, ours for the call.
    let r = unsafe { libc::getsockopt(fd.as_raw_fd(), level, name, (&raw mut v).cast(), &mut len) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(v)
}

fn setsockopt(fd: &OwnedFd, level: i32, name: i32, v: i32) -> io::Result<()> {
    // SAFETY: an int and its length, ours for the call.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw const v).cast(),
            size_of::<i32>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The kernel's own address.
fn kernel() -> libc::sockaddr_nl {
    // SAFETY: all zeros is a valid `sockaddr_nl` (port 0 — the kernel's — no groups).
    let mut a: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    a.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    a
}

impl Socket {
    /// `fd` as a netlink socket of `protocol` — refused unless it is one. The kernel is asked to echo only a
    /// refused request's header and to say why it refused, where it can (kernels without the extended ack leave
    /// that out: their refusals still come).
    pub fn from_fd(fd: OwnedFd, protocol: Protocol) -> io::Result<Socket> {
        let domain = getsockopt(&fd, libc::SOL_SOCKET, libc::SO_DOMAIN)?;
        let proto = getsockopt(&fd, libc::SOL_SOCKET, libc::SO_PROTOCOL)?;
        if domain != libc::AF_NETLINK || proto != protocol.number() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "not a netlink socket of protocol {} (family {domain}, protocol {proto})",
                    protocol.number()
                ),
            ));
        }
        let _ = setsockopt(&fd, SOL_NETLINK, NETLINK_CAP_ACK, 1);
        let _ = setsockopt(&fd, SOL_NETLINK, NETLINK_EXT_ACK, 1);
        Ok(Socket { fd, protocol, seq: 1, buf: vec![0; 64 * 1024], foreign: 0 })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The network namespace the socket is in, as (device, inode) — what tells one namespace from another. Only a
    /// caller with `CAP_NET_ADMIN` over that namespace is told: anyone else gets `EPERM`.
    pub fn netns(&self) -> io::Result<(u64, u64)> {
        // SAFETY: an ioctl on our own descriptor; the descriptor it makes is ours.
        let ns = unsafe { libc::ioctl(self.fd.as_raw_fd(), SIOCGSKNS as _) };
        if ns < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just made, ours.
        let ns = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(ns) });
        let m = ns.metadata()?;
        use std::os::unix::fs::MetadataExt;
        Ok((m.dev(), m.ino()))
    }

    /// Datagrams dropped so far because they weren't the kernel's.
    pub fn foreign(&self) -> u64 {
        self.foreign
    }

    /// Sends `batch` and waits, by `deadline`, for every message's ack — or the first refusal, which is the error
    /// (with the message's index, and the attribute and words the kernel named). A refused route change is partial:
    /// see `route::Changes`.
    pub fn apply(&mut self, batch: &Batch, deadline: Instant) -> Result<(), Error> {
        let first = self.send(batch)?;
        let mut owed = Owed::new(batch, first);
        while !owed.answered() {
            // (a refused batch: what the kernel says of it was queued with the send — once read, nothing more comes;
            // a sender without CAP_NET_ADMIN gets one EPERM for `BATCH_BEGIN` and no answer for the rest)
            if owed.refused() && !self.ready_now()? {
                break;
            }
            let n = self.receive(deadline)?;
            owed.take(&self.buf[..n])?;
        }
        owed.outcome()
    }

    /// Sends one request — a dump, or a single request asking its ack — and reads its answer by `deadline`.
    pub(crate) fn read(&mut self, request: &Batch, deadline: Instant) -> Result<Vec<Message>, Error> {
        let dump = request.bytes().get(6..8).is_some_and(|f| u16::from_ne_bytes([f[0], f[1]]) & DUMP == DUMP);
        let seq = self.send(request)?;
        let mut reading = Reading::new(seq, dump);
        while !reading.ended() {
            let n = self.receive(deadline)?;
            reading.take(&self.buf[..n])?;
        }
        reading.outcome()
    }

    /// `batch` numbered from the socket's count and sent whole, to the kernel's address: its first number.
    fn send(&mut self, batch: &Batch) -> Result<u32, Error> {
        if batch.protocol() != self.protocol {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a batch of another protocol",
            )));
        }
        let first = self.seq;
        self.seq = first.wrapping_add(batch.len() as u32);
        let bytes = batch.numbered(first);
        let to = kernel();
        // SAFETY: our socket, a buffer and an address of ours, live for the call.
        let n = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
                (&raw const to).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if n < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        if n as usize != bytes.len() {
            return Err(Error::Io(io::Error::other("netlink took part of a batch")));
        }
        Ok(first)
    }

    /// Whether a datagram waits now.
    fn ready_now(&self) -> Result<bool, Error> {
        let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd of ours.
        let r = unsafe { libc::poll(&mut p, 1, 0) };
        if r < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(r > 0)
    }

    /// The next datagram from the kernel, by `deadline`, into `buf`: its length. Its size is asked first (`MSG_PEEK
    /// | MSG_TRUNC`) so it's read whole; one from any port but the kernel's is dropped and counted.
    fn receive(&mut self, deadline: Instant) -> Result<usize, Error> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
            // SAFETY: one pollfd of ours.
            match unsafe { libc::poll(&mut p, 1, ms) } {
                0 => continue,
                r if r < 0 => {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(Error::Io(e));
                }
                _ => {}
            }
            let size = match self.recv(None)? {
                Some((size, _)) => size,
                None => continue,
            };
            if size > self.buf.len() {
                self.buf.resize(size, 0);
            }
            let Some((n, from)) = self.recv(Some(size))? else { continue };
            if from != 0 {
                self.foreign += 1;
                continue;
            }
            if n < HEADER {
                return Err(Malformed("a datagram shorter than a header").into());
            }
            return Ok(n);
        }
    }

    /// One `recvmsg`: with `into` None, the next datagram's size, peeked; else that many bytes into `buf`. The
    /// length and the sender's port — None when nothing waits after all.
    fn recv(&mut self, into: Option<usize>) -> Result<Option<(usize, u32)>, Error> {
        // SAFETY: all zeros is a valid `sockaddr_nl` to be filled.
        let mut from: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec { iov_base: self.buf.as_mut_ptr().cast(), iov_len: into.unwrap_or(0) };
        // SAFETY: all zeros is a valid `msghdr`; its pointers are set below to buffers that outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&raw mut from).cast();
        msg.msg_namelen = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        let flags = libc::MSG_DONTWAIT | if into.is_none() { libc::MSG_PEEK | libc::MSG_TRUNC } else { 0 };
        // SAFETY: our socket and a message header whose buffers are ours for the call.
        let n = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut msg, flags) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
                return Ok(None);
            }
            return Err(Error::Io(e));
        }
        if into.is_some() && msg.msg_flags & libc::MSG_TRUNC != 0 {
            return Err(Malformed("a datagram longer than its peeked size").into());
        }
        Ok(Some((n as usize, from.nl_pid)))
    }
}
