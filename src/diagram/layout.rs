//! The cells layout (the diagram plan's P3, its §8–9; the drawings `target/sb/diagram-cells-2.txt` and `-3.txt`):
//! a diagram in style A, folded to its room in the user's order. Pure — lines of styled text; the view is P4's.
//!
//! - **The spine** runs down each scope from its start. A way out of the node in hand hangs as a side branch
//!   (`├─gist─▶ ● name`, its own ways under its `●`) when all it leads to is the spine's next node, another side
//!   branch of that node, an end or back up, and it isn't the scope's way out — the spine takes the farthest next node that leaves the others so
//!   (`run card` and `judged` hang off `asked`, `admitted` is next); else the nearest (the keeper's chain stays
//!   on the spine). Sides leading to each other go in that order, so their rails don't interleave.
//! - **Words**: an end (a node no way leaves) is a word after `▶` at each place it's reached from; so are a way
//!   to itself (`itself`), into the next run (`↑ name · next run`) and to the diagram's end (`end`).
//! - **Inline**: a row takes its node's first way out when nothing comes in on it, no label row follows it (a
//!   composite's own ways go on its title row) and that way is its only one, a word or back up; the rest go
//!   under it, `├` while more follow, `╰` last.
//! - **Rails**: every other way — one rail per target, `◀` on the target's row, each way's label on its
//!   source's row. A rail hugs what it passes (a cell after text, two after another rail); the inner of two is
//!   the one whose crossings cost least; a node's rails go in the diagram's order or nested (ways up nearest
//!   first, ways down farthest first), whichever crosses less; a crossing is a hop (the rail's `│` runs through,
//!   never `┼`); two rails never end and begin in one column on adjacent rows.
//! - **Composites**: an indent behind `╎`; their own ways on the title row; regions that run at once
//!   parted by `┄ at once ┄`. A second way in that only leads to one node rides on its row (`◀─ ▸● name`).
//! - **Folds** (§8.3), each kept only if it narrows — width: the anchor column → gists cut to `GIST_FLOOR` →
//!   rails as `↑`/`↓` references, outermost first → gists below the floor → names; height: label rows move to the
//!   end of their node's text (`↓ y`) while the drawing stays in the room's width, nodes without branches first
//!   — then the view scrolls. The selected node's names and gists are never cut.

use std::collections::{HashMap, HashSet};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::Diagram;
use crate::cut::fit_ellipsis;
use crate::sorting;

/// How short gists get before rails fold into references (§8.3: "a floor (~6)").
pub const GIST_FLOOR: usize = 6;
/// How short names get, last.
const NAME_FLOOR: usize = 4;
/// Cells between the drawing and its anchor column.
const ANCHOR_GAP: usize = 3;
/// Between regions that run at the same time ("and" would read as "then").
const AT_ONCE: &str = "┄ at once ┄";

/// What a node's code says of it: `●` tied, `◦` untied (no anchor, or one in more than one place), `▲` changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Tied,
    Untied,
    Changed,
}

impl Mark {
    fn glyph(self) -> &'static str {
        match self {
            Mark::Tied => "●",
            Mark::Untied => "◦",
            Mark::Changed => "▲",
        }
    }

    fn anchor_glyph(self) -> &'static str {
        match self {
            Mark::Tied => "ƒ",
            Mark::Untied => "◦",
            Mark::Changed => "▲",
        }
    }
}

/// How a node shows (one per `Diagram::nodes`): its mark and its anchor as the column says it
/// (`Editor::rebase_run`, `keeper::child · 2 places`) — the view's to make from the checks.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub mark: Mark,
    pub anchor: Option<String>,
}

/// What a span is, for the view's colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Title,
    Faint,
    Name,
    Selected,
    Composite,
    Mark(Mark),
    Gist,
    Word,
    Line,
    Way,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub spans: Vec<(String, Role)>,
    /// The nodes the row draws: its own (a branch's node on a side row), then an entry riding on it.
    pub nodes: Vec<usize>,
}

impl Line {
    pub fn text(&self) -> String {
        self.spans.iter().map(|(s, _)| s.as_str()).collect()
    }
}

/// A fold the drawing took to fit, in the order taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    Anchors,
    /// Gists cut to this many cells.
    Gists(usize),
    /// This many rails drawn as references.
    Refs(usize),
    /// Names cut to this many cells.
    Names(usize),
    /// This many label rows moved to their node's row.
    Moved(usize),
}

#[derive(Clone, Copy, Debug)]
pub struct Room {
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug)]
pub struct Drawing {
    /// The title row first.
    pub lines: Vec<Line>,
    /// The drawing's own width (the title row spans the room).
    pub width: usize,
    pub folds: Vec<Fold>,
}

impl Drawing {
    pub fn text(&self) -> String {
        self.lines.iter().map(Line::text).collect::<Vec<_>>().join("\n")
    }
}

/// `d` drawn for `room`, folded as it needs; `looks` one per node, `anchors` the column toggled on.
pub fn draw(d: &Diagram, looks: &[Look], selected: Option<usize>, anchors: bool, room: Room) -> Drawing {
    let g = Graph::new(d, looks, selected);
    let fits = |l: &Lay| l.width <= room.width;
    let mut f = Folds { anchors, ..Folds::default() };
    let mut lay = g.lay(&f, room.width);
    let mut folds = Vec::new();
    if !fits(&lay) && f.anchors {
        f.anchors = false;
        lay = g.lay(&f, room.width);
        folds.push(Fold::Anchors);
    }
    let longest = (0..d.edges.len()).map(|i| d.says(&d.edges[i]).width()).max().unwrap_or(0);
    if !fits(&lay)
        && let Some(n) =
            g.shrink(&mut f, &mut lay, (GIST_FLOOR..longest).rev(), |f, n| f.gist = Some(n), room)
    {
        folds.push(Fold::Gists(n));
    }
    if !fits(&lay) {
        // Outermost first, one more each time: a single rail rarely narrows (the ones inside it stay), and a
        // reference names its target, so it can widen — the fewest that make it narrowest, if any narrows.
        let mut rails = lay.rails.clone();
        sorting::by_key(&mut rails, |&(_, col)| std::cmp::Reverse(col));
        let mut t = f.clone();
        let mut best: Option<(Folds, Lay, usize)> = None;
        for (n, (target, _)) in rails.into_iter().enumerate() {
            t.refs.insert(target);
            let l = g.lay(&t, room.width);
            let done = fits(&l);
            if l.width < best.as_ref().map_or(lay.width, |(_, b, _)| b.width) {
                best = Some((t.clone(), l, n + 1));
            }
            if done {
                break;
            }
        }
        if let Some((t, l, n)) = best {
            (f, lay) = (t, l);
            folds.push(Fold::Refs(n));
        }
    }
    let floor = f.gist.unwrap_or(GIST_FLOOR).min(longest);
    if !fits(&lay)
        && let Some(n) = g.shrink(&mut f, &mut lay, (1..floor).rev(), |f, n| f.gist = Some(n), room)
    {
        folds.push(Fold::Gists(n));
    }
    let longest = (0..d.nodes.len()).map(|n| d.nodes[n].label.width()).max().unwrap_or(0);
    if !fits(&lay)
        && let Some(n) =
            g.shrink(&mut f, &mut lay, (NAME_FLOOR..longest).rev(), |f, n| f.names = Some(n), room)
    {
        folds.push(Fold::Names(n));
    }
    if lay.height > room.height {
        let mut n = 0;
        for e in lay.labels.clone() {
            if lay.height <= room.height {
                break;
            }
            let mut t = f.clone();
            t.moved.insert(e);
            let l = g.lay(&t, room.width);
            if l.width <= room.width.max(lay.width) {
                (f, lay, n) = (t, l, n + 1);
            }
        }
        if n > 0 {
            folds.push(Fold::Moved(n));
        }
    }
    Drawing { lines: lay.lines, width: lay.width, folds }
}

/// What's folded.
#[derive(Clone, Default)]
struct Folds {
    anchors: bool,
    gist: Option<usize>,
    names: Option<usize>,
    /// Rails drawn as references, by their target.
    refs: HashSet<usize>,
    /// Label rows moved to their node's row, by their edge.
    moved: HashSet<usize>,
}

/// A layout: its lines (the title's first), its width and height, its rails (target, column) and the label rows
/// the height fold may move, in the order it tries them.
struct Lay {
    lines: Vec<Line>,
    width: usize,
    height: usize,
    /// Crossings: the cells where a run to a rail hops over another.
    hops: usize,
    rails: Vec<(usize, usize)>,
    labels: Vec<usize>,
}

/// The diagram's ids resolved, and what each node is.
struct Graph<'a> {
    d: &'a Diagram,
    looks: &'a [Look],
    selected: Option<usize>,
    from: Vec<Option<usize>>,
    to: Vec<Option<usize>>,
    within: Vec<Option<usize>>,
    /// Every id it names is a node.
    known: Vec<bool>,
    parent: Vec<Option<usize>>,
    kids: Vec<Vec<usize>>,
    /// No way leaves it: drawn as a word.
    end: Vec<bool>,
    /// Second ways in that may ride on their one target's row: entry → (target, its edge).
    entries: HashMap<usize, (usize, usize)>,
}

/// A scope's spines, and how each node hangs.
#[derive(Default)]
struct Plan {
    /// (composite or the top, region) → its spine, in segments (a segment begins where nothing above led).
    spines: HashMap<(Option<usize>, usize), Vec<Vec<usize>>>,
    sides: HashMap<usize, Vec<usize>>,
    /// A spine node → the edge drawn as the spine down to its next node.
    spine_edge: HashMap<usize, usize>,
    /// A side edge → the side node it hangs.
    side_edge: HashMap<usize, usize>,
    /// A node → the entry riding on its row.
    entries: HashMap<usize, usize>,
}

/// A way out drawn under (or beside) its node.
#[derive(Clone)]
enum Item {
    Side(usize, usize),
    /// An end, `itself`, `end` or a reference — and the end it names, for its anchor.
    Word {
        edge: usize,
        word: String,
        end: Option<usize>,
    },
    Rail(usize),
}

impl<'a> Graph<'a> {
    fn new(d: &'a Diagram, looks: &'a [Look], selected: Option<usize>) -> Self {
        let ids: HashMap<&str, usize> = d.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let id = |s: &Option<String>| s.as_deref().map(|s| ids.get(s).copied());
        let n = d.nodes.len();
        let mut parent = vec![None; n];
        let mut kids = vec![Vec::new(); n];
        for (i, node) in d.nodes.iter().enumerate() {
            if let Some(Some(p)) = id(&node.parent)
                && p != i
            {
                parent[i] = Some(p);
                kids[p].push(i);
            }
        }
        let (mut from, mut to, mut within, mut known) = (vec![], vec![], vec![], vec![]);
        for e in &d.edges {
            let (f, t, w) = (id(&e.from), id(&e.to), id(&e.within));
            known.push(![f, t, w].contains(&Some(None)));
            from.push(f.flatten());
            to.push(t.flatten());
            within.push(w.flatten());
        }
        let mut g = Graph {
            d,
            looks,
            selected,
            from,
            to,
            within,
            known,
            parent,
            kids,
            end: vec![false; n],
            entries: HashMap::new(),
        };
        let edges =
            |pick: &dyn Fn(usize) -> bool| (0..d.edges.len()).filter(|&i| g.known[i] && pick(i)).count();
        let end: Vec<bool> = (0..n)
            .map(|x| {
                g.kids[x].is_empty()
                    && edges(&|i| g.from[i] == Some(x)) == 0
                    && edges(&|i| g.to[i] == Some(x)) > 0
            })
            .collect();
        g.end = end;
        // Each scope's first way in — the diagram's, a composite's — is its start, not an entry.
        let mut firsts: HashMap<Option<usize>, usize> = HashMap::new();
        for i in 0..d.edges.len() {
            if g.known[i]
                && g.from[i].is_none()
                && let Some(t) = g.to[i]
            {
                firsts.entry(g.within[i]).or_insert(t);
            }
        }
        let starts: HashSet<usize> = firsts.into_values().collect();
        let mut taken = HashSet::new();
        for x in 0..n {
            let ins: Vec<usize> = (0..d.edges.len()).filter(|&i| g.known[i] && g.to[i] == Some(x)).collect();
            let outs: Vec<usize> =
                (0..d.edges.len()).filter(|&i| g.known[i] && g.from[i] == Some(x)).collect();
            if !starts.contains(&x)
                && g.kids[x].is_empty()
                && !ins.is_empty()
                && ins.iter().all(|&i| g.from[i].is_none())
                && let [o] = outs[..]
                && let Some(t) = g.to[o]
                && t != x
                && !g.end[t]
                && taken.insert(t)
            {
                g.entries.insert(x, (t, o));
            }
        }
        g
    }

    fn composite(&self, n: usize) -> bool {
        !self.kids[n].is_empty()
    }

    fn regions(&self, c: usize) -> Vec<usize> {
        let mut r: Vec<usize> = self.kids[c].iter().map(|&k| self.d.nodes[k].region).collect();
        sorting::sort(&mut r);
        r.dedup();
        r
    }

    /// The node of scope (`owner`, `region`) that is `n` or holds it.
    fn member(&self, owner: Option<usize>, region: usize, mut n: usize) -> Option<usize> {
        for _ in 0..=self.d.nodes.len() {
            if self.parent[n] == owner {
                return (owner.is_none() || self.d.nodes[n].region == region).then_some(n);
            }
            n = self.parent[n]?;
        }
        None
    }

    /// The node a composite's run ends in: its last region's last spine node, down to a leaf.
    fn deep_last(&self, plan: &Plan, n: usize) -> usize {
        if !self.composite(n) {
            return n;
        }
        let last = self.regions(n).last().and_then(|&r| plan.spines.get(&(Some(n), r)));
        last.and_then(|s| s.last()).and_then(|s| s.last()).map_or(n, |&m| self.deep_last(plan, m))
    }

    fn name(&self, f: &Folds, n: usize) -> String {
        let label = &self.d.nodes[n].label;
        match f.names {
            Some(w) if self.selected != Some(n) => fit_ellipsis(label, w),
            _ => label.clone(),
        }
    }

    /// What edge `i` says on the drawing, cut by the folds unless it's the selected node's.
    fn says(&self, f: &Folds, i: usize) -> String {
        let says = self.d.says(&self.d.edges[i]);
        let mine = self.selected.is_some() && (self.from[i] == self.selected || self.to[i] == self.selected);
        match f.gist {
            Some(w) if !mine => fit_ellipsis(says, w),
            _ => says.to_string(),
        }
    }

    /// A fold step by `set` through `steps` (widest first) until it fits — kept only if it narrowed: the step
    /// it stopped at.
    fn shrink(
        &self,
        f: &mut Folds,
        lay: &mut Lay,
        steps: impl Iterator<Item = usize>,
        set: impl Fn(&mut Folds, usize),
        room: Room,
    ) -> Option<usize> {
        let mut last = None;
        for n in steps {
            let mut t = f.clone();
            set(&mut t, n);
            last = Some((n, self.lay(&t, room.width), t));
            if last.as_ref().is_some_and(|(_, l, _)| l.width <= room.width) {
                break;
            }
        }
        let (n, l, t) = last?;
        (l.width < lay.width).then(|| {
            (*f, *lay) = (t, l);
            n
        })
    }

    /// Laid out with each node's rails in the diagram's order and nested (ways up nearest first, then ways
    /// down farthest first) — the one with fewer crossings; the diagram's order when even.
    fn lay(&self, f: &Folds, room: usize) -> Lay {
        let given = self.lay_in(f, room, false);
        let nested = self.lay_in(f, room, true);
        if nested.hops < given.hops { nested } else { given }
    }

    fn lay_in(&self, f: &Folds, room: usize, nest: bool) -> Lay {
        // Entries ride on their target's row unless a rail comes in there — then they're nodes of their own.
        let mut riding: HashSet<usize> = self.entries.keys().copied().collect();
        loop {
            let plan = self.plan(&riding);
            let items = self.items(&plan, f, nest);
            let targets: HashSet<usize> = items
                .values()
                .flatten()
                .filter_map(|it| if let Item::Rail(i) = it { self.to[*i] } else { None })
                .collect();
            let walk_off: Vec<usize> =
                riding.iter().copied().filter(|x| targets.contains(&self.entries[x].0)).collect();
            if walk_off.is_empty() {
                return Emit::lay(self, f, plan, items, targets, room);
            }
            for x in walk_off {
                riding.remove(&x);
            }
        }
    }

    fn plan(&self, riding: &HashSet<usize>) -> Plan {
        let mut plan = Plan::default();
        for &x in riding {
            plan.entries.insert(self.entries[&x].0, x);
        }
        self.plan_in(None, riding, &mut plan);
        plan
    }

    /// A scope's composites first (the edges into and out of them pick their deep ends), then its regions.
    fn plan_in(&self, owner: Option<usize>, riding: &HashSet<usize>, plan: &mut Plan) {
        let kids: Vec<usize> = match owner {
            None => (0..self.d.nodes.len()).filter(|&n| self.parent[n].is_none()).collect(),
            Some(c) => self.kids[c].clone(),
        };
        for &k in &kids {
            if self.composite(k) {
                self.plan_in(Some(k), riding, plan);
            }
        }
        let regions = owner.map_or(vec![0], |c| self.regions(c));
        for r in regions {
            self.plan_scope(owner, r, riding, plan);
        }
    }

    fn plan_scope(&self, owner: Option<usize>, region: usize, riding: &HashSet<usize>, plan: &mut Plan) {
        let members: Vec<usize> = (0..self.d.nodes.len())
            .filter(|&n| {
                self.parent[n] == owner
                    && (owner.is_none() || self.d.nodes[n].region == region)
                    && !self.end[n]
                    && !riding.contains(&n)
            })
            .collect();
        if members.is_empty() {
            return;
        }
        let member = |n: usize| self.member(owner, region, n).filter(|m| members.contains(m));
        // The scope's ways between its members (a, b, edge), out of it (a, edge), and in.
        let (mut pairs, mut exits, mut starts) = (Vec::new(), Vec::new(), Vec::new());
        for (i, e) in self.d.edges.iter().enumerate() {
            if !self.known[i] || e.next_run {
                continue;
            }
            match (self.from[i], self.to[i]) {
                (None, Some(t)) if self.within[i] == owner => starts.extend(member(t)),
                (Some(f), Some(t)) if t != f && !self.end[t] => {
                    let Some(a) = member(f) else { continue };
                    match self.member(owner, region, t) {
                        Some(b) if b != a && members.contains(&b) => pairs.push((a, b, i)),
                        None => exits.push(a),
                        _ => {}
                    }
                }
                (Some(f), None) if owner.is_some() && self.within[i] == owner => exits.extend(member(f)),
                _ => {}
            }
        }
        let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
        for &(a, b, _) in &pairs {
            let o = out.entry(a).or_default();
            if !o.contains(&b) {
                o.push(b);
            }
        }
        // Back edges: those a walk from the start meets on its own way.
        let mut state: HashMap<usize, u8> = HashMap::new();
        let mut back: HashSet<(usize, usize)> = HashSet::new();
        let mut post = Vec::new();
        for &r in starts.first().into_iter().chain(&members) {
            if !state.contains_key(&r) {
                dfs(r, &out, &mut state, &mut back, &mut post);
            }
        }
        let fwd = |a: usize| -> Vec<usize> {
            out.get(&a).into_iter().flatten().copied().filter(|&b| !back.contains(&(a, b))).collect()
        };
        // Layers: the longest way down from the start.
        let mut layer: HashMap<usize, usize> = members.iter().map(|&m| (m, 0)).collect();
        for &a in post.iter().rev() {
            for b in fwd(a) {
                let l = layer[&a] + 1;
                layer.entry(b).and_modify(|x| *x = (*x).max(l));
            }
        }
        let ins = |x: usize| -> Vec<usize> {
            pairs.iter().filter(|&&(a, b, _)| b == x && !back.contains(&(a, b))).map(|&(a, _, _)| a).collect()
        };
        let straight =
            |a: usize, b: usize| pairs.iter().find(|&&(p, q, i)| (p, q) == (a, b) && self.from[i] == Some(a));
        let mut placed: HashSet<usize> = HashSet::new();
        let mut segments = Vec::new();
        for &s0 in starts.first().into_iter().chain(&members) {
            if !placed.insert(s0) {
                continue;
            }
            let mut seg = vec![s0];
            let mut cur = s0;
            loop {
                let cands: Vec<usize> = fwd(cur).into_iter().filter(|b| !placed.contains(b)).collect();
                if cands.is_empty() {
                    break;
                }
                let mut by_layer = cands.clone();
                sorting::by_key(&mut by_layer, |b| std::cmp::Reverse(layer[b]));
                // None: the spine ends here — at the scope's way out, or with every way a side.
                let mut options: Vec<Option<usize>> = Vec::new();
                if exits.contains(&cur) || cands.len() >= 2 {
                    options.push(None);
                }
                options.extend(by_layer.iter().map(|&b| Some(b)));
                let side = |x: usize, t: Option<usize>, lower: &[usize]| {
                    !self.composite(x)
                        && straight(cur, x).is_some()
                        && fwd(x).iter().all(|y| Some(*y) == t || lower.contains(y) || placed.contains(y))
                        && !exits.contains(&x)
                        && ins(x).iter().all(|p| *p == cur || lower.contains(p))
                };
                let chosen = options.iter().find_map(|&t| {
                    let lower: Vec<usize> = cands
                        .iter()
                        .copied()
                        .filter(|&x| Some(x) != t && t.is_none_or(|t| layer[&x] <= layer[&t]))
                        .collect();
                    lower.iter().all(|&x| side(x, t, &lower)).then_some((t, lower))
                });
                let (t, lower) = chosen.unwrap_or((by_layer.last().copied(), Vec::new()));
                for x in lower {
                    placed.insert(x);
                    plan.sides.entry(cur).or_default().push(x);
                    if let Some(&(_, _, i)) = straight(cur, x) {
                        plan.side_edge.insert(i, x);
                    }
                }
                let Some(t) = t else { break };
                let ways: Vec<usize> =
                    pairs.iter().filter(|&&(a, b, _)| (a, b) == (cur, t)).map(|&(_, _, i)| i).collect();
                let last = self.deep_last(plan, cur);
                let way = ways
                    .iter()
                    .find(|&&i| self.from[i] == Some(cur))
                    .or_else(|| ways.iter().find(|&&i| self.from[i] == Some(last)))
                    .or(ways.first());
                if let Some(&i) = way {
                    plan.spine_edge.insert(cur, i);
                }
                placed.insert(t);
                seg.push(t);
                cur = t;
            }
            segments.push(seg);
        }
        plan.spines.insert((owner, region), segments);
    }

    /// The nodes in the order they're drawn.
    fn walk(&self, plan: &Plan, scope: (Option<usize>, usize), out: &mut Vec<usize>) {
        for seg in plan.spines.get(&scope).into_iter().flatten() {
            for &s in seg {
                out.push(s);
                out.extend(plan.entries.get(&s));
                if self.composite(s) {
                    for r in self.regions(s) {
                        self.walk(plan, (Some(s), r), out);
                    }
                }
                out.extend(plan.sides.get(&s).into_iter().flatten());
            }
        }
    }

    /// Every drawn node's ways out but its spine's, in the diagram's order — sides that lead to each other in
    /// that order.
    fn items(&self, plan: &Plan, f: &Folds, nest: bool) -> HashMap<usize, Vec<Item>> {
        let mut order = Vec::new();
        self.walk(plan, (None, 0), &mut order);
        let at: HashMap<usize, usize> = order.iter().enumerate().map(|(k, &n)| (n, k)).collect();
        let spine: HashSet<usize> = plan.spine_edge.values().copied().collect();
        let riding: HashSet<usize> = plan.entries.values().map(|x| self.entries[x].1).collect();
        let mut all = HashMap::new();
        for &n in &order {
            let mut items = Vec::new();
            for (i, e) in self.d.edges.iter().enumerate() {
                if !self.known[i] || self.from[i] != Some(n) || spine.contains(&i) || riding.contains(&i) {
                    continue;
                }
                if let Some(&x) = plan.side_edge.get(&i) {
                    items.push(Item::Side(x, i));
                    continue;
                }
                let word = |word: String, end| Item::Word { edge: i, word, end };
                items.push(match self.to[i] {
                    None => {
                        let implicit = self.within[i].is_some_and(|c| self.deep_last(plan, c) == n);
                        if implicit || self.d.says(e).is_empty() {
                            continue;
                        }
                        word("end".into(), None)
                    }
                    Some(t) if t == n => word("itself".into(), None),
                    Some(t) if self.end[t] => word(self.name(f, t), Some(t)),
                    Some(t) if e.next_run => word(format!("↑ {} · next run", self.name(f, t)), None),
                    Some(t) if f.refs.contains(&t) || !at.contains_key(&t) => {
                        let up = at.get(&t).is_none_or(|&k| k <= at[&n]);
                        word(format!("{} {}", if up { "↑" } else { "↓" }, self.name(f, t)), None)
                    }
                    Some(_) => Item::Rail(i),
                });
            }
            // Sides that lead to each other in that order (the run's judge above its card: else their rails
            // interleave), the rest where they were.
            let slots: Vec<usize> =
                (0..items.len()).filter(|&k| matches!(items[k], Item::Side(..))).collect();
            let mut sides: Vec<Item> = slots.iter().map(|&k| items[k].clone()).collect();
            let node = |it: &Item| if let Item::Side(x, _) = it { *x } else { usize::MAX };
            let leads = |a: usize, b: usize| {
                (0..self.d.edges.len()).any(|i| self.from[i] == Some(a) && self.to[i] == Some(b))
            };
            let mut sorted = Vec::new();
            while !sides.is_empty() {
                let k = (0..sides.len())
                    .find(|&k| {
                        !sides.iter().any(|o| node(o) != node(&sides[k]) && leads(node(o), node(&sides[k])))
                    })
                    .unwrap_or(0);
                sorted.push(sides.remove(k));
            }
            for (k, it) in slots.into_iter().zip(sorted) {
                items[k] = it;
            }
            if nest {
                let slots: Vec<usize> =
                    (0..items.len()).filter(|&k| matches!(items[k], Item::Rail(_))).collect();
                let mut rails: Vec<Item> = slots.iter().map(|&k| items[k].clone()).collect();
                let place = |it: &Item| match it {
                    Item::Rail(i) => self.to[*i].and_then(|t| at.get(&t)).copied().unwrap_or(0),
                    _ => 0,
                };
                sorting::by_key(&mut rails, |it| (place(it) > at[&n], std::cmp::Reverse(place(it))));
                for (k, it) in slots.into_iter().zip(rails) {
                    items[k] = it;
                }
            }
            all.insert(n, items);
        }
        all
    }
}

fn dfs(
    a: usize,
    out: &HashMap<usize, Vec<usize>>,
    state: &mut HashMap<usize, u8>,
    back: &mut HashSet<(usize, usize)>,
    post: &mut Vec<usize>,
) {
    state.insert(a, 1);
    for &b in out.get(&a).into_iter().flatten() {
        match state.get(&b) {
            None => dfs(b, out, state, back, post),
            Some(&1) => {
                back.insert((a, b));
            }
            _ => {}
        }
    }
    state.insert(a, 2);
    post.push(a);
}

/// A row before its rails: glyphs left of its text (`lanes`), its pieces, and the rail it taps.
#[derive(Default)]
struct Row {
    lanes: Vec<(usize, &'static str)>,
    pieces: Vec<(usize, String, Role)>,
    /// Where its text ends.
    end: usize,
    tap: Option<Tap>,
    node: Option<usize>,
    /// The entry riding on its node's row.
    rider: Option<usize>,
    /// Whose anchors the column names on it.
    anchors: Vec<usize>,
    marker: Option<(&'static str, Role)>,
    /// The composite whose regions it parts.
    divider: Option<usize>,
}

/// A row's way onto a rail: `lead`, then the label, then `─` to the rail. `into`: the target's own row.
struct Tap {
    target: usize,
    into: bool,
    lead: &'static str,
    label: String,
}

impl Row {
    fn at(lanes: &[(usize, &'static str)], col: usize) -> Row {
        Row { lanes: lanes.to_vec(), end: col, ..Row::default() }
    }

    fn put(&mut self, s: impl Into<String>, role: Role) {
        let s = s.into();
        let w = s.width();
        self.pieces.push((self.end, s, role));
        self.end += w;
    }

    /// Where its text and its tap's least run end — a rail it taps goes there or further.
    fn base(&self) -> usize {
        self.end
            + self
                .tap
                .as_ref()
                .map_or(0, |t| t.lead.width() + t.label.width() + usize::from(!t.label.is_empty()))
    }
}

struct Rail {
    target: usize,
    taps: Vec<usize>,
    top: usize,
    bottom: usize,
    col: usize,
}

impl Rail {
    fn spans(&self, r: usize) -> bool {
        (self.top..=self.bottom).contains(&r)
    }
}

/// Rows made from a plan, then railed and rendered.
struct Emit<'g, 'a> {
    g: &'g Graph<'a>,
    f: &'g Folds,
    plan: Plan,
    items: HashMap<usize, Vec<Item>>,
    inline: HashMap<usize, Item>,
    targets: HashSet<usize>,
    /// Top-level nodes a way in leads to (`▸`).
    ways: HashSet<usize>,
    /// Moved label rows, by the node whose row they move to.
    moved: HashMap<usize, Vec<usize>>,
    rows: Vec<Row>,
    /// A composite's rows (title to its last).
    blocks: HashMap<usize, (usize, usize)>,
    /// Ends whose anchor the column named already.
    named: HashSet<usize>,
    /// Label rows: (edge, its node has no branches, row).
    labels: Vec<(usize, bool, usize)>,
}

impl<'g, 'a> Emit<'g, 'a> {
    fn lay(
        g: &'g Graph<'a>,
        f: &'g Folds,
        plan: Plan,
        mut items: HashMap<usize, Vec<Item>>,
        targets: HashSet<usize>,
        room: usize,
    ) -> Lay {
        // A row takes its node's first way inline when nothing comes in on it and no label row follows (a
        // composite's label row follows its block: its own ways go on its title row) — if that way is its only
        // one, a word or back up: a rail down from it would run past the ways under it.
        let labelled =
            |n: usize| plan.spine_edge.get(&n).is_some_and(|&i| !g.d.says(&g.d.edges[i]).is_empty());
        let mut order = Vec::new();
        g.walk(&plan, (None, 0), &mut order);
        let at: HashMap<usize, usize> = order.iter().enumerate().map(|(k, &n)| (n, k)).collect();
        let up = |n: usize, i: usize| g.to[i].and_then(|t| at.get(&t)).is_some_and(|&k| k <= at[&n]);
        let mut inline = HashMap::new();
        for (&n, its) in &mut items {
            let free =
                !targets.contains(&n) && !plan.entries.contains_key(&n) && (g.composite(n) || !labelled(n));
            let fits = match its.first() {
                Some(Item::Word { .. }) => true,
                Some(&Item::Rail(i)) => its.len() == 1 || up(n, i),
                _ => false,
            };
            if free && fits {
                inline.insert(n, its.remove(0));
            }
        }
        let ways = (0..g.d.edges.len())
            .filter(|&i| g.known[i] && g.from[i].is_none() && g.within[i].is_none())
            .filter_map(|i| g.member(None, 0, g.to[i]?))
            .collect();
        let mut moved: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut spine: Vec<usize> =
            plan.spine_edge.values().copied().filter(|e| f.moved.contains(e)).collect();
        sorting::sort(&mut spine);
        for e in spine {
            moved.entry(g.from[e].unwrap_or_default()).or_default().push(e);
        }
        let mut emit = Emit {
            g,
            f,
            plan,
            items,
            inline,
            targets,
            ways,
            moved,
            rows: Vec::new(),
            blocks: HashMap::new(),
            named: HashSet::new(),
            labels: Vec::new(),
        };
        emit.scope((None, 0), 1, &[]);
        emit.finish(room)
    }

    fn scope(&mut self, scope: (Option<usize>, usize), col: usize, lanes: &[(usize, &'static str)]) {
        let segments = self.plan.spines.get(&scope).cloned().unwrap_or_default();
        for seg in segments {
            for (k, &s) in seg.iter().enumerate() {
                let next = k + 1 < seg.len();
                self.node(s, col, lanes);
                let items = self.items.get(&s).cloned().unwrap_or_default();
                let label = self.plan.spine_edge.get(&s).copied().filter(|e| !self.f.moved.contains(e));
                let label = label.map(|e| (e, self.g.says(self.f, e))).filter(|(_, g)| !g.is_empty());
                for (j, it) in items.iter().enumerate() {
                    let more = j + 1 < items.len() || label.is_some() || next;
                    self.item(it, col, lanes, more);
                }
                if let Some((e, gist)) = label {
                    self.labels.push((e, items.is_empty(), self.rows.len()));
                    let mut r = Row::at(lanes, col);
                    r.put("│ ", Role::Line);
                    r.put(gist, Role::Gist);
                    self.rows.push(r);
                }
            }
        }
    }

    /// A node's row — a composite's title row and its regions.
    fn node(&mut self, n: usize, col: usize, lanes: &[(usize, &'static str)]) {
        let first = self.rows.len();
        let mut r = Row::at(lanes, col);
        if self.g.composite(n) {
            r.lanes.push((col, "╎"));
            r.end = col + 2;
            let role = if self.g.selected == Some(n) { Role::Selected } else { Role::Composite };
            r.put(self.g.name(self.f, n), role);
            r.node = Some(n);
            r.anchors.push(n);
        } else {
            self.mark(&mut r, n);
        }
        self.extras(&mut r, n);
        if self.ways.contains(&n) {
            r.marker = Some(("▸", Role::Way));
        }
        self.rows.push(r);
        if self.g.composite(n) {
            let mut inner = lanes.to_vec();
            inner.push((col, "╎"));
            for (k, region) in self.g.regions(n).into_iter().enumerate() {
                if k > 0 {
                    let mut d = Row::at(&inner, col + 2);
                    d.put(AT_ONCE, Role::Line);
                    d.divider = Some(n);
                    self.rows.push(d);
                }
                self.scope((Some(n), region), col + 2, &inner);
            }
            self.blocks.insert(n, (first, self.rows.len()));
        }
    }

    /// `● name` — the row is the node's.
    fn mark(&mut self, r: &mut Row, n: usize) {
        let mark = self.g.looks.get(n).map_or(Mark::Untied, |l| l.mark);
        r.put(mark.glyph(), Role::Mark(mark));
        r.put(" ", Role::Line);
        let role = if self.g.selected == Some(n) { Role::Selected } else { Role::Name };
        r.put(self.g.name(self.f, n), role);
        r.node = Some(n);
        r.anchors.push(n);
    }

    /// After a node's text: its moved labels, then what comes in (a rail, an entry) or its inline way out.
    fn extras(&mut self, r: &mut Row, n: usize) {
        for e in self.moved.get(&n).cloned().unwrap_or_default() {
            r.put("  ↓ ", Role::Line);
            r.put(self.g.says(self.f, e), Role::Gist);
        }
        if let Some(&x) = self.plan.entries.get(&n) {
            r.put(" ◀─ ", Role::Line);
            r.put("▸", Role::Way);
            let node = r.node;
            self.mark(r, x);
            r.node = node;
            r.rider = Some(x);
        } else if self.targets.contains(&n) {
            r.tap = Some(Tap { target: n, into: true, lead: " ◀─", label: String::new() });
        } else if let Some(it) = self.inline.get(&n).cloned() {
            match it {
                Item::Word { edge, word, end } => {
                    r.put(" ", Role::Line);
                    self.word(r, edge, word, end);
                }
                Item::Rail(i) => {
                    let label = self.g.says(self.f, i);
                    r.tap = Some(Tap {
                        target: self.g.to[i].unwrap_or_default(),
                        into: false,
                        lead: " ─",
                        label,
                    });
                }
                Item::Side(..) => {}
            }
        }
    }

    /// `─gist─▶ word` (`─▶ word` with no gist).
    fn word(&mut self, r: &mut Row, edge: usize, word: String, end: Option<usize>) {
        self.arrow(r, edge);
        r.put(word, Role::Word);
        if let Some(e) = end
            && self.named.insert(e)
            && self.g.looks.get(e).is_some_and(|l| l.anchor.is_some())
        {
            r.anchors.push(e);
        }
    }

    fn arrow(&self, r: &mut Row, edge: usize) {
        r.put("─", Role::Line);
        let gist = self.g.says(self.f, edge);
        if !gist.is_empty() {
            r.put(gist, Role::Gist);
            r.put("─", Role::Line);
        }
        r.put("▶ ", Role::Line);
    }

    /// A way out on a row of its own under its node (at `col`), `├` while `more` follow.
    fn item(&mut self, it: &Item, col: usize, lanes: &[(usize, &'static str)], more: bool) {
        let mut r = Row::at(lanes, col);
        r.put(if more { "├" } else { "╰" }, Role::Line);
        match it.clone() {
            Item::Word { edge, word, end } => self.word(&mut r, edge, word, end),
            Item::Rail(i) => {
                r.put("─", Role::Line);
                let label = self.g.says(self.f, i);
                r.tap = Some(Tap { target: self.g.to[i].unwrap_or_default(), into: false, lead: "", label });
            }
            Item::Side(x, i) => {
                self.arrow(&mut r, i);
                let at = r.end;
                self.mark(&mut r, x);
                self.extras(&mut r, x);
                self.rows.push(r);
                let mut sub = lanes.to_vec();
                if more {
                    sub.push((col, "│"));
                }
                let items = self.items.get(&x).cloned().unwrap_or_default();
                for (j, it) in items.iter().enumerate() {
                    self.item(it, at, &sub, j + 1 < items.len());
                }
                return;
            }
        }
        self.rows.push(r);
    }

    /// Rails placed, everything rendered, the anchor column and the title added.
    fn finish(mut self, room: usize) -> Lay {
        if let Some(s) = self.g.selected
            && let Some(r) = self.rows.iter_mut().find(|r| r.node == Some(s) || r.rider == Some(s))
        {
            r.marker = Some(("▎", Role::Selected));
        }
        let mut rails = self.rails();
        self.place(&mut rails);
        let mut grid: Vec<Vec<(String, Role)>> = vec![Vec::new(); self.rows.len()];
        for (row, cells) in self.rows.iter().zip(&mut grid) {
            if let Some((m, role)) = row.marker {
                put(cells, 0, m, role);
            }
            for &(col, glyph) in &row.lanes {
                put(cells, col, glyph, Role::Line);
            }
            for (col, s, role) in &row.pieces {
                put(cells, *col, s, *role);
            }
        }
        for rail in &rails {
            for (r, cells) in grid.iter_mut().enumerate().take(rail.bottom + 1).skip(rail.top) {
                let glyph = match r {
                    _ if r == rail.top => "╮",
                    _ if r == rail.bottom => "╯",
                    _ if rail.taps.contains(&r) => "┤",
                    _ => "│",
                };
                put(cells, rail.col, glyph, Role::Line);
            }
        }
        let mut hops = 0;
        for rail in &rails {
            for &r in &rail.taps {
                let row = &self.rows[r];
                let Some(tap) = &row.tap else { continue };
                let cells = &mut grid[r];
                let mut c = put(cells, row.end, tap.lead, Role::Line);
                if !tap.label.is_empty() {
                    c = put(cells, c, &tap.label, Role::Gist);
                }
                // the run to the rail hops over the rails it crosses
                for c in c..rail.col {
                    if cells.get(c).is_none_or(|(s, _)| s != "│") {
                        put(cells, c, "─", Role::Line);
                    } else {
                        hops += 1;
                    }
                }
            }
        }
        for (r, row) in self.rows.iter().enumerate() {
            let Some(c) = row.divider else { continue };
            let (a, b) = self.blocks[&c];
            let widest = self.rows[a..b].iter().map(Row::base).max().unwrap_or(0);
            let rail = rails.iter().filter(|q| q.spans(r) && q.col > row.end).map(|q| q.col - 2).min();
            for c in row.end..rail.map_or(widest, |q| q.min(widest)) {
                put(&mut grid[r], c, "┄", Role::Line);
            }
        }
        let mut width = grid.iter().map(|c| trimmed(c).len()).max().unwrap_or(0);
        if self.f.anchors {
            let col = width + ANCHOR_GAP;
            for (row, cells) in self.rows.iter().zip(&mut grid) {
                if let Some((glyph, mark, text)) = self.anchor(&row.anchors) {
                    let c = put(cells, col, glyph, Role::Mark(mark));
                    put(cells, c, &format!(" {text}"), Role::Faint);
                }
            }
            width = grid.iter().map(|c| trimmed(c).len()).max().unwrap_or(0);
        }
        let mut lines = vec![self.title(room)];
        lines.extend(self.rows.iter().zip(&grid).map(|(row, cells)| Line {
            spans: spans(trimmed(cells)),
            nodes: row.node.into_iter().chain(row.rider).collect(),
        }));
        let mut labels = self.labels.clone();
        sorting::by_key(&mut labels, |&(_, bare, row)| (!bare, row));
        Lay {
            height: lines.len(),
            lines,
            width,
            hops,
            rails: rails.iter().map(|q| (q.target, q.col)).collect(),
            labels: labels.into_iter().map(|(e, _, _)| e).collect(),
        }
    }

    /// One rail per target, through every row that taps it.
    fn rails(&self) -> Vec<Rail> {
        let mut rails: Vec<Rail> = Vec::new();
        for (r, row) in self.rows.iter().enumerate() {
            let Some(tap) = &row.tap else { continue };
            match rails.iter_mut().find(|q| q.target == tap.target) {
                Some(q) => {
                    q.taps.push(r);
                    q.bottom = r;
                }
                None => rails.push(Rail { target: tap.target, taps: vec![r], top: r, bottom: r, col: 0 }),
            }
        }
        // A rail needs its target's row and a way onto it.
        rails.retain(|q| {
            q.taps.len() > 1 && q.taps.iter().any(|&r| self.rows[r].tap.as_ref().is_some_and(|t| t.into))
        });
        rails
    }

    /// Columns: the rail whose crossings cost least goes in first (nearest the text) — a rail inside another's
    /// span with none of the other's taps in it costs nothing; each hugs the rows it passes: a cell after text,
    /// two after a rail; its own taps' runs reach it.
    fn place(&self, rails: &mut [Rail]) {
        let mut left: Vec<usize> = (0..rails.len()).collect();
        let mut placed: Vec<usize> = Vec::new();
        while !left.is_empty() {
            let cost = |a: usize| {
                let q = &rails[a];
                let crossed: usize = left
                    .iter()
                    .filter(|&&b| b != a)
                    .map(|&b| rails[b].taps.iter().filter(|&&r| q.spans(r)).count())
                    .sum();
                (crossed, q.bottom - q.top, q.top)
            };
            let k = (0..left.len()).min_by_key(|&k| cost(left[k])).unwrap_or(0);
            let a = left.remove(k);
            let (top, bottom, target) = (rails[a].top, rails[a].bottom, rails[a].target);
            let mut col = (top..=bottom)
                .map(|r| {
                    let row = &self.rows[r];
                    let own = row.tap.as_ref().is_some_and(|t| t.target == target);
                    let text = row.base() + usize::from(!own);
                    placed
                        .iter()
                        .filter(|&&q| rails[q].spans(r))
                        .map(|&q| rails[q].col + 3)
                        .fold(text, usize::max)
                })
                .max()
                .unwrap_or(0);
            // Two rails never end and begin in one column on adjacent rows (they'd read as one line).
            while placed
                .iter()
                .any(|&q| rails[q].col == col && rails[q].top <= bottom + 1 && top <= rails[q].bottom + 1)
            {
                col += 1;
            }
            rails[a].col = col;
            placed.push(a);
        }
    }

    /// The column's text for a row: the first anchored node's glyph, then each node's anchor (`◦ untied` for a
    /// node with none; an end with none says nothing).
    fn anchor(&self, nodes: &[usize]) -> Option<(&'static str, Mark, String)> {
        let mut glyph = None;
        let mut parts = Vec::new();
        for &n in nodes {
            let look = self.g.looks.get(n);
            let mark = look.map_or(Mark::Untied, |l| l.mark);
            match look.and_then(|l| l.anchor.as_ref()) {
                Some(a) => parts.push(a.clone()),
                None if !self.g.end[n] => parts.push("untied".into()),
                None => continue,
            }
            glyph.get_or_insert((mark.anchor_glyph(), mark));
        }
        let (g, m) = glyph?;
        Some((g, m, parts.join(" · ")))
    }

    /// ` name · title` and, right-aligned to the room, `N states · N ends · N transitions · N ▲ changed` — parts
    /// dropped from the end, then the title to its gist, then cut.
    fn title(&self, room: usize) -> Line {
        let g = self.g;
        let n = g.d.nodes.len();
        let states = (0..n).filter(|&x| !g.composite(x) && !g.end[x]).count();
        let ends = (0..n).filter(|&x| g.end[x]).count();
        let ways =
            (0..g.d.edges.len()).filter(|&i| g.known[i] && g.from[i].is_some() && g.to[i].is_some()).count();
        let changed = g.looks.iter().filter(|l| l.mark == Mark::Changed).count();
        let mut parts = vec![format!("{states} states")];
        if ends > 0 {
            parts.push(format!("{ends} ends"));
        }
        parts.push(format!("{ways} transitions"));
        if changed > 0 {
            parts.push(format!("{changed} ▲ changed"));
        }
        let full = format!(" {} · {}", g.d.name, g.d.title);
        let short = format!(" {} · {}", g.d.name, super::gist(&g.d.title));
        let tries = [&full, &short].into_iter().flat_map(|t| (1..=parts.len()).rev().map(move |k| (t, k)));
        for (left, k) in tries {
            let right = parts[..k].join(" · ");
            if left.width() + 2 + right.width() <= room {
                let pad = " ".repeat(room - left.width() - right.width());
                let spans = vec![(left.clone(), Role::Title), (pad, Role::Line), (right, Role::Faint)];
                return Line { spans, nodes: Vec::new() };
            }
        }
        Line { spans: vec![(fit_ellipsis(&short, room), Role::Title)], nodes: Vec::new() }
    }
}

/// `s` written into `cells` from `col`, a grapheme a cell (a wide one's second cell empty); where it ends.
fn put(cells: &mut Vec<(String, Role)>, col: usize, s: &str, role: Role) -> usize {
    let mut c = col;
    for g in s.graphemes(true) {
        let w = g.width();
        if w == 0 {
            continue;
        }
        if cells.len() < c + w {
            cells.resize(c + w, (" ".into(), Role::Line));
        }
        cells[c] = (g.to_string(), role);
        if w == 2 {
            cells[c + 1] = (String::new(), role);
        }
        c += w;
    }
    c
}

fn trimmed(cells: &[(String, Role)]) -> &[(String, Role)] {
    let end = cells.iter().rposition(|(s, _)| s != " ").map_or(0, |k| k + 1);
    &cells[..end]
}

fn spans(cells: &[(String, Role)]) -> Vec<(String, Role)> {
    let mut out: Vec<(String, Role)> = Vec::new();
    for (s, role) in cells {
        match out.last_mut() {
            Some((t, r)) if r == role => t.push_str(s),
            _ => out.push((s.clone(), *role)),
        }
    }
    out
}

#[cfg(test)]
mod tests;
