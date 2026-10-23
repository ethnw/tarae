//! What a page's own language server answers from, and its answers (`serve.rs` serves them on the LSP routes): a
//! page's links, anchors and headings (`Facts`, made on a worker from what the reader found) and the four requests it
//! takes — `gd` a link's target, `space k` where it goes, `space s` the headings, `gr` the links to a heading or to
//! the same address (`answer`). Pure — no editor — so `fuzz/parsers`' `facts` target runs it as it is.
//!
//! The server runs in tarae's process, and release aborts on a panic: what a process would keep apart, the reader's
//! rules keep here. Its input is anyone's (a page's bytes, through the reader) and a request's numbers are the
//! client's: no panic on any facts or any request — no index past what was found, no arithmetic a request's numbers
//! can overflow —; bounded work a request (`SYMBOLS_MAX` headings, `PLACES_MAX` places, the rest one look-up or one
//! pass over the page's facts); every position it answers in the text, on a character boundary (`check::facts` holds
//! all of it: its loop in every `cargo test`, and the `facts` fuzz target).

use std::collections::{HashMap, HashSet};
use std::sync::Weak;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Page;
use super::convert::{Anchor, Link};

/// The headings `space s` lists at most (a page of more is a table, not an outline).
pub const SYMBOLS_MAX: usize = 2000;
/// The places `gr` answers at most.
pub const PLACES_MAX: usize = 1000;

/// A heading of the page's markdown: its level, words, line (0-based) and where its section ends (exclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct Heading {
    pub level: usize,
    pub text: String,
    pub line: usize,
    pub end: usize,
}

/// The markdown's ATX headings — none inside a fence — each with its section (to the next one as high or higher).
pub fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut out: Vec<Heading> = Vec::new();
    let mut fence: Option<&str> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        let mark = ["```", "~~~"].into_iter().find(|m| t.starts_with(m));
        match (fence, mark) {
            (Some(f), Some(m)) if f == m => {
                fence = None;
                continue;
            }
            (None, Some(m)) => {
                fence = Some(m);
                continue;
            }
            (Some(_), _) => continue,
            (None, None) => {}
        }
        let level = t.bytes().take_while(|&b| b == b'#').count();
        let words = &t[level..];
        if (1..=6).contains(&level) && (words.starts_with(' ') || words.is_empty()) {
            let text = words.trim().trim_end_matches('#').trim().to_string();
            out.push(Heading { level, text, line: i, end: lines.len() });
        }
    }
    // (each section's end: the next heading as high — one pass, a stack of the open ones)
    let mut open: Vec<usize> = Vec::new();
    for k in 0..out.len() {
        while let Some(&o) = open.last().filter(|&&o| out[o].level >= out[k].level) {
            out[o].end = out[k].line;
            open.pop();
        }
        open.push(k);
    }
    out
}

/// A fragment as its anchor is written: `%XX` taken back to its bytes (UTF-8, else as it was).
pub fn decoded(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while let Some(&c) = b.get(i) {
        let hex = |c: u8| (c as char).to_digit(16);
        match (c, b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// The address without its fragment, and the fragment.
pub fn split_fragment(url: &str) -> (&str, Option<&str>) {
    match url.split_once('#') {
        Some((u, f)) => (u, Some(f).filter(|f| !f.trim().is_empty())),
        None => (url, None),
    }
}

/// What a page's server answers from: its links (byte spans of its text, in order), anchors and headings, where its
/// lines start and its length. Its lines are the buffer's — the rope's, which breaks at a lone `\r`, `\x0B`, `\x0C`,
/// U+0085, U+2028 and U+2029 too (a code block keeps them) — never the reader's own count at `\n`.
#[derive(Debug, Default)]
pub struct Facts {
    pub links: Vec<Link>,
    pub anchors: Vec<Anchor>,
    pub headings: Vec<Heading>,
    pub line_starts: Vec<usize>,
    pub len: usize,
    /// Where the reader's lines start (at `\n`: its anchors', its headings', Claude's `read`'s) — only where the rope
    /// breaks a line elsewhere too; else empty, the two counts the same.
    reader_starts: Vec<usize>,
}

impl Facts {
    /// `page`'s facts on `text`, its text as the buffer's rope.
    pub fn of(page: &Page, text: &ropey::Rope) -> Facts {
        let lines: Vec<&str> = page.text.lines().collect();
        let line_starts: Vec<usize> = (0..text.len_lines()).map(|l| text.line_to_byte(l)).collect();
        let reader_starts: Vec<usize> =
            std::iter::once(0).chain(page.text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        let mut f = Facts {
            links: page.links.clone(),
            anchors: page.anchors.clone(),
            headings: headings(&lines),
            line_starts,
            len: page.text.len(),
            reader_starts: Vec::new(),
        };
        if f.line_starts != reader_starts {
            f.reader_starts = reader_starts;
            let (mut anchors, mut heads) = (std::mem::take(&mut f.anchors), std::mem::take(&mut f.headings));
            for a in &mut anchors {
                a.line = f.buffer_line(a.line);
            }
            for h in &mut heads {
                // (by its `#`: what the heading's line trims at its start may be a break of the rope's)
                let indent = lines.get(h.line).map_or(0, |l| l.len() - l.trim_start().len());
                let at = f.reader_starts.get(h.line).map_or(f.len, |s| s + indent);
                (h.line, h.end) = (f.position(at).0, f.buffer_line(h.end));
            }
            (f.anchors, f.headings) = (anchors, heads);
        }
        f
    }

    /// The reader's line `line` (counted at `\n`) as the buffer's: the line its start is on (past the last: the last).
    pub fn buffer_line(&self, line: usize) -> usize {
        match self.reader_starts.is_empty() {
            true => line,
            false => self.position(self.reader_starts.get(line).copied().unwrap_or(self.len)).0,
        }
    }

    /// The link whose markdown holds byte `at`.
    pub fn link_at(&self, at: usize) -> Option<&Link> {
        let i = self.links.partition_point(|l| l.to <= at);
        self.links.get(i).filter(|l| l.from <= at)
    }

    /// Line `line`'s byte `col` (UTF-8: the server's encoding) as an offset in the text — none past it.
    pub fn offset(&self, line: usize, col: usize) -> Option<usize> {
        self.line_starts.get(line)?.checked_add(col).filter(|&b| b <= self.len)
    }

    /// Byte `at`'s line and its byte in it (past the text: its end).
    pub fn position(&self, at: usize) -> (usize, usize) {
        let at = at.min(self.len);
        let line = self.line_starts.partition_point(|&s| s <= at).saturating_sub(1);
        (line, at.saturating_sub(self.line_starts.get(line).copied().unwrap_or(0)))
    }

    /// Where line `line` starts, as a position (past the last: the text's end).
    fn line(&self, line: usize) -> (usize, usize) {
        self.position(self.line_starts.get(line).copied().unwrap_or(self.len))
    }

    /// The line (from 0) a fragment names: its anchor's, else a heading's of those words (case aside).
    pub fn line_of(&self, fragment: &str) -> Option<usize> {
        let name = decoded(fragment.trim());
        if let Some(a) = self.anchors.iter().find(|a| a.name == name) {
            return Some(a.line);
        }
        let want = name.to_lowercase().replace('-', " ");
        self.headings.iter().find(|h| h.text.to_lowercase() == want).map(|h| h.line)
    }
}

/// What the server answers from: each open page's facts and when its copy was read, by address; the copies the
/// session keeps (a hover says `kept`), by address, when read, kept for `keep`.
#[derive(Default)]
pub struct Known {
    pub(super) pages: HashMap<String, (Weak<Facts>, Option<Instant>)>,
    pub(super) kept: HashMap<String, Instant>,
    pub(super) keep: Duration,
}

/// The server's answer to `method` with `params` (None: `null`), at `now`, an age said by `ago`.
pub fn answer(
    k: &Known,
    method: &str,
    params: &Value,
    now: Instant,
    ago: fn(Duration) -> String,
) -> Option<Value> {
    if method == "initialize" {
        return Some(json!({
            "capabilities": {
                "positionEncoding": "utf-8",
                "definitionProvider": true,
                "hoverProvider": true,
                "documentSymbolProvider": true,
                "referencesProvider": true,
            },
            "serverInfo": { "name": "web" },
        }));
    }
    let uri = params["textDocument"]["uri"].as_str()?;
    let facts = k.pages.get(uri)?.0.upgrade()?;
    let at = || {
        let p = &params["position"];
        let n = |v: &Value| v.as_u64().and_then(|n| usize::try_from(n).ok());
        facts.offset(n(&p["line"])?, n(&p["character"])?)
    };
    match method {
        "textDocument/definition" => definition(&facts, uri, at()?),
        "textDocument/hover" => hover(k, &facts, uri, at()?, now, ago),
        "textDocument/documentSymbol" => Some(symbols(&facts)),
        "textDocument/references" => references(&facts, uri, at()?),
        _ => None,
    }
}

/// Only http(s) is followed: `file://` (a saved page's own links), `mailto:` and the rest never.
pub fn followed(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// `gd` on a link: another page's address (its fragment kept — `page::open` goes there), or a line of this page for
/// an `#anchor` of its own. None: no link there, one that isn't followed, an anchor this page hasn't.
fn definition(facts: &Facts, uri: &str, at: usize) -> Option<Value> {
    let link = facts.link_at(at)?;
    let (to, fragment) = split_fragment(&link.url);
    if to == uri {
        let line = fragment.map_or(Some(0), |f| facts.line_of(f))?;
        return Some(location(uri, facts.line(line)));
    }
    followed(&link.url).then(|| location(&link.url, (0, 0)))
}

/// `space k` on a link: where it goes, whole; `another host` where it leaves this page's; `open` or `kept` with how
/// old its copy is where there's one.
fn hover(
    k: &Known,
    facts: &Facts,
    uri: &str,
    at: usize,
    now: Instant,
    ago: fn(Duration) -> String,
) -> Option<Value> {
    let link = facts.link_at(at)?;
    let mut words = vec![format!("`{}`", link.url)];
    let (to, _) = split_fragment(&link.url);
    let host = super::url::host;
    if !followed(&link.url) {
        words.push("not followed — tarae follows only http(s)".into());
    } else if host(to) != host(uri) {
        words.push("another host".into());
    }
    let age = |t: Instant| ago(now.saturating_duration_since(t));
    if to == uri {
        words.push("this page".into());
    } else if let Some((_, at)) = k.pages.get(to).filter(|(f, _)| f.strong_count() > 0) {
        words.push(at.map_or_else(|| "open".into(), |t| format!("open · read {}", age(t))));
    } else if let Some(&t) = k.kept.get(to).filter(|t| now.saturating_duration_since(**t) < k.keep) {
        words.push(format!("kept · read {}", age(t)));
    }
    Some(json!({ "contents": { "kind": "markdown", "value": words.join("\n\n") } }))
}

/// `space s`: the headings, nested by level (a section as a module: `§`) — the first `SYMBOLS_MAX`.
fn symbols(facts: &Facts) -> Value {
    // (levels are 1–6 and each nesting is a level deeper: at most six calls deep)
    fn under(f: &Facts, hs: &[Heading], level: usize) -> (Vec<Value>, usize) {
        let (mut out, mut i) = (Vec::new(), 0);
        while let Some(h) = hs.get(i).filter(|h| h.level > level) {
            let (children, n) = under(f, hs.get(i + 1..).unwrap_or_default(), h.level);
            let (start, end) = (pos(f.line(h.line)), pos(f.line(h.end)));
            let range = json!({ "start": start, "end": end });
            let name = json!({ "start": start, "end": start });
            out.push(json!({ "name": h.text, "kind": 2, "range": range, "selectionRange": name, "children": children }));
            i += 1 + n;
        }
        (out, i)
    }
    let hs = facts.headings.get(..SYMBOLS_MAX).unwrap_or(&facts.headings);
    Value::Array(under(facts, hs, 0).0)
}

/// `gr`: on a link, every link to the same address; on a heading's line, the links to its anchors — the first
/// `PLACES_MAX`.
fn references(facts: &Facts, uri: &str, at: usize) -> Option<Value> {
    let targets: HashSet<String> = match facts.link_at(at) {
        Some(l) => HashSet::from([l.url.clone()]),
        None => {
            let line = facts.position(at).0;
            facts.anchors.iter().filter(|a| a.line == line).map(|a| format!("{uri}#{}", a.name)).collect()
        }
    };
    if targets.is_empty() {
        return None;
    }
    let places = facts.links.iter().filter(|l| targets.contains(&l.url)).take(PLACES_MAX);
    Some(Value::Array(places.map(|l| location(uri, facts.position(l.from))).collect()))
}

fn pos((line, character): (usize, usize)) -> Value {
    json!({ "line": line, "character": character })
}

fn location(uri: &str, at: (usize, usize)) -> Value {
    let at = pos(at);
    json!({ "uri": uri, "range": { "start": at, "end": at } })
}
