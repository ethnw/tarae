//! `:commit <file>` from a review, simulated (frictions29 #77): a repository with changed files, a review of them
//! (its bases jobs), and at random a file shown and marked viewed, `:commit` with a message file, a file changed
//! again, the review opened again, a frame, the user taking their time — the commit's hooks look and git runs
//! jobs among them. After every step: a commit only for a `:commit` taken with every file viewed, never more
//! commits than those taken. Once it's all quiet: each one taken committed, or its failure said (a second taken
//! before the first's git ran finds nothing left: `nothing to commit`), and nothing staged but what one said stays.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use super::sessions::git;
use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 30;

const LIMIT: Duration = Duration::from_secs(60);

const FILES: [&str; 3] = ["a.txt", "b.txt", "c.txt"];

#[test]
fn commit_from_a_review() {
    super::seeds("commit_from_a_review", SEEDS, run);
}

/// What the checks keep: the `:commit`s taken, one taken with a file not viewed, the message's file.
struct Seen {
    taken: usize,
    early: Option<String>,
    msg: PathBuf,
    n: usize,
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-commit");
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    let repo = std::fs::canonicalize(dir.join("proj")).unwrap();
    for f in FILES {
        std::fs::write(repo.join(f), "one\n").unwrap();
    }
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
        &["config", "commit.gpgsign", "false"],
        &["add", "-A"],
        &["commit", "-q", "-m", "base"],
    ] {
        git(&repo, args);
    }
    for f in FILES {
        std::fs::write(repo.join(f), "one\ntwo\n").unwrap();
    }
    let msg = repo.with_file_name("message.txt");
    std::fs::write(&msg, "The change\n\nWhy.\n# left out\n").unwrap();
    let mut sim = Sim::new("commit_from_a_review", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = repo.clone();
    crate::codereview::start_in(&mut ed, repo.clone(), None);
    let seen = RefCell::new(Seen { taken: 0, early: None, msg, n: 0 });
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &repo, &seen),
        |ed| held(ed, &repo, &seen.borrow()),
        |ed| print(ed, &repo),
    );
    let trace = sim.trace().to_vec();
    // (quiet now: every `:commit` taken has run)
    let failed = ed.messages.iter().filter(|m| m.2.starts_with("commit: ")).count();
    let commits: usize = out(&repo, &["rev-list", "--count", "HEAD"]).parse().unwrap_or(0);
    let taken = seen.borrow().taken;
    assert_eq!(commits, 1 + taken - failed, "{taken} taken, {failed} said failed:\n{}", trace.join("\n"));
    let staged = !out(&repo, &["diff", "--cached", "--name-only"]).is_empty();
    let said = ed.messages.iter().any(|m| m.2.contains("stays staged"));
    assert!(!staged || said, "staged, unsaid:\n{}", trace.join("\n"));
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, repo: &std::path::Path, seen: &RefCell<Seen>) -> String {
    let c = &mut sim.choices;
    // (by arm: a file shown and marked 4, `:commit` 3, a file changed again 2, the review again 1, a frame 1, time 1)
    match c.weighted(&[4, 3, 2, 1, 1, 1]) {
        0 => {
            let Some(n) = ed.session.code_review.as_ref().map(|r| r.files.len()).filter(|&n| n > 0) else {
                return "(no review)".into();
            };
            let i = *c.pick(&(0..n).collect::<Vec<_>>());
            crate::codereview::show(ed, i, false);
            crate::codereview::mark_viewed(ed);
            format!("file {i} shown, marked")
        }
        1 => {
            let all = ed
                .session
                .code_review
                .as_ref()
                .map(|r| (r.files.iter().all(|f| r.viewed(ed, f)), r.files.len()));
            let cmd = format!("commit {}", seen.borrow().msg.display());
            match crate::typed::execute(ed, &cmd) {
                Ok(()) => {
                    let mut s = seen.borrow_mut();
                    s.taken += 1;
                    if !all.is_some_and(|(all, _)| all) {
                        s.early = Some(format!("taken with {all:?}"));
                    }
                    ":commit taken".into()
                }
                Err(e) => format!(":commit refused — {e}"),
            }
        }
        2 => {
            let f = *c.pick(&FILES);
            let mut s = seen.borrow_mut();
            s.n += 1;
            std::fs::write(repo.join(f), format!("one\nfix {}\n", s.n)).unwrap();
            if let Some(d) = ed.docs.iter_mut().find(|d| d.path.as_deref() == Some(&repo.join(f))) {
                crate::disk::reload_text(
                    d,
                    &ropey::Rope::from_str(&std::fs::read_to_string(repo.join(f)).unwrap()),
                );
            }
            format!("{f} changed again")
        }
        3 => {
            crate::codereview::close(ed);
            crate::codereview::start_in(ed, repo.to_path_buf(), None);
            "the review again".into()
        }
        4 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        5 => {
            let ms = *c.pick(&[50, 300, 2000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        _ => unreachable!("one arm a weight"),
    }
}

fn out(repo: &std::path::Path, args: &[&str]) -> String {
    let mut o = std::process::Command::new("git");
    o.args(args).current_dir(repo).env("GIT_CONFIG_GLOBAL", "/dev/null");
    String::from_utf8_lossy(&o.output().unwrap().stdout).trim().to_string()
}

/// No `:commit` taken with a file not viewed; never more commits than those taken.
fn held(_: &Editor, repo: &std::path::Path, seen: &Seen) -> Result<(), String> {
    if let Some(e) = &seen.early {
        return Err(format!(":commit {e} — a file not viewed"));
    }
    let commits: usize = out(repo, &["rev-list", "--count", "HEAD"]).parse().unwrap_or(0);
    if commits > 1 + seen.taken {
        return Err(format!("{commits} commits for {} taken", seen.taken));
    }
    Ok(())
}

fn print(ed: &Editor, repo: &std::path::Path) -> String {
    let viewed = ed.session.code_review.as_ref().map_or("no review".to_string(), |r| {
        format!("{} of {} viewed", r.files.iter().filter(|f| r.viewed(ed, f)).count(), r.files.len())
    });
    format!("{viewed} · {} commits", out(repo, &["rev-list", "--count", "HEAD"]))
}
