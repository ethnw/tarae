//! A page read (web-plan D3): bytes, the address they came from and their content type → the page's text as
//! markdown, its links and anchors, its title, how it read. A pure function: no network, no clock, nothing kept.
//!
//! Which part is the page's text (`pick.rs`): its `<main>` when that holds it — right on every fixture that has one;
//! else a lone `<article>`, its `role=main`, the block its text is densest in (readability.js's scoring); else the
//! whole `<body>`. `Prefer` overrides the guess (`:web main|article|page`).
//! A page whose text comes to almost nothing while it carries scripts says it needs JavaScript (D12): its title,
//! its `<noscript>` text, what tarae doesn't do, its links.
//!
//! Limits, each said when hit (`Page.said`): the first `PAGE_MAX` bytes read, elements nested past `DEPTH_MAX`
//! left out, the text cut at a line past `TEXT_MAX`.

use super::convert::{self, Anchor, Link, Out};
pub use super::convert::{DEPTH_MAX, TEXT_MAX};
use super::pick::{LITTLE, Pick};
use super::tree::{self, Tree};
use super::{charset, url};

/// A page's bytes read at most (D6: the fetch's body limit too) — past it the rest is left out, said.
pub const PAGE_MAX: usize = 8 << 20;
/// The `<noscript>`s looked at at most: each is a walk of what it holds, and a page can nest thousands (no page
/// has more than a few).
const LOOKED_MAX: usize = 16;

/// Which part of the page to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefer {
    /// `<main>`, else the article, else the page
    #[default]
    Guess,
    Main,
    Article,
    Page,
}

impl Prefer {
    /// `main`, `article`, `page` — as `:web` takes them.
    pub fn named(w: &str) -> Option<Prefer> {
        Some(match w {
            "main" => Prefer::Main,
            "article" => Prefer::Article,
            "page" => Prefer::Page,
            _ => return None,
        })
    }
}

/// How it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// Its `<main>` (or `role=main`)
    Main,
    /// The article: a lone `<article>`, else the block its text is densest in
    Article,
    /// The whole `<body>`
    Page,
    /// Not HTML: the text as it came (plain text, markdown, JSON)
    Text,
    /// Not text at all (a PDF, an image): nothing read
    None,
}

impl Read {
    pub fn word(self) -> &'static str {
        match self {
            Read::Main => "main",
            Read::Article => "article",
            Read::Page => "page",
            Read::Text => "text",
            Read::None => "not read",
        }
    }
}

/// A `<meta http-equiv=refresh>`: after `secs`, the page goes to `url` (D5: within 5 s, a redirect).
#[derive(Debug, Clone, PartialEq)]
pub struct Refresh {
    pub secs: u64,
    pub url: String,
}

#[derive(Debug)]
pub struct Page {
    pub title: String,
    /// Markdown for HTML; the text as it came otherwise
    pub text: String,
    /// The language `text` is in: "markdown" for a page read, "json", … (none: plain)
    pub language: Option<String>,
    pub links: Vec<Link>,
    pub anchors: Vec<Anchor>,
    pub read: Read,
    /// What the reader has to say about it, one line: needs JavaScript, a limit hit, not read
    pub said: Option<String>,
    pub refresh: Option<Refresh>,
    /// The encoding the bytes were read in ("UTF-8", "EUC-KR", …)
    pub charset: &'static str,
}

/// The page in `bytes`, which came from `url` (the final address, after redirects) as `content_type` (the
/// header's, if any).
pub fn read(bytes: &[u8], url: &str, content_type: Option<&str>, prefer: Prefer) -> Page {
    let mut said = Said::default();
    let bytes = match bytes.get(..PAGE_MAX) {
        Some(first) if bytes.len() > PAGE_MAX => {
            said.add(format!("only its first {} read", size(PAGE_MAX)));
            first
        }
        _ => bytes,
    };
    let mime = content_type
        .map(|c| c.split(';').next().unwrap_or_default().trim().to_ascii_lowercase())
        .unwrap_or_default();
    let html = mime.is_empty() && looks_like_html(bytes)
        || matches!(mime.as_str(), "text/html" | "application/xhtml+xml");
    if !html {
        return as_text(bytes, url, &mime, content_type, said);
    }
    let (text, enc) = charset::decode(bytes, content_type);
    let parsed = tree::parse(&text, DEPTH_MAX);
    let t = &parsed.tree;
    let base = t
        .named("base")
        .find(|&b| t.has(b, "href"))
        .and_then(|b| url::resolve(url, t.attr(b, "href").unwrap_or_default()))
        .unwrap_or_else(|| url.to_string());
    let refresh = refresh(t, &base);
    let mut title = title(t);

    let pick = Pick::new(t);
    let main = || pick.main().or_else(|| pick.role_main()).map(|m| (m, Read::Main));
    let article = || pick.article().or_else(|| pick.block(&title)).map(|a| (a, Read::Article));
    let (root, read) = match prefer {
        Prefer::Guess => pick
            .main()
            .map(|m| (m, Read::Main))
            .or_else(|| pick.article().map(|a| (a, Read::Article)))
            .or_else(|| pick.role_main().map(|m| (m, Read::Main)))
            .or_else(|| pick.block(&title).map(|b| (b, Read::Article))),
        Prefer::Main => main(),
        Prefer::Article => article(),
        Prefer::Page => None,
    }
    .unwrap_or_else(|| (t.body().unwrap_or(Tree::ROOT), Read::Page));
    let mut out = convert::markdown(t, root, &base);

    let scripts = t.named("script").next().is_some();
    if read == Read::Page && scripts && substance(&out.text) < LITTLE {
        out = needs_scripts(t, &title, out);
        said.add("needs JavaScript: tarae doesn't run a page's scripts".into());
    }
    if parsed.stopped {
        said.add(format!("nested past {DEPTH_MAX} levels: read up to there, what's deeper left out"));
    } else if out.deep {
        said.add(format!("nested past {DEPTH_MAX} levels: what's deeper left out"));
    }
    if out.text.len() > TEXT_MAX {
        said.add(cut(&mut out));
    }
    if title.is_empty() {
        title = out.text.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or_default().to_string();
    }
    // (one line)
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    Page {
        title,
        text: out.text,
        language: Some("markdown".into()),
        links: out.links,
        anchors: out.anchors,
        read,
        said: said.0,
        refresh,
        charset: enc.name(),
    }
}

/// What the reader says of a page: its lines joined.
#[derive(Default)]
struct Said(Option<String>);

impl Said {
    fn add(&mut self, s: String) {
        self.0 = Some(match self.0.take() {
            Some(was) => format!("{was} · {s}"),
            None => s,
        });
    }
}

/// The first `<title>`'s text, on one line.
fn title(t: &Tree) -> String {
    let title = t.named("title").next().map(|n| t.text(n)).unwrap_or_default();
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `<meta http-equiv="refresh" content="0; url=…">` — one with an address (one without reloads the page: not
/// followed).
fn refresh(t: &Tree, base: &str) -> Option<Refresh> {
    t.named("meta").find_map(|m| {
        if !t.attr(m, "http-equiv")?.trim().eq_ignore_ascii_case("refresh") {
            return None;
        }
        let (secs, to) = refresh_content(t.attr(m, "content")?)?;
        Some(Refresh { secs, url: url::resolve(base, to?)? })
    })
}

/// A refresh's `content` as the HTML spec reads it ("shared declarative refresh steps"): its seconds, and the
/// address after them if there's one — `0;url=x`, `0; URL = 'x'`, `0, x`, `5 x` (spaces around `=` allowed, the
/// address's quotes taken off up to the closing one).
fn refresh_content(content: &str) -> Option<(u64, Option<&str>)> {
    let ws = |c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ');
    let s = content.trim_start_matches(ws);
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (time, s) = s.split_at(digits);
    if time.is_empty() && !s.starts_with('.') {
        return None;
    }
    // (a number past u64 is as good as forever)
    let secs = time.parse::<u64>().unwrap_or(if time.is_empty() { 0 } else { u64::MAX });
    let s = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    if s.is_empty() {
        return Some((secs, None));
    }
    if !s.starts_with([';', ',']) && !s.starts_with(ws) {
        return None;
    }
    let s = s.trim_start_matches(ws);
    let s = s.strip_prefix([';', ',']).unwrap_or(s).trim_start_matches(ws);
    if s.is_empty() {
        return Some((secs, None));
    }
    // `url`, any case, then `=` — else the whole of it is the address
    let after_url = s
        .get(..3)
        .filter(|k| k.eq_ignore_ascii_case("url"))
        .and_then(|_| s.get(3..))
        .map(|r| r.trim_start_matches(ws))
        .and_then(|r| r.strip_prefix('='))
        .map(|r| r.trim_start_matches(ws));
    let to = after_url.unwrap_or(s);
    let to = match to.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &to[q.len_utf8()..];
            inner.split(q).next().unwrap_or(inner)
        }
        _ => to,
    };
    Some((secs, Some(to)))
}

/// A page that came to almost nothing and runs scripts (D12): said so in its first lines, its links after.
fn needs_scripts(t: &Tree, title: &str, out: Out) -> Out {
    let mut head = String::new();
    if !title.is_empty() {
        head.push_str(&format!("# {title}\n\n"));
    }
    for n in t.named("noscript").take(LOOKED_MAX) {
        let said = t.text(n).split_whitespace().collect::<Vec<_>>().join(" ");
        // (what's past the text's limit is cut anyway)
        if !said.is_empty() && head.len() <= TEXT_MAX {
            head.push_str(&said);
            head.push_str("\n\n");
        }
    }
    head.push_str("This page needs JavaScript to show its text, and tarae doesn't run a page's scripts.\n");
    if !out.text.is_empty() {
        head.push('\n');
    }
    let shift = head.len();
    let lines = head.matches('\n').count();
    Out {
        text: head + &out.text,
        links: out
            .links
            .into_iter()
            .map(|l| Link { from: l.from + shift, to: l.to + shift, url: l.url })
            .collect(),
        anchors: out.anchors.into_iter().map(|a| Anchor { name: a.name, line: a.line + lines }).collect(),
        deep: out.deep,
    }
}

/// Cut at the last line that fits; what's past it goes. The line said.
fn cut(out: &mut Out) -> String {
    let mut end = TEXT_MAX.min(out.text.len());
    while !out.text.is_char_boundary(end) {
        end -= 1;
    }
    let end = out.text[..end].rfind('\n').map_or(end, |i| i + 1);
    out.text.truncate(end);
    let lines = out.text.matches('\n').count();
    out.links.retain(|l| l.to <= end);
    out.anchors.retain(|a| a.line < lines);
    format!("its text cut at {}", size(end))
}

fn size(n: usize) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n => format!("{} KB", n.div_ceil(1024)),
    }
}

/// Characters that aren't spaces.
fn substance(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_ascii_lowercase();
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.contains("<head")
        || head.contains("<body")
}

/// Not HTML: text as it came, in its language (D6); anything else isn't read — what it is said, by its type or,
/// with none given, by its bytes (`media::kind`: a PDF, a picture; else a NUL in its first 8 KB).
fn as_text(bytes: &[u8], url: &str, mime: &str, content_type: Option<&str>, mut said: Said) -> Page {
    let language = match mime {
        "text/markdown" | "text/x-markdown" => Some("markdown"),
        "application/json" | "text/json" => Some("json"),
        m if m.ends_with("+json") => Some("json"),
        "application/toml" => Some("toml"),
        "text/yaml" | "application/yaml" | "application/x-yaml" => Some("yaml"),
        "text/css" => Some("css"),
        "text/javascript" | "application/javascript" => Some("javascript"),
        "application/xml" | "text/xml" => Some("xml"),
        _ => None,
    };
    let what = match mime {
        "" => match crate::media::kind(bytes) {
            Some(crate::media::Kind::Pdf) => Some("a PDF".to_string()),
            Some(k) => Some(format!("a {} picture", k.said())),
            None => binary(bytes).then(|| "binary data".to_string()),
        },
        m if m.starts_with("text/") || language.is_some() => None,
        m => Some(m.to_string()),
    };
    let name = url.rsplit('/').next().unwrap_or_default().to_string();
    if let Some(what) = what {
        said.add(format!("{what}, not a page tarae reads"));
        return Page {
            title: name,
            text: String::new(),
            language: None,
            links: Vec::new(),
            anchors: Vec::new(),
            read: Read::None,
            said: said.0,
            refresh: None,
            charset: "",
        };
    }
    let (text, enc) = charset::decode(bytes, content_type);
    let mut out = Out { text: text.into_owned(), ..Out::default() };
    if out.text.len() > TEXT_MAX {
        said.add(cut(&mut out));
    }
    Page {
        title: name,
        text: out.text,
        language: language.map(String::from),
        links: Vec::new(),
        anchors: Vec::new(),
        read: Read::Text,
        said: said.0,
        refresh: None,
        charset: enc.name(),
    }
}

/// A NUL in the first 8 KB: not text (git's rule, and tarae's).
fn binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}
