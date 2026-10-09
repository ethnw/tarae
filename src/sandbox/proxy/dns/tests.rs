use super::*;
use crate::sandbox::proxy::parse;
use crate::sandbox::proxy::parse::tests::{Rng, mutate};
use std::net::{Ipv4Addr, Ipv6Addr};

/// A stub's question for `a.test`'s A record, laid out as RFC 1035 §4.1 and RFC 6891 §6.1 have it and as glibc
/// sends it under `options edns0 trust-ad` (M7's stub setup): ID 0x1234, RD and AD set, one question, one OPT
/// (UDP size 1200).
const GLIBC_A: &[u8] = &[
    0x12, 0x34, 0x01, 0x20, 0, 1, 0, 0, 0, 0, 0, 1, // header
    1, b'a', 4, b't', b'e', b's', b't', 0, 0, 1, 0, 1, // a.test A IN
    0, 0, 41, 0x04, 0xb0, 0, 0, 0, 0, 0, 0, // OPT
];

/// The same question without EDNS, its name in mixed case (0x20 randomization: `A.tEsT`).
const MIXED_A: &[u8] =
    &[0xbe, 0xef, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 1, b'A', 4, b't', b'E', b's', b'T', 0, 0, 1, 0, 1];

/// A question is read as its client sent it — its name as text, lowercase, its type, class and EDNS — and the
/// answer is what that client takes: its ID and RD echoed, QR and RA set, the question echoed byte for byte (the
/// name's case as asked), each record's name a pointer to the question's, the OPT back where one came. The bytes
/// are RFC 1035's, written out here.
#[test]
fn a_question_is_answered_as_its_client_reads_it() {
    let q = question(GLIBC_A).unwrap();
    assert_eq!(
        (q.id, q.name.as_str(), q.qtype, q.qclass, q.edns),
        (0x1234, "a.test", TYPE_A, CLASS_IN, true)
    );
    let a = answer(&q, &Answer::Addresses(vec![IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1))]), 300);
    #[rustfmt::skip]
    let want: &[u8] = &[
        0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 1,
        1, b'a', 4, b't', b'e', b's', b't', 0, 0, 1, 0, 1,
        0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0x01, 0x2c, 0, 4, 198, 18, 0, 1,
        0, 0, 41, 0x04, 0xd0, 0, 0, 0, 0, 0, 0,
    ];
    assert_eq!(a, want);
    let q = question(MIXED_A).unwrap();
    assert_eq!((q.name.as_str(), q.edns), ("a.test", false));
    let six = Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, 1);
    let a = answer(&q, &Answer::Addresses(vec![IpAddr::V6(six)]), 300);
    #[rustfmt::skip]
    let want: &[u8] = &[
        0xbe, 0xef, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0,
        1, b'A', 4, b't', b'E', b's', b'T', 0, 0, 1, 0, 1,
        0xc0, 0x0c, 0, 28, 0, 1, 0, 0, 0x01, 0x2c, 0, 16,
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    assert_eq!(a, want, "the question's case kept");
    // (the rest: an rcode alone, nothing but the question; NOERROR with no record; a PTR's name on the wire)
    let a = answer(&q, &Answer::Rcode(NXDOMAIN), 300);
    assert_eq!((&a[2..4], &a[6..8], a.len()), (&[0x81, 0x83][..], &[0, 0][..], 12 + 12));
    let a = answer(&q, &Answer::Empty, 300);
    assert_eq!((&a[2..4], &a[6..8], a.len()), (&[0x81, 0x80][..], &[0, 0][..], 12 + 12));
    let a = answer(&q, &Answer::Ptr("a.test".into()), 300);
    assert_eq!(
        &a[24..],
        &[0xc0, 0x0c, 0, 12, 0, 1, 0, 0, 0x01, 0x2c, 0, 8, 1, b'a', 4, b't', b'e', b's', b't', 0]
    );
    assert_eq!(refusal(0xbeef, 0x01, FORMERR), [0xbe, 0xef, 0x81, 0x81, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(framed(&[1, 2, 3]), [0, 3, 1, 2, 3]);
}

/// What isn't a question tarae answers is dropped (a response — answering it could loop — or less than a header)
/// or answered by its rcode alone: another opcode NOTIMP; a message that isn't one question, a compressed or
/// over-long name, a label past 63, bytes after the question, an additional record that isn't an OPT whole within
/// the message — FORMERR. A label holding a dot is read, but escaped: it's never a host name.
#[test]
fn what_isnt_a_question_is_dropped_or_refused() {
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = MIXED_A.to_vec();
        f(&mut b);
        question(&b)
    };
    let formerr = Err(Refused::Answered { id: 0xbeef, flags: 0x01, rcode: FORMERR });
    // (the name made one label of length byte `n` and `n` bytes, then the root)
    let label = |b: &mut Vec<u8>, n: u8| {
        let mut name = vec![n];
        name.extend(std::iter::repeat_n(b'x', usize::from(n)));
        name.push(0);
        b.splice(12..20, name);
    };
    let long_name = |b: &mut Vec<u8>| {
        let mut name = Vec::new();
        for _ in 0..5 {
            name.push(63);
            name.extend([b'x'; 63]);
        }
        b.splice(12..19, name);
    };
    // (what, what was read, what it should be)
    type Case = (&'static str, Result<Question, Refused>, Result<Question, Refused>);
    let cases: Vec<Case> = vec![
        ("a response", with(&|b| b[2] |= 0x80), Err(Refused::Dropped)),
        ("under a header", question(&MIXED_A[..11]), Err(Refused::Dropped)),
        ("STATUS", with(&|b| b[2] = 0x10), Err(Refused::Answered { id: 0xbeef, flags: 0x10, rcode: NOTIMP })),
        ("two questions", with(&|b| b[5] = 2), formerr.clone()),
        ("an answer in a question", with(&|b| b[7] = 1), formerr.clone()),
        // (each with the bytes its length byte claims there: refused for what it is, not for being cut short)
        ("a compressed name", with(&|b| label(b, 0xc0)), formerr.clone()),
        ("a label past 63", with(&|b| label(b, 64)), formerr.clone()),
        ("a name past 255", with(&long_name), formerr.clone()),
        ("cut in its type", with(&|b| b.truncate(20)), formerr.clone()),
        ("bytes after it", with(&|b| b.push(0)), formerr.clone()),
        (
            "a record that isn't an OPT",
            with(&|b| {
                b[11] = 1;
                b.extend([0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
            }),
            formerr.clone(),
        ),
        (
            "an OPT past the end",
            with(&|b| {
                b[11] = 1;
                b.extend([0, 0, 41, 4, 0xb0, 0, 0, 0, 0, 0, 9]);
            }),
            formerr.clone(),
        ),
    ];
    for (what, got, want) in cases {
        assert_eq!(got, want, "{what}");
    }
    let dotted = with(&|b| {
        b[13] = b'.';
    })
    .unwrap();
    assert_eq!(dotted.name, "\\046.test");
    assert!(parse::name(&dotted.name).is_err(), "never a host name");
}

/// 6e+: records are answered as RFC 2782 and RFC 1035 §3.3.14 lay them out, each under the question's name (a
/// pointer to it): an SRV's priority, weight, port and target name; a TXT's character-strings as they came. Past the
/// size a datagram's client takes, the answer is the question alone with TC set — the client asks again over TCP;
/// over TCP it's whole.
#[test]
fn records_are_answered_on_the_wire() {
    let q = question(MIXED_A).unwrap();
    let srv = Record::Srv { priority: 1, weight: 2, port: 27017, target: "db.test".into() };
    let a = answer(&q, &Answer::Records(vec![srv]), 60);
    #[rustfmt::skip]
    let want: &[u8] = &[
        0xc0, 0x0c, 0, 33, 0, 1, 0, 0, 0, 60, 0, 15,
        0, 1, 0, 2, 0x69, 0x89, 2, b'd', b'b', 4, b't', b'e', b's', b't', 0,
    ];
    assert_eq!((&a[6..8], &a[24..]), (&[0, 1][..], want));
    let txt = Record::Txt(vec![b"authSource=admin".to_vec(), Vec::new()]);
    let a = answer(&q, &Answer::Records(vec![txt]), 60);
    assert_eq!(&a[36..], [&[16][..], b"authSource=admin", &[0]].concat(), "each string, its length first");
    let big = Answer::Records(vec![Record::Txt(vec![vec![b'x'; 255]; 3])]);
    let cut = fitted(&q, &big, 60, true);
    assert_eq!((cut[2] & 0x02, &cut[6..8], cut.len()), (0x02, &[0, 0][..], 24), "TC, the question alone");
    assert_eq!(fitted(&q, &big, 60, false), answer(&q, &big, 60), "over TCP: whole");
    let small = Answer::Records(vec![Record::Txt(vec![b"v=1".to_vec()])]);
    assert_eq!(fitted(&q, &small, 60, true), answer(&q, &small, 60), "within 512: whole");
}

/// tarae's own question (`query`), as a stub sends it: its ID, RD, one question, an OPT (RFC 6891) — readable by the
/// reader of a run's question; a name that doesn't fit the wire isn't asked.
#[test]
fn tarae_asks_as_a_stub_does() {
    let m = query(0x4242, "_mongodb._tcp.cluster.test", TYPE_SRV).unwrap();
    let q = question(&m).unwrap();
    assert_eq!(
        (q.id, q.name.as_str(), q.qtype, q.edns),
        (0x4242, "_mongodb._tcp.cluster.test", TYPE_SRV, true)
    );
    assert_eq!(&m[2..4], &[0x01, 0x00], "RD");
    assert!(query(1, &["x"; 130].join("."), TYPE_TXT).is_none(), "past 255");
}

/// A response to `query`'s question, built here: `an` records (each as written — owner, type, class, TTL, data),
/// its rcode and TC.
fn response_to(query: &[u8], rcode: u8, tc: bool, an: &[Vec<u8>]) -> Vec<u8> {
    let qend = read_name(query, 12, false).unwrap().1 + 4;
    let mut m = query[..2].to_vec();
    m.extend([0x81 | if tc { 0x02 } else { 0 }, 0x80 | rcode, 0, 1, 0, an.len() as u8, 0, 0, 0, 0]);
    m.extend_from_slice(&query[12..qend]);
    for r in an {
        m.extend_from_slice(r);
    }
    m
}

/// A record: `owner` (wire bytes, a pointer or labels), its type, TTL and data.
fn rr(owner: &[u8], kind: u16, ttl: u32, data: &[u8]) -> Vec<u8> {
    let mut r = owner.to_vec();
    r.extend(kind.to_be_bytes());
    r.extend(CLASS_IN.to_be_bytes());
    r.extend(ttl.to_be_bytes());
    r.extend((data.len() as u16).to_be_bytes());
    r.extend_from_slice(data);
    r
}

/// The response to tarae's question is read as resolvers write it: names compressed (a pointer into the question,
/// into another record's data), an alias (CNAME) followed to the records it leads to, the type asked kept and the
/// rest left (another owner's, another type), the lowest TTL. What isn't a response to it is none (another ID, not a
/// response, another question, a record past the end); TC, NXDOMAIN, SERVFAIL say so; a pointer forward isn't
/// followed (only back: no loop can be made).
#[test]
fn a_response_is_read_as_resolvers_write_it() {
    let q = query(0x1234, "_mongodb._tcp.cluster.test", TYPE_SRV).unwrap();
    // (the question's name at 12, 28 bytes: `_mongodb` 12, `_tcp` 21, `cluster` 26, `test` 34)
    let alias = [&[5][..], b"alias", &[0xc0, 26]].concat();
    let cname = rr(&[0xc0, 12], TYPE_CNAME, 100, &alias);
    // (the alias as owner: a pointer to the CNAME's data — after the question (12 + 28 + 4) and its 12-byte head)
    let alias_at: u8 = 12 + 28 + 4 + 12;
    let target = [&[3][..], b"db1", &[0xc0, 26]].concat();
    let srv = rr(&[0xc0, alias_at], TYPE_SRV, 60, &[&[0, 1, 0, 2, 0x69, 0x89][..], &target].concat());
    let stranger = rr(&[&[5][..], b"other", &[4], b"test", &[0]].concat(), TYPE_SRV, 9, &[0; 7]);
    let a_rec = rr(&[0xc0, 12], TYPE_A, 9, &[192, 0, 2, 1]);
    let m = response_to(&q, NOERROR, false, &[cname, srv, stranger, a_rec]);
    let want = Record::Srv { priority: 1, weight: 2, port: 27017, target: "db1.cluster.test".into() };
    assert_eq!(response(&m, &q), Ok(Found::Records(vec![want], 60)));
    // TXT: its strings
    let tq = query(7, "cluster.test", TYPE_TXT).unwrap();
    let txt = rr(&[0xc0, 12], TYPE_TXT, 30, &[&[5][..], b"hello", &[0]].concat());
    let cut_txt = rr(&[0xc0, 12], TYPE_TXT, 30, &[9, b'x']);
    let m = response_to(&tq, NOERROR, false, &[txt, cut_txt]);
    assert_eq!(response(&m, &tq), Ok(Found::Records(vec![Record::Txt(vec![b"hello".to_vec(), vec![]])], 30)));
    // What it came to
    assert_eq!(response(&response_to(&q, NOERROR, false, &[]), &q), Ok(Found::Empty));
    assert_eq!(response(&response_to(&q, NXDOMAIN, false, &[]), &q), Ok(Found::No));
    assert_eq!(response(&response_to(&q, SERVFAIL, false, &[]), &q), Ok(Found::Failed));
    assert_eq!(response(&response_to(&q, NOERROR, true, &[]), &q), Ok(Found::Truncated));
    // Not a response to it
    let mut other = response_to(&q, NOERROR, false, &[]);
    other[1] ^= 1;
    assert_eq!(response(&other, &q), Err(()), "another ID");
    let mut asked = response_to(&q, NOERROR, false, &[]);
    asked[2] &= 0x7f;
    assert_eq!(response(&asked, &q), Err(()), "not a response");
    let same_id = query(0x1234, "cluster.test", TYPE_TXT).unwrap();
    assert_eq!(response(&response_to(&same_id, NOERROR, false, &[]), &q), Err(()), "another question");
    let mut past = response_to(&tq, NOERROR, false, &[rr(&[0xc0, 12], TYPE_TXT, 1, &[1, b'a'])]);
    past.truncate(past.len() - 1);
    assert_eq!(response(&past, &tq), Err(()), "a record past the end");
    // (its owner a pointer to its own data, a name there — `hello` — further on: question 12..30, the record's head
    // 30..42)
    let forward = rr(&[0xc0, 42], TYPE_TXT, 1, &[&[5][..], b"hello", &[0]].concat());
    assert_eq!(response(&response_to(&tq, NOERROR, false, &[forward]), &tq), Err(()), "a pointer forward");
    let mixed = response_to(&tq, NOERROR, false, &[]);
    let mut upper = mixed.clone();
    upper[13] = b'C';
    assert_eq!(response(&upper, &tq), Ok(Found::Empty), "the question's case changed by the server");
}

/// The response reader never panics: 100,000 mutations of the responses above (bits, lengths, pointers, cuts) are
/// each read, or not taken — and what's read holds only records whole within the message.
#[test]
fn the_response_reader_never_panics() {
    let q = query(0x1234, "_mongodb._tcp.cluster.test", TYPE_SRV).unwrap();
    let target = [&[3][..], b"db1", &[0xc0, 26]].concat();
    let srv = rr(&[0xc0, 12], TYPE_SRV, 60, &[&[0, 1, 0, 2, 0x69, 0x89][..], &target].concat());
    let cname = rr(&[0xc0, 12], TYPE_CNAME, 100, &[&[5][..], b"alias", &[0xc0, 26]].concat());
    let base = response_to(&q, NOERROR, false, &[cname, srv]);
    let mut rng = Rng(0x5e5e_0d0d_1234_9876);
    let (mut read, mut not) = (0, 0);
    for _ in 0..100_000 {
        let mut b = base.clone();
        mutate(&mut rng, &mut b, 4096);
        match response(&b, &q) {
            Ok(Found::Records(r, _)) => {
                read += 1;
                assert!(r.len() <= RECORDS_MAX);
                for x in r {
                    assert!(x.data().is_some(), "{x:?} writes back");
                }
            }
            Ok(_) => read += 1,
            Err(()) => not += 1,
        }
    }
    assert!(read > 1000 && not > 1000, "{read} / {not}");
}

/// The reader never panics: 100,000 mutations of the questions above (bits, lengths, labels, cuts, runs past
/// `QUESTION_MAX` — `parse`'s loop) are each read as a question, or refused. A question read is whole: its name
/// fits the wire (≤ 255) where it's a host name, and the answer written to it echoes it and reads back as one
/// answer to one question.
#[test]
fn the_dns_reader_never_panics() {
    let mut rng = Rng(0x6d6d_0d0d_5eed_0e01);
    let (mut read, mut refused) = (0, 0);
    for i in 0..100_000 {
        let mut b = if i % 2 == 0 { GLIBC_A } else { MIXED_A }.to_vec();
        mutate(&mut rng, &mut b, QUESTION_MAX);
        match question(&b) {
            Ok(q) => {
                read += 1;
                if let Ok(n) = parse::name(&q.name) {
                    assert!(wire_name(&n).is_some(), "{n}");
                }
                let a = answer(&q, &Answer::Rcode(REFUSED), 300);
                assert_eq!((&a[..2], &a[4..6]), (&b[..2], &[0, 1][..]));
                assert_eq!(&a[12..12 + q.raw.len()], &q.raw[..]);
            }
            Err(_) => refused += 1,
        }
    }
    assert!(read > 1000 && refused > 10_000, "{read} / {refused}");
}
