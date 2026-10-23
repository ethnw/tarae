//! A page fetched (`target/sb/web-fetch-plan.md`; the user's, 2026-10-23): tarae's own HTTP/1.1 GET, on a worker —
//! never the main thread — through tarae's egress proxy as a run's connection goes (`proxy::pair` and
//! `ToProxy::Connection`, the Linux supervisor's way, on every system): every address refusal, the metadata servers'
//! names, the ClientHello's server name and the user's upstream proxy, as for a run. This machine (`localhost`, a
//! loopback address) is reached directly, as runs reach it. TLS is rustls on ring, verified as the OS verifies
//! (`rustls-platform-verifier`: its keychain, or the distro's bundle — a company's own root works).
//!
//! Yours (`:web`): never asked (the address you typed is the yes — `Egress::any`), the address rules held. Redirects
//! as D5: at most `REDIRECTS_MAX`, another host's followed and said (`Fetched.from`), never from beyond this machine
//! into it. Limits as D6: the body on the wire and what it unpacks to (gzip only, `gzip.rs`) at most `max`, cut and
//! said past it; one deadline to the last byte. Nothing of the user's sent (D7, `http.rs`). `Stop` ends it: its socket
//! shut, its worker's read returns. Fetched pages are the session's for `KEEP` (`Pages`, D11) — Claude's `read` (P4)
//! takes them from there.
//!
//! Claude's (`claudes`, P4 — `tool.rs`): the one host its card let through named in its rules (asked before the
//! fetch starts, on the main loop, so the deadline runs from the answer), `DEADLINE_CLAUDE`, and another host's
//! redirect or refresh not followed — said instead (`Fetched.elsewhere`), its next read asked at its own host.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use super::gzip::{self, Unpacked};
use super::http::{self, Target};
use crate::sandbox::proxy::egress::{self, Egress, Resolve};
use crate::sandbox::proxy::parse::{self, Body, Framing, Response};
use crate::sandbox::proxy::{self, Connection, Proxy, ToProxy};

/// Redirects followed at most, a `<meta refresh>` within 5 s counted as one (D5).
pub const REDIRECTS_MAX: usize = 5;

/// Yours: to the last byte (D6).
pub const DEADLINE: Duration = Duration::from_secs(20);

/// Claude's: to the last byte (D6) — from after its card's answer.
pub const DEADLINE_CLAUDE: Duration = Duration::from_secs(30);

/// A fetched page is the session's this long (D11).
pub const KEEP: Duration = Duration::from_secs(15 * 60);

/// The session keeps this many pages at most, and this many bytes of them.
const PAGES_MAX: usize = 20;
const PAGES_BYTES: usize = 64 << 20;

/// What a fetch goes by.
#[derive(Clone)]
pub struct Way {
    pub proxy: Proxy,
    /// The session as the proxy names it (`proxy::session_of`).
    pub session: u64,
    pub rules: Arc<Egress>,
    /// How this machine's names are looked up (the system's resolver; a stand-in in tests) — the proxy looks up
    /// the rest.
    pub resolve: Resolve,
    pub tls: Arc<rustls::ClientConfig>,
    /// To the last byte, redirects and all.
    pub deadline: Duration,
    /// The most a body may be, on the wire and unpacked.
    pub max: usize,
    /// What the proxy's refusals name it by.
    pub process: String,
    /// Another host's redirect followed (yours) — else not, said in `Fetched.elsewhere` (Claude's, D5).
    pub others: bool,
    pub stop: Stop,
}

/// Tests: a stand-in for `yours` — a way made for the fetch's `Stop` (`Session.web_way`).
#[cfg(test)]
pub type MakeWay = Arc<dyn Fn(Stop) -> Way + Send + Sync>;

/// The way of yours (`:web`): never asked, every address rule and the user's upstream proxy (`upstream`) as a run's
/// — this machine's own addresses read as a run's policy reads them. Blocks (the first one makes the TLS config): a
/// worker's.
pub fn yours(
    root: &std::path::Path,
    upstream: proxy::upstream::Upstreams,
    stop: Stop,
) -> Result<Way, String> {
    // (loopback is `route::this_machine`'s: the own addresses are the others — a LAN's, a bridge's)
    let own = egress::addresses()
        .ok()
        .map(|a| a.into_iter().filter(|ip| !ip.to_canonical().is_loopback()).collect());
    let rules = Egress { any: true, own_addresses: own, upstream, ..Egress::default() };
    Ok(Way {
        proxy: proxy::get().clone(),
        session: proxy::session_of(root),
        rules: Arc::new(rules),
        resolve: egress::system(),
        tls: tls()?,
        deadline: DEADLINE,
        max: super::PAGE_MAX,
        process: ":web".into(),
        others: true,
        stop,
    })
}

/// Claude's way (`read` of a URL, P4): only `host:port` — the host its card (or a run's grant) let through, a
/// same-host redirect with it — every address rule and the user's upstream proxy as a run's; another host's redirect
/// said, not followed. Blocks (it may make the TLS config): a worker's.
pub fn claudes(
    root: &std::path::Path,
    upstream: proxy::upstream::Upstreams,
    (host, port): (&str, u16),
    stop: Stop,
) -> Result<Way, String> {
    let mut way = yours(root, upstream, stop)?;
    let name = crate::sandbox::proxy::parse::name(host).map_err(|e| format!("{host}: {e}"))?;
    let rules =
        Egress { any: false, hosts: vec![egress::Pattern::Name(name, Some(port))], ..(*way.rules).clone() };
    (way.rules, way.deadline, way.process, way.others) =
        (Arc::new(rules), DEADLINE_CLAUDE, "read".into(), false);
    Ok(way)
}

/// The one TLS config: rustls on ring, the OS's verifier, TLS 1.2 and 1.3, http/1.1 alone — made once, on the first
/// fetch's worker (Linux reads the distro's certificates there).
pub fn tls() -> Result<Arc<rustls::ClientConfig>, String> {
    static TLS: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    TLS.get_or_init(|| {
        use rustls_platform_verifier::BuilderVerifierExt;
        let mut c =
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("TLS: {e}"))?
                .with_platform_verifier()
                .map_err(|e| format!("the system's certificates: {e}"))?
                .with_no_client_auth();
        c.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(c))
    })
    .clone()
}

/// A fetch's way to stop: its socket, shut down from another thread — one call that never waits — so its worker's
/// read returns at once.
#[derive(Clone, Default)]
pub struct Stop(Arc<Mutex<(bool, Option<TcpStream>)>>);

impl Stop {
    pub fn stop(&self) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.0 = true;
        if let Some(t) = &s.1 {
            let _ = t.shutdown(Shutdown::Both);
        }
    }

    pub fn stopped(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    /// Whether `other` is this fetch's.
    pub fn same(&self, other: &Stop) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The socket a stop shuts — refused once stopped.
    fn hold(&self, t: &TcpStream) -> Result<(), String> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 {
            return Err(STOPPED.into());
        }
        s.1 = t.try_clone().ok();
        Ok(())
    }
}

const STOPPED: &str = "stopped";

/// A page fetched.
#[derive(Clone, Debug)]
pub struct Fetched {
    /// The address asked, and the one its bytes came from (after redirects).
    pub asked: String,
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    /// Its body, unpacked: `max` bytes and one more where it went past (the reader says it was cut).
    pub bytes: Vec<u8>,
    /// The hosts it was sent on from by another host's redirect, in order (said: `from github.com`).
    pub from: Vec<String>,
    /// Redirects followed, a refresh's among them.
    pub hops: usize,
    /// `http://` asked of a host beyond this machine: read over https.
    pub upgraded: bool,
    /// What's said beside it: a body that may be cut short.
    pub said: Option<String>,
    /// Its body went past the limit: `bytes` is its first `max` and one.
    pub cut: bool,
    pub took: Duration,
    /// Where another host's redirect (then `bytes` is empty) or refresh sends it, not followed (`Way.others`
    /// off — Claude's: its next read is asked at that host).
    pub elsewhere: Option<String>,
}

/// `url` fetched and read (`super::read`), a `<meta refresh>` within 5 s followed as a redirect is (D5) — one
/// budget of `REDIRECTS_MAX` hops for both — the page and what it came from. A refresh that isn't followed (past
/// the budget, an address the fetch refuses, from beyond this machine into it) leaves the page as read, said.
pub fn page(url: &str, way: &Way, prefer: super::Prefer) -> Result<(Fetched, super::Page), String> {
    let started = crate::clock::now();
    let by = started + way.deadline;
    let mut f = get_by(url, way, by, 0)?;
    loop {
        let page = super::read(&f.bytes, &f.url, f.content_type.as_deref(), prefer);
        let Some(next) =
            page.refresh.as_ref().filter(|r| r.secs <= 5 && r.url != f.url).map(|r| r.url.clone())
        else {
            f.took = crate::clock::since(started);
            return Ok((f, page));
        };
        let here = http::target(&f.url).is_ok_and(|t| t.loopback);
        let host = |u: &str| http::target(u).map(|t| t.host).unwrap_or_default();
        if !way.others && host(&next) != host(&f.url) {
            f.elsewhere = Some(next);
            f.took = crate::clock::since(started);
            return Ok((f, page));
        }
        let not = match http::target(&next) {
            _ if f.hops >= REDIRECTS_MAX => Some(format!("more than {REDIRECTS_MAX} redirects")),
            Err(e) => Some(e),
            Ok(t) if t.loopback && !here => Some("from beyond this machine into it".into()),
            Ok(_) => None,
        };
        if let Some(why) = not {
            let line = format!("its refresh to {next} isn't followed: {why}");
            f.said = Some(f.said.map_or(line.clone(), |s| format!("{s}; {line}")));
            f.took = crate::clock::since(started);
            return Ok((f, page));
        }
        let mut n = get_by(&next, way, by, f.hops + 1)?;
        if host(&next) != host(&f.url) {
            f.from.push(host(&f.url));
        }
        n.from = [std::mem::take(&mut f.from), n.from].concat();
        (n.asked, n.upgraded) = (f.asked.clone(), f.upgraded || n.upgraded);
        f = n;
    }
}

/// `url` fetched by `by`, its redirects followed with `used` of the budget gone — one run of the proxy's for all its
/// hops.
fn get_by(url: &str, way: &Way, by: Instant, used: usize) -> Result<Fetched, String> {
    let run = crate::sandbox::next_run();
    let got = hops(url, way, run, by, used);
    way.proxy.send(ToProxy::RunEnded { run });
    got
}

fn hops(url: &str, way: &Way, run: u64, by: Instant, used: usize) -> Result<Fetched, String> {
    let mut t = http::target(url)?;
    let upgraded = t.upgraded;
    let mut from = Vec::new();
    let mut hop = used;
    loop {
        let (r, body, said) = exchange(&t, way, run, by)?;
        let location = r.field("location").filter(|_| matches!(r.status, 301 | 302 | 303 | 307 | 308));
        if let Some(location) = location {
            if hop >= REDIRECTS_MAX {
                return Err(format!("{}: more than {REDIRECTS_MAX} redirects", t.url()));
            }
            let next = super::url::resolve(&t.url(), location)
                .ok_or_else(|| format!("{}: a redirect to nowhere ({location})", t.url()))?;
            let n = http::target(&next).map_err(|e| format!("{}: its redirect — {e}", t.url()))?;
            if n.loopback && !t.loopback {
                return Err(format!("{}: a redirect from {} to this machine isn't followed", next, t.host));
            }
            // (Claude's: another host is its own read, asked at its own gate — said, not followed)
            if n.host != t.host && !way.others {
                return Ok(Fetched {
                    asked: url.trim().to_string(),
                    url: t.url(),
                    status: r.status,
                    content_type: None,
                    bytes: Vec::new(),
                    from,
                    hops: hop,
                    upgraded,
                    said,
                    cut: false,
                    took: Duration::ZERO,
                    elsewhere: Some(next),
                });
            }
            if n.host != t.host {
                from.push(t.host.clone());
            }
            (t, hop) = (n, hop + 1);
            continue;
        }
        let bytes = unpacked(&r, body, way.max).map_err(|e| format!("{}: {e}", t.url()))?;
        let cut = bytes.len() > way.max;
        return Ok(Fetched {
            asked: url.trim().to_string(),
            url: t.url(),
            status: r.status,
            content_type: r.field("content-type").map(str::to_string),
            bytes,
            from,
            hops: hop,
            upgraded,
            said,
            cut,
            took: Duration::ZERO,
            elsewhere: None,
        });
    }
}

/// A body as it was sent: gzip unpacked within `max`, none or `identity` as it is; another coding said.
fn unpacked(r: &Response, body: Vec<u8>, max: usize) -> Result<Vec<u8>, String> {
    let codings: Vec<String> = r
        .fields
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .flat_map(|(_, v)| v.split(','))
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty() && c != "identity")
        .collect();
    match codings.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => Ok(body),
        ["gzip" | "x-gzip"] => match gzip::gunzip(&body, max) {
            Ok(Unpacked::Whole(b) | Unpacked::Cut(b)) => Ok(b),
            Err(e) => Err(format!("its gzip body: {e}")),
        },
        _ => Err(format!(
            "its body is compressed as {}, which tarae doesn't read (gzip only)",
            codings.join(", ")
        )),
    }
}

/// The socket under the fetch's one deadline: every read and write on it armed with what's left — rustls loops on
/// the socket inside one read or write of its own (`complete_io`), so a server dripping bytes would otherwise get a
/// whole timeout each call — and refused (`TimedOut`) once it's gone. Plain http and TLS read through it alike.
struct Deadline {
    s: TcpStream,
    by: Instant,
}

impl Deadline {
    fn arm(&self) -> io::Result<()> {
        let left = self.by.saturating_duration_since(crate::clock::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.s.set_read_timeout(Some(left))?;
        self.s.set_write_timeout(Some(left))
    }
}

impl Read for Deadline {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.arm().and_then(|()| self.s.read(b))
    }
}

impl Write for Deadline {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.arm().and_then(|()| self.s.write(b))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// One request and its response: the head, the body's data (framing taken off, past `max` cut at `max` and one), and
/// what's said beside it.
fn exchange(
    t: &Target,
    way: &Way,
    run: u64,
    by: Instant,
) -> Result<(Response, Vec<u8>, Option<String>), String> {
    let mut tcp = connect(t, way, run, by)?;
    let at = t.url();
    let said = |e: String| format!("{at}: {e}");
    if !t.https {
        return talk(&mut tcp, t, way).map_err(said);
    }
    let name = rustls::pki_types::ServerName::try_from(t.host.clone()).map_err(|e| said(e.to_string()))?;
    let conn = rustls::ClientConnection::new(way.tls.clone(), name).map_err(|e| said(format!("TLS: {e}")))?;
    let mut s = rustls::StreamOwned::new(conn, tcp);
    talk(&mut s, t, way).map_err(said)
}

/// Err once the fetch was stopped.
fn going(way: &Way) -> Result<(), String> {
    match way.stop.stopped() {
        true => Err(STOPPED.into()),
        false => Ok(()),
    }
}

fn late(way: &Way) -> String {
    format!("no whole answer within {}", crate::term::run_duration(way.deadline))
}

/// An I/O error in words: the stop's, the deadline's, TLS's (a certificate the system doesn't trust …), else its own.
fn io_said(e: &io::Error, way: &Way) -> String {
    if way.stop.stopped() {
        return STOPPED.into();
    }
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => late(way),
        io::ErrorKind::InvalidData => format!("TLS: {e}"),
        _ => os_words(e),
    }
}

/// The OS's words for an I/O error after a why of tarae's: `connection refused · os error 61` (std's own
/// `Connection refused (os error 61)`, its code apart, never in parentheses inside a why's).
fn os_words(e: &io::Error) -> String {
    let said = e.to_string();
    let Some(code) = e.raw_os_error() else { return said };
    let words = said.strip_suffix(&format!(" (os error {code})")).unwrap_or(&said);
    let mut c = words.chars();
    let words = c.next().map_or_else(String::new, |f| f.to_lowercase().chain(c).collect());
    format!("{words} · os error {code}")
}

fn talk(
    s: &mut (impl Read + Write),
    t: &Target,
    way: &Way,
) -> Result<(Response, Vec<u8>, Option<String>), String> {
    going(way)?;
    s.write_all(&http::request(t)).and_then(|()| s.flush()).map_err(|e| io_said(&e, way))?;
    let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 64 << 10]);
    // (an interim 1xx head goes, the next one read)
    let r = loop {
        let r = loop {
            match parse::response(&buf).map_err(|r| format!("its answer: {}", r.said()))? {
                Some(r) => break r,
                None => match read(s, &mut chunk, way)? {
                    0 => return Err("the connection closed before an answer".into()),
                    n => buf.extend_from_slice(&chunk[..n]),
                },
            }
        };
        buf.drain(..r.len);
        if !(100..200).contains(&r.status) {
            break r;
        }
    };
    let (mut body, mut data, mut said) = (Body::new(r.body), Vec::new(), None);
    let took = |body: &mut Body, b: &[u8], data: &mut Vec<u8>| {
        body.data(b, data).map_err(|e| format!("its body: {e}"))
    };
    took(&mut body, &buf, &mut data)?;
    while !body.done() && data.len() <= way.max {
        match read(s, &mut chunk, way) {
            Ok(0) if r.body == Framing::Close => break,
            Ok(0) => return Err("its body cut short: the connection closed before its end".into()),
            Ok(n) => {
                took(&mut body, &chunk[..n], &mut data)?;
            }
            // (rustls: closed without TLS's close_notify — a close-delimited body can't tell that from its end)
            Err(Cut) if r.body == Framing::Close => {
                said = Some("it may be cut short: its connection closed without TLS's last word".to_string());
                break;
            }
            Err(Cut) => return Err("its body cut short: the connection closed before its end".into()),
            Err(Failed(e)) => return Err(e),
        }
    }
    data.truncate(way.max + 1);
    Ok((r, data, said))
}

/// Why a read gave nothing.
enum ReadErr {
    /// The peer went without TLS's close_notify.
    Cut,
    Failed(String),
}
use ReadErr::{Cut, Failed};

impl From<String> for ReadErr {
    fn from(e: String) -> ReadErr {
        Failed(e)
    }
}

impl From<ReadErr> for String {
    fn from(e: ReadErr) -> String {
        match e {
            Cut => "the connection closed without TLS's last word".into(),
            Failed(e) => e,
        }
    }
}

fn read(s: &mut impl Read, chunk: &mut [u8], way: &Way) -> Result<usize, ReadErr> {
    loop {
        going(way)?;
        match s.read(chunk) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !way.stop.stopped() => return Err(Cut),
            Err(e) => return Err(Failed(io_said(&e, way))),
        }
    }
}

/// The connection for `t`: this machine's directly — every address its name has this machine's —, any other through
/// tarae's proxy: one end of a pair to it as a run's connection, `CONNECT host:port` asked, its 200 read.
fn connect(t: &Target, way: &Way, run: u64, by: Instant) -> Result<Deadline, String> {
    let at = t.url();
    let left = || by.saturating_duration_since(crate::clock::now());
    if t.loopback {
        let found = (way.resolve)(&t.host, t.port)
            .map_err(|e| format!("{at}: its name isn't found · {}", os_words(&e)))?;
        if found.is_empty() || found.iter().any(|a| !a.ip().to_canonical().is_loopback()) {
            return Err(format!("{at}: {} isn't only this machine's", t.host));
        }
        let mut last = None;
        for a in found {
            match TcpStream::connect_timeout(&a, left().max(Duration::from_millis(1))) {
                Ok(s) => {
                    way.stop.hold(&s)?;
                    return Ok(Deadline { s, by });
                }
                Err(e) => last = Some(e),
            }
        }
        let why = last.map(|e| format!(" · {}", os_words(&e))).unwrap_or_default();
        return Err(format!("{at}: nothing answers there{why}"));
    }
    let (mine, theirs) = proxy::pair(libc::AF_INET, |_| Ok(()), |_| {})
        .map_err(|e| format!("{at}: a way to tarae's proxy couldn't be made · {}", os_words(&e)))?;
    let (told, refusals) = mpsc::channel();
    let c = Connection {
        fd: theirs.into(),
        run,
        session: way.session,
        process: way.process.clone(),
        rules: way.rules.clone(),
        told,
    };
    if !way.proxy.send(ToProxy::Connection(c)) {
        return Err(format!("{at}: tarae's proxy isn't there"));
    }
    let s = TcpStream::from(mine);
    way.stop.hold(&s)?;
    let mut s = Deadline { s, by };
    going(way)?;
    let ask = format!("CONNECT {} HTTP/1.1\r\nHost: {0}\r\n\r\n", authority(t));
    s.write_all(ask.as_bytes()).map_err(|e| format!("{at}: {}", io_said(&e, way)))?;
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 1024]);
    let r = loop {
        if let Some(r) = parse::response(&buf).map_err(|r| format!("{at}: tarae's proxy: {}", r.said()))? {
            break r;
        }
        match read(&mut s, &mut chunk, way).map_err(String::from) {
            Ok(0) => return Err(format!("{at}: tarae's proxy closed without an answer")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(format!("{at}: {e}")),
        }
    };
    if r.status == 200 && buf.len() == r.len {
        return Ok(s);
    }
    // (its refusal said where it's sent — else the answer's body, which is its why too)
    let why = refusals.recv_timeout(Duration::from_millis(200)).map(|d| d.target).unwrap_or_else(|_| {
        let mut rest = buf[r.len..].to_vec();
        let _ = (&mut s).take(4096).read_to_end(&mut rest);
        String::from_utf8_lossy(&rest).trim().to_string()
    });
    Err(format!("refused by tarae's proxy — {why}"))
}

/// `host:port` as a CONNECT names it: always the port.
fn authority(t: &Target) -> String {
    match t.host.contains(':') {
        true => format!("[{}]:{}", t.host, t.port),
        false => format!("{}:{}", t.host, t.port),
    }
}

/// The session's pages (D11): each fetched page by the address asked and the one it came from, for `KEEP`, at most
/// `PAGES_MAX` and `PAGES_BYTES` of them, the oldest out first; never on disk, never restored. A page Claude read
/// that isn't one to read again (an error's, one cut short, this machine's) is kept too, for follow and replay alone
/// (`kept`, D14: the copy Claude read) — never given for another read (`get`).
#[derive(Default)]
pub struct Pages(Vec<Kept>);

/// A page kept: when it was fetched, the page, whether a read may take it again.
type Kept = (Instant, Arc<Fetched>, bool);

impl Pages {
    /// The page fetched for `url` within `KEEP` of `now`, and when.
    pub fn get(&self, url: &str, now: Instant) -> Option<(Instant, Arc<Fetched>)> {
        let url = url.trim();
        let fresh = |at: &Instant| now.saturating_duration_since(*at) < KEEP;
        let found = self
            .0
            .iter()
            .rev()
            .find(|(at, f, again)| *again && fresh(at) && (f.asked == url || f.url == url));
        found.map(|(at, f, _)| (*at, f.clone()))
    }

    /// The copy of `url` fetched at `at`, while it's still kept, fresh or not (follow shows what Claude read).
    pub fn kept(&self, url: &str, at: Instant) -> Option<Arc<Fetched>> {
        let found = self.0.iter().find(|(t, f, _)| *t == at && (f.url == url || f.asked == url));
        found.map(|(_, f, _)| f.clone())
    }

    /// `f` kept from `now`: what goes for it (stale, or past the limits) handed back — to be freed off the main loop.
    pub fn put(&mut self, f: Arc<Fetched>, now: Instant) -> Vec<Arc<Fetched>> {
        self.keep(f, now, true)
    }

    /// `put`, `again` saying whether a read may take it again (else it's for follow and replay alone).
    pub fn keep(&mut self, f: Arc<Fetched>, now: Instant, again: bool) -> Vec<Arc<Fetched>> {
        let mut gone = Vec::new();
        let (keep, old): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.0).into_iter().partition(|(at, g, _)| {
                now.saturating_duration_since(*at) < KEEP && g.asked != f.asked && g.url != f.url
            });
        gone.extend(old.into_iter().map(|(_, g, _)| g));
        self.0 = keep;
        self.0.push((now, f, again));
        while self.0.len() > PAGES_MAX
            || self.0.iter().map(|(_, g, _)| g.bytes.len()).sum::<usize>() > PAGES_BYTES
        {
            if self.0.len() == 1 {
                break;
            }
            gone.push(self.0.remove(0).1);
        }
        gone
    }
}

#[cfg(test)]
pub(crate) mod tests;
