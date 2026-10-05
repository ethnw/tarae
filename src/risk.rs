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
    /// (`100755`) or a symlink (`120000`), the only ones `parse_diff` gives.
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
    pub keys: Vec<Key>,
    /// A change of mode (`Hunk.mode`), not of lines — an open document's diff doesn't have it.
    pub mode: bool,
}

impl Graded {
    /// Whether a person looked at every line of it (`looked`: the session's keys — `space v h`).
    pub fn looked_at(&self, looked: &HashSet<Key>) -> bool {
        self.keys.iter().all(|k| looked.contains(k))
    }
}

/// The language a file is, by its path (as the editor picks it).
pub fn lang_of(rel: &str) -> Option<&'static str> {
    crate::syntax::detect(Path::new(rel)).map(|s| s.name.as_str())
}

/// Hunk `h` of file `rel` (from the repository's root; `lang` its language) graded by `rules`; `in_test`: it
/// lies wholly inside a test (the document's tree says).
pub fn grade(rel: &str, lang: Option<&str>, h: &Hunk, rules: &Rules, in_test: bool) -> (Grade, String) {
    // (a permission: `parse_diff` gives only the modes that matter)
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
    Graded { grade, why, at: h.at, lines: h.added.len().max(1), keys: keys(rel, h), mode: h.mode.is_some() }
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

/// One file's block of a diff while it's read: its path from the header (a change of mode only has no
/// `---`/`+++`), from `---`, the modes it names, and its entry once it has one.
#[derive(Default)]
struct Block {
    header: String,
    old_path: Option<String>,
    old_mode: Option<String>,
    new_mode: Option<String>,
    entry: Option<usize>,
}

/// A name as git writes it in a diff (`a/x`, `"b/\303\274/x"`, `b/a b.rs` + a tab): the path itself.
fn diff_path(s: &str) -> String {
    let s = s.trim_end_matches('\t');
    let s = match s.strip_prefix('"').and_then(|q| q.strip_suffix('"')) {
        Some(q) => unquote(q),
        None => s.to_string(),
    };
    match s.strip_prefix("a/").or_else(|| s.strip_prefix("b/")) {
        Some(rel) => rel.to_string(),
        None => s,
    }
}

/// git's C-style quoting taken off (what's between the quotes): `\"` `\\` `\t` `\n` … and `\ooo` bytes (a
/// name's UTF-8, byte by byte).
fn unquote(q: &str) -> String {
    let b = q.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = b[i + 1];
        let simple = match c {
            b'a' => Some(7),
            b'b' => Some(8),
            b't' => Some(b'\t'),
            b'n' => Some(b'\n'),
            b'v' => Some(11),
            b'f' => Some(12),
            b'r' => Some(b'\r'),
            b'"' | b'\\' => Some(c),
            _ => None,
        };
        match simple {
            Some(x) => {
                out.push(x);
                i += 2;
            }
            None if i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|d| (b'0'..=b'7').contains(d)) => {
                let v = b[i + 1..i + 4].iter().fold(0u32, |v, d| v * 8 + u32::from(d - b'0'));
                out.push(v as u8);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The new path a `diff --git <a> <b>` header names (`--no-renames`: both the same name).
fn header_path(h: &str) -> String {
    if let Some(rest) = h.strip_prefix('"') {
        // The first name quoted: up to its closing quote (not one escaped), the second after it
        let mut esc = false;
        let end = rest.char_indices().find(|&(_, c)| {
            let close = c == '"' && !esc;
            esc = c == '\\' && !esc;
            close
        });
        if let Some((at, _)) = end {
            return diff_path(rest[at + 1..].trim_start());
        }
    }
    // `a/X b/X`: the second half
    let mid = h.len().saturating_sub(1) / 2;
    match h.get(mid + 1..) {
        Some(b) if h.as_bytes().get(mid) == Some(&b' ') => diff_path(b),
        _ => diff_path(h.split_once(' ').map_or(h, |(_, b)| b)),
    }
}

/// A block read to its end: a change of mode that matters — made executable, or a symlink — as a change of
/// its own, in its file's entry.
fn block_done(out: &mut Vec<(String, Vec<Hunk>)>, b: Option<Block>) {
    let Some(b) = b else { return };
    let Some(new) = b.new_mode else { return };
    let old = b.old_mode.unwrap_or_else(|| "none".into());
    if !matches!(new.as_str(), "100755" | "120000") || old == new {
        return;
    }
    let i = match b.entry {
        Some(i) => i,
        None => {
            out.push((b.header, Vec::new()));
            out.len() - 1
        }
    };
    out[i].1.push(Hunk { mode: Some((old, new)), ..Hunk::default() });
}

/// A `git diff -U0` (or `diff-index -p -U0`) as each file's changes — by its path now (a deleted file's by
/// its path then), git's quoting taken off. Binary files have no lines here; a file made executable or a
/// symlink has a change of mode (`Hunk.mode`).
pub fn parse_diff(text: &str) -> Vec<(String, Vec<Hunk>)> {
    let mut out: Vec<(String, Vec<Hunk>)> = Vec::new();
    let mut block: Option<Block> = None;
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("diff --git ") {
            block_done(&mut out, block.take());
            block = Some(Block { header: header_path(h), ..Block::default() });
            in_hunk = false;
            continue;
        }
        if !in_hunk && let Some(b) = block.as_mut() {
            let field = |p: &str| line.strip_prefix(p).map(|m| m.trim().to_string());
            if let Some(m) = field("old mode ").or_else(|| field("deleted file mode ")) {
                b.old_mode = Some(m);
            } else if let Some(m) = field("new mode ").or_else(|| field("new file mode ")) {
                b.new_mode = Some(m);
            } else if let Some(p) = line.strip_prefix("--- ") {
                b.old_path = (p != "/dev/null").then(|| diff_path(p));
            } else if let Some(p) = line.strip_prefix("+++ ") {
                let rel = match p {
                    "/dev/null" => b.old_path.clone().unwrap_or_else(|| b.header.clone()),
                    p => diff_path(p),
                };
                out.push((rel, Vec::new()));
                b.entry = Some(out.len() - 1);
            }
        }
        if let Some(h) = line.strip_prefix("@@ ") {
            in_hunk = true;
            // `@@ -a,b +c,d @@`: c is where it is now (1-based); with d 0 the lines removed were after line c
            let new = h.split(' ').find(|w| w.starts_with('+')).unwrap_or("+0");
            let (c, d): (usize, usize) = match new[1..].split_once(',') {
                Some((c, d)) => (c.parse().unwrap_or(0), d.parse().unwrap_or(1)),
                None => (new[1..].parse().unwrap_or(0), 1),
            };
            let at = if d == 0 { c } else { c.saturating_sub(1) };
            if let Some(f) = out.last_mut() {
                f.1.push(Hunk { at, ..Hunk::default() });
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let Some(h) = out.last_mut().and_then(|f| f.1.last_mut()) else { continue };
        if let Some(l) = line.strip_prefix('-') {
            h.removed.push(l.to_string());
        } else if let Some(l) = line.strip_prefix('+') {
            h.added.push(l.to_string());
        }
    }
    block_done(&mut out, block);
    out
}

/// `base` → `now` as changes (`git::hunks`, with their lines).
pub fn hunks_of(base: &str, now: &str) -> Vec<Hunk> {
    let (b, n): (Vec<&str>, Vec<&str>) = (base.lines().collect(), now.lines().collect());
    let take = |v: &[&str], r: &Range<usize>| {
        v.get(r.clone()).unwrap_or_default().iter().map(|s| s.to_string()).collect()
    };
    crate::git::hunks(base, now)
        .iter()
        .map(|h| Hunk {
            at: h.lines.start,
            removed: take(&b, &h.base),
            added: take(&n, &h.lines),
            mode: None,
        })
        .collect()
}

/// A whole new file's lines as one change.
pub fn added_whole(text: &str) -> Hunk {
    Hunk { at: 0, removed: Vec::new(), added: text.lines().map(str::to_string).collect(), mode: None }
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

    /// `git diff -U0` read back as changes: where each is now, the lines out and in — a new file, a deleted
    /// one, and lines only removed (placed after the line before them).
    #[test]
    fn a_diff_reads_as_changes() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n\
                    @@ -3 +3 @@ fn x()\n-old\n+new\n@@ -9,2 +8,0 @@\n-gone 1\n-gone 2\n\
                    diff --git a/n.rs b/n.rs\nnew file mode 100644\n--- /dev/null\n+++ b/n.rs\n@@ -0,0 +1,2 @@\n+a\n+b\n\
                    diff --git a/d.rs b/d.rs\ndeleted file mode 100644\n--- a/d.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let f = parse_diff(diff);
        type Change = (usize, Vec<String>, Vec<String>);
        let got: Vec<(&str, Vec<Change>)> = f
            .iter()
            .map(|(rel, hs)| {
                (rel.as_str(), hs.iter().map(|h| (h.at, h.removed.clone(), h.added.clone())).collect())
            })
            .collect();
        assert_eq!(
            got,
            [
                ("src/a.rs", vec![(2, s(&["old"]), s(&["new"])), (8, s(&["gone 1", "gone 2"]), s(&[]))]),
                ("n.rs", vec![(0, s(&[]), s(&["a", "b"]))]),
                ("d.rs", vec![(0, s(&["x"]), s(&[]))]),
            ]
        );
    }

    /// git quotes a name with special characters C-style (`"b/\303\274/build.rs"`, `"b/x\"y\\z\tw.rs"`) and ends
    /// one with a space in it with a tab: each comes back as the name itself — the same rel as `-z` gives.
    #[test]
    fn quoted_and_spaced_names_read_back_as_themselves() {
        let one = |header: &str, minus: &str, plus: &str| {
            let diff = format!(
                "diff --git {header}\nindex 1..2 100644\n--- {minus}\n+++ {plus}\n@@ -1 +1 @@\n-a\n+b\n"
            );
            let f = parse_diff(&diff);
            assert_eq!(f.len(), 1, "{diff}");
            f[0].0.clone()
        };
        let q = r#""a/\303\274/build.rs" "b/\303\274/build.rs""#;
        assert_eq!(one(q, r#""a/\303\274/build.rs""#, r#""b/\303\274/build.rs""#), "ü/build.rs");
        assert_eq!(one("a/a b.rs b/a b.rs", "a/a b.rs\t", "b/a b.rs\t"), "a b.rs");
        let odd = r#""b/x\"y\\z\tw.rs""#;
        assert_eq!(
            one(r#""a/x\"y\\z\tw.rs" "b/x\"y\\z\tw.rs""#, r#""a/x\"y\\z\tw.rs""#, odd),
            "x\"y\\z\tw.rs"
        );
        // (a deleted file's name is its `---` one)
        let gone = "diff --git \"a/\\303\\274.rs\" \"b/\\303\\274.rs\"\ndeleted file mode 100644\n--- \"a/\\303\\274.rs\"\n\
                    +++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        assert_eq!(parse_diff(gone)[0].0, "ü.rs");
    }

    /// A change of mode has no lines: made executable, or made a symlink, it's a change of its own — high, its
    /// key its path and its modes; any other mode change (or a deleted file's) isn't one.
    #[test]
    fn a_file_made_executable_or_a_symlink_is_a_high_change() {
        type Modes = Vec<Option<(String, String)>>;
        let modes = |diff: &str| -> Vec<(String, Modes)> {
            parse_diff(diff)
                .into_iter()
                .map(|(rel, hs)| (rel, hs.into_iter().map(|h| h.mode).collect()))
                .collect()
        };
        let m = |o: &str, n: &str| Some((o.to_string(), n.to_string()));
        // Mode only: no `---`/`+++` at all
        let only = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        assert_eq!(modes(only), [("run.sh".to_string(), vec![m("100644", "100755")])]);
        // With lines too, then a new executable file, then a symlink in a file's place, then one made plain
        let more = "diff --git a/a.sh b/a.sh\nold mode 100644\nnew mode 100755\nindex 1..2\n--- a/a.sh\n+++ b/a.sh\n\
                    @@ -1 +1 @@\n-a\n+b\n\
                    diff --git a/new.sh b/new.sh\nnew file mode 100755\nindex 0..1\n--- /dev/null\n+++ b/new.sh\n@@ -0,0 +1 @@\n+x\n\
                    diff --git a/l b/l\ndeleted file mode 100644\nindex 1..0\n--- a/l\n+++ /dev/null\n@@ -1 +0,0 @@\n-t\n\
                    diff --git a/l b/l\nnew file mode 120000\nindex 0..1\n--- /dev/null\n+++ b/l\n@@ -0,0 +1 @@\n+/etc\n\
                    diff --git a/p.sh b/p.sh\nold mode 100755\nnew mode 100644\n";
        let got = modes(more);
        assert_eq!(got[0], ("a.sh".to_string(), vec![None, m("100644", "100755")]));
        assert_eq!(got[1], ("new.sh".to_string(), vec![None, m("none", "100755")]));
        assert_eq!(got[3], ("l".to_string(), vec![None, m("none", "120000")]));
        assert!(got.iter().all(|(rel, ms)| rel != "p.sh" || ms.iter().all(Option::is_none)), "{got:?}");
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
            mode: false,
        };
        assert!(g(&h(&[], &["unsafe { a() }"])).looked_at(&looked), "a part of it, regrouped");
        assert!(!g(&h(&["old"], &["unsafe { a2() }", "b()"])).looked_at(&looked), "a line changed since");
        assert!(!graded("src/b.rs", None, &a, &Rules::default(), false).looked_at(&looked), "another file");
    }
}
