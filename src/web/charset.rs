//! A page's bytes as text (web-plan D3.4): a byte-order mark, else the `Content-Type` header's charset, else what
//! the page's own `<meta>` says in its first 1024 bytes (the HTML spec's prescan, the two forms), else UTF-8. Labels
//! are the Encoding Standard's (`euc-kr` is windows-949, as browsers read it); bytes that don't decode become U+FFFD.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8};

/// The text, and the encoding it was read in.
pub fn decode<'a>(bytes: &'a [u8], content_type: Option<&str>) -> (Cow<'a, str>, &'static Encoding) {
    let enc = Encoding::for_bom(bytes)
        .map(|(e, _)| e)
        .or_else(|| content_type.and_then(header_charset))
        .or_else(|| meta_charset(bytes))
        .unwrap_or(UTF_8);
    let (text, used, _) = enc.decode(bytes);
    (text, used)
}

/// `text/html; charset=EUC-KR` → its encoding.
fn header_charset(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| label(v))?
    })
}

/// `<meta charset="x">` or `<meta http-equiv="Content-Type" content="text/html; charset=x">` in the first 1024
/// bytes. A page that says UTF-16 there is read as UTF-8 (the spec's rule: bytes it could read that far are ASCII).
fn meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    let mut rest = head.as_str();
    while let Some(i) = rest.find("<meta") {
        let tag = &rest[i..];
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        if let Some(j) = tag.find("charset") {
            let v = tag[j + "charset".len()..].trim_start();
            if let Some(v) = v.strip_prefix('=') {
                let v = v.trim_start().trim_start_matches(['"', '\'']);
                let end = v
                    .find(|c: char| matches!(c, '"' | '\'' | ';' | '/') || c.is_whitespace())
                    .unwrap_or(v.len());
                if let Some(e) = label(&v[..end]) {
                    return Some(if e.name().starts_with("UTF-16") { UTF_8 } else { e });
                }
            }
        }
        rest = &rest[i + 5..];
    }
    None
}

fn label(v: &str) -> Option<&'static Encoding> {
    Encoding::for_label(v.trim().trim_matches(['"', '\'']).as_bytes())
}
