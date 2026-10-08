//! The D-Bus a keeper's scope takes (6g, D1: in process — no `busctl` to find or spawn): one method call on the
//! user's bus — systemd's `StartTransientUnit` for a scope with `Delegate=yes` and the keeper's pid — and its answer;
//! nothing else of D-Bus. The wire is the specification's ("Message Format"): a 12-byte fixed header (endianness,
//! kind, flags, version 1, body length, serial), the header fields `a(yv)`, padding to 8, the body; every value aligned
//! to its size from the message's start, padding nul. Sent little-endian, its header fields in sd-bus's order (path,
//! member, interface, destination, signature) — held byte for byte against what `busctl` sent (M11's capture, the
//! bus's own `sender` field aside). Read either way round, bounded: a message past `MAX`, a length past what's there,
//! padding or a terminator that isn't nul, a field of a type it doesn't know — each a refusal (`Malformed`), never a
//! panic. The body is read only as far as a reply's first string (the job's path, an error's words).
//! Authentication is `EXTERNAL` with this process's uid, the way every client on a local bus does it.
// (elsewhere only its tests build it)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// The longest message read — a reply here is a few hundred bytes; the bus's own limit is 128 MiB.
pub const MAX: usize = 64 * 1024;

pub const CALL: u8 = 1;
pub const RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

/// Header field codes.
pub const PATH: u8 = 1;
pub const INTERFACE: u8 = 2;
pub const MEMBER: u8 = 3;
pub const ERROR_NAME: u8 = 4;
pub const REPLY_SERIAL: u8 = 5;
pub const DESTINATION: u8 = 6;
/// (the bus writes it in; only the tests read it)
#[cfg_attr(not(test), allow(dead_code))]
pub const SENDER: u8 = 7;
pub const SIGNATURE: u8 = 8;

/// Why bytes aren't a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

/// A header field's value: a string (`s`, `o`, `g`) or a number (`u`), with its type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(char, String),
    U32(u32),
}

/// A message: its kind, flags, serial, header fields in order, and its body as bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub fields: Vec<(u8, Value)>,
    pub body: Vec<u8>,
    /// Sent big-endian (`B`).
    pub big: bool,
}

impl Message {
    /// A method call (little-endian, no flags), its fields in sd-bus's order.
    pub fn call(serial: u32, destination: &str, path: &str, interface: &str, member: &str) -> Message {
        let s = |c, v: &str| Value::Str(c, v.to_string());
        Message {
            kind: CALL,
            flags: 0,
            serial,
            fields: vec![
                (PATH, s('o', path)),
                (MEMBER, s('s', member)),
                (INTERFACE, s('s', interface)),
                (DESTINATION, s('s', destination)),
            ],
            body: Vec::new(),
            big: false,
        }
    }

    /// With `body`, of `signature`.
    pub fn with_body(mut self, signature: &str, body: Vec<u8>) -> Message {
        self.fields.push((SIGNATURE, Value::Str('g', signature.to_string())));
        self.body = body;
        self
    }

    /// Header field `code`, a string.
    pub fn str(&self, code: u8) -> Option<&str> {
        self.fields.iter().find_map(|(c, v)| match v {
            Value::Str(_, s) if *c == code => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn reply_serial(&self) -> Option<u32> {
        self.fields.iter().find_map(|(c, v)| match v {
            Value::U32(n) if *c == REPLY_SERIAL => Some(*n),
            _ => None,
        })
    }

    /// The body's first value where it's a string or an object path (a return's job, an error's words).
    pub fn first_str(&self) -> Result<Option<String>, Malformed> {
        let sig = self.str(SIGNATURE).unwrap_or("");
        if !sig.starts_with(['s', 'o']) {
            return Ok(None);
        }
        let mut r = Reader { b: &self.body, at: 0, big: self.big };
        r.string().map(Some)
    }
}

/// A body (or a header) being written, little-endian, aligned from where it starts — a message's body starts at a
/// multiple of 8, so that's its alignment from the message's start too.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    fn align(&mut self, n: usize) {
        while !self.0.len().is_multiple_of(n) {
            self.0.push(0);
        }
    }

    pub fn u32(&mut self, v: u32) {
        self.align(4);
        self.0.extend(v.to_le_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.u32(v as u32);
    }

    /// `s` or `o`.
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend(s.as_bytes());
        self.0.push(0);
    }

    /// `g`, a variant's signature.
    pub fn sig(&mut self, s: &str) {
        self.0.push(s.len() as u8);
        self.0.extend(s.as_bytes());
        self.0.push(0);
    }

    pub fn byte(&mut self, v: u8) {
        self.0.push(v);
    }

    /// An array of elements aligned to `align`: its length counts from after the padding to its first element,
    /// which is there even when it's empty.
    pub fn array(&mut self, align: usize, items: impl FnOnce(&mut Writer)) {
        self.u32(0);
        let at = self.0.len() - 4;
        self.align(align);
        let start = self.0.len();
        items(self);
        let len = (self.0.len() - start) as u32;
        self.0[at..at + 4].copy_from_slice(&len.to_le_bytes());
    }

    /// A struct: aligned to 8.
    pub fn structure(&mut self, items: impl FnOnce(&mut Writer)) {
        self.align(8);
        items(self);
    }
}

/// `m` on the wire (little-endian whatever it was read as).
pub fn encode(m: &Message) -> Vec<u8> {
    let mut w = Writer::default();
    w.0.extend([b'l', m.kind, m.flags, 1]);
    w.u32(m.body.len() as u32);
    w.u32(m.serial);
    w.array(8, |w| {
        for (code, v) in &m.fields {
            w.structure(|w| {
                w.byte(*code);
                match v {
                    Value::Str(c, s) => {
                        w.sig(&c.to_string());
                        match c {
                            'g' => w.sig(s),
                            _ => w.str(s),
                        }
                    }
                    Value::U32(n) => {
                        w.sig("u");
                        w.u32(*n);
                    }
                }
            });
        }
    });
    w.align(8);
    w.0.extend(&m.body);
    w.0
}

/// A message's whole length from its first 16 bytes — what to read before `decode` — or why it can't be one.
pub fn length(head: &[u8]) -> Result<usize, Malformed> {
    let mut r = Reader { b: head, at: 0, big: false };
    r.big = match r.u8()? {
        b'l' => false,
        b'B' => true,
        _ => return Err(Malformed("an endianness that isn't one")),
    };
    r.at = 4;
    let body = r.u32()? as usize;
    r.at = 12;
    let fields = r.u32()? as usize;
    let len = 16usize.checked_add(fields.next_multiple_of(8)).and_then(|n| n.checked_add(body));
    len.filter(|&n| n <= MAX).ok_or(Malformed("past the longest message"))
}

/// The message `b` is, exactly.
pub fn decode(b: &[u8]) -> Result<Message, Malformed> {
    if length(b)? != b.len() {
        return Err(Malformed("not one message's length"));
    }
    let mut r = Reader { b, at: 0, big: b[0] == b'B' };
    r.at = 1;
    let (kind, flags, version) = (r.u8()?, r.u8()?, r.u8()?);
    if !(CALL..=SIGNAL).contains(&kind) || version != 1 {
        return Err(Malformed("a kind or a version that isn't one"));
    }
    let body_len = r.u32()? as usize;
    let serial = r.u32()?;
    if serial == 0 {
        return Err(Malformed("serial 0"));
    }
    let fields_len = r.u32()? as usize;
    let end = r.at + fields_len;
    let mut fields = Vec::new();
    while r.at < end {
        r.pad(8)?;
        let code = r.u8()?;
        let sig = r.signature()?;
        let v = match sig.as_str() {
            "s" | "o" => Value::Str(sig.chars().next().unwrap_or('s'), r.string()?),
            "g" => Value::Str('g', r.signature()?),
            "u" => Value::U32(r.u32()?),
            _ => return Err(Malformed("a header field of a type it doesn't read")),
        };
        if code == 0 {
            return Err(Malformed("header field 0"));
        }
        fields.push((code, v));
    }
    if r.at != end {
        return Err(Malformed("header fields past their length"));
    }
    r.pad(8)?;
    if r.at + body_len != b.len() {
        return Err(Malformed("a body not its length"));
    }
    let m = Message { kind, flags, serial, fields, body: b[r.at..].to_vec(), big: r.big };
    // (what each kind must carry, as the bus would refuse it otherwise)
    let needs: &[u8] = match kind {
        CALL => &[PATH, MEMBER],
        RETURN => &[REPLY_SERIAL],
        ERROR => &[ERROR_NAME, REPLY_SERIAL],
        _ => &[PATH, INTERFACE, MEMBER],
    };
    if needs.iter().any(|c| !m.fields.iter().any(|(f, _)| f == c)) {
        return Err(Malformed("a header field its kind needs missing"));
    }
    if !m.body.is_empty() && m.str(SIGNATURE).is_none() {
        return Err(Malformed("a body without its signature"));
    }
    Ok(m)
}

/// A message read so far.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
    big: bool,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or(Malformed("cut short"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    /// Padding to `n`: nul bytes only.
    fn pad(&mut self, n: usize) -> Result<(), Malformed> {
        let to = self.at.next_multiple_of(n);
        if self.take(to - self.at)?.iter().any(|&p| p != 0) {
            return Err(Malformed("padding that isn't nul"));
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        self.pad(4)?;
        let a: [u8; 4] = self.take(4)?.try_into().map_err(|_| Malformed("cut short"))?;
        Ok(if self.big { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    }

    /// `n` bytes of text and their nul.
    fn text(&mut self, n: usize) -> Result<String, Malformed> {
        let s = self.take(n)?;
        if self.u8()? != 0 || s.contains(&0) {
            return Err(Malformed("a string not ended by its nul"));
        }
        String::from_utf8(s.to_vec()).map_err(|_| Malformed("a string that isn't UTF-8"))
    }

    fn string(&mut self) -> Result<String, Malformed> {
        let n = self.u32()? as usize;
        self.text(n)
    }

    fn signature(&mut self) -> Result<String, Malformed> {
        let n = self.u8()? as usize;
        self.text(n)
    }
}

/// The body of `StartTransientUnit(name, mode, properties, aux)` for scope `unit` holding `pid`: `fail` (a unit of
/// that name already there is an error, never replaced), `Delegate=yes` (the subtree is the user's — the keeper makes
/// its runs' cgroups in it), `PIDs=[pid]`, `CollectMode=inactive-or-failed` (gone once empty, failed or not); no aux.
pub fn start_transient_unit(unit: &str, pid: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.str(unit);
    w.str("fail");
    w.array(8, |w| {
        w.structure(|w| {
            w.str("Delegate");
            w.sig("b");
            w.bool(true);
        });
        w.structure(|w| {
            w.str("PIDs");
            w.sig("au");
            w.array(4, |w| w.u32(pid));
        });
        w.structure(|w| {
            w.str("CollectMode");
            w.sig("s");
            w.str("inactive-or-failed");
        });
    });
    w.array(8, |_| {});
    w.0
}

/// The call that asks systemd for scope `unit` holding `pid` (`start_transient_unit`), as serial `serial`.
pub fn scope_call(serial: u32, unit: &str, pid: u32) -> Message {
    Message::call(
        serial,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
    )
    .with_body("ssa(sv)a(sa(sv))", start_transient_unit(unit, pid))
}

/// Asks the user's systemd manager, over the bus at `bus`, for scope `unit` holding `pid` — by `deadline`: the job's
/// path once it's taken (the move itself comes a little after: the caller watches for it), else why not — the
/// manager's error and words (`org.freedesktop.systemd1.UnitExists: …`), or what went wrong on the way.
pub fn start_scope(bus: &Path, unit: &str, pid: u32, deadline: Instant) -> Result<String, String> {
    let left = || deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let io = |what: &str, e: std::io::Error| format!("the user bus ({}): {what}: {e}", bus.display());
    let mut s = std::os::unix::net::UnixStream::connect(bus).map_err(|e| io("connect", e))?;
    s.set_write_timeout(Some(left())).map_err(|e| io("connect", e))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
    s.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes()).map_err(|e| io("auth", e))?;
    let mut line = Vec::new();
    while !line.ends_with(b"\r\n") {
        s.set_read_timeout(Some(left())).map_err(|e| io("auth", e))?;
        let mut c = [0u8; 1];
        match s.read(&mut c) {
            Ok(1) if line.len() < 256 => line.push(c[0]),
            Ok(_) => return Err(format!("the user bus ({}): no answer to its auth", bus.display())),
            Err(e) => return Err(io("auth", e)),
        }
    }
    if !line.starts_with(b"OK ") {
        let said = String::from_utf8_lossy(&line).trim().to_string();
        return Err(format!("the user bus ({}) refused its auth: {said}", bus.display()));
    }
    s.write_all(b"BEGIN\r\n").map_err(|e| io("auth", e))?;
    let hello =
        Message::call(1, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello");
    let call = scope_call(2, unit, pid);
    s.write_all(&[encode(&hello), encode(&call)].concat()).map_err(|e| io("the call", e))?;
    // (its answer among what else comes: Hello's, NameAcquired — at most this many)
    for _ in 0..64 {
        let m = read_message(&mut s, left).map_err(|e| format!("the user bus ({}): {e}", bus.display()))?;
        if let Some(said) = answer(&m, 2) {
            return said;
        }
    }
    Err(format!("the user bus ({}): no answer to the call", bus.display()))
}

/// What `m` answers call `serial` with — None: it isn't that call's answer (another's, a signal). A return: its first
/// value (a job's path); an error: its name and words.
pub fn answer(m: &Message, serial: u32) -> Option<Result<String, String>> {
    if m.reply_serial() != Some(serial) || !matches!(m.kind, RETURN | ERROR) {
        return None;
    }
    let said = m.first_str().ok().flatten().unwrap_or_default();
    Some(match m.kind {
        RETURN => Ok(said),
        _ => Err(format!("{}: {said}", m.str(ERROR_NAME).unwrap_or("an error"))),
    })
}

/// The next message on `s`, each read within what's `left`.
fn read_message(
    s: &mut std::os::unix::net::UnixStream,
    left: impl Fn() -> Duration,
) -> Result<Message, String> {
    let mut read = |b: &mut [u8]| {
        s.set_read_timeout(Some(left())).and_then(|()| s.read_exact(b)).map_err(|e| e.to_string())
    };
    let mut head = [0u8; 16];
    read(&mut head)?;
    let len = length(&head).map_err(|e| e.0.to_string())?;
    let mut b = head.to_vec();
    b.resize(len.max(16), 0);
    read(&mut b[16..])?;
    decode(&b).map_err(|e| e.0.to_string())
}

/// Tests: the packets of a pcapng capture (`busctl capture`'s): each Enhanced Packet Block's data — one message.
#[cfg(test)]
pub fn pcapng_packets(b: &[u8]) -> Vec<Vec<u8>> {
    let u32_at = |at: usize| b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    let mut out = Vec::new();
    let mut at = 0;
    while let (Some(kind), Some(len)) = (u32_at(at), u32_at(at + 4)) {
        if kind == 6
            && let Some(n) = u32_at(at + 20)
        {
            out.extend(b.get(at + 28..at + 28 + n as usize).map(<[u8]>::to_vec));
        }
        if len < 12 {
            break;
        }
        at += len as usize;
    }
    out
}

#[cfg(test)]
mod tests;
