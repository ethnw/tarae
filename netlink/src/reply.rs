//! What the kernel sends back, read from one datagram: acks and refusals (`NLMSG_ERROR`, with the extended ack's
//! offset and words where it gave them), a dump's end (`NLMSG_DONE`), and data messages. Every length is checked
//! against what's there; a datagram that doesn't hold whole messages is malformed, never read past.

use crate::attr;
use crate::{HEADER, Malformed};

const NOOP: u16 = 1;
const ERROR: u16 = 2;
const DONE: u16 = 3;
const OVERRUN: u16 = 4;
/// The first type that's a family's own, not netlink's.
const MIN_TYPE: u16 = 0x10;

/// An error message's flags: the request echoed only as its header (`NETLINK_CAP_ACK`), and attributes after it.
const CAPPED: u16 = 0x100;
const ACK_TLVS: u16 = 0x200;
/// `NLMSGERR_ATTR_*`.
const ATTR_MSG: u16 = 1;
const ATTR_OFFS: u16 = 2;

/// One message of a reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reply<'a> {
    /// Message `seq` landed.
    Ack {
        seq: u32,
    },
    /// Message `seq` was refused (`errno` positive), naming the attribute at `offset` into it and saying `said`
    /// where the kernel did.
    Refused {
        seq: u32,
        errno: i32,
        offset: Option<u32>,
        said: Option<String>,
    },
    /// The end of dump `seq` (`errno`: 0, or why the dump stopped).
    Done {
        seq: u32,
        errno: i32,
    },
    Data {
        seq: u32,
        kind: u16,
        body: &'a [u8],
    },
}

/// The messages in datagram `b`.
pub(crate) fn replies(b: &[u8]) -> Result<Vec<Reply<'_>>, Malformed> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let left = &b[at..];
        if left.len() < HEADER {
            return Err(Malformed("a message shorter than its header"));
        }
        let len = u32::from_ne_bytes(left[0..4].try_into().unwrap_or_default()) as usize;
        let kind = u16::from_ne_bytes(left[4..6].try_into().unwrap_or_default());
        let flags = u16::from_ne_bytes(left[6..8].try_into().unwrap_or_default());
        let seq = u32::from_ne_bytes(left[8..12].try_into().unwrap_or_default());
        if len < HEADER || len > left.len() {
            return Err(Malformed("a message's length past its datagram"));
        }
        let body = &left[HEADER..len];
        match kind {
            NOOP => {}
            ERROR => out.push(error(seq, flags, body)?),
            DONE => {
                // (a dump that stopped says why in the int after the header — 0 when it ended whole)
                let errno = match body.get(0..4) {
                    Some(e) => i32::from_ne_bytes(e.try_into().unwrap_or_default()).saturating_neg(),
                    None => 0,
                };
                out.push(Reply::Done { seq, errno });
            }
            OVERRUN => return Err(Malformed("the kernel's overrun: replies were lost")),
            k if k < MIN_TYPE => return Err(Malformed("a control message tarae doesn't take")),
            _ => out.push(Reply::Data { seq, kind, body }),
        }
        at += attr::align(len).min(left.len());
    }
    Ok(out)
}

/// `NLMSG_ERROR`'s body: the error (negative, 0 for an ack), the request echoed (its header only when capped, else
/// whole), then — flagged — the extended ack's attributes.
fn error(seq: u32, flags: u16, body: &[u8]) -> Result<Reply<'_>, Malformed> {
    let raw = i32::from_ne_bytes(
        body.get(0..4).ok_or(Malformed("an error without its number"))?.try_into().unwrap_or_default(),
    );
    if raw > 0 {
        return Err(Malformed("an error number that isn't negative"));
    }
    let echoed = body.get(4..4 + HEADER).ok_or(Malformed("an error without the request's header"))?;
    let echoed_len = match flags & CAPPED {
        0 => u32::from_ne_bytes(echoed[0..4].try_into().unwrap_or_default()) as usize,
        _ => HEADER,
    };
    if echoed_len < HEADER || 4 + echoed_len > body.len() {
        return Err(Malformed("an error's echoed request past its message"));
    }
    if raw == 0 {
        return Ok(Reply::Ack { seq });
    }
    let (mut offset, mut said) = (None, None);
    if flags & ACK_TLVS != 0 {
        for a in attr::attrs(body, attr::align(4 + echoed_len)) {
            let a = a?;
            match a.kind {
                ATTR_MSG => said = Some(a.string()),
                ATTR_OFFS => offset = Some(a.u32_ne()?),
                _ => {}
            }
        }
    }
    Ok(Reply::Refused { seq, errno: raw.saturating_neg(), offset, said })
}
