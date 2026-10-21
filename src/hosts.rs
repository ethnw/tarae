//! The egress proxy's questions, on the main loop (`target/sb/step5e-plan.md`). A host a run's rules don't name
//! is held by tarae's proxy, which asks here (`proxy::FromProxy::Ask`): one card per host in the run's own
//! session (`What::Host`) — `y` this run · `s` this session · `A` always here · `n` no, for the session. The
//! answer goes back as `ToProxy::Answer` for the card's own session and run, never the session in front's: a card
//! parks with its session, so only that session's keys reach it. How a connection came to the proxy (a proxy
//! variable, a supervisor's swap, a redirect in the session's network) isn't this module's: it sees hosts, runs
//! and sessions.
//!
//! What lasts: `y` lives in the proxy for that run; `s` and `n` in the proxy for the session, `s` listed in
//! `Session.hosts` (`:granted` takes it back); `A` in `$state/sandbox.json` (`allow::Allowed.hosts`),
//! which the next runs' rules carry (`tools::with_allowed`).
//!
//! Piece C (`target/sb/sandbox-ux-pieceC-plan.md`): with `llm.sandbox-network` off every host is asked — `y` this
//! run · `a` this chat (`Chat.allowed`, the proxy's for the session until the conversation ends: `chat_ended`) ·
//! `s` this session · `n`; no `A` (cards.md decision 4: always is the setting). A host that's a pack's registry
//! is answered with the pack's other registries, listed on the card (Q6: the suggestion is words, the yes grants).
//! A grant naming a registry of a pack with caches opens them to the runs that start under it
//! (`Policy::open_granted_caches`).
//!
//! 6e+ (`target/sb/step6eplus-design.md`): raw TCP to a host:port (`What::HostRaw`) and a name's SRV/TXT records
//! (`What::Lookup`) are asked the same way, each kept apart from the others (F4) — `y` · `s` · `n`, no `A`: a
//! lasting allow is the user's own line in `llm.sandbox-hosts`.

use crate::editor::Editor;
use crate::event::SessionId;
use crate::offer::{Offer, What};
use crate::sandbox::proxy::{FromProxy, Kind, Scope, ToProxy};

/// A host a run asked to reach, as its card holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    /// The proxy's session (`proxy::session_of` its project) and run — what the answer names.
    pub session: u64,
    pub run: u64,
    /// What's asked for: the host over TLS or HTTP, raw TCP, or its records.
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    /// A lookup's questions so far (`SRV _mongodb._tcp.db.example`).
    pub asked: Vec<String>,
    /// What asked (its process) and in which run (its panel's words).
    pub process: String,
    pub label: String,
    /// The user's own run (Claude's command run again — `blocked.rs`), not Claude's.
    pub users: bool,
    /// What asked was left running by a run that's over (Linux: a daemon keeps its run's way to the proxy) —
    /// no run's words to name (`label` empty).
    pub left: bool,
    /// `llm.sandbox-network` on when it asked (a host list's unknown host): `A` is offered — off, every host is
    /// asked and nothing lasts past the session (piece C).
    pub open: bool,
    /// `llm.sandbox-hosts` when it asked — what the open network's card says is reached without asking (C1b).
    pub hosts: crate::llm::Hosts,
    /// A host's: the pack's other registries when it's one (as written, `*.crates.io`) — a yes grants them too.
    pub also: Vec<String>,
}

impl Ask {
    /// `host:port` — a lookup's name alone.
    pub fn at(&self) -> String {
        at(self.kind, &self.host, self.port)
    }

    /// Whether `other` asks for the same in the same session (one card for both).
    fn same(&self, other: &Ask) -> bool {
        (self.session, self.kind, &self.host, self.port)
            == (other.session, other.kind, &other.host, other.port)
    }
}

/// `host:port` — a lookup's name alone.
fn at(kind: Kind, host: &str, port: u16) -> String {
    match kind {
        Kind::Lookup => host.to_string(),
        _ => format!("{host}:{port}"),
    }
}

/// What an answer lets commands do, in words (`reach github.com:443`).
fn doing(kind: Kind, host: &str, port: u16) -> String {
    match kind {
        Kind::Host => format!("reach {host}:{port}"),
        Kind::Raw => format!("open raw connections to {host}:{port}"),
        Kind::Lookup => format!("look up {host}'s service records"),
    }
}

/// How `:granted` lists it and `:sandbox-disallow` takes it (`host a:443`, `raw a:22`, `lookup a`).
pub fn listed(kind: Kind, host: &str, port: u16) -> String {
    match kind {
        Kind::Host => format!("host {host}:{port}"),
        Kind::Raw => format!("raw {host}:{port}"),
        Kind::Lookup => format!("lookup {host}"),
    }
}

/// What `listed` says, read back.
pub fn unlisted(s: &str) -> Option<(Kind, String, u16)> {
    let (word, rest) = s.split_once(' ')?;
    match word {
        "host" => crate::allow::host_port(rest).map(|(h, p)| (Kind::Host, h, p)),
        "raw" => crate::allow::host_port(rest).map(|(h, p)| (Kind::Raw, h, p)),
        "lookup" => crate::sandbox::proxy::parse::name(rest).ok().map(|n| (Kind::Lookup, n, 0)),
        _ => None,
    }
}

/// The proxy's questions come to the main loop from now on (`main`, before any run): a thread of their own
/// hands each over as a job.
pub fn listen(tx: crate::event::Tx) {
    let (asks, rx) = std::sync::mpsc::channel();
    crate::sandbox::proxy::listen(asks);
    crate::event::thread("tarae-asks", move || {
        for ask in rx {
            let apply = move |ed: &mut Editor| asked(ed, ask);
            if tx.send(crate::event::Event::Job(Box::new(apply))).is_err() {
                break;
            }
        }
    });
}

/// The session a run going on (by its policy's id, `Policy.run_id`) is in, what its panel calls it, and whether
/// it's the user's own. A run over — its record kept after its end — isn't one.
fn find_run(ed: &Editor, run: u64) -> Option<(SessionId, String, bool)> {
    let ours = |g: Option<&crate::sandbox::Guard>| matches!(g, Some(crate::sandbox::Guard::In(p)) if p.run_id == run);
    for s in std::iter::once(&ed.session).chain(&ed.sessions) {
        let task = s.task_run.as_ref().filter(|r| r.state == crate::task::State::Running);
        if let Some(r) = task.filter(|r| ours(r.guard.as_ref())) {
            return Some((s.id, r.task.cmd.clone(), r.tell.is_some()));
        }
        let test = s.test_run.as_ref().filter(|r| r.state == crate::testing::RunState::Running);
        if let Some(r) = test.filter(|r| ours(r.guard.as_ref())) {
            return Some((s.id, format!("tests: {}", r.target.label), r.tell.is_some()));
        }
    }
    None
}

/// The proxy asks about a host: a card in the run's session — or the card already up (or parked) for that host
/// takes it, naming the run that asks now (`y` lets that one through). A run that's over asks through what it
/// left running (Linux: a daemon keeps its run's way to the proxy): the card is in the session the question
/// names, saying so — never a wait with no card. A session that's gone raises nothing (the proxy dropped its
/// questions with it).
pub fn asked(ed: &mut Editor, ask: FromProxy) {
    let FromProxy::Ask { session, run, kind, host, port, process, asked } = ask;
    let found = find_run(ed, run).map(|(id, label, users)| (id, label, users, false)).or_else(|| {
        let s = std::iter::once(&ed.session).chain(&ed.sessions);
        let id = s.into_iter().find(|s| s.proxy_id() == session)?.id;
        Some((id, String::new(), false, true))
    });
    let Some((id, label, users, left)) = found else {
        let msg = format!("the proxy asked about {} for a session that's gone", at(kind, &host, port));
        return crate::log::line("sandbox", "info", &msg);
    };
    let also = match kind {
        Kind::Host => suggested(&crate::sandboxpack::get().registries, &host, port),
        Kind::Raw | Kind::Lookup => Vec::new(),
    };
    let (open, hosts) = (ed.config.llm.sandbox_network, ed.config.llm.sandbox_hosts.clone());
    let a = Ask { session, run, kind, host, port, asked, process, label, users, left, open, hosts, also };
    ed.with_session(id, move |ed| {
        let front = !ed.background();
        let cards = ed.session.parked.iter_mut().chain(ed.offers.iter_mut().filter(|_| front));
        for o in cards {
            if let Some(h) = o.host()
                && h.same(&a)
            {
                // (another run's, or a lookup's next question)
                if h.run != a.run || h.asked != a.asked {
                    let fresh = card(a);
                    (o.question, o.what, o.asked) = (fresh.question, fresh.what, fresh.asked);
                    o.rev += 1;
                }
                return;
            }
        }
        ed.push_offer(card(a));
    });
}

/// Its title.
pub const TITLE: &str = "Let a command reach";
/// A raw connection's card's title (6e+).
pub const RAW_TITLE: &str = "Let a command open a raw connection to";
/// A lookup's card's title (6e+).
pub const LOOKUP_TITLE: &str = "Let a command look up the service records of";

/// The open network's line on a host card: what's reached without asking, as `llm.sandbox-hosts` holds it (C1b).
fn open_lasts(hosts: &crate::llm::Hosts) -> &'static str {
    match hosts {
        crate::llm::Hosts::Registries => {
            "Without asking, commands reach the language packs' registries — llm.sandbox-hosts' default; a list \
             there is reached instead, \"*\" any host."
        }
        crate::llm::Hosts::List(_) => {
            "Without asking, commands reach llm.sandbox-hosts' list alone — the packs' registries aren't added to it."
        }
        crate::llm::Hosts::Any => "llm.sandbox-hosts lists the hosts reached without asking.",
    }
}

/// The card for `a` — of its kind.
pub fn card(a: Ask) -> Offer {
    use crate::cards::{Asked, Request, Why};
    let by = match (a.left, a.users) {
        (true, _) => format!("asked by {}, left running by a run that's over", a.process),
        (false, true) => format!("asked by {} in `{}`, your command", a.process, a.label),
        (false, false) => format!("asked by {} in `{}`, Claude's command", a.process, a.label),
    };
    let waits = waits();
    // (piece C: with the network closed nothing lasts past the session — always is the setting)
    let lasting = match a.open {
        true => open_lasts(&a.hosts),
        false => CLOSED_LASTS,
    };
    match a.kind {
        Kind::Host if !a.open => closed_card(a),
        Kind::Host => {
            let packs = crate::sandboxpack::get();
            let pack = registry_of(&packs.registries, &a.host, a.port);
            let named = match (&pack, a.also.is_empty()) {
                (None, _) => String::new(),
                (Some(pack), true) => format!("\n{} is the {pack} pack's registry", a.host),
                (Some(pack), false) => format!(
                    "\n{} is the {pack} pack's registry — a yes lets them reach its others too: {}",
                    a.host,
                    a.also.join(", ")
                ),
            };
            let question =
                format!("on port {}, through tarae's proxy\n{by}{named}\n\n{waits} {lasting}", a.port);
            let decided = 2 + usize::from(pack.is_some());
            // (what a yes gives besides the host: what's read can go there — the network open, the caches are)
            let warn = format!("What your commands read can be sent to {} too.", a.host);
            let at = question.split('\n').count() + 1;
            let question = format!("{question}\n\n{warn}");
            let asked = Asked::new(Why::Host, Request::Host(a.at()));
            let title = format!("{TITLE} {}?", a.host);
            Offer::new(title, &question, "", String::new(), What::Host(Box::new(a)))
                .deciding(0..decided)
                .warning(at..at + 1)
                .asked(asked)
        }
        // (Q2: what passes can't be checked — said in the warning colour)
        Kind::Raw => {
            let question = format!(
                "on port {}, through tarae's proxy\n{by}\nNot TLS or HTTP: tarae can't see what passes, only that it \
                 goes to the name the command looked up.\n\n{waits} {}",
                a.port,
                match a.open {
                    true => format!("A line {} in llm.sandbox-hosts lets it through without asking.", a.at()),
                    false => lasting.to_string(),
                }
            );
            let asked = Asked::new(Why::HostRaw, Request::HostRaw(a.at()));
            let title = format!("{RAW_TITLE} {}?", a.host);
            Offer::new(title, &question, "", String::new(), What::HostRaw(Box::new(a)))
                .deciding(0..2)
                .warning(2..3)
                .asked(asked)
        }
        // (M13: a held lookup is asked again at 5 s by glibc and Go — an answer before then reaches it)
        Kind::Lookup => {
            let question = format!(
                "{}\n{by}\ntarae asks the network's DNS for them: the name is seen by that DNS's operators; the \
                 records may name other hosts, each asked about when connected to.\n\nIts lookup gets the records if \
                 you answer within about 5 s; after that, from its next try. A name tarae already looks up, by the \
                 rules (llm.sandbox-hosts) or your yes on its host or raw card, gets its service records without this \
                 card.",
                a.asked.join(", ")
            );
            let types = a.asked.iter().filter_map(|q| q.split_once(' ')).map(|(t, _)| t.to_string());
            let mut types: Vec<String> = types.collect();
            crate::sorting::sort(&mut types);
            types.dedup();
            let asked = Asked::new(Why::Lookup, Request::Lookup { name: a.host.clone(), types });
            let title = format!("{LOOKUP_TITLE} {}?", a.host);
            Offer::new(title, &question, "", String::new(), What::Lookup(Box::new(a)))
                .deciding(0..2)
                .warning(2..3)
                .asked(asked)
        }
    }
}

/// The pack whose `registries` name `host:port` (Q6: words on the card, never a grant) — at the port a registry
/// lists, else 443 (`REGISTRY_PORT`): the same name on another port is no registry — None: none does.
fn registry_of(
    registries: &[(String, crate::sandbox::proxy::egress::Pattern)],
    host: &str,
    port: u16,
) -> Option<String> {
    let named = |p: &crate::sandbox::proxy::egress::Pattern| p.matches(host, port, REGISTRY_PORT);
    registries.iter().find(|(_, p)| named(p)).map(|(pack, _)| pack.clone())
}

/// A registry's port where its pack lists none: HTTPS's.
const REGISTRY_PORT: u16 = 443;

/// The other registries of the pack whose `registries` name `host:port`, as written (`*.crates.io`) — those at
/// that port: what a yes grants with it (Q6). Empty: none, or it's no registry.
fn suggested(
    registries: &[(String, crate::sandbox::proxy::egress::Pattern)],
    host: &str,
    port: u16,
) -> Vec<String> {
    use crate::sandbox::proxy::egress::Pattern;
    let Some(pack) = registry_of(registries, host, port) else { return Vec::new() };
    let at = |p: &Option<u16>| p.unwrap_or(REGISTRY_PORT) == port;
    let mut out: Vec<String> = Vec::new();
    for (_, p) in registries.iter().filter(|(of, _)| *of == pack) {
        let written = match p {
            Pattern::Name(n, at_port) if at(at_port) && n != host => n.clone(),
            Pattern::Under(d, at_port) if at(at_port) => format!("*.{d}"),
            _ => continue,
        };
        if !out.contains(&written) {
            out.push(written);
        }
    }
    out
}

/// The host card with the network closed (piece C) — short, as it holds a connection: whose command asks, by what
/// and where; what a yes lets in besides (a pack's other registries, decided); what it risks (one warning: what's
/// read can go there, a pack's caches open from the next run); the keys. The wait, how long a yes lasts and the
/// caches' long form are behind `v` (`details`).
fn closed_card(a: Ask) -> Offer {
    use crate::cards::{Asked, Request, Why};
    let packs = crate::sandboxpack::get();
    let pack = registry_of(&packs.registries, &a.host, a.port);
    let (whose, by) = match (a.left, a.users) {
        (true, _) => ("a", format!("{}, left running by a run that's over", a.process)),
        (false, true) => ("your", format!("{}, in `{}`", a.process, a.label)),
        (false, false) => ("Claude's", format!("{}, in `{}`", a.process, a.label)),
    };
    let mut lines = vec![format!("{by} · port {} · through tarae's proxy", a.port)];
    lines.extend(pack.as_deref().map(|p| registry_line(p, &a.also)));
    let decided = lines.len();
    let gists = pack.as_deref().map(|p| cache_gists(&packs.caches, p)).unwrap_or_default();
    lines.push(match gists.is_empty() {
        true => "What it read can be sent there.".to_string(),
        false => format!(
            "What it read can be sent there. From the next run, runs can also write {}, which your own builds \
             run.",
            gists.join(" and ")
        ),
    });
    let question = lines.join("\n");
    let asked = Asked::new(Why::Host, Request::Host(a.at()));
    let title = format!("Let {whose} command reach {}?", a.host);
    Offer::new(title, &question, "", String::new(), What::Host(Box::new(a)))
        .deciding(0..decided)
        .warning(decided..decided + 1)
        .asked(asked)
}

/// The line naming pack `pack`'s registry and `also`, what a yes lets in with it — the pack's name starts it:
/// `Rust's registry` (58 cells with its others, inside the 60 a host card's answers make it).
fn registry_line(pack: &str, also: &[String]) -> String {
    let mut c = pack.chars();
    let p: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
    match also.is_empty() {
        true => format!("{p}'s registry"),
        false => format!("{p}'s registry: a yes also lets in {}", also.join(", ")),
    }
}

/// Pack `pack`'s caches, each by its gist — `cargo's caches`, the part before ` — ` (whole behind `v`).
fn cache_gists(caches: &[crate::sandboxpack::Caches], pack: &str) -> Vec<String> {
    let whole = caches_of(caches, pack);
    whole.iter().map(|c| c.split(" — ").next().unwrap_or(c).to_string()).collect()
}

/// `host:port` as asked and what a yes lets in with it (Q6): the pack's other registries when it's one — what
/// every yes on it grants, a card's, `:sandbox-blocked`'s or Claude's `also` (C2).
pub fn with_suggested(host: &str, port: u16) -> Vec<String> {
    let packs = crate::sandboxpack::get();
    std::iter::once(host.to_string()).chain(suggested(&packs.registries, host, port)).collect()
}

/// What a grant card for `host:port` says of it besides the host (C2: Claude's `also`, `:sandbox-blocked`'s
/// choice): the pack's registry and what a yes lets in with it (decided), and what it risks — what's read can go
/// there, and, the network closed (`closed`), the pack's caches its runs write from then on (the run it opens
/// for, too: its rules are made with the grant).
pub fn grant_said(host: &str, port: u16, closed: bool) -> (Option<String>, String) {
    let packs = crate::sandboxpack::get();
    let pack = registry_of(&packs.registries, host, port);
    let also = suggested(&packs.registries, host, port);
    let line = pack.as_deref().map(|p| registry_line(p, &also));
    let gists = match (&pack, closed) {
        (Some(p), true) => cache_gists(&packs.caches, p),
        _ => Vec::new(),
    };
    let warn = match gists.is_empty() {
        true => "What it read can be sent there.".to_string(),
        false => format!(
            "What it read can be sent there. Runs under this yes can also write {}, which your own builds run.",
            gists.join(" and ")
        ),
    };
    (line, warn)
}

/// What the closed host card leaves to `v` — after its text in the card's buffer: the wait and when an answer
/// counts, how long a yes lasts, a pack's caches in full.
pub fn details(a: &Ask) -> String {
    let mut s = format!("{}\n\n{CLOSED_LASTS}", waits());
    let packs = crate::sandboxpack::get();
    if let Some(pack) = registry_of(&packs.registries, &a.host, a.port) {
        let caches = caches_of(&packs.caches, &pack);
        if !caches.is_empty() {
            s.push_str(&format!(
                "\n\nUnder a yes, runs from the next one on can also write {} — which your own builds use: what \
                 a run puts there runs in them.",
                caches.join(" and ")
            ));
        }
    }
    s
}

/// How long a held connection waits, and when an answer counts.
fn waits() -> String {
    let hold = crate::sandbox::proxy::Limits::default().hold.as_secs();
    format!(
        "Its connection waits up to {hold} s for your answer; after that it's refused, and your answer counts from \
         its next try."
    )
}

/// How long a yes lasts with the network closed (cards.md decision 4: always is the setting).
const CLOSED_LASTS: &str = "The network stays closed but for what you let through, for this session at most; \
                            llm.sandbox-network opens it for good.";

/// How pack `pack`'s caches are said (`rule`) — none for a pack with no caches.
fn caches_of(caches: &[crate::sandboxpack::Caches], pack: &str) -> Vec<String> {
    caches.iter().filter(|c| c.pack == pack).map(|c| c.rule.clone()).collect()
}

/// The user's answer on `a`'s card (`id`): to the proxy for the card's session and run, kept as far as it
/// reaches, said — and Claude hears it with the next message (but a run's own `y`).
pub fn answer(ed: &mut Editor, id: u64, a: Ask, scope: Scope) {
    ed.offer_finished(id, Ok(()));
    // (Q6: a yes grants the registries the card listed with it; a no is for the host asked alone)
    let also = if scope == Scope::Refused { &[][..] } else { &a.also[..] };
    let scope = if scope == Scope::Project && !a.open { Scope::Session } else { scope };
    // (C2: a no on Claude's command is its `also`'s too — that conversation asks for the host on no card again)
    let ours = ed.session.proxy_id() == a.session;
    if let Some(c) = ed.session.chat.as_mut().filter(|_| ours && scope == Scope::Refused)
        && a.kind == Kind::Host
        && !a.users
    {
        let g = crate::allow::Grant::Host(a.host.clone(), a.port);
        if !c.also_refused.contains(&g) {
            c.also_refused.push(g);
        }
    }
    keep(ed, a.session, a.run, a.kind, &a.host, also, a.port, scope);
}

/// An answer for `host:port` of `kind` (`session`'s, the proxy's id) and `also` (the registries the card listed, at
/// that port): to the proxy, kept as far as it reaches, said. Only a host's lasts in the project (`A`): raw and
/// lookups have no `A` (F7).
#[allow(clippy::too_many_arguments)]
fn keep(
    ed: &mut Editor,
    session: u64,
    run: u64,
    kind: Kind,
    host: &str,
    also: &[String],
    port: u16,
    scope: Scope,
) {
    let scope = match scope {
        Scope::Project if kind != Kind::Host => Scope::Session,
        s => s,
    };
    let (at, mut doing) = (at(kind, host, port), doing(kind, host, port));
    for h in std::iter::once(host).chain(also.iter().map(String::as_str)) {
        send(ToProxy::Answer { session, run, kind, host: h.into(), port, scope });
    }
    if !also.is_empty() {
        doing = format!("{doing} and {}", also.join(", "));
    }
    if scope == Scope::Chat {
        // (the conversation's: its runs' rules carry it, and its end takes it back from the proxy — `chat_ended`)
        let ours = ed.session.proxy_id() == session;
        if let Some(c) = ed.session.chat.as_mut().filter(|_| ours) {
            for h in std::iter::once(host).chain(also.iter().map(String::as_str)) {
                c.allowed.add(&crate::allow::Grant::Host(h.into(), port));
            }
        }
        ed.caps_sync();
        ed.session
            .checkpoints
            .tell
            .push(format!("The user let your commands {doing} — for this conversation."));
        return ed.set_success(format!("Commands may {doing} in this conversation — :granted lists it"));
    }
    for h in also {
        keep_one(ed, session, kind, h, port, scope);
    }
    keep_one(ed, session, kind, host, port, scope);
    match scope {
        Scope::Run => ed.set_success(format!("Commands may {doing} in this run")),
        Scope::Session => {
            ed.session
                .checkpoints
                .tell
                .push(format!("The user let your commands {doing} — for this session."));
            ed.set_success(format!("Commands may {doing} in this session — :granted lists it"));
        }
        Scope::Project => {
            ed.session.checkpoints.tell.push(format!(
                "The user let your commands {doing} — in this project, until they take it back."
            ));
            ed.set_success(format!("Commands may {doing} in this project — :granted lists it"));
        }
        Scope::Refused => {
            ed.session.checkpoints.tell.push(format!(
                "The user refused your commands to {doing} for this session: don't try it again."
            ));
            let lift = match kind {
                Kind::Host => " — space m s on the run lets it through",
                _ => "",
            };
            ed.set_status(format!("{at} refused for this session{lift}"));
        }
        Scope::Chat => {}
    }
}

/// `keep`'s record of one host, as far as `scope` reaches: the session's list (`s`, `n` takes it off) or the
/// project's (`A`).
fn keep_one(ed: &mut Editor, session: u64, kind: Kind, host: &str, port: u16, scope: Scope) {
    // (the card's session is the one in front — it parks otherwise; what's kept here is that session's)
    let ours = ed.session.proxy_id() == session;
    let this = |(k, h, p): &(Kind, String, u16)| *k == kind && h == host && *p == port;
    match scope {
        Scope::Session if ours && !ed.session.hosts.iter().any(this) => {
            ed.session.hosts.push((kind, host.into(), port));
        }
        Scope::Project => {
            let kept = host.to_string();
            crate::allow::keep_later(ed, move |k| k.add_host(&kept, port));
        }
        Scope::Refused if ours => ed.session.hosts.retain(|e| !this(e)),
        _ => {}
    }
}

/// The session in front's conversation ends (a new one, `C-l`; the chat closed): the hosts its `a` let through go
/// from the proxy — but those the session's `s` holds too. (The proxy keeps a conversation's answers as its
/// session's: right while a session has one conversation at a time, as it does.)
pub fn chat_ended(ed: &Editor) {
    let Some(c) = ed.session.chat.as_ref() else { return };
    let session = ed.session.proxy_id();
    for (host, port) in &c.allowed.hosts {
        let held = |(k, h, p): &(Kind, String, u16)| *k == Kind::Host && h == host && p == port;
        if !ed.session.hosts.iter().any(held) {
            send(ToProxy::Taken { session, kind: Kind::Host, host: host.clone(), port: *port });
        }
    }
}

/// A host granted before a run goes (`:sandbox-blocked`'s choice, Claude's `also` — the session in front), with
/// what a yes on it lets in (`with_suggested`): `Once` lifts a refusal for the session (the run's own rules carry
/// the hosts — `Extra.grant`, `tools::with_allowed`), the others as the card's `a` and `s`.
pub fn granted(ed: &mut Editor, host: &str, port: u16, scope: crate::allow::Scope) {
    let session = ed.session.proxy_id();
    let also = &with_suggested(host, port)[1..];
    match scope {
        crate::allow::Scope::Once => {
            send(ToProxy::Lifted { session, kind: Kind::Host, host: host.into(), port })
        }
        crate::allow::Scope::ThisChat => keep(ed, session, 0, Kind::Host, host, also, port, Scope::Chat),
        // (piece C: a host is never kept past a session — `Caps::storable`; always is the setting)
        crate::allow::Scope::Session | crate::allow::Scope::Always => {
            keep(ed, session, 0, Kind::Host, host, also, port, Scope::Session)
        }
    }
}

/// Run `run` ended (reaped — its panel's end, in its session): a card for it goes, and the proxy is told — a
/// question asked for it goes to whoever still waits on it (another run's connection, or one of a process it
/// left), asked again: a card for that one comes.
pub fn run_ended(ed: &mut Editor, run: Option<u64>) {
    let Some(run) = run else { return };
    let its = |w: &What| matches!(w, What::Host(h) | What::HostRaw(h) | What::Lookup(h) if h.run == run);
    ed.drop_cards_for(its, "its run ended");
    send(ToProxy::RunEnded { run });
}

/// Session `s` ended: the proxy drops its answers.
pub fn session_ended(s: &crate::session::Session) {
    send(ToProxy::SessionEnded { session: s.proxy_id() });
}

/// The user took back `host:port` of `kind` for session `s` (and what the project keeps): asked again.
pub fn taken(s: &crate::session::Session, kind: Kind, host: &str, port: u16) {
    send(ToProxy::Taken { session: s.proxy_id(), kind, host: host.into(), port });
}

/// A message to the proxy — none while it isn't running: nothing it holds to change (tests: kept to look at, and
/// sent as well where a test's runs started it — the compat driver's: else a `n` never reaches the connection that
/// waits on it, which sits out its hold, and the tool's next try is asked again).
fn send(m: ToProxy) {
    #[cfg(test)]
    {
        if let (Some(p), Some(copy)) = (crate::sandbox::proxy::running(), copied(&m)) {
            p.send(copy);
        }
        SENT.with(|s| s.borrow_mut().push(m));
    }
    #[cfg(not(test))]
    if let Some(p) = crate::sandbox::proxy::running() {
        p.send(m);
    }
}

/// The session's messages the main loop sends, copied — not a run's end: tests here name made-up runs, which may be
/// another test's real one (a session is its root's, each test's own).
#[cfg(test)]
fn copied(m: &ToProxy) -> Option<ToProxy> {
    Some(match m {
        ToProxy::Answer { session, run, kind, host, port, scope } => ToProxy::Answer {
            session: *session,
            run: *run,
            kind: *kind,
            host: host.clone(),
            port: *port,
            scope: *scope,
        },
        ToProxy::Taken { session, kind, host, port } => {
            ToProxy::Taken { session: *session, kind: *kind, host: host.clone(), port: *port }
        }
        ToProxy::Lifted { session, kind, host, port } => {
            ToProxy::Lifted { session: *session, kind: *kind, host: host.clone(), port: *port }
        }
        ToProxy::SessionEnded { session } => ToProxy::SessionEnded { session: *session },
        _ => return None,
    })
}

#[cfg(test)]
thread_local! {
    /// What this test thread's editor told the proxy.
    pub static SENT: std::cell::RefCell<Vec<ToProxy>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests;
