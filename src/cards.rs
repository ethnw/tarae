//! What Claude's cards ask and how they're answered, measured (P0 of `docs/design/cards.md`): one `card`
//! line in `tarae.log` per answer — the session, the kind of card, why today's rules asked (`Why`), where a
//! read goes (`Class`), what the capability model would do instead (`decide` — the one function P3 decides
//! by, so what's logged now is what it will do), the answer, and how long after the card was first drawn:
//!
//! `size · card#12 · run · why=not-a-check:awk · shadow=none · a · 900ms · up 4.2s`
//!
//! (The card's id since cardburden 3: a later card's `same=#12` names it. Since cardburden 5 every card has
//! its line — tarae's own questions as `why=own · shadow=own`.)
//!
//! And one `denied` line per sandboxed run the sandbox denied something — what the capability model turns
//! into a grant asked for.
//!
//! The log keeps two starts at most, and a busy session's oldest lines go first: so every card is also kept in
//! **the record** (cardauto piece 1, D2) — `$state/cards/<yyyy-mm>.jsonl`, an entry a line: the card line's
//! fields, with when, the session and what it was about (a run's command) — and so is every run of Claude's
//! refused with no card (`refused`). Appended on a worker under the folder's lock; on this machine only, in
//! tarae's state, which no run and no claude reads (`sandbox::kept_reads`); `llm.card-record` (your own config
//! only) turns it off. `cargo xtask cards` reads it.

use std::io;
use std::path::Path;
use std::time::{Instant, SystemTime};

use serde_json::{Value, json};

use crate::offer::Offer;

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
    /// Elsewhere in home, or a folder holding home (`/`, `/Users`).
    Home,
    /// Outside home — read by default, as runs read it (D3).
    Elsewhere,
    /// Another user's home (`HOMES`; not `/Users/Shared`), or a folder holding one.
    OtherHome,
    /// A volume mounted where the systems mount them (`VOLUMES`), or a folder holding one — an external disk,
    /// a network share: other projects, their keys (cardauto §8.6).
    Volume,
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
            Class::OtherHome => "other-home",
            Class::Volume => "volume",
            Class::Secret => "secret",
            Class::Unknown => "unknown",
        }
    }
}

/// Where the systems mount other volumes (macOS `/Volumes` — a real path there is never the boot volume's
/// link — and `/Network`; Linux `/media`, `/mnt`, `/run/media`). By the folders, not the mounts themselves:
/// piece 3 (`caps.rs`) asks the system for those.
const VOLUMES: [&str; 5] = ["/Volumes", "/Network", "/media", "/mnt", "/run/media"];
/// Where the systems keep their users' homes — a home there not yours is another user's (`/Users/Shared`,
/// everyone's, isn't).
const HOMES: [&str; 2] = ["/Users", "/home"];

/// Where `at` (a real path; None: can't be told) is, for a session that may do `caps` (its defaults, home, what
/// runs never read). A folder holding a place reads it too: `/` holds home, `/run` the volumes in `/run/media`.
pub fn class(at: Option<&Path>, caps: &crate::caps::Caps) -> Class {
    use crate::caps::Why;
    let Some(at) = at else { return Class::Unknown };
    let home = caps.home.as_deref();
    let within = |why: Why| caps.defaults(why).any(|p| at.starts_with(p));
    // (what runs never read is never a default, outside home too: a moved state home, CARGO_HOME, GOPATH —
    // a folder holding one reads it, but home's class says that already)
    let kept = caps.kept.iter().any(|k| at.starts_with(k))
        || caps.kept.iter().any(|k| k.starts_with(at) && !home.is_some_and(|h| k.starts_with(h)));
    let holds_or_in = |p: &Path| at.starts_with(p) || p.starts_with(at);
    let other_home = HOMES.iter().map(Path::new).any(|h| {
        let first = at.strip_prefix(h).ok().and_then(|r| r.iter().next());
        h.starts_with(at) || first.is_some_and(|f| f != "Shared" && home.is_none_or(|me| !at.starts_with(me)))
    });
    if crate::allow::secret(at, home) || kept {
        Class::Secret
    } else if within(Why::OtherCheckout) {
        Class::OtherCheckout
    } else if within(Why::Temp) {
        Class::Temp
    } else if within(Why::Toolchain) {
        Class::Toolchain
    } else if home.is_some_and(holds_or_in) {
        Class::Home
    } else if VOLUMES.iter().any(|v| holds_or_in(Path::new(v))) {
        Class::Volume
    } else if other_home {
        Class::OtherHome
    } else {
        Class::Elsewhere
    }
}

/// The classes of `places`, each once, in `Class`'s order.
pub fn classes(places: &[std::path::PathBuf], caps: &crate::caps::Caps) -> Vec<Class> {
    let mut v: Vec<Class> = places.iter().map(|p| class(Some(p), caps)).collect();
    crate::sorting::sort(&mut v);
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
    /// Claude's last run made a protected name where none was — what Landlock can't refuse.
    ProtectedMade,
    /// Claude's Read, Grep or Glob outside the project.
    Read,
    /// Claude's `screen`.
    Screen,
    /// Claude's `commit`.
    Commit,
    /// The user opening what the sandbox blocked (`blocked.rs`).
    Blocked,
    /// A host a run's rules don't name, held by tarae's proxy (`hosts.rs`).
    Host,
    /// A warning of it asks each time, whatever was allowed before (`cardwarn.rs`, `asks` — D2, D4): its ids.
    Asks(String),
    /// Claude's `also`: one thing past the session's capabilities its command asks for (cardauto P3).
    Also,
    /// tarae's own question, not about what Claude does (a preflight, a stuck run, notes gone stale, a review's
    /// end …) — logged all the same (cardburden 5), so every card's answer and time are measured.
    Own,
    /// A conversation would bill your API key, or leave it (`billing.rs`: `$` in the session list, the plan's
    /// limit hit) — spending credits, no capability.
    Spend,
}

impl Why {
    pub(crate) fn word(&self) -> String {
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
            Why::ProtectedMade => "protected-made".into(),
            Why::Read => "read".into(),
            Why::Screen => "screen".into(),
            Why::Commit => "commit".into(),
            Why::Blocked => "blocked".into(),
            Why::Host => "host".into(),
            Why::Asks(ids) => format!("asks:{ids}"),
            Why::Also => "also".into(),
            Why::Own => "own".into(),
            Why::Spend => "spend".into(),
        }
    }

    /// Why a run card asks, in one line (#164) — None where the card says it already in its own words (what
    /// it reads outside, the sandbox it goes past, no sandbox here) or isn't a run's.
    pub fn said(&self) -> Option<String> {
        Some(match self {
            Why::NotACheck(p) => {
                format!("It runs {p} — not a check, a read, or a command allowed in this chat.")
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
            Why::Asks(_) => "What it warns of, above, is asked each time, whatever this chat allowed.".into(),
            Why::ReadsOutside(_)
            | Why::Socket
            | Why::Outside
            | Why::NoSandbox
            | Why::ProtectedMade
            | Why::Read
            | Why::Screen
            | Why::Commit
            | Why::Blocked
            | Why::Also
            | Why::Host
            | Why::Own
            | Why::Spend => return None,
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
    Terminals,
    Localhost,
    Env,
}

/// Whether Claude's commands run in a sandbox here: `llm.sandbox` on and one that holds — a partial one (Linux
/// Landlock ABI 2: it can't control truncating files) counts as none.
pub fn sandboxed(on: bool, here: &crate::sandbox::Availability) -> bool {
    use crate::sandbox::Availability;
    on && !matches!(here, Availability::Missing(_) | Availability::Partial(_))
}

/// Whether a sandboxed run of Claude's could reach beyond this machine unasked (D4): the network open — which host a
/// command reaches isn't known before it runs (any, `llm.sandbox-hosts`, a session's or a kept grant may be the one)
/// — or, closed (piece C), with a host this session or conversation let through (`granted`): every other host waits
/// on its own card, which names the command.
pub fn reaches_out(llm: &crate::llm::LlmConfig, granted: bool) -> bool {
    llm.sandbox_network || granted
}

/// What a card asks for, in the capability model's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A command of Claude's: how far past the sandbox, whether there's one here, the classes of what it
    /// reads outside the project, and whether a warning of it asks (`cardwarn.rs` `asks`: publishing,
    /// reaching beyond this machine) where the run could reach out (`reaches_out`) — `outward`.
    Run { past: Past, sandboxed: bool, reads: Vec<Class>, outward: bool },
    /// Claude's Read, Grep or Glob of a place outside the project.
    Read(Class),
    /// Claude's `screen`.
    Screen,
    /// Claude's `commit`: its message and the diff of the paths it names.
    Commit,
    /// The sandbox opened for what it blocked: a place to read, a folder to write in, a port — or one socket.
    Grant(crate::allow::Grant),
    /// A host reached through tarae's proxy (`host:port`).
    Host(String),
    /// tarae's own question (`Why::Own`): no capability asked for.
    Own,
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
    /// A command that publishes or reaches beyond this machine, where it could (D4) — asked each time.
    Outward,
    Screen,
    /// A commit: asked each time, the diff on the card.
    Commit,
    /// A host past the session's (the proxy holds it).
    Host,
    /// tarae's own question: asked as today, the model has nothing to say of it.
    Own,
}

impl Decision {
    fn word(self) -> &'static str {
        match self {
            Decision::None => "none",
            Decision::Grant(Verb::Read) => "grant:read",
            Decision::Grant(Verb::Write) => "grant:write",
            Decision::Grant(Verb::Port) => "grant:port",
            Decision::Grant(Verb::Terminals) => "grant:terminals",
            Decision::Grant(Verb::Localhost) => "grant:localhost",
            Decision::Grant(Verb::Env) => "grant:env",
            Decision::Socket => "socket",
            Decision::Outside => "outside",
            Decision::NoSandbox => "no-sandbox",
            Decision::Outward => "outward",
            Decision::Screen => "screen",
            Decision::Commit => "commit",
            Decision::Host => "host",
            Decision::Own => "own",
        }
    }
}

/// The capability model's decision: any command runs inside the session's capabilities, whatever it is —
/// with no sandbox to keep it there (a partial one too), it asks; past the sandbox, it asks; one that would
/// publish or reach beyond this machine asks where it could (D4) — with the network closed it runs, and fails
/// in the sandbox; a read outside the places read by default asks for that read (a command's, once the
/// sandbox confines reads, from its denial) — outside home is read by default where the session reads it so
/// (`caps`: D3), others' homes and volumes aren't.
pub fn decide(r: &Request, caps: &crate::caps::Caps) -> Decision {
    let reads = |c: &[Class]| match c.iter().all(|c| caps.by_default(*c)) {
        true => Decision::None,
        false => Decision::Grant(Verb::Read),
    };
    match r {
        Request::Run { sandboxed: false, .. } => Decision::NoSandbox,
        Request::Run { past: Past::Outside, .. } => Decision::Outside,
        Request::Run { past: Past::Socket, .. } => Decision::Socket,
        Request::Run { outward: true, .. } => Decision::Outward,
        Request::Run { reads: c, .. } => reads(c),
        Request::Read(c) => reads(std::slice::from_ref(c)),
        Request::Screen => Decision::Screen,
        Request::Commit => Decision::Commit,
        Request::Grant(crate::allow::Grant::Read(_) | crate::allow::Grant::Key(_)) => {
            Decision::Grant(Verb::Read)
        }
        Request::Grant(crate::allow::Grant::Write(_)) => Decision::Grant(Verb::Write),
        Request::Grant(crate::allow::Grant::Port(_)) => Decision::Grant(Verb::Port),
        Request::Grant(crate::allow::Grant::Terminals) => Decision::Grant(Verb::Terminals),
        Request::Grant(crate::allow::Grant::Localhost) => Decision::Grant(Verb::Localhost),
        Request::Grant(crate::allow::Grant::Env(_)) => Decision::Grant(Verb::Env),
        Request::Grant(crate::allow::Grant::Socket(_)) => Decision::Socket,
        Request::Grant(crate::allow::Grant::Host(..)) => Decision::Host,
        Request::Host(_) => Decision::Host,
        Request::Own => Decision::Own,
    }
}

/// The shadow of `r` — `decide`'s word — by the session's `caps`; with none made yet, `unbuilt` where a made one
/// could decide it otherwise (a read's class read by default or not — the only thing `decide` asks of it), never a
/// guess: the shadow is the number auto mode is weighed by (D1).
pub fn shadow(r: &Request, caps: Option<&crate::caps::Caps>) -> &'static str {
    match caps {
        Some(c) => decide(r, c).word(),
        None => decide_now(r).map_or("unbuilt", Decision::word),
    }
}

/// `decide` with no `Caps` made: the decision where none could change it (a run asking for no read past the
/// project — what it may read is the only thing `decide` asks of the session) — None where one could.
pub fn decide_now(r: &Request) -> Option<Decision> {
    use crate::caps::Caps;
    let (off, on) = (decide(r, &Caps::default()), decide(r, &Caps { outside: true, ..Caps::default() }));
    (off == on).then_some(off)
}

/// What a card of Claude's (or about Claude's commands) asked: why, and the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    pub why: Why,
    pub request: Request,
    /// A run card the same as one answered before (`sinceyes`): that card, and whether anything changed since
    /// — `same=#41`, `same=#41:changed` in the line, so its drawn → answered time can be set against the rest.
    pub same: Option<(u64, bool)>,
    /// The warnings it said (`cardwarn.rs`), by id — `warn=docker-own-config,outward` in the line.
    pub warn: Vec<String>,
    /// The run it's about (`RunTrace::id`) — `run#7` in the line, so its `timing` line is found by id.
    pub run: Option<u64>,
    /// What it's about, for the record: a run's command, a read's place as Claude gave it.
    pub what: Option<String>,
    /// Where a run's command runs, from the project's root (None: at it).
    pub cwd: Option<String>,
}

impl Asked {
    pub fn new(why: Why, request: Request) -> Asked {
        Asked { why, request, same: None, warn: Vec::new(), run: None, what: None, cwd: None }
    }

    /// It's about run `run` (`RunTrace::id`), which runs `cmd` in `cwd` (`root`'s: none said).
    pub fn ran(self, run: Option<u64>, cmd: &str, cwd: &Path, root: &Path) -> Asked {
        let cwd = (cwd != root).then(|| crate::chat::shown(root, cwd));
        Asked { run, what: Some(cmd.to_string()), cwd, ..self }
    }

    /// It's about `what` (a read's place as Claude gave it).
    pub fn about(self, what: &str) -> Asked {
        Asked { what: Some(what.to_string()), ..self }
    }

    /// It said these warnings (ids).
    pub fn warned(self, warn: Vec<String>) -> Asked {
        Asked { warn, ..self }
    }

    /// The same as card `card`, answered before — `unchanged`: nothing changed since.
    pub fn same_as(self, card: u64, unchanged: bool) -> Asked {
        Asked { same: Some((card, unchanged)), ..self }
    }
}

/// The classes a request reads (a run's, a Read's) — none for the rest.
fn read_classes(r: &Request) -> Vec<Class> {
    match r {
        Request::Run { reads, .. } => reads.clone(),
        Request::Read(c) => vec![*c],
        _ => Vec::new(),
    }
}

/// The `card` line: `card#<id> · run#<id> · <kind> · why=… · class=… · shadow=… · same=#41 · warn=… ·
/// <answer> · <drawn → answered> · up <asked → answered>` (`run#` only for a card about a run — its trace's id,
/// as its `timing` line names it; the kind as `tarae state` names it, `Offer::kind`; `class` only for reads;
/// `same` only for a run card the same as one answered before — that card's id, `same=#41:changed` when
/// something changed since; `warn` the warnings it said, by id; `by=shuttle:<name>` when a shuttle's key answered
/// it — `shuttle.rs`; a person's says nothing). Its session is the log's column; the shadow is decided by its
/// `caps` (`shadow`).
pub fn line(
    o: &Offer,
    asked: &Asked,
    (answer, by): (&str, Option<&str>),
    now: Instant,
    caps: Option<&crate::caps::Caps>,
) -> String {
    let classes = read_classes(&asked.request);
    let class = match classes.is_empty() {
        true => String::new(),
        false => {
            format!(" · class={}", classes.iter().map(|c| c.word()).collect::<Vec<_>>().join(","))
        }
    };
    let same = match asked.same {
        Some((card, true)) => format!(" · same=#{card}"),
        Some((card, false)) => format!(" · same=#{card}:changed"),
        None => String::new(),
    };
    let same = match asked.warn.is_empty() {
        true => same,
        false => format!("{same} · warn={}", asked.warn.join(",")),
    };
    let took = |at: Instant| crate::term::run_duration(now.saturating_duration_since(at));
    let drawn = o.drawn.get().map_or_else(|| "not drawn".to_string(), took);
    let run = asked.run.map(|r| format!(" · run#{r}")).unwrap_or_default();
    let answer = match by.filter(|b| b.starts_with("shuttle:")) {
        Some(b) => format!("{answer} · by={b}"),
        None => answer.to_string(),
    };
    format!(
        "card#{}{run} · {} · why={}{class} · shadow={}{same} · {answer} · {drawn} · up {}",
        o.id,
        o.kind(),
        asked.why.word(),
        shadow(&asked.request, caps),
        took(o.since)
    )
}

/// Card `o` was answered (`y` `a` `A` `n`, or a letter of its own) or dropped unanswered: its line — what it
/// asked, or `why=own · shadow=own` for tarae's own questions (cardburden 5: a preflight's, a stuck run's, a
/// review's end were never measured). `by`: who answered — `you`, or `shuttle:<name>` (a program's key over the
/// UI socket, `shuttle.rs`); None for a card dropped. Its entry for the record is handed back
/// (`Editor::cards_record`).
pub fn said(
    session: &crate::log::Session,
    o: &Offer,
    (answer, by): (&str, Option<&str>),
    caps: Option<&crate::caps::Caps>,
) -> Value {
    let own = Asked::new(Why::Own, Request::Own);
    let asked = o.asked.as_ref().unwrap_or(&own);
    // (the card's times are on logic's clock: `Offer.since`, `drawn`)
    let now = crate::clock::now();
    logged(session, "card", &line(o, asked, (answer, by), now, caps));
    let mut v = record(o, asked, answer, (now, crate::clock::system()), session.name(), caps);
    if let Some(by) = by {
        v["by"] = json!(by);
    }
    v
}

// ── The record ─────────────────────────────────────────────────────────────────────────────

/// The record's folder in tarae's state: a file a month, `2026-10.jsonl`.
pub const DIR: &str = "cards";
/// The record's version, in each entry (`v`).
const RECORD_V: u64 = 1;

/// Card `o`'s entry: the line's fields as JSON, with when (local, to the second), the session, the card's id
/// and kind, its times in ms (`drawn_ms` absent: never drawn) — and what it was about (`what`, `cwd`), which
/// the line leaves out. Null and empty fields left out.
pub fn record(
    o: &Offer,
    asked: &Asked,
    answer: &str,
    (now, at): (Instant, SystemTime),
    session: Option<&str>,
    caps: Option<&crate::caps::Caps>,
) -> Value {
    let ms = |t: Instant| now.saturating_duration_since(t).as_millis() as u64;
    let mut v = entry(asked, answer, (session, at), caps);
    v["card"] = json!(o.id);
    v["kind"] = json!(o.kind());
    v["drawn_ms"] = json!(o.drawn.get().map(ms));
    v["up_ms"] = json!(ms(o.since));
    pruned(v)
}

/// A run of Claude's refused before any card was up (`tools::showable`: a file it names was written this turn
/// unseen, and the card can't show it): its entry — kind `run`, answer `refused`, no card, and what Claude was
/// told (`said`). The card log never saw these (cardauto §8.3).
pub fn refused(
    asked: &Asked,
    said: &str,
    (session, at): (Option<&str>, SystemTime),
    caps: Option<&crate::caps::Caps>,
) -> Value {
    let mut v = entry(asked, "refused", (session, at), caps);
    v["kind"] = json!("run");
    v["said"] = json!(crate::tokens::redacted(said));
    pruned(v)
}

fn entry(
    asked: &Asked,
    answer: &str,
    (session, at): (Option<&str>, SystemTime),
    caps: Option<&crate::caps::Caps>,
) -> Value {
    let class: Vec<&str> = read_classes(&asked.request).into_iter().map(Class::word).collect();
    json!({
        "v": RECORD_V,
        "at": crate::log::stamp(at),
        "session": session,
        "run": asked.run,
        "why": asked.why.word(),
        "class": class,
        "shadow": shadow(&asked.request, caps),
        "same": asked.same.map(|(c, _)| c),
        "changed": asked.same.and_then(|(_, unchanged)| (!unchanged).then_some(true)),
        "warn": asked.warn,
        "answer": answer,
        // (a token written into a command isn't kept: the record outlives the log by months)
        "what": asked.what.as_deref().map(crate::tokens::redacted),
        "cwd": asked.cwd,
    })
}

/// `v` without its null and empty fields.
fn pruned(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| match x {
            Value::Null => false,
            Value::Array(a) => !a.is_empty(),
            Value::String(s) => !s.is_empty(),
            _ => true,
        });
    }
    v
}

/// The record's file for the month `at` falls in, local time: `2026-10.jsonl`.
pub fn month_file(at: SystemTime) -> String {
    let (y, m, _) = crate::log::day(at);
    month_name(y, m)
}

/// The record's file for month `m` of year `y`.
pub fn month_name(y: i32, m: i32) -> String {
    format!("{y:04}-{m:02}.jsonl")
}

/// How many months of the record are kept, this one included: a year. What it's for — P3's and auto mode's
/// numbers per class (cardauto §4.2) — takes months of use at today's rate of cards, and a class mustn't lose
/// its first months while it's still being judged; past a year a command's text has no use left, and the
/// record is a copy of commands (tokens taken out, `tokens::redacted`) that shouldn't outlive its use.
pub const KEEP_MONTHS: i32 = 12;

/// The months of the record older than `KEEP_MONTHS` before `now`'s, gone — only files named as the record
/// names them (`2025-09.jsonl`), under its lock. When tarae starts, on a worker. The names removed.
pub fn prune(dir: &Path, now: SystemTime) -> io::Result<Vec<String>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let (y, m, _) = crate::log::day(now);
    let month = |name: &str| -> Option<i32> {
        let stem = name.strip_suffix(".jsonl")?;
        let (y, m) = stem.split_once('-')?;
        let ok = y.len() == 4 && m.len() == 2 && stem.bytes().filter(u8::is_ascii_digit).count() == 6;
        ok.then(|| y.parse::<i32>().ok().zip(m.parse::<i32>().ok()))?.map(|(y, m)| y * 12 + m - 1)
    };
    let this = y * 12 + m - 1;
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let old: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| month(n).is_some_and(|at| this - at >= KEEP_MONTHS))
        .collect();
    if old.is_empty() {
        return Ok(old);
    }
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(".lock"))?;
    held.lock()?;
    for n in &old {
        std::fs::remove_file(dir.join(n))?;
    }
    Ok(old)
}

/// Appends `entries` to `file` in `dir`, a line each — one write under the folder's lock (`.lock`), so taraes
/// side by side never cut into each other's lines. The folder made 0700, the file 0600; neither the file nor the
/// lock is opened through a link. On a worker.
pub fn append(dir: &Path, file: &str, entries: &[Value]) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    if entries.is_empty() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let open = |name: &str| {
        let mut o = std::fs::OpenOptions::new();
        o.create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
        (o, dir.join(name))
    };
    let (mut o, lock) = open(".lock");
    let held = o.write(true).truncate(false).open(lock)?;
    held.lock()?;
    let (mut o, path) = open(file);
    let mut f = o.append(true).open(path)?;
    let text: String = entries.iter().map(|e| format!("{e}\n")).collect();
    io::Write::write_all(&mut f, text.as_bytes())
}

/// The record's entries in `dir`, every month's, oldest first (tests; `cargo xtask cards` reads it on its own).
#[cfg(test)]
pub fn read_record(dir: &Path) -> Vec<Value> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|p| {
            std::fs::read_to_string(p).unwrap_or_default().lines().map(String::from).collect::<Vec<_>>()
        })
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// The record's writes on a worker not done yet — tarae's way out waits for them (`wait_written`).
pub(crate) static WRITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The most tarae's way out waits for the record's writes.
const QUIT_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits until the record's writes under way are done, at most `max` — false: some still weren't.
pub fn wait_written(max: std::time::Duration) -> bool {
    let until = Instant::now() + max;
    while WRITING.load(std::sync::atomic::Ordering::Acquire) > 0 {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    true
}

impl crate::editor::Editor {
    /// Keeps `entries` in the record (`$state/cards/`), on a worker with nothing to bring back — unless
    /// `llm.card-record` is off or there's no record (tests: none unless given).
    pub fn cards_record(&mut self, entries: Vec<Value>) {
        let Some(dir) = self.cards_dir.clone().filter(|_| self.config.llm.card_record && !entries.is_empty())
        else {
            return;
        };
        let file = month_file(SystemTime::now());
        WRITING.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.events.jobs().background(move || {
            if let Err(e) = append(&dir, &file, &entries) {
                crate::log::line("cards", "warn", &format!("{}: {e}", dir.join(&file).display()));
            }
            WRITING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        });
    }

    /// When tarae ends (`:q`, `:restart`, a signal): the record's writes under way finished first.
    pub fn cards_flush(&self) {
        if !wait_written(QUIT_WAIT) {
            crate::log::line(
                "cards",
                "warn",
                "the card record's last writes were still under way at the end",
            );
        }
    }
}

/// A command of Claude's that ran with no card (a check, an `a` before) and warns of something (`cardwarn.rs`):
/// a `warned` line — `<command> · warn=<ids>` — as its chat row says it (D2: a warning doesn't ask).
pub fn ran_warned(session: &crate::log::Session, cmd: &str, warned: &[crate::cardwarn::Warned]) {
    if warned.is_empty() {
        return;
    }
    let ids: Vec<&str> = warned.iter().map(|w| w.id.as_str()).collect();
    logged(session, "warned", &format!("{} · warn={}", cmd.trim().replace('\n', " "), ids.join(",")));
}

/// The `denied` line for a run the sandbox denied something: `<what ran> · N things, M times: <each, as
/// Claude reads it>` — None when it denied nothing (or couldn't be watched).
pub fn denied_line(what: &str, b: &crate::sandbox::Blocked) -> Option<String> {
    if b.total == 0 {
        return None;
    }
    let mut each: Vec<String> = b.seen.iter().map(crate::sandbox::Denial::reported).collect();
    if b.distinct > b.seen.len() {
        each.push(format!("… and {} more", b.distinct - b.seen.len()));
    }
    let what = what.trim().replace('\n', " ");
    Some(format!(
        "{what} · {} thing{}, {} time{}: {}",
        b.distinct,
        if b.distinct == 1 { "" } else { "s" },
        b.total,
        if b.total == 1 { "" } else { "s" },
        each.join("; ")
    ))
}

/// A run ended under the sandbox's watch: what it denied, to the log.
pub fn denied(session: &crate::log::Session, what: &str, w: &crate::sandbox::Watch) {
    if let Some(l) = denied_line(what, &w.blocked()) {
        logged(session, "denied", &l);
    }
}

fn logged(session: &crate::log::Session, level: &str, line: &str) {
    session.line("tarae", level, line);
    #[cfg(test)]
    LOGGED.with(|l| l.borrow_mut().push(format!("{level} {} · {line}", session.name().unwrap_or_default())));
}

#[cfg(test)]
thread_local! {
    /// (tests: the lines logged on this thread, each after its level and its session's name — `card s · …`,
    /// `denied s · …`)
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
    use crate::offer::What;
    use std::path::PathBuf;

    /// What the capability model asks about: past the sandbox, no sandbox, a read outside the places read
    /// by default, the screen, and what the sandbox blocked — never a command for what it is. Its gaps closed
    /// before P3 decides by it (cardauto piece 2): outside home is a default read, as runs read it (D3) — but
    /// others' homes and volumes; a partial sandbox (Linux Landlock ABI 2) is none; a warning that asks
    /// (publishing, reaching out) asks only where the run could reach out (D4).
    #[test]
    fn the_capability_model_asks_for_places_not_commands() {
        // (a session reading outside home, as tarae does)
        let on = crate::caps::Caps { outside: true, ..Default::default() };
        let decide = |r: &Request| decide(r, &on);
        let run = |past, sandboxed, reads: &[Class]| {
            decide(&Request::Run { past, sandboxed, reads: reads.to_vec(), outward: false })
        };
        assert_eq!(run(Past::No, true, &[]), Decision::None, "any command, inside");
        // (D3: outside home, read by default — a run's and Claude's Read alike; not others' homes, not volumes)
        assert_eq!(run(Past::No, true, &[Class::Temp, Class::Elsewhere]), Decision::None);
        assert_eq!(decide(&Request::Read(Class::Elsewhere)), Decision::None);
        for apart in [Class::OtherHome, Class::Volume] {
            assert_eq!(decide(&Request::Read(apart)), Decision::Grant(Verb::Read), "{apart:?}");
            assert_eq!(run(Past::No, true, &[Class::Elsewhere, apart]), Decision::Grant(Verb::Read));
        }
        assert!(
            crate::reads::by_default(Class::Elsewhere, &on) && !crate::reads::by_default(Class::Volume, &on)
        );
        // (where it doesn't — tests' sessions: asked)
        let off = crate::caps::Caps::default();
        assert_eq!(super::decide(&Request::Read(Class::Elsewhere), &off), Decision::Grant(Verb::Read));
        // (Linux ABI 2: "every run asks" — as no sandbox; the setting off likewise)
        use crate::sandbox::Availability;
        let abi2 =
            Availability::Partial("this kernel's Landlock (ABI 2) can't control truncating files".into());
        assert!(!sandboxed(true, &abi2) && !sandboxed(true, &Availability::Missing("x".into())));
        assert!(sandboxed(true, &Availability::Ready) && !sandboxed(false, &Availability::Ready));
        assert_eq!(run(Past::No, sandboxed(true, &abi2), &[]), Decision::NoSandbox);
        // (D4: a warning that asks — `git push`, `cargo publish` — asks where the run could reach out unasked:
        // the network open, or closed with a host let through (piece C); closed with none, the host's own card asks)
        let llm = |network| crate::llm::LlmConfig { sandbox_network: network, ..Default::default() };
        let warned =
            |reach| Request::Run { past: Past::No, sandboxed: true, reads: Vec::new(), outward: reach };
        assert_eq!(decide(&warned(reaches_out(&llm(true), false))), Decision::Outward);
        assert_eq!(decide(&warned(reaches_out(&llm(false), true))), Decision::Outward);
        assert_eq!(decide(&warned(reaches_out(&llm(false), false))), Decision::None);
        assert_eq!(Decision::Outward.word(), "outward");
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
            ..Default::default()
        };
        let caps = crate::caps::Caps::of_defaults(d.clone(), Some(home));
        let at = |p: &str| class(Some(Path::new(p)), &caps);
        assert_eq!(at("/Users/u/src/repo/target/x"), Class::OtherCheckout);
        assert_eq!(at("/Users/u/src/repository/x"), Class::Home, "by path parts");
        assert_eq!(at("/private/tmp/shot/a.png"), Class::Temp);
        assert_eq!(at("/Users/u/.cargo/registry/src/x/lib.rs"), Class::Toolchain);
        assert_eq!(at("/Users/u/.cargo/credentials.toml"), Class::Home);
        assert_eq!(at("/Users/u/.ssh/id_ed25519"), Class::Secret);
        assert_eq!(at("/opt/x"), Class::Elsewhere);
        assert_eq!(class(None, &caps), Class::Unknown);
        // (cardauto piece 2: a folder holding home is home's; others' homes and volumes apart — §8.6)
        assert_eq!((at("/"), at("/Users")), (Class::Home, Class::Home), "they hold home");
        assert_eq!(at("/Users/other/src/.env"), Class::OtherHome);
        assert_eq!(at("/Users/Shared/x"), Class::Elsewhere, "everyone's");
        assert_eq!(at("/Users/u-not/x"), Class::OtherHome, "by path parts");
        assert_eq!((at("/Volumes/Ext/proj"), at("/Volumes")), (Class::Volume, Class::Volume));
        assert_eq!(
            (at("/mnt/share"), at("/run/media/u/usb"), at("/run")),
            (Class::Volume, Class::Volume, Class::Volume)
        );
        assert_eq!(at("/run/user/501"), Class::Elsewhere);
        let root = crate::caps::Caps::of_defaults(d, Some(Path::new("/root")));
        let root_home = |p: &str| class(Some(Path::new(p)), &root);
        assert_eq!(
            (root_home("/home/u/x"), root_home("/home"), root_home("/")),
            (Class::OtherHome, Class::OtherHome, Class::Home)
        );
        assert_eq!(root_home("/etc/hosts"), Class::Elsewhere);
    }

    /// Review round 1 (the pieces' order): piece 2 opens outside home to Claude's Read before piece 3 narrows
    /// runs to the system's mounts — but not other drives nor other users' homes: those are classes of their
    /// own already, and ask, on either system's homes.
    #[test]
    fn other_drives_and_other_users_homes_still_ask_before_piece_3() {
        let d = crate::allow::Defaults::default();
        for home in ["/Users/u", "/home/u"] {
            let caps = crate::caps::Caps::of_defaults(d.clone(), Some(Path::new(home)));
            let at = |p: &str| class(Some(Path::new(p)), &caps);
            for (p, c) in [
                ("/Volumes/x", Class::Volume),
                ("/mnt/x", Class::Volume),
                ("/Users/other/x", Class::OtherHome),
                ("/home/other/x", Class::OtherHome),
            ] {
                assert_eq!(at(p), c, "{p} with home {home}");
                assert!(!crate::reads::by_default(at(p), &caps), "{p} asks");
            }
            assert!(crate::reads::by_default(at("/opt/x"), &caps), "the rest outside home doesn't");
        }
    }

    /// Review round 1: what runs never read (`sandbox::kept_places`) is never read unasked by Claude either,
    /// wherever the variables put it — a state home and a CARGO_HOME moved outside home sorted as `Elsewhere`, a
    /// default read since D3. A folder holding one asks too; the rest of a moved toolchain home doesn't.
    #[test]
    fn what_runs_never_read_is_never_a_default_outside_home_either() {
        let home = Path::new("/Users/u");
        let mut here = crate::sandbox::Here::default();
        for (k, v) in [("HOME", "/Users/u"), ("XDG_STATE_HOME", "/data/state"), ("CARGO_HOME", "/opt/cargo")]
        {
            here.env.insert(k.into(), v.into());
        }
        let d =
            crate::allow::Defaults { kept: crate::sandbox::kept_places(home, &here), ..Default::default() };
        let caps = crate::caps::Caps::of_defaults(d, Some(home));
        let at = |p: &str| class(Some(Path::new(p)), &caps);
        for p in ["/data/state/tarae/sandbox.json", "/data/state/tarae", "/opt/cargo/credentials.toml"] {
            assert_eq!(at(p), Class::Secret, "{p}");
            assert!(!crate::reads::by_default(at(p), &caps), "{p}");
        }
        assert_eq!((at("/opt"), at("/data")), (Class::Secret, Class::Secret), "a folder holding one");
        assert_eq!(at("/opt/cargo/registry/src/x/lib.rs"), Class::Elsewhere, "the rest of it");
        assert_eq!(at("/data/other/x"), Class::Elsewhere);
        // (at home, as before: kept and secret alike)
        assert_eq!(at("/Users/u/.cargo/credentials.toml"), Class::Secret);
    }

    /// The line: the card's id, the kind from the card, why, the classes of what's read, the model's decision,
    /// the card it's the same as (cardburden 3), the answer, and the time from drawn — or that it never was.
    #[test]
    fn a_cards_line_says_what_it_asked_and_how_it_went() {
        let call = crate::tools::Call::test(1);
        let mut o = Offer::new("t".into(), "q", "", String::new(), What::Screen { call });
        o.id = 12;
        let asked = Asked::new(
            Why::ReadsOutside(vec![Class::Temp, Class::Home]),
            Request::Run {
                past: Past::No,
                sandboxed: true,
                reads: vec![Class::Temp, Class::Home],
                outward: false,
            },
        );
        let t0 = Instant::now();
        o.since = t0;
        let at = |s: u64| t0 + std::time::Duration::from_millis(s);
        let line =
            |o: &Offer, a: &Asked, answer: &str, now: Instant| line(o, a, (answer, Some("you")), now, None);
        assert_eq!(
            line(&o, &asked, "n", at(4200)),
            "card#12 · screen · why=reads-outside · class=temp,home · shadow=grant:read · n · not drawn · \
             up 4.2s"
        );
        o.drawn.set(Some(at(3300)));
        let a = Asked::new(
            Why::NotACheck("awk".into()),
            Request::Run { past: Past::No, sandboxed: true, reads: vec![], outward: false },
        );
        assert_eq!(
            line(&o, &a, "a", at(4200)),
            "card#12 · screen · why=not-a-check:awk · shadow=none · a · 900ms · up 4.2s"
        );
        let same = |unchanged| line(&o, &a.clone().same_as(9, unchanged), "y", at(4200));
        assert!(same(true).contains(" · shadow=none · same=#9 · y · "), "{}", same(true));
        assert!(same(false).contains(" · shadow=none · same=#9:changed · y · "), "{}", same(false));
        // (cardburden 4: the warnings it said, by id)
        let warned = a.clone().warned(vec!["docker-own-config".into(), "outward".into()]);
        let l = line(&o, &warned, "y", at(4200));
        assert!(l.contains(" · shadow=none · warn=docker-own-config,outward · y · "), "{l}");
        // (cardauto 1: a run's card names its run, as the run's `timing` line does — the command found by id)
        let root = Path::new("/p");
        let ran = a.clone().ran(Some(7), "awk 1 x", root, root);
        let l = line(&o, &ran, "y", at(4200));
        assert!(l.starts_with("card#12 · run#7 · screen · why=not-a-check:awk · "), "{l}");
        // (shuttle keys: a program's key that answered it says whose — a person's says nothing)
        let l = super::line(&o, &a, ("n", Some("shuttle:claude-code")), at(4200), None);
        assert!(l.contains(" · shadow=none · n · by=shuttle:claude-code · 900ms · "), "{l}");
    }

    /// cardauto piece 1 (D2): every card goes in the record with what it was about — the command, where it
    /// ran from, its run — and so does a run refused with no card, as `refused`. Each start adds to the
    /// month's file, never over it: a tarae started again (`:restart`) finds what the last one kept, and its
    /// way out waits for the writes under way. 0700 the folder, 0600 the file; `llm.card-record` off, nothing.
    /// Review of piece 3: the shadow is the session's `Caps` as made on a worker. With none made yet (a session's
    /// first moments) it says `unbuilt` where a made one could decide otherwise, never a guess — the rest as
    /// ever. Made as the session is (`session_kept_back`: at start, and after `:restart` before its chat is
    /// restored), a read outside home where tarae reads it is `none` — on a card (`offer_said`) and on a run
    /// refused with no card (`refused` with the session's `caps()`, as `tools::card` gives it).
    #[test]
    fn the_shadow_is_the_sessions_caps_or_says_unbuilt() {
        let tmp = std::env::temp_dir().join(format!("tarae-card-shadow-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let dir = tmp.join(DIR);
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        (ed.cards_dir, ed.read_outside) = (Some(dir.clone()), true);
        // (each named: the record's batches land in any order)
        let card = |ed: &mut crate::editor::Editor, request: Request, what: &str| {
            let o = Offer::new("t".into(), "q", "", String::new(), What::NotesStale { ids: Vec::new() });
            ed.push_offer(o.asked(Asked::new(Why::Read, request).about(what)));
            ed.offer_dismiss();
        };
        let run = |reads: Vec<Class>| Request::Run { past: Past::No, sandboxed: true, reads, outward: false };
        assert!(ed.session.caps.is_none(), "none made yet");
        card(&mut ed, Request::Read(Class::Elsewhere), "read, none made");
        card(&mut ed, run(Vec::new()), "run, none made");
        ed.session_kept_back(ed.session.id);
        let end = Instant::now() + std::time::Duration::from_secs(10);
        while ed.session.caps.is_none() {
            let ev =
                ed.events.recv_timeout(end.saturating_duration_since(Instant::now())).expect("caps made");
            ed.handle_event(ev);
        }
        card(&mut ed, Request::Read(Class::Elsewhere), "read, made");
        let asked = Asked::new(Why::ReadsOutside(vec![Class::Elsewhere]), run(vec![Class::Elsewhere]))
            .about("refused, made");
        let e = refused(&asked, "not run", (None, SystemTime::now()), ed.session.caps());
        ed.cards_record(vec![e]);
        ed.cards_flush();
        let mut shadows: Vec<(String, String)> = read_record(&dir)
            .iter()
            .map(|e| {
                (
                    e["what"].as_str().unwrap_or_default().into(),
                    e["shadow"].as_str().unwrap_or_default().into(),
                )
            })
            .collect();
        shadows.sort();
        let want = [
            ("read, made", "none"),
            ("read, none made", "unbuilt"),
            ("refused, made", "none"),
            ("run, none made", "none"),
        ];
        assert_eq!(shadows, want.map(|(w, s)| (w.to_string(), s.to_string())));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn the_record_keeps_every_card_with_its_command_across_starts() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = std::env::temp_dir().join(format!("tarae-card-record-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let dir = tmp.join("state").join(DIR);
        let started = || {
            let mut ed = crate::editor::Editor::new(crate::config::Config::default());
            ed.cards_dir = Some(dir.clone());
            ed
        };
        let mut ed = started();
        let root = ed.session.root.clone();
        let run = Request::Run { past: Past::No, sandboxed: true, reads: Vec::new(), outward: false };
        let asked = Asked::new(Why::NotACheck("awk".into()), run.clone()).ran(
            Some(7),
            "awk 'NR<3' x",
            &root.join("src"),
            &root,
        );
        ed.push_offer(
            Offer::new("t".into(), "q", "", String::new(), What::NotesStale { ids: Vec::new() }).asked(asked),
        );
        let id = ed.offers[0].id;
        ed.offer_dismiss();
        ed.cards_flush();
        // Started again: a run refused with no card goes in after it
        let mut ed = started();
        let asked = Asked::new(Why::Script, run).ran(Some(8), "sh -n x.sh", &root, &root);
        let said = "x.sh was written this turn without the user seeing it, and it has 200 lines";
        ed.cards_record(vec![refused(&asked, said, (Some("s"), SystemTime::now()), None)]);
        ed.cards_flush();
        let all = read_record(&dir);
        assert_eq!(all.len(), 2, "{all:?}");
        let (card, no_card) = (&all[0], &all[1]);
        assert_eq!(
            (&card["card"], &card["run"], &card["kind"], &card["why"], &card["shadow"], &card["answer"]),
            (
                &json!(id),
                &json!(7),
                &json!("notes-stale"),
                &json!("not-a-check:awk"),
                &json!("none"),
                &json!("n")
            )
        );
        assert_eq!(
            (&card["what"], &card["cwd"], &card["v"]),
            (&json!("awk 'NR<3' x"), &json!("src"), &json!(1))
        );
        assert!(card.get("drawn_ms").is_none() && card["up_ms"].is_u64(), "never drawn: no time from drawn");
        assert!(card.get("class").is_none() && card.get("same").is_none(), "nothing empty kept: {card}");
        let at = card["at"].as_str().unwrap();
        assert!(at.len() == 19 && at.as_bytes()[10] == b' ', "{at}");
        assert_eq!(
            (&no_card["kind"], &no_card["answer"], &no_card["run"], &no_card["session"], &no_card["said"]),
            (&json!("run"), &json!("refused"), &json!(8), &json!("s"), &json!(said))
        );
        assert!(no_card.get("card").is_none() && no_card.get("cwd").is_none(), "{no_card}");
        let file = dir.join(month_file(SystemTime::now()));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&dir), mode(&file)), (0o700, 0o600));
        // Off: nothing more
        ed.config.llm.card_record = false;
        ed.cards_record(vec![json!({ "x": 1 })]);
        ed.cards_flush();
        assert_eq!(read_record(&dir).len(), 2);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Review round 1: the record keeps commands for a year, so a token written into one isn't kept
    /// (`tokens::redacted`, `token_like`'s rules a word at a time; a line it can't take apart goes whole) — and
    /// the months past `KEEP_MONTHS` go when tarae starts, only files named as the record names them.
    #[test]
    fn the_record_keeps_no_token_and_no_month_past_a_year() {
        use crate::tokens::redacted;
        for (cmd, kept) in [
            ("FOO_TOKEN=s3cr3tvalue cargo publish", "FOO_TOKEN=… cargo publish"),
            (
                "curl -H 'Authorization: Bearer abcdefgh12345' https://x.test",
                "curl -H 'Authorization: Bearer …' https://x.test",
            ),
            ("curl -H \"x-api-key:abcdefgh12345\" x.test", "curl -H \"x-api-key:… x.test"),
            ("git clone https://u:hunter22@git.test/r.git", "git clone https://…@git.test/r.git"),
            ("gh auth login --with-token ghp_0123456789abcdefABCD", "gh auth login --with-token …"),
            ("./gradlew publish -Dsigning.password=hunter22", "./gradlew publish -Dsigning.password=…"),
            ("npm publish --api-key='hunter22'", "npm publish --api-key=…'"),
            ("cargo test -q -- token_like", "cargo test -q -- token_like"),
        ] {
            assert_eq!(redacted(cmd), kept, "{cmd}");
        }
        assert_eq!(redacted("echo ok\npassword: hunter22"), "echo ok\n… (a line holding a token, left out)");
        let root = Path::new("/p");
        let run = Request::Run { past: Past::No, sandboxed: true, reads: Vec::new(), outward: false };
        let asked = Asked::new(Why::Script, run).ran(Some(1), "FOO_TOKEN=s3cr3tvalue sh x.sh", root, root);
        let e = refused(&asked, "not run", (None, SystemTime::now()), None);
        assert_eq!(e["what"], json!("FOO_TOKEN=… sh x.sh"));
        assert!(!e.to_string().contains("s3cr3t"), "{e}");
        // A year kept, this month included; the rest gone — only the record's own names
        let dir = std::env::temp_dir().join(format!("tarae-card-prune-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let (y, m, _) = crate::log::day(now);
        let ago = |n: i32| {
            let i = y * 12 + m - 1 - n;
            format!("{:04}-{:02}.jsonl", i / 12, i % 12 + 1)
        };
        let names = [ago(0), ago(KEEP_MONTHS - 1), ago(KEEP_MONTHS), ago(KEEP_MONTHS + 30)];
        let others = ["notes.txt", "2001-01.jsonl.bak", "x2001-01.jsonl", ".lock"];
        for n in names.iter().map(String::as_str).chain(others) {
            std::fs::write(dir.join(n), "{}\n").unwrap();
        }
        let mut gone = prune(&dir, now).unwrap();
        gone.sort();
        assert_eq!(gone, [ago(KEEP_MONTHS + 30), ago(KEEP_MONTHS)]);
        for n in [&ago(0), &ago(KEEP_MONTHS - 1)].into_iter().map(String::as_str).chain(others) {
            assert!(dir.join(n).exists(), "{n} kept");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The record lives in tarae's state, which no run reads: a new file there needs no rule of its own — the
    /// runs' `kept_reads` keep the whole state folder.
    #[test]
    fn the_record_is_kept_from_runs() {
        let (Some(state), Some(home)) = (crate::config::state_dir(), crate::allow::home()) else { return };
        let here = crate::sandbox::Here::now();
        let record = crate::sandbox::profile_path(&state.join(DIR).join("2026-10.jsonl"));
        let runs = crate::sandbox::kept_reads(&home, &crate::sandbox::vars_in(&home, &here));
        assert!(runs.iter().any(|k| record.starts_with(k)), "{record:?} in none of {runs:?}");
    }

    /// cardburden 5: tarae's own cards were never in the log — a preflight's, a stuck run's, notes gone stale,
    /// a rebase's — so how long they held anyone wasn't known. Every card's answer is a `card` line now, a
    /// letter of its own too (`X`): `why=own · shadow=own`, the kind as `tarae state` names it. (The rebase
    /// card's root is no session's: its `X` does nothing.)
    #[test]
    fn every_card_answered_has_its_line_tarae_s_own_too() {
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let up = |ed: &mut crate::editor::Editor, what| {
            ed.push_offer(Offer::new("t".into(), "q", "", String::new(), what));
            ed.offers[0].id
        };
        let id = up(&mut ed, What::NotesStale { ids: Vec::new() });
        ed.offer_dismiss();
        let at = |id: u64| logged_lines("card ").into_iter().find(|l| l.contains(&format!("card#{id} ")));
        let l = at(id).expect("its line");
        assert!(
            l.contains(&format!(" · card#{id} · notes-stale · why=own · shadow=own · n · not drawn · up ")),
            "{l}"
        );
        let root = std::env::temp_dir().join(format!("tarae-cards-no-session-{}", std::process::id()));
        let id = up(&mut ed, What::RebaseResolved { root, message: false });
        ed.offer_abort();
        assert!(ed.offers.is_empty());
        let l = at(id).expect("its line");
        assert!(l.contains(&format!(" · card#{id} · rebase-resolved · why=own · shadow=own · X · ")), "{l}");
    }

    /// A run's denials as one line — the distinct ones as Claude reads them, past what's kept counted.
    #[test]
    fn a_runs_denials_are_one_line() {
        let mut b = crate::sandbox::Blocked::default();
        assert_eq!(denied_line("x", &b), None, "nothing denied, no line");
        let d = |op: &str, t: &str| crate::sandbox::Denial {
            process: "touch".into(),
            op: op.into(),
            target: t.into(),
        };
        b.add(d("file-write-create", "/opt/a"));
        b.add(d("file-write-create", "/opt/a"));
        assert_eq!(
            denied_line("touch /opt/a\n", &b).unwrap(),
            "touch /opt/a · 1 thing, 2 times: writing /opt/a (file-write-create, touch)"
        );
        for i in 0..crate::sandbox::DENIALS_KEPT + 1 {
            b.add(d("file-write-create", &format!("/opt/{i}")));
        }
        let l = denied_line("x", &b).unwrap();
        assert!(l.starts_with("x · 12 things, 13 times: ") && l.ends_with("; … and 2 more"), "{l}");
    }
}
