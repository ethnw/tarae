//! The marks a session's runs' connections carry (step 6d, `target/sb/step6d-design.md` (0′)): tarae's supervisor sets
//! one on a run's socket (`SO_MARK`, which needs `CAP_NET_ADMIN` over the session's netns — tarae's, never a run's)
//! before it makes the run's connect or addressed send beyond the session; the session's rules keep it and the planted
//! socket reads it back (`tcp_fwmark_accept`; a datagram's, `SO_RCVMARK`, is 6e's). This is the table between: a mark
//! per (run, program) — at most `PER_RUN` programs a run, then one for the rest — and what the egress proxy needs of
//! each: the run's rules, where its refusals are said (held weakly: they end with the run), what the report names.
//!
//! A mark is even and never 0: bit 0 is the session's output chain's to add (`redirect::MARK`, the route rule's — a
//! socket whose own mark carried it would meet that rule at its first route lookup, and IPv4 would take the
//! destination itself as its source: M12). What the planted socket reads is the mark with that bit; `owner` takes
//! either. Kept for the session's life — what a run left running connects as its run, long after it — at most
//! `KEPT`, the oldest given up past that (said).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};

use super::Denial;
use super::proxy::egress::Egress;

/// The most marks a session keeps: past it the oldest goes, and a connection still carrying it is refused as one
/// tarae didn't mark.
pub const KEPT: usize = 65536;

/// The bit the session's output chain adds (`redirect::MARK`) — never a socket's own.
const ROUTE_BIT: u32 = 1;

/// What a mark stands for: the run and program that connected, and what the proxy needs of it.
#[derive(Clone)]
pub struct Owner {
    pub run: u64,
    pub session: u64,
    /// What the report names as the one refused (the program that connected).
    pub process: String,
    pub rules: Arc<Egress>,
    /// Where the run's refusals are said — held weakly: the mark outlives its run, and a strong hold would keep the
    /// run's refusal thread (`sandbox::told`) and its policy for the session's life. While a process of the run (or
    /// one it left) is still served, its supervisor holds it; once none is, nothing of the run can connect.
    pub told: Weak<Sender<Denial>>,
}

impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Owner(run {}, session {}, {})", self.run, self.session, self.process)
    }
}

/// The most programs of one run told apart: past them, the run's further programs share one mark (`others`) — so one
/// run renaming itself in a loop takes at most `PER_RUN` + 1 of the session's `KEPT`, never another run's.
pub const PER_RUN: usize = 64;

/// A mark's key: its run, and its program — None for the run's programs past `PER_RUN`.
type Key = (u64, Option<String>);

#[derive(Default)]
struct Inner {
    next: u32,
    by_mark: HashMap<u32, (Owner, Key)>,
    by_key: HashMap<Key, u32>,
    /// Programs told apart per run (its shared mark not counted).
    per_run: HashMap<u64, usize>,
    order: VecDeque<u32>,
    /// Marks given up so far (the log says the first, then the count at each power of two).
    gone: u64,
}

/// What the report names for a run's programs past `PER_RUN`.
pub fn others(run: u64) -> String {
    format!("other programs of run {run}")
}

/// A session's marks.
#[derive(Default)]
pub struct Marks {
    inner: Mutex<Inner>,
}

impl Marks {
    pub fn new() -> Marks {
        Marks::default()
    }

    /// `owner`'s mark — its run and program's, given at the first connect seen, the same after; past `PER_RUN`
    /// programs of its run, the run's shared one (`others`). None: none left (past 2³¹ of them in one session). A
    /// mark given again whose sender is gone (its run's supervisor ended, and a supervisor of the same run id asks)
    /// takes the asker's owner whole — its rules, session and sender: what it connects or asks is judged by its own
    /// rules and said in its own report, never by the ended one's.
    pub fn mark(&self, mut owner: Owner) -> Option<u32> {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut key = (owner.run, Some(owner.process.clone()));
        if i.per_run.get(&owner.run).copied().unwrap_or(0) >= PER_RUN && !i.by_key.contains_key(&key) {
            key = (owner.run, None);
            owner.process = others(owner.run);
        }
        if let Some(&m) = i.by_key.get(&key) {
            if let Some((kept, _)) = i.by_mark.get_mut(&m)
                && kept.told.strong_count() == 0
            {
                *kept = owner;
            }
            return Some(m);
        }
        let m = i.next.checked_add(2).filter(|m| *m != 0)?;
        i.next = m;
        if i.order.len() == KEPT
            && let Some(old) = i.order.pop_front()
            && let Some((o, old_key)) = i.by_mark.remove(&old)
        {
            i.by_key.remove(&old_key);
            if old_key.1.is_some()
                && let Some(n) = i.per_run.get_mut(&o.run)
            {
                *n -= 1;
                if *n == 0 {
                    i.per_run.remove(&o.run);
                }
            }
            i.gone += 1;
            // (a bounded number of lines however many go: the first, then the count at each power of two)
            if i.gone.is_power_of_two() {
                let what = if i.gone == 1 {
                    format!("run {}'s {} given up", o.run, o.process)
                } else {
                    format!("{} given up so far, the latest run {}'s {}", i.gone, o.run, o.process)
                };
                crate::log::line(
                    "sandbox",
                    "info",
                    &format!(
                        "the session's marks: {KEPT} kept, the oldest go — {what} (a connection still carrying one \
                         is refused)"
                    ),
                );
            }
        }
        if key.1.is_some() {
            *i.per_run.entry(owner.run).or_default() += 1;
        }
        i.by_mark.insert(m, (owner, key.clone()));
        i.by_key.insert(key, m);
        i.order.push_back(m);
        Some(m)
    }

    /// Whose `mark` is — as a socket carries it, or as the planted socket reads it (bit 0 added). None: no mark
    /// tarae gave (0 or the output chain's alone — a socket that never went through the supervisor — or one given up).
    pub fn owner(&self, mark: u32) -> Option<Owner> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.by_mark.get(&(mark & !ROUTE_BIT)).map(|(o, _)| o.clone())
    }

    /// Tests: `run`'s marks kept, with their owners.
    #[cfg(test)]
    pub fn of_run(&self, run: u64) -> Vec<(u32, Owner)> {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.by_mark.iter().filter(|(_, (o, _))| o.run == run).map(|(m, (o, _))| (*m, o.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(run: u64, process: &str) -> Owner {
        let rules = Arc::new(Egress::default());
        Owner { run, session: 7, process: process.into(), rules, told: Weak::new() }
    }

    /// A mark per (run, program), the same each time it's asked: two runs connecting at once are told apart, and so
    /// are two programs of one run; a mark is even and never 0 — bit 0 is the output chain's, and a socket's own mark
    /// with it would meet the route rule at its first lookup (M12: IPv4 then took the destination as its source).
    /// What the planted socket reads (bit 0 added) finds its owner; 0 and the output chain's alone (1) find none.
    #[test]
    fn a_mark_names_its_run_and_program() {
        let marks = Marks::new();
        let (a, b, c) = (owner(1, "cargo"), owner(2, "cargo"), owner(1, "git"));
        let ma = marks.mark(a.clone()).unwrap();
        let mb = marks.mark(b).unwrap();
        let mc = marks.mark(c).unwrap();
        assert_eq!(marks.mark(a).unwrap(), ma, "asked again: the same");
        assert!(ma != mb && ma != mc && mb != mc, "{ma} {mb} {mc}");
        for m in [ma, mb, mc] {
            assert!(m != 0 && m & ROUTE_BIT == 0, "{m:#x}: even, never 0");
        }
        let read = |m: u32| marks.owner(m | ROUTE_BIT).map(|o| (o.run, o.process));
        assert_eq!(read(ma), Some((1, "cargo".into())));
        assert_eq!(read(mb), Some((2, "cargo".into())));
        assert_eq!(read(mc), Some((1, "git".into())));
        assert_eq!(marks.owner(ma).map(|o| o.run), Some(1), "as the socket carries it");
        assert!(marks.owner(0).is_none() && marks.owner(1).is_none(), "no mark tarae gave");
    }

    /// The table is bounded (`KEPT`): past it the oldest mark is given up — a connection carrying it is then one
    /// tarae didn't mark — and the newest are kept; nothing grows past it. What's given up is said in a bounded
    /// number of lines (tarae's log has no cap): the first by its run and program, then the count at each power of
    /// two — five gone, three lines.
    #[test]
    fn the_oldest_mark_goes_past_the_bound() {
        let marks = Marks::new();
        let first = marks.mark(owner(0, "first")).unwrap();
        for run in 1..KEPT as u64 {
            marks.mark(owner(run, "p")).unwrap();
        }
        assert!(marks.owner(first).is_some(), "at the bound: still kept");
        let captured = crate::log::capture();
        let last = marks.mark(owner(KEPT as u64, "p")).unwrap();
        assert!(marks.owner(first).is_none(), "one past: the oldest gone");
        assert!(marks.owner(last).is_some());
        for run in 1..5 {
            marks.mark(owner(KEPT as u64 + run, "p")).unwrap();
        }
        let said: Vec<String> =
            captured.lines().into_iter().filter(|l| l.contains("the session's marks:")).collect();
        assert_eq!(said.len(), 3, "{said:#?}");
        assert!(said[0].contains("run 0's first given up"), "{said:#?}");
        assert!(said[1].contains("2 given up so far, the latest run 1's p"), "{said:#?}");
        assert!(said[2].contains("4 given up so far, the latest run 3's p"), "{said:#?}");
        let i = marks.inner.lock().unwrap();
        assert_eq!((i.by_mark.len(), i.by_key.len(), i.order.len()), (KEPT, KEPT, KEPT));
        assert_eq!(i.per_run.len(), KEPT, "what's given up isn't counted for its run");
    }

    /// A mark given again keeps its owner while that one's sender is there, and takes the asker's whole where it's
    /// gone — a supervisor of the same run id after the first ended: its own rules, session and sender, so what it
    /// connects or asks is judged by its rules and said in its report, never by the ended one's.
    #[test]
    fn a_mark_given_again_takes_a_live_owner_whole() {
        let marks = Marks::new();
        let first = Arc::new(std::sync::mpsc::channel().0);
        let old_rules = Arc::new(Egress::default());
        let new_rules = Arc::new(Egress { any: true, ..Egress::default() });
        let old = Owner { told: Arc::downgrade(&first), rules: old_rules.clone(), ..owner(1, "cargo") };
        let m = marks.mark(old).unwrap();
        let second = Arc::new(std::sync::mpsc::channel().0);
        let new = || Owner {
            told: Arc::downgrade(&second),
            rules: new_rules.clone(),
            session: 8,
            ..owner(1, "cargo")
        };
        assert_eq!(marks.mark(new()), Some(m));
        let kept = marks.owner(m).unwrap();
        assert!(Arc::ptr_eq(&kept.told.upgrade().unwrap(), &first), "live: its sender kept");
        assert!(Arc::ptr_eq(&kept.rules, &old_rules), "live: its rules kept");
        drop(first);
        assert_eq!(marks.mark(new()), Some(m), "the same mark");
        let revived = marks.owner(m).unwrap();
        assert!(Arc::ptr_eq(&revived.told.upgrade().unwrap(), &second), "the asker's sender");
        assert!(Arc::ptr_eq(&revived.rules, &new_rules), "the asker's rules");
        assert_eq!(revived.session, 8, "the asker's session");
    }

    /// One run can't take the session's marks from the others: past `PER_RUN` programs (a run renaming itself in a
    /// loop — `prctl(PR_SET_NAME)` or an exec per name), its further programs share one mark, named as such in what
    /// the report says. Renaming past the whole bound leaves another run's mark where it was.
    #[test]
    fn a_run_renaming_many_times_doesnt_evict_another_runs_mark() {
        let marks = Marks::new();
        let other = marks.mark(owner(1, "cargo")).unwrap();
        let mut seen = std::collections::HashSet::new();
        for n in 0..KEPT + 10 {
            seen.insert(marks.mark(owner(2, &format!("p{n}"))).unwrap());
        }
        assert_eq!(marks.owner(other).map(|o| (o.run, o.process)), Some((1, "cargo".into())));
        assert_eq!(seen.len(), PER_RUN + 1, "its first programs, then one for the rest");
        let shared = marks.mark(owner(2, "yet-another")).unwrap();
        assert_eq!(marks.owner(shared).map(|o| o.process), Some(others(2)));
        assert_eq!(marks.mark(owner(2, "p0")).map(|m| marks.owner(m).unwrap().process), Some("p0".into()));
        assert_eq!(marks.inner.lock().unwrap().order.len(), PER_RUN + 2);
    }
}
