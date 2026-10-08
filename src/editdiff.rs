//! Edits previewed before applying — LSP WorkspaceEdit (code actions, rename) as per-file before/after diffs.
//!
//! The pure part (`build`) only takes a function returning the old text — an open buffer's rope,
//! else disk (worker thread).
//! Line diff (imara-diff) → hunks with 2 lines of context; paired −/+ lines also highlight the changed
//! middle, syntax colors per line (before and after each parsed in that file's language).

use std::path::{Path, PathBuf};

use ropey::Rope;
use serde_json::Value;
use tree_sitter::QueryCursor;

use crate::lsp::{self, Encoding};
use crate::syntax::{self, Syntax};
use crate::transaction::{Change, Transaction};

/// Context lines shown around a hunk.
const CONTEXT: usize = 2;
/// Cap on diff lines shown per file (huge reformatting etc.).
const MAX_LINES: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Minus,
    Plus,
    /// Skipped context (`…`).
    Gap,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number (from 1) — old number for deleted lines, new number otherwise.
    pub number: usize,
    pub text: String,
    /// Syntax colors: (start, end, global capture id) — bytes within the line.
    pub spans: Vec<(usize, usize, u32)>,
    /// The actually changed middle of a paired −/+ line (bytes).
    pub emph: Option<(usize, usize)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
    Edit,
    Create,
    Rename(PathBuf),
    Delete,
}

#[derive(Clone, Debug)]
pub struct FileDiff {
    pub path: PathBuf,
    pub op: FileOp,
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

/// Summary of one edit (for picker title and hints).
pub fn summary(files: &[FileDiff]) -> String {
    let (a, r) = files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let n = files.len();
    let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
    format!("{files}  +{a} −{r}")
}

/// Result of applying TextEdit[] to the text.
pub fn apply_edits(text: &Rope, edits: &[Value], enc: Encoding) -> Rope {
    let changes = edits
        .iter()
        .filter_map(|e| {
            let from = lsp::from_position(text, &e["range"]["start"], enc)?;
            let to = lsp::from_position(text, &e["range"]["end"], enc)?;
            Some(Change { from, to: to.max(from), insert: e["newText"].as_str()?.to_string() })
        })
        .collect();
    let mut out = text.clone();
    Transaction::new(changes).apply(&mut out, false);
    out
}

/// WorkspaceEdit → per-file diffs. `old(path)` = current text (None if the file doesn't exist).
pub fn build(edit: &Value, enc: Encoding, mut old: impl FnMut(&Path) -> Option<Rope>) -> Vec<FileDiff> {
    // File → accumulated text (several edits to one file chain on)
    let mut files: Vec<(PathBuf, FileOp, Option<Rope>, Rope)> = Vec::new();
    type Files = Vec<(PathBuf, FileOp, Option<Rope>, Rope)>;
    fn touch(
        files: &mut Files,
        old: &mut impl FnMut(&Path) -> Option<Rope>,
        path: PathBuf,
        edits: &[Value],
        enc: Encoding,
    ) {
        let i = match files.iter().position(|f| f.0 == path) {
            Some(i) => i,
            None => {
                let before = old(&path);
                let now = before.clone().unwrap_or_default();
                files.push((path, FileOp::Edit, before, now));
                files.len() - 1
            }
        };
        files[i].3 = apply_edits(&files[i].3, edits, enc);
    }
    let path_of = |u: &Value| u.as_str().and_then(lsp::path_from_uri);
    // `documentChanges` wins when both are present (LSP spec — servers send both for old clients)
    if let Some(map) = edit["changes"].as_object().filter(|_| edit["documentChanges"].is_null()) {
        for (u, e) in map {
            if let Some(p) = lsp::path_from_uri(u) {
                touch(&mut files, &mut old, p, e.as_array().map(Vec::as_slice).unwrap_or_default(), enc);
            }
        }
    }
    for dc in edit["documentChanges"].as_array().into_iter().flatten() {
        match dc["kind"].as_str() {
            Some("create") => {
                if let Some(p) = path_of(&dc["uri"]) {
                    files.push((p, FileOp::Create, None, Rope::new()));
                }
            }
            Some("rename") => {
                if let (Some(from), Some(to)) = (path_of(&dc["oldUri"]), path_of(&dc["newUri"])) {
                    // Edits after a rename come under the new name — carry the old text over to the new name
                    let before = files
                        .iter()
                        .position(|f| f.0 == from)
                        .map(|i| files.remove(i).3)
                        .or_else(|| old(&from));
                    let now = before.clone().unwrap_or_default();
                    files.push((to, FileOp::Rename(from), before, now));
                }
            }
            Some("delete") => {
                if let Some(p) = path_of(&dc["uri"]) {
                    let before = old(&p);
                    files.push((p, FileOp::Delete, before, Rope::new()));
                }
            }
            _ => {
                if let Some(p) = path_of(&dc["textDocument"]["uri"]) {
                    touch(
                        &mut files,
                        &mut old,
                        p,
                        dc["edits"].as_array().map(Vec::as_slice).unwrap_or_default(),
                        enc,
                    );
                }
            }
        }
    }
    files
        .into_iter()
        .map(|(path, op, before, now)| diff_file(path, op, before.unwrap_or_default(), now))
        .collect()
}

/// Line-level changes old text → new text: (start byte, end byte, old piece, new piece) — one per review
/// change (an edit's review, a turn's undo, a change set).
pub fn line_changes(old: &str, new: &str) -> Vec<(usize, usize, String, String)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let input = InternedInput::new(old, new);
    let starts = |s: &str| {
        let mut v = vec![0];
        v.extend(s.match_indices('\n').map(|(i, _)| i + 1));
        if *v.last().unwrap() != s.len() {
            v.push(s.len());
        }
        v
    };
    let (os, ns) = (starts(old), starts(new));
    let at = |v: &Vec<usize>, i: u32| v.get(i as usize).copied().unwrap_or(*v.last().unwrap());
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |b: std::ops::Range<u32>, a: std::ops::Range<u32>| {
        let (bf, bt) = (at(&os, b.start), at(&os, b.end));
        let (af, at_) = (at(&ns, a.start), at(&ns, a.end));
        out.push((bf, bt, old[bf..bt].to_string(), new[af..at_].to_string()));
    });
    out
}

/// One change of a formatter's to the text it was given: that text's lines `old` become its lines `new`.
#[derive(Clone, Debug, PartialEq)]
pub struct FormatHunk {
    pub old: std::ops::Range<usize>,
    pub new: std::ops::Range<usize>,
}

/// The formatter's changes `now` → `formatted`, by lines: the line diff's hunks, one that replaces as many
/// lines as it puts in split into one-line replacements (each on its own line — a re-indented block is
/// then a change per line, and the lines you touched can take theirs alone). Lines end with their `\n`.
pub fn format_hunks(now: &[&str], formatted: &[&str]) -> Vec<FormatHunk> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let (before, after) = (now.concat(), formatted.concat());
    let input = InternedInput::new(before.as_str(), after.as_str());
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |b: std::ops::Range<u32>, a: std::ops::Range<u32>| {
        let (old, new) = (b.start as usize..b.end as usize, a.start as usize..a.end as usize);
        match old.len() == new.len() {
            true => out.extend(old.zip(new).map(|(o, n)| FormatHunk { old: o..o + 1, new: n..n + 1 })),
            false => out.push(FormatHunk { old, new }),
        }
    });
    out
}

/// Which of `hunks` (`format_hunks`) a save keeps: one whose every line is among those changed since the
/// save (`touched`, a flag a line of `now` — a line the formatter only adds, beside one) — and with it, all
/// it moves a line with: a line one takes out that another puts back (by its text) ties the two, kept both
/// or neither. Kept alone, the move's other half duplicated or lost a line of yours (the reviewer, sorting
/// imports after a `use` was added).
pub fn format_kept(hunks: &[FormatHunk], now: &[&str], formatted: &[&str], touched: &[bool]) -> Vec<bool> {
    let at = |i: usize| touched.get(i).copied().unwrap_or(false);
    let within = |h: &FormatHunk| match h.old.is_empty() {
        true => at(h.old.start) || h.old.start.checked_sub(1).is_some_and(at),
        false => h.old.clone().all(at),
    };
    // (tied hunks share a root: a line's text, the hunks putting it in)
    let mut root: Vec<usize> = (0..hunks.len()).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    let mut puts: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, h) in hunks.iter().enumerate() {
        for &line in &formatted[h.new.clone()] {
            puts.entry(line).or_default().push(i);
        }
    }
    for (i, h) in hunks.iter().enumerate() {
        for line in &now[h.old.clone()] {
            for &j in puts.get(line).into_iter().flatten() {
                let (a, b) = (find(&mut root, i), find(&mut root, j));
                root[a] = b;
            }
        }
    }
    let mut whole = vec![true; hunks.len()];
    for (i, h) in hunks.iter().enumerate() {
        if !within(h) {
            let r = find(&mut root, i);
            whole[r] = false;
        }
    }
    (0..hunks.len()).map(|i| whole[find(&mut root, i)]).collect()
}

/// What a format-on-save keeps of the formatter's text: `now` with the formatter's changes (`format_hunks`)
/// that lie within the lines changed since the file was saved (`saved` → `now`) applied whole, and with them
/// all they move a line with (`format_kept`) — every other line as it is. Always your text with a subset of
/// the formatter's changes, each whole: never a line of yours lost or doubled. A save doesn't re-format what
/// you didn't touch (a YAML file in 2-space indent came back all in 4, +57 −57, for a two-line edit).
pub fn format_only_touched(saved: &str, now: &str, formatted: &str) -> String {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let lines: Vec<&str> = now.split_inclusive('\n').collect();
    let fresh: Vec<&str> = formatted.split_inclusive('\n').collect();
    // (the lines of `now` changed since the save — a deletion marks the line after it)
    let mut touched = vec![false; lines.len() + 1];
    diff(Algorithm::Histogram, &InternedInput::new(saved, now), |_, a: std::ops::Range<u32>| {
        match a.is_empty() {
            true => touched[a.start as usize] = true,
            false => a.for_each(|i| touched[i as usize] = true),
        }
    });
    let hunks = format_hunks(&lines, &fresh);
    let kept = format_kept(&hunks, &lines, &fresh, &touched);
    format_apply(&lines, &fresh, &hunks, &kept)
}

/// `now` with the `kept` ones of `hunks` applied, each whole.
pub fn format_apply(now: &[&str], formatted: &[&str], hunks: &[FormatHunk], kept: &[bool]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for (h, &keep) in hunks.iter().zip(kept) {
        out.extend(now[at..h.old.start].iter().copied());
        at = h.old.end;
        match keep {
            true => out.extend(formatted[h.new.clone()].iter().copied()),
            false => out.extend(now[h.old.clone()].iter().copied()),
        }
    }
    out.extend(now[at..].iter().copied());
    out
}

/// One file: line diff + context + highlight + syntax colors.
pub fn diff_file(path: PathBuf, op: FileOp, before: Rope, after: Rope) -> FileDiff {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let (bs, as_) = (before.to_string(), after.to_string());
    let input = InternedInput::new(bs.as_str(), as_.as_str());
    let mut hunks: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> = Vec::new();
    diff(Algorithm::Histogram, &input, |b: std::ops::Range<u32>, a: std::ops::Range<u32>| {
        hunks.push((b.start as usize..b.end as usize, a.start as usize..a.end as usize));
    });
    let (bl, al): (Vec<&str>, Vec<&str>) = (lines(&bs), lines(&as_));
    let highlighter = |text: &Rope| -> Option<Syntax> {
        let spec = syntax::detect(&path)?;
        let lang = syntax::Loader::global().load(spec).ok()?;
        Some(Syntax::parsed(lang, text))
    };
    let (hb, ha) = if bs.len() + as_.len() < 1 << 20 {
        (highlighter(&before), highlighter(&after))
    } else {
        (None, None)
    };
    let mut cursor = QueryCursor::new();
    let mut spans_of = |syn: &Option<Syntax>, text: &Rope, line: usize| -> Vec<(usize, usize, u32)> {
        let Some(syn) = syn else { return Vec::new() };
        if line >= text.len_lines() {
            return Vec::new();
        }
        let start = text.line_to_byte(line);
        let end = start + text.line(line).len_bytes();
        syntax::highlights(syn, text, start..end, &mut cursor)
            .into_iter()
            .map(|(s, e, c)| (s - start, e - start, c))
            .collect()
    };
    // Expand the whole file into rows: (kind, old line, new line) — context has both, − only old, + only new.
    let mut rows: Vec<(LineKind, usize, usize)> = Vec::new();
    let (mut ob, mut oa) = (0, 0);
    for (b, a) in &hunks {
        while ob < b.start {
            rows.push((LineKind::Context, ob, oa));
            (ob, oa) = (ob + 1, oa + 1);
        }
        rows.extend(b.clone().map(|l| (LineKind::Minus, l, 0)));
        rows.extend(a.clone().map(|l| (LineKind::Plus, 0, l)));
        (ob, oa) = (b.end, a.end);
    }
    while ob < bl.len() {
        rows.push((LineKind::Context, ob, oa));
        (ob, oa) = (ob + 1, oa + 1);
    }
    // Keep only context within CONTEXT of changed rows
    let mut keep = vec![false; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        if r.0 != LineKind::Context {
            keep[i.saturating_sub(CONTEXT)..(i + CONTEXT + 1).min(rows.len())].fill(true);
        }
    }
    let clip = |s: &str| s.trim_end_matches(['\n', '\r']).to_string();
    let mut out: Vec<DiffLine> = Vec::new();
    let mut skipped = false;
    for (i, &(kind, o, n)) in rows.iter().enumerate() {
        if !keep[i] {
            skipped = true;
            continue;
        }
        if out.len() >= MAX_LINES {
            break;
        }
        if skipped {
            out.push(DiffLine {
                kind: LineKind::Gap,
                number: 0,
                text: String::new(),
                spans: Vec::new(),
                emph: None,
            });
            skipped = false;
        }
        out.push(match kind {
            LineKind::Minus => DiffLine {
                kind,
                number: o + 1,
                text: clip(bl[o]),
                spans: spans_of(&hb, &before, o),
                emph: None,
            },
            _ => DiffLine {
                kind,
                number: n + 1,
                text: clip(al[n]),
                spans: spans_of(&ha, &after, n),
                emph: None,
            },
        });
    }
    if skipped && !out.is_empty() {
        out.push(DiffLine {
            kind: LineKind::Gap,
            number: 0,
            text: String::new(),
            spans: Vec::new(),
            emph: None,
        });
    }
    // Pairing: consecutive −s with the +s right after them, in order
    let mut i = 0;
    while i < out.len() {
        let m0 = i;
        while i < out.len() && out[i].kind == LineKind::Minus {
            i += 1;
        }
        let p0 = i;
        while i < out.len() && out[i].kind == LineKind::Plus {
            i += 1;
        }
        for k in 0..(p0 - m0).min(i - p0) {
            if let Some((m, p)) = emphasis(&out[m0 + k].text, &out[p0 + k].text) {
                out[m0 + k].emph = Some(m);
                out[p0 + k].emph = Some(p);
            }
        }
        if i == m0 {
            i += 1;
        }
    }
    let removed = rows.iter().filter(|r| r.0 == LineKind::Minus).count();
    let added = rows.iter().filter(|r| r.0 == LineKind::Plus).count();
    FileDiff { path, op, lines: out, added, removed }
}

fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// Changed middle of two paired lines (minus common prefix/suffix, on char boundaries).
/// No highlight if nearly all changed.
fn emphasis(a: &str, b: &str) -> Option<((usize, usize), (usize, usize))> {
    let c = crate::disk::minimal_change(&Rope::from_str(a), &Rope::from_str(b))?;
    let (af, at) = (c.from, c.to);
    let (bf, bt) = (c.from, c.from + c.insert.len());
    let longest = a.len().max(b.len()).max(1);
    if (at - af).max(bt - bf) * 10 > longest * 8 {
        return None;
    }
    Some(((af, at), (bf, bt)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_changes_cover_edits_inserts_and_deletes() {
        let c = line_changes("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(
            c,
            [(2, 4, "b\n".to_string(), "B\n".to_string()), (6, 6, String::new(), "d\n".to_string())]
        );
        // even without a trailing newline on the last line
        let c = line_changes("x\ny", "x\nz");
        assert_eq!(c, [(2, 3, "y".to_string(), "z".to_string())]);
        assert!(line_changes("same\n", "same\n").is_empty());
    }

    /// Format-on-save keeps the formatter's change only on lines changed since the save: line for line
    /// where its change pairs up; whole where it lies within them; a line it adds beside one; and not at all
    /// where it reaches untouched lines without pairing (joining an untouched line into a changed one).
    #[test]
    fn a_save_formats_only_the_lines_changed_since_it_was_saved() {
        let saved = "a  \nb\nc  \n";
        let now = "a  \nB  \nc  \n";
        // (every line's trailing blanks gone: only the changed line's)
        assert_eq!(format_only_touched(saved, now, "a\nB\nc\n"), "a  \nB\nc  \n");
        // (a line added beside the changed one: kept)
        assert_eq!(format_only_touched(saved, now, "a  \n\nB\nc  \n"), "a  \n\nB\nc  \n");
        // (the changed line joined with an untouched one: left out)
        assert_eq!(format_only_touched(saved, now, "a  B\nc  \n"), now);
        // (nothing changed since the save: nothing formatted)
        assert_eq!(format_only_touched(now, now, "a\nB\nc\n"), now);
    }

    /// The reviewer's case: a `use` added, the formatter sorts the imports — it moves the added line above
    /// untouched ones. Its two halves (out where you put it, in where it sorts) are tied: kept both or neither,
    /// and the move reaches untouched lines, so neither — the file is yours as you wrote it. Kept apart, the
    /// line came twice (`use b; use a; use b;`) or was lost.
    #[test]
    fn a_save_never_doubles_or_loses_a_line_the_formatter_moves() {
        let saved = "use b;\nuse c;\nfn main() {}\n";
        let now = "use b;\nuse c;\nuse a;\nfn main() {}\n";
        assert_eq!(format_only_touched(saved, now, "use a;\nuse b;\nuse c;\nfn main() {}\n"), now);
        let (saved, now) = ("use b;\n", "use b;\nuse a;\n");
        assert_eq!(format_only_touched(saved, now, "use a;\nuse b;\n"), now, "the reviewer's own example");
        // A line moved into the lines you changed, from where you didn't: left out
        let saved = "x\nm\nn\n";
        let now = "x\nm\nN\nO\n";
        assert_eq!(format_only_touched(saved, now, "m\nN\nx\nO\n"), now);
        // …and one of yours moved out among untouched ones: left out
        assert_eq!(format_only_touched(saved, now, "N\nx\nm\nO\n"), now);
        // A move within your own lines: kept whole
        let (saved, now) = ("x\ny\n", "x\ny\nB\nA\n");
        assert_eq!(format_only_touched(saved, now, "x\ny\nA\nB\n"), "x\ny\nA\nB\n");
    }

    /// Over random saves, edits and formatter answers (lines shuffled, moved, sorted, added, dropped,
    /// rewritten): what a save writes is always your text with a subset of the formatter's changes, each
    /// whole, and never has a line more than both your text and the formatter's, nor fewer than both — no
    /// line of yours doubled or lost.
    #[test]
    fn a_save_is_your_text_with_whole_changes_of_the_formatters() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        let words = ["a\n", "b\n", "c\n", "d\n", "  e\n", "\n"];
        let edit = |v: &mut Vec<&str>, rand: &mut dyn FnMut(usize) -> usize, moves: bool| {
            for _ in 0..1 + rand(3) {
                let at = rand(v.len() + 1);
                match rand(if moves { 7 } else { 3 }) {
                    0 => v.insert(at, words[rand(words.len())]),
                    1 if at < v.len() => drop(v.remove(at)),
                    2 if at < v.len() => v[at] = words[rand(words.len())],
                    3 if at < v.len() => {
                        let l = v.remove(at);
                        v.insert(rand(v.len() + 1), l);
                    }
                    4 => v.sort(),
                    5 if v.len() > 1 => {
                        let (i, j) = (at.min(v.len() - 1), rand(v.len()));
                        v.swap(i, j);
                    }
                    _ => v.reverse(),
                }
            }
        };
        let count = |v: &[&str], l: &str| v.iter().filter(|x| **x == l).count();
        let mut tried = 0;
        for _ in 0..3000 {
            let saved: Vec<&str> = (0..rand(8)).map(|_| words[rand(words.len())]).collect();
            let mut now = saved.clone();
            edit(&mut now, &mut rand, false);
            let mut formatted = now.clone();
            edit(&mut formatted, &mut rand, true);
            let wrote = format_only_touched(&saved.concat(), &now.concat(), &formatted.concat());
            let out: Vec<&str> = wrote.split_inclusive('\n').collect();
            let case = format!("saved {saved:?} now {now:?} formatted {formatted:?} wrote {out:?}");
            for l in words {
                let (n, f, o) = (count(&now, l), count(&formatted, l), count(&out, l));
                assert!(n.min(f) <= o && o <= n.max(f), "{l:?} {o} times — {case}");
            }
            let hunks = format_hunks(&now, &formatted);
            if hunks.len() <= 10 {
                tried += 1;
                let subset = (0..1u32 << hunks.len()).any(|m| {
                    let kept: Vec<bool> = (0..hunks.len()).map(|i| m & (1 << i) != 0).collect();
                    format_apply(&now, &formatted, &hunks, &kept) == wrote
                });
                assert!(subset, "not your text with whole changes — {case}");
            }
        }
        assert!(tried > 2000, "{tried}");
    }
    use serde_json::json;

    fn edit(line: u64, from: u64, to: u64, text: &str) -> Value {
        json!({ "range": { "start": { "line": line, "character": from }, "end": { "line": line, "character": to } }, "newText": text })
    }

    #[test]
    fn diff_of_a_text_edit_with_context_and_emphasis() {
        let src = "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{x}\");\n}\n";
        let we = json!({ "changes": { "file:///p/a.rs": [edit(1, 8, 9, "_x")] } });
        let files = build(&we, Encoding::Utf8, |_| Some(Rope::from_str(src)));
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!((f.added, f.removed, &f.op), (1, 1, &FileOp::Edit));
        let kinds: Vec<LineKind> = f.lines.iter().map(|l| l.kind).collect();
        use LineKind::*;
        assert_eq!(kinds, [Context, Minus, Plus, Context, Context, Gap]);
        let minus = &f.lines[1];
        assert_eq!((minus.number, minus.text.as_str()), (2, "    let x = 1;"));
        assert_eq!(minus.emph, Some((8, 8)), "nothing deleted");
        assert_eq!(f.lines[2].emph, Some((8, 9)), "inserted one `_`");
        assert_eq!(summary(&files), "1 file  +1 −1");
    }

    #[test]
    fn document_changes_win_over_changes() {
        let we = json!({
            "changes": { "file:///p/a.rs": [edit(0, 0, 0, "y")] },
            "documentChanges": [
                { "textDocument": { "uri": "file:///p/a.rs", "version": 1 }, "edits": [edit(0, 0, 0, "y")] },
            ],
        });
        let files = build(&we, Encoding::Utf8, |_| Some(Rope::from_str("x\n")));
        assert_eq!(files.len(), 1);
        let plus: Vec<&str> =
            files[0].lines.iter().filter(|l| l.kind == LineKind::Plus).map(|l| l.text.as_str()).collect();
        assert_eq!(plus, ["yx"], "applied once, not twice");
    }

    #[test]
    fn file_operations() {
        let we = json!({ "documentChanges": [
            { "kind": "create", "uri": "file:///p/new.rs" },
            { "textDocument": { "uri": "file:///p/new.rs", "version": null }, "edits": [edit(0, 0, 0, "pub fn f() {}\n")] },
            { "kind": "rename", "oldUri": "file:///p/old.rs", "newUri": "file:///p/moved.rs" },
            { "kind": "delete", "uri": "file:///p/gone.rs" },
        ] });
        let files =
            build(&we, Encoding::Utf8, |p| (p != Path::new("/p/new.rs")).then(|| Rope::from_str("x\n")));
        let ops: Vec<(&str, &FileOp)> = files.iter().map(|f| (f.path.to_str().unwrap(), &f.op)).collect();
        assert_eq!(ops[0], ("/p/new.rs", &FileOp::Create), "edits following a created file form one entry");
        assert_eq!(files[0].added, 1);
        assert_eq!(ops[1], ("/p/moved.rs", &FileOp::Rename("/p/old.rs".into())));
        assert_eq!(files[1].lines.len(), 0, "move with content unchanged");
        assert_eq!(ops[2], ("/p/gone.rs", &FileOp::Delete));
        assert_eq!(files[2].removed, 1);
    }
}
