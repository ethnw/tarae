//! The fetch against servers of the test's own on loopback — TLS with a certificate made here (a CA and its leaf,
//! ECDSA P-256 by ring, laid out in DER by hand), never a real host — through a proxy of the test's own (its
//! resolver a stand-in, loopback taken for beyond this machine: `Egress::beyond`), and through a stand-in upstream
//! proxy. One test reaches the network, `#[ignore]`d, in CI's network job.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Mutex;
use std::sync::mpsc::Receiver;

use super::*;
use crate::sandbox::proxy::Limits;

const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

// --- A certificate of the test's own ---

fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len();
    let mut out = vec![tag];
    match n {
        0..128 => out.push(n as u8),
        128..256 => out.extend_from_slice(&[0x81, n as u8]),
        _ => out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend_from_slice(body);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

const EC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const P256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
const BASIC_CONSTRAINTS: &[u8] = &[0x06, 0x03, 0x55, 0x1d, 0x13];
const ALT_NAMES: &[u8] = &[0x06, 0x03, 0x55, 0x1d, 0x11];
const KEY_PURPOSES: &[u8] = &[0x06, 0x03, 0x55, 0x1d, 0x25];
const SERVER_AUTH: &[u8] = &[0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

fn name(cn: &str) -> Vec<u8> {
    seq(&[&tlv(0x31, &seq(&[COMMON_NAME, &tlv(0x0c, cn.as_bytes())]))])
}

fn extension(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let flag = if critical { tlv(0x01, &[0xff]) } else { Vec::new() };
    seq(&[oid, &flag, &tlv(0x04, value)])
}

/// A v3 certificate of `subject` (its public key `public`), issued by `issuer` and signed with `signer`.
fn certificate(
    serial: u8,
    subject: &str,
    public: &[u8],
    issuer: &str,
    signer: &ring::signature::EcdsaKeyPair,
    extensions: &[Vec<u8>],
) -> Vec<u8> {
    let spki = seq(&[&seq(&[EC_KEY, P256]), &tlv(0x03, &[&[0][..], public].concat())]);
    let validity = seq(&[&tlv(0x17, b"200101000000Z"), &tlv(0x17, b"491231235959Z")]);
    let tbs = seq(&[
        &tlv(0xa0, &tlv(0x02, &[2])),
        &tlv(0x02, &[serial]),
        &seq(&[ECDSA_SHA256]),
        &name(issuer),
        &validity,
        &name(subject),
        &spki,
        &tlv(0xa3, &seq(&extensions.iter().map(Vec::as_slice).collect::<Vec<_>>())),
    ]);
    let signature = signer.sign(&ring::rand::SystemRandom::new(), &tbs).unwrap();
    seq(&[&tbs, &seq(&[ECDSA_SHA256]), &tlv(0x03, &[&[0][..], signature.as_ref()].concat())])
}

struct Made {
    ca: Vec<u8>,
    leaf: Vec<u8>,
    key: Vec<u8>,
}

/// The test's CA, and its leaf for `page.test`, `other.test` and `localhost` with the leaf's PKCS#8 key — made once.
fn made() -> &'static Made {
    static MADE: OnceLock<Made> = OnceLock::new();
    MADE.get_or_init(|| {
        use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING as ALG, EcdsaKeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pair = || {
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ALG, &rng).unwrap();
            let key = EcdsaKeyPair::from_pkcs8(&ALG, pkcs8.as_ref(), &rng).unwrap();
            (key, pkcs8.as_ref().to_vec())
        };
        let ((ca_key, _), (leaf_key, key)) = (pair(), pair());
        let ca_ext = [extension(BASIC_CONSTRAINTS, true, &seq(&[&tlv(0x01, &[0xff])]))];
        let ca =
            certificate(1, "tarae test CA", ca_key.public_key().as_ref(), "tarae test CA", &ca_key, &ca_ext);
        let names: Vec<Vec<u8>> =
            ["page.test", "other.test", "localhost"].map(|n| tlv(0x82, n.as_bytes())).into();
        let leaf_ext = [
            extension(ALT_NAMES, false, &seq(&names.iter().map(Vec::as_slice).collect::<Vec<_>>())),
            extension(KEY_PURPOSES, false, &seq(&[SERVER_AUTH])),
        ];
        let leaf =
            certificate(2, "page.test", leaf_key.public_key().as_ref(), "tarae test CA", &ca_key, &leaf_ext);
        Made { ca, leaf, key }
    })
}

fn ring() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The client's TLS: as `tls()`'s, but trusting the test's CA alone.
fn client() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(made().ca.clone().into()).unwrap();
    let mut c = rustls::ClientConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(c)
}

fn server_tls() -> Arc<rustls::ServerConfig> {
    let m = made();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(m.key.clone().into());
    let c = rustls::ServerConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![m.leaf.clone().into()], key)
        .unwrap();
    Arc::new(c)
}

// --- Servers of the test's own ---

/// What a server answers a request with.
pub(crate) struct Reply {
    pub bytes: Vec<u8>,
    /// TLS's close_notify sent before it closes.
    pub close_notify: bool,
    /// Never answers: holds the connection until the client goes.
    pub hold: bool,
    /// TLS: its records written a byte at a time, this long apart.
    pub drip: Option<Duration>,
    /// Answers once this is sent something (or its sender goes).
    pub gate: Option<Arc<Mutex<Receiver<()>>>>,
}

pub(crate) fn reply(bytes: impl Into<Vec<u8>>) -> Reply {
    Reply { bytes: bytes.into(), close_notify: true, hold: false, drip: None, gate: None }
}

type Tls = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

/// A server on loopback, TLS or not, answering each request's head with `answer` — the heads it got sent on `seen`.
pub(crate) fn server(
    tls: bool,
    answer: impl Fn(&str) -> Reply + Send + Sync + 'static,
) -> (SocketAddr, Receiver<String>) {
    let l = TcpListener::bind((LO, 0)).unwrap();
    let at = l.local_addr().unwrap();
    let (seen, heads) = mpsc::channel();
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { return };
            let (answer, seen) = (answer.clone(), seen.clone());
            std::thread::spawn(move || match tls {
                true => {
                    let conn = rustls::ServerConnection::new(server_tls()).unwrap();
                    serve(rustls::StreamOwned::new(conn, s), &*answer, &seen, send_tls)
                }
                false => serve(s, &*answer, &seen, |s, r| {
                    let _ = s.write_all(&r.bytes);
                }),
            });
        }
    });
    (at, heads)
}

/// A reply over TLS: whole, close_notify after it if it says so; or its records a byte at a time.
fn send_tls(t: &mut Tls, r: &Reply) {
    let Some(every) = r.drip else {
        let _ = t.write_all(&r.bytes).and_then(|()| t.flush());
        if r.close_notify {
            t.conn.send_close_notify();
            let _ = t.flush();
        }
        return;
    };
    let _ = t.conn.writer().write_all(&r.bytes);
    let mut records = Vec::new();
    while t.conn.wants_write() && t.conn.write_tls(&mut records).is_ok() {}
    for b in records {
        if t.sock.write_all(&[b]).is_err() {
            return;
        }
        crate::event::sleep(every);
    }
}

fn serve<S: Read + Write>(
    mut s: S,
    answer: &dyn Fn(&str) -> Reply,
    seen: &mpsc::Sender<String>,
    send: impl Fn(&mut S, &Reply),
) {
    let (mut head, mut b) = (Vec::new(), [0u8; 1]);
    while !head.ends_with(b"\r\n\r\n") {
        match s.read(&mut b) {
            Ok(1) => head.push(b[0]),
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let r = answer(&head);
    let _ = seen.send(head);
    if r.hold {
        let _ = s.read(&mut b);
        return;
    }
    if let Some(gate) = &r.gate {
        let _ = gate.lock().unwrap_or_else(|e| e.into_inner()).recv();
    }
    send(&mut s, &r);
}

/// The request's path, from its head.
pub(crate) fn path(head: &str) -> &str {
    head.split(' ').nth(1).unwrap_or_default()
}

pub(crate) fn ok(body: &str) -> Reply {
    reply(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ))
}

/// The rules of yours (any host, the address rules held), the test's loopback taken for beyond this machine.
pub(crate) fn yours_rules() -> Egress {
    Egress { any: true, own_addresses: Some(Vec::new()), beyond: vec![LO], ..Egress::default() }
}

/// A way through a proxy of the test's own whose stand-in resolver sends `page.test`, `other.test` and `localhost`
/// to `at`.
pub(crate) fn way(at: SocketAddr, rules: Egress) -> Way {
    let resolve: Resolve = Arc::new(move |host: &str, _| match host {
        "page.test" | "other.test" | "localhost" => Ok(vec![at]),
        _ => Err(io::Error::new(io::ErrorKind::NotFound, "no such name")),
    });
    let (asks, _) = mpsc::channel();
    Way {
        proxy: Proxy::start(resolve.clone(), asks, Limits::default()),
        session: 7,
        rules: Arc::new(rules),
        resolve,
        tls: client(),
        deadline: Duration::from_secs(10),
        max: 1 << 20,
        process: ":web".into(),
        others: true,
        stop: Stop::default(),
    }
}

fn fetch(url: &str, way: &Way) -> Result<Fetched, String> {
    get_by(url, way, Instant::now() + way.deadline, 0)
}

// --- The tests ---

/// A page comes through tarae's proxy as its server sent it — its status, type and body — and the request asks for it
/// alone: its path and query, its host, nothing of the user's.
#[test]
fn a_page_comes_through_the_proxy_as_it_was_sent() {
    let (at, heads) = server(true, |_| ok("<p>hello</p>"));
    let f = fetch("https://page.test/a b?q=1#frag", &way(at, yours_rules())).unwrap();
    assert_eq!(
        (f.status, f.content_type.as_deref(), &f.bytes[..]),
        (200, Some("text/html"), &b"<p>hello</p>"[..])
    );
    assert_eq!((f.url.as_str(), f.hops, f.from.len()), ("https://page.test/a%20b?q=1", 0, 0));
    let head = heads.recv().unwrap();
    assert!(head.starts_with("GET /a%20b?q=1 HTTP/1.1\r\nHost: page.test\r\n"), "{head}");
    for never in ["cookie", "authorization", "referer", "frag"] {
        assert!(!head.to_ascii_lowercase().contains(never), "{never}: {head}");
    }
}

/// A chunked body gzipped comes unpacked; the answer after interim heads (a 103, a 100) is read, they go; a
/// close-delimited one is whole with TLS's close_notify and said to be maybe cut without it.
#[test]
fn a_body_comes_as_its_framing_and_coding_say() {
    let page = "<p>packed</p>".repeat(500);
    let packed = crate::web::gzip::tests::gzip(page.as_bytes(), 0);
    let (at, _) = server(true, move |head| match path(head) {
        "/gzip" => {
            let mut b =
                b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            for c in packed.chunks(700) {
                b.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
                b.extend_from_slice(c);
                b.extend_from_slice(b"\r\n");
            }
            b.extend_from_slice(b"0\r\n\r\n");
            reply(b)
        }
        "/close" => reply("HTTP/1.1 200 OK\r\n\r\nto the close"),
        "/early" => reply(
            "HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\nHTTP/1.1 100 Continue\r\n\r\n\
             HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nat last",
        ),
        _ => Reply { close_notify: false, ..reply("HTTP/1.1 200 OK\r\n\r\nto the close") },
    });
    let w = way(at, yours_rules());
    assert_eq!(fetch("https://page.test/gzip", &w).unwrap().bytes, page.as_bytes());
    // (interim heads go, the answer after them read)
    let early = fetch("https://page.test/early", &w).unwrap();
    assert_eq!((early.status, &early.bytes[..]), (200, &b"at last"[..]));
    let whole = fetch("https://page.test/close", &w).unwrap();
    assert_eq!((&whole.bytes[..], whole.said), (&b"to the close"[..], None));
    let cut = fetch("https://page.test/cut", &w).unwrap();
    assert_eq!(cut.bytes, b"to the close");
    assert!(cut.said.is_some_and(|s| s.contains("may be cut short")));
}

/// Redirects are followed — at most five, another host's said —, a `<meta refresh>` within 5 s as one; never from
/// beyond this machine into it.
#[test]
fn redirects_are_followed_and_another_host_said() {
    let (at, _) = server(true, |head| {
        let host = head.lines().find_map(|l| l.strip_prefix("Host: ")).unwrap_or_default().to_string();
        let moved =
            |to: &str| reply(format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n"));
        match (host.as_str(), path(head)) {
            ("page.test", "/a") => moved("/b"),
            ("page.test", "/b") => moved("https://other.test/c"),
            ("other.test", "/c") => ok("<p>there</p>"),
            (_, "/loop") => moved("/loop"),
            (_, "/home") => moved("http://localhost/"),
            (_, "/meta") => ok(r#"<meta http-equiv="refresh" content="0;url=/fresh"><p>old</p>"#),
            (_, "/fresh") => ok("<p>fresh</p>"),
            _ => ok("<p>?</p>"),
        }
    });
    let w = way(at, yours_rules());
    let f = fetch("https://page.test/a", &w).unwrap();
    assert_eq!(
        (f.url.as_str(), f.hops, &f.from[..]),
        ("https://other.test/c", 2, &["page.test".to_string()][..])
    );
    assert_eq!(f.asked, "https://page.test/a");
    let e = fetch("https://page.test/loop", &w).unwrap_err();
    assert!(e.contains("more than 5 redirects"), "{e}");
    let e = fetch("https://page.test/home", &w).unwrap_err();
    assert!(e.contains("to this machine isn't followed"), "{e}");
    let (f, page) = super::page("https://page.test/meta", &w, crate::web::Prefer::Guess).unwrap();
    assert_eq!(
        (f.url.as_str(), f.hops, f.asked.as_str()),
        ("https://page.test/fresh", 1, "https://page.test/meta")
    );
    assert!(page.text.contains("fresh"), "{}", page.text);
}

/// A body past the limit is cut at it and one byte, on the wire or unpacked (a bomb); a coding but gzip is said, not
/// read; an answer that never comes ends at the deadline; a stop ends a fetch under way.
#[test]
fn limits_deadlines_and_a_stop_end_a_fetch() {
    let bomb = crate::web::gzip::tests::gzip(&vec![b'a'; 4 << 20], 0);
    let (at, heads) = server(true, move |head| match path(head) {
        "/big" => ok(&"x".repeat(5000)),
        "/bomb" => {
            let h = format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                bomb.len()
            );
            reply([h.as_bytes(), &bomb].concat())
        }
        "/br" => reply("HTTP/1.1 200 OK\r\nContent-Encoding: br\r\nContent-Length: 1\r\n\r\nx"),
        _ => Reply { hold: true, ..reply("") },
    });
    let mut w = way(at, yours_rules());
    w.max = 1000;
    assert_eq!(fetch("https://page.test/big", &w).unwrap().bytes.len(), 1001);
    assert_eq!(fetch("https://page.test/bomb", &w).unwrap().bytes.len(), 1001);
    let e = fetch("https://page.test/br", &w).unwrap_err();
    assert!(e.contains("compressed as br"), "{e}");
    w.deadline = Duration::from_millis(400);
    let e = fetch("https://page.test/never", &w).unwrap_err();
    assert!(e.contains("no whole answer within"), "{e}");
    w.deadline = Duration::from_secs(10);
    while heads.try_recv().is_ok() {}
    let stop = w.stop.clone();
    let fetching = std::thread::spawn(move || fetch("https://page.test/never", &w));
    heads.recv_timeout(Duration::from_secs(5)).expect("the request came");
    let asked = Instant::now();
    stop.stop();
    assert_eq!(fetching.join().unwrap().unwrap_err(), "https://page.test/never: stopped");
    assert!(asked.elapsed() < Duration::from_secs(2), "stopped at once");
}

/// One budget of five hops for a fetch, HTTP redirects and a `<meta refresh>` alike: a refresh as the fifth followed, as
/// the sixth left — the page as read, said —, a redirect after a refresh counted on; a refresh to an address the fetch
/// refuses leaves the page as read, said, never failing it.
#[test]
fn redirects_and_refreshes_share_one_budget() {
    let (at, _) = server(true, |head| {
        let moved =
            |to: &str| reply(format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n"));
        let refresh =
            |to: &str| ok(&format!(r#"<meta http-equiv="refresh" content="0;url={to}"><p>old</p>"#));
        let p = path(head);
        match p.strip_prefix("/chain/").and_then(|r| r.split_once('/')) {
            Some((n, end)) if n != "0" => {
                moved(&format!("/chain/{}/{end}", n.parse::<usize>().unwrap_or(1) - 1))
            }
            Some((_, "meta")) => refresh("/fresh"),
            Some((_, "metamove")) => refresh("/moved"),
            Some((_, "badmeta")) => refresh("https://u:p@page.test/"),
            _ if p == "/moved" => moved("/fresh"),
            _ => ok("<p>fresh</p>"),
        }
    });
    let w = way(at, yours_rules());
    let page = |url: &str| super::page(url, &w, crate::web::Prefer::Guess);
    let (f, _) = page("https://page.test/chain/4/meta").unwrap();
    assert_eq!(
        (f.url.as_str(), f.hops, f.said),
        ("https://page.test/fresh", 5, None),
        "the refresh the fifth"
    );
    let (f, read) = page("https://page.test/chain/5/meta").unwrap();
    assert_eq!((f.url.as_str(), f.hops), ("https://page.test/chain/0/meta", 5));
    let said = f.said.unwrap_or_default();
    assert!(
        said.contains("refresh to https://page.test/fresh isn't followed: more than 5 redirects"),
        "{said}"
    );
    assert!(read.text.contains("old"), "the page as read");
    let e = page("https://page.test/chain/4/metamove").unwrap_err();
    assert!(e.contains("more than 5 redirects"), "a redirect after the refresh, the sixth: {e}");
    let (f, _) = page("https://page.test/chain/0/badmeta").unwrap();
    let said = f.said.unwrap_or_default();
    assert!(said.contains("isn't followed") && said.contains("credentials"), "{said}");
}

/// The deadline holds over TLS whatever the server does: one dripping its records a byte every 300 ms — each socket
/// read inside rustls getting bytes, none a whole record — ends at the deadline, not when the drip does (30 s).
#[test]
fn a_dripping_server_ends_at_the_deadline() {
    let (at, _) = server(true, |_| Reply { drip: Some(Duration::from_millis(300)), ..ok("<p>slowly</p>") });
    let mut w = way(at, yours_rules());
    w.deadline = Duration::from_secs(1);
    let started = Instant::now();
    let e = fetch("https://page.test/", &w).unwrap_err();
    assert!(e.contains("no whole answer within"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(3), "ended at the deadline: {:?}", started.elapsed());
}

/// What the proxy refuses a run it refuses a fetch: this machine through it (without the test's stand-in for the
/// outside), a metadata server's name — in its words.
#[test]
fn the_proxy_refuses_a_fetch_what_it_refuses_a_run() {
    let (at, _) = server(true, |_| ok("never"));
    let rules = Egress { any: true, own_addresses: Some(Vec::new()), ..Egress::default() };
    let e = fetch("https://page.test/", &way(at, rules)).unwrap_err();
    assert!(e.contains("refused by tarae's proxy") && e.contains("this machine"), "{e}");
    let e = fetch("https://metadata.google.internal/", &way(at, yours_rules())).unwrap_err();
    assert!(e.contains("refused by tarae's proxy") && e.contains("metadata"), "{e}");
}

/// This machine is reached directly, as runs reach it: plain http there, and https with its certificate.
#[test]
fn this_machine_is_reached_directly() {
    let (plain, _) = server(false, |_| ok("<p>dev server</p>"));
    let rules = Egress::default();
    let f = fetch(&format!("http://localhost:{}/docs", plain.port()), &way(plain, rules.clone())).unwrap();
    assert_eq!((f.bytes, f.upgraded), (b"<p>dev server</p>".to_vec(), false));
    let (tls, _) = server(true, |_| ok("<p>local tls</p>"));
    let f = fetch(&format!("https://localhost:{}/", tls.port()), &way(tls, rules)).unwrap();
    assert_eq!(f.bytes, b"<p>local tls</p>");
}

/// Where tarae goes out through an upstream proxy, a fetch does too: the stand-in upstream asked for the host by name.
#[test]
fn the_upstream_proxy_carries_a_fetch() {
    let (at, _) = server(true, |_| ok("<p>upstream</p>"));
    let l = TcpListener::bind((LO, 0)).unwrap();
    let up = l.local_addr().unwrap();
    let (asked_tx, asked) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut c, _)) = l.accept() else { return };
        let (mut head, mut b) = (Vec::new(), [0u8; 1]);
        while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).is_ok_and(|n| n == 1) {
            head.push(b[0]);
        }
        let _ = asked_tx.send(String::from_utf8_lossy(&head).into_owned());
        let Ok(s) = TcpStream::connect(at) else { return };
        let _ = c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
        let (mut c2, mut s2) = (c.try_clone().unwrap(), s.try_clone().unwrap());
        std::thread::spawn(move || io::copy(&mut c2, &mut s2));
        let _ = io::copy(&mut &s, &mut c);
    });
    let env: HashMap<String, std::ffi::OsString> =
        [("HTTPS_PROXY".to_string(), format!("http://{up}").into())].into();
    let rules =
        Egress { upstream: proxy::upstream::of(&env, proxy::upstream::Setting::Env), ..yours_rules() };
    let f = fetch("https://page.test/", &way(at, rules)).unwrap();
    assert_eq!(f.bytes, b"<p>upstream</p>");
    assert!(asked.recv().unwrap().starts_with("CONNECT page.test:443 HTTP/1.1\r\n"));
}

/// The session keeps a page by the address asked and the one it came from, for `KEEP`; the same page again takes the
/// older's place; past the count the oldest goes.
#[test]
fn the_session_keeps_its_pages_a_while() {
    let page = |asked: &str, url: &str| {
        let f = Fetched {
            asked: asked.into(),
            url: url.into(),
            status: 200,
            content_type: None,
            bytes: Vec::new(),
            from: Vec::new(),
            hops: 0,
            upgraded: false,
            said: None,
            cut: false,
            took: Duration::ZERO,
            elsewhere: None,
        };
        Arc::new(f)
    };
    let (mut pages, t0) = (Pages::default(), Instant::now());
    assert!(pages.put(page("http://a.test", "https://a.test/"), t0).is_empty());
    for url in ["http://a.test", "https://a.test/"] {
        assert!(pages.get(url, t0 + Duration::from_secs(60)).is_some(), "{url}");
    }
    assert!(pages.get("https://a.test/", t0 + KEEP).is_none(), "kept for KEEP");
    assert_eq!(
        pages.put(page("https://a.test/", "https://a.test/"), t0).len(),
        1,
        "the same page's older copy goes"
    );
    let more = |k: usize| format!("https://{k}.test/");
    let gone: usize = (0..PAGES_MAX).map(|k| pages.put(page(&more(k), &more(k)), t0).len()).sum();
    assert_eq!((gone, pages.0.len()), (1, PAGES_MAX));
}

/// A real page, through tarae's proxy (and the upstream proxy the environment names, as `:web`'s) and verified by the
/// system's certificates (the network: CI's network job, with `TARAE_MEASURE_REACHES_NETWORK` — the proxy's tests
/// never dial past this machine without it).
#[test]
#[ignore = "network: fetches https://example.com/ through tarae's proxy"]
fn a_real_page_is_fetched() {
    assert!(
        std::env::var_os("TARAE_MEASURE_REACHES_NETWORK").is_some(),
        "run with TARAE_MEASURE_REACHES_NETWORK=1: a test's proxy dials past this machine only then"
    );
    // (as `upstream_now` reads it outside tests — in tests `Here` leaves the proxy variables out)
    let env: HashMap<String, std::ffi::OsString> =
        std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))).collect();
    let upstream = proxy::upstream::of(&env, proxy::upstream::Setting::Env);
    let way = yours(std::path::Path::new(env!("CARGO_MANIFEST_DIR")), upstream, Stop::default()).unwrap();
    let (f, page) = super::page("http://example.com/", &way, crate::web::Prefer::Guess).unwrap();
    assert_eq!((f.status, f.upgraded, f.url.as_str()), (200, true, "https://example.com/"));
    assert!(page.title.contains("Example Domain"), "{}", page.title);
}
