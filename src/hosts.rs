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
    let a = Ask { session, run, kind, host, port, asked, process, label, users, left };
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

/// The card for `a` — of its kind.
pub fn card(a: Ask) -> Offer {
    use crate::cards::{Asked, Request, Why};
    let by = match (a.left, a.users) {
        (true, _) => format!("asked by {}, left running by a run that's over", a.process),
        (false, true) => format!("asked by {} in `{}`, your command", a.process, a.label),
        (false, false) => format!("asked by {} in `{}`, Claude's command", a.process, a.label),
    };
    let hold = crate::sandbox::proxy::Limits::default().hold.as_secs();
    let waits = format!(
        "Its connection waits up to {hold} s for your answer; after that it's refused, and your answer counts from \
         its next try."
    );
    match a.kind {
        Kind::Host => {
            let named = registry_of(&crate::sandboxpack::get().registries, &a.host, a.port)
                .map(|pack| format!("\n{} is the {pack} pack's registry", a.host))
                .unwrap_or_default();
            let question = format!(
                "on port {}, through tarae's proxy\n{by}{named}\n\n{waits} llm.sandbox-hosts lists the hosts reached \
                 without asking.",
                a.port
            );
            let asked = Asked::new(Why::Host, Request::Host(a.at()));
            let title = format!("{TITLE} {}?", a.host);
            Offer::new(title, &question, "", String::new(), What::Host(Box::new(a)))
                .deciding(0..2)
                .asked(asked)
        }
        // (Q2: what passes can't be checked — said in the warning colour)
        Kind::Raw => {
            let question = format!(
                "on port {}, through tarae's proxy\n{by}\nNot TLS or HTTP: tarae can't see what passes, only that it \
                 goes to the name the command looked up.\n\n{waits} A line {} in llm.sandbox-hosts lets it through \
                 without asking.",
                a.port,
                a.at()
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

/// The user's answer on `a`'s card (`id`): to the proxy for the card's session and run, kept as far as it
/// reaches, said — and Claude hears it with the next message (but a run's own `y`).
pub fn answer(ed: &mut Editor, id: u64, a: Ask, scope: Scope) {
    ed.offer_finished(id, Ok(()));
    keep(ed, a.session, a.run, a.kind, &a.host, a.port, scope);
}

/// An answer for `host:port` of `kind` (`session`'s, the proxy's id): to the proxy, kept as far as it reaches, said.
/// Only a host's lasts in the project (`A`): raw and lookups have no `A` (F7).
fn keep(ed: &mut Editor, session: u64, run: u64, kind: Kind, host: &str, port: u16, scope: Scope) {
    let scope = match scope {
        Scope::Project if kind != Kind::Host => Scope::Session,
        s => s,
    };
    let (at, doing) = (at(kind, host, port), doing(kind, host, port));
    send(ToProxy::Answer { session, run, kind, host: host.into(), port, scope });
    // (the card's session is the one in front — it parks otherwise; what's kept here is that session's)
    let ours = ed.session.proxy_id() == session;
    let tell = |ed: &mut Editor, s: String| ed.session.checkpoints.tell.push(s);
    let this = |(k, h, p): &(Kind, String, u16)| *k == kind && h == host && *p == port;
    match scope {
        Scope::Run => ed.set_success(format!("Commands may {doing} in this run")),
        Scope::Session => {
            if ours && !ed.session.hosts.iter().any(this) {
                ed.session.hosts.push((kind, host.into(), port));
            }
            tell(ed, format!("The user let your commands {doing} — for this session."));
            ed.set_success(format!("Commands may {doing} in this session — :granted lists it"));
        }
        Scope::Project => {
            let kept = host.to_string();
            crate::allow::keep_later(ed, move |k| k.add_host(&kept, port));
            tell(
                ed,
                format!("The user let your commands {doing} — in this project, until they take it back."),
            );
            ed.set_success(format!("Commands may {doing} in this project — :granted lists it"));
        }
        Scope::Refused => {
            if ours {
                ed.session.hosts.retain(|e| !this(e));
            }
            tell(
                ed,
                format!("The user refused your commands to {doing} for this session: don't try it again."),
            );
            let lift = match kind {
                Kind::Host => " — space m s on the run lets it through",
                _ => "",
            };
            ed.set_status(format!("{at} refused for this session{lift}"));
        }
    }
}

/// A host granted from what a run was refused (`:sandbox-blocked`, the session in front), before that run goes
/// again: `Once` lifts a refusal for the session (the run's own rules carry the host — `Extra.grant`), the others
/// as the card's `s` and `A`.
pub fn granted(ed: &mut Editor, host: &str, port: u16, scope: crate::allow::Scope) {
    let session = ed.session.proxy_id();
    match scope {
        crate::allow::Scope::Once => {
            send(ToProxy::Lifted { session, kind: Kind::Host, host: host.into(), port })
        }
        crate::allow::Scope::Session | crate::allow::Scope::ThisChat => {
            keep(ed, session, 0, Kind::Host, host, port, Scope::Session)
        }
        crate::allow::Scope::Always => keep(ed, session, 0, Kind::Host, host, port, Scope::Project),
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

/// A message to the proxy — none while it isn't running: nothing it holds to change (tests: kept to look at).
fn send(m: ToProxy) {
    #[cfg(test)]
    SENT.with(|s| s.borrow_mut().push(m));
    #[cfg(not(test))]
    if let Some(p) = crate::sandbox::proxy::running() {
        p.send(m);
    }
}

#[cfg(test)]
thread_local! {
    /// What this test thread's editor told the proxy.
    pub static SENT: std::cell::RefCell<Vec<ToProxy>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests;
