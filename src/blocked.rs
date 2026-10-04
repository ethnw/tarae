//! Acting on what the sandbox blocked — the user's call, never Claude's: `space m s` on a command's panel,
//! `space x s` on a test run's, `:sandbox-blocked`. A list made of the run's denials: let Claude's commands
//! write in a folder (each folder from the denied path's own up to just below home — how wide is the
//! user's call; `allow::write_choices` leaves out what must never open), connect to a port on this machine
//! (not the risky ones — `sandbox::RISKY_PORTS`, by hand only), or run that one command outside the
//! sandbox. Each is a weighty card. A write or a port is for once (the command runs again now with it),
//! for this conversation, or always (`allow.rs`) — and runs it again now too. That run is the user's: how it
//! ended goes to Claude with the next message (`tools::For::User`).

use std::path::{Path, PathBuf};

use crate::allow::{Grant, Scope};
use crate::editor::Editor;
use crate::sandbox::Denial;
use crate::tools::{Extra, For, Job};

/// A run of Claude's the sandbox blocked something in: what ran, and what was denied.
#[derive(Clone, Debug)]
pub struct BlockedRun {
    pub job: Job,
    pub denials: Vec<Denial>,
}

impl BlockedRun {
    /// What ran, as the cards and Claude say it.
    pub fn label(&self) -> String {
        match &self.job {
            Job::Command(c, _) => c.clone(),
            Job::Tests(t) => format!("tests: {}", t.label),
        }
    }
}

/// Which run's denials: the command's panel, the test run's, or whichever has them (newest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Task,
    Test,
    Either,
}

/// One thing that could open: what, its list row's hint (what was denied, short — the part below the
/// folder offered), and what its card says was denied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub grant: Grant,
    pub hint: String,
    pub why: String,
}

/// What could open for `denials`: the folders a write may open (`allow::write_choices`), a port on this
/// machine (not a risky one) — each once. Home, the project and the folders on PATH passed in.
pub fn choices(denials: &[Denial], home: &Path, root: &Path, path: &[PathBuf]) -> Vec<Choice> {
    let mut out: Vec<Choice> = Vec::new();
    for d in denials {
        let found: Vec<Choice> = if d.op.starts_with("file-write") && d.target.starts_with('/') {
            let target = crate::sandbox::real(Path::new(&d.target));
            crate::allow::write_choices(&target, home, root, path)
                .into_iter()
                .map(|dir| {
                    let below = target.strip_prefix(&dir).map(|r| format!("…/{}", r.display()));
                    let below = below.unwrap_or_else(|_| d.target.clone());
                    let why = format!("{} was writing {below} in it", d.process);
                    Choice { grant: Grant::Write(dir), hint: below, why }
                })
                .collect()
        } else if d.op == "network-outbound"
            && let Some(n) = d.target.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok())
            && !crate::sandbox::RISKY_PORTS.iter().any(|(p, _)| *p == n)
        {
            let why = format!("{} was connecting to it", d.process);
            vec![Choice { grant: Grant::Port(n), hint: format!("{} connecting", d.process), why }]
        } else {
            Vec::new()
        };
        for c in found {
            if !out.iter().any(|o| o.grant == c.grant) {
                out.push(c);
            }
        }
    }
    out
}

/// The folders on `PATH` (real paths) — never opened for writing: a program put there runs as the user.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).filter_map(|d| std::fs::canonicalize(d).ok()).collect())
        .unwrap_or_default()
}

impl Editor {
    /// The run of Claude's whose denials `which` means, if the sandbox blocked anything in it.
    fn blocked_run(&self, which: Which) -> Option<BlockedRun> {
        let denials = |g: Option<&crate::sandbox::Guard>| -> Option<Vec<Denial>> {
            let seen = g?.watch()?.blocked().seen;
            (!seen.is_empty()).then_some(seen)
        };
        let task = self.task_run.as_ref().and_then(|r| {
            Some((
                r.started,
                BlockedRun {
                    job: Job::Command(r.task.cmd.clone(), r.task.cwd.clone()),
                    denials: denials(r.guard.as_ref())?,
                },
            ))
        });
        let test = self.test_run.as_ref().and_then(|r| {
            let job = Job::Tests(Box::new(r.target.clone()));
            Some((r.started, BlockedRun { job, denials: denials(r.guard.as_ref())? }))
        });
        match which {
            Which::Task => task.map(|t| t.1),
            Which::Test => test.map(|t| t.1),
            Which::Either => match (task, test) {
                (Some(a), Some(b)) => Some(if a.0 >= b.0 { a.1 } else { b.1 }),
                (a, b) => a.or(b).map(|t| t.1),
            },
        }
    }

    /// `space m s` · `space x s` · `:sandbox-blocked` — what could open for what the sandbox blocked in that
    /// run: a list; Enter puts one on its card.
    pub fn sandbox_blocked(&mut self, which: Which) {
        let Some(run) = self.blocked_run(which) else {
            return self.set_status("the sandbox blocked nothing in that run — its panel says when it does");
        };
        let home = crate::allow::home().unwrap_or_else(|| PathBuf::from("/"));
        let list = choices(&run.denials, &home, &self.root, &path_dirs());
        use crate::picker::{Action, Item};
        let mut items: Vec<Item> = list
            .iter()
            .enumerate()
            .map(|(i, c)| Item {
                label: match &c.grant {
                    Grant::Write(p) => format!("allow writing in {}", crate::allow::tilde(p)),
                    Grant::Port(n) => format!("allow port {n} on this machine"),
                },
                action: Action::Typed(format!("sandbox-grant {i}")),
                hint: c.hint.clone(),
                glyph: Some(("◦", "ui.virtual")),
            })
            .collect();
        items.push(Item {
            label: "run it outside the sandbox, once".into(),
            action: Action::Typed("sandbox-grant outside".into()),
            hint: run.label(),
            glyph: Some(("▲", "warning")),
        });
        self.blocked = Some((run, list));
        let p = crate::picker::Picker::new("what the sandbox blocked", items, false).without_preview();
        self.open_picker(p, None);
    }

    /// `:sandbox-grant <n | outside>` (the list's rows) — its card.
    pub fn sandbox_grant(&mut self, arg: &str) -> Result<(), String> {
        let (run, list) =
            self.blocked.clone().ok_or("nothing blocked to act on — space m s on a run's panel")?;
        let cmd = run.label();
        let run = Box::new(run);
        let card = match arg.trim() {
            "outside" => crate::offer::Offer::new(
                OUTSIDE_TITLE.into(),
                &format!(
                    "{cmd}\n\nOutside the sandbox it runs with all your permissions — your files, your network — \
                     this once. Claude hears how it ended with your next message."
                ),
                "",
                String::new(),
                crate::offer::What::Outside { run },
            ),
            n => {
                let Choice { grant, why, .. } =
                    n.parse::<usize>().ok().and_then(|i| list.get(i).cloned()).ok_or("no such row")?;
                let question = format!(
                    "Let Claude's commands {}\n\n{why}, in `{cmd}` — which runs again now with it open, whichever \
                     you choose. Always lasts until :sandbox-allowed takes it back.",
                    grant.shown()
                );
                let what = crate::offer::What::Grant { grant, run };
                crate::offer::Offer::new(GRANT_TITLE.into(), &question, "", String::new(), what)
            }
        };
        self.push_offer(card);
        Ok(())
    }
}

pub const GRANT_TITLE: &str = "Open the sandbox a little?";
pub const OUTSIDE_TITLE: &str = "Run it outside the sandbox?";

/// The answer on a grant's card: None = no; else `grant` is allowed for `scope` and the run goes again now
/// with it — the user's run, its end told to Claude.
pub fn grant_answer(ed: &mut Editor, id: u64, grant: Grant, run: BlockedRun, scope: Option<Scope>) {
    ed.offer_finished(id, Ok(()));
    let Some(scope) = scope else {
        return ed.set_status("nothing opened — the sandbox stays as it was");
    };
    crate::allow::grant(ed, &grant, scope);
    let head = format!(
        "The user ran `{}` again, letting it {}{}. How it ended:",
        run.label(),
        grant.shown(),
        if scope == Scope::Once { " this once" } else { "" }
    );
    let extra = Extra { grant: (scope == Scope::Once).then_some(grant), outside: false };
    crate::tools::guard_and_go(ed, For::User(head), run.job, true, extra);
}

/// The answer on the run-outside card: `y` runs it now, outside the sandbox — the user's run.
pub fn outside_answer(ed: &mut Editor, id: u64, run: BlockedRun, yes: bool) {
    ed.offer_finished(id, Ok(()));
    if !yes {
        return ed.set_status("not run — the sandbox stays as it was");
    }
    let head = format!("The user ran `{}` again outside the sandbox, this once. How it ended:", run.label());
    crate::tools::guard_and_go(ed, For::User(head), run.job, true, Extra { grant: None, outside: true });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run's denials offer: the folders a write may open, a port that isn't a risky one — once each.
    #[test]
    fn what_a_runs_denials_offer() {
        let base = std::env::temp_dir().join(format!("tarae-blocked-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("home/.npm/_cacache")).unwrap();
        std::fs::create_dir_all(base.join("home/proj")).unwrap();
        let home = std::fs::canonicalize(base.join("home")).unwrap();
        let root = home.join("proj");
        let d = |op: &str, target: String| Denial { process: "npm".into(), op: op.into(), target };
        let denials = [
            d("file-write-create", home.join(".npm/_cacache/x").display().to_string()),
            d("file-write-data", home.join(".npm/_cacache/y").display().to_string()),
            d("network-outbound", "remote:*:5432".into()),
            d("network-outbound", "remote:*:2375".into()),
            d("file-write-create", root.join(".git/index.lock").display().to_string()),
            d("mach-lookup", "com.apple.lsd.mapdb".into()),
        ];
        let all = choices(&denials, &home, &root, &[]);
        assert_eq!((all[0].hint.as_str(), all[1].hint.as_str()), ("…/x", "…/_cacache/x"), "below the folder");
        assert_eq!(all[2].why, "npm was connecting to it");
        let got: Vec<Grant> = all.into_iter().map(|c| c.grant).collect();
        assert_eq!(
            got,
            [Grant::Write(home.join(".npm/_cacache")), Grant::Write(home.join(".npm")), Grant::Port(5432)],
            "Docker's port and the project's .git aren't offered; each folder once"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
