//! The syscall probe against a program that makes a known number of one syscall from a known function
//! (`trace-fixture syscalls`). Needs CAP_BPF and CAP_PERFMON — without them it skips, saying so (`cargo xtask
//! trace test` has them).
#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};

use tarae_trace::Output;
use tarae_trace::attach::{Error, Probe, Target};
use tarae_trace::syscalls::{self, Tracer};
use tarae_trace::sysreport::SyscallReport;

const FIXTURE: &str = env!("CARGO_BIN_EXE_trace-fixture");
const N: u64 = 1000;

/// The fixture traced, with these syscalls' stacks kept — None (said) where the probe isn't permitted.
fn traced(test: &str, stacks: &[&str]) -> Option<(SyscallReport, u32)> {
    let mut t = match Tracer::attach(&Target::default()) {
        Err(Error::NotPermitted(why)) => {
            eprintln!("tarae-test: skipped {test} — {why}");
            return None;
        }
        r => r.unwrap(),
    };
    t.keep_stacks_for(&stacks.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    // (it calls once it has read a line — after it's added)
    let mut p =
        Command::new(FIXTURE).args(["syscalls", &N.to_string()]).stdin(Stdio::piped()).spawn().unwrap();
    t.add(p.id()).unwrap();
    writeln!(p.stdin.take().unwrap()).unwrap();
    let mut status = None;
    t.run_until(|| {
        status = p.try_wait().unwrap();
        status.is_some()
    });
    let r = t.finish("fixture").unwrap();
    assert!(status.unwrap().success());
    assert!(r.lost.values().all(|n| *n == 0), "lost: {:?}\n{}", r.lost, r.summary(10));
    Some((r, p.id()))
}

#[test]
fn a_known_number_of_calls_is_counted_exactly() {
    let Some((r, pid)) = traced("a_known_number_of_calls_is_counted_exactly", &[]) else { return };
    let summary = r.summary(10);
    let nr = syscalls::number("getppid").unwrap();
    let mine: Vec<_> = r.calls.iter().filter(|c| c.nr == nr).collect();
    assert_eq!(mine.iter().map(|c| c.count).sum::<u64>(), N, "getppid calls\n{summary}");
    assert!(mine.iter().all(|c| c.pid == pid && c.failed == 0 && c.name == "getppid"), "{mine:?}");
    // Only the process asked for: the test binary's own calls aren't there; no stacks unless asked
    assert!(r.calls.iter().all(|c| c.pid == pid && c.user.is_empty()), "{summary}");
    assert!(r.folded().is_none());
    eprintln!("{summary}");
}

/// `--stacks getppid`: the same count, now by caller — all of it from the function that makes them; the
/// other syscalls stay without stacks.
#[test]
fn the_calls_of_a_syscall_asked_for_say_where_they_come_from() {
    let test = "the_calls_of_a_syscall_asked_for_say_where_they_come_from";
    let Some((r, _)) = traced(test, &["getppid"]) else { return };
    let summary = r.summary(10);
    let (mine, rest): (Vec<_>, Vec<_>) = r.calls.iter().partition(|c| c.name == "getppid");
    assert_eq!(mine.iter().map(|c| c.count).sum::<u64>(), N, "{summary}");
    let known: u64 = mine
        .iter()
        .filter(|c| c.user.iter().any(|f| f.ends_with("call_in_a_known_place")))
        .map(|c| c.count)
        .sum();
    assert_eq!(known, N, "every call from call_in_a_known_place\n{summary}\n{mine:#?}");
    assert!(rest.iter().all(|c| c.user.is_empty()), "only getppid's stacks kept");
    // (folded: `…;call_in_a_known_place;…;getppid <calls>` — the syscall last, weighed by calls)
    let folded = r.folded().unwrap();
    let in_folded: u64 = folded
        .lines()
        .filter(|l| l.contains("call_in_a_known_place;"))
        .filter_map(|l| l.rsplit_once(";getppid ").map(|(_, n)| n.parse::<u64>().unwrap()))
        .sum();
    assert_eq!(in_folded, N, "{folded}");
    eprintln!("{summary}");
}
