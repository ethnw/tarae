//! The user's own patch landing, simulated (sessapply): a project of the scenario's own, a patch made in it by
//! git and kept outside it, and at random `:session-apply` asked (worked out on a worker), `y`, `n` or `r` on the
//! card in front after a frame, another program writing a file the patch changes, a Claude turn ending
//! (`refactor::turn_over` — Claude's change sets go, the user's stays), the project put back, a frame, the user
//! taking their time. After every step: the user's landing asks exactly while its card is up or parked, and every
//! file is as it was or as the patch has it — never half, never over another program's write (a file changed
//! since the card came is left alone, said; the rest are written, as every change set's are).

use std::path::Path;
use std::time::Duration;

use super::sessions::git;
use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::offer::{Offer, What};
use crate::refactor::Whose;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 40;

const LIMIT: Duration = crate::test_results::PATIENCE;

/// The project as it starts: a changed file, a deleted one (the patch's are a move or delete — its card has
/// no `r`), the patch's new one not yet there.
const A: &str = "1\n2\n3\n4\n5\n";
const A_PATCHED: &str = "1\nTWO\n3\n4\n5\n";
const GONE: &str = "bye\n";
const MADE: &str = "new\n";

/// Another program's line, written at the end of `a.txt`.
const OUTSIDE: &str = "outside\n";

#[test]
fn the_users_patch_never_writes_half_a_file_or_over_a_change() {
    super::seeds("the_users_patch_never_writes_half_a_file_or_over_a_change", SEEDS, run);
}

/// How many lines another program wrote to `a.txt` since it was put back — kept beside the project.
fn outside(repo: &Path) -> std::path::PathBuf {
    repo.parent().unwrap().join("outside-lines")
}

fn put_back(repo: &Path) {
    std::fs::write(outside(repo), "0").unwrap();
    std::fs::write(repo.join("a.txt"), A).unwrap();
    std::fs::write(repo.join("gone.txt"), GONE).unwrap();
    let _ = std::fs::remove_file(repo.join("new.txt"));
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-applies");
    let repo = dir.join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    put_back(&repo);
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
    // The patch, made by git in the project and kept beside it (the other tree's), the project put back
    std::fs::write(repo.join("a.txt"), A_PATCHED).unwrap();
    std::fs::write(repo.join("new.txt"), MADE).unwrap();
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    git(&repo, &["add", "-A"]);
    let out = std::process::Command::new("git")
        .args(["-C", &repo.display().to_string(), "diff", "--cached", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    git(&repo, &["reset", "-q", "--hard", "HEAD"]);
    let patch = dir.join("x.patch");
    std::fs::write(&patch, &out.stdout).unwrap();
    let mut sim = Sim::new("the_users_patch_never_writes_half_a_file_or_over_a_change", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = repo.clone();
    let d = [dir.path().display().to_string(), crate::allow::tilde(dir.path())];
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &repo, &patch),
        |ed| check(ed, &repo),
        |ed| print(ed, &d),
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, repo: &Path, patch: &Path) -> String {
    let c = &mut sim.choices;
    // (by arm: asked 3, a key on the card 4, another program's write 1, a turn's end 1, put back 1, a frame 1,
    // time passing 2)
    match c.weighted(&[3, 4, 1, 1, 1, 1, 2]) {
        0 => match crate::typed::execute(ed, &format!("session-apply {}", patch.display())) {
            Ok(()) => ":session-apply".into(),
            Err(e) => format!(":session-apply: {e}"),
        },
        // (most after the card has settled — a key as it appears is swallowed)
        1 => {
            let (key, settled) = (*c.pick(&['y', 'n', 'r']), c.below(4) > 0);
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            if settled {
                sim.wait(Duration::from_millis(800));
                crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            }
            ed.handle_key(Key::plain(Code::Char(key)));
            format!("{key} on the card{}", if settled { ", settled" } else { "" })
        }
        2 => {
            let a = repo.join("a.txt");
            let now = std::fs::read_to_string(&a).unwrap_or_default();
            std::fs::write(&a, now + OUTSIDE).unwrap();
            let n: usize = std::fs::read_to_string(outside(repo)).unwrap().parse().unwrap();
            std::fs::write(outside(repo), (n + 1).to_string()).unwrap();
            "another program writes a.txt".into()
        }
        3 => {
            crate::refactor::turn_over(ed);
            "a Claude turn ends".into()
        }
        4 => {
            put_back(repo);
            "the project put back".into()
        }
        5 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        _ => {
            let ms = *c.pick(&[50, 600]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
    }
}

fn yours(o: &Offer) -> bool {
    matches!(o.what, What::ChangeSet { whose: Whose::Yours, .. })
}

/// The landing asks exactly while its card is up or parked (one card); each file as it was or as the patch has
/// it, every line another program wrote still there.
fn check(ed: &Editor, repo: &Path) -> Result<(), String> {
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    let cards = ed.offers.iter().chain(parked).filter(|o| yours(o)).count();
    let asking = ed.session.applying.as_ref().is_some_and(|l| l.asking());
    if cards != usize::from(asking) {
        return Err(format!("{cards} change set cards of the user's, its landing asking: {asking}"));
    }
    let read = |p: &str| std::fs::read_to_string(repo.join(p)).ok();
    let a = read("a.txt").unwrap_or_default();
    let wrote: usize = std::fs::read_to_string(outside(repo)).unwrap().parse().unwrap();
    if a.matches(OUTSIDE).count() != wrote {
        return Err(format!("a.txt lost another program's lines ({wrote} written): {a:?}"));
    }
    let a = a.replace(OUTSIDE, "");
    if a != A && a != A_PATCHED {
        return Err(format!("a.txt is half: {a:?}"));
    }
    if read("new.txt").is_some_and(|t| t != MADE) || read("gone.txt").is_some_and(|t| t != GONE) {
        return Err(format!("new.txt {:?} · gone.txt {:?}", read("new.txt"), read("gone.txt")));
    }
    Ok(())
}

/// What a trace line ends with: the cards up, the landing's stage, the last thing said.
fn print(ed: &Editor, dir: &[String; 2]) -> String {
    let up: Vec<String> = ed.offers.iter().map(|o| format!("#{} {}", o.id, o.title)).collect();
    let landing = match &ed.session.applying {
        None => "none",
        Some(l) if l.asking() => "asking",
        Some(_) => "going",
    };
    let last = ed.messages.last().map(|m| m.2.replace(&dir[0], "<dir>").replace(&dir[1], "<dir>"));
    let last = last.unwrap_or_default();
    format!("up [{}] · landing {landing} · said {last:?}", up.join(", "))
}
