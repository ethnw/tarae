//! The egress proxy's questions, on the main loop (`target/sb/step5e-plan.md`). A host a run's rules don't name
//! is held by tarae's proxy, which asks here (`proxy::FromProxy::Ask`): one card per host in the run's own
//! session (`What::Host`) — `y` once (this run) · `s` this session · `A` always here · `n` no, for the session. The
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

use crate::editor::Editor;
use crate::event::SessionId;
use crate::offer::{Offer, What};
use crate::sandbox::proxy::{FromProxy, Scope, ToProxy};

/// A host a run asked to reach, as its card holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    /// The proxy's session (`proxy::session_of` its project) and run — what the answer names.
    pub session: u64,
    pub run: u64,
    pub host: String,
    pub port: u16,
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
    /// Its run ended while the card waited (the tool gave up first — npm's ~20 s): the card stays, and `y` lets
    /// the session's next run reach it (frictions29 16: the card went with the run, and the chat worked around
    /// the host rather than asking again).
    pub ended: bool,
}

impl Ask {
    /// `host:port`.
    pub fn at(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether `other` asks for the same in the same session (one card for both).
    fn same(&self, other: &Ask) -> bool {
        (self.session, &self.host, self.port) == (other.session, &other.host, other.port)
    }
}

/// How `:granted` lists it and `:sandbox-disallow` takes it (`host a:443`).
pub fn listed(host: &str, port: u16) -> String {
    format!("host {host}:{port}")
}

/// What `listed` says, read back.
pub fn unlisted(s: &str) -> Option<(String, u16)> {
    crate::allow::host_port(s.strip_prefix("host ")?)
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
    let FromProxy::Ask { session, run, host, port, process } = ask;
    let found = find_run(ed, run).map(|(id, label, users)| (id, label, users, false)).or_else(|| {
        let s = std::iter::once(&ed.session).chain(&ed.sessions);
        let id = s.into_iter().find(|s| s.proxy_id() == session)?.id;
        Some((id, String::new(), false, true))
    });
    let Some((id, label, users, left)) = found else {
        let msg = format!("the proxy asked about {host}:{port} for a session that's gone");
        return crate::log::line("sandbox", "info", &msg);
    };
    let also = suggested(&crate::sandboxpack::get().registries, &host, port);
    let (open, hosts) = (ed.config.llm.sandbox_network, ed.config.llm.sandbox_hosts.clone());
    let a = Ask { session, run, host, port, process, label, users, left, open, hosts, also, ended: false };
    ed.with_session(id, move |ed| {
        let front = !ed.background();
        let cards = ed.session.parked.iter_mut().chain(ed.offers.iter_mut().filter(|_| front));
        for o in cards {
            if let Some(h) = o.host()
                && h.same(&a)
            {
                // (another run's)
                if h.run != a.run {
                    // (whole: an ended card's extra line moved what's decided and the warning's row)
                    let fresh = card(a);
                    (o.question, o.what, o.asked, o.decided, o.warning) =
                        (fresh.question, fresh.what, fresh.asked, fresh.decided, fresh.warning);
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

/// The card for `a` — the network open's, else the closed one's.
pub fn card(a: Ask) -> Offer {
    use crate::cards::{Asked, Request, Why};
    if !a.open {
        return closed_card(a);
    }
    let by = match (a.left, a.users) {
        (true, _) => format!("asked by {}, left running by a run that's over", a.process),
        (false, true) => format!("asked by {} in `{}`, your command", a.process, a.label),
        (false, false) => format!("asked by {} in `{}`, Claude's command", a.process, a.label),
    };
    let (waits, lasting) = (waits(), open_lasts(&a.hosts));
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
    // (its run over, no connection waits for the answer: the wait isn't said)
    let (ended, waits) = match a.ended {
        true => (format!("\n{ENDED}"), String::new()),
        false => (String::new(), format!("{waits} ")),
    };
    let question =
        format!("on port {}, through tarae's proxy\n{by}{named}{ended}\n\n{waits}{lasting}", a.port);
    let decided = 2 + usize::from(pack.is_some()) + usize::from(a.ended);
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

/// A card whose run ended, said under what asked.
const ENDED: &str = "Its run ended before this was answered — y lets the next run reach it.";

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
    if a.ended {
        lines.push(ENDED.into());
    }
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

/// The line naming pack `pack`'s registry and `also`, what a yes lets in with it — the language's own name starts it
/// (`grammar::pretty`, its pack's `title`: `JavaScript`, not the folder's name capitalised): `Rust's registry` (58
/// cells with its others, inside the 60 a host card's answers make it).
fn registry_line(pack: &str, also: &[String]) -> String {
    let p = crate::grammar::pretty(pack);
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
    // (its run over, no connection waits for the answer: the wait isn't said)
    let mut s = match a.ended {
        true => CLOSED_LASTS.to_string(),
        false => format!("{}\n\n{CLOSED_LASTS}", waits()),
    };
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
    // (its run over: `y` is for the next one — it carries the host in its rules, then it's spent)
    if a.ended && scope == Scope::Run {
        return next_run(ed, &a);
    }
    // (Q6: a yes grants the registries the card listed with it; a no is for the host asked alone)
    let also = if scope == Scope::Refused { &[][..] } else { &a.also[..] };
    let scope = if scope == Scope::Project && !a.open { Scope::Session } else { scope };
    // (C2: a no on Claude's command is its `also`'s too — that conversation asks for the host on no card again)
    let ours = ed.session.proxy_id() == a.session;
    if let Some(c) = ed.session.chat.as_mut().filter(|_| ours && scope == Scope::Refused)
        && !a.users
    {
        let g = crate::allow::Grant::Host(a.host.clone(), a.port);
        if !c.also_refused.contains(&g) {
            c.also_refused.push(g);
        }
    }
    keep(ed, a.session, a.run, &a.host, also, a.port, scope);
}

/// An answer for `host:port` (`session`'s, the proxy's id) and `also` (the registries the card listed, at that
/// port): to the proxy, kept as far as it reaches, said.
fn keep(ed: &mut Editor, session: u64, run: u64, host: &str, also: &[String], port: u16, scope: Scope) {
    let (at, mut doing) = (format!("{host}:{port}"), format!("reach {host}:{port}"));
    for h in std::iter::once(host).chain(also.iter().map(String::as_str)) {
        send(ToProxy::Answer { session, run, host: h.into(), port, scope });
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
        keep_one(ed, session, h, port, scope);
    }
    keep_one(ed, session, host, port, scope);
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
            // (for every checkout of the repository, said as the other cards say it — review J 3)
            let grants: Vec<crate::allow::Grant> = std::iter::once(host)
                .chain(also.iter().map(String::as_str))
                .map(|h| crate::allow::Grant::Host(h.into(), port))
                .collect();
            let (when, toast) = crate::allow::lasting(ed, crate::allow::Scope::Always, &grants);
            ed.session.checkpoints.tell.push(format!("The user let your commands {doing} — {when}."));
            ed.set_success(format!("Commands may {doing}{toast}"));
        }
        Scope::Refused => {
            ed.session.checkpoints.tell.push(format!(
                "The user refused your commands to {doing} for this session: don't try it again."
            ));
            ed.set_status(format!("{at} refused for this session — space m s on the run lets it through"));
        }
        Scope::Chat => {}
    }
}

/// `keep`'s record of one host, as far as `scope` reaches: the session's list (`s`, `n` takes it off) or the
/// project's (`A`).
fn keep_one(ed: &mut Editor, session: u64, host: &str, port: u16, scope: Scope) {
    // (the card's session is the one in front — it parks otherwise; what's kept here is that session's)
    let ours = ed.session.proxy_id() == session;
    let this = |(h, p): &(String, u16)| h == host && *p == port;
    match scope {
        Scope::Session if ours && !ed.session.hosts.iter().any(this) => {
            ed.session.hosts.push((host.into(), port));
        }
        // (as a card's other `A`s: a failed take-back of it ends once this is written — `allow::keep_answer`)
        Scope::Project => crate::allow::keep_answer(ed, vec![crate::allow::Grant::Host(host.into(), port)]),
        Scope::Refused if ours => ed.session.hosts.retain(|e| !this(e)),
        _ => {}
    }
}

/// The session in front's conversation ends (a new one, `C-l`; the chat closed): the hosts its `a` let through go
/// from the proxy — but those the session's `s` holds too. (The proxy keeps a conversation's answers as its
/// session's: right while a session has one conversation at a time, as it does.) A card whose run of Claude's
/// ended goes too — its `y` was for that conversation's next run; what one already gave goes with the chat's
/// `hosts_next` (`Chat::conversation_left`).
pub fn chat_ended(ed: &mut Editor) {
    ed.drop_cards(|w| matches!(w, What::Host(h) if h.ended));
    let Some(c) = ed.session.chat.as_ref() else { return };
    let session = ed.session.proxy_id();
    for (host, port) in &c.allowed.hosts {
        if !ed.session.hosts.iter().any(|(h, p)| h == host && p == port) {
            send(ToProxy::Taken { session, host: host.clone(), port: *port });
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
        crate::allow::Scope::Once => send(ToProxy::Lifted { session, host: host.into(), port }),
        crate::allow::Scope::ThisChat => keep(ed, session, 0, host, also, port, Scope::Chat),
        // (piece C: a host is never kept past a session — `Caps::storable`; always is the setting)
        crate::allow::Scope::Session | crate::allow::Scope::Always => {
            keep(ed, session, 0, host, also, port, Scope::Session)
        }
    }
}

/// `y` on a card whose run ended: the host (and the registries listed with it) for the next run of this
/// conversation's Claude that starts, once (`Chat.hosts_next`, taken in `tools::guard_then_go`) — Claude hears it
/// with the next message, never a run it didn't ask for (user decision 2026-10-19). Its conversation over, the card
/// went with it (`chat_ended`).
fn next_run(ed: &mut Editor, a: &Ask) {
    let hosts: Vec<String> = std::iter::once(&a.host).chain(&a.also).cloned().collect();
    let ours = ed.session.proxy_id() == a.session;
    let Some(c) = ed.session.chat.as_mut().filter(|_| ours) else { return };
    for h in &hosts {
        if !c.hosts_next.iter().any(|(n, p)| n == h && *p == a.port) {
            c.hosts_next.push((h.clone(), a.port));
        }
    }
    let doing = format!("reach {}:{}", a.host, a.port);
    ed.session.checkpoints.tell.push(format!(
        "The user let your next command {doing} — its run had ended before they answered. Run it again if you \
         still need it."
    ));
    ed.set_success(format!("Claude's next run may {doing}"));
}

/// Run `run` ended (reaped — its panel's end, in its session): a card for a run of Claude's stays, saying so — `y`
/// lets Claude's next run reach the host (frictions29 16); one for the user's own run goes, as before (they run it
/// again themselves, and its card comes). The proxy is told: a question asked for it goes to whoever still waits on
/// it (another run's connection, or one of a process it left), asked again: that one's card takes it.
pub fn run_ended(ed: &mut Editor, run: Option<u64>) {
    let Some(run) = run else { return };
    let yours = |w: &What| matches!(w, What::Host(h) if h.run == run && h.users && !h.left);
    ed.drop_cards_for(yours, "its run ended");
    let front = !ed.background();
    let cards = ed.session.parked.iter_mut().chain(ed.offers.iter_mut().filter(|_| front));
    for o in cards {
        let Some(a) = o.host().filter(|h| h.run == run && !h.ended && !h.left).cloned() else { continue };
        let fresh = card(Ask { ended: true, ..a });
        (o.question, o.what, o.asked, o.decided, o.warning) =
            (fresh.question, fresh.what, fresh.asked, fresh.decided, fresh.warning);
        o.rev += 1;
    }
    send(ToProxy::RunEnded { run });
}

/// Session `s` ended: the proxy drops its answers.
pub fn session_ended(s: &crate::session::Session) {
    send(ToProxy::SessionEnded { session: s.proxy_id() });
}

/// The user took back `host:port` for session `s` (and what the project keeps): asked again.
pub fn taken(s: &crate::session::Session, host: &str, port: u16) {
    send(ToProxy::Taken { session: s.proxy_id(), host: host.into(), port });
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
        ToProxy::Answer { session, run, host, port, scope } => {
            ToProxy::Answer { session: *session, run: *run, host: host.clone(), port: *port, scope: *scope }
        }
        ToProxy::Taken { session, host, port } => {
            ToProxy::Taken { session: *session, host: host.clone(), port: *port }
        }
        ToProxy::Lifted { session, host, port } => {
            ToProxy::Lifted { session: *session, host: host.clone(), port: *port }
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
