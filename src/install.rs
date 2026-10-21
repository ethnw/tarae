//! Installing what a language needs — a language server, a debugger — when it isn't there. The packs say
//! how (`[install.<tool>]`, `packs.rs`); tarae only offers: a card with the command it would run, `y` runs it
//! in the task panel (its output in sight), `n` not now, `N` never for that tool (kept in the state folder).
//! Once installed, the work goes on: language servers attach to the open files that have none, debugging
//! starts again as it was (the file, the test, the attach).
//!
//! Asked once per language until quit when a file opens and no server is found (quiet otherwise: the log
//! says, the status line's dot is faint) — or when a rustup proxy says its component isn't installed — and
//! each time debugging finds no debugger. A server none of whose ways fits here (or that the packs only have
//! a `hint` for) isn't offered: how one gets it is said on screen, where an offer would be, and logged (a
//! debugger's error says what to install). A pack's `download` way is fetched by tarae itself (`fetch.rs`):
//! what it runs on is looked at first, then the card.
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
        (found.run.is_some() || found.download.is_some()) && self.may_offer(&found.tool)
    }

    /// Offers are on (`editor.offer-installs`; not in tests unless asked) and `tool` isn't one never to offer.
    fn may_offer(&self, tool: &str) -> bool {
        self.offer_popups
            && self.config.offer_installs
            && !never_path().is_some_and(|p| never_read(&p).contains(tool))
    }

    /// No language server (for a language, the first time since tarae started; or one a rustup proxy says isn't
    /// installed): offer to install the first of `tried` that a pack knows how to — else say how one gets it.
    pub(crate) fn offer_install_server(&mut self, tried: &[String]) {
        let Some(found) = tried.iter().find_map(|t| crate::packs::install(t)) else { return };
        self.offer_server(found);
    }

    fn offer_server(&mut self, found: Install) {
        self.offer_install(found, Then::Lsp);
    }

    /// Attaches servers to the open files that have none — a server just installed.
    pub(crate) fn attach_servers_missing(&mut self) {
        let ids: Vec<_> = self.docs.iter().filter(|d| d.lsp.client.is_none()).map(|d| d.id).collect();
        for id in ids {
            self.attach_lsp(id);
        }
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
            // No way fits here: said where it's seen (opening a file showed nothing else — #165), and logged —
            // once per server until quit, as an offer is (not on every file opened, nor every time it exits)
            if found.run.is_none()
                && found.download.is_none()
                && (found.hint.is_some() || !found.every.is_empty())
                && self.lsp.no_way_said.insert(found.tool.clone())
            {
                let how = found.no_way();
                crate::log::line("tarae", "info", &format!("{}: {how}", found.tool));
                if self.may_offer(&found.tool) {
                    self.set_warning(how);
                }
            }
            return;
        }
        if found.download.is_some() {
            return self.offer_fetch(found, then);
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
        // (it may have just exited for want of it: the card says why, the exit toast goes — #166)
        self.exit_explained(&tool);
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
        if self.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
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
        self.installed(then);
    }

    /// On with what waited for it.
    fn installed(&mut self, then: Then) {
        match then {
            Then::Lsp => self.attach_servers_missing(),
            Then::Resume(r) => self.resume(r),
            Then::Plugin { plugin, id } => {
                self.plugin_events_from(&plugin, vec![crate::plugin::PluginHostEvent::Installed(id)])
            }
        }
    }

    /// A `download` way: what it runs on is looked at on a worker (it runs `java -version`, say) — then the
    /// card, or on screen why not (once until quit, as "no way fits" is).
    fn offer_fetch(&mut self, found: Install, then: Then) {
        let runtime = found.download.as_ref().and_then(|d| d.runtime.clone());
        self.events.jobs().spawn(move || {
            let probed = runtime.as_ref().and_then(crate::fetch::probe);
            move |ed: &mut Editor| ed.fetch_probed(found, then, probed)
        });
    }

    fn fetch_probed(&mut self, found: Install, then: Then, probed: Option<(PathBuf, Option<u64>)>) {
        let Some(way) = found.download else { return };
        let dir = crate::packs::fetched_dir(&found.tool);
        let blocker = match (&way.runtime, &dir) {
            (_, None) => Some("there's no home folder to put it in".to_string()),
            (Some(rt), _) => crate::fetch::runtime_blocker(rt, probed.as_ref(), &found.tool),
            (None, _) => None,
        };
        let (Some(dir), None) = (dir, &blocker) else {
            let why = blocker.unwrap_or_default();
            if let Then::Plugin { plugin, id } = &then {
                let ev = crate::plugin::PluginHostEvent::Failed(*id, why.clone());
                self.plugin_events_from(plugin, vec![ev]);
            }
            if self.lsp.no_way_said.insert(found.tool.clone()) {
                crate::log::line("tarae", "info", &format!("{}: {why}", found.tool));
                self.set_warning(why);
            }
            return;
        };
        let tool = found.tool;
        let mut title = found.what;
        if let Some(first) = title.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let runs_on = match (&way.runtime, &probed) {
            (Some(rt), Some((at, Some(v)))) => format!("\nIt runs on {} ({} {v}).", at.display(), rt.program),
            (Some(_), Some((at, None))) => format!("\nIt runs on {}.", at.display()),
            _ => String::new(),
        };
        self.exit_explained(&tool);
        self.push_offer(
            Offer::new(
                title,
                &format!(
                    "{tool} isn't installed. Download {} — into:\n{}{runs_on}",
                    way.said(),
                    dir.display()
                ),
                &format!("Downloading {tool}…"),
                format!("Couldn't download {tool}"),
                What::Fetch { tool, way: Box::new(way), dir, then },
            )
            .deciding(1..2),
        );
    }

    /// `y` on a download's card: fetched on a worker, then on with what waited for it.
    pub(crate) fn fetch_start(
        &mut self,
        id: u64,
        tool: String,
        way: crate::fetch::Download,
        dir: PathBuf,
        then: Then,
    ) {
        self.events.jobs().spawn(move || {
            let got = crate::fetch::fetch(&way, &dir);
            move |ed: &mut Editor| {
                if ed.offer_by_id(id).is_none() {
                    return;
                }
                match got {
                    Ok(got) => {
                        ed.offer_finished(id, Ok(()));
                        let at = got.program.display();
                        crate::log::line(
                            "tarae",
                            "info",
                            &format!("{tool} {} downloaded: {at}", got.version),
                        );
                        ed.set_success(format!("{tool} {} installed — in {}", got.version, dir.display()));
                        ed.installed(then);
                    }
                    Err(e) => {
                        if let Then::Plugin { plugin, id } = &then {
                            let ev = crate::plugin::PluginHostEvent::Failed(*id, e.clone());
                            ed.plugin_events_from(plugin, vec![ev]);
                        }
                        ed.offer_finished(id, Err(e));
                    }
                }
            }
        });
    }

    /// A plugin's `offer-install`: the card for a tool a pack knows how to install — once per tool until
    /// quit. There already: said at once. Nothing to offer: it's told why.
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
        let r = ed.session.task_run.as_ref().expect("in the task panel");
        assert_eq!(r.task.name, "install sh");
        assert!(r.output.iter().any(|(_, l)| l == "installing"));
        assert!(ed.session.task_last.is_none(), "space m m still runs your task, not this");
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
        let y = Key { code: Code::Char('y'), ctrl: false, alt: false, shift: false };
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

    fn no_way(tool: &str) -> Install {
        Install {
            tool: tool.into(),
            what: "Lua's language server".into(),
            run: None,
            download: None,
            in_project: false,
            hint: Some("or a release from github.com/LuaLS/lua-language-server".into()),
            every: vec![format!("brew install {tool}")],
        }
    }

    /// A download way at a site that isn't there (nothing is fetched in these tests), running on `runtime`.
    fn download_way(runtime: Option<&str>) -> crate::fetch::Download {
        use crate::fetch::{Download, Name, Pick, Runtime, Sum, Unpack, Version};
        Download {
            url: "http://127.0.0.1:9/{version}/{file}".into(),
            version: Version::Newest(Pick { from: "http://127.0.0.1:9/".into(), pick: r"(\d+\.\d+)".into() }),
            file: Name::Is("tool.tar.gz".into()),
            sha256: Sum::Published("http://127.0.0.1:9/{version}/{file}.sha256".into()),
            size: None,
            unpack: Unpack::TarGz,
            program: "bin/tool".into(),
            needs: vec![],
            runtime: runtime.map(|p| Runtime {
                program: p.into(),
                home: None,
                ask: vec!["-version".into()],
                version: r"(\d+)".into(),
                at_least: 21,
                get: "install one".into(),
            }),
            rename: vec![],
        }
    }

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool, within: std::time::Duration) {
        let end = std::time::Instant::now() + within;
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    /// Where no way fits, opening a file says how to get its server — on screen, not only in the log (#165).
    #[test]
    fn no_way_here_is_said_on_screen() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        ed.offer_server(no_way("tarae-test-lua-ls"));
        assert!(ed.offers.is_empty());
        let said: Vec<_> = ed.toasts.iter().map(|t| (t.kind, t.text.as_str())).collect();
        assert_eq!(
            said,
            [(
                crate::editor::ToastKind::Warning,
                "Lua's language server isn't installed: brew install tarae-test-lua-ls, or a release from \
                 github.com/LuaLS/lua-language-server"
            )]
        );
        // Once per server until quit — not again for the next file, nor when another language asks for it
        ed.toasts.clear();
        ed.offer_server(no_way("tarae-test-lua-ls"));
        assert!(ed.toasts.is_empty(), "{:?}", ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>());
        ed.offer_server(no_way("tarae-test-other-ls"));
        assert_eq!(ed.toasts.len(), 1, "another server is its own");
        // Offers off (and in tests): only the log
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_server(no_way("tarae-test-lua-ls"));
        assert!(ed.toasts.is_empty());
    }

    /// The install card for a server that just exited for want of it takes its exit toast's place (#166).
    #[test]
    fn the_install_card_takes_the_exit_toasts_place() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        ed.set_error("rust-analyzer exited · :log-open shows why");
        let found = Install { run: Some(vec!["rustup".into()]), ..no_way("rust-analyzer") };
        ed.offer_server(found);
        assert_eq!(ed.offers.len(), 1);
        assert!(ed.toasts.iter().all(|t| !t.text.contains("exited")), "the card says why");
    }

    /// A download way whose runtime isn't here: looked at on a worker, then why not on screen — once until
    /// quit, never the card.
    #[test]
    fn a_download_needing_what_isnt_here_says_so_once() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        let found = || Install {
            download: Some(download_way(Some("tarae-no-such-runtime"))),
            ..no_way("tarae-test-dl")
        };
        ed.offer_server(found());
        assert!(ed.toasts.is_empty() && ed.offers.is_empty(), "not said yet: its runtime is looked at first");
        let start = std::time::Instant::now();
        settle_until(
            &mut ed,
            |ed| !ed.toasts.is_empty() || !ed.offers.is_empty(),
            std::time::Duration::from_secs(10),
        );
        assert!(ed.offers.is_empty(), "nothing to offer");
        let said: Vec<_> = ed.toasts.iter().map(|t| (t.kind, t.text.as_str())).collect();
        assert_eq!(
            said,
            [(
                crate::editor::ToastKind::Warning,
                "tarae-test-dl runs on tarae-no-such-runtime 21 or newer, and there's none here (PATH) — install one"
            )]
        );
        // Once until quit (counted as said: the same toast again would merge)
        let (took, said) = (start.elapsed(), ed.said);
        ed.offer_server(found());
        settle_until(&mut ed, |_| false, took * 3 + std::time::Duration::from_millis(200));
        assert_eq!(ed.said, said);
    }

    /// A download way that fits: its card (weighty, `N` never as an install's); `y` fetches on a worker, and a
    /// site that isn't there leaves the card saying so, to try again.
    #[test]
    fn a_download_is_offered_and_fetched_from_its_card() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        ed.offer_server(Install { download: Some(download_way(None)), ..no_way("tarae-test-dl") });
        settle_until(&mut ed, |ed| !ed.offers.is_empty(), std::time::Duration::from_secs(10));
        let o = &ed.offers[0];
        let What::Fetch { tool, dir, .. } = &o.what else { panic!("{:?}", o.what) };
        assert_eq!(tool, "tarae-test-dl");
        assert!(dir.ends_with("tarae/tarae-test-dl"), "{}", dir.display());
        assert!(
            o.question.contains("from 127.0.0.1:9 — checked against its published SHA-256"),
            "{}",
            o.question
        );
        assert!(o.weighty());
        assert!(o.answers(false).iter().any(|(k, _)| *k == 'N'), "never, as an install");
        ed.offer_accept();
        assert!(ed.offers[0].installing(), "fetching");
        settle_until(&mut ed, |ed| !ed.offers[0].installing(), std::time::Duration::from_secs(10));
        let crate::offer::OfferState::Failed(why) = &ed.offers[0].state else {
            panic!("{:?}", ed.offers[0].state)
        };
        assert!(!why.is_empty());
    }

    /// Java debugging without java-debug: the card is the java pack's download — the release it pins, from
    /// Maven Central, checked against the SHA-256 the pack pins — and debugging starts again once it's in.
    #[test]
    fn java_debug_is_offered_as_its_pack_pins_it() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        ed.dap_resume = Some(Resume::Debug);
        assert!(ed.offer_install_debugger("java-debug"), "the java pack says how");
        settle_until(&mut ed, |ed| !ed.offers.is_empty(), std::time::Duration::from_secs(10));
        let o = &ed.offers[0];
        let What::Fetch { tool, dir, then, .. } = &o.what else { panic!("{:?}", o.what) };
        assert_eq!((tool.as_str(), then), ("java-debug", &Then::Resume(Resume::Debug)));
        assert!(dir.ends_with("tarae/java-debug"), "{}", dir.display());
        assert!(
            o.question.contains(
                "Download version 0.53.1 from repo1.maven.org — checked against the SHA-256 its pack pins"
            ),
            "{}",
            o.question
        );
    }

    #[test]
    fn an_install_stopped_says_so_and_isnt_stopped_by_a_task() {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.offer_popups = true;
        let task = crate::task::Task { name: "build".into(), cmd: "true".into(), cwd: std::env::temp_dir() };
        install(&mut ed, "sleep 5", "sh", |ed| {
            ed.task_start(task.clone());
            assert_eq!(
                ed.session.task_run.as_ref().map(|r| r.task.name.as_str()),
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
