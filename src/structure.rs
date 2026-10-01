//! Moving by syntax (tree-sitter), as in helix: `A-o`/`A-i` grow/shrink the selection one node, `A-n`/`A-p`
//! select the next/previous sibling, `]f` `[f` (and `t` `a` `c` `T`) select the next/previous textobject
//! from `textobjects.scm`. Pure — tree + text + range in, range out.

use ropey::Rope;
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

use crate::movement::Direction;
use crate::selection::Range;

/// The selection as a byte span — a cursor counts as the char under it.
fn span(text: &Rope, r: Range) -> (usize, usize) {
    let r = r.min_width_1(text);
    (r.from(), r.to())
}

/// Same direction as `r`.
fn like(r: Range, from: usize, to: usize) -> Range {
    if r.is_forward() { Range::new(from, to) } else { Range::new(to, from) }
}

/// Smallest named node covering the span.
fn covering(tree: &Tree, (from, to): (usize, usize)) -> Option<Node<'_>> {
    tree.root_node().named_descendant_for_byte_range(from, to)
}

/// `A-o` — the smallest named node strictly larger than the selection.
pub fn expand(tree: &Tree, text: &Rope, r: Range) -> Option<Range> {
    let (from, to) = span(text, r);
    let mut node = covering(tree, (from, to))?;
    while node.start_byte() == from && node.end_byte() == to {
        node = node.parent()?;
    }
    Some(like(r, node.start_byte(), node.end_byte()))
}

/// `A-i` without history — the first named child inside the selection's node.
pub fn shrink(tree: &Tree, text: &Rope, r: Range) -> Option<Range> {
    let node = covering(tree, span(text, r))?;
    let child = node.named_child(0)?;
    Some(like(r, child.start_byte(), child.end_byte()))
}

/// `A-n` / `A-p` — the next/previous named sibling of the selection's node (climbing when it has none).
pub fn sibling(tree: &Tree, text: &Rope, r: Range, dir: Direction) -> Option<Range> {
    let mut node = covering(tree, span(text, r))?;
    loop {
        let next = match dir {
            Direction::Forward => node.next_named_sibling(),
            Direction::Backward => node.prev_named_sibling(),
        };
        match next {
            Some(n) => return Some(like(r, n.start_byte(), n.end_byte())),
            None => node = node.parent()?,
        }
    }
}

/// `]f` / `[f` … — forward: the first `<name>.around` starting after the cursor (the outermost of those
/// starting there); backward: the last one ending before it, selected head-first.
pub fn textobject(
    query: &Query,
    tree: &Tree,
    text: &Rope,
    r: Range,
    name: &str,
    dir: Direction,
) -> Option<Range> {
    textobject_within(query, tree, text, r, name, dir, 64 * 1024)
}

/// `textobject`, looked for in a window from the cursor that grows (×8, from `window` bytes) until it holds
/// one — the query over a whole big file took 90 ms a key. Each step looks only at what the last didn't.
/// (A match is whole even where the window cuts it: a run of comments comes back with all its lines.)
fn textobject_within(
    query: &Query,
    tree: &Tree,
    text: &Rope,
    r: Range,
    name: &str,
    dir: Direction,
    mut window: usize,
) -> Option<Range> {
    let caps = [format!("{name}.around"), format!("{name}.movement")];
    let ids: Vec<u32> = query
        .capture_names()
        .iter()
        .enumerate()
        .filter(|(_, n)| caps.iter().any(|c| c == *n))
        .map(|(i, _)| i as u32)
        .collect();
    if ids.is_empty() {
        return None;
    }
    let (pos, len) = (r.cursor(text), text.len_bytes());
    // The objects starting after the cursor start in the window if any does (those ending before it end
    // in it) — `done` is how far from the cursor has been looked through
    let mut done = 0;
    loop {
        let reach = window.min(len);
        let found = match dir {
            Direction::Forward => {
                let range = (pos + done).min(len)..pos.saturating_add(reach).min(len);
                objects_in(query, &ids, tree, text, range)
                    .into_iter()
                    .filter(|&(s, _)| s > pos)
                    .min_by_key(|&(s, e)| (s, std::cmp::Reverse(e)))
                    .map(|(s, e)| Range::new(s, e))
            }
            Direction::Backward => {
                let range = pos.saturating_sub(reach)..pos.saturating_sub(done);
                objects_in(query, &ids, tree, text, range)
                    .into_iter()
                    .filter(|&(_, e)| e <= pos)
                    .max_by_key(|&(s, e)| (e, std::cmp::Reverse(s)))
                    .map(|(s, e)| Range::new(e, s))
            }
        };
        let whole = match dir {
            Direction::Forward => pos.saturating_add(reach) >= len,
            Direction::Backward => reach >= pos,
        };
        if found.is_some() || whole {
            return found;
        }
        done = reach;
        window = window.saturating_mul(8);
    }
}

/// The objects (start, end) of captures `ids` that meet `range`.
fn objects_in(
    query: &Query,
    ids: &[u32],
    tree: &Tree,
    text: &Rope,
    range: std::ops::Range<usize>,
) -> Vec<(usize, usize)> {
    // (a node's text for predicates — ends snapped to chars: the tree may be a beat behind the text)
    let provider = |node: Node| crate::syntax::node_slice(text, node).chunks().map(str::as_bytes);
    // A capture may be several nodes (quantified) — one object spans them all
    let mut objects: Vec<(usize, usize)> = Vec::new();
    if range.is_empty() {
        return objects; // (an empty range would mean no limit to tree-sitter)
    }
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range);
    let mut matches = cursor.matches(query, tree.root_node(), provider);
    while let Some(m) = matches.next() {
        let mut obj: Option<(usize, usize)> = None;
        for c in m.captures().iter().filter(|c| ids.contains(&c.index)) {
            let (s, e) = (c.node.start_byte(), c.node.end_byte());
            obj = Some(obj.map_or((s, e), |(a, b)| (a.min(s), b.max(e))));
        }
        objects.extend(obj);
    }
    objects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Loader, Syntax, spec};

    /// Rust tree + textobjects, or None when the grammar isn't built in.
    fn rust(src: &str) -> Option<(Rope, Tree, std::sync::Arc<crate::syntax::LangData>)> {
        let lang = Loader::global().load(spec("rust")?).ok()?;
        let text = Rope::from_str(src);
        let mut syn = Syntax::new(lang.clone());
        let job = syn.start_parse(&text);
        let generation = job.generation;
        let tree = job.run();
        syn.finish_parse(generation, tree);
        Some((text, syn.tree.clone()?, lang))
    }

    const SRC: &str = "fn a(x: u8, y: u8) {\n    call(x);\n}\n\n// note\nfn b() {}\n";

    #[test]
    fn expand_and_shrink_by_nodes() {
        let Some((t, tree, _)) = rust(SRC) else { return };
        let x = SRC.find("x)").unwrap(); // the `x` in call(x)
        let r = expand(&tree, &t, Range::point(x)).unwrap();
        assert_eq!(&SRC[r.from()..r.to()], "(x)", "identifier → its argument list");
        let r = expand(&tree, &t, r).unwrap();
        assert_eq!(&SRC[r.from()..r.to()], "call(x)");
        let r = shrink(&tree, &t, r).unwrap();
        assert_eq!(&SRC[r.from()..r.to()], "call", "first child");
    }

    #[test]
    fn siblings_step_and_climb() {
        let Some((t, tree, _)) = rust(SRC) else { return };
        let x = SRC.find("x:").unwrap();
        let p = expand(&tree, &t, Range::point(x)).unwrap();
        assert_eq!(&SRC[p.from()..p.to()], "x: u8");
        let n = sibling(&tree, &t, p, Direction::Forward).unwrap();
        assert_eq!(&SRC[n.from()..n.to()], "y: u8");
        let back = sibling(&tree, &t, n, Direction::Backward).unwrap();
        assert_eq!(back, p);
    }

    /// Looking in a small window that grows finds what a query over the whole file does — from every
    /// position, both ways, with runs of comments (a quantified capture the window's edge cuts).
    #[test]
    fn a_growing_window_finds_what_the_whole_file_does() {
        let mut src = String::new();
        for i in 0..30 {
            src.push_str(&format!(
                "// about {i}\n// more {i}\n// and more\nfn f{i}(x: u8) {{\n    g(x);\n}}\n"
            ));
            if i % 7 == 0 {
                src.push_str("\n/// doc\n/// doc\nstruct S;\n");
            }
        }
        let Some((t, tree, lang)) = rust(&src) else { return };
        let Some(q) = lang.textobjects.as_ref() else { return };
        for name in ["function", "comment", "class"] {
            let caps = [format!("{name}.around"), format!("{name}.movement")];
            let ids: Vec<u32> = (0..q.capture_names().len() as u32)
                .filter(|&i| caps.iter().any(|c| c == q.capture_names()[i as usize]))
                .collect();
            let all = objects_in(q, &ids, &tree, &t, 0..t.len_bytes());
            for pos in 0..t.len_bytes() {
                for dir in [Direction::Forward, Direction::Backward] {
                    let r = Range::point(pos);
                    let whole = match dir {
                        Direction::Forward => all
                            .iter()
                            .filter(|o| o.0 > pos)
                            .min_by_key(|o| (o.0, std::cmp::Reverse(o.1)))
                            .map(|o| Range::new(o.0, o.1)),
                        Direction::Backward => all
                            .iter()
                            .filter(|o| o.1 <= pos)
                            .max_by_key(|o| (o.1, std::cmp::Reverse(o.0)))
                            .map(|o| Range::new(o.1, o.0)),
                    };
                    let small = textobject_within(q, &tree, &t, r, name, dir, 16);
                    assert_eq!(small, whole, "{name} {dir:?} from {pos}");
                }
            }
        }
    }

    #[test]
    fn next_and_previous_textobjects() {
        let Some((t, tree, lang)) = rust(SRC) else { return };
        let Some(q) = lang.textobjects.as_ref() else { return };
        let r = textobject(q, &tree, &t, Range::point(0), "function", Direction::Forward).unwrap();
        assert_eq!(&SRC[r.from()..r.to()], "fn b() {}", "the one after the cursor");
        let end = SRC.len() - 1;
        let r = textobject(q, &tree, &t, Range::point(end), "function", Direction::Backward).unwrap();
        assert_eq!(&SRC[r.from()..r.to()], "fn b() {}");
        assert!(r.head < r.anchor, "backward selects head-first");
        let c = textobject(q, &tree, &t, Range::point(0), "comment", Direction::Forward).unwrap();
        assert_eq!(&SRC[c.from()..c.to()], "// note");
        assert!(textobject(q, &tree, &t, Range::point(end), "function", Direction::Forward).is_none());
    }
}
