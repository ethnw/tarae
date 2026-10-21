use super::*;

/// A stub's question for `name`'s `qtype` in `class`, as it sends it.
fn asked(name: &str, qtype: u16, class: u16) -> Vec<u8> {
    let mut m = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for l in name.split('.').filter(|l| !l.is_empty()) {
        m.push(l.len() as u8);
        m.extend_from_slice(l.as_bytes());
    }
    m.push(0);
    m.extend(qtype.to_be_bytes());
    m.extend(class.to_be_bytes());
    m
}

/// A stub's question for `name`'s `qtype` (class IN unless `class`), as `dns::question` reads it.
fn ask_in(name: &str, qtype: u16, class: u16) -> Question {
    dns::question(&asked(name, qtype, class)).unwrap()
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

/// `ip`'s reverse name (`d.c.b.a.in-addr.arpa`, 32 nibbles of `ip6.arpa`), as a stub asks it.
fn reverse(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(v6) => {
            let nibbles: Vec<String> =
                v6.octets().iter().rev().flat_map(|b| [b & 0xf, b >> 4]).map(|n| format!("{n:x}")).collect();
            format!("{}.ip6.arpa", nibbles.join("."))
        }
    }
}

/// Every address's reverse name reads back as it (2,000 random ones of each family); any name, mutated 20,000
/// times, is read without a panic, an address only from a name under `in-addr.arpa` or `ip6.arpa`.
#[test]
fn a_reverse_name_reads_back_as_its_address() {
    use crate::mutate::{self, Rng};
    let mut rng = Rng(0x5eed_0add_7e55_010b);
    let mut seeds = Vec::new();
    for _ in 0..2_000 {
        for ip in [
            IpAddr::V4(Ipv4Addr::from(rng.next() as u32)),
            IpAddr::V6(Ipv6Addr::from(u128::from(rng.next()) << 64 | u128::from(rng.next()))),
        ] {
            assert_eq!(reversed(&reverse(ip)), Some(ip), "{}", reverse(ip));
            seeds.push(reverse(ip));
        }
    }
    for _ in 0..20_000 {
        let mut n = rng.pick(&seeds).clone();
        mutate::text(&mut rng, &mut n, &[".", "in-addr.arpa", "ip6.arpa", "256", "+1", "ff"]);
        if reversed(&n).is_some() {
            assert!(n.ends_with(".in-addr.arpa") || n.ends_with(".ip6.arpa"), "{n}");
        }
    }
}

/// Whatever a run asks, `plan` answers only as its rules say: a real lookup only of a name they name and the
/// upstream doesn't take, the name as `parse::name` reads it; records only for SRV and TXT, asked for where the
/// rules don't name their base; addresses only the session's map's for that name (loopback for `localhost`); a
/// PTR's name only the map's for the address the question reverses. Seeded questions mutated 50,000 times.
#[test]
fn a_question_is_answered_only_as_the_rules_say() {
    use crate::sandbox::proxy::parse::tests::{Rng, mutate};
    let s = session();
    let through = |n: &str| n.ends_with(".through.example");
    let given = s.names.give("github.com");
    let (pool, pool6) = (reverse(IpAddr::V4(given.v4)), reverse(IpAddr::V6(given.v6)));
    let seeds: Vec<Vec<u8>> = [
        ("github.com", dns::TYPE_A),
        ("x.corp.example", dns::TYPE_AAAA),
        ("a.b.other.example", dns::TYPE_A),
        ("api.localhost", dns::TYPE_AAAA),
        ("_svc._tcp.x.corp.example", dns::TYPE_SRV),
        ("x.through.example", dns::TYPE_TXT),
        ("x.through.example", dns::TYPE_A),
        (&pool, dns::TYPE_PTR),
        (&pool6, dns::TYPE_PTR),
        ("1.0.0.127.in-addr.arpa", dns::TYPE_PTR),
        ("metadata.google.internal", dns::TYPE_A),
        ("example.com", dns::TYPE_HTTPS),
        ("example.com", 15),
    ]
    .iter()
    .map(|(n, t)| asked(n, *t, dns::CLASS_IN))
    .collect();
    let mut rng = Rng(0x5eed_0add_7e55_010c);
    let mut seen = std::collections::BTreeMap::<&str, usize>::new();
    for _ in 0..50_000 {
        let mut b = seeds[rng.below(seeds.len())].clone();
        mutate(&mut rng, &mut b, 64);
        let Ok(q) = dns::question(&b) else { continue };
        let name = parse::name(&q.name);
        let what = match plan(&q, &s, named, through).0 {
            Plan::LookUp(n) => {
                assert!(named(&n) && !through(&n) && name.as_deref() == Ok(n.as_str()), "{n} for {q:?}");
                assert!([dns::TYPE_A, dns::TYPE_AAAA].contains(&q.qtype), "{q:?}");
                "looked up"
            }
            Plan::Records { base: b, ask } => {
                let n = name.as_deref().unwrap();
                assert!([dns::TYPE_SRV, dns::TYPE_TXT].contains(&q.qtype) && b == base(q.qtype, n), "{q:?}");
                assert!(ask == !named(&b) && !through(&b), "{b}: ask {ask}");
                "records"
            }
            Plan::Now(Answer::Addresses(a)) => {
                let n = name.as_deref().unwrap();
                match n == "localhost" || n.ends_with(".localhost") {
                    true => assert!(a.iter().all(IpAddr::is_loopback), "{n}: {a:?}"),
                    false => assert!(
                        a.len() == 1 && s.names.name_of(a[0]).as_deref() == Some(n),
                        "{n}: {a:?}, not the map's"
                    ),
                }
                "addresses"
            }
            Plan::Now(Answer::Ptr(n)) => {
                let of = reversed(&q.name).and_then(|ip| s.names.name_of(ip));
                assert!(q.qtype == dns::TYPE_PTR && of.as_deref() == Some(n.as_str()), "{n} for {q:?}");
                "ptr"
            }
            _ => "other",
        };
        *seen.entry(what).or_default() += 1;
    }
    assert!(seen.len() == 5 && seen.values().all(|&n| n > 50), "{seen:?}");
}
