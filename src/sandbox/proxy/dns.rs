//! The DNS messages tarae reads and writes where it answers a session's names itself (step 6e,
//! `target/sb/step6e-design.md`): a run's question, and tarae's answer to it. Pure: bytes in, an answer out, no I/O;
//! the session's answering reads, decides and sends. Bounded — nothing read past its message, no length trusted
//! before what it claims is there — and every input answered with a question, a refusal that can be answered
//! (the header was read), or one that can't (dropped): never a panic (a panic aborts tarae).
//!
//! Only what a stub resolver sends is read (RFC 1035 §4, RFC 6891's OPT): one question, no name compression in it,
//! at most one additional record. The answer echoes the question byte for byte — a name's case as asked (0x20
//! randomization: a client may check it) — and carries an OPT when the question did.
//!
//! 6e+: tarae's own question for a name's SRV or TXT records (`query`), and the response to it read back
//! (`response`) — names compressed there (RFC 1035 §4.1.4), pointers only backwards, every length checked against
//! what's there — so the run's question is answered with those records (`Answer::Records`).

use std::net::IpAddr;

/// The longest question read: a stub's is well under 512; EDNS padding (RFC 7830) can take it towards the 468 a
/// client pads to, and over TCP a length prefix can claim up to 65,535 — past this it isn't a question we answer.
pub const QUESTION_MAX: usize = 4096;
/// A name's wire length at most (RFC 1035 §3.1).
const NAME_WIRE_MAX: usize = 255;
/// The UDP payload size tarae's OPT says (DNS Flag Day 2020's).
const UDP_PAYLOAD: u16 = 1232;
/// A datagram's answer at most where the question carried no OPT (RFC 1035 §4.2.1).
const UDP_PLAIN: u16 = 512;
/// Pointers followed in one name of a response at most — each goes back, so a loop can't be made; this bounds the
/// work.
const NAME_JUMPS: usize = 32;
/// Records of the type asked kept from one response at most.
pub const RECORDS_MAX: usize = 64;

pub const TYPE_A: u16 = 1;
pub const TYPE_CNAME: u16 = 5;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_SRV: u16 = 33;
pub const TYPE_SVCB: u16 = 64;
pub const TYPE_HTTPS: u16 = 65;
const TYPE_OPT: u16 = 41;
pub const CLASS_IN: u16 = 1;

/// A run's question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: u16,
    /// The header's opcode and RD bits as asked (echoed in the answer).
    flags: u8,
    /// The name as text: lowercase, no trailing dot, `""` for the root. A byte that isn't printable ASCII, or is a
    /// `.` or `\` inside a label, is written `\DDD` (RFC 1035 §5.1) — so no such name passes as a host name
    /// (`parse::name`).
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// The question section as sent (name, type, class): echoed in the answer.
    raw: Vec<u8>,
    /// It carried an OPT record (EDNS): the answer carries one.
    pub edns: bool,
    /// The largest datagram its client takes: its OPT's size (512 at least), else 512.
    pub udp: u16,
}

/// What a message that isn't a question we take gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Nothing: not a query (a response, a message under a header) — answering it could loop.
    Dropped,
    /// An answer of the rcode alone, its header's ID and flags echoed (FORMERR, NOTIMP).
    Answered { id: u16, flags: u8, rcode: u8 },
}

pub const NOERROR: u8 = 0;
pub const FORMERR: u8 = 1;
pub const SERVFAIL: u8 = 2;
pub const NXDOMAIN: u8 = 3;
pub const NOTIMP: u8 = 4;
pub const REFUSED: u8 = 5;

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// The question in `msg`, one whole DNS message (a datagram, or a TCP message without its length).
pub fn question(msg: &[u8]) -> Result<Question, Refused> {
    if msg.len() < 12 || msg.len() > QUESTION_MAX {
        return Err(Refused::Dropped);
    }
    let id = u16::from_be_bytes([msg[0], msg[1]]);
    // (QR set: a response, never answered)
    if msg[2] & 0x80 != 0 {
        return Err(Refused::Dropped);
    }
    // (the opcode and RD: what the answer echoes)
    let flags = msg[2] & 0x79;
    let refuse = |rcode| Err(Refused::Answered { id, flags, rcode });
    if (msg[2] >> 3) & 0x0f != 0 {
        return refuse(NOTIMP);
    }
    let count = |at| be16(msg, at).unwrap_or(0);
    let (qd, an, ns, ar) = (count(4), count(6), count(8), count(10));
    if qd != 1 || an != 0 || ns != 0 || ar > 1 {
        return refuse(FORMERR);
    }
    let Some((name, end)) = read_name(msg, 12, false) else { return refuse(FORMERR) };
    let (Some(qtype), Some(qclass)) = (be16(msg, end), be16(msg, end + 2)) else { return refuse(FORMERR) };
    let after = end + 4;
    let (edns, udp) = match ar {
        0 => (false, UDP_PLAIN),
        _ => match opt(msg, after) {
            Some((end, size)) if end == msg.len() => (true, size.max(UDP_PLAIN)),
            _ => return refuse(FORMERR),
        },
    };
    if ar == 0 && after != msg.len() {
        return refuse(FORMERR);
    }
    Ok(Question { id, flags, name, qtype, qclass, raw: msg[12..after].to_vec(), edns, udp })
}

/// The name at `at` as text, and where it ends where it started. A question's has no compression pointer (it has
/// nothing to point back to); a response's (`pointers`) may — each to an earlier place, at most `NAME_JUMPS`.
fn read_name(msg: &[u8], start: usize, pointers: bool) -> Option<(String, usize)> {
    let (mut at, mut end, mut jumps, mut wire) = (start, None, 0, 0);
    let mut text = String::new();
    loop {
        let n = usize::from(*msg.get(at)?);
        if n == 0 {
            at += 1;
            break;
        }
        if n >= 0xc0 && pointers {
            let to = usize::from(u16::from_be_bytes([(n & 0x3f) as u8, *msg.get(at + 1)?]));
            end.get_or_insert(at + 2);
            jumps += 1;
            // (only back: what's pointed to was there before — no loop)
            if jumps > NAME_JUMPS || to >= at {
                return None;
            }
            at = to;
            continue;
        }
        // (a pointer in a question, or the reserved label types)
        wire += 1 + n;
        if n > 63 || wire + 1 > NAME_WIRE_MAX {
            return None;
        }
        if !text.is_empty() {
            text.push('.');
        }
        for &c in msg.get(at + 1..at + 1 + n)? {
            match c.to_ascii_lowercase() {
                c @ (b'!'..=b'~') if c != b'.' && c != b'\\' => text.push(char::from(c)),
                c => text.push_str(&format!("\\{c:03}")),
            }
        }
        at += 1 + n;
    }
    Some((text, end.unwrap_or(at)))
}

/// The OPT record at `at` (RFC 6891 §6.1.2: the root name, type 41, its RDATA within the message): where it ends,
/// and the UDP size it says (its class).
fn opt(msg: &[u8], at: usize) -> Option<(usize, u16)> {
    if *msg.get(at)? != 0 || be16(msg, at + 1)? != TYPE_OPT {
        return None;
    }
    let len = usize::from(be16(msg, at + 9)?);
    let end = at + 11 + len;
    (end <= msg.len()).then_some((end, be16(msg, at + 3)?))
}

/// tarae's answer to a question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// These addresses — each as the record its family is (the caller gives only the question's type's).
    Addresses(Vec<IpAddr>),
    /// A PTR record naming this host name.
    Ptr(String),
    /// NOERROR with no record: the name is there, not this type (NODATA), or a type answered empty (HTTPS/SVCB).
    Empty,
    /// Only an rcode: NXDOMAIN, REFUSED, SERVFAIL.
    Rcode(u8),
    /// SRV or TXT records tarae's own lookup brought back (6e+), each under the question's name.
    Records(Vec<Record>),
}

/// A record tarae's own lookup brought back (6e+), as it answers a run's question with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// RFC 2782: where the service is — `target` a host name (`parse::name`'s form), or `""` for none there.
    Srv { priority: u16, weight: u16, port: u16, target: String },
    /// RFC 1035 §3.3.14: its character-strings as sent, each at most 255 bytes.
    Txt(Vec<Vec<u8>>),
}

impl Record {
    /// Its RDATA on the wire — None where it doesn't fit one.
    fn data(&self) -> Option<Vec<u8>> {
        let d = match self {
            Record::Srv { priority, weight, port, target } => {
                let mut d = [priority.to_be_bytes(), weight.to_be_bytes(), port.to_be_bytes()].concat();
                d.extend(wire_name(target)?);
                d
            }
            Record::Txt(strings) => {
                let mut d = Vec::new();
                for s in strings {
                    d.push(u8::try_from(s.len()).ok()?);
                    d.extend_from_slice(s);
                }
                d
            }
        };
        (d.len() <= usize::from(u16::MAX)).then_some(d)
    }

    fn kind(&self) -> u16 {
        match self {
            Record::Srv { .. } => TYPE_SRV,
            Record::Txt(_) => TYPE_TXT,
        }
    }
}

/// The answer to `q`: `a`, its records living `ttl` seconds.
pub fn answer(q: &Question, a: &Answer, ttl: u32) -> Vec<u8> {
    let rcode = match a {
        Answer::Rcode(r) => *r,
        _ => NOERROR,
    };
    let mut records = Vec::new();
    let mut count = 0u16;
    let mut record = |kind: u16, data: &[u8]| {
        // (the name: a pointer to the question's, at 12)
        records.extend_from_slice(&[0xc0, 0x0c]);
        records.extend_from_slice(&kind.to_be_bytes());
        records.extend_from_slice(&CLASS_IN.to_be_bytes());
        records.extend_from_slice(&ttl.to_be_bytes());
        records.extend_from_slice(&(data.len() as u16).to_be_bytes());
        records.extend_from_slice(data);
        count += 1;
    };
    match a {
        Answer::Addresses(all) => {
            for ip in all {
                match ip {
                    IpAddr::V4(v4) => record(TYPE_A, &v4.octets()),
                    IpAddr::V6(v6) => record(TYPE_AAAA, &v6.octets()),
                }
            }
        }
        Answer::Ptr(name) => {
            if let Some(wire) = wire_name(name) {
                record(TYPE_PTR, &wire);
            }
        }
        Answer::Records(all) => {
            for r in all {
                if let Some(data) = r.data() {
                    record(r.kind(), &data);
                }
            }
        }
        Answer::Empty | Answer::Rcode(_) => {}
    }
    let mut m = header(q.id, q.flags, rcode, 1, count, u16::from(q.edns));
    m.extend_from_slice(&q.raw);
    m.extend_from_slice(&records);
    if q.edns {
        m.extend_from_slice(&[0, 0, 41]);
        m.extend_from_slice(&UDP_PAYLOAD.to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    }
    m
}

/// The answer to what `Refused::Answered` says: its rcode alone, the question left out (it couldn't be read).
pub fn refusal(id: u16, flags: u8, rcode: u8) -> Vec<u8> {
    header(id, flags, rcode, 0, 0, 0)
}

/// `answer`, as a datagram (`udp`) carries it: past the size its client takes (`Question.udp`), the question alone
/// with TC set — the client asks again over TCP (RFC 1035 §4.2.1).
pub fn fitted(q: &Question, a: &Answer, ttl: u32, udp: bool) -> Vec<u8> {
    let m = answer(q, a, ttl);
    if !udp || m.len() <= usize::from(q.udp) {
        return m;
    }
    let mut t = answer(q, &Answer::Empty, 0);
    t[2] |= 0x02;
    t
}

/// tarae's own question for `name`'s records of `qtype` (6e+): ID `id`, RD set, an OPT saying `UDP_PAYLOAD` — None
/// where `name` isn't one that fits the wire.
pub fn query(id: u16, name: &str, qtype: u16) -> Option<Vec<u8>> {
    let mut m = id.to_be_bytes().to_vec();
    m.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 1]);
    m.extend(wire_name(name)?);
    m.extend_from_slice(&qtype.to_be_bytes());
    m.extend_from_slice(&CLASS_IN.to_be_bytes());
    m.extend_from_slice(&[0, 0, 41]);
    m.extend_from_slice(&UDP_PAYLOAD.to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    Some(m)
}

/// What the response to tarae's own question came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// The records of the type asked — the name's, or an alias's the answer followed — at most `RECORDS_MAX`, and
    /// the lowest TTL among them.
    Records(Vec<Record>, u32),
    /// The name is there, without a record of that type (NODATA).
    Empty,
    /// No such name (NXDOMAIN).
    No,
    /// Cut short (TC): asked again over TCP.
    Truncated,
    /// The server failed, or refused (SERVFAIL, REFUSED …): the next one is asked.
    Failed,
}

/// `msg` read as the response to `query` (`query`'s message) — Err where it isn't: another ID, not a response,
/// another question, a record cut short or reaching past the message. Never trusted past what's there: names through
/// `read_name`'s pointers, every length checked.
pub fn response(msg: &[u8], query: &[u8]) -> Result<Found, ()> {
    let (Some((asked, end)), true) = (read_name(query, 12, false), msg.len() >= 12) else { return Err(()) };
    let (qtype, qend) = (be16(query, end).ok_or(())?, end + 4);
    // (its ID, QR, and the question as asked — a server may change its case)
    let same = msg.get(12..qend).zip(query.get(12..qend)).is_some_and(|(a, q)| a.eq_ignore_ascii_case(q));
    if msg[..2] != query[..2] || msg[2] & 0x80 == 0 || be16(msg, 4) != Some(1) || !same {
        return Err(());
    }
    if msg[2] & 0x02 != 0 {
        return Ok(Found::Truncated);
    }
    match msg[3] & 0x0f {
        NOERROR => {}
        NXDOMAIN => return Ok(Found::No),
        _ => return Ok(Found::Failed),
    }
    let an = be16(msg, 6).ok_or(())?;
    let mut at = qend;
    // (owner, type, its data's place)
    let mut read = Vec::new();
    for _ in 0..an {
        let (owner, end) = read_name(msg, at, true).ok_or(())?;
        let (kind, class) = (be16(msg, end).ok_or(())?, be16(msg, end + 2).ok_or(())?);
        let ttl = u32::from_be_bytes(msg.get(end + 4..end + 8).ok_or(())?.try_into().map_err(|_| ())?);
        let len = usize::from(be16(msg, end + 8).ok_or(())?);
        let data = end + 10;
        if data + len > msg.len() {
            return Err(());
        }
        if class == CLASS_IN {
            read.push((owner, kind, ttl, data, len));
        }
        at = data + len;
    }
    // (the name asked, and the aliases the answer leads it through)
    let mut owners = vec![asked];
    for _ in 0..8 {
        let more: Vec<String> = read
            .iter()
            .filter(|(o, k, ..)| *k == TYPE_CNAME && owners.contains(o))
            .filter_map(|&(_, _, _, d, len)| {
                read_name(msg, d, true).filter(|(_, e)| *e == d + len).map(|(n, _)| n)
            })
            .filter(|n| !owners.contains(n))
            .collect();
        if more.is_empty() {
            break;
        }
        owners.extend(more);
    }
    let (mut records, mut ttl) = (Vec::new(), u32::MAX);
    for &(ref owner, kind, t, d, len) in &read {
        if kind != qtype || !owners.contains(owner) || records.len() >= RECORDS_MAX {
            continue;
        }
        if let Some(r) = rdata(msg, kind, d, len) {
            records.push(r);
            ttl = ttl.min(t);
        }
    }
    Ok(match records.is_empty() {
        true => Found::Empty,
        false => Found::Records(records, ttl),
    })
}

/// A record's data at `d` (`len` bytes) as tarae answers with it — None where it isn't whole, or (SRV) names a
/// target that isn't a host name.
fn rdata(msg: &[u8], kind: u16, d: usize, len: usize) -> Option<Record> {
    let data = msg.get(d..d + len)?;
    match kind {
        TYPE_SRV => {
            let (priority, weight, port) = (be16(data, 0)?, be16(data, 2)?, be16(data, 4)?);
            let (target, end) = read_name(msg, d + 6, true)?;
            let named = target.is_empty() || super::parse::name(&target).as_deref() == Ok(target.as_str());
            (end == d + len && named).then_some(Record::Srv { priority, weight, port, target })
        }
        TYPE_TXT => {
            let (mut strings, mut at) = (Vec::new(), 0);
            while at < data.len() {
                let n = usize::from(data[at]);
                strings.push(data.get(at + 1..at + 1 + n)?.to_vec());
                at += 1 + n;
            }
            Some(Record::Txt(strings))
        }
        _ => None,
    }
}

/// A response's header: QR, the asked opcode and RD, RA (tarae recurses for it, in the sense a stub means).
fn header(id: u16, flags: u8, rcode: u8, qd: u16, an: u16, ar: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(512);
    m.extend_from_slice(&id.to_be_bytes());
    m.push(0x80 | (flags & 0x79));
    m.push(0x80 | (rcode & 0x0f));
    for n in [qd, an, 0, ar] {
        m.extend_from_slice(&n.to_be_bytes());
    }
    m
}

/// A host name (`parse::name`'s form) on the wire; None where it isn't one that fits.
fn wire_name(name: &str) -> Option<Vec<u8>> {
    let mut w = Vec::with_capacity(name.len() + 2);
    for label in name.split('.').filter(|l| !l.is_empty()) {
        if label.len() > 63 {
            return None;
        }
        w.push(label.len() as u8);
        w.extend_from_slice(label.as_bytes());
    }
    w.push(0);
    (w.len() <= NAME_WIRE_MAX).then_some(w)
}

/// `msg` as DNS over TCP sends it: its length first (RFC 1035 §4.2.2).
pub fn framed(msg: &[u8]) -> Vec<u8> {
    let mut f = (msg.len() as u16).to_be_bytes().to_vec();
    f.extend_from_slice(msg);
    f
}

/// The properties the loops and the fuzz targets hold these readers to (`fuzz/parsers`: `--cfg fuzzing`).
#[cfg(any(test, fuzzing))]
#[path = "dns/check.rs"]
pub(crate) mod check;

#[cfg(test)]
mod tests;
