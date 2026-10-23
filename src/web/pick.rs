//! Which part of a page is its text, when the reader guesses (web-plan D3.1; the user's 2026-10-23: tarae's own, no
//! readability port): its `<main>` → a lone outermost `<article>` → its `[role=main]` — each only when it holds the
//! page's text — → the block its text is densest in, scored as readability.js scores: each paragraph's text not in
//! links, its commas and its length, given to five of what holds it, the best of the highest by its words and its
//! links — unless the page's title heads another block apart from it (`Pick::block`). One pass down the tree and one
//! up, whatever the page: no limit needed on its size.

use super::convert::{SECTIONING, left_out};
use super::tree::{Id, Tree};

/// Fewer characters than this (spaces left out) is "almost nothing": readability's own floor for an article.
pub const LITTLE: usize = 140;
/// A `<main>` holds the page's text when it has this much, or half the body's.
const MAIN_ENOUGH: usize = 500;
/// What a paragraph is, for its score (readability's, with lists' and definitions' items).
const PARAGRAPHS: &[&str] = &["p", "pre", "li", "td", "dd", "blockquote", "h2", "h3"];
/// A paragraph's text not in links, at least — shorter isn't scored (readability's 25).
const PARAGRAPH_MIN: usize = 25;
/// How far up a paragraph's score goes, and what each level's share is divided by (readability's: 1, 2, then
/// three times the level).
const DIVIDERS: [f64; 5] = [1.0, 2.0, 6.0, 9.0, 12.0];
/// The highest scored looked at for the best (readability looks at five; ten leaves room for the weights).
const CANDIDATES: usize = 10;
/// Below this the best block isn't one: the page is read whole.
const SCORE_MIN: f64 = 20.0;
/// A block this much links (or more) is navigation, not the text.
const LINKS_MAX: f64 = 0.5;

/// What a subtree holds: its characters (spaces left out), those in links, its commas.
#[derive(Clone, Copy, Default)]
struct Sum {
    chars: usize,
    links: usize,
    commas: usize,
}

/// How each part is found, from one walk of the page.
pub struct Pick<'a> {
    t: &'a Tree,
    /// The page in document order
    order: Vec<Id>,
    /// By id: never read (the converter leaves it out, or it's inside what it leaves out)
    out: Vec<bool>,
    /// By id: inside an `<article>`
    in_article: Vec<bool>,
    sums: Vec<Sum>,
    body: usize,
}

impl<'a> Pick<'a> {
    pub fn new(t: &'a Tree) -> Pick<'a> {
        let order: Vec<Id> = t.walk(Tree::ROOT).collect();
        let n = t.count();
        let (mut out, mut in_article, mut sectioned) = (vec![false; n], vec![false; n], vec![false; n]);
        let mut sums = vec![Sum::default(); n];
        // Down: what holds each node first
        for &id in &order {
            let p = t.parent(id).unwrap_or(Tree::ROOT);
            let (p_out, p_article, p_sectioned) = (flag(&out, p), flag(&in_article, p), flag(&sectioned, p));
            let name = t.name(id).unwrap_or_default();
            let page_chrome = matches!(name, "header" | "footer") && !p_sectioned;
            set(&mut out, id, p_out || t.name(id).is_some_and(|name| left_out(t, id, name)) || page_chrome);
            set(&mut in_article, id, p_article || t.name(p) == Some("article"));
            set(&mut sectioned, id, p_sectioned || SECTIONING.contains(&name));
        }
        // Up: each node's own, then into what holds it
        for &id in order.iter().rev() {
            let mut s = sums.get(id).copied().unwrap_or_default();
            if let Some(text) = t.own_text(id) {
                s.chars += text.chars().filter(|c| !c.is_whitespace()).count();
                s.commas += text.matches(',').count();
            }
            if t.name(id) == Some("a") {
                s.links = s.chars;
            }
            if let Some(own) = sums.get_mut(id) {
                *own = s;
            }
            if let Some(p) = t.parent(id).and_then(|p| sums.get_mut(p)) {
                p.chars += s.chars;
                p.links += s.links;
                p.commas += s.commas;
            }
        }
        let body = t.body().map_or(0, |b| sum(&sums, b).chars);
        Pick { t, order, out, in_article, sums, body }
    }

    /// Characters in `id` and all it holds, spaces left out.
    pub fn chars(&self, id: Id) -> usize {
        sum(&self.sums, id).chars
    }

    /// It holds the page's text: enough of it, or half the body's.
    fn enough(&self, id: Id) -> bool {
        let n = self.chars(id);
        n >= MAIN_ENOUGH || 2 * n >= self.body && n >= LITTLE
    }

    fn first(&self, test: impl Fn(Id) -> bool) -> Option<Id> {
        self.order.iter().copied().find(|&id| test(id) && self.enough(id))
    }

    /// The outermost `<main>` that holds the page's text.
    pub fn main(&self) -> Option<Id> {
        self.first(|id| self.t.name(id) == Some("main"))
    }

    /// The outermost `[role=main]` that holds the page's text.
    pub fn role_main(&self) -> Option<Id> {
        self.first(|id| self.t.attr(id, "role") == Some("main"))
    }

    /// The one `<article>` not inside another that holds the page's text (several: none of them is the page's).
    pub fn article(&self) -> Option<Id> {
        let mut outer = self.order.iter().copied().filter(|&id| {
            self.t.name(id) == Some("article") && !flag(&self.in_article, id) && self.enough(id)
        });
        let one = outer.next();
        one.filter(|_| outer.next().is_none())
    }

    /// The page's article when it has no `<main>` or lone `<article>`: the block its text is densest in — unless
    /// the page's title heads another block, apart from it: a page's text starts at its title, and a dense block
    /// elsewhere is something else on the page (php.net's manual is the article, its long user notes below it
    /// aren't). `title` is the page's `<title>`.
    pub fn block(&self, title: &str) -> Option<Id> {
        let dense = self.densest();
        let titled = self.titled(title);
        match (dense, titled) {
            (Some(d), Some(h)) if !self.holds(h, d) && !self.holds(d, h) => Some(h),
            (Some(d), _) => Some(d),
            (None, h) => h,
        }
    }

    /// `a` holds `b`.
    fn holds(&self, a: Id, b: Id) -> bool {
        self.t.up(b, usize::MAX).any(|x| x == a)
    }

    /// A block fit to be the article: not left out, not mostly links.
    fn fits(&self, id: Id) -> bool {
        let s = sum(&self.sums, id);
        let density = s.links as f64 / s.chars.max(1) as f64;
        density < LINKS_MAX && !flag(&self.out, id) && self.t.name(id).is_some()
    }

    /// The nearest block holding the page's titled heading — its first `<h1>` or `<h2>` read whose words are in the
    /// `<title>` (`strlen` in "PHP: strlen - Manual") — that holds the page's text and fits.
    fn titled(&self, title: &str) -> Option<Id> {
        let t = self.t;
        let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let title = words(title);
        let heading = self.order.iter().copied().find(|&id| {
            matches!(t.name(id), Some("h1" | "h2")) && !flag(&self.out, id) && {
                let h = words(&t.text(id));
                !h.is_empty() && title.contains(&h)
            }
        })?;
        t.up(heading, usize::MAX).find(|&a| self.enough(a) && self.fits(a))
    }

    /// The block the page's text is densest in, readability.js's way.
    fn densest(&self) -> Option<Id> {
        let t = self.t;
        let mut score = vec![0.0f64; self.sums.len()];
        let mut scored = Vec::new();
        for &p in &self.order {
            if !t.name(p).is_some_and(|n| PARAGRAPHS.contains(&n)) || flag(&self.out, p) {
                continue;
            }
            let s = sum(&self.sums, p);
            let own = s.chars.saturating_sub(s.links);
            if own < PARAGRAPH_MIN {
                continue;
            }
            // (many short paragraphs outweigh one long one — a user's comment)
            let points = 1.0 + (s.commas as f64).min(3.0) + (own as f64 / 100.0).min(3.0);
            for (a, divider) in t.up(p, DIVIDERS.len()).zip(DIVIDERS) {
                if let Some(x) = score.get_mut(a) {
                    if *x == 0.0 {
                        scored.push(a);
                    }
                    *x += points / divider;
                }
            }
        }
        let at = |id: Id| score.get(id).copied().unwrap_or(0.0);
        // (the highest, in no order: the best of them is taken by its value)
        if scored.len() > CANDIDATES {
            scored.select_nth_unstable_by(CANDIDATES - 1, |a, b| at(*b).total_cmp(&at(*a)));
            scored.truncate(CANDIDATES);
        }
        scored
            .into_iter()
            .filter(|&id| self.fits(id))
            .map(|id| {
                let s = sum(&self.sums, id);
                let density = s.links as f64 / s.chars.max(1) as f64;
                (id, (at(id) + weight(t, id)) * (1.0 - density))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .filter(|(_, s)| *s >= SCORE_MIN)
            .map(|(id, _)| id)
    }
}

/// Readability's class and id weight: ±25 for the words that say a block is or isn't the text.
fn weight(t: &Tree, id: Id) -> f64 {
    let said =
        format!("{} {}", t.attr(id, "class").unwrap_or_default(), t.attr(id, "id").unwrap_or_default())
            .to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| said.contains(w));
    let mut w = 0.0;
    if has(&[
        "comment", "footer", "footnote", "masthead", "meta", "promo", "related", "share", "sidebar",
        "sponsor", "widget",
    ]) {
        w -= 25.0;
    }
    if has(&["article", "body", "content", "entry", "main", "page", "post", "text", "blog", "story"]) {
        w += 25.0;
    }
    w
}

fn flag(v: &[bool], id: Id) -> bool {
    v.get(id).copied().unwrap_or(false)
}

fn set(v: &mut [bool], id: Id, to: bool) {
    if let Some(x) = v.get_mut(id) {
        *x = to;
    }
}

fn sum(sums: &[Sum], id: Id) -> Sum {
    sums.get(id).copied().unwrap_or_default()
}
