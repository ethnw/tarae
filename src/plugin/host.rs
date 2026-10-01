//! The plugin thread: it loads what's in the plugins folder and runs commands and events one at a time, so a slow plugin
//! never holds up the editor. The editor talks to it with messages only: `Host::sync` with the folder's
//! contents, `Host::run` with a command; it answers through the callbacks it was given.
//!
//! - **Budget**: each call gets fuel for about a second of typical code (measured: a tight loop burns 7×10⁹ a
//!   second on the JIT, 5×10⁸ on the interpreter), then it's stopped. Floating-point division chains are the
//!   slowest per unit — under 3 seconds. Either way only this thread waits, never the editor.
//! - **After a failure** the instance may be half-way through something, so the next call gets a fresh one.
//! - **Text**: the editor hands over its rope (cloning one is O(1)) and the selections' positions; turning
//!   them into strings happens here, and the buffer's text only for commands registered with `text: true`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::time::SystemTime;

use ropey::Rope;

use super::abi::{
    Action, Buffer, Command, Context, Event, HostEvent, Now, Permission, Plugin, Selection, Subscription,
};
use crate::wasm::Limits;

const JIT_FUEL: u64 = 2_000_000_000;
const INTERPRETER_FUEL: u64 = 500_000_000;
/// For starting a plugin and asking its commands and subscriptions.
const LOAD_FUEL: u64 = 100_000_000;
/// The deepest call chain the default limits allow needs up to 1.9 MB of native stack (see
/// `Limits::call_depth`); twice that leaves room for the rest.
const STACK: usize = 4 << 20;

/// A plugin file's modification time and length — a change to either means reload.
pub type Stamp = (SystemTime, u64);
/// The plugins folder: name (file stem) → file.
pub type Found = BTreeMap<String, (PathBuf, Stamp)>;

pub fn scan(dir: &Path) -> Found {
    let Ok(rd) = std::fs::read_dir(dir) else { return Found::new() };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "wasm" {
                return None;
            }
            let m = e.metadata().ok()?;
            let name = p.file_stem()?.to_str()?.to_string();
            Some((name, (p, (m.modified().ok()?, m.len()))))
        })
        .collect()
}

/// Watching the plugins folder from a thread that already watches (the config watcher): `changed` returns the
/// folder's contents when they differ from the last look.
pub struct Watch {
    dir: Option<PathBuf>,
    last: Option<Found>,
}

impl Watch {
    pub fn new(dir: Option<PathBuf>) -> Watch {
        Watch { dir, last: None }
    }

    pub fn changed(&mut self) -> Option<Found> {
        let found = self.dir.as_deref().map(scan).unwrap_or_default();
        // An empty folder at start is no news: nothing to load, no thread to start
        let first_and_empty = self.last.is_none() && found.is_empty();
        if self.last.as_ref() == Some(&found) || first_and_empty {
            self.last = Some(found);
            return None;
        }
        self.last = Some(found.clone());
        Some(found)
    }
}

/// What the plugin thread tells the editor, besides answers.
#[derive(Debug, PartialEq)]
pub enum Note {
    /// A plugin (re)loaded — what it offers, or why it can't be used.
    Loaded {
        plugin: String,
        manifest: Result<Manifest, String>,
        reload: bool,
        /// Which load this is: what's sent to it (`Call::Events`) says which it was meant for.
        generation: u64,
    },
    Removed {
        plugin: String,
    },
}

/// What a plugin offers: its commands, and the events it wants.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub commands: Vec<Command>,
    pub subscriptions: Vec<Subscription>,
    /// What it may do beyond the buffer.
    pub permissions: Vec<Permission>,
}

/// What a plugin is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Command(String),
    Event(Event),
    /// What came back from what it asked for — dropped if it was for an earlier load (`Note::Loaded`).
    Events(u64, Vec<HostEvent>),
}

/// What a command (or event) runs on, as the editor had it then.
#[derive(Clone)]
pub struct Input {
    pub args: String,
    pub text: Rope,
    /// (anchor, head) per selection, in bytes.
    pub ranges: Vec<(usize, usize)>,
    pub primary: usize,
    pub path: Option<String>,
    pub name: String,
    pub language: Option<String>,
    pub version: u64,
    pub modified: bool,
}

impl Input {
    fn context(&self, with_text: bool) -> Context {
        let t = &self.text;
        let len = t.len_bytes();
        let selections = self
            .ranges
            .iter()
            .map(|&(anchor, head)| {
                let (from, to) = (anchor.min(head).min(len), anchor.max(head).min(len));
                Selection {
                    anchor: anchor as u64,
                    head: head as u64,
                    from: from as u64,
                    to: to as u64,
                    text: t.byte_slice(from..to).to_string(),
                    line: t.byte_to_line(head.min(len)) as u64,
                }
            })
            .collect();
        Context {
            args: self.args.clone(),
            buffer: Buffer {
                path: self.path.clone(),
                name: self.name.clone(),
                language: self.language.clone(),
                text: with_text.then(|| t.to_string()),
                version: self.version,
                modified: self.modified,
            },
            selections,
            primary: self.primary as u64,
            // (taken on the plugin thread, as the call starts)
            now: Now::fresh(),
        }
    }
}

pub type Reply = Box<dyn FnOnce(Result<Vec<Action>, String>) + Send>;

enum Req {
    Sync(Found),
    Call { plugin: String, call: Call, input: Input, reply: Reply },
}

struct Loaded {
    bytes: Vec<u8>,
    /// `None` after a failure, until the next call starts a fresh one.
    live: Option<Plugin>,
    manifest: Manifest,
    generation: u64,
}

impl Loaded {
    /// Whether `call` gets the buffer's text.
    fn with_text(&self, call: &Call) -> bool {
        match call {
            Call::Command(name) => self.manifest.commands.iter().any(|c| &c.name == name && c.text),
            Call::Event(e) => self.manifest.subscriptions.iter().any(|s| s.event == *e && s.text),
            Call::Events(..) => false,
        }
    }
}

fn budget(p: &Plugin) -> u64 {
    if p.compiled() { JIT_FUEL } else { INTERPRETER_FUEL }
}

fn load(bytes: &[u8]) -> Result<(Plugin, Manifest), String> {
    let mut p = Plugin::new(bytes, Limits::default(), LOAD_FUEL)?;
    let commands = p.commands(LOAD_FUEL)?;
    let subscriptions = p.subscriptions(LOAD_FUEL)?;
    let permissions = p.permissions(LOAD_FUEL)?;
    if let Some(why) = permissions.iter().find_map(super::perm::invalid) {
        return Err(why);
    }
    Ok((p, Manifest { commands, subscriptions, permissions }))
}

fn worker(rx: std::sync::mpsc::Receiver<Req>, notify: impl Fn(Note)) {
    let mut known: HashMap<String, Stamp> = HashMap::new();
    let mut loaded: HashMap<String, Loaded> = HashMap::new();
    let mut generation = 0;
    for req in rx {
        match req {
            Req::Sync(found) => {
                for (name, (path, stamp)) in &found {
                    if known.get(name) == Some(stamp) {
                        continue;
                    }
                    let reload = known.insert(name.clone(), *stamp).is_some();
                    loaded.remove(name);
                    generation += 1;
                    let bytes = std::fs::read(path).map_err(|e| format!("can't read it: {e}"));
                    let manifest = bytes.and_then(|bytes| {
                        let (p, manifest) = load(&bytes)?;
                        loaded.insert(
                            name.clone(),
                            Loaded { bytes, live: Some(p), manifest: manifest.clone(), generation },
                        );
                        Ok(manifest)
                    });
                    notify(Note::Loaded { plugin: name.clone(), manifest, reload, generation });
                }
                known.retain(|name, _| {
                    let keep = found.contains_key(name);
                    if !keep {
                        loaded.remove(name);
                        notify(Note::Removed { plugin: name.clone() });
                    }
                    keep
                });
            }
            Req::Call { plugin, call, input, reply } => {
                let Some(l) = loaded.get_mut(&plugin) else {
                    reply(Err(format!("plugin {plugin} isn't loaded")));
                    continue;
                };
                if let Call::Events(g, _) = &call
                    && *g != l.generation
                {
                    reply(Ok(Vec::new()));
                    continue;
                }
                let p = match l.live.take() {
                    Some(p) => Ok(p),
                    None => load(&l.bytes).map(|(p, _)| p),
                };
                let result = p.and_then(|mut p| {
                    let fuel = budget(&p);
                    let r = match &call {
                        Call::Command(name) => p.run(name, &input.context(l.with_text(&call)), fuel),
                        Call::Event(e) => p.on(*e, &input.context(l.with_text(&call)), fuel),
                        Call::Events(_, events) => p.on_events(events, &Now::fresh(), fuel),
                    };
                    if r.is_ok() {
                        l.live = Some(p);
                    }
                    r
                });
                reply(result);
            }
        }
    }
}

/// The editor's end of the plugin thread.
pub struct Host {
    tx: Sender<Req>,
}

impl Host {
    /// Starts the thread; `notify` gets its notes (on that thread).
    pub fn start(notify: impl Fn(Note) + Send + 'static) -> Option<Host> {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("tarae-plugins".into())
            .stack_size(STACK)
            .spawn(move || worker(rx, notify))
            .ok()?;
        Some(Host { tx })
    }

    /// The plugins folder's contents: new and changed files are (re)loaded, missing ones dropped.
    pub fn sync(&self, found: Found) {
        let _ = self.tx.send(Req::Sync(found));
    }

    /// Runs a command, or tells a plugin about an event; `reply` gets the answer (on the plugin thread).
    pub fn call(&self, plugin: &str, call: Call, input: Input, reply: Reply) -> Result<(), String> {
        let req = Req::Call { plugin: plugin.into(), call, input, reply };
        self.tx.send(req).map_err(|_| "the plugin thread stopped".to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    use super::super::abi::tests::DEMO;
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarae-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn start() -> (Host, Receiver<Note>) {
        let (tx, rx) = channel();
        let host = Host::start(move |n| tx.send(n).unwrap()).unwrap();
        (host, rx)
    }

    fn input(args: &str, text: &str, ranges: &[(usize, usize)]) -> Input {
        Input {
            args: args.into(),
            text: Rope::from_str(text),
            ranges: ranges.to_vec(),
            primary: 0,
            path: None,
            name: "scratch".into(),
            language: None,
            version: 1,
            modified: false,
        }
    }

    fn run(host: &Host, command: &str, input: Input) -> Result<Vec<Action>, String> {
        let (tx, rx) = channel();
        host.call("demo", Call::Command(command.into()), input, Box::new(move |r| tx.send(r).unwrap()))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(30)).expect("an answer")
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn loads_runs_reloads_and_drops() {
        let d = dir("life");
        std::fs::write(d.join("demo.wasm"), DEMO).unwrap();
        let (host, notes) = start();
        host.sync(scan(&d));
        let Note::Loaded { plugin, manifest: Ok(Manifest { commands: cmds, .. }), reload: false, .. } =
            notes.recv_timeout(WAIT).unwrap()
        else {
            panic!("loaded")
        };
        assert_eq!(plugin, "demo");
        assert!(cmds.iter().any(|c| c.name == "shout"));
        // Selections become text here; the buffer's text only goes to commands that asked for it
        let r = run(&host, "shout", input("", "héllo world\n", &[(0, 6), (13, 7)]));
        assert_eq!(r, Ok(vec![Action::ReplaceSelections(vec!["HÉLLO".into(), "WORLD\n".into()])]));
        let r = run(&host, "echo", input("", "a\nb", &[(2, 3)]));
        let Ok([Action::Status(s)]) = r.as_deref() else { panic!("{r:?}") };
        assert!(s.contains("text=false") && s.contains(r#"sels=2..3@1:"b""#), "{s}");
        let r = run(&host, "grab", input("b", "abcb", &[(0, 0)]));
        assert_eq!(r, Ok(vec![Action::Select(vec![(1, 2), (3, 4)])]));
        // The same file again: nothing happens. A new build: reloaded. Gone: removed.
        host.sync(scan(&d));
        std::fs::write(d.join("demo.wasm"), [DEMO, &[0, 1, 0]].concat()).unwrap(); // a custom section more
        host.sync(scan(&d));
        let Note::Loaded { reload: true, manifest: Ok(_), generation: now, .. } =
            notes.recv_timeout(WAIT).unwrap()
        else {
            panic!("reloaded")
        };
        // What was meant for the load before (its connection's data, …) doesn't reach this one
        let events = |generation| {
            let (tx, rx) = channel();
            let call = Call::Events(generation, vec![HostEvent::Tick(5)]);
            host.call("demo", call, input("", "", &[]), Box::new(move |r| tx.send(r).unwrap())).unwrap();
            rx.recv_timeout(WAIT).unwrap().unwrap()
        };
        assert_eq!(events(now - 1), []);
        assert!(matches!(&events(now)[..], [Action::Status(s)] if s.starts_with("tick 5")));
        std::fs::remove_file(d.join("demo.wasm")).unwrap();
        host.sync(scan(&d));
        assert_eq!(notes.recv_timeout(WAIT).unwrap(), Note::Removed { plugin: "demo".into() });
        assert_eq!(run(&host, "shout", input("", "x", &[(0, 1)])), Err("plugin demo isn't loaded".into()));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn failures_are_answers_and_the_next_call_starts_fresh() {
        let d = dir("fail");
        std::fs::write(d.join("demo.wasm"), DEMO).unwrap();
        std::fs::write(d.join("junk.wasm"), b"not wasm").unwrap();
        let (host, notes) = start();
        host.sync(scan(&d));
        let mut loaded = BTreeMap::new();
        for _ in 0..2 {
            let Note::Loaded { plugin, manifest, .. } = notes.recv_timeout(WAIT).unwrap() else { panic!() };
            loaded.insert(plugin, manifest);
        }
        assert!(loaded["junk"].as_ref().unwrap_err().contains("not a WebAssembly module"));
        let t = Instant::now();
        let r = run(&host, "spin", input("", "", &[]));
        assert!(r.unwrap_err().contains("ran too long"));
        assert!(t.elapsed() < Duration::from_secs(10), "stopped by its budget: {:?}", t.elapsed());
        assert_eq!(run(&host, "boom", input("", "", &[])), Err("crashed (a panic?)".into()));
        assert!(run(&host, "stray", input("", "", &[])).is_ok(), "an odd edit is the editor's to refuse");
        assert_eq!(run(&host, "send", input(":w", "", &[])), Ok(vec![Action::Run(":w".into())]));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn the_watch_reports_changes_only() {
        let d = dir("watch");
        let mut w = Watch::new(Some(d.clone()));
        assert_eq!(w.changed(), None, "empty at start: nothing to do");
        std::fs::write(d.join("a.wasm"), DEMO).unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        assert_eq!(w.changed().map(|f| f.into_keys().collect::<Vec<_>>()), Some(vec!["a".to_string()]));
        assert_eq!(w.changed(), None);
        std::fs::remove_file(d.join("a.wasm")).unwrap();
        assert_eq!(w.changed(), Some(Found::new()), "emptied: the editor drops what it had");
        assert_eq!(Watch::new(None).changed(), None);
        std::fs::remove_dir_all(d).ok();
    }
}
