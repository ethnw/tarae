//! The host card on the main loop: where it goes, what each answer tells the proxy and keeps, who can answer it,
//! when it goes. The proxy is stood in by what `send` keeps (`SENT`); runs are records with a sandboxed policy,
//! no process. Names are `.example.invalid`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::*;
use crate::event::SessionId;
use crate::key::{Code, Key};
use crate::sandbox::proxy::session_of;
use crate::sandbox::{Env, Guard, Policy};

const HOST: &str = "static.example.invalid";

/// An editor in `front` with a session for `back` behind it — folders in the test's own temp folder — each with
/// a sandboxed run of Claude's under way: their policies' run ids.
fn two(name: &str) -> (Editor, PathBuf, SessionId, u64, u64) {
    let dir = crate::scratch::Scratch::new(&format!("hosts-{name}")).into_path();
    for p in ["front", "back"] {
        std::fs::create_dir_all(dir.join(p)).unwrap();
    }
    let dir = std::fs::canonicalize(dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.join("front"), "front".into());
    ed.allow_path = Some(dir.join("state/sandbox.json"));
    let back = ed.session_open(&dir.join("back")).unwrap();
    assert!(ed.session_switch(0));
    let front_run = running(&mut ed.session, "cargo build");
    let i = ed.sessions.iter().position(|s| s.id == back).unwrap();
    let back_run = running(&mut ed.sessions[i], "npm install");
    (ed, dir, back, front_run, back_run)
}

/// A run of Claude's under way in `s` (no process), sandboxed with the network open and a list of hosts: its
/// policy's run id.
fn running(s: &mut crate::session::Session, cmd: &str) -> u64 {
    let env = Env { root: s.root.clone(), network: true, hosts: Some(Vec::new()), ..Default::default() };
    let p = Policy::for_project(&env);
    let id = p.run_id;
    let task = crate::task::Task { name: cmd.into(), cmd: cmd.into(), cwd: s.root.clone() };
    let mut r = crate::task::Run::for_test(task);
    (r.by_claude, r.guard, r.state) = (true, Some(Guard::In(Box::new(p))), crate::task::State::Running);
    s.task_run = Some(r);
    id
}

/// The proxy asking, for run `run` of the project at `root`.
fn ask(root: &Path, run: u64, host: &str) -> FromProxy {
    FromProxy::Ask {
        session: session_of(root),
        run,
        kind: Kind::Host,
        host: host.into(),
        port: 443,
        process: "curl".into(),
        asked: vec![],
    }
}

fn hosts_cards(o: &[Offer]) -> Vec<&Ask> {
    o.iter().filter_map(Offer::host).collect()
}

/// The front card drawn and in sight long enough for a key to answer it.
fn settled(ed: &mut Editor) {
    crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
    let o = (ed.offers[0].id, ed.offers[0].rev);
    ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE)));
}

fn key(ed: &mut Editor, c: char) {
    ed.handle_key(Key::plain(Code::Char(c)));
}

/// What the editor told the proxy since the last look.
fn sent() -> Vec<ToProxy> {
    SENT.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

fn answered(m: &ToProxy) -> Option<(u64, u64, String, u16, Scope)> {
    match m {
        ToProxy::Answer { kind: Kind::Host, session, run, host, port, scope } => {
            Some((*session, *run, host.clone(), *port, *scope))
        }
        _ => None,
    }
}

/// One card per host, in the session of the run that asked: the proxy asking again (another connection, the
/// tool's retry) joins the card up, and a run of a session behind gets its card parked there — never pushed on the
/// session in front, whose keys would answer it.
#[test]
fn one_card_per_host_in_its_runs_session() {
    let (mut ed, dir, back, front_run, back_run) = two("one");
    asked(&mut ed, ask(&dir.join("front"), front_run, HOST));
    asked(&mut ed, ask(&dir.join("front"), front_run, HOST));
    let up = hosts_cards(&ed.offers);
    assert_eq!(up.len(), 1, "one card a host");
    assert_eq!((up[0].run, up[0].label.as_str(), up[0].process.as_str()), (front_run, "cargo build", "curl"));
    assert!(ed.offers[0].question.contains("asked by curl in `cargo build`, Claude's command"));
    assert_eq!(ed.offers[0].title, format!("Let a command reach {HOST}?"));
    assert!(ed.offers[0].question.starts_with("on port 443, through tarae's proxy\n"));
    asked(&mut ed, ask(&dir.join("back"), back_run, HOST));
    assert_eq!(hosts_cards(&ed.offers).len(), 1, "the session behind's isn't in front");
    let parked = &ed.sessions.iter().find(|s| s.id == back).unwrap().parked;
    let parked = hosts_cards(parked);
    assert_eq!((parked.len(), parked[0].session), (1, session_of(&dir.join("back"))));
    std::fs::remove_dir_all(&dir).ok();
}

/// The card goes with whichever run asks now: the proxy asks again for another run (the first one's ended, or
/// was let through alone) and the card up for that host names it — so `y` answers the run that waits, never one
/// that no longer does — said anew (its text changed: it settles again).
#[test]
fn the_card_moves_to_the_run_asking_now() {
    let (mut ed, dir, ..) = two("moves");
    let first = ed.session.task_run.as_ref().map(|r| match &r.guard {
        Some(Guard::In(p)) => p.run_id,
        _ => 0,
    });
    asked(&mut ed, ask(&dir.join("front"), first.unwrap(), HOST));
    let rev = ed.offers[0].rev;
    let next = running(&mut ed.session, "cargo test");
    asked(&mut ed, ask(&dir.join("front"), next, HOST));
    let up = hosts_cards(&ed.offers);
    assert_eq!(
        (up.len(), up[0].run, up[0].label.as_str()),
        (1, next, "cargo test"),
        "one card, the run asking now"
    );
    assert!(ed.offers[0].question.contains("in `cargo test`") && ed.offers[0].rev > rev);
    let _ = sent();
    settled(&mut ed);
    key(&mut ed, 'y');
    let told: Vec<_> = sent().iter().filter_map(answered).map(|t| (t.1, t.4)).collect();
    assert_eq!(told, [(next, Scope::Run)], "`y` for the run that waits");
    std::fs::remove_dir_all(&dir).ok();
}

/// A question for a run that's over — what it left running asks (Linux: a daemon keeps its run's way to the
/// proxy), or the run's record is kept after its end — still gets a card, in the session the question names,
/// saying what asked was left by a run that's over: never a wait with no card. A session that's gone raises
/// nothing.
#[test]
fn a_process_left_by_a_run_thats_over_gets_a_card() {
    let (mut ed, dir, ..) = two("left");
    asked(&mut ed, ask(&dir.join("front"), 999_999, HOST));
    let up = hosts_cards(&ed.offers);
    assert_eq!((up.len(), up[0].run, up[0].left), (1, 999_999, true));
    assert!(ed.offers[0].question.contains("asked by curl, left running by a run that's over"));
    // (a run whose record is kept after its end is over too)
    let ended = running(&mut ed.session, "cargo build");
    ed.session.task_run.as_mut().unwrap().state = crate::task::State::Passed;
    asked(&mut ed, ask(&dir.join("front"), ended, "other.example.invalid"));
    assert!(
        hosts_cards(&ed.offers).iter().any(|a| a.run == ended && a.left),
        "the record kept isn't the run"
    );
    asked(&mut ed, ask(&dir.join("gone"), 999_998, "third.example.invalid"));
    assert_eq!(hosts_cards(&ed.offers).len(), 2, "a session that's gone raises nothing");
    std::fs::remove_dir_all(&dir).ok();
}

/// Each key reaches as far as it says, for the card's own session and run: `y` this run, `s` the session (listed
/// in it, Claude told), `A` the project (kept in `sandbox.json`, read back for the next runs), `n` refused for the
/// session (no longer listed). `a` isn't one of its answers, and says so.
#[test]
fn each_answer_reaches_as_far_as_its_key() {
    let (mut ed, dir, _, run, _) = two("keys");
    let session = session_of(&dir.join("front"));
    let _ = sent();
    for (k, scope) in [('y', Scope::Run), ('s', Scope::Session), ('A', Scope::Project), ('n', Scope::Refused)]
    {
        asked(&mut ed, ask(&dir.join("front"), run, HOST));
        settled(&mut ed);
        key(&mut ed, 'a');
        assert_eq!(hosts_cards(&ed.offers).len(), 1, "{k}: `a` answers nothing");
        key(&mut ed, k);
        assert!(hosts_cards(&ed.offers).is_empty(), "{k}: answered");
        let told: Vec<_> = sent().iter().filter_map(answered).collect();
        assert_eq!(told, [(session, run, HOST.to_string(), 443, scope)], "{k}");
        let listed = ed.session.hosts.contains(&(Kind::Host, HOST.to_string(), 443));
        // (`s` lists it; `A` keeps it for the project instead, the `s` before still listed; `n` takes it off)
        assert_eq!(listed, matches!(scope, Scope::Session | Scope::Project), "{k}: the session's list");
    }
    let tell = ed.session.checkpoints.tell.join("\n");
    assert!(tell.contains(&format!("reach {HOST}:443 — for this session")), "{tell}");
    assert!(tell.contains(&format!("reach {HOST}:443 — in this project")), "{tell}");
    assert!(tell.contains(&format!("refused your commands to reach {HOST}:443 for this session")), "{tell}");
    // `A`'s keeping, on a worker: the project's hosts, read back as the next run's policy reads them
    let path = ed.allow_path.clone().unwrap();
    let end = Instant::now() + std::time::Duration::from_secs(5);
    let kept = loop {
        let k = crate::allow::Store::read(Some(&path)).get(&ed.session.root).hosts;
        if !k.is_empty() || Instant::now() > end {
            break k;
        }
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
            ed.handle_event(ev);
        }
    };
    assert_eq!(kept, [(HOST.to_string(), 443)]);
    std::fs::remove_dir_all(&dir).ok();
}

/// A peer session's key never answers it: with the card's session behind, its keys are the front's — a `y`
/// there tells the proxy nothing. Brought to the front, the card answers for its own session and run.
#[test]
fn a_peer_sessions_key_never_answers_it() {
    let (mut ed, dir, back, _, back_run) = two("peer");
    let _ = sent();
    asked(&mut ed, ask(&dir.join("back"), back_run, HOST));
    assert!(ed.offers.is_empty(), "nothing in front");
    for k in ['y', 's', 'A', 'n'] {
        key(&mut ed, k);
    }
    assert!(sent().iter().all(|m| answered(m).is_none()), "the front's keys answered nothing");
    assert!(ed.session_switch(back));
    settled(&mut ed);
    key(&mut ed, 'y');
    let told: Vec<_> = sent().iter().filter_map(answered).collect();
    assert_eq!(told, [(session_of(&dir.join("back")), back_run, HOST.to_string(), 443, Scope::Run)]);
    // Its session ended: the proxy drops that session's answers
    assert!(ed.session_switch(0));
    let _ = sent();
    ed.session_close(&dir.join("back")).expect("closed");
    let ended = sent()
        .into_iter()
        .any(|m| matches!(m, ToProxy::SessionEnded { session } if session == session_of(&dir.join("back"))));
    assert!(ended, "SessionEnded sent");
    std::fs::remove_dir_all(&dir).ok();
}

/// A host granted from what a run was refused (`:sandbox-blocked`) reaches as far as its key: `y` lifts the
/// session's refusal (the run going again carries the host in its rules — asked again it would wait, not go), `s`
/// lets it go for the session (listed), `A` for the project — all for the session in front's project.
#[test]
fn a_grant_from_a_refused_run_reaches_as_far_as_its_key() {
    let (mut ed, dir, ..) = two("grant");
    let session = session_of(&dir.join("front"));
    let _ = sent();
    granted(&mut ed, HOST, 443, crate::allow::Scope::Once);
    let lifted = sent();
    assert!(
        matches!(&lifted[..], [ToProxy::Lifted { kind: Kind::Host, session: s, host, port: 443 }] if *s == session && host == HOST),
        "lifted, not taken"
    );
    granted(&mut ed, HOST, 443, crate::allow::Scope::Session);
    granted(&mut ed, HOST, 443, crate::allow::Scope::Always);
    let told: Vec<_> = sent().iter().filter_map(answered).map(|t| (t.0, t.4)).collect();
    assert_eq!(told, [(session, Scope::Session), (session, Scope::Project)]);
    assert_eq!(ed.session.hosts, [(Kind::Host, HOST.to_string(), 443)]);
    std::fs::remove_dir_all(&dir).ok();
}

/// A project opened through a link (its root as given, not resolved): the main loop names its session as the
/// proxy does — by the real path a run's policy holds (`proxy::session_of`, one way for both) — so a take-back
/// reaches the session the runs' connections are of, a card for a process left running lands in that session,
/// and an `A` is kept under the path the next run reads it by.
#[test]
fn a_project_opened_through_a_link_is_one_session_to_both() {
    let (mut ed, dir, ..) = two("link");
    let link = dir.join("link");
    std::os::unix::fs::symlink(dir.join("front"), &link).unwrap();
    ed.session.root = link.clone();
    // (the policy's root as `env_in` makes it: the real path)
    let real = crate::sandbox::profile_path(&link);
    assert_ne!(real, link, "a root through a link");
    let theirs = Policy::for_project(&Env { root: real.clone(), ..Default::default() }).session;
    assert_eq!(ed.session.proxy_id(), theirs, "one session to both");
    let _ = sent();
    ed.sandbox_disallow(&format!("host {HOST}:443")).unwrap();
    let taken = sent()
        .into_iter()
        .any(|m| matches!(m, ToProxy::Taken { kind: Kind::Host, session, .. } if session == theirs));
    assert!(taken, "the take-back reaches the proxy's session");
    let left = FromProxy::Ask {
        session: theirs,
        run: 999_997,
        kind: Kind::Host,
        host: HOST.into(),
        port: 443,
        process: "java".into(),
        asked: Vec::new(),
    };
    asked(&mut ed, left);
    assert!(
        hosts_cards(&ed.offers).iter().any(|a| a.left && a.session == theirs),
        "the card in that session"
    );
    // `A`, kept where the next run reads it (`tools::guard_then_go`: the real path)
    granted(&mut ed, HOST, 443, crate::allow::Scope::Always);
    let path = ed.allow_path.clone().unwrap();
    let end = Instant::now() + std::time::Duration::from_secs(5);
    while crate::allow::Store::read(Some(&path)).get(&real).hosts.is_empty() {
        assert!(Instant::now() < end, "kept under the real path");
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
            ed.handle_event(ev);
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A host a built-in pack names among its registries is said so on its card — words only (Q6): the card asks
/// the same, its answers and the capability model's decision are those of a host no pack names, whose card says
/// nothing more. (`static.crates.io`: the rust pack's `*.crates.io`.) Only at the registry's port — 443 where the
/// pack names none: the same name on another port (`:22`) is no registry, and the card mustn't make that
/// connection look trusted while the user decides.
#[test]
fn a_registry_is_named_on_the_hosts_card() {
    let card_at = |host: &str, port: u16| {
        card(Ask {
            session: 1,
            run: 2,
            kind: Kind::Host,
            host: host.into(),
            port,
            asked: Vec::new(),
            process: "cargo".into(),
            label: "cargo build".into(),
            users: false,
            left: false,
        })
    };
    let (named, plain) = (card_at("static.crates.io", 443), card_at(HOST, 443));
    assert!(
        named.question.contains("\nstatic.crates.io is the rust pack's registry\n"),
        "{}",
        named.question
    );
    assert!(!plain.question.contains("registry"), "{}", plain.question);
    let odd = card_at("static.crates.io", 22);
    assert!(!odd.question.contains("registry"), "another port is no registry: {}", odd.question);
    // (a registry a pack lists at a port of its own: that port, and only it)
    let own =
        [("x".to_string(), crate::sandbox::proxy::egress::Pattern::parse("r.example.invalid:8443").unwrap())];
    assert_eq!(registry_of(&own, "r.example.invalid", 8443).as_deref(), Some("x"));
    assert_eq!(registry_of(&own, "r.example.invalid", 443), None);
    let decided = |o: &Offer| {
        let a = o.asked.clone().unwrap();
        (a.why, crate::cards::decide(&a.request, &Default::default()), o.answers(false))
    };
    assert_eq!(decided(&named), decided(&plain));
}

/// The proxy asking about `kind` for run `run` of the project at `root`: `host:port`, a lookup's questions.
fn ask_of(root: &Path, run: u64, kind: Kind, host: &str, port: u16, asked: &[&str]) -> FromProxy {
    let asked = asked.iter().map(|s| s.to_string()).collect();
    FromProxy::Ask {
        session: session_of(root),
        run,
        kind,
        host: host.into(),
        port,
        process: "psql".into(),
        asked,
    }
}

fn told_of(m: &ToProxy) -> Option<(Kind, String, u16, Scope)> {
    match m {
        ToProxy::Answer { kind, host, port, scope, .. } => Some((*kind, host.clone(), *port, *scope)),
        _ => None,
    }
}

/// 6e+ (F4, F7): raw TCP to a host:port is its own card (`host-raw`), beside the host card for the same host:port —
/// its warning that tarae can't see what passes in the warning colour; `y` this run, `s` this session, `n` — no `A`
/// (a lasting allow is the user's own line in `llm.sandbox-hosts`): `A` answers nothing, and says so. `s` is listed
/// as raw, Claude told; `:sandbox-disallow raw host:port` takes it back, as raw.
#[test]
fn raw_tcp_is_its_own_card_without_always() {
    let (mut ed, dir, _, run, _) = two("raw");
    let front = dir.join("front");
    asked(&mut ed, ask_of(&front, run, Kind::Host, HOST, 5432, &[]));
    asked(&mut ed, ask_of(&front, run, Kind::Raw, HOST, 5432, &[]));
    assert_eq!(hosts_cards(&ed.offers).len(), 2, "kept apart from the host card");
    let raw = ed.offers.iter().position(|o| o.kind() == "host-raw").expect("its kind");
    ed.offers.swap(0, raw);
    let o = &ed.offers[0];
    assert_eq!(o.title, format!("Let a command open a raw connection to {HOST}?"));
    let warning = o.question.lines().nth(2).unwrap();
    assert!(warning.starts_with("Not TLS or HTTP: tarae can't see what passes"), "{}", o.question);
    assert_eq!(o.warning, Some(2..3), "in the warning colour");
    let keys: Vec<char> = o.answers(false).iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, ['y', 's', 'n']);
    let _ = sent();
    settled(&mut ed);
    key(&mut ed, 'A');
    assert_eq!(hosts_cards(&ed.offers).len(), 2, "`A` answers nothing");
    assert!(sent().iter().all(|m| told_of(m).is_none()));
    key(&mut ed, 's');
    let told: Vec<_> = sent().iter().filter_map(told_of).collect();
    assert_eq!(told, [(Kind::Raw, HOST.to_string(), 5432, Scope::Session)]);
    assert_eq!(ed.session.hosts, [(Kind::Raw, HOST.to_string(), 5432)]);
    let tell = ed.session.checkpoints.tell.join("\n");
    assert!(tell.contains(&format!("open raw connections to {HOST}:5432 — for this session")), "{tell}");
    assert_eq!(hosts_cards(&ed.offers)[0].kind, Kind::Host, "the host card still up");
    ed.sandbox_disallow(&format!("raw {HOST}:5432")).unwrap();
    assert!(ed.session.hosts.is_empty());
    let taken = sent();
    assert!(matches!(&taken[..], [ToProxy::Taken { kind: Kind::Raw, host, port: 5432, .. }] if host == HOST));
    std::fs::remove_dir_all(&dir).ok();
}

/// 6e+ (F2, F7): a name's records are one card for the name (`lookup`) whatever the types asked — the proxy asking
/// again with the next question grows its list, said anew; `y` `s` `n`, no `A`. `n` refuses them for the session,
/// Claude told; the card goes when its run ends, as the others do.
#[test]
fn a_lookup_is_one_card_for_its_name() {
    let (mut ed, dir, _, run, _) = two("lookup");
    let front = dir.join("front");
    let srv = "SRV _mongodb._tcp.cluster.example.invalid";
    let txt = "TXT cluster.example.invalid";
    asked(&mut ed, ask_of(&front, run, Kind::Lookup, "cluster.example.invalid", 0, &[srv]));
    let rev = ed.offers[0].rev;
    asked(&mut ed, ask_of(&front, run, Kind::Lookup, "cluster.example.invalid", 0, &[srv, txt]));
    assert_eq!(hosts_cards(&ed.offers).len(), 1, "one card for the name");
    let o = &ed.offers[0];
    assert_eq!((o.kind(), o.rev > rev), ("lookup", true));
    assert_eq!(o.title, "Let a command look up the service records of cluster.example.invalid?");
    assert!(
        o.question.starts_with(&format!("{srv}, {txt}\nasked by psql in `cargo build`")),
        "{}",
        o.question
    );
    assert!(o.question.lines().nth(2).unwrap().starts_with("tarae asks the network's DNS for them"));
    let a = o.asked.clone().unwrap();
    let types = vec!["SRV".to_string(), "TXT".to_string()];
    let want = crate::cards::Request::Lookup { name: "cluster.example.invalid".into(), types };
    assert_eq!((a.why, a.request), (crate::cards::Why::Lookup, want));
    let _ = sent();
    settled(&mut ed);
    key(&mut ed, 'n');
    let told: Vec<_> = sent().iter().filter_map(told_of).collect();
    assert_eq!(told, [(Kind::Lookup, "cluster.example.invalid".to_string(), 0, Scope::Refused)]);
    let tell = ed.session.checkpoints.tell.join("\n");
    assert!(
        tell.contains("refused your commands to look up cluster.example.invalid's service records"),
        "{tell}"
    );
    asked(
        &mut ed,
        ask_of(&front, run, Kind::Lookup, "other.example.invalid", 0, &["TXT other.example.invalid"]),
    );
    let up: Vec<(u64, &str)> = hosts_cards(&ed.offers).iter().map(|a| (a.run, a.host.as_str())).collect();
    assert_eq!(up, [(run, "other.example.invalid")], "its card up before its run ends");
    run_ended(&mut ed, Some(run));
    assert!(hosts_cards(&ed.offers).is_empty(), "gone with its run");
    std::fs::remove_dir_all(&dir).ok();
}

/// Its run reaped, the card for it goes, and the proxy is told (`RunEnded`): whoever else still waits on that
/// host — another run's connection, a process it left — is asked about again, so a card for that one comes
/// (`proxy::tests::a_run_ended_hands_its_question_to_whoever_still_waits`). Another run's card stays.
#[test]
fn the_card_goes_when_its_run_ends() {
    let (mut ed, dir, _, run, _) = two("ends");
    asked(&mut ed, ask(&dir.join("front"), run, HOST));
    let other = running(&mut ed.session, "cargo test");
    asked(&mut ed, ask(&dir.join("front"), other, "other.example.invalid"));
    assert_eq!(hosts_cards(&ed.offers).len(), 2);
    let _ = sent();
    run_ended(&mut ed, Some(run));
    let left: Vec<u64> = hosts_cards(&ed.offers).iter().map(|a| a.run).collect();
    assert_eq!(left, [other]);
    assert!(matches!(&sent()[..], [ToProxy::RunEnded { run: r }] if *r == run), "the proxy told");
    std::fs::remove_dir_all(&dir).ok();
}
