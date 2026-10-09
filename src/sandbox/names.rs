//! The names a session's runs looked up, each with the synthetic addresses tarae answered (step 6e,
//! `target/sb/step6e-design.md`; Q1: `198.18.0.0/15`, and a /64 beside the session's own IPv6 address). A run asks
//! for a name; tarae answers from here and never forwards the question — and a connection to one of these addresses
//! names its host before any byte (the proxy reads the name back from here).
//!
//! One index per name, both families from it: `198.18.0.0 + i` and `2001:db8:0:1::i`. Kept for the session's life
//! (a daemon a run left, the JVM's cache of 30 s, Gradle's daemon: an answer must hold as long as anyone may use it),
//! bounded at `KEPT` — the IPv4 pool, its first and last address left out. Past it the name used least recently goes,
//! and its address is the next one given: an address is never reused short of the bound.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

/// The IPv4 pool's base (`198.18.0.0/15`: benchmarking's, RFC 2544 — no one's hosts).
const POOL_V4: u32 = u32::from_be_bytes([198, 18, 0, 0]);
/// The IPv4 pool's size.
const POOL_V4_LEN: u32 = 1 << 17;
/// The IPv6 pool's /64, beside the session's own `2001:db8::1`.
const POOL_V6: [u16; 4] = [0x2001, 0xdb8, 0, 1];

/// The most names a session keeps: the IPv4 pool but its first and last address.
pub const KEPT: u32 = POOL_V4_LEN - 2;

/// A name's addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Given {
    pub v4: Ipv4Addr,
    pub v6: Ipv6Addr,
}

fn given(i: u32) -> Given {
    let s = POOL_V6;
    Given {
        v4: Ipv4Addr::from(POOL_V4 + i),
        v6: Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, (i >> 16) as u16, i as u16),
    }
}

/// The index `ip` stands for in either pool — None outside them, or at an index no name can have.
fn index(ip: IpAddr) -> Option<u32> {
    let i = match ip {
        IpAddr::V4(v4) => u32::from(v4).checked_sub(POOL_V4).filter(|i| *i < POOL_V4_LEN)?,
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if s[..4] != POOL_V6 || s[4] != 0 || s[5] != 0 {
                return None;
            }
            u32::from(s[6]) << 16 | u32::from(s[7])
        }
    };
    (1..=KEPT).contains(&i).then_some(i)
}

/// Whether `ip` is in a pool tarae answers names from — given to a name or not (a literal address there that no
/// name has is refused: risk 2).
pub fn in_pool(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => u32::from(v4).wrapping_sub(POOL_V4) < POOL_V4_LEN,
        IpAddr::V6(v6) => v6.segments()[..4] == POOL_V6,
    }
}

#[derive(Default)]
struct Inner {
    by_name: HashMap<String, u32>,
    /// Index → (name, when it was last used).
    by_index: HashMap<u32, (String, u64)>,
    /// When last used → index: the least recent first.
    by_use: BTreeMap<u64, u32>,
    clock: u64,
    /// The next index never given (the pool is used in order up to the bound).
    next: u32,
    /// Names given up so far (the log says the first, then the count at each power of two).
    gone: u64,
}

impl Inner {
    fn touch(&mut self, i: u32) {
        self.clock += 1;
        let now = self.clock;
        if let Some((_, used)) = self.by_index.get_mut(&i) {
            self.by_use.remove(used);
            *used = now;
            self.by_use.insert(now, i);
        }
    }
}

/// A session's names.
pub struct Names {
    inner: Mutex<Inner>,
    bound: u32,
}

impl Default for Names {
    fn default() -> Names {
        Names { inner: Default::default(), bound: KEPT }
    }
}

impl Names {
    pub fn new() -> Names {
        Names::default()
    }

    /// Tests: a map bounded at `n` names.
    #[cfg(test)]
    fn bounded(n: u32) -> Names {
        Names { inner: Default::default(), bound: n.min(KEPT) }
    }

    /// `name`'s addresses (`name` as `parse::name` makes it): the same each time it's asked, given at the first.
    pub fn give(&self, name: &str) -> Given {
        let mut n = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&i) = n.by_name.get(name) {
            n.touch(i);
            return given(i);
        }
        let i = if n.next < self.bound {
            n.next += 1;
            n.next
        } else {
            // (the bound reached: the least recently used name gives up its index)
            let (_, oldest) = n.by_use.pop_first().expect("a full map has a least recent name");
            let (old, _) = n.by_index.remove(&oldest).expect("every index used has its name");
            n.by_name.remove(&old);
            n.gone += 1;
            if n.gone.is_power_of_two() {
                let what = if n.gone == 1 {
                    format!("{old} given up")
                } else {
                    format!("{} given up so far, the latest {old}", n.gone)
                };
                crate::log::line(
                    "sandbox",
                    "info",
                    &format!(
                        "the session's names: {} kept, the least recently used go — {what} (a connection to its \
                         address is refused)",
                        self.bound
                    ),
                );
            }
            oldest
        };
        n.by_name.insert(name.to_string(), i);
        n.by_index.insert(i, (name.to_string(), 0));
        n.touch(i);
        given(i)
    }

    /// The name `ip` was given to — None where it's no address of the pools or no name has it (now).
    pub fn name_of(&self, ip: IpAddr) -> Option<String> {
        let i = index(ip)?;
        let mut n = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let name = n.by_index.get(&i)?.0.clone();
        n.touch(i);
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name keeps its addresses — one of each family, from one index — and each names it back; another name gets
    /// others. An address of the pools no name has, and any address outside them, names nothing; `in_pool` tells
    /// the first from the second.
    #[test]
    fn a_name_keeps_its_addresses_and_they_name_it() {
        let names = Names::new();
        let a = names.give("a.test");
        assert_eq!(a, names.give("a.test"), "asked again: the same");
        assert_eq!(
            a,
            Given { v4: Ipv4Addr::new(198, 18, 0, 1), v6: "2001:db8:0:1::1".parse().unwrap() },
            "the pool's first"
        );
        let b = names.give("b.test");
        assert!(a.v4 != b.v4 && a.v6 != b.v6);
        for ip in [IpAddr::V4(a.v4), IpAddr::V6(a.v6)] {
            assert_eq!(names.name_of(ip).as_deref(), Some("a.test"), "{ip}");
        }
        assert_eq!(names.name_of(IpAddr::V6(b.v6)).as_deref(), Some("b.test"));
        let unused: IpAddr = "198.19.0.7".parse().unwrap();
        let outside: IpAddr = "198.20.0.1".parse().unwrap();
        assert_eq!((names.name_of(unused), in_pool(unused)), (None, true), "in the pool, no name's");
        assert_eq!((names.name_of(outside), in_pool(outside)), (None, false));
        assert!(in_pool("2001:db8:0:1:ffff::1".parse().unwrap()) && !in_pool("2001:db8::1".parse().unwrap()));
        let first: IpAddr = "198.18.0.0".parse().unwrap();
        assert_eq!(names.name_of(first), None, "the pool's first address is no name's");
    }

    /// Bounded: past the bound, the name **used** least recently goes — a name asked or connected to lately stays,
    /// whenever it was first given — and its address is the one given next: until then no address is reused. What
    /// goes is said in a bounded number of lines (the first, then the count at each power of two).
    #[test]
    fn the_least_recently_used_name_goes_past_the_bound() {
        let names = Names::bounded(3);
        let captured = crate::log::capture();
        let first = names.give("first.test");
        let second = names.give("second.test");
        names.give("third.test");
        // (the first used again — by a connection — after the others were given)
        assert_eq!(names.name_of(IpAddr::V4(first.v4)).as_deref(), Some("first.test"));
        let fourth = names.give("fourth.test");
        assert_eq!(fourth, second, "the least recently used one's address, given again");
        assert_eq!(names.name_of(IpAddr::V4(first.v4)).as_deref(), Some("first.test"), "used lately: kept");
        assert_eq!(names.name_of(IpAddr::V4(second.v4)).as_deref(), Some("fourth.test"));
        for n in 0..4 {
            names.give(&format!("more{n}.test"));
        }
        let said: Vec<String> =
            captured.lines().into_iter().filter(|l| l.contains("the session's names:")).collect();
        assert_eq!(said.len(), 3, "five gone: the first, 2, 4 — {said:#?}");
        assert!(said[0].contains("second.test given up"), "{said:#?}");
        let n = names.inner.lock().unwrap();
        assert_eq!((n.by_name.len(), n.by_index.len(), n.by_use.len()), (3, 3, 3), "nothing grows past it");
    }
}
