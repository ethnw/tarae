//! Replay — every stop of Claude's turns, kept in order: what it read (the range), what it edited (the
//! changed lines and what they replaced), what a search or a lookup found (its first hit and the count), a
//! command's or a test run's verdict, a note. `]r`/`[r` step through them at any time — while it works or
//! after — the editor going to each stop's file and lines (an edit's changed lines on the review's wash, the
//! lines it replaced above them); a click on a chat row of a call, or `C-]` there, goes to that call's
//! stop. The last `TURNS` turns of the chat are kept.
//!
//! Live follow (`follow.rs`) shows the newest stop as it comes; replay is the record, stepped at your pace.

use std::collections::VecDeque;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::chat::Look;

/// Turns of the chat whose stops are kept.
pub const TURNS: usize = 8;

/// A change an edit made: the lines it left (0-based, in the file right after it), the lines they replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Range<usize>,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

/// One stop: the call it came from, its row (the tool's name, what, range), where it was.
#[derive(Clone, Debug)]
pub struct Stop {
    pub call: String,
    pub look: Look,
    pub path: Option<PathBuf>,
    /// 0-based lines (a read's range, an edit's changed lines, a search's first hit).
    pub lines: Option<Range<usize>>,
    /// An edit's changes (empty for the rest).
    pub hunks: Vec<Hunk>,
    /// What Claude said right before the call (the card says it at this stop).
    pub why: String,
}

#[derive(Default)]
pub struct Turn {
    pub stops: Vec<Stop>,
}

#[derive(Default)]
pub struct Replay {
    /// Oldest first; the last is the turn under way (or the latest).
    pub turns: VecDeque<Turn>,
    /// The stop showing — (turn, stop) — while you step through them; None: live.
    pub at: Option<(usize, usize)>,
}

impl Replay {
    /// A new question: a turn of its own (an empty last one is reused), the oldest beyond `TURNS` dropped.
    pub fn begin(&mut self) {
        if self.turns.back().is_some_and(|t| t.stops.is_empty()) {
            return;
        }
        self.turns.push_back(Turn::default());
        while self.turns.len() > TURNS {
            self.turns.pop_front();
            self.at = self.at.and_then(|(t, s)| t.checked_sub(1).map(|t| (t, s)));
        }
    }

    pub fn add(&mut self, stop: Stop) {
        if self.turns.is_empty() {
            self.turns.push_back(Turn::default());
        }
        self.turns.back_mut().expect("just made").stops.push(stop);
    }

    /// The stop of tool call `call` (the newest with that id).
    pub fn by_call(&mut self, call: &str) -> Option<&mut Stop> {
        if call.is_empty() {
            return None;
        }
        self.turns.iter_mut().rev().flat_map(|t| t.stops.iter_mut().rev()).find(|s| s.call == call)
    }

    /// Where the stop of `call` is.
    pub fn place_of(&self, call: &str) -> Option<(usize, usize)> {
        self.turns.iter().enumerate().rev().find_map(|(t, turn)| {
            turn.stops.iter().rposition(|s| !call.is_empty() && s.call == call).map(|s| (t, s))
        })
    }

    pub fn get(&self, at: (usize, usize)) -> Option<&Stop> {
        self.turns.get(at.0)?.stops.get(at.1)
    }

    /// Stops of the latest turn that has any.
    pub fn latest(&self) -> usize {
        self.turns.iter().rev().map(|t| t.stops.len()).find(|&n| n > 0).unwrap_or(0)
    }

    /// An edit to `path` moved its lines (`changes`: old → new line ranges, in order): the stops recorded
    /// before it there follow (a read of lines below it, an earlier edit's lines).
    pub fn edited(&mut self, path: &Path, changes: &[(Range<usize>, Range<usize>)]) {
        for stop in self.turns.iter_mut().flat_map(|t| t.stops.iter_mut()) {
            if stop.path.as_deref() != Some(path) {
                continue;
            }
            if let Some(l) = &mut stop.lines {
                *l = moved_range(l, changes);
                // (its row's `L12–13` says where it is now — a ` +2` after it stays)
                if let Some(rest) = stop.look.range.strip_prefix('L')
                    && rest.starts_with(|c: char| c.is_ascii_digit())
                {
                    let tail = rest.find(' ').map_or("", |i| &rest[i..]);
                    stop.look.range = format!("{}{tail}", l_range(l));
                }
            }
            for h in &mut stop.hunks {
                h.lines = if h.lines.is_empty() {
                    let at = moved(h.lines.start, changes, false);
                    at..at
                } else {
                    moved_range(&h.lines, changes)
                };
            }
        }
    }

    /// The next (`forward`) or previous stop from where it is — from live, back is the newest stop and
    /// forward the latest turn's first. None: past the newest (back to live) or before the oldest.
    pub fn step(&self, forward: bool) -> Option<(usize, usize)> {
        let all: Vec<(usize, usize)> = self
            .turns
            .iter()
            .enumerate()
            .flat_map(|(t, turn)| (0..turn.stops.len()).map(move |s| (t, s)))
            .collect();
        let last = *all.last()?;
        let Some(at) = self.at else {
            return Some(if forward { (last.0, 0) } else { last });
        };
        let i = all.iter().position(|&p| p == at)?;
        if forward { all.get(i + 1).copied() } else { i.checked_sub(1).map(|i| all[i]) }
    }

    /// Which stop of how many in its turn, and how many turns back (0: the latest).
    pub fn position(&self, at: (usize, usize)) -> (usize, usize, usize) {
        let n = self.turns.get(at.0).map_or(0, |t| t.stops.len());
        let latest = self.turns.iter().rposition(|t| !t.stops.is_empty()).unwrap_or(0);
        (at.1 + 1, n, latest.saturating_sub(at.0))
    }
}

/// `L12` · `L12–30` — 0-based lines as a row says them.
pub fn l_range(lines: &Range<usize>) -> String {
    if lines.len() > 1 {
        format!("L{}–{}", lines.start + 1, lines.end)
    } else {
        format!("L{}", lines.start + 1)
    }
}

/// `old` → `new` as an edit's hunks, with the old lines each replaced (for `Replay::edited`).
pub fn hunks(old: &str, new: &str) -> Vec<(Range<usize>, Hunk)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let input = InternedInput::new(old, new);
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |before: Range<u32>, after: Range<u32>| {
        let (bs, be) = (before.start as usize, before.end as usize);
        let (as_, ae) = (after.start as usize, after.end as usize);
        let lines = |v: &[&str], r: Range<usize>| -> Vec<String> {
            v.get(r.start.min(v.len())..r.end.min(v.len()))
                .unwrap_or_default()
                .iter()
                .map(|l| l.to_string())
                .collect()
        };
        out.push((bs..be, Hunk { lines: as_..ae, removed: lines(&a, bs..be), added: lines(&b, as_..ae) }));
    });
    out
}

/// Line `line` of the old text in the new one — a line inside a replaced range lands on the replacement's
/// first line (its last, for a range's `last`).
fn moved(line: usize, changes: &[(Range<usize>, Range<usize>)], last: bool) -> usize {
    let mut delta: isize = 0;
    for (old, new) in changes {
        if line < old.start {
            break;
        }
        if old.is_empty() || line >= old.end {
            delta += new.len() as isize - old.len() as isize;
        } else if last {
            return new.end.saturating_sub(1).max(new.start);
        } else {
            return new.start;
        }
    }
    (line as isize + delta).max(0) as usize
}

fn moved_range(r: &Range<usize>, changes: &[(Range<usize>, Range<usize>)]) -> Range<usize> {
    let start = moved(r.start, changes, false);
    let end = moved(r.end.saturating_sub(1).max(r.start), changes, true) + 1;
    start..end.max(start + 1)
}

/// The places a search's or a lookup's answer names, in order (its first 4 KB): `path:line` (Grep's
/// content mode, a definition), a path line followed by indented `  12: code` hits (tarae's search and
/// references), or bare paths (files only). Relative paths are from `root`.
pub fn hits(root: &Path, text: &str) -> Vec<(PathBuf, usize)> {
    let mut out: Vec<(PathBuf, usize)> = Vec::new();
    // A path line not yet followed by its hits — a hit of its own (line 1) if none come
    let mut file: Option<(PathBuf, bool)> = None;
    let close = |file: &mut Option<(PathBuf, bool)>, out: &mut Vec<(PathBuf, usize)>| {
        if let Some((p, false)) = file.take() {
            out.push((p, 0));
        }
    };
    for l in text.lines() {
        if l.trim().is_empty() {
            continue;
        }
        if l.starts_with(char::is_whitespace) {
            let t = l.trim_start();
            let digits = t.bytes().take_while(u8::is_ascii_digit).count();
            if let Some((p, seen)) = file.as_mut()
                && digits > 0
                && t[digits..].starts_with(':')
                && let Ok(n) = t[..digits].parse::<usize>()
            {
                out.push((p.clone(), n.saturating_sub(1)));
                *seen = true;
            }
            continue;
        }
        if let Some((_, link)) = crate::chat::refs_in(root, l).into_iter().next() {
            close(&mut file, &mut out);
            out.push((link.path, link.line));
            continue;
        }
        // A bare path: one word that looks like a file
        let t = l.trim();
        let file_like = !t.contains(char::is_whitespace)
            && (t.contains('/') || t.rsplit_once('.').is_some_and(|(a, e)| !a.is_empty() && !e.is_empty()));
        if file_like {
            close(&mut file, &mut out);
            file = Some((crate::chat::resolve(root, t), false));
        }
    }
    close(&mut file, &mut out);
    out
}

/// What a search found, as the card and the row say it: the tool's own count (`12 references in 3
/// files`) — else counted from its hits (`12 hits in 4 files`, `+` when its answer was cut).
pub fn count(text: &str, hits: &[(PathBuf, usize)], cut: bool) -> String {
    if let Some(v) = crate::nav::verdict(text) {
        return v;
    }
    if hits.is_empty() {
        return String::new();
    }
    let mut files: Vec<&PathBuf> = hits.iter().map(|h| &h.0).collect();
    files.sort();
    files.dedup();
    let plus = if cut { "+" } else { "" };
    let n = hits.len();
    let s = |n: usize, w: &str| if n == 1 { format!("{n}{plus} {w}") } else { format!("{n}{plus} {w}s") };
    if hits.iter().all(|h| h.1 == 0) && files.len() == n {
        return s(n, "file");
    }
    format!(
        "{} in {}",
        s(n, "hit"),
        if files.len() == 1 { "1 file".into() } else { format!("{} files", files.len()) }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(path: &str, lines: Range<usize>) -> Stop {
        let look = Look { verb: "read", what: path.into(), pattern: false, range: String::new(), link: None };
        let path = Some(PathBuf::from(path));
        Stop { call: String::new(), look, path, lines: Some(lines), hunks: Vec::new(), why: String::new() }
    }

    /// An edit's hunks: the lines it left, the ones they replaced — and the old ranges, for moving others.
    #[test]
    fn an_edit_becomes_hunks() {
        let h = hunks("a\nb\nc\nd\n", "a\nB\nB2\nc\n");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].0, 1..2);
        assert_eq!(
            h[0].1,
            Hunk { lines: 1..3, removed: vec!["b".into()], added: vec!["B".into(), "B2".into()] }
        );
        assert_eq!(h[1].1.removed, vec!["d".to_string()]);
        assert!(h[1].1.added.is_empty() && h[1].1.lines == (4..4));
    }

    /// A later edit in the same file moves what was recorded there before it: lines below go with it, a
    /// range inside what it replaced lands on the replacement, lines above stay.
    #[test]
    fn later_edits_move_earlier_stops() {
        let mut r = Replay::default();
        r.begin();
        r.add(stop("a.rs", 0..2));
        r.add(stop("a.rs", 10..15));
        r.add(stop("b.rs", 10..15));
        r.add(stop("a.rs", 4..6));
        // Lines 3..5 (3 lines) became 2 new lines, and 2 lines were inserted at old line 8
        r.edited(Path::new("a.rs"), &[(3..6, 3..5), (8..8, 7..9)]);
        let lines: Vec<_> = r.turns[0].stops.iter().map(|s| s.lines.clone().unwrap()).collect();
        assert_eq!(lines, [0..2, 11..16, 10..15, 3..5]);
    }

    /// Stepping: from live, back is the newest stop and forward the latest turn's first; across turns; past
    /// the newest is live again (None).
    #[test]
    fn stepping_through_turns() {
        let mut r = Replay::default();
        r.begin();
        r.add(stop("a", 0..1));
        r.add(stop("b", 0..1));
        r.begin();
        r.begin(); // (an empty turn is reused)
        r.add(stop("c", 0..1));
        r.add(stop("d", 0..1));
        assert_eq!(r.turns.len(), 2);
        assert_eq!(r.step(false), Some((1, 1)));
        assert_eq!(r.step(true), Some((1, 0)));
        r.at = Some((1, 0));
        assert_eq!(r.step(false), Some((0, 1)), "into the turn before");
        r.at = Some((1, 1));
        assert_eq!(r.step(true), None, "past the newest: live");
        r.at = Some((0, 0));
        assert_eq!(r.step(false), None);
        assert_eq!(r.position((0, 1)), (2, 2, 1));
        for _ in 0..TURNS + 2 {
            r.begin();
            r.add(stop("e", 0..1));
        }
        assert_eq!(r.turns.len(), TURNS, "only the last few turns");
    }

    /// The places a search names, in its tools' shapes.
    #[test]
    fn hits_in_search_answers() {
        let root = Path::new("/p");
        let tarae = "3 matching lines in 2 files:\nsrc/a.rs\n   12:     fn a()\n  --\n   40: a()\nsrc/b.rs\n    7: a()\n";
        assert_eq!(
            hits(root, tarae),
            [(root.join("src/a.rs"), 11), (root.join("src/a.rs"), 39), (root.join("src/b.rs"), 6)]
        );
        assert_eq!(count(tarae, &hits(root, tarae), false), "3 matching lines in 2 files");
        let grep = "/p/src/a.rs:3:fn a\n/p/src/c.rs:9:a()\n";
        let h = hits(root, grep);
        assert_eq!(h, [(root.join("src/a.rs"), 2), (root.join("src/c.rs"), 8)]);
        assert_eq!(count(grep, &h, false), "2 hits in 2 files");
        let files = "Found 2 files\n/p/src/a.rs\n/p/src/b.rs\n";
        let h = hits(root, files);
        assert_eq!(h, [(root.join("src/a.rs"), 0), (root.join("src/b.rs"), 0)]);
        assert_eq!(count(files, &h, true), "2+ files");
        assert!(hits(root, "No matches found").is_empty());
    }
}
