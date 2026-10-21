//! "Same as" (2026-10-05 for runs outside the sandbox, cardburden 3 for every run card): Claude asking again
//! to run a command whose card the user answered since tarae started — the same run, from the same folder —
//! gets a card whose top says so and, a row a part, what changed since in what it runs:
//!
//! ```text
//! Same as card#41 — y at 18:02
//!   xtask/ unchanged
//!   build files unchanged
//!   3 other project files changed since: src/sandbox/keeper.rs, src/sandbox/tier.rs, src/sandbox/redirect.rs
//! ```
//!
//! Under the command, how what it runs changed: a script's own diff, the files a pack says the command runs
//! (`[claude] entry`: `cargo xtask` runs `xtask/`), the build files (the protected names — what runs as it
//! builds); past the sandbox — outside it, or with a socket (Docker's is root's) — the project's whole diff,
//! as before. Kept until tarae quits, not with the conversation. It never answers for
//! the user — the card still waits for its key, as it would; it makes the answer quick (in step 6c 14 of 27
//! cards repeated an earlier line word for word, each 22–35 s, "has anything it runs changed?" checked by
//! hand). Never the content of a file outside the project: a script there is named, not shown.
//!
//! "The same command" is what runs (`key`, from a day of answering these, #158): the program and its
//! arguments before the first pipe — what's after it only filters what's shown — and for a script, the
//! script, whatever its arguments. A script that runs the project's code (`cargo test` inside it, a binary
//! built here) isn't all that runs: the card counts the project's changes too.
//!
//! At each answer (`tools::run_answer`) what it runs is kept in the session (`Session.answered`, until you
//! quit): for scripts, their text (and the project's tree, as above); for the rest, the project's working tree
//! as a git tree (`snapshot::tree`: the real index isn't touched, no hook runs) — taken before it runs. At the
//! next ask the tree is taken again and compared from the project's root. Both on workers, before the run
//! starts and before the card.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// An answer on a run card of Claude's: the card, the answer (`y` `a` `A` `n`), the line, what made it the
/// same run (`key`), where it ran, when, and what it ran as it was then.
#[derive(Clone, Debug)]
pub struct Answered {
    pub card: u64,
    pub answer: char,
    /// What it was asked past the sandbox with: outside it whole, one socket — only a card asking the same is
    /// the same (a `y` in the sandbox isn't one outside it).
    pub beyond: (bool, Option<PathBuf>),
    pub cmd: String,
    pub key: String,
    pub cwd: PathBuf,
    pub at: SystemTime,
    pub kept: Kept,
}

#[derive(Clone, Debug)]
pub enum Kept {
    /// The project's tree at the yes (a git tree's id).
    Tree(String),
    /// The scripts it runs as they were: real path, text (None: not text, or not there) — and, when one of
    /// them runs the project's code (`runs_project`), the project's tree (Err: why it couldn't be taken).
    Scripts { scripts: Vec<(PathBuf, Option<String>)>, project: Option<Result<String, String>> },
    /// Not kept, and why (not a git checkout, git failed) — the next card says it can't tell.
    Not(String),
    /// Nothing: it only reads (`readonly::read_only`) — what it reads is on its card.
    Reads,
}

/// What the next card says of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Since {
    /// The card answered, and how.
    pub card: u64,
    pub answer: char,
    /// When the answer was (`01:12`).
    pub at: String,
    /// The line answered, when it isn't this one (`key` made them one run).
    pub was: Option<String>,
    /// What it runs, part by part, changed or not: `xtask/ unchanged`, `build files changed: Cargo.toml`,
    /// `3 other project files changed since: a.rs, b.rs, c.rs`.
    pub said: Vec<String>,
    /// What's shown below of how it changed.
    pub changed: Changed,
    /// Nothing changed since — what it runs, nor any other file of the project it compared.
    pub unchanged: bool,
    /// A script it runs changed since (its own text) — a card warns of it (`cardwarn`, `script-changed`).
    pub script_changed: bool,
}

/// A card's word of the one before it (`tarae state`'s `same_as`): that card, its answer and when, and whether
/// anything changed since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SameAs {
    pub card: u64,
    pub answer: char,
    pub at: String,
    pub unchanged: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Changed {
    Nothing,
    /// What changed in `of` (`the project`, `x.sh`): a diff's lines — each file's path, then its hunks
    /// (`@@` headers, ` ` `-` `+` lines).
    Lines {
        of: String,
        lines: Vec<String>,
    },
    /// It can't be told (and why) — the card is read as a first one.
    Unknown(String),
}

/// A card shows at most this many changed lines — the rest are counted (read them before you say yes).
const LINES_MAX: usize = 400;

/// What makes two asks one run: what runs — the parts before the first pipe (`cd`s and `NAME=value`s
/// included; `cargo test x | grep y` and `cargo test x | tail` are one, `cargo test z` another) — and for
/// a head that runs one script, that script whatever its arguments (`sh x.sh a` and `sh x.sh b`: its diff
/// is what changed). A line the shell reader can't read is its whole self.
pub fn key(cmd: &str) -> String {
    use crate::readonly::Part;
    let Some(head) = crate::readonly::before_pipe(cmd) else { return cmd.trim().to_string() };
    // (words apart by a character no word has: `a "b c"` isn't `a b c`)
    let shown: Vec<String> = head
        .iter()
        .map(|p| match p {
            Part::Cd(d) => format!("cd\u{1f}{}", d.text),
            Part::Cmd(w) => w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join("\u{1f}"),
        })
        .collect();
    let commands: Vec<&Vec<crate::readonly::Word>> = head
        .iter()
        .filter_map(|p| match p {
            Part::Cmd(w) => Some(w),
            Part::Cd(_) => None,
        })
        .collect();
    let line: Vec<String> = head
        .iter()
        .map(|p| match p {
            Part::Cd(d) => format!("cd '{}'", d.text),
            Part::Cmd(w) => w.iter().map(|w| format!("'{}'", w.text)).collect::<Vec<_>>().join(" "),
        })
        .collect();
    if let ([one], [script]) = (&commands[..], &crate::tools::scripts_run(&line.join(" && "))[..]) {
        let words: Vec<String> = one.iter().map(|w| w.text.clone()).collect();
        let program = match crate::readonly::env_words(&words) {
            Some((_, rest)) => rest.first().cloned(),
            None => words.first().cloned(),
        };
        return format!("script\u{1f}{}\u{1f}{}", program.unwrap_or_default(), script.display());
    }
    shown.join("\u{1e}")
}

/// What `cmd` (run in `cwd`, in the project at `root`) runs, as it is now — on a worker.
pub fn take(root: &Path, cwd: &Path, cmd: &str) -> Kept {
    take_with(root, cwd, cmd, || crate::snapshot::tree(root))
}

/// `take`, the project's tree from `tree` when one's needed (a run's own snapshot, taken right before it
/// starts — `tools::go`).
pub fn take_with(root: &Path, cwd: &Path, cmd: &str, tree: impl FnOnce() -> Result<String, String>) -> Kept {
    let scripts = crate::tools::scripts_run(cmd);
    if !scripts.is_empty() {
        let read = |s: &PathBuf| {
            let path = std::fs::canonicalize(cwd.join(s)).unwrap_or_else(|_| cwd.join(s));
            let text = std::fs::read(&path).ok().and_then(|b| String::from_utf8(b).ok());
            (path, text)
        };
        let scripts: Vec<(PathBuf, Option<String>)> = scripts.iter().map(read).collect();
        let project = runs_project(root, &scripts).then(tree);
        return Kept::Scripts { scripts, project };
    }
    match tree() {
        Ok(id) => Kept::Tree(id),
        Err(e) => Kept::Not(e),
    }
}

impl Answered {
    /// The answer `answer` on card `card` for `cmd` run in `cwd` (asked past the sandbox with `beyond`), at
    /// `at` — what it runs as `kept`.
    pub fn new(
        (card, answer): (u64, char),
        beyond: (bool, Option<PathBuf>),
        cmd: &str,
        cwd: &Path,
        at: SystemTime,
        kept: Kept,
    ) -> Answered {
        let (cmd, key, cwd) = (cmd.to_string(), key(cmd), cwd.to_path_buf());
        Answered { card, answer, beyond, cmd, key, cwd, at, kept }
    }

    /// What changed since this answer in what `cmd` runs — on a worker. `outside`: it asks past the sandbox
    /// (outside it, or with a socket — Docker's is root's), where the project's whole diff is shown; in it,
    /// the diff of what it runs (scripts, a pack's `entry` files, build files) and the rest named.
    pub fn since(&self, root: &Path, cwd: &Path, cmd: &str, outside: bool) -> Since {
        if let Some(s) = self.only_reads(cmd) {
            return s;
        }
        let entry = entry_files(cmd);
        let mut said = Vec::new();
        // (a file of the project changed, whether it's shown or named; a script it runs, its own text)
        let mut touched = false;
        let mut script_changed = false;
        let changed = match (&self.kept, take(root, cwd, cmd)) {
            (Kept::Not(why), _) => Changed::Unknown(format!("what it ran then wasn't kept ({why})")),
            (Kept::Reads, _) => Changed::Unknown("it only read then, and runs more now".into()),
            (_, Kept::Not(why)) => Changed::Unknown(why),
            (Kept::Tree(then), Kept::Tree(now)) => match files_changed(root, then, &now, &entry, &[]) {
                Err(e) => Changed::Unknown(e),
                Ok(files) => {
                    files.said(&entry, &mut said);
                    touched = !files.is_empty();
                    let shown = match outside {
                        true => (then != &now).then(|| diff(root, then, &now, &[], &[])),
                        false => files.runs(root, then, &now),
                    };
                    match shown {
                        None => Changed::Nothing,
                        Some(Ok(lines)) if lines.is_empty() => Changed::Nothing,
                        Some(Ok(lines)) => Changed::Lines { of: "the project".into(), lines },
                        Some(Err(e)) => Changed::Unknown(e),
                    }
                }
            },
            (Kept::Scripts { scripts: then, project: kept }, Kept::Scripts { scripts: now, project }) => {
                let own = scripts_changed(root, then, &now);
                script_changed = matches!(own, Changed::Lines { .. });
                let names: Vec<String> = now.iter().map(|(p, _)| crate::chat::shown(root, p)).collect();
                said.push(match &own {
                    Changed::Lines { .. } => format!("script changed: {}", listed(&names)),
                    _ => format!("script unchanged ({})", listed(&names)),
                });
                // (the project, when the script runs its code — its trees then and now)
                let trees = match (kept, &project) {
                    (Some(Ok(a)), Some(Ok(b))) => Some((a.clone(), b.clone())),
                    _ => None,
                };
                let files =
                    trees.as_ref().map(|(a, b)| files_changed(root, a, b, &entry, &left_out(root, &now)));
                match &files {
                    Some(Ok(f)) => {
                        f.said(&entry, &mut said);
                        touched = !f.is_empty();
                    }
                    Some(Err(e)) => said.push(format!("what changed in the project can't be told ({e})")),
                    None => {}
                }
                match (outside, project, trees, files) {
                    (_, None, ..) => own,
                    (false, Some(_), Some((a, b)), Some(Ok(f))) => with_runs(own, f.runs(root, &a, &b)),
                    // (outside: the project's whole diff; in, its trees not both there: why, as outside)
                    (_, Some(project), ..) => with_project(root, own, &now, kept.as_ref(), project),
                }
            }
            // (it runs a script now and didn't then, or the other way: not the same command's files)
            _ => Changed::Unknown("it runs other files than it did".into()),
        };
        Since { script_changed, ..self.said(cmd, said, changed, !touched) }
    }

    /// `since` with no git (the main thread's): the answer was to a command that only reads, and `cmd` only
    /// reads — it runs nothing of the project; None otherwise.
    pub fn only_reads(&self, cmd: &str) -> Option<Since> {
        let reads = matches!(self.kept, Kept::Reads) && crate::readonly::read_only(cmd);
        reads.then(|| self.said(cmd, vec!["it only reads".into()], Changed::Nothing, true))
    }

    fn said(&self, cmd: &str, said: Vec<String>, changed: Changed, quiet: bool) -> Since {
        let was = (self.cmd.trim() != cmd.trim()).then(|| self.cmd.clone());
        let unchanged = changed == Changed::Nothing && quiet;
        let at = crate::log::clock(self.at);
        Since {
            card: self.card,
            answer: self.answer,
            at,
            was,
            said,
            changed,
            unchanged,
            script_changed: false,
        }
    }
}

/// The files the packs say `cmd` runs that its words don't name (`[claude] entry`: a part of the line before
/// its first pipe with an entry's words in a row — after the variables it sets and a wrapper, as `timeout
/// 1800 cargo xtask linux`; read too widely, it only shows more), from the project's root.
pub fn entry_files(cmd: &str) -> Vec<String> {
    entry_files_in(cmd, &crate::sandboxpack::get().entry)
}

pub(crate) fn entry_files_in(cmd: &str, entries: &[crate::sandboxpack::Entry]) -> Vec<String> {
    let Some(head) = crate::readonly::before_pipe(cmd) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for p in head {
        let crate::readonly::Part::Cmd(w) = p else { continue };
        let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        for e in entries.iter().filter(|e| words.windows(e.run.len()).any(|r| r == e.run)) {
            for f in e.files.iter().filter(|f| !out.contains(f)).collect::<Vec<_>>() {
                out.push(f.clone());
            }
        }
    }
    out
}

/// The changed files of a project between two trees, by what they are to the command: its `entry` files,
/// build files (the protected names — `sandbox::protected_in`: what runs as it builds), the rest.
#[derive(Debug, Default)]
struct Files {
    entry: Vec<String>,
    build: Vec<String>,
    other: Vec<String>,
}

/// At most this many files are named in a part of the first line — the rest counted.
const NAMED_MAX: usize = 5;

/// `names`, at most `NAMED_MAX` of them, the rest counted.
fn listed(names: &[String]) -> String {
    match names.len() > NAMED_MAX {
        true => format!("{} and {} more", names[..NAMED_MAX].join(", "), names.len() - NAMED_MAX),
        false => names.join(", "),
    }
}

/// The project's files changed from tree `then` to `now` (from its root), but those `left_out` (pathspecs:
/// the scripts, said apart), sorted by what they are to a command running `entry`.
fn files_changed(
    root: &Path,
    then: &str,
    now: &str,
    entry: &[String],
    left_out: &[String],
) -> Result<Files, String> {
    let mut f = Files::default();
    if then == now {
        return Ok(f);
    }
    let mut args = vec!["diff", "--name-only", "-z", "--no-renames", "--relative", then, now, "--", "."];
    args.extend(left_out.iter().map(String::as_str));
    let out = crate::snapshot::git(root, &args, None)?;
    for rel in out.split('\0').filter(|r| !r.is_empty()) {
        let into = match () {
            _ if entry.iter().any(|e| in_entry(rel, e)) => &mut f.entry,
            _ if crate::sandbox::protected_in(root, &root.join(rel)) => &mut f.build,
            _ => &mut f.other,
        };
        into.push(rel.to_string());
    }
    Ok(f)
}

/// Whether `rel` is entry file `e`, or in it (a folder ends in `/`).
fn in_entry(rel: &str, e: &str) -> bool {
    match e.strip_suffix('/') {
        Some(folder) => rel.strip_prefix(folder).is_some_and(|r| r.starts_with('/')),
        None => rel == e,
    }
}

/// The scripts of `now` inside the project as pathspecs that leave them out of the project's changes.
fn left_out(root: &Path, now: &[(PathBuf, Option<String>)]) -> Vec<String> {
    now.iter()
        .filter_map(|(p, _)| p.strip_prefix(root).ok())
        .map(|p| format!(":(exclude,literal){}", p.display()))
        .collect()
}

impl Files {
    fn is_empty(&self) -> bool {
        self.entry.is_empty() && self.build.is_empty() && self.other.is_empty()
    }

    /// Its rows at the card's top: the `entry` files (when the command has some), the build files, the rest —
    /// what changed named (the diff under the command shows what it runs; outside the sandbox, all of it).
    fn said(&self, entry: &[String], said: &mut Vec<String>) {
        if !entry.is_empty() {
            said.push(match self.entry.is_empty() {
                true => format!("{} unchanged", entry.join(", ")),
                false => format!("{} changed: {}", entry.join(", "), listed(&self.entry)),
            });
        }
        said.push(match self.build.is_empty() {
            true => "build files unchanged".to_string(),
            false => format!("build files changed: {}", listed(&self.build)),
        });
        let n = self.other.len();
        said.push(match n {
            0 => "nothing else in the project changed".to_string(),
            _ => format!(
                "{n} other project file{} changed since: {}",
                if n == 1 { "" } else { "s" },
                listed(&self.other)
            ),
        });
    }

    /// The diff of what it runs — its entry files and build files — from tree `then` to `now`: None when
    /// neither changed.
    fn runs(&self, root: &Path, then: &str, now: &str) -> Option<Result<Vec<String>, String>> {
        let paths: Vec<String> =
            self.entry.iter().chain(&self.build).map(|p| format!(":(literal){p}")).collect();
        (!paths.is_empty()).then(|| diff(root, then, now, &paths, &[]))
    }
}

/// A script's own changes (`own`) and the diff of the rest of what it runs (`runs`: its entry and build files).
fn with_runs(own: Changed, runs: Option<Result<Vec<String>, String>>) -> Changed {
    match (own, runs) {
        (own, None) => own,
        (Changed::Unknown(why), _) => Changed::Unknown(why),
        (_, Some(Err(e))) => Changed::Unknown(format!("what it runs in the project can't be told ({e})")),
        (own, Some(Ok(more))) if more.is_empty() => own,
        (Changed::Nothing, Some(Ok(lines))) => Changed::Lines { of: "the project".into(), lines },
        (Changed::Lines { of, mut lines }, Some(Ok(more))) => {
            lines.extend(more);
            Changed::Lines { of: format!("{of}, and the project"), lines: capped(lines) }
        }
    }
}

/// Programs that build or test what's in the folder they run in, no pack naming them — under the packs'
/// `[claude] builders` (cargo, go, make…), which only add: a script calling one runs the project's code.
const BUILDERS_FLOOR: [&str; 3] = ["ninja", "bazel", "xcodebuild"];

/// Whether `name` is a program that builds or tests (`BUILDERS_FLOOR`, the packs' `builders`).
pub(crate) fn builder(name: &str) -> bool {
    BUILDERS_FLOOR.contains(&name) || crate::sandboxpack::get().builders.iter().any(|b| b == name)
}

/// Whether one of `scripts` runs the project at `root`'s code — so it isn't all that runs: a program in the
/// project that isn't text (a binary built here), or a script with a line that builds or tests (`builder`),
/// runs a file by a relative path (`./x`, `target/debug/t`), or hands one to an interpreter (`python3 x.py`).
fn runs_project(root: &Path, scripts: &[(PathBuf, Option<String>)]) -> bool {
    scripts.iter().any(|(path, text)| match text {
        None => path.starts_with(root) && path.is_file(),
        Some(t) => t.lines().any(runs_project_line),
    })
}

/// One line of a script: whether it runs the project's code (`runs_project`) — read by words, as a shell
/// would split it at `;` `&` `|` and brackets; a comment says nothing.
fn runs_project_line(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with('#') {
        return false;
    }
    let lead = |w: &&str| {
        (w.contains('=') && !w.starts_with('-'))
            || matches!(
                *w,
                "exec"
                    | "time"
                    | "env"
                    | "nice"
                    | "nohup"
                    | "command"
                    | "then"
                    | "do"
                    | "else"
                    | "if"
                    | "while"
                    | "until"
                    | "!"
                    | "{"
            )
    };
    line.split([';', '|', '&', '(', ')', '`']).any(|part| {
        let mut words = part.split_whitespace().map(|w| w.trim_matches(['"', '\''])).skip_while(lead);
        let Some(program) = words.next() else { return false };
        let name = program.rsplit('/').next().unwrap_or(program);
        let relative = |w: &str| !w.starts_with(['/', '-', '$', '~']) && w.contains(['/', '.']);
        builder(name)
            || (program.contains('/') && relative(program))
            || (crate::tools::runner(name)
                && !builder(name)
                && words.find(|w| !w.starts_with('-')).is_some_and(relative))
    })
}

/// A script's own changes (`own`) and, as it runs the project's code, the project's since tree `kept` (None:
/// it didn't run the project's code then) — the scripts themselves left out of the project's diff.
fn with_project(
    root: &Path,
    own: Changed,
    scripts: &[(PathBuf, Option<String>)],
    kept: Option<&Result<String, String>>,
    now: Result<String, String>,
) -> Changed {
    let inside: Vec<&PathBuf> = scripts.iter().map(|(p, _)| p).filter(|p| p.starts_with(root)).collect();
    let names: Vec<String> = scripts.iter().map(|(p, _)| crate::chat::shown(root, p)).collect();
    let names = names.join(", ");
    let exclude: Vec<String> = inside
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|p| format!(":(exclude){}", p.display()))
        .collect();
    let project = match (kept, now) {
        (Some(Ok(then)), Ok(now)) if *then == now => Ok(Vec::new()),
        (Some(Ok(then)), Ok(now)) => diff(root, then, &now, &[], &exclude),
        (Some(Err(e)), _) => Err(e.clone()),
        (_, Err(e)) => Err(e),
        (None, _) => Err("it didn't run the project's code then, so the project wasn't kept".into()),
    };
    match (own, project) {
        (Changed::Unknown(why), _) => Changed::Unknown(why),
        (Changed::Nothing, Ok(p)) if p.is_empty() => Changed::Nothing,
        (Changed::Nothing, Ok(lines)) => {
            Changed::Lines { of: format!("the project {names} runs ({names} itself unchanged)"), lines }
        }
        (Changed::Nothing, Err(e)) => Changed::Unknown(format!(
            "{names} is unchanged, but it runs the project's code, and what changed there can't be told ({e})"
        )),
        (Changed::Lines { of, mut lines }, Err(e)) => {
            lines.push(format!("and it runs the project's code: what changed there can't be told ({e})"));
            Changed::Lines { of, lines: capped(lines) }
        }
        (Changed::Lines { of, lines }, Ok(p)) if p.is_empty() => Changed::Lines { of, lines },
        (Changed::Lines { of, mut lines }, Ok(p)) => {
            lines.extend(p);
            Changed::Lines { of: format!("{of}, and the project it runs"), lines: capped(lines) }
        }
    }
}

/// The scripts' changes — inside the project as a diff, outside it named only.
fn scripts_changed(
    root: &Path,
    then: &[(PathBuf, Option<String>)],
    now: &[(PathBuf, Option<String>)],
) -> Changed {
    let mut lines = Vec::new();
    let mut names = Vec::new();
    for (path, text) in now {
        let name = crate::chat::shown(root, path);
        names.push(name.clone());
        let Some((_, before)) = then.iter().find(|(p, _)| p == path) else {
            return Changed::Unknown(format!("it runs {name} now, not what it ran then"));
        };
        if before == text {
            continue;
        }
        if !path.starts_with(root) {
            lines.push(format!("{name} — changed; outside the project, not shown here"));
            continue;
        }
        let (Some(before), Some(text)) = (before, text) else {
            lines.push(format!("{name} — {}", if text.is_none() { "gone, or not text" } else { "new" }));
            continue;
        };
        if !crate::tools::draws_as_itself(text) {
            lines.push(format!(
                "{name} has characters a card can't show as they are — read it before you say yes"
            ));
            continue;
        }
        lines.push(name);
        lines.extend(hunks(before, text));
    }
    match lines.is_empty() {
        true => Changed::Nothing,
        false => Changed::Lines { of: names.join(", "), lines: capped(lines) },
    }
}

/// `old` → `new` as hunks: `@@ line 12` and the lines gone (`-`) and come (`+`).
fn hunks(old: &str, new: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (from, _, gone, came) in crate::editdiff::line_changes(old, new) {
        let line = old[..from.min(old.len())].matches('\n').count() + 1;
        out.push(format!("@@ line {line}"));
        out.extend(gone.lines().map(|l| format!("-{l}")));
        out.extend(came.lines().map(|l| format!("+{l}")));
    }
    out
}

fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > LINES_MAX {
        let more = lines.len() - LINES_MAX;
        lines.truncate(LINES_MAX);
        lines.push(format!("… {more} more changed lines — read them before you say yes"));
    }
    lines.iter_mut().for_each(|l| *l = l.replace('\t', "    "));
    lines
}

/// What changed from tree `then` to `now` in the project (paths from its root) — in the pathspecs `paths`
/// (none: all of it) but `exclude`: each file's path, then its hunks. A file a card can't show as it is
/// (control characters, bidi marks) is named, not shown.
fn diff(
    root: &Path,
    then: &str,
    now: &str,
    paths: &[String],
    exclude: &[String],
) -> Result<Vec<String>, String> {
    let mut args =
        vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--relative", "-U2", then, now, "--"];
    match paths.is_empty() {
        true => args.push("."),
        false => args.extend(paths.iter().map(String::as_str)),
    }
    args.extend(exclude.iter().map(String::as_str));
    let text = crate::snapshot::git(root, &args, None)?;
    let mut out = Vec::new();
    let mut file: Vec<String> = Vec::new();
    let flush = |file: &mut Vec<String>, out: &mut Vec<String>| {
        // (an empty line of context at a file's end reads as a gap after it)
        while file.len() > 1 && file.last().is_some_and(|l| l.trim().is_empty()) {
            file.pop();
        }
        if file.iter().skip(1).all(|l| crate::tools::draws_as_itself(l)) {
            out.append(file);
        } else if let Some(name) = file.first() {
            out.push(format!(
                "{name} has characters a card can't show as they are — read it before you say yes"
            ));
            file.clear();
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut file, &mut out);
            // (`a/x b/x` — the name after ` b/`)
            let name = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b);
            file.push(name.to_string());
        } else if line.starts_with("new file mode") {
            if let Some(n) = file.first_mut() {
                n.push_str(" (new)");
            }
        } else if line.starts_with("deleted file mode") {
            if let Some(n) = file.first_mut() {
                n.push_str(" (deleted)");
            }
        } else if line.starts_with("Binary files") {
            file.push("binary — changed".into());
        } else if line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
            || line.starts_with('\\')
        {
            continue;
        } else {
            file.push(line.to_string());
        }
    }
    flush(&mut file, &mut out);
    Ok(capped(out))
}

impl Since {
    pub fn same_as(&self) -> SameAs {
        SameAs { card: self.card, answer: self.answer, at: self.at.clone(), unchanged: self.unchanged }
    }

    /// The top of the card: `Same as card#41 — y at 18:02` — the line answered when it isn't this one
    /// (`` (`sh x.sh one`) ``) — then a row a part (`  xtask/ unchanged`, `  build files changed:
    /// Cargo.toml`, …): read at a glance, where one line of them wrapped to six rows in an 80-column card;
    /// what changed can't be told: why, and to read it as new.
    pub fn head(&self) -> String {
        let was = self.was.as_ref().map(|w| format!(" (`{w}`)")).unwrap_or_default();
        let same = format!("Same as card#{}{was} — {} at {}", self.card, self.answer, self.at);
        match &self.changed {
            Changed::Unknown(why) => format!("{same} — {why}; read it as new."),
            _ if self.said.is_empty() => format!("{same} — what it runs is as it was then."),
            _ => std::iter::once(same)
                .chain(self.said.iter().map(|s| format!("  {s}")))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// What's shown under the command of how what it runs changed, and which of those lines are a diff's
    /// (drawn in the diff's colours) — none when nothing did.
    pub fn below(&self) -> (Vec<String>, std::ops::Range<usize>) {
        match &self.changed {
            Changed::Lines { of, lines } => {
                let mut v = vec![format!("Changed since card#{}, in {of}:", self.card)];
                v.extend(lines.iter().cloned());
                let n = v.len();
                (v, 1..n)
            }
            Changed::Nothing | Changed::Unknown(_) => (Vec::new(), 0..0),
        }
    }
}

#[cfg(test)]
mod tests;
