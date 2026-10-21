//! Markdown → colored lines (shared by hover and completion docs). The light subset language servers send:
//! paragraphs·headings·lists·quotes·rules·code blocks (tree-sitter colored)·tables·inline `code`·**bold**·
//! *italic*·[links](url)·backslash escapes. Colors come from the theme at build time; wrapping (and a table's
//! layout) happens at draw time.

use ropey::Rope;
use tree_sitter::QueryCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::syntax::{self, Loader, Syntax};
use crate::theme::{Style, Theme};

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// Paragraph — wrapped to width; wrapped lines indented `indent` cells (aligned after a list bullet).
    Text {
        spans: Vec<Span>,
        indent: usize,
    },
    /// One code line — wraps under its own indentation.
    Code(Vec<Span>),
    /// A table — every row has as many cells as the header. Laid out at wrap time (`table`).
    Table {
        aligns: Vec<Align>,
        header: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
    Blank,
}

/// A table column's alignment (`---` · `:---:` · `---:`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

impl Line {
    /// Width before wrapping (for sizing the box).
    pub fn width(&self) -> usize {
        match self {
            Line::Text { spans, .. } | Line::Code(spans) => spans_width(spans),
            Line::Table { header, rows, .. } => {
                column_max(header, rows, spans_width).iter().sum::<usize>()
                    + GAP * header.len().saturating_sub(1)
            }
            Line::Rule | Line::Blank => 0,
        }
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.width()).sum()
}

/// `default_lang` = language for code blocks without one (usually the current document's language).
pub fn render(src: &str, theme: &Theme, default_lang: Option<&str>) -> Vec<Line> {
    render_reading(src, theme, default_lang, &mut Read::default())
}

/// What a `render` went by beyond its text and theme — what laid-out rows kept for later depend on (the chat's).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Read {
    /// A code block without a language took `default_lang`.
    pub default_lang: bool,
    /// Languages a code block was in whose grammar wasn't loaded yet: painted plain.
    pub unloaded: Vec<String>,
}

/// `render`, saying what it went by in `read`.
pub fn render_reading(src: &str, theme: &Theme, default_lang: Option<&str>, read: &mut Read) -> Vec<Line> {
    let heading =
        theme.try_get("markup.heading").unwrap_or_default().patch(Style { bold: true, ..Style::default() });
    let quote = theme.try_get("markup.quote").unwrap_or(Style { italic: true, ..Style::default() });
    let bullet = theme.try_get("markup.list").unwrap_or_default();
    let mut out: Vec<Line> = Vec::new();
    // Paragraph being collected: (text, indent, base style, bullet)
    let mut para: Option<(String, usize, Style, Option<Span>)> = None;
    let flush = |para: &mut Option<(String, usize, Style, Option<Span>)>, out: &mut Vec<Line>| {
        if let Some((text, indent, base, head)) = para.take() {
            let mut spans: Vec<Span> = head.into_iter().collect();
            spans.extend(inline(&text, base, theme));
            out.push(Line::Text { spans, indent });
        }
    };
    let mut lines = src.lines().peekable();
    while let Some(raw) = lines.next() {
        let l = raw.trim_end();
        let t = l.trim_start();
        let lead = l.len() - t.len();
        if let Some(info) = t.strip_prefix("```").or_else(|| t.strip_prefix("~~~")) {
            flush(&mut para, &mut out);
            let fence = &t[..3];
            let mut code = String::new();
            for c in lines.by_ref() {
                if c.trim_start().starts_with(fence) {
                    break;
                }
                code.push_str(c);
                code.push('\n');
            }
            let lang = info.split([',', ' ']).next().unwrap_or_default().trim();
            // Always one blank line before and after a code block (even if the server sends it flush)
            if !matches!(out.last(), None | Some(Line::Blank)) {
                out.push(Line::Blank);
            }
            read.default_lang |= lang.is_empty();
            let lang = if lang.is_empty() { default_lang } else { Some(lang) };
            if let Some(s) = lang.and_then(syntax::resolve)
                && Loader::global().loaded(&s.name).is_none()
                && !read.unloaded.contains(&s.name)
            {
                read.unloaded.push(s.name.clone());
            }
            out.extend(code_lines(&code, lang, theme));
            out.push(Line::Blank);
            continue;
        }
        // A table: a row of cells, then a delimiter row of as many (`|---|:--:|`); it goes on while lines
        // have a pipe. A blank line on each side, as for code blocks
        if t.contains('|')
            && let Some(aligns) = lines.peek().and_then(|n| delimiter(n))
            && aligns.len() == cells(t).len()
        {
            flush(&mut para, &mut out);
            lines.next();
            let row = |l: &str, base: Style| {
                let mut row: Vec<Vec<Span>> = cells(l).iter().map(|c| inline(c, base, theme)).collect();
                row.resize(aligns.len(), Vec::new());
                row
            };
            let header = row(t, Style { bold: true, ..Style::default() });
            let mut rows = Vec::new();
            while let Some(l) = lines.next_if(|l| l.contains('|')) {
                rows.push(row(l, Style::default()));
            }
            if !matches!(out.last(), None | Some(Line::Blank)) {
                out.push(Line::Blank);
            }
            out.push(Line::Table { aligns, header, rows });
            out.push(Line::Blank);
            continue;
        }
        // Reference link definitions (`[Hash]: https://…`) have nothing to show
        if t.starts_with('[') && t.contains("]: ") {
            continue;
        }
        if t.is_empty() {
            flush(&mut para, &mut out);
            if !matches!(out.last(), None | Some(Line::Blank | Line::Rule)) {
                out.push(Line::Blank);
            }
            continue;
        }
        if t.len() >= 3 && ["-", "*", "_"].iter().any(|c| t.chars().all(|x| x.to_string() == *c || x == ' '))
        {
            flush(&mut para, &mut out);
            // A rule is itself a separator, so it swallows blank lines around it
            while matches!(out.last(), Some(Line::Blank)) {
                out.pop();
            }
            if !out.is_empty() {
                out.push(Line::Rule);
            }
            continue;
        }
        let hashes = t.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            flush(&mut para, &mut out);
            out.push(Line::Text { spans: inline(t[hashes..].trim(), heading, theme), indent: 0 });
            continue;
        }
        if let Some(rest) = t.strip_prefix("> ").or_else(|| (t == ">").then_some("")) {
            flush(&mut para, &mut out);
            let head = Span { text: "▎ ".into(), style: quote };
            para = Some((rest.to_string(), 2, quote, Some(head)));
            continue;
        }
        let item = ["- ", "* ", "+ "]
            .iter()
            .find_map(|m| t.strip_prefix(m).map(|r| ("•".to_string(), r)))
            .or_else(|| {
                let digits = t.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0 && t[digits..].starts_with(". "))
                    .then(|| (t[..digits + 1].to_string(), &t[digits + 2..]))
            });
        if let Some((mark, rest)) = item {
            flush(&mut para, &mut out);
            let head = Span { text: format!("{}{mark} ", " ".repeat(lead)), style: bullet };
            let indent = head.text.width();
            para = Some((rest.to_string(), indent, Style::default(), Some(head)));
            continue;
        }
        match &mut para {
            Some((text, ..)) => {
                text.push(' ');
                text.push_str(t);
            }
            None => para = Some((t.to_string(), 0, Style::default(), None)),
        }
        // Backslash or two spaces at line end = hard line break
        if raw.ends_with("  ") || raw.ends_with('\\') {
            if let Some((text, ..)) = &mut para
                && text.ends_with('\\')
            {
                text.pop();
            }
            flush(&mut para, &mut out);
        }
    }
    flush(&mut para, &mut out);
    while matches!(out.last(), Some(Line::Blank | Line::Rule)) {
        out.pop();
    }
    out
}

/// Inline formatting of one line only (doc-comment rendering — keeps line structure, changes display only).
pub fn inline_spans(text: &str, theme: &Theme) -> Vec<Span> {
    inline(text, Style::default(), theme)
}

/// Inline formatting → spans grouped by style.
fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span> {
    let code = theme.try_get("markup.raw.inline").or_else(|| theme.try_get("markup.raw")).unwrap_or_default();
    let link = theme.try_get("markup.link.text").unwrap_or(Style { underline: true, ..Style::default() });
    let (mut strong, mut emph) = (false, false);
    // `_`/`__` pairs, tracked apart from `*` so one never closes the other
    let (mut ustrong, mut uemph) = (false, false);
    let mut spans: Vec<Span> = Vec::new();
    let mut push = |s: &str, style: Style| match spans.last_mut() {
        Some(last) if last.style == style => last.text.push_str(s),
        _ => spans.push(Span { text: s.to_string(), style }),
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let style = base.patch(Style { bold: strong || ustrong, italic: emph || uemph, ..Style::default() });
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                push(&chars[i + 1].to_string(), style);
                i += 2;
            }
            '`' => {
                let ticks = chars[i..].iter().take_while(|&&x| x == '`').count();
                let body = i + ticks;
                let close = (body..chars.len())
                    .find(|&j| chars[j..].iter().take_while(|&&x| x == '`').count() == ticks);
                match close {
                    Some(j) => {
                        let s: String = chars[body..j].iter().collect();
                        push(s.trim(), style.patch(code));
                        i = j + ticks;
                    }
                    None => {
                        push(&"`".repeat(ticks), style);
                        i = body;
                    }
                }
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                strong = !strong;
                i += 2;
            }
            // Only a paired standalone '*' is italic (multiplication `a * b` stays as-is)
            '*' => {
                let opens = !emph
                    && chars.get(i + 1).is_some_and(|n| !n.is_whitespace())
                    && chars[i + 1..].contains(&'*');
                let closes = emph && i > 0 && !chars[i - 1].is_whitespace();
                if opens || closes {
                    emph = !emph;
                } else {
                    push("*", style);
                }
                i += 1;
            }
            // `_emphasis_`·`__strong__` only at word edges — `snake_case`·`x_1_y` stay as-is
            '_' => {
                let n = if chars.get(i + 1) == Some(&'_') { 2 } else { 1 };
                let open = if n == 2 { ustrong } else { uemph };
                let word =
                    |j: Option<usize>| j.and_then(|j| chars.get(j)).is_some_and(|c| c.is_alphanumeric());
                let space = |j: Option<usize>| j.and_then(|j| chars.get(j)).is_none_or(|c| c.is_whitespace());
                // A closer: not after a space, not followed by a word character
                let closer = |at: usize| !space(at.checked_sub(1)) && !word(Some(at + n));
                let opens = !open
                    && !word(i.checked_sub(1))
                    && !space(Some(i + n))
                    && chars.get(i + n) != Some(&'_')
                    && (i + n + 1..chars.len()).any(|j| {
                        chars[j..].iter().take_while(|&&c| c == '_').count() == n
                            && chars[j - 1] != '_'
                            && closer(j)
                    });
                if opens || (open && closer(i)) {
                    if n == 2 {
                        ustrong = !ustrong;
                    } else {
                        uemph = !uemph;
                    }
                } else {
                    push(&"_".repeat(n), style);
                }
                i += n;
            }
            '[' => {
                // [text](url) → text only (styled as a link). rustdoc reference links [`Hash`]·[Eq][ref] too
                // (only for a single word without spaces — text like `[1, 2]` stays as-is)
                let close = chars[i..].iter().position(|&x| x == ']').map(|p| i + p);
                let end = close.and_then(|j| match chars.get(j + 1) {
                    Some('(') => chars[j..].iter().position(|&x| x == ')').map(|p| j + p),
                    Some('[') => chars[j + 1..].iter().position(|&x| x == ']').map(|p| j + 1 + p),
                    _ => (j > i + 1 && !chars[i + 1..j].iter().any(|c| c.is_whitespace() || *c == ','))
                        .then_some(j),
                });
                match (close, end) {
                    (Some(j), Some(e)) => {
                        let s: String = chars[i + 1..j].iter().collect();
                        for sp in inline(&s, style.patch(link), theme) {
                            push(&sp.text, sp.style);
                        }
                        i = e + 1;
                    }
                    _ => {
                        push("[", style);
                        i += 1;
                    }
                }
            }
            _ => {
                push(&c.to_string(), style);
                i += 1;
            }
        }
    }
    spans
}

/// A table row's cells, split at `|` — not at an escaped one (`\|`, left for `inline` to unescape) nor at one
/// inside backticks (Claude writes `a | b` in code; GFM would split it). The outer pipes are optional.
fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = match line.strip_suffix('|') {
        Some(l) if !l.ends_with('\\') => l,
        _ => line,
    };
    let chars: Vec<char> = line.chars().collect();
    let run = |j: usize| chars[j..].iter().take_while(|&&c| c == '`').count();
    let mut out = vec![String::new()];
    // The backtick run that opened the code span we're in
    let mut code: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        let cell = out.last_mut().unwrap();
        match chars[i] {
            '`' => {
                let n = run(i);
                // (only a run that closes later opens one)
                let closes = (i + n..chars.len()).any(|j| chars[j - 1] != '`' && run(j) == n);
                code = match code {
                    Some(o) if o == n => None,
                    None if closes => Some(n),
                    keep => keep,
                };
                cell.push_str(&"`".repeat(n));
                i += n;
                continue;
            }
            // In code nothing unescapes it later
            '\\' if chars.get(i + 1) == Some(&'|') => {
                cell.push_str(if code.is_some() { "|" } else { "\\|" });
                i += 1;
            }
            '|' if code.is_none() => out.push(String::new()),
            c => cell.push(c),
        }
        i += 1;
    }
    out.into_iter().map(|c| c.trim().to_string()).collect()
}

/// A table's delimiter row (`|---|:--:|--:|`) → its columns' alignment.
fn delimiter(line: &str) -> Option<Vec<Align>> {
    if !line.contains('|') {
        return None;
    }
    cells(line)
        .iter()
        .map(|c| {
            let d = c.strip_prefix(':').unwrap_or(c);
            let d = d.strip_suffix(':').unwrap_or(d);
            (!d.is_empty() && d.bytes().all(|b| b == b'-')).then(|| {
                match (c.starts_with(':'), c.ends_with(':')) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                }
            })
        })
        .collect()
}

/// Code block → lines (same colors as the editor if that language is already loaded — loading a grammar is
/// slow, and this runs on the main thread; usually it's the open doc's language anyway).
pub fn code_lines(code: &str, lang: Option<&str>, theme: &Theme) -> Vec<Line> {
    let code = code.trim_end_matches('\n').replace('\t', "    ");
    let mut paint = std::sync::Arc::new(vec![u32::MAX; code.len()]);
    let mut styles = std::sync::Arc::default();
    // Language name (`rust`) or an extension-like name (`rs`·`py`·`ts`)
    let spec = lang.and_then(syntax::resolve);
    if let Some(data) = spec.and_then(|s| Loader::global().loaded(&s.name)) {
        styles = theme.capture_styles();
        paint = painted(data, &code);
    }
    let mut out = Vec::new();
    let mut start = 0;
    for line in code.split('\n') {
        let mut spans: Vec<Span> = Vec::new();
        for (off, ch) in line.char_indices() {
            let cap = paint[start + off];
            let style = styles.get(cap as usize).copied().flatten().unwrap_or_default();
            match spans.last_mut() {
                Some(last) if last.style == style => last.text.push(ch),
                _ => spans.push(Span { text: ch.to_string(), style }),
            }
        }
        out.push(Line::Code(spans));
        start += line.len() + 1;
    }
    out
}

/// Per byte of `code`: the highlight capture painting it (`u32::MAX` none). Remembered for the last few
/// dozen blocks — a chat answer, a signature or a diagnostic card is drawn every frame, and each block is a
/// tree-sitter parse of its own. The captures don't depend on the theme, so they never go stale.
pub(crate) fn painted(data: std::sync::Arc<syntax::LangData>, code: &str) -> std::sync::Arc<Vec<u32>> {
    type Memo = Vec<(String, String, std::sync::Arc<Vec<u32>>)>;
    static MEMO: std::sync::Mutex<Memo> = std::sync::Mutex::new(Vec::new());
    const KEEP: usize = 64;
    const MAX_CODE: usize = 16 * 1024; // bigger blocks are parsed each time (a few MB at most kept)
    let memo = || MEMO.lock().unwrap_or_else(|e| e.into_inner());
    {
        // Least recently shown goes first — a block still streaming in adds one each time it grows
        let mut memo = memo();
        if let Some(k) = memo.iter().position(|(l, c, _)| *l == data.name && c == code) {
            let hit = memo.remove(k);
            let p = hit.2.clone();
            memo.push(hit);
            return p;
        }
    }
    let rope = Rope::from_str(code);
    let mut paint = vec![u32::MAX; code.len()];
    let syn = Syntax::parsed(data.clone(), &rope);
    for (a, b, c) in syntax::highlights(&syn, &rope, 0..code.len(), &mut QueryCursor::new()) {
        paint[a..b.min(code.len())].fill(c);
    }
    let paint = std::sync::Arc::new(paint);
    if code.len() <= MAX_CODE {
        let mut memo = memo();
        if memo.len() == KEEP {
            memo.remove(0);
        }
        memo.push((data.name.clone(), code.to_string(), paint.clone()));
    }
    paint
}

/// Screen lines wrapped to width. Paragraphs wrap by word; code is truncated.
pub fn wrap(lines: &[Line], width: usize, rule: Style) -> Vec<Vec<Span>> {
    wrap_with(lines, width, rule, rule)
}

/// `wrap`, with a stacked table's header names in `label` (a step above `rule` when the rules are faint).
pub fn wrap_with(lines: &[Line], width: usize, rule: Style, label: Style) -> Vec<Vec<Span>> {
    wrap_drawn(lines, width, rule, label).0
}

/// What of a laid-out row the layout drew rather than the text wrote: all of it (a rule, a table's rule), its
/// first `lead` characters (a list item's bullet or number, a quote's bar — merged into the text's span when
/// they share its style), its last `tail` (a cut code row's `…` and padding); `code`: a code row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drawn {
    pub whole: bool,
    pub lead: usize,
    pub tail: usize,
    pub code: bool,
}

/// `wrap_with`, and what of each row it drew (`Drawn`), row for row.
pub fn wrap_drawn(lines: &[Line], width: usize, rule: Style, label: Style) -> (Vec<Vec<Span>>, Vec<Drawn>) {
    let mut out = Vec::new();
    let mut drawn = Vec::new();
    for line in lines {
        let from = out.len();
        let mut lead = 0;
        match line {
            Line::Blank => out.push(Vec::new()),
            Line::Rule => out.push(vec![Span { text: "─".repeat(width), style: rule }]),
            // Code wraps too (a card is narrower than code) — continuation rows hang under its indentation
            Line::Code(spans) => {
                let lead = spans.iter().flat_map(|s| s.text.chars()).take_while(|c| *c == ' ').count();
                wrap_row(spans, lead, width, &mut out);
            }
            Line::Text { spans, indent } => {
                wrap_row(spans, *indent, width, &mut out);
                // (a list item's or a quote's head: the first row's first `indent` cells — `render` makes it
                // exactly that wide; a paragraph has none)
                let mut cells = 0;
                for c in out.get(from).into_iter().flatten().flat_map(|s| s.text.chars()) {
                    if cells >= *indent {
                        break;
                    }
                    cells += c.width().unwrap_or(0);
                    lead += 1;
                }
            }
            Line::Table { aligns, header, rows } => {
                table(aligns, header, rows, width, (rule, label), &mut out)
            }
        }
        for (k, row) in out[from..].iter().enumerate() {
            let whole = match line {
                Line::Rule => true,
                // (a table's rule under its header: nothing but `─` and the gaps between columns)
                Line::Table { .. } => {
                    row.iter().any(|s| s.text.contains('─'))
                        && row.iter().all(|s| {
                            s.style == rule && s.text.chars().all(|c| c == '─') || s.text.trim().is_empty()
                        })
                }
                _ => false,
            };
            let code = matches!(line, Line::Code(_));
            drawn.push(Drawn { whole, lead: if k == 0 { lead } else { 0 }, tail: 0, code });
        }
    }
    (out, drawn)
}

/// One line's spans into rows of `width`, broken at the last space (else after a word's last `/` — a path
/// keeps its names whole — else mid-word); rows after the first start with `indent` spaces.
fn wrap_row(spans: &[Span], indent: usize, width: usize, out: &mut Vec<Vec<Span>>) {
    // Narrow box: a deep indent mustn't leave no room for text
    let indent = indent.min(width / 2);
    let cells: Vec<(char, Style)> =
        spans.iter().flat_map(|s| s.text.chars().map(move |c| (c, s.style))).collect();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut w = 0;
    let mut i = 0;
    while i < cells.len() {
        let (c, _) = cells[i];
        let cw = c.width().unwrap_or(0);
        // A row holding only the indent takes the char even if it overflows (else no progress)
        if w + cw > width && row.len() > indent {
            // The word before just fits: break at this space
            if c == ' ' {
                out.push(group(&row));
                row = vec![(' ', Style::default()); indent];
                w = indent;
                i += 1;
                continue;
            }
            // Break at the last space (excluding spaces inside the indent), else mid-word
            let brk = row.iter().rposition(|(x, _)| *x == ' ').filter(|&p| p >= indent.max(1));
            let rest: Vec<(char, Style)> = match brk {
                Some(p) => {
                    let rest = row.split_off(p + 1);
                    row.pop();
                    rest
                }
                None => match row.iter().rposition(|(x, _)| *x == '/').filter(|&p| p > indent) {
                    Some(p) => row.split_off(p + 1),
                    None => Vec::new(),
                },
            };
            out.push(group(&row));
            row = vec![(' ', Style::default()); indent];
            row.extend(rest);
            w = row.iter().map(|(x, _)| x.width().unwrap_or(0)).sum();
            continue;
        }
        row.push(cells[i]);
        w += cw;
        i += 1;
    }
    out.push(group(&row));
}

fn group(cells: &[(char, Style)]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for &(c, style) in cells {
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push(c),
            _ => spans.push(Span { text: c.to_string(), style }),
        }
    }
    spans
}

/// Table columns are this many cells apart.
const GAP: usize = 2;
/// The narrowest a column gets: its longest word, up to this many cells (longer words break mid-word).
const WORD_MAX: usize = 14;

/// Per column, the most `f` gives over its cells (header included).
fn column_max(header: &[Vec<Span>], rows: &[Vec<Vec<Span>>], f: impl Fn(&[Span]) -> usize) -> Vec<usize> {
    let all = || std::iter::once(header).chain(rows.iter().map(Vec::as_slice));
    (0..header.len()).map(|i| all().map(|r| f(&r[i])).max().unwrap_or(0)).collect()
}

fn longest_word(spans: &[Span]) -> usize {
    let (mut best, mut cur) = (0, 0);
    for c in spans.iter().flat_map(|s| s.text.chars()) {
        if c == ' ' {
            cur = 0;
        } else {
            cur += c.width().unwrap_or(0);
            best = best.max(cur);
        }
    }
    best
}

/// A table at `width`: columns padded to their alignment under a rule, when they fit; too wide → the narrow
/// columns keep their width, the wide ones share the rest and wrap inside it (a blank line between rows once
/// one takes more than a line); not even each column's longest word fits → `stacked`. `(rule, label)`: the
/// rules' tone and a stacked row's header names'.
fn table(
    aligns: &[Align],
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    width: usize,
    (rule, label): (Style, Style),
    out: &mut Vec<Vec<Span>>,
) {
    let natural = column_max(header, rows, spans_width);
    let least = column_max(header, rows, |s| longest_word(s).min(WORD_MAX));
    let gaps = GAP * header.len().saturating_sub(1);
    if least.iter().sum::<usize>() + gaps > width {
        return stacked(header, rows, width, label, out);
    }
    let widths = share(&least, &natural, width - gaps);
    let wrapped = |row: &[Vec<Span>]| -> Vec<Vec<Vec<Span>>> {
        let wrap = |(cell, &w): (&Vec<Span>, &usize)| {
            let mut lines = Vec::new();
            wrap_row(cell, 0, w, &mut lines);
            lines
        };
        row.iter().zip(&widths).map(wrap).collect()
    };
    let body: Vec<_> = rows.iter().map(|r| wrapped(r)).collect();
    let tall = body.iter().any(|r| r.iter().any(|c| c.len() > 1));
    table_row(&wrapped(header), aligns, &widths, out);
    let mut line = Vec::new();
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            line.push(pad(GAP));
        }
        line.push(Span { text: "─".repeat(w), style: rule });
    }
    out.push(trimmed(line));
    for (k, r) in body.iter().enumerate() {
        if tall && k > 0 {
            out.push(Vec::new());
        }
        table_row(r, aligns, &widths, out);
    }
}

/// Column widths summing to at most `budget`: one water level, each column held between its least and its
/// natural width — the narrow ones whole, the wide ones level.
fn share(least: &[usize], natural: &[usize], budget: usize) -> Vec<usize> {
    let fill = |level: usize| -> Vec<usize> {
        least.iter().zip(natural).map(|(&l, &n)| level.clamp(l, n.max(l))).collect()
    };
    let top = natural.iter().copied().max().unwrap_or(0);
    let mut level = 0;
    while level < top && fill(level + 1).iter().sum::<usize>() <= budget {
        level += 1;
    }
    let mut widths = fill(level);
    // The level raises every wide column at once: the few cells left go one each, left to right
    let mut left = budget.saturating_sub(widths.iter().sum());
    for (w, &n) in widths.iter_mut().zip(natural) {
        if left > 0 && *w < n {
            *w += 1;
            left -= 1;
        }
    }
    widths
}

/// One table row, its cells already wrapped → screen rows, each piece padded to its column's alignment.
fn table_row(cells: &[Vec<Vec<Span>>], aligns: &[Align], widths: &[usize], out: &mut Vec<Vec<Span>>) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
    for j in 0..height {
        let mut line = Vec::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push(pad(GAP));
            }
            let piece = cell.get(j).map_or(&[][..], Vec::as_slice);
            let room = widths[i].saturating_sub(spans_width(piece));
            let before = match aligns[i] {
                Align::Left => 0,
                Align::Center => room / 2,
                Align::Right => room,
            };
            line.push(pad(before));
            line.extend(piece.iter().cloned());
            line.push(pad(room - before));
        }
        out.push(trimmed(line));
    }
}

/// Too many columns for the width: each row a small card — its first cell the title (bold), then
/// `header  value` for the other cells that have something, the headers in `label` and the values wrapping
/// under themselves.
fn stacked(
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    width: usize,
    label: Style,
    out: &mut Vec<Vec<Span>>,
) {
    let label_w = header.iter().skip(1).map(|h| spans_width(h)).max().unwrap_or(0).min(width / 3);
    let indent = 2 + label_w + GAP;
    let bold = Style { bold: true, ..Style::default() };
    for (k, row) in rows.iter().enumerate() {
        if k > 0 {
            out.push(Vec::new());
        }
        let title: Vec<Span> =
            row[0].iter().map(|s| Span { text: s.text.clone(), style: s.style.patch(bold) }).collect();
        wrap_row(&title, 0, width, out);
        for (h, cell) in header.iter().zip(row).skip(1) {
            if cell.iter().all(|s| s.text.trim().is_empty()) {
                continue;
            }
            let name = cut(&h.iter().map(|s| s.text.as_str()).collect::<String>(), label_w);
            let mut lines = Vec::new();
            wrap_row(cell, 0, width.saturating_sub(indent), &mut lines);
            for (j, mut l) in lines.into_iter().enumerate() {
                let mut line = if j == 0 {
                    let gap = indent - 2 - name.width();
                    vec![pad(2), Span { text: name.clone(), style: label }, pad(gap)]
                } else {
                    vec![pad(indent)]
                };
                line.append(&mut l);
                out.push(line);
            }
        }
    }
}

fn pad(n: usize) -> Span {
    Span { text: " ".repeat(n), style: Style::default() }
}

/// Without empty spans and trailing spaces (a row's padding).
fn trimmed(mut line: Vec<Span>) -> Vec<Span> {
    line.retain(|s| !s.text.is_empty());
    while let Some(last) = line.last_mut() {
        let keep = last.text.trim_end_matches(' ').len();
        if keep > 0 {
            last.text.truncate(keep);
            break;
        }
        line.pop();
    }
    line
}

/// `s` in at most `w` cells, `…` at the end if cut.
fn cut(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        used += cw;
        out.push(c);
    }
    if w > 0 {
        out.truncate(out.trim_end().len());
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Code in a card wraps at a space, continuing under its own indentation.
    #[test]
    fn code_wraps_under_its_indentation() {
        let line = Line::Code(vec![Span {
            text: "    let total = items.iter().sum();".into(),
            style: Style::default(),
        }]);
        let rows: Vec<String> = wrap(&[line], 20, Style::default())
            .iter()
            .map(|r| r.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(rows, ["    let total =", "    items.iter().sum", "    ();"]);
    }

    fn plain(lines: &[Vec<Span>]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect()
    }

    #[test]
    fn blocks_and_inline() {
        let t = Theme::builtin();
        let md = "```rust\nfn get(&self) -> u32\n```\n\n---\n\n# Title\n\nReturns a **reference** to `x`,\nor *none*.\n\n- one\n- two \\* three\n\n[docs](https://x)\n\n\n";
        // Only already-loaded languages are colored (the open doc's, normally)
        let rust = syntax::spec("rust").and_then(|s| Loader::global().load(s).ok()).is_some();
        let lines = render(md, &t, None);
        let text = plain(&wrap(&lines, 40, Style::default()));
        assert_eq!(
            text,
            [
                "fn get(&self) -> u32",
                "────────────────────────────────────────",
                "Title",
                "",
                "Returns a reference to x, or none.",
                "",
                "• one",
                "• two * three",
                "",
                "docs"
            ]
        );
        let Line::Text { spans, .. } = &lines[4] else { panic!() };
        assert!(spans.iter().any(|s| s.text == "reference" && s.style.bold));
        assert!(spans.iter().any(|s| s.text == "none" && s.style.italic));
        assert!(spans.iter().any(|s| s.text == "x" && s.style == t.get("markup.raw")));
        // Code gets colored if the grammar exists (plain text otherwise)
        if rust {
            let Line::Code(spans) = &lines[0] else { panic!() };
            assert!(spans.iter().any(|s| s.text == "fn" && s.style == t.get("keyword")));
        }
    }

    #[test]
    fn underscore_emphasis_only_at_word_edges() {
        let t = Theme::builtin();
        let spans = |s: &str| inline_spans(s, &t);
        let text = |s: &str| spans(s).iter().map(|sp| sp.text.clone()).collect::<String>();
        let italic = |s: &str, word: &str| spans(s).iter().any(|sp| sp.text == word && sp.style.italic);
        let bold = |s: &str, word: &str| spans(s).iter().any(|sp| sp.text == word && sp.style.bold);
        assert!(italic("the _squared_ value", "squared"));
        assert_eq!(text("the _squared_ value"), "the squared value");
        assert!(bold("a __strong__ one", "strong"));
        assert!(italic("(_x_),", "x"), "punctuation around is a word edge");
        // Inside words, alone, or unclosed: literal
        for s in ["snake_case", "x_1_y", "a _ b", "_unclosed", "trailing_", "__", "a__b__c"] {
            assert_eq!(text(s), s);
            assert!(spans(s).iter().all(|sp| !sp.style.italic && !sp.style.bold), "{s}");
        }
        // `_` never closes `*` and vice versa
        assert_eq!(text("*a_b* _c*d_"), "a_b c*d");
        assert!(italic("*a_b*", "a_b"));
    }

    #[test]
    fn wraps_words_and_list_indent() {
        let t = Theme::builtin();
        let lines = render(
            "- 실타래 한 가닥을 이어 붙인다 aaaaaaaaaaaa\n\nsnake_case * 2 [Hash] [`Eq`][e] [1, 2]\n\n[e]: https://x",
            &t,
            None,
        );
        assert_eq!(
            plain(&wrap(&lines, 16, Style::default())),
            [
                "• 실타래 한",
                "  가닥을 이어",
                "  붙인다",
                "  aaaaaaaaaaaa",
                "",
                "snake_case * 2",
                "Hash Eq [1, 2]"
            ]
        );
    }

    /// A word that exactly fills the row stays on it (the space after it used to push it down).
    #[test]
    fn a_word_that_just_fits_stays_on_its_row() {
        let text = Line::Text {
            spans: vec![Span { text: "aaa bbb ccc".into(), style: Style::default() }],
            indent: 0,
        };
        assert_eq!(plain(&wrap(&[text], 7, Style::default())), ["aaa bbb", "ccc"]);
    }

    const FITS: &str = "| 부분 | 줄 수 | 비고 |\n|---|---:|:---:|\n| `markdown.rs` 표 파싱 | ~120 | **핵심** |\n\
                        | `wrap` 배치 | ~80 | |\n| 테스트 | ~150 | 스냅샷 1 |";
    const WRAPS: &str = "| 방식 | 장점 | 단점 |\n|---|---|---|\n\
                         | 셀 안에서 줄바꿈 | 내용이 전부 보이고 열 정렬이 유지된다 | 행 높이가 들쭉날쭉해진다 |\n\
                         | 잘라내기(`…`) | 한 행 = 한 줄, 훑어보기 좋다 | 잘린 내용은 볼 방법이 없다 |";
    const STACKS: &str = "| 설정 | 기본값 | 범위 | 설명 | 예시 | 비고 |\n|---|---|---|---|---|---|\n\
                          | `chat.width` | `0.4` | `0.2–0.6` | 채팅 패널이 차지하는 화면 비율 | `:set chat.width 0.5` | 좁으면 전체 화면 |\n\
                          | `chat.follow` | `true` | `true·false` | 답이 가리키는 코드로 에디터가 따라감 | `:set chat.follow false` | |";

    fn table(md: &str, width: usize) -> Vec<String> {
        plain(&wrap(&render(md, &Theme::builtin(), None), width, Style::default()))
    }

    /// A table that fits: columns two cells apart, a rule under each, cells padded by display width (Korean is
    /// two cells) to their column's alignment.
    #[test]
    fn table_fits_aligned() {
        let rows = table(FITS, 56);
        assert_eq!(
            rows,
            [
                "부분                 줄 수    비고",
                "───────────────────  ─────  ────────",
                "markdown.rs 표 파싱   ~120    핵심",
                "wrap 배치              ~80",
                "테스트                ~150  스냅샷 1",
            ]
        );
        // Right-aligned: the numbers end where the header does; centered: 핵심 two cells into its column
        let end = |r: &str, s: &str| r[..r.find(s).unwrap() + s.len()].width();
        for (r, s) in [(2, "~120"), (3, "~80"), (4, "~150")] {
            assert_eq!(end(&rows[r], s), end(&rows[0], "줄 수"), "{s}");
        }
        assert_eq!(rows[2][..rows[2].find("핵심").unwrap()].width(), 19 + 2 + 5 + 2 + 2);
        // A card sizes itself by the table's natural width
        assert_eq!(render(FITS, &Theme::builtin(), None)[0].width(), 36);
    }

    #[test]
    fn table_cells_keep_inline_styles() {
        let t = Theme::builtin();
        let rows = wrap(
            &render("| **a** | b |\n|---|---|\n| `code` x | **bold** |", &t, None),
            40,
            Style::default(),
        );
        let style = |r: usize, s: &str| rows[r].iter().find(|sp| sp.text == s).map(|sp| sp.style);
        assert!(rows[0].iter().filter(|s| !s.text.trim().is_empty()).all(|s| s.style.bold), "header is bold");
        assert_eq!(style(2, "code"), Some(t.get("markup.raw")));
        assert!(style(2, "bold").is_some_and(|s| s.bold));
    }

    /// Too wide: narrow columns keep their width, the wide ones share the rest; cells wrap in their column and
    /// rows taller than one line get a blank line between them.
    #[test]
    fn table_wraps_long_cells() {
        assert_eq!(
            table(WRAPS, 56),
            [
                "방식              장점                단점",
                "────────────────  ──────────────────  ──────────────────",
                "셀 안에서 줄바꿈  내용이 전부 보이고  행 높이가",
                "                  열 정렬이 유지된다  들쭉날쭉해진다",
                "",
                "잘라내기(…)       한 행 = 한 줄,      잘린 내용은 볼",
                "                  훑어보기 좋다       방법이 없다",
            ]
        );
    }

    /// Columns that can't fit even at their longest words: each row a small card — the first cell its title,
    /// the rest `header  value`, empty cells left out.
    #[test]
    fn table_stacks_when_columns_cant_fit() {
        assert_eq!(
            table(STACKS, 56),
            [
                "chat.width",
                "  기본값  0.4",
                "  범위    0.2–0.6",
                "  설명    채팅 패널이 차지하는 화면 비율",
                "  예시    :set chat.width 0.5",
                "  비고    좁으면 전체 화면",
                "",
                "chat.follow",
                "  기본값  true",
                "  범위    true·false",
                "  설명    답이 가리키는 코드로 에디터가 따라감",
                "  예시    :set chat.follow false",
            ]
        );
    }

    /// A stacked row's header names say what each value is: their own tone (`label`), a step above the
    /// rules' (`rule`), which stay as they were.
    #[test]
    fn stacked_labels_take_their_own_tone() {
        let t = Theme::builtin();
        let (rule, label) =
            (Style { italic: true, ..Style::default() }, Style { underline: true, ..Style::default() });
        let style = |md: &str, s: &str| {
            let rows = wrap_with(&render(md, &t, None), 56, rule, label);
            rows.iter().flatten().find(|sp| sp.text == s).map(|sp| sp.style)
        };
        assert_eq!(style(STACKS, "기본값"), Some(label));
        assert_eq!(style(FITS, &"─".repeat(5)), Some(rule));
        // `wrap` gives both the rule's tone
        let rows = wrap(&render(STACKS, &t, None), 56, rule);
        assert_eq!(rows.iter().flatten().find(|sp| sp.text == "기본값").map(|sp| sp.style), Some(rule));
    }

    #[test]
    fn tables_stay_inside_any_width() {
        for md in [FITS, WRAPS, STACKS] {
            // Every body cell's text is there (header labels may be cut when stacked)
            let want: Vec<char> = md
                .lines()
                .skip(2)
                .flat_map(str::chars)
                .filter(|c| !c.is_whitespace() && !"|`*".contains(*c))
                .collect();
            for w in 12..=80 {
                let rows = table(md, w);
                for r in &rows {
                    assert!(r.width() <= w, "{w}: {r:?}");
                }
                let mut got: Vec<char> = rows.concat().chars().filter(|c| !c.is_whitespace()).collect();
                for c in &want {
                    let k = got
                        .iter()
                        .position(|g| g == c)
                        .unwrap_or_else(|| panic!("{w}: {c:?} lost in {rows:#?}"));
                    got.remove(k);
                }
            }
        }
    }

    #[test]
    fn table_parsing_edges() {
        // No outer pipes; `\|` and a pipe in backticks stay in their cell; a short row gets blank cells, a long
        // one loses the extra
        assert_eq!(
            table("a | b\n:-|-:\n`x|y` | p \\| q\n| 1 | 2 | 3 |\n| 4 |", 40),
            ["a        b", "───  ─────", "x|y  p | q", "1        2", "4"]
        );
        // Not tables: a delimiter row of another size, no delimiter row
        assert_eq!(table("a | b\n|---|---|---|", 40), ["a | b |---|---|---|"]);
        assert_eq!(table("x | y\nz", 40), ["x | y z"]);
        // It cuts off a paragraph, ends at a line without a pipe, and gets a blank line on each side
        assert_eq!(table("intro\n| a |\n|---|\n| 1 |\nafter", 40), ["intro", "", "a", "─", "1", "", "after"]);
        // Still streaming in: a row cut short
        assert_eq!(table("| a | b |\n|---|---|\n| 1", 40), ["a  b", "─  ─", "1"]);
    }

    #[test]
    fn deep_indent_in_a_narrow_box_still_wraps() {
        let text = |s: &str, indent| Line::Text {
            spans: vec![Span { text: s.into(), style: Style::default() }],
            indent,
        };
        // Indent wider than the box (used to loop forever: rows of only indent)
        let rows = plain(&wrap(&[text("abcdef", 6)], 4, Style::default()));
        assert_eq!(rows, ["abcd", "  ef"]);
        // A wide char that never fits next to the indent still lands somewhere
        let rows = plain(&wrap(&[text("ab타래", 2)], 3, Style::default()));
        assert_eq!(rows.concat().replace(' ', ""), "ab타래");
        assert!(plain(&wrap(&[text("xyz", 3)], 0, Style::default())).len() <= 3);
    }

    /// A path longer than the row breaks after a `/` — its folder names stay whole; a word with none still
    /// breaks mid-word.
    #[test]
    fn a_long_path_breaks_after_a_slash() {
        let text =
            |s: &str| Line::Text { spans: vec![Span { text: s.into(), style: Style::default() }], indent: 6 };
        let rows =
            plain(&wrap(&[text("  ~/.local/share/tarae/worktrees/proj-ba8dcb47")], 34, Style::default()));
        assert_eq!(rows, ["  ~/.local/share/tarae/worktrees/", "      proj-ba8dcb47"]);
        let rows = plain(&wrap(&[text("abcdefghij")], 6, Style::default()));
        assert_eq!(rows, ["abcdef", "   ghi", "   j"], "no slash: mid-word, as before");
    }
}
