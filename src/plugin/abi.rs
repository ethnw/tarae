//! tarae's plugin API ([`plugin-sdk/wit/tarae.wit`]) on the canonical ABI: its types, and loading and calling
//! one plugin. No threads and no editor here — `host.rs` has those.
//!
//! A plugin is a core module wit-bindgen made from the WIT, for `wasm32-unknown-unknown`. Its exports:
//! `commands`, `subscriptions` and `permissions` (called when it loads), `run(name, cx)`, `on(event, cx)` and
//! `on-events(events, now)` — each answering a list of actions through a pointer, with a `cabi_post_*` to free
//! it — and `cabi_realloc`, which tarae allocates the arguments with.
//!
//! [`plugin-sdk/wit/tarae.wit`]: ../../plugin-sdk/wit/tarae.wit

use super::cabi::{Mem, T, V};
use crate::wasm::{Instance, Limits, Module, Trap, Ty, Val};

/// The API version plugins are built for. Until 1.0 each 0.x breaks plugins: only this one loads.
pub const API: &str = "0.2";
/// For freeing an answer (`cabi_post_*`).
const POST_FUEL: u64 = 100_000_000;
/// The wit-bindgen the SDK pins (what a plugin should be rebuilt with).
pub const WIT_BINDGEN: &str = "0.62";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub doc: String,
    pub text: bool,
}

/// What happens to a buffer (the WIT `event`, in its order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    Opened,
    Saved,
    Closed,
    Changed,
    BeforeSave,
    Shown,
}

const EVENTS: [Event; 6] =
    [Event::Opened, Event::Saved, Event::Closed, Event::Changed, Event::BeforeSave, Event::Shown];

impl Event {
    /// Its name in the WIT.
    pub fn name(self) -> &'static str {
        match self {
            Event::Opened => "opened",
            Event::Saved => "saved",
            Event::Closed => "closed",
            Event::Changed => "changed",
            Event::BeforeSave => "before-save",
            Event::Shown => "shown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub event: Event,
    pub text: bool,
    /// Only buffers in these languages (none: every buffer).
    pub languages: Vec<String>,
}

/// What a plugin may do beyond the buffer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    Run(String),
    Read(String),
    Env(String),
    Network,
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: u64,
    pub head: u64,
    pub from: u64,
    pub to: u64,
    pub text: String,
    pub line: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Buffer {
    pub path: Option<String>,
    pub name: String,
    pub language: Option<String>,
    pub text: Option<String>,
    pub version: u64,
    pub modified: bool,
}

/// The time and fresh randomness every call carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Now {
    pub millis: u64,
    pub entropy: Vec<u8>,
}

impl Now {
    /// Now, with 32 bytes from the system's random source.
    pub fn fresh() -> Now {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        use std::io::Read;
        // (opened once: opening it for every call was a tenth of a watch's work)
        static URANDOM: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
        let mut entropy = vec![0u8; 32];
        let urandom = URANDOM.get_or_init(|| std::fs::File::open("/dev/urandom").ok());
        let read = urandom.as_ref().map(|mut f| f.read_exact(&mut entropy));
        if !matches!(read, Some(Ok(()))) {
            // (no source: still different each call, but not secret — a plugin's TLS would be weak)
            entropy.iter_mut().zip(millis.to_le_bytes().iter().cycle()).for_each(|(e, m)| *e ^= *m);
        }
        Now { millis, entropy }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub args: String,
    pub buffer: Buffer,
    pub selections: Vec<Selection>,
    pub primary: u64,
    pub now: Now,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub from: u64,
    pub to: u64,
    pub insert: String,
}

/// A program a plugin starts (argv, no shell).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub id: u64,
    pub program: String,
    pub args: Vec<String>,
    /// Variables to set for it (only ones the plugin may read).
    pub env: Vec<(String, String)>,
}

/// A TCP connection a plugin opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    pub id: u64,
    pub host: String,
    pub port: u64,
}

/// A plugin's panel: a table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// A key per row (none: rows go by where they are).
    pub ids: Vec<String>,
    pub status: String,
    /// (key, what it does), for the header.
    pub keys: Vec<(String, String)>,
}

/// A read-only buffer a plugin fills.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub id: u64,
    pub title: String,
    pub text: String,
    pub language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Status(String),
    Success(String),
    Warning(String),
    Error(String),
    ReplaceSelections(Vec<String>),
    Edits(Vec<Edit>),
    Select(Vec<(u64, u64)>),
    Run(String),
    Open(String),
    StatusBar(String),
    Spawn(Spawn),
    Connect(Connect),
    Send(u64, Vec<u8>),
    Stop(u64),
    ReadFile(u64, String),
    ReadEnv(u64, String),
    Timer(u64, u64),
    Panel(Option<Panel>),
    Prompt(u64, String),
    Show(View),
    Append(u64, String),
    Hints(Hints),
    Side(Option<Side>),
    Pick(Pick),
    Badge(Option<(String, Tone)>),
    Find(u64, String),
    WriteOwn(u64, String, Vec<u8>),
    OfferInstall(u64, String),
}

impl Action {
    /// Whether it changes nothing on screen by itself (asking for I/O, a timer): an answer of only these
    /// isn't drawn.
    pub fn unseen(&self) -> bool {
        matches!(
            self,
            Action::Spawn(_)
                | Action::Connect(_)
                | Action::Send(..)
                | Action::Stop(_)
                | Action::ReadFile(..)
                | Action::ReadEnv(..)
                | Action::Timer(..)
                | Action::Find(..)
                | Action::WriteOwn(..)
        )
    }
}

/// How a hint or a card row reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Info,
    Warning,
    Error,
}

/// A plugin's hints for a buffer, at its `version`: (line, text, tone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hints {
    pub path: String,
    pub version: u64,
    pub hints: Vec<Hint>,
}

/// (line, text, tone)
pub type Hint = (u64, String, Tone);

/// A plugin's tab in the side panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Side {
    pub title: String,
    pub note: String,
    pub rows: Vec<SideRow>,
    pub keys: Vec<(String, String)>,
    pub open: bool,
    pub focus: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideRow {
    pub id: String,
    pub depth: u64,
    pub text: String,
    pub detail: String,
    pub tone: Tone,
    /// (path, first line, last line)
    pub at: Option<(String, u64, u64)>,
    pub note: bool,
}

/// A list to choose from: items are (id, label).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    pub id: u64,
    pub title: String,
    pub items: Vec<(String, String)>,
}

/// What comes back to a plugin from what it asked for (the WIT `host-event`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEvent {
    Lines(u64, Vec<String>),
    /// Its exit code (None: a signal), and its last lines on stderr.
    Exit(u64, Option<u64>, Vec<String>),
    Connected(u64),
    Data(u64, Vec<u8>),
    Closed(u64, String),
    File(u64, Vec<u8>),
    Env(u64, Option<String>),
    Tick(u64),
    /// A key in its panel, the row selected then, and that row's id.
    Key(String, Option<u64>, Option<String>),
    Answer(u64, Option<String>),
    Failed(u64, String),
    Found(u64, Vec<String>),
    Written(u64, String),
    Installed(u64),
}

// ── The WIT types, and converting to and from them ───────────────────────────

fn list(t: T) -> T {
    T::List(Box::new(t))
}

fn opt(t: T) -> T {
    T::Opt(Box::new(t))
}

fn rec(fs: Vec<T>) -> T {
    T::Rec(fs)
}

fn commands_t() -> T {
    list(rec(vec![T::Str, T::Str, T::Bool]))
}

fn event_t() -> T {
    T::Var(vec![None; EVENTS.len()])
}

fn subscriptions_t() -> T {
    list(rec(vec![event_t(), T::Bool, list(T::Str)]))
}

fn permissions_t() -> T {
    list(T::Var(vec![Some(T::Str), Some(T::Str), Some(T::Str), None, None]))
}

fn now_t() -> T {
    rec(vec![T::U64, T::Bytes])
}

/// The arguments of `run` (name, context) and `on` (event, context): `head` and a context.
fn cx_args_t(head: T) -> T {
    let selection = rec(vec![T::U64, T::U64, T::U64, T::U64, T::Str, T::U64]);
    let buffer = rec(vec![opt(T::Str), T::Str, opt(T::Str), opt(T::Str), T::U64, T::Bool]);
    let context = rec(vec![T::Str, buffer, list(selection), T::U64, now_t()]);
    rec(vec![head, context])
}

fn actions_t() -> T {
    let s = || Some(T::Str);
    let id_str = || Some(rec(vec![T::U64, T::Str]));
    let panel = rec(vec![
        T::Str,
        list(T::Str),
        list(list(T::Str)),
        list(T::Str),
        T::Str,
        list(rec(vec![T::Str, T::Str])),
    ]);
    list(T::Var(vec![
        s(),
        s(),
        s(),
        s(),
        Some(list(T::Str)),
        Some(list(rec(vec![T::U64, T::U64, T::Str]))),
        Some(list(rec(vec![T::U64, T::U64]))),
        s(),
        s(),
        s(),
        Some(rec(vec![T::U64, T::Str, list(T::Str), list(rec(vec![T::Str, T::Str]))])),
        Some(rec(vec![T::U64, T::Str, T::U64])),
        Some(rec(vec![T::U64, T::Bytes])),
        Some(T::U64),
        id_str(),
        id_str(),
        Some(rec(vec![T::U64, T::U64])),
        Some(opt(panel)),
        id_str(),
        Some(rec(vec![T::U64, T::Str, T::Str, opt(T::Str)])),
        id_str(),
        Some(rec(vec![T::Str, T::U64, list(rec(vec![T::U64, T::Str, tone_t()]))])),
        Some(opt(rec(vec![
            T::Str,
            T::Str,
            list(rec(vec![
                T::Str,
                T::U64,
                T::Str,
                T::Str,
                tone_t(),
                opt(rec(vec![T::Str, T::U64, T::U64])),
                T::Bool,
            ])),
            list(rec(vec![T::Str, T::Str])),
            T::Bool,
            T::Bool,
        ]))),
        Some(rec(vec![T::U64, T::Str, list(rec(vec![T::Str, T::Str]))])),
        Some(opt(rec(vec![T::Str, tone_t()]))),
        id_str(),
        Some(rec(vec![T::U64, T::Str, T::Bytes])),
        id_str(),
    ]))
}

fn tone_t() -> T {
    T::Var(vec![None; 4])
}

fn tone_of(v: V) -> Tone {
    match v {
        V::Var(0, None) => Tone::Ok,
        V::Var(1, None) => Tone::Info,
        V::Var(2, None) => Tone::Warning,
        _ => Tone::Error,
    }
}

fn host_events_t() -> T {
    list(T::Var(vec![
        Some(rec(vec![T::U64, list(T::Str)])),
        Some(rec(vec![T::U64, opt(T::U64), list(T::Str)])),
        Some(T::U64),
        Some(rec(vec![T::U64, T::Bytes])),
        Some(rec(vec![T::U64, T::Str])),
        Some(rec(vec![T::U64, T::Bytes])),
        Some(rec(vec![T::U64, opt(T::Str)])),
        Some(T::U64),
        Some(rec(vec![T::Str, opt(T::U64), opt(T::Str)])),
        Some(rec(vec![T::U64, opt(T::Str)])),
        Some(rec(vec![T::U64, T::Str])),
        Some(rec(vec![T::U64, list(T::Str)])),
        Some(rec(vec![T::U64, T::Str])),
        Some(T::U64),
    ]))
}

fn str_v(s: &str) -> V {
    V::Str(s.to_string())
}

fn opt_v(s: &Option<String>) -> V {
    V::Opt(s.as_deref().map(|s| Box::new(str_v(s))))
}

fn strs_v(l: &[String]) -> V {
    V::List(l.iter().map(|s| str_v(s)).collect())
}

fn now_v(n: &Now) -> V {
    V::Rec(vec![V::U64(n.millis), V::Bytes(n.entropy.clone())])
}

fn cx_args(head: V, cx: &Context) -> V {
    let b = &cx.buffer;
    let buffer = V::Rec(vec![
        opt_v(&b.path),
        str_v(&b.name),
        opt_v(&b.language),
        opt_v(&b.text),
        V::U64(b.version),
        V::Bool(b.modified),
    ]);
    let selections = cx
        .selections
        .iter()
        .map(|s| {
            V::Rec(vec![
                V::U64(s.anchor),
                V::U64(s.head),
                V::U64(s.from),
                V::U64(s.to),
                str_v(&s.text),
                V::U64(s.line),
            ])
        })
        .collect();
    V::Rec(vec![
        head,
        V::Rec(vec![str_v(&cx.args), buffer, V::List(selections), V::U64(cx.primary), now_v(&cx.now)]),
    ])
}

fn host_events_v(events: &[HostEvent]) -> V {
    let case = |c: u32, v: V| V::Var(c, Some(Box::new(v)));
    let pair = |id: u64, v: V| V::Rec(vec![V::U64(id), v]);
    V::List(
        events
            .iter()
            .map(|e| match e {
                HostEvent::Lines(id, l) => case(0, pair(*id, strs_v(l))),
                HostEvent::Exit(id, code, err) => {
                    case(1, V::Rec(vec![V::U64(*id), V::Opt(code.map(|c| Box::new(V::U64(c)))), strs_v(err)]))
                }
                HostEvent::Connected(id) => case(2, V::U64(*id)),
                HostEvent::Data(id, b) => case(3, pair(*id, V::Bytes(b.clone()))),
                HostEvent::Closed(id, why) => case(4, pair(*id, str_v(why))),
                HostEvent::File(id, b) => case(5, pair(*id, V::Bytes(b.clone()))),
                HostEvent::Env(id, v) => case(6, pair(*id, opt_v(v))),
                HostEvent::Tick(id) => case(7, V::U64(*id)),
                HostEvent::Key(k, row, id) => {
                    case(8, V::Rec(vec![str_v(k), V::Opt(row.map(|r| Box::new(V::U64(r)))), opt_v(id)]))
                }
                HostEvent::Answer(id, a) => case(9, pair(*id, opt_v(a))),
                HostEvent::Failed(id, why) => case(10, pair(*id, str_v(why))),
                HostEvent::Found(id, paths) => case(11, pair(*id, strs_v(paths))),
                HostEvent::Written(id, path) => case(12, pair(*id, str_v(path))),
                HostEvent::Installed(id) => case(13, V::U64(*id)),
            })
            .collect(),
    )
}

// `load` builds values exactly after their type, so these shapes are the only ones that reach here.

fn items(v: V) -> Vec<V> {
    match v {
        V::List(vs) | V::Rec(vs) => vs,
        _ => unreachable!("a list or record"),
    }
}

fn string(v: V) -> String {
    match v {
        V::Str(s) => s,
        _ => unreachable!("a string"),
    }
}

fn u64_of(v: V) -> u64 {
    match v {
        V::U64(n) => n,
        _ => unreachable!("a u64"),
    }
}

fn bytes(v: V) -> Vec<u8> {
    match v {
        V::Bytes(b) => b,
        _ => unreachable!("bytes"),
    }
}

fn opt_string(v: V) -> Option<String> {
    match v {
        V::Opt(o) => o.map(|s| string(*s)),
        _ => unreachable!("an option"),
    }
}

fn strings(v: V) -> Vec<String> {
    items(v).into_iter().map(string).collect()
}

/// A record's fields, in order.
struct Fields(std::vec::IntoIter<V>);

impl Fields {
    fn of(v: V) -> Fields {
        Fields(items(v).into_iter())
    }
    fn next(&mut self) -> V {
        self.0.next().expect("a field the type has")
    }
    fn str(&mut self) -> String {
        string(self.next())
    }
    fn u64(&mut self) -> u64 {
        u64_of(self.next())
    }
}

fn commands_of(v: V) -> Vec<Command> {
    items(v)
        .into_iter()
        .map(|c| {
            let mut f = Fields::of(c);
            Command { name: f.str(), doc: f.str(), text: f.next() == V::Bool(true) }
        })
        .collect()
}

fn subscriptions_of(v: V) -> Vec<Subscription> {
    items(v)
        .into_iter()
        .map(|s| {
            let mut f = Fields::of(s);
            let V::Var(e, None) = f.next() else { unreachable!("an event") };
            Subscription {
                event: EVENTS[e as usize],
                text: f.next() == V::Bool(true),
                languages: strings(f.next()),
            }
        })
        .collect()
}

fn permissions_of(v: V) -> Vec<Permission> {
    items(v)
        .into_iter()
        .map(|p| match p {
            V::Var(0, Some(s)) => Permission::Run(string(*s)),
            V::Var(1, Some(s)) => Permission::Read(string(*s)),
            V::Var(2, Some(s)) => Permission::Env(string(*s)),
            V::Var(4, None) => Permission::Project,
            _ => Permission::Network,
        })
        .collect()
}

fn actions_of(v: V) -> Vec<Action> {
    items(v)
        .into_iter()
        .map(|a| {
            let V::Var(case, Some(payload)) = a else { unreachable!("every action has a payload") };
            let p = *payload;
            match case {
                0 => Action::Status(string(p)),
                1 => Action::Success(string(p)),
                2 => Action::Warning(string(p)),
                3 => Action::Error(string(p)),
                4 => Action::ReplaceSelections(strings(p)),
                5 => Action::Edits(
                    items(p)
                        .into_iter()
                        .map(|e| {
                            let mut f = Fields::of(e);
                            Edit { from: f.u64(), to: f.u64(), insert: f.str() }
                        })
                        .collect(),
                ),
                6 => Action::Select(
                    items(p)
                        .into_iter()
                        .map(|r| {
                            let mut f = Fields::of(r);
                            (f.u64(), f.u64())
                        })
                        .collect(),
                ),
                7 => Action::Run(string(p)),
                8 => Action::Open(string(p)),
                9 => Action::StatusBar(string(p)),
                10 => {
                    let mut f = Fields::of(p);
                    Action::Spawn(Spawn {
                        id: f.u64(),
                        program: f.str(),
                        args: strings(f.next()),
                        env: items(f.next())
                            .into_iter()
                            .map(|kv| {
                                let mut f = Fields::of(kv);
                                (f.str(), f.str())
                            })
                            .collect(),
                    })
                }
                11 => {
                    let mut f = Fields::of(p);
                    Action::Connect(Connect { id: f.u64(), host: f.str(), port: f.u64() })
                }
                12 => {
                    let mut f = Fields::of(p);
                    Action::Send(f.u64(), bytes(f.next()))
                }
                13 => Action::Stop(u64_of(p)),
                14 | 15 | 18 | 20 => {
                    let mut f = Fields::of(p);
                    let (id, s) = (f.u64(), f.str());
                    match case {
                        14 => Action::ReadFile(id, s),
                        15 => Action::ReadEnv(id, s),
                        18 => Action::Prompt(id, s),
                        _ => Action::Append(id, s),
                    }
                }
                16 => {
                    let mut f = Fields::of(p);
                    Action::Timer(f.u64(), f.u64())
                }
                17 => Action::Panel(match p {
                    V::Opt(None) => None,
                    V::Opt(Some(panel)) => {
                        let mut f = Fields::of(*panel);
                        Some(Panel {
                            title: f.str(),
                            columns: strings(f.next()),
                            rows: items(f.next()).into_iter().map(strings).collect(),
                            ids: strings(f.next()),
                            status: f.str(),
                            keys: items(f.next())
                                .into_iter()
                                .map(|k| {
                                    let mut f = Fields::of(k);
                                    (f.str(), f.str())
                                })
                                .collect(),
                        })
                    }
                    _ => unreachable!("an option"),
                }),
                19 => {
                    let mut f = Fields::of(p);
                    Action::Show(View {
                        id: f.u64(),
                        title: f.str(),
                        text: f.str(),
                        language: opt_string(f.next()),
                    })
                }
                21 => {
                    let mut f = Fields::of(p);
                    Action::Hints(Hints {
                        path: f.str(),
                        version: f.u64(),
                        hints: items(f.next())
                            .into_iter()
                            .map(|h| {
                                let mut f = Fields::of(h);
                                (f.u64(), f.str(), tone_of(f.next()))
                            })
                            .collect(),
                    })
                }
                22 => Action::Side(match p {
                    V::Opt(None) => None,
                    V::Opt(Some(side)) => {
                        let mut f = Fields::of(*side);
                        Some(Side {
                            title: f.str(),
                            note: f.str(),
                            rows: items(f.next())
                                .into_iter()
                                .map(|r| {
                                    let mut f = Fields::of(r);
                                    SideRow {
                                        id: f.str(),
                                        depth: f.u64(),
                                        text: f.str(),
                                        detail: f.str(),
                                        tone: tone_of(f.next()),
                                        at: match f.next() {
                                            V::Opt(Some(at)) => {
                                                let mut f = Fields::of(*at);
                                                Some((f.str(), f.u64(), f.u64()))
                                            }
                                            _ => None,
                                        },
                                        note: f.next() == V::Bool(true),
                                    }
                                })
                                .collect(),
                            keys: items(f.next())
                                .into_iter()
                                .map(|k| {
                                    let mut f = Fields::of(k);
                                    (f.str(), f.str())
                                })
                                .collect(),
                            open: f.next() == V::Bool(true),
                            focus: f.next() == V::Bool(true),
                        })
                    }
                    _ => unreachable!("an option"),
                }),
                25 | 27 => {
                    let mut f = Fields::of(p);
                    let (id, s) = (f.u64(), f.str());
                    if case == 25 { Action::Find(id, s) } else { Action::OfferInstall(id, s) }
                }
                26 => {
                    let mut f = Fields::of(p);
                    Action::WriteOwn(f.u64(), f.str(), bytes(f.next()))
                }
                24 => Action::Badge(match p {
                    V::Opt(Some(b)) => {
                        let mut f = Fields::of(*b);
                        Some((f.str(), tone_of(f.next())))
                    }
                    _ => None,
                }),
                _ => {
                    let mut f = Fields::of(p);
                    Action::Pick(Pick {
                        id: f.u64(),
                        title: f.str(),
                        items: items(f.next())
                            .into_iter()
                            .map(|i| {
                                let mut f = Fields::of(i);
                                (f.str(), f.str())
                            })
                            .collect(),
                    })
                }
            }
        })
        .collect()
}

// ── A loaded plugin ──────────────────────────────────────────────────────────

/// The exports tarae calls.
struct Exports {
    realloc: u32,
    commands: u32,
    post_commands: u32,
    run: u32,
    post_run: u32,
    subscriptions: u32,
    post_subscriptions: u32,
    permissions: u32,
    post_permissions: u32,
    on: u32,
    post_on: u32,
    on_events: u32,
    post_on_events: u32,
}

pub struct Plugin {
    inst: Instance,
    x: Exports,
}

/// What a trap means to someone who wrote the plugin.
fn trapped(t: Trap) -> String {
    match t {
        Trap::OutOfFuel => "ran too long and was stopped (a loop that never ends?)".into(),
        Trap::Unreachable => "crashed (a panic?)".into(),
        Trap::StackOverflow => "crashed: recursion too deep".into(),
        t => format!("crashed: {t}"),
    }
}

/// The plugin API version wit-bindgen recorded (its `component-type:…` section names the package). Until
/// 1.0, major.minor must match.
fn check_api(m: &Module) -> Result<(), String> {
    let Some(world) = m.customs().find_map(|n| n.strip_prefix("component-type:")) else { return Ok(()) };
    let Some(v) = world.split("tarae:plugin@").nth(1) else {
        return Err("it's built for some other WIT world, not tarae's plugin API".into());
    };
    let version = v.split(|c: char| !(c.is_ascii_digit() || c == '.')).next().unwrap_or(v);
    let major_minor: String = version.split('.').take(2).collect::<Vec<_>>().join(".");
    if major_minor == API {
        return Ok(());
    }
    Err(format!(
        "it's built for tarae plugin API {version}, this tarae speaks {API} — rebuild it with this tarae's SDK"
    ))
}

fn exports(m: &Module) -> Result<Exports, String> {
    use Ty::{I32, I64};
    let f = |name: &str, params: &[Ty], results: &[Ty]| -> Result<u32, String> {
        let i = m.func(name).ok_or_else(|| {
            format!("it has no `{name}` export — is it a tarae plugin? (`tarae plugin new` starts one)")
        })?;
        let t = m.func_type(i);
        if *t.params != *params || *t.results != *results {
            return Err(format!(
                "its `{name}` doesn't match tarae's plugin API — rebuild it with the SDK (wit-bindgen {WIT_BINDGEN})"
            ));
        }
        Ok(i)
    };
    Ok(Exports {
        realloc: f("cabi_realloc", &[I32; 4], &[I32])?,
        commands: f("commands", &[], &[I32])?,
        post_commands: f("cabi_post_commands", &[I32], &[])?,
        run: f("run", &[I32], &[I32])?,
        post_run: f("cabi_post_run", &[I32], &[])?,
        subscriptions: f("subscriptions", &[], &[I32])?,
        post_subscriptions: f("cabi_post_subscriptions", &[I32], &[])?,
        permissions: f("permissions", &[], &[I32])?,
        post_permissions: f("cabi_post_permissions", &[I32], &[])?,
        on: f("on", &[I32], &[I32])?,
        post_on: f("cabi_post_on", &[I32], &[])?,
        on_events: f("on-events", &[I32, I32, I64, I32, I32], &[I32])?,
        post_on_events: f("cabi_post_on-events", &[I32], &[])?,
    })
}

/// The runtime's reasons, in terms of how the plugin was built.
fn rejected(e: crate::wasm::Error) -> String {
    let e = e.to_string();
    if e.contains("SIMD") {
        "it uses SIMD — build it without `+simd128`".into()
    } else {
        format!("not a plugin tarae can load: {e}")
    }
}

impl Plugin {
    /// Decodes, checks and starts a plugin (`fuel` for its start function).
    pub fn new(bytes: &[u8], limits: Limits, fuel: u64) -> Result<Plugin, String> {
        let m = Module::new(bytes).map_err(rejected)?;
        check_api(&m)?;
        let x = exports(&m)?;
        let inst = Instance::new(m, limits, fuel).map_err(|t| format!("failed to start: {}", trapped(t)))?;
        Ok(Plugin { inst, x })
    }

    /// Whether it runs as machine code — the interpreter does less for the same fuel.
    pub fn compiled(&self) -> bool {
        self.inst.module().compiled()
    }

    fn call(&mut self, f: u32, args: &[Val]) -> Result<Vec<Val>, String> {
        self.inst.call(f, args).map_err(trapped)
    }

    /// Calls `f` for a result pointer, reads what it points at, and lets the plugin free it.
    fn answer(&mut self, f: u32, args: &[Val], t: &T, post: u32) -> Result<V, String> {
        let ret = match self.call(f, args)?[..] {
            [Val::I32(p)] => p,
            _ => unreachable!("the signature was checked"),
        };
        let v = Mem::new(&mut self.inst, self.x.realloc).load(t, ret as u32)?;
        // Freeing gets fuel of its own: a call that finished close to its budget still counts
        self.inst.set_fuel(POST_FUEL);
        self.call(post, &[Val::I32(ret)])?;
        Ok(v)
    }

    /// The commands it registers.
    pub fn commands(&mut self, fuel: u64) -> Result<Vec<Command>, String> {
        self.inst.set_fuel(fuel);
        let v = self.answer(self.x.commands, &[], &commands_t(), self.x.post_commands)?;
        Ok(commands_of(v))
    }

    /// The events it subscribes to.
    pub fn subscriptions(&mut self, fuel: u64) -> Result<Vec<Subscription>, String> {
        self.inst.set_fuel(fuel);
        let v = self.answer(self.x.subscriptions, &[], &subscriptions_t(), self.x.post_subscriptions)?;
        Ok(subscriptions_of(v))
    }

    /// What it may do beyond the buffer.
    pub fn permissions(&mut self, fuel: u64) -> Result<Vec<Permission>, String> {
        self.inst.set_fuel(fuel);
        let v = self.answer(self.x.permissions, &[], &permissions_t(), self.x.post_permissions)?;
        Ok(permissions_of(v))
    }

    /// Runs command `name`. After an `Err` the instance may be half-way through something — start a new one.
    pub fn run(&mut self, name: &str, cx: &Context, fuel: u64) -> Result<Vec<Action>, String> {
        let (f, post) = (self.x.run, self.x.post_run);
        self.with_context(f, post, T::Str, str_v(name), cx, fuel)
    }

    /// Tells it about `event` (as `run`, for errors).
    pub fn on(&mut self, event: Event, cx: &Context, fuel: u64) -> Result<Vec<Action>, String> {
        let e = EVENTS.iter().position(|&x| x == event).expect("every event") as u32;
        let (f, post) = (self.x.on, self.x.post_on);
        self.with_context(f, post, event_t(), V::Var(e, None), cx, fuel)
    }

    /// What came back from what it asked for (as `run`, for errors).
    pub fn on_events(&mut self, events: &[HostEvent], now: &Now, fuel: u64) -> Result<Vec<Action>, String> {
        self.inst.set_fuel(fuel);
        // The arguments flatten to (events pointer, length, millis, entropy pointer, length): the lists
        // passed as they are, in memory the plugin then owns
        let mut mem = Mem::new(&mut self.inst, self.x.realloc);
        let (events, n) = mem.lay(&host_events_t(), &host_events_v(events))?;
        let (entropy, m) = mem.lay(&T::Bytes, &V::Bytes(now.entropy.clone()))?;
        let args = [
            Val::I32(events as i32),
            Val::I32(n as i32),
            Val::I64(now.millis as i64),
            Val::I32(entropy as i32),
            Val::I32(m as i32),
        ];
        let (f, post) = (self.x.on_events, self.x.post_on_events);
        let v = self.answer(f, &args, &actions_t(), post)?;
        Ok(actions_of(v))
    }

    /// Calls `f(head, cx)` — its arguments through memory — for a list of actions.
    fn with_context(
        &mut self,
        f: u32,
        post: u32,
        head_t: T,
        head: V,
        cx: &Context,
        fuel: u64,
    ) -> Result<Vec<Action>, String> {
        self.inst.set_fuel(fuel);
        let t = cx_args_t(head_t);
        let mut mem = Mem::new(&mut self.inst, self.x.realloc);
        let at = mem.alloc(t.align(), t.size())?;
        mem.store(&t, &cx_args(head, cx), at)?;
        let v = self.answer(f, &[Val::I32(at as i32)], &actions_t(), post)?;
        Ok(actions_of(v))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub const DEMO: &[u8] = include_bytes!("../../plugin-sdk/demo.wasm");
    const FUEL: u64 = 1 << 32;

    /// Each call's randomness is the system's (a plugin's TLS keys come from it): new each time, and not
    /// just the clock's bytes.
    #[test]
    fn each_call_gets_fresh_entropy() {
        let (a, b) = (Now::fresh(), Now::fresh());
        assert_eq!(a.entropy.len(), 32);
        assert_ne!(a.entropy, b.entropy);
        let clock: Vec<u8> = a.millis.to_le_bytes().iter().cycle().take(32).copied().collect();
        assert_ne!(a.entropy, clock, "not the fallback");
    }

    fn demo(jit: bool) -> Plugin {
        Plugin::new(DEMO, Limits { jit, ..Limits::default() }, FUEL).unwrap()
    }

    fn cx(args: &str, text: Option<&str>) -> Context {
        Context {
            args: args.into(),
            buffer: Buffer {
                path: Some("/tmp/a.rs".into()),
                name: "a.rs".into(),
                language: Some("rust".into()),
                text: text.map(str::to_string),
                version: 7,
                modified: true,
            },
            selections: vec![
                Selection { anchor: 0, head: 5, from: 0, to: 5, text: "hello".into(), line: 0 },
                Selection { anchor: 9, head: 6, from: 6, to: 9, text: "wörld".into(), line: 1 },
            ],
            primary: 1,
            now: Now { millis: 1_700_000_000_000, entropy: vec![7; 32] },
        }
    }

    #[test]
    fn the_demo_plugin_answers_on_both_engines() {
        for jit in [true, false] {
            let mut p = demo(jit);
            let cmds = p.commands(FUEL).unwrap();
            assert_eq!(
                cmds[0],
                Command { name: "shout".into(), doc: "Upper-case the selections".into(), text: false }
            );
            assert!(cmds.iter().any(|c| c.name == "trim-trailing" && c.text));
            assert_eq!(
                p.run("shout", &cx("", None), FUEL).unwrap(),
                [Action::ReplaceSelections(vec!["HELLO".into(), "WÖRLD".into()])]
            );
            assert_eq!(
                p.run("trim-trailing", &cx("", Some("a  \nb\nc \n")), FUEL).unwrap(),
                [
                    Action::Success("2 lines trimmed".into()),
                    Action::Edits(vec![
                        Edit { from: 1, to: 3, insert: String::new() },
                        Edit { from: 7, to: 8, insert: String::new() }
                    ])
                ]
            );
            assert_eq!(
                p.run("grab", &cx("ab", Some("ab ab")), FUEL).unwrap(),
                [Action::Select(vec![(0, 2), (3, 5)])]
            );
            assert_eq!(p.run("send", &cx(":w", None), FUEL).unwrap(), [Action::Run(":w".into())]);
            assert_eq!(p.run("visit", &cx("x.rs", None), FUEL).unwrap(), [Action::Open("x.rs".into())]);
            assert_eq!(p.run("notify", &cx("hi", None), FUEL).unwrap().len(), 4);
            let [Action::Status(echo)] = &p.run("echo", &cx("a b", None), FUEL).unwrap()[..] else {
                panic!()
            };
            assert_eq!(
                echo,
                r#"args="a b" path=Some("/tmp/a.rs") name=a.rs language=Some("rust") text=false version=7 modified=true primary=1 sels=0..5@0:"hello",6..9@1:"wörld""#
            );
        }
    }

    #[test]
    fn events_go_to_on() {
        for jit in [true, false] {
            let mut p = demo(jit);
            let subs = p.subscriptions(FUEL).unwrap();
            assert_eq!(subs.len(), 7);
            assert!(subs.contains(&Subscription { event: Event::Changed, text: true, languages: vec![] }));
            assert!(
                subs.contains(&Subscription { event: Event::Shown, text: false, languages: vec![] }),
                "the last event"
            );
            assert!(subs.contains(&Subscription { event: Event::Opened, text: false, languages: vec![] }));
            assert_eq!(
                p.on(Event::Opened, &cx("", None), FUEL).unwrap(),
                [Action::Status("opened a.rs".into())]
            );
            assert_eq!(
                p.on(Event::Saved, &cx("", None), FUEL).unwrap(),
                [Action::StatusBar("saved v7".into())]
            );
            assert_eq!(
                p.on(Event::Changed, &cx("", Some("one two  three")), FUEL).unwrap(),
                [Action::StatusBar("3 words".into())]
            );
            assert_eq!(p.on(Event::BeforeSave, &cx("", Some("x ")), FUEL).unwrap(), []);
        }
    }

    #[test]
    fn requests_and_what_comes_back() {
        for jit in [true, false] {
            let mut p = demo(jit);
            assert_eq!(
                p.permissions(FUEL).unwrap(),
                [
                    Permission::Run("true".into()),
                    Permission::Read("/tmp/tarae-demo/**".into()),
                    Permission::Env("TARAE_DEMO".into()),
                    Permission::Network,
                    Permission::Project
                ]
            );
            let a = p.run("requests", &cx("", None), FUEL).unwrap();
            assert_eq!(
                a[0],
                Action::Spawn(Spawn {
                    id: 1,
                    program: "true".into(),
                    args: vec!["a b".into()],
                    env: vec![("TARAE_DEMO".into(), "1".into())]
                })
            );
            assert_eq!(a[1], Action::Connect(Connect { id: 2, host: "127.0.0.1".into(), port: 6443 }));
            assert_eq!(
                a[2..7],
                [
                    Action::Send(2, vec![0, 1, 255]),
                    Action::Stop(1),
                    Action::ReadFile(3, "~/.kube/config".into()),
                    Action::ReadEnv(4, "KUBECONFIG".into()),
                    Action::Timer(5, 1000),
                ]
            );
            let Action::Panel(Some(panel)) = &a[7] else { panic!("{:?}", a[7]) };
            assert_eq!(
                (panel.rows.len(), &panel.keys[0].1[..], &panel.status[..]),
                (2, "describe", "2 pods")
            );
            assert_eq!(a[8], Action::Prompt(6, "filter".into()));
            assert_eq!(
                a[9],
                Action::Show(View {
                    id: 7,
                    title: "web-1".into(),
                    text: "kind: Pod".into(),
                    language: Some("yaml".into())
                })
            );
            assert_eq!(a[10..12], [Action::Append(7, "\nmore".into()), Action::Panel(None)]);
            assert_eq!(
                a[12],
                Action::Status("now 1700000000000 · 32 random bytes".into()),
                "the time and randomness came"
            );
            let events = [
                HostEvent::Lines(1, vec!["x".into()]),
                HostEvent::Exit(1, Some(0), vec![]),
                HostEvent::Connected(2),
                HostEvent::Data(2, vec![0, 255]),
                HostEvent::Closed(2, String::new()),
                HostEvent::File(3, vec![1; 1000]),
                HostEvent::Env(4, None),
                HostEvent::Tick(5),
                HostEvent::Key("d".into(), Some(1), Some("default/web".into())),
                HostEvent::Answer(6, Some("web".into())),
                HostEvent::Failed(9, "no".into()),
            ];
            let now = Now { millis: 42, entropy: vec![1; 32] };
            let [Action::Status(said)] = &p.on_events(&events, &now, FUEL).unwrap()[..] else { panic!() };
            assert_eq!(
                said,
                r#"lines 1 ["x"] | exit 1 Some(0) [] | connected 2 | data 2 [0, 255] | closed 2 "" | file 3 1000 | env 4 None | tick 5 | key d Some(1) Some("default/web") | answer 6 Some("web") | failed 9 no · 42 · 32"#
            );
        }
    }

    #[test]
    fn misbehaving_plugins_fail_cleanly() {
        let mut p = demo(true);
        assert_eq!(p.run("spin", &cx("", None), 1 << 20).unwrap_err(), trapped(Trap::OutOfFuel));
        let mut p = demo(true);
        assert_eq!(p.run("boom", &cx("", None), FUEL).unwrap_err(), "crashed (a panic?)");
        // Deep recursion ends in a trap (the plugin's own stack, or tarae's call depth) — not in tarae
        for jit in [true, false] {
            let mut p = demo(jit);
            assert!(p.run("deep", &cx("", None), FUEL).unwrap_err().starts_with("crashed"));
        }
    }

    #[test]
    fn what_isnt_a_plugin_is_refused_with_a_reason() {
        let refuse = |wat: &str| match Plugin::new(&wat::parse_str(wat).unwrap(), Limits::default(), FUEL) {
            Ok(_) => panic!("loaded {wat}"),
            Err(e) => e,
        };
        let e = refuse(
            r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))))"#,
        );
        assert!(e.contains("wasm32-unknown-unknown"), "{e}");
        let e = refuse(r#"(module (func (export "f") (param v128)))"#);
        assert!(e.contains("+simd128"), "{e}");
        let e = refuse("(module)");
        assert!(e.contains("no `cabi_realloc` export"), "{e}");
        let e = refuse(
            r#"(module (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 0)
                       (func (export "commands") (result i64) i64.const 0))"#,
        );
        assert!(e.contains("`commands` doesn't match"), "{e}");
        // Built for another 0.x of the API (each breaks plugins until 1.0)
        let mut other = DEMO.to_vec();
        let at = other.windows(13).position(|w| w == b"tarae:plugin@").unwrap();
        other[at + 15] = b'3';
        let e = match Plugin::new(&other, Limits::default(), FUEL) {
            Ok(_) => panic!("loaded a plugin for API 0.3"),
            Err(e) => e,
        };
        assert!(e.contains("API 0.3.0, this tarae speaks 0.2"), "{e}");
    }
}
