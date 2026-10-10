//! tarae's log — `$XDG_STATE_HOME/tarae/tarae.log` (`:log-open`). What language servers print on stderr, when
//! they start, get ready, and exit, what they report (`window/logMessage`, `showMessage`), and every
//! message the editor shows. Callable from any thread: a line goes over a channel to a writer thread,
//! so nothing waits on the disk. The file is started fresh each run (the previous one is kept as `.old`).
//! Tests write nothing.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

pub fn path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("tarae.log"))
}

/// Whether `p` is the log — open, it's followed quietly (a toast about it would be logged, change the file
/// again, and loop).
pub fn is_log(p: Option<&std::path::Path>) -> bool {
    p.is_some_and(|p| path().is_some_and(|l| l == p || std::fs::canonicalize(&l).is_ok_and(|l| l == p)))
}

/// Whether `real` (a real path) is the log itself — Claude reads it unasked (#45: servers' progress and
/// tarae's own messages are there); on a worker (the log's own path is resolved).
pub fn is_log_real(real: &std::path::Path) -> bool {
    let Some(log) = path() else { return false };
    let log = std::fs::canonicalize(&log).ok().or_else(|| crate::paths::real(&log));
    log.is_some_and(|l| l == real)
}

fn sender() -> Option<&'static Sender<String>> {
    static TX: OnceLock<Option<Sender<String>>> = OnceLock::new();
    TX.get_or_init(|| {
        if cfg!(test) {
            return None;
        }
        let path = path()?;
        let (tx, rx) = channel::<String>();
        crate::event::thread("tarae-log", move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::rename(&path, path.with_extension("log.old"));
            let Ok(mut file) = std::fs::File::create(&path) else { return };
            let (y, mo, d, h, m, sec) = now();
            let _ = writeln!(
                file,
                "tarae {} · started {y}-{mo:02}-{d:02} {h:02}:{m:02}:{sec:02}\n",
                env!("CARGO_PKG_VERSION")
            );
            while let Ok(line) = rx.recv() {
                let _ = file.write_all(line.as_bytes());
                // Whatever else arrived meanwhile goes in the same write, then flush once
                while let Ok(more) = rx.try_recv() {
                    let _ = file.write_all(more.as_bytes());
                }
                let _ = file.flush();
            }
        });
        Some(tx)
    })
    .as_ref()
}

/// Tests: the lines `line` was given (every thread's) while any test captures them — tests write no log, so what a
/// line would carry is read here (`capture`) — and the captures under way, each its id and where its lines start.
#[cfg(test)]
struct Captured {
    lines: Vec<String>,
    live: Vec<(u64, usize)>,
    next: u64,
}

#[cfg(test)]
static CAPTURED: std::sync::Mutex<Captured> =
    std::sync::Mutex::new(Captured { lines: Vec::new(), live: Vec::new(), next: 0 });

#[cfg(test)]
fn captured() -> std::sync::MutexGuard<'static, Captured> {
    CAPTURED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tests: every line from here on, until the capture is dropped — a test that panics ends its capture as it
/// unwinds, so none is left under way to keep every later line. Tests running at once share the lines: each reads
/// those since its own start (`Capture::lines`) and picks out its own; with none under way, none are kept.
#[cfg(test)]
pub fn capture() -> Capture {
    let mut c = captured();
    // (with none under way, none are kept — the last to end cleared them: this one starts at the first line)
    let (id, from) = (c.next, c.lines.len());
    c.next += 1;
    c.live.push((id, from));
    Capture { id, from }
}

/// A capture under way (`capture`).
#[cfg(test)]
pub struct Capture {
    id: u64,
    from: usize,
}

#[cfg(test)]
impl Capture {
    /// The lines since it started.
    pub fn lines(&self) -> Vec<String> {
        captured().lines.get(self.from..).unwrap_or_default().to_vec()
    }

    /// Its id — `is_capturing` tells whether it's still under way.
    pub fn id(&self) -> u64 {
        self.id
    }
}

#[cfg(test)]
impl Drop for Capture {
    fn drop(&mut self) {
        let mut c = captured();
        c.live.retain(|(id, _)| *id != self.id);
        if c.live.is_empty() {
            c.lines.clear();
        }
    }
}

/// Tests: whether capture `id` is still under way.
#[cfg(test)]
pub fn is_capturing(id: u64) -> bool {
    captured().live.iter().any(|(i, _)| *i == id)
}

/// Tests: how many lines are kept with no capture under way (none, when the last to end cleared them) — None:
/// one is under way.
#[cfg(test)]
fn kept_while_idle() -> Option<usize> {
    let c = captured();
    c.live.is_empty().then_some(c.lines.len())
}

/// Local (year, month, day, hour, minute, second).
fn now() -> (i32, i32, i32, i32, i32, i32) {
    local(std::time::SystemTime::now())
}

/// `14:02` — when `at` was, local time.
pub fn clock(at: std::time::SystemTime) -> String {
    let (.., h, m, _) = local(at);
    format!("{h:02}:{m:02}")
}

/// `Tue 10:00` — the local weekday and time of `at` (a plan window's reset: within a week).
pub fn day_clock(at: std::time::SystemTime) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let tm = local_tm(at);
    format!("{} {:02}:{:02}", DAYS[tm.tm_wday.rem_euclid(7) as usize], tm.tm_hour, tm.tm_min)
}

/// The local day `at` falls on (year, month, day).
pub fn day(at: std::time::SystemTime) -> (i32, i32, i32) {
    let (y, mo, d, ..) = local(at);
    (y, mo, d)
}

fn local(at: std::time::SystemTime) -> (i32, i32, i32, i32, i32, i32) {
    let tm = local_tm(at);
    (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn local_tm(at: std::time::SystemTime) -> libc::tm {
    let secs = at.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = secs as libc::time_t;
    // SAFETY: localtime_r only writes the `tm` we own; a zeroed `tm` is a valid value to start from.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// One line: `12:03:04  rust-analyzer  error  message` (multi-line text is indented under it).
pub fn line(source: &str, level: &str, text: &str) {
    #[cfg(test)]
    {
        let mut c = captured();
        if !c.live.is_empty() {
            c.lines.push(format!("{source}  {level}  {text}"));
        }
    }
    let Some(tx) = sender() else { return };
    let (.., h, m, sec) = now();
    let text = text.trim_end().replace('\n', &format!("\n{:36}", ""));
    let _ = tx.send(format!("{h:02}:{m:02}:{sec:02}  {source:<16} {level:<8} {text}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reset's day is said by its local weekday: seven days in a row are the seven names, each the one after
    /// the day before's — whatever time zone the tests run in.
    #[test]
    fn a_day_clock_says_the_local_weekday() {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_791_853_200);
        let names: Vec<String> = (0..8u64)
            .map(|d| day_clock(start + std::time::Duration::from_secs(d * 86_400))[..3].to_string())
            .collect();
        let first = DAYS.iter().position(|d| *d == names[0]).expect("a weekday's name");
        for (i, n) in names.iter().enumerate() {
            assert_eq!(n, DAYS[(first + i) % 7], "{names:?}");
        }
        assert_eq!(day_clock(start).len(), "Tue 10:00".len());
    }

    /// Captures under way at once each read their own lines — those since each began — and a test that panics
    /// mid-capture ends its capture as it unwinds: none is left under way keeping every later line, and with none
    /// under way none are kept. (Other tests capture alongside: this one reads only its own tagged lines.)
    #[test]
    fn captures_end_with_their_test_even_one_that_panics() {
        let tag = format!("log-capture-test-{}", std::process::id());
        let mine = |c: &Capture| -> Vec<String> {
            c.lines()
                .into_iter()
                .filter_map(|l| l.split_once(&tag).map(|(_, w)| w.trim().to_string()))
                .collect()
        };
        let a = capture();
        line("test", "info", &format!("{tag} one"));
        let b = capture();
        line("test", "info", &format!("{tag} two"));
        assert_eq!(mine(&a), ["one", "two"], "a: every line since it began");
        assert_eq!(mine(&b), ["two"], "b: only those since it began");
        drop(b);
        line("test", "info", &format!("{tag} three"));
        assert_eq!(mine(&a), ["one", "two", "three"], "b's end isn't a's");
        drop(a);
        let mut id = u64::MAX;
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let c = capture();
            id = c.id();
            line("test", "info", &format!("{tag} panicked"));
            panic!("a test failing mid-capture (expected here)");
        }));
        assert!(failed.is_err());
        assert!(!is_capturing(id), "ended as its test unwound");
        // (with none under way, none kept — what this test's captures saw went with the last to end; unless another
        // test captures right now: then its lines are rightly kept, and this can't be seen)
        match kept_while_idle() {
            Some(kept) => assert_eq!(kept, 0, "lines kept with no capture under way: stale for the next"),
            None => eprintln!(
                "captures_end_with_their_test_even_one_that_panics: another test captures — not seen"
            ),
        }
        let next = capture();
        line("test", "info", &format!("{tag} four"));
        assert_eq!(mine(&next), ["four"], "the next starts with its own");
    }
}
