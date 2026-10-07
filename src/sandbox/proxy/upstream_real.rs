//! The upstream proxy through the real path, on both systems (step 5g): a sandboxed run's `curl` → tarae's proxy
//! (the one `get` gives; macOS through the run's own port and Seatbelt, Linux through the supervisor) → a stand-in
//! upstream on loopback — an http one, a SOCKS5 one — named as tarae's environment would name it. Nothing leaves
//! this machine: the hosts are `example.invalid` names the upstream never looks up — it only records what comes.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::packs_tests::{finish, on_path, skip};
use super::{parse, upstream};

/// The host the run asks for.
const NAME: &str = "e2e-5g.example.invalid";

/// What the stand-in upstream got on one connection: what asked it for the way there (an http CONNECT's or plain
/// request's head; SOCKS5's greeting, sign-in and CONNECT), the plain request that went that way (its head), and
/// through a tunnel the server its ClientHello names.
struct Got {
    asked: Vec<u8>,
    request: String,
    hello: Option<String>,
}

fn read_some(s: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 4096];
    match s.read(&mut chunk) {
        Ok(n @ 1..) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
        _ => false,
    }
}

/// What comes once the way is open (`buf`: what came already): a ClientHello — its server name kept, the tunnel
/// closed, no TLS server behind it — or a plain request, answered `200 ok`.
fn after(s: &mut TcpStream, mut buf: Vec<u8>) -> (String, Option<String>) {
    if buf.is_empty() && !read_some(s, &mut buf) {
        return (String::new(), None);
    }
    if buf[0] == 22 {
        loop {
            match parse::server_name(&buf) {
                Ok(None) if read_some(s, &mut buf) => {}
                Ok(Some(n)) => return (String::new(), Some(n)),
                _ => return (String::new(), None),
            }
        }
    }
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && read_some(s, &mut buf) {}
    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    (String::from_utf8_lossy(&buf).into_owned(), None)
}

/// An http upstream: a plain request answered as the host would; a CONNECT answered 200, then what comes.
fn serve_http(mut s: TcpStream) -> Got {
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = Vec::new();
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && read_some(&mut s, &mut buf) {}
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").map_or(buf.len(), |i| i + 4);
    let rest = buf.split_off(end);
    if !buf.starts_with(b"CONNECT ") {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let request = String::from_utf8_lossy(&buf).into_owned();
        return Got { asked: buf, request, hello: None };
    }
    let _ = s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
    let (request, hello) = after(&mut s, rest);
    Got { asked: buf, request, hello }
}

/// A SOCKS5 upstream (RFC 1928, 1929): a user and password where offered, taken; every CONNECT granted; then
/// what comes.
fn serve_socks(mut s: TcpStream) -> Got {
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut asked = Vec::new();
    let take = |s: &mut TcpStream, n: usize, asked: &mut Vec<u8>| {
        let mut b = vec![0u8; n];
        let ok = s.read_exact(&mut b).is_ok();
        asked.extend_from_slice(&b);
        ok
    };
    let none = |asked| Got { asked, request: String::new(), hello: None };
    if !take(&mut s, 2, &mut asked) || !take(&mut s, usize::from(asked[1]), &mut asked) {
        return none(asked);
    }
    let way = if asked[2..].contains(&2) { 2 } else { 0 };
    let _ = s.write_all(&[5, way]);
    if way == 2 {
        let at = asked.len();
        if !take(&mut s, 2, &mut asked)
            || !take(&mut s, usize::from(asked[at + 1]) + 1, &mut asked)
            || !take(&mut s, usize::from(asked[asked.len() - 1]), &mut asked)
        {
            return none(asked);
        }
        let _ = s.write_all(&[1, 0]);
    }
    let at = asked.len();
    if !take(&mut s, 5, &mut asked) || !take(&mut s, usize::from(asked[at + 4]) + 2, &mut asked) {
        return none(asked);
    }
    let _ = s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    let (request, hello) = after(&mut s, Vec::new());
    Got { asked, request, hello }
}

/// A sandboxed run with the network open, where tarae's environment names the stand-in upstream `serve` as
/// `named` gives it (its address in): the run's plain request, its TLS tunnel, and its own connect straight to the
/// upstream's port — what the run printed, and what the upstream got. None: skipped (said), where no sandbox can
/// start here (macOS inside one; Linux without the Landlock tarae uses or a supervisor), or without curl on PATH.
fn run_through(
    test: &str,
    named: fn(SocketAddr) -> Vec<(&'static str, String)>,
    serve: fn(TcpStream) -> Got,
) -> Option<(String, Vec<Got>)> {
    if on_path("curl").is_none() {
        skip(test, "no curl on PATH");
        return None;
    }
    #[cfg(not(target_os = "linux"))]
    if crate::sandbox::skipped_inside(&format!("sandbox::proxy::{test}")) {
        return None;
    }
    #[cfg(target_os = "linux")]
    if crate::landlock::abi().is_ok_and(|n| n < 2) || crate::landlock::abi().is_err() {
        skip(test, "no Landlock tarae uses");
        return None;
    }
    #[cfg(target_os = "linux")]
    if let Some(why) = super::super::supervisor::unavailable() {
        skip(test, &why);
        return None;
    }
    // (under the build's `target/`, not `/tmp`: a run writes there)
    let short = test.rsplit("::").next().unwrap_or(test);
    let base =
        std::env::current_dir().unwrap().join(format!("target/tarae-5g-{short}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let root = std::fs::canonicalize(&base).unwrap();
    // The stand-in upstream: every connection it takes until the run is over
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let at = l.local_addr().unwrap();
    l.set_nonblocking(true).unwrap();
    let over = Arc::new(AtomicBool::new(false));
    let done = over.clone();
    let took = crate::event::thread("tarae-5g-test", move || {
        let mut got = Vec::new();
        while !done.load(Ordering::SeqCst) {
            match l.accept() {
                Ok((s, _)) => {
                    s.set_nonblocking(false).unwrap();
                    got.push(serve(s));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        got
    });
    let llm = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
    #[allow(unused_mut)]
    let mut p = crate::sandbox::policy(&root, &llm, &[]);
    // (as tarae's environment names it — a test's own `Here` keeps no proxy variable: `Here::now`)
    let vars = named(at).into_iter().map(|(k, v)| (k.to_string(), v.into())).collect();
    p.egress.upstream = upstream::of(&vars, upstream::Setting::Env);
    assert!(p.proxy_url().is_some(), "the run has a proxy URL");
    let script = format!(
        "curl -s --max-time 10 http://{NAME}/through; echo; \
         curl -s --max-time 10 -o /dev/null https://{NAME}/tls; echo \"tls=$?\"; \
         curl -s --noproxy '*' --max-time 3 http://{at}/direct; echo \"direct=$?\""
    );
    let mut cmd = crate::sandbox::command(Some(&p), "/bin/sh", &["-c".into(), script]);
    cmd.current_dir(&root);
    let ended = crate::sandbox::after_run(Some(&p));
    let (_, out, err) = finish(crate::sandbox::spawn(Some(&p), &cmd).expect("the run starts"), 60);
    ended();
    over.store(true, Ordering::SeqCst);
    let got = took.join().unwrap();
    #[cfg(target_os = "linux")]
    if let Some(Ok(c)) = &p.confine
        && let Some(k) = &c.keeper
    {
        k.end();
    }
    std::fs::remove_dir_all(&base).ok();
    Some((format!("{out}\n— stderr —\n{err}"), got))
}

/// Through an http upstream (with fake credentials): the run's plain request and its TLS tunnel both reach it —
/// the request in absolute form, the tunnel as `CONNECT host:443` with the run's ClientHello after it, naming that
/// host — each with tarae's credentials, never the run's token. The run's own connect to the upstream's port is
/// refused: it isn't opened, so nothing gets around tarae's host checks to it.
#[test]
fn a_run_goes_out_through_the_upstream() {
    let named = |at| {
        let url = format!("http://u:tarae-fake-token-5g@{at}");
        vec![("HTTPS_PROXY", url.clone()), ("http_proxy", url)]
    };
    let Some((said, got)) =
        run_through("upstream_real::a_run_goes_out_through_the_upstream", named, serve_http)
    else {
        return;
    };
    let basic = format!("Proxy-Authorization: Basic {}\r\n", parse::base64(b"u:tarae-fake-token-5g"));
    assert!(said.starts_with("ok\n"), "through the upstream: {said}");
    assert_eq!(got.len(), 2, "the plain request and the tunnel, nothing direct: {said}");
    assert!(
        got[0].request.starts_with(&format!("GET http://{NAME}/through HTTP/1.1\r\n")),
        "{}",
        got[0].request
    );
    let tunnel = String::from_utf8_lossy(&got[1].asked);
    assert!(
        tunnel.starts_with(&format!("CONNECT {NAME}:443 HTTP/1.1\r\n"))
            && got[1].hello.as_deref() == Some(NAME),
        "{tunnel} — its ClientHello's name: {:?}",
        got[1].hello
    );
    for g in &got {
        let asked = String::from_utf8_lossy(&g.asked);
        assert_eq!(asked.matches(&basic).count(), 1, "tarae's credentials, once: {asked}");
        assert!(!asked.contains("run:") && asked.matches("Proxy-Authorization").count() == 1, "{asked}");
    }
    assert!(said.contains("direct=7"), "the run couldn't connect to the upstream itself: {said}");
}

/// Through a SOCKS5 upstream named as a local client names it (`ALL_PROXY=socks5h://…`, fake credentials): the
/// run's plain request and its TLS tunnel both reach it, each asked by the host's name (never an address), signed
/// in with tarae's credentials (RFC 1929) — the request then in origin form with no `Proxy-*`, the tunnel's
/// ClientHello naming the host. The run's own connect to the upstream's port is refused.
#[test]
fn a_run_goes_out_through_a_socks5_upstream() {
    let named = |at| vec![("ALL_PROXY", format!("socks5h://u:tarae-fake-token-5g@{at}"))];
    let Some((said, got)) =
        run_through("upstream_real::a_run_goes_out_through_a_socks5_upstream", named, serve_socks)
    else {
        return;
    };
    let password = b"tarae-fake-token-5g";
    let signed = [&[5, 2, 0, 2, 1, 1, b'u', password.len() as u8][..], password].concat();
    let connect =
        |port: u16| [&[5, 1, 0, 3, NAME.len() as u8][..], NAME.as_bytes(), &port.to_be_bytes()].concat();
    assert!(said.starts_with("ok\n"), "through the upstream: {said}");
    assert_eq!(got.len(), 2, "the plain request and the tunnel, nothing direct: {said}");
    assert_eq!(got[0].asked, [&signed[..], &connect(80)].concat(), "{said}");
    assert!(got[0].request.starts_with("GET /through HTTP/1.1\r\n"), "origin form: {}", got[0].request);
    assert!(!got[0].request.to_ascii_lowercase().contains("proxy-"), "{}", got[0].request);
    assert_eq!(got[1].asked, [&signed[..], &connect(443)].concat(), "{said}");
    assert_eq!(got[1].hello.as_deref(), Some(NAME), "its ClientHello's name");
    assert!(said.contains("direct=7"), "the run couldn't connect to the upstream itself: {said}");
}
