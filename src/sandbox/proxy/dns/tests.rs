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
