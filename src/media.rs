//! What `read` hands Claude that isn't text (readimg, 2026-10-20): an image as MCP image content. Measured first
//! (tests/live.rs `mcp_image_and_pdf_reach_the_model`, Claude Code 2.1.302): an MCP `image` block reaches the model
//! as an image; a PDF doesn't — as an embedded resource or an `image` with `application/pdf`, Claude Code saves the
//! bytes under `~/.claude/projects/` and hands the model a line naming that file. PDFs are a later piece
//! (docs/roadmap.md): `read` says it doesn't read their pages yet.
//!
//! An image's bytes are anyone's (a cloned repository's, one Claude downloaded), so they're parsed only by
//! memory-safe code, in-process: the `image` crate (pure Rust; PNG, JPEG, GIF, WebP only) — no external converter,
//! no decoder of tarae's own (a format's header included: its size is the crate's to read). Decoding has limits
//! (`limits`): a picture that would take more memory than `ALLOC_MAX`, or wider or taller than `SIDE_MAX`, is
//! refused before it's decoded (a decompression bomb), said. A worker's, all of it.
//!
//! Two parts (the user, 2026-10-20 — tarae will show pictures itself later: a `.png` opened, the picker's preview,
//! the chat's tool row, markdown's pictures): the core — a file's `size`, `decode` within the limits to RGBA
//! (`Pixels`), `fit_into` a box — which a viewer reuses as it is; and what goes to Claude, `for_model`, the ladder
//! on top of it.
//!
//! Sizes and the ladder as Claude Code's Read has them (its bundled code, 2.1.302): one inside 2000×2000 and within
//! 3.75 MiB (5 MiB of base64) goes as it is — once it decoded whole here (one cut or broken, which the API couldn't
//! read, would stay in the conversation's history); else it's fit inside 2000×2000, its shape kept, never enlarged, and
//! written as it was (a JPEG as JPEG, the rest as PNG — Claude Code keeps WebP and GIF as theirs; `image`'s WebP
//! encoder is lossless only, so PNG); still over 3.75 MiB: a PNG source at the strongest compression (Claude Code
//! also quantizes to a palette there — left out: it needs a quantizer `image` doesn't have), then JPEG at quality
//! 80, 60, 40, 20, then at most 1000 wide at 20. What's still over isn't sent (Claude Code sends that last one
//! whatever its size; the API would refuse it).

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, ImageReader, Limits};

/// Long edge at most · bytes at most (5 MiB of base64) — Claude Code's Read's (`maxWidth`/`maxHeight`,
/// `maxBase64Size`, `targetRawSize`).
pub const EDGE_MAX: u32 = 2000;
pub const RAW_MAX: usize = 5 * 1024 * 1024 * 3 / 4;
/// The last step's width — Claude Code's.
const LAST_WIDTH: u32 = 1000;
/// A file read for a picture at most (bytes).
pub const FILE_MAX: u64 = 100 * 1024 * 1024;
/// Decoding at most: a side's pixels, and the memory the decoder's picture takes (64 megapixels of 8-bit RGBA).
/// The most a picture takes on the worker is that, and while it's scaled a buffer the size of twice the box
/// (`fit_into`: about 130 MB at 2000×2000 — its RGBA and the finer filter's working rows); never a copy of the
/// whole picture in another format.
pub const SIDE_MAX: u32 = 20_000;
pub const ALLOC_MAX: u64 = 256 * 1024 * 1024;
/// The pictures of one answer at most, as sent (base64): the API takes a request of about 32 MB, the conversation
/// so far included.
pub const ANSWER_MAX: usize = 10 * 1024 * 1024;

/// What a file's first bytes say it is — a picture `read` shows, or a PDF (said, not read yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Pdf,
}

impl Kind {
    pub fn said(self) -> &'static str {
        match self {
            Kind::Png => "PNG",
            Kind::Jpeg => "JPEG",
            Kind::Gif => "GIF",
            Kind::Webp => "WebP",
            Kind::Pdf => "PDF",
        }
    }

    fn format(self) -> Option<ImageFormat> {
        match self {
            Kind::Png => Some(ImageFormat::Png),
            Kind::Jpeg => Some(ImageFormat::Jpeg),
            Kind::Gif => Some(ImageFormat::Gif),
            Kind::Webp => Some(ImageFormat::WebP),
            Kind::Pdf => None,
        }
    }
}

/// A picture as the model gets it: an MCP `image` block.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

impl Image {
    /// What it costs the model, about: one token a 28×28 patch — Claude Code's own estimate (2.1.302); a
    /// screenshot's turn measured it so (tests/live.rs `chat_reads_a_screenshot_ab`).
    pub fn tokens(&self) -> u64 {
        u64::from(self.w.div_ceil(28)) * u64::from(self.h.div_ceil(28))
    }

    /// Its bytes as sent: base64, padded.
    pub fn sent_len(&self) -> usize {
        self.bytes.len().div_ceil(3) * 4
    }
}

/// A file's kind from its first bytes (`image`'s own table; a PDF by its `%PDF-`) — None: text, or a kind
/// `read` doesn't show.
pub fn kind(b: &[u8]) -> Option<Kind> {
    if b.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    match image::guess_format(b).ok()? {
        ImageFormat::Png => Some(Kind::Png),
        ImageFormat::Jpeg => Some(Kind::Jpeg),
        ImageFormat::Gif => Some(Kind::Gif),
        ImageFormat::WebP => Some(Kind::Webp),
        _ => None,
    }
}

/// How a picture went: as it is, or how it was written (said in `read`'s heading).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    AsIs,
    /// As PNG (`best`: at the strongest compression).
    Png {
        best: bool,
    },
    /// As JPEG at this quality.
    Jpeg(u8),
}

// ── The core: a picture's size, decoded, fit into a box — what a viewer of tarae's own will reuse as it is ────

/// A picture as drawn or encoded: RGBA, 8 bits a channel — what `fit_into` makes.
pub type Pixels = image::RgbaImage;

/// A picture as its decoder gave it, in its own pixel format (no copy into another: `fit_into` converts what it
/// scaled down).
#[derive(Clone, Debug)]
pub struct Decoded(DynamicImage);

/// The decoding limits (`SIDE_MAX`, `ALLOC_MAX`).
fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(SIDE_MAX);
    l.max_image_height = Some(SIDE_MAX);
    l.max_alloc = Some(ALLOC_MAX);
    l
}

fn reader(kind: Kind, bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, String> {
    let format = kind.format().ok_or("not a picture")?;
    let mut r = ImageReader::with_format(Cursor::new(bytes), format);
    r.limits(limits());
    Ok(r)
}

/// The size an image file says it has (its header, read by `image`, within the limits) — nothing decoded.
pub fn size(kind: Kind, bytes: &[u8]) -> Result<(u32, u32), String> {
    reader(kind, bytes)?.into_dimensions().map_err(|e| refused(e, "its size couldn't be read"))
}

/// An image file decoded, within the limits: a picture past them — a decompression bomb, or just too big — is
/// refused before it's decoded, said. A GIF: its first frame.
pub fn decode(kind: Kind, bytes: &[u8]) -> Result<Decoded, String> {
    let decoded = reader(kind, bytes)?.decode().map_err(|e| refused(e, "it couldn't be decoded"))?;
    Ok(Decoded(decoded))
}

/// What a decoder's error says: past the limits, else `what` and why.
fn refused(e: image::ImageError, what: &str) -> String {
    match e {
        image::ImageError::Limits(_) => format!(
            "too big to decode here (at most {SIDE_MAX} pixels a side, {} MB decoded)",
            ALLOC_MAX / (1024 * 1024)
        ),
        e => format!("{what}: {e}"),
    }
}

/// `(w, h)` fit inside a `bw`×`bh` box, its shape kept, never made bigger nor below a pixel.
pub fn fit(w: u32, h: u32, bw: u32, bh: u32) -> (u32, u32) {
    if w <= bw && h <= bh {
        return (w, h);
    }
    // (the tighter side decides: w/bw against h/bh, compared without division)
    let (num, den) = match u64::from(w) * u64::from(bh) >= u64::from(h) * u64::from(bw) {
        true => (u64::from(bw), u64::from(w)),
        false => (u64::from(bh), u64::from(h)),
    };
    let scale = |x: u32| ((u64::from(x) * num + den / 2) / den).max(1) as u32;
    (scale(w).min(bw), scale(h).min(bh))
}

/// `img` fit inside a `bw`×`bh` box (`fit`), as RGBA — as it is when it already fits. More than twice too big,
/// a box filter takes it down to twice the size first (it allocates only its output; the picture is let go
/// after), then Lanczos3 the rest, as Claude Code's `sharp` scales: the finer filter's working rows and the RGBA
/// copy are the box's size, never the picture's.
pub fn fit_into(img: Decoded, bw: u32, bh: u32) -> Pixels {
    let (w, h) = fit(img.0.width(), img.0.height(), bw, bh);
    let (tw, th) = (w.saturating_mul(2), h.saturating_mul(2));
    let near = match img.0.width() > tw && img.0.height() > th {
        true => img.0.thumbnail_exact(tw, th),
        false => img.0,
    };
    let rgba = near.into_rgba8();
    match rgba.dimensions() == (w, h) {
        true => rgba,
        false => image::imageops::resize(&rgba, w, h, image::imageops::FilterType::Lanczos3),
    }
}

// ── What goes to Claude: the ladder ──────────────────────────────────────────────────────────────────────────────────

/// An image file's bytes as the model gets them (the module's ladder, on the core): (picture, its size as the file
/// has it, how it went). Err: why it isn't sent — its size unread, past the decoding limits, or over `RAW_MAX` at
/// the ladder's end.
pub fn for_model(kind: Kind, bytes: Vec<u8>) -> Result<(Image, (u32, u32), Sent), String> {
    let not_sent = |e: String| format!("{e} — not sent");
    let (w, h) = size(kind, &bytes).map_err(not_sent)?;
    let decoded = decode(kind, &bytes).map_err(not_sent)?;
    // (as it is only once it decoded whole here: a cut or broken one the API can't read would stay in the
    // conversation's history, every request after it refused)
    if w <= EDGE_MAX && h <= EDGE_MAX && bytes.len() <= RAW_MAX {
        let mime = kind.format().ok_or("not a picture")?.to_mime_type();
        return Ok((Image { mime, bytes, w, h }, (w, h), Sent::AsIs));
    }
    drop(bytes);
    let scaled = fit_into(decoded, EDGE_MAX, EDGE_MAX);
    let (fw, fh) = scaled.dimensions();
    let mut ladder = vec![match kind {
        Kind::Jpeg => Sent::Jpeg(80),
        _ => Sent::Png { best: false },
    }];
    if kind == Kind::Png {
        ladder.push(Sent::Png { best: true });
    }
    ladder.extend([80, 60, 40, 20].map(Sent::Jpeg));
    ladder.dedup();
    for step in ladder {
        let made = encode(&scaled, step)?;
        if made.len() <= RAW_MAX {
            return Ok((picture(made, step, fw, fh), (w, h), step));
        }
    }
    // (the last: at most 1000 wide, quality 20 — one that narrow already was just tried at 20)
    if fw > LAST_WIDTH {
        let last = fit_into(Decoded(DynamicImage::ImageRgba8(scaled)), LAST_WIDTH, u32::MAX);
        let (tw, th) = last.dimensions();
        let made = encode(&last, Sent::Jpeg(20))?;
        if made.len() <= RAW_MAX {
            return Ok((picture(made, Sent::Jpeg(20), tw, th), (w, h), Sent::Jpeg(20)));
        }
    }
    Err(over(fw.min(LAST_WIDTH)))
}

/// Why a picture that went down the whole ladder isn't sent (`width`: its last one's).
fn over(width: u32) -> String {
    let mib = RAW_MAX as f64 / (1024.0 * 1024.0);
    format!("still over {mib:.2} MB at {width} wide, JPEG quality 20 — not sent")
}

fn picture(bytes: Vec<u8>, step: Sent, w: u32, h: u32) -> Image {
    let mime = match step {
        Sent::Jpeg(_) => "image/jpeg",
        _ => "image/png",
    };
    Image { mime, bytes, w, h }
}

/// `img` written as `step` says: a PNG without its alpha when every pixel is opaque (a quarter smaller), a JPEG
/// always without (it has none).
fn encode(img: &Pixels, step: Sent) -> Result<Vec<u8>, String> {
    use image::buffer::ConvertBuffer as _;
    use image::codecs::{jpeg::JpegEncoder, png};
    let rgb = || DynamicImage::ImageRgb8(img.convert());
    let mut out = Vec::new();
    let r = match step {
        Sent::Png { best } => {
            let level = if best { png::CompressionType::Best } else { png::CompressionType::Default };
            let enc = png::PngEncoder::new_with_quality(&mut out, level, png::FilterType::Adaptive);
            match img.pixels().all(|p| p.0[3] == 255) {
                true => rgb().write_with_encoder(enc),
                false => img.write_with_encoder(enc),
            }
        }
        Sent::Jpeg(q) => rgb().write_with_encoder(JpegEncoder::new_with_quality(&mut out, q)),
        Sent::AsIs => return Err("nothing to write".into()),
    };
    r.map_err(|e| format!("it couldn't be written: {e}"))?;
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests;
