//! Tests: descriptors handed over a unix socket (`SCM_RIGHTS`) — a keeper's sockets made outside a session, a
//! netlink socket a run can't make itself (the keeper's and the supervisor's tests).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// The descriptors one message on `s` carries (`SCM_RIGHTS`), at most four.
pub(super) fn received_fds(s: &UnixStream) -> Vec<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut control = [0u64; 8];
    let mut fds = Vec::new();
    // SAFETY: a zeroed msghdr pointed at the buffers above, live for the call; the control data read only
    // within what the kernel wrote, each descriptor taken once.
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &raw mut iov;
        m.msg_iovlen = 1;
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = std::mem::size_of_val(&control) as _;
        if libc::recvmsg(s.as_raw_fd(), &mut m, libc::MSG_CMSG_CLOEXEC) < 0 {
            return fds;
        }
        let c = libc::CMSG_FIRSTHDR(&m);
        if !c.is_null() && (*c).cmsg_type == libc::SCM_RIGHTS {
            let data = libc::CMSG_DATA(c) as usize - c as usize;
            let count = ((*c).cmsg_len as usize).saturating_sub(data) / std::mem::size_of::<RawFd>();
            for i in 0..count.min(4) {
                fds.push(OwnedFd::from_raw_fd(libc::CMSG_DATA(c).cast::<RawFd>().add(i).read_unaligned()));
            }
        }
    }
    fds
}

/// `fds` sent on `s` with one byte, as one message (`SCM_RIGHTS`), at most four.
pub(super) fn send_fds(s: &UnixStream, fds: &[BorrowedFd]) {
    let sent = try_send_fds(s, fds);
    assert!(sent.is_ok(), "sent: {sent:?}");
}

/// `send_fds`, its failure said.
pub(super) fn try_send_fds(s: &UnixStream, fds: &[BorrowedFd]) -> io::Result<()> {
    let raw: Vec<RawFd> = fds.iter().take(4).map(|f| f.as_raw_fd()).collect();
    let byte = [1u8];
    let mut iov = libc::iovec { iov_base: byte.as_ptr().cast_mut().cast(), iov_len: 1 };
    let mut control = [0u64; 8];
    // SAFETY: a zeroed msghdr pointed at the buffers above, live for the call; the header written within the
    // control buffer (`CMSG_SPACE` of four descriptors, under its 64 bytes).
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &raw mut iov;
        m.msg_iovlen = 1;
        let len = std::mem::size_of_val(raw.as_slice()) as u32;
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = libc::CMSG_SPACE(len) as _;
        let c = libc::CMSG_FIRSTHDR(&m);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(len) as _;
        std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), raw.len());
        match libc::sendmsg(s.as_raw_fd(), &m, 0) {
            1 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
