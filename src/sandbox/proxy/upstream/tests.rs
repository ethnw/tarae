use super::*;

fn env(vars: &[(&str, &str)]) -> HashMap<String, OsString> {
    vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect()
}

/// The variable a way through names, or `direct` / `unusable: why`.
fn way_of(u: &Upstreams, host: &str, port: u16, tunnel: bool) -> String {
    match u.way(host, port, tunnel) {
        Way::Direct => "direct".into(),
        Way::Through(up) => format!("{} {}", up.var, up.at()),
        Way::Unusable(e) => format!("unusable: {}", e.why),
    }
}

/// The upstream comes from tarae's own environment as the tools read theirs: a tunnel's from `https_proxy`, then
/// `HTTPS_PROXY`, then `ALL_PROXY` — lowercase before uppercase (curl's order), a plain request's from
/// `http_proxy`/`HTTP_PROXY` — an empty value is none; `llm.sandbox-upstream = "none"` takes none. An upstream
/// tarae can't speak to fails closed — that way goes nowhere, said without its value (it may hold a password) —
/// never direct.
#[test]
fn the_upstream_comes_from_taraes_environment() {
    let u = of(
        &env(&[
            ("HTTPS_PROXY", "http://upper.example.invalid:3128"),
            ("https_proxy", "http://lower.example.invalid:3128"),
            ("HTTP_PROXY", "plain.example.invalid:8080"),
        ]),
        Setting::Env,
    );
    assert_eq!(way_of(&u, "a.example.invalid", 443, true), "https_proxy lower.example.invalid:3128");
    assert_eq!(way_of(&u, "a.example.invalid", 80, false), "HTTP_PROXY plain.example.invalid:8080");
    assert_eq!(u.mixed, [("https_proxy", "HTTPS_PROXY")], "both cases, differing: noted");
    let all = of(&env(&[("ALL_PROXY", "http://all.example.invalid"), ("https_proxy", "")]), Setting::Env);
    assert_eq!(
        way_of(&all, "a.example.invalid", 443, true),
        "ALL_PROXY all.example.invalid:80",
        "empty: unset"
    );
    assert_eq!(way_of(&all, "a.example.invalid", 80, false), "ALL_PROXY all.example.invalid:80");
    let none = of(&env(&[("HTTPS_PROXY", "http://p.example.invalid:3128")]), Setting::None);
    assert_eq!(way_of(&none, "a.example.invalid", 443, true), "direct");
    assert_eq!(none.said(), None);
    assert_eq!(way_of(&of(&env(&[]), Setting::Env), "a.example.invalid", 443, true), "direct");
    let secret = "tarae-fake-token-7";
    for (url, why) in [
        (format!("https://u:{secret}@p.example.invalid:3128"), "https://"),
        (format!("socks4a://u:{secret}@p.example.invalid:1080"), "other than SOCKS5"),
        (format!("socks://u:{secret}@p.example.invalid:1080"), "other than SOCKS5"),
        // (RFC 1929 carries 1–255 bytes of user, at most 255 of password)
        (format!("socks5h://{}:{secret}@p.example.invalid", "u".repeat(256)), "SOCKS5 can't carry"),
        (format!("socks5h://:{secret}@p.example.invalid"), "SOCKS5 can't carry"),
        (format!("ftp://u:{secret}@p.example.invalid"), "isn't a proxy URL"),
        (format!("http://u:{secret}@p.example.invalid:99999"), "isn't a proxy URL"),
        (format!("http://u:{secret}%zz@p.example.invalid:3128"), "isn't a proxy URL"),
        (format!("http://u:{secret}@[::1:3128"), "isn't a proxy URL"),
    ] {
        let u = of(&env(&[("HTTPS_PROXY", &url)]), Setting::Env);
        let said = way_of(&u, "a.example.invalid", 443, true);
        assert!(said.starts_with("unusable: ") && said.contains(why), "{url}: {said}");
        let report = u.said().unwrap();
        assert!(
            report.contains("https nowhere — tarae's own HTTPS_PROXY") && !report.contains(secret),
            "{report}"
        );
        assert!(!format!("{u:?}").contains(secret));
    }
}

/// An upstream's URL as the tools write it: no scheme is http, the port 80 when it has none, an address (IPv6 in
/// brackets) as well as a name, a path after it ignored; its credentials percent-decoded — a password holding
/// `@`, `:` or `%` — and sent as `Basic` exactly as a proxy reads them back; a user alone is `user:`.
#[test]
fn an_upstreams_url_and_credentials() {
    let one = |url: &str| match of(&env(&[("HTTPS_PROXY", url)]), Setting::Env).tunnel {
        Some(Ok(u)) => u,
        other => panic!("{url}: {other:?}"),
    };
    assert_eq!(one("p.example.invalid:3128").at(), "p.example.invalid:3128");
    assert_eq!(one("http://P.Example.Invalid/").at(), "p.example.invalid:80");
    assert_eq!(one("http://[::1]:3128").at(), "[::1]:3128");
    assert_eq!(one("http://127.0.0.1:3128/x").at(), "127.0.0.1:3128");
    let auth = |u: &Upstream| -> Option<(Vec<u8>, Vec<u8>)> {
        let h = String::from_utf8(u.connect_head("a.example.invalid", 443)).unwrap();
        let line = h.lines().find_map(|l| l.strip_prefix("Proxy-Authorization: "))?.to_string();
        match parse::head(
            format!("CONNECT a.test:443 HTTP/1.1\r\nProxy-Authorization: {line}\r\n\r\n").as_bytes(),
        ) {
            Ok(Some(parse::Head { auth: parse::Auth::Basic { user, password }, .. })) => {
                Some((user, password))
            }
            other => panic!("{other:?}"),
        }
    };
    let u = one("http://us%40er:p%40ss%3Aw%25rd@p.example.invalid:3128");
    assert_eq!(auth(&u), Some((b"us@er".to_vec(), b"p@ss:w%rd".to_vec())));
    assert!(u.has_credentials());
    // (an `@` left unencoded in the password: the last one ends the credentials, as Go and curl read it)
    assert_eq!(auth(&one("http://u:a@b@p.example.invalid:3128")), Some((b"u".to_vec(), b"a@b".to_vec())));
    assert_eq!(auth(&one("http://u@p.example.invalid:3128")), Some((b"u".to_vec(), Vec::new())));
    assert_eq!(auth(&one("http://p.example.invalid:3128")), None);
    assert!(!format!("{u:?}").contains("p%40ss") && !format!("{u:?}").contains("us@er"), "{u:?}");
    assert_eq!(format!("{u:?}"), "Upstream(HTTPS_PROXY p.example.invalid:3128, credentials: true)");
    // SOCKS5, either scheme: port 1080 when it has none, the credentials as RFC 1929 carries them
    for scheme in ["socks5h", "SOCKS5"] {
        let s = one(&format!("{scheme}://us%40er:p%3Ass@p.example.invalid"));
        assert_eq!((s.proto, s.at()), (Proto::Socks5, "p.example.invalid:1080".to_string()));
        assert_eq!(s.socks_sign_in(), Some([&[1, 5][..], b"us@er", &[4], b"p:ss"].concat()));
        assert_eq!(
            format!("{s:?}"),
            "Upstream(HTTPS_PROXY socks5 p.example.invalid:1080, credentials: true)"
        );
    }
    assert_eq!(one("socks5h://p.example.invalid:9050").socks_sign_in(), None);
}

/// What tarae says to a SOCKS5 upstream is RFC 1928's and 1929's, byte for byte: its greeting offers none, and a
/// user and password only with credentials; its CONNECT names the host by name (ATYP 3), never an address. A reply
/// is read to its exact end whatever its bound address's type — never past it (what follows is the host's) — and
/// what isn't a SOCKS5 reply is refused.
#[test]
fn socks5_messages_are_rfc_1928s() {
    let one = |url: &str| match of(&env(&[("ALL_PROXY", url)]), Setting::Env).tunnel {
        Some(Ok(u)) => u,
        other => panic!("{url}: {other:?}"),
    };
    assert_eq!(one("socks5h://p.example.invalid").socks_greeting(), [5, 1, 0]);
    assert_eq!(one("socks5h://u:pw@p.example.invalid").socks_greeting(), [5, 2, 0, 2]);
    assert_eq!(
        Upstream::socks_connect("example.invalid", 443),
        [&[5, 1, 0, 3, 15][..], b"example.invalid", &[1, 187]].concat()
    );
    let v4 = [5, 0, 0, 1, 10, 0, 0, 1, 0x1f, 0x90];
    let name = [&[5, 0, 0, 3, 3][..], b"a.b", &[0, 80]].concat();
    let v6 = [&[5, 2, 0, 4][..], &[0; 16], &[0, 0]].concat();
    for (reply, want) in [(&v4[..], (0, 10)), (&name, (0, 10)), (&v6, (2, 22))] {
        assert_eq!(socks_reply(reply), Ok(Some(want)), "{reply:?}");
        // (cut anywhere short of its end: more to come; with what follows it: its own length still)
        for cut in 0..reply.len() {
            assert_eq!(socks_reply(&reply[..cut]), Ok(None), "{reply:?} cut at {cut}");
        }
        assert_eq!(socks_reply(&[reply, b"HTTP/1.1 200"].concat()), Ok(Some(want)));
    }
    for bad in [&b"HTTP/1.1 200 OK"[..], &[5, 0, 1, 1], &[5, 0, 0, 9, 0], &[4, 90, 0, 0]] {
        assert!(socks_reply(bad).is_err(), "{bad:?}");
    }
}

/// `NO_PROXY` (lowercase first) sends a host direct as curl and Go read it: `*` every host; a name, `.name` and
/// `*.name` that name and its subdomains (not a name that only ends the same); `name:port` at that port only;
/// case and a trailing dot don't matter. Addresses and ranges never match a name.
#[test]
fn no_proxy_sends_its_hosts_direct() {
    let u = |no: &str| {
        of(
            &env(&[("HTTPS_PROXY", "http://p.example.invalid:3128"), ("no_proxy", no), ("NO_PROXY", "*")]),
            Setting::Env,
        )
    };
    let through = "HTTPS_PROXY p.example.invalid:3128";
    let cases = [
        ("d.example.invalid", "d.example.invalid", 443, "direct"),
        ("d.example.invalid", "x.d.example.invalid", 443, "direct"),
        ("d.example.invalid", "xd.example.invalid", 443, through),
        (".d.example.invalid", "d.example.invalid", 443, "direct"),
        ("*.d.example.invalid", "y.d.example.invalid", 443, "direct"),
        ("D.Example.Invalid.", "d.example.invalid", 443, "direct"),
        ("d.example.invalid:8443", "d.example.invalid", 443, through),
        ("d.example.invalid:8443", "d.example.invalid", 8443, "direct"),
        ("10.0.0.0/8, 127.0.0.1, [::1], ::1", "d.example.invalid", 443, through),
        (" a.example.invalid ,, d.example.invalid ", "d.example.invalid", 443, "direct"),
        ("*", "anything.example.invalid", 443, "direct"),
    ];
    for (no, host, port, want) in cases {
        assert_eq!(way_of(&u(no), host, port, true), want, "NO_PROXY={no}: {host}:{port}");
    }
    assert_eq!(way_of(&u(""), "a.example.invalid", 443, true), "direct", "an empty no_proxy: NO_PROXY's");
}

/// An upstream's answer is read by its status and what its 407 offers, in tarae's own names — never its words:
/// a head over its limit, or what isn't HTTP, is refused; a challenge's parameters aren't schemes.
#[test]
fn an_upstreams_answer_is_read_by_its_status() {
    let a = answer(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap().unwrap();
    assert_eq!((a.status, a.len), (200, 39));
    assert_eq!(answer(b"HTTP/1.1 200 OK\r\n"), Ok(None), "more to come");
    assert_eq!(answer(b"HTTP/1.").map(|a| a.is_none()), Ok(true));
    assert!(answer(b"SSH-2.0-x\r\n").is_err() && answer(b"HTTP/1.1 2x0 Hm\r\n\r\n").is_err());
    assert!(answer(&[b"HTTP/1.1 200 OK\r\nX: ".to_vec(), vec![b'a'; parse::HEAD_MAX]].concat()).is_err());
    let challenge = b"HTTP/1.1 407 Proxy Authentication Required\r\n\
        Proxy-Authenticate: Negotiate\r\nproxy-authenticate: NTLM, Digest realm=\"r\", qop=\"auth\"\r\n\
        Proxy-Authenticate: Secret-Sauce tarae-fake-token-3\r\n\r\n";
    let a = answer(challenge).unwrap().unwrap();
    assert_eq!((a.status, a.basic), (407, false));
    assert_eq!(a.schemes, ["Negotiate", "NTLM", "Digest", "another scheme"]);
    let basic =
        answer(b"HTTP/1.1 407 x\r\nProxy-Authenticate: Basic realm=\"corp\"\r\n\r\n").unwrap().unwrap();
    assert!(basic.basic);
    assert_eq!(Failed::of_407(&basic, false), Failed::NoCredentials);
    assert_eq!(Failed::of_407(&basic, true), Failed::WrongCredentials);
    let said = Failed::of_407(&a, true).said("HTTPS_PROXY");
    assert_eq!(
        said,
        "through the proxy tarae itself uses (tarae's HTTPS_PROXY): it asks for Negotiate or NTLM or Digest or \
         another scheme (407), which tarae doesn't speak — only Basic"
    );
    assert!(!said.contains("tarae-fake-token"));
}
