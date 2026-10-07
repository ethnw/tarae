//! The proxy against a stand-in supervisor that only writes messages, the test's own listeners on loopback as
//! the outside (`Egress::beyond`), and a stand-in resolver. Nothing reaches past this machine — `egress::connect`
//! holds back any address that isn't loopback in tests, so a broken check shows as a failed connect.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::egress::Pattern;
use super::*;

const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// How long a test waits for what it expects; the proxy's own limits are short (`limits`).
const WAIT: Duration = Duration::from_secs(5);

fn limits() -> Limits {
    Limits {
        head_time: Duration::from_millis(300),
        hello_time: Duration::from_millis(300),
        hold: WAIT,
        connect: Duration::from_secs(2),
        per_session: 256,
        total: 1024,
    }
}

/// A captured client's first ClientHello (`parse`'s fixtures): curl's names `example.invalid`, cargo's
/// `index.crates.io`.
fn captured_hello(raw: &'static [u8]) -> &'static [u8] {
    let h = parse::head(raw).unwrap().unwrap();
    let rest = &raw[h.len..];
    let whole = (1..=rest.len()).find(|&n| parse::server_name(&rest[..n]) != Ok(None)).unwrap();
    &rest[..whole]
}

fn curl_hello() -> &'static [u8] {
    captured_hello(include_bytes!("captured/curl.raw"))
}

fn cargo_hello() -> &'static [u8] {
    captured_hello(include_bytes!("captured/cargo.raw"))
}

/// The test's outside: a listener on loopback, how many connections it took, and what each sent (once it ended).
struct Outside {
    at: SocketAddr,
    accepted: Arc<AtomicUsize>,
    got: Receiver<Vec<u8>>,
}

fn outside(serve: fn(TcpStream) -> Vec<u8>) -> Outside {
    let l = TcpListener::bind((LO, 0)).unwrap();
    let at = l.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let (tx, got) = mpsc::channel();
    let n = accepted.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            n.fetch_add(1, Ordering::SeqCst);
            let tx = tx.clone();
            std::thread::spawn(move || tx.send(serve(s)));
        }
    });
    Outside { at, accepted, got }
}

/// Echoes what comes; what came.
fn echo(mut s: TcpStream) -> Vec<u8> {
    let (mut got, mut b) = (Vec::new(), [0u8; 4096]);
    while let Ok(n @ 1..) = s.read(&mut b) {
        got.extend_from_slice(&b[..n]);
        if s.write_all(&b[..n]).is_err() {
            break;
        }
    }
    got
}

/// A ServerHello's first handshake record with this random (a HelloRetryRequest's, or another).
fn server_hello(random: &[u8; 32]) -> Vec<u8> {
    let mut body = vec![3, 3];
    body.extend_from_slice(random);
    body.extend_from_slice(&[0, 0x13, 0x01, 0, 0, 0]);
    let mut hs = vec![2, 0, 0, body.len() as u8];
    hs.extend_from_slice(&body);
    let mut rec = vec![22, 3, 3, 0, hs.len() as u8];
    rec.extend_from_slice(&hs);
    rec
}

const HRR: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91, 0xc2,
    0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// Reads a first ClientHello, answers `answer`, and gives back what came after it.
fn after_answer(mut s: TcpStream, answer: &[u8]) -> Vec<u8> {
    let (mut first, mut b) = (Vec::new(), [0u8; 4096]);
    while !matches!(parse::server_name(&first), Ok(Some(_))) {
        match s.read(&mut b) {
            Ok(n @ 1..) => first.extend_from_slice(&b[..n]),
            _ => return Vec::new(),
        }
    }
    let _ = s.write_all(answer);
    let mut after = Vec::new();
    while let Ok(n @ 1..) = s.read(&mut b) {
        after.extend_from_slice(&b[..n]);
    }
    after
}

fn retrying_server(s: TcpStream) -> Vec<u8> {
    after_answer(s, &server_hello(&HRR))
}

fn plain_server(s: TcpStream) -> Vec<u8> {
    after_answer(s, &server_hello(&[7; 32]))
}

/// An http server that answers 200 at once, reads everything that comes until its client's side ends, and gives
/// it back: what reached the checked address.
fn listening_web(mut s: TcpStream) -> Vec<u8> {
    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let mut got = Vec::new();
    let _ = s.read_to_end(&mut got);
    got
}

/// An http server: reads a head and its `Content-Length` body, answers 200, gives back what came.
fn web(mut s: TcpStream) -> Vec<u8> {
    let (mut got, mut b) = (Vec::new(), [0u8; 4096]);
    let whole = |g: &[u8]| {
        let end = g.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&g[..end]).to_ascii_lowercase();
        let len = head.lines().find_map(|l| l.strip_prefix("content-length: ")?.trim().parse().ok());
        (g.len() >= end + 4 + len.unwrap_or(0)).then_some(())
    };
    while whole(&got).is_none() {
        match s.read(&mut b) {
            Ok(n @ 1..) => got.extend_from_slice(&b[..n]),
            _ => return got,
        }
    }
    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    got
}

/// A stand-in resolver: each name's answers, the first list at the first lookup, the next at the next (the last
/// one after) — and how many lookups it took.
fn resolver(names: Vec<(&'static str, Vec<Vec<SocketAddr>>)>) -> (Resolve, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let n = calls.clone();
    let resolve: Resolve = Arc::new(move |host: &str, _port| {
        n.fetch_add(1, Ordering::SeqCst);
        let (_, answers) = names
            .iter()
            .find(|(name, _)| *name == host)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such name"))?;
        let mut seen = seen.lock().unwrap();
        let i = seen.entry(host.to_string()).or_default();
        let answer = answers[(*i).min(answers.len() - 1)].clone();
        *i += 1;
        Ok(answer)
    });
    (resolve, calls)
}

struct Fixture {
    proxy: Proxy,
    asks: Receiver<FromProxy>,
}

fn start(resolve: Resolve, limits: Limits) -> Fixture {
    let (tx, asks) = mpsc::channel();
    Fixture { proxy: Proxy::start(resolve, tx, limits), asks }
}

/// Rules naming these hosts, the test's loopback taken for beyond this machine, no own address.
fn rules(hosts: &[&str]) -> Egress {
    Egress {
        hosts: hosts.iter().map(|h| Pattern::parse(h).unwrap()).collect(),
        own_addresses: Some(Vec::new()),
        beyond: vec![LO],
        ..Egress::default()
    }
}

/// What the supervisor does for a run's connection, stood in: a TCP pair, one end to the proxy in a message
/// with these rules — the run's end back, and the sender its refusals come on.
fn connection(p: &Proxy, session: u64, run: u64, rules: Egress) -> (TcpStream, Receiver<Denial>) {
    let l = TcpListener::bind((LO, 0)).unwrap();
    let run_end = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (ours, _) = l.accept().unwrap();
    run_end.set_read_timeout(Some(WAIT)).unwrap();
    let (told, said) = mpsc::channel();
    let c =
        Connection { fd: ours.into(), run, session, process: "curl".into(), rules: Arc::new(rules), told };
    assert!(p.send(ToProxy::Connection(c)));
    (run_end, said)
}

/// An answer's head, read to its blank line.
fn head_of(c: &mut TcpStream) -> String {
    let (mut h, mut b) = (Vec::new(), [0u8; 1]);
    while !h.ends_with(b"\r\n\r\n") {
        match c.read(&mut b) {
            Ok(1) => h.push(b[0]),
            r => panic!("the answer's head cut at {:?}: {r:?}", String::from_utf8_lossy(&h)),
        }
    }
    String::from_utf8(h).unwrap()
}

fn status(c: &mut TcpStream) -> u16 {
    head_of(c)[9..12].parse().unwrap()
}

fn ask(c: &mut TcpStream, head: &str) -> u16 {
    c.write_all(head.as_bytes()).unwrap();
    status(c)
}

fn connect_head(host: &str) -> String {
    format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n\r\n")
}

/// The client's end read to its close (nothing more coming): what came.
fn to_close(c: &mut TcpStream) -> Vec<u8> {
    let mut got = Vec::new();
    c.read_to_end(&mut got).expect("closed within the wait");
    got
}

fn said(d: &Receiver<Denial>) -> String {
    let d = d.recv_timeout(WAIT).expect("a refusal said");
    assert_eq!(d.op, OP);
    d.target
}

/// A tunnel to `host` through `p`, the curl ClientHello sent and echoed back: an established one.
fn tunnel(p: &Proxy, session: u64, run: u64, rules: Egress, host: &str) -> (TcpStream, Receiver<Denial>) {
    let (mut c, d) = connection(p, session, run, rules);
    assert_eq!(ask(&mut c, &connect_head(host)), 200);
    c.write_all(curl_hello()).unwrap();
    let mut back = vec![0u8; curl_hello().len()];
    c.read_exact(&mut back).unwrap();
    assert_eq!(back, curl_hello(), "the outside's echo comes back through");
    (c, d)
}

/// A tunnel goes where its name was looked up — once — and checked: the connection lands on the first answer's
/// address, never on what a second lookup would say; it carries both ways, the ClientHello first, as it came.
#[test]
fn it_connects_to_the_address_it_checked() {
    let (good, trap) = (outside(echo), outside(echo));
    let (resolve, lookups) = resolver(vec![("example.invalid", vec![vec![good.at], vec![trap.at]])]);
    let f = start(resolve, limits());
    let (mut c, _) = tunnel(&f.proxy, 1, 1, rules(&["example.invalid"]), "example.invalid:443");
    c.write_all(b"ping").unwrap();
    let mut back = [0u8; 4];
    c.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"ping");
    drop(c);
    let got = good.got.recv_timeout(WAIT).unwrap();
    assert_eq!(got, [curl_hello(), b"ping"].concat());
    assert_eq!(trap.accepted.load(Ordering::SeqCst), 0, "nothing reached the second lookup's address");
    assert_eq!(lookups.load(Ordering::SeqCst), 1, "one lookup");
}

/// No address beyond the proxy's reach is connected to, however a name resolves: this machine (loopback,
/// none, an IPv4-mapped loopback), the never-reached (link-local, the cloud metadata servers, one carried in
/// NAT64), this machine's own addresses beyond loopback, and every address when those couldn't be read — each
/// refused with 403 and said; a name with one reachable address among refused ones goes there.
#[test]
fn the_never_reached_are_refused_by_the_proxy() {
    let (trap, good) = (outside(echo), outside(echo));
    let port = trap.at.port();
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let not_beyond = || Egress { beyond: Vec::new(), ..rules(&["*.test"]) };
    let own = || Egress { own_addresses: Some(vec![LO]), ..rules(&["*.test"]) };
    let unread = || Egress { own_addresses: None, ..rules(&["*.test"]) };
    let cases: Vec<(&str, SocketAddr, Egress, &str)> = vec![
        ("loop.test", trap.at, not_beyond(), "this machine"),
        ("mapped.test", at(&format!("[::ffff:127.0.0.1]:{port}")), not_beyond(), "this machine"),
        ("none.test", at(&format!("0.0.0.0:{port}")), not_beyond(), "this machine"),
        ("meta.test", at("169.254.169.254:443"), rules(&["*.test"]), "never reached"),
        ("linklocal.test", at("[fe80::1]:443"), rules(&["*.test"]), "never reached"),
        ("alibaba.test", at("100.100.100.200:443"), rules(&["*.test"]), "never reached"),
        ("nat64.test", at("[64:ff9b::a9fe:a9fe]:443"), rules(&["*.test"]), "never reached"),
        ("own.test", trap.at, own(), "own addresses"),
        ("unread.test", trap.at, unread(), "couldn't be read"),
    ];
    let names = cases.iter().map(|(n, a, ..)| (*n, vec![vec![*a]])).collect::<Vec<_>>();
    let mixed = vec![vec![at("169.254.169.254:443"), good.at]];
    let (resolve, _) = resolver([names, vec![("mixed.test", mixed)]].concat());
    let f = start(resolve, limits());
    for (name, _, rules, why) in cases {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules);
        assert_eq!(ask(&mut c, &connect_head(&format!("{name}:443"))), 403, "{name}");
        let target = said(&d);
        assert!(target.starts_with(&format!("{name}:443: ")) && target.contains(why), "{name}: {target}");
    }
    assert_eq!(trap.accepted.load(Ordering::SeqCst), 0, "nothing connected");
    let (mut c, _) = connection(&f.proxy, 1, 1, rules(&["*.test"]));
    assert_eq!(ask(&mut c, &connect_head("mixed.test:443")), 200);
    assert_eq!(good.accepted.load(Ordering::SeqCst), 1, "its one reachable address");
}

/// A tunnel's ClientHello must name the host its CONNECT asked for: another name, or bytes that aren't TLS,
/// close it — said — and none of them reaches the outside.
#[test]
fn sni_must_match_the_connect_host() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("other.test", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    for (first, why) in
        [(curl_hello(), "names example.invalid"), (&b"SSH-2.0-OpenSSH_9.9\r\n"[..], "not TLS")]
    {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules(&["other.test"]));
        assert_eq!(ask(&mut c, &connect_head("other.test:443")), 200);
        c.write_all(first).unwrap();
        assert_eq!(to_close(&mut c), b"", "closed, nothing relayed back");
        let target = said(&d);
        assert!(target.contains(why) && target.ends_with("the tunnel closed"), "{target}");
        assert_eq!(out.got.recv_timeout(WAIT).unwrap(), b"", "nothing of the client's reached the outside");
    }
}

/// After a HelloRetryRequest the client's second ClientHello must name the server the first did (a
/// ChangeCipherSpec may come before it): the same name goes on; another closes the tunnel, said, and doesn't
/// reach the server. Without a HelloRetryRequest nothing more is looked at.
#[test]
fn a_retried_client_hello_must_name_the_same_server() {
    let (retry, plain) = (outside(retrying_server), outside(plain_server));
    let ccs: &[u8] = &[20, 3, 3, 0, 1, 1];
    let hrr = server_hello(&HRR);
    let open = |f: &Fixture| {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules(&["example.invalid"]));
        assert_eq!(ask(&mut c, &connect_head("example.invalid:443")), 200);
        c.write_all(curl_hello()).unwrap();
        (c, d)
    };
    let f = start(resolver(vec![("example.invalid", vec![vec![retry.at]])]).0, limits());
    // The same name, after a ChangeCipherSpec: on to the server
    let (mut c, _) = open(&f);
    let mut back = vec![0u8; hrr.len()];
    c.read_exact(&mut back).unwrap();
    assert_eq!(back, hrr, "the HelloRetryRequest comes through");
    c.write_all(&[ccs, curl_hello()].concat()).unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    assert_eq!(retry.got.recv_timeout(WAIT).unwrap(), [ccs, curl_hello()].concat());
    // Another name: closed, said, and the server got nothing more
    let (mut c, d) = open(&f);
    c.read_exact(&mut back).unwrap();
    c.write_all(cargo_hello()).unwrap();
    assert_eq!(to_close(&mut c), b"");
    let target = said(&d);
    assert!(target.contains("second ClientHello names index.crates.io, not example.invalid"), "{target}");
    assert_eq!(retry.got.recv_timeout(WAIT).unwrap(), b"", "the second ClientHello didn't reach the server");
    // No HelloRetryRequest: what follows isn't read — another ClientHello goes on as any bytes
    let f = start(resolver(vec![("example.invalid", vec![vec![plain.at]])]).0, limits());
    let (mut c, _) = open(&f);
    let mut back = vec![0u8; server_hello(&[7; 32]).len()];
    c.read_exact(&mut back).unwrap();
    c.write_all(cargo_hello()).unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    assert_eq!(plain.got.recv_timeout(WAIT).unwrap(), cargo_hello());
}

/// Plain http goes only to a host the rules name, each standard method with its body: the head the outside gets
/// is the rebuilt one (origin form, its `Host`, no `Proxy-*`); another host is asked, and with no answer
/// refused; a method beyond the standard seven is 405.
#[test]
fn plain_http_is_checked_by_host() {
    let out = outside(web);
    let (resolve, _) = resolver(vec![("web.test", vec![vec![out.at]])]);
    let f = start(resolve, Limits { hold: Duration::from_millis(300), ..limits() });
    let go = |head: &str| {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules(&["web.test"]));
        (ask(&mut c, head), d)
    };
    let (s, _) =
        go("GET http://web.test/x HTTP/1.1\r\nHost: web.test\r\nProxy-Authorization: Basic cnVuOng=\r\n\r\n");
    assert_eq!(s, 200);
    let got = String::from_utf8(out.got.recv_timeout(WAIT).unwrap()).unwrap();
    assert!(got.starts_with("GET /x HTTP/1.1\r\nHost: web.test\r\n") && !got.contains("Proxy-"), "{got}");
    let (s, _) = go("POST http://web.test/f HTTP/1.1\r\nContent-Length: 3\r\n\r\nx=1");
    assert_eq!(s, 200);
    assert!(out.got.recv_timeout(WAIT).unwrap().ends_with(b"\r\n\r\nx=1"), "the body goes on");
    let (s, d) = go("GET http://other.test/ HTTP/1.1\r\n\r\n");
    assert_eq!(s, 403);
    assert!(
        matches!(f.asks.recv_timeout(WAIT), Ok(FromProxy::Ask { host, port: 80, .. }) if host == "other.test")
    );
    assert!(said(&d).contains("no answer"));
    let (s, _) = go("TRACE http://web.test/ HTTP/1.1\r\n\r\n");
    assert_eq!(s, 405);
    assert_eq!(out.accepted.load(Ordering::SeqCst), 2, "only the two let go reached the outside");
}

/// One plain request in, one out: what reaches the checked address is the request's rebuilt head and its body as
/// its framing ends it (by length, or chunked to its last chunk), then the server's side half-closes — a request
/// pipelined behind it, under another `Host`, never reaches it, and its dropping is said.
#[test]
fn a_pipelined_request_never_reaches_the_checked_address() {
    let out = outside(listening_web);
    let (resolve, _) = resolver(vec![("web.test", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let smuggled = "GET http://web.test/evil HTTP/1.1\r\nHost: other.test\r\n\r\n";
    let cases = [
        ("GET http://web.test/a HTTP/1.1\r\n\r\n", ""),
        ("POST http://web.test/a HTTP/1.1\r\nContent-Length: 3\r\n\r\n", "x=1"),
        ("POST http://web.test/a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", "3\r\nx=1\r\n0\r\n\r\n"),
    ];
    for (head, body) in cases {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules(&["web.test"]));
        c.write_all(format!("{head}{body}{smuggled}").as_bytes()).unwrap();
        assert_eq!(status(&mut c), 200);
        // (the server's side half-closed once the body was whole: its reading ends, the client's side still open)
        let got = out.got.recv_timeout(WAIT).expect("the server's side half-closed");
        let got = String::from_utf8(got).unwrap();
        assert!(
            got.starts_with(&head[..head.find(" HTTP").unwrap()].replace("http://web.test", "")),
            "{got}"
        );
        assert!(got.ends_with(&format!("\r\n\r\n{body}")) && !got.contains("evil"), "{got:?}");
        let target = said(&d);
        let n = smuggled.len();
        assert!(target.contains(&format!("{n} bytes sent after its request were dropped")), "{target}");
    }
}

/// An answer for one run leaves another run's connection to the same host waiting — and the user is asked for
/// it, so it's answered too, never refused unasked at the hold.
#[test]
fn a_run_answer_leaves_the_other_runs_asked() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("ask.test", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let (mut one, _) = connection(&f.proxy, 20, 1, rules(&[]));
    one.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    assert!(matches!(f.asks.recv_timeout(WAIT), Ok(FromProxy::Ask { run: 1, .. })));
    let (mut two, _) = connection(&f.proxy, 20, 2, rules(&[]));
    two.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    // (both waiting on the one question before it's answered)
    let by = Instant::now() + WAIT;
    while f.proxy.waiting(20, "ask.test", 443) < 2 {
        assert!(Instant::now() < by, "the second run's connection never waited");
        std::thread::yield_now();
    }
    let answer =
        |run| ToProxy::Answer { session: 20, run, host: "ask.test".into(), port: 443, scope: Scope::Run };
    f.proxy.send(answer(1));
    assert_eq!(status(&mut one), 200);
    let asked = f.asks.recv_timeout(WAIT).expect("the other run is asked");
    assert_eq!(asked, FromProxy::Ask { session: 20, run: 2, host: "ask.test".into(), port: 443 });
    f.proxy.send(answer(2));
    assert_eq!(status(&mut two), 200);
}

/// A connection whose hold ran out is no longer waiting: the next connection to that host is asked again, not
/// left to wait out its own hold unasked.
#[test]
fn a_question_whose_connection_gave_up_is_asked_again() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("ask.test", vec![vec![out.at]])]);
    let f = start(resolve, Limits { hold: Duration::from_millis(300), ..limits() });
    let (mut c, d) = connection(&f.proxy, 21, 1, rules(&[]));
    assert_eq!(ask(&mut c, &connect_head("ask.test:443")), 403);
    assert!(said(&d).contains("no answer"));
    assert!(f.asks.recv_timeout(WAIT).is_ok());
    let (mut c, _) = connection(&f.proxy, 21, 1, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("asked again");
    assert_eq!(asked, FromProxy::Ask { session: 21, run: 1, host: "ask.test".into(), port: 443 });
}

/// Past a session's cap, and past the cap in all, a connection is answered 503 and said; once a tunnel ends,
/// its place is free again.
#[test]
fn past_a_cap_is_503() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("example.invalid", vec![vec![out.at]])]);
    let f = start(resolve, Limits { per_session: 2, total: 3, ..limits() });
    let ok = || rules(&["example.invalid"]);
    let a1 = tunnel(&f.proxy, 1, 1, ok(), "example.invalid:443");
    let _a2 = tunnel(&f.proxy, 1, 1, ok(), "example.invalid:443");
    let (mut a3, d) = connection(&f.proxy, 1, 1, ok());
    assert_eq!(status(&mut a3), 503);
    assert!(said(&d).contains("past 2 connections at once for the session"));
    let _b1 = tunnel(&f.proxy, 2, 2, ok(), "example.invalid:443");
    let (mut b2, d) = connection(&f.proxy, 2, 2, ok());
    assert_eq!(status(&mut b2), 503);
    assert!(said(&d).contains("past 3 connections at once in all"));
    drop(a1);
    let freed = Instant::now() + WAIT;
    loop {
        let (mut c, _) = connection(&f.proxy, 1, 1, ok());
        match ask(&mut c, &connect_head("example.invalid:443")) {
            200 => break,
            503 if Instant::now() < freed => continue,
            s => panic!("a freed place: {s}"),
        }
    }
}

/// A host the rules don't name is held and the user asked, once: `y` lets the held connection go on, `n`
/// refuses it (and the session's next one, unasked), no answer within the hold refuses it — each said.
#[test]
fn an_unknown_host_is_held_and_asked() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("ask.test", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let ask_it = |session| {
        let (mut c, d) = connection(&f.proxy, session, 1, rules(&[]));
        c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
        let asked = f.asks.recv_timeout(WAIT).expect("asked");
        assert_eq!(asked, FromProxy::Ask { session, run: 1, host: "ask.test".into(), port: 443 });
        (c, d)
    };
    let (mut c, _) = ask_it(1);
    f.proxy.send(ToProxy::Answer {
        session: 1,
        run: 1,
        host: "ask.test".into(),
        port: 443,
        scope: Scope::Run,
    });
    assert_eq!(status(&mut c), 200);
    let (mut c, d) = ask_it(2);
    f.proxy.send(ToProxy::Answer {
        session: 2,
        run: 1,
        host: "ask.test".into(),
        port: 443,
        scope: Scope::Refused,
    });
    assert_eq!(status(&mut c), 403);
    assert!(said(&d).contains("refused"));
    let (mut c, d) = connection(&f.proxy, 2, 3, rules(&[]));
    assert_eq!(ask(&mut c, &connect_head("ask.test:443")), 403, "the session's refusal holds, unasked");
    assert!(said(&d).contains("refused it for this session"));
    let short = start(
        resolver(vec![("ask.test", vec![vec![out.at]])]).0,
        Limits { hold: Duration::from_millis(300), ..limits() },
    );
    let (mut c, d) = connection(&short.proxy, 3, 1, rules(&[]));
    assert_eq!(ask(&mut c, &connect_head("ask.test:443")), 403);
    assert!(said(&d).contains("asked the user — no answer within 0.3 s"));
    assert!(f.asks.try_recv().is_err() && short.asks.try_recv().is_ok());
}

/// The user's answer for the session lets that run's next connections and the session's other runs through
/// unasked; an answer for one run reaches only that run.
#[test]
fn a_session_answer_lets_the_runs_next_connections_through() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("ask.test", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let (mut c, _) = connection(&f.proxy, 5, 1, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    assert!(f.asks.recv_timeout(WAIT).is_ok());
    f.proxy.send(ToProxy::Answer {
        session: 5,
        run: 1,
        host: "ask.test".into(),
        port: 443,
        scope: Scope::Session,
    });
    assert_eq!(status(&mut c), 200);
    for run in [1, 2] {
        let (mut c, _) = connection(&f.proxy, 5, run, rules(&[]));
        assert_eq!(ask(&mut c, &connect_head("ask.test:443")), 200, "run {run}");
    }
    assert!(f.asks.try_recv().is_err(), "asked once");
    let (mut c, _) = connection(&f.proxy, 6, 1, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    assert!(f.asks.recv_timeout(WAIT).is_ok());
    f.proxy.send(ToProxy::Answer {
        session: 6,
        run: 1,
        host: "ask.test".into(),
        port: 443,
        scope: Scope::Run,
    });
    assert_eq!(status(&mut c), 200);
    let (mut c, _) = connection(&f.proxy, 6, 2, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    let again = f.asks.recv_timeout(WAIT).expect("another run is asked again");
    assert_eq!(again, FromProxy::Ask { session: 6, run: 2, host: "ask.test".into(), port: 443 });
    drop(c);
    // "Always for the project" reaches the session's runs here (the main loop keeps it for the next rules)
    let (mut c, _) = connection(&f.proxy, 12, 1, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    assert!(f.asks.recv_timeout(WAIT).is_ok());
    let project = Scope::Project;
    f.proxy.send(ToProxy::Answer { session: 12, run: 1, host: "ask.test".into(), port: 443, scope: project });
    assert_eq!(status(&mut c), 200);
    let (mut c, _) = connection(&f.proxy, 12, 2, rules(&[]));
    assert_eq!(ask(&mut c, &connect_head("ask.test:443")), 200);
    assert!(f.asks.try_recv().is_err());
}

/// The system's resolver, on the connection's thread, answers a name this machine knows (no network).
#[test]
fn the_systems_resolver_answers_a_name() {
    let found = egress::system()("localhost", 443).unwrap();
    assert!(found.iter().all(|a| a.port() == 443) && found.iter().any(|a| a.ip().is_loopback()), "{found:?}");
}

/// One session's answers aren't another's: a host let go for one session is asked in the next.
#[test]
fn two_sessions_rules_stay_apart() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("ask.test", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let (mut c, _) = connection(&f.proxy, 7, 1, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    assert!(f.asks.recv_timeout(WAIT).is_ok());
    f.proxy.send(ToProxy::Answer {
        session: 7,
        run: 1,
        host: "ask.test".into(),
        port: 443,
        scope: Scope::Session,
    });
    assert_eq!(status(&mut c), 200);
    let (mut c, _) = connection(&f.proxy, 8, 1, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("the other session is asked");
    assert_eq!(asked, FromProxy::Ask { session: 8, run: 1, host: "ask.test".into(), port: 443 });
    drop(c);
    // A session that ended takes its answers with it
    assert!(f.proxy.send(ToProxy::SessionEnded { session: 7 }));
    let (mut c, _) = connection(&f.proxy, 7, 2, rules(&[]));
    c.write_all(connect_head("ask.test:443").as_bytes()).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("asked again once its session ended");
    assert_eq!(asked, FromProxy::Ask { session: 7, run: 2, host: "ask.test".into(), port: 443 });
}

/// The proxy knows only what a message carries (D23): a stand-in supervisor that only writes messages hands it
/// three connections — a name the first's rules allow is relayed, the same name under the second's rules is asked
/// and refused, the third's rules allow it again — and each refusal comes back on its own message's sender.
#[test]
fn the_proxy_knows_only_what_a_message_carries() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("example.invalid", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let (_a, a_said) = tunnel(&f.proxy, 9, 1, rules(&["example.invalid"]), "example.invalid:443");
    let (mut b, b_said) = connection(&f.proxy, 10, 2, rules(&[]));
    b.write_all(connect_head("example.invalid:443").as_bytes()).unwrap();
    assert!(f.asks.recv_timeout(WAIT).is_ok());
    let host = "example.invalid".to_string();
    f.proxy.send(ToProxy::Answer { session: 10, run: 2, host, port: 443, scope: Scope::Refused });
    assert_eq!(status(&mut b), 403);
    assert!(said(&b_said).contains("refused"));
    let _c = tunnel(&f.proxy, 11, 3, rules(&["example.invalid"]), "example.invalid:443");
    assert!(a_said.try_recv().is_err(), "the refusal went to its own message's sender only");
}

/// A client that doesn't finish its head within the limit is answered 408 and cut, and a tunnel whose
/// ClientHello doesn't come whole within its limit is closed — each said, nothing reaching the outside.
#[test]
fn a_slow_client_is_cut_at_the_time_limit() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("example.invalid", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let (mut c, d) = connection(&f.proxy, 1, 1, rules(&["example.invalid"]));
    c.write_all(b"CONNECT exa").unwrap();
    assert_eq!(status(&mut c), 408);
    let _ = to_close(&mut c);
    assert!(said(&d).contains("no whole request head within 0.3 s"));
    let (mut c, d) = connection(&f.proxy, 1, 1, rules(&["example.invalid"]));
    assert_eq!(ask(&mut c, &connect_head("example.invalid:443")), 200);
    c.write_all(&curl_hello()[..20]).unwrap();
    assert_eq!(to_close(&mut c), b"");
    assert!(said(&d).contains("no whole ClientHello within 0.3 s"));
    assert_eq!(out.got.recv_timeout(WAIT).unwrap(), b"");
}

/// A run's own listener (macOS) takes only requests carrying its token as the password: none is answered 407
/// with the challenge git waits for (not said — a client's first try), a wrong one 407 and said, the right one
/// goes on; and once the run ended, its port takes nothing.
#[test]
fn no_token_or_a_wrong_one_is_407() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("example.invalid", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let l = TcpListener::bind((LO, 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    let (told, d) = mpsc::channel();
    let rules = Arc::new(rules(&["example.invalid"]));
    let token = "tarae-fake-token-6".to_string();
    let process = "curl".to_string();
    assert!(f.proxy.send(ToProxy::RunStarts {
        run: 4,
        session: 1,
        process,
        listener: l.into(),
        token,
        rules,
        told
    }));
    let client = || {
        let c = TcpStream::connect((LO, port)).unwrap();
        c.set_read_timeout(Some(WAIT)).unwrap();
        c
    };
    let with = |auth: &str| format!("CONNECT example.invalid:443 HTTP/1.1\r\n{auth}\r\n");
    let mut c = client();
    c.write_all(with("").as_bytes()).unwrap();
    let h = head_of(&mut c);
    assert!(
        h.starts_with("HTTP/1.1 407") && h.contains("\r\nProxy-Authenticate: Basic realm=\"tarae\"\r\n"),
        "{h}"
    );
    // run:wrong / run:tarae-fake-token-6
    let mut c = client();
    assert_eq!(ask(&mut c, &with("Proxy-Authorization: Basic cnVuOndyb25n\r\n")), 407);
    assert!(said(&d).contains("wrong proxy password"));
    assert!(d.try_recv().is_err(), "the first try without a password wasn't said");
    let mut c = client();
    assert_eq!(ask(&mut c, &with("Proxy-Authorization: Basic cnVuOnRhcmFlLWZha2UtdG9rZW4tNg==\r\n")), 200);
    assert!(f.proxy.send(ToProxy::RunEnded { run: 4 }));
    let gone = Instant::now() + WAIT;
    while TcpStream::connect((LO, port)).is_ok() {
        assert!(Instant::now() < gone, "the run's port still takes connections");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The pair a run's connection to the proxy is made of (Linux's `supervisor::proxied`) holds only the run's
/// end and the proxy's: another connect into the pair's listener in its window (an intruder stood there by the
/// test) is closed, never taken for the run's — else a process of this machine could stand in for a run, its
/// bytes going out under the run's rules. The run's end is readied (what the run set) before it connects, and is
/// a real TCP socket of the run's family, for IPv4 and IPv6 alike.
#[test]
fn the_pair_takes_only_its_own_peer() {
    for domain in [libc::AF_INET, libc::AF_INET6] {
        let intruder = std::cell::RefCell::new(None);
        let readied_unconnected = std::cell::Cell::new(false);
        let ready = |s: &OwnedFd| {
            let probe = TcpStream::from(s.try_clone()?);
            readied_unconnected.set(probe.peer_addr().is_err());
            probe.set_nodelay(true)
        };
        let between = |at: SocketAddr| *intruder.borrow_mut() = Some(TcpStream::connect(at).unwrap());
        let (run, mut proxy) = pair(domain, ready, between).unwrap();
        proxy.set_read_timeout(Some(WAIT)).unwrap();
        let mut run = TcpStream::from(run);
        assert!(readied_unconnected.get() && run.nodelay().unwrap(), "readied before it connects");
        let family = |a: SocketAddr| if a.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
        assert_eq!(family(run.local_addr().unwrap()), domain, "the run's own family");
        run.write_all(b"from the run").unwrap();
        let mut got = [0u8; 12];
        proxy.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"from the run", "the proxy's end is the run's peer");
        let mut intruder = intruder.into_inner().unwrap();
        intruder.set_read_timeout(Some(WAIT)).unwrap();
        assert_eq!(to_close(&mut intruder), b"", "the intruder was closed, not taken");
    }
}

/// A token is the same only byte for byte, whatever the lengths.
#[test]
fn a_token_is_the_same_only_byte_for_byte() {
    let t = b"tarae-fake-token-7";
    assert!(same_token(t, t));
    for other in
        [&b"Tarae-fake-token-7"[..], b"tarae-fake-token-8", b"tarae-fake-token-", b"tarae-fake-token-77", b""]
    {
        assert!(!same_token(other, t), "{other:?}");
    }
}

/// A rule names a host — one name, or every name under a domain but the domain itself — at its port, else the
/// request's own; an address, a bare `*`, a `*` inside a name or a port out of range is no rule.
#[test]
fn a_rule_names_hosts_and_their_ports() {
    let r = |s| Pattern::parse(s).unwrap();
    let e =
        Egress { hosts: vec![r("GitHub.com"), r("*.crates.io"), r("api.test:8443")], ..Egress::default() };
    assert!(e.allows("github.com", 443, 443) && e.allows("github.com", 80, 80));
    assert!(!e.allows("github.com", 8443, 443) && !e.allows("api.github.com", 443, 443));
    assert!(e.allows("static.crates.io", 443, 443) && e.allows("a.b.crates.io", 443, 443));
    assert!(!e.allows("crates.io", 443, 443) && !e.allows("evilcrates.io", 443, 443));
    assert!(e.allows("api.test", 8443, 443) && !e.allows("api.test", 443, 443));
    for no in ["1.2.3.4", "::1", "*", "a.*.b", "*.*.b", "x.test:0", "x.test:70000", "x.test:", ""] {
        assert!(Pattern::parse(no).is_err(), "{no}");
    }
    assert!(Egress { any: true, ..Egress::default() }.allows("anything.test", 1, 443));
}
