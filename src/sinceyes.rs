//! "Since your last y" (2026-10-05): Claude asking again to run a command outside the sandbox that the user
//! said yes to in this conversation — the same command, from the same folder — gets a card that says so and
//! shows only what changed since that yes in what the command runs: for a command that runs the project's
//! code (`cargo test … <filter>`), the project's changed hunks; for a script (`sh x.sh`, `python3 x.py`), the
//! script's own diff. Nothing changed: `Unchanged since your y at 01:12`. It never answers for the user —
//! the card still waits for `y`, as it would; it makes the answer quick (one session asked to run the same
//! test outside the sandbox ten times in an hour, a small edit between each, and each time the whole test was
//! to be read again). Never the content of a file outside the project: a script there is named, not shown.
//!
//! "The same command" is what runs (`key`, from a day of answering these, #158): the program and its
//! arguments before the first pipe — what's after it only filters what's shown — and for a script, the
//! script, whatever its arguments. A script that runs the project's code (`cargo test` inside it, a binary
//! built here) isn't all that runs: the card shows the project's changes too.
//!
//! At each `y` (`tools::run_answer`) a snapshot is kept with the conversation (`Chat.outside_yes`, gone with
//! it): for scripts, their text (and the project's tree, as above); for the rest, the project's working tree
//! as a git tree (`snapshot::tree`: the real index isn't touched, no hook runs). At the next ask the tree is
//! taken again and `git diff <then> <now> -- .` from the project's root is what changed. Both on workers,
//! before the run starts and before the card.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A yes to a command outside the sandbox: what it was, what made it the same run (`key`), where it ran,
/// when, and what it ran as it was then.
#[derive(Clone, Debug)]
pub struct Yes {
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
}

/// What the next card says of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Since {
    /// When the yes was (`01:12`).
    pub at: String,
    /// The line said yes to, when it isn't this one (`key` made them one run).
    pub was: Option<String>,
    pub changed: Changed,
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
    let scripts = crate::tools::scripts_run(cmd);
    if !scripts.is_empty() {
        let read = |s: &PathBuf| {
            let path = std::fs::canonicalize(cwd.join(s)).unwrap_or_else(|_| cwd.join(s));
            let text = std::fs::read(&path).ok().and_then(|b| String::from_utf8(b).ok());
            (path, text)
        };
        let scripts: Vec<(PathBuf, Option<String>)> = scripts.iter().map(read).collect();
        let project = runs_project(root, &scripts).then(|| crate::snapshot::tree(root));
        return Kept::Scripts { scripts, project };
    }
    match crate::snapshot::tree(root) {
        Ok(id) => Kept::Tree(id),
        Err(e) => Kept::Not(e),
    }
}

impl Yes {
    /// What changed since this yes in what `cmd` runs — on a worker.
    pub fn since(&self, root: &Path, cwd: &Path, cmd: &str) -> Since {
        let changed = match (&self.kept, take(root, cwd, cmd)) {
            (Kept::Not(why), _) => Changed::Unknown(format!("what it ran then wasn't kept ({why})")),
            (_, Kept::Not(why)) => Changed::Unknown(why),
            (Kept::Tree(then), Kept::Tree(now)) if *then == now => Changed::Nothing,
            (Kept::Tree(then), Kept::Tree(now)) => match diff(root, then, &now, &[]) {
                Ok(lines) if lines.is_empty() => Changed::Nothing,
                Ok(lines) => Changed::Lines { of: "the project".into(), lines },
                Err(e) => Changed::Unknown(e),
            },
            (Kept::Scripts { scripts: then, project: kept }, Kept::Scripts { scripts: now, project }) => {
                let own = scripts_changed(root, then, &now);
                match project {
                    Some(project) => with_project(root, own, &now, kept.as_ref(), project),
                    None => own,
                }
            }
            // (it runs a script now and didn't then, or the other way: not the same command's files)
            _ => Changed::Unknown("it runs other files than it did".into()),
        };
        let was = (self.cmd.trim() != cmd.trim()).then(|| self.cmd.clone());
        Since { at: crate::log::clock(self.at), was, changed }
    }
}

/// Programs that build or test what's in the folder they run in, no pack naming them — under the packs'
/// `[claude] builders` (cargo, go, make…), which only add: a script calling one runs the project's code.
const BUILDERS_FLOOR: [&str; 3] = ["ninja", "bazel", "xcodebuild"];

/// Whether `name` is a program that builds or tests (`BUILDERS_FLOOR`, the packs' `builders`).
fn builder(name: &str) -> bool {
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
        (Some(Ok(then)), Ok(now)) => diff(root, then, &now, &exclude),
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

/// What changed from tree `then` to `now` in the project (paths from its root), but the pathspecs `exclude`:
/// each file's path, then its hunks. A file a card can't show as it is (control characters, bidi marks) is
/// named, not shown.
fn diff(root: &Path, then: &str, now: &str, exclude: &[String]) -> Result<Vec<String>, String> {
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--relative",
        "-U2",
        then,
        now,
        "--",
        ".",
    ];
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
    /// ` to `sh x.sh one`` — the line said yes to, when it isn't this one.
    fn to(&self) -> String {
        self.was.as_ref().map(|w| format!(" to `{w}`")).unwrap_or_default()
    }

    /// Its part of the card: its lines, and which of them are a diff's (drawn in the diff's colours).
    pub fn card_lines(&self) -> (Vec<String>, std::ops::Range<usize>) {
        match &self.changed {
            Changed::Nothing => {
                let same = match self.was {
                    None => "the same as it ran then",
                    Some(_) => "what it runs is as it was then",
                };
                (vec![format!("Unchanged since your y at {}{} — {same}.", self.at, self.to())], 0..0)
            }
            Changed::Lines { of, lines } => {
                let mut v = vec![format!("Since your y at {}{}, changed in {of}:", self.at, self.to())];
                v.extend(lines.iter().cloned());
                let n = v.len();
                (v, 1..n)
            }
            Changed::Unknown(why) => {
                (vec![format!("You said y to it at {}{} — {why}; read it as new.", self.at, self.to())], 0..0)
            }
        }
    }
}

#[cfg(test)]
mod tests;
