//! A run's output, kept once (panes P2c): its lines as the command printed them — tabs too, expanded only where
//! they're drawn — in one `Rope`, each line's stream beside it. The run panel's document holds the same text, its rope's nodes shared with
//! this one (`rundoc::follow` lays a clone of it over the document once it follows): a run that keeps 20,000 lines
//! keeps them once. What reads a run back — its report to Claude, a preflight step's `o`, the sandbox's denials, the
//! test results — reads these; a reader that takes a list of lines gets one made for it then (`to_vec`), kept by
//! nobody.

use std::borrow::Cow;

use ropey::Rope;

/// A run's lines (`task::Run.output`, `testing::TestRun.output`).
#[derive(Clone, Default)]
pub struct Output {
    /// Every line, each ending in a line break.
    text: Rope,
    /// Each line's stream: stderr.
    err: Vec<bool>,
}

/// A line as it's kept: as printed, but a line break the rope would count — none should be left by then
/// (`task::as_shown`) — a space. A tab stays a tab: Claude's reports, the matcher and `o` read what was printed.
fn shown(line: &str) -> Cow<'_, str> {
    let breaks =
        |c: char| matches!(c, '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}');
    match line.contains(breaks) {
        true => Cow::Owned(line.chars().map(|c| if breaks(c) { ' ' } else { c }).collect()),
        false => Cow::Borrowed(line),
    }
}

impl Output {
    /// Its lines.
    pub fn len(&self) -> usize {
        self.err.len()
    }

    pub fn is_empty(&self) -> bool {
        self.err.is_empty()
    }

    /// The text as kept — the run panel's document's (`rundoc.rs`).
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// Where line `i` starts in `text` (its end past the last).
    pub fn byte_of(&self, i: usize) -> usize {
        match i < self.len() {
            true => self.text.line_to_byte(i),
            false => self.text.len_bytes(),
        }
    }

    /// Line `i`, without its line break.
    pub fn line(&self, i: usize) -> Option<Cow<'_, str>> {
        if i >= self.len() {
            return None;
        }
        let l = self.text.line(i);
        Some(l.slice(..l.len_chars().saturating_sub(1)).into())
    }

    /// Every line from `from` on: (stderr, the line).
    pub fn iter_from(&self, from: usize) -> impl Iterator<Item = (bool, Cow<'_, str>)> + '_ {
        (from..self.len()).map(|i| (self.err[i], self.line(i).unwrap_or_default()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (bool, Cow<'_, str>)> + '_ {
        self.iter_from(0)
    }

    /// The first line `f` says yes to.
    pub fn position(&self, mut f: impl FnMut(&str) -> bool) -> Option<usize> {
        self.iter().position(|(_, l)| f(&l))
    }

    /// Its lines as a list, for a reader that takes one (the sandbox's report) — made now, kept by nobody.
    pub fn to_vec(&self) -> Vec<(bool, String)> {
        self.iter().map(|(e, l)| (e, l.into_owned())).collect()
    }

    /// A line more at the end.
    pub fn push(&mut self, err: bool, line: &str) {
        let end = self.text.len_chars();
        self.text.insert(end, &shown(line));
        self.text.insert_char(self.text.len_chars(), '\n');
        self.err.push(err);
    }

    /// A line put in before line `at`.
    pub fn insert(&mut self, at: usize, err: bool, line: &str) {
        let at = at.min(self.len());
        let c = self.text.byte_to_char(self.byte_of(at));
        self.text.insert(c, &format!("{}\n", shown(line)));
        self.err.insert(at, err);
    }

    /// Line `at`'s text replaced.
    pub fn set(&mut self, at: usize, line: &str) {
        if at >= self.len() {
            return;
        }
        let (a, b) = (self.text.line_to_char(at), self.text.line_to_char(at + 1) - 1);
        self.text.remove(a..b);
        self.text.insert(a, &shown(line));
    }

    /// Its first `n` lines kept, the rest taken out.
    #[cfg(test)]
    pub fn truncate(&mut self, n: usize) {
        self.drain(n..);
    }

    /// Lines `r` taken out.
    pub fn drain(&mut self, r: impl std::ops::RangeBounds<usize>) {
        use std::ops::Bound;
        let to = match r.end_bound() {
            Bound::Included(&e) => e + 1,
            Bound::Excluded(&e) => e,
            Bound::Unbounded => self.len(),
        };
        let to = to.min(self.len());
        let from = match r.start_bound() {
            Bound::Included(&s) => s,
            Bound::Excluded(&s) => s + 1,
            Bound::Unbounded => 0,
        };
        let from = from.min(to);
        let (a, b) = (self.text.line_to_char(from), self.text.byte_to_char(self.byte_of(to)));
        self.text.remove(a..b);
        self.err.drain(from..to);
    }
}

/// As its lines: (stderr, line) each.
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<S: AsRef<str>> FromIterator<(bool, S)> for Output {
    fn from_iter<I: IntoIterator<Item = (bool, S)>>(lines: I) -> Output {
        let mut o = Output::default();
        o.extend(lines);
        o
    }
}

impl<S: AsRef<str>> Extend<(bool, S)> for Output {
    fn extend<I: IntoIterator<Item = (bool, S)>>(&mut self, lines: I) {
        for (e, l) in lines {
            self.push(e, l.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    fn lines_are_kept_once_as_printed_and_trimmed_line_for_line() {
        let mut o: Output = [(false, "a\tb"), (true, "two"), (false, "")].into_iter().collect();
        assert_eq!(
            (o.len(), o.line(0).as_deref(), o.iter().nth(1).map(|l| l.0)),
            (3, Some("a\tb"), Some(true))
        );
        assert_eq!(o.text().to_string(), "a\tb\ntwo\n\n");
        o.insert(1, false, "cut");
        o.set(1, "… 1 line not kept …");
        o.drain(2..3);
        assert_eq!(o.text().to_string(), "a\tb\n… 1 line not kept …\n\n");
        assert_eq!(
            o.to_vec(),
            vec![(false, "a\tb".into()), (false, "… 1 line not kept …".into()), (false, String::new())]
        );
        o.push(true, "x\ry");
        assert_eq!((o.line(3).as_deref(), o.position(|l| l == "x y")), (Some("x y"), Some(3)));
        o.drain(0..10);
        assert!(o.is_empty() && o.text().len_bytes() == 0);
    }
}
