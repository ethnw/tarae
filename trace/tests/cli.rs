//! The `tarae-trace` command against a target that never exits on its own: it must still end — at `--for`,
//! and at SIGTERM without one — and write what it has. (A run once waited 23 min on a tarae that never quit.)
//! Needs CAP_BPF and CAP_PERFMON — without them it skips, saying so (`cargo xtask trace test` has them).
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const TRACE: &str = env!("CARGO_BIN_EXE_tarae-trace");
const FIXTURE: &str = env!("CARGO_BIN_EXE_trace-fixture");
/// Longer than any of these tests needs; a hang is caught at it.
const PATIENCE: Duration = Duration::from_secs(15);

/// A target that won't exit during the test (3 × 20 s of sleep); killed when dropped.
struct Waiting(Child);

impl Waiting {
    fn start() -> Waiting {
        Waiting(Command::new(FIXTURE).arg("60000").spawn().unwrap())
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// An output folder of the test's own, removed when dropped.
struct Out(PathBuf);

impl Out {
    fn new(test: &str) -> Out {
        Out(std::env::temp_dir().join(format!("tarae-trace-cli-{}-{test}", std::process::id())))
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// `tarae-trace` with `args`, its stderr lines on a channel.
fn trace(args: &[&str]) -> (Child, Receiver<String>) {
    let mut c = Command::new(TRACE).args(args).stderr(Stdio::piped()).spawn().unwrap();
    let (tx, rx) = mpsc::channel();
    let err = c.stderr.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(err).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    (c, rx)
}

/// Reads lines into `said` until one contains `what` — false when the output ended or `PATIENCE` passed.
fn wait_line(rx: &Receiver<String>, what: &str, said: &mut Vec<String>) -> bool {
    let end = Instant::now() + PATIENCE;
    while let Ok(l) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        let found = l.contains(what);
        said.push(l);
        if found {
            return true;
        }
    }
    false
}

/// Its exit within `PATIENCE`; a hang is killed and fails the test.
fn exits(c: &mut Child, said: &[String]) -> ExitStatus {
    let end = Instant::now() + PATIENCE;
    while Instant::now() < end {
        if let Some(st) = c.try_wait().unwrap() {
            return st;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    c.kill().ok();
    c.wait().ok();
    panic!("tarae-trace still running after {PATIENCE:?} — it hangs\n{}", said.join("\n"));
}

/// Started and tracing — or None (said) where the probe isn't permitted.
fn started(test: &str, c: &mut Child, rx: &Receiver<String>, said: &mut Vec<String>) -> Option<()> {
    if wait_line(rx, "tarae-trace: tracing", said) {
        return Some(());
    }
    if said.iter().any(|l| l.contains("not permitted")) {
        eprintln!("tarae-test: skipped {test} — {}", said.join(" "));
        c.wait().ok();
        return None;
    }
    c.kill().ok();
    panic!("tarae-trace never started tracing:\n{}", said.join("\n"));
}

#[test]
fn a_target_that_never_exits_ends_at_for_with_what_it_has() {
    let (target, out) = (Waiting::start(), Out::new("for"));
    let pid = target.0.id().to_string();
    let t0 = Instant::now();
    let (mut c, rx) = trace(&["offcpu", "--pid", &pid, "--for", "1s", "--out", out.0.to_str().unwrap()]);
    let mut said = Vec::new();
    let Some(()) = started("a_target_that_never_exits_ends_at_for_with_what_it_has", &mut c, &rx, &mut said)
    else {
        return;
    };
    let st = exits(&mut c, &said);
    said.extend(rx.iter()); // (to its end: the process is gone, its stderr closed)
    assert!(st.success(), "{st}\n{}", said.join("\n"));
    assert!(said.iter().any(|l| l.contains("ended — 1 s are up")), "{}", said.join("\n"));
    assert!(t0.elapsed() < Duration::from_secs(8), "1 s asked, {:?} taken", t0.elapsed());
    let json = out.0.join(format!("offcpu-pid{pid}.json"));
    let r: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(r["probe"], "offcpu");
}

#[test]
fn sigterm_ends_it_with_what_it_has() {
    let (target, out) = (Waiting::start(), Out::new("term"));
    let pid = target.0.id().to_string();
    // (no --for: only its default bound, 10 min, would end it)
    let (mut c, rx) = trace(&["offcpu", "--pid", &pid, "--out", out.0.to_str().unwrap()]);
    let mut said = Vec::new();
    let Some(()) = started("sigterm_ends_it_with_what_it_has", &mut c, &rx, &mut said) else { return };
    assert!(said.iter().any(|l| l.contains("at most 10 min (--for to change)")), "{}", said.join("\n"));
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: a signal to the test's own child.
    assert_eq!(unsafe { libc::kill(c.id() as libc::pid_t, libc::SIGTERM) }, 0);
    let st = exits(&mut c, &said);
    said.extend(rx.iter()); // (to its end: the process is gone, its stderr closed)
    assert!(st.success(), "{st}\n{}", said.join("\n"));
    assert!(said.iter().any(|l| l.contains("ended — signal 15")), "{}", said.join("\n"));
    assert!(out.0.join(format!("offcpu-pid{pid}.json")).is_file(), "{}", said.join("\n"));
    assert!(out.0.join(format!("offcpu-pid{pid}.folded")).is_file());
}
