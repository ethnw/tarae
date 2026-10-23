//! The properties the proxy's parsers keep, one function a parser — whatever the bytes, each holds or the function
//! panics: the in-tree loops (`tests.rs`) call them on seeded mutations, the fuzz targets (`fuzz/parsers`, built with
//! `--cfg fuzzing`) on libFuzzer's inputs. Each says what came of its input in a word.

use super::*;

/// Whatever a head holds, the parser answers a request, "more", or a refusal — never a panic, never a byte past
/// `HEAD_MAX`: a request's head ends at its first blank line, its host is a name as `name` makes it (or an
/// address), a plain request's head goes on with no `Proxy-*` header and `Connection: close`, its body's framing as
/// the client sent it; "more" only while no blank line is in the first `HEAD_MAX` bytes.
pub(crate) fn head(b: &[u8]) -> &'static str {
    let blank = find(&b[..b.len().min(HEAD_MAX)], b"\r\n\r\n");
    match super::head(b) {
        Ok(Some(h)) => {
            assert!(h.len <= HEAD_MAX && Some(h.len - 4) == blank, "ends at its first blank line");
            let (Request::Connect { host, port } | Request::Plain { host, port, .. }) = &h.request;
            assert!(*port > 0);
            if let Host::Name(n) = host {
                assert_eq!(name(n).as_ref(), Ok(n), "a name as `name` makes it");
            }
            if let Request::Plain { head: on, .. } = &h.request {
                let low = on.to_ascii_lowercase();
                assert!(on.ends_with(b"\r\nConnection: close\r\n\r\n") && find(&low, b"\nproxy-").is_none());
                // (the body's framing goes on as the client sent it, or the request isn't taken)
                let sent = b[..h.len].to_ascii_lowercase();
                for f in [&b"\ncontent-length:"[..], b"\ntransfer-encoding:"] {
                    assert_eq!(find(&low, f).is_some(), find(&sent, f).is_some(), "its framing kept");
                }
            }
            "request"
        }
        Ok(None) => {
            assert!(blank.is_none() && b.len() < HEAD_MAX, "more only while no blank line came");
            "more"
        }
        Err(_) => "refused",
    }
}

/// What a tunnel's first bytes claim, read apart from the parser: the handshake's length and how many records it
/// took to come whole — None where they don't hold one.
pub(crate) fn claimed(b: &[u8]) -> Option<(usize, usize)> {
    let (mut hs, mut at, mut records) = (Vec::new(), 0, 0);
    loop {
        if hs.len() >= 4 {
            let n = usize::from(hs[1]) << 16 | usize::from(hs[2]) << 8 | usize::from(hs[3]);
            if hs.len() >= 4 + n {
                return Some((n, records));
            }
        }
        let h = b.get(at..at + 5)?;
        let len = usize::from(u16::from_be_bytes([h[3], h[4]]));
        hs.extend_from_slice(b.get(at + 5..at + 5 + len)?);
        at += 5 + len;
        records += 1;
    }
}

/// Whatever a tunnel's first bytes hold, the parser answers a name, "more", or a refusal — never a panic: a name it
/// answers is a name as `name` makes it, never an address, and only from a ClientHello within `HELLO_MAX` that came
/// in `RECORDS_MAX` records at most (`claimed`).
pub(crate) fn sni(b: &[u8]) -> &'static str {
    match server_name(b) {
        Ok(Some(n)) => {
            assert_eq!(name(&n).as_ref(), Ok(&n));
            assert!(n.parse::<IpAddr>().is_err(), "never an address: {n}");
            let (len, records) = claimed(b).expect("a name only from a whole ClientHello");
            assert!(len <= HELLO_MAX && records <= RECORDS_MAX, "within its limits: {len} in {records}");
            "name"
        }
        Ok(None) => "more",
        Err(_) => "refused",
    }
}

/// Whatever a chunked body's bytes hold, given `size()` bytes at a time, its parser takes at most what it's given,
/// all of it until it's done, never panics, and once done takes nothing more. Done, refused, or "more" (the bytes
/// ran out first).
pub(crate) fn body(b: &[u8], mut size: impl FnMut() -> usize) -> &'static str {
    let mut body = Body::new(Framing::Chunked);
    let mut at = 0;
    while at < b.len() {
        let piece = &b[at..(at + size().max(1)).min(b.len())];
        let Ok(k) = body.take(piece) else { return "refused" };
        assert!(k <= piece.len());
        if body.done() {
            assert_eq!(body.take(b"more"), Ok(0), "nothing more once done");
            return "done";
        }
        assert_eq!(k, piece.len(), "short only once done");
        at += k;
    }
    "more"
}

/// Whatever a server sends, the parser answers a response, "more", or a refusal — never a panic, never a byte past
/// `RESPONSE_HEAD_MAX`: a response's head ends at its first blank line, its status is 100–599, a 1xx, 204 or 304 has
/// no body and any other one some framing; "more" only while no blank line is in the first `RESPONSE_HEAD_MAX` bytes.
pub(crate) fn response(b: &[u8]) -> &'static str {
    let blank = find(&b[..b.len().min(RESPONSE_HEAD_MAX)], b"\r\n\r\n");
    match super::response(b) {
        Ok(Some(r)) => {
            assert!(r.len <= RESPONSE_HEAD_MAX && Some(r.len - 4) == blank, "ends at its first blank line");
            assert!((100..=599).contains(&r.status) && r.fields.len() <= RESPONSE_FIELDS_MAX);
            let none = matches!(r.status, 100..=199 | 204 | 304);
            assert_eq!(none, r.body == Framing::Empty, "a body only where its status has one");
            "response"
        }
        Ok(None) => {
            assert!(blank.is_none() && b.len() < RESPONSE_HEAD_MAX, "more only while no blank line came");
            "more"
        }
        Err(Refusal::AnswerTooLong) => {
            assert!(
                blank.is_none() && b.len() >= RESPONSE_HEAD_MAX,
                "past its bytes only without a blank line"
            );
            "refused"
        }
        Err(_) => "refused",
    }
}

/// A chunked body read apart from the parser, as RFC 9112 §7.1 has it: its data, and its length to the trailers'
/// blank line — None where it isn't one whole.
pub(crate) fn dechunked(b: &[u8]) -> Option<(Vec<u8>, usize)> {
    let (mut out, mut at) = (Vec::new(), 0);
    loop {
        let eol = at + find(&b[at..], b"\r\n")?;
        let hex = b[at..eol].split(|&c| c == b';').next()?;
        let size = usize::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
        at = eol + 2;
        if size == 0 {
            loop {
                let end = at + find(&b[at..], b"\r\n")?;
                if end == at {
                    return Some((out, at + 2));
                }
                at = end + 2;
            }
        }
        out.extend_from_slice(b.get(at..at.checked_add(size)?)?);
        at += size;
        if b.get(at..at + 2)? != b"\r\n" {
            return None;
        }
        at += 2;
    }
}

/// A whole response, its body given `size()` bytes at a time to `Body::data`: whatever it holds, the data is what its
/// framing says — a length's first bytes, everything until the close, a chunked body's data as `dechunked` reads it
/// apart — and never more than the bytes taken. Read, refused, or "more" (the bytes ran out first).
pub(crate) fn exchange(b: &[u8], mut size: impl FnMut() -> usize) -> &'static str {
    let r = match super::response(b) {
        Ok(Some(r)) => r,
        Ok(None) => return "more",
        Err(_) => return "refused",
    };
    let rest = &b[r.len..];
    let mut body = Body::new(r.body);
    let (mut out, mut at) = (Vec::new(), 0);
    while at < rest.len() && !body.done() {
        let piece = &rest[at..(at + size().max(1)).min(rest.len())];
        let before = out.len();
        let Ok(k) = body.data(piece, &mut out) else { return "refused" };
        assert!(k <= piece.len() && out.len() - before <= k, "data only from what it took");
        assert!(k == piece.len() || body.done(), "short only once done");
        at += k;
    }
    match r.body {
        Framing::Empty => assert!(out.is_empty() && at == 0),
        Framing::Length(n) if body.done() => assert_eq!(out, &rest[..n as usize]),
        Framing::Close => assert_eq!(out, rest, "until the close: every byte"),
        Framing::Chunked if body.done() => assert_eq!(dechunked(rest), Some((out, at)), "as read apart"),
        _ => return "more",
    }
    "read"
}

/// `response` and `exchange` on a fuzz input: its first byte how many bytes at a time (1 + twice it), the response
/// after it.
pub(crate) fn response_input(b: &[u8]) -> &'static str {
    let Some((&n, rest)) = b.split_first() else { return "more" };
    response(rest);
    exchange(rest, || 1 + 2 * usize::from(n))
}

/// `body` on a fuzz input: its first byte how many bytes at a time (1 + twice it), the body after it.
pub(crate) fn body_input(b: &[u8]) -> &'static str {
    let Some((&n, rest)) = b.split_first() else { return "more" };
    body(rest, || 1 + 2 * usize::from(n))
}
