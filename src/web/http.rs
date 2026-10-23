//! A page's address as the fetch asks for it, and the request it sends (`target/sb/web-fetch-plan.md` §2.1, D7):
//! `https://` (or `http://`, made `https://` but on this machine — D4), a host by its name — never a user or password
//! in the address, never a bare address but this machine's (the proxy names hosts, as runs reach them), not yet a name
//! that isn't ASCII (IDNA's tables: later, their own call) —; a GET with nothing of the user's in it: no cookie, no
//! credentials, no referrer, the fragment never sent. Pure.

use std::net::IpAddr;

/// Where a fetch goes, and what it asks there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub https: bool,
    /// A name as the proxy takes it (lowercase, no trailing dot), or this machine's address.
    pub host: String,
    pub port: u16,
    /// The request's target: its path and query, percent-encoded where they hold what a request line can't.
    pub path: String,
    /// This machine (`localhost`, a loopback address): reached directly, as runs reach it — never through the proxy.
    pub loopback: bool,
    /// Asked as `http://` of a host beyond this machine: read over https instead.
    pub upgraded: bool,
}

impl Target {
    /// `host[:port]` as the `Host` field and a URL write it (an IPv6 address in brackets, the scheme's port left out).
    pub fn authority(&self) -> String {
        let host = match self.host.contains(':') {
            true => format!("[{}]", self.host),
            false => self.host.clone(),
        };
        match self.port == if self.https { 443 } else { 80 } {
            true => host,
            false => format!("{host}:{}", self.port),
        }
    }

    /// Its address, as fetched (no fragment).
    pub fn url(&self) -> String {
        format!("{}://{}{}", if self.https { "https" } else { "http" }, self.authority(), self.path)
    }
}

/// `url` as the fetch goes there — or why it doesn't, in words.
pub fn target(url: &str) -> Result<Target, String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("not an address to fetch: {url}"))?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(format!("only https:// and http:// pages are fetched, not {scheme}://")),
    };
    let rest = rest.split('#').next().unwrap_or_default();
    let cut = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(cut);
    if authority.contains('@') {
        return Err(
            "an address with a user or password in it isn't fetched: tarae never sends credentials".into()
        );
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (addr, after) = v6.split_once(']').ok_or("an IPv6 address without its `]`")?;
            (addr, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    let port = match port {
        None => {
            if https {
                443
            } else {
                80
            }
        }
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 && p.bytes().all(|c| c.is_ascii_digit()) => n,
            _ => return Err(format!("a port that isn't 1–65535: {p}")),
        },
    };
    if host.is_empty() {
        return Err(format!("an address without a host: {url}"));
    }
    if !host.is_ascii() {
        return Err(format!("a host name that isn't ASCII isn't fetched yet: {host}"));
    }
    let (host, loopback) = match host.parse::<IpAddr>() {
        Ok(ip) if ip.to_canonical().is_loopback() => (ip.to_string(), true),
        Ok(_) => return Err(format!("a bare address isn't fetched — a page by its host's name: {host}")),
        Err(_) => {
            let name =
                crate::sandbox::proxy::parse::name(host).map_err(|why| format!("its host {host}: {why}"))?;
            let loopback = name == "localhost";
            (name, loopback)
        }
    };
    let upgraded = !https && !loopback;
    let port = match (upgraded, port) {
        (true, 80) => 443,
        (_, p) => p,
    };
    Ok(Target { https: https || upgraded, host, port, path: encoded(path), loopback, upgraded })
}

/// A path and query as a request line takes them: `/` where there's none; a control character, a space, what isn't
/// ASCII and `"<>\`{}|\^` percent-encoded as UTF-8 (WHATWG's path set, near enough) — a `%` already there kept.
fn encoded(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    if !path.starts_with('/') {
        out.push('/');
    }
    for &b in path.as_bytes() {
        match b {
            0..=0x20 | 0x7f.. | b'"' | b'<' | b'>' | b'`' | b'{' | b'}' | b'|' | b'\\' | b'^' => {
                out.push_str(&format!("%{b:02X}"))
            }
            _ => out.push(char::from(b)),
        }
    }
    out
}

/// What the fetch asks for: the page, as a reader — its markup first, gzip taken (`gzip.rs`), one request a
/// connection. Nothing else, ever (D7).
pub fn request(t: &Target) -> Vec<u8> {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: tarae/{} (reader)\r\nAccept: text/html, text/markdown;q=0.9, \
         text/plain;q=0.8\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
        t.path,
        t.authority(),
        env!("CARGO_PKG_VERSION"),
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An address is fetched as it says — its port, its path and query encoded, its fragment left — `http://` made
    /// `https://` but on this machine; what it can't be is refused in words.
    #[test]
    fn an_address_is_fetched_as_it_says() {
        let t = target("HTTPS://Docs.RS./ropey/latest/ropey/struct.Rope.html?q=a b#method.len").unwrap();
        assert_eq!((t.https, t.host.as_str(), t.port, t.loopback), (true, "docs.rs", 443, false));
        assert_eq!(t.path, "/ropey/latest/ropey/struct.Rope.html?q=a%20b");
        assert_eq!(t.url(), "https://docs.rs/ropey/latest/ropey/struct.Rope.html?q=a%20b");
        assert_eq!(target("https://example.com").unwrap().path, "/");
        assert_eq!(target("https://example.com?x").unwrap().path, "/?x");
        assert_eq!(target("https://example.com/é").unwrap().path, "/%C3%A9");
        let up = target("http://example.com/a").unwrap();
        assert_eq!(
            (up.https, up.port, up.upgraded, up.url().as_str()),
            (true, 443, true, "https://example.com/a")
        );
        let local = target("http://localhost:8000/docs").unwrap();
        assert_eq!(
            (local.https, local.loopback, local.authority().as_str()),
            (false, true, "localhost:8000")
        );
        let v6 = target("https://[::1]:8443/").unwrap();
        assert_eq!((v6.host.as_str(), v6.loopback, v6.authority().as_str()), ("::1", true, "[::1]:8443"));
        assert!(target("http://127.0.0.1/").unwrap().loopback);
        for (url, said) in [
            ("https://user:pw@example.com/", "credentials"),
            ("https://93.184.215.14/", "bare address"),
            ("https://[2606:2800::1]/", "bare address"),
            ("https://127.1/", "its host"),
            ("https://bücher.de/", "isn't ASCII"),
            ("ftp://example.com/", "only https://"),
            ("example.com", "not an address"),
            ("https://example.com:0/", "port"),
            ("https://example.com:99999/", "port"),
            ("https:///path", "without a host"),
        ] {
            let e = target(url).unwrap_err();
            assert!(e.contains(said), "{url}: {e}");
        }
    }

    /// The request asks for the page alone: its path, its host, who reads it, gzip taken, the connection closed after —
    /// no cookie, credentials or referrer, ever (D7).
    #[test]
    fn the_request_holds_nothing_of_the_user_s() {
        let r = String::from_utf8(request(&target("https://example.com:8443/a?b#c").unwrap())).unwrap();
        assert!(r.starts_with("GET /a?b HTTP/1.1\r\nHost: example.com:8443\r\nUser-Agent: tarae/"), "{r}");
        assert!(r.contains("\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n"), "{r}");
        assert_eq!(r.matches("\r\n").count(), 7, "the request line and five fields: {r}");
        for never in ["cookie", "authorization", "referer", "#c"] {
            assert!(!r.to_ascii_lowercase().contains(never), "{never}: {r}");
        }
    }
}
