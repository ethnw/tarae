//! The off-CPU probe against a program that waits in a known function for a known time (`trace-fixture`):
//! chosen by comm while it's started, and attached to by pid once it runs — with its children or without.
//! Needs CAP_BPF and CAP_PERFMON — without them it skips, saying so (`cargo xtask trace test` has them).
#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};

use tarae_trace::attach::{Error, Probe, Target};
use tarae_trace::offcpu::Tracer;
use tarae_trace::report::Report;

const FIXTURE: &str = env!("CARGO_BIN_EXE_trace-fixture");
const KNOWN: &str = "wait_in_a_known_place";

/// One test at a time: a probe by comm would see another test's fixtures.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Attached, or None (said) when the probe isn't permitted here.
fn attach(test: &str, target: &Target) -> Option<Tracer> {
    match Tracer::attach(target) {
        Err(Error::NotPermitted(why)) => {
            eprintln!("tarae-test: skipped {test} — {why}");
            None
        }
        r => Some(r.unwrap()),
    }
}

/// The time in stacks of `pid` (any, when None) that pass through the known function, in ms.
fn known_ms(r: &Report, pid: Option<u32>) -> f64 {
    let ns: u64 = r
        .stacks
        .iter()
        .filter(|s| pid.is_none_or(|p| s.pid == p) && s.user.iter().any(|f| f.ends_with(KNOWN)))
        .map(|s| s.ns)
        .sum();
    ns as f64 / 1e6
}

#[test]
fn a_known_wait_shows_up_in_its_function_with_its_time() {
    const WAIT_MS: f64 = 900.0;
    let _one = one_at_a_time();
    let target = Target { comm: Some("trace-fixture".into()), ..Target::default() };
    let Some(mut t) = attach("a_known_wait_shows_up_in_its_function_with_its_time", &target) else { return };
    let mut child = Command::new(FIXTURE).arg((WAIT_MS as u64).to_string()).spawn().unwrap();
    let mut status = None;
    t.run_until(|| {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    let r = t.finish("fixture").unwrap();
    assert!(status.unwrap().success());
    assert!(r.lost.values().all(|n| *n == 0), "lost: {:?}", r.lost);
    let summary = r.summary(10);
    // The waiting thread's off-CPU time: the sleeps, a little over (timer slack, waking up)
    let thread = r.threads().into_iter().find(|t| t.comm == "fixture-wait").expect(&summary);
    let ms = (thread.blocked_ns + thread.preempted_ns) as f64 / 1e6;
    assert!((WAIT_MS..WAIT_MS * 1.1).contains(&ms), "fixture-wait off CPU {ms:.1} ms\n{summary}");
    // …and all of it in stacks that name the function it waits in, under the sleep
    let known = known_ms(&r, None);
    assert!(known >= WAIT_MS, "{known:.1} ms in {KNOWN}\n{summary}");
    let s = r.stacks.iter().find(|s| s.user.iter().any(|f| f.ends_with(KNOWN))).unwrap();
    let at = s.user.iter().position(|f| f.ends_with(KNOWN)).unwrap();
    assert!(s.kernel.iter().any(|f| f.contains("nanosleep")), "the kernel's nanosleep: {s:#?}");
    // The frames under it are std's and glibc's — walked by frame pointers only where those keep them
    if cfg!(target_arch = "aarch64") {
        assert!(s.user[at + 1..].iter().any(|f| f.contains("sleep")), "the sleep under it: {s:#?}");
    } else {
        eprintln!(
            "tarae-test: not checked here — std::thread::sleep under {KNOWN}: on x86_64 std and Debian's \
             glibc keep no frame pointers, so a frame-pointer walk loses them (DWARF unwinding is next)"
        );
    }
    // The main thread waited on the join about as long — in other stacks
    let main = r.threads().into_iter().find(|t| t.comm == "trace-fixture").expect(&summary);
    assert!(main.blocked_ns as f64 / 1e6 >= WAIT_MS * 0.95, "{summary}");
    eprintln!(
        "{summary}  the known wait, root first:\n    {}\n    ⇒ {}",
        s.user.join("\n    "),
        s.kernel.join(" → ")
    );
}

/// `--pid` on a process already running: its waits that start after the attach counted; its children only
/// with `children` — one already running at the attach (seeded from /proc) and one forked after it
/// (followed at the fork). Without `children`, nothing of the child's.
#[test]
fn a_running_process_is_attached_to_and_its_children_followed_when_asked() {
    const WAIT_MS: f64 = 600.0;
    let _one = one_at_a_time();
    for (when, children) in [("later", true), ("now", true), ("later", false)] {
        let mut parent = Command::new(FIXTURE)
            .args(["tree", &(WAIT_MS as u64).to_string(), when])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        // (`now`: the child must be there before the attach — the parent starts it first thing)
        if when == "now" {
            let t0 = std::time::Instant::now();
            while tarae_trace::procs::descendants(&tarae_trace::procs::all(), parent.id()).is_empty() {
                assert!(t0.elapsed().as_secs() < 5, "the fixture's child never started");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let target = Target { pids: vec![parent.id()], children, ..Target::default() };
        let test = "a_running_process_is_attached_to_and_its_children_followed_when_asked";
        let Some(mut t) = attach(test, &target) else {
            parent.kill().ok();
            parent.wait().ok();
            return;
        };
        writeln!(parent.stdin.take().unwrap()).unwrap();
        let mut status = None;
        t.run_until(|| {
            status = parent.try_wait().unwrap();
            status.is_some()
        });
        let r = t.finish("tree").unwrap();
        assert!(status.unwrap().success());
        let summary = format!("({when}, children: {children})\n{}", r.summary(10));
        let pids: std::collections::BTreeSet<u32> = r.stacks.iter().map(|s| s.pid).collect();
        // `later`: the parent's wait for its child began after the attach — counted. (`now`: it began
        // before, and a wait under way at the attach can't be timed: not there.)
        let waited: u64 = r.stacks.iter().filter(|s| s.pid == parent.id()).map(|s| s.ns).sum();
        if when == "later" {
            assert!(waited as f64 / 1e6 >= WAIT_MS * 0.95, "the parent's wait\n{summary}");
        }
        let child = pids.iter().copied().find(|&p| p != parent.id());
        if children {
            assert!(child.is_some() && pids.len() <= 2, "the child (and the parent)\n{summary}");
            let child_known = known_ms(&r, child);
            assert!(child_known >= WAIT_MS, "{child_known:.1} ms in the child's {KNOWN}\n{summary}");
        } else {
            assert_eq!(pids.into_iter().collect::<Vec<_>>(), [parent.id()], "the parent only\n{summary}");
        }
        eprintln!("{summary}");
    }
}

/// Stopping while the traced process waits (Ctrl-C on an idle tarae): the wait under way counts up to the
/// stop, in its own stack.
#[test]
fn a_wait_under_way_at_the_end_counts_up_to_it() {
    let _one = one_at_a_time();
    // (3 × 1000 ms, once it has read a line — the first sleep starts after the attach)
    let mut p = Command::new(FIXTURE).args(["3000", "after-line"]).stdin(Stdio::piped()).spawn().unwrap();
    let target = Target { pids: vec![p.id()], ..Target::default() };
    let Some(mut t) = attach("a_wait_under_way_at_the_end_counts_up_to_it", &target) else {
        p.kill().ok();
        p.wait().ok();
        return;
    };
    writeln!(p.stdin.take().unwrap()).unwrap();
    let t0 = std::time::Instant::now();
    t.run_until(|| t0.elapsed().as_millis() >= 500);
    let r = t.finish("under-way").unwrap();
    p.kill().ok();
    p.wait().ok();
    let summary = r.summary(10);
    let known = known_ms(&r, Some(p.id()));
    assert!(r.under_way >= 1, "{summary}");
    assert!((350.0..=600.0).contains(&known), "{known:.1} ms in {KNOWN}, about 500 so far\n{summary}");
    eprintln!("{summary}");
}
