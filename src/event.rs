//! Event queue — key input, config changes and job results all arrive through one channel.
//!
//! Principle 4 (non-blocking): slow work runs on a thread via `Jobs::spawn`, and the result comes back as
//! "a closure to apply to the editor" that runs on the main loop. The main thread never blocks except
//! while waiting on the channel.
//! Zero dependencies — std threads + mpsc. (An async runtime only if LSP/LLM streaming proves it necessary.)

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::editor::Editor;
use crate::key::Key;

pub type Apply = Box<dyn FnOnce(&mut Editor) + Send>;

pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// Terminal focus (regained = true).
    Focus(bool),
    Resize,
    /// Text pasted into the terminal tarae runs in (bracketed paste) — one event, not a key per character.
    Paste(String),
    Job(Apply),
    /// A job's result for a session (`Tx` makes these): applied in that session (`Editor::with_session`).
    For(SessionId, Apply),
}

/// One mouse action (screen cell coordinates). Press, drag, release, wheel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub x: u16,
    pub y: u16,
    /// alt+press = add a cursor.
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Drag,
    Up,
    ScrollUp,
    ScrollDown,
}

/// Which session a result belongs to (`session.rs`).
pub type SessionId = u64;

/// The queue the editor holds. The sending side is cloned and handed out to threads.
pub struct Queue {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    /// The session in front — what `sender` and `jobs` tag their results with (set on a switch, and while a
    /// background session's result is applied).
    session: std::cell::Cell<SessionId>,
}

impl Default for Queue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, session: Default::default() }
    }
}

impl Queue {
    /// A sender whose results are applied in the session in front now, wherever the user is when they land.
    pub fn sender(&self) -> Tx {
        Tx { tx: self.tx.clone(), session: Some(self.session.get()) }
    }

    /// A sender whose results are applied as they come, in whatever session is in front then — for what every
    /// session shares (language servers, the saver, input, the config watch, plugins, the debugger).
    pub fn shared_sender(&self) -> Tx {
        Tx { tx: self.tx.clone(), session: None }
    }

    pub fn jobs(&self) -> Jobs {
        Jobs { tx: self.sender() }
    }

    /// Jobs whose results are applied in whatever session is in front when they land — work about sessions
    /// themselves (making, ending one: `worktree.rs`), never swapped into the session it's about.
    pub fn shared_jobs(&self) -> Jobs {
        Jobs { tx: self.shared_sender() }
    }

    /// Results made from now on are for `session`.
    pub fn set_session(&self, session: SessionId) {
        self.session.set(session);
    }

    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Like `recv`, but gives up after `d`: Ok(None) when it passed with nothing (Err: the queue is closed).
    pub fn recv_within(&self, d: std::time::Duration) -> Result<Option<Event>, ()> {
        match self.rx.recv_timeout(d) {
            Ok(ev) => Ok(Some(ev)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(()),
        }
    }

    #[cfg(test)]
    pub fn recv_timeout(&self, d: std::time::Duration) -> Option<Event> {
        self.rx.recv_timeout(d).ok()
    }
}

/// The sending side: a job's result sent through it is applied in the session it was made for (`None`: the
/// one in front when it lands).
#[derive(Clone)]
pub struct Tx {
    tx: Sender<Event>,
    session: Option<SessionId>,
}

impl Tx {
    pub fn send(&self, ev: Event) -> Result<(), mpsc::SendError<Event>> {
        self.tx.send(match (ev, self.session) {
            (Event::Job(apply), Some(session)) => Event::For(session, apply),
            (ev, _) => ev,
        })
    }
}

#[derive(Clone)]
pub struct Jobs {
    tx: Tx,
}

impl Jobs {
    /// Runs `work` on a thread and sends its result (a closure to apply to the editor) to the main loop.
    pub fn spawn<W, A>(&self, work: W)
    where
        W: FnOnce() -> A + Send + 'static,
        A: FnOnce(&mut Editor) + Send + 'static,
    {
        let tx = self.tx.clone();
        run(Box::new(move || {
            let apply = work();
            let _ = tx.send(Event::Job(Box::new(apply)));
        }));
    }

    /// Runs `work` on a thread with nothing to bring back (a state file saved, history written): no result, so
    /// the main loop isn't woken for it — a result, even one that does nothing, is an event and a frame.
    pub fn background(&self, work: impl FnOnce() + Send + 'static) {
        run(Box::new(work));
    }
}

/// Lets `x` go on a worker — a big file's parse trees take milliseconds to free (`editor.rs`'s, perf
/// profile: 4.6 ms the main tree, 1.1 ms its layers — and those go at every parse). No result comes back.
pub fn drop_later<T: Send + 'static>(x: T) {
    #[cfg(test)]
    DROPPED_LATER.with(|n| n.set(n.get() + 1));
    run(Box::new(move || drop(x)));
}

#[cfg(test)]
thread_local! {
    /// How many `drop_later`s this thread asked for (tests: what a test's own editor handed off).
    pub static DROPPED_LATER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The bytes of a thread's name Linux keeps (the rest is cut: `top -H` and a profiler see only these).
pub const THREAD_NAME_MAX: usize = 15;

/// A thread name of tarae's fits `THREAD_NAME_MAX` (checked in debug builds, where every thread started says so).
pub fn check_thread_name(name: &str) {
    debug_assert!(
        name.len() <= THREAD_NAME_MAX,
        "{name}: Linux keeps {THREAD_NAME_MAX} bytes of a thread's name"
    );
}

/// A thread of tarae's, by name — `tarae-<what>`, at most `THREAD_NAME_MAX` bytes — so `top -H`, a profiler
/// or `tarae-trace` tells tarae's threads apart (unnamed, every one is called `tarae`).
pub fn thread<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    check_thread_name(name);
    thread::Builder::new().name(name.into()).spawn(f).expect("failed to spawn thread")
}

/// One copy of the thread-spawn machinery for every job, not one per job type.
fn run(job: Box<dyn FnOnce() + Send>) {
    #[cfg(not(test))]
    thread("tarae-job", job);
    // Tests: the worker carries the test's name (libtest names its threads after them), so a failure on
    // it says which test it ran for
    #[cfg(test)]
    {
        let name = thread::current().name().map(str::to_string);
        let worker = name.map_or_else(thread::Builder::new, |n| thread::Builder::new().name(n));
        worker.spawn(job).expect("failed to spawn thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Work with nothing to bring back (a state file saved) sends nothing: the main loop isn't woken, and no
    /// frame is drawn for it — a spawned job's result, even an empty one, is an event.
    #[test]
    fn background_work_sends_nothing_back() {
        let q = Queue::default();
        let (done, ran) = mpsc::channel();
        q.jobs().background(move || {
            let _ = done.send(());
        });
        ran.recv_timeout(Duration::from_secs(5)).expect("it ran");
        assert!(q.recv_timeout(Duration::from_millis(100)).is_none(), "nothing came back");
        q.jobs().spawn(|| |_: &mut Editor| {});
        assert!(q.recv_timeout(Duration::from_secs(5)).is_some(), "a spawned job's result does");
    }

    /// Every thread tarae starts has a name: `thread(…)`, or a `Builder` given one — the e2e test
    /// (`every_thread_tarae_starts_has_a_name`, Linux) sees only the threads its scenario starts. A thread
    /// started from the main thread without one is called `tarae` like it; from another, that one's name.
    /// Scoped threads are left out (they end before their scope returns, inside a named thread's work). And
    /// every name given as `thread("…")` fits `THREAD_NAME_MAX` — the assert only sees threads that start.
    #[test]
    fn every_thread_in_the_code_is_named() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("tests.rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        let (mut unnamed, mut long) = (Vec::new(), Vec::new());
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            // (a file's own tests start at its top-level `#[cfg(test)] mod`)
            let code = text.split("\n#[cfg(test)]\nmod ").next().unwrap_or_default();
            for (i, line) in code.lines().enumerate() {
                let at = || format!("{}:{}", f.strip_prefix(&src).unwrap().display(), i + 1);
                let from = code.lines().skip(i).take(4).collect::<String>();
                let builder = line.contains("thread::Builder::new()") && !from.contains(".name(");
                if line.contains("thread::spawn(") || builder {
                    unnamed.push(at());
                }
                for (_, rest) in line.match_indices("thread(\"").map(|(j, m)| line.split_at(j + m.len())) {
                    let name = rest.split('"').next().unwrap_or_default();
                    if name.len() > THREAD_NAME_MAX {
                        long.push(format!("{}: {name} ({} bytes)", at(), name.len()));
                    }
                }
            }
        }
        assert!(
            unnamed.is_empty(),
            "threads started without a name (event::thread names one):\n{unnamed:#?}"
        );
        assert!(long.is_empty(), "names Linux cuts at {THREAD_NAME_MAX} bytes:\n{long:#?}");
    }
}
