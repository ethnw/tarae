//! Indentation for a new line (`o`, `O`, Enter) from the syntax tree — `indents.scm` in Helix's format,
//! read by our own implementation of its documented rules: captures `@indent` `@outdent` (at most one
//! level per line, and they cancel on one line), `@indent.always` `@outdent.always` (they stack),
//! `@align`/`@anchor`, `@extend`, `@extend.prevent-once`; scopes `tail` (all but the node's first line —
//! the default for indents) and `all` (the default for outdents and aligns), set with `#set! "scope"`;
//! predicates `#not-kind-eq?` `#same-line?` `#not-same-line?` `#one-line?` `#not-one-line?`. Pure.
//!
//! Hybrid, as Helix does by default: the query gives a level to the new line and to the nearest non-blank
//! line above it, and the new line gets that line's real indentation plus the difference — so a query's
//! blind spots and a file's own style don't turn into wrong absolute indents. An `@align` gives an
//! absolute column instead. Without a query (or a tree), a heuristic: one level more after an opening
//! bracket.

use std::collections::{BTreeMap, HashMap};

use ropey::Rope;
use tree_sitter::{Node, Query, QueryCursor, QueryPredicateArg, StreamingIterator, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cap {
    Indent,
    Outdent,
    IndentAlways,
    OutdentAlways,
    Align,
    Anchor,
    Extend,
    PreventOnce,
    Other,
}

/// A compiled `indents.scm` and what its captures mean.
pub struct Indents {
    query: Query,
    caps: Vec<Cap>,
    /// Per pattern: its `#set! "scope"` (true = `all`), if any.
    scope_all: Vec<Option<bool>>,
}

impl Indents {
    pub fn new(query: Query) -> Self {
        let caps = query
            .capture_names()
            .iter()
            .map(|n| match *n {
                "indent" => Cap::Indent,
                "outdent" => Cap::Outdent,
                "indent.always" => Cap::IndentAlways,
                "outdent.always" => Cap::OutdentAlways,
                "align" => Cap::Align,
                "anchor" => Cap::Anchor,
                "extend" => Cap::Extend,
                "extend.prevent-once" => Cap::PreventOnce,
                _ => Cap::Other,
            })
            .collect();
        let scope_all = (0..query.pattern_count())
            .map(|p| {
                let props = query.property_settings(p);
                let scope = props.iter().find(|s| &*s.key == "scope")?;
                Some(scope.value.as_deref() == Some("all"))
            })
            .collect();
        Indents { query, caps, scope_all }
    }
}

/// How the file indents: tabs, or spaces this wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unit {
    pub tabs: bool,
    /// Columns per level.
    pub width: usize,
    /// Columns per tab.
    pub tab: usize,
}

impl Unit {
    /// Whitespace reaching `cols` columns.
    pub fn fill(&self, cols: usize) -> String {
        if self.tabs {
            format!("{}{}", "\t".repeat(cols / self.tab), " ".repeat(cols % self.tab))
        } else {
            " ".repeat(cols)
        }
    }

    /// One level.
    pub fn level(&self) -> String {
        if self.tabs { "\t".into() } else { " ".repeat(self.width) }
    }
}

/// The file's indent unit, from its first 2000 lines: tabs if more lines start with one than with a space,
/// otherwise the commonest step between neighbouring lines (2–8). When nothing says: tabs if the language
/// wants them (`tabs` — Go, Makefiles), else `tab` spaces.
pub fn detect(text: &Rope, tab: usize, tabs_by_default: bool) -> Unit {
    let (mut tabs, mut spaces) = (0, 0);
    let mut steps: HashMap<usize, usize> = HashMap::new();
    let mut prev = 0;
    for line in text.lines().take(2000) {
        if line.chars().all(char::is_whitespace) {
            continue;
        }
        let first = line.chars().next();
        match first {
            Some('\t') => tabs += 1,
            Some(' ') => spaces += 1,
            _ => {}
        }
        if first == Some('\t') {
            continue;
        }
        let w = line.chars().take_while(|c| *c == ' ').count();
        if w > prev && (2..=8).contains(&(w - prev)) {
            *steps.entry(w - prev).or_default() += 1;
        }
        prev = w;
    }
    if tabs > spaces || (tabs_by_default && spaces == 0) {
        return Unit { tabs: true, width: tab, tab };
    }
    let width =
        steps.into_iter().max_by_key(|&(step, n)| (n, std::cmp::Reverse(step))).map_or(tab, |(s, _)| s);
    Unit { tabs: false, width, tab }
}

/// Columns the leading whitespace of `line` takes.
pub fn width_of(text: &Rope, line: usize, tab: usize) -> usize {
    let mut w = 0;
    for c in text.line(line).chars() {
        match c {
            ' ' => w += 1,
            '\t' => w += tab - w % tab,
            _ => break,
        }
    }
    w
}

fn blank(text: &Rope, line: usize) -> bool {
    text.line(line).chars().all(char::is_whitespace)
}

/// The indentation for a line broken at `at` (the cursor for Enter, a line's end for `o`); `follows` =
/// what's after `at` on its line moves down with it (Enter — a closer there moves back, `{ x|}`). After a
/// comma, though, the list goes on (`f(a,|)`): the closer just rides along on the continued line, which
/// indents (or aligns) like the list. None = no answer from the tree (use the heuristic).
pub fn new_line(
    ix: &Indents,
    tree: &Tree,
    text: &Rope,
    at: usize,
    follows: bool,
    unit: Unit,
) -> Option<String> {
    let content = if follows { content_after(text, at) } else { None };
    let closer =
        content.is_some_and(|b| matches!(text.byte_slice(b..).chars().next(), Some(')' | ']' | '}')));
    let after_comma = last_before(text, at).is_some_and(|(b, _)| text.byte(b) == b',');
    let content = if closer && after_comma { None } else { content };
    let new = level(ix, tree, text, at, content)?;
    if let Some((anchor, after)) = new.align {
        return Some(unit.fill(add(column(text, anchor, unit.tab), after, unit)));
    }
    // The nearest non-blank line at or above the break, as it stands
    let line = text.byte_to_line(at);
    let reference = (0..=line).rev().find(|&l| !blank(text, l));
    let absolute = || Some(unit.fill(add(0, new.total, unit)));
    let Some(r) = reference else { return absolute() };
    let start = crate::movement::first_non_whitespace(text, r);
    let before = crate::movement::line_start(text, r).saturating_sub(1);
    let old = level(ix, tree, text, before, Some(start))?;
    if old.align.is_some() {
        return absolute(); // an aligned line's indentation isn't levels
    }
    Some(unit.fill(add(width_of(text, r, unit.tab), new.total - old.total, unit)))
}

/// Without a query: the nearest non-blank line's indentation, one level more after an opening bracket (or
/// a `:` in `colon` languages) — the last text before the break, blank lines skipped; a closer that moves
/// down with it (`follows`, Enter at `[|]`) stays at the opener's line's level.
pub fn heuristic(text: &Rope, at: usize, unit: Unit, colon: bool, follows: bool) -> String {
    let line = text.byte_to_line(at);
    let Some(r) = (0..=line).rev().find(|&l| !blank(text, l)) else { return String::new() };
    let last = last_before(text, at).and_then(|(b, _)| text.byte_slice(b..).chars().next());
    let opens = matches!(last, Some('{' | '[' | '(')) || (colon && last == Some(':'));
    let closes = follows
        && content_after(text, at)
            .is_some_and(|b| matches!(text.byte_slice(b..).chars().next(), Some(')' | ']' | '}')));
    unit.fill(width_of(text, r, unit.tab) + if opens && !closes { unit.width } else { 0 })
}

fn add(cols: usize, levels: i32, unit: Unit) -> usize {
    (cols as i64 + levels as i64 * unit.width as i64).max(0) as usize
}

fn column(text: &Rope, byte: usize, tab: usize) -> usize {
    let line = text.byte_to_line(byte);
    let start = text.line_to_byte(line);
    let mut w = 0;
    for c in text.byte_slice(start..byte).chars() {
        w += if c == '\t' { tab - w % tab } else { 1 };
    }
    w
}

/// The first non-blank byte after `at` on its line — the new line's first text.
fn content_after(text: &Rope, at: usize) -> Option<usize> {
    let line = text.byte_to_line(at);
    let end = crate::movement::line_end(text, line);
    let mut b = at;
    for c in text.byte_slice(at..end).chars() {
        if !c.is_whitespace() {
            return Some(b);
        }
        b += c.len_utf8();
    }
    None
}

/// The last non-blank byte before `at` (and its length).
fn last_before(text: &Rope, at: usize) -> Option<(usize, usize)> {
    let before = text.byte_slice(..at);
    let mut chars = before.chars_at(before.len_chars());
    let mut b = at;
    while let Some(c) = chars.prev() {
        b -= c.len_utf8();
        if !c.is_whitespace() {
            return Some((b, c.len_utf8()));
        }
    }
    None
}

struct Level {
    total: i32,
    /// An `@align` over the new line: its anchor, and the levels added by nodes starting below it.
    align: Option<(usize, i32)>,
}

#[derive(Default)]
struct Bucket {
    indent: bool,
    outdent: bool,
    indent_always: i32,
    outdent_always: i32,
}

impl Bucket {
    fn delta(&self) -> i32 {
        let up = if self.indent_always > 0 { self.indent_always } else { i32::from(self.indent) };
        let down = if self.outdent_always > 0 { self.outdent_always } else { i32::from(self.outdent) };
        up - down
    }
}

struct Hit<'t> {
    cap: Cap,
    all: bool,
    anchor: Option<Node<'t>>,
}

/// The level of a line broken at `at` whose first text (if any) is at `content`: walking up from the node
/// there (or at the last text before the break), every node that covers the new line adds its captures,
/// grouped by the line the node starts on.
fn level(ix: &Indents, tree: &Tree, text: &Rope, at: usize, content: Option<usize>) -> Option<Level> {
    let root = tree.root_node();
    let (from, len) = match content {
        Some(b) => (b, text.byte_slice(b..).chars().next().map_or(1, char::len_utf8)),
        None => match last_before(text, at) {
            Some(x) => x,
            None => return Some(Level { total: 0, align: None }),
        },
    };
    let start = root.descendant_for_byte_range(from, from + len)?;
    let mut chain = Vec::new();
    let mut n = Some(start);
    while let Some(x) = n {
        chain.push(x);
        n = x.parent();
    }
    let hits = captures(ix, text, root, &chain, start);
    let line = |b: usize| text.byte_to_line(b.min(text.len_bytes()));
    let break_line = line(at);
    const NEW: usize = usize::MAX;
    let mut buckets: BTreeMap<usize, Bucket> = BTreeMap::new();
    let mut align: Option<(usize, usize)> = None; // (anchor byte, line of the align node)
    let mut prevent = false;
    for node in &chain {
        let mine = hits.get(&node.id()).map(Vec::as_slice).unwrap_or_default();
        let has = |c: Cap| mine.iter().any(|h| h.cap == c);
        let on_new = content.is_some_and(|b| node.start_byte() >= b);
        let covers = on_new
            || node.end_byte() > at
            || (node.end_byte() == at && unfinished(*node))
            || (has(Cap::Extend) && !std::mem::take(&mut prevent) && extends(text, *node, break_line));
        if has(Cap::PreventOnce) {
            prevent = true;
        }
        if !covers {
            continue;
        }
        let at_line = if on_new { NEW } else { line(node.start_byte()) };
        for h in mine {
            if !(h.all || !on_new) {
                continue; // `tail`: not the node's own first line
            }
            let b = buckets.entry(at_line).or_default();
            match h.cap {
                Cap::Indent => b.indent = true,
                Cap::Outdent => b.outdent = true,
                Cap::IndentAlways => b.indent_always += 1,
                Cap::OutdentAlways => b.outdent_always += 1,
                Cap::Align if align.is_none() => {
                    if let Some(a) = h.anchor.filter(|a| a.start_byte() < content.unwrap_or(at)) {
                        align = Some((a.start_byte(), at_line));
                    }
                }
                _ => {}
            }
        }
    }
    let total = buckets.values().map(Bucket::delta).sum();
    let align = align.map(|(anchor, from_line)| {
        let after = buckets.range(from_line.saturating_add(1)..).map(|(_, b)| b.delta()).sum();
        (anchor, after)
    });
    Some(Level { total, align })
}

/// Closed only by a token the parser made up (`MISSING ")"`) — still open where it stops.
fn unfinished(node: Node) -> bool {
    let mut n = node;
    while let Some(last) = n.child(n.child_count().wrapping_sub(1)) {
        if last.is_missing() {
            return true;
        }
        n = last;
    }
    false
}

/// `@extend`: a node that ended before the break still holds it if every non-blank line after it, up to
/// the break's, is indented deeper than the line it starts on.
fn extends(text: &Rope, node: Node, break_line: usize) -> bool {
    let tab = 8; // only compared with each other
    let base = width_of(text, text.byte_to_line(node.start_byte()), tab);
    let last = text.byte_to_line(node.end_byte().saturating_sub(1).max(node.start_byte()));
    (last + 1..=break_line).all(|l| blank(text, l) || width_of(text, l, tab) > base)
}

/// Captures on the nodes of `chain` (id → what they are), predicates checked.
fn captures<'t>(
    ix: &Indents,
    text: &Rope,
    root: Node<'t>,
    chain: &[Node<'t>],
    start: Node<'t>,
) -> HashMap<usize, Vec<Hit<'t>>> {
    let ids: std::collections::HashSet<usize> = chain.iter().map(Node::id).collect();
    let mut out: HashMap<usize, Vec<Hit<'t>>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(start.start_byte()..start.end_byte().max(start.start_byte() + 1));
    // (a node's text for predicates — ends snapped to chars: the tree may be a beat behind the text)
    let provider = |node: Node| crate::syntax::node_slice(text, node).chunks().map(str::as_bytes);
    let line = |n: Node| n.start_position().row;
    let mut matches = cursor.matches(&ix.query, root, provider);
    while let Some(m) = matches.next() {
        let node_of = |i: u32| m.captures().iter().find(|c| c.index == i).map(|c| c.node);
        let ok = ix.query.general_predicates(m.pattern_index).iter().all(|p| {
            let arg_node = |k: usize| match p.args.get(k) {
                Some(QueryPredicateArg::Capture(i)) => node_of(*i),
                _ => None,
            };
            let arg_str = |k: usize| match p.args.get(k) {
                Some(QueryPredicateArg::String(s)) => Some(&**s),
                _ => None,
            };
            match &*p.operator {
                "not-kind-eq?" => arg_node(0).zip(arg_str(1)).is_none_or(|(n, k)| n.kind() != k),
                "same-line?" => arg_node(0).zip(arg_node(1)).is_some_and(|(a, b)| line(a) == line(b)),
                "not-same-line?" => arg_node(0).zip(arg_node(1)).is_some_and(|(a, b)| line(a) != line(b)),
                "one-line?" => arg_node(0).is_some_and(|n| n.start_position().row == n.end_position().row),
                "not-one-line?" => {
                    arg_node(0).is_some_and(|n| n.start_position().row != n.end_position().row)
                }
                _ => true,
            }
        });
        if !ok {
            continue;
        }
        let anchor = m.captures().iter().find(|c| ix.caps[c.index as usize] == Cap::Anchor).map(|c| c.node);
        for c in m.captures() {
            let cap = ix.caps[c.index as usize];
            if matches!(cap, Cap::Other | Cap::Anchor) || !ids.contains(&c.node.id()) {
                continue;
            }
            let default_all = matches!(cap, Cap::Outdent | Cap::OutdentAlways | Cap::Align);
            let all = ix.scope_all[m.pattern_index].unwrap_or(default_all);
            out.entry(c.node.id()).or_default().push(Hit { cap, all, anchor });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Loader, Syntax, spec};

    /// Where Enter at each `|` in `src` puts the new line's text: its indentation, or None without the
    /// grammar.
    fn enter(lang: &str, src: &str) -> Option<String> {
        let at = src.find('|').unwrap();
        let text = Rope::from_str(&src.replace('|', ""));
        let data = Loader::global().load(spec(lang)?).ok()?;
        let ix = data.indents.as_ref()?;
        let mut syn = Syntax::new(data.clone());
        let job = syn.start_parse(&text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
        new_line(ix, syn.tree.as_ref()?, &text, at, true, detect(&text, 4, lang == "go"))
    }

    #[test]
    fn rust_blocks_arguments_and_closers() {
        let Some(got) = enter("rust", "fn a() {|\n}\n") else { return };
        assert_eq!(got, "    ", "into a block");
        assert_eq!(enter("rust", "fn a() {\n    let x = 1;|\n}\n").unwrap(), "    ", "stays in it");
        assert_eq!(enter("rust", "fn a() {\n    f(1,|\n}\n").unwrap(), "        ", "a call's arguments");
        assert_eq!(enter("rust", "fn a() {\n    x\n}|\n").unwrap(), "", "after the block");
        assert_eq!(
            enter("rust", "fn a() {\n    if x {\n        y|}\n}\n").unwrap(),
            "    ",
            "a closer moves back"
        );
        assert_eq!(
            enter("rust", "fn a() {\n    f(1,|)\n}\n").unwrap(),
            "        ",
            "after a comma the list goes on — the closer rides along"
        );
        assert_eq!(enter("rust", "fn a() {\n    f(1|)\n}\n").unwrap(), "    ", "…not without one");
        // A file indenting by 2 keeps its 2
        assert_eq!(enter("rust", "fn a() {\n  let x = {|\n  };\n}\n").unwrap(), "    ");
    }

    #[test]
    fn python_extends_and_ends_blocks() {
        let Some(got) = enter("python", "def f():|\n") else { return };
        assert_eq!(got, "    ");
        assert_eq!(enter("python", "class A:\n    def f(self):\n        x = 1|\n").unwrap(), "        ");
        assert_eq!(
            enter("python", "class A:\n    def f(self):\n        return x|\n").unwrap(),
            "    ",
            "return ends it"
        );
        assert_eq!(
            enter("python", "def f(a,|\n      b):\n    pass\n").unwrap(),
            "      ",
            "aligned under the first parameter"
        );
        assert_eq!(
            enter("python", "class A:\n    def f(self, a,|):\n        pass\n").unwrap(),
            "          ",
            "aligned while the closer (auto-paired) rides along"
        );
    }

    #[test]
    fn go_uses_tabs() {
        let Some(got) = enter("go", "package a\n\nfunc f() {\n\tx := 1|\n}\n") else { return };
        assert_eq!(got, "\t");
        assert_eq!(enter("go", "package a\n\nfunc f() {|\n}\n").unwrap(), "\t");
    }

    /// A sweep over languages (each only when its grammar is installed): Enter at `|` gives this
    /// indentation — in the file's own unit.
    #[test]
    fn languages() {
        let cases = [
            ("javascript", "function f() {\n  if (x) {\n    y();|\n  }\n}\n", "    "),
            ("javascript", "function f() {\n  if (x) {\n    y();\n  }|\n}\n", "  "),
            ("typescript", "const o = {\n  a: 1,|\n};\n", "  "),
            ("tsx", "const a = (\n  <div>|\n  </div>\n);\n", "    "),
            ("java", "class A {\n    void f() {|\n    }\n}\n", "        "),
            ("c", "int main() {\n    return 0;|\n}\n", "    "),
            ("json", "{\n  \"a\": [|]\n}\n", "  "), // the closer moving down lines up with its opener's line
            ("yaml", "a:\n  b: 1|\n", "  "),
            ("yaml", "items:\n  - name: x|\n", "    "),
            ("lua", "function f()\n    return 1|\nend\n", "    "),
            ("ruby", "class A\n  def f\n    x|\n  end\nend\n", "    "),
            ("python", "if x:\n    a = 1\nelse:|\n", "    "),
            ("python", "def f():\n    if x:\n        pass|\n", "    "),
            ("go", "package a\n\nfunc f() {\n\tswitch x {\n\tcase 1:|\n\t}\n}\n", "\t\t"),
            ("rust", "fn f() {\n    match x {\n        A => 1,|\n    }\n}\n", "        "),
            ("rust", "struct A {\n    a: u8,|\n}\n", "    "),
        ];
        for (lang, src, want) in cases {
            if let Some(got) = enter(lang, src) {
                assert_eq!(got, want, "{lang}: {src:?}");
            }
        }
    }

    #[test]
    fn units_and_heuristic() {
        let two = Rope::from_str("a {\n  b {\n    c\n  }\n}\n");
        assert_eq!(detect(&two, 4, false), Unit { tabs: false, width: 2, tab: 4 });
        let tabs = Rope::from_str("a {\n\tb\n\tc\n}\n");
        assert!(detect(&tabs, 4, false).tabs);
        assert_eq!(detect(&Rope::from_str("x\n"), 4, false).width, 4, "nothing says: tab width");
        assert!(detect(&Rope::from_str("x\n"), 4, true).tabs, "…or tabs where the language wants them");
        assert!(!detect(&two, 4, true).tabs, "the file says otherwise");
        let t = Rope::from_str("  call(\n");
        let u = Unit { tabs: false, width: 2, tab: 4 };
        assert_eq!(heuristic(&t, t.len_bytes() - 1, u, false, true), "    ");
        let t = Rope::from_str("  if x:\n");
        assert_eq!(heuristic(&t, t.len_bytes() - 1, u, true, true), "    ");
        assert_eq!(heuristic(&t, t.len_bytes() - 1, u, false, true), "  ");
        // The closer moving down lines up with the opener's line; the line between goes in
        let t = Rope::from_str("  x = []\n");
        let at = t.len_bytes() - 2;
        assert_eq!(heuristic(&t, at, u, false, true), "  ");
        assert_eq!(heuristic(&t, at, u, false, false), "    ");
        // From a blank line under an opener: still in
        let t = Rope::from_str("a {\n\n");
        assert_eq!(heuristic(&t, t.len_bytes() - 1, u, false, true), "  ");
    }
}
