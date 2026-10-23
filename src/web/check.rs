//! The reader's property, for its loop in `tests.rs` and `fuzz/parsers`' `web` target: any bytes read — never a
//! panic, the limits held, every link's place a link in the text, every anchor on a line there.

use super::{Prefer, Read, read};

/// libFuzzer's input: its first byte says how it's read — which part (`& 3`), and the content type (`>> 2 & 3`:
/// none — sniffed —, HTML, HTML in EUC-KR, plain text); the rest is the page.
pub fn page_input(b: &[u8]) {
    let Some((&how, bytes)) = b.split_first() else { return };
    let prefer = [Prefer::Guess, Prefer::Main, Prefer::Article, Prefer::Page][usize::from(how & 3)];
    let content_type = [None, Some("text/html"), Some("text/html; charset=euc-kr"), Some("text/plain")]
        [usize::from(how >> 2 & 3)];
    page(bytes, content_type, prefer);
}

/// libFuzzer's input for `gzip`: its first byte says how much it may make (64 × it; 0: one byte), the rest the body.
pub fn gzip_input(b: &[u8]) -> &'static str {
    let Some((&m, rest)) = b.split_first() else { return "refused" };
    gzip(rest, (64 * usize::from(m)).max(1))
}

/// Whatever a gzip body holds: never a panic, never more than `max` bytes and one; one unpacked whole ends with its
/// CRC of what it made, one cut is `max` and one.
pub fn gzip(b: &[u8], max: usize) -> &'static str {
    match super::gzip::gunzip(b, max) {
        Ok(super::gzip::Unpacked::Whole(out)) => {
            assert!(out.len() <= max, "{} past {max}", out.len());
            assert_eq!(b[b.len() - 8..b.len() - 4], crc32fast::hash(&out).to_le_bytes(), "its check");
            "whole"
        }
        Ok(super::gzip::Unpacked::Cut(out)) => {
            assert_eq!(out.len(), max + 1);
            "cut"
        }
        Err(_) => "refused",
    }
}

/// `bytes` read as `content_type` says: what's read holds.
pub fn page(bytes: &[u8], content_type: Option<&str>, prefer: Prefer) {
    let p = read(bytes, "https://example.com/a/b.html?q#f", content_type, prefer);
    assert!(p.text.len() <= super::reader::TEXT_MAX, "{} bytes of text", p.text.len());
    if p.read == Read::None {
        assert!(p.text.is_empty() && p.said.is_some());
        return;
    }
    for l in &p.links {
        let md = p.text.get(l.from..l.to).unwrap_or_else(|| panic!("a link at {}..{}", l.from, l.to));
        assert!(md.starts_with('[') && md.ends_with(')'), "{md:?}");
    }
    let lines = p.text.lines().count();
    assert!(p.anchors.iter().all(|a| a.line <= lines), "an anchor past the text's {lines} lines");
    assert!(!p.title.contains('\n'), "{:?}", p.title);
}
