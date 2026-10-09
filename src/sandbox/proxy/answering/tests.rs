use super::*;

/// A stub's question for `name`'s `qtype` (class IN unless `class`), as `dns::question` reads it.
fn ask_in(name: &str, qtype: u16, class: u16) -> Question {
    let mut m = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for l in name.split('.').filter(|l| !l.is_empty()) {
        m.push(l.len() as u8);
        m.extend_from_slice(l.as_bytes());
    }
    m.push(0);
    m.extend(qtype.to_be_bytes());
    m.extend(class.to_be_bytes());
    dns::question(&m).unwrap()
}

fn ask(name: &str, qtype: u16) -> Question {
    ask_in(name, qtype, dns::CLASS_IN)
}

fn session() -> Session {
    Session::new(vec!["corp.example".into(), "other.example".into()], true)
}

/// The rules of these tests: `github.com` and everything under `corp.example`.
fn named(n: &str) -> bool {
    n == "github.com" || n.ends_with(".corp.example")
}

const NOT_THROUGH: fn(&str) -> bool = |_| false;

/// E6 (b): under `ndots:5` (or after the name as written came back NXDOMAIN) the client asks the search list's
/// candidates first. A name of two or more labels and then a search domain, not named by the rules, is NXDOMAIN
/// without a lookup — so `github.com.other.example` doesn't win over `github.com` (M7: every client but musl and
/// c-ares took it). A single label and a search domain (`redis.corp.example`, `redis.other.example`) stays as any
/// name: the candidate the real resolver would usually take. One the rules name is looked up for real (E1). A name
/// that only ends like a search domain isn't one of its candidates.
#[test]
fn a_search_candidate_of_a_name_with_dots_is_nxdomain_unless_named() {
    let s = session();
    let p = |n: &str| plan(&ask(n, dns::TYPE_A), &s, named, NOT_THROUGH).0;
    assert_eq!(p("github.com.other.example"), Plan::Now(Answer::Rcode(dns::NXDOMAIN)));
    assert_eq!(p("a.b.c.other.example"), Plan::Now(Answer::Rcode(dns::NXDOMAIN)));
    assert_eq!(
        p("github.com.corp.example"),
        Plan::LookUp("github.com.corp.example".into()),
        "named: looked up"
    );
    let redis = Answer::Addresses(vec![IpAddr::V4(s.names.give("redis.other.example").v4)]);
    assert_eq!(p("redis.other.example"), Plan::Now(redis), "one label and a search domain: any name");
    assert!(matches!(p("github.comother.example"), Plan::Now(Answer::Addresses(_))), "not a candidate");
    assert!(matches!(p("other.example"), Plan::Now(Answer::Addresses(_))), "the domain itself");
    let github = p("github.com");
    assert_eq!(github, Plan::LookUp("github.com".into()), "the name as written: named, looked up");
}

/// E1 (b) and Q3: a name the rules name is looked up for real (and its lookup kept a while: asked again, answered at
/// once from what it came to); any other gets the map's address unlooked-up, both families from one index, AAAA only
/// where the session carries IPv6; through an upstream nothing is looked up. HTTPS/SVCB an empty NOERROR; another
/// type (MX, but SRV and TXT — `records_are_looked_up_for_the_name_they_are_about`) REFUSED and said with the name;
/// another class REFUSED; a metadata server's own name NXDOMAIN, said; `localhost` loopback; a name that isn't one
/// NXDOMAIN, the root REFUSED.
#[test]
fn a_name_gets_its_answer() {
    let s = session();
    let p = |n: &str, t: u16| plan(&ask(n, t), &s, named, NOT_THROUGH);
    let given = s.names.give("a.test");
    assert_eq!(p("a.test", dns::TYPE_A), (Plan::Now(Answer::Addresses(vec![IpAddr::V4(given.v4)])), None));
    assert_eq!(p("a.test", dns::TYPE_AAAA).0, Plan::Now(Answer::Addresses(vec![IpAddr::V6(given.v6)])));
    let no_v6 = Session::new(Vec::new(), false);
    let a6 = plan(&ask("a.test", dns::TYPE_AAAA), &no_v6, named, NOT_THROUGH).0;
    assert_eq!(a6, Plan::Now(Answer::Empty), "no IPv6 in the session: NODATA");
    assert_eq!(p("github.com", dns::TYPE_AAAA).0, Plan::LookUp("github.com".into()));
    s.remember("github.com", Exists::Yes { v4: true, v6: false });
    assert_eq!(p("github.com", dns::TYPE_AAAA).0, Plan::Now(Answer::Empty), "looked up: v4 only, NODATA");
    let through = plan(&ask("github.com", dns::TYPE_A), &no_v6, named, |_| true).0;
    assert!(matches!(through, Plan::Now(Answer::Addresses(_))), "through the upstream: never looked up");
    assert_eq!(p("a.test", dns::TYPE_HTTPS), (Plan::Now(Answer::Empty), None));
    assert_eq!(p("a.test", dns::TYPE_SVCB).0, Plan::Now(Answer::Empty));
    let (mx, said) = p("a.test", 15);
    assert_eq!(mx, Plan::Now(Answer::Rcode(dns::REFUSED)));
    assert!(said.unwrap().starts_with("a.test (MX): "), "said with its name");
    let chaos = plan(&ask_in("version.bind", 16, 3), &s, named, NOT_THROUGH);
    assert_eq!((chaos.0, chaos.1.is_some()), (Plan::Now(Answer::Rcode(dns::REFUSED)), true));
    let (meta, said) = p("metadata.google.internal", dns::TYPE_A);
    assert_eq!((meta, said.is_some()), (Plan::Now(Answer::Rcode(dns::NXDOMAIN)), true));
    let local = Answer::Addresses(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    assert_eq!(p("api.localhost", dns::TYPE_A).0, Plan::Now(local));
    assert_eq!(
        p("1.2.3.4", dns::TYPE_A).0,
        Plan::Now(Answer::Rcode(dns::NXDOMAIN)),
        "an address isn't a name"
    );
    assert_eq!(p("", 2).0, Plan::Now(Answer::Rcode(dns::REFUSED)), "the root");
}

/// 6e+ (F1, F2): an SRV or TXT question is looked up by tarae for the name it's about — an SRV owner's without its
/// leading `_` labels, a TXT's as asked — with no card where the rules name that name, else asked first. Service
/// labels with no name under them aren't a name (NXDOMAIN); through an upstream refused, said (tarae looks nothing
/// up); a search candidate not named NXDOMAIN, as for an address (E6).
#[test]
fn records_are_looked_up_for_the_name_they_are_about() {
    let s = session();
    let p = |n: &str, t: u16| plan(&ask(n, t), &s, named, NOT_THROUGH);
    let records = |base: &str, ask: bool| (Plan::Records { base: base.into(), ask }, None);
    assert_eq!(p("_mongodb._tcp.db.corp.example", dns::TYPE_SRV), records("db.corp.example", false), "named");
    assert_eq!(p("db.corp.example", dns::TYPE_TXT), records("db.corp.example", false));
    assert_eq!(p("_mongodb._tcp.cluster0.mongo.test", dns::TYPE_SRV), records("cluster0.mongo.test", true));
    assert_eq!(p("_dmarc.mongo.test", dns::TYPE_TXT), records("_dmarc.mongo.test", true), "TXT: as asked");
    assert_eq!(p("_mongodb._tcp", dns::TYPE_SRV), (Plan::Now(Answer::Rcode(dns::NXDOMAIN)), None));
    let through = plan(&ask("_x._tcp.github.com", dns::TYPE_SRV), &s, named, |_| true);
    assert_eq!(through.0, Plan::Now(Answer::Rcode(dns::REFUSED)));
    assert!(through.1.unwrap().contains("through the proxy tarae uses"));
    let candidate = p("_x._tcp.db.mongo.test.other.example", dns::TYPE_SRV).0;
    assert_eq!(candidate, Plan::Now(Answer::Rcode(dns::NXDOMAIN)), "a search candidate");
    assert_eq!(base(dns::TYPE_SRV, "_a._b._c.d.test"), "d.test");
    assert_eq!(base(dns::TYPE_SRV, "_a.b_c.test"), "b_c.test");
}

/// What tarae's lookup of a name's records comes to, answered (6e+): the records as they came, their TTL at most
/// the map's; NODATA and NXDOMAIN as they came; a lookup that failed SERVFAIL (the client asks again).
#[test]
fn records_are_answered_as_they_came() {
    let srv = dns::Record::Srv { priority: 0, weight: 5, port: 27017, target: "db1.test".into() };
    let r = |ttl| records(dns::Found::Records(vec![srv.clone()], ttl));
    assert_eq!(r(60), (Answer::Records(vec![srv.clone()]), 60));
    assert_eq!(r(86_400), (Answer::Records(vec![srv.clone()]), TTL), "capped");
    assert_eq!(records(dns::Found::Empty), (Answer::Empty, 0));
    assert_eq!(records(dns::Found::No), (Answer::Rcode(dns::NXDOMAIN), 0));
    assert_eq!(records(dns::Found::Failed), (Answer::Rcode(dns::SERVFAIL), 0));
}

/// What a real lookup comes to (E1): no such name NXDOMAIN; a failure SERVFAIL (the client tries again); a name
/// that is, its families as they are — each from the map, the other NODATA.
#[test]
fn a_lookup_is_answered_as_it_came() {
    let s = session();
    let a = ask("github.com", dns::TYPE_A);
    let aaaa = ask("github.com", dns::TYPE_AAAA);
    let given = s.names.give("github.com");
    assert_eq!(after(&a, &s, Exists::No), Answer::Rcode(dns::NXDOMAIN));
    assert_eq!(after(&a, &s, Exists::Unknown), Answer::Rcode(dns::SERVFAIL));
    let v4_only = Exists::Yes { v4: true, v6: false };
    assert_eq!(after(&a, &s, v4_only), Answer::Addresses(vec![IpAddr::V4(given.v4)]));
    assert_eq!(after(&aaaa, &s, v4_only), Answer::Empty);
    let both = Exists::Yes { v4: true, v6: true };
    assert_eq!(after(&aaaa, &s, both), Answer::Addresses(vec![IpAddr::V6(given.v6)]));
}

/// A PTR question for a pool address is answered from the map, in either reverse form; any other address's is
/// NXDOMAIN — never looked up.
#[test]
fn a_reverse_name_is_the_maps() {
    let s = session();
    let given = s.names.give("a.test");
    let [a, b, c, d] = given.v4.octets();
    let v4 = format!("{d}.{c}.{b}.{a}.in-addr.arpa");
    let nibbles: String =
        given.v6.octets().iter().rev().map(|o| format!("{:x}.{:x}.", o & 0xf, o >> 4)).collect();
    let v6 = format!("{nibbles}ip6.arpa");
    for n in [v4, v6] {
        assert_eq!(
            plan(&ask(&n, dns::TYPE_PTR), &s, named, NOT_THROUGH).0,
            Plan::Now(Answer::Ptr("a.test".into())),
            "{n}"
        );
    }
    let other = plan(&ask("8.8.8.8.in-addr.arpa", dns::TYPE_PTR), &s, named, NOT_THROUGH).0;
    assert_eq!(other, Plan::Now(Answer::Rcode(dns::NXDOMAIN)));
}
