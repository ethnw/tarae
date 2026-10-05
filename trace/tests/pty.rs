//! The terminal-write probe against a write to a pty whose reader stalls a known time (`trace-fixture pty`):
//! the write waits for room about that long, and says where it was written from.
//! Needs CAP_BPF and CAP_PERFMON — without them it skips, saying so (`cargo xtask trace test` has them).
#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};

use tarae_trace::Output;
use tarae_trace::attach::{Error, Probe, Target};
use tarae_trace::ptywrite::Tracer;

const FIXTURE: &str = env!("CARGO_BIN_EXE_trace-fixture");

#[test]
fn a_write_to_a_slow_reader_waits_about_its_stall() {
    const STALL_MS: f64 = 400.0;
    const BYTES: u64 = 4 << 20;
    let mut t = match Tracer::attach(&Target::default()) {
        Err(Error::NotPermitted(why)) => {
            eprintln!("tarae-test: skipped a_write_to_a_slow_reader_waits_about_its_stall — {why}");
            return;
        }
        r => r.unwrap(),
    };
    let mut p = Command::new(FIXTURE)
        .args(["pty", &(STALL_MS as u64).to_string(), &BYTES.to_string()])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    t.add(p.id()).unwrap();
    writeln!(p.stdin.take().unwrap()).unwrap();
    let mut status = None;
    t.run_until(|| {
        status = p.try_wait().unwrap();
        status.is_some()
    });
    let r = t.finish("fixture").unwrap();
    assert!(status.unwrap().success());
    let summary = r.summary(10);
    assert!(r.lost.values().all(|n| *n == 0), "lost: {:?}\n{summary}", r.lost);
    let w = r.threads().into_iter().find(|w| w.comm == "fixture-write").expect(&summary);
    assert_eq!(w.bytes, BYTES, "every byte, written to the terminal\n{summary}");
    let max = w.max_ns as f64 / 1e6;
    assert!(
        (STALL_MS * 0.9..STALL_MS + 1000.0).contains(&max),
        "the write waited {max:.1} ms for a reader stalled {STALL_MS} ms\n{summary}"
    );
    let s = r.writes.iter().find(|s| s.comm == "fixture-write").unwrap();
    // The writing function calls glibc's write (→ __internal_syscall_cancel → __syscall_cancel_arch): found
    // by frame pointers only where glibc keeps them
    if cfg!(target_arch = "aarch64") {
        assert!(s.user.iter().any(|f| f.ends_with("write_in_a_known_place")), "{:#?}", s.user);
    } else {
        eprintln!(
            "tarae-test: not checked here — write_in_a_known_place in the write's stack: on x86_64 glibc's \
             cancellable write keeps no frame pointer, so a frame-pointer walk loses its caller (DWARF \
             unwinding is next)"
        );
    }
    eprintln!("{summary}");
}
