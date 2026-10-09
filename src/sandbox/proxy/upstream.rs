//! The proxy tarae itself goes out by (step 5g, `target/sb/step5g-plan.md`): where tarae's own environment names
//! one, the egress proxy chains to it, with the credentials its URL carries, sent at once. An http:// one: a
//! tunnel becomes a `CONNECT host:port` to it, a plain request goes to it in absolute form. A SOCKS5 one
//! (`socks5h://` and `socks5://` alike — the user's decision 2026-10-08: the name goes to it either way, never an
//! address of tarae's looking up, so the same checks hold as an http one's): RFC 1928's CONNECT with the domain
//! name, signed in with none or RFC 1929's user and password, then a tunnel as it is and a plain request in origin
//! form. Read from tarae's environment where a run's policy is made (`of`), it travels in the run's rules
//! (`Egress.upstream`): the proxy knows it only from a message (D23). The runs never see it — their proxy
//! variables are tarae's own proxy's (`runenv::PROXIES`).
//!
//! Through an upstream the proxy never looks the run's names up: the upstream does. So the host and server-name
//! checks hold and the address checks can't — the run's report says so. `NO_PROXY`'s hosts go direct, with every
//! check. An upstream tarae can't speak to (`https://`, SOCKS4, a value it can't read) fails closed: nothing goes
//! that way, said; `llm.sandbox-upstream = "none"` sends the runs direct.
//!
//! The credentials are kept as they're sent (`Secret`), whose `Debug` shows nothing; the report
//! names the variable only (the user's decision 2026-10-07: a corporate proxy's name can be an internal one, and
//! Claude's context leaves the machine), the log host:port — never a user or password, never the upstream's own
//! words.

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::IpAddr;

use super::parse;

/// `llm.sandbox-upstream`: the proxy tarae itself uses, as its environment names it, or none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Setting {
    #[default]
    Env,
    None,
}

impl Setting {
    pub fn as_str(self) -> &'static str {
        match self {
            Setting::Env => "env",
            Setting::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Setting> {
        match s {
            "env" => Some(Setting::Env),
            "none" => Some(Setting::None),
            _ => None,
        }
    }
}

/// The variables a tunnel's upstream is read from, in the order taken: lowercase first, as curl.
const TUNNEL_VARS: [&str; 4] = ["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"];
/// A plain request's.
const PLAIN_VARS: [&str; 4] = ["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"];
const NO_PROXY_VARS: [&str; 2] = ["no_proxy", "NO_PROXY"];

/// An upstream's credentials, decoded — for it alone (as `Basic` to an http one, RFC 1929's to a SOCKS5 one):
/// what its `Debug` shows is nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    user: Vec<u8>,
    password: Vec<u8>,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

impl Secret {
    /// `Proxy-Authorization`'s value.
    fn basic(&self) -> String {
        format!("Basic {}", parse::base64(&[&self.user[..], b":", &self.password[..]].concat()))
    }
}

/// How tarae speaks to an upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Http,
    Socks5,
}

/// An upstream proxy: the variable it came from (what a report names), how it's spoken to, where it is, its
/// credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Upstream {
    pub var: &'static str,
    pub proto: Proto,
    /// A name (lowercase) or an address.
    pub host: String,
    pub port: u16,
    auth: Option<Secret>,
}

/// Its variable, how and where — host:port goes to tarae's log, never to a report — and whether it has
/// credentials.
impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let how = match self.proto {
            Proto::Http => "",
            Proto::Socks5 => "socks5 ",
        };
        write!(f, "Upstream({} {how}{}, credentials: {})", self.var, self.at(), self.auth.is_some())
    }
}

impl Upstream {
    /// host:port, an IPv6 address in brackets — for tarae's log.
    pub fn at(&self) -> String {
        match self.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(a)) => format!("[{a}]:{}", self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }

    pub fn has_credentials(&self) -> bool {
        self.auth.is_some()
    }

    /// Its `Proxy-Authorization` line, or nothing.
    fn auth_line(&self) -> String {
        self.auth.as_ref().map(|a| format!("Proxy-Authorization: {}\r\n", a.basic())).unwrap_or_default()
    }

    /// SOCKS5's greeting (RFC 1928 §3): the ways tarae signs in — none, and with credentials a user and password.
    pub fn socks_greeting(&self) -> Vec<u8> {
        match self.auth {
            Some(_) => vec![5, 2, 0, 2],
            None => vec![5, 1, 0],
        }
    }

    /// RFC 1929's sign-in with its credentials (`read` keeps only credentials it can carry: a user of 1–255
    /// bytes, a password of at most 255) — None without any.
    pub fn socks_sign_in(&self) -> Option<Vec<u8>> {
        let a = self.auth.as_ref()?;
        let mut v = vec![1, a.user.len() as u8];
        v.extend_from_slice(&a.user);
        v.push(a.password.len() as u8);
        v.extend_from_slice(&a.password);
        Some(v)
    }

    /// SOCKS5's CONNECT to `name:port` by its domain name (ATYP 3) — never an address: the upstream looks it up.
    /// `name` as `parse::name` makes it (at most 253 bytes).
    pub fn socks_connect(name: &str, port: u16) -> Vec<u8> {
        let mut v = vec![5, 1, 0, 3, name.len().min(255) as u8];
        v.extend_from_slice(&name.as_bytes()[..name.len().min(255)]);
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    /// What asks it for a tunnel to `name:port` (a name as `parse::name` makes it: no byte that breaks a line).
    pub fn connect_head(&self, name: &str, port: u16) -> Vec<u8> {
        format!("CONNECT {name}:{port} HTTP/1.1\r\nHost: {name}:{port}\r\n{}\r\n", self.auth_line())
            .into_bytes()
    }

    /// A plain request's head (`parse::absolute`'s, ending in its blank line) with its credentials put in.
    pub fn plain_head(&self, absolute: &[u8]) -> Vec<u8> {
        let mut out = absolute.strip_suffix(b"\r\n").unwrap_or(absolute).to_vec();
        out.extend_from_slice(self.auth_line().as_bytes());
        out.extend_from_slice(b"\r\n");
        out
    }
}

/// An upstream that can't be chained to: its variable, and why (fixed words — never the value, which may hold a
/// password).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unusable {
    pub var: &'static str,
    pub why: &'static str,
}

/// A `NO_PROXY` entry: every host, or a name and its subdomains (at a port, or any).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Direct {
    All,
    Name(String, Option<u16>),
}

/// How a request goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way<'a> {
    /// Direct: looked up by tarae, every address checked.
    Direct,
    Through(&'a Upstream),
    /// Nowhere: the upstream its variable names can't be chained to.
    Unusable(Unusable),
}

/// The upstreams tarae's environment names — a tunnel's, a plain request's — and the hosts that go direct.
/// Empty (the default): every request direct.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Upstreams {
    tunnel: Option<Result<Upstream, Unusable>>,
    plain: Option<Result<Upstream, Unusable>>,
    direct: Vec<Direct>,
    /// Variables set in both cases with different values: (the one taken, the one left).
    pub mixed: Vec<(&'static str, &'static str)>,
}

impl std::fmt::Debug for Upstreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstreams")
            .field("tunnel", &self.tunnel)
            .field("plain", &self.plain)
            .field("direct", &self.direct)
            .finish()
    }
}

/// What tarae's environment `env` names, as `setting` takes it.
pub fn of(env: &HashMap<String, OsString>, setting: Setting) -> Upstreams {
    if setting == Setting::None {
        return Upstreams::default();
    }
    let mut mixed = Vec::new();
    let mut first = |vars: &[&'static str]| -> Option<(&'static str, String)> {
        let value = |k: &str| {
            let v = env.get(k)?.to_str().map_or_else(|| "\u{fffd}".to_string(), |s| s.trim().to_string());
            (!v.is_empty()).then_some(v)
        };
        let (i, var) = vars.iter().enumerate().find(|(_, k)| value(k).is_some())?;
        let taken = value(var)?;
        // (its other case, beside it in the list)
        if i % 2 == 0
            && let Some(other) = value(vars[i + 1])
            && other != taken
            && !mixed.contains(&(*var, vars[i + 1]))
        {
            mixed.push((*var, vars[i + 1]));
        }
        Some((*var, taken))
    };
    let tunnel = first(&TUNNEL_VARS).map(|(var, v)| read(var, &v));
    let plain = first(&PLAIN_VARS).map(|(var, v)| read(var, &v));
    let direct = first(&NO_PROXY_VARS).map(|(_, v)| no_proxy(&v)).unwrap_or_default();
    Upstreams { tunnel, plain, direct, mixed }
}

/// An upstream's URL: `[http://|socks5://|socks5h://][user[:password]@]host[:port][/…]` — the port 80 (http) or
/// 1080 (SOCKS5) when it has none; the credentials percent-decoded. Another scheme, credentials SOCKS5 can't
/// carry, or what isn't such a URL: unusable, said without the value.
fn read(var: &'static str, url: &str) -> Result<Upstream, Unusable> {
    let unusable = |why| Unusable { var, why };
    let unreadable = unusable("isn't a proxy URL tarae reads");
    let (proto, rest) = match url.split_once("://") {
        Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
            "http" => (Proto::Http, rest),
            // (socks5:// asks the client to look the name up; tarae never does — the name goes to it as with
            // socks5h://, so its checks are the same either way)
            "socks5" | "socks5h" => (Proto::Socks5, rest),
            "https" => {
                return Err(unusable("names an https:// proxy, which tarae's proxy doesn't chain to yet"));
            }
            s if s.starts_with("socks") => {
                return Err(unusable(
                    "names a SOCKS proxy other than SOCKS5 (socks5:// or socks5h://), which tarae's proxy doesn't \
                     chain to",
                ));
            }
            _ => return Err(unreadable),
        },
        None => (Proto::Http, url),
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let auth = match userinfo.filter(|u| !u.is_empty()) {
        Some(u) => {
            let (user, password) = u.split_once(':').unwrap_or((u, ""));
            let (user, password) = (decoded(user).ok_or(unreadable)?, decoded(password).ok_or(unreadable)?);
            // (RFC 1929: a user of 1–255 bytes, a password of at most 255)
            if proto == Proto::Socks5 && (user.is_empty() || user.len() > 255 || password.len() > 255) {
                return Err(unusable(
                    "has credentials SOCKS5 can't carry (a user of 1–255 bytes, a password of at most 255)",
                ));
            }
            Some(Secret { user, password })
        }
        None => None,
    };
    let (host, port) = match hostport.strip_prefix('[') {
        Some(v6) => {
            let (a, port) = v6.split_once(']').ok_or(unreadable)?;
            let a: std::net::Ipv6Addr = a.parse().map_err(|_| unreadable)?;
            (a.to_string(), port.strip_prefix(':').map_or(Ok(None), |p| port_of(p).map(Some)))
        }
        None => {
            let (h, port) = match hostport.rsplit_once(':') {
                Some((h, p)) => (h, port_of(p).map(Some)),
                None => (hostport, Ok(None)),
            };
            let h = match h.parse::<std::net::Ipv4Addr>() {
                Ok(a) => a.to_string(),
                Err(_) => parse::name(h).map_err(|_| unreadable)?,
            };
            (h, port)
        }
    };
    let default = match proto {
        Proto::Http => 80,
        Proto::Socks5 => 1080,
    };
    let port = port.map_err(|_| unreadable)?.unwrap_or(default);
    Ok(Upstream { var, proto, host, port, auth })
}

fn port_of(p: &str) -> Result<u16, ()> {
    match p.parse::<u16>() {
        Ok(n) if n > 0 && !p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit()) => Ok(n),
        _ => Err(()),
    }
}

/// `%XX` decoded — None: a `%` not followed by two hex digits.
fn decoded(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// `NO_PROXY`'s entries as curl and Go read them: `*` every host; `example.com`, `.example.com` and
/// `*.example.com` that name and its subdomains; `host:port` at that port only; names without case or a trailing
/// dot. Addresses and CIDR ranges are left out: the proxy takes no address (a run's literal is refused before).
fn no_proxy(v: &str) -> Vec<Direct> {
    let mut out = Vec::new();
    for e in v.split([',', ' ', '\t']).map(str::trim).filter(|e| !e.is_empty()) {
        if e == "*" {
            out.push(Direct::All);
            continue;
        }
        let e = e.strip_prefix("*.").or_else(|| e.strip_prefix('.')).unwrap_or(e);
        if e.starts_with('[') || e.contains('/') || e.matches(':').count() > 1 {
            continue;
        }
        let (h, port) = match e.rsplit_once(':') {
            Some((h, p)) => match port_of(p) {
                Ok(p) => (h, Some(p)),
                Err(()) => continue,
            },
            None => (e, None),
        };
        if h.parse::<IpAddr>().is_ok() {
            continue;
        }
        if let Ok(name) = parse::name(h) {
            out.push(Direct::Name(name, port));
        }
    }
    out
}

impl Upstreams {
    /// Whether `NO_PROXY`'s entry `d` sends `host` direct at `port` (None: at every port).
    fn direct_at(d: &Direct, host: &str, port: Option<u16>) -> bool {
        match d {
            Direct::All => true,
            Direct::Name(n, at) => {
                let named = host == n
                    || (host.len() > n.len()
                        && host.ends_with(n.as_str())
                        && host[..host.len() - n.len()].ends_with('.'));
                named && (at.is_none() || (port.is_some() && *at == port))
            }
        }
    }

    /// Whether a request for `host` may go through an upstream — at any port, a tunnel or a plain one (6e, E1:
    /// tarae then looks the name up for nothing — the upstream resolves it). No only where no upstream is named, or
    /// `NO_PROXY` sends the name direct at every port; an entry for some of its ports leaves the others through, so
    /// it's yes — a real lookup is what E1 keeps from names that may go out another way.
    pub fn may_go_through(&self, host: &str) -> bool {
        (self.tunnel.is_some() || self.plain.is_some())
            && !self.direct.iter().any(|d| Self::direct_at(d, host, None))
    }

    /// How a request to `host:port` (a name as `parse::name` makes it) goes: a tunnel's or a plain one's.
    pub fn way(&self, host: &str, port: u16, tunnel: bool) -> Way<'_> {
        let direct = self.direct.iter().any(|d| Self::direct_at(d, host, Some(port)));
        let choice = if tunnel { &self.tunnel } else { &self.plain };
        match choice {
            _ if direct => Way::Direct,
            None => Way::Direct,
            Some(Ok(u)) => Way::Through(u),
            Some(Err(e)) => Way::Unusable(*e),
        }
    }

    /// The ports of the upstreams on this machine's loopback (`localhost`, `127.x`, `::1`) — a run that may
    /// connect there reaches that proxy itself, around tarae's host checks.
    pub fn on_loopback(&self) -> Vec<u16> {
        let lo = |u: &Upstream| match u.host.parse::<IpAddr>() {
            Ok(ip) => ip.to_canonical().is_loopback(),
            Err(_) => u.host == "localhost" || u.host.ends_with(".localhost"),
        };
        let ups = [&self.tunnel, &self.plain].into_iter().filter_map(|c| c.as_ref()?.as_ref().ok());
        let mut ports: Vec<u16> = ups.filter(|u| lo(u)).map(|u| u.port).collect();
        ports.dedup();
        ports
    }

    /// Whether any request may go through an upstream (the address checks don't hold for it).
    pub fn through(&self) -> bool {
        [&self.tunnel, &self.plain].iter().any(|c| matches!(c, Some(Ok(_))))
    }

    /// For the run's report: where https and plain http go beyond tarae's proxy — by the variables' names only.
    /// None: no upstream named, all direct.
    pub fn said(&self) -> Option<String> {
        if self.tunnel.is_none() && self.plain.is_none() {
            return None;
        }
        let way = |c: &Option<Result<Upstream, Unusable>>, what: &str| match c {
            None => format!("{what} direct"),
            Some(Ok(u)) => format!("{what} through tarae's own {}", u.var),
            Some(Err(e)) => format!(
                "{what} nowhere — tarae's own {} {} (llm.sandbox-upstream = \"none\" sends it direct)",
                e.var, e.why
            ),
        };
        Some(format!("{}, {}", way(&self.tunnel, "https"), way(&self.plain, "plain http")))
    }
}

/// What an upstream answered a request with: its status, its head's length, and what its `Proxy-Authenticate`
/// offers — named from tarae's own list, never in the upstream's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub len: usize,
    pub basic: bool,
    pub schemes: Vec<&'static str>,
}

/// The schemes a 407 may offer that tarae names (it speaks only Basic).
const SCHEMES: [&str; 6] = ["Basic", "Negotiate", "NTLM", "Digest", "Kerberos", "Bearer"];

/// An answer's head at `buf`'s start — None while its blank line hasn't come (within `parse::HEAD_MAX`); Err:
/// not an HTTP answer, or past the limit.
pub fn answer(buf: &[u8]) -> Result<Option<Answer>, &'static str> {
    let window = &buf[..buf.len().min(parse::HEAD_MAX)];
    let Some(end) = window.windows(4).position(|w| w == b"\r\n\r\n") else {
        return match buf.len() >= parse::HEAD_MAX {
            true => Err("an answer head past 8192 bytes"),
            // (what's there must start as an answer does, or it never will)
            false if !b"HTTP/1."[..buf.len().min(7)].eq(&buf[..buf.len().min(7)]) => {
                Err("not an HTTP answer")
            }
            false => Ok(None),
        };
    };
    let head = &window[..end];
    let mut lines = head.split(|&c| c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let first = lines.next().unwrap_or_default();
    let status = match first {
        [b'H', b'T', b'T', b'P', b'/', b'1', b'.', b'0' | b'1', b' ', d @ ..] if d.len() >= 3 => {
            std::str::from_utf8(&d[..3]).ok().and_then(|s| s.parse::<u16>().ok()).filter(|s| *s >= 100)
        }
        _ => None,
    }
    .ok_or("not an HTTP answer")?;
    let mut schemes = Vec::new();
    for l in lines {
        let Some(colon) = l.iter().position(|&c| c == b':') else { continue };
        if !l[..colon].eq_ignore_ascii_case(b"proxy-authenticate") {
            continue;
        }
        // (challenges and their parameters share commas: a challenge is a lone first word, no `=` in it)
        for part in l[colon + 1..].split(|&c| c == b',') {
            let word = part.trim_ascii().split(|&c| c == b' ').next().unwrap_or_default();
            if word.contains(&b'=') {
                continue;
            }
            let named = SCHEMES.iter().find(|s| s.as_bytes().eq_ignore_ascii_case(word));
            match named {
                Some(s) if !schemes.contains(s) => schemes.push(*s),
                None if !word.is_empty() && !schemes.contains(&"another scheme") => {
                    schemes.push("another scheme")
                }
                _ => {}
            }
        }
    }
    let basic = schemes.contains(&"Basic");
    Ok(Some(Answer { status, len: end + 4, basic, schemes }))
}

/// A SOCKS5 server's reply to CONNECT at `buf`'s start (RFC 1928 §6): its REP and its length — None while it isn't
/// all there; Err: not a SOCKS5 reply. Its bound address (any of the three types) is skipped, never used.
pub fn socks_reply(buf: &[u8]) -> Result<Option<(u8, usize)>, &'static str> {
    let not = "not a SOCKS5 reply";
    match buf {
        [] => Ok(None),
        [v, ..] if *v != 5 => Err(not),
        [_, _, rsv, ..] if *rsv != 0 => Err(not),
        [_, _, _, _, ..] if buf.len() < 5 => Ok(None),
        [_, rep, _, atyp, n, ..] => {
            let addr = match atyp {
                1 => 4,
                4 => 16,
                3 => 1 + usize::from(*n),
                _ => return Err("a SOCKS5 reply with an address of no type it has"),
            };
            let len = 4 + addr + 2;
            Ok((buf.len() >= len).then_some((*rep, len)))
        }
        _ => Ok(None),
    }
}

/// RFC 1928's reply codes, in tarae's words.
fn socks_said(rep: u8) -> &'static str {
    match rep {
        1 => "a failure of its own",
        2 => "its rules don't allow it",
        3 => "the network is unreachable from it",
        4 => "the host is unreachable from it",
        5 => "the host refused it",
        6 => "its time to live ran out",
        7 => "it doesn't take CONNECT",
        8 => "it doesn't take a name",
        _ => "a reply SOCKS5 doesn't define",
    }
}

/// Why a request didn't go through the upstream — in the report's words (`said`), which name the variable only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failed {
    /// Its variable names one tarae can't chain to.
    Unusable(&'static str),
    /// Its name wasn't found.
    NotFound,
    /// No address of it took the connection (the error's kind, in the system's words).
    Unreachable(String),
    /// No whole answer within the time.
    Silent(std::time::Duration),
    /// It closed before its answer was whole.
    Cut,
    /// Its answer isn't one tarae reads.
    Unreadable(&'static str),
    /// It sent bytes past its tunnel's answer, before the client's first.
    Extra,
    /// 407, and tarae's variable carries no credentials.
    NoCredentials,
    /// 407 to the credentials tarae sent.
    WrongCredentials,
    /// 407 offering only schemes tarae doesn't speak.
    Schemes(Vec<&'static str>),
    /// Another status than 2xx to a tunnel.
    Status(u16),
    /// SOCKS5: it takes no way tarae offered — none, and tarae's variable carries no credentials.
    SocksNoCredentials,
    /// SOCKS5: RFC 1929's sign-in refused.
    SocksWrongCredentials,
    /// SOCKS5: it takes no way tarae signs in (GSSAPI, its own), or picked one tarae didn't offer.
    SocksMethod,
    /// SOCKS5: its CONNECT refused, with this reply code.
    SocksRefused(u8),
}

impl Failed {
    /// The words after `host:port: ` in a report — `var` the variable the upstream came from.
    pub fn said(&self, var: &str) -> String {
        let why = match self {
            Failed::Unusable(why) => {
                return format!(
                    "tarae's own {var} {why} — nothing goes beyond that way (llm.sandbox-upstream = \"none\" sends \
                     runs direct)"
                );
            }
            Failed::NotFound => "its name isn't found".into(),
            Failed::Unreachable(e) => format!("it can't be reached ({e})"),
            Failed::Silent(t) => format!("it didn't answer within {} s", t.as_secs_f32()),
            Failed::Cut => "it closed before its answer".into(),
            Failed::Unreadable(why) => format!("its answer: {why}"),
            Failed::Extra => "it sent bytes before the client's own".into(),
            Failed::NoCredentials => format!("it asks for credentials (407), and tarae's {var} gives none"),
            Failed::WrongCredentials => format!("it refused the credentials in tarae's {var} (407)"),
            Failed::Schemes(s) => {
                format!("it asks for {} (407), which tarae doesn't speak — only Basic", s.join(" or "))
            }
            Failed::Status(s) => match reason(*s) {
                Some(r) => format!("it answered {s} ({r})"),
                None => format!("it answered {s}"),
            },
            Failed::SocksNoCredentials => {
                format!(
                    "it asks for credentials (or a way tarae doesn't speak), and tarae's {var} gives none"
                )
            }
            Failed::SocksWrongCredentials => format!("it refused the credentials in tarae's {var}"),
            Failed::SocksMethod => {
                "it asks for a way to sign in tarae doesn't speak — only none, or a user and password".into()
            }
            Failed::SocksRefused(rep) => format!("it refused it: {} (SOCKS5 reply {rep})", socks_said(*rep)),
        };
        format!("through the proxy tarae itself uses (tarae's {var}): {why}")
    }

    /// A 407 read as tarae's credentials meet it.
    pub fn of_407(a: &Answer, had_credentials: bool) -> Failed {
        match (a.basic, had_credentials) {
            (false, _) => Failed::Schemes(a.schemes.clone()),
            (true, false) => Failed::NoCredentials,
            (true, true) => Failed::WrongCredentials,
        }
    }
}

/// tarae's own reason phrase for a status an upstream answers — never the upstream's.
fn reason(status: u16) -> Option<&'static str> {
    Some(match status {
        400 => "Bad Request",
        403 => "Forbidden — its own refusal",
        404 => "Not Found",
        405 => "Method Not Allowed",
        407 => "Proxy Authentication Required",
        500 => "Internal Server Error",
        502 => "Bad Gateway — it couldn't reach the host",
        503 => "Service Unavailable",
        504 => "Gateway Timeout — it couldn't reach the host in time",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
