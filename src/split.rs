//! The split tree (pure part, panes P2b — `target/sb/panes-plan.md` §2.1): a leaf is a pane's id, a node a
//! direction and its children, each with its share of the room. Nothing it draws: what a pane shows is
//! `pane.rs`'s (a code pane's document and scroll, editor.rs's `View`), drawing term.rs's — which lays the tree out
//! in the frame's size every frame (`layout`), so the tree is data: ids and shares, no cells.

/// A pane — any surface the screen shows: a code pane, the chat, the side panel, the bottom slot … One counter
/// for every pane (`Editor.next_pane`).
pub type PaneId = u64;

/// A child's share of its node's room along the node's direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// An equal part of what the others leave (the remainder a cell each to the leading ones).
    Even,
    /// As many cells as you set (a drag, `C-w < > - +`) — clamped to the room there is, never written back.
    Cells(u16),
    /// The surface's own: the chat's width (`sizes::widths`), the slot's height (`sizes::bottom`) … — asked of
    /// the layout's `Room`; 0 = no room (closed, folded away). An `Auto` child carries its own edge cell.
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Side by side (vertical divider) — `C-w v`.
    Vertical,
    /// Stacked (horizontal divider) — `C-w s`.
    Horizontal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Split {
    Leaf(PaneId),
    Node(Dir, Vec<(Share, Split)>),
}

/// What a layout asks of the surfaces: an `Auto` child's size, and the rows a node keeps over its children (the
/// path bar over the code's column).
pub trait Room {
    /// How many cells along `dir` the `Auto` child `kid` takes, of `avail`.
    fn auto(&self, kid: &Split, dir: Dir, avail: usize) -> usize;
    /// Rows node `node` keeps at its top, over its children.
    fn over(&self, _node: &Split) -> usize {
        0
    }
}

/// Every `Auto` child gets nothing: only the shares that say their size.
#[cfg(test)]
pub struct NoAuto;

#[cfg(test)]
impl Room for NoAuto {
    fn auto(&self, _: &Split, _: Dir, _: usize) -> usize {
        0
    }
}

/// Screen cell rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Split {
    /// Insert `new` next to `target`. Inside a same-direction node as a sibling (an `Even` share), else wrap the
    /// leaf in a new node (which keeps the leaf's share; the two inside split it evenly).
    pub fn split(&mut self, target: PaneId, new: PaneId, dir: Dir) -> bool {
        match self {
            Split::Leaf(v) if *v == target => {
                *self = Split::Node(
                    dir,
                    vec![(Share::Even, Split::Leaf(target)), (Share::Even, Split::Leaf(new))],
                );
                true
            }
            Split::Leaf(_) => false,
            Split::Node(d, kids) => {
                if *d == dir
                    && let Some(i) = kids.iter().position(|k| k.1 == Split::Leaf(target))
                {
                    kids.insert(i + 1, (Share::Even, Split::Leaf(new)));
                    return true;
                }
                kids.iter_mut().any(|k| k.1.split(target, new, dir))
            }
        }
    }

    /// Remove a leaf. A node left with one child collapses into it (the child taking the node's share). true if
    /// removed (the last leaf can't be).
    pub fn remove(&mut self, target: PaneId) -> bool {
        let Split::Node(_, kids) = self else { return false };
        if let Some(i) = kids.iter().position(|k| k.1 == Split::Leaf(target)) {
            kids.remove(i);
        } else if !kids.iter_mut().any(|k| k.1.remove(target)) {
            return false;
        }
        if kids.len() == 1 {
            *self = kids.pop().expect("one child").1;
        }
        true
    }

    /// Every leaf's id as `to` says (panes given new ids — `:restart`'s).
    pub fn renumber(&mut self, to: &impl Fn(PaneId) -> PaneId) {
        match self {
            Split::Leaf(v) => *v = to(*v),
            Split::Node(_, kids) => kids.iter_mut().for_each(|k| k.1.renumber(to)),
        }
    }

    pub fn leaves(&self) -> Vec<PaneId> {
        match self {
            Split::Leaf(v) => vec![*v],
            Split::Node(_, kids) => kids.iter().flat_map(|k| k.1.leaves()).collect(),
        }
    }

    /// Whether leaf `id` is in it.
    pub fn holds(&self, id: PaneId) -> bool {
        match self {
            Split::Leaf(v) => *v == id,
            Split::Node(_, kids) => kids.iter().any(|k| k.1.holds(id)),
        }
    }

    /// The node whose child holds leaf `id` (that child's index) — the split a pane's size is set in along `dir`:
    /// the innermost one of that direction around it.
    pub fn parent_along(&mut self, id: PaneId, dir: Dir) -> Option<(&mut Vec<(Share, Split)>, usize)> {
        let Split::Node(d, kids) = self else { return None };
        let i = kids.iter().position(|k| k.1.holds(id))?;
        if kids[i].1.has_parent_along(id, dir) {
            return kids[i].1.parent_along(id, dir);
        }
        (*d == dir).then_some((kids, i))
    }

    fn has_parent_along(&self, id: PaneId, dir: Dir) -> bool {
        let Split::Node(d, kids) = self else { return false };
        kids.iter().find(|k| k.1.holds(id)).is_some_and(|k| *d == dir || k.1.has_parent_along(id, dir))
    }

    /// Every share back to `Even` but the surfaces' own (`Auto`) — `C-w =`.
    pub fn even(&mut self) {
        if let Split::Node(_, kids) = self {
            for (share, kid) in kids.iter_mut() {
                if matches!(share, Share::Cells(_)) {
                    *share = Share::Even;
                }
                kid.even();
            }
        }
    }

    /// Whether a share was set (a size of yours to make automatic again).
    #[cfg(test)]
    pub fn sized(&self) -> bool {
        match self {
            Split::Leaf(_) => false,
            Split::Node(_, kids) => kids.iter().any(|(s, k)| matches!(s, Share::Cells(_)) || k.sized()),
        }
    }

    /// Lay out the panes in `r`: each child its share along the node's direction (`Auto` from `room`, `Cells` as
    /// set — clamped to leave each `Even` one a cell —, `Even` the rest); side by side, one divider cell between
    /// two children that have room unless one is `Auto` (it carries its own edge); stacked panes touch (each one's
    /// top line is its header). Every leaf is listed — one with no room with a zero width or height.
    pub fn layout(&self, r: Rect, room: &impl Room) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.lay(r, room, &mut out);
        out
    }

    fn lay(&self, r: Rect, room: &impl Room, out: &mut Vec<(PaneId, Rect)>) {
        let Split::Node(dir, kids) = self else {
            if let Split::Leaf(v) = self {
                out.push((*v, r));
            }
            return;
        };
        let over = room.over(self).min(r.h);
        let r = Rect { y: r.y + over, h: r.h - over, ..r };
        let total = match dir {
            Dir::Vertical => r.w,
            Dir::Horizontal => r.h,
        };
        // The surfaces' own first, then what's set, then the even ones share the rest
        let mut sizes: Vec<Option<usize>> = vec![None; kids.len()];
        let mut left = total;
        for (i, (share, kid)) in kids.iter().enumerate() {
            if *share == Share::Auto {
                let n = room.auto(kid, *dir, left).min(left);
                sizes[i] = Some(n);
                left -= n;
            }
        }
        // (side by side: a divider between two that have room, neither `Auto` — kept back before the shares that
        // aren't the surfaces' are sized, each of those counted as having room: one with none takes no divider, so
        // what's laid out never runs past the node)
        let gaps = match dir {
            Dir::Horizontal => 0,
            Dir::Vertical => {
                let open: Vec<usize> = (0..kids.len()).filter(|&i| sizes[i].is_none_or(|n| n > 0)).collect();
                open.windows(2).filter(|w| kids[w[0]].0 != Share::Auto && kids[w[1]].0 != Share::Auto).count()
            }
        };
        left = left.saturating_sub(gaps);
        let evens = kids.iter().filter(|k| k.0 == Share::Even).count();
        let mut cells = kids.iter().filter(|k| matches!(k.0, Share::Cells(_))).count();
        for (i, (share, _)) in kids.iter().enumerate() {
            if let Share::Cells(n) = share {
                // (a cell kept for each even one and each of yours after it)
                cells -= 1;
                let n = (*n as usize).min(left.saturating_sub(evens + cells)).max(1).min(left);
                sizes[i] = Some(n);
                left -= n;
            }
        }
        let mut k = 0;
        for (i, (share, _)) in kids.iter().enumerate() {
            if *share == Share::Even {
                // the remainder goes one cell each to the leading panes
                sizes[i] = Some(left / evens + usize::from(k < left % evens));
                k += 1;
            }
        }
        // (all of them set by you and short of the room: the last of yours takes the rest — no blank cells)
        if evens == 0
            && let Some(i) = (0..kids.len()).rev().find(|&i| matches!(kids[i].0, Share::Cells(_)))
        {
            sizes[i] = sizes[i].map(|n| n + left);
        }
        let mut at = 0;
        let mut before: Option<usize> = None;
        for (i, (_, kid)) in kids.iter().enumerate() {
            let size = sizes[i].unwrap_or(0);
            if size > 0 {
                if *dir == Dir::Vertical
                    && let Some(b) = before
                    && kids[b].0 != Share::Auto
                    && kids[i].0 != Share::Auto
                {
                    at += 1;
                }
                before = Some(i);
            }
            let sub = match dir {
                Dir::Vertical => Rect { x: r.x + at, y: r.y, w: size, h: r.h },
                Dir::Horizontal => Rect { x: r.x, y: r.y + at, w: r.w, h: size },
            };
            kid.lay(sub, room, out);
            at += size;
        }
    }
}

/// The pane a hop from `from` toward (dx, dy) lands on: `rather` when it's right there — touching `from`'s edge that
/// way (a divider between them at most) and beside it across the hop: the code pane last focused, coming back to
/// the code —, else the nearest (`neighbor`).
pub fn neighbor_from(
    panes: &[(PaneId, Rect)],
    from: PaneId,
    dx: i32,
    dy: i32,
    rather: PaneId,
) -> Option<PaneId> {
    let (_, a) = *panes.iter().find(|(v, _)| *v == from)?;
    let touching = |b: &Rect| match (dx, dy) {
        (1, _) => (a.x + a.w..=a.x + a.w + 1).contains(&b.x),
        (-1, _) => (a.x.saturating_sub(1)..=a.x).contains(&(b.x + b.w)),
        (_, 1) => b.y == a.y + a.h,
        _ => b.y + b.h == a.y,
    };
    let beside = |b: &Rect| match dx != 0 {
        true => b.y < a.y + a.h && a.y < b.y + b.h,
        false => b.x < a.x + a.w && a.x < b.x + b.w,
    };
    match panes.iter().find(|(v, b)| *v == rather && *v != from && b.w > 0 && b.h > 0) {
        Some((_, b)) if touching(b) && beside(b) => Some(rather),
        _ => neighbor(panes, from, dx, dy),
    }
}

/// Nearest pane from pane `from` in direction (dx, dy) — overlapping ones preferred.
pub fn neighbor(panes: &[(PaneId, Rect)], from: PaneId, dx: i32, dy: i32) -> Option<PaneId> {
    let (_, a) = *panes.iter().find(|(v, _)| *v == from)?;
    let (acx, acy) = (a.x as i64 * 2 + a.w as i64, a.y as i64 * 2 + a.h as i64);
    panes
        .iter()
        .filter(|(v, b)| *v != from && b.w > 0 && b.h > 0)
        .filter(|(_, b)| match (dx, dy) {
            (1, _) => b.x >= a.x + a.w,
            (-1, _) => b.x + b.w <= a.x,
            (_, 1) => b.y >= a.y + a.h,
            _ => b.y + b.h <= a.y,
        })
        .min_by_key(|(_, b)| {
            let (bcx, bcy) = (b.x as i64 * 2 + b.w as i64, b.y as i64 * 2 + b.h as i64);
            // Distance along the direction + sideways offset (in half cells)
            if dx != 0 {
                ((bcx - acx).abs(), (bcy - acy).abs())
            } else {
                ((bcy - acy).abs(), (bcx - acx).abs())
            }
        })
        .map(|(v, _)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_remove_and_layout() {
        let even = |v| (Share::Even, Split::Leaf(v));
        let mut s = Split::Leaf(1);
        assert!(s.split(1, 2, Dir::Vertical));
        assert!(s.split(2, 3, Dir::Vertical), "same direction = sibling");
        assert_eq!(s, Split::Node(Dir::Vertical, vec![even(1), even(2), even(3)]));
        assert!(s.split(3, 4, Dir::Horizontal), "other direction = wrap");
        assert_eq!(s.leaves(), [1, 2, 3, 4]);
        let panes = s.layout(Rect { x: 0, y: 0, w: 32, h: 10 }, &NoAuto);
        // 32 cells - 2 dividers = 30 → 10 each, the third pane stacked 5·5
        assert_eq!(panes[0].1, Rect { x: 0, y: 0, w: 10, h: 10 });
        assert_eq!(panes[1].1, Rect { x: 11, y: 0, w: 10, h: 10 });
        assert_eq!(panes[2].1, Rect { x: 22, y: 0, w: 10, h: 5 });
        assert_eq!(panes[3].1, Rect { x: 22, y: 5, w: 10, h: 5 });
        assert_eq!(neighbor(&panes, 1, 1, 0), Some(2));
        assert_eq!(neighbor(&panes, 3, 0, 1), Some(4));
        assert_eq!(neighbor(&panes, 4, -1, 0), Some(2));
        assert!(s.remove(3));
        assert!(s.remove(4));
        assert_eq!(s, Split::Node(Dir::Vertical, vec![even(1), even(2)]));
        assert!(s.remove(2));
        assert_eq!(s, Split::Leaf(1), "collapses when one is left");
        assert!(!s.remove(1), "can't remove the last");
    }

    /// The surfaces' own sizes first (an `Auto` child carries its edge: no divider beside it), then what you set
    /// (clamped to leave the even ones a cell), the even ones the rest; a child with no room is listed empty and
    /// takes no divider.
    #[test]
    fn shares_lay_out_the_surfaces_own_then_yours_then_the_rest() {
        struct Side(usize);
        impl Room for Side {
            fn auto(&self, kid: &Split, _: Dir, _: usize) -> usize {
                if *kid == Split::Leaf(9) { self.0 } else { 0 }
            }
        }
        let s = Split::Node(
            Dir::Vertical,
            vec![
                (Share::Auto, Split::Leaf(9)),
                (Share::Even, Split::Leaf(1)),
                (Share::Cells(10), Split::Leaf(2)),
                (Share::Auto, Split::Leaf(8)),
            ],
        );
        let r = Rect { x: 0, y: 0, w: 50, h: 4 };
        let at = |panes: Vec<(PaneId, Rect)>, id| panes.into_iter().find(|p| p.0 == id).unwrap().1;
        // 50 − 20 (the side panel's) − 10 (yours) − 1 divider = 19 for the even one
        let panes = s.layout(r, &Side(20));
        assert_eq!(at(panes.clone(), 9), Rect { x: 0, y: 0, w: 20, h: 4 });
        assert_eq!(at(panes.clone(), 1), Rect { x: 20, y: 0, w: 19, h: 4 });
        assert_eq!(at(panes.clone(), 2), Rect { x: 40, y: 0, w: 10, h: 4 });
        assert_eq!(at(panes.clone(), 8).w, 0, "no room: listed, empty");
        // Yours clamped: the even one keeps a cell
        let panes = s.layout(Rect { w: 12, ..r }, &Side(0));
        assert_eq!((at(panes.clone(), 1).w, at(panes, 2).w), (1, 10));
        let mut t = s.clone();
        assert!(t.sized());
        t.even();
        assert!(!t.sized(), "C-w =: yours even again, the surfaces' own kept");
        let (kids, i) = t.parent_along(2, Dir::Vertical).unwrap();
        assert_eq!((kids.len(), i), (4, 2));
        assert!(t.parent_along(2, Dir::Horizontal).is_none(), "no stacked split around it");
    }

    /// The panes of a node never run past it, nor leave cells blank: both children set (`C-w >` on each) share the
    /// room less their divider, the last taking what's left; one even child squeezed by yours keeps a cell beside
    /// its divider.
    #[test]
    fn your_shares_stay_inside_their_node() {
        let r = Rect { x: 0, y: 0, w: 41, h: 4 };
        let both = |a, b| {
            Split::Node(
                Dir::Vertical,
                vec![(Share::Cells(a), Split::Leaf(1)), (Share::Cells(b), Split::Leaf(2))],
            )
        };
        let end = |panes: &[(PaneId, Rect)]| panes.iter().map(|p| p.1.x + p.1.w).max().unwrap();
        for (a, b) in [(30, 30), (24, 16), (10, 10), (40, 1)] {
            let panes = both(a, b).layout(r, &NoAuto);
            assert_eq!(end(&panes), 41, "{a}+{b}: fills the node, no further");
            assert_eq!(panes[1].1.x, panes[0].1.w + 1, "{a}+{b}: one divider between");
            assert!(panes.iter().all(|p| p.1.w >= 1), "{a}+{b}: {panes:?}");
        }
        let squeezed = Split::Node(
            Dir::Vertical,
            vec![(Share::Cells(60), Split::Leaf(1)), (Share::Even, Split::Leaf(2))],
        );
        let panes = squeezed.layout(r, &NoAuto);
        assert_eq!(
            (panes[0].1.w, panes[1].1),
            (39, Rect { x: 40, y: 0, w: 1, h: 4 }),
            "the even one keeps a cell"
        );
    }
}
