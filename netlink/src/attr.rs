//! Netlink attributes: written (a type, a value, padded to four bytes; a nest's length set once its contents are
//! written) and read back — every length checked against what's there before a byte is taken.

use crate::Malformed;

/// The nested flag of an attribute's type (`NLA_F_NESTED`); with `NLA_F_NET_BYTEORDER` the two bits a type's
/// number leaves out.
pub(crate) const NESTED: u16 = 0x8000;
const FLAGS: u16 = 0xc000;

pub(crate) fn align(n: usize) -> usize {
    (n + 3) & !3
}

pub(crate) fn pad(b: &mut Vec<u8>) {
    b.resize(align(b.len()), 0);
}

/// An attribute's or a nest's length as its header holds it: at most 65535 bytes. Past that a panic, never a wrapped
/// length — the kernel would read a short one as another layout. Every length here is the crate's caller's own code
/// (names, a 16-byte address: nf_tables takes no more than 64 bytes of data), never what a run or a person typed: one
/// that long is a bug, stopped before anything is sent.
fn header_len(len: usize) -> u16 {
    u16::try_from(len)
        .unwrap_or_else(|_| panic!("a netlink attribute of {len} bytes: past the 65535 its header can say"))
}

/// An attribute: its length counts its header and value, not its padding.
pub(crate) fn put(b: &mut Vec<u8>, kind: u16, value: &[u8]) {
    b.extend(header_len(4 + value.len()).to_ne_bytes());
    b.extend(kind.to_ne_bytes());
    b.extend(value);
    pad(b);
}

/// A string, NUL-terminated.
pub(crate) fn put_str(b: &mut Vec<u8>, kind: u16, s: &str) {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    put(b, kind, &v);
}

/// A 32-bit number in network order (nf_tables' numbers).
pub(crate) fn put_be32(b: &mut Vec<u8>, kind: u16, v: u32) {
    put(b, kind, &v.to_be_bytes());
}

/// A 32-bit number in host order (rtnetlink's numbers).
pub(crate) fn put_u32(b: &mut Vec<u8>, kind: u16, v: u32) {
    put(b, kind, &v.to_ne_bytes());
}

/// A nest: `contents` written inside it, its type flagged nested.
pub(crate) fn nest(b: &mut Vec<u8>, kind: u16, contents: impl FnOnce(&mut Vec<u8>)) {
    let at = b.len();
    b.extend([0; 4]);
    contents(b);
    let len = header_len(b.len() - at);
    b[at..at + 2].copy_from_slice(&len.to_ne_bytes());
    b[at + 2..at + 4].copy_from_slice(&(kind | NESTED).to_ne_bytes());
}

/// An attribute read back: its type (flags taken off), whether it's flagged nested, its value, and where it starts
/// in what it was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Attr<'a> {
    pub kind: u16,
    pub nested: bool,
    pub value: &'a [u8],
    pub at: usize,
}

impl Attr<'_> {
    pub fn u32_ne(&self) -> Result<u32, Malformed> {
        Ok(u32::from_ne_bytes(self.four()?))
    }

    pub fn u32_be(&self) -> Result<u32, Malformed> {
        Ok(u32::from_be_bytes(self.four()?))
    }

    fn four(&self) -> Result<[u8; 4], Malformed> {
        self.value.try_into().map_err(|_| Malformed("a 32-bit attribute of another length"))
    }

    /// A string: up to its NUL (or its end), as UTF-8 where it is.
    pub fn string(&self) -> String {
        let s = self.value.split(|&c| c == 0).next().unwrap_or_default();
        String::from_utf8_lossy(s).into_owned()
    }
}

/// The attributes in `b[from..]`, in order. One whose length is under its header or past what's left ends the walk
/// with an error; nothing after it is read.
pub(crate) fn attrs(b: &[u8], from: usize) -> Attrs<'_> {
    Attrs { b, at: from, broken: false }
}

pub(crate) struct Attrs<'a> {
    b: &'a [u8],
    at: usize,
    broken: bool,
}

impl<'a> Iterator for Attrs<'a> {
    type Item = Result<Attr<'a>, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.broken || self.at >= self.b.len() {
            return None;
        }
        let left = &self.b[self.at..];
        let header = |b: &[u8]| -> Option<(usize, u16)> {
            let len = u16::from_ne_bytes(b.get(0..2)?.try_into().ok()?) as usize;
            let kind = u16::from_ne_bytes(b.get(2..4)?.try_into().ok()?);
            Some((len, kind))
        };
        let Some((len, kind)) = header(left) else {
            self.broken = true;
            return Some(Err(Malformed("an attribute cut short in its header")));
        };
        if len < 4 || len > left.len() {
            self.broken = true;
            return Some(Err(Malformed("an attribute's length past what's there")));
        }
        let a = Attr { kind: kind & !FLAGS, nested: kind & NESTED != 0, value: &left[4..len], at: self.at };
        self.at += align(len);
        Some(Ok(a))
    }
}

/// The type of the attribute starting at `offset` in `b`, its attributes from `from` — looked for at every depth
/// of nesting (a nest's contents start four bytes into it). None: no attribute starts there.
pub(crate) fn find(b: &[u8], from: usize, offset: usize) -> Option<u16> {
    for a in attrs(b, from) {
        let a = a.ok()?;
        let end = a.at + 4 + a.value.len();
        if a.at == offset {
            return Some(a.kind);
        }
        if a.nested && offset > a.at && offset < end {
            return find(&b[..end], a.at + 4, offset);
        }
    }
    None
}
