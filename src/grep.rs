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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Case {
    /// Any case when the pattern has no uppercase letter (`smart_insensitive`).
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mode {
    /// Each matching line (and `context` lines around it).
    #[default]
    Lines,
    /// The files with a match — each file's search ends at its first.
    Files,
    /// How many lines match in each file.
    Count,
    /// Every file the walk takes in, its pattern unread (rg's `--files`).
    Names,
}

#[derive(Default)]
pub struct Query<'a> {
    pub pattern: &'a str,
    pub literal: bool,
    pub case: Case,
    /// Whole words only.
    pub word: bool,
    /// rg's `--glob`s: `*.rs`, `!*_test.go` — only files they let in.
    pub globs: &'a [String],
    /// rg's `--type`s: `rust`, `py` — only files of these (rg's own table).
    pub types: &'a [String],
    pub context: usize,
    pub mode: Mode,
    /// A match may span lines, `.` taking a line's end too (rg's `-U --multiline-dotall`).
    pub multiline: bool,
    /// Lines that also match this regex aren't hits (empty: none left out) — grep's `-v` after a search.
    pub exclude: &'a str,
    /// Claude's search (`nav::search`, its places judged): each file opened first and searched only when the
    /// descriptor's real path is in one of the scopes (`paths::open_read`) — a folder swapped for a link
    /// between the walk and the open can't send it where nothing was judged. (The scopes are real paths.)
    pub judged: bool,
}

/// (Tests: run with each file's path before a judged search opens it — what a run writing the project could
/// do in between.)
#[cfg(test)]
pub static BEFORE_OPEN: Mutex<Option<BeforeOpen>> = Mutex::new(None);

#[cfg(test)]
pub type BeforeOpen = Box<dyn Fn(&Path) + Send + Sync>;

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
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(insensitive).fixed_strings(q.literal).word(q.word);
    match q.multiline {
        true => b.multi_line(true).dot_matches_new_line(true),
        // (a line at a time: `\s` never takes the line's end, as in rg)
        false => b.line_terminator(Some(b'\n')),
    };
    b.build(q.pattern).map_err(|e| why(&e.to_string()))
}

/// The exclude pattern as the search reads it (as the pattern's case) — None: none.
fn excluder(q: &Query) -> Result<Option<RegexMatcher>, String> {
    if q.exclude.is_empty() {
        return Ok(None);
    }
    let insensitive = match q.case {
        Case::Smart => smart_insensitive(q.exclude, false),
        Case::Sensitive => false,
        Case::Insensitive => true,
    };
    RegexMatcherBuilder::new()
        .case_insensitive(insensitive)
        .build(q.exclude)
        .map(Some)
        .map_err(|e| format!("exclude: {}", why(&e.to_string())))
}

/// A regex error's last line, without `error: ` — the regex crate's messages end with the reason.
pub fn why(e: &str) -> String {
    let last = e.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("invalid regex").trim();
    last.strip_prefix("error: ").unwrap_or(last).to_string()
}

/// Searches `scopes` (folders or files under `root`; the root when none) for `q`. Each file with a match
/// (`Mode::Names`: each file) goes to `each` as it's found, on this thread (false: enough — the search
/// ends). `stop` is asked before each file and at each match, in the searching threads: a newer key's
/// search ends this one where it is. Ok: the first thing it couldn't read, if any (a folder, a file) — the
/// rest was searched.
pub fn search(
    root: &Path,
    scopes: &[std::path::PathBuf],
    q: &Query,
    stop: &(dyn Fn() -> bool + Sync),
    each: &mut dyn FnMut(File) -> bool,
) -> Result<Option<String>, String> {
    let matcher = matcher(q)?;
    let exclude = excluder(q)?;
    let mut globs = ignore::overrides::OverrideBuilder::new(root);
    for g in q.globs {
        globs.add(g).map_err(|e| format!("glob {g}: {e}"))?;
    }
    let globs = globs.build().map_err(|e| e.to_string())?;
    let mut types = ignore::types::TypesBuilder::new();
    types.add_defaults();
    for t in q.types {
        types.select(t);
    }
    let types =
        types.build().map_err(|e| format!("{e} (rg's type names: rust, py, js, ts, go, java, md …)"))?;
    let threads = std::thread::available_parallelism().map_or(THREADS, |n| n.get()).min(THREADS);
    let mut walk = WalkBuilder::new(scopes.first().map_or(root, |s| s.as_path()));
    for s in scopes.iter().skip(1) {
        walk.add(s);
    }
    let walk = walk
        .hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .overrides(globs)
        .types(types)
        .filter_entry(|e| e.file_name() != ".git")
        .threads(threads)
        .build_parallel();
    // (where a judged search's files must really be)
    let within: Vec<std::path::PathBuf> = match scopes.is_empty() {
        true => vec![root.to_path_buf()],
        false => scopes.to_vec(),
    };
    let within = &within;
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
                let (tx, matcher, exclude) = (tx.clone(), matcher.clone(), exclude.clone());
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .multi_line(q.multiline)
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
                    if q.mode == Mode::Names {
                        let file = File { rel, rows: Vec::new(), hits: 0 };
                        return if tx.send(file).is_err() { WalkState::Quit } else { WalkState::Continue };
                    }
                    let mut sink = Collect {
                        matcher: &matcher,
                        exclude: exclude.as_ref(),
                        mode: q.mode,
                        ended,
                        file: File { rel, rows: Vec::new(), hits: 0 },
                    };
                    let searched = match q.judged {
                        false => searcher.search_path(&matcher, path, &mut sink),
                        true => {
                            #[cfg(test)]
                            if let Some(hook) = BEFORE_OPEN.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
                            {
                                hook(path);
                            }
                            match crate::paths::open_read(path) {
                                Ok(o) if o.meta.is_file() && within.iter().any(|s| o.real.starts_with(s)) => {
                                    searcher.search_file(&matcher, &o.file, &mut sink)
                                }
                                // (what was opened isn't where the walk was: changed meanwhile — not read)
                                Ok(_) => {
                                    Err(std::io::Error::other("changed while it was searched — not read"))
                                }
                                Err(e) => Err(e),
                            }
                        }
                    };
                    if let Err(e) = searched {
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
    exclude: Option<&'a RegexMatcher>,
    mode: Mode,
    ended: &'a (dyn Fn() -> bool + Sync),
    file: File,
}

impl Collect<'_> {
    /// Line `line`'s row — a multiline match's bytes are a row a line, all hits.
    fn row(&mut self, line: Option<u64>, bytes: &[u8], hit: bool) {
        let first = line.unwrap_or(0) as usize;
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        for (k, bytes) in bytes.split(|&b| b == b'\n').enumerate() {
            let col = match hit && k == 0 {
                true => self.matcher.find(bytes).ok().flatten().map_or(0, |m| m.start()),
                false => 0,
            };
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            let mut text = String::from_utf8_lossy(bytes).into_owned();
            if text.len() > TEXT_MAX {
                let mut end = TEXT_MAX;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            self.file.rows.push(Row { line: first + k, col, hit, text });
        }
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        if self.exclude.is_some_and(|x| x.is_match(m.bytes()).unwrap_or(false)) {
            return Ok(!(self.ended)());
        }
        self.file.hits += 1;
        match self.mode {
            Mode::Lines => self.row(m.line_number(), m.bytes(), true),
            Mode::Files => return Ok(false),
            Mode::Count | Mode::Names => {}
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
        Query { pattern: text, ..Default::default() }
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
        let r = search(&dir, &[dir.join("big.txt")], &lines("hit"), &third, &mut |f| {
            found.push(f.hits);
            true
        });
        assert_eq!((r, found), (Ok(None), vec![2]));
        for i in 0..50 {
            std::fs::write(dir.join(format!("f{i}.txt")), "hit\n").unwrap();
        }
        let mut files = 0;
        search(&dir, &[], &Query { mode: Mode::Files, ..lines("hit") }, &|| false, &mut |_| {
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
            search(&dir, &[], q, &|| false, &mut |f| {
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

    /// A judged search reads each file where it really is: a folder swapped for a link to a place outside
    /// between the walk and the open (a run writing the project can) isn't followed — the file isn't read,
    /// and the answer says so.
    #[test]
    fn a_judged_search_doesnt_follow_a_folder_swapped_for_a_link() {
        let base = std::env::temp_dir().join(format!("tarae-grep-swap-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let (dir, outside) = (base.join("p"), base.join("outside"));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "marker in\n").unwrap();
        std::fs::write(outside.join("a.txt"), "marker out\n").unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (sub, out) = (dir.join("sub"), std::fs::canonicalize(&outside).unwrap());
        let swapped = std::sync::Arc::new(AtomicBool::new(false));
        let s = swapped.clone();
        let target = sub.join("a.txt");
        *BEFORE_OPEN.lock().unwrap() = Some(Box::new(move |p: &Path| {
            if p == target && !s.swap(true, Ordering::SeqCst) {
                std::fs::rename(&sub, sub.with_extension("old")).unwrap();
                std::os::unix::fs::symlink(&out, &sub).unwrap();
            }
        }));
        let mut found = Vec::new();
        let q = Query { judged: true, ..lines("marker") };
        let warn = search(&dir, std::slice::from_ref(&dir), &q, &|| false, &mut |f| {
            found.extend(f.rows.into_iter().map(|r| r.text));
            true
        });
        *BEFORE_OPEN.lock().unwrap() = None;
        assert!(swapped.load(Ordering::SeqCst), "the swap happened");
        assert!(found.is_empty(), "nothing read through the link: {found:?}");
        assert!(warn.unwrap().is_some_and(|w| w.contains("changed while it was searched")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// What Claude Code's Grep offers, in-process: rg's file types, several places at once, a match across
    /// lines (each of its lines a hit row), hit lines left out by another pattern, and the files alone
    /// without reading them (rg `--files`).
    #[test]
    fn types_places_multiline_exclude_and_names() {
        let dir = std::env::temp_dir().join(format!("tarae-grep-more-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for d in ["a", "b", "c"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            std::fs::write(dir.join(d).join("x.rs"), "fn one() {\n    two()\n}\n// fn note\n").unwrap();
            std::fs::write(dir.join(d).join("x.md"), "fn one\n").unwrap();
        }
        let all = |scopes: &[std::path::PathBuf], q: &Query| {
            let mut v: Vec<File> = Vec::new();
            search(&dir, scopes, q, &|| false, &mut |f| {
                v.push(f);
                true
            })
            .map(|_| {
                v.sort_by(|a, b| a.rel.cmp(&b.rel));
                v
            })
        };
        let names = |v: &[File]| v.iter().map(|f| f.rel.clone()).collect::<Vec<_>>();
        let rust = ["rust".to_string()];
        let typed = all(&[], &Query { types: &rust, ..lines("fn") }).unwrap();
        assert_eq!(names(&typed), ["a/x.rs", "b/x.rs", "c/x.rs"], "rg's rust type");
        assert_eq!(typed[0].hits, 2);
        let two = all(&[dir.join("a"), dir.join("c")], &Query { types: &rust, ..lines("fn") }).unwrap();
        assert_eq!(names(&two), ["a/x.rs", "c/x.rs"], "two places");
        let across =
            all(&[dir.join("a")], &Query { multiline: true, types: &rust, ..lines(r"one\(\) \{.*two") });
        let rows: Vec<(usize, bool)> = across.unwrap()[0].rows.iter().map(|r| (r.line, r.hit)).collect();
        assert_eq!(rows, [(1, true), (2, true)], "a match across lines: a hit row each");
        let code = all(&[dir.join("a")], &Query { exclude: "^\\s*//", types: &rust, ..lines("fn") }).unwrap();
        assert_eq!((code[0].hits, code[0].rows.len()), (1, 1), "the comment's line left out");
        let files = all(&[dir.join("b")], &Query { mode: Mode::Names, ..lines("") }).unwrap();
        assert_eq!(names(&files), ["b/x.md", "b/x.rs"], "every file, none read");
        let nope = ["nope".to_string()];
        assert!(all(&[], &Query { types: &nope, ..lines("fn") }).unwrap_err().contains("nope"));
        assert!(all(&[], &Query { exclude: "(", ..lines("fn") }).unwrap_err().starts_with("exclude: "));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
