//! A page's tree, tarae's own (the user's 2026-10-23: only html5ever and encoding_rs): html5ever parses — the WHATWG
//! algorithm, scripting off as a browser without JavaScript (`<noscript>` is elements) — into an arena through its
//! `TreeSink`, each node its parent, its siblings and its first and last child, so every change the parser makes is
//! a few links set. What the reader asks of it and nothing more: children, ancestors, descendants, a subtree's
//! text, a name, an attribute; no selectors, no mutation, no serializer.
//!
//! Fed a piece at a time, and no deeper than the reader reads: html5ever's tree building walks its stack of open
//! elements for most tags (the spec's "in scope" checks), so a page nested n deep costs n² (measured: 20,000 nested
//! `<ul><li>` took 17 s; an 8 MB page of them would take hours). Each node's depth is counted as it's put in the
//! tree; once one is past the limit, the rest of the page isn't fed (Chrome's parser stops nesting at 512 too). The
//! count follows where nodes go (a template's contents at its depth), not where the adoption agency moves them
//! later — the converter's walk keeps the exact limit. Every walk here is a loop, never a recursion.

use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};
use std::sync::LazyLock;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeBuilderOpts, TreeSink};
use html5ever::{Attribute, ParseOpts, QualName, local_name, ns, parse_document};

/// A node: its place in the arena.
pub type Id = usize;

/// What's fed at a time: how far a page can nest past the limit before it's seen is one piece's worth of tags.
const PIECE: usize = 8 << 10;

enum Data {
    Document,
    /// A template's contents: in no walk (the converter never reads a template)
    Fragment,
    Element {
        name: QualName,
        attrs: Vec<Attribute>,
        /// A template's contents
        template: Option<Id>,
        /// MathML's `annotation-xml` holding HTML (the parser asks)
        integration: bool,
    },
    Text(StrTendril),
    /// A comment, a doctype's place, a processing instruction: kept as a place (text on either side of one stays
    /// two texts, as in the parser's DOM), nothing of it read
    Other,
}

struct Node {
    parent: Option<Id>,
    prev: Option<Id>,
    next: Option<Id>,
    first: Option<Id>,
    last: Option<Id>,
    depth: u32,
    data: Data,
}

impl Node {
    fn new(data: Data) -> Node {
        Node { parent: None, prev: None, next: None, first: None, last: None, depth: 0, data }
    }
}

pub struct Tree {
    nodes: Vec<Node>,
}

pub struct Parsed {
    pub tree: Tree,
    /// It nested past the limit: the page after that point wasn't read
    pub stopped: bool,
}

/// `text` parsed until it nests past `depth_max`.
pub fn parse(text: &str, depth_max: usize) -> Parsed {
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts { scripting_enabled: false, ..Default::default() },
        ..Default::default()
    };
    let mut parser = parse_document(Sink::default(), opts);
    let (mut rest, mut stopped) = (text, false);
    while !rest.is_empty() {
        let mut at = rest.len().min(PIECE);
        while !rest.is_char_boundary(at) {
            at += 1;
        }
        let (piece, after) = rest.split_at(at);
        parser.process(StrTendril::from_slice(piece));
        rest = after;
        if parser.tokenizer.sink.sink.most.get() > depth_max {
            stopped = !rest.is_empty();
            break;
        }
    }
    Parsed { tree: parser.finish(), stopped }
}

impl Tree {
    /// The document.
    pub const ROOT: Id = 0;

    fn node(&self, id: Id) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// `id`'s children, in order.
    pub fn children(&self, id: Id) -> impl Iterator<Item = Id> + '_ {
        std::iter::successors(self.node(id).and_then(|n| n.first), |&c| self.node(c).and_then(|n| n.next))
    }

    /// `id`'s children that are elements.
    pub fn elements(&self, id: Id) -> impl Iterator<Item = Id> + '_ {
        self.children(id).filter(|&c| self.name(c).is_some())
    }

    pub fn parent(&self, id: Id) -> Option<Id> {
        self.node(id)?.parent
    }

    /// What holds `id`, nearest first, `max` of them at most.
    pub fn up(&self, id: Id, max: usize) -> impl Iterator<Item = Id> + '_ {
        let parent = |i: Id| self.node(i).and_then(|n| n.parent);
        std::iter::successors(parent(id), move |&p| parent(p)).take(max)
    }

    /// What's in `id`, in document order (not `id` itself) — a walk by the links, no stack.
    pub fn walk(&self, id: Id) -> Walk<'_> {
        Walk { tree: self, root: id, next: self.node(id).and_then(|n| n.first) }
    }

    /// The text of `id` and all it holds, as written.
    pub fn text(&self, id: Id) -> String {
        let mut s = String::new();
        for t in std::iter::once(id).chain(self.walk(id)).filter_map(|n| self.own_text(n)) {
            s.push_str(t);
        }
        s
    }

    /// A text node's text.
    pub fn own_text(&self, id: Id) -> Option<&str> {
        match &self.node(id)?.data {
            Data::Text(t) => Some(t),
            _ => None,
        }
    }

    /// An element's name (its local name: `svg`'s `foreignObject` as written); none for what isn't one.
    pub fn name(&self, id: Id) -> Option<&str> {
        match &self.node(id)?.data {
            Data::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    /// An element's attribute, by its local name.
    pub fn attr(&self, id: Id, name: &str) -> Option<&str> {
        match &self.node(id)?.data {
            Data::Element { attrs, .. } => attrs.iter().find(|a| &*a.name.local == name).map(|a| &*a.value),
            _ => None,
        }
    }

    pub fn has(&self, id: Id, name: &str) -> bool {
        self.attr(id, name).is_some()
    }

    /// The document or a fragment: read for what it holds.
    pub fn holds_only(&self, id: Id) -> bool {
        self.node(id).is_some_and(|n| matches!(n.data, Data::Document | Data::Fragment))
    }

    /// `<html>`'s `<body>`.
    pub fn body(&self) -> Option<Id> {
        let html = self.elements(Tree::ROOT).find(|&e| self.name(e) == Some("html"))?;
        self.elements(html).find(|&e| self.name(e) == Some("body"))
    }

    /// The elements of the page named `name`, in document order.
    pub fn named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = Id> + 'a {
        self.walk(Tree::ROOT).filter(move |&n| self.name(n) == Some(name))
    }

    /// How many nodes: every id is below it.
    pub fn count(&self) -> usize {
        self.nodes.len()
    }
}

/// `Tree::walk`'s: down to the first child, else on to the next sibling, else up until one has one — never past the
/// node it started from.
pub struct Walk<'a> {
    tree: &'a Tree,
    root: Id,
    next: Option<Id>,
}

impl Iterator for Walk<'_> {
    type Item = Id;

    fn next(&mut self) -> Option<Id> {
        let at = self.next?;
        let node = self.tree.node(at)?;
        self.next = node.first.or_else(|| {
            let mut n = at;
            loop {
                if n == self.root {
                    return None;
                }
                let here = self.tree.node(n)?;
                if here.next.is_some() {
                    return here.next;
                }
                n = here.parent?;
            }
        });
        Some(at)
    }
}

/// The name for a handle that isn't an element's (the parser never asks one).
static NAMELESS: LazyLock<QualName> = LazyLock::new(|| QualName::new(None, ns!(), local_name!("")));

/// The arena as the parser builds it, each node's depth beside it.
struct Sink {
    nodes: RefCell<Vec<Node>>,
    most: Cell<usize>,
}

impl Default for Sink {
    fn default() -> Sink {
        Sink { nodes: RefCell::new(vec![Node::new(Data::Document)]), most: Cell::new(0) }
    }
}

impl Sink {
    fn add(&self, data: Data) -> Id {
        let mut n = self.nodes.borrow_mut();
        n.push(Node::new(data));
        n.len() - 1
    }

    fn depth(&self, id: Id) -> u32 {
        self.nodes.borrow().get(id).map_or(0, |n| n.depth)
    }

    fn parent(&self, id: Id) -> Option<Id> {
        self.nodes.borrow().get(id).and_then(|n| n.parent)
    }

    /// `child` put in `parent` before `before` (at the end without one), at `depth`; text joins a text before it.
    fn put(&self, parent: Id, before: Option<Id>, child: NodeOrText<Id>, depth: u32) {
        let mut nodes = self.nodes.borrow_mut();
        let prev = match before {
            Some(b) => nodes.get(b).and_then(|n| n.prev),
            None => nodes.get(parent).and_then(|n| n.last),
        };
        let id = match child {
            NodeOrText::AppendNode(id) => {
                detach(&mut nodes, id);
                id
            }
            NodeOrText::AppendText(t) => {
                if let Some(Node { data: Data::Text(s), .. }) = prev.and_then(|p| nodes.get_mut(p)) {
                    s.push_tendril(&t);
                    return;
                }
                nodes.push(Node::new(Data::Text(t)));
                nodes.len() - 1
            }
        };
        // (detached, `before`'s previous may have moved: read again)
        let prev = match before {
            Some(b) => nodes.get(b).and_then(|n| n.prev),
            None => nodes.get(parent).and_then(|n| n.last),
        };
        let contents = match nodes.get_mut(id) {
            Some(n) => {
                (n.parent, n.prev, n.next, n.depth) = (Some(parent), prev, before, depth);
                match n.data {
                    Data::Element { template, .. } => template,
                    _ => None,
                }
            }
            None => return,
        };
        if let Some(c) = contents.and_then(|c| nodes.get_mut(c)) {
            c.depth = depth;
        }
        match prev.and_then(|p| nodes.get_mut(p)) {
            Some(p) => p.next = Some(id),
            None => {
                if let Some(p) = nodes.get_mut(parent) {
                    p.first = Some(id);
                }
            }
        }
        match before.and_then(|b| nodes.get_mut(b)) {
            Some(b) => b.prev = Some(id),
            None => {
                if let Some(p) = nodes.get_mut(parent) {
                    p.last = Some(id);
                }
            }
        }
        self.most.set(self.most.get().max(depth as usize));
    }
}

/// `id` out of its parent's children, its links to them cleared.
fn detach(nodes: &mut [Node], id: Id) {
    let Some(n) = nodes.get_mut(id) else { return };
    let (parent, prev, next) = (n.parent.take(), n.prev.take(), n.next.take());
    match prev.and_then(|p| nodes.get_mut(p)) {
        Some(p) => p.next = next,
        None => {
            if let Some(p) = parent.and_then(|p| nodes.get_mut(p)) {
                p.first = next;
            }
        }
    }
    match next.and_then(|x| nodes.get_mut(x)) {
        Some(x) => x.prev = prev,
        None => {
            if let Some(p) = parent.and_then(|p| nodes.get_mut(p)) {
                p.last = prev;
            }
        }
    }
}

impl TreeSink for Sink {
    type Handle = Id;
    type Output = Tree;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Tree {
        Tree { nodes: self.nodes.into_inner() }
    }

    fn parse_error(&self, _: Cow<'static, str>) {}

    fn get_document(&self) -> Id {
        Tree::ROOT
    }

    fn elem_name<'a>(&'a self, target: &'a Id) -> Ref<'a, QualName> {
        Ref::map(self.nodes.borrow(), |n| match n.get(*target).map(|n| &n.data) {
            Some(Data::Element { name, .. }) => name,
            _ => &NAMELESS,
        })
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Id {
        let template = flags.template.then(|| self.add(Data::Fragment));
        let integration = flags.mathml_annotation_xml_integration_point;
        self.add(Data::Element { name, attrs, template, integration })
    }

    fn create_comment(&self, _: StrTendril) -> Id {
        self.add(Data::Other)
    }

    fn create_pi(&self, _: StrTendril, _: StrTendril) -> Id {
        self.add(Data::Other)
    }

    fn append(&self, parent: &Id, child: NodeOrText<Id>) {
        self.put(*parent, None, child, self.depth(*parent).saturating_add(1))
    }

    fn append_based_on_parent_node(&self, element: &Id, prev: &Id, child: NodeOrText<Id>) {
        // (beside `element` when it has a parent, else in `prev`: the depth the deeper of the two)
        let depth = self.depth(*element).max(self.depth(*prev).saturating_add(1));
        match self.parent(*element) {
            Some(p) => self.put(p, Some(*element), child, depth),
            None => self.put(*prev, None, child, depth),
        }
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {
        let id = self.add(Data::Other);
        self.put(Tree::ROOT, None, NodeOrText::AppendNode(id), 1)
    }

    fn get_template_contents(&self, target: &Id) -> Id {
        match self.nodes.borrow().get(*target).map(|n| &n.data) {
            Some(Data::Element { template: Some(c), .. }) => *c,
            _ => *target,
        }
    }

    fn same_node(&self, x: &Id, y: &Id) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &Id, child: NodeOrText<Id>) {
        if let Some(p) = self.parent(*sibling) {
            self.put(p, Some(*sibling), child, self.depth(*sibling))
        }
    }

    fn add_attrs_if_missing(&self, target: &Id, attrs: Vec<Attribute>) {
        if let Some(Node { data: Data::Element { attrs: have, .. }, .. }) =
            self.nodes.borrow_mut().get_mut(*target)
        {
            for a in attrs {
                if !have.iter().any(|h| h.name == a.name) {
                    have.push(a)
                }
            }
        }
    }

    fn associate_with_form(&self, _: &Id, _: &Id, _: (&Id, Option<&Id>)) {}

    fn remove_from_parent(&self, target: &Id) {
        detach(&mut self.nodes.borrow_mut(), *target)
    }

    fn reparent_children(&self, node: &Id, new_parent: &Id) {
        // (each keeps its depth, as counted where it was put)
        loop {
            let first = self.nodes.borrow().get(*node).and_then(|n| n.first);
            let Some(c) = first else { break };
            self.put(*new_parent, None, NodeOrText::AppendNode(c), self.depth(c))
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Id) -> bool {
        matches!(
            self.nodes.borrow().get(*handle).map(|n| &n.data),
            Some(Data::Element { integration: true, .. })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(t: &Tree, ids: impl Iterator<Item = Id>) -> Vec<String> {
        ids.map(|i| t.name(i).map_or_else(|| format!("{:?}", t.own_text(i).unwrap_or("")), String::from))
            .collect()
    }

    #[test]
    fn the_tree_is_the_parsers_dom() {
        let t =
            parse("<!doctype html><title>T</title><p id=a>one<!-- c -->two<b>three<p>four</b>five", 512).tree;
        let body = t.body().unwrap();
        assert_eq!(names(&t, t.children(body)), ["p", "p"]);
        assert_eq!(
            names(&t, t.walk(body)),
            ["p", "\"one\"", "\"\"", "\"two\"", "b", "\"three\"", "p", "b", "\"four\"", "\"five\""]
        );
        assert_eq!(t.text(body), "onetwothreefourfive");
        let p = t.named("p").next().unwrap();
        assert_eq!((t.attr(p, "id"), t.has(p, "id"), t.has(p, "class")), (Some("a"), true, false));
        let b = t.named("b").nth(1).unwrap();
        assert_eq!(names(&t, t.up(b, 3)), ["p", "body", "html"]);
        assert_eq!(t.up(b, 99).count(), 4);
        assert_eq!(t.text(t.named("title").next().unwrap()), "T");
    }

    #[test]
    fn a_tables_stray_text_goes_before_it_and_joins() {
        // (foster parenting: text before the table, through append_based_on_parent_node)
        let t = parse("<div>a<table>b<tr><td>c</td></tr>d</table></div>", 512).tree;
        let div = t.named("div").next().unwrap();
        assert_eq!(names(&t, t.children(div)), ["\"abd\"", "table"]);
    }

    #[test]
    fn a_templates_contents_are_in_no_walk() {
        let t = parse("<body><template><p>inside</p></template>out", 512).tree;
        assert_eq!(t.text(t.body().unwrap()), "out");
        assert!(t.named("p").next().is_none());
    }

    #[test]
    fn the_parse_stops_past_the_depth() {
        let page = format!("{}deep", "<div>".repeat(30_000));
        let p = parse(&page, 512);
        assert!(p.stopped);
        assert!(p.tree.count() < 30_000, "{}", p.tree.count());
        assert!(!parse(&"<div>".repeat(100), 512).stopped);
    }
}
