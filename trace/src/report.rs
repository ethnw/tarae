//! A probe's results once named: stacks with their time, per-thread totals, and the two outputs — JSON (what
//! `cargo xtask trace` keeps and compares) and folded stacks (flamegraph.pl / inferno input).

use std::collections::BTreeMap;

use serde_json::json;

/// One (thread, stack, runnable-or-blocked) with the off-CPU time spent there. Frames root → leaf.
#[derive(Clone, Debug, PartialEq)]
pub struct Stack {
    pub pid: u32,
    pub tid: u32,
    /// The thread's own name (tarae names its workers).
    pub comm: String,
    /// Still runnable when switched out — waiting for a CPU, not for something.
    pub preempted: bool,
    pub ns: u64,
    pub count: u64,
    pub max_ns: u64,
    pub user: Vec<String>,
    /// Per user frame, whether it's the program's own code (`own_frame`) — empty when not known.
    pub own: Vec<bool>,
    pub kernel: Vec<String>,
}

/// Off-CPU time of one thread over the run.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub pid: u32,
    pub tid: u32,
    pub comm: String,
    pub blocked_ns: u64,
    pub preempted_ns: u64,
    pub switches: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub probe: String,
    pub name: String,
    pub kernel: String,
    pub secs: f64,
    pub stacks: Vec<Stack>,
    /// What the BPF side couldn't count, by cause — non-zero means the numbers are short.
    pub lost: BTreeMap<String, u64>,
    /// Switches of a program of the same name in another pid namespace (another container's) — left out.
    pub other_ns: u64,
    /// Waits still under way at the end, counted up to it (in `stacks` with the rest).
    pub under_way: u64,
}

fn ms(ns: u64) -> f64 {
    (ns as f64 / 1e4).round() / 1e2
}

impl Report {
    /// The stacks with the same thread and frames merged (different stack ids can name the same frames), the
    /// biggest first.
    pub fn merged(mut self) -> Self {
        // (pid, tid, comm, preempted, user, kernel)
        type Same = (u32, u32, String, bool, Vec<String>, Vec<String>);
        let mut by: BTreeMap<Same, Stack> = BTreeMap::new();
        for s in self.stacks.drain(..) {
            let k = (s.pid, s.tid, s.comm.clone(), s.preempted, s.user.clone(), s.kernel.clone());
            match by.get_mut(&k) {
                Some(m) => {
                    m.ns += s.ns;
                    m.count += s.count;
                    m.max_ns = m.max_ns.max(s.max_ns);
                }
                None => {
                    by.insert(k, s);
                }
            }
        }
        self.stacks = by.into_values().collect();
        self.stacks.sort_by(|a, b| b.ns.cmp(&a.ns).then(a.tid.cmp(&b.tid)));
        self
    }

    /// Per thread, the most off-CPU first.
    pub fn threads(&self) -> Vec<Thread> {
        let mut by: BTreeMap<(u32, u32), Thread> = BTreeMap::new();
        for s in &self.stacks {
            let t = by.entry((s.pid, s.tid)).or_insert_with(|| Thread {
                pid: s.pid,
                tid: s.tid,
                comm: s.comm.clone(),
                blocked_ns: 0,
                preempted_ns: 0,
                switches: 0,
            });
            if s.preempted {
                t.preempted_ns += s.ns;
            } else {
                t.blocked_ns += s.ns;
            }
            t.switches += s.count;
        }
        let mut v: Vec<Thread> = by.into_values().collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.blocked_ns + t.preempted_ns));
        v
    }

    pub fn to_json(&self) -> serde_json::Value {
        let threads: Vec<_> = self
            .threads()
            .iter()
            .map(|t| {
                json!({ "pid": t.pid, "tid": t.tid, "comm": t.comm,
                    "off_cpu_ms": ms(t.blocked_ns + t.preempted_ns), "blocked_ms": ms(t.blocked_ns),
                    "preempted_ms": ms(t.preempted_ns), "switches": t.switches })
            })
            .collect();
        let stacks: Vec<_> = self
            .stacks
            .iter()
            .map(|s| {
                json!({ "pid": s.pid, "tid": s.tid, "comm": s.comm, "preempted": s.preempted,
                    "ms": ms(s.ns), "count": s.count, "max_ms": ms(s.max_ns),
                    "user": s.user, "own": s.own, "kernel": s.kernel })
            })
            .collect();
        json!({ "probe": self.probe, "name": self.name, "kernel": self.kernel,
            "secs": (self.secs * 100.0).round() / 100.0, "lost": self.lost,
            "other_namespaces": self.other_ns, "under_way_at_end": self.under_way,
            "threads": threads, "stacks": stacks })
    }

    /// `comm;[preempted;]user…;kernel… ns` per line — threads of one name together, the ids left out.
    pub fn folded(&self) -> String {
        let mut by: BTreeMap<String, u64> = BTreeMap::new();
        for s in &self.stacks {
            let mut line = vec![s.comm.replace(';', ":")];
            if s.preempted {
                line.push("[preempted]".into());
            }
            line.extend(s.user.iter().chain(&s.kernel).map(|f| f.replace(';', ":").replace(' ', "_")));
            *by.entry(line.join(";")).or_default() += s.ns;
        }
        by.into_iter().map(|(l, ns)| format!("{l} {ns}\n")).collect()
    }

    /// Binaries whose frames came out as `[file+0x…]`, with how many stacks have one.
    pub fn unnamed(&self) -> BTreeMap<String, usize> {
        let mut by: BTreeMap<String, usize> = BTreeMap::new();
        for st in &self.stacks {
            let modules: std::collections::BTreeSet<&str> = st
                .user
                .iter()
                .filter_map(|f| f.strip_prefix('[')?.split_once("+0x").map(|(m, _)| m))
                .collect();
            for m in modules {
                *by.entry(m.to_string()).or_default() += 1;
            }
        }
        by
    }

    /// A few lines for a person: threads (each with where it waited most), the main threads' waits, then the
    /// top stacks of all — each as where it waits.
    pub fn summary(&self, top: usize) -> String {
        let mut s = format!("{} {} — {:.1} s\n", self.probe, self.name, self.secs);
        for (cause, n) in self.lost.iter().filter(|(_, n)| **n > 0) {
            s += &format!("  LOST {n} ({cause}) — the numbers below are short\n");
        }
        if self.other_ns > 0 {
            s += &format!("  ({} switches of {} in other containers left out)\n", self.other_ns, self.name);
        }
        if self.under_way > 0 {
            s += &format!("  ({} waits still under way at the end, counted up to it)\n", self.under_way);
        }
        let unnamed = self.unnamed();
        if !unnamed.is_empty() {
            let list: Vec<String> = unnamed.iter().map(|(m, n)| format!("{m} ({n} stacks)")).collect();
            s += &format!(
                "  frames without names in {} — a stripped binary? A traceable tarae: \
                 RUSTFLAGS='-C force-frame-pointers=yes' cargo build --profile perf\n",
                list.join(", ")
            );
        }
        s += "  threads — off-CPU ms = blocked + preempted, switches, where it waited most\n";
        for t in self.threads() {
            let most = self.stacks.iter().find(|st| st.tid == t.tid && st.pid == t.pid);
            s += &format!(
                "    {:>6} {:<14} {:>9.1} = {:>9.1} + {:>6.1} {:>5}×  {}\n",
                t.tid,
                t.comm,
                ms(t.blocked_ns + t.preempted_ns),
                ms(t.blocked_ns),
                ms(t.preempted_ns),
                t.switches,
                most.map(where_).unwrap_or_default()
            );
        }
        let line = |st: &Stack| {
            format!(
                "    {:>9.1} ms {:>5}× max {:>7.1}  {}{}  {}\n",
                ms(st.ns),
                st.count,
                ms(st.max_ns),
                st.comm,
                if st.preempted { " [preempted]" } else { "" },
                where_(st)
            )
        };
        s += "  main threads' waits\n";
        for st in self.stacks.iter().filter(|st| st.pid == st.tid).take(top) {
            s += &line(st);
        }
        s += &format!("  top {top} waits\n");
        for st in self.stacks.iter().take(top) {
            s += &line(st);
        }
        s
    }
}

/// Kernel frames the scheduler and the probe itself add on top of every wait — not the reason for it.
const KERNEL_PLUMBING: [&str; 12] = [
    "__schedule",
    "schedule",
    "__traceiter_sched_switch",
    "__bpf_trace_sched_switch",
    "bpf_trace_run4",
    "preempt_schedule",
    "preempt_schedule_common",
    "schedule_timeout",
    "schedule_hrtimeout_range",
    "schedule_hrtimeout_range_clock",
    "io_schedule",
    "__cond_resched",
];

/// Source paths of code that isn't the program's own: std/core/alloc (`/rustc/<hash>/library/…`, or the
/// toolchain's rust-src) and dependencies (cargo's registry and git checkouts).
const LIBRARY_SOURCES: [&str; 4] = ["/rustc/", "/rustlib/src/", "/cargo/registry/", "/cargo/git/"];

/// Whether a frame is the program's own code: in its binary (not libc's), and — when the binary says where
/// the code came from — not in std or a dependency's sources; without that, not glue by its name.
/// Inlined frames have short names (`recv`, `save_restore`), so a path in the name can't tell.
pub fn own_frame(name: &str, src: Option<&str>, in_exe: bool) -> bool {
    in_exe
        && match src {
            Some(src) => !LIBRARY_SOURCES.iter().any(|l| src.contains(l)),
            None => !glue(name),
        }
}

/// Frames that are never where a wait comes from, by name (when sources aren't known): the runtime under
/// the program (std, core, alloc, libc's, unnamed `[libc.so.6+0x…]`) and the glue between a thread's start
/// and its code (closures, `call_once`, `spawn`).
fn glue(f: &str) -> bool {
    let f = f.trim_start_matches('<');
    ["std::", "core::", "alloc::", "__", "[", "{closure", "call_once", "spawn"]
        .iter()
        .any(|p| f.starts_with(p))
        || ["_start", "thread_start", "start_thread", "syscall"].contains(&f)
}

/// A frame without its generic arguments: `write_atomic<tarae::…>` → `write_atomic`; a qualified
/// `<std::fs::File>::sync_all` stays.
fn short(f: &str) -> String {
    let (mut out, mut depth, mut prev) = (String::new(), 0, ' ');
    for c in f.chars() {
        match c {
            '<' if depth > 0 || prev.is_alphanumeric() || prev == '_' || prev == '}' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            c => out.push(c),
        }
        prev = c;
    }
    out
}

/// `own → own → own → what it called ⇒ kernel reason`: the innermost three of the program's own frames
/// (`Stack.own`, else not glue by name), the first non-closure frame under them, and the kernel's reason past
/// the scheduler and the probe.
pub fn where_(s: &Stack) -> String {
    let user = where_user(&s.user, &s.own);
    let reason = s
        .kernel
        .iter()
        .rev()
        .find(|f| !KERNEL_PLUMBING.contains(&f.as_str()) && !f.starts_with("bpf_prog_"))
        .cloned()
        .unwrap_or_default();
    format!("{user} ⇒ {reason}")
}

/// `own → own → own → what it called` of a user stack (root first; `own` marks, else glue by name).
pub fn where_user(user: &[String], own: &[bool]) -> String {
    let is_own = |i: usize| if own.len() == user.len() { own[i] } else { !glue(&user[i]) };
    let mine: Vec<usize> = (0..user.len()).filter(|&i| is_own(i)).collect();
    match mine.last() {
        Some(&last) => {
            let mut v: Vec<String> =
                mine[mine.len().saturating_sub(3)..].iter().map(|&i| short(&user[i])).collect();
            let called =
                user[last + 1..].iter().find(|f| !f.starts_with("{closure") && !f.starts_with("call_once"));
            v.extend(called.map(|f| short(f)));
            v.join(" → ")
        }
        None => user.last().map(|f| short(f)).unwrap_or_else(|| "[no user stack]".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(tid: u32, comm: &str, preempted: bool, ns: u64, user: &[&str], kernel: &[&str]) -> Stack {
        Stack {
            pid: 1,
            tid,
            comm: comm.into(),
            preempted,
            ns,
            count: 1,
            max_ns: ns,
            user: user.iter().map(|s| s.to_string()).collect(),
            own: vec![],
            kernel: kernel.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Own code by where it came from: the binary's, not std's or a dependency's sources; libc never; without
    /// sources, by name. Then `where_` goes by those marks (std's inlined `recv` isn't the program's).
    #[test]
    fn own_frames_by_their_sources() {
        let std = "/rustc/0123abcd/library/std/src/sync/mpmc/list.rs";
        let dep =
            "/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/crossterm-0.29.0/src/event.rs";
        assert!(own_frame("event_loop", Some("/work/tarae/src/editor.rs"), true));
        assert!(!own_frame("recv", Some(std), true) && !own_frame("poll", Some(dep), true));
        assert!(!own_frame("__GI___poll", Some("/build/glibc/io/poll.c"), false), "libc's");
        assert!(own_frame("trace_fixture::wait", None, true) && !own_frame("std::thread::sleep", None, true));
        let mut s = stack(
            1,
            "tarae",
            false,
            1,
            &["main", "run", "event_loop", "recv", "with", "try_with", "syscall"],
            &["futex_wait_queue", "schedule"],
        );
        s.own = vec![true, true, true, false, false, false, false];
        assert_eq!(where_(&s), "main → run → event_loop → recv ⇒ futex_wait_queue");
        // A stripped tarae: its frames unnamed, said once per stack
        let stripped = stack(2, "tarae", false, 1, &["[tarae+0x1f00]", "[tarae+0x2a0]", "__GI___poll"], &[]);
        let r = Report { name: "pid1".into(), stacks: vec![s, stripped], ..Default::default() };
        assert_eq!(r.unnamed().into_iter().collect::<Vec<_>>(), [("tarae".to_string(), 1)]);
        assert!(r.summary(5).contains("frames without names in tarae (1 stacks) — a stripped binary?"));
    }

    /// Two stack ids that name the same frames are one stack; threads add up blocked and preempted apart; the
    /// folded lines leave the ids out and sum threads of one name.
    #[test]
    fn stacks_merge_threads_sum_and_fold() {
        let r = Report {
            probe: "offcpu".into(),
            stacks: vec![
                stack(
                    1,
                    "tarae",
                    false,
                    3_000_000,
                    &["main", "tarae::run", "std::sys::poll"],
                    &["do_sys_poll"],
                ),
                stack(
                    1,
                    "tarae",
                    false,
                    2_000_000,
                    &["main", "tarae::run", "std::sys::poll"],
                    &["do_sys_poll"],
                ),
                stack(1, "tarae", true, 500_000, &["main", "tarae::draw"], &[]),
                stack(
                    2,
                    "tarae-jobs",
                    false,
                    9_000_000,
                    &["std::thread", "tarae::jobs::wait"],
                    &["futex_wait"],
                ),
                stack(
                    3,
                    "tarae-jobs",
                    false,
                    1_000_000,
                    &["std::thread", "tarae::jobs::wait"],
                    &["futex_wait"],
                ),
            ],
            ..Default::default()
        }
        .merged();
        assert_eq!(r.stacks.len(), 4, "{:?}", r.stacks);
        assert_eq!((r.stacks[0].tid, r.stacks[0].ns), (2, 9_000_000));
        assert_eq!((r.stacks[1].ns, r.stacks[1].count, r.stacks[1].max_ns), (5_000_000, 2, 3_000_000));
        let t = r.threads();
        assert_eq!((t[0].tid, t[1].tid, t[2].tid), (2, 1, 3));
        assert_eq!((t[1].blocked_ns, t[1].preempted_ns, t[1].switches), (5_000_000, 500_000, 3));
        let f = r.folded();
        assert!(f.contains("tarae-jobs;std::thread;tarae::jobs::wait;futex_wait 10000000\n"), "{f}");
        assert!(f.contains("tarae;[preempted];main;tarae::draw 500000\n"), "{f}");
        let j = r.to_json();
        assert_eq!(j["threads"][1]["off_cpu_ms"], 5.5);
        assert_eq!(j["stacks"][0]["kernel"][0], "futex_wait");
    }

    /// Where a stack waits: the innermost own frame and what it called, then the kernel's reason past the
    /// scheduler and the probe.
    #[test]
    fn where_skips_the_runtime_and_the_scheduler() {
        let s = stack(
            1,
            "tarae",
            false,
            1,
            &["main", "tarae::main_loop", "crossterm::event::poll", "std::sys::pal::unix::poll", "__poll"],
            &[
                "el0_svc",
                "do_sys_poll",
                "schedule_hrtimeout_range",
                "schedule",
                "__schedule",
                "bpf_prog_x_on_switch",
            ],
        );
        assert_eq!(
            where_(&s),
            "main → tarae::main_loop → crossterm::event::poll → std::sys::pal::unix::poll ⇒ do_sys_poll"
        );
        // (as measured: inlined frames with short names, generics, a closure between own code and std)
        let fsync = stack(
            1,
            "tarae",
            false,
            1,
            &[
                "_start",
                "__libc_start_main_impl",
                "main",
                "call_once<fn() -> core::result::Result<(), anyhow::Error>, ()>",
                "save_restore",
                "write_atomic<tarae::restore::{impl#1}::save_restore::{closure_env#6}>",
                "{closure#3}<tarae::restore::{impl#1}::save_restore::{closure_env#6}>",
                "<std::fs::File>::sync_all",
                "<std::sys::fs::unix::File>::fsync",
                "__GI_fsync",
            ],
            &["el0_svc", "jbd2_log_wait_commit", "schedule", "__schedule"],
        );
        assert_eq!(
            where_(&fsync),
            "main → save_restore → write_atomic → <std::fs::File>::sync_all ⇒ jbd2_log_wait_commit"
        );
        let none = stack(1, "x", false, 1, &[], &[]);
        assert_eq!(where_(&none), "[no user stack] ⇒ ");
    }
}
