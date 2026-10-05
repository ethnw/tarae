//! What Claude's cards ask and how they're answered, measured (P0 of `docs/design/cards.md`): one `card`
//! line in `tarae.log` per answer — the session, the kind of card, why today's rules asked (`Why`), where a
//! read goes (`Class`), what the capability model would do instead (`decide` — the one function P3 decides
//! by, so what's logged now is what it will do), the answer, and how long after the card was first drawn:
//!
//! `size · run · why=not-a-check:awk · shadow=none · a · 900ms · up 4.2s`
//!
//! And one `denied` line per sandboxed run the sandbox denied something — what the capability model turns
//! into a grant asked for.

use std::path::Path;
use std::time::Instant;

use crate::offer::{Offer, What};

/// Where a read goes, as the capability model sorts places (`class`) — most telling last, the order a
/// line lists them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Another checkout of the repository (`allow::checkouts`).
    OtherCheckout,
    /// The temp folders.
    Temp,
    /// The toolchains' sources, downloaded crates, the project's build folders.
    Toolchain,
    /// Elsewhere in home.
    Home,
    /// Outside home.
    Elsewhere,
    /// Where keys are (`allow::SECRETS`).
    Secret,
    /// Where it leads can't be told (a `..` past a wildcard).
    Unknown,
}

impl Class {
    fn word(self) -> &'static str {
        match self {
            Class::OtherCheckout => "other-checkout",
            Class::Temp => "temp",
            Class::Toolchain => "toolchain",
            Class::Home => "home",
            Class::Elsewhere => "elsewhere",
            Class::Secret => "secret",
            Class::Unknown => "unknown",
        }
    }

    /// Whether the capability model reads it without asking.
    fn by_default(self) -> bool {
        matches!(self, Class::OtherCheckout | Class::Temp | Class::Toolchain)
    }
}

/// Where `at` (a real path; None: can't be told) is, for a project whose unasked places are `d`.
pub fn class(at: Option<&Path>, d: &crate::allow::Defaults, home: Option<&Path>) -> Class {
    let Some(at) = at else { return Class::Unknown };
    let within = |v: &[std::path::PathBuf]| v.iter().any(|p| at.starts_with(p));
    if crate::allow::secret(at, home) {
        Class::Secret
    } else if within(&d.checkouts) {
        Class::OtherCheckout
    } else if within(&d.temp) {
        Class::Temp
    } else if within(&d.code) {
        Class::Toolchain
    } else if home.is_some_and(|h| at.starts_with(h)) {
        Class::Home
    } else {
        Class::Elsewhere
    }
}

/// The classes of `places`, each once, in `Class`'s order.
pub fn classes(places: &[std::path::PathBuf], d: &crate::allow::Defaults) -> Vec<Class> {
    let home = crate::allow::home();
    let mut v: Vec<Class> = places.iter().map(|p| class(Some(p), d, home.as_deref())).collect();
    v.sort();
    v.dedup();
    v
}

/// Why today's rules put it to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// A command none of the rules lets through — the program of its first part that isn't a check, an
    /// allowed pattern or a read.
    NotACheck(String),
    /// A line the shell reader doesn't read — what in it (`readonly::unreadable`: `$`, a redirect to a file,
    /// a newline …).
    Unreadable(&'static str),
    /// It runs a script, or a file of code through an interpreter (#117).
    Script,
    /// A script the chat allowed isn't as it was (#93).
    ScriptChanged,
    /// It sets names in front of the command (#44, #114).
    EnvName,
    /// It passes an option that makes a command write or run what it names (`tools::WRITES`: `--config`,
    /// `--output` …) — asked each time, whatever the chat allowed (#169).
    Writes(&'static str),
    /// A glob where a command's words must be plain.
    Glob,
    /// It reads outside the project, where nothing allowed it (its places' classes).
    ReadsOutside(Vec<Class>),
    /// Where it reads can't be told — a `cd` into what isn't one folder, … (`readonly::outside`'s Err).
    CdUnknown(String),
    /// A check or git that would run outside the project.
    RunsOutside,
    /// Claude asked for one unix socket past the sandbox (#119).
    Socket,
    /// Claude asked to run it outside the sandbox.
    Outside,
    /// There's no sandbox here, or one that goes only so far.
    NoSandbox,
    /// Claude's Read, Grep or Glob outside the project.
    Read,
    /// Claude's `screen`.
    Screen,
    /// The user opening what the sandbox blocked (`blocked.rs`).
    Blocked,
}

impl Why {
    fn word(&self) -> String {
        match self {
            Why::NotACheck(p) => format!("not-a-check:{p}"),
            Why::Unreadable(_) => "unreadable".into(),
            Why::Script => "script".into(),
            Why::ScriptChanged => "script-changed".into(),
            Why::EnvName => "env-name".into(),
            Why::Writes(o) => format!("writes:{o}"),
            Why::Glob => "glob".into(),
            Why::ReadsOutside(_) => "reads-outside".into(),
            Why::CdUnknown(_) => "cd-unknown".into(),
            Why::RunsOutside => "runs-outside".into(),
            Why::Socket => "socket".into(),
            Why::Outside => "outside".into(),
            Why::NoSandbox => "no-sandbox".into(),
            Why::Read => "read".into(),
            Why::Screen => "screen".into(),
            Why::Blocked => "blocked".into(),
        }
    }

    /// Why a run card asks, in one line (#164) — None where the card says it already in its own words (what
    /// it reads outside, the sandbox it goes past, no sandbox here) or isn't a run's.
    pub fn said(&self) -> Option<String> {
        Some(match self {
            Why::NotACheck(p) => {
                format!("It runs {p} — not a check, a read, or a pattern allowed in this chat.")
            }
            Why::Unreadable(what) => format!("What it does can't be told from its words: {what}."),
            Why::Script => "It runs a script — what runs is in the file.".into(),
            Why::ScriptChanged => "A script it runs isn't as it was when this chat allowed it.".into(),
            Why::EnvName => "It sets names in front of the command.".into(),
            Why::Writes(o) => format!(
                "It passes {o}, which can make it write or run what that names — asked each time, nothing \
                 in this chat allows it."
            ),
            Why::Glob => "A pattern in a command that isn't a read — what it runs on can't be told.".into(),
            Why::CdUnknown(why) => format!("Where it reads can't be told — {why}."),
            Why::RunsOutside => {
                "It runs a check or git outside the project — that folder's code runs.".into()
            }
            Why::ReadsOutside(_)
            | Why::Socket
            | Why::Outside
            | Why::NoSandbox
            | Why::Read
            | Why::Screen
            | Why::Blocked => return None,
        })
    }
}

/// Past the sandbox, a command asked to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Past {
    No,
    /// One unix socket.
    Socket,
    /// Outside it whole.
    Outside,
}

/// What a grant opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Read,
    Write,
    Port,
}

/// What a card asks for, in the capability model's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A command of Claude's: how far past the sandbox, whether there's one here, and the classes of what it
    /// reads outside the project.
    Run { past: Past, sandboxed: bool, reads: Vec<Class> },
    /// Claude's Read, Grep or Glob of a place outside the project.
    Read(Class),
    /// Claude's `screen`.
    Screen,
    /// The sandbox opened for what it blocked: a place to read, a folder to write in, a port — or one socket.
    Grant(crate::allow::Grant),
}

/// What the capability model does with a request (docs/design/cards.md §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Runs or reads without asking — inside the session's capabilities.
    None,
    /// A grant card for this.
    Grant(Verb),
    Socket,
    Outside,
    NoSandbox,
    Screen,
}

impl Decision {
    fn word(self) -> &'static str {
        match self {
            Decision::None => "none",
            Decision::Grant(Verb::Read) => "grant:read",
            Decision::Grant(Verb::Write) => "grant:write",
            Decision::Grant(Verb::Port) => "grant:port",
            Decision::Socket => "socket",
            Decision::Outside => "outside",
            Decision::NoSandbox => "no-sandbox",
            Decision::Screen => "screen",
        }
    }
}

/// The capability model's decision: any command runs inside the session's capabilities, whatever it is —
/// with no sandbox to keep it there, it asks; past the sandbox, it asks; a read outside the places read by
/// default asks for that read (a command's, once the sandbox confines reads, from its denial).
pub fn decide(r: &Request) -> Decision {
    let reads = |c: &[Class]| match c.iter().all(|c| c.by_default()) {
        true => Decision::None,
        false => Decision::Grant(Verb::Read),
    };
    match r {
        Request::Run { sandboxed: false, .. } => Decision::NoSandbox,
        Request::Run { past: Past::Outside, .. } => Decision::Outside,
        Request::Run { past: Past::Socket, .. } => Decision::Socket,
        Request::Run { reads: c, .. } => reads(c),
        Request::Read(c) => reads(std::slice::from_ref(c)),
        Request::Screen => Decision::Screen,
        Request::Grant(crate::allow::Grant::Read(_)) => Decision::Grant(Verb::Read),
        Request::Grant(crate::allow::Grant::Write(_)) => Decision::Grant(Verb::Write),
        Request::Grant(crate::allow::Grant::Port(_)) => Decision::Grant(Verb::Port),
        Request::Grant(crate::allow::Grant::Socket(_)) => Decision::Socket,
    }
}

/// What a card of Claude's (or about Claude's commands) asked: why, and the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    pub why: Why,
    pub request: Request,
}

impl Asked {
    pub fn new(why: Why, request: Request) -> Asked {
        Asked { why, request }
    }
}

/// The card's kind, as the line says it.
fn kind(w: &What) -> &'static str {
    match w {
        What::Run(_) => "run",
        What::NoSandbox { .. } => "no-sandbox",
        What::Read { .. } => "read",
        What::Screen { .. } => "screen",
        What::Grant { .. } => "grant",
        What::Outside { .. } => "outside",
        _ => "other",
    }
}

/// The `card` line: `<session> · <kind> · why=… · class=… · shadow=… · <answer> · <drawn → answered> · up
/// <asked → answered>` (`class` only for reads).
pub fn line(session: &str, o: &Offer, asked: &Asked, answer: &str, now: Instant) -> String {
    let classes: Vec<Class> = match &asked.request {
        Request::Run { reads, .. } => reads.clone(),
        Request::Read(c) => vec![*c],
        _ => Vec::new(),
    };
    let class = match classes.is_empty() {
        true => String::new(),
        false => {
            format!(" · class={}", classes.iter().map(|c| c.word()).collect::<Vec<_>>().join(","))
        }
    };
    let took = |at: Instant| crate::term::duration(now.saturating_duration_since(at).as_secs_f32());
    let drawn = o.drawn.get().map_or_else(|| "not drawn".to_string(), took);
    format!(
        "{session} · {} · why={}{class} · shadow={} · {answer} · {drawn} · up {}",
        kind(&o.what),
        asked.why.word(),
        decide(&asked.request).word(),
        took(o.since)
    )
}

/// Card `o` was answered (`y` `a` `A` `n`) or dropped unanswered: its line, if it says what it asked.
pub fn said(session: &str, o: &Offer, answer: &str) {
    if let Some(asked) = &o.asked {
        logged("card", &line(session, o, asked, answer, Instant::now()));
    }
}

/// The `denied` line for a run the sandbox denied something: `<session> · <what ran> · N things, M times:
/// <each, as Claude reads it>` — None when it denied nothing (or couldn't be watched).
pub fn denied_line(session: &str, what: &str, b: &crate::sandbox::Blocked) -> Option<String> {
    if b.total == 0 {
        return None;
    }
    let mut each: Vec<String> = b.seen.iter().map(crate::sandbox::Denial::reported).collect();
    if b.distinct > b.seen.len() {
        each.push(format!("… and {} more", b.distinct - b.seen.len()));
    }
    let what = what.trim().replace('\n', " ");
    Some(format!(
        "{session} · {what} · {} thing{}, {} time{}: {}",
        b.distinct,
        if b.distinct == 1 { "" } else { "s" },
        b.total,
        if b.total == 1 { "" } else { "s" },
        each.join("; ")
    ))
}

/// A run ended under the sandbox's watch: what it denied, to the log.
pub fn denied(session: &str, what: &str, w: &crate::sandbox::Watch) {
    if let Some(l) = denied_line(session, what, &w.blocked()) {
        logged("denied", &l);
    }
}

fn logged(level: &str, line: &str) {
    crate::log::line("tarae", level, line);
    #[cfg(test)]
    LOGGED.with(|l| l.borrow_mut().push(format!("{level} {line}")));
}

#[cfg(test)]
thread_local! {
    /// (tests: the lines logged on this thread, each after its level — `card …`, `denied …`)
    pub static LOGGED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// (tests: the lines logged on this thread so far that start with `prefix`)
#[cfg(test)]
pub fn logged_lines(prefix: &str) -> Vec<String> {
    LOGGED.with(|l| l.borrow().iter().filter(|x| x.starts_with(prefix)).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// What the capability model asks about: past the sandbox, no sandbox, a read outside the places read
    /// by default, the screen, and what the sandbox blocked — never a command for what it is.
    #[test]
    fn the_capability_model_asks_for_places_not_commands() {
        let run = |past, sandboxed, reads: &[Class]| {
            decide(&Request::Run { past, sandboxed, reads: reads.to_vec() })
        };
        assert_eq!(run(Past::No, true, &[]), Decision::None, "any command, inside");
        assert_eq!(run(Past::No, true, &[Class::OtherCheckout, Class::Temp]), Decision::None);
        assert_eq!(run(Past::No, true, &[Class::Temp, Class::Home]), Decision::Grant(Verb::Read));
        assert_eq!(run(Past::No, false, &[]), Decision::NoSandbox);
        assert_eq!(run(Past::Socket, true, &[]), Decision::Socket);
        assert_eq!(run(Past::Outside, true, &[Class::Toolchain]), Decision::Outside);
        assert_eq!(decide(&Request::Read(Class::Toolchain)), Decision::None);
        assert_eq!(decide(&Request::Read(Class::Secret)), Decision::Grant(Verb::Read));
        assert_eq!(decide(&Request::Read(Class::Unknown)), Decision::Grant(Verb::Read));
        assert_eq!(decide(&Request::Screen), Decision::Screen);
        let g = |g| decide(&Request::Grant(g));
        assert_eq!(g(crate::allow::Grant::Write("/w".into())), Decision::Grant(Verb::Write));
        assert_eq!(g(crate::allow::Grant::Port(5432)), Decision::Grant(Verb::Port));
        assert_eq!(g(crate::allow::Grant::Socket("/s".into())), Decision::Socket);
    }

    /// A place's class: keys first (wherever else they are), then the places read by default, then home,
    /// then the rest.
    #[test]
    fn where_a_read_goes() {
        let home = Path::new("/Users/u");
        let d = crate::allow::Defaults {
            checkouts: vec![PathBuf::from("/Users/u/src/repo")],
            temp: vec![PathBuf::from("/private/tmp")],
            code: vec![PathBuf::from("/Users/u/.cargo/registry/src")],
        };
        let at = |p: &str| class(Some(Path::new(p)), &d, Some(home));
        assert_eq!(at("/Users/u/src/repo/target/x"), Class::OtherCheckout);
        assert_eq!(at("/Users/u/src/repository/x"), Class::Home, "by path parts");
        assert_eq!(at("/private/tmp/shot/a.png"), Class::Temp);
        assert_eq!(at("/Users/u/.cargo/registry/src/x/lib.rs"), Class::Toolchain);
        assert_eq!(at("/Users/u/.cargo/credentials.toml"), Class::Home);
        assert_eq!(at("/Users/u/.ssh/id_ed25519"), Class::Secret);
        assert_eq!(at("/opt/x"), Class::Elsewhere);
        assert_eq!(class(None, &d, Some(home)), Class::Unknown);
    }

    /// The line: the kind from the card, why, the classes of what's read, the model's decision, the answer,
    /// and the time from drawn — or that it never was.
    #[test]
    fn a_cards_line_says_what_it_asked_and_how_it_went() {
        let call = crate::tools::Call::test(1);
        let mut o = Offer::new("t".into(), "q", "", String::new(), What::Screen { call });
        let asked = Asked::new(
            Why::ReadsOutside(vec![Class::Temp, Class::Home]),
            Request::Run { past: Past::No, sandboxed: true, reads: vec![Class::Temp, Class::Home] },
        );
        let t0 = Instant::now();
        o.since = t0;
        let at = |s: u64| t0 + std::time::Duration::from_millis(s);
        assert_eq!(
            line("size", &o, &asked, "n", at(4200)),
            "size · screen · why=reads-outside · class=temp,home · shadow=grant:read · n · not drawn · up 4.2s"
        );
        o.drawn.set(Some(at(3300)));
        let a = Asked::new(
            Why::NotACheck("awk".into()),
            Request::Run { past: Past::No, sandboxed: true, reads: vec![] },
        );
        assert_eq!(
            line("size", &o, &a, "a", at(4200)),
            "size · screen · why=not-a-check:awk · shadow=none · a · 900ms · up 4.2s"
        );
    }

    /// A run's denials as one line — the distinct ones as Claude reads them, past what's kept counted.
    #[test]
    fn a_runs_denials_are_one_line() {
        let mut b = crate::sandbox::Blocked::default();
        assert_eq!(denied_line("s", "x", &b), None, "nothing denied, no line");
        let d = |op: &str, t: &str| crate::sandbox::Denial {
            process: "touch".into(),
            op: op.into(),
            target: t.into(),
        };
        b.add(d("file-write-create", "/opt/a"));
        b.add(d("file-write-create", "/opt/a"));
        assert_eq!(
            denied_line("size", "touch /opt/a\n", &b).unwrap(),
            "size · touch /opt/a · 1 thing, 2 times: writing /opt/a (file-write-create, touch)"
        );
        for i in 0..crate::sandbox::DENIALS_KEPT + 1 {
            b.add(d("file-write-create", &format!("/opt/{i}")));
        }
        let l = denied_line("size", "x", &b).unwrap();
        assert!(l.starts_with("size · x · 12 things, 13 times: ") && l.ends_with("; … and 2 more"), "{l}");
    }
}
