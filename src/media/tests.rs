use super::*;

/// `img` as a file of `format`, written by `image`'s own encoder.
fn file_of(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// A white PNG `w`×`h`.
pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255]));
    file_of(&DynamicImage::ImageRgb8(img), ImageFormat::Png)
}

/// A PNG `w`×`h` of noise — one that compresses badly (xorshift, the same each time).
pub(crate) fn noise(w: u32, h: u32) -> Vec<u8> {
    let mut x: u32 = 0x9E37_79B9;
    let img = image::RgbImage::from_fn(w, h, |_, _| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let [a, b, c, _] = x.to_le_bytes();
        image::Rgb([a, b, c])
    });
    file_of(&DynamicImage::ImageRgb8(img), ImageFormat::Png)
}

/// A decompression bomb's stand-in: a PNG whose header says `w`×`h` (8-bit RGB) over a few bytes of data —
/// written by hand, as no encoder makes one.
pub(crate) fn bomb(w: u32, h: u32) -> Vec<u8> {
    fn crc(b: &[u8]) -> u32 {
        let mut c = !0u32;
        for &x in b {
            c ^= u32::from(x);
            for _ in 0..8 {
                c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let body = [kind, data].concat();
        out.extend(&body);
        out.extend(crc(&body).to_be_bytes());
    };
    chunk(b"IHDR", &[&w.to_be_bytes()[..], &h.to_be_bytes(), &[8, 2, 0, 0, 0]].concat());
    chunk(b"IDAT", &[0x78, 0x9C, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01]);
    chunk(b"IEND", &[]);
    out
}

/// Each kind by its first bytes (`image`'s table), a PDF by its `%PDF-`; text and other formats are none.
#[test]
fn kinds_come_from_the_first_bytes() {
    let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 4, image::Rgb([1, 2, 3])));
    assert_eq!(kind(&png(8, 4)), Some(Kind::Png));
    assert_eq!(kind(&file_of(&img, ImageFormat::Jpeg)), Some(Kind::Jpeg));
    assert_eq!(kind(&file_of(&img, ImageFormat::Gif)), Some(Kind::Gif));
    assert_eq!(kind(b"RIFF\0\0\0\0WEBPVP8L"), Some(Kind::Webp));
    assert_eq!(kind(b"%PDF-1.7\n"), Some(Kind::Pdf));
    for other in [&b"fn main() {}\n"[..], b"", b"\x89PN", b"RIFF\0\0\0\0WAVEfmt ", b"BM\0\0"] {
        assert_eq!(kind(other), None, "{other:?}");
    }
}

/// A size fit inside a box keeps its shape and is never made bigger — a square one (the model's), a wide one (a
/// viewer's cells), one with no bound on a side.
#[test]
fn a_fit_keeps_the_shape_and_never_grows() {
    assert_eq!(fit(2880, 1800, 2000, 2000), (2000, 1250));
    assert_eq!(fit(1000, 3000, 2000, 2000), (667, 2000));
    assert_eq!(fit(640, 480, 2000, 2000), (640, 480));
    assert_eq!(fit(40000, 1, 2000, 2000), (2000, 1), "never below a pixel");
    assert_eq!(fit(2880, 1800, 160, 80), (128, 80), "the tighter side decides");
    assert_eq!(fit(2000, 1250, 1000, u32::MAX), (1000, 625));
}

/// The core a viewer will reuse: a file's size without decoding it; decoded to RGBA within the limits, a bomb
/// refused before it's decoded (said without the model's words); fit into a box.
#[test]
fn the_core_decodes_within_the_limits_and_fits_into_a_box() {
    let file = png(300, 120);
    assert_eq!(size(Kind::Png, &file), Ok((300, 120)));
    let decoded = decode(Kind::Png, &file).unwrap();
    let pixels = fit_into(decoded.clone(), 1000, 1000);
    assert_eq!(
        (pixels.dimensions(), pixels.get_pixel(0, 0).0),
        ((300, 120), [255, 255, 255, 255]),
        "never enlarged"
    );
    assert_eq!(fit_into(decoded, 100, 100).dimensions(), (100, 40));
    // (more than twice too big: down by the box filter first, then the finer one)
    assert_eq!(fit_into(decode(Kind::Png, &png(3000, 1500)).unwrap(), 160, 80).dimensions(), (160, 80));
    assert_eq!(
        decode(Kind::Png, &bomb(15_000, 15_000)).unwrap_err(),
        "too big to decode here (at most 20000 pixels a side, 256 MB decoded)"
    );
}

/// An image within the limits goes as it is, untouched; one past 2000 a side is scaled to fit, a PNG as PNG — and
/// what's sent is what's said; its cost is one token a 28×28 patch.
#[test]
fn an_image_goes_as_it_is_or_scaled_to_fit() {
    let small = png(400, 300);
    let (img, from, sent) = for_model(Kind::Png, small.clone()).unwrap();
    assert_eq!((img.tokens(), sent, from), (15 * 11, Sent::AsIs, (400, 300)));
    assert_eq!((img.bytes, img.mime), (small, "image/png"));
    let (img, from, sent) = for_model(Kind::Png, png(3000, 1500)).unwrap();
    assert_eq!(
        (img.mime, from, (img.w, img.h), sent),
        ("image/png", (3000, 1500), (2000, 1000), Sent::Png { best: false })
    );
    let back = image::load_from_memory(&img.bytes).unwrap();
    assert_eq!((back.width(), back.height()), (2000, 1000), "what's sent is what's said");
}

/// A small picture goes as it is only once it decoded whole: one cut short behind a good header — which the API
/// couldn't read, and which would stay in the conversation — is refused, said.
#[test]
fn a_cut_picture_is_refused_not_sent_as_it_is() {
    let whole = noise(64, 64);
    let cut = whole[..whole.len() / 2].to_vec();
    assert_eq!(size(Kind::Png, &cut), Ok((64, 64)), "its header is good");
    let err = for_model(Kind::Png, cut).unwrap_err();
    assert!(err.starts_with("it couldn't be decoded: ") && err.ends_with(" — not sent"), "{err}");
}

/// The ladder's last word says the cap as it is — 3.75 MB, not a rounded 3.
#[test]
fn the_ladders_end_says_the_cap_as_it_is() {
    assert_eq!(over(1000), "still over 3.75 MB at 1000 wide, JPEG quality 20 — not sent");
}

/// A big picture that won't compress is scaled, then goes down Claude Code's ladder until it's under the cap.
#[test]
fn a_big_image_is_scaled_and_under_the_cap() {
    let big = noise(3000, 2000);
    assert!(big.len() > RAW_MAX);
    let (img, from, sent) = for_model(Kind::Png, big).unwrap();
    assert_eq!((from, (img.w, img.h), img.mime), ((3000, 2000), (2000, 1333), "image/jpeg"));
    assert!(matches!(sent, Sent::Jpeg(_)), "{sent:?}");
    assert!(img.bytes.len() <= RAW_MAX, "{} bytes", img.bytes.len());
    let back = image::load_from_memory(&img.bytes).unwrap();
    assert_eq!((back.width(), back.height()), (2000, 1333));
}

/// A decompression bomb — a header of 15000×15000 over a few bytes, 675 MB decoded — is refused before it's
/// decoded, said; so is one past the pixels a side.
#[test]
fn a_decompression_bomb_is_refused_said() {
    for (w, h) in [(15_000, 15_000), (SIDE_MAX + 1, 10)] {
        let err = for_model(Kind::Png, bomb(w, h)).unwrap_err();
        assert_eq!(
            err, "too big to decode here (at most 20000 pixels a side, 256 MB decoded) — not sent",
            "{w}×{h}"
        );
    }
}
