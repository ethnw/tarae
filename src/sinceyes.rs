//! "Since your last y" (2026-10-05): Claude asking again to run a command outside the sandbox that the user
//! said yes to in this conversation — the same command, from the same folder — gets a card that says so and
//! shows only what changed since that yes in what the command runs: for a command that runs the project's
//! code (`cargo test … <filter>`), the project's changed hunks; for a script (`sh x.sh`, `python3 x.py`), the
//! script's own diff. Nothing changed: `Unchanged since your y at 01:12`. It never answers for the user —
//! the card still waits for `y`, as it would; it makes the answer quick (one session asked to run the same
//! test outside the sandbox ten times in an hour, a small edit between each, and each time the whole test was
//! to be read again). Never the content of a file outside the project: a script there is named, not shown.
//!
//! At each `y` (`tools::run_answer`) a snapshot is kept with the conversation (`Chat.outside_yes`, gone with
//! it): for scripts, their text; for the rest, the project's working tree as a git tree — `git add -A` into a
//! temporary copy of the index (only what changed since the index is hashed; the real index isn't touched,
//! no hook runs), then `write-tree`. At the next ask the tree is taken again and `git diff <then> <now> -- .`
//! from the project's root is what changed. Both on workers, before the run starts and before the card.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A yes to a command outside the sandbox: what it was, where it ran, when, and what it ran as it was then.
#[derive(Clone, Debug)]
pub struct Yes {
    pub cmd: String,
    pub cwd: PathBuf,
    pub at: SystemTime,
    pub kept: Kept,
}

#[derive(Clone, Debug)]
pub enum Kept {
    /// The project's tree at the yes (a git tree's id).
    Tree(String),
    /// The scripts it runs as they were: real path, text (None: not text, or not there).
    Scripts(Vec<(PathBuf, Option<String>)>),
    /// Not kept, and why (not a git checkout, git failed) — the next card says it can't tell.
    Not(String),
}

/// What the next card says of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Since {
    /// When the yes was (`01:12`).
    pub at: String,
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

/// A temporary index's name is unique per process with this.
static INDEX_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What `cmd` (run in `cwd`, in the project at `root`) runs, as it is now — on a worker.
pub fn take(root: &Path, cwd: &Path, cmd: &str) -> Kept {
    let scripts = crate::tools::scripts_run(cmd);
    if !scripts.is_empty() {
        let read = |s: &PathBuf| {
            let path = std::fs::canonicalize(cwd.join(s)).unwrap_or_else(|_| cwd.join(s));
            let text = std::fs::read(&path).ok().and_then(|b| String::from_utf8(b).ok());
            (path, text)
        };
        return Kept::Scripts(scripts.iter().map(read).collect());
    }
    match tree(root) {
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
            (Kept::Tree(then), Kept::Tree(now)) => match diff(root, then, &now) {
                Ok(lines) if lines.is_empty() => Changed::Nothing,
                Ok(lines) => Changed::Lines { of: "the project".into(), lines },
                Err(e) => Changed::Unknown(e),
            },
            (Kept::Scripts(then), Kept::Scripts(now)) => scripts_changed(root, then, &now),
            // (it runs a script now and didn't then, or the other way: not the same command's files)
            _ => Changed::Unknown("it runs other files than it did".into()),
        };
        Since { at: crate::log::clock(self.at), changed }
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

/// git as tarae runs it here: no hooks, the index `index` if given.
fn git(root: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    #[cfg(test)]
    crate::gitmenu::not_in_this_repository(root, args);
    let mut cmd = std::process::Command::new("git");
    crate::dap::setsid(&mut cmd);
    cmd.arg("-C").arg(root).args(crate::gitmenu::NO_HOOKS).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0").stdin(std::process::Stdio::null());
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The project's working tree as a git tree (what `git add -A` would stage, ignored files left out) —
/// staged into a copy of the index in the temp folder, so the real one isn't touched.
fn tree(root: &Path) -> Result<String, String> {
    let dir = git(root, &["rev-parse", "--absolute-git-dir"], None)
        .map_err(|_| "not a git checkout, so what changed can't be told".to_string())?;
    let seq = INDEX_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = std::env::temp_dir().join(format!("tarae-yes-{}-{seq}.index", std::process::id()));
    // (from the checkout's own index: only what changed since it is hashed again)
    if let Err(e) = std::fs::copy(Path::new(dir.trim()).join("index"), &index)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("git's index: {e}"));
    }
    let id = git(root, &["add", "-A", "--", "."], Some(&index))
        .and_then(|_| git(root, &["write-tree"], Some(&index)));
    std::fs::remove_file(&index).ok();
    Ok(id?.trim().to_string())
}

/// What changed from tree `then` to `now` in the project (paths from its root): each file's path, then its
/// hunks. A file a card can't show as it is (control characters, bidi marks) is named, not shown.
fn diff(root: &Path, then: &str, now: &str) -> Result<Vec<String>, String> {
    let args =
        ["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--relative", "-U2", then, now, "--", "."];
    let text = git(root, &args, None)?;
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
    /// Its part of the card: its lines, and which of them are a diff's (drawn in the diff's colours).
    pub fn card_lines(&self) -> (Vec<String>, std::ops::Range<usize>) {
        match &self.changed {
            Changed::Nothing => {
                (vec![format!("Unchanged since your y at {} — the same as it ran then.", self.at)], 0..0)
            }
            Changed::Lines { of, lines } => {
                let mut v = vec![format!("Since your y at {}, changed in {of}:", self.at)];
                v.extend(lines.iter().cloned());
                let n = v.len();
                (v, 1..n)
            }
            Changed::Unknown(why) => {
                (vec![format!("You said y to it at {} — {why}; read it as new.", self.at)], 0..0)
            }
        }
    }
}

#[cfg(test)]
mod tests;
