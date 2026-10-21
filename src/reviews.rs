//! The review record (the review lens's R — `target/sb/reviewlens-plan.md` §4): how long a person spends on
//! each change they review and what each change ended as, kept in `$state/reviews/<yyyy-mm>.jsonl` — the card
//! record's twin (`cards.rs`: its folder rules and lock, `cards::KEEP_MONTHS`, written on a worker). Paths, ids,
//! counts, grades and times only, **never a line of code**: a change is named by an HMAC-SHA256, under this
//! machine's `$state/reviews.key` (`keyed`), of its id in memory (`change_id`: a SHA-256 of its file and its
//! lines, never written as it is) — so the same change has the same id in an edit's review, a turn's and a
//! branch's, and a short line can't be found again from it. A key-named file's change or a token-like line's
//! has no id at all.
//!
//! Time (`Watch`): a change counts while it's on screen and a key, click, wheel or paste came within `IDLE` of
//! that moment, before or after — reading before a key counts, lunch doesn't. Clock reads go through
//! `crate::clock`, so a simulation can drive them.
//!
//! Where it hooks — none inside the high-risk gate's functions: each frame (`term::render` → `after_frame`:
//! what of a review is on screen and in hand); input (`Editor::handle_key`, `handle_mouse`, `handle_paste` →
//! `Editor::review_input`); an edit review's end (`llm::finish`, `llm::close_review` → `edit_ended`); a code
//! review's end (`codereview::end` → `code_ended`, which reads the grades, looks, viewed marks and notes
//! through their public functions); every take-back of a change (`Editor::git_reset_hunk`: `space v u`, the git
//! tab's `d`, the palette's "Undo this change" → `undo_asked`); a session's end (`worktree::session_close` →
//! `Editor::reviews_end`); quitting and `:restart` (`Editor::reviews_flush`). Your own edits are told apart where
//! keys and pastes are handled (`Document.own`).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::document::DocId;
use crate::editor::Editor;

/// The record's folder in tarae's state: a file a month, `2026-10.jsonl`.
pub const DIR: &str = "reviews";
/// The record's version, in each entry (`v`).
const RECORD_V: u64 = 1;
/// A moment counts when an input came within this long of it, before or after.
pub const IDLE: Duration = Duration::from_secs(20);

/// Which review a watch follows: an edit's (its `llm_generation`) or a code review (when it started).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Of {
    Edit(u64),
    Code(Instant),
}

/// One change followed: which (its file and id), its counts, where it is, its times, what it ended as.
#[derive(Clone, Debug, Default)]
pub struct Change {
    /// Its path from the session's root (absolute outside it).
    pub file: String,
    /// Its id (`change_id`: its text's hash and which of its file's changes with that text it is) — never
    /// written as it is: the record keeps it keyed (`keyed`).
    pub id: String,
    /// Its text's hash (`text_id`) as first seen · now — another once you edit it in a code review (`edited`).
    text0: String,
    text: String,
    /// Its file is named like a key, or a line of it looks like a token: no id in the record at all.
    pub secret: bool,
    pub added: usize,
    pub removed: usize,
    /// Its lines (0-based, as of the last frame) — what notes and grades are matched by.
    pub lines: Range<usize>,
    /// Drawn or in hand once — what the record keeps: a code review follows each change of a file whose diff
    /// it read, seen or not (an edit review's are each seen).
    pub seen: bool,
    /// In its file's diff as last read — one gone keeps the grade and rule it was last seen with, however it
    /// went (`space v u`, by hand, a checkpoint).
    pub present: bool,
    /// The file's change of mode (`now executable`, `now a symlink`): no lines, no text — no id.
    pub mode: bool,
    /// When it was first on screen, from the review's start.
    pub drawn: Option<Duration>,
    /// On screen with you there · in hand (the cursor in it, or the edit review's current change) with you there.
    pub active: Duration,
    pub in_hand: Duration,
    /// accepted · rejected · undone · edited · commented · looked · viewed · left.
    pub outcome: Option<&'static str>,
    /// Your own keys or pastes changed its lines during the review, or took it out of the diff.
    pub edited: bool,
    /// Its grade (`risk.rs`; a code review's) and, high, the rule that made it so — by its name, never the
    /// text it matched (`risk::rule`; `moved since a look`, `taken out since a look`).
    pub grade: Option<&'static str>,
    pub rule: Option<String>,
    /// A reviewer's finding (`space v c`) on its lines.
    pub finding: bool,
    /// `space v u` asked for it: undone once its document's diff no longer has it.
    undo: bool,
}

/// A stretch of time not counted yet — no input within `IDLE` before it — with what was on screen and in hand:
/// an input within `IDLE` after counts its end.
#[derive(Clone, Debug)]
struct Span {
    from: Instant,
    to: Instant,
    shown: Vec<usize>,
    hand: Option<usize>,
}

/// A change of a document's diff: its lines now, its id and counts.
#[derive(Clone, Debug)]
struct Found {
    lines: Range<usize>,
    /// Its text's hash (`text_id`) · how many of the file's changes before it have that text.
    text: String,
    nth: usize,
    added: usize,
    removed: usize,
    /// What would make it high (`risk::rule`) · named like a key or token-like (`Change.secret`).
    rule: Option<String>,
    secret: bool,
}

/// One review followed, frame by frame.
#[derive(Debug)]
pub struct Watch {
    pub of: Of,
    /// `edit` · `turn` · `uncommitted` · `branch` · `replay`.
    pub kind: &'static str,
    /// Its id in the record (`review_id`: this tarae's start, its pid, a count).
    pub id: String,
    pub started: Instant,
    pub changes: Vec<Change>,
    /// The review on screen with you there — any of its changes shown.
    pub active: Duration,
    /// All the changes it had as it ended, seen or not (a code review's).
    pub total: Option<usize>,
    shown: Vec<usize>,
    hand: Option<usize>,
    /// Counted up to here · the last input.
    mark: Option<Instant>,
    last_input: Option<Instant>,
    pending: Vec<Span>,
    /// The newest note when it began: one after it, yours, on a change's lines — `commented`.
    notes_before: u64,
    /// An edit review: the hunk each change is (no-op hunks aren't changes) and its decision as last seen.
    hunks: Vec<usize>,
    decided: Vec<crate::llm::Decision>,
    /// A code review: each file's changes as last found, and the followed change each is · the files a worker
    /// is finding them in, for the stamp it was given (one at a time each).
    found: HashMap<String, FileFound>,
    asked: HashMap<String, Stamp>,
    /// What decided the last frame's changes on screen — the same again, the same changes (`code_shown`).
    sight: Option<Sight>,
    /// The reviewer's findings on this review, as it ended (`outcomes`) — a record line each.
    pub findings: Vec<Finding>,
    /// The files whose changes it stopped pairing: too many alike to weigh (`ALIGN_MAX`) — their changes as they
    /// were then, each said so in the record.
    unpaired: HashSet<String>,
    /// It ended because tarae restarted (`:restart`): a review it opens again is a new one.
    pub restarted: bool,
}

/// A document's text as a file's changes were found in it: (the document, its version, its diff's).
type Stamp = (DocId, u64, u64);

/// A file's changes as last found — at `stamp` — the followed change each is, and its change of mode's. Their
/// places as the text moved since, and where your own input changed it, are the document's (`Document.own`).
#[derive(Debug)]
struct FileFound {
    stamp: Stamp,
    found: Vec<Found>,
    of: Vec<usize>,
    mode: Option<usize>,
}

/// What decides a frame's changes on screen: each pane's document, its versions (its text's, its diff's) and
/// the lines drawn, and where the cursor is.
#[derive(Debug, PartialEq)]
struct Sight {
    panes: Vec<(DocId, (u64, u64), Vec<usize>)>,
    focused: DocId,
    cursor: usize,
}

/// One finding of a reviewer's, as the record keeps it: where (its file, the change its lines touch) and what
/// came of it. Its `finding` object is the room the review panel (reviewpanel) fills: which lens found it, the
/// skeptic's verdict, what came of it — today only who found it, whether it was sent and answered.
#[derive(Clone, Debug, Default)]
pub struct Finding {
    pub file: String,
    /// The id of the change it's on (`change_id`) — None: on no change the review followed.
    pub change: Option<String>,
    pub finding: serde_json::Map<String, Value>,
}

/// A session's watches: its edit review's and its code review's (both can be up — a protected file's edit
/// over a review at the end).
#[derive(Debug, Default)]
pub struct Watches {
    pub edit: Option<Watch>,
    pub code: Option<Watch>,
}

/// The hash of a change's text, in memory: a SHA-256 of its file and its lines, removed then added — what a
/// change is found again by, frame to frame.
pub fn text_id<'a>(
    file: &str,
    removed: impl IntoIterator<Item = &'a str>,
    added: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut s = format!("tarae review change v1\0{file}\0");
    for l in removed {
        s += &format!("-{l}\n");
    }
    for l in added {
        s += &format!("+{l}\n");
    }
    crate::sha256::hex(s.as_bytes())
}

/// A change's id, in memory: its text's hash and which of its file's changes with that text it is (`nth`) —
/// two changes alike in one file are two changes. It never leaves tarae as it is: a short line could be found
/// again by trying candidates. The record keeps it keyed (`keyed`, with `KEY`, a secret of this machine's).
pub fn change_id(text: &str, nth: usize) -> String {
    crate::sha256::hex(format!("{text}#{nth}").as_bytes())
}

/// Whether a change of `path` (real; `root` its checkout) with these lines may have no id at all: its file is
/// named like a key — in the checkout as its runs are kept from reading it, outside it by its name alone — or a
/// line looks like a token.
fn secret<'a>(
    root: &std::path::Path,
    path: Option<&std::path::Path>,
    mut lines: impl Iterator<Item = &'a str>,
) -> bool {
    let named = |p: &std::path::Path| match p.starts_with(root) {
        true => crate::sandbox::key_named_in(root, p),
        false => crate::secretnames::named_secret(p),
    };
    path.is_some_and(named) || lines.any(crate::tokens::token_like)
}

/// This tarae's part of a review's id: when it started (ms, hex) and its pid — `:restart` keeps the pid, never
/// the time.
fn run_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let since = crate::clock::system().duration_since(std::time::UNIX_EPOCH);
        format!("{:x}.{}", since.map_or(0, |d| d.as_millis()), std::process::id())
    })
}

/// Review `n`'s id of the tarae tagged `tag` (`run_tag`).
fn review_id(tag: &str, n: u64) -> String {
    format!("r{tag}-{n}")
}

/// A line of a rope without its line break.
fn line(text: &ropey::Rope, l: usize) -> String {
    let s = text.line(l).to_string();
    let s = s.strip_suffix('\n').unwrap_or(&s);
    s.strip_suffix('\r').unwrap_or(s).to_string()
}

/// Ranges `a` and `b` share a line — an empty one (lines only removed) counts as its place's line.
fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    let wide = |r: &Range<usize>| r.start..r.end.max(r.start + 1);
    let (a, b) = (wide(a), wide(b));
    a.start < b.end && b.start < a.end
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Watch {
    pub fn new(of: Of, kind: &'static str, now: Instant, notes_before: u64) -> Watch {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Watch {
            of,
            kind,
            id: review_id(run_tag(), n),
            started: now,
            changes: Vec::new(),
            active: Duration::ZERO,
            total: None,
            shown: Vec::new(),
            hand: None,
            mark: None,
            last_input: None,
            pending: Vec::new(),
            notes_before,
            hunks: Vec::new(),
            decided: Vec::new(),
            found: HashMap::new(),
            asked: HashMap::new(),
            sight: None,
            findings: Vec::new(),
            unpaired: HashSet::new(),
            restarted: false,
        }
    }

    /// A change to follow — of `file`, its text's hash `text`, the `nth` of the file's changes with that text —
    /// its index.
    pub fn add(
        &mut self,
        file: &str,
        (text, nth): (String, usize),
        (added, removed): (usize, usize),
        lines: Range<usize>,
    ) -> usize {
        let id = change_id(&text, nth);
        let (text0, file) = (text.clone(), file.into());
        let (seen, present) = (true, true);
        let c = Change { file, id, text0, text, added, removed, lines, seen, present, ..Default::default() };
        self.changes.push(c);
        self.changes.len() - 1
    }

    fn credit(&mut self, shown: &[usize], hand: Option<usize>, d: Duration) {
        if shown.is_empty() && hand.is_none() {
            return;
        }
        self.active += d;
        for &i in shown {
            self.changes[i].active += d;
        }
        if let Some(h) = hand {
            self.changes[h].in_hand += d;
        }
    }

    /// Time up to `now` with what was on screen since the last frame: what an input within `IDLE` before
    /// covers counts now, the rest waits for an input within `IDLE` after it (`input`) — and what no input
    /// can reach any more goes.
    fn advance(&mut self, now: Instant) {
        let Some(m) = self.mark else {
            self.mark = Some(now);
            return;
        };
        if now <= m {
            return;
        }
        self.mark = Some(now);
        let (shown, hand) = (self.shown.clone(), self.hand);
        if !shown.is_empty() || hand.is_some() {
            let covered = self.last_input.map_or(m, |i| i + IDLE).clamp(m, now);
            if covered > m {
                self.credit(&shown, hand, covered - m);
            }
            if now > covered {
                match self.pending.last_mut() {
                    Some(s) if s.to == covered && s.shown == shown && s.hand == hand => s.to = now,
                    _ => self.pending.push(Span { from: covered, to: now, shown, hand }),
                }
            }
        }
        self.pending.retain(|s| s.to + IDLE > now);
    }

    /// A key, click, wheel or paste at `now`: the time waiting within `IDLE` before it counts.
    pub fn input(&mut self, now: Instant) {
        self.advance(now);
        let reach = now.checked_sub(IDLE);
        for s in std::mem::take(&mut self.pending) {
            let from = reach.map_or(s.from, |r| r.max(s.from));
            if s.to > from {
                self.credit(&s.shown, s.hand, s.to - from);
            }
        }
        self.last_input = Some(now);
    }

    /// A frame at `now`: the changes it shows (indices) and the one in hand.
    pub fn frame(&mut self, now: Instant, shown: Vec<usize>, hand: Option<usize>) {
        self.advance(now);
        let since = now.saturating_duration_since(self.started);
        for &i in &shown {
            self.changes[i].drawn.get_or_insert(since);
            self.changes[i].seen = true;
        }
        if let Some(h) = hand {
            self.changes[h].seen = true;
        }
        (self.shown, self.hand) = (shown, hand);
    }

    /// Its session went behind at `now`: what was on screen counts up to it, and nothing after — no time away
    /// waits for a key, and none counts again until a frame in front shows it.
    pub fn pause(&mut self, now: Instant) {
        self.advance(now);
        self.pending.clear();
        (self.shown, self.hand, self.mark, self.last_input, self.sight) =
            (Vec::new(), None, None, None, None);
    }

    /// It ends at `now`: what waited for an input never will.
    pub fn finish(&mut self, now: Instant) {
        self.advance(now);
        self.pending.clear();
        (self.shown, self.hand, self.sight) = (Vec::new(), None, None);
    }

    /// The changes it records: those seen.
    fn kept(&self) -> impl Iterator<Item = &Change> + Clone {
        self.changes.iter().filter(|c| c.seen)
    }

    /// File `file`'s changes `found` (by a worker, at `stamp`, the document still at it) — and `mode`, its change
    /// of mode's rule — followed. `was`: of the changes found there last, each one's lines now (moved with the
    /// edits since) and whether your own keys or pastes changed it since (`was`, from `Document.own`). Past
    /// `ALIGN_MAX` alike, the file's changes stay as they were, `unpaired`.
    fn apply(
        &mut self,
        file: &str,
        stamp: Stamp,
        mut found: Vec<Found>,
        mode: Option<&str>,
        was: &[(Range<usize>, bool)],
    ) {
        let last: Vec<usize> = (self.found.get(file))
            .filter(|f| f.stamp.0 == stamp.0 && f.of.len() == was.len())
            .map_or(Vec::new(), |f| f.of.clone());
        let mut yours: HashSet<usize> = HashSet::new();
        for (i, (lines, own)) in last.into_iter().zip(was) {
            self.changes[i].lines = lines.clone();
            if *own {
                yours.insert(i);
            }
        }
        let of = match self.unpaired.contains(file) {
            true => None,
            false => self.assign(file, &found, &yours),
        };
        let of = of.unwrap_or_else(|| {
            self.unpaired.insert(file.to_string());
            found.clear();
            Vec::new()
        });
        let was = self.found.get(file).and_then(|f| f.mode);
        let mode = mode.map(|rule| match was {
            Some(i) => i,
            None => {
                let i = self.add(file, (String::new(), 0), (0, 0), 0..0);
                let c = &mut self.changes[i];
                (c.mode, c.seen, c.grade, c.rule) = (true, false, Some("high"), Some(rule.to_string()));
                i
            }
        });
        self.found.insert(file.to_string(), FileFound { stamp, found, of, mode });
    }

    /// Which followed change each of file `file`'s changes `found` is, those not followed yet followed from
    /// now — by file, never by buffer (one closed and opened again is the same file). Alike ones (the same text)
    /// are paired one to one in their order in the file (`align`): the one `space v u` asked back is the one
    /// gone, never its twin; one taken back that comes back (`u`) takes its entry again. One `space v u` asked of
    /// that no change took is `undone`; a new text on a followed change no change took is that one. Of those
    /// `yours` (your own input changed their lines since), one changed is `edited`, one gone too. None: a group
    /// too big to weigh (`ALIGN_MAX`) — nothing paired rather than paired wrong.
    fn assign(&mut self, file: &str, found: &[Found], yours: &HashSet<usize>) -> Option<Vec<usize>> {
        let mine: Vec<usize> = (0..self.changes.len())
            .filter(|&i| {
                let c = &self.changes[i];
                c.file == file && !c.mode && matches!(c.outcome, None | Some("undone"))
            })
            .collect();
        // (each text: the changes followed with it and those found with it)
        let mut groups: HashMap<&str, (Vec<usize>, Vec<usize>)> = HashMap::new();
        for (k, x) in found.iter().enumerate() {
            groups.entry(x.text.as_str()).or_default().1.push(k);
        }
        for &i in &mine {
            if let Some(g) = groups.get_mut(self.changes[i].text.as_str()) {
                g.0.push(i);
            }
        }
        if groups.values().any(|g| g.0.len().saturating_mul(g.1.len()) > ALIGN_MAX) {
            return None;
        }
        // (where a text no change followed has is: a followed change edited there is left out first)
        let new: Vec<&Range<usize>> = groups
            .values()
            .filter(|g| g.0.is_empty())
            .flat_map(|g| g.1.iter().map(|&k| &found[k].lines))
            .collect();
        let mut of: Vec<Option<usize>> = vec![None; found.len()];
        let mut used: HashSet<usize> = HashSet::new();
        for (cs, xs) in groups.values_mut().filter(|g| !g.0.is_empty()) {
            crate::sorting::by_key(cs, |&i| (self.changes[i].lines.start, i));
            let left_out = |c: &Change| match () {
                _ if c.outcome.is_some() || c.undo => 0,
                _ if new.iter().any(|r| overlaps(r, &c.lines)) => 1,
                _ => 2,
            };
            let followed: Vec<Followed> = cs
                .iter()
                .map(|&i| {
                    let c = &self.changes[i];
                    Followed { at: c.lines.start, leave: left_out(c), gone: c.outcome.is_some() || c.undo }
                })
                .collect();
            let at: Vec<usize> = xs.iter().map(|&k| found[k].lines.start).collect();
            for (a, b) in align(&followed, &at) {
                of[xs[b]] = Some(cs[a]);
                used.insert(cs[a]);
            }
        }
        for &i in &mine {
            let c = &mut self.changes[i];
            c.present = used.contains(&i);
            match c.present {
                // (still there, or back: `u` after `space v u`)
                true => (c.undo, c.outcome) = (false, None),
                false if std::mem::take(&mut c.undo) && c.outcome.is_none() => c.outcome = Some("undone"),
                // (gone by your own keys: `xd`, its old text typed back)
                false if yours.contains(&i) && c.outcome.is_none() => c.edited = true,
                false => {}
            }
        }
        for (k, x) in found.iter().enumerate() {
            if of[k].is_some() {
                continue;
            }
            let gone = |i: &usize| {
                let c = &self.changes[*i];
                !used.contains(i) && c.outcome.is_none() && overlaps(&c.lines, &x.lines)
            };
            if let Some(i) = mine.iter().copied().find(gone) {
                let c = &mut self.changes[i];
                (c.text, c.present) = (x.text.clone(), true);
                of[k] = Some(i);
                used.insert(i);
            }
        }
        for &i in &mine {
            let c = &mut self.changes[i];
            if c.present && yours.contains(&i) {
                c.edited = c.text != c.text0;
            }
        }
        let of: Vec<usize> = found
            .iter()
            .zip(of)
            .map(|(x, i)| match i {
                Some(i) => {
                    let c = &mut self.changes[i];
                    (c.lines, c.rule) = (x.lines.clone(), x.rule.clone());
                    c.secret |= x.secret;
                    i
                }
                None => self.follow(file, x),
            })
            .collect();
        Some(of)
    }

    /// Change `x` followed from now on (not seen yet), its index.
    fn follow(&mut self, file: &str, x: &Found) -> usize {
        let i = self.add(file, (x.text.clone(), x.nth), (x.added, x.removed), x.lines.clone());
        let c = &mut self.changes[i];
        (c.rule, c.secret, c.seen) = (x.rule.clone(), x.secret, false);
        i
    }
}

/// Past this many pairs to weigh in a group of alike changes, the file's changes aren't paired any more
/// (`Watch.unpaired`): `align` is quadratic, and pairing in order alone paired them wrong.
const ALIGN_MAX: usize = 1 << 16;

/// A followed change as `align` weighs it: its first line, what leaving it out costs (0 asked back or ended,
/// 1 where a new text is, 2 else), and whether it's gone — taken back, or asked to be.
struct Followed {
    at: usize,
    leave: u32,
    gone: bool,
}

/// Changes followed paired with changes found (their first lines), both in their order in the file — order
/// kept, one to one: the pairing that leaves out least (one found left out — new — costs 2), then the one
/// whose pairs moved least. One gone pairs only where it was (back: `u`) — elsewhere it costs 3, more than a
/// change new there. Pairs of indices into the two.
fn align(followed: &[Followed], found: &[usize]) -> Vec<(usize, usize)> {
    let (n, m) = (followed.len(), found.len());
    let w = m + 1;
    let pair = |i: usize, j: usize| {
        let f = &followed[i];
        (if f.gone && f.at != found[j] { 3 } else { 0 }, f.at.abs_diff(found[j]))
    };
    // (the least cost from (i, j) to the ends: what's left out, then how far the pairs moved)
    let mut best = vec![(0u32, 0usize); (n + 1) * w];
    for i in (0..=n).rev() {
        for j in (0..=m).rev() {
            if i == n && j == m {
                continue;
            }
            let mut b = (u32::MAX, usize::MAX);
            if i < n && j < m {
                let (t, (c, d)) = (best[(i + 1) * w + j + 1], pair(i, j));
                b = b.min((t.0 + c, t.1 + d));
            }
            if i < n {
                let t = best[(i + 1) * w + j];
                b = b.min((t.0 + followed[i].leave, t.1));
            }
            if j < m {
                let t = best[i * w + j + 1];
                b = b.min((t.0 + 2, t.1));
            }
            best[i * w + j] = b;
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n || j < m {
        let here = best[i * w + j];
        if i < n && j < m {
            let (t, (c, d)) = (best[(i + 1) * w + j + 1], pair(i, j));
            if (t.0 + c, t.1 + d) == here {
                out.push((i, j));
                (i, j) = (i + 1, j + 1);
                continue;
            }
        }
        if i < n && (best[(i + 1) * w + j].0 + followed[i].leave, best[(i + 1) * w + j].1) == here {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The names a change of mode is graded by (`risk::high`'s): the only rule a code review's change of mode has.
const MODES: [&str; 2] = ["now executable", "now a symlink"];

#[cfg(test)]
thread_local! {
    /// How many times a file's changes were all found on this thread (`found_of`) — never the one that draws.
    static FOUND_HERE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A file's text as its changes are found in it (on a worker): the document's text, its review base and its
/// diff then.
struct Text {
    text: ropey::Rope,
    base: Option<ropey::Rope>,
    hunks: Vec<crate::git::Hunk>,
}

impl Text {
    fn of(d: &crate::document::Document) -> Text {
        Text { text: d.text.clone(), base: d.review_base.clone(), hunks: d.git_hunks.clone() }
    }

    /// Hunk `h`'s lines: removed, added.
    fn lines(&self, h: &crate::git::Hunk) -> (Vec<String>, Vec<String>) {
        let n = self.text.len_lines();
        let removed: Vec<String> = (self.base.as_ref())
            .map(|b| h.base.clone().filter(|&l| l < b.len_lines()).map(|l| line(b, l)).collect())
            .unwrap_or_default();
        let added: Vec<String> = h.lines.clone().filter(|&l| l < n).map(|l| line(&self.text, l)).collect();
        (removed, added)
    }
}

/// The changes of `t` (of `file`, at `path` in the checkout `root`; graded by `rules`). On a worker, never on a
/// key or a frame: hashing, the token check and the packs' patterns over every line.
fn found_of(
    t: &Text,
    (file, path, root): (&str, &std::path::Path, &std::path::Path),
    rules: &crate::risk::Rules,
) -> Vec<Found> {
    #[cfg(test)]
    FOUND_HERE.with(|n| n.set(n.get() + 1));
    let lang = crate::risk::lang_of(file);
    let mut found: Vec<Found> = Vec::new();
    let mut alike: HashMap<String, usize> = HashMap::new();
    for h in &t.hunks {
        let (removed, added) = t.lines(h);
        let text = text_id(file, removed.iter().map(String::as_str), added.iter().map(String::as_str));
        let nth = alike.entry(text.clone()).or_default();
        let nth = std::mem::replace(nth, *nth + 1);
        let all = || removed.iter().chain(&added).map(String::as_str);
        let secret = secret(root, Some(path), all());
        let (counts, at) = ((added.len(), removed.len()), h.lines.start);
        let rule =
            crate::risk::rule(file, lang, &crate::risk::Hunk { at, removed, added, mode: None }, rules);
        let (added, removed) = counts;
        found.push(Found { lines: h.lines.clone(), text, nth, added, removed, rule, secret });
    }
    found
}

// ── Following reviews ────────────────────────────────────────────────────────────────────────────────

/// A path as the record names it: from the session's root, else whole.
fn rel(ed: &Editor, path: Option<&std::path::Path>) -> String {
    let Some(p) = path else { return String::new() };
    p.strip_prefix(&ed.session.root).unwrap_or(p).display().to_string()
}

/// The newest note's id (0: none).
fn notes_max(ed: &Editor) -> u64 {
    ed.session.notes.list.iter().map(|n| n.id).max().unwrap_or(0)
}

/// A watch over edit review `rev`: a change per hunk that changes something.
fn edit_watch(ed: &Editor, rev: &crate::llm::Review, now: Instant) -> Watch {
    let doc = ed.docs.iter().find(|d| d.id == rev.doc_id);
    let file = rel(ed, doc.and_then(|d| d.path.as_deref()));
    let mut w = Watch::new(Of::Edit(rev.generation), "edit", now, notes_max(ed));
    let mut texts: Vec<String> = Vec::new();
    for (k, h) in rev.hunks.iter().enumerate().filter(|(_, h)| h.old != h.new) {
        let text = text_id(&file, h.old.lines(), h.new.lines());
        let nth = texts.iter().filter(|t| **t == text).count();
        texts.push(text.clone());
        let lines = doc.map_or(0..0, |d| {
            let at = |b: usize| d.text.byte_to_line(b.min(d.text.len_bytes()));
            at(h.from)..at(h.to)
        });
        let i = w.add(&file, (text, nth), (h.new.lines().count(), h.old.lines().count()), lines);
        let path = doc.and_then(|d| d.path.as_deref());
        w.changes[i].secret = secret(&ed.session.root, path, h.old.lines().chain(h.new.lines()));
        w.hunks.push(k);
        w.decided.push(h.decision);
    }
    w
}

/// What a code review is of, as the record says it.
fn kind_of(base: &crate::codereview::Base) -> &'static str {
    match base {
        crate::codereview::Base::Head => "uncommitted",
        crate::codereview::Base::Branch { .. } => "branch",
        crate::codereview::Base::Turn { .. } => "turn",
        crate::codereview::Base::Replay { .. } => "replay",
    }
}

/// After each frame (`term::render`): the front session's reviews — what of them is on screen and in hand.
pub fn after_frame(ed: &mut Editor) {
    let now = crate::clock::now();
    // (a session gone behind: its reviews stop counting — the time in another session isn't theirs)
    for l in &mut ed.sessions {
        for w in [l.reviews.edit.as_mut(), l.reviews.code.as_mut()].into_iter().flatten() {
            if w.mark.is_some() {
                w.pause(now);
            }
        }
    }
    edit_frame(ed, now);
    code_frame(ed, now);
}

fn edit_frame(ed: &mut Editor, now: Instant) {
    let generation = ed.session.review.as_ref().filter(|r| r.ready && r.hunks.iter().any(|h| h.old != h.new));
    let generation = generation.map(|r| r.generation);
    let same = ed.session.reviews.edit.as_ref().map(|w| w.of) == generation.map(Of::Edit);
    if !same {
        // (one that went another way than `edit_ended`: what it was is gone with it — left)
        if let Some(w) = ed.session.reviews.edit.take() {
            ended(ed, w, now);
        }
        if let Some(rev) = ed.session.review.as_ref().filter(|_| generation.is_some()) {
            let w = edit_watch(ed, rev, now);
            began(ed, &w);
            ed.session.reviews.edit = Some(w);
        }
    }
    let (Some(rev), Some(mut w)) = (ed.session.review.as_ref(), ed.session.reviews.edit.take()) else {
        return;
    };
    let rows = ed.screen.review_rows.borrow();
    let mut shown: Vec<usize> =
        rows.iter().filter_map(|&(_, k)| w.hunks.iter().position(|&h| h == k)).collect::<Vec<_>>();
    crate::sorting::sort(&mut shown);
    shown.dedup();
    let hand = w.hunks.iter().position(|&h| h == rev.current).filter(|_| !shown.is_empty());
    // (a change answered since the last frame: said to watchers)
    let now_decided: Vec<crate::llm::Decision> = w.hunks.iter().map(|&k| rev.hunks[k].decision).collect();
    let newly: Vec<usize> = (0..now_decided.len()).filter(|&i| now_decided[i] != w.decided[i]).collect();
    drop(rows);
    w.decided = now_decided;
    for i in newly {
        let c = &w.changes[i];
        let outcome = decision_word(w.decided[i], false);
        let said = format!("review — {} change {} of {} {outcome}", c.file, i + 1, w.changes.len());
        // (its index, not its id: the id leaves tarae only salted, into the record)
        let ev = json!({ "what": "decided", "review": w.id, "file": c.file, "change": i + 1,
                         "outcome": outcome, "said": said });
        say(ed, ev);
    }
    w.frame(now, shown, hand);
    ed.session.reviews.edit = Some(w);
}

fn decision_word(d: crate::llm::Decision, conflict: bool) -> &'static str {
    match d {
        crate::llm::Decision::Accept if !conflict => "accepted",
        crate::llm::Decision::Reject => "rejected",
        _ => "left",
    }
}

fn code_frame(ed: &mut Editor, now: Instant) {
    let Some(r) = ed.session.code_review.as_ref() else {
        // (one that ended another way than `codereview::end`: left)
        if let Some(w) = ed.session.reviews.code.take() {
            ended(ed, w, now);
        }
        return;
    };
    let (of, kind) = (Of::Code(r.started), kind_of(&r.base));
    if ed.session.reviews.code.as_ref().is_none_or(|w| w.of != of) {
        if let Some(w) = ed.session.reviews.code.take() {
            ended(ed, w, now);
        }
        let w = Watch::new(of, kind, now, notes_max(ed));
        began(ed, &w);
        ed.session.reviews.code = Some(w);
    }
    look(ed, now);
}

/// A file's text given to a worker to find its changes in (`found_of`), at `at`.
struct Ask {
    file: String,
    path: std::path::PathBuf,
    at: Stamp,
    text: Text,
}

/// What of the code review is on screen and in hand now — at a frame, or as a file's changes land (the frame
/// on screen then) — and a worker asked to find the changes of each file in sight whose changes aren't found
/// for its text yet.
fn look(ed: &mut Editor, now: Instant) {
    let Some(mut w) = ed.session.reviews.code.take() else { return };
    // (an edit's review drawn over it: none of its changes is in sight)
    let (shown, hand, asks) = match ed.screen.review_rows.borrow().is_empty() {
        true => code_shown(ed, &mut w),
        false => {
            w.sight = None;
            (Vec::new(), None, Vec::new())
        }
    };
    w.frame(now, shown, hand);
    if let Some(r) = ed.session.code_review.as_ref() {
        for a in asks {
            w.asked.insert(a.file.clone(), a.at);
            let (of, root, rules) = (w.of, r.root.clone(), r.rules.clone());
            ed.events.jobs().spawn(move || {
                let found = found_of(&a.text, (&a.file, &a.path, &root), &rules);
                // (the text let go here, not on the main loop)
                let Ask { file, at, .. } = a;
                move |ed: &mut Editor| landed(ed, of, file, at, found)
            });
        }
    }
    ed.session.reviews.code = Some(w);
}

/// A worker found file `file`'s changes (`at` the text it was given): followed — only while the document is
/// still at that text, else they're of a text gone (a take-back since, an edit) and the next frame asks again —,
/// graded as they are now, and what's in sight looked at again on the frame on screen — nothing on screen
/// changed for it.
fn landed(ed: &mut Editor, of: Of, file: String, at: Stamp, found: Vec<Found>) {
    let Some(mut w) = ed.session.reviews.code.take() else { return };
    let mode = ed.session.code_review.as_ref().filter(|r| Of::Code(r.started) == of).and_then(|r| {
        let f = r.files.iter().find(|f| f.rel == file)?;
        Some(mode_rule(r, &f.path))
    });
    if w.of == of
        && let Some(mode) = mode
    {
        w.asked.remove(&file);
        if let Some(d) = ed.docs.iter_mut().find(|d| (d.id, d.version(), d.git_version) == at) {
            let was = was(d, w.found.get(&file).map(|f| f.stamp));
            let place = |l: usize| d.text.line_to_byte(l.min(d.text.len_lines()));
            let spans = found.iter().map(|x| place(x.lines.start)..place(x.lines.end)).collect();
            w.apply(&file, at, found, mode, &was);
            // (from here, the places of the changes as found, and your input since)
            let kept = w.found.get(&file).is_some_and(|f| !f.found.is_empty());
            d.own.found = Some((at.1, if kept { spans } else { Vec::new() }));
            d.own.typed.clear();
            grade_file(ed, &mut w, &file);
            w.sight = None;
        }
    }
    ed.session.reviews.code = Some(w);
    if !ed.behind {
        look(ed, crate::clock::now());
    }
    ed.unchanged = true;
}

/// File `path`'s change of mode as the review graded it: its rule's name.
fn mode_rule(r: &crate::codereview::CodeReview, path: &std::path::Path) -> Option<&'static str> {
    let g = r.grades.get(path)?.iter().find(|g| g.mode)?;
    MODES.into_iter().find(|m| *m == g.why)
}

/// File `file`'s changes in its diff graded as the review grades them now — one gone keeps its last.
fn grade_file(ed: &Editor, w: &mut Watch, file: &str) {
    let Some(r) = ed.session.code_review.as_ref() else { return };
    let Some(f) = r.files.iter().find(|f| f.rel == file) else { return };
    let graded = r.graded(ed, f);
    let lines = of_lines(&graded);
    let now = |c: &&mut Change| c.file == file && c.present && !c.mode && !c.undo && c.outcome.is_none();
    for c in w.changes.iter_mut().filter(now) {
        (c.grade, c.rule) = grade_rule(&on_lines(&lines, &c.lines), c.rule.take());
    }
}

/// The code review's changes on screen this frame (as `codereview::drawn_sync` finds its documents in the
/// panes) and the one the cursor is in (`gitmenu::hunk_at`'s, as `space v u` takes it); a document whose
/// changes aren't found for its text yet keeps what it showed — and is asked of a worker (`Ask`). Worked out
/// again only when a document's text or diff, the rows drawn or the cursor changed.
fn code_shown(ed: &Editor, w: &mut Watch) -> (Vec<usize>, Option<usize>, Vec<Ask>) {
    let Some(r) = ed.session.code_review.as_ref() else { return (Vec::new(), None, Vec::new()) };
    let (focused, cursor) = (ed.doc().id, ed.cursor_line());
    let mut sight = Sight { panes: Vec::new(), focused, cursor };
    for ((_, _, _, rows), doc) in ed.screen.panes.iter().zip(&ed.screen.pane_docs) {
        let Some(d) = ed.docs.iter().find(|d| d.id == *doc) else { continue };
        let mut lines: Vec<usize> = rows.iter().map(|row| row.0).collect();
        crate::sorting::sort(&mut lines);
        lines.dedup();
        sight.panes.push((d.id, (d.version(), d.git_version), lines));
    }
    if w.sight.as_ref() == Some(&sight) {
        return (w.shown.clone(), w.hand, Vec::new());
    }
    let (mut shown, mut hand, mut asks) = (Vec::new(), None, Vec::new());
    for (id, _, lines) in &sight.panes {
        let Some(d) = ed.docs.iter().find(|d| d.id == *id) else { continue };
        let Some(f) = d.path.as_ref().and_then(|p| r.files.iter().find(|f| f.path == *p)) else { continue };
        let current = d.review_base.is_some() && d.git_version == d.version();
        let stamp = (d.id, d.version(), d.git_version);
        let Some(file) = w.found.get(&f.rel).filter(|x| current && x.stamp == stamp) else {
            if current && !w.asked.contains_key(&f.rel) {
                let (file, path) = (f.rel.clone(), f.path.clone());
                asks.push(Ask { file, path, at: stamp, text: Text::of(d) });
            }
            let mine = |i: &usize| w.changes[*i].file == f.rel;
            shown.extend(w.shown.iter().copied().filter(mine));
            if d.id == focused {
                hand = hand.or(w.hand.filter(mine));
            }
            continue;
        };
        // (a change drawn: one of its lines among those drawn — lines only removed, the line above or below)
        let drawn = |x: &Range<usize>| {
            let (a, b) = match x.is_empty() {
                true => (x.start.saturating_sub(1), x.start + 1),
                false => (x.start, x.end),
            };
            lines.get(lines.partition_point(|&l| l < a)).is_some_and(|&l| l < b)
        };
        for (x, &i) in file.found.iter().zip(&file.of) {
            if drawn(&x.lines) {
                shown.push(i);
            }
        }
        // (its change of mode: shown at its first line, in hand there when no change of lines is)
        if let Some(i) = file.mode.filter(|_| lines.first() == Some(&0)) {
            shown.push(i);
        }
        if d.id == focused {
            let at = crate::gitmenu::hunk_at(&d.git_hunks, cursor);
            let k = at.and_then(|h| d.git_hunks.iter().position(|x| std::ptr::eq(x, h)));
            hand = hand.or(k.and_then(|k| file.of.get(k).copied()));
            hand = hand.or(file.mode.filter(|_| cursor == 0));
        }
    }
    crate::sorting::sort(&mut shown);
    shown.dedup();
    w.sight = Some(sight);
    (shown, hand, asks)
}

/// Every take-back of a change (`Editor::git_reset_hunk`: `space v u`, the git tab's `d`, the palette's "Undo
/// this change"): what `reset` takes back is decided as it decides it — the hunk at the cursor as the key runs
/// (`gitmenu::hunk_at`: a deletion on the line above its gap), never what the last frame had in hand (keys in
/// one batch, a macro). That change is undone once its diff no longer has it, or as the review ends
/// (`outcomes`), with its grade and rule as they were. Nothing taken back (no change there), or no change
/// found there yet: nothing asked.
pub fn undo_asked(ed: &mut Editor, reset: impl FnOnce(&mut Editor)) {
    let version = ed.doc().version();
    let asked = asked_back(ed);
    reset(ed);
    if ed.doc().version() == version {
        return;
    }
    let (Some((i, grade, rule)), Some(w)) = (asked, ed.session.reviews.code.as_mut()) else { return };
    let c = &mut w.changes[i];
    (c.undo, c.seen, c.grade, c.rule) = (true, true, grade, rule);
}

/// What a take-back takes, as `git_reset_hunk` finds it: the hunk at the cursor now — the followed change
/// there as the review last found the file's changes (their places moved with every edit since: `was`), and
/// its grade and rule now. None found there yet: none — no change's text is hashed on a key.
fn asked_back(ed: &Editor) -> Option<(usize, Option<&'static str>, Option<String>)> {
    let (r, w, d) = (ed.session.code_review.as_ref()?, ed.session.reviews.code.as_ref()?, ed.doc());
    let f = r.files.iter().find(|f| d.path.as_ref() == Some(&f.path))?;
    let h = crate::gitmenu::hunk_at(&d.git_hunks, ed.cursor_line())?;
    let file = w.found.get(&f.rel).filter(|x| x.stamp.0 == d.id)?;
    let was = was(d, Some(file.stamp));
    // (the one on the hunk's lines; else the one sharing them)
    let at = |same: bool| {
        let on = |l: &Range<usize>| if same { *l == h.lines } else { overlaps(l, &h.lines) };
        was.iter().position(|(l, _)| on(l))
    };
    let i = *file.of.get(at(true).or_else(|| at(false))?)?;
    let graded = r.graded(ed, f);
    let (grade, rule) = grade_rule(&on_lines(&of_lines(&graded), &h.lines), w.changes[i].rule.clone());
    Some((i, grade, rule))
}

/// Of the changes found in document `d` when its file's changes were last found (`last`, kept with the document
/// then: `Document.own`), each one's lines now — moved with every edit since — and whether your own keys or
/// pastes changed it since. None when the document doesn't keep that find.
fn was(d: &crate::document::Document, last: Option<Stamp>) -> Vec<(Range<usize>, bool)> {
    let kept = last.and_then(|s| d.own.found.as_ref().filter(|(v, _)| (s.0, s.1) == (d.id, *v)));
    let Some((_, spans)) = kept else { return Vec::new() };
    let wide = |r: &Range<usize>| r.end.max(r.start + 1);
    // (yours: sorted, apart — their lines' ends in order too)
    let typed: Vec<Range<usize>> = d.own.typed.iter().map(|r| span_lines(&d.text, r)).collect();
    spans
        .iter()
        .map(|s| {
            let l = span_lines(&d.text, s);
            let first = typed.partition_point(|t| wide(t) <= l.start);
            (l.clone(), typed.get(first).is_some_and(|t| overlaps(t, &l)))
        })
        .collect()
}

/// The lines bytes `r` of `text` are on — an empty range its line, as `overlaps` reads it.
fn span_lines(text: &ropey::Rope, r: &Range<usize>) -> Range<usize> {
    let line = |b: usize| text.byte_to_line(b.min(text.len_bytes()));
    let start = line(r.start);
    start..if r.end > r.start { line(r.end - 1) + 1 } else { start }
}

/// Of a file's graded changes, those of lines (not its change of mode), in their order.
fn of_lines(graded: &[crate::risk::Graded]) -> Vec<&crate::risk::Graded> {
    let mut lines: Vec<&crate::risk::Graded> = graded.iter().filter(|g| !g.mode).collect();
    crate::sorting::by_key(&mut lines, |g| g.at);
    lines
}

/// Of graded changes in their order (`of_lines`: none on another's lines), those on `lines`.
fn on_lines<'a>(graded: &[&'a crate::risk::Graded], lines: &Range<usize>) -> Vec<&'a crate::risk::Graded> {
    let to = lines.end.max(lines.start + 1);
    let past = graded.partition_point(|g| g.at < to);
    let mut on: Vec<&crate::risk::Graded> = graded[..past]
        .iter()
        .rev()
        .take_while(|g| overlaps(&(g.at..g.at + g.lines), lines))
        .copied()
        .collect();
    on.reverse();
    on
}

/// The grade of a change with the graded changes `on` its lines and, high, the rule that made it so by its
/// name — never what it matched or showed (`why` quotes the line: never kept); `rule` its own (`risk::rule`).
fn grade_rule(on: &[&crate::risk::Graded], rule: Option<String>) -> (Option<&'static str>, Option<String>) {
    use crate::risk::Grade;
    let high: Vec<&&crate::risk::Graded> = on.iter().filter(|g| g.grade == Grade::High).collect();
    let since = |what: &str| high.iter().any(|g| g.why.contains(&format!("{what} since your look")));
    let rule = match () {
        _ if high.is_empty() => None,
        _ if since("taken out") => Some("taken out since a look".into()),
        _ if since("moved") => Some("moved since a look".into()),
        _ => rule,
    };
    (on.iter().map(|g| g.grade).max().map(Grade::word), rule)
}

/// Edit review `rev` ends: answered (`llm::finish`) — each change as you answered it, `accepted` only when
/// `applied` (its buffer gone, nothing landed: `left`); `closed` (cancelled, or another review in its place:
/// `llm::close_review` applies nothing) — each `left`, whatever was answered.
pub fn edit_ended(ed: &mut Editor, rev: &crate::llm::Review, closed: bool, applied: bool) {
    if !rev.ready || rev.hunks.iter().all(|h| h.old == h.new) {
        return;
    }
    let now = crate::clock::now();
    let mut w = match ed.session.reviews.edit.take() {
        Some(w) if w.of == Of::Edit(rev.generation) => w,
        other => {
            if let Some(o) = other {
                ended(ed, o, now);
            }
            // (answered before any frame drew it: its changes, no times)
            edit_watch(ed, rev, now)
        }
    };
    w.finish(now);
    for (c, &k) in w.changes.iter_mut().zip(&w.hunks) {
        let h = &rev.hunks[k];
        c.outcome = Some(match decision_word(h.decision, h.conflict) {
            _ if closed => "left",
            "accepted" if !applied => "left",
            word => word,
        });
    }
    ended(ed, w, now);
}

/// The code review ends (`codereview::end`, before it's taken): each change's grade and what it ended as.
pub fn code_ended(ed: &mut Editor) {
    let Some(mut w) = ed.session.reviews.code.take() else { return };
    // (no review follows its documents now: their edits move nothing for it)
    for d in ed.docs.iter_mut().filter(|d| d.own.followed()) {
        d.own = crate::document::Own::default();
    }
    let now = crate::clock::now();
    w.finish(now);
    if let Some(r) = ed.session.code_review.as_ref().filter(|r| Of::Code(r.started) == w.of) {
        outcomes(ed, r, &mut w);
    }
    ended(ed, w, now);
}

/// A code review's changes: their grades (high: looked at or not), viewed marks, your notes and the
/// reviewer's findings on them — and how many changes it had in all.
fn outcomes(ed: &Editor, r: &crate::codereview::CodeReview, w: &mut Watch) {
    use crate::notes::By;
    use crate::risk::Grade;
    let mut total = 0;
    for f in &r.files {
        let graded = r.graded(ed, f);
        let lines = of_lines(&graded);
        let modes: Vec<&crate::risk::Graded> = graded.iter().filter(|g| g.mode).collect();
        // (what it had as it ended, as the record counts it: a file read here, its changes still there — a
        // change of mode one of them; else what the review's worker found)
        let worker = |g: &&crate::risk::Graded| g.added + g.removed > 0 || g.mode;
        total += match w.found.contains_key(&f.rel) {
            true => w.changes.iter().filter(|c| c.file == f.rel && c.present).count(),
            false => r.grades.get(&f.path).map_or(0, |g| g.iter().filter(worker).count()),
        };
        let viewed = r.viewed(ed, f);
        for c in w.changes.iter_mut().filter(|c| c.file == f.rel) {
            // (one gone — taken back, by hand, by a checkpoint — keeps its grade and rule as last seen: what's on
            // its old lines now isn't it; one asked back is undone though its diff wasn't read again since)
            let undone = c.undo || c.outcome == Some("undone");
            let on = match () {
                _ if c.mode => modes.clone(),
                _ if c.present && !undone => on_lines(&lines, &c.lines),
                _ => Vec::new(),
            };
            if c.present && !undone && !c.mode {
                (c.grade, c.rule) = grade_rule(&on, c.rule.take());
            }
            let high: Vec<&&crate::risk::Graded> = on.iter().filter(|g| g.grade == Grade::High).collect();
            let looked = !high.is_empty() && high.iter().all(|g| g.looked_at(&ed.session.looked));
            let notes =
                ed.session.notes.list.iter().filter(|n| n.path == f.path && overlaps(&n.lines, &c.lines));
            let notes: Vec<&crate::notes::Note> = notes.collect();
            let yours =
                |n: &&crate::notes::Note| n.id > w.notes_before && n.thread.iter().any(|e| e.by == By::You);
            let commented = notes.iter().any(yours);
            c.finding = notes.iter().any(|n| n.thread.first().is_some_and(|e| e.by == By::Reviewer));
            c.outcome = c.outcome.or(Some(match () {
                _ if undone => "undone",
                _ if c.edited => "edited",
                _ if commented => "commented",
                _ if looked => "looked",
                _ if viewed => "viewed",
                _ => "left",
            }));
        }
    }
    w.total = Some(total);
    // (each finding of its reviewer's — when the reviewer read for this review: of its base, begun while it was
    // open or this one opened while it read; never an earlier run's into a later review)
    if ed.session.reviewer.as_ref().is_some_and(|x| x.review == Some(r.started)) {
        for n in crate::reviewer::findings_in(&ed.session).unwrap_or_default() {
            let file = match r.files.iter().find(|f| f.path == n.path) {
                Some(f) => f.rel.clone(),
                None => rel(ed, Some(&n.path)),
            };
            let on = w.kept().find(|c| c.file == file && overlaps(&c.lines, &n.lines));
            let mut finding = serde_json::Map::new();
            finding.insert("by".into(), json!("reviewer"));
            finding.insert("sent".into(), json!(!crate::notes::unsent(n)));
            finding.insert("answered".into(), json!(crate::reviewer::answered(n)));
            let change = on.filter(|c| !c.secret && !c.mode).map(|c| c.id.clone());
            w.findings.push(Finding { file, change, finding });
        }
    }
}

/// Its entries into the record, said to watchers.
fn ended(ed: &mut Editor, mut w: Watch, now: Instant) {
    w.finish(now);
    let session = ed.session.label().to_string();
    let entries = entries(&w, &session, crate::clock::system(), review_findings(ed, &w));
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let changes = w.kept().count();
    for c in w.kept() {
        let o = c.outcome.unwrap_or("left");
        match counts.iter_mut().find(|x| x.0 == o) {
            Some(x) => x.1 += 1,
            None => counts.push((o, 1)),
        }
    }
    let outcomes: serde_json::Map<String, Value> =
        counts.iter().map(|(o, n)| (o.to_string(), json!(n))).collect();
    let list: Vec<String> = counts.iter().map(|(o, n)| format!("{n} {o}")).collect();
    let said = format!(
        "review ended ({}) — {} change{}, {} active{}",
        w.kind,
        changes,
        if changes == 1 { "" } else { "s" },
        crate::term::duration(w.active.as_secs_f32()),
        if list.is_empty() { String::new() } else { format!(": {}", list.join(" · ")) }
    );
    say(
        ed,
        json!({ "what": "ended", "review": w.id, "kind": w.kind, "changes": changes,
                "active_ms": ms(w.active), "outcomes": outcomes, "said": said }),
    );
    ed.reviews_record(entries);
}

fn began(ed: &mut Editor, w: &Watch) {
    let said = format!("review began ({})", w.kind);
    say(ed, json!({ "what": "began", "review": w.id, "kind": w.kind, "said": said }));
}

/// A `review` event for watchers (`tarae state --watch review`), its session's.
fn say(ed: &mut Editor, mut ev: Value) {
    let id = crate::uistate::session_id(ed.session.id);
    let said = format!("{id} {}: {}", ed.session.label(), ev["said"].as_str().unwrap_or_default());
    ev["event"] = json!("review");
    ev["session"] = json!(id);
    ev["said"] = json!(said);
    ed.ui_said(ev);
}

/// The reviewer's findings and those not sent yet — when it read for this review (`Reviewer.review`).
fn review_findings(ed: &Editor, w: &Watch) -> Option<(usize, usize)> {
    let review = ed.session.reviewer.as_ref()?.review?;
    (w.of == Of::Code(review))
        .then(|| crate::reviewer::review_tally(ed))
        .flatten()
        .map(|t| (t.findings, t.unsent))
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Watch `w`'s entries: one per change it followed, one per finding, then the review's. No text of the code —
/// its file, its id, counts, grades, times; a finding's place and its `finding` object, never what it says.
pub fn entries(
    w: &Watch,
    session: &str,
    at: std::time::SystemTime,
    findings: Option<(usize, usize)>,
) -> Vec<Value> {
    let at = crate::log::stamp(at);
    let mut out: Vec<Value> = w
        .kept()
        .map(|c| {
            pruned(json!({
                "v": RECORD_V, "at": at, "session": session, "review": w.id, "kind": w.kind, "entry": "change",
                // (a key-named file's or a token-like line's change: no id at all)
                "file": c.file, "change": (!c.secret && !c.mode).then_some(&c.id),
                "lines": { "added": c.added, "removed": c.removed },
                "grade": c.grade, "rule": c.rule, "drawn_ms": c.drawn.map(ms), "active_ms": ms(c.active),
                "in_hand_ms": ms(c.in_hand), "outcome": c.outcome.unwrap_or("left"), "edited": c.edited,
                "finding": c.finding, "lens": [],
                // (past `ALIGN_MAX` alike: as it was when the file's changes stopped being paired)
                "unpaired": w.unpaired.contains(&c.file).then_some("too many alike changes"),
            }))
        })
        .collect();
    // (its object whole — `false` says something there)
    out.extend(w.findings.iter().map(|f| {
        let mut v = pruned(json!({
            "v": RECORD_V, "at": at, "session": session, "review": w.id, "kind": w.kind, "entry": "finding",
            "file": f.file, "change": f.change,
        }));
        v["finding"] = Value::Object(f.finding.clone());
        v
    }));
    let (added, removed) = w.kept().fold((0, 0), |(a, r), c| (a + c.added, r + c.removed));
    out.push(pruned(json!({
        "v": RECORD_V, "at": at, "session": session, "review": w.id, "kind": w.kind, "entry": "review",
        "changes": w.kept().count(), "total": w.total, "lines": { "added": added, "removed": removed },
        "active_ms": ms(w.active), "findings": findings.map(|f| f.0), "unsent": findings.map(|f| f.1),
        "lens": [], "restarted": w.restarted,
    })));
    out
}

/// `v` without its null, false and empty-text fields — a list stays, empty too (`lens`: which arm of the A/B a
/// line was in says something empty).
fn pruned(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| match x {
            Value::Null | Value::Bool(false) => false,
            Value::String(s) => !s.is_empty(),
            _ => true,
        });
    }
    v
}

/// The record's months in `dir` past `cards::KEEP_MONTHS` removed (at start, on a worker) — never through a
/// link at `dir` (`no_link`). The months removed.
pub fn prune(dir: &std::path::Path, now: std::time::SystemTime) -> std::io::Result<Vec<String>> {
    no_link(dir).and_then(|_| crate::cards::prune(dir, now))
}

// ── The state ─────────────────────────────────────────────────────────────────────────────────────────

/// A session's review under way as `tarae state` says it (`sessions[].review`): an edit's review if one is up,
/// else its code review's. Times in whole seconds, and no record id (it names the process): the state is pinned
/// by a snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub kind: &'static str,
    /// Changes seen so far · those answered (an edit's review) or undone (a code review).
    pub changes: usize,
    pub decided: usize,
    /// A code review in front: its high-risk changes looked at, of how many.
    pub looked: Option<(usize, usize)>,
    pub active_s: u64,
    /// The change in hand: its file, its first line (1-based) and its time in hand (s).
    pub in_hand: Option<(String, usize, u64)>,
}

impl Run {
    /// Session `l`'s review under way (`front`: it's `ed.session`, whose grades can be read) — None: none.
    pub fn of(ed: &Editor, l: &crate::session::Session, front: bool) -> Option<Run> {
        let w = l.reviews.edit.as_ref().or(l.reviews.code.as_ref())?;
        let decided = match w.of {
            Of::Edit(_) => {
                let rev = l.review.as_ref();
                let undecided = |&k: &usize| rev.and_then(|r| r.hunks.get(k)).map(|h| h.decision);
                w.hunks
                    .iter()
                    .filter(|k| undecided(k).is_some_and(|d| d != crate::llm::Decision::Undecided))
                    .count()
            }
            Of::Code(_) => w.kept().filter(|c| c.outcome.is_some()).count(),
        };
        let looked = match (w.of, l.code_review.as_ref()) {
            (Of::Code(_), Some(r)) if front => {
                let highs = r.files.iter().map(|f| {
                    let g = r.graded(ed, f);
                    let high = g.iter().filter(|g| g.grade == crate::risk::Grade::High);
                    let n = high.clone().count();
                    (high.filter(|g| g.looked_at(&l.looked)).count(), n)
                });
                Some(highs.fold((0, 0), |(a, b), (x, y)| (a + x, b + y)))
            }
            _ => None,
        };
        let in_hand =
            w.hand.map(|h| &w.changes[h]).map(|c| (c.file.clone(), c.lines.start + 1, c.in_hand.as_secs()));
        Some(Run {
            kind: w.kind,
            changes: w.kept().count(),
            decided,
            looked,
            active_s: w.active.as_secs(),
            in_hand,
        })
    }

    pub fn json(&self) -> Value {
        json!({
            "kind": self.kind, "changes": self.changes, "decided": self.decided,
            "looked": self.looked.map(|(l, n)| json!({ "looked": l, "high": n })),
            "active_s": self.active_s,
            "in_hand": self.in_hand.as_ref().map(|(f, l, s)| json!({ "file": f, "line": l, "s": s })),
        })
    }

    /// One line: `turn · 3 changes · 1 decided · 1 of 2 high looked at · active 1m 12s · in hand a.rs:10 (18s)`.
    pub fn text(&self) -> String {
        let d = |s: u64| crate::term::duration(s as f32);
        let mut out = format!(
            "{} · {} change{} · {} decided",
            self.kind,
            self.changes,
            if self.changes == 1 { "" } else { "s" },
            self.decided
        );
        if let Some((l, n)) = self.looked.filter(|l| l.1 > 0) {
            out += &format!(" · {l} of {n} high looked at");
        }
        out += &format!(" · active {}", d(self.active_s));
        if let Some((f, l, s)) = &self.in_hand {
            out += &format!(" · in hand {f}:{l} ({})", d(*s));
        }
        out
    }
}

// ── The record ────────────────────────────────────────────────────────────────────────────────────────

/// The record's key: 32 random bytes, 0600, made once — the same change keeps one id on this machine, and an id
/// can't be tried against candidate lines without it (`keyed`). Beside the record's folder in tarae's state,
/// never in it: a copy of the record (the lens's A/B, `cargo xtask reviews`) carries no key.
pub const KEY: &str = "reviews.key";

/// The key of the record kept in `dir`.
pub fn key_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.with_file_name(KEY)
}

/// `dir`, the record's folder, isn't a link (there or not): the record and the months pruned from it are never
/// somewhere a link sends them.
pub fn no_link(dir: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => {
            Err(std::io::Error::other("the record's folder is a link — not followed"))
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The record's folder `dir` and its key (`KEY`, beside it), made if not there (0700 — a link at `dir`
/// refused): the key written whole under a name of its own, then linked into place — a tarae beside this one
/// reads the whole of the first one made, never half. Neither is opened through a link. On a worker.
fn key(dir: &std::path::Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read as _;
    use std::os::unix::fs::DirBuilderExt as _;
    no_link(dir)?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let path = key_path(dir);
    match read_key(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        other => return other,
    }
    let mut s = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut s)?;
    made_key(&path, s)
}

/// The key at `path`, never read through a link.
fn read_key(path: &std::path::Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let mut s = [0u8; 32];
    f.read_exact(&mut s)?;
    Ok(s)
}

/// Key `s` put at `path` — unless one got there first (another tarae, another worker, after this one found
/// none): then that one, never overwritten. The key in place.
fn made_key(path: &std::path::Path, s: [u8; 32]) -> std::io::Result<[u8; 32]> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // (a name of this write's own: two writes of one tarae may make it at once)
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{KEY}.{}.{n}", std::process::id()));
    // (one a tarae with this pid left half made, gone first)
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
    o.open(&tmp)?.write_all(&s)?;
    let linked = std::fs::hard_link(&tmp, path);
    std::fs::remove_file(&tmp)?;
    match linked {
        Ok(()) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_key(path),
        Err(e) => Err(e),
    }
}

/// HMAC-SHA256 of `msg` under `key` (RFC 2104; a key past a block is hashed first).
pub fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    match key.len() > 64 {
        true => k[..32].copy_from_slice(&crate::sha256::digest(key)),
        false => k[..key.len()].copy_from_slice(key),
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let mut inner = pad(0x36);
    inner.extend_from_slice(msg);
    let mut outer = pad(0x5c);
    outer.extend_from_slice(&crate::sha256::digest(&inner));
    crate::sha256::digest(&outer)
}

/// A change's id as the record keeps it: its HMAC under the record's key, in hex.
pub fn keyed(key: &[u8; 32], id: &str) -> String {
    hmac(key, id.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Entry `e` with its `change` keyed.
fn keyed_entry(key: &[u8; 32], mut e: Value) -> Value {
    if let Some(id) = e["change"].as_str().map(|id| keyed(key, id)) {
        e["change"] = json!(id);
    }
    e
}

impl Editor {
    /// A key, click, wheel or paste: the front session's reviews count the time before it.
    pub fn review_input(&mut self) {
        let w = &mut self.session.reviews;
        if w.edit.is_none() && w.code.is_none() {
            return;
        }
        let now = crate::clock::now();
        for w in [w.edit.as_mut(), w.code.as_mut()].into_iter().flatten() {
            w.input(now);
        }
    }

    /// Keeps `entries` in the record (`$state/reviews/`), on a worker with nothing to bring back — unless
    /// `review.record` is off or there's no record (tests: none unless given).
    pub fn reviews_record(&mut self, entries: Vec<Value>) {
        let Some(dir) = self.reviews_dir.clone().filter(|_| self.config.review_record && !entries.is_empty())
        else {
            return;
        };
        let file = crate::cards::month_file(crate::clock::system());
        crate::cards::WRITING.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.events.jobs().background(move || {
            let written = key(&dir).and_then(|key| {
                let entries: Vec<Value> = entries.into_iter().map(|e| keyed_entry(&key, e)).collect();
                crate::cards::append(&dir, &file, &entries)
            });
            if let Err(e) = written {
                crate::log::line("reviews", "warn", &format!("{}: {e}", dir.join(&file).display()));
            }
            crate::cards::WRITING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        });
    }

    /// When tarae ends: every session's reviews under way are kept as they stand (`reviews_end`; `cards_flush`
    /// then waits for the writes). `:restart` too, each said `restarted` — a review it opens again is a new one:
    /// nothing of one is carried across processes.
    pub fn reviews_flush(&mut self) {
        let restarted = self.restart.as_ref().is_some_and(|r| r.ready().is_some());
        let sessions = std::iter::once(&mut self.session).chain(&mut self.sessions);
        for w in sessions.flat_map(|l| [l.reviews.edit.as_mut(), l.reviews.code.as_mut()]).flatten() {
            w.restarted = restarted;
        }
        self.reviews_end();
        let behind: Vec<_> = self.sessions.iter().map(|l| l.id).collect();
        for id in behind {
            self.with_session(id, Editor::reviews_end);
        }
    }

    /// This session's reviews under way kept as they stand — it ends (`session_close`) or tarae does: its edit
    /// review applies nothing (`left`, a `y` too), its code review graded as `codereview::end` does it.
    pub fn reviews_end(&mut self) {
        if let Some(w) = self.session.reviews.edit.take() {
            ended(self, w, crate::clock::now());
        }
        code_ended(self);
    }
}

/// What the simulation's checks read of the front session's reviews (`sim/reviews.rs`) — read only.
#[cfg(test)]
pub mod probe {
    use super::*;

    /// A file's changes as found: its name, the text they were found in (`Stamp`), whether they're its changes.
    pub type Landed = (String, (DocId, u64, u64), Option<bool>);

    /// A watch's times: when it began, its active time, each change's file, active and in-hand time.
    pub type Times = (Instant, Duration, Vec<(String, Duration, Duration)>);

    /// Each file's changes as the code review last found them: its name, the text they were found in (`Stamp`)
    /// and — while its document is still at that text — whether they're the changes that text has.
    pub fn landed(ed: &Editor) -> Vec<Landed> {
        let (Some(r), Some(w)) = (ed.session.code_review.as_ref(), ed.session.reviews.code.as_ref()) else {
            return Vec::new();
        };
        let key =
            |v: &[Found]| v.iter().map(|x| (x.lines.clone(), x.text.clone(), x.nth)).collect::<Vec<_>>();
        let mut out: Vec<_> = (w.found.iter())
            .map(|(file, f)| {
                let at = ed.docs.iter().find(|d| (d.id, d.version(), d.git_version) == f.stamp);
                let same = at.zip(r.files.iter().find(|x| x.rel == *file)).map(|(d, x)| {
                    let now = found_of(&Text::of(d), (file, &x.path, &r.root), &r.rules);
                    w.unpaired.contains(file) || key(&now) == key(&f.found)
                });
                (file.clone(), f.stamp, same)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The front session's watches' times.
    pub fn times(ed: &Editor) -> Vec<Times> {
        let w = &ed.session.reviews;
        [w.edit.as_ref(), w.code.as_ref()]
            .into_iter()
            .flatten()
            .map(|w| {
                let changes = w.changes.iter().map(|c| (c.file.clone(), c.active, c.in_hand)).collect();
                (w.started, w.active, changes)
            })
            .collect()
    }

    /// The front session's followed changes of lines: each one's id (made from its first text) and its text's
    /// hash now — a change followed keeps its id as its text changes.
    pub fn followed(ed: &Editor) -> Vec<(String, String)> {
        let w = &ed.session.reviews;
        let all = [w.edit.as_ref(), w.code.as_ref()].into_iter().flatten().flat_map(|w| &w.changes);
        all.filter(|c| !c.mode).map(|c| (c.id.clone(), c.text.clone())).collect()
    }

    /// The changes of the code review's documents whose diff is of their text now, worked out here: each one's
    /// file, its text's hash and its id in memory (`text_id`, `change_id`), its lines removed then added.
    pub fn changes_now(ed: &Editor) -> Vec<(String, String, String, Vec<String>)> {
        let Some(r) = ed.session.code_review.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        for f in &r.files {
            let Some(d) = ed.docs.iter().find(|d| d.path.as_deref() == Some(f.path.as_path())) else {
                continue;
            };
            if d.review_base.is_none() || d.git_version != d.version() {
                continue;
            }
            let t = Text::of(d);
            let mut alike: HashMap<String, usize> = HashMap::new();
            for h in &t.hunks {
                let (removed, added) = t.lines(h);
                let text =
                    text_id(&f.rel, removed.iter().map(String::as_str), added.iter().map(String::as_str));
                let nth = alike.entry(text.clone()).or_default();
                let id = change_id(&text, std::mem::replace(nth, *nth + 1));
                out.push((f.rel.clone(), text, id, removed.into_iter().chain(added).collect()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
