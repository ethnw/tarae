//! The syscall probe's results: per (thread, syscall) the calls, their time, the longest, the failures —
//! and per thread, what it spends in the kernel by syscall. For the syscalls asked for (`--stacks`), per
//! caller's stack too: where a storm of one syscall comes from.

use std::collections::BTreeMap;

use serde_json::json;

use crate::report::where_user;

/// One (thread, syscall, caller's stack) — the stack empty when it wasn't asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub pid: u32,
    pub tid: u32,
    pub comm: String,
    pub nr: u32,
    pub name: String,
    pub count: u64,
    pub ns: u64,
    pub max_ns: u64,
    pub failed: u64,
    /// The caller's user stack, root first.
    pub user: Vec<String>,
    pub own: Vec<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct SyscallReport {
    pub name: String,
    pub kernel: String,
    pub secs: f64,
    /// The most time first.
    pub calls: Vec<Call>,
    pub lost: BTreeMap<String, u64>,
    pub other_ns: u64,
}

fn ms(ns: u64) -> f64 {
    (ns as f64 / 1e4).round() / 1e2
}

/// One thread's syscalls: how many, how long in all.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadCalls {
    pub pid: u32,
    pub tid: u32,
    pub comm: String,
    pub count: u64,
    pub ns: u64,
}

impl SyscallReport {
    pub fn sorted(mut self) -> Self {
        self.calls.sort_by(|a, b| b.ns.cmp(&a.ns).then(b.count.cmp(&a.count)).then(a.tid.cmp(&b.tid)));
        self
    }

    /// Per thread, the most time in syscalls first.
    pub fn threads(&self) -> Vec<ThreadCalls> {
        let mut by: BTreeMap<(u32, u32), ThreadCalls> = BTreeMap::new();
        for c in &self.calls {
            let t = by.entry((c.pid, c.tid)).or_insert_with(|| ThreadCalls {
                pid: c.pid,
                tid: c.tid,
                comm: c.comm.clone(),
                count: 0,
                ns: 0,
            });
            t.count += c.count;
            t.ns += c.ns;
        }
        let mut v: Vec<ThreadCalls> = by.into_values().collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.ns));
        v
    }

    /// The process-wide mix: per syscall name, over every thread — calls, time, failures.
    pub fn mix(&self) -> Vec<(String, u64, u64, u64)> {
        let mut by: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
        for c in &self.calls {
            let e = by.entry(&c.name).or_default();
            e.0 += c.count;
            e.1 += c.ns;
            e.2 += c.failed;
        }
        let mut v: Vec<_> = by.into_iter().map(|(n, (c, ns, f))| (n.to_string(), c, ns, f)).collect();
        // (the most time first)
        v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        v
    }

    /// One thread's syscalls by name (its stacks added up), the most time first.
    fn of_thread(&self, pid: u32, tid: u32) -> Vec<(&str, u64, u64)> {
        let mut by: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for c in self.calls.iter().filter(|c| c.pid == pid && c.tid == tid) {
            let e = by.entry(&c.name).or_default();
            e.0 += c.count;
            e.1 += c.ns;
        }
        let mut v: Vec<_> = by.into_iter().map(|(n, (c, ns))| (n, c, ns)).collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        v
    }
}

impl crate::Output for SyscallReport {
    fn to_json(&self) -> serde_json::Value {
        let threads: Vec<_> = self
            .threads()
            .iter()
            .map(|t| json!({ "pid": t.pid, "tid": t.tid, "comm": t.comm, "calls": t.count, "ms": ms(t.ns) }))
            .collect();
        let mix: Vec<_> = self
            .mix()
            .iter()
            .map(|(n, c, ns, f)| json!({ "syscall": n, "calls": c, "ms": ms(*ns), "failed": f }))
            .collect();
        let calls: Vec<_> = self
            .calls
            .iter()
            .map(|c| {
                let mut row = json!({ "pid": c.pid, "tid": c.tid, "comm": c.comm, "nr": c.nr,
                    "syscall": c.name, "calls": c.count, "ms": ms(c.ns), "max_ms": ms(c.max_ns),
                    "failed": c.failed });
                if !c.user.is_empty() {
                    row["user"] = json!(c.user);
                    row["own"] = json!(c.own);
                }
                row
            })
            .collect();
        json!({ "probe": "syscalls", "name": self.name, "kernel": self.kernel,
            "secs": (self.secs * 100.0).round() / 100.0, "lost": self.lost, "other_namespaces": self.other_ns,
            "threads": threads, "mix": mix, "calls": calls })
    }

    /// With stacks kept: `comm;user…;syscall calls` — where the calls come from, by count.
    fn folded(&self) -> Option<String> {
        let mut by: BTreeMap<String, u64> = BTreeMap::new();
        for c in self.calls.iter().filter(|c| !c.user.is_empty()) {
            let mut line = vec![c.comm.replace(';', ":")];
            line.extend(c.user.iter().map(|f| f.replace(';', ":").replace(' ', "_")));
            line.push(c.name.clone());
            *by.entry(line.join(";")).or_default() += c.count;
        }
        (!by.is_empty()).then(|| by.into_iter().map(|(l, n)| format!("{l} {n}\n")).collect())
    }

    /// The mix by count and by time, then each thread's top syscalls.
    fn summary(&self, top: usize) -> String {
        let mut s = format!("syscalls {} — {:.1} s\n", self.name, self.secs);
        for (cause, n) in self.lost.iter().filter(|(_, n)| **n > 0) {
            s += &format!("  LOST {n} ({cause}) — the numbers below are short\n");
        }
        if self.other_ns > 0 {
            s += &format!("  ({} events of {} in other containers left out)\n", self.other_ns, self.name);
        }
        let mix = self.mix();
        let total: u64 = mix.iter().map(|m| m.1).sum();
        s += &format!("  mix — {total} calls; by count\n");
        let mut by_count = mix.clone();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (n, c, ns, f) in by_count.iter().take(top) {
            let failed = if *f > 0 { format!("  ({f} failed)") } else { String::new() };
            s += &format!("    {n:<20} {c:>8}×  {:>9.2} ms{failed}\n", ms(*ns));
        }
        s += "  threads — time in syscalls, calls, their top 3 by time\n";
        for t in self.threads() {
            let mine: Vec<String> = self
                .of_thread(t.pid, t.tid)
                .iter()
                .take(3)
                .map(|(n, c, ns)| format!("{n} {:.1} ms/{c}×", ms(*ns)))
                .collect();
            s += &format!(
                "    {:>6} {:<14} {:>9.1} ms {:>7}×  {}\n",
                t.tid,
                t.comm,
                ms(t.ns),
                t.count,
                mine.join(", ")
            );
        }
        let mut stacked: Vec<&Call> = self.calls.iter().filter(|c| !c.user.is_empty()).collect();
        if !stacked.is_empty() {
            stacked.sort_by(|a, b| b.count.cmp(&a.count).then(b.ns.cmp(&a.ns)));
            s += &format!("  where they're called from — top {top} by calls\n");
            for c in stacked.iter().take(top) {
                s += &format!(
                    "    {:>8}× {:>8.2} ms  {:<12} {} {}  {}\n",
                    c.count,
                    ms(c.ns),
                    c.name,
                    c.tid,
                    c.comm,
                    where_user(&c.user, &c.own)
                );
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;

    fn call(tid: u32, name: &str, count: u64, ns: u64, failed: u64) -> Call {
        let comm = "tarae".into();
        Call {
            pid: 1,
            tid,
            comm,
            nr: 0,
            name: name.into(),
            count,
            ns,
            max_ns: ns,
            failed,
            user: vec![],
            own: vec![],
        }
    }

    /// One syscall's calls from two stacks: the thread's line adds them up; the stacks section and the
    /// folded lines tell them apart, by calls.
    #[test]
    fn stacks_of_a_storm() {
        let stack = |frames: &[&str]| frames.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        let mut a = call(7, "mprotect", 64_000, 25_000_000, 0);
        (a.user, a.own) = (stack(&["thread_start", "parse_rope", "ts_parser_parse", "sysmalloc"]), vec![]);
        let mut b = call(7, "mprotect", 300, 100_000, 0);
        (b.user, b.own) = (stack(&["thread_start", "read_file", "sysmalloc"]), vec![]);
        let r = SyscallReport { name: "x".into(), calls: vec![b, a], ..Default::default() }.sorted();
        let s = r.summary(5);
        assert!(s.contains("mprotect 25.1 ms/64300×"), "{s}");
        let stacks = &s[s.find("where they're called from").expect(&s)..];
        let (big, small) = (stacks.find("   64000×").unwrap(), stacks.find("     300×").unwrap());
        assert!(big < small && stacks[big..small].contains("parse_rope"), "{s}");
        let f = r.folded().unwrap();
        assert!(
            f.contains("tarae;thread_start;parse_rope;ts_parser_parse;sysmalloc;mprotect 64000\n"),
            "{f}"
        );
        assert!(SyscallReport::default().folded().is_none(), "no stacks: no folded file");
        assert_eq!(r.to_json()["calls"][0]["user"][1], "parse_rope");
    }

    /// Threads add their syscalls up; the mix adds threads up per syscall; the summary lists both.
    #[test]
    fn threads_and_the_mix() {
        let r = SyscallReport {
            name: "x".into(),
            calls: vec![
                call(1, "futex", 40, 600_000_000, 0),
                call(1, "write", 41, 2_000_000, 0),
                call(2, "read", 100, 9_000_000, 3),
                call(2, "futex", 5, 1_000_000, 0),
            ],
            ..Default::default()
        }
        .sorted();
        assert_eq!(r.calls[0].name, "futex");
        let t = r.threads();
        assert_eq!((t[0].tid, t[0].count, t[0].ns), (1, 81, 602_000_000));
        assert_eq!(r.mix()[0], ("futex".into(), 45, 601_000_000, 0));
        let s = r.summary(5);
        assert!(s.contains("mix — 186 calls; by count\n    read"), "{s}");
        assert!(s.contains("(3 failed)") && s.contains("futex 600.0 ms/40×, write 2.0 ms/41×"), "{s}");
        assert_eq!(r.to_json()["mix"][1]["syscall"], "read");
    }
}
