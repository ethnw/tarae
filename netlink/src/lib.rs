//! tarae's netlink: what the transparent proxy (step 6, `target/sb/step6-plan.md`) sends the kernel for a session's
//! network — one nf_tables transaction (an owned table, its chains, its rules), the routes and the policy rule that
//! bring marked packets home — and the few reads that say they're there (the ruleset's generation, its tables and
//! rules, the routes and policy rules).
//!
//! Sync, `libc` only, and small on purpose: no sets, no NAT, no conntrack, no socket diagnostics, no groups. It never
//! opens a socket. It's given one — the session keeper's, opened before any run — keeps its descriptor in sight
//! (`Socket::as_fd`), and always sends to the kernel's own address with an explicit `sockaddr_nl`. The kernel then
//! judges the sender's capability, never the opener's (`netlink_sendmsg` sets `NETLINK_SKB_DST`;
//! step6-checks-notes §1). It hears only the kernel: a datagram from any other port is dropped.
//!
//! Building messages and reading replies is plain byte work and compiles everywhere (its tests run on macOS too,
//! against the bytes nft and ip sent on Linux — `fixtures/`); the socket is Linux's.
// (elsewhere the reading has no socket to read from: only the tests call it)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod answer;
mod attr;
pub mod nft;
mod reply;
pub mod route;
#[cfg(target_os = "linux")]
mod socket;

#[cfg(test)]
mod fixture;
#[cfg(all(test, target_os = "linux"))]
mod live;
#[cfg(test)]
mod tests;

use std::fmt;

#[cfg(target_os = "linux")]
pub use socket::Socket;

/// A netlink family tarae speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `NETLINK_ROUTE`: routes and policy rules.
    Route,
    /// `NETLINK_NETFILTER`: nf_tables.
    Netfilter,
}

impl Protocol {
    /// As `socket(2)` and `SO_PROTOCOL` number it.
    pub fn number(self) -> i32 {
        match self {
            Protocol::Route => 0,
            Protocol::Netfilter => 12,
        }
    }
}

/// An address family, as both rtnetlink (`AF_INET`, `AF_INET6`) and nf_tables (`NFPROTO_IPV4`, `NFPROTO_IPV6`) number
/// it — the same numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn number(self) -> u8 {
        match self {
            Family::V4 => 2,
            Family::V6 => 10,
        }
    }
}

/// Why something the kernel sent isn't a reply tarae can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a malformed netlink reply: {}", self.0)
    }
}

/// Why a change or a read didn't happen.
#[derive(Debug)]
pub enum Error {
    /// The kernel refused message `index` of what was sent (found by its sequence number). `attribute`: the type of
    /// the attribute it named (`NLMSGERR_ATTR_OFFS` — an offset into that message), `said`: its words
    /// (`NLMSGERR_ATTR_MSG`), each where it gave one.
    Kernel {
        errno: i32,
        index: Option<usize>,
        attribute: Option<u16>,
        said: Option<String>,
    },
    Io(std::io::Error),
    Malformed(Malformed),
    /// No whole answer by the deadline.
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Kernel { errno, index, attribute, said } => {
                write!(f, "the kernel refused it: {}", std::io::Error::from_raw_os_error(*errno))?;
                if let Some(i) = index {
                    write!(f, " (message {i}")?;
                    if let Some(a) = attribute {
                        write!(f, ", attribute {a}")?;
                    }
                    write!(f, ")")?;
                }
                if let Some(s) = said {
                    write!(f, " — {s}")?;
                }
                Ok(())
            }
            Error::Io(e) => write!(f, "netlink: {e}"),
            Error::Malformed(m) => m.fmt(f),
            Error::Timeout => write!(f, "netlink: no answer in time"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<Malformed> for Error {
    fn from(m: Malformed) -> Error {
        Error::Malformed(m)
    }
}

/// `nlmsghdr`'s length.
const HEADER: usize = 16;

/// Header flags (`linux/netlink.h`).
pub(crate) const REQUEST: u16 = 0x1;
pub(crate) const ACK: u16 = 0x4;
pub(crate) const DUMP: u16 = 0x300;
pub(crate) const EXCL: u16 = 0x200;
pub(crate) const CREATE: u16 = 0x400;
pub(crate) const APPEND: u16 = 0x800;

/// Messages for one send, in order: their bytes as they go out (sequence numbers from 0 — `Socket` renumbers them
/// from its own count as it sends), and for each where it starts, how long its fixed header is (what comes before
/// its attributes) and whether it asks the kernel's answer (an ack, or a dump's end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    protocol: Protocol,
    bytes: Vec<u8>,
    messages: Vec<Placed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
    start: usize,
    fixed: usize,
    answered: bool,
}

impl Batch {
    pub(crate) fn new(protocol: Protocol) -> Batch {
        Batch { protocol, bytes: Vec::new(), messages: Vec::new() }
    }

    /// A message of `kind` with `flags`, its fixed header `fixed`, its attributes written by `attrs`; its sequence
    /// number its index, its length set once they're written. It asks the kernel's answer if `flags` ask an ack or
    /// a dump.
    pub(crate) fn message(&mut self, kind: u16, flags: u16, fixed: &[u8], attrs: impl FnOnce(&mut Vec<u8>)) {
        let start = self.bytes.len();
        let seq = self.messages.len() as u32;
        let b = &mut self.bytes;
        b.extend(0u32.to_ne_bytes());
        b.extend(kind.to_ne_bytes());
        b.extend(flags.to_ne_bytes());
        b.extend(seq.to_ne_bytes());
        b.extend(0u32.to_ne_bytes());
        b.extend(fixed);
        attr::pad(b);
        attrs(b);
        // (can't wrap: every attribute is at most 65535 bytes — `attr::put`, `nest` — and a message holds a handful)
        let len = (b.len() - start) as u32;
        b[start..start + 4].copy_from_slice(&len.to_ne_bytes());
        let answered = flags & ACK != 0 || flags & DUMP == DUMP;
        self.messages.push(Placed { start, fixed: fixed.len(), answered });
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The bytes as they'd go out with sequence numbers from 0.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// How many messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The bytes with message `i`'s sequence number `first + i`.
    pub(crate) fn numbered(&self, first: u32) -> Vec<u8> {
        let mut b = self.bytes.clone();
        for (i, m) in self.messages.iter().enumerate() {
            b[m.start + 8..m.start + 12].copy_from_slice(&first.wrapping_add(i as u32).to_ne_bytes());
        }
        b
    }

    /// The indices of the messages that ask the kernel's answer.
    pub(crate) fn answered(&self) -> impl Iterator<Item = usize> + '_ {
        self.messages.iter().enumerate().filter(|(_, m)| m.answered).map(|(i, _)| i)
    }

    /// Message `i`'s bytes.
    fn message_bytes(&self, i: usize) -> Option<&[u8]> {
        let m = self.messages.get(i)?;
        let end = self.messages.get(i + 1).map_or(self.bytes.len(), |n| n.start);
        self.bytes.get(m.start..end)
    }

    /// The type of the attribute that starts `offset` bytes into message `i` (what `NLMSGERR_ATTR_OFFS` names), at
    /// any depth of nesting.
    pub(crate) fn attribute_at(&self, i: usize, offset: u32) -> Option<u16> {
        let m = self.messages.get(i)?;
        let msg = self.message_bytes(i)?;
        let first = attr::align(HEADER + m.fixed);
        attr::find(msg, first, offset as usize)
    }
}
