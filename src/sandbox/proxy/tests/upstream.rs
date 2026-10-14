//! The proxy chaining to the one tarae itself goes out by (step 5g): a stand-in upstream on loopback, named as
//! tarae's environment would name it, records every byte it gets and answers as the test says. Hosts are
//! `*.example.invalid`, credentials fake ones; nothing leaves this machine.

use std::path::Path;

use super::*;
use crate::sandbox::proxy::upstream::{self as up, Setting, Upstreams};

/// A head, read to its blank line.
fn read_head(s: &mut TcpStream) -> Vec<u8> {
    let (mut got, mut b) = (Vec::new(), [0u8; 1]);
    while !got.ends_with(b"\r\n\r\n") {
        match s.read(&mut b) {
            Ok(1) => got.push(b[0]),
            _ => break,
        }
    }
    got
}

/// An upstream that opens every tunnel: its CONNECT read, 200, then what comes echoed — all it got.
fn opens(mut s: TcpStream) -> Vec<u8> {
    let mut got = read_head(&mut s);
    let _ = s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
    got.extend(echo(s));
    got
}

/// Answers `answer` to the first head, then reads to the end: all it got.
fn answering(mut s: TcpStream, answer: &[u8]) -> Vec<u8> {
    let mut got = read_head(&mut s);
    let _ = s.write_all(answer);
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    got.extend(rest);
    got
}

/// Words of the upstream's own, which never reach a run or its report.
const ITS_WORDS: &str = "upstream-words-never-passed";

fn asks_basic(s: TcpStream) -> Vec<u8> {
    answering(
        s,
        b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"corp\"\r\n\
          Content-Length: 28\r\n\r\nupstream-words-never-passed\n",
    )
}

fn asks_negotiate(s: TcpStream) -> Vec<u8> {
    answering(
        s,
        b"HTTP/1.1 407 upstream-words-never-passed\r\nProxy-Authenticate: Negotiate\r\nProxy-Authenticate: NTLM\r\n\
          Content-Length: 0\r\n\r\n",
    )
}

fn forbids(s: TcpStream) -> Vec<u8> {
    answering(s, b"HTTP/1.1 403 upstream-words-never-passed\r\nContent-Length: 0\r\n\r\n")
}

/// Takes the request and never answers.
fn silent(mut s: TcpStream) -> Vec<u8> {
    let mut got = read_head(&mut s);
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    got.extend(rest);
    got
}

/// `%XX` for every byte but the URL's unreserved ones — as a proxy URL holds credentials.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(b).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn env(vars: &[(&str, String)]) -> HashMap<String, std::ffi::OsString> {
    vars.iter().map(|(k, v)| (k.to_string(), v.into())).collect()
}

/// tarae's environment naming the upstream at `at` for both kinds (with these credentials).
fn via(at: SocketAddr, creds: Option<(&str, &str)>) -> Upstreams {
    let creds = creds.map(|(u, p)| format!("{}:{}@", pct(u), pct(p))).unwrap_or_default();
    let url = format!("http://{creds}{at}");
    up::of(&env(&[("HTTPS_PROXY", url.clone()), ("HTTP_PROXY", url)]), Setting::Env)
}

fn rules_via(hosts: &[&str], upstream: Upstreams) -> Egress {
    Egress { upstream, ..rules(hosts) }
}

/// `o` took `n` connections — waited for: its own thread counts them.
fn took(o: &Outside, n: usize) -> bool {
    let by = Instant::now() + WAIT;
    while o.accepted.load(Ordering::SeqCst) < n && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(5));
    }
    o.accepted.load(Ordering::SeqCst) == n
}

/// Through an upstream, an allowed host's tunnel is asked of it — `CONNECT host:port`, tarae's credentials at
/// once — and the run's ClientHello follows after the upstream's 200, checked as ever. The run's name is never
/// looked up by tarae: the upstream does that, which is why tarae's address checks can't hold there. A host the
/// rules don't let go never reaches the upstream; a ClientHello naming another server never goes past the CONNECT.
#[test]
fn an_upstream_proxy_is_chained() {
    let upstream = outside(opens);
    let (resolve, lookups) = resolver(Vec::new());
    let f = start(resolve, Limits { hold: Duration::from_millis(300), ..limits() });
    let rules = || rules_via(&["example.invalid"], via(upstream.at, Some(("u", "tarae-fake-token-1"))));
    let (c, _) = tunnel(&f.proxy, 1, 1, rules(), "example.invalid:443");
    drop(c);
    let basic = parse::base64(b"u:tarae-fake-token-1");
    let head = format!(
        "CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\nProxy-Authorization: Basic {basic}\r\n\r\n"
    );
    let got = upstream.got.recv_timeout(WAIT).unwrap();
    assert_eq!(got, [head.as_bytes(), curl_hello()].concat(), "{}", String::from_utf8_lossy(&got));
    assert_eq!(lookups.load(Ordering::SeqCst), 0, "the run's name is the upstream's to look up");
    // A host the rules don't name: asked, unanswered, refused — the upstream never hears of it
    let (mut c, d) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("refused.example.invalid:443")), 403);
    assert!(said_host(&d).contains("didn't answer"));
    // A ClientHello naming another server: the tunnel was asked, the hello never goes on
    let (mut c, d) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("example.invalid:443")), 200);
    c.write_all(cargo_hello()).unwrap();
    assert_eq!(to_close(&mut c), b"");
    assert!(said(&d).contains("names index.crates.io"));
    assert_eq!(upstream.got.recv_timeout(WAIT).unwrap(), head.as_bytes(), "the CONNECT, nothing after");
    assert!(took(&upstream, 2), "the refused host never reached it");
}

/// A plain request goes to the upstream in absolute form — `GET http://host/path` as proxies take it — with
/// tarae's credentials, the run's own `Proxy-*` gone, `Connection: close`, its body to its end; its name never
/// looked up by tarae.
#[test]
fn plain_http_goes_upstream_in_absolute_form() {
    let upstream = outside(web);
    let (resolve, lookups) = resolver(Vec::new());
    let f = start(resolve, limits());
    let hosts = ["web.example.invalid", "web.example.invalid:8080"];
    let creds = Some(("u", "tarae-fake-token-2"));
    let basic = parse::base64(b"u:tarae-fake-token-2");
    let go = |head: &str| {
        let (mut c, _) = connection(&f.proxy, 1, 1, rules_via(&hosts, via(upstream.at, creds)));
        let s = ask(&mut c, head);
        (s, String::from_utf8(upstream.got.recv_timeout(WAIT).unwrap()).unwrap())
    };
    let (s, got) = go("GET http://web.example.invalid/x?y=1 HTTP/1.1\r\nHost: web.example.invalid\r\n\
         Proxy-Authorization: Basic cnVuOng=\r\n\r\n");
    assert_eq!(s, 200);
    assert_eq!(
        got,
        format!(
            "GET http://web.example.invalid/x?y=1 HTTP/1.1\r\nHost: web.example.invalid\r\nConnection: close\r\n\
             Proxy-Authorization: Basic {basic}\r\n\r\n"
        )
    );
    let (s, got) = go("POST http://web.example.invalid:8080/f HTTP/1.1\r\nContent-Length: 3\r\n\r\nx=1");
    assert_eq!(s, 200);
    assert!(got.starts_with(
        "POST http://web.example.invalid:8080/f HTTP/1.1\r\nHost: web.example.invalid:8080\r\n"
    ));
    assert!(got.ends_with(&format!("Proxy-Authorization: Basic {basic}\r\n\r\nx=1")), "{got}");
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
}

/// What failed at the upstream is said as the upstream's (`network-upstream`), apart from tarae's own refusals:
/// unreachable, silent, its 407 three ways (credentials refused, none given, a scheme tarae doesn't speak), its
/// own refusal, an https:// one tarae can't chain to, a plain request's 407. The run gets 502 — never the
/// upstream's challenge (it would answer it with its own token) nor its words; the report names the variable,
/// never where the upstream is; `:sandbox-blocked` offers nothing, as no answer of the user's would lift it.
#[test]
fn the_upstreams_failures_are_said_as_its_own() {
    let closed = TcpListener::bind((LO, 0)).unwrap().local_addr().unwrap();
    let (basic, negotiate, refusing, quiet) =
        (outside(asks_basic), outside(asks_negotiate), outside(forbids), outside(silent));
    let f = start(resolver(Vec::new()).0, Limits { connect: Duration::from_millis(400), ..limits() });
    let creds = Some(("u", "tarae-fake-token-3"));
    let https = format!("https://u:tarae-fake-token-3@{}", basic.at);
    let get = "GET http://example.invalid/ HTTP/1.1\r\n\r\n".to_string();
    let connect = connect_head("example.invalid:443");
    let cases: Vec<(Upstreams, &String, &str)> = vec![
        (via(closed, creds), &connect, "it can't be reached (connection refused)"),
        (via(quiet.at, creds), &connect, "it didn't answer within 0.4 s"),
        (via(basic.at, creds), &connect, "it refused the credentials in tarae's HTTPS_PROXY (407)"),
        (via(basic.at, None), &connect, "it asks for credentials (407), and tarae's HTTPS_PROXY gives none"),
        (
            via(negotiate.at, creds),
            &connect,
            "it asks for Negotiate or NTLM (407), which tarae doesn't speak",
        ),
        (via(refusing.at, creds), &connect, "it answered 403 (Forbidden — its own refusal)"),
        (
            up::of(&env(&[("HTTPS_PROXY", https)]), Setting::Env),
            &connect,
            "tarae's own HTTPS_PROXY names an https:// proxy, which tarae's proxy doesn't chain to yet",
        ),
        (via(basic.at, creds), &get, "it refused the credentials in tarae's HTTP_PROXY (407)"),
    ];
    for (upstream, request, why) in cases {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules_via(&["example.invalid"], upstream));
        c.write_all(request.as_bytes()).unwrap();
        let head = head_of(&mut c);
        assert!(head.starts_with("HTTP/1.1 502 "), "{why}: {head}");
        assert!(!head.to_ascii_lowercase().contains("proxy-authenticate"), "{why}: {head}");
        let body = String::from_utf8(to_close(&mut c)).unwrap();
        let denial = d.recv_timeout(WAIT).expect("said");
        assert_eq!(denial.op, UPSTREAM_OP, "{why}: {}", denial.target);
        let port = if request.starts_with("GET") { 80 } else { 443 };
        assert!(
            denial
                .target
                .starts_with(&format!("example.invalid:{port}: through the proxy tarae itself uses"))
                || denial.target.starts_with(&format!("example.invalid:{port}: tarae's own")),
            "{}",
            denial.target
        );
        assert!(denial.target.contains(why), "{why}: {}", denial.target);
        for said in [&denial.target, &denial.reported(), &body] {
            assert!(!said.contains(ITS_WORDS) && !said.contains("127.0.0.1"), "{why}: {said}");
        }
        let home = Path::new("/nonexistent-home");
        assert_eq!(crate::blocked::choices(&[denial], home, Path::new("/w/p"), &[]), [], "{why}");
    }
}

/// `NO_PROXY`'s hosts go direct — looked up by tarae, every address checked: one resolving to a metadata server is
/// refused again; the upstream hears of none of them. Another host goes through the upstream.
#[test]
fn no_proxy_hosts_go_direct_with_the_address_checks() {
    let (upstream, out) = (outside(opens), outside(echo));
    let meta: SocketAddr = "169.254.169.254:443".parse().unwrap();
    let (resolve, lookups) = resolver(vec![
        ("direct.example.invalid", vec![vec![out.at]]),
        ("meta.direct.example.invalid", vec![vec![meta]]),
    ]);
    let f = start(resolve, limits());
    let named = [
        ("HTTPS_PROXY", format!("http://{}", upstream.at)),
        ("NO_PROXY", "localhost,.direct.example.invalid".to_string()),
    ];
    let rules = || rules_via(&["*.example.invalid"], up::of(&env(&named), Setting::Env));
    let (mut c, _) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("direct.example.invalid:443")), 200);
    assert!(took(&out, 1), "straight to the address tarae checked");
    let (mut c, d) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("meta.direct.example.invalid:443")), 403);
    assert!(said(&d).contains("never reached"), "the address checks are back");
    assert!(took(&upstream, 0) && lookups.load(Ordering::SeqCst) == 2);
    let (mut c, _) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("other.example.invalid:443")), 200);
    assert!(took(&upstream, 1), "a host NO_PROXY doesn't name: through the upstream");
    assert_eq!(lookups.load(Ordering::SeqCst), 2, "and not looked up by tarae");
}

/// A cloud metadata server's own name is refused by name whichever way it would go — through an upstream, which
/// would look it up where tarae can't check the address, or direct, for any host allowed — said; nothing is looked
/// up or reached. (Partial: a name that only leads there isn't caught through an upstream — the report says so.)
#[test]
fn metadata_names_are_refused_by_name() {
    let upstream = outside(opens);
    let (resolve, lookups) = resolver(Vec::new());
    let f = start(resolve, limits());
    let any = || Egress { any: true, ..rules(&[]) };
    let through = || Egress { upstream: via(upstream.at, None), ..any() };
    for (rules, request) in [
        (through(), connect_head("metadata.google.internal:443")),
        (through(), "GET http://metadata.google.internal/computeMetadata/v1/ HTTP/1.1\r\n\r\n".into()),
        (through(), connect_head("metadata.goog:443")),
        (any(), connect_head("instance-data.ec2.internal:443")),
        (any(), "GET http://Metadata./latest HTTP/1.1\r\n\r\n".into()),
    ] {
        let (mut c, d) = connection(&f.proxy, 1, 1, rules);
        assert_eq!(ask(&mut c, &request), 403, "{request}");
        assert!(said(&d).contains("a cloud metadata server's own name"), "{request}");
    }
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
    assert!(took(&upstream, 0));
}

/// `n` bytes from `s` into `got` — false: it ended first.
fn take(s: &mut TcpStream, n: usize, got: &mut Vec<u8>) -> bool {
    let mut b = vec![0u8; n];
    let ok = s.read_exact(&mut b).is_ok();
    if ok {
        got.extend_from_slice(&b);
    }
    ok
}

/// A stand-in SOCKS5 server (RFC 1928, 1929) answering as told: the way it signs in (`way`; None: a user and
/// password where offered, else none), whether it takes the credentials, its CONNECT's reply code — then, its
/// tunnel open, `then` serves what comes. All it got, in order.
fn socks(mut s: TcpStream, way: Option<u8>, takes: bool, rep: u8, then: fn(TcpStream) -> Vec<u8>) -> Vec<u8> {
    let mut got = Vec::new();
    let rest = |mut s: TcpStream, mut got: Vec<u8>| {
        let _ = s.read_to_end(&mut got);
        got
    };
    if !take(&mut s, 2, &mut got) || !take(&mut s, usize::from(got[1]), &mut got) {
        return got;
    }
    let way = way.unwrap_or(if got[2..].contains(&2) { 2 } else { 0 });
    let _ = s.write_all(&[5, way]);
    if way == 0xff {
        return rest(s, got);
    }
    if way == 2 {
        let at = got.len();
        let sign_in = take(&mut s, 2, &mut got)
            && take(&mut s, usize::from(got[at + 1]), &mut got)
            && take(&mut s, 1, &mut got)
            && take(&mut s, usize::from(got[got.len() - 1]), &mut got);
        let _ = s.write_all(&[1, if takes { 0 } else { 1 }]);
        if !sign_in || !takes {
            return rest(s, got);
        }
    }
    let at = got.len();
    if !take(&mut s, 5, &mut got) || !take(&mut s, usize::from(got[at + 4]) + 2, &mut got) {
        return got;
    }
    let _ = s.write_all(&[5, rep, 0, 1, 0, 0, 0, 0, 0, 0]);
    if rep != 0 {
        return rest(s, got);
    }
    got.extend(then(s));
    got
}

fn socks_echo(s: TcpStream) -> Vec<u8> {
    socks(s, None, true, 0, echo)
}

fn socks_web(s: TcpStream) -> Vec<u8> {
    socks(s, None, true, 0, web)
}

fn socks_refuses_sign_in(s: TcpStream) -> Vec<u8> {
    socks(s, None, false, 0, echo)
}

fn socks_takes_no_way(s: TcpStream) -> Vec<u8> {
    socks(s, Some(0xff), true, 0, echo)
}

/// Its rules don't allow the host (RFC 1928's reply 2).
fn socks_refuses_host(s: TcpStream) -> Vec<u8> {
    socks(s, None, true, 2, echo)
}

/// `socks_refuses_host`, 0.6 s late — an upstream on a loaded machine standing in: later than a 0.4 s limit.
fn socks_refuses_host_late(s: TcpStream) -> Vec<u8> {
    std::thread::sleep(Duration::from_millis(600));
    socks_refuses_host(s)
}

/// tarae's environment naming a SOCKS5 upstream at `at` (`scheme`: socks5h or socks5) for everything, as a local
/// client's `ALL_PROXY` does.
fn via_socks(scheme: &str, at: SocketAddr, creds: Option<(&str, &str)>) -> Upstreams {
    let creds = creds.map(|(u, p)| format!("{}:{}@", pct(u), pct(p))).unwrap_or_default();
    up::of(&env(&[("ALL_PROXY", format!("{scheme}://{creds}{at}"))]), Setting::Env)
}

/// RFC 1928's CONNECT for `name:port` by its name, and RFC 1929's sign-in — as the stand-in gets them.
fn socks_connect(name: &str, port: u16) -> Vec<u8> {
    [&[5, 1, 0, 3, name.len() as u8][..], name.as_bytes(), &port.to_be_bytes()].concat()
}

/// Through a SOCKS5 upstream — `socks5h://` with credentials, `socks5://` without: the same, since tarae never
/// looks the name up — an allowed host's tunnel is asked of it by name (ATYP 3), signed in with tarae's
/// credentials (RFC 1929) where it has any, and the run's ClientHello follows, checked as ever; the run's name is
/// never looked up by tarae. A host the rules don't let go never reaches it; a ClientHello naming another server
/// never goes past the CONNECT.
#[test]
fn a_socks5_upstream_is_chained() {
    let upstream = outside(socks_echo);
    let (resolve, lookups) = resolver(Vec::new());
    let f = start(resolve, Limits { hold: Duration::from_millis(300), ..limits() });
    let signed = [&[5, 2, 0, 2, 1, 1, b'u', 18][..], b"tarae-fake-token-6"].concat();
    for (scheme, creds, hello) in
        [("socks5h", Some(("u", "tarae-fake-token-6")), signed.clone()), ("socks5", None, vec![5, 1, 0])]
    {
        let rules = rules_via(&["example.invalid"], via_socks(scheme, upstream.at, creds));
        let (c, _) = tunnel(&f.proxy, 1, 1, rules, "example.invalid:443");
        drop(c);
        let got = upstream.got.recv_timeout(WAIT).unwrap();
        assert_eq!(
            got,
            [&hello[..], &socks_connect("example.invalid", 443), curl_hello()].concat(),
            "{scheme}"
        );
    }
    assert_eq!(lookups.load(Ordering::SeqCst), 0, "the run's name is the upstream's to look up");
    let rules = || {
        rules_via(&["example.invalid"], via_socks("socks5h", upstream.at, Some(("u", "tarae-fake-token-6"))))
    };
    let (mut c, d) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("refused.example.invalid:443")), 403);
    assert!(said_host(&d).contains("didn't answer"));
    let (mut c, d) = connection(&f.proxy, 1, 1, rules());
    assert_eq!(ask(&mut c, &connect_head("example.invalid:443")), 200);
    c.write_all(cargo_hello()).unwrap();
    assert_eq!(to_close(&mut c), b"");
    assert!(said(&d).contains("names index.crates.io"));
    let got = upstream.got.recv_timeout(WAIT).unwrap();
    assert_eq!(
        got,
        [&signed[..], &socks_connect("example.invalid", 443)].concat(),
        "the CONNECT, nothing after"
    );
    assert!(took(&upstream, 3), "the refused host never reached it");
}

/// A plain request through a SOCKS5 upstream goes as it would to the host: CONNECT host:80 by name, then the head
/// in origin form — its `Host`, `Connection: close`, no `Proxy-*` of the run's or tarae's (SOCKS5 carries the
/// credentials itself) — and the host's answer back as it came.
#[test]
fn plain_http_goes_through_a_socks5_upstream_in_origin_form() {
    let upstream = outside(socks_web);
    let (resolve, lookups) = resolver(Vec::new());
    let f = start(resolve, limits());
    let rules = rules_via(&["web.example.invalid"], via_socks("socks5h", upstream.at, None));
    let (mut c, _) = connection(&f.proxy, 1, 1, rules);
    let head = "GET http://web.example.invalid/x HTTP/1.1\r\nHost: web.example.invalid\r\n\
                Proxy-Authorization: Basic cnVuOng=\r\n\r\n";
    assert_eq!(ask(&mut c, head), 200);
    let got = upstream.got.recv_timeout(WAIT).unwrap();
    let origin = b"GET /x HTTP/1.1\r\nHost: web.example.invalid\r\nConnection: close\r\n\r\n";
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(
            &[&[5, 1, 0][..], &socks_connect("web.example.invalid", 80), origin].concat()
        )
    );
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
}

/// What failed at a SOCKS5 upstream is said as the upstream's (`network-upstream`), as an http one's: its sign-in
/// refused, no way of signing in it takes (without credentials, and with), the host refused by its rules (a tunnel
/// and a plain request), silent, unreachable — the run gets 502, the report names the variable, never where it is,
/// and `:sandbox-blocked` offers nothing. Only the silent one is waited on to the proxy's limit (0.4 s, a proxy of
/// its own); the others answer, and their proxy waits for that up to `WAIT` — under load a 0.4 s limit had one
/// that answers said as silent (#232).
#[test]
fn a_socks5_upstreams_failures_are_said_as_its_own() {
    let closed = TcpListener::bind((LO, 0)).unwrap().local_addr().unwrap();
    let (refusing, no_way, no_host, late, quiet) = (
        outside(socks_refuses_sign_in),
        outside(socks_takes_no_way),
        outside(socks_refuses_host),
        outside(socks_refuses_host_late),
        outside(silent),
    );
    let quick = start(resolver(Vec::new()).0, Limits { connect: Duration::from_millis(400), ..limits() });
    let f = start(resolver(Vec::new()).0, Limits { connect: WAIT, ..limits() });
    let creds = Some(("u", "tarae-fake-token-7"));
    let get = "GET http://example.invalid/ HTTP/1.1\r\n\r\n".to_string();
    let connect = connect_head("example.invalid:443");
    let cases: Vec<(Upstreams, &String, &str)> = vec![
        (
            via_socks("socks5h", refusing.at, creds),
            &connect,
            "it refused the credentials in tarae's ALL_PROXY",
        ),
        (
            via_socks("socks5h", no_way.at, None),
            &connect,
            "it asks for credentials (or a way tarae doesn't speak), and tarae's ALL_PROXY gives none",
        ),
        (via_socks("socks5h", no_way.at, creds), &connect, "a way to sign in tarae doesn't speak"),
        (
            via_socks("socks5", no_host.at, creds),
            &connect,
            "it refused it: its rules don't allow it (SOCKS5 reply 2)",
        ),
        (
            via_socks("socks5", no_host.at, creds),
            &get,
            "it refused it: its rules don't allow it (SOCKS5 reply 2)",
        ),
        (
            via_socks("socks5", late.at, creds),
            &connect,
            "it refused it: its rules don't allow it (SOCKS5 reply 2)",
        ),
        (via_socks("socks5h", quiet.at, creds), &connect, "it didn't answer within 0.4 s"),
        (via_socks("socks5h", closed, creds), &connect, "it can't be reached (connection refused)"),
    ];
    for (upstream, request, why) in cases {
        let proxy = if why.contains("didn't answer") { &quick.proxy } else { &f.proxy };
        let (mut c, d) = connection(proxy, 1, 1, rules_via(&["example.invalid"], upstream));
        c.write_all(request.as_bytes()).unwrap();
        let head = head_of(&mut c);
        assert!(head.starts_with("HTTP/1.1 502 "), "{why}: {head}");
        let denial = d.recv_timeout(WAIT).expect("said");
        assert_eq!(denial.op, UPSTREAM_OP, "{why}: {}", denial.target);
        assert!(denial.target.contains(why), "{why}: {}", denial.target);
        assert!(!denial.target.contains("127.0.0.1"), "{}", denial.target);
        let home = Path::new("/nonexistent-home");
        assert_eq!(crate::blocked::choices(&[denial], home, Path::new("/w/p"), &[]), [], "{why}");
    }
}

/// A small fixed-seed generator: the same credentials every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// `n` characters that a URL must encode, or that end a part of it if left bare.
    fn awkward(&mut self, n: usize, from: &[u8]) -> String {
        (0..n).map(|_| char::from(from[(self.next() % from.len() as u64) as usize])).collect()
    }
}

/// The credentials in tarae's proxy variable go to the upstream and nowhere else. Random fake ones — `@ : % /`
/// and more in them, percent-encoded as a URL holds them — through every way a connection goes: let through,
/// refused before the upstream, a ClientHello for another server, the upstream's 407s, its 403, unreachable,
/// silent, a plain request and its 407, an https:// and a SOCKS4 upstream; through SOCKS5, a tunnel, a plain
/// request, its sign-in refused. What the run gets back, every refusal as its report says it, tarae's log, the
/// run's rules line, the policy's `Debug` and key, and the run's environment hold none of the user, the password
/// (as written or encoded) or the `Basic` value; the upstream gets them once a request — as `Basic` (http), as
/// RFC 1929's sign-in (SOCKS5).
#[test]
fn upstream_credentials_go_only_to_the_upstream() {
    let (opening, basic, negotiate, refusing, quiet, webs) = (
        outside(opens),
        outside(asks_basic),
        outside(asks_negotiate),
        outside(forbids),
        outside(silent),
        outside(web),
    );
    let (socks_tunnel, socks_plain, socks_refusing) =
        (outside(socks_echo), outside(socks_web), outside(socks_refuses_sign_in));
    let closed = TcpListener::bind((LO, 0)).unwrap().local_addr().unwrap();
    let limits = Limits { hold: Duration::from_millis(200), connect: Duration::from_millis(300), ..limits() };
    let f = start(resolver(Vec::new()).0, limits);
    let mut rng = Rng(0x5ec2_e75e_ed00_0005);
    let mut seen: Vec<String> = Vec::new();
    let mut forms: Vec<String> = Vec::new();
    let captured = crate::log::capture();
    for _ in 0..6 {
        let (u, p) = (format!("{:016x}", rng.next()), format!("{:016x}", rng.next()));
        let user = format!("tarae-fake-user-{u}{}", rng.awkward(3, b"@%/?# "));
        let password = format!("tarae-fake-token-{p}{}", rng.awkward(4, b"@:%/?#& "));
        let value = parse::base64(format!("{user}:{password}").as_bytes());
        let header = format!("Proxy-Authorization: Basic {value}\r\n").into_bytes();
        let sign_in =
            [&[1, user.len() as u8][..], user.as_bytes(), &[password.len() as u8], password.as_bytes()]
                .concat();
        let url = |scheme: &str, at: SocketAddr| format!("{scheme}://{}:{}@{at}", pct(&user), pct(&password));
        let named = |scheme: &str, at| {
            let u = url(scheme, at);
            up::of(&env(&[("HTTPS_PROXY", u.clone()), ("http_proxy", u)]), Setting::Env)
        };
        let connect = connect_head("example.invalid:443");
        let hello = [connect.as_bytes(), curl_hello()].concat();
        let other = [connect.as_bytes(), cargo_hello()].concat();
        let get = b"GET http://example.invalid/x HTTP/1.1\r\nContent-Length: 2\r\n\r\nok".to_vec();
        // (the request, the upstream it names, and the stand-in that gets it with what carries the credentials
        // there — None: it reaches none)
        let (http, socks) = (&header[..], &sign_in[..]);
        type Reaches<'a> = Option<(&'a Outside, &'a [u8])>;
        let paths: Vec<(Vec<u8>, Upstreams, Reaches)> = vec![
            (hello.clone(), named("http", opening.at), Some((&opening, http))),
            (connect_head("refused.example.invalid:443").into_bytes(), named("http", opening.at), None),
            (other, named("http", opening.at), Some((&opening, http))),
            (hello.clone(), named("http", basic.at), Some((&basic, http))),
            (hello.clone(), named("http", negotiate.at), Some((&negotiate, http))),
            (hello.clone(), named("http", refusing.at), Some((&refusing, http))),
            (hello.clone(), named("http", closed), None),
            (hello.clone(), named("http", quiet.at), Some((&quiet, http))),
            (get.clone(), named("http", webs.at), Some((&webs, http))),
            (get.clone(), named("http", basic.at), Some((&basic, http))),
            (hello.clone(), named("https", opening.at), None),
            (hello.clone(), named("socks4a", opening.at), None),
            (hello.clone(), named("socks5h", socks_tunnel.at), Some((&socks_tunnel, socks))),
            (get, named("socks5", socks_plain.at), Some((&socks_plain, socks))),
            (hello, named("socks5h", socks_refusing.at), Some((&socks_refusing, socks))),
        ];
        for (request, upstream, reaches) in paths {
            seen.push(format!("{upstream:?}"));
            let (mut c, d) = connection(&f.proxy, 1, 1, rules_via(&["example.invalid"], upstream));
            c.write_all(&request).unwrap();
            c.shutdown(std::net::Shutdown::Write).unwrap();
            let mut back = Vec::new();
            let _ = c.read_to_end(&mut back);
            seen.push(String::from_utf8_lossy(&back).into_owned());
            seen.extend(d.try_iter().flat_map(|d| [d.target.clone(), d.reported()]));
            if let Some((o, carried)) = reaches {
                let got = o.got.recv_timeout(WAIT).expect("it reached the upstream");
                let n = got.windows(carried.len()).filter(|w| w == &carried).count();
                assert_eq!(n, 1, "once a request: {}", String::from_utf8_lossy(&got));
            }
        }
        // The run's side of it: a policy made with this upstream — its rules line, `Debug`, key, environment
        let policy = crate::sandbox::Policy::for_project(&crate::sandbox::Env {
            root: "/w/p".into(),
            network: true,
            upstream: named("http", opening.at),
            ..Default::default()
        });
        seen.push(crate::sandbox::Guard::In(Box::new(policy.clone())).rules());
        seen.push(format!("{policy:?}"));
        seen.push(crate::sandbox::policy_key(&policy));
        let cmd = crate::sandbox::command(Some(&policy), "true", &[]);
        seen.extend(cmd.get_envs().map(|(k, v)| format!("{k:?}={v:?}")));
        // (each whole, and its random part alone: a part of it anywhere is a leak too)
        forms.extend([user.clone(), password.clone(), pct(&user), pct(&password), value, u, p]);
    }
    seen.extend(captured.lines());
    drop(captured);
    assert!(seen.iter().any(|s| s.contains("the upstream proxy 127.0.0.1:")), "the log says where it is");
    for form in &forms {
        for s in &seen {
            assert!(!s.contains(form.as_str()), "{form:?} in {s:?}");
        }
    }
}
