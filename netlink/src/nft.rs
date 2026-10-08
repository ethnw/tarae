//! nf_tables as tarae writes it: family `inet` only, one transaction per send (`Transaction`: `BATCH_BEGIN`, the
//! changes, `BATCH_END` — the kernel takes it whole or not at all), and only the expressions the session's ruleset is
//! made of. Each is written as nft 1.1.3 writes it — the same attributes in the same order, the same byte order of
//! each value — so the bytes can be held against nft's (`fixtures/`); the table and chains as its `create`, which
//! adopts nothing already there. What differs, on purpose: every change asks the kernel's ack (nft asks none and
//! learns only of errors); there are no sets (a set's elements are a rule each).
//!
//! Byte order, as nf_tables keeps it: attribute numbers (registers, keys, offsets, verdicts) in network order; a
//! compared or loaded value as the packet or the kernel holds it — an address and a port in network order
//! (`address`, `port`), a mark in host order (`mark`).

use std::net::IpAddr;

use crate::attr::{self, nest, put, put_be32, put_str};
use crate::{ACK, APPEND, Batch, CREATE, DUMP, EXCL, Family, Malformed, Protocol, REQUEST};

/// nf_tables' messages are its subsystem's (`NFNL_SUBSYS_NFTABLES`) in the type's high byte.
const SUBSYS: u16 = 10 << 8;
/// (`NFT_MSG_NEWTABLE` is 0)
const NEWTABLE: u16 = SUBSYS;
const GETTABLE: u16 = SUBSYS | 1;
const NEWCHAIN: u16 = SUBSYS | 3;
const NEWRULE: u16 = SUBSYS | 6;
const GETRULE: u16 = SUBSYS | 7;
const NEWGEN: u16 = SUBSYS | 15;
const GETGEN: u16 = SUBSYS | 16;
/// A batch's ends (`NFNL_MSG_BATCH_*`), their `res_id` the subsystem they're for.
const BATCH_BEGIN: u16 = 0x10;
const BATCH_END: u16 = 0x11;

/// `NFPROTO_INET`, and `AF_UNSPEC` for what's no family's.
const INET: u8 = 1;
const UNSPEC: u8 = 0;

/// `NFT_TABLE_F_OWNER`: the table is its socket's — no other socket changes it, and it goes when that socket closes.
const OWNER: u32 = 2;

/// `struct nfgenmsg`.
fn nfgen(family: u8, res_id: u16) -> [u8; 4] {
    let r = res_id.to_be_bytes();
    [family, 0, r[0], r[1]]
}

/// Where a chain hangs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    Prerouting,
    Output,
}

/// A chain's kind: `route` reroutes what it marks (output only), `filter` judges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainKind {
    Route,
    Filter,
}

/// A register: `VERDICT` (0) holds a rule's verdict, the others 16 bytes of data each (nft's old numbering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg(pub u32);

impl Reg {
    pub const VERDICT: Reg = Reg(0);
    pub const R1: Reg = Reg(1);
    pub const R2: Reg = Reg(2);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaKey {
    /// The packet's family (`NFPROTO_IPV4`/`_IPV6` — `Family::number`), one byte.
    NfProto,
    /// Its transport protocol (6 TCP, 17 UDP), one byte.
    L4Proto,
    /// Its mark, four bytes in host order.
    Mark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    /// The network header (an address: 16 bytes in for IPv4's destination, 24 for IPv6's).
    Network,
    /// The transport header (a destination port: 2 bytes in).
    Transport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Drop,
    /// Back to where the chain was entered from: the hook's policy.
    Return,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketKey {
    /// Whether the socket the packet would be delivered to is transparent (`IP_TRANSPARENT`).
    Transparent,
    /// The id of the packet's own socket's cgroup's ancestor at `level` (8 bytes, `cgroup`) — the cgroup of the
    /// process that made the socket, not of one that holds it now. What `level` counts from is the kernel's: the
    /// sender's cgroup namespace's root on Debian's 6.1 and on 7.3, the machine's root on Ubuntu's 6.8.0-1013 — the
    /// same only where the sender's namespace is the machine's (spike6, Q2).
    Cgroupv2 { level: u32 },
}

/// An expression of a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// Loads `key` into `dreg`.
    Meta { key: MetaKey, dreg: Reg },
    /// Sets `key` from `sreg`.
    MetaSet { key: MetaKey, sreg: Reg },
    /// Loads `len` bytes at `offset` into `base` into `dreg`.
    Payload { base: Base, offset: u32, len: u32, dreg: Reg },
    /// Goes on only where `sreg` holds `data`.
    Cmp { sreg: Reg, data: Vec<u8> },
    /// Puts `data` in `dreg`.
    Immediate { dreg: Reg, data: Vec<u8> },
    /// The rule's verdict.
    Verdict(Verdict),
    /// Hands the packet to the transparent socket at the address in `addr` and the port in `port` (that family's).
    Tproxy { family: Family, addr: Reg, port: Reg },
    /// Loads what `key` says of the socket the packet would reach into `dreg`.
    Socket { key: SocketKey, dreg: Reg },
}

/// A port as a packet holds it.
pub fn port(p: u16) -> Vec<u8> {
    p.to_be_bytes().to_vec()
}

/// A mark as the kernel holds it.
pub fn mark(m: u32) -> Vec<u8> {
    m.to_ne_bytes().to_vec()
}

/// A cgroup's id as the kernel holds it (its folder's inode), to compare with what `SocketKey::Cgroupv2` loads.
pub fn cgroup(id: u64) -> Vec<u8> {
    id.to_ne_bytes().to_vec()
}

/// An address as a packet holds it.
pub fn address(a: IpAddr) -> Vec<u8> {
    match a {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

/// One nf_tables transaction: `new`, its changes, `end`.
#[derive(Clone, Debug)]
pub struct Transaction(Batch);

impl Default for Transaction {
    fn default() -> Transaction {
        Transaction::new()
    }
}

impl Transaction {
    pub fn new() -> Transaction {
        let mut b = Batch::new(Protocol::Netfilter);
        b.message(BATCH_BEGIN, REQUEST, &nfgen(UNSPEC, 10), |_| {});
        Transaction(b)
    }

    /// A new table named `name` (as nft's `create table`): one of that name already there is refused (`EEXIST`),
    /// never taken as it is — the table is the sender's and new, said by the kernel. `owned`: the sending socket's
    /// alone, and gone with it.
    pub fn table(&mut self, name: &str, owned: bool) {
        self.0.message(NEWTABLE, REQUEST | EXCL | ACK, &nfgen(INET, 0), |b| {
            put_str(b, 1, name);
            put_be32(b, 2, if owned { OWNER } else { 0 });
        });
    }

    /// A new base chain `name` in `table` on `hook` at `priority`, its policy accept (as nft's `create chain`: one of
    /// that name already there is refused, never updated).
    pub fn chain(&mut self, table: &str, name: &str, hook: Hook, priority: i32, kind: ChainKind) {
        let hooknum = match hook {
            Hook::Prerouting => 0,
            Hook::Output => 3,
        };
        let kind = match kind {
            ChainKind::Route => "route",
            ChainKind::Filter => "filter",
        };
        self.0.message(NEWCHAIN, REQUEST | CREATE | EXCL | ACK, &nfgen(INET, 0), |b| {
            put_str(b, 1, table);
            put_str(b, 3, name);
            put_be32(b, 5, 1);
            put_str(b, 7, kind);
            nest(b, 4, |b| {
                put_be32(b, 1, hooknum);
                put_be32(b, 2, priority as u32);
            });
        });
    }

    /// A rule at the end of `chain` in `table`.
    pub fn rule(&mut self, table: &str, chain: &str, exprs: &[Expr]) {
        self.0.message(NEWRULE, REQUEST | CREATE | APPEND | ACK, &nfgen(INET, 0), |b| {
            put_str(b, 1, table);
            put_str(b, 2, chain);
            nest(b, 4, |b| {
                for e in exprs {
                    nest(b, 1, |b| expr(b, e));
                }
            });
        });
    }

    /// The transaction, ended: what `Socket::apply` sends.
    pub fn end(mut self) -> Batch {
        self.0.message(BATCH_END, REQUEST, &nfgen(UNSPEC, 10), |_| {});
        self.0
    }
}

/// An expression: its name, then its data's attributes in nft's order.
fn expr(b: &mut Vec<u8>, e: &Expr) {
    let data = |b: &mut Vec<u8>, name: &str, attrs: &dyn Fn(&mut Vec<u8>)| {
        put_str(b, 1, name);
        nest(b, 2, |b| attrs(b));
    };
    let meta = |k: MetaKey| match k {
        MetaKey::Mark => 3,
        MetaKey::NfProto => 15,
        MetaKey::L4Proto => 16,
    };
    let value = |b: &mut Vec<u8>, kind: u16, v: &[u8]| nest(b, kind, |b| put(b, 1, v));
    match e {
        Expr::Meta { key, dreg } => data(b, "meta", &|b| {
            put_be32(b, 2, meta(*key));
            put_be32(b, 1, dreg.0);
        }),
        Expr::MetaSet { key, sreg } => data(b, "meta", &|b| {
            put_be32(b, 2, meta(*key));
            put_be32(b, 3, sreg.0);
        }),
        Expr::Payload { base, offset, len, dreg } => data(b, "payload", &|b| {
            put_be32(b, 1, dreg.0);
            put_be32(b, 2, if *base == Base::Network { 1 } else { 2 });
            put_be32(b, 3, *offset);
            put_be32(b, 4, *len);
        }),
        Expr::Cmp { sreg, data: v } => data(b, "cmp", &|b| {
            put_be32(b, 1, sreg.0);
            put_be32(b, 2, 0);
            value(b, 3, v);
        }),
        Expr::Immediate { dreg, data: v } => data(b, "immediate", &|b| {
            put_be32(b, 1, dreg.0);
            value(b, 2, v);
        }),
        Expr::Verdict(v) => data(b, "immediate", &|b| {
            let code: i32 = match v {
                Verdict::Drop => 0,
                Verdict::Accept => 1,
                Verdict::Return => -5,
            };
            put_be32(b, 1, Reg::VERDICT.0);
            nest(b, 2, |b| nest(b, 2, |b| put_be32(b, 1, code as u32)));
        }),
        Expr::Tproxy { family, addr, port } => data(b, "tproxy", &|b| {
            put_be32(b, 1, family.number().into());
            put_be32(b, 2, addr.0);
            put_be32(b, 3, port.0);
        }),
        Expr::Socket { key: SocketKey::Transparent, dreg } => data(b, "socket", &|b| {
            put_be32(b, 1, 0);
            put_be32(b, 2, dreg.0);
            // (the cgroup level: nft writes 0 for every other key)
            put_be32(b, 3, 0);
        }),
        Expr::Socket { key: SocketKey::Cgroupv2 { level }, dreg } => data(b, "socket", &|b| {
            put_be32(b, 1, 3);
            put_be32(b, 2, dreg.0);
            put_be32(b, 3, *level);
        }),
    }
}

/// The request for the ruleset's generation (`NFT_MSG_GETGEN`): a number every committed change moves by one, in
/// this netns.
pub(crate) fn generation_request() -> Batch {
    let mut b = Batch::new(Protocol::Netfilter);
    b.message(GETGEN, REQUEST | ACK, &nfgen(UNSPEC, 0), |_| {});
    b
}

/// The request for every `inet` table.
pub(crate) fn tables_request() -> Batch {
    let mut b = Batch::new(Protocol::Netfilter);
    b.message(GETTABLE, REQUEST | DUMP, &nfgen(INET, 0), |_| {});
    b
}

/// The request for every `inet` rule.
pub(crate) fn rules_request() -> Batch {
    let mut b = Batch::new(Protocol::Netfilter);
    b.message(GETRULE, REQUEST | DUMP, &nfgen(INET, 0), |_| {});
    b
}

/// A rule as a dump lists it: where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSeen {
    pub table: String,
    pub chain: String,
}

/// The attributes of an nf_tables message's body (after its `nfgenmsg`).
fn body_attrs(body: &[u8]) -> impl Iterator<Item = Result<attr::Attr<'_>, Malformed>> {
    attr::attrs(body, 4)
}

/// `NFT_MSG_NEWGEN`'s generation.
pub(crate) fn generation_of(kind: u16, body: &[u8]) -> Result<u32, Malformed> {
    if kind != NEWGEN || body.len() < 4 {
        return Err(Malformed("not the ruleset's generation"));
    }
    for a in body_attrs(body) {
        let a = a?;
        if a.kind == 1 {
            return a.u32_be();
        }
    }
    Err(Malformed("a generation message without its number"))
}

/// A table's name, from a table dump's message.
pub(crate) fn table_of(kind: u16, body: &[u8]) -> Result<String, Malformed> {
    if kind != NEWTABLE || body.len() < 4 {
        return Err(Malformed("not a table"));
    }
    for a in body_attrs(body) {
        let a = a?;
        if a.kind == 1 {
            return Ok(a.string());
        }
    }
    Err(Malformed("a table without its name"))
}

/// A rule's table and chain, from a rule dump's message.
pub(crate) fn rule_of(kind: u16, body: &[u8]) -> Result<RuleSeen, Malformed> {
    if kind != NEWRULE || body.len() < 4 {
        return Err(Malformed("not a rule"));
    }
    let (mut table, mut chain) = (None, None);
    for a in body_attrs(body) {
        let a = a?;
        match a.kind {
            1 => table = Some(a.string()),
            2 => chain = Some(a.string()),
            _ => {}
        }
    }
    match (table, chain) {
        (Some(table), Some(chain)) => Ok(RuleSeen { table, chain }),
        _ => Err(Malformed("a rule without its table or chain")),
    }
}

#[cfg(target_os = "linux")]
mod read {
    use std::time::Instant;

    use super::*;
    use crate::{Error, Socket};

    /// The ruleset's generation in the socket's netns.
    pub fn generation(sock: &mut Socket, deadline: Instant) -> Result<u32, Error> {
        let got = sock.read(&generation_request(), deadline)?;
        let m = got.first().ok_or(Malformed("no generation in the answer"))?;
        Ok(generation_of(m.kind, &m.body)?)
    }

    /// Every `inet` table's name in the socket's netns.
    pub fn tables(sock: &mut Socket, deadline: Instant) -> Result<Vec<String>, Error> {
        let got = sock.read(&tables_request(), deadline)?;
        got.iter().map(|m| table_of(m.kind, &m.body).map_err(Error::from)).collect()
    }

    /// Every `inet` rule in the socket's netns, in the order the kernel lists them.
    pub fn rules(sock: &mut Socket, deadline: Instant) -> Result<Vec<RuleSeen>, Error> {
        let got = sock.read(&rules_request(), deadline)?;
        got.iter().map(|m| rule_of(m.kind, &m.body).map_err(Error::from)).collect()
    }
}

#[cfg(target_os = "linux")]
pub use read::{generation, rules, tables};
