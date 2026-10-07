//! The egress proxy's two parsers (`docs/design/sandbox-keeper.md` §The egress proxy, `target/sb/step5-plan.md`):
//! a request's head — `CONNECT host:port`, or a plain-http request in absolute form — and the server name a TLS
//! ClientHello carries. Pure: bytes in, an answer out, no I/O; the proxy reads, times and relays. Each is bounded —
//! nothing read past its limit, nothing allocated by a claimed length before what it claims is there — and
//! answers every input with what it found, "more bytes" (None), or a refusal: never a panic (a panic aborts
//! tarae).
//!
//! A host is a name as DNS would look it up or a literal address, never something between: a name's last label
//! starts with a letter, so nothing an address parser reads as an address (`127.1`, `0x7f000001`,
//! `2130706433` — `inet_aton`'s forms, which `getaddrinfo` takes) passes as a name.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The longest head, to its blank line.
pub const HEAD_MAX: usize = 8192;
/// The most header lines a head holds.
pub const HEADERS_MAX: usize = 100;
/// The longest host name (RFC 1035's 253 in text form, without a trailing dot).
pub const HOST_MAX: usize = 253;
/// The longest ClientHello: its handshake body.
pub const HELLO_MAX: usize = 16384;
/// The most TLS records a ClientHello may come in (the clients measured send one).
pub const RECORDS_MAX: usize = 8;
/// The longest TLS record body (RFC 8446 §5.1, 2^14).
const RECORD_MAX: usize = 16384;

/// The plain-http methods taken (the user, 2026-10-07: the standard ones), each host-checked.
pub const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// Where a request goes: a name (lowercase, no trailing dot) or a literal address (an IPv4-mapped IPv6 one as
/// its IPv4 address).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Name(String),
    Addr(IpAddr),
}

impl Host {
    /// As a `Host` header says it, with `port` unless it's `default`.
    fn header(&self, port: u16, default: u16) -> String {
        let host = match self {
            Host::Name(n) => n.clone(),
            Host::Addr(IpAddr::V6(a)) => format!("[{a}]"),
            Host::Addr(a) => a.to_string(),
        };
        if port == default { host } else { format!("{host}:{port}") }
    }
}

/// What a head's `Proxy-Authorization` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    None,
    Basic {
        user: Vec<u8>,
        password: Vec<u8>,
    },
    /// One that can't be read (another scheme, bad base64, no `:`, two of them): given, but wrong.
    Unreadable,
}

/// A request a proxy takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `CONNECT host:port`: a tunnel.
    Connect { host: Host, port: u16 },
    /// A plain-http request in absolute form. `head` is what goes to `host:port`: in origin form, its `Host` the
    /// URI's, no `Proxy-*` nor hop-by-hop header, `Connection: close` (one request per connection).
    Plain { method: &'static str, host: Host, port: u16, head: Vec<u8> },
}

/// A head read whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub request: Request,
    pub auth: Auth,
    /// Its length in what was read, the blank line's too: what follows is the tunnel's or the body's.
    pub len: usize,
}

/// Why a head isn't taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Past `HEAD_MAX` or `HEADERS_MAX` without its end.
    TooLong,
    /// Not a request a proxy takes.
    Malformed(&'static str),
    /// A method neither CONNECT nor a standard one.
    Method,
    /// Its host isn't a name or an address, or its port isn't 1–65535.
    Host(&'static str),
}

impl Refusal {
    /// The status it's answered with.
    pub fn status(self) -> u16 {
        match self {
            Refusal::TooLong => 431,
            Refusal::Malformed(_) | Refusal::Host(_) => 400,
            Refusal::Method => 405,
        }
    }

    /// In a report's words.
    pub fn said(self) -> String {
        match self {
            Refusal::TooLong => format!("a request head past {HEAD_MAX} bytes or {HEADERS_MAX} lines"),
            Refusal::Malformed(why) => format!("not a request a proxy takes: {why}"),
            Refusal::Method => format!("a method other than CONNECT and {}", METHODS.join(" ")),
            Refusal::Host(why) => format!("its host: {why}"),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A token's character (RFC 9110 §5.6.2).
fn tchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

fn token(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(|&c| tchar(c))
}

/// A head in `buf` — what a client sent so far: the request, None while its blank line hasn't come (and
/// `HEAD_MAX` isn't reached), or why it's refused.
pub fn head(buf: &[u8]) -> Result<Option<Head>, Refusal> {
    let window = &buf[..buf.len().min(HEAD_MAX)];
    let Some(end) = find(window, b"\r\n\r\n") else {
        return if buf.len() >= HEAD_MAX { Err(Refusal::TooLong) } else { Ok(None) };
    };
    let mut lines = Vec::new();
    let mut rest = &window[..end];
    while let Some(i) = find(rest, b"\r\n") {
        lines.push(&rest[..i]);
        rest = &rest[i + 2..];
    }
    lines.push(rest);
    // (no bare CR or LF, no control character but a tab: what a line means is only what it shows)
    let line = |l: &'_ [u8]| match l.iter().all(|&c| c == b'\t' || (c >= 0x20 && c != 0x7f)) {
        true => Ok(()),
        false => Err(Refusal::Malformed("a control character in a line")),
    };
    let first = lines[0];
    line(first)?;
    let mut parts = first.split(|&c| c == b' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::Malformed("a request line that isn't three words"));
    };
    if version != b"HTTP/1.1" && version != b"HTTP/1.0" {
        return Err(Refusal::Malformed("not HTTP/1.0 or 1.1"));
    }
    let mut headers: Vec<(&[u8], &[u8])> = Vec::new();
    for &l in &lines[1..] {
        line(l)?;
        if l.first().is_some_and(|&c| c == b' ' || c == b'\t') {
            return Err(Refusal::Malformed("a folded header line"));
        }
        let colon = l.iter().position(|&c| c == b':').ok_or(Refusal::Malformed("a header without `:`"))?;
        let (name, value) = (&l[..colon], l[colon + 1..].trim_ascii());
        if !token(name) {
            return Err(Refusal::Malformed("a header's name"));
        }
        if headers.len() == HEADERS_MAX {
            return Err(Refusal::TooLong);
        }
        headers.push((name, value));
    }
    let named = |n: &'static str| -> Vec<&[u8]> {
        headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(n.as_bytes())).map(|(_, v)| *v).collect()
    };
    let auth = match named("proxy-authorization")[..] {
        [] => Auth::None,
        [v] => basic(v).unwrap_or(Auth::Unreadable),
        _ => Auth::Unreadable,
    };
    let request = if method == b"CONNECT" {
        let (host, port) = authority(target, None)?;
        Request::Connect { host, port }
    } else {
        let method = *METHODS.iter().find(|m| m.as_bytes() == method).ok_or(if token(method) {
            Refusal::Method
        } else {
            Refusal::Malformed("a method that isn't a token")
        })?;
        framing(&named("content-length"), &named("transfer-encoding"))?;
        plain(method, target, version, &headers)?
    };
    Ok(Some(Head { request, auth, len: end + 4 }))
}

/// A plain request's body framed one way only, so the proxy and the server can't read its end differently: no
/// framing, one `Content-Length` of digits alone (19 at most: it fits a `u64`), or one `Transfer-Encoding:
/// chunked` — never both, a list, two of either, or another coding.
fn framing(lengths: &[&[u8]], codings: &[&[u8]]) -> Result<(), Refusal> {
    match (lengths, codings) {
        ([], []) => Ok(()),
        ([n], []) if (1..=19).contains(&n.len()) && n.iter().all(u8::is_ascii_digit) => Ok(()),
        ([], [c]) if c.eq_ignore_ascii_case(b"chunked") => Ok(()),
        ([_, ..], [_, ..]) => Err(Refusal::Malformed("both Content-Length and Transfer-Encoding")),
        ([_], []) | ([_, _, ..], []) => Err(Refusal::Malformed("a Content-Length that isn't one number")),
        _ => Err(Refusal::Malformed("a Transfer-Encoding that isn't one `chunked`")),
    }
}

/// `Basic <base64 of user:password>`.
fn basic(v: &[u8]) -> Option<Auth> {
    let (scheme, b64) = v.split_at(v.iter().position(|&c| c == b' ')?);
    if !scheme.eq_ignore_ascii_case(b"basic") {
        return None;
    }
    let raw = unbase64(&b64[1..])?;
    let colon = raw.iter().position(|&c| c == b':')?;
    Some(Auth::Basic { user: raw[..colon].to_vec(), password: raw[colon + 1..].to_vec() })
}

/// Standard base64, padded, nothing else in it.
fn unbase64(s: &[u8]) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(4) {
        return None;
    }
    let pad = s.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return None;
    }
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::with_capacity(s.len() / 4 * 3), 0u32, 0);
    for &c in &s[..s.len() - pad] {
        acc = (acc << 6) | u32::from(val(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The headers a plain-http request's head loses on its way (`Host` made again from the URI) besides every
/// `Proxy-*` one: the hop-by-hop ones (RFC 9110 §7.6.1).
const HOP_BY_HOP: [&str; 5] = ["host", "connection", "keep-alive", "te", "upgrade"];

/// A plain-http request's target in absolute form (`http://host[:port]/path?query`) and the head that goes on.
fn plain(
    method: &'static str,
    target: &[u8],
    version: &[u8],
    headers: &[(&[u8], &[u8])],
) -> Result<Request, Refusal> {
    let starts = |p: &[u8]| target.get(..p.len()).is_some_and(|s| s.eq_ignore_ascii_case(p));
    let rest = if starts(b"http://") {
        &target[7..]
    } else if starts(b"https://") {
        return Err(Refusal::Malformed("https in absolute form — https goes through CONNECT"));
    } else {
        return Err(Refusal::Malformed("a target that isn't absolute http://"));
    };
    let cut = rest.iter().position(|&c| matches!(c, b'/' | b'?' | b'#')).unwrap_or(rest.len());
    let (auth, path) = rest.split_at(cut);
    if auth.contains(&b'@') {
        return Err(Refusal::Malformed("a user in the target"));
    }
    if path.contains(&b'#') {
        return Err(Refusal::Malformed("a fragment in the target"));
    }
    let (host, port) = authority(auth, Some(80))?;
    // Hop-by-hop: these, and what `Connection` names (RFC 9110 §7.6.1) — but never the body's framing: the head
    // would go on without it while its body still follows, read by the server as another request
    let mut drop: Vec<Vec<u8>> = HOP_BY_HOP.iter().map(|h| h.as_bytes().to_vec()).collect();
    for (_, v) in headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(b"connection")) {
        drop.extend(v.split(|&c| c == b',').map(|n| n.trim_ascii().to_ascii_lowercase()));
    }
    if drop.iter().any(|n| n == b"content-length" || n == b"transfer-encoding") {
        return Err(Refusal::Malformed("`Connection` naming the body's framing"));
    }
    let mut out = Vec::with_capacity(HEAD_MAX);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    if !path.starts_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(path);
    out.push(b' ');
    out.extend_from_slice(version);
    out.extend_from_slice(format!("\r\nHost: {}\r\n", host.header(port, 80)).as_bytes());
    for (k, v) in headers {
        let k_low = k.to_ascii_lowercase();
        // (every `Proxy-*` header is this proxy's, whatever its name)
        if !drop.contains(&k_low) && !k_low.starts_with(b"proxy-") {
            out.extend_from_slice(k);
            out.extend_from_slice(b": ");
            out.extend_from_slice(v);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(Request::Plain { method, host, port, head: out })
}

/// `host:port` (an IPv6 address in brackets) — the port `default` when there's none (CONNECT's has one).
fn authority(a: &[u8], default: Option<u16>) -> Result<(Host, u16), Refusal> {
    let a =
        std::str::from_utf8(a).map_err(|_| Refusal::Host("not ASCII (a name is sent in its A-labels)"))?;
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let close = rest.find(']').ok_or(Refusal::Host("an unclosed `[`"))?;
        let v6 =
            rest[..close].parse::<Ipv6Addr>().map_err(|_| Refusal::Host("not an IPv6 address in `[]`"))?;
        let port = match &rest[close + 1..] {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(Refusal::Host("something after `]`"))?),
        };
        // (an IPv4 address written as IPv6, `[::ffff:a.b.c.d]`, is that IPv4 address: every rule meets it as one)
        (Host::Addr(v6.to_canonical()), port)
    } else {
        let (h, p) = match a.rfind(':') {
            Some(i) => (&a[..i], Some(&a[i + 1..])),
            None => (a, None),
        };
        if h.contains(':') {
            return Err(Refusal::Host("an IPv6 address outside `[]`"));
        }
        (host(h)?, p)
    };
    let port = match (port, default) {
        (Some(p), _) => port_of(p)?,
        (None, Some(d)) => d,
        (None, None) => return Err(Refusal::Host("no port")),
    };
    Ok((host, port))
}

/// A port: one to five digits, 1–65535.
fn port_of(p: &str) -> Result<u16, Refusal> {
    let digits = !p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit());
    match p.parse::<u16>() {
        Ok(n) if digits && n > 0 => Ok(n),
        _ => Err(Refusal::Host("a port that isn't 1–65535")),
    }
}

/// A host outside brackets: a dotted-quad IPv4 address, or a name (`name`).
fn host(h: &str) -> Result<Host, Refusal> {
    if let Ok(v4) = h.parse::<Ipv4Addr>() {
        return Ok(Host::Addr(IpAddr::V4(v4)));
    }
    name(h).map(Host::Name).map_err(Refusal::Host)
}

/// A name as DNS looks it up — lowercase, its trailing dot off: labels of 1–63 letters, digits, `-` (not at
/// either end) and `_`; 253 at most; the last label starting with a letter (no TLD starts with a digit, and so
/// no address form passes as a name). Err why not.
pub fn name(h: &str) -> Result<String, &'static str> {
    let h = h.strip_suffix('.').unwrap_or(h);
    if h.is_empty() || h.len() > HOST_MAX {
        return Err("a name that's empty or longer than 253");
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    };
    if !h.split('.').all(label_ok) {
        return Err("a name's label isn't 1–63 letters, digits, `-` and `_`");
    }
    if !h.rsplit('.').next().and_then(|l| l.bytes().next()).is_some_and(|c| c.is_ascii_alphabetic()) {
        return Err("a name whose last label doesn't start with a letter (an address in another form?)");
    }
    Ok(h.to_ascii_lowercase())
}

/// Why a ClientHello isn't taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelloRefusal {
    /// The bytes aren't a TLS handshake record (ssh, plain text through a tunnel).
    NotTls,
    /// Past `HELLO_MAX` or `RECORDS_MAX`.
    TooLong,
    /// Not a ClientHello as TLS writes one.
    Malformed(&'static str),
    /// No server name in it.
    NoName,
    /// A server name that isn't a name (an address, an invalid one).
    Name(&'static str),
    /// Encrypted Client Hello: the real name is hidden, so it can't be checked (fail closed).
    Ech,
}

impl HelloRefusal {
    /// In a report's words.
    pub fn said(self) -> String {
        match self {
            HelloRefusal::NotTls => "not TLS — only TLS goes through a tunnel".into(),
            HelloRefusal::TooLong => format!("a ClientHello past {HELLO_MAX} bytes or {RECORDS_MAX} records"),
            HelloRefusal::Malformed(why) => format!("not a ClientHello: {why}"),
            HelloRefusal::NoName => "a ClientHello naming no server".into(),
            HelloRefusal::Name(why) => format!("its server name: {why}"),
            HelloRefusal::Ech => "an encrypted ClientHello (ECH) — its server name can't be checked".into(),
        }
    }
}

/// A cursor over bytes: every read checked against what's there.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], HelloRefusal> {
        if n > self.0.len() {
            return Err(HelloRefusal::Malformed("a length past what's there"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn u8(&mut self) -> Result<usize, HelloRefusal> {
        Ok(usize::from(self.take(1)?[0]))
    }

    fn u16(&mut self) -> Result<usize, HelloRefusal> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    /// A vector with a `width`-byte length.
    fn vec(&mut self, width: usize) -> Result<Reader<'a>, HelloRefusal> {
        let n = if width == 1 { self.u8()? } else { self.u16()? };
        Ok(Reader(self.take(n)?))
    }
}

const EXT_SERVER_NAME: usize = 0;
const EXT_ECH: usize = 0xfe0d;

/// The server name of the ClientHello at `buf`'s start — what a client sends first in a tunnel: the name
/// (as `name` makes it), None while the ClientHello isn't all there yet, or why it's refused.
pub fn server_name(buf: &[u8]) -> Result<Option<String>, HelloRefusal> {
    if buf.first().is_some_and(|&c| c != 22) {
        return Err(HelloRefusal::NotTls);
    }
    // The handshake message, from as many records as it takes (the bytes the records hold, never a claim)
    let mut hs: Vec<u8> = Vec::new();
    let (mut at, mut records) = (0, 0);
    let body = loop {
        if hs.len() >= 4 {
            if hs[0] != 1 {
                return Err(HelloRefusal::Malformed("a handshake that isn't a ClientHello"));
            }
            let n = usize::from(hs[1]) << 16 | usize::from(hs[2]) << 8 | usize::from(hs[3]);
            if n > HELLO_MAX {
                return Err(HelloRefusal::TooLong);
            }
            if hs.len() >= 4 + n {
                break &hs[4..4 + n];
            }
        }
        if records == RECORDS_MAX {
            return Err(HelloRefusal::TooLong);
        }
        let Some(h) = buf.get(at..at + 5) else { return Ok(None) };
        if h[0] != 22 {
            return Err(match records {
                0 => HelloRefusal::NotTls,
                _ => HelloRefusal::Malformed("another record inside the ClientHello"),
            });
        }
        if h[1] != 3 {
            return Err(HelloRefusal::NotTls);
        }
        let len = usize::from(u16::from_be_bytes([h[3], h[4]]));
        if len == 0 || len > RECORD_MAX {
            return Err(HelloRefusal::Malformed("a record's length"));
        }
        let Some(fragment) = buf.get(at + 5..at + 5 + len) else { return Ok(None) };
        hs.extend_from_slice(fragment);
        at += 5 + len;
        records += 1;
    };
    let mut r = Reader(body);
    r.take(2 + 32)?; // legacy_version, random
    if r.vec(1)?.0.len() > 32 {
        return Err(HelloRefusal::Malformed("a session id past 32 bytes"));
    }
    let suites = r.vec(2)?;
    if suites.0.is_empty() || !suites.0.len().is_multiple_of(2) {
        return Err(HelloRefusal::Malformed("its cipher suites"));
    }
    if r.vec(1)?.0.is_empty() {
        return Err(HelloRefusal::Malformed("no compression method"));
    }
    if r.0.is_empty() {
        return Err(HelloRefusal::NoName);
    }
    let mut exts = r.vec(2)?;
    if !r.0.is_empty() {
        return Err(HelloRefusal::Malformed("bytes after its extensions"));
    }
    let (mut seen, mut found) = (Vec::new(), None);
    while !exts.0.is_empty() {
        let kind = exts.u16()?;
        let mut data = exts.vec(2)?;
        if seen.contains(&kind) {
            return Err(HelloRefusal::Malformed("an extension twice"));
        }
        seen.push(kind);
        match kind {
            EXT_ECH => return Err(HelloRefusal::Ech),
            EXT_SERVER_NAME => {
                let mut list = data.vec(2)?;
                if !data.0.is_empty() || list.0.is_empty() {
                    return Err(HelloRefusal::Malformed("its server_name extension"));
                }
                while !list.0.is_empty() {
                    let kind = list.u8()?;
                    let n = list.vec(2)?;
                    if kind != 0 {
                        continue;
                    }
                    if found.is_some() {
                        return Err(HelloRefusal::Malformed("two server names"));
                    }
                    found = Some(n.0);
                }
            }
            _ => {}
        }
    }
    let n = found.ok_or(HelloRefusal::NoName)?;
    let n = std::str::from_utf8(n).map_err(|_| HelloRefusal::Name("not ASCII"))?;
    if n.parse::<IpAddr>().is_ok() {
        return Err(HelloRefusal::Name("an address (TLS names none)"));
    }
    name(n).map(Some).map_err(HelloRefusal::Name)
}

/// Whether a ClientHello's server name is the host a CONNECT named — never for an address.
pub fn same_host(connect: &Host, sni: &str) -> bool {
    matches!(connect, Host::Name(n) if n == sni)
}

#[cfg(test)]
mod tests;
