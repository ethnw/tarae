//! Unified diffs, for `:session-apply` (`apply.rs`): `parse` reads git's (`diff --git`, its extended headers)
//! and plain ones (`--- a` / `+++ b`); `lay` puts a file's hunks on a text — exactly where they say, or (asked)
//! the one place their lines all are, never loosened; `merge3` merges a base's two descendants by lines, what
//! both changed differently as a diff3 conflict block (`conflict.rs` finds those). Pure: no disk, no git.

/// A patch: the files it changes, in its order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Patch {
    pub files: Vec<FilePatch>,
}

/// One file's part of a patch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilePatch {
    /// Its path before, from the patch's top (None: no file — it makes one).
    pub old: Option<String>,
    /// Its path after (None: it deletes the file).
    pub new: Option<String>,
    /// The blob it was made against, as its `index` line names it (None: no line, or a new file's zeros).
    pub base: Option<String>,
    /// A mode it sets (`new mode` / `new file mode` other than a plain file's) — said, not applied.
    pub mode: Option<String>,
    pub hunks: Vec<Hunk>,
    /// Its `diff --git` (or `---`) line in the patch, 1-based.
    pub line: usize,
}

/// One `@@` hunk: its old lines (context and `-`) at `start`, which become its new ones (context and `+`).
/// Lines keep their `\n` — but a last line `\ No newline at end of file` speaks of.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hunk {
    /// The old text's line it starts at, 0-based (for no old lines: the line it goes before).
    pub start: usize,
    pub old: Vec<String>,
    pub new: Vec<String>,
    /// Its `@@` line in the patch, 1-based.
    pub line: usize,
}

impl FilePatch {
    /// The path it's named by: after, else before.
    pub fn shown(&self) -> &str {
        self.new.as_deref().or(self.old.as_deref()).unwrap_or_default()
    }
}

/// A line of the patch without its line end.
fn bare(l: &str) -> &str {
    let l = l.strip_suffix('\n').unwrap_or(l);
    l.strip_suffix('\r').unwrap_or(l)
}

/// The patch in `text` — Err: why it can't be applied (a binary change, a copy, a hunk cut short, a file named
/// twice, no diff in it at all). What comes before the first file (a mail's headers, a commit message, a
/// diffstat) is passed over.
pub fn parse(text: &str) -> Result<Patch, String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut files: Vec<FilePatch> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = bare(lines[i]);
        let git = l.strip_prefix("diff --git ");
        let plain = l.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "));
        if git.is_none() && !plain {
            i += 1;
            continue;
        }
        let f = file(&lines, &mut i, git)?;
        if let Some(was) = files.iter().find(|g| g.shown() == f.shown()) {
            return Err(format!(
                "{} is in it twice (lines {} and {}) — a series of patches? Apply one commit's diff at a time",
                f.shown(),
                was.line,
                f.line
            ));
        }
        files.push(f);
    }
    match files.is_empty() {
        true => Err("no diff in it — a unified diff (`git diff`, `diff -u`) is what it reads".into()),
        false => Ok(Patch { files }),
    }
}

/// One file from line `*i` (its `diff --git` line — `git`, what follows `diff --git ` — or its `---`), `*i` left
/// after its last hunk.
fn file(lines: &[&str], i: &mut usize, git: Option<&str>) -> Result<FilePatch, String> {
    let mut f = FilePatch { line: *i + 1, ..Default::default() };
    let (mut made, mut deleted) = (false, false);
    let (mut from, mut to) = (None, None);
    // (a rename's own lines name it with no `a/` or `b/`)
    let (mut renamed_from, mut renamed_to) = (None, None);
    if let Some(names) = git {
        if let Some((a, b)) = git_names(names) {
            (from, to) = (Some(a), Some(b));
        }
        *i += 1;
        while let Some(l) = lines.get(*i).map(|l| bare(l)) {
            let named = |p: &str| l.strip_prefix(p).map(str::to_string);
            if let Some(m) = named("new file mode ") {
                made = true;
                f.mode = (m != "100644").then_some(m);
            } else if l.starts_with("deleted file mode ") {
                deleted = true;
            } else if let Some(m) = named("new mode ") {
                f.mode = Some(m);
            } else if let Some(p) = named("rename from ") {
                renamed_from = Some(unquote_whole(&p));
            } else if let Some(p) = named("rename to ") {
                renamed_to = Some(unquote_whole(&p));
            } else if let Some(ids) = named("index ") {
                let old = ids.split("..").next().unwrap_or_default();
                f.base = old.bytes().any(|b| b != b'0').then(|| old.to_string());
            } else if l.starts_with("old mode ") || l.contains("similarity index ") {
                // (nothing to do: `new mode` says the mode, a rename's lines its names)
            } else if l.starts_with("copy from ") || l.starts_with("copy to ") {
                return Err(format!(
                    "line {}: a copy — not applied (only changes, new files, renames and deletions)",
                    *i + 1
                ));
            } else if l.starts_with("GIT binary patch")
                || (l.starts_with("Binary files ") && l.ends_with(" differ"))
            {
                let name = to.as_deref().or(from.as_deref()).map(strip_prefix).unwrap_or_default();
                return Err(format!("{name}: a binary change (line {}) — a patch of text only", *i + 1));
            } else {
                break;
            }
            *i += 1;
        }
    }
    if let (Some(a), Some(b)) = (lines.get(*i), lines.get(*i + 1))
        && a.starts_with("--- ")
        && b.starts_with("+++ ")
    {
        from = Some(name_of(&bare(a)[4..]));
        to = Some(name_of(&bare(b)[4..]));
        *i += 2;
    }
    let null = |n: &Option<String>| n.as_deref() == Some("/dev/null");
    made |= null(&from);
    deleted |= null(&to);
    // (`a/` and `b/` off when each side there has its own — a plain diff's names stay as they are)
    let prefixed = (made || from.as_deref().is_some_and(|p| p.starts_with("a/")))
        && (deleted || to.as_deref().is_some_and(|p| p.starts_with("b/")));
    let clean = |n: Option<String>| n.map(|n| if prefixed { strip_prefix(&n).to_string() } else { n });
    f.old = renamed_from.or(clean(from)).filter(|_| !made);
    f.new = renamed_to.or(clean(to)).filter(|_| !deleted);
    // A plain diff names one file twice (`x.orig` → `x`, `old/a` → `new/a`): not a move (only git's says one) —
    // what both names share past their first folder, else the new name
    if git.is_none()
        && let (Some(a), Some(b)) = (&f.old, &f.new)
        && a != b
    {
        let rest = |n: &str| n.split_once('/').map(|(_, r)| r.to_string());
        let name = match (rest(a), rest(b)) {
            (Some(x), Some(y)) if x == y => x,
            _ => b.clone(),
        };
        (f.old, f.new) = (Some(name.clone()), Some(name));
    }
    if f.old.is_none() && f.new.is_none() {
        return Err(format!("line {}: a file with no name", f.line));
    }
    while let Some(l) = lines.get(*i).filter(|l| l.starts_with("@@ ")) {
        let h = hunk(lines, i, bare(l)).map_err(|e| format!("{}: {e}", f.shown()))?;
        f.hunks.push(h);
    }
    if let Some(l) = lines.get(*i).map(|l| bare(l))
        && (l.starts_with("Subproject commit ") || l.starts_with("-Subproject commit "))
    {
        return Err(format!("{}: a submodule's commit — not applied", f.shown()));
    }
    Ok(f)
}

/// `a/` or `b/` taken off.
fn strip_prefix(n: &str) -> &str {
    n.strip_prefix("a/").or_else(|| n.strip_prefix("b/")).unwrap_or(n)
}

/// The two names after `diff --git ` (`a/x b/x`, quoted or not) — None when they can't be told apart (a name
/// with ` b/` in it, renamed: its `rename` lines say them).
fn git_names(s: &str) -> Option<(String, String)> {
    if let Some((a, rest)) = unquote(s) {
        let rest = rest.trim_start();
        let b = unquote(rest).map_or_else(|| rest.to_string(), |(b, _)| b);
        return Some((a, b));
    }
    // (the same name twice, as most are: `a/<n> b/<n>`)
    let half = s.len().checked_sub(1)? / 2;
    if s.len() % 2 == 1 && s.is_char_boundary(half) {
        let (a, b) = (&s[..half], &s[half + 1..]);
        if a.strip_prefix("a/").is_some_and(|n| Some(n) == b.strip_prefix("b/")) {
            return Some((a.to_string(), b.to_string()));
        }
    }
    let at = s.find(" b/")?;
    (!s[at + 1..].contains(" b/")).then(|| (s[..at].to_string(), s[at + 1..].to_string()))
}

/// A `---`/`+++` line's name: quoted, or up to a tab (a plain diff's timestamp after it).
fn name_of(s: &str) -> String {
    match unquote(s) {
        Some((n, _)) => n,
        None => s.split('\t').next().unwrap_or_default().to_string(),
    }
}

/// A whole name, unquoted when it's quoted.
fn unquote_whole(s: &str) -> String {
    unquote(s).map_or_else(|| s.to_string(), |(n, _)| n)
}

/// A C-quoted name at `s`'s start (git's, for names with special characters): it and what follows — None:
/// `s` isn't one.
fn unquote(s: &str) -> Option<(String, &str)> {
    let body = s.strip_prefix('"')?;
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut k = 0;
    while k < b.len() {
        match b[k] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &body[k + 1..])),
            b'\\' => {
                let e = *b.get(k + 1)?;
                k += 2;
                out.push(match e {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'a' => 7,
                    b'b' => 8,
                    b'f' => 12,
                    b'v' => 11,
                    b'0'..=b'7' => {
                        let digits = b.get(k - 1..k + 2)?;
                        k += 2;
                        u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok()?
                    }
                    other => other,
                });
            }
            c => {
                out.push(c);
                k += 1;
            }
        }
    }
    None
}

/// The hunk whose `@@` line `head` is at `*i`, `*i` left after it.
fn hunk(lines: &[&str], i: &mut usize, head: &str) -> Result<Hunk, String> {
    let line = *i + 1;
    let bad = || format!("line {line}: not a hunk's head — {head}");
    let ranges = head.strip_prefix("@@ -").and_then(|r| r.split(" @@").next()).ok_or_else(bad)?;
    let (old, new) = ranges.split_once(" +").ok_or_else(bad)?;
    let range = |r: &str| -> Option<(usize, usize)> {
        match r.split_once(',') {
            Some((s, n)) => Some((s.parse().ok()?, n.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let ((start, mut olds), (_, mut news)) = (range(old).ok_or_else(bad)?, range(new).ok_or_else(bad)?);
    let mut h =
        Hunk { start: if olds == 0 { start } else { start.saturating_sub(1) }, line, ..Default::default() };
    *i += 1;
    // Which side the last line went to (for `\ No newline at end of file`)
    let mut last: (bool, bool) = (false, false);
    while let Some(&l) = lines.get(*i) {
        if l.starts_with('\\') {
            for (to, side) in [(last.0, &mut h.old), (last.1, &mut h.new)] {
                if to && let Some(t) = side.last_mut().filter(|t| t.ends_with('\n')) {
                    t.pop();
                }
            }
            *i += 1;
            continue;
        }
        if olds == 0 && news == 0 {
            break;
        }
        // (an empty line in a hunk: a context line whose space was trimmed away)
        let (kind, text) = match l {
            "\n" => (' ', "\n"),
            _ => (l.chars().next().unwrap_or_default(), l.get(1..).unwrap_or_default()),
        };
        match kind {
            ' ' if olds > 0 && news > 0 => {
                (olds, news) = (olds - 1, news - 1);
                h.old.push(text.into());
                h.new.push(text.into());
                last = (true, true);
            }
            '-' if olds > 0 => {
                olds -= 1;
                h.old.push(text.into());
                last = (true, false);
            }
            '+' if news > 0 => {
                news -= 1;
                h.new.push(text.into());
                last = (false, true);
            }
            _ => break,
        }
        *i += 1;
    }
    if olds > 0 || news > 0 {
        return Err(format!("the hunk at line {line} ends early ({head})"));
    }
    Ok(h)
}

/// The hunks `lay` found elsewhere: each one's index, by how many lines.
pub type Moved = Vec<(usize, isize)>;

/// One side's change of `merge3`'s base: (from, to, its new text, theirs?).
type Change = (usize, usize, String, bool);

/// Why a hunk didn't go on.
#[derive(Clone, Debug, PartialEq)]
pub enum Unlaid {
    /// Its lines aren't there.
    Missing,
    /// They're in more than one place, none where it says (these lines, 1-based).
    Several(Vec<usize>),
    /// It has no old lines to find its place by (a `-U0` diff), and the file isn't empty.
    NoContext,
}

/// `hunks` on `text`, in order: each exactly at its line (after an earlier one's move) — else, with `moving`,
/// the one place its old lines all are, as written (no fuzz, no whitespace loosened). Ok: the text and each
/// hunk that moved (its index, by how many lines); Err: each hunk that didn't go on (its index, why).
pub fn lay(text: &str, hunks: &[Hunk], moving: bool) -> Result<(String, Moved), Vec<(usize, Unlaid)>> {
    let src: Vec<&str> = text.split_inclusive('\n').collect();
    let (mut out, mut at, mut by, mut moved, mut failed) = (String::new(), 0, 0isize, Vec::new(), Vec::new());
    let fits = |p: usize, h: &Hunk| src.get(p..p + h.old.len()).is_some_and(|w| w.iter().eq(h.old.iter()));
    for (n, h) in hunks.iter().enumerate() {
        let want = h.start.checked_add_signed(by).unwrap_or(0);
        let p = match () {
            _ if h.old.is_empty() && moving && !text.is_empty() => Err(Unlaid::NoContext),
            _ if want >= at && want <= src.len() && fits(want, h) => Ok(want),
            _ if !moving || h.old.is_empty() => Err(Unlaid::Missing),
            _ => match (at..src.len()).filter(|&p| fits(p, h)).collect::<Vec<_>>()[..] {
                [] => Err(Unlaid::Missing),
                [p] => {
                    by = p as isize - h.start as isize;
                    moved.push((n, by));
                    Ok(p)
                }
                ref ps => Err(Unlaid::Several(ps.iter().map(|p| p + 1).collect())),
            },
        };
        match p {
            Ok(p) => {
                out.extend(src[at..p].iter().copied());
                out.extend(h.new.iter().map(String::as_str));
                at = p + h.old.len();
            }
            Err(why) => failed.push((n, why)),
        }
    }
    if !failed.is_empty() {
        return Err(failed);
    }
    out.extend(src[at..].iter().copied());
    Ok((out, moved))
}

/// `base`'s two descendants merged by lines: what one changed and the other didn't, taken; what both changed
/// the same way, once; what both changed on or next to the same lines differently, a diff3 block —
/// `<<<<<<< labels[0]` `this`'s lines, `||||||| labels[1]` the base's, `=======`, `theirs`', `>>>>>>> labels[2]`.
/// (text, how many conflicts).
pub fn merge3(base: &str, this: &str, theirs: &str, labels: [&str; 3]) -> (String, usize) {
    if this == base || this == theirs {
        return (theirs.to_string(), 0);
    }
    if theirs == base {
        return (this.to_string(), 0);
    }
    // Both sides' changes on `base`, by where they start
    let mut all: Vec<Change> = crate::editdiff::line_changes(base, this)
        .into_iter()
        .map(|c| (c.0, c.1, c.3, false))
        .chain(crate::editdiff::line_changes(base, theirs).into_iter().map(|c| (c.0, c.1, c.3, true)))
        .collect();
    crate::sorting::by_key(&mut all, |c| (c.0, c.1));
    // Changes that overlap or touch are one place (as git's merge has it: neighbouring lines too)
    let mut groups: Vec<(usize, usize, Vec<Change>)> = Vec::new();
    for c in all {
        match groups.last_mut() {
            Some(g) if c.0 <= g.1 => {
                g.1 = g.1.max(c.1);
                g.2.push(c);
            }
            _ => groups.push((c.0, c.1, vec![c])),
        }
    }
    // One side's text of `base[lo..hi]`, its changes there made
    let side = |lo: usize, hi: usize, cs: &[&(usize, usize, String, bool)]| {
        let (mut s, mut at) = (String::new(), lo);
        for c in cs {
            s.push_str(&base[at..c.0]);
            s.push_str(&c.2);
            at = c.1;
        }
        s + &base[at..hi]
    };
    let ended = |mut s: String| {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s
    };
    let (mut out, mut at, mut conflicts) = (String::with_capacity(this.len()), 0, 0);
    for (lo, hi, cs) in groups {
        out.push_str(&base[at..lo]);
        at = hi;
        let (mine, wanted): (Vec<_>, Vec<_>) = cs.iter().partition(|c| !c.3);
        let (ours, them) = (side(lo, hi, &mine), side(lo, hi, &wanted));
        match (mine.is_empty(), wanted.is_empty()) {
            (_, true) => out.push_str(&ours),
            (true, _) => out.push_str(&them),
            _ if ours == them => out.push_str(&ours),
            _ => {
                conflicts += 1;
                let [a, b, c] = labels;
                out.push_str(&format!("<<<<<<< {a}\n{}", ended(ours)));
                out.push_str(&format!("||||||| {b}\n{}", ended(base[lo..hi].to_string())));
                out.push_str(&format!("=======\n{}>>>>>>> {c}\n", ended(them)));
            }
        }
    }
    out.push_str(&base[at..]);
    (out, conflicts)
}

#[cfg(test)]
mod tests;
