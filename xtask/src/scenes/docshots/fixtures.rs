//! The docs' own projects, and what's in tarae's state before a docs shot starts (`docshots.rs`).
//!
//! Beside the scenes' demo crate: `calc` — small arithmetic with a bug for Claude to find (`add` subtracts) and
//! its failing test, doc comments worth rendering, a README of code blocks in three languages — and `words` — a
//! change a review grades high (an `unsafe` line, a new build script). Each is committed in a repository of its
//! own, then changed, as `demo_crate` is. A seed writes the files tarae reads at its start, naming the projects by
//! their real paths, as tarae will.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::super::{demo_crate, write};
use crate::Result;

/// A docs scene's project, a folder of the shot's home.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Fixture {
    /// The scenes' demo crate (`scenes.rs`) — `Trusted`: with settings that wait for trust.
    Demo,
    Trusted,
    Calc,
    Words,
}

impl Fixture {
    /// Its folder in the shot's home — the name its path bar and the start screen show.
    pub(super) fn folder(self) -> &'static str {
        match self {
            Fixture::Demo | Fixture::Trusted => "tarae-demo",
            Fixture::Calc => "tarae-calc",
            Fixture::Words => "tarae-words",
        }
    }

    /// The project made at `dir`: committed, then changed.
    pub(super) fn make(self, dir: &Path) -> Result<()> {
        match self {
            Fixture::Demo => demo_crate(dir, false),
            Fixture::Trusted => demo_crate(dir, true),
            Fixture::Calc => repo(dir, &calc(), &calc_since(), "Small arithmetic"),
            Fixture::Words => repo(dir, &words(), &words_since(), "Count words"),
        }
    }
}

/// `committed` written and committed at `dir` in a repository of its own, then `since` written over it.
fn repo(dir: &Path, committed: &[(&str, String)], since: &[(&str, String)], message: &str) -> Result<()> {
    committed.iter().try_for_each(|(p, text)| write(&dir.join(p), text))?;
    git(dir, &["init", "-q"])?;
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "-q", "-m", message])?;
    since.iter().try_for_each(|(p, text)| write(&dir.join(p), text))
}

/// git at `dir` with none of your settings (no hooks, no signing), finding no repository above it — as
/// `demo_crate` runs it.
fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=demo", "-c", "user.email=demo@example.com", "-c", "commit.gpgsign=false"])
        .args(["-c", "core.hooksPath=/dev/null", "-c", "init.defaultBranch=main"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("git: {e}"))?;
    if st.success() { Ok(()) } else { Err(format!("git {} failed in {}", args.join(" "), dir.display())) }
}

// ── calc ──────────────────────────────────────────────────────────────────────────

const CALC_LIB: &str = r#"//! Small arithmetic: sums, quotients and means, checked where they can fail.

mod stats;

pub use stats::average;

/// Adds two numbers.
///
/// # Examples
///
/// ```
/// assert_eq!(tarae_calc::add(2, 2), 4);
/// ```
pub fn add(a: i32, b: i32) -> i32 {
    a - b
}

/// Divides `a` by `b`, checked.
///
/// Returns `None` when
/// - `b` is zero, or
/// - the quotient overflows (`i32::MIN / -1`).
pub fn div(a: i32, b: i32) -> Option<i32> {
    a.checked_div(b)
}

/// The larger of two numbers.
pub fn max(a: i32, b: i32) -> i32 {
    if a > b { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 2), 4);
    }

    #[test]
    fn divides() {
        assert_eq!(div(7, 2), Some(3));
        assert_eq!(div(1, 0), None);
    }

    #[test]
    fn averages() {
        assert_eq!(average(&[1, 2, 3]), Some(2.0));
    }
}
"#;

const CALC_STATS: &str = r#"//! Means of a list.

/// The mean of `xs`, or `None` when it's empty.
pub fn average(xs: &[i32]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let sum: i32 = xs.iter().sum();
    Some(f64::from(sum) / xs.len() as f64)
}
"#;

/// The README: Markdown with code blocks in three languages, a `TODO` and a `NOTE` in their comments.
const CALC_README: &str = r#"# tarae-calc

**Small** arithmetic, *checked* where it can fail: `div` returns an `Option`. See [the docs](https://docs.rs).

```rust
/// The sum of the knots.
fn total(xs: &[i32]) -> i32 {
    xs.iter().sum() // TODO: say when it overflows
}
```

```python
def mean(xs: list[int]) -> float:
    return sum(xs) / len(xs)  # NOTE: an empty list raises
```

```sh
#!/usr/bin/env bash
for f in src/*.rs; do echo "$f"; done
```

- a list with `code` and **bold** in it
"#;

fn calc() -> Vec<(&'static str, String)> {
    vec![
        (
            "Cargo.toml",
            "[package]\nname = \"tarae-calc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"
                .into(),
        ),
        (".gitignore", "target/\nCargo.lock\n".into()),
        ("README.md", CALC_README.into()),
        ("src/lib.rs", CALC_LIB.into()),
        ("src/stats.rs", CALC_STATS.into()),
        // (a tree for the file picker: `stat` finds a file in four folders)
        ("src/parse.rs", "//! Numbers read from text.\n\n/// `s` as a number, spaces around it allowed.\npub fn number(s: &str) -> Option<i32> {\n    s.trim().parse().ok()\n}\n".into()),
        ("src/format.rs", "/// `x` with its sign always shown.\npub fn signed(x: i32) -> String {\n    format!(\"{x:+}\")\n}\n".into()),
        ("tests/stats.rs", "#[test]\nfn averages_a_list() {\n    assert_eq!(tarae_calc::average(&[2, 4]), Some(3.0));\n}\n".into()),
        ("tests/sums.rs", "#[test]\nfn adds_negatives() {\n    assert_eq!(tarae_calc::add(-2, -3), -5);\n}\n".into()),
        ("examples/stats.rs", "fn main() {\n    println!(\"{:?}\", tarae_calc::average(&[1, 2, 3, 4]));\n}\n".into()),
        ("docs/statistics.md", "# Statistics\n\n`average` is the mean of a list; `None` when it's empty.\n".into()),
    ]
}

/// Changed since: `min` beside `max` (whose doc says more now), `median` beside `average`.
fn calc_since() -> Vec<(&'static str, String)> {
    let lib = CALC_LIB
        .replace("pub use stats::average;", "pub use stats::{average, median};")
        .replace(
            "/// The larger of two numbers.\npub fn max(a: i32, b: i32) -> i32 {\n    if a > b { a } else { b }\n}\n",
            "/// The larger of two numbers — `b` when they're equal.\npub fn max(a: i32, b: i32) -> i32 {\n    if a > b { a } else { b }\n}\n\n\
             /// The smaller of two numbers.\npub fn min(a: i32, b: i32) -> i32 {\n    if a < b { a } else { b }\n}\n",
        );
    let stats = format!(
        "{CALC_STATS}\n/// The middle of `xs` once sorted (the lower one for an even length), or `None` when it's empty.\n\
         pub fn median(xs: &[i32]) -> Option<i32> {{\n    let mut sorted = xs.to_vec();\n    sorted.sort_unstable();\n    \
         sorted.get(sorted.len().saturating_sub(1) / 2).copied()\n}}\n"
    );
    vec![("src/lib.rs", lib), ("src/stats.rs", stats)]
}

// ── words ────────────────────────────────────────────────────────────────────────────

const WORDS_MAIN: &str = "fn main() {\n    let n = count(\"a b c\");\n    println!(\"{n}\");\n}\n\n\
                          fn count(s: &str) -> usize {\n    s.split_whitespace().count()\n}\n";

fn words() -> Vec<(&'static str, String)> {
    vec![
        (
            "Cargo.toml",
            "[package]\nname = \"tarae-words\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"
                .into(),
        ),
        (".gitignore", "target/\nCargo.lock\n".into()),
        ("README.md", "# tarae-words\n\nCounts the words of a line.\n".into()),
        ("src/main.rs", WORDS_MAIN.into()),
    ]
}

/// Changed since: the count printed with the process id (an `unsafe` call), a test, a new build script, a line
/// more in the README.
fn words_since() -> Vec<(&'static str, String)> {
    let main = WORDS_MAIN.replace(
        "    println!(\"{n}\");\n",
        "    let pid = unsafe { libc::getpid() };\n    println!(\"{n} {pid}\");\n",
    ) + "\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn counts_words() {\n        assert_eq!(super::count(\"a b\"), 2);\n    }\n}\n";
    vec![
        ("src/main.rs", main),
        ("build.rs", "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n".into()),
        ("README.md", "# tarae-words\n\nCounts the words of a line.\n\nRun it with `cargo run`.\n".into()),
    ]
}

// ── What tarae's state holds at the start ───────────────────────────────────────────────────

/// What a docs shot's tarae finds in its state folder when it starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Seed {
    Nothing,
    /// Files opened lately in two projects (`recent.rs`) — the demo's and calc's, both made.
    Recent,
    /// The files last open in the shot's project (`restore.rs`: the start screen's `r`), the first recent.
    LastOpen,
    /// Three earlier conversations of the shot's project (`chats.rs`), the newest a day old — older than the
    /// chat continues on its own.
    Chats,
}

/// `seed` written in `state` (`$XDG_STATE_HOME/tarae`) for a tarae started in `cwd`, its projects in `home` —
/// every path real, as tarae names them; `now` in Unix seconds.
pub(super) fn seed(seed: Seed, state: &Path, home: &Path, cwd: &Path, now: u64) -> Result<()> {
    let lines = |paths: &[std::path::PathBuf]| -> String {
        paths.iter().map(|p| format!("{}\n", p.display())).collect()
    };
    match seed {
        Seed::Nothing => Ok(()),
        Seed::Recent => {
            let (demo, calc) = (home.join(Fixture::Demo.folder()), home.join(Fixture::Calc.folder()));
            for (f, dir) in [(Fixture::Demo, &demo), (Fixture::Calc, &calc)] {
                if !dir.exists() {
                    f.make(dir)?;
                }
            }
            let recent = [
                demo.join("src/main.rs"),
                calc.join("src/lib.rs"),
                demo.join("src/thread.rs"),
                calc.join("README.md"),
            ];
            write(&state.join("recent"), &lines(&recent))
        }
        Seed::LastOpen => {
            let files = [cwd.join("src/lib.rs"), cwd.join("src/stats.rs")];
            let named: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
            let mut folders = serde_json::Map::new();
            folders.insert(cwd.display().to_string(), json!({ "files": named, "current": 0 }));
            write(&state.join("restore.json"), &json!({ "folders": folders }).to_string())?;
            write(&state.join("recent"), &lines(&files[..1]))
        }
        Seed::Chats => {
            let root = cwd.display().to_string();
            let body = json!({ "root": root, "chats": conversations(now) });
            write(&state.join("chats").join(chats_file(&root)), &body.to_string())
        }
    }
}

/// The name of a project's conversations file in `chats/` — `chats::file`'s: FNV-1a (64) of its root.
fn chats_file(root: &str) -> String {
    let h =
        root.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{h:016x}.json")
}

/// calc's earlier conversations, newest first, as the chat saves them (`chats::rows_of`).
fn conversations(now: u64) -> Vec<Value> {
    const DAY: u64 = 24 * 3600;
    let user = |t: &str| json!({ "role": "user", "text": t });
    let said = |t: &str| json!({ "role": "assistant", "text": t });
    let read = |what: &str, path: &str, line: u64| {
        json!({ "role": "tool", "text": "", "look": { "verb": "read", "what": what, "pattern": false,
            "range": "", "path": path, "line": line } })
    };
    let chat = |id: &str, ago: u64, rows: Vec<Value>| {
        let title = rows[0]["text"].clone();
        json!({ "id": id, "title": title, "updated": now - ago, "rows": rows, "seq": (now - ago) * 1_000_000_000 })
    };
    vec![
        chat(
            "conv-add",
            DAY,
            vec![
                user("why does adds() fail?"),
                read("src/lib.rs:14-16", "src/lib.rs", 14),
                said(
                    "`add` returns `a - b`, so `add(2, 2)` is 0 and the test's `assert_eq!(add(2, 2), 4)` fails. \
                     The fix is one character: `a + b`.",
                ),
                user("and the doc example?"),
                said("It runs as a doctest and fails the same way — the same fix makes both pass."),
            ],
        ),
        chat(
            "conv-div",
            3 * DAY,
            vec![
                user("should div return a Result instead of an Option?"),
                read("src/lib.rs:18-25", "src/lib.rs", 18),
                said(
                    "Only if callers need to tell the two failures apart. Today both — a zero divisor and \
                     `i32::MIN / -1` — mean \"no quotient\", and `checked_div` says exactly that.",
                ),
            ],
        ),
        chat(
            "conv-mean",
            16 * DAY,
            vec![
                user("can average overflow?"),
                read("src/stats.rs:4-10", "src/stats.rs", 4),
                said(
                    "Yes: the sum is an `i32`. Summing into an `i64` keeps any slice shorter than 2³² safe.",
                ),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git's word for each path of `dir` that's changed since its commit (`git status --porcelain`) — with none of
    /// your settings, as `git` makes the fixture (a global `status.showUntrackedFiles=no` would hide `build.rs`).
    fn status(dir: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Each project is committed in a repository of its own, then changed — only what its pictures need changed.
    #[test]
    fn the_docs_projects_are_committed_then_changed() {
        let dir = super::super::super::Fresh::new("tarae-docs-fixtures").unwrap();
        let (calc, words) = (dir.0.join("calc"), dir.0.join("words"));
        Fixture::Calc.make(&calc).unwrap();
        Fixture::Words.make(&words).unwrap();
        assert_eq!(status(&calc), " M src/lib.rs\n M src/stats.rs\n");
        assert_eq!(status(&words), " M README.md\n M src/main.rs\n?? build.rs\n");
        let lib = std::fs::read_to_string(calc.join("src/lib.rs")).unwrap();
        assert!(
            lib.contains("    a - b\n") && lib.contains("pub fn min("),
            "the bug stays, min is new:\n{lib}"
        );
        let stat: Vec<&str> = ["src/stats.rs", "tests/stats.rs", "examples/stats.rs", "docs/statistics.md"]
            .into_iter()
            .filter(|p| calc.join(p).is_file())
            .collect();
        assert_eq!(stat.len(), 4, "the picker's `stat` finds a file in four folders: {stat:?}");
        let main = std::fs::read_to_string(words.join("src/main.rs")).unwrap();
        assert!(main.contains("unsafe { libc::getpid() }"), "{main}");
    }

    /// A seed names files as tarae reads them: recent files that exist (in both projects), the last open files
    /// under the shot's folder as its key, a conversations file by `chats::file`'s name (FNV-1a's own test
    /// vectors) whose titles are their first questions, older than the chat continues.
    #[test]
    fn a_seed_names_files_as_tarae_reads_them() {
        let dir = super::super::super::Fresh::new("tarae-docs-seed").unwrap();
        let (home, state) = (dir.0.join("home"), dir.0.join("state"));
        let cwd = home.join(Fixture::Calc.folder());
        Fixture::Calc.make(&cwd).unwrap();
        seed(Seed::Recent, &state, &home, &cwd, 1_000_000).unwrap();
        let recent = std::fs::read_to_string(state.join("recent")).unwrap();
        assert_eq!(recent.lines().count(), 4);
        assert!(
            recent.lines().all(|l| Path::new(l).is_file() && l.starts_with(&*home.to_string_lossy())),
            "{recent}"
        );
        assert!(
            recent.contains("tarae-demo/src/main.rs") && recent.contains("tarae-calc/src/lib.rs"),
            "{recent}"
        );

        seed(Seed::LastOpen, &state, &home, &cwd, 1_000_000).unwrap();
        let restore: Value =
            serde_json::from_str(&std::fs::read_to_string(state.join("restore.json")).unwrap()).unwrap();
        let files = &restore["folders"][cwd.to_string_lossy().as_ref()]["files"];
        assert_eq!(files[0], json!(cwd.join("src/lib.rs").to_string_lossy()), "{restore}");

        assert_eq!(
            (chats_file(""), chats_file("a")),
            ("cbf29ce484222325.json".into(), "af63dc4c8601ec8c.json".into())
        );
        let now = 10_000_000;
        seed(Seed::Chats, &state, &home, &cwd, now).unwrap();
        let file = state.join("chats").join(chats_file(&cwd.to_string_lossy()));
        let chats: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(chats["root"], json!(cwd.to_string_lossy()));
        let all = chats["chats"].as_array().unwrap();
        assert_eq!(all.len(), 3);
        for c in all {
            assert_eq!(c["title"], c["rows"][0]["text"]);
            assert!(now - c["updated"].as_u64().unwrap() > 8 * 3600, "older than `chats::FRESH`: {c}");
        }
    }
}
