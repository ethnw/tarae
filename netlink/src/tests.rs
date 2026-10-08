//! What the crate builds, held against what nft and ip sent the kernel for the same thing; what it reads, against
//! what the kernel answered them (`fixture.rs`). Every test here runs on every system: bytes only.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::answer::{Owed, Reading};
use crate::fixture::{without_acks, without_seqs};
use crate::nft::{self, Base, ChainKind, Expr, Hook, MetaKey, Reg, SocketKey, Transaction, Verdict};
use crate::route::{self, Changes, RouteKind};
use crate::{Batch, Error, Family, fixture, reply};

const TCP: u8 = 6;
const UDP: u8 = 17;
const SESSION_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
const SESSION_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

fn assert_bytes(ours: &[u8], theirs: &[u8], what: &str) {
    if ours != theirs {
        let at = ours.iter().zip(theirs).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()));
        panic!(
            "{what}: the bytes differ from byte {at} (ours {} long, theirs {})\n ours:   {:02x?}\n theirs: {:02x?}",
            ours.len(),
            theirs.len(),
            &ours[at.saturating_sub(8)..(at + 24).min(ours.len())],
            &theirs[at.saturating_sub(8)..(at + 24).min(theirs.len())],
        );
    }
}

// ── the expressions, as the session's ruleset uses them ─────────────────────────

fn nfproto(f: Family) -> Vec<Expr> {
    vec![
        Expr::Meta { key: MetaKey::NfProto, dreg: Reg::R1 },
        Expr::Cmp { sreg: Reg::R1, data: vec![f.number()] },
    ]
}

fn l4proto(p: u8) -> Vec<Expr> {
    vec![Expr::Meta { key: MetaKey::L4Proto, dreg: Reg::R1 }, Expr::Cmp { sreg: Reg::R1, data: vec![p] }]
}

fn udp_dport(p: u16) -> Vec<Expr> {
    let mut e = l4proto(UDP);
    e.extend([
        Expr::Payload { base: Base::Transport, offset: 2, len: 2, dreg: Reg::R1 },
        Expr::Cmp { sreg: Reg::R1, data: nft::port(p) },
    ]);
    e
}

/// The destination's first `bytes` bytes are `a`'s (nft writes a whole-byte prefix as a shorter load).
fn daddr(a: IpAddr, bytes: u32) -> Vec<Expr> {
    let (family, offset) = match a {
        IpAddr::V4(_) => (Family::V4, 16),
        IpAddr::V6(_) => (Family::V6, 24),
    };
    let mut e = nfproto(family);
    e.extend([
        Expr::Payload { base: Base::Network, offset, len: bytes, dreg: Reg::R1 },
        Expr::Cmp { sreg: Reg::R1, data: nft::address(a)[..bytes as usize].to_vec() },
    ]);
    e
}

fn set_mark() -> Vec<Expr> {
    vec![
        Expr::Immediate { dreg: Reg::R1, data: nft::mark(1) },
        Expr::MetaSet { key: MetaKey::Mark, sreg: Reg::R1 },
    ]
}

fn transparent(is: u8) -> Vec<Expr> {
    vec![
        Expr::Socket { key: SocketKey::Transparent, dreg: Reg::R1 },
        Expr::Cmp { sreg: Reg::R1, data: vec![is] },
    ]
}

fn tproxy(to: IpAddr, p: u16) -> Vec<Expr> {
    let family = if to.is_ipv4() { Family::V4 } else { Family::V6 };
    vec![
        Expr::Immediate { dreg: Reg::R1, data: nft::address(to) },
        Expr::Immediate { dreg: Reg::R2, data: nft::port(p) },
        Expr::Tproxy { family, addr: Reg::R1, port: Reg::R2 },
    ]
}

fn then(mut e: Vec<Expr>, more: impl IntoIterator<Item = Expr>) -> Vec<Expr> {
    e.extend(more);
    e
}

fn verdict(v: Verdict) -> Expr {
    Expr::Verdict(v)
}

/// A transaction of one rule in `table`'s `chain`.
fn one_rule(table: &str, chain: &str, exprs: &[Expr]) -> Batch {
    let mut t = Transaction::new();
    t.rule(table, chain, exprs);
    t.end()
}

/// The session's ruleset as M1 wrote it for nft (`fixtures/capture/r6b-inner.sh`'s `split.nft`, as `create.nft`):
/// t1's, its sets split into one rule per element, the final drop.
pub(crate) fn session_ruleset(table: &str) -> Batch {
    let mut t = Transaction::new();
    t.table(table, true);
    t.chain(table, "out", Hook::Output, -150, ChainKind::Route);
    t.chain(table, "pre", Hook::Prerouting, -150, ChainKind::Filter);
    let lo4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let lo6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let rv = verdict(Verdict::Return);
    let out: Vec<Vec<Expr>> = vec![
        then(then(udp_dport(53), set_mark()), [rv.clone()]),
        then(daddr(lo4, 1), [rv.clone()]),
        then(daddr(SESSION_V4, 4), [rv.clone()]),
        then(daddr(lo6, 16), [rv.clone()]),
        then(daddr(SESSION_V6, 16), [rv.clone()]),
        then(l4proto(TCP), set_mark()),
        then(l4proto(UDP), set_mark()),
    ];
    let accept = verdict(Verdict::Accept);
    let to = |f: Family, more: Vec<Expr>, at: IpAddr, p: u16| {
        then(then(then(then(nfproto(f), more), tproxy(at, p)), set_mark()), [accept.clone()])
    };
    let pre: Vec<Vec<Expr>> = vec![
        then(then(then(l4proto(TCP), transparent(1)), set_mark()), [accept.clone()]),
        then(then(then(l4proto(UDP), transparent(1)), set_mark()), [accept.clone()]),
        to(Family::V4, udp_dport(53), SESSION_V4, 15053),
        to(Family::V6, udp_dport(53), SESSION_V6, 15053),
        then(daddr(lo4, 1), [rv.clone()]),
        then(daddr(SESSION_V4, 4), [rv.clone()]),
        then(daddr(lo6, 16), [rv.clone()]),
        then(daddr(SESSION_V6, 16), [rv.clone()]),
        to(Family::V4, l4proto(TCP), SESSION_V4, 15001),
        to(Family::V6, l4proto(TCP), SESSION_V6, 15001),
        to(Family::V4, l4proto(UDP), SESSION_V4, 15053),
        to(Family::V6, l4proto(UDP), SESSION_V6, 15053),
        vec![verdict(Verdict::Drop)],
    ];
    for r in &out {
        t.rule(table, "out", r);
    }
    for r in &pre {
        t.rule(table, "pre", r);
    }
    t.end()
}

/// The routes TPROXY needs: everything leaving by `lo`, and marked packets taken as this machine's own.
pub(crate) fn session_routes(family: Family) -> Batch {
    let mut c = Changes::new();
    c.default_route(family, route::MAIN, RouteKind::Unicast, 1);
    c.fwmark_rule(family, 1, None, 100);
    c.default_route(family, 100, RouteKind::Local, 1);
    c.end()
}

// ── nf_tables: byte for byte ─────────────────────────────────────────────

/// Each change the crate writes is the bytes nft wrote for the same change — its acks aside — so what the kernel
/// does with nft's, it does with tarae's: every r6/r6b case written without sets.
#[test]
fn each_message_is_byte_for_byte_what_nft_sent() {
    let lo4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let lo6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let rv = verdict(Verdict::Return);
    let accept = verdict(Verdict::Accept);
    let tproxied = |f: Family, more: Vec<Expr>, at: IpAddr, p: u16| {
        then(then(then(then(nfproto(f), more), tproxy(at, p)), set_mark()), [accept.clone()])
    };
    let mut table = Transaction::new();
    table.table("tarae", false);
    let mut route_chain = Transaction::new();
    route_chain.chain("tarae", "out", Hook::Output, -150, ChainKind::Route);
    let mut filter_chain = Transaction::new();
    filter_chain.chain("tarae", "pre", Hook::Prerouting, -150, ChainKind::Filter);
    let cases: Vec<(&str, Batch)> = vec![
        // (the table and chains as nft's `create`: `EXCL` — one already there refused, not adopted)
        ("create-table", table.end()),
        ("create-chain-route", route_chain.end()),
        ("create-chain-prerouting", filter_chain.end()),
        ("rule-out-dns", one_rule("rd", "out", &then(then(udp_dport(53), set_mark()), [rv.clone()]))),
        ("rule-pre-dns4", one_rule("rd", "pre", &tproxied(Family::V4, udp_dport(53), SESSION_V4, 15053))),
        ("rule-pre-dns6", one_rule("rd", "pre", &tproxied(Family::V6, udp_dport(53), SESSION_V6, 15053))),
        ("rule-pre-tcp4", one_rule("rd", "pre", &tproxied(Family::V4, l4proto(TCP), SESSION_V4, 15001))),
        ("rule-pre-tcp6", one_rule("rd", "pre", &tproxied(Family::V6, l4proto(TCP), SESSION_V6, 15001))),
        ("rule-pre-udp4", one_rule("rd", "pre", &tproxied(Family::V4, l4proto(UDP), SESSION_V4, 15053))),
        ("rule-pre-udp6", one_rule("rd", "pre", &tproxied(Family::V6, l4proto(UDP), SESSION_V6, 15053))),
        ("split-out-lo4", one_rule("tarae", "out", &then(daddr(lo4, 1), [rv.clone()]))),
        ("split-out-session4", one_rule("tarae", "out", &then(daddr(SESSION_V4, 4), [rv.clone()]))),
        ("split-out-lo6", one_rule("tarae", "out", &then(daddr(lo6, 16), [rv.clone()]))),
        ("split-out-session6", one_rule("tarae", "out", &then(daddr(SESSION_V6, 16), [rv.clone()]))),
        ("split-out-tcp", one_rule("tarae", "out", &then(l4proto(TCP), set_mark()))),
        ("split-out-udp", one_rule("tarae", "out", &then(l4proto(UDP), set_mark()))),
        ("split-out-dns", one_rule("tarae", "out", &then(then(udp_dport(53), set_mark()), [rv.clone()]))),
        (
            "split-pre-socket-tcp",
            one_rule(
                "tarae",
                "pre",
                &then(then(then(l4proto(TCP), transparent(1)), set_mark()), [accept.clone()]),
            ),
        ),
        (
            "split-pre-socket-udp",
            one_rule(
                "tarae",
                "pre",
                &then(then(then(l4proto(UDP), transparent(1)), set_mark()), [accept.clone()]),
            ),
        ),
        ("split-pre-lo4", one_rule("tarae", "pre", &then(daddr(lo4, 1), [rv.clone()]))),
        ("split-pre-session4", one_rule("tarae", "pre", &then(daddr(SESSION_V4, 4), [rv.clone()]))),
        ("split-pre-lo6", one_rule("tarae", "pre", &then(daddr(lo6, 16), [rv.clone()]))),
        ("split-pre-session6", one_rule("tarae", "pre", &then(daddr(SESSION_V6, 16), [rv.clone()]))),
        ("pre-drop", one_rule("tarae", "pre", &[verdict(Verdict::Drop)])),
        (
            "pre-socket-plain",
            one_rule("tarae", "pre", &then(then(l4proto(TCP), transparent(0)), [accept.clone()])),
        ),
    ];
    for (case, ours) in cases {
        assert_bytes(&without_acks(ours.bytes()), &fixture::last_sent(case), case);
    }
}

/// The session's whole ruleset is, message for message, the batch nft sent for M1's `split.nft` written as `create`s
/// and `add rule`s (`create.nft`) — one transaction, the table owned, nothing already there adopted.
#[test]
fn the_session_ruleset_is_what_nft_sent_for_it() {
    assert_bytes(
        &without_acks(session_ruleset("tarae").bytes()),
        &fixture::last_sent("create-batch"),
        "create-batch",
    );
}

/// An owned table says so (`NFT_TABLE_F_OWNER`) and a plain one doesn't — the flag that makes the session's rules
/// tarae's socket's alone, and gone with it.
#[test]
fn an_owned_table_says_so() {
    let built = |owned| {
        let mut t = Transaction::new();
        t.table("tarae", owned);
        without_acks(t.end().bytes())
    };
    assert_bytes(&built(true), &fixture::last_sent("create-table-owner"), "create-table-owner");
    assert_bytes(&built(false), &fixture::last_sent("create-table"), "create-table");
}

/// A chain already there is refused, not updated: the kernel's EEXIST for nft's `create chain` names the chain's name
/// (`NFTA_CHAIN_NAME`, 3) in message 1 — read against the crate's own chain message, which is nft's.
#[test]
fn a_chain_already_there_is_refused_by_its_name() {
    let mut t = Transaction::new();
    t.chain("tarae", "out", Hook::Output, -150, ChainKind::Route);
    let sent = t.end();
    let mut owed = Owed::new(&sent, 0);
    owed.take(&fixture::received("err-chain-exists").pop().unwrap()).unwrap();
    match owed.outcome() {
        Err(Error::Kernel { errno, index, attribute, .. }) => {
            assert_eq!((errno, index, attribute), (libc::EEXIST, Some(1), Some(3)))
        }
        other => panic!("{other:?}"),
    }
}

// ── rtnetlink: byte for byte ─────────────────────────────────────────────

/// Each route and policy rule the crate writes is the bytes ip wrote for it — the sequence number aside (ip numbers
/// by the clock): ip asks the ack and refuses to replace (`EXCL`) as tarae does.
#[test]
fn routes_and_rules_are_what_ip_sent() {
    let one = |f: &dyn Fn(&mut Changes)| {
        let mut c = Changes::new();
        f(&mut c);
        c.end()
    };
    let cases: Vec<(&str, Batch)> = vec![
        ("route-default4", one(&|c| c.default_route(Family::V4, route::MAIN, RouteKind::Unicast, 1))),
        ("route-default6", one(&|c| c.default_route(Family::V6, route::MAIN, RouteKind::Unicast, 1))),
        ("route-local4", one(&|c| c.default_route(Family::V4, 100, RouteKind::Local, 1))),
        ("route-local6", one(&|c| c.default_route(Family::V6, 100, RouteKind::Local, 1))),
        ("rule-fwmark4", one(&|c| c.fwmark_rule(Family::V4, 1, None, 100))),
        ("rule-fwmark6", one(&|c| c.fwmark_rule(Family::V6, 1, None, 100))),
    ];
    for (case, ours) in cases {
        assert_bytes(&without_seqs(ours.bytes()), &without_seqs(&fixture::last_sent(case)), case);
    }
}

/// A mark under a mask carries the mask as `FRA_FWMASK`, after the mark — not as a second mark.
#[test]
fn a_rule_with_a_mask_carries_fwmask() {
    let mut c = Changes::new();
    c.fwmark_rule(Family::V4, 1, Some(0xff), 100);
    let ours = without_seqs(c.end().bytes());
    assert_bytes(&ours, &without_seqs(&fixture::last_sent("rule-fwmark4-mask")), "rule-fwmark4-mask");
}

// ── lengths a header can say ─────────────────────────────────────────────

/// The longest attribute is written whole: 65531 bytes of value, 65535 with its header — the length says exactly that.
#[test]
fn the_longest_attribute_says_its_length() {
    let mut b = Vec::new();
    crate::attr::put(&mut b, 1, &[7; 65531]);
    assert_eq!(u16::from_ne_bytes([b[0], b[1]]), u16::MAX);
    assert_eq!(b.len(), 65536, "and padded to four bytes");
}

/// A value one byte past what an attribute's header can say stops the build — never a wrapped, short length the
/// kernel would read as another layout.
#[test]
#[should_panic(expected = "past the 65535 its header can say")]
fn a_value_too_long_for_its_header_stops_the_build() {
    let mut t = Transaction::new();
    t.rule("tarae", "out", &[Expr::Cmp { sreg: Reg::R1, data: vec![0; 65532] }]);
}

/// So does a nest whose contents are too long together, each of them fitting.
#[test]
#[should_panic(expected = "past the 65535 its header can say")]
fn a_nest_too_long_for_its_header_stops_the_build() {
    let half = Expr::Immediate { dreg: Reg::R1, data: vec![0; 40_000] };
    let mut t = Transaction::new();
    t.rule("tarae", "out", &[half.clone(), half]);
}

// ── what the kernel answers ─────────────────────────────────────────────

/// A refusal names the message it refused, by its sequence number, and the attribute in it the kernel pointed at:
/// the extended ack's offset is into that message, not into the batch (`create table` that exists — its name;
/// a rule for a chain that isn't there — its chain).
#[test]
fn a_refusal_names_the_message_and_attribute_refused() {
    // (r6b's `create table inet tarae`: tarae's own table message)
    let mut t = Transaction::new();
    t.table("tarae", false);
    let created = t.end();
    let mut owed = Owed::new(&created, 0);
    for d in fixture::received("err-exists-again").iter().rev().take(1) {
        owed.take(d).unwrap();
    }
    match owed.outcome() {
        Err(Error::Kernel { errno, index, attribute, .. }) => {
            assert_eq!(
                (errno, index, attribute),
                (libc::EEXIST, Some(1), Some(1)),
                "the table's name, in message 1"
            )
        }
        other => panic!("{other:?}"),
    }
    let no_chain = one_rule("tarae", "nosuch", &[verdict(Verdict::Accept)]);
    let mut owed = Owed::new(&no_chain, 0);
    owed.take(fixture::received("err-no-chain").iter().find(|d| d.len() == 124).unwrap()).unwrap();
    match owed.outcome() {
        Err(Error::Kernel { errno, index, attribute, .. }) => {
            assert_eq!(
                (errno, index, attribute),
                (libc::ENOENT, Some(1), Some(2)),
                "the rule's chain, in message 1"
            )
        }
        other => panic!("{other:?}"),
    }
}

/// A refusal carries the kernel's own words where it gave them (`NLMSGERR_ATTR_MSG`).
#[test]
fn a_refusal_says_the_kernels_words() {
    let mut c = Changes::new();
    c.default_route(Family::V4, route::MAIN, RouteKind::Unicast, 1);
    let sent = c.end();
    let reply = fixture::received("err-route-prefix").pop().unwrap();
    // (ip numbered its request by the clock: the reply's number is the one to wait for)
    let seq = u32::from_ne_bytes(reply[8..12].try_into().unwrap());
    let mut owed = Owed::new(&sent, seq);
    owed.take(&reply).unwrap();
    match owed.outcome() {
        Err(Error::Kernel { errno, said, .. }) => {
            assert_eq!(errno, libc::EINVAL);
            assert_eq!(said.as_deref(), Some("Invalid prefix for given prefix length"));
        }
        other => panic!("{other:?}"),
    }
}

/// A batch is answered only when every message that asked has its ack: one ack short, it still waits.
#[test]
fn every_asked_message_is_answered_before_a_change_is_done() {
    let batch = session_routes(Family::V4);
    let acks: Vec<Vec<u8>> = (0..batch.len() as u32).map(ack_for).collect();
    let mut owed = Owed::new(&batch, 0);
    for a in &acks[..acks.len() - 1] {
        owed.take(a).unwrap();
    }
    assert!(!owed.answered(), "an ack short");
    owed.take(&acks[acks.len() - 1]).unwrap();
    assert!(owed.answered());
    assert!(owed.outcome().is_ok());
}

/// A sender without CAP_NET_ADMIN gets one EPERM for a transaction's first message and nothing for the rest: the
/// change is refused while every acked message still waits — the state in which the socket stops reading
/// (`live::capless`, which also breaks if the socket waits on).
#[test]
fn a_transaction_refused_at_its_first_message_is_refused_unanswered() {
    let batch = session_ruleset("tarae");
    let mut owed = Owed::new(&batch, 0);
    // (the kernel's EPERM for nft's first message, numbered 0: `nfnetlink_rcv` refuses a capless sender before
    // reading which message it is)
    let eperm = fixture::received("err-eperm").remove(0);
    owed.take(&eperm).unwrap();
    assert!(owed.refused() && !owed.answered(), "refused, the rules' acks still owed");
    match owed.outcome() {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::EPERM, Some(0))),
        other => panic!("{other:?}"),
    }
}

/// rtnetlink answers each route change on its own: a refusal of the first doesn't end the others, which are still
/// waited for and may land — the outcome names the one refused (`route::Changes` says the change is partial).
#[test]
fn a_refused_route_change_still_waits_for_the_others() {
    let batch = session_routes(Family::V4);
    // (ip's `route add default dev lo` refused, EEXIST: the number it carries is the one to send from)
    let refusal = fixture::received("err-route-exists").pop().unwrap();
    let first = u32::from_ne_bytes(refusal[8..12].try_into().unwrap());
    let mut owed = Owed::new(&batch, first);
    owed.take(&refusal).unwrap();
    assert!(owed.refused() && !owed.answered(), "the rule's and the local route's answers still owed");
    owed.take(&ack_for(first.wrapping_add(1))).unwrap();
    owed.take(&ack_for(first.wrapping_add(2))).unwrap();
    assert!(owed.answered());
    match owed.outcome() {
        Err(Error::Kernel { errno, index, .. }) => assert_eq!((errno, index), (libc::EEXIST, Some(0))),
        other => panic!("{other:?}"),
    }
}

/// An ack (an error message of number 0, the request's header echoed — capped) for message `seq`.
fn ack_for(seq: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(36u32.to_ne_bytes());
    b.extend(2u16.to_ne_bytes());
    b.extend(0x100u16.to_ne_bytes());
    b.extend(seq.to_ne_bytes());
    b.extend(0u32.to_ne_bytes());
    b.extend(0i32.to_ne_bytes());
    b.extend([16, 0, 0, 0, 0, 0, 0, 0]);
    b.extend(seq.to_ne_bytes());
    b.extend(0u32.to_ne_bytes());
    b
}

/// The reads give what the kernel listed: the generation, the tables, the rules (r6b's dumps after the whole
/// ruleset), the routes and the policy rules.
#[test]
fn the_reads_give_what_the_kernel_listed() {
    let data = |case: &str| -> Vec<(u16, Vec<u8>)> {
        fixture::received(case)
            .iter()
            .flat_map(|d| reply::replies(d).unwrap())
            .filter_map(|r| match r {
                reply::Reply::Data { kind, body, .. } => Some((kind, body.to_vec())),
                _ => None,
            })
            .collect()
    };
    let rules = data("dump-rules");
    let generation = rules.iter().find_map(|(k, b)| nft::generation_of(*k, b).ok());
    assert!(generation.is_some(), "the generation nft asked first");
    let tables: Vec<String> = rules.iter().filter_map(|(k, b)| nft::table_of(*k, b).ok()).collect();
    assert_eq!(tables, ["tarae"]);
    let seen: Vec<nft::RuleSeen> = rules.iter().filter_map(|(k, b)| nft::rule_of(*k, b).ok()).collect();
    let count = |chain: &str| seen.iter().filter(|r| r.table == "tarae" && r.chain == chain).count();
    assert_eq!((count("out"), count("pre")), (7, 13));

    let routes: Vec<route::RouteSeen> =
        data("dump-routes4").iter().filter_map(|(k, b)| route::route_of(*k, b).ok()).collect();
    let default = route::RouteSeen {
        table: route::MAIN.into(),
        kind: route::UNICAST,
        scope: route::SCOPE_LINK,
        dst: None,
        dst_len: 0,
        oif: Some(1),
        priority: None,
    };
    assert!(routes.contains(&default), "{routes:#?}");
    let local = route::RouteSeen { table: 100, kind: route::LOCAL, scope: route::SCOPE_HOST, ..default };
    assert!(routes.contains(&local), "{routes:#?}");

    let rules: Vec<route::RuleSeen> =
        data("dump-rules6").iter().filter_map(|(k, b)| route::rule_of(*k, b).ok()).collect();
    assert!(rules.iter().any(|r| r.table == 100 && r.fwmark == Some(1)), "{rules:#?}");
}

/// A read is answered by its dump's end, or a single request by its ack — not by another request's.
#[test]
fn a_read_ends_with_its_own_answer() {
    let mut reading = Reading::new(7, false);
    reading.take(&ack_for(6)).unwrap();
    assert!(!reading.ended(), "another request's ack");
    reading.take(&ack_for(7)).unwrap();
    assert!(reading.ended());
}

/// A dump's end (`NLMSG_DONE`, multi) for request `seq`, and what the kernel puts after its header: minus `why` it
/// stopped, 0 when it ended whole.
fn done_for(seq: u32, why: i32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(20u32.to_ne_bytes());
    b.extend(3u16.to_ne_bytes());
    b.extend(2u16.to_ne_bytes());
    b.extend(seq.to_ne_bytes());
    b.extend(0u32.to_ne_bytes());
    b.extend((-why).to_ne_bytes());
    b
}

/// A dump is answered by its own end — the kernel's `NLMSG_DONE`, as ip got it — with every message listed before
/// it, none after; that end read for another request's number, or an ack of its own (a single request's answer),
/// doesn't end it.
#[test]
fn a_dump_ends_with_its_own_done() {
    let got = fixture::received("dump-routes4");
    let seq = u32::from_ne_bytes(got[0][8..12].try_into().unwrap());
    let replies: Vec<reply::Reply> = got.iter().flat_map(|d| reply::replies(d).unwrap()).collect();
    let listed =
        replies.iter().filter(|r| matches!(r, reply::Reply::Data { seq: s, .. } if *s == seq)).count();
    assert!(listed > 0);
    // (a number none of the replies carries — ip numbers each request one past the last)
    let elsewhere = seq.wrapping_sub(1);
    assert!(!replies.iter().any(|r| match r {
        reply::Reply::Ack { seq } | reply::Reply::Refused { seq, .. } => *seq == elsewhere,
        reply::Reply::Done { seq, .. } | reply::Reply::Data { seq, .. } => *seq == elsewhere,
    }));
    let mut other = Reading::new(elsewhere, true);
    for d in &got {
        other.take(d).unwrap();
    }
    assert!(!other.ended(), "another dump's end");
    let mut reading = Reading::new(seq, true);
    reading.take(&ack_for(seq)).unwrap();
    assert!(!reading.ended(), "an ack of its number");
    for d in &got {
        reading.take(d).unwrap();
    }
    assert!(reading.ended(), "the kernel's own end");
    reading.take(&got[0]).unwrap();
    assert_eq!(reading.outcome().unwrap().len(), listed, "what came before the end, nothing after");
}

/// A dump the kernel stopped says why in its end: the read is that refusal, not the part listed so far. (No capture
/// holds one — the kernel puts a dump's own failure there, minus its errno, only mid-way through a dump that went
/// wrong — so the end is built here as `netlink_dump_done` writes it.)
#[test]
fn a_dump_stopped_by_the_kernel_is_refused() {
    let got = fixture::received("dump-routes4");
    let seq = u32::from_ne_bytes(got[0][8..12].try_into().unwrap());
    let mut reading = Reading::new(seq, true);
    reading.take(&got[0]).unwrap();
    reading.take(&done_for(seq, libc::EINTR)).unwrap();
    assert!(reading.ended());
    match reading.outcome() {
        Err(Error::Kernel { errno, .. }) => assert_eq!(errno, libc::EINTR),
        other => panic!("{other:?}"),
    }
}

/// A dump request the kernel refuses is answered by an error message of its number, as a single request is — the
/// kernel's own (ip's refused route, with its words): the read is that refusal. Another request's doesn't end it.
#[test]
fn a_refused_dump_is_refused() {
    let refusal = fixture::received("err-route-prefix").pop().unwrap();
    let seq = u32::from_ne_bytes(refusal[8..12].try_into().unwrap());
    let mut other = Reading::new(seq.wrapping_add(1), true);
    other.take(&refusal).unwrap();
    assert!(!other.ended(), "another request's refusal");
    let mut reading = Reading::new(seq, true);
    reading.take(&refusal).unwrap();
    assert!(reading.ended());
    match reading.outcome() {
        Err(Error::Kernel { errno, said, .. }) => {
            assert_eq!(
                (errno, said.as_deref()),
                (libc::EINVAL, Some("Invalid prefix for given prefix length"))
            )
        }
        other => panic!("{other:?}"),
    }
}

// ── never a read past a length ──────────────────────────────────────────

/// Every reply the kernel gave in every captured case, mutated 200,000 times — bytes flipped, set, cut, repeated —
/// is read to a value or an error: never a panic, never a read past a length. Each read the crate makes of a
/// reply is in the loop: the replies, what a change and a read wait for, each message's typed reading.
#[test]
fn the_reply_parser_never_panics() {
    let seeds = fixture::every_reply();
    // (every kind of reply the crate reads is among the seeds: an ack, a refusal with its extended ack, a dump's end,
    // the generation, a table, a rule, a route, a policy rule, and the 1040-byte link message)
    let kinds: std::collections::HashSet<String> = seeds
        .iter()
        .flat_map(|d| reply::replies(d).unwrap())
        .map(|r| match r {
            reply::Reply::Ack { .. } => "ack".into(),
            reply::Reply::Refused { offset: Some(_), .. } | reply::Reply::Refused { said: Some(_), .. } => {
                "extended refusal".into()
            }
            reply::Reply::Refused { .. } => "refusal".into(),
            reply::Reply::Done { .. } => "done".into(),
            reply::Reply::Data { kind, .. } => format!("{kind:#x}"),
        })
        .collect();
    for k in ["ack", "extended refusal", "refusal", "done", "0xa0f", "0xa00", "0xa06", "0x18", "0x20", "0x10"]
    {
        assert!(kinds.contains(k), "no seed of kind {k}: {kinds:?}");
    }
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let any_batch = session_ruleset("tarae");
    for _ in 0..200_000 {
        let mut b = seeds[next() as usize % seeds.len()].clone();
        for _ in 0..1 + next() % 4 {
            let at = if b.is_empty() { 0 } else { next() as usize % b.len() };
            match next() % 6 {
                0 if !b.is_empty() => b[at] ^= 1 << (next() % 8),
                1 if !b.is_empty() => b[at] = [0, 0xff, 0x10, 0x04, 0x02][next() as usize % 5],
                2 => b.truncate(at),
                3 if !b.is_empty() => {
                    let end = (at + 1 + next() as usize % 16).min(b.len());
                    let piece = b[at..end].to_vec();
                    b.splice(at..at, piece);
                }
                4 if b.len() >= at + 4 => b[at..at + 4].copy_from_slice(&(next() as u32).to_ne_bytes()),
                _ => {}
            }
        }
        if let Ok(replies) = reply::replies(&b) {
            for r in replies {
                if let reply::Reply::Data { kind, body, .. } = r {
                    let _ = nft::generation_of(kind, body);
                    let _ = nft::table_of(kind, body);
                    let _ = nft::rule_of(kind, body);
                    let _ = route::route_of(kind, body);
                    let _ = route::rule_of(kind, body);
                }
            }
        }
        let seq = b.get(8..12).map_or(0, |s| u32::from_ne_bytes(s.try_into().unwrap()));
        let _ = Owed::new(&any_batch, seq.wrapping_sub(next() as u32 % 3)).take(&b);
        let _ = Reading::new(seq, next() % 2 == 0).take(&b);
    }
}
