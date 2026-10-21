//! One writer per store per process (`trust.json`, `sandbox.json`): the answers and take-backs given on the main
//! loop take a turn there as they're given (`turn`), and each store's writes land one at a time in that order —
//! never as their workers happen to reach the store's lock. What isn't written yet is kept as data (`Pending`):
//! `:restart` carries a take-back's and gives it again (`pending_carried`), a quit that gives up waiting names
//! them (`pending_said`).
//!
//! Between taraes no clock orders anything: each key of a store (a trust place, a sandbox.json project's name)
//! has a counter every write bumps under the store's lock, with a random tag of the write's (`Rev`). A no or a
//! take-back is always written; a yes is a compare-and-set — it holds only when every change at its key since the
//! one it had seen as it was given (`seen_now`) is one this tarae made (`mine`, `holds`): another's, unseen,
//! narrows, and the yes isn't written — the card asks again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

/// A key's change in a store: its counter and the random tag of the write that made it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rev {
    pub n: u64,
    pub tag: u64,
}

impl Rev {
    /// As a store keeps it.
    pub fn json(self) -> serde_json::Value {
        serde_json::json!([self.n, self.tag])
    }

    pub fn of(v: &serde_json::Value) -> Option<Rev> {
        Some(Rev { n: v[0].as_u64()?, tag: v[1].as_u64()? })
    }
}

/// A store's turns: the next one given, the one writing, and what each one not landed is.
#[derive(Default)]
struct Turns {
    next: u64,
    serving: u64,
    pending: BTreeMap<u64, Pending>,
    /// The newest change read or written there, by key: what this tarae has seen.
    seen: BTreeMap<String, Rev>,
    /// The changes this tarae made there, by key: counter → tag.
    mine: BTreeMap<String, BTreeMap<u64, u64>>,
    /// The givens of the last answer and the last take-back queued there, by key: a read before one misses it
    /// (`trust::Store.read_at`, `since`).
    asked: BTreeMap<String, (u64, u64)>,
    /// Tests: how many more turns may start (None: any).
    #[cfg(test)]
    held: Option<u64>,
    /// Tests: the turns below this were cut off (an exec, a quit that gave up) — they write nothing.
    #[cfg(test)]
    cut: u64,
    /// Tests: the next write fails.
    #[cfg(test)]
    fail_next: bool,
    /// Tests: the next write takes this long to start, its turn taken (a slow disk).
    #[cfg(test)]
    slow_next: Option<std::time::Duration>,
}

/// A write not landed: what a quit names, what it is (`carry`), and whether `:restart` gives it again (`again`: a
/// no or a take-back — a yes not written is lost, its card asks again). One of those whose write failed stays
/// (`failed`) — `:restart` gives it again, a quit names it — until a later write of this tarae's lands that wrote
/// over every part it narrows (`over`: a key, or `key\0part` — a key written over covers its parts). One whose
/// parts aren't known yet (its place worked out at the write) is never covered.
#[derive(Clone, Debug)]
pub struct Pending {
    pub said: String,
    pub carry: serde_json::Value,
    pub again: bool,
    over: Vec<String>,
    /// The keys it answers or takes back at (`asked`), each with whether it takes back.
    at: Vec<(String, bool)>,
    pub failed: bool,
}

/// Whether a write that wrote over `wrote` covers `part`: the same, or `part` one of its parts.
fn covers(wrote: &str, part: &str) -> bool {
    part.strip_prefix(wrote).is_some_and(|rest| rest.is_empty() || rest.starts_with('\0'))
}

static STORES: Mutex<BTreeMap<PathBuf, Turns>> = Mutex::new(BTreeMap::new());
static TURNED: Condvar = Condvar::new();

fn stores() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, Turns>> {
    STORES.lock().unwrap_or_else(|e| e.into_inner())
}

/// A write's place in its store's line, taken on the main loop as it's given (`turn` waits for it).
#[derive(Debug)]
pub struct Ticket {
    path: PathBuf,
    n: u64,
}

/// The next turn at `path`'s store for a write said `said` (`carry`: what it is; `again`: `:restart` gives it
/// again) that narrows `over`.
pub fn ticket(path: &Path, said: String, carry: serde_json::Value, again: bool, over: Vec<String>) -> Ticket {
    let mut s = stores();
    let t = s.entry(path.to_path_buf()).or_default();
    let n = t.next;
    t.next += 1;
    t.pending.insert(n, Pending { said, carry, again, over, at: Vec::new(), failed: false });
    Ticket { path: path.to_path_buf(), n }
}

/// `ticket`'s write placed (on its worker, before its turn): what it is and narrows, now its place is known.
pub fn placed(ticket: &Ticket, carry: serde_json::Value, over: Vec<String>) {
    let mut s = stores();
    let p = s.get_mut(&ticket.path).and_then(|t| t.pending.get_mut(&ticket.n));
    if let Some(p) = p {
        (p.carry, p.over) = (carry, over);
    }
}

/// A write's turn, until dropped (the next one goes then) — `done` says how it went (dropped without: failed).
pub struct Turn {
    path: PathBuf,
    n: u64,
    /// Landed, and what it wrote over (None: failed).
    landed: Option<Vec<String>>,
    /// Tests: this write is to fail.
    pub fails: bool,
}

impl Turn {
    /// The write went: landed (`ok`), having written over `over` (`Pending.over`'s form), or failed.
    pub fn done(mut self, ok: bool, over: Vec<String>) {
        self.landed = ok.then_some(over);
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut s = stores();
        if let Some(t) = s.get_mut(&self.path) {
            t.serving = t.serving.max(self.n + 1);
            if let Some(p) = t.pending.remove(&self.n) {
                if let Some(wrote) = &self.landed {
                    // (a failed one this one wrote over — every part it narrows —, in the order given: not a
                    // write at the same place that left it as it was — F1, N3)
                    let over = |q: &Pending| {
                        !q.over.is_empty() && q.over.iter().all(|part| wrote.iter().any(|w| covers(w, part)))
                    };
                    t.pending.retain(|_, q| !(q.failed && over(q)));
                } else if p.again {
                    t.pending.insert(self.n, Pending { failed: true, ..p });
                }
            }
        }
        TURNED.notify_all();
    }
}

/// Waits for `ticket`'s turn (a worker's): every write given before it has landed or failed. None: it was cut off
/// (tests).
pub fn turn(ticket: Ticket) -> Option<Turn> {
    let mut s = stores();
    loop {
        let t = s.entry(ticket.path.clone()).or_default();
        #[cfg(test)]
        if ticket.n < t.cut {
            t.pending.remove(&ticket.n);
            return None;
        }
        #[cfg(test)]
        let free = t.held != Some(0);
        #[cfg(not(test))]
        let free = true;
        if t.serving == ticket.n && free {
            #[cfg(test)]
            if let Some(n) = t.held.as_mut() {
                *n -= 1;
            }
            #[cfg(test)]
            let (fails, slow) = (std::mem::take(&mut t.fail_next), t.slow_next.take());
            #[cfg(not(test))]
            let fails = false;
            let turn = Turn { path: ticket.path, n: ticket.n, landed: None, fails };
            #[cfg(test)]
            if let Some(d) = slow {
                drop(s);
                std::thread::sleep(d);
            }
            return Some(turn);
        }
        s = TURNED.wait(s).unwrap_or_else(|e| e.into_inner());
    }
}

/// The writes not landed at `path`'s store, oldest first — the failed ones kept too.
pub fn pending(path: &Path) -> Vec<Pending> {
    stores().get(path).map(|t| t.pending.values().cloned().collect()).unwrap_or_default()
}

/// What `:restart` gives again at `path`'s store, oldest first.
pub fn pending_carried(path: &Path) -> Vec<serde_json::Value> {
    pending(path).into_iter().filter(|p| p.again).map(|p| p.carry).collect()
}

/// The writes not landed at these stores, as a quit names them.
pub fn pending_said(paths: &[&Path]) -> Vec<String> {
    paths.iter().flat_map(|p| pending(p)).map(|p| p.said).collect()
}

/// The newest change this tarae has seen at `path`'s store, by key — what a yes given now knew of.
pub fn seen_now(path: &Path) -> BTreeMap<String, Rev> {
    stores().get(path).map(|t| t.seen.clone()).unwrap_or_default()
}

/// `path`'s store read whole: each key's change there (a read that lands late doesn't lower what was seen) — and a
/// key it hasn't is forgotten: pruned (`TAKEN_KEPT`), its counter starts again, and what was seen of it before
/// would hold no answer given now (`holds`: a counter below what's seen never holds). A late read that hadn't a
/// key yet only makes the next answer there ask again.
pub fn saw(path: &Path, revs: impl IntoIterator<Item = (String, Rev)>) {
    let mut s = stores();
    let t = s.entry(path.to_path_buf()).or_default();
    let mut there = std::collections::BTreeSet::new();
    for (k, r) in revs {
        let e = t.seen.entry(k.clone()).or_default();
        if r.n > e.n {
            *e = r;
        }
        there.insert(k);
    }
    t.seen.retain(|k, _| there.contains(k));
}

/// Under the store's lock `key` is at `now`: what this tarae made there that `now` contradicts — the counter went
/// back (the store removed, restored, the key pruned) — is forgotten, and what it saw follows `now` (made again at
/// the same counter too: a yes given after it holds — N4).
fn agree(t: &mut Turns, key: &str, now: Rev) {
    if let Some(m) = t.mine.get(key) {
        let back = m.keys().next_back().is_some_and(|&top| now.n < top);
        if back || m.get(&now.n).is_some_and(|&tag| tag != now.tag) {
            t.mine.remove(key);
        }
    }
    if t.seen.get(key).is_some_and(|s| s.n > now.n || (s.n == now.n && s.tag != now.tag)) {
        t.seen.insert(key.to_string(), now);
    }
}

/// The change a write makes at `key` of `path`'s store over `now` (under its lock): the next counter, a new tag.
pub fn next(path: &Path, key: &str, now: Rev) -> Rev {
    agree(stores().entry(path.to_path_buf()).or_default(), key, now);
    Rev { n: now.n + 1, tag: tag() }
}

/// Whether a yes that had seen `seen` at `key` (`seen_now` as it was given) holds over `now` (read under the
/// store's lock, after `made`: what this same write changed there first): every change since `seen` is this
/// tarae's — none another tarae made unseen.
pub fn holds(path: &Path, key: &str, seen: Rev, now: Rev, made: &[Rev]) -> bool {
    let mut s = stores();
    let t = s.entry(path.to_path_buf()).or_default();
    agree(t, key, now);
    let mine = t.mine.get(key);
    let tag_of = |n: u64| {
        let made = made.iter().find(|r| r.n == n).map(|r| r.tag);
        made.or_else(|| mine.and_then(|m| m.get(&n).copied()))
    };
    match now.n.cmp(&seen.n) {
        std::cmp::Ordering::Equal => now.tag == seen.tag,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Greater => {
            let ours = mine.map_or(0, BTreeMap::len) + made.len();
            now.n - seen.n <= ours as u64
                && (seen.n + 1..=now.n).all(|n| tag_of(n).is_some())
                && tag_of(now.n) == Some(now.tag)
        }
    }
}

/// This tarae's write of `rev` at `key` of `path`'s store landed.
pub fn wrote(path: &Path, key: &str, rev: Rev) {
    let mut s = stores();
    let t = s.entry(path.to_path_buf()).or_default();
    t.mine.entry(key.to_string()).or_default().insert(rev.n, rev.tag);
    let e = t.seen.entry(key.to_string()).or_default();
    if rev.n > e.n {
        *e = rev;
    }
}

/// A random tag for a change.
pub fn tag() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    h.write_u32(std::process::id());
    h.finish().max(1)
}

/// `ticket`'s write, given at `given`, is at `key` of its store — a take-back (`takes`) or an answer.
pub fn asked(ticket: &Ticket, key: &str, given: u64, takes: bool) {
    let mut s = stores();
    let t = s.entry(ticket.path.clone()).or_default();
    let e = t.asked.entry(key.to_string()).or_default();
    match takes {
        true => e.1 = e.1.max(given),
        false => e.0 = e.0.max(given),
    }
    if let Some(p) = t.pending.get_mut(&ticket.n) {
        p.at.push((key.to_string(), takes));
    }
}

/// Whether an answer, and a take-back, at `key` of `path`'s store was queued after `at` (`last()` before a read)
/// — or is still waiting for its write, or failed: given before the read, it wasn't in what was read (N9).
pub fn since(path: &Path, key: &str, at: u64) -> (bool, bool) {
    let s = stores();
    let Some(t) = s.get(path) else { return (false, false) };
    let (a, b) = t.asked.get(key).copied().unwrap_or_default();
    let waits = |takes: bool| t.pending.values().any(|p| p.at.iter().any(|(k, w)| k == key && *w == takes));
    (a > at || waits(false), b > at || waits(true))
}

static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn raise_last(v: u64) {
    LAST.fetch_max(v, std::sync::atomic::Ordering::SeqCst);
}

/// A sequence for an answer or take-back, taken on the main thread as it's given: one past the last this tarae
/// gave (`LAST` — `:restart` carries it). No clock: it orders this tarae's writes and marks only.
pub fn given() -> u64 {
    LAST.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

/// `given`'s last, as `:restart` carries it.
pub fn last() -> u64 {
    LAST.load(std::sync::atomic::Ordering::SeqCst)
}

/// `:restart`'s `last` back.
pub fn last_again(v: u64) {
    raise_last(v);
}

/// Tests: `path`'s store lets `n` more writes start (None: any).
#[cfg(test)]
pub fn hold(path: &Path, n: Option<u64>) {
    stores().entry(path.to_path_buf()).or_default().held = n;
    TURNED.notify_all();
}

/// Tests: whether `path`'s store holds its writes.
#[cfg(test)]
pub fn is_held(path: &Path) -> bool {
    stores().get(path).is_some_and(|t| t.held.is_some())
}

/// Tests: `n` more of `path`'s held writes may start.
#[cfg(test)]
pub fn release(path: &Path, n: u64) {
    if let Some(h) = stores().entry(path.to_path_buf()).or_default().held.as_mut() {
        *h += n;
    }
    TURNED.notify_all();
}

/// Tests: the next write at `path`'s store fails.
#[cfg(test)]
pub fn fail_next(path: &Path) {
    stores().entry(path.to_path_buf()).or_default().fail_next = true;
}

/// Tests: the next write at `path`'s store waits `d` before it writes, its turn taken.
#[cfg(test)]
pub fn slow_next(path: &Path, d: std::time::Duration) {
    stores().entry(path.to_path_buf()).or_default().slow_next = Some(d);
}

/// Tests: the process ended — every write at `path`'s store not landed is cut off (none writes), and this tarae's
/// memory of it goes (the next tarae's own). The store's line goes on from there, not held.
#[cfg(test)]
pub fn cut(path: &Path) {
    let mut s = stores();
    let t = s.entry(path.to_path_buf()).or_default();
    t.cut = t.next;
    t.serving = t.serving.max(t.next);
    t.pending.clear();
    t.seen.clear();
    t.mine.clear();
    t.asked.clear();
    t.held = None;
    t.fail_next = false;
    t.slow_next = None;
    TURNED.notify_all();
}

/// Tests: how many writes at `path`'s store have taken their turn (landed, failed or cut).
#[cfg(test)]
pub fn served(path: &Path) -> u64 {
    stores().get(path).map_or(0, |t| t.serving)
}

/// Tests: how many turns at `path`'s store were given.
#[cfg(test)]
pub fn given_turns(path: &Path) -> u64 {
    stores().get(path).map_or(0, |t| t.next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store's path of its own (only a key here: nothing is written).
    fn store(s: &crate::scratch::Scratch, name: &str) -> PathBuf {
        s.to_path_buf().join(name)
    }

    /// R4 (N6, N10): this tarae's changes a counter contradicts are forgotten — another tag at one of them (the
    /// store made again), or the counter gone below them — so another tarae's change isn't taken for this one's.
    #[test]
    fn a_counter_that_contradicts_mine_clears_it() {
        let s = crate::scratch::Scratch::new("storewrite-mine");
        let path = store(&s, "made-again.json");
        wrote(&path, "k", Rev { n: 2, tag: 11 });
        let mine = next(&path, "k", Rev { n: 2, tag: 22 });
        wrote(&path, "k", mine);
        assert!(!holds(&path, "k", Rev { n: 1, tag: 1 }, mine, &[]), "their 2 taken for mine");
        let path = store(&s, "gone-back.json");
        wrote(&path, "k", Rev { n: 5, tag: 55 });
        next(&path, "k", Rev { n: 2, tag: 2 });
        assert_eq!(
            stores().get(&path).and_then(|t| t.mine.get("k").cloned()),
            None,
            "kept past a counter below it"
        );
    }

    /// R3: a counter below what a yes had seen (the store gone back) never holds; N4: made again at the counter seen,
    /// with another tag, what's seen follows it — a yes given after holds.
    #[test]
    fn a_yes_holds_only_over_what_it_saw() {
        let s = crate::scratch::Scratch::new("storewrite-holds");
        let path = store(&s, "store.json");
        assert!(
            !holds(&path, "k", Rev { n: 5, tag: 5 }, Rev { n: 3, tag: 3 }, &[]),
            "over a counter gone back"
        );
        let path = store(&s, "again.json");
        saw(&path, [("k".to_string(), Rev { n: 1, tag: 1 })]);
        assert!(!holds(&path, "k", Rev { n: 1, tag: 1 }, Rev { n: 1, tag: 2 }, &[]), "over another tag");
        let seen = seen_now(&path)["k"];
        assert!(holds(&path, "k", seen, Rev { n: 1, tag: 2 }, &[]), "what's seen didn't follow: {seen:?}");
    }

    /// A key a read no longer finds (pruned after `TAKEN_KEPT`, its counter starting again) is forgotten: an answer
    /// given after that read holds over it, never "changed in another tarae" for a counter that only went back.
    #[test]
    fn a_pruned_key_is_forgotten_by_the_next_read() {
        let s = crate::scratch::Scratch::new("storewrite-pruned");
        let path = store(&s, "store.json");
        saw(&path, [("k".to_string(), Rev { n: 5, tag: 5 })]);
        saw(&path, [("other".to_string(), Rev { n: 1, tag: 1 })]);
        let seen = seen_now(&path).get("k").copied();
        assert_eq!(seen, None, "kept past a read without it");
        assert!(holds(&path, "k", seen.unwrap_or_default(), Rev::default(), &[]), "an answer given now");
    }

    /// N9: an answer or take-back given before a read whose write hasn't landed — or failed — is since the read.
    #[test]
    fn a_write_not_landed_is_since_any_read() {
        let s = crate::scratch::Scratch::new("storewrite-since");
        let path = store(&s, "store.json");
        let t = ticket(&path, String::new(), serde_json::Value::Null, true, vec!["k".into()]);
        asked(&t, "k", 7, true);
        let waiting = since(&path, "k", 100);
        drop(turn(t));
        let failed = since(&path, "k", 100);
        assert_eq!(waiting, (false, true), "waiting");
        assert_eq!(failed, (false, true), "failed");
    }

    /// F1, N3: a landed write drops a failed one only when it wrote over every part it narrows.
    #[test]
    fn a_failed_write_goes_only_with_what_wrote_it_over() {
        assert!(covers("p", "p\0v1") && covers("p", "p") && covers("p\0v1", "p\0v1"));
        assert!(!covers("p\0v2", "p\0v1") && !covers("p\0v1", "p") && !covers("p", "pq\0v1"));
    }
}
