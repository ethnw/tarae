//! The session against Redis 7: transcripts captured once from the test server (`captured/`, written by the
//! `#[ignore]`d `capture_transcripts` after seeding its keys, with Redis's own text cut out — `without_redis_text`),
//! replayed here with no server; what it can't show (a
//! renamed command, a keyspace past the cap, a password off this machine) by hand. Live tests (`#[ignore]`) need
//! `target/sb/db/testdb`'s compose file up: `round11.sh`.

use super::*;
use crate::secret::Secret;

const LIVE: (&str, u16) = ("127.0.0.1", 56379);
const LIVE_TLS: (&str, u16) = ("127.0.0.1", 56380);
/// The test server's accounts (testdb/compose.yaml — fake): `ro` may read (`+@read -@dangerous`), `default` all.
const RO: (&str, &str) = ("ro", "tarae-fake-redis-ro");
const DEFAULT: (&str, &str) = ("default", "tarae-fake-redis");
/// Keys the capture seeds: hashes, each expiring in 30 minutes.
const SEEDED: u32 = 2500;

fn params((user, password): (&str, &str)) -> Params {
    Params { user: user.into(), database: None, password: Some(Secret::new(password)), tls: None, loopback: true }
}

enum Ask {
    Command(&'static [&'static str]),
    Keys(&'static str),
}

fn scenario(name: &str) -> ((&'static str, &'static str), Vec<Ask>) {
    use Ask::*;
    match name {
        "signin-ro" => (RO, vec![]),
        "signin-default" => (DEFAULT, vec![]),
        "read" => (RO, vec![Command(&["HGETALL", "tarae:session:1"]), Command(&["TYPE", "tarae:session:1"])]),
        // (SORT_RO is @dangerous: the ACL refuses `ro` — the server's word, not ours)
        "noperm" => (RO, vec![Command(&["SORT_RO", "tarae:session:1"])]),
        "keys" => (RO, vec![Keys("tarae:session:*"), Keys("tarae:nothing:*"), Keys("tarae:session:7")]),
        _ => panic!("no scenario {name}"),
    }
}

const SCENARIOS: [&str; 5] = ["signin-ro", "signin-default", "read", "noperm", "keys"];

fn ask(s: &mut Session, a: &Ask) -> Vec<Event> {
    match a {
        Ask::Command(words) => s.command(words),
        Ask::Keys(pattern) => s.keys(pattern),
    }
}

fn sent(events: &[Event]) -> Vec<u8> {
    events.iter().filter_map(|e| if let Event::Send(b) = e { Some(b.clone()) } else { None }).flatten().collect()
}

fn others(events: Vec<Event>) -> Vec<Event> {
    events.into_iter().filter(|e| !matches!(e, Event::Send(_))).collect()
}

fn captured(name: &str) -> (Vec<u8>, Vec<u8>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/redis/captured");
    let read = |side: &str| std::fs::read(dir.join(format!("{name}.{side}.raw"))).unwrap_or_else(|e| panic!("{name}.{side}: {e}"));
    (read("client"), read("server"))
}

fn replay(name: &str, piece: usize) -> (Vec<u8>, Vec<Event>) {
    let (account, asks) = scenario(name);
    let (_, server) = captured(name);
    let (mut s, mut events) = Session::start(params(account), [0; 32]);
    for a in &asks {
        events.extend(ask(&mut s, a));
    }
    for chunk in server.chunks(piece) {
        events.extend(s.feed(chunk));
    }
    (sent(&events), others(events))
}

/// The commands sent, as their words.
fn commands(client: &[u8]) -> Vec<Vec<String>> {
    let (mut out, mut rest) = (Vec::new(), client);
    while let Ok(Some((Frame::Array(words), n))) = decode(rest) {
        out.push(words.iter().map(|w| text(w).unwrap_or_default()).collect());
        rest = &rest[n..];
    }
    out
}

/// A scenario fed `server` whole: what it said, and the reads it was left with.
fn replayed(name: &str, server: &[u8]) -> (Vec<Event>, BTreeSet<String>) {
    let (account, asks) = scenario(name);
    let (mut s, mut events) = Session::start(params(account), [0; 32]);
    for a in &asks {
        events.extend(ask(&mut s, a));
    }
    events.extend(s.feed(server));
    (events, s.reads)
}

/// What a server sent, each `COMMAND INFO` entry cut to its first six fields — name, arity, flags, first key, last
/// key, step: the shape a server before 7 sends. The driver reads only the flags; what Redis 7 adds after them (ACL
/// categories, tips, key specs and their notes, subcommands) is Redis's own text, kept out of the repository
/// (plugin-sdk/db/NOTICE). Every other frame stays byte for byte; an entry already cut is left as it is.
fn without_redis_text(server: &[u8]) -> Vec<u8> {
    let header = |b: &[u8]| b.windows(2).position(|w| w == b"\r\n").expect("a RESP header") + 2;
    let next = |b: &[u8]| decode(b).ok().flatten().expect("a whole RESP frame").1;
    let (mut out, mut rest) = (Vec::new(), server);
    while let Ok(Some((frame, n))) = decode(rest) {
        let (bytes, after) = rest.split_at(n);
        rest = after;
        let entry = |f: &Frame| matches!(f, Frame::Null) || matches!(f, Frame::Array(p) if matches!(p.get(2), Some(Frame::Array(_))));
        let Frame::Array(entries) = &frame else {
            out.extend_from_slice(bytes);
            continue;
        };
        if entries.is_empty() || !entries.iter().all(entry) {
            out.extend_from_slice(bytes);
            continue;
        }
        let h = header(bytes);
        out.extend_from_slice(&bytes[..h]);
        let mut at = &bytes[h..];
        for e in entries {
            let (this, after) = at.split_at(next(at));
            at = after;
            match e {
                Frame::Array(fields) if fields.len() > 6 => {
                    out.extend_from_slice(b"*6\r\n");
                    let mut field = &this[header(this)..];
                    for _ in 0..6 {
                        let (f, after) = field.split_at(next(field));
                        out.extend_from_slice(f);
                        field = after;
                    }
                }
                _ => out.extend_from_slice(this),
            }
        }
    }
    out.extend_from_slice(rest);
    out
}

/// A session signed in as `ro` from its transcript.
fn signed_in() -> Session {
    let (mut s, _) = Session::start(params(RO), [0; 32]);
    let out = s.feed(&captured("signin-ro").1);
    assert!(matches!(out.last(), Some(Event::Ready { .. })), "{out:?}");
    s
}

// ── Replayed from the server ──

/// It signs in as the ACL user it's given — `AUTH ro …`, not the default user — byte for byte what Redis took.
/// `ro` is told the version (HELLO) but refused COMMAND INFO (`@connection`): signed in all the same, the plugin's
/// own list of reads standing.
#[test]
fn hello_signs_in_as_the_acl_user() {
    let (client, server) = captured("signin-ro");
    let (sent, events) = replay("signin-ro", usize::MAX);
    assert_eq!(sent, client);
    assert_eq!(commands(&client)[0], ["AUTH", "ro", "tarae-fake-redis-ro"]);
    assert!(matches!(&events[..], [Event::Ready { version }] if version.starts_with("7.")), "{events:?}");
    assert!(server.windows(41).any(|w| w == b"-NOPERM User ro has no permissions to run"), "COMMAND INFO refused to ro");
    let (_, events) = replay("signin-default", usize::MAX);
    assert!(matches!(&events[..], [Event::Ready { version }] if version.starts_with("7.")), "the default user may HELLO: {events:?}");
}

/// A read's answer comes back as rows of text; what the ACL refuses is the server's refusal, said in its words
/// (NOPERM) — the session goes on.
#[test]
fn a_read_comes_back_and_the_acl_has_the_last_word() {
    let (sent, events) = replay("read", usize::MAX);
    assert_eq!(sent, captured("read").0);
    let Event::Rows { rows, .. } = &events[1] else { panic!("{events:?}") };
    assert_eq!(rows, &[vec![Some("user".to_string())], vec![Some("1".to_string())]], "the seeded hash's field and value");
    assert!(matches!(&events[2], Event::Rows { rows, .. } if rows == &[vec![Some("hash".to_string())]]));
    let (_, events) = replay("noperm", usize::MAX);
    assert!(matches!(&events[1], Event::Refused(r) if r.code == "NOPERM"), "{events:?}");
}

/// A key pattern is counted by walking SCAN to its end — never KEYS, which blocks the server — with one key's type
/// and time to live; a pattern nothing matches says so; one key with no pattern in it is asked after (EXISTS), not
/// searched for. And KEYS asked for is refused, nothing sent.
#[test]
fn keys_is_never_sent() {
    let (client, _) = captured("keys");
    let (sent, events) = replay("keys", usize::MAX);
    assert_eq!(sent, client);
    let names: BTreeSet<String> = commands(&client).iter().skip(3).map(|c| c[0].clone()).collect();
    assert_eq!(names, ["EXISTS", "PTTL", "SCAN", "TYPE"].iter().map(|s| s.to_string()).collect(), "only these after signing in");
    let Event::Keys { matched, capped: false, sample: Some((key, kind, Some(ttl))) } = &events[1] else { panic!("{events:?}") };
    assert!(*matched >= SEEDED as u64 && key.starts_with("tarae:session:") && kind == "hash" && *ttl > 0, "{events:?}");
    assert_eq!(events[2], Event::Keys { matched: 0, capped: false, sample: None });
    assert!(matches!(&events[3], Event::Keys { matched: 1, sample: Some((k, _, _)), .. } if k == "tarae:session:7"), "{events:?}");
    let mut s = signed_in();
    assert!(matches!(&s.command(&["KEYS", "*"])[..], [Event::Refused(r)] if r.message.contains("SCAN instead")));
}

/// The transcripts hold none of Redis's own text — its command table's notes, tips, key specs, categories: each
/// `COMMAND INFO` entry is its six classic fields, what the driver reads (plugin-sdk/db/NOTICE). And the real
/// server's flags still reach the driver through them: the default user keeps every one of the plugin's reads, as
/// Redis 7.4 flags each `readonly`.
#[test]
fn the_transcripts_hold_no_text_of_redis() {
    for name in SCENARIOS {
        let (_, server) = captured(name);
        assert_eq!(without_redis_text(&server), server, "{name}: cut it (strip_redis_text)");
    }
    let (_, server) = captured("signin-default");
    let (_, reads) = replayed("signin-default", &server);
    assert_eq!(reads, commands::READS.iter().map(|r| r.to_string()).collect(), "narrowed by the transcript's flags");
}

/// Bytes may come in any pieces — one at a time changes nothing.
#[test]
fn bytes_may_come_in_any_pieces() {
    for name in SCENARIOS {
        assert_eq!(replay(name, 1), replay(name, usize::MAX), "{name}");
    }
}

// ── Made by hand ──

/// What this server flags as a write — a renamed command, a module's, or `rename-command` turning a name into
/// something else — leaves the plugin's reads: `COMMAND INFO` narrows the list, and a name it doesn't know is
/// dropped too.
#[test]
fn the_servers_flags_narrow_the_read_list() {
    let (mut s, _) = Session::start(params(DEFAULT), [0; 32]);
    s.feed(b"+OK\r\n");
    s.feed(b"-NOPERM no HELLO here\r\n");
    let entry = |name: &str, flags: &[&str]| {
        let flags: String = flags.iter().map(|f| format!("+{f}\r\n")).collect();
        format!("*3\r\n${}\r\n{name}\r\n:2\r\n*{}\r\n{flags}", name.len(), flags.matches("\r\n").count())
    };
    let mut info = format!("*{}\r\n", commands::READS.len());
    for name in commands::READS {
        info += &match *name {
            "get" => entry("get", &["write", "denyoom"]),
            "hgetall" => "*-1\r\n".to_string(),
            n => entry(n, &["readonly"]),
        };
    }
    let out = s.feed(info.as_bytes());
    assert!(matches!(&out[..], [Event::Ready { .. }]), "{out:?}");
    for refused in [&["GET", "k"][..], &["HGETALL", "h"]] {
        assert!(matches!(&s.command(refused)[..], [Event::Refused(r)] if r.message.contains("flagged")), "{refused:?}");
    }
    assert!(matches!(&s.command(&["TYPE", "k"])[..], [Event::Send(_)]), "a read still goes");
}

/// A keyspace too big to walk at once is walked to the cap and said as "at least": the plugin can't be held by
/// a hundred million keys, and doesn't pretend it counted them all.
#[test]
fn a_scan_stops_at_its_cap_and_says_so() {
    let mut s = signed_in();
    let mut out = s.keys("big:*");
    for i in 1..=SCAN_CALLS {
        // (a cursor that never comes back to 0: always more)
        out.extend(s.feed(format!("*2\r\n${}\r\n{i}\r\n*1\r\n$5\r\nbig:{}\r\n", i.to_string().len(), i % 10).as_bytes()));
    }
    let scans = commands(&sent(&out)).iter().filter(|c| c[0] == "SCAN").count();
    assert_eq!(scans as u32, SCAN_CALLS, "no more than the cap");
    out.extend(s.feed(b"+hash\r\n:-1\r\n"));
    let keys = others(out);
    assert_eq!(keys, [Event::Keys { matched: SCAN_CALLS as u64, capped: true, sample: Some(("big:1".into(), "hash".into(), None)) }]);
}

/// AUTH carries the password as it is: off this machine without TLS it isn't sent — not even started.
#[test]
fn a_password_never_goes_in_plain_text_off_this_machine_without_tls() {
    let (_, out) = Session::start(Params { loopback: false, ..params(RO) }, [0; 32]);
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("in the clear")), "{out:?}");
    let (_, out) = Session::start(params(RO), [0; 32]);
    assert_eq!(commands(&sent(&out))[0], ["AUTH", "ro", "tarae-fake-redis-ro"], "to this machine it goes");
}

// ── Live, against the test server (#[ignore]: round11.sh) ──

fn live(at: (&str, u16), mut s: Session, mut events: Vec<Event>, done: impl Fn(&Session, &[Event]) -> bool) -> (Vec<u8>, Vec<u8>, Vec<Event>) {
    use std::io::{Read, Write};
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    tarae_plugin::now::set(ms, &[0x5a; 32]);
    let mut tcp = std::net::TcpStream::connect(at).expect("the test server (round9.sh)");
    tcp.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let (mut client, mut server) = (Vec::new(), Vec::new());
    let (start, mut flushed) = (std::time::Instant::now(), 0);
    loop {
        for e in &events[flushed..] {
            if let Event::Send(b) = e {
                tcp.write_all(b).unwrap();
                client.extend(b);
            }
        }
        flushed = events.len();
        if done(&s, &events) || start.elapsed().as_secs() > 20 || events.iter().any(|e| matches!(e, Event::Failed(_))) {
            return (client, server, others(events));
        }
        let mut buf = [0u8; 65536];
        match tcp.read(&mut buf) {
            Ok(0) => return (client, server, others(events)),
            Ok(n) => {
                server.extend(&buf[..n]);
                events.extend(s.feed(&buf[..n]));
            }
            Err(_) => {}
        }
    }
}

fn answered(want: usize) -> impl Fn(&Session, &[Event]) -> bool {
    move |s, ev| s.idle() && ev.iter().filter(|e| !matches!(e, Event::Send(_))).count() >= want
}

/// Writes `captured/` — seeding the keys first, as the default user — when the session's bytes change.
#[test]
#[ignore]
fn capture_transcripts() {
    let (mut s, mut events) = Session::start(params(DEFAULT), [0; 32]);
    for i in 1..=SEEDED {
        let key = format!("tarae:session:{i}");
        events.extend(s.raw(&["HSET", &key, "user", &i.to_string()]));
        events.extend(s.raw(&["EXPIRE", &key, "1800"]));
    }
    let (_, _, seeded) = live(LIVE, s, events, answered(1 + 2 * SEEDED as usize));
    assert!(!seeded.iter().any(|e| matches!(e, Event::Refused(_) | Event::Failed(_))), "seeding: {:?}", seeded.last());
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/redis/captured");
    std::fs::create_dir_all(&dir).unwrap();
    for name in SCENARIOS {
        let (account, asks) = scenario(name);
        let (mut s, mut events) = Session::start(params(account), [0; 32]);
        for a in &asks {
            events.extend(ask(&mut s, a));
        }
        let (client, server, events) = live(LIVE, s, events, answered(1 + asks.len()));
        assert!(!events.iter().any(|e| matches!(e, Event::Failed(_))), "{name}: {events:?}");
        std::fs::write(dir.join(format!("{name}.client.raw")), client).unwrap();
        std::fs::write(dir.join(format!("{name}.server.raw")), without_redis_text(&server)).unwrap();
    }
}

/// Cuts Redis's own text out of transcripts captured before `capture_transcripts` did (`without_redis_text`) —
/// each replayed before and after the cut says the same, sends the same and keeps the same reads.
#[test]
#[ignore]
fn strip_redis_text() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/redis/captured");
    for name in SCENARIOS {
        let path = dir.join(format!("{name}.server.raw"));
        let before = std::fs::read(&path).unwrap();
        let after = without_redis_text(&before);
        assert_eq!(replayed(name, &before), replayed(name, &after), "{name}");
        eprintln!("{name}: {} → {} bytes", before.len(), after.len());
        std::fs::write(&path, after).unwrap();
    }
}

/// A wrong password is the server's refusal (WRONGPASS) — no `Ready`.
#[test]
#[ignore]
fn live_a_wrong_password_is_the_servers_refusal() {
    let (s, events) = Session::start(params(("ro", "tarae-fake-wrong")), [0; 32]);
    let (_, _, events) = live(LIVE, s, events, |_, _| false);
    assert!(matches!(&events[..], [Event::Failed(w)] if w.starts_with("WRONGPASS")), "{events:?}");
}

/// The wall is the ACL: `ro` sent a write as it is (the plugin never would) is refused by the server — NOPERM.
#[test]
#[ignore]
fn live_ro_cannot_write() {
    let (mut s, mut events) = Session::start(params(RO), [0; 32]);
    events.extend(s.raw(&["SET", "tarae:written", "x"]));
    let (_, _, events) = live(LIVE, s, events, answered(2));
    assert!(matches!(&events[1], Event::Refused(r) if r.code == "NOPERM"), "{events:?}");
}

/// What `ro` may do around signing in, measured: SELECT of another database (`@connection`).
#[test]
#[ignore]
fn live_select_as_ro() {
    let (s, events) = Session::start(Params { database: Some("1".into()), ..params(RO) }, [0; 32]);
    let (_, _, events) = live(LIVE, s, events, answered(1));
    assert!(matches!(&events[..], [Event::Failed(w)] if w.starts_with("NOPERM")), "{events:?}");
}

fn test_ca(name: &str) -> std::sync::Arc<tarae_plugin::tls::Config> {
    let found = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|a| a.join(format!("target/sb/db/testdb/certs/{name}.pem")))
        .find(|p| p.exists())
        .expect("the test certificates (testdb/certs.sh)");
    tarae_plugin::tls::config(&std::fs::read(found).unwrap(), None).unwrap()
}

/// TLS from the first byte, verified against the server's CA — and another CA's certificate refused.
#[test]
#[ignore]
fn live_tls() {
    let tls = |ca| Params { tls: Some((test_ca(ca), "127.0.0.1".into())), ..params(RO) };
    let (mut s, mut events) = Session::start(tls("ca"), [0; 32]);
    events.extend(s.command(&["TYPE", "tarae:session:1"]));
    let (_, _, events) = live(LIVE_TLS, s, events, answered(2));
    assert!(matches!(&events[..], [Event::Ready { .. }, Event::Rows { .. }]), "{events:?}");
    let (s, events) = Session::start(tls("other-ca"), [0; 32]);
    let (_, _, events) = live(LIVE_TLS, s, events, answered(1));
    assert!(matches!(&events[..], [Event::Failed(w)] if w.contains("UnknownIssuer")), "{events:?}");
}
