//! The project search — ripgrep's own crates in-process: `ignore`'s parallel walk (`.gitignore`, `.ignore`,
//! globs), `grep-searcher` (binary files, BOMs, context lines) and `grep-regex` (smart case, whole words,
//! literals). One path for `space /`, `g r`'s text matches and Claude's `search` tool, the same with or
//! without rg installed. As rg searches with `--hidden --glob=!.git`: hidden files in, `.git` out,
//! `.gitignore` read only inside a git repository. Runs on a worker; stopped in its own threads.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::{WalkBuilder, WalkState};

/// Threads at most — more contend in the kernel opening files and are slower (rg's default, a thread a
/// core: a 200k-file tree took 6.8 s with 14, 75 s of it system time; 2.3 s with 4 — measured 2026-10-03,
/// macOS).
const THREADS: usize = 4;

/// A line's text kept at most this long, in bytes (a minified file's line) — what's shown is shorter.
const TEXT_MAX: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Case {
    /// Any case when the pattern has no uppercase letter (`smart_insensitive`).
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Each matching line (and `context` lines around it).
    Lines,
    /// The files with a match — each file's search ends at its first.
    Files,
    /// How many lines match in each file.
    Count,
}

pub struct Query<'a> {
    pub pattern: &'a str,
    pub literal: bool,
    pub case: Case,
    /// Whole words only.
    pub word: bool,
    /// rg's `--glob`s: `*.rs`, `!*_test.go` — only files they let in.
    pub globs: &'a [String],
    pub context: usize,
    pub mode: Mode,
}

/// One line of a file: its number (from 1), where its first match starts (bytes; 0 for context), whether
/// it matches (else context), its text without the line end.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub line: usize,
    pub col: usize,
    pub hit: bool,
    pub text: String,
}

/// A file's part of the answer: its path from the root (`/`-separated), its rows (`Mode::Lines`), how many
/// lines match (1 in `Mode::Files`).
#[derive(Debug)]
pub struct File {
    pub rel: String,
    pub rows: Vec<Row>,
    pub hits: usize,
}

/// rg's smart case: case-insensitive when no letter the pattern names is uppercase — escapes (`\S`, `\W`)
/// and classes by name (`\p{Lu}`) aren't letters it names; in a literal every character is.
pub fn smart_insensitive(pattern: &str, literal: bool) -> bool {
    if literal {
        return !pattern.chars().any(char::is_uppercase);
    }
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            // (the escaped character, and after `\p`/`\P` a class's name: `\pL`, `\p{Greek}`, `\P{Lu}`)
            '\\' => {
                if matches!(chars.next(), Some('p' | 'P')) && chars.next() == Some('{') {
                    chars.by_ref().find(|&c| c == '}');
                }
            }
            c if c.is_uppercase() => return false,
            _ => {}
        }
    }
    true
}

/// The pattern as the search reads it. Err: why it isn't a pattern (yet), in a few words (`unclosed
/// group`).
pub fn matcher(q: &Query) -> Result<RegexMatcher, String> {
    let insensitive = match q.case {
        Case::Smart => smart_insensitive(q.pattern, q.literal),
        Case::Sensitive => false,
        Case::Insensitive => true,
    };
    RegexMatcherBuilder::new()
        .case_insensitive(insensitive)
        .fixed_strings(q.literal)
        .word(q.word)
        // (a line at a time: `\s` never takes the line's end, as in rg)
        .line_terminator(Some(b'\n'))
        .build(q.pattern)
        .map_err(|e| why(&e.to_string()))
}

/// A regex error's last line, without `error: ` — the regex crate's messages end with the reason.
pub fn why(e: &str) -> String {
    let last = e.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("invalid regex").trim();
    last.strip_prefix("error: ").unwrap_or(last).to_string()
}

/// Searches `scope` (a folder or file under `root`; the root when None) for `q`. Each file with a match
/// goes to `each` as it's found, on this thread (false: enough — the search ends). `stop` is asked before
/// each file and at each match, in the searching threads: a newer key's search ends this one where it
/// is. Ok: the first thing it couldn't read, if any (a folder, a file) — the rest was searched.
pub fn search(
    root: &Path,
    scope: Option<&Path>,
    q: &Query,
    stop: &(dyn Fn() -> bool + Sync),
    each: &mut dyn FnMut(File) -> bool,
) -> Result<Option<String>, String> {
    let matcher = matcher(q)?;
    let mut globs = ignore::overrides::OverrideBuilder::new(root);
    for g in q.globs {
        globs.add(g).map_err(|e| format!("glob {g}: {e}"))?;
    }
    let globs = globs.build().map_err(|e| e.to_string())?;
    let threads = std::thread::available_parallelism().map_or(THREADS, |n| n.get()).min(THREADS);
    let walk = WalkBuilder::new(scope.unwrap_or(root))
        .hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .overrides(globs)
        .filter_entry(|e| e.file_name() != ".git")
        .threads(threads)
        .build_parallel();
    let quit = AtomicBool::new(false);
    let unread: Mutex<Option<String>> = Mutex::new(None);
    let note = |e: String| {
        let mut u = unread.lock().unwrap_or_else(|p| p.into_inner());
        u.get_or_insert(e);
    };
    let ended = || quit.load(Ordering::Relaxed) || stop();
    let (tx, rx) = std::sync::mpsc::channel::<File>();
    std::thread::scope(|s| {
        // (named: the walker's own threads, started from this one, take its name)
        crate::event::scoped(s, "tarae-grep-walk", || {
            walk.run(|| {
                let (tx, matcher) = (tx.clone(), matcher.clone());
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .before_context(if q.mode == Mode::Lines { q.context } else { 0 })
                    .after_context(if q.mode == Mode::Lines { q.context } else { 0 })
                    // (as rg does for the files it walks: a NUL byte = binary, not searched on)
                    .binary_detection(BinaryDetection::quit(b'\x00'))
                    .build();
                let (note, ended) = (&note, &ended);
                Box::new(move |entry| {
                    if ended() {
                        return WalkState::Quit;
                    }
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            note(e.to_string());
                            return WalkState::Continue;
                        }
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let path = entry.path();
                    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
                    let mut sink = Collect {
                        matcher: &matcher,
                        mode: q.mode,
                        ended,
                        file: File { rel, rows: Vec::new(), hits: 0 },
                    };
                    if let Err(e) = searcher.search_path(&matcher, path, &mut sink) {
                        note(format!("{}: {e}", sink.file.rel));
                    }
                    if sink.file.hits > 0 && tx.send(sink.file).is_err() {
                        return WalkState::Quit;
                    }
                    WalkState::Continue
                })
            });
            // (the last sender: the receiver below ends once every searching thread is done)
            drop(tx);
        });
        for file in rx.iter() {
            if !each(file) {
                quit.store(true, Ordering::Relaxed);
                break;
            }
        }
        // (dropped: a thread still sending finds no one and quits)
        drop(rx);
    });
    Ok(unread.into_inner().unwrap_or_else(|p| p.into_inner()))
}

/// A file's matches (and context lines), as the searcher reports them.
struct Collect<'a> {
    matcher: &'a RegexMatcher,
    mode: Mode,
    ended: &'a (dyn Fn() -> bool + Sync),
    file: File,
}

impl Collect<'_> {
    fn row(&mut self, line: Option<u64>, bytes: &[u8], hit: bool) {
        let col = match hit {
            true => self.matcher.find(bytes).ok().flatten().map_or(0, |m| m.start()),
            false => 0,
        };
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if text.len() > TEXT_MAX {
            let mut end = TEXT_MAX;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        self.file.rows.push(Row { line: line.unwrap_or(0) as usize, col, hit, text });
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        self.file.hits += 1;
        match self.mode {
            Mode::Lines => self.row(m.line_number(), m.bytes(), true),
            Mode::Files => return Ok(false),
            Mode::Count => {}
        }
        Ok(!(self.ended)())
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        self.row(c.line_number(), c.bytes(), false);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_reads_the_letters_the_pattern_names() {
        for (p, insensitive) in [
            ("alpha", true),
            ("Alpha", false),
            (r"\Sfoo\W", true),
            (r"\p{Lu}x", true),
            (r"\pLx", true),
            (r"\P{Greek}X", false),
            ("[A-Z]", false),
        ] {
            assert_eq!(smart_insensitive(p, false), insensitive, "{p}");
        }
        assert!(!smart_insensitive(r"\S", true), "in a literal every character is one");
    }

    fn lines(text: &str) -> Query<'_> {
        Query {
            pattern: text,
            literal: false,
            case: Case::Smart,
            word: false,
            globs: &[],
            context: 0,
            mode: Mode::Lines,
        }
    }

    /// Stopped in its own threads: told at a match, a file's search ends there (a newer key's search ends
    /// this one in the middle of a big file); `each` saying enough ends the walk — no more files come.
    #[test]
    fn a_search_stops_where_it_is() {
        let dir = std::env::temp_dir().join(format!("tarae-grep-stop-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big.txt"), "hit\n".repeat(1000)).unwrap();
        // (asked for the file itself, then after each match: the third ask says stop)
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let third = || asked.fetch_add(1, Ordering::SeqCst) >= 2;
        let mut found = Vec::new();
        let r = search(&dir, Some(&dir.join("big.txt")), &lines("hit"), &third, &mut |f| {
            found.push(f.hits);
            true
        });
        assert_eq!((r, found), (Ok(None), vec![2]));
        for i in 0..50 {
            std::fs::write(dir.join(format!("f{i}.txt")), "hit\n").unwrap();
        }
        let mut files = 0;
        search(&dir, None, &Query { mode: Mode::Files, ..lines("hit") }, &|| false, &mut |_| {
            files += 1;
            false
        })
        .unwrap();
        assert_eq!(files, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Context lines around a hit, a run's lines once; counts; globs (and a bad one said); a pattern that
    /// isn't one yet said in a few words.
    #[test]
    fn context_counts_globs_and_errors() {
        let dir = std::env::temp_dir().join(format!("tarae-grep-ctx-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "one\ntwo\r\nthree\ntwo\nfive\n").unwrap();
        std::fs::write(dir.join("b.md"), "two\n").unwrap();
        let all = |q: &Query| {
            let mut v: Vec<File> = Vec::new();
            search(&dir, None, q, &|| false, &mut |f| {
                v.push(f);
                true
            })
            .map(|_| {
                v.sort_by(|a, b| a.rel.cmp(&b.rel));
                v
            })
        };
        let files = all(&Query { context: 1, globs: &["*.rs".into()], ..lines("two") }).unwrap();
        let rows: Vec<(usize, bool, &str)> =
            files[0].rows.iter().map(|r| (r.line, r.hit, r.text.as_str())).collect();
        assert_eq!(files.len(), 1, "only *.rs");
        assert_eq!(
            rows,
            [(1, false, "one"), (2, true, "two"), (3, false, "three"), (4, true, "two"), (5, false, "five")]
        );
        let counts = all(&Query { mode: Mode::Count, ..lines("two") }).unwrap();
        assert_eq!(
            counts.iter().map(|f| (f.rel.as_str(), f.hits)).collect::<Vec<_>>(),
            [("a.rs", 2), ("b.md", 1)]
        );
        let not_md = all(&Query { globs: &["!*.md".into()], ..lines("two") }).unwrap();
        assert_eq!(not_md.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), ["a.rs"]);
        assert_eq!(all(&lines("(two")).unwrap_err(), "unclosed group");
        assert!(all(&Query { globs: &["{a".into()], ..lines("two") }).unwrap_err().starts_with("glob {a: "));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
