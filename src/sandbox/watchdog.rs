//! Tests: a watchdog — a test that can hang fails within its own time limit, saying what it waited for, never waits
//! for whoever runs it (6f′: a `copies` test hung for 21 minutes under Docker's default profile, nothing said).
//!
//! It ends the whole test binary (a thread can't fail another thread's test), so it writes straight on fd 2 — no
//! output capture holds it back — and exits 1: run such tests one at a time to see which.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Running while it lives; dropped (the test ended, or panicked), it stops.
pub struct Watchdog {
    waiting: Arc<Mutex<String>>,
    _stop: mpsc::Sender<()>,
}

impl Watchdog {
    /// For test `test`, which must end within `limit`.
    pub fn new(test: &'static str, limit: Duration) -> Watchdog {
        let waiting = Arc::new(Mutex::new("its start".to_string()));
        let (stop, stopped) = mpsc::channel::<()>();
        let at = waiting.clone();
        crate::event::thread("tarae-watchdog", move || {
            if stopped.recv_timeout(limit) != Err(mpsc::RecvTimeoutError::Timeout) {
                return;
            }
            let what = at.lock().map(|w| w.clone()).unwrap_or_default();
            let line = format!(
                "\nWATCHDOG: {test} didn't end within {limit:?} — it was waiting for {what}; the test binary ends here\n"
            );
            // SAFETY: a write of bytes we own on stderr.
            unsafe { libc::write(2, line.as_ptr().cast(), line.len()) };
            std::process::exit(1);
        });
        Watchdog { waiting, _stop: stop }
    }

    /// What the test waits for from here on (said if it never ends).
    pub fn waiting_for(&self, what: &str) {
        if let Ok(mut w) = self.waiting.lock() {
            *w = what.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scene: a test that hangs, its watchdog at 300 ms.
    #[test]
    #[ignore = "run by a_hang_ends_within_its_limit_saying_what_it_waited_for in a process of its own"]
    fn hanging_scene() {
        if std::env::var_os("TARAE_WATCHDOG_SCENE").is_none() {
            return;
        }
        let w = Watchdog::new("hanging_scene", Duration::from_millis(300));
        w.waiting_for("a reply that never comes");
        std::thread::sleep(Duration::from_secs(30));
    }

    /// A test that hangs ends within its watchdog's limit — the binary ended, 1 — saying which and what it waited
    /// for, past libtest's capture; one that ends in time, nothing (its watchdog dropped). The scene in a process of
    /// its own (the watchdog ends the whole binary).
    #[test]
    fn a_hang_ends_within_its_limit_saying_what_it_waited_for() {
        let at = std::time::Instant::now();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sandbox::watchdog::tests::hanging_scene", "--ignored"])
            .env("TARAE_WATCHDOG_SCENE", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(at.elapsed() < Duration::from_secs(10), "ended by its watchdog, not its sleep");
        assert_eq!(out.status.code(), Some(1), "{err}");
        assert!(
            err.contains("WATCHDOG: hanging_scene didn't end within 300ms — it was waiting for a reply that never comes"),
            "{err}"
        );
        let w = Watchdog::new("in time", Duration::from_millis(100));
        drop(w);
        std::thread::sleep(Duration::from_millis(300));
    }
}
