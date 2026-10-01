//! Plugins: WebAssembly modules on tarae's own runtime (`crate::wasm`), speaking the API in
//! `plugin-sdk/wit/tarae.wit`. They live in `~/.config/tarae/plugins/*.wasm`.
//!
//! - `cabi.rs`: the canonical ABI's values in a plugin's memory. `abi.rs`: the API's types, and loading and
//!   calling one plugin. `host.rs`: the plugin thread. `cli.rs`: `tarae plugin new|dev|list`. Here: the
//!   editor's side — commands and answers.
//! - **Nothing when unused**: the config watcher looks at the plugins folder (`host::Watch`), and the plugin
//!   thread starts the first time there's something in it.
//! - **Commands**: each plugin's commands become `:name args`, `":name"` in a keymap, and palette entries.
//!   A name tarae already uses, or another plugin took first, is skipped with a warning.
//! - **Answers**: actions apply in order. The edits among them — `replace-selections`, `edits`, `select` —
//!   are one transaction (one undo step) where the first of them is, on the buffer and selections as the
//!   plugin saw them: if the text changed meanwhile, or the user is typing in it, they're refused. The
//!   buffer in front stays in front.
//! - **Nothing a plugin says is trusted**: `run` takes only movement commands (so a plugin can't reach
//!   `:sh`, `:w` or anything else with effects outside the buffer), text is shown without control
//!   characters (no terminal escapes) and capped, and names are checked.

mod abi;
mod cabi;
mod cli;
mod events;
mod host;
mod io;
mod panel;
mod perm;

use std::path::{Path, PathBuf};

pub use cli::cli;
#[cfg(test)]
pub use host::scan;
pub use host::{Found, Watch};

use abi::Action;
pub use abi::Event as PluginEvent;
pub use abi::HostEvent as PluginHostEvent;
pub use abi::Permission as PluginPermission;
pub use abi::Side as PluginSide;
pub use abi::Tone as PluginTone;
use host::{Call, Host, Input, Manifest, Note};
pub use panel::{PanelState as PluginPanelState, key as plugin_panel_key};
pub use perm::Ask as PluginAsk;

use crate::document::DocId;
use crate::editor::Editor;
use crate::event::Event;
use crate::selection::{Range, Selection};
use crate::transaction::{Change, Transaction};

pub fn dir() -> Option<PathBuf> {
    Some(crate::config::config_path()?.parent()?.join("plugins"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCommand {
    pub name: String,
    pub doc: String,
    pub plugin: String,
}

/// The most commands one plugin registers.
const MAX_COMMANDS: usize = 256;
/// The longest message or description shown from a plugin (in characters).
const MAX_TEXT: usize = 2000;

/// A character that reorders text around it (so what's shown isn't what's there).
pub(crate) fn bidi(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
}

/// Plugin text fit for the screen: control and reordering characters (terminal escapes among them)
/// replaced, and capped.
pub(crate) fn shown(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(MAX_TEXT)
        .map(|c| if (c.is_control() && c != '\n') || bidi(c) { '\u{FFFD}' } else { c })
        .collect();
    if s.chars().nth(MAX_TEXT).is_some() {
        out.push('…');
    }
    out
}

/// Why a plugin's command name can't be registered, if it can't.
fn bad_name(name: &str) -> Option<&'static str> {
    if name.is_empty()
        || name.chars().count() > 64
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        Some("isn't a command name")
    } else if name.parse::<usize>().is_ok() || crate::cmdline::find(name).is_some() {
        Some("is tarae's own")
    } else {
        None
    }
}

/// The longest status-line text from a plugin (in characters).
const MAX_STATUS: usize = 60;

/// The editor's plugin state: the thread, the commands the plugins registered, what they show in the
/// status line (per plugin and buffer), and the events they subscribed to.
pub struct Plugins {
    host: Host,
    pub commands: Vec<PluginCommand>,
    status: std::collections::BTreeMap<(String, DocId), String>,
    events: events::Events,
    /// What each plugin may do beyond the buffer (as it says), and the programs it runs, by (plugin, id).
    permissions: std::collections::HashMap<String, Vec<abi::Permission>>,
    procs: std::collections::HashMap<(String, u64), io::Proc>,
    /// Its connections (open, and being opened), and its timers — by (plugin, id), with their run's serial.
    socks: std::collections::HashMap<(String, u64), io::Sock>,
    pending: std::collections::HashMap<(String, u64), u64>,
    timers: std::collections::HashMap<(String, u64), (std::time::Instant, u64)>,
    clock: io::Clock,
    /// What's on its way to each plugin (for its current load).
    outbox: std::collections::HashMap<String, io::Outbox>,
    /// What you approved (kept), and the plugins asked about this session.
    approvals: std::collections::BTreeMap<String, perm::Approval>,
    asked: std::collections::HashSet<String>,
    /// Where approvals are kept, and when it was last read.
    perm_store: Option<PathBuf>,
    perm_mtime: Option<std::time::SystemTime>,
    serial: u64,
    /// The panel a plugin shows below the code (the last one to show one), and the plugin whose answer to
    /// you (to a command, a key in its panel, a line typed) is being applied: its panel takes the keys, and
    /// it may ask for a line.
    pub panel: Option<panel::PanelState>,
    answering: Option<String>,
    /// Its lines being typed at (by token), and the buffers it shows (by its id).
    prompts: std::collections::HashMap<u64, (String, u64)>,
    views: std::collections::HashMap<(String, u64), DocId>,
    /// What plugins write beside buffers' lines, by (plugin, path): the version they're for, and the hints.
    hints: std::collections::HashMap<(String, String), (u64, Vec<abi::Hint>)>,
    /// The side panel's plugin tab (the last plugin to show one), and the status line's plugin text.
    pub side: Option<(String, abi::Side)>,
    pub badge: Option<(String, String, abi::Tone)>,
    /// The tools plugins offered to install this session (once each).
    pub(crate) offered: std::collections::HashSet<String>,
    /// Where each plugin's own folder is (`<this>/<plugin>`): the state folder's `plugins`.
    own_root: Option<PathBuf>,
}

impl Plugins {
    /// Stops what a plugin started: its programs, connections and timers.
    fn stop_all(&mut self, plugin: &str) {
        self.procs.retain(|(p, _), _| p != plugin);
        self.socks.retain(|(p, _), _| p != plugin);
        self.pending.retain(|(p, _), _| p != plugin);
        let clock = &self.clock;
        self.timers.retain(|(p, _), at| {
            let keep = p != plugin;
            if !keep {
                clock.cancel(*at);
            }
            keep
        });
    }

    /// Forgets what a plugin registered (it's gone, or reloading).
    fn drop_plugin(&mut self, plugin: &str) {
        self.commands.retain(|c| c.plugin != plugin);
        self.status.retain(|(p, _), _| p != plugin);
        self.events.subs.retain(|(p, ..)| p != plugin);
        self.permissions.remove(plugin);
        self.outbox.remove(plugin);
        self.stop_all(plugin);
        if self.panel.as_ref().is_some_and(|s| s.plugin == plugin) {
            self.panel = None;
        }
        self.prompts.retain(|_, (p, _)| p != plugin);
        self.views.retain(|(p, _), _| p != plugin);
        self.hints.retain(|(p, _), _| p != plugin);
        if self.side.as_ref().is_some_and(|(p, _)| p == plugin) {
            self.side = None;
        }
        if self.badge.as_ref().is_some_and(|(p, ..)| p == plugin) {
            self.badge = None;
        }
    }
}

impl Editor {
    /// The plugins folder's contents changed (the config watcher noticed): load new and changed plugins, drop
    /// missing ones. The first time there's something, the plugin thread starts.
    pub fn plugins_sync(&mut self, found: Found) {
        if self.plugins.is_none() {
            if found.is_empty() {
                return;
            }
            let events = self.events.sender();
            let started = Host::start(move |note| {
                let _ = events.send(Event::Job(Box::new(move |ed: &mut Editor| ed.plugin_note(note))));
            });
            let Some(host) = started else {
                self.set_error("plugins: couldn't start their thread");
                return;
            };
            self.plugins = Some(Plugins {
                host,
                commands: Vec::new(),
                status: Default::default(),
                events: Default::default(),
                permissions: Default::default(),
                approvals: perm::load(perm::store_path().as_deref()),
                asked: Default::default(),
                perm_store: perm::store_path(),
                perm_mtime: None,
                procs: Default::default(),
                socks: Default::default(),
                pending: Default::default(),
                timers: Default::default(),
                clock: Default::default(),
                outbox: Default::default(),
                serial: 0,
                panel: None,
                answering: None,
                prompts: Default::default(),
                views: Default::default(),
                hints: Default::default(),
                side: None,
                badge: None,
                offered: Default::default(),
                own_root: crate::config::state_dir().map(|s| s.join("plugins")),
            });
        }
        if let Some(p) = &self.plugins {
            p.host.sync(found);
        }
    }

    fn plugin_note(&mut self, note: Note) {
        let Some(p) = &mut self.plugins else { return };
        match note {
            Note::Loaded { plugin, manifest: Err(e), .. } => {
                p.drop_plugin(&plugin);
                self.plugin_cards_close(&plugin);
                self.plugin_commands_changed();
                self.set_error(format!("plugin {plugin}: {}", shown(&e)));
            }
            Note::Loaded {
                plugin,
                manifest: Ok(Manifest { commands, subscriptions, permissions }),
                reload,
                generation,
            } => {
                p.drop_plugin(&plugin);
                p.outbox.insert(plugin.clone(), io::Outbox::new(generation));
                p.asked.remove(&plugin);
                p.permissions.insert(plugin.clone(), permissions.clone());
                for s in subscriptions {
                    if !p.events.subs.iter().any(|(q, e, _)| *q == plugin && *e == s.event) {
                        p.events.subs.push((plugin.clone(), s.event, s.languages));
                    }
                }
                p.events.subs.sort_by(|a, b| a.0.cmp(&b.0));
                let mut taken: std::collections::HashSet<String> =
                    p.commands.iter().map(|c| c.name.clone()).collect();
                let mut skipped = Vec::new();
                let mut added = Vec::new();
                let too_many = commands.len().saturating_sub(MAX_COMMANDS);
                for c in commands.into_iter().take(MAX_COMMANDS) {
                    let why = bad_name(&c.name).or_else(|| taken.contains(&c.name).then_some("is taken"));
                    match why {
                        Some(why) => skipped.push(format!("“{}” {why}", shown(&c.name))),
                        None => {
                            added.push(format!(":{}", c.name));
                            taken.insert(c.name.clone());
                            p.commands.push(PluginCommand {
                                name: c.name,
                                doc: shown(&c.doc),
                                plugin: plugin.clone(),
                            });
                        }
                    }
                }
                if too_many > 0 {
                    skipped.push(format!("{too_many} commands past the first {MAX_COMMANDS}"));
                }
                self.plugin_commands_changed();
                // A card from before it loaded again asks about what it asked then
                self.plugin_cards_close(&plugin);
                self.plugin_permissions_loaded(&plugin, &permissions);
                if !skipped.is_empty() {
                    self.set_warning(format!("plugin {plugin}: skipped {}", skipped.join(", ")));
                }
                // Loading at start is routine; a reload is what `tarae plugin dev` waits to see
                if reload {
                    self.set_success(format!("Plugin {plugin} reloaded  {}", added.join(" ")));
                }
            }
            Note::Removed { plugin } => {
                p.drop_plugin(&plugin);
                self.plugin_cards_close(&plugin);
                self.plugin_commands_changed();
            }
        }
    }

    /// Keeps `:` completion in step with the registered commands.
    fn plugin_commands_changed(&mut self) {
        let cmds = self.plugins.as_ref().map(|p| p.commands.as_slice()).unwrap_or_default();
        self.cmd_lists.plugins =
            cmds.iter().map(|c| (c.name.clone(), format!("{}  · {}", c.doc, c.plugin))).collect();
    }

    pub fn plugin_command(&self, name: &str) -> Option<&PluginCommand> {
        self.plugins.as_ref()?.commands.iter().find(|c| c.name == name)
    }

    /// Runs a plugin command on the current buffer. Here only the rope is cloned (O(1)) and the selections
    /// copied — the plugin thread does the rest, and the answer comes back as an event.
    pub fn plugin_run(&mut self, name: &str, args: &str) -> Result<(), String> {
        let cmd = self.plugin_command(name).cloned().ok_or_else(|| format!("no such command: '{name}'"))?;
        let (input, seen) = self.plugin_input(self.doc().id, args).expect("the current buffer is open");
        let events = self.events.sender();
        let plugin = cmd.plugin.clone();
        let reply = Box::new(move |result: Result<Vec<Action>, String>| {
            let apply = move |ed: &mut Editor| match result {
                Ok(actions) => {
                    // A panel this command shows takes the keys
                    if let Some(p) = &mut ed.plugins {
                        p.answering = Some(plugin.clone());
                    }
                    ed.plugin_apply(&plugin, &seen, actions);
                    if let Some(p) = &mut ed.plugins {
                        p.answering = None;
                    }
                }
                Err(e) => ed.set_error(format!("{plugin}: {}", shown(&e))),
            };
            let _ = events.send(Event::Job(Box::new(apply)));
        });
        let p = self.plugins.as_ref().ok_or("plugins aren't running")?;
        p.host.call(&cmd.plugin, Call::Command(name.to_string()), input, reply)
    }

    /// What a plugin is given about buffer `doc`, and what its answer applies to. Only the rope is cloned
    /// (O(1)) and the selections copied: the plugin thread makes the strings.
    fn plugin_input(&self, doc: DocId, args: &str) -> Option<(Input, Seen)> {
        let doc = self.docs.iter().find(|d| d.id == doc)?;
        let sel = doc.selection();
        let input = Input {
            args: args.to_string(),
            text: doc.text.clone(),
            ranges: sel.ranges().iter().map(|r| (r.anchor, r.head)).collect(),
            primary: sel.primary_index(),
            path: doc.path.as_ref().map(|p| p.display().to_string()),
            name: doc.display_name(),
            language: doc.lang_name().map(str::to_string),
            version: doc.version(),
            modified: doc.is_modified(),
        };
        Some((input, Seen { doc: doc.id, version: doc.version(), ranges: sel.ranges().to_vec() }))
    }

    fn plugin_apply(&mut self, plugin: &str, seen: &Seen, actions: Vec<Action>) {
        let edits =
            |a: &Action| matches!(a, Action::ReplaceSelections(_) | Action::Edits(_) | Action::Select(_));
        let mut edited = false;
        for a in &actions {
            let r = match a {
                Action::Status(m) => {
                    self.set_status(shown(m));
                    Ok(())
                }
                Action::Success(m) => {
                    self.set_success(shown(m));
                    Ok(())
                }
                Action::Warning(m) => {
                    self.set_warning(shown(m));
                    Ok(())
                }
                Action::Error(m) => Err(shown(m)),
                Action::StatusBar(m) => {
                    if let Some(p) = &mut self.plugins {
                        let key = (plugin.to_string(), seen.doc);
                        let text: String = shown(m).replace('\n', " ").chars().take(MAX_STATUS).collect();
                        if text.trim().is_empty() {
                            p.status.remove(&key);
                        } else {
                            p.status.insert(key, text);
                        }
                    }
                    Ok(())
                }
                // Movement only: anything else could reach past the buffer (`:sh`, `:w`, …)
                Action::Run(name) => match crate::commands::find(name) {
                    Some(c) if c.motion => {
                        self.run_command_by_name(name);
                        Ok(())
                    }
                    _ => Err(format!("plugins can run movement commands only, not “{}”", shown(name))),
                },
                Action::Open(path) if path.chars().any(char::is_control) => {
                    Err("a path with control characters".into())
                }
                // `path:line` (from 1): there, if `path` alone is the file
                Action::Open(path) => match path.rsplit_once(':') {
                    Some((file, line))
                        if !Path::new(path).exists() && line.parse::<usize>().is_ok_and(|l| l > 0) =>
                    {
                        let line = line.parse::<usize>().expect("checked") - 1;
                        self.run_picker_action(crate::picker::Action::Goto {
                            path: file.into(),
                            line,
                            col: 0,
                        });
                        Ok(())
                    }
                    _ => self.open(Path::new(path)).map_err(|e| format!("{e:#}")),
                },
                Action::Spawn(sp) => self.plugin_spawn(plugin, sp.clone()),
                Action::Stop(id) => {
                    self.plugin_stop(plugin, *id);
                    Ok(())
                }
                Action::ReadFile(id, path) => {
                    match self.plugin_may_read(plugin, path) {
                        Ok(real) => self.plugin_read_now(plugin, *id, real),
                        Err(None) => {
                            let at = perm::expand(path).unwrap_or_default();
                            self.plugin_ask(
                                plugin,
                                perm::Ask::Read(*id, std::fs::canonicalize(&at).unwrap_or(at)),
                            );
                        }
                        Err(Some(why)) => self.plugin_events(plugin, vec![abi::HostEvent::Failed(*id, why)]),
                    }
                    Ok(())
                }
                Action::ReadEnv(id, name) => {
                    let ev = match self.plugin_may(plugin, &abi::Permission::Env(name.clone())) {
                        Ok(()) => abi::HostEvent::Env(*id, std::env::var(name).ok()),
                        Err(why) => abi::HostEvent::Failed(*id, why),
                    };
                    self.plugin_events(plugin, vec![ev]);
                    Ok(())
                }
                Action::Connect(c) => {
                    match self.plugin_may_connect(plugin, c) {
                        Ok(()) => self.plugin_connect_now(plugin, c.clone()),
                        Err(None) => self.plugin_ask(plugin, perm::Ask::Connect(c.clone())),
                        Err(Some(why)) => self.plugin_events(plugin, vec![abi::HostEvent::Failed(c.id, why)]),
                    }
                    Ok(())
                }
                Action::Send(id, bytes) => {
                    self.plugin_send(plugin, *id, bytes.clone());
                    Ok(())
                }
                Action::Timer(id, ms) => {
                    self.plugin_timer(plugin, *id, *ms);
                    Ok(())
                }
                Action::Prompt(id, label) => {
                    self.plugin_prompt(plugin, *id, label);
                    Ok(())
                }
                Action::Show(view) => {
                    self.plugin_show(plugin, view.clone());
                    Ok(())
                }
                Action::Append(id, text) => self.plugin_append(plugin, *id, text),
                Action::Hints(h) => {
                    self.plugin_hints(plugin, h.clone());
                    Ok(())
                }
                Action::Side(s) => {
                    self.plugin_side(plugin, s.clone());
                    Ok(())
                }
                Action::Badge(b) => {
                    self.plugin_badge(plugin, b.clone());
                    Ok(())
                }
                Action::Find(id, pattern) => {
                    self.plugin_find(plugin, *id, pattern);
                    Ok(())
                }
                Action::WriteOwn(id, name, bytes) => {
                    self.plugin_write_own(plugin, *id, name, bytes);
                    Ok(())
                }
                Action::OfferInstall(id, tool) => {
                    self.plugin_offer_install(plugin, *id, tool);
                    Ok(())
                }
                Action::Pick(p) => {
                    self.plugin_pick(plugin, p.clone());
                    Ok(())
                }
                Action::Panel(panel) => {
                    self.plugin_panel(plugin, panel.clone());
                    Ok(())
                }
                // All of the answer's edits, where the first of them is
                _ if edited => Ok(()),
                _ => {
                    edited = true;
                    self.plugin_edit(seen, actions.iter().filter(|a| edits(a)))
                }
            };
            if let Err(e) = r {
                self.set_error(format!("{plugin}: {e}"));
            }
        }
    }

    /// One answer's edits as one undo step, if the buffer is as the plugin saw it. The buffer in front stays
    /// in front.
    fn plugin_edit<'a>(
        &mut self,
        seen: &Seen,
        actions: impl Iterator<Item = &'a Action>,
    ) -> Result<(), String> {
        let i = self.docs.iter().position(|d| d.id == seen.doc).ok_or("the buffer was closed")?;
        if self.docs[i].version() != seen.version {
            return Err("the buffer changed while the plugin ran — its edits weren't applied".into());
        }
        if self.editing(seen.doc) || self.docs[i].loading {
            return Err("the buffer is being edited — the plugin's edits weren't applied".into());
        }
        let doc = &self.docs[i];
        let on_char = |text: &ropey::Rope, b: u64| {
            b <= text.len_bytes() as u64 && text.char_to_byte(text.byte_to_char(b as usize)) == b as usize
        };
        let mut changes = Vec::new();
        let mut select = None;
        for a in actions {
            match a {
                // The selections the plugin was given, whatever they are now
                Action::ReplaceSelections(texts) => {
                    if texts.len() != seen.ranges.len() {
                        return Err(format!("{} texts for {} selections", texts.len(), seen.ranges.len()));
                    }
                    changes.extend(seen.ranges.iter().zip(texts).map(|(r, t)| Change {
                        from: r.from(),
                        to: r.to(),
                        insert: t.clone(),
                    }));
                }
                Action::Edits(es) => {
                    for e in es {
                        if e.from > e.to || !on_char(&doc.text, e.from) || !on_char(&doc.text, e.to) {
                            return Err(format!(
                                "edit {}..{} isn't within the text, on character boundaries",
                                e.from, e.to
                            ));
                        }
                        changes.push(Change {
                            from: e.from as usize,
                            to: e.to as usize,
                            insert: e.insert.clone(),
                        });
                    }
                }
                Action::Select(ranges) if ranges.is_empty() => return Err("an empty selection list".into()),
                Action::Select(ranges) => select = Some(ranges.clone()),
                _ => {}
            }
        }
        changes.sort_by_key(|c| (c.from, c.to));
        if changes.windows(2).any(|w| w[1].from < w[0].to) {
            return Err("its edits overlap".into());
        }
        let tx = Transaction::new(changes);
        // Selections are positions in the buffer after the edits: checked there before anything changes
        let selection = match select {
            None => None,
            Some(ranges) => {
                let mut after = doc.text.clone();
                tx.apply(&mut after, false);
                if let Some(&(a, h)) =
                    ranges.iter().find(|&&(a, h)| !on_char(&after, a) || !on_char(&after, h))
                {
                    return Err(format!("selection {a}..{h} isn't on character boundaries"));
                }
                Some(Selection::new(
                    ranges.iter().map(|&(a, h)| Range::new(a as usize, h as usize)).collect(),
                    0,
                ))
            }
        };
        let doc = &mut self.docs[i];
        let before = doc.snapshot();
        match selection {
            Some(sel) => doc.apply_with(&tx, sel),
            None => doc.apply(&tx),
        }
        doc.commit_undo(before);
        // A plugin's own edit isn't reported to `changed` subscribers (no plugin and edit calling each other)
        let (id, v) = (doc.id, doc.version());
        if let Some(p) = &mut self.plugins {
            p.events.by_plugin.insert(id, v);
        }
        Ok(())
    }
}

/// What a command's answer applies to: the buffer and selections as they were when it was asked.
#[derive(Clone)]
struct Seen {
    doc: DocId,
    version: u64,
    ranges: Vec<Range>,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::abi::tests::DEMO;
    use super::*;
    use crate::config::Config;

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(ed) {
            let left = end.saturating_duration_since(Instant::now());
            let texts: Vec<String> = ed.docs.iter().map(|d| d.text.to_string()).collect();
            let ev = ed
                .events
                .recv_timeout(left)
                .unwrap_or_else(|| panic!("timed out on {texts:?}: {}", toasts(ed)));
            ed.handle_event(ev);
        }
    }

    fn toasts(ed: &Editor) -> String {
        ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ")
    }

    /// An editor with the demo plugin loaded from its own folder, on `text`.
    fn setup(name: &str, text: &str) -> (Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-plugins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.wasm"), DEMO).unwrap();
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = ropey::Rope::from_str(text);
        ed.plugins_sync(host::scan(&dir));
        settle_until(&mut ed, |ed| ed.plugin_command("shout").is_some());
        (ed, dir)
    }

    fn key(ed: &mut Editor, k: &str) {
        ed.handle_key(k.parse().unwrap());
    }

    /// A message starting `what` about a file named `name`.
    fn said(ed: &Editor, what: &str, name: &str) -> bool {
        ed.toasts.iter().any(|t| t.text.starts_with(what) && t.text.contains(name))
    }

    /// A key the way the terminal sends it (through the event loop, which is what `changed` watches).
    fn press(ed: &mut Editor, k: &str) {
        ed.handle_event(crate::event::Event::Key(k.parse().unwrap()));
    }

    /// Runs the event loop for a while.
    fn settle_for(ed: &mut Editor, d: Duration) {
        let end = Instant::now() + d;
        while let Some(ev) = ed.events.recv_timeout(end.saturating_duration_since(Instant::now())) {
            ed.handle_event(ev);
        }
    }

    fn disk(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn until_said(ed: &mut Editor, what: &str) {
        settle_until(ed, |ed| toasts(ed).contains(what));
    }

    /// `y` on the front card, if it's what `is` says.
    fn answer(ed: &mut Editor, is: impl Fn(&crate::offer::What) -> bool, yes: bool) {
        let front = ed.offers.first().map(|o| &o.what);
        assert!(
            front.is_some_and(&is),
            "the card: {:?}",
            ed.offers.iter().map(|o| &o.title).collect::<Vec<_>>()
        );
        if yes { ed.offer_accept() } else { ed.offer_dismiss() }
    }

    fn approval(w: &crate::offer::What) -> bool {
        matches!(w, crate::offer::What::PluginPermissions { plugin, .. } if plugin == "demo")
    }

    fn ask(w: &crate::offer::What) -> bool {
        matches!(w, crate::offer::What::PluginAsk { .. })
    }

    #[test]
    fn permissions_wait_for_approval_and_hold_to_what_was_asked() {
        let (mut ed, dir) = setup("perm", "x\n");
        // Held: the card asks as it loads, and what needs a permission fails meanwhile
        ed.plugin_run("exec", "").unwrap();
        until_said(&mut ed, "failed 1 its permissions wait for your approval");
        answer(&mut ed, approval, true);
        ed.plugin_run("exec", "").unwrap();
        // (its output and its end go to the plugin undrawn; its answers, which say them, drawn)
        let flags = quiet_until(&mut ed, "exit 1 Some(0) []");
        assert!(flags.contains(&true) && flags.last() == Some(&false), "{flags:?}");
        // A variable set for it: only one it may read
        ed.toasts.clear();
        ed.plugin_run("exec-with", "TARAE_DEMO=1").unwrap();
        until_said(&mut ed, "exit 1 Some(0) []");
        ed.plugin_run("exec-with", "DYLD_INSERT_LIBRARIES=/tmp/x.dylib").unwrap();
        until_said(&mut ed, "failed 1 it may set only variables it may read, not DYLD_INSERT_LIBRARIES");
        // Only what it asked for
        ed.plugin_run("getenv", "PATH").unwrap();
        until_said(&mut ed, "failed 11 it didn't ask to read the environment variable PATH");
        ed.plugin_run("getenv", "TARAE_DEMO").unwrap();
        until_said(&mut ed, "env 11 None");
        // Taken back: held again, and asked again
        ed.plugin_revoke("demo").unwrap();
        ed.toasts.clear();
        ed.plugin_run("exec", "").unwrap();
        until_said(&mut ed, "failed 1 its permissions wait for your approval (:plugin-approve demo)");
        // … but the card isn't pushed at you again by the plugin; you bring it back
        assert!(!ed.offers.iter().any(|o| approval(&o.what)));
        ed.plugin_approve("demo").unwrap();
        assert!(ed.offers.first().is_some_and(|o| approval(&o.what)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn files_in_its_patterns_are_read_others_are_asked_about() {
        let base = PathBuf::from("/tmp/tarae-demo").join(std::process::id().to_string());
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.txt"), "hello").unwrap();
        let outside = std::env::temp_dir().join(format!("tarae-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, "secret").unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let (mut ed, dir) = setup("files", "x\n");
        answer(&mut ed, approval, true);
        ed.plugin_run("fetch", &base.join("a.txt").display().to_string()).unwrap();
        until_said(&mut ed, "file 10 5");
        // Out by `..`, or by a symlink: asked about, not read
        for sneaky in [format!("{}/../../../etc/hosts", base.display()), link.display().to_string()] {
            ed.toasts.clear();
            ed.plugin_run("fetch", &sneaky).unwrap();
            settle_until(&mut ed, |ed| ed.offers.first().is_some_and(|o| ask(&o.what)));
            answer(&mut ed, ask, false);
            until_said(&mut ed, "failed 10 you said no");
        }
        // Allowed once: kept
        ed.plugin_run("fetch", &outside.display().to_string()).unwrap();
        settle_until(&mut ed, |ed| ed.offers.first().is_some_and(|o| ask(&o.what)));
        answer(&mut ed, ask, true);
        until_said(&mut ed, "file 10 6");
        ed.toasts.clear();
        ed.plugin_run("fetch", &outside.display().to_string()).unwrap();
        until_said(&mut ed, "file 10 6");
        assert!(ed.offers.is_empty(), "not asked twice");
        // A relative path isn't one
        ed.plugin_run("fetch", "a.txt").unwrap();
        until_said(&mut ed, "an absolute path, or one from ~/");
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn connections_are_asked_about_then_carry_bytes_both_ways() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = [0u8; 4];
            s.read_exact(&mut got).unwrap();
            s.write_all(b"pong").unwrap();
            got
        });
        let (mut ed, dir) = setup("socket", "x\n");
        answer(&mut ed, approval, true);
        ed.plugin_run("dial", &port.to_string()).unwrap();
        // A new host:port: asked about
        settle_until(&mut ed, |ed| ed.offers.first().is_some_and(|o| ask(&o.what)));
        // Its panel shown meanwhile doesn't take the keys: they're the card's
        ed.plugin_run("table", "").unwrap();
        settle_until(&mut ed, |ed| panel(ed).is_some());
        assert!(!panel(&ed).unwrap().focus);
        answer(&mut ed, ask, true);
        until_said(&mut ed, "connected 20");
        // (the bytes and the close go to the plugin undrawn; its answers, which say them, drawn)
        let flags = quiet_until(&mut ed, r#"closed 20 ""#);
        assert!(toasts(&ed).contains("data 20 [112, 111, 110, 103]"), "{}", toasts(&ed));
        assert!(flags.contains(&true) && flags.last() == Some(&false), "{flags:?}");
        assert_eq!(&server.join().unwrap(), b"ping", "what the plugin sent arrived");
        assert!(ed.plugins.as_ref().unwrap().socks.is_empty(), "closed: gone");
        // The same host:port again: not asked
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let hold = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        ed.toasts.clear();
        ed.plugin_run("dial", &port.to_string()).unwrap();
        until_said(&mut ed, "connected 20");
        assert!(ed.offers.is_empty());
        // Hung up: closed, and nothing more comes from it
        ed.plugin_run("hang-up", "").unwrap();
        settle_until(&mut ed, |ed| ed.plugins.as_ref().unwrap().socks.is_empty());
        let _ = hold.join();
        // Nobody there: failed
        ed.plugin_run("dial", "1").unwrap();
        settle_until(&mut ed, |ed| ed.offers.first().is_some_and(|o| ask(&o.what)));
        answer(&mut ed, ask, true);
        until_said(&mut ed, "failed 20 127.0.0.1:1");
        std::fs::remove_dir_all(dir).ok();
    }

    /// `shown` goes out when the buffer in front changes — switching buffers, opening one — and not again
    /// while it stays.
    #[test]
    fn the_buffer_in_front_is_shown() {
        let (mut ed, dir) = setup("shown", "x\n");
        let (a, b) = (dir.join("a.front"), dir.join("b.front"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&b, "b\n").unwrap();
        // (each says the file and when — each its own message: the file names, in order)
        let shown = |ed: &Editor| {
            let named = |t: &String| {
                t.rsplit(' ').nth(1).and_then(|p| p.rsplit('/').next()).unwrap_or("").to_string()
            };
            ed.toasts
                .iter()
                .map(|t| &t.text)
                .filter(|t| t.starts_with("shown "))
                .map(named)
                .collect::<Vec<_>>()
        };
        ed.open(&a).unwrap();
        settle_until(&mut ed, |ed| shown(ed) == ["a.front"]);
        ed.open(&b).unwrap();
        settle_until(&mut ed, |ed| shown(ed) == ["a.front", "b.front"]);
        key(&mut ed, "j");
        settle_for(&mut ed, Duration::from_millis(200));
        assert_eq!(shown(&ed), ["a.front", "b.front"], "the same one in front: not again");
        // (back to one already open)
        ed.cycle_buffer(-1);
        ed.handle_event(crate::event::Event::Resize);
        settle_until(&mut ed, |ed| shown(ed) == ["a.front", "b.front", "a.front"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn timers_tick_unless_stopped() {
        let (mut ed, dir) = setup("timer", "x\n");
        ed.plugin_run("later", "10").unwrap();
        until_said(&mut ed, "tick 30");
        ed.toasts.clear();
        ed.plugin_run("later", "300").unwrap();
        ed.plugin_run("never", "").unwrap();
        settle_for(&mut ed, Duration::from_millis(600));
        assert!(!toasts(&ed).contains("tick"), "{}", toasts(&ed));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Each event handled as the main loop does: whether it said it changed nothing (then it isn't drawn).
    fn quiet_flags(ed: &mut Editor, d: Duration) -> Vec<bool> {
        let end = Instant::now() + d;
        let mut out = Vec::new();
        while let Some(ev) = ed.events.recv_timeout(end.saturating_duration_since(Instant::now())) {
            ed.unchanged = false;
            ed.handle_event(ev);
            out.push(std::mem::take(&mut ed.unchanged));
        }
        out
    }

    /// `quiet_flags` until a message says `what`.
    fn quiet_until(ed: &mut Editor, what: &str) -> Vec<bool> {
        let end = Instant::now() + Duration::from_secs(20);
        let mut out = Vec::new();
        while !toasts(ed).contains(what) {
            let ev = ed.events.recv_timeout(end.saturating_duration_since(Instant::now()));
            let ev = ev.unwrap_or_else(|| panic!("timed out: {}", toasts(ed)));
            ed.unchanged = false;
            ed.handle_event(ev);
            out.push(std::mem::take(&mut ed.unchanged));
        }
        out
    }

    /// A watch's every change comes and goes without a frame: what's only for the plugin (a tick, a
    /// connection's bytes) and an answer of only I/O aren't drawn — an answer that says something is.
    #[test]
    fn what_changes_nothing_on_screen_isnt_drawn() {
        let (mut ed, dir) = setup("quiet", "x\n");
        // (what loading sent it answered first — its buffer shown — and quietly)
        assert!(quiet_flags(&mut ed, Duration::from_millis(200)).iter().all(|q| *q));
        ed.plugin_run("quiet", "").unwrap();
        let flags = quiet_flags(&mut ed, Duration::from_millis(400));
        // (the command's answer; then the tick, and the plugin's answer to it: `stop` only)
        assert_eq!(flags.len(), 3, "{flags:?}");
        assert_eq!(&flags[1..], [true, true]);
        ed.plugin_run("later", "10").unwrap();
        let flags = quiet_flags(&mut ed, Duration::from_millis(400));
        assert!(toasts(&ed).contains("tick 30"), "{}", toasts(&ed));
        assert_eq!(flags.last(), Some(&false), "an answer that says something is drawn: {flags:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    fn panel(ed: &Editor) -> Option<&panel::PanelState> {
        ed.plugins.as_ref()?.panel.as_ref()
    }

    #[test]
    fn a_panel_takes_the_keys_until_esc() {
        let (mut ed, dir) = setup("panel", "x\n");
        answer(&mut ed, approval, true);
        ed.plugin_run("table", "").unwrap();
        settle_until(&mut ed, |ed| panel(ed).is_some());
        assert!(panel(&ed).unwrap().focus, "shown by a command: it has the keys");
        // tarae moves through the rows
        key(&mut ed, "j");
        key(&mut ed, "j");
        assert_eq!(panel(&ed).unwrap().selected, 2);
        key(&mut ed, "G");
        assert_eq!(panel(&ed).unwrap().selected, 29);
        key(&mut ed, "k");
        // …the plugin gets the rest, with the row
        key(&mut ed, "d");
        until_said(&mut ed, r#"key d Some(28) Some("default/web-28")"#);
        key(&mut ed, "C-k");
        until_said(&mut ed, "key C-k Some(28)");
        // The editor's still: space, `:`
        key(&mut ed, ":");
        assert!(ed.prompt.is_some(), "the command line opened");
        key(&mut ed, "esc");
        // esc: the keys go back to the code, the panel stays
        key(&mut ed, "esc");
        assert!(!panel(&ed).unwrap().focus);
        ed.toasts.clear();
        key(&mut ed, "d");
        settle_for(&mut ed, Duration::from_millis(100));
        assert!(!toasts(&ed).contains("key d"), "{}", toasts(&ed));
        // A click on a row: the keys, and that row
        ed.plugin_panel_click(Some(5));
        assert_eq!((panel(&ed).unwrap().focus, panel(&ed).unwrap().selected), (true, 5));
        // Rows that moved: the selection stays on its row (by id)
        let mut moved = panel(&ed).unwrap().panel.clone();
        moved.rows.rotate_left(3);
        moved.ids.rotate_left(3);
        ed.plugin_panel("demo", Some(moved));
        let st = panel(&ed).unwrap();
        assert_eq!((st.selected, st.panel.ids[st.selected].as_str()), (2, "default/web-5"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_plugins_line_answers_what_was_typed() {
        let (mut ed, dir) = setup("prompt", "x\n");
        ed.plugin_run("question", "").unwrap();
        settle_until(&mut ed, |ed| ed.prompt.is_some());
        assert_eq!(ed.prompt.as_ref().unwrap().label.as_deref(), Some("demo · filter:"), "it says whose");
        // Not over a line being typed
        ed.plugin_run("question", "").unwrap();
        until_said(&mut ed, "failed 6 you're typing something else");
        for k in ["w", "e", "b", "ret"] {
            key(&mut ed, k);
        }
        until_said(&mut ed, r#"answer 6 Some("web")"#);
        ed.plugin_run("question", "").unwrap();
        settle_until(&mut ed, |ed| ed.prompt.is_some());
        key(&mut ed, "esc");
        until_said(&mut ed, "answer 6 None");
        // Replaced by another line: it's told nothing was typed
        ed.plugin_run("question", "").unwrap();
        settle_until(&mut ed, |ed| ed.prompt.is_some());
        ed.toasts.clear();
        ed.open_prompt(crate::editor::PromptKind::Command, "");
        until_said(&mut ed, "answer 6 None");
        ed.prompt = None;
        // Only in answer to you: not on its own a moment later, but after a key in its panel, yes
        ed.plugin_run("nag", "").unwrap();
        until_said(&mut ed, "failed 6 a line is asked for only in answer to you");
        assert!(ed.prompt.is_none());
        ed.plugin_run("table", "").unwrap();
        settle_until(&mut ed, |ed| panel(ed).is_some());
        key(&mut ed, "/");
        settle_until(&mut ed, |ed| ed.prompt.is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_plugins_buffer_is_shown_replaced_and_grown() {
        let (mut ed, dir) = setup("view", "x\n");
        let title = "web-1 · demo";
        let text = |ed: &Editor| {
            ed.docs
                .iter()
                .filter(|d| d.title.as_deref() == Some(title))
                .map(|d| d.text.to_string())
                .collect::<Vec<_>>()
        };
        ed.plugin_run("view", "").unwrap();
        settle_until(&mut ed, |ed| !text(ed).is_empty());
        // (a panel it shows along with it doesn't take the keys back from the buffer)
        ed.plugin_run("view-and-panel", "").unwrap();
        settle_until(&mut ed, |ed| panel(ed).is_some());
        assert!(!panel(&ed).unwrap().focus);
        assert_eq!(text(&ed), ["kind: Pod\n"]);
        assert_eq!(ed.doc().title.as_deref(), Some(title), "in front");
        ed.plugin_run("more", "").unwrap();
        settle_until(&mut ed, |ed| text(ed) == ["kind: Pod\nstatus: Running\n"]);
        ed.plugin_run("view", "").unwrap();
        settle_until(&mut ed, |ed| text(ed) == ["kind: Pod\n"]);
        assert_eq!(text(&ed).len(), 1, "one buffer per id");
        // Closed by you: appending fails, and it stays closed
        let id = ed.doc().id;
        ed.close_doc(id);
        until_said(&mut ed, "failed 7 its buffer is closed");
        ed.toasts.clear();
        ed.plugin_run("more", "").unwrap();
        until_said(&mut ed, "failed 7 its buffer is closed");
        assert!(text(&ed).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn approvals_are_kept() {
        let (mut ed, dir) = setup("kept", "x\n");
        let store = dir.join("state/plugin-permissions.json");
        ed.plugins.as_mut().unwrap().perm_store = Some(store.clone());
        answer(&mut ed, approval, true);
        let kept = perm::load(Some(&store));
        assert_eq!(
            kept["demo"].granted,
            ["env TARAE_DEMO", "network", "project", "read /tmp/tarae-demo/**", "run true"]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn commands_run_and_their_edits_are_one_undo_step() {
        let (mut ed, dir) = setup("run", "hello world\n");
        assert_eq!(ed.plugin_command("shout").map(|c| c.plugin.as_str()), Some("demo"));
        assert!(ed.cmd_lists.plugins.iter().any(|(n, _)| n == "shout"), "in `:` completion");
        key(&mut ed, "e"); // select "hello"
        crate::typed::execute(&mut ed, "shout").unwrap();
        settle_until(&mut ed, |ed| ed.doc().text != "hello world\n");
        assert_eq!(ed.doc().text.to_string(), "HELLO world\n");
        key(&mut ed, "u");
        assert_eq!(ed.doc().text.to_string(), "hello world\n", "one undo step");
        // Only commands registered with `text: true` see the buffer
        crate::typed::execute(&mut ed, "echo x").unwrap();
        settle_until(&mut ed, |ed| toasts(ed).contains("args="));
        assert!(toasts(&ed).contains(r#"args="x""#) && toasts(&ed).contains("text=false"), "{}", toasts(&ed));
        crate::typed::execute(&mut ed, "grab o").unwrap();
        settle_until(&mut ed, |ed| ed.doc().selection().ranges().len() == 2);
        assert_eq!(ed.doc().selection().ranges(), [Range::new(4, 5), Range::new(7, 8)]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn edits_are_refused_when_the_buffer_changed_meanwhile() {
        let (mut ed, dir) = setup("stale", "a  \nb \n");
        crate::typed::execute(&mut ed, "trim-trailing").unwrap();
        // Typing before the answer arrives
        key(&mut ed, "i");
        key(&mut ed, "x");
        key(&mut ed, "esc");
        settle_until(&mut ed, |ed| toasts(ed).contains("changed while"));
        assert_eq!(ed.doc().text.to_string(), "xa  \nb \n");
        // Unchanged: applied
        crate::typed::execute(&mut ed, "trim-trailing").unwrap();
        settle_until(&mut ed, |ed| ed.doc().text == "xa\nb\n");
        assert!(toasts(&ed).contains("2 lines trimmed"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn failures_and_bad_answers_are_reported() {
        let (mut ed, dir) = setup("fail", "text\n");
        for (cmd, says) in [
            ("boom", "demo: crashed (a panic?)"),
            ("stray", "demo: edit 1099511627776..1099511627776 isn't within the text"),
            ("send nonsense", "demo: plugins can run movement commands only, not “nonsense”"),
            ("notify hi", "demo: error hi"),
            ("spin", "demo: ran too long"),
        ] {
            crate::typed::execute(&mut ed, cmd).unwrap();
            settle_until(&mut ed, |ed| toasts(ed).contains(says));
        }
        assert!(toasts(&ed).contains("warning hi"));
        assert_eq!(ed.doc().text.to_string(), "text\n");
        // After all that, it still works
        key(&mut ed, "e");
        crate::typed::execute(&mut ed, "shout").unwrap();
        settle_until(&mut ed, |ed| ed.doc().text == "TEXT\n");
        std::fs::remove_dir_all(dir).ok();
    }

    /// An answer lands on the buffer and selections the plugin saw, leaves the buffer in front in front,
    /// and never lands in the middle of typing.
    #[test]
    fn answers_land_where_they_were_asked() {
        let (mut ed, dir) = setup("where", "hello world\n");
        // The selection moves on before the answer: the plugin's selections are the ones replaced
        key(&mut ed, "e");
        crate::typed::execute(&mut ed, "shout").unwrap();
        key(&mut ed, "w");
        settle_until(&mut ed, |ed| ed.doc().text != "hello world\n");
        assert_eq!(ed.doc().text.to_string(), "HELLO world\n");
        // Another buffer comes to the front: it stays there, and typing goes on in it
        key(&mut ed, "u");
        ed.doc_mut().set_selection(Selection::new(vec![Range::new(0, 5)], 0));
        let first = ed.doc().id;
        crate::typed::execute(&mut ed, "shout").unwrap();
        crate::typed::execute(&mut ed, "new").unwrap();
        key(&mut ed, "i");
        key(&mut ed, "z");
        settle_until(&mut ed, |ed| ed.docs.iter().any(|d| d.id == first && d.text == "HELLO world\n"));
        key(&mut ed, "q");
        assert_eq!(ed.doc().text.to_string(), "zq");
        key(&mut ed, "esc");
        // Typing in the buffer itself (insert mode, nothing typed yet): refused, not mixed into the typing
        crate::typed::execute(&mut ed, "bc!").unwrap();
        assert_eq!(ed.doc().id, first);
        crate::typed::execute(&mut ed, "shout").unwrap();
        key(&mut ed, "i");
        settle_until(&mut ed, |ed| toasts(ed).contains("being edited"));
        assert_eq!(ed.doc().text.to_string(), "HELLO world\n");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn opening_a_file_at_a_line() {
        let (mut ed, dir) = setup("goto", "");
        let path = dir.join("lines.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        ed.plugin_run("visit", &format!("{}:3", path.display())).unwrap();
        settle_until(&mut ed, |ed| ed.doc().path.as_deref().is_some_and(|p| p.ends_with("lines.txt")));
        let d = ed.doc();
        assert_eq!(d.text.char_to_line(d.selection().primary().cursor(&d.text)), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hints_beside_lines_until_edited_a_card_and_a_pick() {
        let (mut ed, dir) = setup("beside", "");
        answer(&mut ed, approval, true);
        // (not .yaml: no language server started by a test)
        let path = dir.join("web.txt");
        std::fs::write(&path, "kind: Deployment\nreplicas: 3\n").unwrap();
        ed.open(&path).unwrap();
        let hints = |ed: &Editor| {
            ed.plugin_hints_for(ed.doc()).map(|(l, t, _)| (l, t.to_string())).collect::<Vec<_>>()
        };
        ed.plugin_run("hint", "").unwrap();
        settle_until(&mut ed, |ed| !hints(ed).is_empty());
        assert_eq!(hints(&ed), [(0, "all good".to_string()), (1, "2/3 ready".to_string())]);
        let mut frame = Vec::new();
        crate::term::render(&mut ed, &mut frame, 120, 20).unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("2/3 ready"), "beside the line");
        // Edited: they wait for the plugin's next ones (the lines may have moved)
        for k in ["i", "x", "esc"] {
            key(&mut ed, k);
        }
        assert!(hints(&ed).is_empty());
        // A pick: enter chooses, esc chooses nothing
        ed.plugin_run("choose", "").unwrap();
        settle_until(&mut ed, |ed| ed.picker.is_some());
        assert_eq!(ed.picker.as_ref().unwrap().title, "demo · choose");
        key(&mut ed, "ret");
        until_said(&mut ed, r#"answer 41 Some("x")"#);
        ed.plugin_run("choose", "").unwrap();
        settle_until(&mut ed, |ed| ed.picker.is_some());
        key(&mut ed, "esc");
        until_said(&mut ed, "answer 41 None");
        ed.reopen_last_picker();
        assert!(ed.picker.is_none(), "not kept for space '");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_tab_in_the_side_panel_and_a_badge() {
        use crate::sidebar::Mode as SMode;
        let (mut ed, dir) = setup("side", "");
        answer(&mut ed, approval, true);
        let path = dir.join("web.txt");
        std::fs::write(&path, "kind: Deployment\nreplicas: 3\n").unwrap();
        ed.open(&path).unwrap();
        ed.plugin_run("side", "").unwrap();
        settle_until(&mut ed, |ed| ed.sidebar.as_ref().is_some_and(|s| s.mode == SMode::Plugin));
        let s = ed.sidebar.as_ref().unwrap();
        assert!(!s.focus, "shown, without the keys");

        let labels = |ed: &Editor| {
            ed.sidebar.as_ref().unwrap().lines().iter().map(|l| l.label.clone()).collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&ed),
            ["web.txt", "payment-gateway-reconciler-canary", "web-1", "CrashLoopBackOff"]
        );
        assert_eq!(s.here(&ed), Some(1), "the row for the code under the cursor: the deepest");
        let mut frame = Vec::new();
        crate::term::render(&mut ed, &mut frame, 140, 20).unwrap();
        let frame = String::from_utf8_lossy(&frame).into_owned();
        assert!(frame.contains("demo") && frame.contains("⎈ test"), "its tab, and the badge");
        assert!(frame.contains("deploy detail"), "a long name is cut, its state isn't");
        // Asked to: with the keys, on the row for the code under the cursor — its name cut short, whole
        // (and its state after it) over the code
        ed.plugin_run("side", "focus").unwrap();
        settle_until(&mut ed, |ed| ed.sidebar.as_ref().is_some_and(|s| s.focus));
        assert_eq!(ed.sidebar.as_ref().unwrap().selected, 1);
        let mut frame = Vec::new();
        crate::term::render(&mut ed, &mut frame, 140, 20).unwrap();
        let frame = String::from_utf8_lossy(&frame).into_owned();
        let whole = frame.find("payment-gateway-reconciler-canary").expect("its whole name");
        assert!(frame[whole..].contains("deploy detail"), "and its state");
        // h folds, l unfolds, enter on a row with no place and other keys go to the plugin
        ed.sidebar.as_mut().unwrap().selected = 0;
        key(&mut ed, "h");
        assert_eq!(labels(&ed), ["web.txt …"], "folded, and saying so");
        key(&mut ed, "l");
        assert_eq!(labels(&ed).len(), 4);
        // (a note isn't a row to land on)
        ed.sidebar.as_mut().unwrap().selected = 2;
        key(&mut ed, "j");
        assert_eq!(ed.sidebar.as_ref().unwrap().selected, 2);
        ed.sidebar.as_mut().unwrap().selected = 2;
        key(&mut ed, "ret");
        until_said(&mut ed, r#"key ret Some(2) Some("pod")"#);
        key(&mut ed, "e");
        until_said(&mut ed, r#"key e Some(2) Some("pod")"#);
        // A buffer it shows then takes the keys from its tab
        ed.plugin_run("view", "").unwrap();
        settle_until(&mut ed, |ed| ed.doc().title.as_deref() == Some("web-1 · demo"));
        assert!(!ed.sidebar.as_ref().unwrap().focus);
        ed.sidebar.as_mut().unwrap().focus = true;
        // Enter on a row with a place: there, in the code
        ed.sidebar.as_mut().unwrap().selected = 1;
        key(&mut ed, "ret");
        assert!(!ed.sidebar.as_ref().unwrap().focus, "the code has the keys");
        ed.plugin_run("unside", "").unwrap();
        settle_until(&mut ed, |ed| {
            ed.plugins.as_ref().is_some_and(|p| p.side.is_none() && p.badge.is_none())
        });
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_project_s_files_found_and_read_its_own_files_written_tools_offered() {
        let (mut ed, dir) = setup("project", "");
        answer(&mut ed, approval, true);
        // The project is the folder tarae started in: its files found by pattern (what git ignores, not)
        ed.plugin_run("find", "**/Cargo.toml").unwrap();
        until_said(&mut ed, r#"found 50 ["Cargo.toml""#);
        ed.plugin_run("find", "../**").unwrap();
        until_said(&mut ed, "failed 50 “../**”: a pattern from the project's folder");
        // …and read without being asked; a file outside it is still asked about
        let manifest = std::env::current_dir().unwrap().join("Cargo.toml");
        ed.plugin_run("fetch", &manifest.display().to_string()).unwrap();
        until_said(&mut ed, "file 10 ");
        // Its own folder: written, nowhere else
        ed.plugins.as_mut().unwrap().own_root = Some(dir.join("own"));
        ed.plugin_run("keep", "").unwrap();
        until_said(&mut ed, "written 51 ");
        assert_eq!(std::fs::read_to_string(dir.join("own/demo/notes/a.txt")).unwrap(), "hello");
        // A tool there already: installed at once; one no pack knows: said so
        ed.plugin_run("install", "sh").unwrap();
        until_said(&mut ed, "installed 52");
        ed.plugin_run("install", "kustomize-not-a-tool").unwrap();
        until_said(&mut ed, "failed 52 no language pack says how to install kustomize-not-a-tool");
        // One a pack knows (and not here): a card, the plugin's — once a session
        if !crate::packs::on_path("kustomize") && crate::packs::installed_at("kustomize").is_none() {
            ed.offer_popups = true;
            ed.plugin_run("install", "kustomize").unwrap();
            settle_until(&mut ed, |ed| !ed.offers.is_empty());
            let is_ours = |w: &crate::offer::What| matches!(w, crate::offer::What::Install { tool, then: crate::install::Then::Plugin { id: 52, .. }, .. } if tool == "kustomize");
            assert!(ed.offers.iter().any(|o| is_ours(&o.what)));
            ed.plugin_run("install", "kustomize").unwrap();
            until_said(&mut ed, "failed 52 kustomize was offered already");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn subscriptions_for_some_languages_hear_only_of_those() {
        let (mut ed, dir) = setup("langs", "");
        for s in &mut ed.plugins.as_mut().unwrap().events.subs {
            s.2 = vec!["rust".into()];
        }
        let (txt, rs) = (dir.join("notes.txt"), dir.join("main.rs"));
        std::fs::write(&txt, "x\n").unwrap();
        std::fs::write(&rs, "fn main() {}\n").unwrap();
        ed.open(&txt).unwrap();
        ed.open(&rs).unwrap();
        settle_until(&mut ed, |ed| said(ed, "opened ", "main.rs"));
        settle_for(&mut ed, Duration::from_millis(100));
        assert!(!said(&ed, "opened ", "notes.txt"), "not a Rust buffer");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Opened, changed (once input stops), saved and closed reach the demo, which answers with messages and
    /// status-line text — shown for its own buffer only.
    #[test]
    fn events_reach_their_subscribers() {
        let (mut ed, dir) = setup("events", "");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "one two\n").unwrap();
        ed.open(&path).unwrap();
        // (the buffer's name is its path here: it's outside the working folder)
        settle_until(&mut ed, |ed| said(ed, "opened ", "notes.txt"));
        assert!(ed.plugin_status().is_empty(), "nothing changed yet");
        for k in ["i", "x", "space", "esc"] {
            press(&mut ed, k);
        }
        settle_until(&mut ed, |ed| ed.plugin_status() == ["3 words"]);
        let mut frame = Vec::new();
        crate::term::render(&mut ed, &mut frame, 120, 20).unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("3 words"), "in the status line");
        crate::typed::execute(&mut ed, "w").unwrap();
        settle_until(&mut ed, |ed| ed.plugin_status().first().is_some_and(|s| s.starts_with("saved v")));
        assert_eq!(disk(&path), "x one two\n");
        crate::typed::execute(&mut ed, "new").unwrap();
        assert!(ed.plugin_status().is_empty(), "another buffer: its own status");
        crate::typed::execute(&mut ed, "bp").unwrap();
        crate::typed::execute(&mut ed, "bc").unwrap();
        settle_until(&mut ed, |ed| said(ed, "closed ", "notes.txt"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// The demo adds a `!` to `.loop` files on every change: its own edit isn't reported back to it, so there's
    /// one `!`, not one after another for ever.
    #[test]
    fn a_plugins_own_edit_is_not_reported_back() {
        let (mut ed, dir) = setup("loop", "");
        let path = dir.join("a.loop");
        std::fs::write(&path, "ab").unwrap();
        ed.open(&path).unwrap();
        for k in ["i", "x", "esc"] {
            press(&mut ed, k);
        }
        settle_until(&mut ed, |ed| ed.doc().text.to_string().ends_with('!'));
        settle_for(&mut ed, Duration::from_millis(1000));
        assert_eq!(ed.doc().text.to_string(), "xab!");
        // A person's next edit is reported again
        for k in ["i", "y", "esc"] {
            press(&mut ed, k);
        }
        settle_until(&mut ed, |ed| ed.doc().text.to_string().matches('!').count() == 2);
        std::fs::remove_dir_all(dir).ok();
    }

    /// `before-save` edits (the demo trims `.trim` files) are in the file written, and the buffer is saved.
    #[test]
    fn before_save_edits_go_into_the_file() {
        let (mut ed, dir) = setup("presave", "");
        let path = dir.join("a.trim");
        std::fs::write(&path, "x  \ny \n").unwrap();
        ed.open(&path).unwrap();
        crate::typed::execute(&mut ed, "w").unwrap();
        settle_until(&mut ed, |ed| said(ed, "Saved ", "a.trim"));
        assert_eq!(disk(&path), "x\ny\n");
        assert!(!ed.doc().is_modified());
        key(&mut ed, "u");
        assert_eq!(ed.doc().text.to_string(), "x  \ny \n", "the plugin's edit is one undo step");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A plugin too slow before saving: the file is saved as it is when the wait runs out, and its late answer
    /// is dropped.
    #[test]
    fn a_slow_before_save_doesnt_hold_saving() {
        let (mut ed, dir) = setup("slowsave", "");
        ed.plugins.as_mut().unwrap().events.wait = Duration::from_millis(50);
        let path = dir.join("a.slow");
        std::fs::write(&path, "hi\n").unwrap();
        ed.open(&path).unwrap();
        key(&mut ed, "i");
        key(&mut ed, "x");
        key(&mut ed, "esc");
        crate::typed::execute(&mut ed, "w").unwrap();
        settle_until(&mut ed, |ed| toasts(ed).contains("took too long before saving"));
        ed.flush_saves();
        assert_eq!(disk(&path), "xhi\n");
        settle_for(&mut ed, Duration::from_millis(800));
        assert!(!toasts(&ed).contains("ran too long"), "the late answer is dropped: {}", toasts(&ed));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A plugin's `run` reaches movement commands, and nothing else: no `:sh`, no `:w`.
    #[test]
    fn plugins_can_only_move() {
        let (mut ed, dir) = setup("only-move", "hello world\n");
        let marker = dir.join("pwned");
        for line in [format!(":sh touch {}", marker.display()), format!(":w {}", marker.display())] {
            crate::typed::execute(&mut ed, &format!("send {line}")).unwrap();
            settle_until(&mut ed, |ed| toasts(ed).contains("movement commands only"));
            ed.toasts.clear();
        }
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists(), "the plugin ran a command it mustn't");
        crate::typed::execute(&mut ed, "send move_next_word_start").unwrap();
        settle_until(&mut ed, |ed| ed.doc().selection().primary().head != 0);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Plugin text reaches the screen without terminal escapes, and capped.
    #[test]
    fn plugin_text_is_made_safe_to_show() {
        assert_eq!(shown("a\x1b]52;c;evil\x07b\nc"), "a\u{FFFD}]52;c;evil\u{FFFD}b\nc");
        let long = "x".repeat(MAX_TEXT + 10);
        assert_eq!(shown(&long).chars().count(), MAX_TEXT + 1);
        assert_eq!(bad_name("42"), Some("is tarae's own"));
        assert_eq!(bad_name("a\x1bb"), Some("isn't a command name"));
        assert_eq!(bad_name("shout"), None);
    }

    #[test]
    fn reloads_skips_taken_names_and_unloads() {
        let (mut ed, dir) = setup("reload", "x\n");
        // A second copy registers the same names: all skipped, the first keeps them
        std::fs::write(dir.join("copy.wasm"), DEMO).unwrap();
        ed.plugins_sync(host::scan(&dir));
        settle_until(&mut ed, |ed| toasts(ed).contains("plugin copy: skipped"));
        assert!(toasts(&ed).contains("“shout” is taken"));
        assert_eq!(ed.plugin_command("shout").unwrap().plugin, "demo");
        // A new build of demo: reloaded
        std::fs::write(dir.join("demo.wasm"), [DEMO, &[0, 1, 0]].concat()).unwrap();
        ed.plugins_sync(host::scan(&dir));
        settle_until(&mut ed, |ed| toasts(ed).contains("Plugin demo reloaded"));
        std::fs::remove_file(dir.join("demo.wasm")).unwrap();
        ed.plugins_sync(host::scan(&dir));
        settle_until(&mut ed, |ed| ed.plugin_command("shout").is_none());
        assert_eq!(crate::typed::execute(&mut ed, "shout"), Err("no such command: 'shout'".into()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn no_plugins_no_thread() {
        let mut ed = Editor::new(Config::default());
        ed.plugins_sync(Found::new());
        assert!(ed.plugins.is_none());
    }

    /// Asking a plugin costs the editor the same for a line and for 8 MB: the rope is shared, not copied.
    #[test]
    fn running_a_command_costs_the_same_for_any_size() {
        let big: String = (0..240_000).map(|i| format!("line {i} with some text on it   \n")).collect();
        assert!(big.len() > 8 << 20);
        let (mut ed, dir) = setup("cost", &big);
        let mut worst = Duration::ZERO;
        for _ in 0..5 {
            let t = Instant::now();
            ed.plugin_run("trim-trailing", "").unwrap();
            worst = worst.max(t.elapsed());
        }
        assert!(worst < Duration::from_millis(2), "the editor's part took {worst:?}");
        settle_until(&mut ed, |ed| toasts(ed).contains("lines trimmed"));
        std::fs::remove_dir_all(dir).ok();
    }
}
