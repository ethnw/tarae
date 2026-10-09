//! 6e+ (`target/sb/step6eplus-design.md`): raw TCP to a name the run looked up, and a name's SRV/TXT records — each
//! asked of the user where the rules don't let it through, kept apart from the host card's answers, against the same
//! stand-ins as the planted tests (`planted`): the test's listeners for the outside, a stand-in for tarae's lookups.

use super::planted::{beyond, planted_in, was_reset};
use super::*;

/// A Postgres client's first message (SSLRequest): neither TLS nor HTTP.
const STARTUP: &[u8] = &[0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f];

fn session_names() -> Arc<answering::Session> {
    Arc::new(answering::Session::new(Vec::new(), true))
}

/// The address the session's names gave `name`, at `port`: where the run's connection to it goes.
fn given(names: &answering::Session, name: &str, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(names.names.give(name).v4), port)
}

fn answer_of(kind: Kind, host: &str, port: u16, run: u64, scope: Scope) -> ToProxy {
    ToProxy::Answer { session: 1, run, kind, host: host.into(), port, scope }
}

/// A refusal said with its own op.
fn said_as(d: &Receiver<Denial>, op: &str) -> String {
    let d = d.recv_timeout(WAIT).expect("a refusal said");
    assert_eq!(d.op, op, "{}", d.target);
    d.target
}

/// Q2: a connection to a name's address whose first bytes are neither TLS nor HTTP is asked about as raw TCP — even
/// with "any" host allowed (F5) — and held: nothing reaches the outside until the user's yes; then it's connected by
/// the name and spliced, what it sent while it waited going first, the outside's bytes coming back.
#[test]
fn raw_tcp_to_a_name_is_asked_then_spliced_with_what_it_sent() {
    let out = outside(echo);
    let f = start(resolver(vec![("db.test", vec![vec![out.at]])]).0, limits());
    let names = session_names();
    let any = Egress { any: true, ..rules(&[]) };
    let (mut c, _) = planted_in(&f.proxy, 1, any, given(&names, "db.test", 5432), &names);
    c.write_all(STARTUP).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("asked");
    let want = FromProxy::Ask {
        session: 1,
        run: 1,
        kind: Kind::Raw,
        host: "db.test".into(),
        port: 5432,
        process: "cargo".into(),
        asked: Vec::new(),
    };
    assert_eq!(asked, want);
    c.write_all(b"more").unwrap();
    assert_eq!(out.accepted.load(Ordering::SeqCst), 0, "nothing reached while asked");
    f.proxy.send(answer_of(Kind::Raw, "db.test", 5432, 1, Scope::Run));
    let mut back = vec![0u8; STARTUP.len() + 4];
    c.read_exact(&mut back).unwrap();
    assert_eq!(back, [STARTUP, b"more"].concat(), "spliced, what it sent first");
    drop(c);
    assert_eq!(out.got.recv_timeout(WAIT).unwrap(), [STARTUP, b"more"].concat());
}

/// A server that speaks first (SMTP, MySQL): its client sends nothing, and past W (`Limits::silent`) the connection
/// is raw. A rule naming that port lets it through unasked (F5) — the banner comes back; a rule without a port (a
/// scheme's 443/80) doesn't: asked, and with no answer within the hold refused — reset, said as raw (`RAW_OP`, not
/// the host card's op: `:sandbox-blocked` offers no host grant for it).
#[test]
fn a_server_that_speaks_first_is_raw_after_w() {
    fn banner(mut s: TcpStream) -> Vec<u8> {
        let _ = s.write_all(b"220 mail.test\r\n");
        echo(s)
    }
    let out = outside(banner);
    let f = start(
        resolver(vec![("mail.test", vec![vec![out.at]])]).0,
        Limits { hold: Duration::from_millis(300), ..limits() },
    );
    let names = session_names();
    let to = given(&names, "mail.test", 25);
    let (mut c, _) = planted_in(&f.proxy, 1, rules(&["mail.test:25"]), to, &names);
    let mut b = [0u8; 15];
    c.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"220 mail.test\r\n", "the server's first words");
    assert!(f.asks.try_recv().is_err(), "a rule naming its port: never asked");
    let (mut c, d) = planted_in(&f.proxy, 1, rules(&["mail.test"]), to, &names);
    let asked = f.asks.recv_timeout(WAIT).expect("a rule without a port: asked");
    assert!(matches!(asked, FromProxy::Ask { kind: Kind::Raw, port: 25, .. }), "{asked:?}");
    assert!(was_reset(&mut c));
    let target = said_as(&d, RAW_OP);
    assert!(target.starts_with("mail.test:25: raw TCP") && target.contains("didn't answer"), "{target}");
    assert_eq!(out.accepted.load(Ordering::SeqCst), 1, "only the one let through");
}

/// F4: raw answers are kept apart from the host card's. A yes for a host over TLS (`s`) doesn't let raw bytes go
/// there — asked; the user's `n` refuses it, said, and the next raw connection is refused at once, unasked — while
/// the host's own yes still stands.
#[test]
fn raw_answers_are_kept_apart_from_hosts() {
    let out = outside(echo);
    let f = start(resolver(vec![("db.test", vec![vec![out.at]])]).0, limits());
    let names = session_names();
    let to = given(&names, "db.test", 5432);
    f.proxy.send(answer_of(Kind::Host, "db.test", 5432, 1, Scope::Session));
    let (mut c, d) = planted_in(&f.proxy, 1, rules(&[]), to, &names);
    c.write_all(STARTUP).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("raw asked, whatever the host's answer");
    assert!(matches!(asked, FromProxy::Ask { kind: Kind::Raw, .. }), "{asked:?}");
    f.proxy.send(answer_of(Kind::Raw, "db.test", 5432, 1, Scope::Refused));
    assert!(was_reset(&mut c));
    assert!(said_as(&d, RAW_OP).starts_with("db.test:5432: the user refused it"));
    let (mut c, d) = planted_in(&f.proxy, 1, rules(&[]), to, &names);
    c.write_all(STARTUP).unwrap();
    assert!(was_reset(&mut c));
    assert!(said_as(&d, RAW_OP).contains("refused it for this session"));
    assert!(f.asks.try_recv().is_err(), "refused unasked");
    assert_eq!(out.accepted.load(Ordering::SeqCst), 0);
    // (the host's yes, untouched: a request to it goes — the outside echoes it)
    let (mut c, _) = planted_in(&f.proxy, 1, rules(&[]), to, &names);
    c.write_all(b"GET / HTTP/1.1\r\nHost: db.test:5432\r\n\r\n").unwrap();
    let mut b = [0u8; 14];
    c.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"GET / HTTP/1.1", "the host card's yes, as it was");
    assert!(f.asks.try_recv().is_err());
}

/// A run's DNS question over TCP, as its resolver frames it.
fn dns_ask(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut m = id.to_be_bytes().to_vec();
    m.extend([1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for l in name.split('.') {
        m.push(l.len() as u8);
        m.extend_from_slice(l.as_bytes());
    }
    m.push(0);
    m.extend(qtype.to_be_bytes());
    m.extend([0, 1]);
    dns::framed(&m)
}

/// An answer read off a DNS-over-TCP connection (None: none within `within`).
fn dns_answer(c: &mut TcpStream, within: Duration) -> Option<Vec<u8>> {
    c.set_read_timeout(Some(within)).unwrap();
    let mut len = [0u8; 2];
    c.read_exact(&mut len).ok()?;
    let mut a = vec![0u8; usize::from(u16::from_be_bytes(len))];
    c.set_read_timeout(Some(WAIT)).unwrap();
    c.read_exact(&mut a).unwrap();
    Some(a)
}

/// tarae's lookups stood in: `cluster.test`'s SRV (`_mongodb._tcp`) and TXT, as MongoDB Atlas has them — how many
/// lookups it took.
fn cluster_records() -> (records::Records, Arc<AtomicUsize>) {
    let n = Arc::new(AtomicUsize::new(0));
    let calls = n.clone();
    let records: records::Records = Arc::new(move |name: &str, qtype| {
        calls.fetch_add(1, Ordering::SeqCst);
        match (name, qtype) {
            ("_mongodb._tcp.cluster.test", dns::TYPE_SRV) => dns::Found::Records(
                vec![dns::Record::Srv {
                    priority: 0,
                    weight: 0,
                    port: 27017,
                    target: "db1.cluster.test".into(),
                }],
                600,
            ),
            ("cluster.test", dns::TYPE_TXT) => {
                dns::Found::Records(vec![dns::Record::Txt(vec![b"authSource=admin".to_vec()])], 30)
            }
            _ => dns::Found::No,
        }
    });
    (records, n)
}

/// F1 (b): a name the rules allow has its SRV and TXT looked up by tarae and answered with no card — the records
/// as they came, their TTL at most the map's (300).
#[test]
fn records_of_an_allowed_name_are_answered_with_no_card() {
    let (records, looked) = cluster_records();
    let f = start_with(resolver(Vec::new()).0, limits(), records);
    let names = session_names();
    let (mut c, _) = planted_in(&f.proxy, 1, rules(&["cluster.test"]), beyond(53), &names);
    c.write_all(&dns_ask(0x5151, "_mongodb._tcp.cluster.test", dns::TYPE_SRV)).unwrap();
    let a = dns_answer(&mut c, WAIT).expect("answered");
    assert_eq!((&a[..2], a[3] & 0x0f, &a[6..8]), (&[0x51, 0x51][..], dns::NOERROR, &[0, 1][..]));
    let srv_at = a.len() - 6 - 18;
    assert_eq!(&a[srv_at - 6..srv_at - 2], &300u32.to_be_bytes(), "TTL capped");
    assert!(a.ends_with(
        &[&[0, 0, 0, 0, 0x69, 0x89, 3][..], b"db1", &[7], b"cluster", &[4], b"test", &[0]].concat()
    ));
    c.write_all(&dns_ask(0x5252, "cluster.test", dns::TYPE_TXT)).unwrap();
    let a = dns_answer(&mut c, WAIT).expect("answered");
    assert!(a.ends_with(b"\x10authSource=admin"), "{a:?}");
    assert!(f.asks.try_recv().is_err(), "no card");
    assert_eq!(looked.load(Ordering::SeqCst), 2);
}

/// F1's reach (the review, 2026-10-09): a name tarae already looks up for real — here by the user's `s` on its host
/// card, for port 443 — gets its records with no lookup card (they show the DNS's operators nothing new), while a name
/// nobody allowed still gets the card.
#[test]
fn a_names_host_yes_answers_its_records_unasked() {
    let (records, looked) = cluster_records();
    let f = start_with(resolver(Vec::new()).0, limits(), records);
    let names = session_names();
    f.proxy.send(answer_of(Kind::Host, "cluster.test", 443, 1, Scope::Session));
    let (mut c, _) = planted_in(&f.proxy, 1, rules(&[]), beyond(53), &names);
    c.write_all(&dns_ask(1, "_mongodb._tcp.cluster.test", dns::TYPE_SRV)).unwrap();
    let a = dns_answer(&mut c, WAIT).expect("answered");
    assert_eq!((a[3] & 0x0f, a[7]), (dns::NOERROR, 1), "its records, unasked");
    assert!(f.asks.try_recv().is_err(), "no lookup card");
    assert_eq!(looked.load(Ordering::SeqCst), 1);
    c.write_all(&dns_ask(2, "_mongodb._tcp.nobody.test", dns::TYPE_SRV)).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("a name nobody allowed: asked");
    assert!(
        matches!(&asked, FromProxy::Ask { kind: Kind::Lookup, host, .. } if host == "nobody.test"),
        "{asked:?}"
    );
    assert_eq!(looked.load(Ordering::SeqCst), 1, "nothing looked up while asked");
}

/// F1–F3: another name's records wait for the user — the question held, unanswered (the client asks again, M13), one
/// card for the name they're about whatever the type (an SRV owner's base, F2: the card's list grows with the TXT
/// question), nothing looked up meanwhile. The user's `s` answers every question waiting, and the next unasked;
/// their `n` answers REFUSED, said with the question (`LOOKUP_OP`).
#[test]
fn records_of_another_name_wait_for_the_users_answer() {
    let (records, looked) = cluster_records();
    let f = start_with(resolver(Vec::new()).0, limits(), records);
    let names = session_names();
    let (mut srv, _) = planted_in(&f.proxy, 1, rules(&[]), beyond(53), &names);
    srv.write_all(&dns_ask(1, "_mongodb._tcp.cluster.test", dns::TYPE_SRV)).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("asked");
    let srv_q = "SRV _mongodb._tcp.cluster.test".to_string();
    let want = |asked: Vec<String>| FromProxy::Ask {
        session: 1,
        run: 1,
        kind: Kind::Lookup,
        host: "cluster.test".into(),
        port: 0,
        process: "cargo".into(),
        asked,
    };
    assert_eq!(asked, want(vec![srv_q.clone()]));
    assert!(dns_answer(&mut srv, Duration::from_millis(200)).is_none(), "held: no answer");
    let (mut txt, _) = planted_in(&f.proxy, 1, rules(&[]), beyond(53), &names);
    txt.write_all(&dns_ask(2, "cluster.test", dns::TYPE_TXT)).unwrap();
    let asked = f.asks.recv_timeout(WAIT).expect("the card's list grows");
    assert_eq!(asked, want(vec![srv_q, "TXT cluster.test".into()]));
    assert_eq!(
        f.proxy.waiting_for(Key::new(1, Kind::Lookup, "cluster.test", 0)),
        2,
        "one question, two waiting"
    );
    assert_eq!(looked.load(Ordering::SeqCst), 0, "nothing looked up while asked");
    f.proxy.send(answer_of(Kind::Lookup, "cluster.test", 0, 1, Scope::Session));
    for (c, id) in [(&mut srv, 1u8), (&mut txt, 2)] {
        let a = dns_answer(c, WAIT).expect("answered once the user said yes");
        assert_eq!((a[1], a[3] & 0x0f, a[7]), (id, dns::NOERROR, 1));
    }
    srv.write_all(&dns_ask(3, "_mongodb._tcp.cluster.test", dns::TYPE_SRV)).unwrap();
    assert_eq!(dns_answer(&mut srv, WAIT).expect("answered")[7], 1, "the session's yes: unasked");
    assert!(f.asks.try_recv().is_err());
    f.proxy.send(answer_of(Kind::Lookup, "cluster.test", 0, 1, Scope::Refused));
    let (mut c, d) = planted_in(&f.proxy, 1, rules(&[]), beyond(53), &names);
    c.write_all(&dns_ask(4, "cluster.test", dns::TYPE_TXT)).unwrap();
    assert_eq!(dns_answer(&mut c, WAIT).expect("answered")[3] & 0x0f, dns::REFUSED);
    let target = said_as(&d, LOOKUP_OP);
    assert!(target.starts_with("TXT cluster.test: the user refused it"), "{target}");
}
