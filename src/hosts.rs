//! The egress proxy's questions, on the main loop (`target/sb/step5e-plan.md`). A host a run's rules don't name
//! is held by tarae's proxy, which asks here (`proxy::FromProxy::Ask`): one card per host in the run's own
//! session (`What::Host`) — `y` this run · `s` this session · `A` always here · `n` no, for the session. The
//! answer goes back as `ToProxy::Answer` for the card's own session and run, never the session in front's: a card
//! parks with its session, so only that session's keys reach it. How a connection came to the proxy (a proxy
//! variable, a supervisor's swap, a redirect in the session's network) isn't this module's: it sees hosts, runs
//! and sessions.
//!
//! What lasts: `y` lives in the proxy for that run; `s` and `n` in the proxy for the session, `s` listed in
//! `Session.hosts` (`:sandbox-allowed` takes it back); `A` in `$state/sandbox.json` (`allow::Allowed.hosts`),
//! which the next runs' rules carry (`tools::with_allowed`).

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
}

impl Ask {
    pub fn at(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether `other` asks for the same host in the same session (one card for both).
    fn same(&self, other: &Ask) -> bool {
        (self.session, &self.host, self.port) == (other.session, &other.host, other.port)
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
    let a = Ask { session, run, host, port, process, label, users, left };
    ed.with_session(id, move |ed| {
        let front = !ed.background();
        let cards = ed.session.parked.iter_mut().chain(ed.offers.iter_mut().filter(|_| front));
        for o in cards {
            if let What::Host(h) = &mut o.what
                && h.same(&a)
            {
                if h.run != a.run {
                    let fresh = card(a);
                    (o.question, o.what) = (fresh.question, fresh.what);
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

/// The card for `a`.
pub fn card(a: Ask) -> Offer {
    use crate::cards::{Asked, Request, Why};
    let by = match (a.left, a.users) {
        (true, _) => format!("asked by {}, left running by a run that's over", a.process),
        (false, true) => format!("asked by {} in `{}`, your command", a.process, a.label),
        (false, false) => format!("asked by {} in `{}`, Claude's command", a.process, a.label),
    };
    let hold = crate::sandbox::proxy::Limits::default().hold.as_secs();
    let named = registry_of(&crate::sandboxpack::get().registries, &a.host, a.port)
        .map(|pack| format!("\n{} is the {pack} pack's registry", a.host))
        .unwrap_or_default();
    let question = format!(
        "on port {}, through tarae's proxy\n{by}{named}\n\nIts connection waits up to {hold} s for your answer; \
         after that it's refused, and your answer counts from its next try. llm.sandbox-hosts lists the hosts \
         reached without asking.",
        a.port
    );
    let asked = Asked::new(Why::Host, Request::Host(a.at()));
    let title = format!("{TITLE} {}?", a.host);
    Offer::new(title, &question, "", String::new(), What::Host(Box::new(a))).deciding(0..2).asked(asked)
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
    keep(ed, a.session, a.run, &a.host, a.port, scope);
}

/// An answer for `host:port` (`session`'s, the proxy's id): to the proxy, kept as far as it reaches, said.
fn keep(ed: &mut Editor, session: u64, run: u64, host: &str, port: u16, scope: Scope) {
    let at = format!("{host}:{port}");
    send(ToProxy::Answer { session, run, host: host.into(), port, scope });
    // (the card's session is the one in front — it parks otherwise; what's kept here is that session's)
    let ours = ed.session.proxy_id() == session;
    let tell = |ed: &mut Editor, s: String| ed.session.checkpoints.tell.push(s);
    match scope {
        Scope::Run => ed.set_success(format!("{at} for this run")),
        Scope::Session => {
            if ours && !ed.session.hosts.iter().any(|(h, p)| h == host && *p == port) {
                ed.session.hosts.push((host.into(), port));
            }
            tell(ed, format!("The user let your commands reach {at} — for this session."));
            ed.set_success(format!("Commands may reach {at} in this session — :sandbox-allowed lists it"));
        }
        Scope::Project => {
            let kept = host.to_string();
            crate::allow::keep_later(ed, move |k| k.add_host(&kept, port));
            tell(
                ed,
                format!("The user let your commands reach {at} — in this project, until they take it back."),
            );
            ed.set_success(format!("Commands may reach {at} in this project — :sandbox-allowed lists it"));
        }
        Scope::Refused => {
            if ours {
                ed.session.hosts.retain(|(h, p)| h != host || *p != port);
            }
            tell(ed, format!("The user refused your commands {at} for this session: don't try it again."));
            ed.set_status(format!("{at} refused for this session — space m s on the run lets it through"));
        }
    }
}

/// A host granted from what a run was refused (`:sandbox-blocked`, the session in front), before that run goes
/// again: `Once` lifts a refusal for the session (the run's own rules carry the host — `Extra.grant`), the others
/// as the card's `s` and `A`.
pub fn granted(ed: &mut Editor, host: &str, port: u16, scope: crate::allow::Scope) {
    let session = ed.session.proxy_id();
    match scope {
        crate::allow::Scope::Once => send(ToProxy::Lifted { session, host: host.into(), port }),
        crate::allow::Scope::Session | crate::allow::Scope::ThisChat => {
            keep(ed, session, 0, host, port, Scope::Session)
        }
        crate::allow::Scope::Always => keep(ed, session, 0, host, port, Scope::Project),
    }
}

/// Run `run` ended (reaped — its panel's end, in its session): a card for it goes, and the proxy is told — a
/// question asked for it goes to whoever still waits on it (another run's connection, or one of a process it
/// left), asked again: a card for that one comes.
pub fn run_ended(ed: &mut Editor, run: Option<u64>) {
    let Some(run) = run else { return };
    ed.drop_cards_for(|w| matches!(w, What::Host(h) if h.run == run), "its run ended");
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
