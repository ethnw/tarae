//! A gzip body unpacked (RFC 1952), the fetch's — gzip only (the user's, 2026-10-23: zstd and br would be more
//! parsers for little). A server's bytes are anyone's: the member's header read by hand within its bounds, its deflate
//! stream inflated by miniz_oxide (what unpacks the embedded queries already) a step at a time and stopped once past
//! `max` — a size, never a ratio (a real page packs 20×: the 4.2 MB Wikipedia page is 194 KB) —, its CRC-32 and
//! length checked against the trailer. Pure: bytes in, an answer out, never a panic (`check::gzip`, fuzzed).

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

/// What a body unpacked to.
#[derive(Debug, PartialEq, Eq)]
pub enum Unpacked {
    /// All of it, its check passed.
    Whole(Vec<u8>),
    /// Its first `max` bytes and one more: it unpacks past `max` (the reader says the page was cut).
    Cut(Vec<u8>),
}

/// The header's flags (RFC 1952 §2.3.1).
const FHCRC: u8 = 2;
const FEXTRA: u8 = 4;
const FNAME: u8 = 8;
const FCOMMENT: u8 = 16;

/// Inflated this much at a time.
const STEP: usize = 32 << 10;

/// Where the deflate stream starts: past the member's header — its magic, method 8 (deflate), no reserved flag, and
/// the extra field, name, comment and header CRC its flags say, each within the bytes.
fn header(b: &[u8]) -> Result<usize, &'static str> {
    match b.get(..4) {
        Some([0x1f, 0x8b, 8, flags]) if flags & 0xe0 == 0 => {}
        Some([0x1f, 0x8b, 8, _]) => return Err("a reserved flag in its header"),
        Some([0x1f, 0x8b, ..]) => return Err("a method other than deflate"),
        _ => return Err("not gzip"),
    }
    let flags = b[3];
    let mut at = 10;
    let short = "its header cut short";
    if flags & FEXTRA != 0 {
        let n = b.get(at..at + 2).ok_or(short)?;
        at += 2 + usize::from(u16::from_le_bytes([n[0], n[1]]));
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            at += b.get(at..).ok_or(short)?.iter().position(|&c| c == 0).ok_or(short)? + 1;
        }
    }
    if flags & FHCRC != 0 {
        at += 2;
    }
    if at > b.len() { Err(short) } else { Ok(at) }
}

/// `b`, one gzip member, unpacked — at most `max` bytes and one; Err: what it isn't (not gzip, cut short, a stream
/// miniz_oxide can't follow, a check that failed, bytes after its end).
pub fn gunzip(b: &[u8], max: usize) -> Result<Unpacked, &'static str> {
    let mut at = header(b)?;
    let mut state = InflateState::new_boxed(DataFormat::Raw);
    let (mut out, mut step) = (Vec::new(), vec![0u8; STEP]);
    loop {
        let r = inflate(&mut state, &b[at..], &mut step, MZFlush::None);
        at += r.bytes_consumed;
        let made = &step[..r.bytes_written];
        if out.len() + made.len() > max {
            let room = max + 1 - out.len();
            out.extend_from_slice(&made[..room]);
            return Ok(Unpacked::Cut(out));
        }
        out.extend_from_slice(made);
        match r.status {
            Ok(MZStatus::StreamEnd) => break,
            Ok(_) if r.bytes_consumed == 0 && r.bytes_written == 0 => return Err("its stream cut short"),
            Ok(_) => {}
            Err(_) if at >= b.len() => return Err("its stream cut short"),
            Err(_) => return Err("a deflate stream it can't read"),
        }
    }
    let t = b.get(at..at + 8).ok_or("its trailer cut short")?;
    let (crc, len) =
        (u32::from_le_bytes([t[0], t[1], t[2], t[3]]), u32::from_le_bytes([t[4], t[5], t[6], t[7]]));
    if crc32fast::hash(&out) != crc {
        return Err("its CRC doesn't match what it unpacked to");
    }
    // (the length modulo 2^32, as the format keeps it)
    if out.len() as u32 != len {
        return Err("its length doesn't match what it unpacked to");
    }
    if at + 8 != b.len() {
        return Err("bytes after its end");
    }
    Ok(Unpacked::Whole(out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `data` as gzip: a header with `flags`' fields, deflate, the trailer.
    pub(crate) fn gzip(data: &[u8], flags: u8) -> Vec<u8> {
        let mut b = vec![0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 3];
        if flags & FEXTRA != 0 {
            b.extend_from_slice(&[3, 0, b'a', b'b', b'c']);
        }
        if flags & FNAME != 0 {
            b.extend_from_slice(b"page.html\0");
        }
        if flags & FCOMMENT != 0 {
            b.extend_from_slice(b"a comment\0");
        }
        if flags & FHCRC != 0 {
            b.extend_from_slice(&[0, 0]);
        }
        b.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(data, 6));
        b.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b
    }

    /// A member unpacks whole whatever fields its header holds; past `max` it's cut at `max` and one; a wrong CRC or
    /// length, a cut stream, bytes after it, another method or not gzip at all refused.
    #[test]
    fn a_gzip_body_unpacks_within_its_limit() {
        let page = b"<p>hello</p>".repeat(1000);
        for flags in [0, FEXTRA, FNAME | FCOMMENT, FHCRC | FNAME | FEXTRA] {
            assert_eq!(gunzip(&gzip(&page, flags), 1 << 20), Ok(Unpacked::Whole(page.clone())), "{flags}");
        }
        let Ok(Unpacked::Cut(cut)) = gunzip(&gzip(&page, 0), 100) else { panic!("cut") };
        assert_eq!(cut, page[..101]);
        let whole = gzip(&page, 0);
        let n = whole.len();
        let mut bad_crc = whole.clone();
        bad_crc[n - 8] ^= 1;
        let mut bad_len = whole.clone();
        bad_len[n - 1] ^= 1;
        let refused = [
            (bad_crc, "its CRC doesn't match what it unpacked to"),
            (bad_len, "its length doesn't match what it unpacked to"),
            (whole[..n / 2].to_vec(), "its stream cut short"),
            (whole[..n - 3].to_vec(), "its trailer cut short"),
            ([&whole[..], b"x"].concat(), "bytes after its end"),
            ([&[0x1f, 0x8b, 7, 0][..], &whole[4..]].concat(), "a method other than deflate"),
            ([&[0x1f, 0x8b, 8, 0x20][..], &whole[4..]].concat(), "a reserved flag in its header"),
            (whole[..12].to_vec(), "its stream cut short"),
            (b"<html>".to_vec(), "not gzip"),
            (vec![0x1f, 0x8b, 8, FNAME, 0, 0, 0, 0, 0, 3, b'n'], "its header cut short"),
        ];
        for (b, why) in refused {
            assert_eq!(gunzip(&b, 1 << 20), Err(why), "{b:?}");
        }
    }

    /// A bomb — 64 MB of one byte, 64 KB packed — stops at the limit, never made whole.
    #[test]
    fn a_bomb_stops_at_the_limit() {
        let packed = gzip(&vec![b'a'; 64 << 20], 0);
        assert!(packed.len() < 128 << 10, "{}", packed.len());
        let Ok(Unpacked::Cut(cut)) = gunzip(&packed, 8 << 20) else { panic!("cut") };
        assert_eq!(cut.len(), (8 << 20) + 1);
    }

    /// The fuzz target's seeds (`fuzz/parsers/seeds/gzip`): pages packed with each header field, as much room as
    /// they need and too little (`check::gzip_input`'s first byte).
    #[test]
    #[ignore = "writes fuzz/parsers/seeds: by hand"]
    fn writes_fuzz_seeds() {
        let page = b"<html><p>a page</p></html>".repeat(40);
        let seeds: Vec<(String, Vec<u8>)> =
            [(0, 255u8), (FNAME | FCOMMENT, 255), (FEXTRA | FHCRC, 255), (0, 2)]
                .into_iter()
                .enumerate()
                .map(|(k, (flags, room))| (format!("page-{k}"), [&[room][..], &gzip(&page, flags)].concat()))
                .collect();
        let words: Vec<&str> = seeds.iter().map(|(_, b)| crate::web::check::gzip_input(b)).collect();
        assert_eq!(words, ["whole", "whole", "whole", "cut"]);
        crate::mutate::fuzz_seeds("gzip", seeds);
    }

    /// Whatever a gzip body holds, it unpacks as `check::gzip` has it: real ones mutated 50,000 times.
    #[test]
    fn the_gzip_reader_never_panics() {
        use crate::mutate::{Rng, bytes};
        let mut rng = Rng(0x921b_e5ee_d1ce_0008);
        let (mut whole, mut refused) = (0, 0);
        for _ in 0..50_000 {
            let data: Vec<u8> = (0..rng.below(2000)).map(|_| b"<p>a </p>"[rng.below(9)]).collect();
            let mut b = gzip(&data, *rng.pick(&[0, FNAME, FEXTRA | FHCRC]));
            if rng.below(8) > 0 {
                bytes(&mut rng, &mut b, &[&[0x1f, 0x8b], &[0; 8], &[0xff; 4]]);
            }
            match crate::web::check::gzip(&b, 1 + rng.below(4000)) {
                "whole" => whole += 1,
                "refused" => refused += 1,
                _ => {}
            }
        }
        assert!(whole > 5000 && refused > 20_000, "{whole} / {refused}");
    }
}
