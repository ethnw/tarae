//! claude's own updates, which tarae keeps off (decision A, V1d): its updater writes a folder of programs on PATH
//! (`~/.local/bin` and `~/.local/share/claude/versions/`), closed to claude's own sandbox (`sandbox::claude`), so
//! every claude tarae starts gets `DISABLE_UPDATES` (`llm::spawn`) — and you update it yourself: `:claude-update`
//! runs `claude update` as your own task, outside the sandbox.
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

/// `:claude-update` — `claude update` (`llm.command`'s) as your own task, outside the sandbox: it asks Anthropic's
/// servers for a newer claude and installs it; the next chat process runs it.
pub fn update(ed: &mut Editor) -> Result<(), String> {
    let command = ed.config.llm.command.clone();
    let cmd = format!("{} update", crate::packs::sh_quote(&command));
    ed.task_start(crate::task::Task { name: "claude update".into(), cmd, cwd: PathBuf::new() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A claude the native installer put there over `STALE` ago is said, with how to look for a newer one; a
    /// fresh one, or one installed another way (no file), isn't; a version that isn't a plain name is never a
    /// path looked at.
    #[test]
    fn an_old_claude_is_said_with_how_to_update_it() {
        let dir = std::env::temp_dir().join(format!("tarae-claudeupdate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `:claude-update` is your own task — `claude update` by `llm.command`, quoted — in the task panel.
    #[test]
    fn claude_update_runs_as_your_own_task() {
        let mut ed = Editor::new(Default::default());
        ed.config.llm.command = "/bin/echo".into();
        crate::typed::execute(&mut ed, "claude-update").unwrap();
        let run = ed.session.task_run.as_ref().expect("a task");
        assert!(!run.by_claude, "yours");
        assert_eq!(run.task.cmd, "'/bin/echo' update");
    }
}
