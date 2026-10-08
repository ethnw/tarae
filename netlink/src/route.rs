//! rtnetlink as tarae writes it: default routes (`default dev lo`, `local default dev lo table N`) and the policy rule
//! that sends marked packets to a table (`fwmark M[/mask] lookup N`), v4 and v6 — each written as iproute2 6.15
//! writes it (its scopes, its `RTA_DST` for IPv6 only, the table in the header), so the bytes can be held
//! against ip's (`fixtures/`). Every change asks its ack, and none replaces what's there (`EXCL`). And the reads that
//! say what's there: every route, every policy rule, of a family.

use std::net::IpAddr;

use crate::attr::{self, put, put_u32};
use crate::{ACK, Batch, CREATE, DUMP, EXCL, Family, Malformed, Protocol, REQUEST};

const NEWROUTE: u16 = 24;
const GETROUTE: u16 = 26;
const NEWRULE: u16 = 32;
const GETRULE: u16 = 34;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_PRIORITY: u16 = 6;
const RTA_TABLE: u16 = 15;
const FRA_PRIORITY: u16 = 6;
const FRA_FWMARK: u16 = 10;
const FRA_TABLE: u16 = 15;
const FRA_FWMASK: u16 = 16;

/// `rtm_protocol`: what ip says of a route a person added.
const RTPROT_BOOT: u8 = 3;
/// `rtm_scope`s.
pub const SCOPE_UNIVERSE: u8 = 0;
pub const SCOPE_LINK: u8 = 253;
pub const SCOPE_HOST: u8 = 254;
/// `rtm_type`s.
pub const UNICAST: u8 = 1;
pub const LOCAL: u8 = 2;
/// `FR_ACT_TO_TBL`.
const TO_TABLE: u8 = 1;
/// The main table.
pub const MAIN: u8 = 254;

/// What a default route does with what it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    /// Sends it out of the device.
    Unicast,
    /// Takes it as this machine's own, whatever its address (TPROXY's way home).
    Local,
}

/// Route and policy-rule changes, each acked, sent in one datagram (each its own: rtnetlink takes them one by one).
/// So a refusal is partial: `Socket::apply` says the first message refused, and the others may have landed — and
/// since none replaces what's there, the same changes sent again are refused again. What's there is for the reads to
/// say (`routes`, `rules`).
#[derive(Clone, Debug)]
pub struct Changes(Batch);

impl Default for Changes {
    fn default() -> Changes {
        Changes::new()
    }
}

impl Changes {
    pub fn new() -> Changes {
        Changes(Batch::new(Protocol::Route))
    }

    /// A default route in `table` through device `oif`. (Tables past 255 — written in `RTA_TABLE` — tarae doesn't
    /// use, so they aren't taken.)
    pub fn default_route(&mut self, family: Family, table: u8, kind: RouteKind, oif: u32) {
        // (ip's scopes: an IPv4 route through a device alone is the link's, a local one the host's; IPv6 has none)
        let scope = match (family, kind) {
            (Family::V6, _) => SCOPE_UNIVERSE,
            (Family::V4, RouteKind::Unicast) => SCOPE_LINK,
            (Family::V4, RouteKind::Local) => SCOPE_HOST,
        };
        let kind = match kind {
            RouteKind::Unicast => UNICAST,
            RouteKind::Local => LOCAL,
        };
        let rtmsg = [family.number(), 0, 0, 0, table, RTPROT_BOOT, scope, kind, 0, 0, 0, 0];
        self.0.message(NEWROUTE, REQUEST | ACK | EXCL | CREATE, &rtmsg, |b| {
            // (ip names IPv6's default as `::/0`; IPv4's it leaves unnamed)
            if family == Family::V6 {
                put(b, RTA_DST, &[0; 16]);
            }
            put_u32(b, RTA_OIF, oif);
        });
    }

    /// A policy rule: packets marked `fwmark` (under `fwmask`) look `table` up.
    pub fn fwmark_rule(&mut self, family: Family, fwmark: u32, fwmask: Option<u32>, table: u8) {
        let hdr = [family.number(), 0, 0, 0, table, 0, 0, TO_TABLE, 0, 0, 0, 0];
        self.0.message(NEWRULE, REQUEST | ACK | EXCL | CREATE, &hdr, |b| {
            put_u32(b, FRA_FWMARK, fwmark);
            if let Some(m) = fwmask {
                put_u32(b, FRA_FWMASK, m);
            }
        });
    }

    pub fn end(self) -> Batch {
        self.0
    }
}

/// The request for every route of `family`, every table's.
pub(crate) fn routes_request(family: Family) -> Batch {
    let mut b = Batch::new(Protocol::Route);
    b.message(GETROUTE, REQUEST | DUMP, &[family.number(), 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], |_| {});
    b
}

/// The request for every policy rule of `family`.
pub(crate) fn rules_request(family: Family) -> Batch {
    let mut b = Batch::new(Protocol::Route);
    b.message(GETRULE, REQUEST | DUMP, &[family.number(), 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], |_| {});
    b
}

/// A route as a dump lists it — what decides where a packet goes (not its counters or cache times, which move).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSeen {
    pub table: u32,
    pub kind: u8,
    pub scope: u8,
    pub dst: Option<IpAddr>,
    pub dst_len: u8,
    pub oif: Option<u32>,
    pub priority: Option<u32>,
}

/// A policy rule as a dump lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSeen {
    pub table: u32,
    pub action: u8,
    pub priority: Option<u32>,
    pub fwmark: Option<u32>,
    pub fwmask: Option<u32>,
}

fn ip(v: &[u8]) -> Option<IpAddr> {
    match v.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(v).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(v).ok()?)),
        _ => None,
    }
}

/// A route dump's message.
pub(crate) fn route_of(kind: u16, body: &[u8]) -> Result<RouteSeen, Malformed> {
    if kind != NEWROUTE || body.len() < 12 {
        return Err(Malformed("not a route"));
    }
    let mut r = RouteSeen {
        table: body[4].into(),
        kind: body[7],
        scope: body[6],
        dst: None,
        dst_len: body[1],
        oif: None,
        priority: None,
    };
    for a in attr::attrs(body, 12) {
        let a = a?;
        match a.kind {
            RTA_DST => {
                r.dst = Some(ip(a.value).ok_or(Malformed("a route's destination of no family's length"))?)
            }
            RTA_OIF => r.oif = Some(a.u32_ne()?),
            RTA_PRIORITY => r.priority = Some(a.u32_ne()?),
            RTA_TABLE => r.table = a.u32_ne()?,
            _ => {}
        }
    }
    Ok(r)
}

/// A policy-rule dump's message.
pub(crate) fn rule_of(kind: u16, body: &[u8]) -> Result<RuleSeen, Malformed> {
    if kind != NEWRULE || body.len() < 12 {
        return Err(Malformed("not a policy rule"));
    }
    let mut r =
        RuleSeen { table: body[4].into(), action: body[7], priority: None, fwmark: None, fwmask: None };
    for a in attr::attrs(body, 12) {
        let a = a?;
        match a.kind {
            FRA_TABLE => r.table = a.u32_ne()?,
            FRA_PRIORITY => r.priority = Some(a.u32_ne()?),
            FRA_FWMARK => r.fwmark = Some(a.u32_ne()?),
            FRA_FWMASK => r.fwmask = Some(a.u32_ne()?),
            _ => {}
        }
    }
    Ok(r)
}

#[cfg(target_os = "linux")]
mod read {
    use std::time::Instant;

    use super::*;
    use crate::{Error, Socket};

    /// Every route of `family` in the socket's netns, every table's.
    pub fn routes(sock: &mut Socket, family: Family, deadline: Instant) -> Result<Vec<RouteSeen>, Error> {
        let got = sock.read(&routes_request(family), deadline)?;
        got.iter().map(|m| route_of(m.kind, &m.body).map_err(Error::from)).collect()
    }

    /// Every policy rule of `family` in the socket's netns.
    pub fn rules(sock: &mut Socket, family: Family, deadline: Instant) -> Result<Vec<RuleSeen>, Error> {
        let got = sock.read(&rules_request(family), deadline)?;
        got.iter().map(|m| rule_of(m.kind, &m.body).map_err(Error::from)).collect()
    }
}

#[cfg(target_os = "linux")]
pub use read::{routes, rules};
