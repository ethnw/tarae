//! 6d: a run's connection the session's network took to a planted socket (`ToProxy::Planted`), against the same
//! stand-ins as the supervisor's (`super`): its host is what its first bytes name, its port where it was going, and a
//! refusal is a reset — the client thinks it reached its server, so no proxy's answer would mean anything to it.

use super::*;

/// Where the session's network found it was going (a documentation address: nothing's there — tarae connects by
/// the name's own addresses, the resolver's stand-in).
fn beyond(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), port)
}

/// What the planted socket hands the proxy for a run's connection, stood in: a TCP pair, one end in the message
/// with these rules and where it was going — the run's end back, and the sender its refusals come on.
fn planted(p: &Proxy, run: u64, rules: Egress, original: SocketAddr) -> (TcpStream, Receiver<Denial>) {
    let l = TcpListener::bind((LO, 0)).unwrap();
    let run_end = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (ours, _) = l.accept().unwrap();
    run_end.set_read_timeout(Some(WAIT)).unwrap();
    let (told, said) = mpsc::channel();
    let fd = ours.into();
    let m = Planted { fd, original, run, session: 1, process: "cargo".into(), rules: Arc::new(rules), told };
    assert!(p.send(ToProxy::Planted(m)));
    (run_end, said)
}

/// Whether the run's end was reset: its read fails `ECONNRESET` — not an end of data a tool would take as an answer.
fn was_reset(c: &mut TcpStream) -> bool {
    let mut b = [0u8; 64];
    loop {
        match c.read(&mut b) {
            Ok(0) => return false,
            Ok(_) => continue,
            Err(e) => return e.kind() == io::ErrorKind::ConnectionReset,
        }
    }
}

/// A connection the session's network took is judged by the host its first bytes name, at the port it was going
/// to: a ClientHello's server name — the hello and what follows reach the name's own address, as it was sent, and
/// the answer comes back — or a plain request's `Host`, its head the one the proxy sends on (origin form, its
/// `Host`, `Connection: close`) and its body after it.
#[test]
fn a_taken_connection_is_judged_by_what_it_names() {
    let (tls, web_out) = (outside(echo), outside(web));
    let names = vec![("example.invalid", vec![vec![tls.at]]), ("web.test", vec![vec![web_out.at]])];
    let f = start(resolver(names).0, limits());
    let (mut c, _) = planted(&f.proxy, 1, rules(&["example.invalid"]), beyond(443));
    c.write_all(curl_hello()).unwrap();
    let mut back = vec![0u8; curl_hello().len()];
    c.read_exact(&mut back).unwrap();
    assert_eq!(back, curl_hello(), "the outside's echo comes back");
    c.write_all(b"ping").unwrap();
    let mut pong = [0u8; 4];
    c.read_exact(&mut pong).unwrap();
    drop(c);
    assert_eq!(tls.got.recv_timeout(WAIT).unwrap(), [curl_hello(), b"ping"].concat());
    let (mut c, _) = planted(&f.proxy, 1, rules(&["web.test"]), beyond(80));
    c.write_all(b"POST /f HTTP/1.1\r\nHost: web.test\r\nContent-Length: 3\r\n\r\nx=1").unwrap();
    assert_eq!(status(&mut c), 200);
    let got = String::from_utf8(web_out.got.recv_timeout(WAIT).unwrap()).unwrap();
    assert!(got.starts_with("POST /f HTTP/1.1\r\nHost: web.test\r\n"), "{got}");
    assert!(got.contains("\r\nConnection: close\r\n") && got.ends_with("\r\n\r\nx=1"), "{got}");
}

/// A host its run's rules don't name is asked — the card, as for a CONNECT — and with no answer refused: a TLS
/// connection reset (the tool reports a reset), a plain request answered 403; each said with its host first (what
/// `:sandbox-blocked` offers) and that the tool saw a reset; nothing of it reaches the outside.
#[test]
fn a_host_not_allowed_resets_a_taken_connection() {
    let out = outside(echo);
    let (resolve, _) =
        resolver(vec![("example.invalid", vec![vec![out.at]]), ("web.test", vec![vec![out.at]])]);
    let f = start(resolve, Limits { hold: Duration::from_millis(300), ..limits() });
    let (mut c, d) = planted(&f.proxy, 1, rules(&["other.test"]), beyond(443));
    c.write_all(curl_hello()).unwrap();
    assert!(
        matches!(f.asks.recv_timeout(WAIT), Ok(FromProxy::Ask { host, port: 443, .. }) if host == "example.invalid")
    );
    assert!(was_reset(&mut c), "reset, not closed");
    let target = said_host(&d);
    assert!(target.starts_with("example.invalid:443: ") && target.contains("didn't answer"), "{target}");
    assert!(target.ends_with("the tool sees its connection reset"), "{target}");
    let (mut c, d) = planted(&f.proxy, 1, rules(&["other.test"]), beyond(80));
    c.write_all(b"GET / HTTP/1.1\r\nHost: web.test\r\n\r\n").unwrap();
    assert_eq!(status(&mut c), 403);
    assert!(said_host(&d).starts_with("web.test:80: "));
    assert_eq!(out.accepted.load(Ordering::SeqCst), 0, "nothing reached the outside");
}

/// What names no host is refused and said — reset: bytes that are neither TLS nor a request tarae reads (ssh's
/// banner), nothing at all within the head's time (a server that speaks first), a request naming an address (403:
/// rules name hosts); and a cloud metadata server's own name, whatever the rules say.
#[test]
fn what_names_no_host_is_refused() {
    let out = outside(echo);
    let (resolve, _) = resolver(vec![("metadata.google.internal", vec![vec![out.at]])]);
    let f = start(resolve, limits());
    let any = || Egress { any: true, ..rules(&[]) };
    let (mut c, d) = planted(&f.proxy, 1, any(), beyond(22));
    c.write_all(b"SSH-2.0-OpenSSH_9.9\r\n\r\n").unwrap();
    assert!(was_reset(&mut c));
    let target = said(&d);
    assert!(target.starts_with("203.0.113.7:22: neither TLS nor a request tarae reads"), "{target}");
    let (mut c, d) = planted(&f.proxy, 1, any(), beyond(3306));
    assert!(was_reset(&mut c), "a server that speaks first: nothing sent, reset");
    assert!(said(&d).contains("nothing sent within"));
    let (mut c, d) = planted(&f.proxy, 1, any(), beyond(80));
    c.write_all(b"GET / HTTP/1.1\r\nHost: 93.184.216.34\r\n\r\n").unwrap();
    assert_eq!(status(&mut c), 403);
    assert!(said(&d).contains("an address, not a name"));
    let (mut c, d) = planted(&f.proxy, 1, any(), beyond(80));
    c.write_all(b"GET / HTTP/1.1\r\nHost: metadata.google.internal\r\n\r\n").unwrap();
    assert_eq!(status(&mut c), 403);
    assert!(said(&d).contains("never reached"));
    assert_eq!(out.accepted.load(Ordering::SeqCst), 0);
}
