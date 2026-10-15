//! claude's own updates, which tarae keeps off (decision A, V1d): its updater writes a folder of programs on PATH
//! (`~/.local/bin` and `~/.local/share/claude/versions/`), closed to claude's own sandbox (`sandbox::claude`), so
//! every claude tarae starts gets `DISABLE_UPDATES` (`llm::spawn`) — and you update it yourself: `:claude-update`
//! runs `claude update` as your own task, outside the sandbox, without that variable, and says what came.
//!
//! What tarae can tell without the network is how old the claude it runs is, not whether a newer one exists
//! (only `claude update` asks that, of Anthropic's servers): claude says its version as each turn starts
//! (`init`'s `claude_code_version`), and the native installer keeps each version as one file,
//! `~/.local/share/claude/versions/<version>`, made as it was installed. Once that's `STALE`, the chat says once
//! since tarae started that its updates are off and how to look for one. Installed another way (npm, Homebrew):
//! no file, nothing said. A claude you use outside tarae still updates the install tarae starts — the next chat
//! process runs it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::editor::Editor;

/// How old a claude is before the chat says so: Claude Code ships several times a week.
pub const STALE: Duration = Duration::from_secs(14 * 24 * 3600);

/// What was looked at and said since tarae started.
#[derive(Debug, Default)]
pub struct Told {
    /// The native installer's versions folder — None: not looked at (tests but their own).
    pub versions: Option<PathBuf>,
    /// The version last looked up (once each).
    looked: Option<String>,
    /// Said (once since tarae started).
    said: bool,
    /// `:claude-update`'s run under way (`ran`).
    before: Option<Before>,
}

/// `:claude-update`'s line, what `versions` held as it started, and the run it became (`started`): only that
/// run's end says what came — not another run of the same line, nor one an update asked again since replaced.
#[derive(Clone, Debug)]
struct Before {
    cmd: String,
    listed: Vec<String>,
    run: Option<u64>,
}

impl Told {
    /// The user's: `~/.local/share/claude/versions` (the native installer's, Claude Code's setup docs).
    pub fn new() -> Told {
        let versions =
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/claude/versions"));
        Told { versions, ..Default::default() }
    }
}

/// When `version` was installed: its file's time in `versions`. None: not there, or no time.
pub fn installed_at(versions: &Path, version: &str) -> Option<SystemTime> {
    // (a version is a name of its own — never a path)
    if version.is_empty() || version.contains(['/', '\\']) || version.starts_with('.') {
        return None;
    }
    let m = std::fs::metadata(versions.join(version)).ok()?;
    m.modified().ok()
}

/// What the chat says of claude `version` installed `at`, by `now` — None while it's newer than `STALE`.
pub fn said(version: &str, at: SystemTime, now: SystemTime) -> Option<String> {
    let age = now.duration_since(at).ok().filter(|a| *a >= STALE)?;
    let days = age.as_secs() / (24 * 3600);
    Some(format!(
        "claude {version} was installed {days} days ago, and tarae keeps it from updating itself — :claude-update \
         looks for a newer one (`claude update`, run as you, outside the sandbox)"
    ))
}

/// claude said its `version` (a turn's start): looked up on a worker, once each; said in the chat when stale.
pub fn seen(ed: &mut Editor, version: &str) {
    let t = &mut ed.claude_update;
    if t.said || t.looked.as_deref() == Some(version) {
        return;
    }
    t.looked = Some(version.to_string());
    let Some(versions) = t.versions.clone() else { return };
    let version = version.to_string();
    ed.events.jobs().spawn(move || {
        let note = installed_at(&versions, &version).and_then(|at| said(&version, at, SystemTime::now()));
        move |ed: &mut Editor| {
            let Some(note) = note else { return ed.unchanged = true };
            if ed.claude_update.said {
                return;
            }
            ed.claude_update.said = true;
            if let Some(c) = ed.session.chat.as_mut() {
                c.note(&note);
            }
        }
    });
}

/// The variables that keep claude from updating itself (`llm::spawn` gives every claude tarae starts one): left
/// out of `:claude-update`'s run — a tarae started with one (from a shell that has it, under a Claude Code that
/// sets it) passed it on to the task, and `claude update` did nothing.
pub const UPDATES_OFF: [&str; 2] = ["DISABLE_UPDATES", "DISABLE_AUTOUPDATER"];

/// `:claude-update` — `claude update` (`llm.command`'s) as your own task, outside the sandbox, without
/// `UPDATES_OFF`: it asks Anthropic's servers for a newer claude and installs it; the next chat process runs it.
/// What the versions folder holds is looked at first, on a worker — its end says what came (`ran`).
pub fn update(ed: &mut Editor) -> Result<(), String> {
    let command = ed.config.llm.command.clone();
    let unset: String = UPDATES_OFF.iter().map(|v| format!("-u {v} ")).collect();
    let cmd = format!("env {unset}{} update", crate::packs::sh_quote(&command));
    let task = crate::task::Task { name: "claude update".into(), cmd, cwd: PathBuf::new() };
    let Some(versions) = ed.claude_update.versions.clone() else {
        ed.task_start(task);
        return Ok(());
    };
    ed.events.jobs().spawn(move || {
        let before = listed(&versions);
        move |ed: &mut Editor| {
            ed.claude_update.before = Some(Before { cmd: task.cmd.clone(), listed: before, run: None });
            ed.task_start(task);
        }
    });
    Ok(())
}

/// The names in `dir`, sorted (none: not there).
fn listed(dir: &Path) -> Vec<String> {
    let names = std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut v: Vec<String> = names.map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    crate::sorting::sort(&mut v);
    v
}

/// The user's run `generation` of `cmd` started (`task_spawn`): the first of `:claude-update`'s line since its
/// versions were listed is its run.
pub fn started(ed: &mut Editor, cmd: &str, generation: u64) {
    if let Some(b) = ed.claude_update.before.as_mut().filter(|b| b.run.is_none() && b.cmd == cmd) {
        b.run = Some(generation);
    }
}

/// A task ended (`task_spawn`): `:claude-update`'s run (`started`), passed → what came in the versions folder
/// since it started is said, looked at on a worker — the installer says "Installation complete" even when it
/// laid out nothing (f22 k), so that's a warning. True: said here; a failed run is said as any task's.
pub fn ran(ed: &mut Editor, passed: bool) -> bool {
    let Some(r) = ed.session.task_run.as_ref() else { return false };
    if ed.claude_update.before.as_ref().is_none_or(|b| b.run != Some(r.generation())) {
        return false;
    }
    let complete = r.output.iter().any(|(_, l)| l.to_lowercase().contains("installation complete"));
    let Some(before) = ed.claude_update.before.take().map(|b| b.listed) else { return false };
    let Some(versions) = ed.claude_update.versions.clone().filter(|_| passed) else { return false };
    ed.events.jobs().spawn(move || {
        let gained: Vec<String> = listed(&versions).into_iter().filter(|v| !before.contains(v)).collect();
        move |ed: &mut Editor| match (&gained[..], complete) {
            ([], true) => ed.set_warning(format!(
                "claude update said its installation is complete, but nothing new came in {} — the claude tarae \
                 starts is the one it was (the panel has its words)",
                versions.display()
            )),
            ([], false) => ed.set_success("claude update — no newer claude installed (the panel says why)"),
            (v, _) => {
                ed.set_success(format!("claude update — {} installed: the next chat process runs it", v.join(", ")))
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A claude the native installer put there over `STALE` ago is said, with how to look for a newer one; a
    /// fresh one, or one installed another way (no file), isn't; a version that isn't a plain name is never a
    /// path looked at.
    #[test]
    fn an_old_claude_is_said_with_how_to_update_it() {
        let dir = crate::scratch::Scratch::new("claudeupdate");
        std::fs::write(dir.join("2.1.296"), "").unwrap();
        let at = installed_at(&dir, "2.1.296").expect("its file's time");
        assert!(installed_at(&dir, "2.1.300").is_none(), "not installed so");
        assert!(installed_at(&dir, "../2.1.296").is_none() && installed_at(&dir, "").is_none());
        assert!(said("2.1.296", at, at + Duration::from_secs(3600)).is_none(), "fresh");
        let old = said("2.1.296", at, at + STALE + Duration::from_secs(24 * 3600)).expect("stale");
        assert!(
            old.contains("2.1.296") && old.contains("15 days") && old.contains(":claude-update"),
            "{old}"
        );
    }

    /// `:claude-update` is your own task — `claude update` by `llm.command`, quoted — in the task panel.
    #[test]
    fn claude_update_runs_as_your_own_task() {
        let mut ed = Editor::new(Default::default());
        ed.config.llm.command = "/bin/echo".into();
        crate::typed::execute(&mut ed, "claude-update").unwrap();
        let run = ed.session.task_run.as_ref().expect("a task");
        assert!(!run.by_claude, "yours");
        assert_eq!(run.task.cmd, "env -u DISABLE_UPDATES -u DISABLE_AUTOUPDATER '/bin/echo' update");
    }

    /// f22 k: run where both variables are set (a tarae started with them), `:claude-update`'s line runs
    /// claude without them. (The stand-in claude is `/bin/sh` reading a script named `update` — read, never
    /// exec'd: no ETXTBSY on Linux.)
    #[test]
    fn claude_update_runs_without_what_keeps_claude_from_updating() {
        let dir = crate::scratch::Scratch::new("claudeupdate-env");
        std::fs::write(
            dir.join("update"),
            "echo \"${DISABLE_UPDATES-unset} ${DISABLE_AUTOUPDATER-unset}\"\n",
        )
        .unwrap();
        let mut ed = Editor::new(Default::default());
        ed.config.llm.command = "/bin/sh".into();
        crate::typed::execute(&mut ed, "claude-update").unwrap();
        let cmd = ed.session.task_run.as_ref().expect("a task").task.cmd.clone();
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &cmd])
            .current_dir(&dir)
            .env("DISABLE_UPDATES", "1")
            .env("DISABLE_AUTOUPDATER", "1")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset unset\n", "{out:?}");
    }

    /// f22 k: `:claude-update`'s end says what came in the versions folder since it started — a version
    /// named; nothing, quietly; nothing after "Installation complete", a warning. Another run — of the same line
    /// too —, or the run failed: nothing of it said here. Its run is the first of its line to start after the
    /// versions were listed (f22 review).
    #[test]
    fn claude_updates_end_says_what_came() {
        let dir = crate::scratch::Scratch::new("claudeupdate-came");
        std::fs::write(dir.join("2.1.296"), "").unwrap();
        let mut ed = Editor::new(Default::default());
        ed.config.llm.command = "/bin/echo".into();
        ed.claude_update.versions = Some(dir.to_path_buf());
        crate::typed::execute(&mut ed, "claude-update").unwrap();
        settle_until(&mut ed, |ed| ed.claude_update.before.is_some());
        let b = ed.claude_update.before.clone().unwrap();
        let (cmd, before) = (b.cmd.clone(), b.listed.clone());
        assert_eq!(before, ["2.1.296"], "looked at before it ran");
        // (its run: the first of its line to start since — not another line's, not a later one)
        ed.claude_update.before = Some(Before { run: None, ..b });
        started(&mut ed, "cargo test", 3);
        started(&mut ed, &cmd, 4);
        started(&mut ed, &cmd, 5);
        assert_eq!(ed.claude_update.before.as_ref().unwrap().run, Some(4));
        // (the run that ended: `ran`'s — generation 0, as `for_test` makes it — or another)
        let said = |ed: &mut Editor, out: &str, passed: bool, ours: u64| {
            ed.claude_update.before =
                Some(Before { cmd: cmd.clone(), listed: before.clone(), run: Some(ours) });
            let mut run = crate::task::Run::for_test(crate::task::Task {
                name: "claude update".into(),
                cmd: cmd.clone(),
                cwd: PathBuf::new(),
            });
            run.output = vec![(false, out.to_string())];
            ed.session.task_run = Some(run);
            ed.toasts.clear();
            if !ran(ed, passed) {
                return None;
            }
            settle_until(ed, |ed| !ed.toasts.is_empty());
            ed.toasts.first().map(|t| (t.kind, t.text.clone()))
        };
        let (kind, text) = said(&mut ed, "Installation complete", true, 0).unwrap();
        assert!(kind == crate::editor::ToastKind::Warning && text.contains("nothing new came"), "{text}");
        let (kind, text) = said(&mut ed, "Claude Code is up to date", true, 0).unwrap();
        assert!(kind == crate::editor::ToastKind::Success && text.contains("no newer"), "{text}");
        assert_eq!(said(&mut ed, "", false, 0), None, "failed: said as any task");
        assert_eq!(said(&mut ed, "Installation complete", true, 4), None, "another run of the line");
        assert!(ed.claude_update.before.is_some(), "still its run's");
        std::fs::write(dir.join("2.1.300"), "").unwrap();
        let (kind, text) = said(&mut ed, "Installation complete", true, 0).unwrap();
        assert!(kind == crate::editor::ToastKind::Success && text.contains("2.1.300 installed"), "{text}");
    }

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }
}
