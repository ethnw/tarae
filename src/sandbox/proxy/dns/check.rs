//! The properties the DNS readers keep, one function a reader — whatever the bytes, each holds or the function
//! panics: the in-tree loops (`tests.rs`) call them on seeded mutations, the fuzz targets (`fuzz/parsers`, built with
//! `--cfg fuzzing`) on libFuzzer's inputs. Each says what came of its input in a word.

use super::super::parse;
use super::*;

/// Whatever a run's question holds, it's read as a question or refused, never a panic. A question read is whole:
/// its name fits the wire (≤ 255) where it's a host name, and the answer written to it echoes it and reads back as
/// one answer to one question.
pub(crate) fn question(b: &[u8]) -> &'static str {
    let Ok(q) = super::question(b) else { return "refused" };
    if let Ok(n) = parse::name(&q.name) {
        assert!(wire_name(&n).is_some(), "{n}");
    }
    let a = answer(&q, &Answer::Rcode(REFUSED), 300);
    assert_eq!((&a[..2], &a[4..6]), (&b[..2], &[0, 1][..]));
    assert_eq!(&a[12..12 + q.raw.len()], &q.raw[..]);
    "read"
}

/// The question tarae asks the resolver that `response` reads against: `_mongodb._tcp.cluster.test`'s SRV records.
pub(crate) fn asked() -> Vec<u8> {
    super::query(0x1234, "_mongodb._tcp.cluster.test", TYPE_SRV).unwrap()
}

/// Whatever a resolver's response to `asked` holds, it's read or not taken, never a panic — and what's read holds
/// only records whole within the message, at most `RECORDS_MAX`.
pub(crate) fn response(b: &[u8], asked: &[u8]) -> &'static str {
    match super::response(b, asked) {
        Ok(Found::Records(r, _)) => {
            assert!(r.len() <= RECORDS_MAX);
            for x in r {
                assert!(x.data().is_some(), "{x:?} writes back");
            }
            "records"
        }
        Ok(_) => "read",
        Err(()) => "not taken",
    }
}
