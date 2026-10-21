//! macOS: the ptys a sandboxed run makes, made by tarae and handed in (frictions29 #23 + #10, terminals A).
//! Seatbelt can't tell a pty a run opened from `/dev/ptmx` from the user's — the same `/dev/ttysN` names,
//! reused — so the run's profile keeps every terminal closed to open, read and write (`TERMINALS`) and allows
//! only ioctl on them: an ioctl needs a descriptor, and a run has none of a terminal but the ones handed in here
//! (measured: a descriptor's read and write are checked at open, its ioctl at every call, by path —
//! `target/sb/frictions31-plan.md`). srt's Linux sandbox gives each run a `/dev` of its own; this is that idea
//! on macOS.
//!
//! A run asks on the socket in its own `RunDir` that `pty::BROKER` names: tarae makes a pair outside the sandbox
//! (`pty::made`) and sends both ends (`SCM_RIGHTS`), its own copies closed at once — the pty hangs up when the
//! run's last copy goes. No card: it makes new ptys only, nothing of the user's (the user's decision,
//! 2026-10-19); the run's report says how many. One thread a run (`tarae-ptys`), blocked in `accept` until the
//! run's end (`sandbox::after_run`) wakes it to go.

use std::collections::HashMap;
use std::io::{self, Read as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The socket's name in the run's folder.
const SOCKET: &str = "pty.sock";

/// How long an asker has to say the size it wants — one that says nothing doesn't hold the run's end up.
const ASK_WITHIN: Duration = Duration::from_secs(1);

/// The ptys a run's broker makes at most — past it an ask is refused (closed with none), counted and said: a
/// run looping on `pty::open` would take the Mac's ~511 and lock the user out of new terminals (the review of
/// terminals A; `pty.rs`'s header has it happen once). This repository's whole e2e suite asks 64.
pub const PTYS_MAX: usize = 128;

/// The brokers of the runs going on, by run id.
static BROKERS: Mutex<Option<HashMap<u64, Broker>>> = Mutex::new(None);

/// A run's broker: its socket, how many ptys it made, its thread.
pub struct Broker {
    path: PathBuf,
    made: Arc<AtomicUsize>,
    refused: Arc<AtomicUsize>,
    over: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// The socket of the broker of the run whose folder is `run`.
pub fn socket(run: &Path) -> PathBuf {
    run.join(SOCKET)
}

/// The broker of run `run_id` started, listening in its folder `run` — before the run starts, so its first ask
/// finds it. Said in the log when it can't be (the run then has no pty, as before).
pub fn start(run_id: u64, run: &Path) {
    start_capped(run_id, run, PTYS_MAX);
}

/// `start`, making at most `cap` (tests: a small one).
fn start_capped(run_id: u64, run: &Path, cap: usize) {
    match Broker::start(run, cap) {
        Ok(b) => {
            let mut all = BROKERS.lock().unwrap_or_else(|e| e.into_inner());
            all.get_or_insert_with(HashMap::new).insert(run_id, b);
        }
        Err(e) => crate::log::line("sandbox", "warning", &format!("a run's pty broker: {e}")),
    }
}

/// Run `run_id` is over (`sandbox::after_run`): its broker stops — how many ptys it made, and how many asks it
/// refused past its cap ((0, 0): none started).
pub fn end(run_id: u64) -> (usize, usize) {
    let b = BROKERS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(&run_id));
    b.map_or((0, 0), Broker::end)
}

/// The runs whose brokers are going (the simulation's check: none left over).
#[cfg(test)]
pub fn going() -> Vec<u64> {
    BROKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(Vec::new(), |m| m.keys().copied().collect())
}

impl Broker {
    fn start(run: &Path, cap: usize) -> io::Result<Broker> {
        let path = socket(run);
        let listener = UnixListener::bind(&path)?;
        let (made, over) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let refused = Arc::new(AtomicUsize::new(0));
        let (m, o, r) = (made.clone(), over.clone(), refused.clone());
        let thread = crate::event::thread("tarae-ptys", move || {
            for conn in listener.incoming() {
                if o.load(Ordering::SeqCst) {
                    break;
                }
                // (past its cap: closed with none — the asker hears "sent no pty"; counted, logged once)
                if m.load(Ordering::SeqCst) >= cap {
                    if r.fetch_add(1, Ordering::SeqCst) == 0 {
                        let said = format!("a run asked for more than {cap} ptys — refused from here");
                        crate::log::line("sandbox", "warning", &said);
                    }
                    continue;
                }
                match conn.map(serve) {
                    Ok(Ok(())) => {
                        m.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(Err(e)) | Err(e) => crate::log::line("sandbox", "info", &format!("a pty asked: {e}")),
                }
            }
        });
        Ok(Broker { path, made, refused, over, thread })
    }

    /// No more ptys: its thread woken (a connect of its own) and gone — how many it made, and refused.
    fn end(self) -> (usize, usize) {
        self.over.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        let _ = self.thread.join();
        (self.made.load(Ordering::SeqCst), self.refused.load(Ordering::SeqCst))
    }
}

/// One ask: its size read (rows, cols, little-endian), a pty made that size and both ends sent — tarae's own
/// copies closed as this returns.
fn serve(mut c: UnixStream) -> io::Result<()> {
    c.set_read_timeout(Some(ASK_WITHIN))?;
    let mut ask = [0u8; 4];
    c.read_exact(&mut ask)?;
    let (rows, cols) = (u16::from_le_bytes([ask[0], ask[1]]), u16::from_le_bytes([ask[2], ask[3]]));
    let (master, slave) = crate::pty::made(rows, cols)?;
    crate::pty::send_fds(c.as_raw_fd(), &[master.as_raw_fd(), slave.as_raw_fd()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::fd::AsRawFd;

    /// A pty asked of a run's broker works as one opened here — its size as asked, what's written on one end
    /// read on the other — and the broker's end says how many it made; after it, nothing answers.
    #[test]
    fn a_runs_broker_hands_in_ptys_and_counts_them() {
        // (the broker makes them here: where no pty opens — a sandbox without one — skipped, said)
        if crate::sandbox::no_pty_here(
            "sandbox::ptybroker::tests::a_runs_broker_hands_in_ptys_and_counts_them",
        ) {
            return;
        }
        let dir = crate::scratch::Scratch::new("ptybroker");
        let run = std::fs::canonicalize(dir.path()).unwrap();
        start(9_000_001, &run);
        let sock = socket(&run);
        for _ in 0..2 {
            let (master, slave) = crate::pty::asked(&sock, 33, 101).unwrap();
            let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: TIOCGWINSZ fills one winsize, a live local.
            assert_eq!(unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws) }, 0);
            assert_eq!((ws.ws_row, ws.ws_col), (33, 101));
            let mut s = std::fs::File::from(slave);
            s.write_all(b"hi\n").unwrap();
            let mut m = std::fs::File::from(master);
            let mut buf = [0u8; 8];
            // (never a read with nothing to come: polled a second first — a hang fails the test)
            let mut p = libc::pollfd { fd: m.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: poll on one live pollfd.
            assert!(unsafe { libc::poll(&mut p, 1, 1000) } > 0, "nothing to read from the master");
            assert!(m.read(&mut buf).unwrap() > 0);
        }
        assert_eq!(end(9_000_001), (2, 0), "two made");
        assert!(crate::pty::asked(&sock, 24, 80).is_err(), "none after the run");
        assert_eq!(end(9_000_001), (0, 0), "ended once");
        // (a descriptor handed in is close-on-exec: never a child's but as the stdio it's given)
        let other = run.join("other");
        std::fs::create_dir(&other).unwrap();
        start(9_000_002, &other);
        let (m, _s) = crate::pty::asked(&socket(&other), 24, 80).unwrap();
        assert_eq!(end(9_000_002), (1, 0));
        // SAFETY: F_GETFD on a live descriptor.
        assert_ne!(unsafe { libc::fcntl(m.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        // The review of terminals A: past its cap a run's asks are refused — none made, each counted
        let capped = run.join("capped");
        std::fs::create_dir(&capped).unwrap();
        start_capped(9_000_003, &capped, 2);
        let got: Vec<bool> = (0..4).map(|_| crate::pty::asked(&socket(&capped), 24, 80).is_ok()).collect();
        assert_eq!(got, [true, true, false, false]);
        assert_eq!(end(9_000_003), (2, 2), "two made, two refused");
    }

    /// terminals A, through Seatbelt: a run with no terminals grant gets a pty from its broker and uses it as a
    /// terminal's program does — sized, written and read, made its controlling terminal (`/dev/tty` then too) —
    /// while opening `/dev/ptmx` or any `/dev/ttys*` itself stays refused; its end says how many it made.
    #[test]
    fn a_run_without_the_grant_uses_the_ptys_its_broker_hands_in() {
        if crate::sandbox::skipped_inside(
            "sandbox::ptybroker::tests::a_run_without_the_grant_uses_the_ptys_its_broker_hands_in",
        ) {
            return;
        }
        let dir = crate::sandbox::tests::scratch("ptybroker-run");
        let root = crate::sandbox::profile_path(&dir);
        let mut p = crate::sandbox::Policy::for_project(&crate::sandbox::tests::env(&root));
        assert!(!p.terminals, "no grant");
        p.run = Some(crate::sandbox::RunDir::new().unwrap());
        let (said, end) = run_helper(&p, &root);
        assert_eq!(
            said,
            "asked=ok size=ok master-size=ok getattr=ok setattr=ok other-ioctl=refused write=ok read=ok ctty=ok \
             sti=refused dev-tty=ok ptmx=refused ttys=refused"
        );
        assert_eq!(end.ptys, 1, "its end says how many");
        // (the review of terminals A: a socket of the user's granted — no broker, no ioctl on terminals: the
        // terminals card, as before)
        let mut socketed = p.clone();
        socketed.sockets = vec![dir.join("granted.sock")];
        socketed.run = Some(crate::sandbox::RunDir::new().unwrap());
        let (said, end) = run_helper(&socketed, &root);
        assert_eq!(said, "asked=refused ptmx=refused ttys=refused");
        assert_eq!(end.ptys, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `helper` as a run in `p` (in `root`), bounded: its `pty:` line and the run's end.
    fn run_helper(p: &crate::sandbox::Policy, root: &Path) -> (String, crate::sandbox::RunEnd) {
        let exe = std::env::current_exe().unwrap();
        let args: Vec<String> = ["--exact", "sandbox::ptybroker::tests::helper", "--ignored", "--nocapture"]
            .map(String::from)
            .to_vec();
        let mut c = crate::sandbox::command(Some(p), &exe.to_string_lossy(), &args);
        c.current_dir(root).stdin(std::process::Stdio::null());
        let ended = crate::sandbox::after_run(Some(p));
        let s = crate::sandbox::spawn(Some(p), &c).unwrap();
        // (never waited on blind: killed after 20 s, and said — a pty's last close can wait for unread output)
        let (pid, done) = (s.pid, Arc::new(AtomicBool::new(false)));
        let watch = done.clone();
        let watchdog = std::thread::spawn(move || {
            let by = std::time::Instant::now() + Duration::from_secs(20);
            while !watch.load(Ordering::SeqCst) && std::time::Instant::now() < by {
                std::thread::sleep(Duration::from_millis(50));
            }
            let late = !watch.load(Ordering::SeqCst);
            if late {
                // SAFETY: SIGKILL to the run's own pid, not yet reaped (`done` unset).
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
            late
        });
        let mut out = String::new();
        std::io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
        done.store(true, Ordering::SeqCst);
        crate::spawn::wait(s.pid).unwrap();
        assert!(!watchdog.join().unwrap(), "the run was killed after 20 s: {out}");
        let end = ended();
        let said = out.lines().find_map(|l| l.strip_prefix("pty: ")).unwrap_or_else(|| panic!("{out}"));
        (said.to_string(), end)
    }

    /// terminals A's proof, by hand (ignored: minutes, and outside a sandbox — Seatbelt can't nest): this
    /// repository's `cargo test --test e2e` as Claude's run of it is made — its policy for this checkout
    /// (`run_policy_in`, `llm.sandbox-localhost` as `.tarae.toml` sets it, no terminals grant), its broker, its
    /// end. Prints the suite's result, the tests that skipped themselves and the ptys its broker made.
    #[test]
    #[ignore = "by hand: terminals A's e2e through the broker (minutes; outside a sandbox)"]
    fn measure_e2e_through_the_broker() {
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let llm = crate::llm::LlmConfig { sandbox_localhost: true, ..Default::default() };
        let here = crate::sandbox::Here::now();
        let p = crate::sandbox::run_policy_in(&root, &llm, &[], &here).unwrap();
        assert!(!p.terminals, "no grant");
        // (`TARAE_MEASURE_ARGS`: another cargo command, its words split by spaces — a shot's)
        let args: Vec<String> = match std::env::var("TARAE_MEASURE_ARGS") {
            Ok(a) => a.split(' ').map(String::from).collect(),
            Err(_) => ["test", "--test", "e2e"].map(String::from).to_vec(),
        };
        let mut c = crate::sandbox::command(Some(&p), "cargo", &args);
        c.current_dir(&root);
        if let Ok(xdg) = std::env::var("TARAE_SHOT_XDG") {
            c.env("TARAE_SHOT_XDG", xdg);
        }
        let ended = crate::sandbox::after_run(Some(&p));
        let s = crate::sandbox::spawn(Some(&p), &c).unwrap();
        let (pid, done) = (s.pid, Arc::new(AtomicBool::new(false)));
        let watch = done.clone();
        std::thread::spawn(move || {
            let by = std::time::Instant::now() + Duration::from_secs(1200);
            while !watch.load(Ordering::SeqCst) && std::time::Instant::now() < by {
                std::thread::sleep(Duration::from_millis(200));
            }
            if !watch.load(Ordering::SeqCst) {
                eprintln!("killed after 20 min");
                // SAFETY: SIGKILL to the run's own pid, not yet reaped.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        });
        let mut err_pipe = s.stderr.unwrap();
        let errs = std::thread::spawn(move || {
            let mut e = String::new();
            let _ = std::io::Read::read_to_string(&mut err_pipe, &mut e);
            e
        });
        let mut out = String::new();
        let _ = std::io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out);
        let err = errs.join().unwrap();
        done.store(true, Ordering::SeqCst);
        crate::spawn::wait(pid).unwrap();
        let end = ended();
        let all = format!("{out}{err}");
        let results: Vec<&str> = all.lines().filter(|l| l.starts_with("test result")).collect();
        let skipped: Vec<&str> =
            all.lines().filter_map(|l| l.strip_prefix(crate::test_results::SELF_SKIP)).collect();
        let failed: Vec<&str> = all.lines().filter(|l| l.ends_with("FAILED")).collect();
        println!("measure: {results:?}");
        println!("measure: failed {failed:?}");
        println!("measure: skipped (output) {}: {skipped:#?}", skipped.len());
        println!("measure: skipped (file) {}, ptys made {}", end.skipped.len(), end.ptys);
        let lines: Vec<&str> = all.lines().collect();
        for (i, l) in lines.iter().enumerate().filter(|(_, l)| l.contains("panicked at")) {
            let n = if l.contains("terminal_panel_runs_the_shell") { 40 } else { 3 };
            let why: Vec<&str> = lines[i + 1..].iter().take(n).copied().collect();
            let cut = if n > 3 { 6000 } else { 400 };
            println!("measure: {l} — {}", why.join("\n").chars().take(cut).collect::<String>());
        }
    }

    /// The run's side of `a_run_without_the_grant_uses_the_ptys_its_broker_hands_in` (ignored: run there, as a
    /// sandboxed run): what it could do, one word each, on its `pty:` line.
    #[test]
    #[ignore = "a sandboxed run's side: a_run_without_the_grant_uses_the_ptys_its_broker_hands_in runs it"]
    fn helper() {
        // (its pty is made its controlling terminal: the master's close hangs it up — this process is the run's
        // own, a child of the test's: SIGHUP ignored so it says what it found)
        // SAFETY: a signal's disposition set to ignore, in a process of its own.
        unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
        // (a deadline of its own too: whatever blocks, it's gone in 15 s — the test's watchdog kills at 20)
        // SAFETY: an alarm for this process, its default action ending it.
        unsafe { libc::alarm(15) };
        let word = |ok: bool| if ok { "ok" } else { "refused" };
        let mut said = Vec::new();
        let pair = crate::pty::open(24, 80);
        said.push(format!("asked={}", word(pair.is_ok())));
        if let Ok((master, slave)) = pair {
            let ws = libc::winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
            // SAFETY: TIOCSWINSZ reads one winsize from a live local.
            let size = unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } == 0;
            said.push(format!("size={}", word(size)));
            // SAFETY: TIOCSWINSZ reads one winsize from a live local — the master's side (the e2e harness resizes so).
            let master_size = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } == 0;
            said.push(format!("master-size={}", word(master_size)));
            // (its modes, as raw mode gets and sets them — and an ioctl not in `TTY_IOCTLS`: TIOCGETD)
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: tcgetattr/tcsetattr on a live descriptor with a live termios.
            let get = unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut t) } == 0;
            said.push(format!("getattr={}", word(get)));
            // SAFETY: as above.
            let set = unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t) } == 0;
            said.push(format!("setattr={}", word(set)));
            let mut disc: libc::c_int = 0;
            // SAFETY: TIOCGETD fills one int, a live local.
            let other = unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGETD as _, &mut disc) } == 0;
            said.push(format!("other-ioctl={}", word(other)));
            let (mut s, m) = (std::fs::File::from(slave), std::fs::File::from(master));
            said.push(format!("write={}", word(s.write_all(b"hi\n").is_ok())));
            // (every read waits a second at most — the first measure's probe hung on one)
            let read_some = |f: &std::fs::File| {
                let mut p = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                let mut buf = [0u8; 64];
                // SAFETY: poll on one live pollfd; read into a live buffer of its length.
                unsafe {
                    libc::poll(&mut p, 1, 1000) > 0
                        && libc::read(f.as_raw_fd(), buf.as_mut_ptr().cast(), 64) > 0
                }
            };
            said.push(format!("read={}", word(read_some(&m))));
            // (a run starts as a session's leader already — `spawn::setsid` — so its setsid may say no: its own
            // session, either way)
            // SAFETY: setsid, then TIOCSCTTY on a live descriptor — this process's own session.
            let ctty = unsafe {
                libc::setsid();
                libc::ioctl(s.as_raw_fd(), libc::TIOCSCTTY as _, 0) == 0
            };
            said.push(format!("ctty={}", word(ctty)));
            // (input pushed into its own controlling terminal: never — not in the rule, and macOS refuses it)
            let c = b'x';
            // SAFETY: TIOCSTI reads one byte from a live local.
            let sti = unsafe { libc::ioctl(s.as_raw_fd(), libc::TIOCSTI as _, &c) } == 0;
            said.push(format!("sti={}", word(sti)));
            // (its controlling terminal by its other name — what a program on a pty opens: measured)
            let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty");
            said.push(format!("dev-tty={}", word(tty.is_ok())));
            // (whatever's left for the master read: a tty's last close waits for its output to be read)
            while read_some(&m) {}
        }
        let ptmx = std::fs::OpenOptions::new().read(true).write(true).open("/dev/ptmx");
        said.push(format!("ptmx={}", word(ptmx.is_ok())));
        let ttys =
            (0..64).any(|n| std::fs::OpenOptions::new().write(true).open(format!("/dev/ttys{n:03}")).is_ok());
        said.push(format!("ttys={}", word(ttys)));
        println!("pty: {}", said.join(" "));
    }
}
