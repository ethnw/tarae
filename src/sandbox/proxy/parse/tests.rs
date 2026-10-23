use super::*;

/// The marker the captures carry as their proxy password (`target/sb/step5/cap.sh`).
const TOKEN: &[u8] = b"tarae-fake-token-5";

/// What clients send first through a proxy, captured from the clients themselves on this machine's loopback
/// (`target/sb/step5/cap.sh`, 2026-10-07: curl 8.7.1, git 2.54, Python 3.14's urllib, cargo 1.99, Go 1.27,
/// Node 26 with `NODE_USE_ENV_PROXY`, Java 25's `HttpsURLConnection`): the head and, through a tunnel, the
/// ClientHello. (name, bytes, the host it names, whether it sent the password)
const CAPTURED: [(&str, &[u8], &str, bool); 8] = [
    ("curl", include_bytes!("../captured/curl.raw"), "example.invalid", true),
    ("curl, plain http POST", include_bytes!("../captured/curl-http.raw"), "example.invalid", true),
    ("git", include_bytes!("../captured/git.raw"), "example.invalid", true),
    ("python", include_bytes!("../captured/python.raw"), "example.invalid", true),
    ("cargo", include_bytes!("../captured/cargo.raw"), "index.crates.io", true),
    ("go", include_bytes!("../captured/go.raw"), "example.invalid", true),
    ("node", include_bytes!("../captured/node.raw"), "example.invalid", true),
    // (the JDK sends Basic on a tunnel only after a 407, and only with `disabledSchemes` cleared — M2)
    ("java", include_bytes!("../captured/java.raw"), "example.invalid", false),
];

/// The captures' heads, and their tunnels' first bytes.
fn heads() -> Vec<&'static [u8]> {
    CAPTURED.iter().map(|(_, raw, ..)| &raw[..head(raw).unwrap().unwrap().len]).collect()
}

fn hellos() -> Vec<&'static [u8]> {
    let tunnels = CAPTURED.iter().filter(|(_, raw, ..)| raw.starts_with(b"CONNECT"));
    tunnels.map(|(_, raw, ..)| &raw[head(raw).unwrap().unwrap().len..]).collect()
}

/// A small fixed-seed generator: the loops are the same every run (the DNS parser's loop too — `dns/tests.rs`).
pub(in crate::sandbox::proxy) struct Rng(pub(in crate::sandbox::proxy) u64);

impl Rng {
    pub(in crate::sandbox::proxy) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub(in crate::sandbox::proxy) fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One to three of: a bit flipped, a byte set to one that means something to a parser, cut, extended, a
/// 2- or 3-byte field set to a length that matters, a slice copied in, a slice taken out — and now and then a
/// run of one byte put in, half to twice `cap` long, so inputs grow past the parser's limit wherever it is.
pub(in crate::sandbox::proxy) fn mutate(rng: &mut Rng, b: &mut Vec<u8>, cap: usize) {
    const BYTES: &[u8] = b" :\r\n[]%@./?#=0x\t\x00\xff\x16\x03";
    const LENGTHS: [u32; 7] = [0, 1, 0x4000, 0x4001, 0xffff, 0xff_ffff, 0x10000];
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(b.len());
        if rng.below(16) == 0 {
            let byte = if b.is_empty() || rng.below(2) == 0 { b'a' } else { b[at] };
            let run = cap / 2 + rng.below(cap * 3 / 2);
            b.splice(at..at, std::iter::repeat_n(byte, run));
            continue;
        }
        match rng.below(7) {
            0 if !b.is_empty() => b[at] ^= 1 << rng.below(8),
            1 if !b.is_empty() => b[at] = BYTES[rng.below(BYTES.len())],
            2 => b.truncate(rng.below(b.len() + 1)),
            3 => b.extend((0..1 + rng.below(16)).map(|_| rng.next() as u8)),
            4 if b.len() >= at + 3 => {
                let l = LENGTHS[rng.below(LENGTHS.len())].to_be_bytes();
                let w = 2 + rng.below(2);
                b[at..at + w].copy_from_slice(&l[4 - w..]);
            }
            5 if !b.is_empty() => {
                let end = (at + 1 + rng.below(64)).min(b.len());
                let piece = b[at..end].to_vec();
                let to = rng.below(b.len() + 1);
                b.splice(to..to, piece);
            }
            6 if !b.is_empty() => {
                let end = (at + 1 + rng.below(64)).min(b.len());
                b.drain(at..end);
            }
            _ => {}
        }
    }
}

/// Every client measured is read as it sent: its host and port, its password (the token), a plain request's
/// head as it goes on (origin form, the URI's `Host`, no `Proxy-*`, `Connection: close`, the body after it
/// untouched), each tunnel's server name — and every head and ClientHello cut short anywhere is "more", never
/// a refusal.
#[test]
fn every_client_measured_is_read_as_it_was_sent() {
    for (who, raw, host, password) in CAPTURED {
        let h = head(raw).unwrap().unwrap_or_else(|| panic!("{who}: a whole head"));
        let expected = match password {
            true => Auth::Basic { user: b"run".to_vec(), password: TOKEN.to_vec() },
            false => Auth::None,
        };
        assert_eq!(h.auth, expected, "{who}");
        for cut in 0..h.len {
            assert_eq!(head(&raw[..cut]), Ok(None), "{who}: cut at {cut}");
        }
        let rest = &raw[h.len..];
        match h.request {
            Request::Connect { host: Host::Name(n), port: 443 } => {
                assert_eq!(n, host, "{who}");
                let whole = (1..=rest.len()).find(|&n| server_name(&rest[..n]) != Ok(None));
                let whole = whole.unwrap_or_else(|| panic!("{who}: a whole ClientHello"));
                assert_eq!(server_name(&rest[..whole]), Ok(Some(host.to_string())), "{who}");
            }
            Request::Plain {
                method: "POST",
                host: Host::Name(n),
                port: 80,
                head: on,
                body: Framing::Length(3),
            } => {
                assert_eq!(n, host);
                let on = String::from_utf8(on).unwrap();
                assert!(on.starts_with("POST /form HTTP/1.1\r\nHost: example.invalid\r\n"), "{on}");
                assert!(on.ends_with("\r\nConnection: close\r\n\r\n") && !on.contains("Proxy-"), "{on}");
                assert!(on.contains("\r\nContent-Length: 3\r\n"), "{on}");
                assert_eq!(rest, b"x=1", "the body is the client's own bytes");
            }
            r => panic!("{who}: {r:?}"),
        }
    }
}

/// Whatever a head holds, the parser answers a request, "more", or a refusal as `check::head` has it. The captured
/// heads mutated 200,000 times, some grown past `HEAD_MAX` (a long run inside a line puts the blank line beyond it).
#[test]
fn the_connect_parser_never_panics() {
    let heads = heads();
    let mut rng = Rng(0x7a7a_e5ee_d1ce_0005);
    let (mut requests, mut more, mut refusals, mut past) = (0, 0, 0, 0);
    for _ in 0..200_000 {
        let mut b = heads[rng.below(heads.len())].to_vec();
        mutate(&mut rng, &mut b, HEAD_MAX);
        past += usize::from(b.len() > HEAD_MAX);
        match check::head(&b) {
            "request" => requests += 1,
            "more" => more += 1,
            _ => refusals += 1,
        }
    }
    assert!(requests > 1000 && more > 1000 && refusals > 10_000, "{requests} / {more} / {refusals}");
    assert!(past > 5000, "grown past the limit: {past}");
}

/// The handshake bytes of a captured ClientHello (from its records), and the same split into `k` records.
fn handshake(hello: &[u8]) -> Vec<u8> {
    let (mut hs, mut at) = (Vec::new(), 0);
    while let Some(h) = hello.get(at..at + 5).filter(|h| h[0] == 22) {
        let len = usize::from(u16::from_be_bytes([h[3], h[4]]));
        hs.extend_from_slice(&hello[at + 5..at + 5 + len]);
        at += 5 + len;
    }
    let n = usize::from(hs[1]) << 16 | usize::from(hs[2]) << 8 | usize::from(hs[3]);
    hs.truncate(4 + n);
    hs
}

fn in_records(hs: &[u8], k: usize) -> Vec<u8> {
    let size = hs.len().div_ceil(k);
    let mut out = Vec::new();
    for piece in hs.chunks(size) {
        out.extend_from_slice(&[22, 3, 1]);
        out.extend_from_slice(&(piece.len() as u16).to_be_bytes());
        out.extend_from_slice(piece);
    }
    out
}

/// The ClientHello `hs` (a handshake message) with a padding extension of `n` bytes after its others, every
/// length fixed: still a ClientHello as TLS writes one, `n` bytes longer.
fn padded(hs: &[u8], n: usize) -> Vec<u8> {
    let mut at = 4 + 2 + 32;
    at += 1 + usize::from(hs[at]);
    at += 2 + usize::from(u16::from_be_bytes([hs[at], hs[at + 1]]));
    at += 1 + usize::from(hs[at]);
    let exts = usize::from(u16::from_be_bytes([hs[at], hs[at + 1]])) + 4 + n;
    let mut out = hs.to_vec();
    out[at..at + 2].copy_from_slice(&(exts as u16).to_be_bytes());
    out.extend_from_slice(&[0, 21]);
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.resize(out.len() + n, 0);
    let len = (out.len() - 4) as u32;
    out[1..4].copy_from_slice(&len.to_be_bytes()[1..]);
    out
}

/// Whatever a tunnel's first bytes hold, the parser answers a name, "more", or a refusal as `check::sni` has it:
/// a name only from a ClientHello within its limits. The captured ClientHellos mutated 200,000 times, whole and split
/// into records, some padded (lengths fixed) up to twice `HELLO_MAX` in as many records as that takes and up
/// to two more than `RECORDS_MAX`; and a real one gives the same name in any number of records up to
/// `RECORDS_MAX`, past it a refusal.
#[test]
fn the_sni_parser_never_panics() {
    let hellos = hellos();
    let mut bases: Vec<Vec<u8>> = hellos.iter().map(|h| h.to_vec()).collect();
    for h in &hellos {
        let hs = handshake(h);
        for k in 1..=RECORDS_MAX {
            assert_eq!(server_name(&in_records(&hs, k)), server_name(h), "{k} records");
        }
        let past = in_records(&hs, RECORDS_MAX + 1);
        assert_eq!(server_name(&past), Err(HelloRefusal::TooLong));
        bases.push(in_records(&hs, 3));
    }
    let mut rng = Rng(0x5e17_cafe_0000_0005);
    let shakes: Vec<Vec<u8>> = hellos.iter().map(|h| handshake(h)).collect();
    let (mut names, mut more, mut refusals, mut past) = (0, 0, 0, 0);
    for _ in 0..200_000 {
        let mut b = if rng.below(8) == 0 {
            let hs = padded(&shakes[rng.below(shakes.len())], rng.below(2 * HELLO_MAX));
            let k = (1 + rng.below(RECORDS_MAX + 2)).max(hs.len().div_ceil(RECORD_MAX));
            past += usize::from(hs.len() - 4 > HELLO_MAX || k > RECORDS_MAX);
            in_records(&hs, k)
        } else {
            bases[rng.below(bases.len())].clone()
        };
        if rng.below(2) == 0 {
            mutate(&mut rng, &mut b, HELLO_MAX);
        }
        match check::sni(&b) {
            "name" => names += 1,
            "more" => more += 1,
            _ => refusals += 1,
        }
    }
    assert!(names > 1000 && more > 1000 && refusals > 10_000, "{names} / {more} / {refusals}");
    assert!(past > 5000, "padded past a limit: {past}");
}

fn connect(target: &str) -> Result<Option<Head>, Refusal> {
    head(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
}

fn connect_host(target: &str) -> Result<Host, Refusal> {
    match connect(target)?.map(|h| h.request) {
        Some(Request::Connect { host, .. }) => Ok(host),
        r => panic!("{r:?}"),
    }
}

/// Each limit holds at its edge: a head whose blank line ends at `HEAD_MAX` is taken, one byte more is refused,
/// and so is `HEAD_MAX` bytes with no end (one fewer is "more"); `HEADERS_MAX` header lines are taken, one more
/// refused; a 253-byte name and a 63-byte label are taken, one more refused; ports 1 and 65535 taken, 0, 65536,
/// a sign and none refused.
#[test]
fn a_head_over_its_limits_is_refused() {
    let at = |n: usize| {
        let start = b"CONNECT a.test:443 HTTP/1.1\r\nX: ";
        let mut b = start.to_vec();
        b.resize(n - 4, b'x');
        b.extend_from_slice(b"\r\n\r\n");
        b
    };
    assert!(matches!(head(&at(HEAD_MAX)), Ok(Some(h)) if h.len == HEAD_MAX));
    assert_eq!(head(&at(HEAD_MAX + 1)), Err(Refusal::TooLong));
    assert_eq!(head(&vec![b'a'; HEAD_MAX]), Err(Refusal::TooLong));
    assert_eq!(head(&vec![b'a'; HEAD_MAX - 1]), Ok(None));
    let with = |n: usize| {
        let lines: String = (0..n).map(|i| format!("X-{i}: y\r\n")).collect();
        head(format!("CONNECT a.test:443 HTTP/1.1\r\n{lines}\r\n").as_bytes())
    };
    assert!(matches!(with(HEADERS_MAX), Ok(Some(_))));
    assert_eq!(with(HEADERS_MAX + 1), Err(Refusal::TooLong));
    let long = |n: usize| format!("{}{}", "a.".repeat(100), "t".repeat(n - 200));
    assert_eq!(long(HOST_MAX).len(), HOST_MAX);
    assert!(connect_host(&format!("{}:443", long(HOST_MAX))).is_ok());
    assert!(matches!(connect(&format!("{}:443", long(HOST_MAX + 1))), Err(Refusal::Host(_))));
    let label = |n: usize| format!("{}.test:443", "a".repeat(n));
    assert!(connect_host(&label(63)).is_ok());
    assert!(matches!(connect(&label(64)), Err(Refusal::Host(_))));
    for ok in ["a.test:1", "a.test:65535"] {
        assert!(connect_host(ok).is_ok(), "{ok}");
    }
    for no in ["a.test:0", "a.test:65536", "a.test:99999", "a.test:+443", "a.test:", "a.test"] {
        assert!(matches!(connect(no), Err(Refusal::Host(_))), "{no}");
    }
}

/// A name is one DNS would look up, never an address in another form: what `inet_aton` (and so `getaddrinfo`)
/// reads as an address is refused as a name — `127.1`, hex, octal, a single number, a trailing dot on a dotted
/// quad; an address is only a dotted quad or an IPv6 one in brackets (no zone); a name comes lowercase without
/// its trailing dot.
#[test]
fn a_name_is_never_an_address_in_another_form() {
    for form in ["127.1", "0x7f.1", "0x7f000001", "2130706433", "017700000001", "127.0.0.1.", "1.2.3", "1e2"]
    {
        assert!(matches!(connect(&format!("{form}:443")), Err(Refusal::Host(_))), "{form}");
    }
    let v4 = connect_host("127.0.0.1:443").unwrap();
    assert_eq!(v4, Host::Addr(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert_eq!(connect_host("[::1]:443").unwrap(), Host::Addr(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    // (an IPv4 address written as IPv6 is that IPv4 address, so a rule on `127.0.0.1` meets it)
    assert_eq!(connect_host("[::ffff:127.0.0.1]:443").unwrap(), v4);
    let plain = plain_request("GET http://[::ffff:169.254.169.254]/ HTTP/1.1", "").unwrap().unwrap();
    let metadata = Host::Addr(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)));
    assert!(matches!(plain.request, Request::Plain { host, .. } if host == metadata));
    for no in ["::1:443", "[fe80::1%en0]:443", "[::1:443", "[::1]x:443"] {
        assert!(matches!(connect(no), Err(Refusal::Host(_))), "{no}");
    }
    assert_eq!(connect_host("Example.INVALID.:443").unwrap(), Host::Name("example.invalid".into()));
    for no in ["-a.test", "a-.test", "a..test", "ü.test", "a b.test", "a.test-"] {
        assert!(name(no).is_err(), "{no}");
    }
}

fn plain_request(line: &str, headers: &str) -> Result<Option<Head>, Refusal> {
    head(format!("{line}\r\n{headers}\r\n").as_bytes())
}

/// Plain http takes the standard methods only, in absolute form, each with its host read from the URI: the head
/// that goes on names the URI's host whatever the client's `Host` said, keeps its port when it isn't 80, and
/// loses the hop-by-hop headers and those `Connection` names; a body's framing goes on as the client sent it.
/// Another method is 405; origin form, https in absolute form, a user in the URI, a fragment are refused — and so
/// is any framing the proxy and the server could read two ways: both kinds, two lengths, a list, a length that
/// isn't digits, a coding other than `chunked`, and `Connection` naming the framing (the head would go on
/// without it, its body read as another request).
#[test]
fn plain_http_takes_the_standard_methods_only() {
    for m in METHODS {
        let h = plain_request(&format!("{m} http://a.test/x?y=1 HTTP/1.1"), "").unwrap().unwrap();
        assert!(matches!(h.request, Request::Plain { method, port: 80, .. } if method == m), "{m}");
    }
    for m in ["TRACE", "get", "PROPFIND"] {
        assert_eq!(plain_request(&format!("{m} http://a.test/ HTTP/1.1"), ""), Err(Refusal::Method), "{m}");
    }
    let h = plain_request(
        "GET http://a.test:8080?q HTTP/1.1",
        "Host: other.test\r\nConnection: keep-alive, X-Hop\r\nX-Hop: 1\r\nAccept: */*\r\nProxy-Authorization: Basic cnVuOng=\r\n",
    )
    .unwrap()
    .unwrap();
    let Request::Plain { host, port: 8080, head: on, .. } = h.request else { panic!("{:?}", h.request) };
    assert_eq!(host, Host::Name("a.test".into()));
    assert_eq!(
        String::from_utf8(on).unwrap(),
        "GET /?q HTTP/1.1\r\nHost: a.test:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(h.auth, Auth::Basic { user: b"run".to_vec(), password: b"x".to_vec() });
    for framing in ["Content-Length: 3", "Transfer-Encoding: chunked", "transfer-encoding: Chunked"] {
        let h = plain_request("POST http://a.test/ HTTP/1.1", &format!("{framing}\r\n")).unwrap().unwrap();
        let Request::Plain { head: on, .. } = h.request else { panic!() };
        let on = String::from_utf8(on).unwrap();
        assert!(on.contains(&format!("\r\n{framing}\r\n")), "{framing}: {on}");
    }
    let post = "POST http://a.test/ HTTP/1.1";
    let refused = [
        (post, "Content-Length: 3\r\nContent-Length: 5\r\n"),
        (post, "Content-Length: 3\r\nContent-Length: 3\r\n"),
        (post, "Content-Length: 3, 5\r\n"),
        (post, "Content-Length: x\r\n"),
        (post, "Content-Length: +3\r\n"),
        (post, "Content-Length: 12345678901234567890\r\n"),
        (post, "Transfer-Encoding: gzip\r\n"),
        (post, "Transfer-Encoding: gzip, chunked\r\n"),
        (post, "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n"),
        (post, "Content-Length: 3\r\nConnection: content-length\r\n"),
        (post, "Transfer-Encoding: chunked\r\nConnection: close, Transfer-Encoding\r\n"),
        ("GET /x HTTP/1.1", ""),
        ("GET https://a.test/ HTTP/1.1", ""),
        ("GET http://u:p@a.test/ HTTP/1.1", ""),
        ("GET http://a.test/#f HTTP/1.1", ""),
        ("POST http://a.test/ HTTP/1.1", "Content-Length: 3\r\nTransfer-Encoding: chunked\r\n"),
        ("GET http://a.test/ HTTP/2", ""),
        ("GET  http://a.test/ HTTP/1.1", ""),
        ("GET http://a.test/ HTTP/1.1", " folded: line\r\n"),
        ("GET http://a.test/ HTTP/1.1", "Bad Name: x\r\n"),
        ("GET http://a.test/ HTTP/1.1", "X: a\rb\r\n"),
    ];
    for (line, headers) in refused {
        assert!(matches!(plain_request(line, headers), Err(Refusal::Malformed(_))), "{line:?} {headers:?}");
    }
}

/// The client's bytes that are its body, `chunk` at a time.
fn taken(f: Framing, bytes: &[u8], chunk: usize) -> Result<(usize, bool), &'static str> {
    let mut b = Body::new(f);
    let mut n = 0;
    for piece in bytes.chunks(chunk.max(1)) {
        let k = b.take(piece)?;
        assert!(k <= piece.len());
        n += k;
        if k < piece.len() {
            break;
        }
    }
    Ok((n, b.done()))
}

/// A body ends where its framing says, however its bytes are split: none for no framing, `Content-Length`'s
/// count, a chunked body at its last chunk's trailers' blank line (extensions and trailers taken) — and what the
/// client sends after it (another request) is never the body's. A chunked framing it can't follow is an error.
#[test]
fn a_body_ends_where_its_framing_says() {
    let next = b"GET http://other.test/ HTTP/1.1\r\n\r\n";
    let chunked = b"3;ext=1\r\nx=1\r\nA\r\n0123456789\r\n0\r\nTrailer: t\r\n\r\n";
    let cases: [(Framing, &[u8]); 4] = [
        (Framing::Empty, b""),
        (Framing::Length(3), b"x=1"),
        (Framing::Chunked, chunked),
        (Framing::Chunked, b"0\r\n\r\n"),
    ];
    for (f, body) in cases {
        let all = [body, &next[..]].concat();
        for chunk in 1..=all.len() {
            assert_eq!(taken(f, &all, chunk), Ok((body.len(), true)), "{f:?} {chunk}");
        }
        if !body.is_empty() {
            assert_eq!(taken(f, &body[..body.len() - 1], 4), Ok((body.len() - 1, false)), "not done yet");
        }
    }
    let wrong: [&[u8]; 8] = [
        b"x\r\n",
        b"\r\n",
        b"3\nabc\r\n",
        b"3\r\nabcX",
        b"11111111111111111\r\n",
        b"3 \r\nabc\r\n",
        b"0\r\nTrailer: t\n\r\n",
        b";ext\r\n",
    ];
    for w in wrong {
        assert!(taken(Framing::Chunked, w, 64).is_err(), "{:?}", String::from_utf8_lossy(w));
    }
    let long_line = [&b"1;"[..], &vec![b'e'; CHUNK_LINE_MAX]].concat();
    assert!(taken(Framing::Chunked, &long_line, 64).is_err(), "a size line past its limit");
    let long_trailers = [&b"0\r\n"[..], &vec![b't'; HEAD_MAX + 1]].concat();
    assert!(taken(Framing::Chunked, &long_trailers, 64).is_err(), "trailers past their limit");
}

/// A chunked body: up to three chunks of random sizes, some with an extension, its last chunk with a trailer or
/// without.
fn chunked(rng: &mut Rng) -> Vec<u8> {
    let mut b = Vec::new();
    for _ in 0..rng.below(4) {
        let n = 1 + rng.below(300);
        b.extend_from_slice(format!("{n:x}").as_bytes());
        if rng.below(3) == 0 {
            b.extend_from_slice(b";name=value");
        }
        b.extend_from_slice(b"\r\n");
        b.extend((0..n).map(|_| rng.next() as u8));
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(if rng.below(2) == 0 { b"0\r\n\r\n" } else { b"0\r\nX-T: 1\r\n\r\n" });
    b
}

/// Whatever a chunked body's bytes hold, its parser takes at most what it's given, never panics, and once done
/// takes nothing more (`check::body`): real chunked bodies mutated 100,000 times, given 1–512 bytes at a time.
#[test]
fn the_body_parser_never_panics() {
    let mut rng = Rng(0xb0d1_e5ee_d1ce_0005);
    let (mut done, mut errors) = (0, 0);
    for _ in 0..100_000 {
        let mut b = chunked(&mut rng);
        mutate(&mut rng, &mut b, CHUNK_LINE_MAX);
        match check::body(&b, || 1 + rng.below(512)) {
            "done" => done += 1,
            "refused" => errors += 1,
            _ => {}
        }
    }
    assert!(done > 1000 && errors > 10_000, "{done} / {errors}");
}

/// A response's head is read to its blank line: its status, its fields, and how its body ends — none for a 1xx, 204
/// and 304, a length, chunked, or until the close where nothing says; a framing a reader could take two ways, a
/// status that isn't three digits or a version but 1.0 and 1.1 refused.
#[test]
fn a_response_is_read_to_its_blank_line() {
    let sent = b"HTTP/1.1 301 Moved Permanently\r\nLocation: /x\r\nContent-Length: 0\r\n\r\nnext";
    let r = response(sent).unwrap().unwrap();
    assert_eq!((r.status, r.field("location"), r.body), (301, Some("/x"), Framing::Length(0)));
    assert_eq!(&sent[r.len..], b"next");
    let framed = [
        (&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"[..], Framing::Chunked),
        (b"HTTP/1.0 200 OK\r\nContent-Type: text/html\r\n\r\n", Framing::Close),
        (b"HTTP/1.1 200\r\n\r\n", Framing::Close),
        (b"HTTP/1.1 204 No Content\r\nContent-Length: 9\r\n\r\n", Framing::Empty),
        (b"HTTP/1.1 304 Not Modified\r\n\r\n", Framing::Empty),
        (b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\n", Framing::Empty),
    ];
    for (head, body) in framed {
        assert_eq!(response(head).unwrap().unwrap().body, body, "{:?}", String::from_utf8_lossy(head));
    }
    assert_eq!(
        response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n"),
        Ok(None),
        "its blank line not yet come"
    );
    for head in [
        &b"HTTP/2 200\r\n\r\n"[..],
        b"HTTP/1.1 20 OK\r\n\r\n",
        b"HTTP/1.1 2000 OK\r\n\r\n",
        b"HTTP/1.1 700 What\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n folded\r\n\r\n",
    ] {
        assert!(response(head).is_err(), "{:?}", String::from_utf8_lossy(head));
    }
    // (its own limits, a response's: a request's 8 KB and 100 lines would refuse heads real servers send)
    let fields = |n: usize, pad: usize| {
        let one = format!("X-F: {}\r\n", "v".repeat(pad));
        format!("HTTP/1.1 200 OK\r\n{}\r\n", one.repeat(n)).into_bytes()
    };
    assert!(
        response(&fields(HEADERS_MAX + 1, 100)).unwrap().is_some(),
        "past a request's 100 lines and 8 KB"
    );
    assert!(response(&fields(RESPONSE_FIELDS_MAX, 200)).unwrap().is_some(), "its fields at their limit");
    assert_eq!(response(&fields(RESPONSE_FIELDS_MAX + 1, 1)), Err(Refusal::AnswerFields));
    assert_eq!(response(&vec![b'a'; RESPONSE_HEAD_MAX]), Err(Refusal::AnswerTooLong));
    assert_eq!(response(&vec![b'a'; RESPONSE_HEAD_MAX - 1]), Ok(None));
    assert_eq!(
        response(&fields(1, RESPONSE_HEAD_MAX)),
        Err(Refusal::AnswerTooLong),
        "one field past its bytes"
    );
    assert!(Refusal::AnswerTooLong.said().contains("64 KB") && Refusal::AnswerFields.said().contains("256"));
    let two = response(b"HTTP/1.1 200 OK\r\nX: 1\r\nx: 2\r\n\r\n").unwrap().unwrap();
    assert_eq!(two.field("x"), None, "a field sent twice is no one value");
}

/// `Body::data` gives a body's data without its framing — a length's bytes, a chunked body's chunks (extensions and
/// trailers left out), every byte until the close — however the bytes are split.
#[test]
fn a_body_s_data_is_read_without_its_framing() {
    let chunked = b"3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nX-T: 1\r\n\r\nNEXT";
    for chunk in [1, 2, 7, 100] {
        for (f, bytes, want) in [
            (Framing::Length(3), &b"abcdef"[..], &b"abc"[..]),
            (Framing::Chunked, chunked, b"abcde"),
            (Framing::Close, b"all of it", b"all of it"),
            (Framing::Empty, b"none", b""),
        ] {
            let (mut b, mut out) = (Body::new(f), Vec::new());
            for piece in bytes.chunks(chunk) {
                if b.data(piece, &mut out).unwrap() < piece.len() {
                    break;
                }
            }
            assert_eq!(out, want, "{f:?} {chunk} at a time");
            assert_eq!(b.done(), f != Framing::Close, "until the close: never done by itself");
        }
    }
}

/// A response: a status, a framing — a length, chunked, none (until the close) — and its body.
fn a_response(rng: &mut Rng) -> Vec<u8> {
    let status = [200, 301, 404, 204, 100, 500][rng.below(6)];
    let mut b = format!("HTTP/1.1 {status} Reason\r\nContent-Type: text/html\r\n").into_bytes();
    let data: Vec<u8> = (0..rng.below(200)).map(|_| rng.next() as u8).collect();
    match rng.below(3) {
        0 => {
            b.extend_from_slice(format!("Content-Length: {}\r\n\r\n", data.len()).as_bytes());
            b.extend_from_slice(&data);
        }
        1 => {
            b.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            b.extend_from_slice(&chunked(rng));
        }
        _ => {
            b.extend_from_slice(b"\r\n");
            b.extend_from_slice(&data);
        }
    }
    b
}

/// Whatever a server sends, the response parser answers as `check::response` has it, and a body's data is what its
/// framing says (`check::exchange`): responses mutated 100,000 times, their bodies given 1–512 bytes at a time.
#[test]
fn the_response_parser_never_panics() {
    let mut rng = Rng(0x7e5b_0e5e_d1ce_0007);
    let (mut read, mut refused) = (0, 0);
    for _ in 0..100_000 {
        let mut b = a_response(&mut rng);
        mutate(&mut rng, &mut b, RESPONSE_HEAD_MAX);
        check::response(&b);
        match check::exchange(&b, || 1 + rng.below(512)) {
            "read" => read += 1,
            "refused" => refused += 1,
            _ => {}
        }
    }
    assert!(read > 10_000 && refused > 5000, "{read} / {refused}");
}

/// The fuzz targets' seeds (`fuzz/parsers/seeds/`): `head` the captured heads, `sni` the captured ClientHellos whole and in three records, `body` chunked bodies (`check::body_input`'s first
/// byte: 1, 3, 255, 511 bytes at a time), `response` responses of each framing (`check::response_input`'s first byte
/// likewise).
#[test]
#[ignore = "writes fuzz/parsers/seeds: by hand"]
fn writes_fuzz_seeds() {
    use crate::mutate::fuzz_seeds;
    let client = |k: usize| CAPTURED[k].0.replace([' ', ','], "-");
    fuzz_seeds("head", heads().into_iter().enumerate().map(|(k, h)| (client(k), h.to_vec())));
    let tunnels: Vec<usize> =
        (0..CAPTURED.len()).filter(|&k| CAPTURED[k].1.starts_with(b"CONNECT")).collect();
    let hellos = hellos().into_iter().zip(&tunnels).flat_map(|(h, &k)| {
        [(client(k), h.to_vec()), (format!("{}-3-records", client(k)), in_records(&handshake(h), 3))]
    });
    fuzz_seeds("sni", hellos);
    let mut rng = Rng(0xb0d1_e5ee_d1ce_0006);
    let bodies: Vec<(String, Vec<u8>)> = [0u8, 1, 127, 255]
        .into_iter()
        .enumerate()
        .map(|(k, n)| (format!("chunked-{k}"), [&[n][..], &chunked(&mut rng)].concat()))
        .collect();
    assert!(bodies.iter().all(|(_, b)| check::body_input(b) == "done"), "each a whole body");
    fuzz_seeds("body", bodies);
    let responses: Vec<(String, Vec<u8>)> = (0..6)
        .map(|k| (format!("response-{k}"), [&[k as u8 * 40][..], &a_response(&mut rng)].concat()))
        .collect();
    assert!(responses.iter().all(|(_, b)| check::response_input(b) == "read"), "each a whole response");
    fuzz_seeds("response", responses);
}

/// Each refusal is answered with its status (too long 431, a method 405, the rest 400) and said in words that
/// name its limit or its reason.
#[test]
fn each_refusal_has_its_status_and_words() {
    let heads = [Refusal::TooLong, Refusal::Method, Refusal::Malformed("why"), Refusal::Host("why")];
    assert_eq!(heads.map(Refusal::status), [431, 405, 400, 400]);
    assert!(Refusal::TooLong.said().contains("8192") && Refusal::Method.said().contains("OPTIONS"));
    assert!(
        Refusal::Host("why").said().ends_with("why") && Refusal::Malformed("why").said().ends_with("why")
    );
    assert!(HelloRefusal::Ech.said().contains("ECH") && HelloRefusal::TooLong.said().contains("16384"));
    for r in
        [HelloRefusal::NotTls, HelloRefusal::NoName, HelloRefusal::Malformed("w"), HelloRefusal::Name("w")]
    {
        assert!(!r.said().is_empty());
    }
}

/// A credential is read only as Basic's `user:password`; anything else given — another scheme, bad base64, no
/// `:`, two of them — is unreadable (given, but wrong), and none is none.
#[test]
fn a_credential_is_basic_or_unreadable() {
    let auth = |headers: &str| plain_request("GET http://a.test/ HTTP/1.1", headers).unwrap().unwrap().auth;
    assert_eq!(auth(""), Auth::None);
    assert_eq!(
        auth("proxy-authorization: basic cnVuOnQ=\r\n"),
        Auth::Basic { user: b"run".to_vec(), password: b"t".to_vec() }
    );
    for bad in ["Bearer cnVuOnQ=", "Basic cnVuOnQ", "Basic cnVu", "Basic !!!!", "Basic"] {
        assert_eq!(auth(&format!("Proxy-Authorization: {bad}\r\n")), Auth::Unreadable, "{bad}");
    }
    let two = "Proxy-Authorization: Basic cnVuOnQ=\r\nProxy-Authorization: Basic cnVuOnQ=\r\n";
    assert_eq!(auth(two), Auth::Unreadable);
}

/// A ClientHello as TLS writes one, with these extensions, in one record.
fn hello_with(exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = vec![3, 3];
    body.extend_from_slice(&[0; 32]);
    body.extend_from_slice(&[0, 0, 2, 0x13, 0x01, 1, 0]);
    let mut e = Vec::new();
    for (kind, data) in exts {
        e.extend_from_slice(&kind.to_be_bytes());
        e.extend_from_slice(&(data.len() as u16).to_be_bytes());
        e.extend_from_slice(data);
    }
    body.extend_from_slice(&(e.len() as u16).to_be_bytes());
    body.extend_from_slice(&e);
    let mut hs = vec![1];
    hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend_from_slice(&body);
    in_records(&hs, 1)
}

/// A server_name extension naming these (type, name).
fn sni(names: &[(u8, &str)]) -> (u16, Vec<u8>) {
    let mut list = Vec::new();
    for (kind, n) in names {
        list.push(*kind);
        list.extend_from_slice(&(n.len() as u16).to_be_bytes());
        list.extend_from_slice(n.as_bytes());
    }
    let mut data = (list.len() as u16).to_be_bytes().to_vec();
    data.extend_from_slice(&list);
    (0, data)
}

/// Only a ClientHello whose one server name can be checked is taken: one name is its name (lowercase); no name,
/// an address, an invalid name, two names, an extension twice, an encrypted ClientHello (ECH), or a first record
/// that isn't a TLS handshake (ssh, plain text, application data) is refused — and a name is the CONNECT host's
/// only when they're the same name.
#[test]
fn only_a_checkable_server_name_is_taken() {
    assert_eq!(server_name(&hello_with(&[sni(&[(0, "A.Test")])])), Ok(Some("a.test".into())));
    let other_kinds = hello_with(&[sni(&[(7, "x"), (0, "a.test")])]);
    assert_eq!(server_name(&other_kinds), Ok(Some("a.test".into())), "a name of another type is skipped");
    assert_eq!(server_name(&hello_with(&[])), Err(HelloRefusal::NoName));
    assert_eq!(server_name(&hello_with(&[(10, vec![0, 2, 0, 29])])), Err(HelloRefusal::NoName));
    for n in ["127.0.0.1", "::1"] {
        assert!(matches!(server_name(&hello_with(&[sni(&[(0, n)])])), Err(HelloRefusal::Name(_))), "{n}");
    }
    assert!(matches!(server_name(&hello_with(&[sni(&[(0, "a..test")])])), Err(HelloRefusal::Name(_))));
    let two = hello_with(&[sni(&[(0, "a.test"), (0, "b.test")])]);
    assert!(matches!(server_name(&two), Err(HelloRefusal::Malformed(_))));
    let twice = hello_with(&[sni(&[(0, "a.test")]), sni(&[(0, "a.test")])]);
    assert!(matches!(server_name(&twice), Err(HelloRefusal::Malformed(_))));
    let ech = hello_with(&[sni(&[(0, "a.test")]), (0xfe0d, vec![0; 8])]);
    assert_eq!(server_name(&ech), Err(HelloRefusal::Ech));
    assert_eq!(server_name(b"SSH-2.0-OpenSSH_9.9\r\n"), Err(HelloRefusal::NotTls));
    assert_eq!(server_name(b"GET / HTTP/1.1\r\n"), Err(HelloRefusal::NotTls));
    let mut data = hello_with(&[sni(&[(0, "a.test")])]);
    data[0] = 23;
    assert_eq!(server_name(&data), Err(HelloRefusal::NotTls));
    let mut server = hello_with(&[sni(&[(0, "a.test")])]);
    server[5] = 2;
    assert!(matches!(server_name(&server), Err(HelloRefusal::Malformed(_))), "a ServerHello");
    let mut huge = hello_with(&[sni(&[(0, "a.test")])]);
    huge[6..9].copy_from_slice(&[0, 0x40, 0x01]);
    assert_eq!(server_name(&huge), Err(HelloRefusal::TooLong));
    let named = Host::Name("a.test".into());
    assert!(same_host(&named, "a.test") && !same_host(&named, "b.test"));
    assert!(!same_host(&Host::Addr(IpAddr::V4(Ipv4Addr::LOCALHOST)), "127.0.0.1"));
}

/// What tarae sends an upstream proxy as `Basic` is what a proxy reads back — every length's padding (none, one
/// `=`, two), every byte value — and RFC 4648's own vectors.
#[test]
fn base64_reads_back_as_it_was_written() {
    for (raw, b64) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
        assert_eq!(base64(raw.as_bytes()), b64, "{raw}");
    }
    let mut rng = Rng(0xba5e_64ee_d1ce_0005);
    for _ in 0..2000 {
        let b: Vec<u8> = (0..rng.below(40) + 1).map(|_| rng.next() as u8).collect();
        assert_eq!(unbase64(base64(&b).as_bytes()), Some(b.clone()), "{b:?}");
    }
}

/// A plain request's head in absolute form is the origin-form head with the scheme and its host put before the
/// path — nothing else changed: its `Host`, its headers and its end as they were (step 5g: what an upstream
/// proxy takes). A port other than 80 is kept; an IPv6 host in brackets.
#[test]
fn a_plain_head_in_absolute_form() {
    let raw = b"POST http://web.test:8080/f?q=1 HTTP/1.1\r\nHost: web.test:8080\r\nContent-Length: 3\r\n\r\n";
    let Some(Head { request: Request::Plain { method, host, port, head: origin, .. }, .. }) =
        head(raw).unwrap()
    else {
        panic!("a plain request")
    };
    let abs = String::from_utf8(absolute(method, &host, port, &origin)).unwrap();
    let origin = String::from_utf8(origin).unwrap();
    assert!(origin.starts_with("POST /f?q=1 HTTP/1.1\r\n"), "{origin}");
    assert_eq!(abs, origin.replacen("POST /f", "POST http://web.test:8080/f", 1));
    let v6 = absolute("GET", &Host::Addr("::1".parse().unwrap()), 80, b"GET / HTTP/1.1\r\n\r\n");
    assert_eq!(v6, b"GET http://[::1]/ HTTP/1.1\r\n\r\n");
}
