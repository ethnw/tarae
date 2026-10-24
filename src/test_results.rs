//! Test results — a run's output read into per-test results (pass·fail·skip · duration · failure message ·
//! failure location): a line at a time by the rules a pack gives (`[[test.line]]`, `Rules` — libtest, `go
//! test -v`, pytest in the built-in packs), or from the JUnit XML reports a run leaves (Gradle, Maven, vitest
//! — read once it ends; location from `at pkg.Class.method(File.java:12)`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use regex::Regex;

use crate::packs::{known, string};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed,
    Skipped,
}

/// One test's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    pub name: String,
    pub status: Status,
    pub secs: Option<f32>,
    /// Failure message (multiple lines — the first is the gist).
    pub message: Vec<String>,
    /// Where it failed (file, 0-based line).
    pub at: Option<(PathBuf, usize)>,
}

impl Case {
    fn new(name: &str, status: Status) -> Case {
        Case { name: name.to_string(), status, secs: None, message: Vec::new(), at: None }
    }
}

/// Relative path → actual file — from the run folder, else its ancestors (paths relative to the
/// workspace root).
fn resolve(cwd: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return std::fs::canonicalize(p).ok();
    }
    // Canonical path so it compares with document paths (macOS /tmp → /private/tmp)
    cwd.ancestors().map(|d| d.join(p)).find(|f| f.is_file()).map(|f| std::fs::canonicalize(&f).unwrap_or(f))
}

/// A fragment like `a/b.java:21` → (path, 0-based line).
fn file_line(s: &str, ext: &str) -> Option<(String, usize)> {
    let i = s.find(ext)?;
    let path = s[..i + ext.len()].rsplit([' ', '(', '\'']).next()?.to_string();
    let n: String = s[i + ext.len()..].strip_prefix(':')?.chars().take_while(char::is_ascii_digit).collect();
    Some((path, n.parse::<usize>().ok()?.saturating_sub(1)))
}

/// A test that skipped itself says so on a line of its own — `tarae-test: skipped <name> — <why>` — written
/// past libtest's capture (`io::stderr()`), which would otherwise call it `ok`: tarae's Seatbelt tests can't
/// run inside a sandbox (`sandbox::skipped_inside`). The case reads as skipped, with the why.
pub const SELF_SKIP: &str = "tarae-test: skipped ";

/// The variable naming the file a run's tests write their skips to (in its `RunDir` — `sandbox::SKIPS_FILE`).
pub const SKIPS: &str = "TARAE_SKIPS";

/// A test that skips itself says so — `SELF_SKIP` then `rest` (`<name> — <why>`) on a line of its own, past
/// libtest's capture — and, in a run that says where (`SKIPS`), in that file too: the run's report counts it
/// whatever a filter on its output kept (frictions29 #29: `| grep 'test result'` dropped the lines, and a
/// sandboxed `cargo test` read as all passed). The one way a skip is said (`every_self_skip_goes_through_said_skip`).
#[cfg(test)]
pub fn said_skip(rest: &str) {
    use std::io::Write as _;
    let line = format!("{SELF_SKIP}{}\n", rest.trim_end());
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(at) = std::env::var_os(SKIPS) {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(at);
        let _ = file.and_then(|mut f| f.write_all(line.as_bytes()));
    }
}

/// A `perf_` test measures an optimized build: at the test profile's opt-level 0 (a fast edit-test loop, where
/// a key costs several times as much) it skips itself — true — saying so past libtest's capture by its full
/// name (`term::tests::perf_…`, else the skip lands on another test's row); `cargo perf` runs it in the `perf`
/// profile.
#[cfg(test)]
pub fn unoptimized(name: &str) -> bool {
    if cfg!(tarae_optimized) {
        return false;
    }
    said_skip(&format!("{name} — built at opt-level 0 (the test profile); `cargo perf` runs it"));
    true
}

/// How long a test waits for what it waits on before it calls it a hang. A guard, never the test's logic: it
/// only stops a test that would otherwise never end, so it's generous — a shared runner, or this Mac while several
/// sessions build at once, runs the suite 3–5× slower, and guards of a few seconds tripped there one a round
/// (CI's rounds 1–6). A passing test
/// never waits this long. One value for every test: the e2e harness's `TIMEOUT` and xtask's `PATIENCE` are the
/// same (`every_test_waits_with_one_patience`, which also keeps new deadlines off). What a test checks against
/// time — a timer under test, a program's own `read -t`, "viewed past 3 s" — is a condition or the
/// simulation's clock, never a deadline; a look that nothing happens meanwhile is `quiet_for`'s.
#[cfg(test)]
pub const PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// The end of a while in which a test looks that nothing happens (no event, no frame, nothing sent) — never a
/// wait for what must happen (that's a condition, `PATIENCE` its guard): a slower machine only sees less happen
/// in it, so it can't fail there.
#[cfg(test)]
pub fn quiet_for(d: std::time::Duration) -> std::time::Instant {
    std::time::Instant::now() + d
}

/// Every test waits with one patience: a deadline written in a test (`Instant::now() + Duration…`, a
/// `recv_timeout` of whole seconds) outside these helpers fails here — a hang guard is `PATIENCE`, a look that
/// nothing happens `quiet_for`, what a test checks against time a condition or the simulation's clock. The list
/// holds what's left, per file, and only shrinks (the message prints the table as it is now). The e2e harness's
/// `TIMEOUT`, xtask's and tarae-trace's `PATIENCE` say the same value.
#[cfg(test)]
#[test]
fn every_test_waits_with_one_patience() {
    let now = crate::sim::lint::test_counts(DEADLINES, &["src/test_results.rs"]);
    crate::sim::lint::only_shrinks("deadlines in tests", &now, TEST_DEADLINES);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let same = format!("Duration::from_secs({})", PATIENCE.as_secs());
    for (file, name) in [
        ("tests/harness/mod.rs", "pub const TIMEOUT: Duration = "),
        ("xtask/src/main.rs", "const PATIENCE: std::time::Duration = std::time::"),
        ("trace/tests/cli.rs", "const PATIENCE: Duration = "),
    ] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(text.contains(&format!("{name}{same};")), "{file}: its {name}… isn't {same}");
    }
}

/// What a deadline written in a test looks like (`every_test_waits_with_one_patience`).
#[cfg(test)]
const DEADLINES: &[&str] = &[
    "Instant::now() + Duration::from_",
    "Instant::now() + std::time::Duration::from_",
    "recv_timeout(Duration::from_secs(",
    "recv_timeout(std::time::Duration::from_secs(",
];

/// Deadlines left in tests, per file (from the repository's root) — none a guard of a test that runs unasked.
#[cfg(test)]
const TEST_DEADLINES: &[(&str, usize)] = &[
    // (a by-hand measure's run limit, its row saying `killed after N s`)
    ("src/sandbox/macdeny_measure.rs", 1),
    // (the mutation properties' length by hand, `TARAE_WEB_MUTATE_SECS`: how long, never a wait)
    ("src/web/tests.rs", 2),
];

/// Only test code counts — a test-only item or file, every file of `tests/` — never a comment, and not code a
/// build without tests has (its deadlines are the program's).
#[cfg(test)]
#[test]
fn a_deadline_counts_only_in_test_code() {
    let main = "mod a;\n#[cfg(test)]\nmod t;\nfn f() { let end = Instant::now() + Duration::from_secs(5); }\n\
                #[cfg(test)]\nmod tests {\n    fn g() { let end = Instant::now() + Duration::from_millis(9); }\n}\n";
    let a = "// Instant::now() + Duration::from_secs(1)\nfn h() {}\n";
    let t = "fn k() { rx.recv_timeout(std::time::Duration::from_secs(3)); }\n";
    let files = [("main.rs", main), ("a.rs", a), ("t.rs", t)].map(|(p, s)| (p.to_string(), s.to_string()));
    let n = crate::sim::lint::test_count_tree(&files, DEADLINES, false);
    assert_eq!(n.into_iter().collect::<Vec<_>>(), [("main.rs".to_string(), 1), ("t.rs".to_string(), 1)]);
    let n = crate::sim::lint::test_count_tree(&files, DEADLINES, true);
    assert_eq!(n.get("main.rs"), Some(&2), "all of it a test crate's");
}

/// A skip's line as `said_skip` writes it, for a fixture to hold (a run's skips file, an output) — never said.
#[cfg(test)]
pub fn skip_line(rest: &str) -> String {
    format!("{SELF_SKIP}{rest}")
}

/// frictions29 #29: a skip said goes past libtest's capture and, where the run names a file (`SKIPS`), into it —
/// in a process of its own (the variable is the process's): this binary on `skip_said_in_its_own_process`.
#[cfg(test)]
#[test]
fn a_skip_is_said_in_the_runs_file_too() {
    let dir = crate::scratch::Scratch::new("skips");
    let file = dir.path().join("skips");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "test_results::skip_said_in_its_own_process", "--ignored", "--nocapture"])
        .env(SKIPS, &file)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let line = format!("{SELF_SKIP}a::b — no pty opens here\n");
    assert!(err.contains(&line), "{err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), line.repeat(2), "appended, each");
}

#[cfg(test)]
#[test]
#[ignore = "a_skip_is_said_in_the_runs_file_too's own process"]
fn skip_said_in_its_own_process() {
    said_skip("a::b — no pty opens here");
    said_skip("a::b — no pty opens here\n");
}

/// The writers of `SELF_SKIP` lines that don't go through `said_skip` yet, by file — a list that only shrinks
/// (frictions29 #29: one that writes the line itself is missed by a filtered run's report). A new writer uses
/// `said_skip`; converting one lowers its file's count.
/// (2026-10-19: the macOS-built writers converted; these are Linux's tests, built only there — `cargo xtask
/// linux` checks a conversion.)
#[cfg(test)]
const SKIP_WRITERS_LEFT: &[(&str, usize)] = &[
    ("editor.rs", 1),
    ("landlock.rs", 5),
    ("sandbox.rs", 11),
    ("sandbox/keeper.rs", 10),
    ("sandbox/linux.rs", 2),
    ("sandbox/owners/tests.rs", 1),
    ("sandbox/supervisor.rs", 4),
    ("sandbox/tier.rs", 2),
    ("seccomp.rs", 1),
];

#[cfg(test)]
#[test]
fn every_self_skip_goes_through_said_skip() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found: Vec<(String, usize)> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("test_results.rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                // (a reader strips the prefix; a writer puts it in a line)
                let writes = |l: &&str| {
                    l.contains("SELF_SKIP")
                        && !l.contains("strip_prefix")
                        && !l.trim_start().starts_with("//")
                };
                let n = text.lines().filter(writes).count();
                if n > 0 {
                    let rel = p.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
                    found.push((rel, n));
                }
            }
        }
    }
    found.sort();
    let left: Vec<(String, usize)> = SKIP_WRITERS_LEFT.iter().map(|(f, n)| (f.to_string(), *n)).collect();
    let over: Vec<String> = found
        .iter()
        .filter_map(|(f, n)| {
            let allowed = left.iter().find(|(g, _)| g == f).map_or(0, |(_, m)| *m);
            (*n > allowed).then(|| format!("(\"{f}\", {n}) — {allowed} allowed"))
        })
        .collect();
    assert!(
        over.is_empty(),
        "lines that write `SELF_SKIP` themselves — use `said_skip`:\n{}",
        over.join("\n")
    );
}

/// How a pack's runner says its results, a line at a time — `[test]`'s `[[test.line]]` and `none-ran`, read
/// on the worker that finds the test (`packs.rs`) and carried with it (a run reads no pack).
///
/// Each line tries the rules in order; the first whose `match` (a regular expression) matches the line acts,
/// by its named groups — `name` (the test), `status`, `secs`, `file` and `line` (where it failed), `message`
/// (a line of why) — and by what it says:
/// - `status = { ok = "passed", FAILED = "failed", ignored = "skipped" }`: by the `status` group's text; or
///   `status = "skipped"` whatever the line.
/// - `begin = true`: the lines after this one are about the test it names (`---- a::b stdout ----`); `end =
///   true`: about none. A rule with no `name` group acts on that test — with none, the line is nobody's.
/// - `known = true`: only a test already listed — by its name, or as the end of one's `::`-joined name — else
///   none (a heading that names no test). `name-replace = { "." = "::" }`: the name as the results say it.
/// - `phase = "…"`: the rule counts only in that phase of the output (`""`: outside every one); `to = "…"`: the
///   phase from this line on (pytest's `FAILURES` block).
/// - `place = "last"`: a later place replaces this one until the test has a message (a traceback's last
///   frame); else the first place that's a file stays.
///
/// `none-ran = [0, 5]`: a run that listed no test and ended with one of these codes ran none (a filter that
/// matched nothing). A test listed is failed until a line says otherwise; one that said it skipped isn't a
/// pass after; and tarae's own `SELF_SKIP` line is read whatever the rules.
#[derive(Clone, Debug)]
pub struct Rules {
    lines: Vec<LineRule>,
    none_ran: Vec<i32>,
    /// What they were read from: two runs read alike when it's the same (`testing::How`'s equality).
    source: String,
}

impl PartialEq for Rules {
    fn eq(&self, o: &Rules) -> bool {
        self.source == o.source
    }
}

impl Eq for Rules {}

#[derive(Clone, Debug)]
struct LineRule {
    re: Regex,
    status: Says,
    begin: bool,
    end: bool,
    known: bool,
    name_replace: Vec<(String, String)>,
    phase: Option<String>,
    to: Option<String>,
    place_last: bool,
}

/// The status a line says.
#[derive(Clone, Debug)]
enum Says {
    Nothing,
    Is(Status),
    /// By the `status` group's text.
    By(Vec<(String, Status)>),
}

/// The named groups a rule's `match` may have.
const GROUPS: &[&str] = &["name", "status", "secs", "file", "line", "message"];

fn status_named(s: &str) -> Result<Status, String> {
    match s {
        "passed" => Ok(Status::Passed),
        "failed" => Ok(Status::Failed),
        "skipped" => Ok(Status::Skipped),
        _ => Err(format!("no status `{s}` (there are: passed, failed, skipped)")),
    }
}

impl Rules {
    /// `[test]`'s `line` and `none-ran` — None when it has no `line` (its output isn't read a line at a time).
    pub fn read(t: &toml::Table) -> Result<Option<Rules>, String> {
        let Some(list) = t.get("line") else {
            return match t.contains_key("none-ran") {
                true => Err("`none-ran` with no [[test.line]] rules to list a test".into()),
                false => Ok(None),
            };
        };
        let list = list.as_array().ok_or("`line` isn't a list ([[test.line]])")?;
        let mut lines = Vec::new();
        for (n, e) in list.iter().enumerate() {
            let r = e.as_table().ok_or("a [[test.line]] isn't a table")?;
            let what = format!("[[test.line]] {}", n + 1);
            let rule = Self::rule(r).map_err(|e| format!("{what}: {e}"))?;
            lines.push(rule);
        }
        let none_ran = match t.get("none-ran") {
            None => Vec::new(),
            Some(v) => {
                let codes = v.as_array().ok_or("`none-ran` isn't a list of exit codes")?;
                let code = |c: &toml::Value| c.as_integer().and_then(|c| i32::try_from(c).ok());
                codes.iter().map(code).collect::<Option<_>>().ok_or("`none-ran` holds a non-number")?
            }
        };
        let source = format!("{} {:?}", toml::Value::Array(list.clone()), none_ran);
        Ok(Some(Rules { lines, none_ran, source }))
    }

    fn rule(r: &toml::Table) -> Result<LineRule, String> {
        known(
            r,
            &["match", "status", "begin", "end", "known", "name-replace", "phase", "to", "place"],
            "it",
        )?;
        let pattern = string(r, "match")?.ok_or("no `match`")?;
        let re = Regex::new(&pattern).map_err(|e| format!("`match`: {e}"))?;
        let groups: Vec<&str> = re.capture_names().flatten().collect();
        if let Some(g) = groups.iter().find(|g| !GROUPS.contains(g)) {
            return Err(format!("no group `{g}` (there are: {})", GROUPS.join(", ")));
        }
        if groups.contains(&"file") != groups.contains(&"line") {
            return Err("a place is `file` and `line` together".into());
        }
        let flag = |k: &str| match r.get(k) {
            None => Ok(false),
            Some(v) => v.as_bool().ok_or(format!("`{k}` is true or false")),
        };
        let status = match r.get("status") {
            None => Says::Nothing,
            Some(toml::Value::String(s)) => Says::Is(status_named(s)?),
            Some(toml::Value::Table(by)) => {
                if !groups.contains(&"status") {
                    return Err("`status = { … }` reads a `status` group, and `match` has none".into());
                }
                let mut said = Vec::new();
                for (text, s) in by {
                    let s = s.as_str().ok_or(format!("`status.{text}` isn't a string"))?;
                    said.push((text.clone(), status_named(s)?));
                }
                Says::By(said)
            }
            Some(_) => return Err("`status` is a status or a table of them".into()),
        };
        let name_replace = match r.get("name-replace") {
            None => Vec::new(),
            Some(v) => {
                let t = v.as_table().ok_or("`name-replace` is a table (`{ \".\" = \"::\" }`)")?;
                let each = |(a, b): (&String, &toml::Value)| b.as_str().map(|b| (a.clone(), b.to_string()));
                t.iter().map(each).collect::<Option<_>>().ok_or("`name-replace` holds a non-string")?
            }
        };
        let place_last = match string(r, "place")?.as_deref() {
            None | Some("first") => false,
            Some("last") => true,
            Some(p) => return Err(format!("no `place = \"{p}\"` (there are: first, last)")),
        };
        Ok(LineRule {
            re,
            status,
            begin: flag("begin")?,
            end: flag("end")?,
            known: flag("known")?,
            name_replace,
            phase: string(r, "phase")?,
            to: string(r, "to")?,
            place_last,
        })
    }

    /// One line for the language table's snapshot: the rules' count, their phases and groups, `none-ran`.
    #[cfg(test)]
    pub(crate) fn summary(&self) -> String {
        let mut phases: Vec<&str> = self.lines.iter().filter_map(|l| l.to.as_deref()).collect();
        phases.retain(|p| !p.is_empty());
        phases.dedup();
        let mut groups: Vec<&str> = self.lines.iter().flat_map(|l| l.re.capture_names().flatten()).collect();
        crate::sorting::sort(&mut groups);
        groups.dedup();
        let mut s = format!("{} line rules · groups {}", self.lines.len(), groups.join(" "));
        if !phases.is_empty() {
            s += &format!(" · phases {}", phases.join(" "));
        }
        if !self.none_ran.is_empty() {
            let codes: Vec<String> = self.none_ran.iter().map(i32::to_string).collect();
            s += &format!(" · none ran at exit {}", codes.join(" "));
        }
        s
    }

    /// A built-in pack's rules (tests that run a pack's tests by hand).
    #[cfg(test)]
    pub(crate) fn builtin(lang: &str) -> Option<Rules> {
        let t = crate::packs::builtin_manifests().iter().find(|(l, _)| l == lang)?.1.as_ref().ok()?;
        Rules::read(t.get("test")?.as_table()?).unwrap()
    }
}

/// Reads a run's output a line at a time, by its pack's rules (none: it isn't read so — JUnit's after).
#[derive(Debug, Default)]
pub struct Parser {
    rules: Option<Rules>,
    phase: String,
    /// The case the lines are about (`begin`).
    current: Option<usize>,
    /// Each case's place in `cases`, by its name — a long run's lines don't look through every case — and
    /// how many cases it has seen.
    index: HashMap<String, usize>,
    indexed: usize,
}

impl Parser {
    pub fn new(rules: Option<Rules>) -> Parser {
        Parser { rules, ..Default::default() }
    }

    /// Whether a finished run ran no test: one read test by test that listed none and ended as its pack says
    /// a run of none ends (libtest's `running 0 tests` and 0, pytest's 5). One not read per test can't tell —
    /// its exit code stands.
    pub fn ran_none(&self, code: Option<i32>, cases: &[Case]) -> bool {
        let Some(r) = &self.rules else { return false };
        cases.is_empty() && code.is_some_and(|c| r.none_ran.contains(&c))
    }

    /// `name`'s case, listed (failed until a line says otherwise) if it isn't yet.
    fn find(&mut self, cases: &mut Vec<Case>, name: &str) -> usize {
        if let Some(i) = self.listed(cases, name) {
            return i;
        }
        cases.push(Case::new(name, Status::Failed));
        self.index.insert(name.to_string(), cases.len() - 1);
        self.indexed = cases.len();
        cases.len() - 1
    }

    /// `name`'s case if it's listed.
    fn listed(&mut self, cases: &[Case], name: &str) -> Option<usize> {
        // (cases listed elsewhere — a finished run's, in tests — are indexed once seen)
        if self.indexed != cases.len() {
            self.index = cases.iter().enumerate().rev().map(|(i, c)| (c.name.clone(), i)).collect();
            self.indexed = cases.len();
        }
        self.index.get(name).copied().filter(|&i| cases.get(i).is_some_and(|c| c.name == name))
    }

    /// Reads one line — adds to or updates `cases`.
    pub fn line(&mut self, cwd: &Path, line: &str, cases: &mut Vec<Case>) {
        let Some(rules) = self.rules.take() else { return };
        self.read(&rules, cwd, line, cases);
        self.rules = Some(rules);
    }

    fn read(&mut self, rules: &Rules, cwd: &Path, line: &str, cases: &mut Vec<Case>) {
        if let Some(rest) = line.strip_prefix(SELF_SKIP) {
            let (name, why) = rest.split_once(" — ").unwrap_or((rest, ""));
            let i = self.find(cases, name.trim());
            cases[i].status = Status::Skipped;
            cases[i].message = vec![why.trim().to_string()].into_iter().filter(|w| !w.is_empty()).collect();
            return;
        }
        let Some((r, caps)) = rules
            .lines
            .iter()
            .filter(|r| r.phase.as_ref().is_none_or(|p| *p == self.phase))
            .find_map(|r| Some((r, r.re.captures(line)?)))
        else {
            return;
        };
        if let Some(to) = &r.to {
            (self.phase, self.current) = (to.clone(), None);
        }
        if r.end {
            self.current = None;
        }
        let i = match caps.name("name") {
            Some(n) => {
                let mut name = n.as_str().trim().to_string();
                for (a, b) in &r.name_replace {
                    name = name.replace(a.as_str(), b);
                }
                let i = match r.known {
                    true => self.listed(cases, &name).or_else(|| {
                        let end = format!("::{name}");
                        cases.iter().position(|c| c.name.ends_with(&end))
                    }),
                    false => Some(self.find(cases, &name)),
                };
                if r.begin {
                    self.current = i;
                }
                i
            }
            None => self.current.filter(|&i| i < cases.len()),
        };
        let Some(i) = i else { return };
        let c = &mut cases[i];
        let status = match &r.status {
            Says::Nothing => None,
            Says::Is(s) => Some(*s),
            Says::By(by) => {
                caps.name("status").and_then(|t| by.iter().find(|(k, _)| k == t.as_str())).map(|b| b.1)
            }
        };
        // (a test that said it skipped stays skipped when the runner calls it a pass after)
        if let Some(s) = status
            && !(s == Status::Passed && c.status == Status::Skipped)
        {
            c.status = s;
        }
        if let Some(secs) = caps.name("secs").and_then(|s| s.as_str().parse().ok()) {
            c.secs = Some(secs);
        }
        if let (Some(f), Some(l)) = (caps.name("file"), caps.name("line"))
            && (if r.place_last { c.message.is_empty() } else { c.at.is_none() })
            && let Some(path) = resolve(cwd, f.as_str())
            && let Ok(l) = l.as_str().parse::<usize>()
        {
            c.at = Some((path, l.saturating_sub(1)));
        }
        if let Some(m) = caps.name("message") {
            c.message.push(m.as_str().to_string());
        }
    }
}

/// Unescapes entities in XML text.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&#13;", "")
        .replace("&amp;", "&")
}

/// `tests/test_a.py:12:` (a word of a failure's text) → the file, under one of `roots`, and its line.
fn place_in(word: &str, roots: &[PathBuf]) -> Option<(PathBuf, usize)> {
    let w = word.trim_matches(|c: char| "()[]<>,'\"".contains(c));
    let mut parts = w.split(':');
    let file = parts.next().filter(|f| f.contains('.'))?;
    let line: usize = parts.next()?.parse().ok().filter(|&l| l > 0)?;
    let path = roots.iter().map(|r| r.join(file)).find(|p| p.is_file())?;
    Some((std::fs::canonicalize(&path).unwrap_or(path), line - 1))
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let rest = &tag[tag.find(&key)? + key.len()..];
    Some(unescape(&rest[..rest.find('"')?]))
}

/// JUnit XML reports (Gradle `build/test-results/test` · Maven `target/surefire-reports` · vitest ·
/// pytest `--junitxml`) written after `since`.
/// Failure location = that class's stack line `at pkg.Class.method(File.java:12)` → file found in
/// `src_roots`; else the first `path:line` in the failure whose file is there. A classname that's a file
/// (vitest: `src/a.test.ts`) leaves the test's own name as it is (`math > adds`).
pub fn junit(report_dirs: &[PathBuf], since: SystemTime, src_roots: &[PathBuf]) -> Vec<Case> {
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = report_dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        .filter(|p| std::fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t >= since))
        .collect();
    crate::sorting::sort(&mut files);
    for f in files {
        let Ok(xml) = std::fs::read_to_string(&f) else { continue };
        let mut rest = xml.as_str();
        while let Some(i) = rest.find("<testcase ") {
            rest = &rest[i..];
            let head_end = rest.find('>').unwrap_or(rest.len());
            let head = &rest[..head_end];
            let self_closing = head.ends_with('/');
            let body_end = if self_closing { head_end } else { rest.find("</testcase>").unwrap_or(head_end) };
            let body = &rest[head_end.min(body_end)..body_end];
            let class = attr(head, "classname").unwrap_or_default();
            let short = class.rsplit('.').next().unwrap_or(&class).replace('$', ".");
            let test = attr(head, "name").unwrap_or_default();
            let is_file = |c: &str| c.contains('/') || src_roots.iter().any(|r| r.join(c).is_file());
            let name = if class.is_empty() || is_file(&class) {
                test
            } else {
                format!("{short}.{}", test.trim_end_matches("()"))
            };
            let mut c = Case::new(&name, Status::Passed);
            c.secs = attr(head, "time").and_then(|t| t.parse().ok());
            if body.contains("<skipped") {
                c.status = Status::Skipped;
            }
            if let Some(j) = body.find("<failure").or_else(|| body.find("<error")) {
                c.status = Status::Failed;
                let tag = &body[j..body[j..].find('>').map_or(body.len(), |k| j + k)];
                if let Some(m) = attr(tag, "message") {
                    // Gradle prefixes the message with the exception name
                    // (`org.opentest4j.AssertionFailedError: …`) — strip it
                    let ty = attr(tag, "type").unwrap_or_default();
                    let m = m.strip_prefix(&format!("{ty}: ")).unwrap_or(&m);
                    c.message.extend(m.lines().map(str::to_string));
                }
                // This class's line in the stack → source file
                let outer = class.split('$').next().unwrap_or(&class);
                let text = unescape(&body[j..]);
                c.at = text.lines().map(str::trim).filter_map(|l| l.strip_prefix("at ")).find_map(|l| {
                    if !l.starts_with(outer) {
                        return None;
                    }
                    let (file, line) = file_line(l.split_once('(')?.1, ".java")?;
                    let pkg_dir =
                        outer.rsplit_once('.').map(|(p, _)| p.replace('.', "/")).unwrap_or_default();
                    let path =
                        src_roots.iter().map(|r| r.join(&pkg_dir).join(&file)).find(|p| p.is_file())?;
                    Some((std::fs::canonicalize(&path).unwrap_or(path), line))
                });
                if c.at.is_none() {
                    c.at = text.split_whitespace().find_map(|w| place_in(w, src_roots));
                }
            }
            out.push(c);
            rest = &rest[body_end.max(1)..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `perf_` test in the editor starts by skipping itself at opt-level 0 (`unoptimized`), by its own
    /// full name — one that didn't held a release budget in the test profile and failed under load.
    #[test]
    fn every_perf_test_skips_itself_by_its_own_name() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        let mut n = 0;
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            // The module the tests are in: `a/b.rs` → `a::b::tests`, `a/tests.rs` → `a::tests`, a file of tests of
            // its own `a/b_tests.rs` → `a::b_tests`
            let rel = file.strip_prefix(&src).unwrap().with_extension("");
            let module: Vec<String> = rel.iter().map(|s| s.to_string_lossy().to_string()).collect();
            let module = match module.last().map(String::as_str) {
                Some(m) if m == "tests" || m.ends_with("_tests") => module.join("::"),
                _ => format!("{}::tests", module.join("::")),
            };
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let Some(name) = line.trim().strip_prefix("fn perf_").and_then(|r| r.strip_suffix("() {"))
                else {
                    continue;
                };
                let next = lines.get(i + 1).copied().unwrap_or_default();
                let want = format!("crate::test_results::unoptimized(\"{module}::perf_{name}\")");
                assert!(next.contains(&want), "{}: perf_{name} starts with `{want}`: {next}", file.display());
                n += 1;
            }
        }
        assert!(n >= 17, "found {n}");
    }

    /// A test that skipped itself (`SELF_SKIP`, written past the capture) reads as skipped — with the why —
    /// though libtest calls it `ok` after; others are as libtest says.
    #[test]
    fn a_test_that_skipped_itself_isnt_a_pass() {
        let mut p = Parser::new(Rules::builtin("rust"));
        let mut cases = Vec::new();
        let cwd = std::env::temp_dir();
        for l in [
            "tarae-test: skipped sandbox::tests::real — Seatbelt can't start inside a sandbox",
            "test sandbox::tests::real ... ok",
            "test sandbox::tests::plain ... ok",
        ] {
            p.line(&cwd, l, &mut cases);
        }
        let got: Vec<_> = cases.iter().map(|c| (c.name.as_str(), c.status, c.message.join(""))).collect();
        assert_eq!(
            got,
            [
                (
                    "sandbox::tests::real",
                    Status::Skipped,
                    "Seatbelt can't start inside a sandbox".to_string()
                ),
                ("sandbox::tests::plain", Status::Passed, String::new()),
            ]
        );
    }

    /// A run that listed no test ran none when it ends as its pack says one of none ends (pytest's 5 too);
    /// one that listed a test, or ended otherwise, ran some; a run not read a line at a time can't tell.
    #[test]
    fn each_pack_says_how_a_run_of_none_ends() {
        let none = |lang: &str, code: i32| Parser::new(Rules::builtin(lang)).ran_none(Some(code), &[]);
        assert!(none("rust", 0) && none("go", 0) && none("python", 0) && none("python", 5));
        assert!(!none("rust", 101) && !none("go", 1) && !none("rust", 5) && !none("python", 1));
        assert!(!none("java", 0), "junit: its exit code stands");
        let one = [Case::new("a", Status::Passed)];
        assert!(!Parser::new(Rules::builtin("rust")).ran_none(Some(0), &one));
    }

    /// A pack's rules that don't hold are said, naming the rule — not skipped, not crashed on.
    #[test]
    fn rules_that_dont_read_are_said() {
        let read = |toml: &str| Rules::read(&toml.parse::<toml::Table>().unwrap()).map(|r| r.is_some());
        assert_eq!(read("run = []"), Ok(false), "no rules: not read a line at a time");
        assert_eq!(read("[[line]]\nmatch = '^ok (?<name>.+)'\nstatus = 'passed'"), Ok(true));
        let bad = |toml: &str| read(toml).unwrap_err();
        assert!(bad("none-ran = [0]").contains("no [[test.line]]"));
        assert!(bad("[[line]]\nmatch = '('").contains("[[test.line]] 1: `match`"));
        assert!(
            bad("[[line]]\nmatch = 'x'\n[[line]]\nmatch = 'x'\nbegins = true")
                .contains("2: it has no `begins`")
        );
        assert!(bad("[[line]]\nmatch = '(?<test>x)'").contains("no group `test`"));
        assert!(bad("[[line]]\nmatch = '(?<file>x)'").contains("`file` and `line` together"));
        assert!(bad("[[line]]\nmatch = 'x'\nstatus = 'ok'").contains("no status `ok`"));
        assert!(bad("[[line]]\nmatch = 'x'\nstatus = { ok = 'passed' }").contains("has none"));
        assert!(bad("[[line]]\nmatch = 'x'\nplace = 'middle'").contains("first, last"));
        assert!(bad("none-ran = ['0']\n[[line]]\nmatch = 'x'").contains("non-number"));
    }

    /// The pack of one's own in the docs reads `go test -v` as the Go pack does — its rules are the pack's.
    #[test]
    fn the_go_rules_in_the_docs_are_the_packs() {
        let md = include_str!("../docs/testing-and-debugging.md");
        let block = md.split("```toml\n").find(|b| b.contains("packs/go/pack.toml — run from")).unwrap();
        let t: toml::Table = block.split("```").next().unwrap().parse().unwrap();
        let docs = Rules::read(t["test"].as_table().unwrap()).unwrap();
        assert!(
            docs.is_some() && docs == Rules::builtin("go"),
            "the docs' [[test.line]] drifted from the go pack"
        );
    }

    /// Lines are about the test a heading names until another heading or an end; a heading that names no
    /// test listed (`known`) leaves its lines nobody's; a later place replaces an earlier one until the
    /// message (`place = "last"`).
    #[test]
    fn rules_follow_the_test_a_heading_names() {
        let d = tmp_with("rules", &["a.x", "b.x"]);
        let rules = "none-ran = [0]\n\
            [[line]]\nmatch = '^(?<name>\\S+) (?<status>ok|bad)$'\nstatus = { ok = 'passed', bad = 'failed' }\n\
            [[line]]\nmatch = '^== (?<name>.+)$'\nbegin = true\nknown = true\nname-replace = { '.' = '::' }\n\
            [[line]]\nmatch = '^--$'\nend = true\n\
            [[line]]\nmatch = '^at (?<file>\\S+):(?<line>\\d+)'\nplace = 'last'\n\
            [[line]]\nmatch = '^! (?<message>.*)$'\n";
        let rules = Rules::read(&rules.parse::<toml::Table>().unwrap()).unwrap();
        let text = "m::t1 bad\nm::t2 ok\n== t1\nat a.x:3\nat b.x:9\n! why\nat a.x:1\n--\n! nobody's\n\
                    == m.t2\n! also\n== setup of t3\n! lost";
        let c = feed(&mut Parser::new(rules), &d, text);
        let got: Vec<_> = c.iter().map(|c| (c.name.as_str(), c.status, c.message.join("|"))).collect();
        assert_eq!(
            got,
            [("m::t1", Status::Failed, "why".to_string()), ("m::t2", Status::Passed, "also".to_string())]
        );
        assert_eq!(c[0].at, Some((d.join("b.x"), 8)), "the last place before the message");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn feed(p: &mut Parser, cwd: &Path, text: &str) -> Vec<Case> {
        let mut cases = Vec::new();
        for l in text.lines() {
            p.line(cwd, l, &mut cases);
        }
        cases
    }

    fn tmp_with(tag: &str, files: &[&str]) -> PathBuf {
        let d = crate::scratch::Scratch::new(&format!("results-{tag}")).into_path();
        for f in files {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        d
    }

    /// Whatever a test run prints, each pack's rules read it line by line without a panic — a place at line 0, a
    /// status no rule names, a name of nothing — and a case's place, where one is read, is a file that's there.
    /// The runners' own words mutated, 20,000 lines a pack.
    #[test]
    fn a_test_runs_output_is_read_without_a_panic() {
        use crate::mutate::{self, Rng};
        let d = crate::scratch::Scratch::new("results-loop");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/math.rs"), "x").unwrap();
        let seeds = [
            "running 3 tests".to_string(),
            "test math::tests::adds ... ok".into(),
            "test math::tests::divides ... FAILED".into(),
            "test math::tests::slow ... ignored, takes long".into(),
            "---- math::tests::divides stdout ----".into(),
            "thread 'math::tests::divides' panicked at src/math.rs:21:9:".into(),
            "thread 'math::tests::x' panicked at src/math.rs:0:1:".into(),
            "test result: FAILED. 1 passed; 1 failed; 0 ignored".into(),
            "=== RUN   TestAdd".into(),
            "--- FAIL: TestAdd (0.00s)".into(),
            "    math_test.go:12: got 3".into(),
            "ok  \texample.com/m\t0.002s".into(),
            "tests/test_a.py::test_y FAILED".into(),
            "FAILED tests/test_a.py::test_y - assert 1 == 2".into(),
            format!("{SELF_SKIP}math::tests::x — needs y"),
        ];
        let words = ["::", " ... ", "ok", "FAILED", ":0:", "src/math.rs", " — ", SELF_SKIP];
        let mut rng = Rng(0x5eed_0add_7e55_010e);
        let mut read = 0;
        for lang in ["rust", "go", "python", "java", "javascript", "typescript"] {
            let Some(rules) = Rules::builtin(lang) else { continue };
            let mut p = Parser::new(Some(rules));
            let mut cases = Vec::new();
            for i in 0..20_000 {
                if i % 200 == 0 {
                    cases.clear();
                }
                let mut l = rng.pick(&seeds).clone();
                mutate::text(&mut rng, &mut l, &words);
                p.line(d.path(), &l, &mut cases);
                for c in &cases {
                    assert!(c.at.as_ref().is_none_or(|(f, _)| f.exists()), "{lang}: {c:?} after {l:?}");
                }
                read += usize::from(!cases.is_empty());
            }
        }
        assert!(read > 10_000, "{read}");
    }

    #[test]
    fn reads_libtest_output() {
        let d = tmp_with("rust", &["src/math.rs"]);
        let out = "running 3 tests\ntest math::tests::adds ... ok\ntest math::tests::divides ... FAILED\ntest math::tests::slow ... ignored, takes long\n\nfailures:\n\n---- math::tests::divides stdout ----\n\nthread 'math::tests::divides' panicked at src/math.rs:21:9:\nassertion `left == right` failed: 7 / 2 rounds down\n  left: 3\n right: 4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    math::tests::divides\n\ntest result: FAILED. 1 passed; 1 failed; 1 ignored";
        let c = feed(&mut Parser::new(Rules::builtin("rust")), &d, out);
        let st: Vec<_> = c.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            st,
            [
                ("math::tests::adds", Status::Passed),
                ("math::tests::divides", Status::Failed),
                ("math::tests::slow", Status::Skipped)
            ]
        );
        assert_eq!(
            c[1].message,
            ["assertion `left == right` failed: 7 / 2 rounds down", "  left: 3", " right: 4"]
        );
        assert_eq!(c[1].at, Some((d.join("src/math.rs"), 20)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_go_test_v_output() {
        let d = tmp_with("go", &["calc_test.go"]);
        let out = "=== RUN   TestAdd\n    calc_test.go:8: Add = 4\n--- FAIL: TestAdd (0.00s)\n=== RUN   TestAddNegative\n--- PASS: TestAddNegative (0.01s)\nFAIL\nexit status 1";
        let c = feed(&mut Parser::new(Rules::builtin("go")), &d, out);
        assert_eq!((c[0].name.as_str(), c[0].status, c[0].secs), ("TestAdd", Status::Failed, Some(0.0)));
        assert_eq!(c[0].message, ["Add = 4"]);
        assert_eq!(c[0].at, Some((d.join("calc_test.go"), 7)));
        assert_eq!((c[1].status, c[1].secs), (Status::Passed, Some(0.01)));
    }

    #[test]
    fn reads_pytest_verbose_short_tracebacks() {
        let d = tmp_with("py", &["tests/test_math.py", "tests/check.py"]);
        let out = "tests/test_math.py::test_add FAILED                                   [ 50%]\ntests/test_math.py::TestDiv::test_half PASSED                        [100%]\n\n=================================== FAILURES ===================================\n___________________________________ test_add ___________________________________\ntests/test_math.py:7: in test_add\n    check(total)\ntests/check.py:3: in check\n    assert total == 6\nE   assert 5 == 6\ntests/test_math.py:1: in nothing\n=========================== short test summary info ============================\nFAILED tests/test_math.py::test_add - assert 5 == 6";
        let c = feed(&mut Parser::new(Rules::builtin("python")), &d, out);
        assert_eq!(
            c.iter().map(|c| (c.name.as_str(), c.status)).collect::<Vec<_>>(),
            [("test_add", Status::Failed), ("TestDiv::test_half", Status::Passed)]
        );
        assert_eq!(c[0].message, ["assert 5 == 6"]);
        assert_eq!(
            c[0].at,
            Some((d.join("tests/check.py"), 2)),
            "the traceback's last frame before the E lines"
        );
    }

    /// The three runners' outputs, 10k lines each or so — passes mostly, a failure in every two hundred
    /// with its section, message and place (`src/m.rs`, `t_test.go`, `tests/test_m.py`).
    fn outputs() -> [(&'static str, String); 3] {
        let (mut rust, mut go, mut py) = (String::new(), String::new(), String::new());
        let mut failures = (String::new(), String::new());
        // (go's two lines a test: every other one)
        for i in 0..10_000 {
            let n = i % 40 + 1;
            if i % 200 == 0 {
                rust += &format!("test m::tests::t{i} ... FAILED\n");
                failures.0 += &format!(
                    "---- m::tests::t{i} stdout ----\nthread 'm::tests::t{i}' panicked at src/m.rs:{n}:9:\n\
                     assertion `left == right` failed\n  left: 1\n right: 2\n"
                );
                go += &format!(
                    "=== RUN   TestT{i}\n    t_test.go:{n}: got 1, want 2\n--- FAIL: TestT{i} (0.00s)\n"
                );
                py += &format!("tests/test_m.py::test_t{i} FAILED                         [ 50%]\n");
                failures.1 += &format!(
                    "_____________ test_t{i} _____________\ntests/test_m.py:{n}: in test_t{i}\n    assert x == 2\n\
                     E   assert 1 == 2\n"
                );
                continue;
            }
            rust += &format!("test m::tests::t{i} ... ok\n");
            py += &format!("tests/test_m.py::TestC::test_t{i} PASSED                  [ 50%]\n");
            if i % 2 == 0 {
                go += &format!("=== RUN   TestT{i}\n--- PASS: TestT{i} (0.01s)\n");
            }
        }
        rust +=
            &format!("\nfailures:\n\n{}\nfailures:\n    m::tests::t0\n\ntest result: FAILED.", failures.0);
        py += &format!(
            "\n=================== FAILURES ===================\n{}=========== short test summary info ===========\n",
            failures.1
        );
        [("rust", rust), ("go", go), ("python", py)]
    }

    /// Reading a run's output — on the main loop, a line at a time as it comes: 30k lines of libtest, `go test
    /// -v` and pytest output, the best of 5. A number for `cargo perf`.
    #[test]
    fn perf_test_output_lines() {
        if crate::test_results::unoptimized("test_results::tests::perf_test_output_lines") {
            return;
        }
        let d = tmp_with("perf", &["src/m.rs", "t_test.go", "tests/test_m.py"]);
        let outs = outputs();
        let lines: usize = outs.iter().map(|(_, o)| o.lines().count()).sum();
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            for (lang, out) in &outs {
                let c = feed(&mut Parser::new(Rules::builtin(lang)), &d, out);
                assert!(c.len() >= 5_000 && c.iter().any(|c| c.at.is_some()), "{lang}: {}", c.len());
            }
            best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
        }
        let line = format!("PERF test_output_30k_lines_ms {best:.3} budget=60 noise=1\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(lines >= 30_000 && best < 60.0, "{best:.1} ms for {lines} lines (budget 60)");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_junit_xml_reports() {
        let d = tmp_with("junit", &["src/test/java/demo/AppTest.java"]);
        let reports = d.join("build/test-results/test");
        std::fs::create_dir_all(&reports).unwrap();
        let since = SystemTime::now() - std::time::Duration::from_secs(5);
        std::fs::write(
            reports.join("TEST-demo.AppTest.xml"),
            r#"<?xml version="1.0"?>
<testsuite name="demo.AppTest" tests="3">
  <testcase name="greets()" classname="demo.AppTest" time="0.012"/>
  <testcase name="fails()" classname="demo.AppTest" time="0.003">
    <failure message="org.opentest4j.AssertionFailedError: expected: &lt;1&gt; but was: &lt;2&gt;" type="org.opentest4j.AssertionFailedError">org.opentest4j.AssertionFailedError: expected: &lt;1&gt; but was: &lt;2&gt;
	at org.junit.jupiter.api.AssertionUtils.fail(AssertionUtils.java:151)
	at demo.AppTest.fails(AppTest.java:17)
</failure>
  </testcase>
  <testcase name="empty()" classname="demo.AppTest$Edge" time="0"><skipped/></testcase>
</testsuite>"#,
        )
        .unwrap();
        let c = junit(&[reports], since, &[d.join("src/test/java")]);
        let st: Vec<_> = c.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            st,
            [
                ("AppTest.greets", Status::Passed),
                ("AppTest.fails", Status::Failed),
                ("AppTest.Edge.empty", Status::Skipped)
            ]
        );
        assert_eq!(c[1].message, ["expected: <1> but was: <2>"]);
        assert_eq!(c[1].at, Some((d.join("src/test/java/demo/AppTest.java"), 16)));
        assert_eq!(c[0].secs, Some(0.012));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_junit_xml_from_vitest_and_pytest() {
        let d = tmp_with("junit-other", &["src/math.test.ts", "tests/test_x.py", "sum.test.ts"]);
        let reports = d.join("reports");
        std::fs::create_dir_all(&reports).unwrap();
        let since = SystemTime::now() - std::time::Duration::from_secs(5);
        std::fs::write(
            reports.join("junit.xml"),
            r#"<testsuites><testsuite name="src/math.test.ts">
  <testcase classname="src/math.test.ts" name="math &gt; adds" time="0.001"></testcase>
  <testcase classname="src/math.test.ts" name="math &gt; edge &gt; fails" time="0.002">
    <failure message="expected 3 to be 4" type="AssertionError">AssertionError: expected 3 to be 4</failure>
  </testcase>
  <testcase classname="sum.test.ts" name="sums" time="0.001"></testcase>
</testsuite></testsuites>"#,
        )
        .unwrap();
        std::fs::write(
            reports.join("pytest.xml"),
            r#"<testsuites><testsuite name="pytest">
  <testcase classname="tests.test_x.TestA" name="test_b" time="0.01">
    <failure message="assert 1 == 2">def test_b(self):
&gt;       assert 1 == 2
E       assert 1 == 2

tests/test_x.py:7: AssertionError</failure>
  </testcase>
</testsuite></testsuites>"#,
        )
        .unwrap();
        let c = junit(&[reports], since, std::slice::from_ref(&d));
        let st: Vec<_> = c.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            st,
            [
                ("math > adds", Status::Passed),
                ("math > edge > fails", Status::Failed),
                ("sums", Status::Passed),
                ("TestA.test_b", Status::Failed)
            ],
            "a file for a classname: the name as it is"
        );
        assert_eq!(c[1].at, None, "no line in this one");
        let at = c[3].at.clone().map(|(p, l)| (std::fs::canonicalize(p).unwrap(), l));
        assert_eq!(at, Some((std::fs::canonicalize(d.join("tests/test_x.py")).unwrap(), 6)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
