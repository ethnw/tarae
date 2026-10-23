//! HTML → markdown, tarae's way (web-plan D3.2–3): one pass over the tree, the text as written (never escaped:
//! it's read, not handed to another renderer), each link where it stands and listed with its place, each element's
//! id kept as an anchor on the line its text starts.
//!
//! - Left out by tag: what a page runs or styles, its forms and its chrome (`nav`, `aside`; a `header` or `footer`
//!   of the page, not of an article or section inside it) — or by the ARIA role that says the same —, what it hides
//!   (`hidden`, `aria-hidden`, `display:none`), GitHub's interactive custom elements and rustdoc's toggle labels
//!   (`summary.hideme`: "Expand description").
//! - Code blocks are fences naming their language (`lang.rs`); a heading's own anchor link (rustdoc's `§`) goes,
//!   the heading keeps the anchor; an image is its alt text in brackets; a table is a pipe table (a table holding a
//!   table, of one column or `role=presentation` is laid out as blocks — it was layout).
//! - Whitespace collapses as a browser's does; a marker (`**`, `[`, `## `) is written only once text comes, so
//!   an empty element leaves nothing and a marker hugs its words.
//! - Within limits (a page's bytes are anyone's): elements nested past `DEPTH_MAX` are left out (`Out.deep`), and
//!   writing stops once the text passes `TEXT_MAX` (the reader cuts it at a line) — the work and the memory a
//!   page takes are the page's size times a small number, whatever it holds. Every walk up the tree is as short.

use std::collections::HashMap;

use super::tree::{Id, Tree};
use super::{lang, url};

/// The markdown written at most (D6) — past it writing stops; the reader cuts at the last line and says so.
pub const TEXT_MAX: usize = 2 << 20;
/// Elements nested deeper than this are left out, said: the converter's walk is recursive (a worker's stack). As
/// Chrome's HTML parser, which puts what's deeper beside its parent instead; real pages stay within ~60.
pub const DEPTH_MAX: usize = 512;
/// A table's columns at most: cells past it are left out (no page's data table comes near).
const COLS_MAX: usize = 64;

/// What a tree read as markdown.
#[derive(Debug, Default)]
pub struct Out {
    pub text: String,
    pub links: Vec<Link>,
    pub anchors: Vec<Anchor>,
    /// Something was nested past `DEPTH_MAX` and left out
    pub deep: bool,
}

/// A link in the text: `text[from..to]` is its markdown (`[words](url)`), `url` where it goes, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    pub url: String,
}

/// An element's id (or an `<a name>`) and the line, from 0, its text starts on: `#method.remove` goes there.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub name: String,
    pub line: usize,
}

/// `root`'s content as markdown; links resolved against `base` (the page's `<base>`, else its own address).
pub fn markdown(t: &Tree, root: Id, base: &str) -> Out {
    let mut w = Writer::new(t, base);
    w.children(root);
    w.finish()
}

/// Never read: what a page runs, styles, asks or draws, and the page's chrome around its text.
const SKIP: &[&str] = &[
    "script",
    "style",
    "svg",
    "math",
    "template",
    "noscript",
    "nav",
    "aside",
    "form",
    "button",
    "input",
    "select",
    "textarea",
    "option",
    "label",
    "iframe",
    "frame",
    "object",
    "embed",
    "canvas",
    "video",
    "audio",
    "picture",
    "source",
    "track",
    "map",
    "dialog",
    "link",
    "meta",
    "head",
    "title",
    "base",
    "wbr",
    // GitHub's interactive custom elements: "Copy link", tooltips, menus
    "clipboard-copy",
    "tool-tip",
    "details-menu",
    "action-menu",
    "anchored-position",
    "focus-group",
];

/// Where a `header` or `footer` is an article's or a section's, not the page's.
pub const SECTIONING: &[&str] = &["article", "aside", "main", "nav", "section"];

/// The same chrome said by ARIA's roles instead of its tags (Wikipedia's navboxes, GitHub's menus).
const SKIP_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
    "menu",
    "menubar",
    "toolbar",
    "dialog",
    "tooltip",
];

/// Blocks set apart by a blank line; and those that only start a line.
const PARAGRAPH: &[&str] = &[
    "p", "section", "article", "main", "figure", "details", "address", "fieldset", "center", "hgroup",
    "search", "dl", "header", "footer",
];
const LINE: &[&str] =
    &["div", "summary", "figcaption", "caption", "legend", "dt", "tr", "li", "body", "html"];

struct Prefix {
    first: String,
    rest: String,
    used: bool,
    /// A list item's: what's in it starts on its marker's line, never a blank line after it
    tight: bool,
}

impl Prefix {
    fn new(first: impl Into<String>, rest: impl Into<String>, tight: bool) -> Prefix {
        Prefix { first: first.into(), rest: rest.into(), used: false, tight }
    }
}

/// A marker waiting for its first text (`## `, `**`, `[`), and whether it was written.
struct Opener {
    text: String,
    written: bool,
    /// Where it went in `out`
    at: usize,
}

struct Writer<'a> {
    t: &'a Tree,
    out: String,
    base: &'a str,
    links: Vec<Link>,
    /// (id, byte offset) — lines once the text is done
    anchors: Vec<(String, usize)>,
    waiting: Vec<String>,
    /// A section's anchors, held for its heading (what floats beside a heading is written before it)
    held: HashMap<Id, Vec<String>>,
    prefixes: Vec<Prefix>,
    openers: Vec<Opener>,
    /// Newlines owed before the next text: 1 a line break, 2 a blank line
    owed: u8,
    space: bool,
    /// Inside a heading, a link, emphasis or a table cell: blocks there are spaces
    inline: u32,
    /// In a heading: the ids it and what holds it go by — a link to one of them is the heading's own anchor
    heading: Option<Vec<String>>,
    /// In a table cell: `|` escaped
    cell: bool,
    /// Elements open around the one being read
    depth: usize,
    deep: bool,
}

impl<'a> Writer<'a> {
    fn new(t: &'a Tree, base: &'a str) -> Writer<'a> {
        Writer {
            t,
            out: String::new(),
            base,
            links: Vec::new(),
            anchors: Vec::new(),
            waiting: Vec::new(),
            held: HashMap::new(),
            prefixes: Vec::new(),
            openers: Vec::new(),
            owed: 0,
            space: false,
            inline: 0,
            heading: None,
            cell: false,
            depth: 0,
            deep: false,
        }
    }

    fn finish(mut self) -> Out {
        let end = self.out.trim_end().len();
        self.out.truncate(end);
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        for name in std::mem::take(&mut self.waiting) {
            self.anchors.push((name, end));
        }
        // Byte offsets → lines, in one pass over the text: anchors are taken as the text grows, so in order
        let (mut line, mut at) = (0, 0);
        let mut seen = std::collections::HashSet::new();
        let mut anchors = Vec::new();
        for (name, offset) in self.anchors {
            let to = offset.clamp(at, self.out.len());
            line += self.out.as_bytes()[at..to].iter().filter(|&&b| b == b'\n').count();
            at = to;
            if seen.insert(name.clone()) {
                anchors.push(Anchor { name, line });
            }
        }
        Out { text: self.out, links: self.links, anchors, deep: self.deep }
    }

    /// Past `TEXT_MAX`: nothing more is written.
    fn full(&self) -> bool {
        self.out.len() > TEXT_MAX
    }

    // ── Writing ──

    fn owe(&mut self, n: u8) {
        if self.inline > 0 {
            self.space = true;
            return;
        }
        // A list item's first paragraph starts on its marker's line: it owes no more than was owed before it
        let n = match self.prefixes.last() {
            Some(p) if p.tight && !p.used => n.min(self.owed.max(1)),
            _ => n,
        };
        self.owed = self.owed.max(n);
    }

    fn at_line_start(&self) -> bool {
        self.out.is_empty() || self.out.ends_with('\n')
    }

    /// The owed newlines, then the line's prefixes if it's a new one.
    fn start(&mut self) {
        if self.owed > 0 && !self.out.is_empty() {
            if !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            if self.owed == 2 && !self.out.ends_with("\n\n") && self.out.len() > 1 {
                // (a quote or an item not begun yet starts after the blank line)
                let blank: String =
                    self.prefixes.iter().filter(|p| p.used).map(|p| p.rest.as_str()).collect();
                self.out.push_str(blank.trim_end());
                self.out.push('\n');
            }
            self.space = false;
        }
        self.owed = 0;
        if self.at_line_start() {
            self.space = false;
            for p in &mut self.prefixes {
                self.out.push_str(if p.used { &p.rest } else { &p.first });
                p.used = true;
            }
        }
    }

    /// Text as it is (a word, a code span): what's owed first, then the markers waiting for it.
    fn put(&mut self, s: &str) {
        if s.is_empty() || self.full() {
            return;
        }
        self.start();
        if self.space && !self.at_line_start() && !self.out.ends_with(' ') {
            self.out.push(' ');
        }
        self.space = false;
        // (the markers not written yet are the last ones opened: only those are looked at)
        let from = self.openers.iter().rposition(|o| o.written).map_or(0, |i| i + 1);
        for o in &mut self.openers[from..] {
            o.at = self.out.len();
            self.out.push_str(&o.text);
            o.written = true;
        }
        for name in self.waiting.drain(..) {
            self.anchors.push((name, self.out.len()));
        }
        if self.cell && s.contains('|') {
            self.out.push_str(&s.replace('|', "\\|"));
        } else {
            self.out.push_str(s);
        }
    }

    /// A text node: whitespace collapsed, as a browser lays it out.
    fn text(&mut self, s: &str) {
        // (a no-break space too: a reader keeps words apart, not the page's layout)
        for (i, word) in s.split(|c: char| c.is_whitespace() || c == '\u{200b}').enumerate() {
            if i > 0 {
                self.space = true;
            }
            self.put(word);
        }
    }

    fn open(&mut self, text: impl Into<String>) {
        self.openers.push(Opener { text: text.into(), written: false, at: 0 });
    }

    /// Closes the last marker: its closing text if it was written. Where it was written, or none.
    fn close(&mut self, text: &str) -> Option<usize> {
        let o = self.openers.pop()?;
        if !o.written {
            return None;
        }
        // A marker with nothing after it (`** **`) is taken back
        if self.out.len() == o.at + o.text.len() {
            self.out.truncate(o.at);
            return None;
        }
        self.out.push_str(text);
        Some(o.at)
    }

    // ── The tree ──

    fn children(&mut self, node: Id) {
        if self.depth >= DEPTH_MAX {
            self.deep = true;
            return;
        }
        self.depth += 1;
        let t = self.t;
        for c in t.children(node) {
            if self.full() {
                break;
            }
            self.node(c);
        }
        self.depth -= 1;
    }

    fn node(&mut self, node: Id) {
        let t = self.t;
        if let Some(s) = t.own_text(node) {
            self.text(s);
            return;
        }
        let Some(name) = t.name(node) else {
            if t.holds_only(node) {
                self.children(node);
            }
            return;
        };
        if left_out(t, node, name) {
            return;
        }
        if matches!(name, "header" | "footer") && !inside(t, node, SECTIONING) {
            return;
        }
        if let Some(id) = t.attr(node, "id").or_else(|| if name == "a" { t.attr(node, "name") } else { None })
        {
            // A section opening with its heading: the anchor goes on the heading's line, not on what floats beside
            // it and comes first (rustdoc's `Source` link)
            let heading = t
                .elements(node)
                .take(3)
                .find(|&c| matches!(t.name(c), Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6")));
            match heading {
                _ if id.is_empty() => {}
                Some(h) => self.held.entry(h).or_default().push(id.to_string()),
                None => self.waiting.push(id.to_string()),
            }
        }
        match name {
            "br" => match self.inline > 0 {
                true => self.space = true,
                false => {
                    self.start();
                    self.out.push('\n');
                    self.space = false;
                }
            },
            "hr" if self.inline == 0 => {
                self.owe(2);
                self.put("---");
                self.owe(2);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.heading(node, name),
            "pre" => self.pre(node),
            "code" | "kbd" | "samp" | "tt" => self.code(node),
            "strong" | "b" => self.emphasis(node, "**"),
            "em" | "i" => self.emphasis(node, "*"),
            "del" | "s" | "strike" => self.emphasis(node, "~~"),
            "a" => self.link(node),
            "img" => {
                let alt = t.attr(node, "alt").unwrap_or_default();
                let alt = alt.split_whitespace().collect::<Vec<_>>().join(" ");
                if !alt.is_empty() {
                    self.put(&format!("[{alt}]"));
                }
            }
            "ul" | "ol" | "menu" => self.list(node, name == "ol"),
            "blockquote" => self.quote(node),
            "table" => self.table(node),
            "dd" => {
                self.owe(1);
                self.prefixes.push(Prefix::new("  ", "  ", true));
                self.children(node);
                self.prefixes.pop();
                self.owe(1);
            }
            _ if PARAGRAPH.contains(&name) => {
                self.owe(2);
                self.children(node);
                self.owe(2);
            }
            _ if LINE.contains(&name) => {
                self.owe(1);
                self.children(node);
                self.owe(1);
            }
            _ => self.children(node),
        }
    }

    fn heading(&mut self, node: Id, name: &str) {
        if self.inline > 0 {
            return self.emphasis(node, "**");
        }
        let level = name.get(1..).and_then(|l| l.parse::<usize>().ok()).unwrap_or(1);
        if let Some(ids) = self.held.remove(&node) {
            self.waiting.extend(ids);
        }
        self.owe(2);
        self.open(format!("{} ", "#".repeat(level)));
        self.inline += 1;
        let t = self.t;
        let ids = std::iter::once(node).chain(up(t, node)).filter_map(|n| t.attr(n, "id"));
        self.heading = Some(ids.map(|id| id.to_string()).collect());
        self.children(node);
        self.heading = None;
        self.inline -= 1;
        self.close("");
        self.owe(2);
    }

    fn emphasis(&mut self, node: Id, mark: &str) {
        // Bold in bold is bold: `**a **b** c**` would close it early
        if self.openers.iter().any(|o| o.text == mark) {
            return self.children(node);
        }
        self.open(mark);
        self.inline += 1;
        self.children(node);
        self.inline -= 1;
        self.close(mark);
    }

    fn code(&mut self, node: Id) {
        let text = self.t.text(node);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        let ticks = "`".repeat(longest_run(&text, '`') + 1);
        let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
        self.put(&format!("{ticks}{pad}{text}{pad}{ticks}"));
    }

    fn link(&mut self, node: Id) {
        let t = self.t;
        let href = t.attr(node, "href").unwrap_or_default();
        let target = url::resolve(self.base, href);
        // An element's own anchor link — to its id or its section's (rustdoc's `§` beside an impl's heading, a
        // heading's `#` or pilcrow, a method's name in its own signature): the anchor stays; a symbol goes, and in
        // a heading words stay as words
        let symbol = t.text(node).trim().chars().count() <= 1;
        let own = href.trim().strip_prefix('#').is_some_and(|id| {
            !id.is_empty()
                && (self.heading.as_ref().is_some_and(|ids| ids.iter().any(|h| h == id))
                    || symbol && up(t, node).any(|a| t.attr(a, "id") == Some(id)))
        });
        let own = own || self.heading.is_some() && symbol && href.trim().starts_with('#');
        let Some(target) = target.filter(|_| !own) else {
            if !(own && symbol) {
                self.children(node);
            }
            return;
        };
        // Within the page: the fragment alone, as written (the same address, said shorter)
        let shown = match href.trim().starts_with('#') && !self.base.contains('#') {
            true => href.trim().to_string(),
            false => url::in_markdown(&target),
        };
        self.open("[");
        self.inline += 1;
        self.children(node);
        self.inline -= 1;
        if let Some(from) = self.close(&format!("]({shown})")) {
            self.links.push(Link { from, to: self.out.len(), url: target });
        }
    }

    fn pre(&mut self, node: Id) {
        if self.inline > 0 {
            return self.code(node);
        }
        let lang = lang::of(self.t, node).unwrap_or_default();
        let code = self.t.text(node);
        let code = code.strip_prefix('\n').unwrap_or(&code).trim_end_matches(['\n', ' ', '\t']);
        if code.trim().is_empty() {
            return;
        }
        let fence = "`".repeat(longest_run(code, '`').max(2) + 1);
        self.owe(2);
        self.put(&format!("{fence}{lang}"));
        let p: String = self.prefixes.iter().map(|p| p.rest.as_str()).collect();
        for line in code.split('\n') {
            if self.full() {
                break;
            }
            self.out.push('\n');
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                self.out.push_str(p.trim_end());
            } else {
                self.out.push_str(&p);
                self.out.push_str(line);
            }
        }
        self.out.push('\n');
        self.owed = 0;
        self.put(&fence);
        self.owe(2);
    }

    fn list(&mut self, node: Id, ordered: bool) {
        if self.inline > 0 {
            return self.children(node);
        }
        self.owe(if self.prefixes.is_empty() { 2 } else { 1 });
        let t = self.t;
        let mut n: i64 = t.attr(node, "start").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
        for c in t.children(node) {
            if self.full() {
                break;
            }
            if t.name(c) != Some("li") {
                self.node(c);
                continue;
            }
            if hidden(t, c) {
                continue;
            }
            let first = if ordered { format!("{n}. ") } else { "- ".into() };
            n = n.saturating_add(1);
            let rest = " ".repeat(first.len());
            if let Some(id) = t.attr(c, "id") {
                self.waiting.push(id.to_string());
            }
            self.owe(1);
            self.prefixes.push(Prefix::new(first, rest, true));
            self.children(c);
            self.prefixes.pop();
            self.owe(1);
        }
        self.owe(if self.prefixes.is_empty() { 2 } else { 1 });
    }

    fn quote(&mut self, node: Id) {
        if self.inline > 0 {
            return self.children(node);
        }
        self.owe(2);
        self.prefixes.push(Prefix::new("> ", "> ", false));
        self.children(node);
        self.prefixes.pop();
        self.owe(2);
    }

    fn table(&mut self, node: Id) {
        let t = self.t;
        let rows = rows(t, node);
        let width = rows.iter().map(|r| r.iter().map(|(_, span)| span).sum::<usize>()).max().unwrap_or(0);
        let width = width.min(COLS_MAX);
        let nested = t.walk(node).any(|d| t.name(d) == Some("table"));
        let layout = t.attr(node, "role").is_some_and(|r| matches!(r.trim(), "presentation" | "none"));
        if self.inline > 0 || nested || layout || width < 2 {
            self.owe(2);
            self.children(node);
            self.owe(2);
            return;
        }
        if let Some(cap) = t.elements(node).find(|&c| t.name(c) == Some("caption")) {
            self.owe(2);
            self.children(cap);
        }
        self.owe(2);
        for (i, row) in rows.iter().enumerate() {
            if self.full() {
                break;
            }
            self.start();
            let line_at = self.out.len();
            let mut line = String::from("|");
            let mut links = Vec::new();
            let mut cols = 0;
            for (cell, span) in row {
                if cols >= width {
                    break;
                }
                let mut w = Writer::new(t, self.base);
                (w.inline, w.cell, w.depth) = (1, true, self.depth + 2);
                w.children(*cell);
                self.deep |= w.deep;
                let out = w.finish();
                let at = line_at + line.len() + 1;
                links.extend(out.links.into_iter().map(|l| Link {
                    from: l.from + at,
                    to: l.to + at,
                    url: l.url,
                }));
                self.anchors.extend(out.anchors.into_iter().map(|a| (a.name, line_at)));
                line.push(' ');
                line.push_str(out.text.trim_end());
                line.push_str(" |");
                let span = (*span).min(width - cols);
                for _ in 1..span {
                    line.push_str("  |");
                }
                cols += span;
            }
            for _ in cols..width {
                line.push_str("  |");
            }
            for name in self.waiting.drain(..) {
                self.anchors.push((name, line_at));
            }
            self.out.push_str(&line);
            self.links.extend(links);
            if i == 0 {
                self.out.push('\n');
                self.start();
                self.out.push('|');
                self.out.push_str(&" --- |".repeat(width));
            }
            self.owe(1);
        }
        self.owe(2);
    }
}

/// What holds `node`, as far up as the walk goes down (`DEPTH_MAX`): never a walk to the root of a page nested
/// deeper than the reader reads.
/// An element never read, whatever holds it: by its tag, its role, or the page hiding it (a `header` or `footer` is
/// the page's chrome only outside `SECTIONING` — asked apart).
pub fn left_out(t: &Tree, node: Id, name: &str) -> bool {
    SKIP.contains(&name)
        || hidden(t, node)
        || t.attr(node, "role").is_some_and(|r| SKIP_ROLES.contains(&r.trim().to_ascii_lowercase().as_str()))
        || control_label(t, node, name)
}

/// A toggle's label its page's CSS hides, the only sign in the HTML its class: rustdoc's `<summary class="hideme">`
/// ("Expand description", "Show 3 methods") — a browser draws a `<summary>` always, so other pages' (MDN's, a
/// README's) are real words and kept.
fn control_label(t: &Tree, node: Id, name: &str) -> bool {
    name == "summary" && t.attr(node, "class").is_some_and(|c| c.split_whitespace().any(|c| c == "hideme"))
}

fn up(t: &Tree, node: Id) -> impl Iterator<Item = Id> + '_ {
    t.up(node, DEPTH_MAX)
}

/// A table's rows, its own (not a nested table's): each cell with its `colspan`.
fn rows(t: &Tree, table: Id) -> Vec<Vec<(Id, usize)>> {
    let is = |n: Id, names: &[&str]| t.name(n).is_some_and(|x| names.contains(&x));
    let mut trs = Vec::new();
    for c in t.elements(table) {
        if is(c, &["tr"]) {
            trs.push(c);
        } else if is(c, &["thead", "tbody", "tfoot"]) {
            trs.extend(t.elements(c).filter(|&r| is(r, &["tr"])));
        }
    }
    trs.iter()
        .filter(|&&tr| !hidden(t, tr))
        .map(|&tr| {
            t.elements(tr)
                .filter(|&c| is(c, &["td", "th"]))
                .map(|c| {
                    let span = t.attr(c, "colspan").and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                    (c, span.clamp(1, COLS_MAX))
                })
                .collect::<Vec<_>>()
        })
        .filter(|r: &Vec<_>| !r.is_empty())
        .collect()
}

/// What the page hides from everyone: `hidden`, `aria-hidden="true"`, an inline `display: none`.
fn hidden(t: &Tree, node: Id) -> bool {
    t.has(node, "hidden")
        || t.attr(node, "aria-hidden").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        || t.attr(node, "style").is_some_and(|s| {
            let s: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
            s.contains("display:none") || s.contains("visibility:hidden")
        })
}

fn inside(t: &Tree, node: Id, names: &[&str]) -> bool {
    up(t, node).any(|a| t.name(a).is_some_and(|n| names.contains(&n)))
}

fn longest_run(s: &str, c: char) -> usize {
    let (mut best, mut run) = (0, 0);
    for x in s.chars() {
        run = if x == c { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}
