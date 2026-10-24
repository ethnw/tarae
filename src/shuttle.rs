//! Shuttle keys (roadmap §4, `docs/design/ui-state.md` "Shuttle keys"): a program you run — an assistant, a test
//! script — types into tarae over its UI socket (`uisocket.rs`, `tarae keys`). Each step says where it must land
//! (`expect`: what has the keys, and fields of elements as the state shows them), and is checked on the main loop
//! in the one job that hands its keys to `handle_key` — nothing (your key, a job's card, a slow pty) comes between
//! the check and the key. That gap is what made a script's `y` answer a card that came in front as it was typed,
//! a paste land before the commit buffer was there, and a toggle act on a state the script never saw.
//!
//! - **Who**: `ui.shuttle-keys` (your own config only); a peer that is this user's, not under Claude's processes
//!   nor under anything that runs the project's code (tasks, tests, servers, debuggers, plugins — the terminal
//!   panel is yours) — as far as the parent chain shows at hello. A peer re-parented to pid 1 (it daemonized or
//!   forked twice) is refused; one that left the chain another way (a subreaper, a program asked to start it)
//!   isn't seen, and then the card, naming its executable, is the guard. The first hello from a process (`for`,
//!   by pid and start: a reused pid inherits nothing) asks you on a card, and `y` holds until that process ends.
//!   One shuttle at a time.
//! - **Your key wins**: any key, click or paste of yours pauses it; it goes on once you've been idle `QUIET` and it
//!   asks (`resume`). `:shuttle-end` (or a click on its chip) ends it, and its grant.
//! - **Keys only**, going where yours go: the same handlers, the same refusals on the card. A weighty card's
//!   answers, and the answers that widen trust or outlive the session (`A`, `s`, `N`, a project's trust, the
//!   sandbox's, this card's own), are refused for now — any card's `n` isn't; so are its keys into tarae's config
//!   files' buffers and the `:` lines that open or write them ("Config files" below). Those guards are about what
//!   its keys reach in tarae: the program runs as you and could write your config or run the commands itself, so
//!   the grant card is the boundary, not them.
//! - **What only a person's look may do** stays theirs: `Editor.person_seq` counts only your input (follow's pause,
//!   a card's place among yours, an error read), and a review's looks (`space v h`) refuse a shuttle's key.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::key::Key;
use crate::uistate::Region;

/// How long you've been idle before a paused shuttle may go on (it asks: `resume`).
pub const QUIET: Duration = Duration::from_secs(5);

/// The chip says a step in the accent this long after it.
pub const LIT: Duration = Duration::from_millis(1500);

/// What a step that landed is counted as (`features.rs`).
pub const STEPS: [&str; 2] = ["keys", "text"];

/// The grant card's title.
pub const TITLE: &str = "A program asks to type into tarae";

/// A shuttle's name: what it says it is, on the card and the chip — a few plain characters.
const NAME_MAX: usize = 32;

/// Cards a shuttle never answers but with `n` (`Offer::kind`): their yes decides what tarae may do from now on.
const NEVER: [&str; 8] = [
    "project-trust",
    "repo-trust",
    "git-trust",
    "claude-settings",
    "hooks",
    "no-sandbox",
    "outside",
    "shuttle",
];

/// `ui.shuttle-keys`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    #[default]
    Off,
    /// Keys outside cards, light cards' answers, every `n`.
    Light,
    /// Weighty answers too, once read — not built yet: as `Light`.
    Weighty,
}

impl Level {
    pub const NAMES: &'static [&'static str] = &["off", "light", "weighty"];

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    pub fn of(s: &str) -> Level {
        match s {
            "light" => Level::Light,
            "weighty" => Level::Weighty,
            _ => Level::Off,
        }
    }
}

/// The process a grant goes to: its pid and when it started (pids are reused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct For {
    pub pid: u32,
    pub started: u64,
}

/// The shuttle connected.
#[derive(Clone, Debug)]
pub struct Shuttle {
    pub name: String,
    pub of: For,
    /// Its process's name (`claude`, `cargo`).
    pub what: String,
    /// Its number: a connection of one that ended since is refused.
    pub id: u64,
    /// Your key paused it: when (logic's clock) and the time as you'd read it.
    pub paused: Option<(Instant, String)>,
    /// Its last step, as the chip says it (`y → card#12`), and when.
    pub last: Option<(String, Instant)>,
    /// Your config folder as written and as its real path (`Hello.config`): what its keys never edit.
    pub config: Vec<PathBuf>,
}

/// What the editor keeps of shuttles (`Editor.shuttles`).
#[derive(Default)]
pub struct Shuttles {
    pub on: Option<Shuttle>,
    /// The processes you said `y` for — in memory only, each until it ends or `:shuttle-end`.
    pub grants: Vec<For>,
    /// Hellos waiting on their card: the card's id, the hello, its answer.
    asking: Vec<(u64, Hello, Reply)>,
    seq: u64,
    /// The step under way: its shuttle's name — what only a person may do refuses meanwhile, and the card a key
    /// answers says whose it was.
    pub stepping: Option<String>,
    /// Your last key, click or paste (logic's clock).
    person_at: Option<Instant>,
    /// Every shuttle's name since tarae started — Claude's level takes them out of what it reads (`for_claude`).
    names: Vec<String>,
    /// A `:` line the step's key ran was refused (`typed_refused`): the reason's word and why — the step stops
    /// there.
    blocked: Option<(&'static str, String)>,
    /// Tests: this tarae and what runs it, as a hello's `above` (their peer is this very process).
    #[cfg(test)]
    pub(crate) above: Option<Vec<u32>>,
}

impl Shuttles {
    /// How long you've been idle (None: no key of yours yet).
    #[cfg(test)]
    pub fn person_idle(&self) -> Option<Duration> {
        self.person_at.map(crate::clock::since)
    }
}

/// `text` as Claude's level says it: a shuttle's name taken out (`shuttle claude-code` → `shuttle`) — its toasts,
/// the gone ring's `answered y by shuttle …`. Claude knows one is connected, never which, nor its steps.
pub fn for_claude(sh: &Shuttles, text: &str) -> String {
    sh.names.iter().fold(text.to_string(), |t, n| t.replace(&format!("shuttle {n}"), "shuttle"))
}

/// A process's name as the card and the toast show it: the program picks its own, so the characters a shuttle's
/// name may have (`name_char` — no control character, escape or mark of the card's), cut to `NAME_MAX`. A word
/// that misleads can't be told from one that doesn't: the card says its executable's path too (`card_path`).
fn shown(what: &str) -> String {
    let s: String = what.chars().filter(|&c| name_char(c)).collect();
    let s = crate::cut::fit_ellipsis(s.trim(), NAME_MAX);
    if s.is_empty() { "?".into() } else { s }
}

/// The executable's path as the card shows it: no control character nor a mark that reorders or hides text (a
/// path may hold any other); the card cuts it from the left (`Offer.ends` — its name stays).
fn card_path(exe: &str) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    let p: String = exe.chars().filter(|&c| !hidden(c)).collect();
    if p.is_empty() { "(not known)".into() } else { p }
}

/// Where an answer goes: the connection's thread, a JSON line.
pub type Reply = std::sync::mpsc::Sender<String>;

/// `{"shuttle": {"name": …, "for": pid}}`, with what the socket's thread found out about the peer.
#[derive(Clone, Debug)]
pub struct Hello {
    pub name: String,
    pub of: For,
    /// Its `for` process's name.
    pub what: String,
    /// The executable its `for` process runs — what the grant goes to (its path; empty: not known).
    pub exe: String,
    /// The program connecting (`line`'s first): its name and executable — the card names it beside a `for` that's
    /// another process.
    pub peer_what: String,
    pub peer_exe: String,
    /// The peer and its parents (`uisocket::ancestors`).
    pub line: Vec<u32>,
    /// This tarae and its parents — its shell, the terminal, the login session: never a `for`.
    pub above: Vec<u32>,
    /// Your config folder as written and as its real path (`config_dirs`, read on the socket's thread: a step
    /// reads no disk for it).
    pub config: Vec<PathBuf>,
}

/// What a step sends.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Keys in tarae's notation (`space v m`, `C-q`, `esc`) — parsed, and as sent.
    Keys(Vec<Key>, String),
    /// One paste, with a person's paste rules (`handle_paste`).
    Text(String),
}

/// One step: `{"keys": … | "text": …, "expect": {…}}` of the shuttle numbered `id`.
#[derive(Clone, Debug)]
pub struct Step {
    pub id: u64,
    pub input: Input,
    pub expect: Value,
}

impl Step {
    /// A step from a request, its keys parsed — Err: why it isn't one.
    pub fn parse(id: u64, req: &Value) -> Result<Step, String> {
        let input = match (req["keys"].as_str(), req["text"].as_str()) {
            (Some(k), None) => {
                let keys: Result<Vec<Key>, String> = k.split_whitespace().map(str::parse).collect();
                let keys = keys?;
                if keys.is_empty() {
                    return Err("no keys — \"keys\": \"space v m\"".into());
                }
                Input::Keys(keys, k.split_whitespace().collect::<Vec<_>>().join(" "))
            }
            (None, Some(t)) => Input::Text(t.to_string()),
            _ => return Err("a step sends \"keys\" or \"text\", one of them".into()),
        };
        Ok(Step { id, input, expect: req["expect"].clone() })
    }
}

/// A refusal: its reason's word, why, and when to try again (ms).
#[derive(Clone, Debug, PartialEq)]
pub struct Refused {
    pub reason: &'static str,
    pub why: String,
    pub retry_after_ms: Option<u64>,
}

fn refused(reason: &'static str, why: impl Into<String>) -> Refused {
    Refused { reason, why: why.into(), retry_after_ms: None }
}

impl Refused {
    /// Its answer: what has the keys now with it, so the shuttle sees where they went instead.
    pub fn json(&self, ed: &Editor) -> Value {
        json!({ "refused": self.reason, "why": self.why, "retry_after_ms": self.retry_after_ms,
                "keys": crate::uistate::keys_json(&ed.keys_to()), "input": ed.input_seq })
    }
}

// ── Connecting ────────────────────────────────────────────────────────────────────

/// Why `name` isn't one a shuttle may go by: it's shown on the card and the chip, and said to Claude as a count.
fn name_refused(name: &str) -> Option<String> {
    let fine = !name.trim().is_empty() && name.chars().count() <= NAME_MAX && name.chars().all(name_char);
    (!fine).then(|| format!("a name of letters, digits, . _ - and spaces, at most {NAME_MAX} — not {name:?}"))
}

/// A character a shuttle's name may have — and what's kept of its process's name on the card (`shown`).
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ')
}

/// The pids that run the project's code, the user's tasks and tests included — a peer under one types nothing
/// (`:preflight`'s `build.rs` would find the socket through `TARAE_UI`). The terminal panel is yours.
fn runs_project_code(ed: &Editor) -> Vec<u32> {
    use crate::procs::Kind;
    let known = ed.procs_known().into_iter();
    known.filter(|k| !matches!(k.kind, Kind::Terminal | Kind::Tarae)).map(|k| k.pid).collect()
}

/// Why hello `h` may not connect now — None: it may (once granted).
fn hello_refused(ed: &Editor, h: &Hello) -> Option<Refused> {
    if ed.config.shuttle_keys == Level::Off {
        return Some(refused("off", "ui.shuttle-keys is off — your own config turns it on"));
    }
    if let Some(why) = name_refused(&h.name) {
        return Some(refused("name", why));
    }
    if !h.line.contains(&h.of.pid) {
        return Some(refused(
            "peer",
            format!("pid {} isn't this connection's process or one of its parents", h.of.pid),
        ));
    }
    // (re-parented to launchd/init — it daemonized or forked twice: the line no longer says what started it, so
    // whether Claude or a task did can't be told)
    if h.line.get(1) == Some(&1) {
        let why = "not started from a shell or a program you run — its parent is pid 1: it daemonized or forked twice";
        return Some(refused("peer", why));
    }
    // Never this tarae, nor what runs it: a grant to one would hold for every process under it
    match h.above.iter().position(|&p| p == h.of.pid) {
        Some(0) => return Some(refused("for", format!("for: pid {} is this tarae", h.of.pid))),
        Some(_) => {
            let why =
                format!("for: pid {} runs this tarae (its shell, its terminal, the login session)", h.of.pid);
            return Some(refused("for", why));
        }
        None => {}
    }
    let claude = ed.claude_pids();
    if let Some(p) = h.line.iter().find(|p| claude.contains(p)) {
        return Some(refused("peer", format!("under a process of Claude's (pid {p})")));
    }
    let project = runs_project_code(ed);
    if let Some(p) = h.line.iter().find(|p| project.contains(p)) {
        return Some(refused("peer", format!("under a process that runs the project's code (pid {p})")));
    }
    // Under this tarae: only from its terminal panel (yours), its grant to a process at or under the panel's
    // shell — never tarae itself or what it runs under, and never a child no list knows of (a hook, a formatter)
    let me = h.above.first().copied().unwrap_or_else(std::process::id);
    if let Some(t) = h.line.iter().position(|&p| p == me) {
        let terminals: Vec<u32> = ed
            .procs_known()
            .into_iter()
            .filter(|k| k.kind == crate::procs::Kind::Terminal)
            .map(|k| k.pid)
            .collect();
        let term = h.line[..t].iter().position(|p| terminals.contains(p));
        let at = h.line.iter().position(|&p| p == h.of.pid);
        match (term, at) {
            (Some(term), Some(at)) if at <= term => {}
            (None, _) => return Some(refused("peer", "under this tarae, but not from its terminal panel")),
            _ => {
                let why = format!("for: pid {} isn't at or under the terminal panel's shell", h.of.pid);
                return Some(refused("peer", why));
            }
        }
    }
    if let Some(s) = ed.shuttles.on.as_ref().filter(|s| s.of != h.of) {
        let why = format!(
            "{} (pid {}) types into tarae now — one at a time; :shuttle-end ends it",
            s.name, s.of.pid
        );
        return Some(refused("another", why));
    }
    None
}

/// A hello (`uisocket.rs`): refused, joined to the shuttle of its process, connected on its grant — or a card asks
/// you, its answer waiting on yours.
pub fn hello(ed: &mut Editor, mut h: Hello, reply: Reply) {
    (h.what, h.peer_what) = (shown(&h.what), shown(&h.peer_what));
    #[cfg(test)]
    if let Some(above) = &ed.shuttles.above {
        h.above = above.clone();
    }
    if let Some(r) = hello_refused(ed, &h) {
        let _ = reply.send(r.json(ed).to_string());
        ed.unchanged = true;
        return;
    }
    if ed.shuttles.on.as_ref().is_some_and(|s| s.of == h.of) || ed.shuttles.grants.contains(&h.of) {
        return connect(ed, h, reply);
    }
    // (a second call while the card for its process is up waits on that one)
    if let Some(card) = ed.shuttles.asking.iter().find(|a| a.1.of == h.of).map(|a| a.0) {
        ed.shuttles.asking.push((card, h, reply));
        ed.unchanged = true;
        return;
    }
    // (a process started on another day says which: `14:02` alone reads as today)
    let at = std::time::UNIX_EPOCH + Duration::from_secs(h.of.started);
    let started = match crate::log::day(at) == crate::log::day(crate::clock::system()) {
        true => crate::log::clock(at),
        false => crate::log::stamp(at).get(..16).unwrap_or_default().to_string(),
    };
    // The grant: the program connecting, until it exits — or, its `for` another process (`--for`), that one and
    // every program it starts: both named, by their executables, and what that means said as a warning
    let wide = h.line.first() != Some(&h.of.pid);
    let q = match wide {
        false => format!(
            "{} — {} (pid {}, started {started}) asks to send keys to tarae over its socket, each step checked \
             where it lands.\nprogram {}\n\ny lets it type until it exits; trust and weighty answers stay yours. \
             Your key pauses it, :shuttle-end ends it.",
            h.name,
            h.what,
            h.of.pid,
            card_path(&h.exe)
        ),
        true => format!(
            "{} — {} (pid {}) for {} (pid {}, started {started}) asks to type into tarae.\nprogram {}\nfor \
             {}\nEvery program {} starts may type into tarae until it ends.",
            h.name,
            h.peer_what,
            h.line.first().copied().unwrap_or_default(),
            h.what,
            h.of.pid,
            card_path(&h.peer_exe),
            card_path(&h.exe),
            h.what
        ),
    };
    let what = crate::offer::What::Shuttle;
    let mut card = crate::offer::Offer::new(TITLE.into(), &q, "", String::new(), what);
    // (who asks and what runs, at full strength: what's decided — each path one row, its end kept; a wider grant's
    // reach in the warning colour)
    (card.decided, card.ends) = match wide {
        false => (Some(0..2), vec![1]),
        true => (Some(0..3), vec![1, 2]),
    };
    card.warning = wide.then_some(3..4);
    ed.push_offer(card);
    ed.shuttles.asking.push((ed.offer_seq, h, reply));
}

/// The grant card's answer: `y` grants its process and connects what waits on it · `n` refuses them.
pub fn answer(ed: &mut Editor, card: u64, yes: bool) {
    ed.offer_finished(card, Ok(()));
    let (waiting, rest): (Vec<_>, Vec<_>) =
        std::mem::take(&mut ed.shuttles.asking).into_iter().partition(|a| a.0 == card);
    ed.shuttles.asking = rest;
    for (_, h, reply) in waiting {
        if !yes {
            let r = refused("no", "you said no on its card");
            let _ = reply.send(r.json(ed).to_string());
            continue;
        }
        if !ed.shuttles.grants.contains(&h.of) {
            ed.shuttles.grants.push(h.of);
            watch_end(ed, h.of);
        }
        // (checked again: another may have connected while the card waited)
        match hello_refused(ed, &h) {
            Some(r) => _ = reply.send(r.json(ed).to_string()),
            None => connect(ed, h, reply),
        }
    }
}

/// Connected (granted): a new shuttle said, or another connection of the one there.
fn connect(ed: &mut Editor, h: Hello, reply: Reply) {
    let joined = ed.shuttles.on.as_ref().is_some_and(|s| s.of == h.of);
    if !joined {
        ed.shuttles.seq += 1;
        let (name, of, what, config) = (h.name, h.of, h.what, h.config);
        let s = Shuttle { name, of, what, id: ed.shuttles.seq, paused: None, last: None, config };
        if !ed.shuttles.names.contains(&s.name) {
            ed.shuttles.names.push(s.name.clone());
        }
        // (its name always as `shuttle <name>` — Claude's level takes that out, `for_claude`; its process and pid
        // only in `:messages`, which Claude's level never has: it may know one types, never which)
        ed.recorded(
            crate::editor::ToastKind::Info,
            format!("shuttle {} connected: {}, pid {}", s.name, s.what, s.of.pid),
        );
        ed.set_status(format!(
            "shuttle {} connected — a program is typing into tarae · :shuttle-end ends it",
            s.name
        ));
        ed.shuttles.on = Some(s);
    } else {
        ed.unchanged = true;
    }
    let s = ed.shuttles.on.as_ref().expect("connected");
    let said = json!({ "shuttle": "connected", "id": s.id, "name": s.name, "paused": s.paused.is_some(),
                       "input": ed.input_seq, "frame": ed.frames });
    let _ = reply.send(said.to_string());
}

/// Every event: a hello whose card went unanswered (dropped with its session, lost to a restart) hears so at
/// once — it isn't left waiting out `HELLO_WAIT`. Nothing asking: a branch.
pub fn asking_sync(ed: &mut Editor) {
    if ed.shuttles.asking.is_empty() {
        return;
    }
    let up = |id: u64| {
        std::iter::once(&ed.session)
            .chain(&ed.sessions)
            .flat_map(|l| &l.parked)
            .chain(&ed.offers)
            .any(|o| o.id == id)
    };
    let (gone, kept): (Vec<_>, Vec<_>) =
        std::mem::take(&mut ed.shuttles.asking).into_iter().partition(|a| !up(a.0));
    ed.shuttles.asking = kept;
    went_unanswered(ed, gone, "its card went unanswered");
}

/// tarae restarts (`restart::exec`): every hello waiting on its card hears so before the process is replaced —
/// the card is lost with it.
pub fn restarting(ed: &mut Editor) {
    let waiting = std::mem::take(&mut ed.shuttles.asking);
    went_unanswered(ed, waiting, "its card went unanswered — tarae restarts");
}

/// Hellos whose card went unanswered: each told, and said in `:messages`.
fn went_unanswered(ed: &mut Editor, hellos: Vec<(u64, Hello, Reply)>, why: &str) {
    for (card, h, reply) in hellos {
        let r = refused("no", format!("{why} — say hello again"));
        let _ = reply.send(r.json(ed).to_string());
        ed.recorded(crate::editor::ToastKind::Info, format!("shuttle {}: {why} (card#{card})", h.name));
    }
}

/// The process a grant went to ended: the grant goes, and its shuttle with it.
pub fn process_ended(ed: &mut Editor, of: For) {
    ed.shuttles.grants.retain(|g| *g != of);
    match ed.shuttles.on.take_if(|s| s.of == of) {
        Some(s) => ed.recorded(
            crate::editor::ToastKind::Info,
            format!("shuttle {} ended — its process (pid {}) is gone", s.name, s.of.pid),
        ),
        None => ed.unchanged = true,
    }
}

/// `:shuttle-end` (a click on its chip): the shuttle cut off, and its grant — its next hello asks again.
pub fn end(ed: &mut Editor) {
    let Some(s) = ed.shuttles.on.take() else {
        return ed.set_status("no shuttle — no program types into tarae");
    };
    ed.shuttles.grants.retain(|g| *g != s.of);
    ed.set_status(format!(
        "shuttle {} ended — its grant with it: if it says hello again, a card asks you",
        s.name
    ));
}

/// A process you granted is waited on, on a thread of its own, until it ends (no polling: an idle tarae sleeps).
fn watch_end(ed: &Editor, of: For) {
    // (tests end a grant's process by hand: `process_ended` — and a simulation's threads are its own)
    if cfg!(test) {
        return;
    }
    let tx = ed.events.shared_sender();
    let _ = std::thread::Builder::new().name("tarae-shuttle-for".into()).spawn(move || {
        // (gone already, or another process now at its pid: ended)
        if crate::procs::read(of.pid).is_some_and(|p| p.started == of.started) {
            wait_gone(of.pid);
        }
        let _ = tx.send(crate::event::Event::Job(Box::new(move |ed: &mut Editor| process_ended(ed, of))));
    });
}

/// Returns once process `pid` has ended (kqueue's `NOTE_EXIT`).
#[cfg(target_os = "macos")]
fn wait_gone(pid: u32) {
    // SAFETY: a kqueue of our own, one event in and one out of the sizes given; closed after.
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return;
        }
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = pid as usize;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        let mut out: libc::kevent = std::mem::zeroed();
        while libc::kevent(kq, &ev, 1, &mut out, 1, std::ptr::null()) < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        libc::close(kq);
    }
}

/// Returns once process `pid` has ended (its pidfd readable).
#[cfg(target_os = "linux")]
fn wait_gone(pid: u32) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: pidfd_open of a pid; a descriptor it gives is ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return;
    }
    // SAFETY: just made.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one live pollfd.
    while unsafe { libc::poll(&mut p, 1, -1) } < 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
    {}
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn wait_gone(_pid: u32) {
    loop {
        std::thread::park();
    }
}

// ── You ─────────────────────────────────────────────────────────────────────────────

/// A key, click or paste of yours (`Editor::input_came`): a shuttle typing pauses — said once, in `:messages`;
/// its chip says it while it lasts. Never a key of yours refused or held back for it.
pub fn person_typed(ed: &mut Editor) {
    let now = crate::clock::now();
    ed.shuttles.person_at = Some(now);
    let Some(s) = ed.shuttles.on.as_mut().filter(|s| s.paused.is_none()) else { return };
    let at = crate::log::stamp(crate::clock::system());
    let at = at.get(11..).unwrap_or(&at).to_string();
    s.paused = Some((now, at));
    let said = format!(
        "shuttle {} paused — you're typing; it goes on once you've been idle 5 s and it asks",
        s.name
    );
    ed.recorded(crate::editor::ToastKind::Info, said);
}

/// `{"resume": true}`: a paused shuttle goes on — once you've been idle `QUIET`. Its answer.
pub fn resume(ed: &mut Editor, id: u64) -> Value {
    ed.unchanged = true;
    let Some(s) = ed.shuttles.on.as_ref().filter(|s| s.id == id) else {
        return refused("ended", "this shuttle was ended — say hello again").json(ed);
    };
    if s.paused.is_none() {
        return json!({ "shuttle": "going", "input": ed.input_seq });
    }
    let idle = ed.shuttles.person_at.map_or(QUIET, crate::clock::since);
    if idle < QUIET {
        let mut r = paused_refusal(ed);
        r.retry_after_ms = Some((QUIET - idle).as_millis() as u64);
        return r.json(ed);
    }
    let s = ed.shuttles.on.as_mut().expect("there");
    s.paused = None;
    let said = format!("◦ shuttle {} resumed — it types again", s.name);
    ed.unchanged = false;
    ed.set_status(said);
    json!({ "shuttle": "resumed", "input": ed.input_seq })
}

fn paused_refusal(ed: &Editor) -> Refused {
    let at = ed.shuttles.on.as_ref().and_then(|s| s.paused.as_ref()).map_or("", |p| p.1.as_str());
    refused("paused", format!("you typed at {at} — resume once you've been idle 5 s"))
}

/// The chip's and the state's word on it: its name, and whether your key paused it.
pub fn keys_said(ed: &Editor) -> Option<(String, bool)> {
    let s = ed.shuttles.on.as_ref().filter(|_| ed.config.shuttle_keys != Level::Off)?;
    Some((s.name.clone(), s.paused.is_some()))
}

/// When the chip's last step goes back to faint — one frame then (`Editor::tick_wanted`).
pub fn lit_wake(ed: &Editor) -> Option<Duration> {
    let (_, at) = ed.shuttles.on.as_ref()?.last.as_ref()?;
    Some(LIT.checked_sub(crate::clock::since(*at))? + Duration::from_millis(2))
}

/// The chip's last step, while it's lit.
pub fn lit(ed: &Editor) -> Option<&str> {
    let (said, at) = ed.shuttles.on.as_ref()?.last.as_ref()?;
    (crate::clock::since(*at) < LIT).then_some(said.as_str())
}

// ── A step ──────────────────────────────────────────────────────────────────────────

/// A step, on the main loop (`uisocket.rs` sends it as one job): checked and handled in this one turn, answered
/// after the frame that draws what it did — at once when nothing of it landed.
pub fn step(ed: &mut Editor, s: Step, reply: Reply) {
    let (said, landed) = run(ed, s);
    if !landed {
        ed.unchanged = true;
        let _ = reply.send(said.to_string());
        return;
    }
    let until = crate::clock::now() + crate::uisocket::WAIT;
    let what = crate::uistate::Asked::Shuttle(said.to_string());
    ed.ui_asks.push(crate::uistate::Ask { after_input: ed.input_seq, json: true, reply, until, what });
}

/// A step checked, then its keys (or text) handed in — its answer, and whether any of it landed.
pub fn run(ed: &mut Editor, s: Step) -> (Value, bool) {
    if let Err(r) = may_step(ed, &s) {
        return (r.json(ed), false);
    }
    let name = ed.shuttles.on.as_ref().map(|s| s.name.clone()).unwrap_or_default();
    let to = ed.keys_to().to;
    let (said_before, toasts_before) = (ed.offer_said.clone(), ed.said);
    let gone_before = ed.ui_card_how.clone();
    ed.shuttles.stepping = Some(name);
    let result = hand_in(ed, &s.input, &to);
    ed.shuttles.stepping = None;
    let (took, done) = match result {
        Ok(took) => (took, None),
        Err((0, r)) => return (r.json(ed), false),
        Err((done, r)) => (None, Some((done, r))),
    };
    let kind = match &s.input {
        Input::Keys(_, said) => {
            if let Some(sh) = ed.shuttles.on.as_mut() {
                sh.last = Some((format!("{said} → {to}"), crate::clock::now()));
            }
            STEPS[0]
        }
        Input::Text(_) => {
            if let Some(sh) = ed.shuttles.on.as_mut() {
                sh.last = Some((format!("text → {to}"), crate::clock::now()));
            }
            STEPS[1]
        }
    };
    ed.count(crate::features::Area::Shuttle, kind);
    // (stopped partway: what landed, and why the rest didn't)
    if let Some((done, r)) = done {
        let mut v = r.json(ed);
        v["landed_keys"] = json!(done);
        return (v, true);
    }
    // What it did, said: the card's refusal (the key swallowed — never reported as landed), how a card went, the
    // message it brought
    let mut v = json!({ "landed": to, "took": took.map(|h| h.name()), "input": ed.input_seq });
    if let Some(said) = ed.offer_said.as_ref().filter(|s| Some(*s) != said_before.as_ref()) {
        let mut swallowed =
            json!({ "to": to, "took": "card", "taken": false, "said": said.2, "input": ed.input_seq });
        swallowed["keys"] = crate::uistate::keys_json(&ed.keys_to());
        return (swallowed, true);
    }
    // (the oldest card first: two gone at once say the same one every run)
    let mut went: Vec<(&u64, &String)> =
        ed.ui_card_how.iter().filter(|(id, how)| gone_before.get(id) != Some(how)).collect();
    crate::sorting::sort(&mut went);
    let went: Vec<String> = went.into_iter().map(|(id, how)| format!("card#{id} went: {how}")).collect();
    if let Some(w) = went.first() {
        v["said"] = json!(w);
    } else if ed.said != toasts_before
        && let Some(t) = ed.toasts.last()
    {
        v["said"] = json!(t.text);
    }
    (v, true)
}

/// The guards before anything: the setting, the shuttle still there, not paused, an `expect` that holds.
fn may_step(ed: &Editor, s: &Step) -> Result<(), Refused> {
    if ed.config.shuttle_keys == Level::Off {
        return Err(refused("off", "ui.shuttle-keys is off"));
    }
    let Some(sh) = ed.shuttles.on.as_ref().filter(|sh| sh.id == s.id) else {
        return Err(refused("ended", "this shuttle was ended — say hello again"));
    };
    if sh.paused.is_some() {
        return Err(paused_refusal(ed));
    }
    if let Some(why) = unmet(ed, &s.expect) {
        return Err(refused("expect", why));
    }
    Ok(())
}

/// The keys handed to `handle_key` one by one — each checked where it goes first (a card it may not answer, a
/// `:` line it may not run), and every one after the first still going to `to`, where the step began. The last
/// key's handler — or how many landed before it stopped, and why.
fn hand_in(
    ed: &mut Editor,
    input: &Input,
    to: &str,
) -> Result<Option<crate::uistate::Handler>, (usize, Refused)> {
    // (each key and the paste an event of its own, as a person's: what runs after every event — the focus, the
    // views, the syncs — runs between them, so the next key's place is the one a person's would find)
    let keys = match input {
        Input::Text(text) => {
            if let Some(r) = config_refused(ed, ed.keys_to().region) {
                return Err((0, r));
            }
            ed.handle_event(crate::event::Event::Paste(text.clone()));
            return match ed.shuttles.blocked.take() {
                Some((reason, why)) => Err((1, refused(reason, why))),
                None => Ok(None),
            };
        }
        Input::Keys(keys, _) => keys,
    };
    let mut took = None;
    for (i, &key) in keys.iter().enumerate() {
        let now = ed.keys_to();
        if i > 0 && now.to != to {
            // (a sequence stays where it began: what was under way is let go, not finished elsewhere)
            ed.pending.clear();
            ed.count = None;
            let why = format!(
                "after {i} of {} keys they go to {}, not {to} — the rest not sent",
                keys.len(),
                now.to
            );
            return Err((i, refused("moved", why)));
        }
        if let Some(r) = key_refused(ed, &now, key) {
            if i > 0 {
                ed.pending.clear();
                ed.count = None;
            }
            return Err((i, r));
        }
        crate::uistate::TOOK.set(None);
        ed.handle_event(crate::event::Event::Key(key));
        took = crate::uistate::TOOK.get();
        // (a key bound to a `:` line that changes what tarae may do: refused where it runs)
        if let Some((reason, why)) = ed.shuttles.blocked.take() {
            return Err((i + 1, refused(reason, why)));
        }
    }
    Ok(took)
}

/// A `:` line about to run (`typed::execute` — typed, a key bound to it, the palette): during a shuttle's step,
/// why it may not — said where the person looks, and the step stops.
pub fn typed_refused(ed: &mut Editor, line: &str) -> Option<String> {
    let name = ed.shuttles.stepping.clone()?;
    let (reason, why) = match line_refused(line) {
        Some(why) => ("never", why),
        None => ("config", config_line_refused(ed, line)?),
    };
    let why = format!("{why} — not from shuttle {name}");
    ed.shuttles.blocked = Some((reason, why.clone()));
    Some(why)
}

/// Why a shuttle may not send `key` where it goes now (`keys`): a card's answer that's yours, a `:` line that
/// changes what tarae may do.
fn key_refused(ed: &Editor, keys: &crate::uistate::KeysTo, key: Key) -> Option<Refused> {
    // (the card the key reaches: where it routes — or the front one, whose letters `offer_key` takes before the
    // code whenever it has the keys, its answers listed or not)
    let card = match keys.route(key) {
        crate::uistate::Region::Card(id) => ed.offers.iter().find(|o| o.id == id),
        _ if ed.card_has_keys() => ed.offers.first(),
        _ => None,
    };
    if let Some(r) = card.and_then(|o| card_refused(o, key)) {
        return Some(r);
    }
    // (a key that lands in a config file's buffer — any key there: tarae isn't what edits those for a program)
    if let Some(r) = config_refused(ed, keys.route(key)) {
        return Some(r);
    }
    // (what submits the line, as the prompt itself decides it — Enter with any modifier)
    if crate::editor::prompt_submits(key)
        && let Some(p) = ed.prompt.as_ref().filter(|p| p.kind == crate::editor::PromptKind::Command)
        && keys.route(key) == Region::Prompt
    {
        if let Some(why) = line_refused(&p.text) {
            return Some(refused("never", why));
        }
        if let Some(why) = config_line_refused(ed, &p.text) {
            return Some(refused("config", why));
        }
    }
    None
}

/// Why a shuttle may not give card `o` key `key`: any card's `n` (and `v`, its text) it may, and a letter the
/// card doesn't act on (`Offer::acts_on` — it goes on to the code); a light card's other answers too, but those
/// that outlive the session (`A` always, `s` the session, `N` never again, `$` who pays); never a card whose yes
/// widens trust; a weighty card's answers stay yours for now (`Y` on its group too).
fn card_refused(o: &crate::offer::Offer, key: Key) -> Option<Refused> {
    let c = key.plain_char()?;
    if matches!(c, 'n' | 'v') || !o.acts_on(c) {
        return None;
    }
    let card = format!("card#{}", o.id);
    if NEVER.contains(&o.kind()) || (o.kind() == "protected-made" && c == 'y') {
        return Some(refused(
            "never",
            format!("{card} ({}) decides what tarae may do from now on — yours only", o.kind()),
        ));
    }
    if matches!(c, 'A' | 's' | 'N' | '$') {
        return Some(refused("never", format!("{c} on {card} outlasts this session — yours only")));
    }
    if o.weighty() {
        return Some(refused(
            "weighty",
            format!(
                "{card} is weighty — its answers are yours (a shuttle's need ui.shuttle-keys = weighty and read, not built yet)"
            ),
        ));
    }
    None
}

/// Why a shuttle may not run `:` line `line`: saving a setting, changing one a project can't (or can only once
/// trusted), what only your own keys run, the sandbox's grants.
fn line_refused(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    let cmd = words.next()?;
    let setting = words.next().unwrap_or_default();
    let why = match cmd {
        "set!" => "saves to your config",
        "set" | "toggle" if crate::trust::yours_only(setting) => "a setting only your own config sets",
        "set" | "toggle" if crate::trust::is_risky(setting) => {
            "a setting that runs something or loosens a guard"
        }
        "sandbox-blocked" => "its choices grant the sandbox's places",
        c if crate::trust::YOURS_ONLY_COMMANDS.contains(&c) => "a command only your own keys run",
        _ => return None,
    };
    Some(format!(":{} — {why}: yours only", line.trim()))
}

// ── Config files ────────────────────────────────────────────────────────────────────────────────
//
// A shuttle's keys never edit tarae's config: your folder's files (`config.toml`, keys, themes) and a project's
// `.tarae.toml` / `.tarae.local.toml`. That keeps tarae from being the tool that does it for a program — not the
// program from doing it: it runs as you and can write those files itself.

/// Your config folder, as written and as its real path (a dotfiles link) — on the socket's thread, at hello.
pub fn config_dirs() -> Vec<PathBuf> {
    crate::config::config_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(dirs_of)
        .unwrap_or_default()
}

/// Folder `dir` as written, and its real path when that's another.
fn dirs_of(dir: PathBuf) -> Vec<PathBuf> {
    let real = std::fs::canonicalize(&dir).ok().filter(|r| *r != dir);
    std::iter::once(dir).chain(real).collect()
}

/// Whether `path` (a buffer's — a real path — or one a `:` line names) is one of tarae's config files: in your
/// config folder (`dirs`), or a project's `.tarae.toml` / `.tarae.local.toml` — by name, wherever it is.
fn is_config(dirs: &[PathBuf], path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == ".tarae.toml" || n == ".tarae.local.toml")
        || dirs.iter().any(|d| path.starts_with(d))
}

fn config_said(path: &Path) -> String {
    format!("config: yours to edit — {} is tarae's config", path.display())
}

/// Why a shuttle's key or paste may not go to `to`: it lands in the buffer of one of tarae's config files.
fn config_refused(ed: &Editor, to: Region) -> Option<Refused> {
    if !matches!(to, Region::Code | Region::EditReview) {
        return None;
    }
    let dirs = &ed.shuttles.on.as_ref()?.config;
    let path = ed.doc().path.as_deref().filter(|p| is_config(dirs, p))?;
    Some(refused("config", config_said(path)))
}

/// Why a shuttle may not run `:` line `line` for tarae's config files: `:config-open`, one a line opens or
/// writes to (`:e`, `:o`, a split's, `:w <path>`), a config buffer written (`:w`, `:wa`).
fn config_line_refused(ed: &Editor, line: &str) -> Option<String> {
    let dirs = ed.shuttles.on.as_ref().map(|s| s.config.clone()).unwrap_or_default();
    let line = line.trim();
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let said = |p: &Path| Some(format!(":{line} — {}", config_said(p)));
    let named: Vec<String> = match cmd {
        "config-open" => return Some(format!(":{line} — config: yours to edit")),
        "o" | "open" | "e" | "edit" => crate::typed::shell_words(rest),
        "vs" | "vsplit" | "hs" | "sp" | "hsplit" | "split" => {
            Some(rest.to_string()).filter(|r| !r.is_empty()).into_iter().collect()
        }
        "w" | "write" | "w!" | "write!" | "wq" | "x" | "write-quit" => {
            match crate::typed::shell_words(rest).into_iter().next() {
                Some(p) => vec![p],
                None => return ed.doc().path.as_deref().filter(|p| is_config(&dirs, p)).and_then(said),
            }
        }
        "wa" | "write-all" | "wqa" | "xa" | "write-quit-all" => {
            let docs = ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.as_deref());
            return docs.into_iter().find(|p| is_config(&dirs, p)).and_then(said);
        }
        _ => return None,
    };
    // (each as the line takes it — home spelled out, from the root, its real path; `path:12:3` a place in it)
    let resolved = |p: &str| {
        let t = crate::typed::typed_path(ed, p);
        std::fs::canonicalize(&t).or_else(|_| std::path::absolute(&t)).unwrap_or(t)
    };
    for p in &named {
        let mut forms = vec![p.as_str()];
        for _ in 0..2 {
            let last = forms.last().copied().unwrap_or_default();
            match last.rsplit_once(':') {
                Some((head, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => forms.push(head),
                _ => break,
            }
        }
        if let Some(p) = forms.iter().map(|f| resolved(f)).find(|p| is_config(&dirs, p)) {
            return said(&p);
        }
    }
    None
}

// ── Expect ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
thread_local!(
    /// The check skipped — `the_routing_test_sees_each_case_broken` breaks it once and sees the test fail.
    pub static SKIP_EXPECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }
);

/// What `expect` says that doesn't hold now — None: all of it does. `keys` is compared with where the keys go
/// (`keys_to`), every other id with that element's fields as the state shows them (`ui_state`), now.
pub fn unmet(ed: &Editor, expect: &Value) -> Option<String> {
    #[cfg(test)]
    if SKIP_EXPECT.get() {
        return None;
    }
    let Some(map) = expect.as_object().filter(|m| !m.is_empty()) else {
        return Some("no expect — every step says where it goes: {\"keys\": {\"to\": …}}".into());
    };
    let Some(keys) = map.get("keys").and_then(Value::as_object).filter(|k| k.contains_key("to")) else {
        return Some("no keys.to — every step says where it goes: {\"keys\": {\"to\": …}}".into());
    };
    let now = crate::uistate::keys_json(&ed.keys_to());
    for (field, want) in keys {
        let have = &now[field];
        if have.is_null() && !want.is_null() && now.get(field).is_none() {
            return Some(format!("keys have no {field}"));
        }
        if have != want {
            return Some(match field.as_str() {
                "to" => format!("the keys go to {}, not {}", plain(have), plain(want)),
                _ => format!("keys.{field} is {}, not {}", plain(have), plain(want)),
            });
        }
    }
    // (the state built only when an element is expected)
    if map.len() == 1 {
        return None;
    }
    let state = ed.ui_state().json();
    for (id, fields) in map.iter().filter(|(id, _)| *id != "keys") {
        let Some(el) = find(&state, id) else {
            return Some(format!("no {id} in sight"));
        };
        let Some(fields) = fields.as_object() else {
            return Some(format!("{id}: expect its fields, {{\"rev\": 3}}"));
        };
        for (field, want) in fields {
            let Some(have) = el.get(field) else {
                return Some(format!("{id} has no {field}"));
            };
            if have != want {
                return Some(format!("{id}'s {field} is {} — you expected {}", plain(have), plain(want)));
            }
        }
    }
    None
}

/// A value as said: a string without its quotes.
fn plain(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// The element of the state whose `id` is `id` — not the inbox's or the gone ring's word about a card.
fn find<'v>(v: &'v Value, id: &str) -> Option<&'v Value> {
    match v {
        Value::Object(m) => {
            if m.get("id").and_then(Value::as_str) == Some(id) {
                return Some(v);
            }
            m.iter().filter(|(k, _)| !matches!(k.as_str(), "inbox" | "gone")).find_map(|(_, x)| find(x, id))
        }
        Value::Array(a) => a.iter().find_map(|x| find(x, id)),
        _ => None,
    }
}

/// A shuttle connected in a test: `name`, granted, at `level` — its id.
#[cfg(test)]
pub fn connect_for_test(ed: &mut Editor, name: &str, level: Level) -> u64 {
    ed.config.shuttle_keys = level;
    ed.shuttles.above = Some(vec![u32::MAX]);
    let of = For { pid: std::process::id(), started: 1 };
    ed.shuttles.grants.push(of);
    let h = Hello {
        name: name.into(),
        of,
        what: "test".into(),
        exe: String::new(),
        peer_what: "test".into(),
        peer_exe: String::new(),
        line: vec![of.pid],
        above: Vec::new(),
        config: Vec::new(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    hello(ed, h, tx);
    let said: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    said["id"].as_u64().unwrap_or_else(|| panic!("connected: {said}"))
}

#[cfg(test)]
mod tests;
