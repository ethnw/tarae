//! Installing what a language needs — a language server, a debugger — when it isn't there. The packs say
//! how (`[install.<tool>]`, `packs.rs`); tarae only offers: a card with the command it would run, `y` runs it
//! in the task panel (its output in sight), `n` not now, `N` never for that tool (kept in the state folder).
//! Once installed, the work goes on: language servers attach to the open files that have none, debugging
//! starts again as it was (the file, the test, the attach).
//!
//! Asked once per language a session when a file opens and no server is found (quiet otherwise: the log
//! says, the status line's dot is faint) — or when a rustup proxy says its component isn't installed — and
//! each time debugging finds no debugger. A server the packs only have a `hint` for isn't offered; the hint
//! goes to the log (a debugger's error says what to install).
//!
//! An install only runs from a card in sight, and while it runs no other task starts (that would stop it
//! half-way); stopping it (`space m c`) says so on its card.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::editor::Editor;
use crate::offer::{Offer, Resume, What};
use crate::packs::Install;

/// What goes on once a tool is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Then {
    /// A language server: attach servers to the open files that have none (one server can be several
    /// languages': TypeScript's and JavaScript's).
    Lsp,
    /// A debugger: start what stopped for it.
    Resume(Resume),
    /// A plugin asked for it (`offer-install`): it hears (`installed`, with its id).
    Plugin { plugin: String, id: u64 },
}

/// Where the tools never to offer again are kept, one a line.
fn never_path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("never-install"))
}

fn never_read(path: &std::path::Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Adds a line — appending, so two tareas saying `N` at once both keep theirs, and a file that can't be
/// read isn't written over.
fn never_add(path: &std::path::Path, tool: &str) -> std::io::Result<()> {
    use std::io::Write;
    if never_read(path).contains(tool) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // (after a last line with no newline)
    let ends_open = std::fs::read(path).is_ok_and(|b| b.last().is_some_and(|&c| c != b'\n'));
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}{tool}", if ends_open { "\n" } else { "" })
}

/// `run` as one `sh` line, for the card and the task panel: each word as the list has it — quoted unless
/// plain, and the first also when `sh` would read it as a keyword or `NAME=value`.
pub fn shown(run: &[String]) -> String {
    const KEYWORDS: &[&str] = &[
        "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done", "in",
        "function", "select", "time", "!", "{", "}", "[[", "]]",
    ];
    let word = |(i, w): (usize, &String)| {
        let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=+,%".contains(c));
        if plain && !(i == 0 && (w.contains('=') || KEYWORDS.contains(&w.as_str()))) {
            w.clone()
        } else {
            format!("'{}'", w.replace('\'', r"'\''"))
        }
    };
    run.iter().enumerate().map(word).collect::<Vec<_>>().join(" ")
}

impl Editor {
    /// Whether to offer it: there's a way here, offers are on (`editor.offer-installs`; not in tests unless
    /// asked), and it isn't one never to offer.
    fn offerable(&self, found: &Install) -> bool {
        found.run.is_some()
            && self.offer_popups
            && self.config.offer_installs
            && !never_path().is_some_and(|p| never_read(&p).contains(&found.tool))
    }

    /// No language server (for a language, the first time this session; or one a rustup proxy says isn't
    /// installed): offer to install the first of `tried` that a pack knows how to — else log what the pack
    /// says.
    pub(crate) fn offer_install_server(&mut self, tried: &[String]) {
        let Some(found) = tried.iter().find_map(|t| crate::packs::install(t)) else { return };
        self.offer_install(found, Then::Lsp);
    }

    /// No debugger (`tool`): offer it, to go on with what stopped. false if there's nothing to offer — then
    /// the caller says what's missing.
    pub(crate) fn offer_install_debugger(&mut self, tool: &str) -> bool {
        let Some(found) = crate::packs::install(tool) else { return false };
        if !self.offerable(&found) {
            return false;
        }
        // What stopped: launching the file, a test's debugging, an attach
        let then = self.dap_resume.clone().unwrap_or(Resume::Debug);
        self.offer_install(found, Then::Resume(then));
        true
    }

    fn offer_install(&mut self, found: Install, then: Then) {
        if !self.offerable(&found) {
            if let (None, Some(hint)) = (&found.run, &found.hint) {
                crate::log::line("tarae", "info", &format!("{} ({}): {hint}", found.tool, found.what));
            }
            return;
        }
        let Some(run) = found.run else { return };
        let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let here = self.doc().path.as_deref().and_then(|p| p.parent()).map(PathBuf::from);
        let cwd = if found.in_project { here.unwrap_or_else(home) } else { home() };
        let tool = found.tool;
        let mut title = found.what;
        if let Some(first) = title.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        // (what's decided: the command, under the line saying why)
        let command = shown(&run);
        let lines = command.split('\n').count();
        self.push_offer(
            Offer::new(
                title,
                &format!("{tool} isn't installed. Install it with:\n{command}"),
                &format!("Installing {tool}…"),
                format!("Couldn't install {tool}"),
                What::Install { tool, run, cwd, then },
            )
            .deciding(1..1 + lines),
        );
    }

    /// `y`: runs the install in the task panel (the card waits with it). Not while a task is running — its
    /// panel is in use; the card says so, and `y` tries again.
    pub(crate) fn install_start(
        &mut self,
        offer: u64,
        tool: String,
        run: Vec<String>,
        cwd: PathBuf,
        then: Then,
    ) {
        if self.lane.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
            return self.offer_finished(offer, Err("a task is running — y once it's done".into()));
        }
        let task = crate::task::Task { name: format!("install {tool}"), cmd: shown(&run), cwd };
        let after = crate::task::After { offer, tool, then };
        self.task_start_install(task, after);
    }

    /// The install ended: on with the work if the tool can now be found.
    pub(crate) fn install_done(&mut self, after: crate::task::After, passed: bool) {
        let crate::task::After { offer, tool, then } = after;
        let failed = |ed: &mut Editor, why: String| {
            if let Then::Plugin { plugin, id } = &then {
                let ev = crate::plugin::PluginHostEvent::Failed(*id, why.clone());
                ed.plugin_events_from(plugin, vec![ev]);
            }
            ed.offer_finished(offer, Err(why));
        };
        if !passed {
            return failed(self, "it failed — the panel below says why".into());
        }
        let off_path = crate::packs::installed_at(&tool);
        if !crate::packs::on_path(&tool) && off_path.is_none() {
            return failed(self, format!("it ran, but {tool} isn't on PATH — see the panel"));
        }
        self.offer_finished(offer, Ok(()));
        match off_path.filter(|_| !crate::packs::on_path(&tool)) {
            Some(p) => self.set_success(format!(
                "{tool} installed — in {}, not on your PATH: tarae uses it from there",
                p.parent().unwrap_or(&p).display()
            )),
            None => self.set_success(format!("{tool} installed")),
        }
        match then {
            Then::Lsp => {
                let ids: Vec<_> = self.docs.iter().filter(|d| d.lsp.client.is_none()).map(|d| d.id).collect();
                for id in ids {
                    self.attach_lsp(id);
                }
            }
            Then::Resume(r) => self.resume(r),
            Then::Plugin { plugin, id } => {
                self.plugin_events_from(&plugin, vec![crate::plugin::PluginHostEvent::Installed(id)])
            }
        }
    }

    /// A plugin's `offer-install`: the card for a tool a pack knows how to install — once a session per
    /// tool. There already: said at once. Nothing to offer: it's told why.
    pub(crate) fn plugin_offer_install(&mut self, plugin: &str, id: u64, tool: &str) {
        use crate::plugin::PluginHostEvent as E;
        if crate::packs::on_path(tool) || crate::packs::installed_at(tool).is_some() {
            return self.plugin_events_from(plugin, vec![E::Installed(id)]);
        }
        let why = match crate::packs::install(tool) {
            None => Some(format!("no language pack says how to install {tool}")),
            Some(_) if self.plugins.as_mut().is_some_and(|p| !p.offered.insert(tool.to_string())) => {
                Some(format!("{tool} was offered already"))
            }
            Some(found) if !self.offerable(&found) => Some(
                found.hint.clone().map_or(format!("{tool} isn't offered here"), |h| format!("{tool}: {h}")),
            ),
            Some(found) => {
                self.offer_install(found, Then::Plugin { plugin: plugin.to_string(), id });
                None
            }
        };
        if let Some(why) = why {
            self.plugin_events_from(plugin, vec![E::Failed(id, why)]);
        }
    }

    /// `N`: never offer this tool again.
    pub(crate) fn install_never(&mut self, tool: &str) {
        let Some(path) = never_path() else { return };
        match never_add(&path, tool) {
            Ok(()) => self.set_status(format!("tarae won't offer {tool} again (it's in {})", path.display())),
            Err(e) => self.set_error(format!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_shown_as_sh_reads_them() {
        let run = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            shown(&run(&["go", "install", "golang.org/x/tools/gopls@latest"])),
            "go install golang.org/x/tools/gopls@latest"
        );
        assert_eq!(shown(&run(&["npm", "i", "-g", "a b", "it's"])), r"npm i -g 'a b' 'it'\''s'");
        assert_eq!(shown(&run(&["A=1", "env", "x=y"])), "'A=1' env x=y", "not an assignment");
        assert_eq!(shown(&run(&["if", "$HOME", "", "*"])), r"'if' '$HOME' '' '*'");
    }

    #[test]
    fn never_again_is_kept() {
        let d = std::env::temp_dir().join(format!("tarae-never-{}", std::process::id()));
        let path = d.join("state/never-install");
        assert!(never_read(&path).is_empty(), "no file yet");
        never_add(&path, "marksman").unwrap();
        never_add(&path, "gopls").unwrap();
        never_add(&path, "gopls").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "marksman\ngopls\n");
        std::fs::write(&path, "marksman\ngopls").unwrap();
        never_add(&path, "zls").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "marksman\ngopls\nzls\n", "after an open line");
        assert!(never_read(&path).contains("marksman"));
        std::fs::remove_dir_all(&d).ok();
    }

    /// An install card, `y` on it (as if in sight), and its run until it's done — or `stop` right away.
    fn install(ed: &mut Editor, script: &str, tool: &str, stop: impl FnOnce(&mut Editor)) {
        ed.push_offer(Offer::new(
            "A tool".into(),
            "?",
            "…",
            "Couldn't".into(),
            What::Install {
                tool: tool.into(),
                run: vec!["sh".into(), "-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                then: Then::Lsp,
            },
        ));
        ed.offer_accept();
        stop(ed);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ed.offers.iter().any(Offer::installing) {
            let ev =
                ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now())).unwrap();
            ed.handle_event(ev);
        }
    }

    #[test]
    fn an_install_runs_in_the_task_panel_then_goes_on() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        install(&mut ed, "echo installing; exit 0", "sh", |_| {});
        assert!(ed.offers.is_empty(), "installed: the card goes");
        let r = ed.lane.task_run.as_ref().expect("in the task panel");
        assert_eq!(r.task.name, "install sh");
        assert!(r.output.iter().any(|(_, l)| l == "installing"));
        assert!(ed.lane.task_last.is_none(), "space m m still runs your task, not this");
        assert!(r.install, "the panel offers no `space m m again` for it");
        install(&mut ed, "exit 3", "sh", |_| {});
        assert!(
            matches!(ed.offers[0].state, crate::offer::OfferState::Failed(_)),
            "failed: the card says so"
        );
        ed.offers.clear();
        install(&mut ed, "exit 0", "tarae-no-such-tool", |_| {});
        let crate::offer::OfferState::Failed(why) = &ed.offers[0].state else { panic!("still missing") };
        assert!(why.contains("isn't on PATH"), "{why}");
    }

    #[test]
    fn an_install_card_answers_only_in_sight() {
        use crate::key::{Code, Key};
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        let run = vec!["sh".into(), "-c".into(), "true".into()];
        ed.push_offer(Offer::new(
            "A tool".into(),
            "?",
            "…",
            "x".into(),
            What::Install { tool: "sh".into(), run, cwd: std::env::temp_dir(), then: Then::Lsp },
        ));
        let y = Key { code: Code::Char('y'), ctrl: false, alt: false };
        assert!(!ed.offer_key(y), "not drawn (too narrow, a picker over it): y isn't an answer");
        assert!(!ed.offers[0].installing());
        ed.screen.offer_buttons = vec![(20, 50, 59, 'y'), (20, 62, 71, 'n'), (20, 74, 81, 'N')];
        // Just drawn: a y typed a moment ago (meant for the code) is swallowed, not an answer
        let o = (ed.offers[0].id, ed.offers[0].rev);
        ed.offer_whole.set(Some(o)); // (drawn uncut)
        ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now())));
        assert!(ed.offer_key(y));
        assert!(!ed.offers[0].installing());
        ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
        assert!(ed.offer_key(y));
        assert!(ed.offers[0].installing());
    }

    #[test]
    fn an_install_stopped_says_so_and_isnt_stopped_by_a_task() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        let task = crate::task::Task { name: "build".into(), cmd: "true".into(), cwd: std::env::temp_dir() };
        install(&mut ed, "sleep 5", "sh", |ed| {
            ed.task_start(task.clone());
            assert_eq!(
                ed.lane.task_run.as_ref().map(|r| r.task.name.as_str()),
                Some("install sh"),
                "still running"
            );
            ed.task_close();
        });
        let crate::offer::OfferState::Failed(why) = &ed.offers[0].state else { panic!("the card is stuck") };
        assert!(why.contains("stopped"), "{why}");
        ed.offer_dismiss();
        assert!(ed.offers.is_empty(), "and it can be closed");
    }
}
