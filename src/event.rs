//! Event queue — key input, config changes and job results all arrive through one channel.
//!
//! Principle 4 (non-blocking): slow work runs on a thread via `Jobs::spawn`, and the result comes back as
//! "a closure to apply to the editor" that runs on the main loop. The main thread never blocks except
//! while waiting on the channel.
//! Zero dependencies — std threads + mpsc. (An async runtime only if LSP/LLM streaming proves it necessary.)

use std::panic::Location;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::editor::Editor;
use crate::key::Key;

pub type Apply = Box<dyn FnOnce(&mut Editor) + Send>;

pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// The mouse moved, no button down (column, row) — not input: it moves nothing but what's under it says
    /// (`presence::hovered`: a session's tag over its mark).
    Hover(u16, u16),
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
    /// ctrl+press = go there: a link followed, a name to its definition, an http(s) address opened as a page (`gf`)
    /// (where the terminal sends it — macOS's
    /// Terminal.app and iTerm2 can make it a right click, which tarae doesn't take).
    pub ctrl: bool,
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
    /// The user's input (its `input_seq`) whose handling is under way — or a job's result it started, through
    /// jobs it started in turn (`Jobs` carries it): what a card it makes was asked by (#205).
    asked: std::cell::Cell<Option<u64>>,
    /// Writes under way the process's end waits for (`Editor::writes_flush`): every `background` job, and what a
    /// spawned job writes that an answer is kept in (`Writes::start`).
    writes: Writes,
    /// The simulation it was made in (`sim.rs`): what its senders send goes there.
    #[cfg(test)]
    sim: Option<crate::sim::Handle>,
}

impl Default for Queue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            session: Default::default(),
            asked: Default::default(),
            writes: Writes::default(),
            #[cfg(test)]
            sim: crate::sim::current(),
        }
    }
}

impl Queue {
    /// A sender whose results are applied in the session in front now, wherever the user is when they land.
    pub fn sender(&self) -> Tx {
        self.tx_for(Some(self.session.get()))
    }

    fn tx_for(&self, session: Option<SessionId>) -> Tx {
        Tx {
            tx: self.tx.clone(),
            session,
            #[cfg(test)]
            sim: self.sim.clone(),
        }
    }

    /// A sender whose results are applied as they come, in whatever session is in front then — for what every
    /// session shares (language servers, the saver, input, the config watch, plugins, the debugger).
    pub fn shared_sender(&self) -> Tx {
        self.tx_for(None)
    }

    pub fn jobs(&self) -> Jobs {
        Jobs { tx: self.sender(), asked: self.asked.get(), writes: self.writes.clone() }
    }

    /// The writes under way the process's end waits for.
    pub fn writes(&self) -> &Writes {
        &self.writes
    }

    /// Jobs whose results are applied in whatever session is in front when they land — work about sessions
    /// themselves (making, ending one: `worktree.rs`), never swapped into the session it's about.
    pub fn shared_jobs(&self) -> Jobs {
        Jobs { tx: self.shared_sender(), asked: self.asked.get(), writes: self.writes.clone() }
    }

    /// Results made from now on are for `session`.
    pub fn set_session(&self, session: SessionId) {
        self.session.set(session);
    }

    /// The user's input that what runs now was asked by (`Queue.asked`) — None: nobody's just now (a task's
    /// output, a server, a timer).
    pub fn asked(&self) -> Option<u64> {
        self.asked.get()
    }

    /// What runs from now on was asked by input `n` (None: by nobody); what it was before.
    pub fn set_asked(&self, n: Option<u64>) -> Option<u64> {
        self.asked.replace(n)
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
    #[cfg(test)]
    sim: Option<crate::sim::Handle>,
}

impl Tx {
    #[track_caller]
    pub fn send(&self, ev: Event) -> Result<(), mpsc::SendError<Event>> {
        self.send_from(ev, Location::caller())
    }

    /// `send`, a job's result named by `site` — where it was sent from, or where the job that sent it was
    /// spawned: what a frame it draws was drawn for (`Editor.event_from`, the state's `frames_for`).
    fn send_from(&self, ev: Event, site: &'static Location<'static>) -> Result<(), mpsc::SendError<Event>> {
        let ev = match ev {
            Event::Job(apply) => Event::Job(Box::new(move |ed: &mut Editor| {
                ed.event_from = Some(crate::editor::DrawnFor::Job(site));
                apply(ed)
            })),
            ev => ev,
        };
        let ev = match (ev, self.session) {
            (Event::Job(apply), Some(session)) => Event::For(session, apply),
            (ev, _) => ev,
        };
        // (in a simulation: its inbox, landed when the seed says — and a sender made outside it, used in it, is
        // refused: what it sends would never land)
        #[cfg(test)]
        if let Some(sim) = &self.sim {
            crate::sim::send(sim, ev);
            return Ok(());
        }
        #[cfg(test)]
        crate::sim::stray();
        self.tx.send(ev)
    }
}

#[derive(Clone)]
pub struct Jobs {
    tx: Tx,
    /// The input it was started under (`Queue.asked`): its result is applied as asked by it too — one-shot
    /// work only; a long-lived sender (a task's output, a server's) carries none.
    asked: Option<u64>,
    writes: Writes,
}

impl Jobs {
    /// Runs `work` on a thread and sends its result (a closure to apply to the editor) to the main loop.
    #[track_caller]
    pub fn spawn<W, A>(&self, work: W)
    where
        W: FnOnce() -> A + Send + 'static,
        A: FnOnce(&mut Editor) + Send + 'static,
    {
        let (tx, asked, site) = (self.tx.clone(), self.asked, Location::caller());
        run(
            site,
            Box::new(move || {
                let apply = work();
                let apply = move |ed: &mut Editor| match asked {
                    None => apply(ed),
                    Some(_) => {
                        let was = ed.events.set_asked(asked);
                        apply(ed);
                        ed.events.set_asked(was);
                    }
                };
                let _ = tx.send_from(Event::Job(Box::new(apply)), site);
            }),
        );
    }

    /// `spawn` for work that writes what's kept (an answer, a store): the process's end waits for the work, as for a
    /// `background` job's — its result isn't waited for (the loop has ended).
    #[track_caller]
    pub fn spawn_kept<W, A>(&self, work: W)
    where
        W: FnOnce() -> A + Send + 'static,
        A: FnOnce(&mut Editor) + Send + 'static,
    {
        let writing = self.writes.start();
        self.spawn(move || {
            let apply = work();
            drop(writing);
            apply
        });
    }

    /// Runs `work` on a thread with nothing to bring back (a state file saved, history written): no result, so
    /// the main loop isn't woken for it — a result, even one that does nothing, is an event and a frame. The
    /// process's end waits for it (`Writes`, `EXIT_WAIT` at most): what it writes isn't cut off by a quit.
    #[track_caller]
    pub fn background(&self, work: impl FnOnce() + Send + 'static) {
        let writing = self.writes.start();
        run(
            Location::caller(),
            Box::new(move || {
                let _writing = writing;
                work()
            }),
        );
    }

    /// `background` the process's end doesn't wait for: a timer, a clean-up a quit may cut short (the start's
    /// prunes, a commit's tree removed) — nothing kept is lost with it.
    #[track_caller]
    pub fn detached(&self, work: impl FnOnce() + Send + 'static) {
        run(Location::caller(), Box::new(work));
    }
}

/// `thread::sleep` for a job's work — a timer, a debounce, a poll. In a simulation (test builds) the job
/// yields until the simulated clock reaches its wake, and other threads run meanwhile.
pub fn sleep(d: Duration) {
    #[cfg(test)]
    if crate::sim::sleep(d) {
        return;
    }
    thread::sleep(d);
}

/// How long the process's end waits for the writes under way (a hung disk isn't waited for forever — what it
/// was writing is lost).
pub const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Writes under way on workers, counted.
#[derive(Clone, Default)]
pub struct Writes(std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>);

/// One write under way — counted until dropped (a worker that panics, too).
pub struct Writing(Writes);

impl Writes {
    /// One more under way, until the returned `Writing` goes.
    pub fn start(&self) -> Writing {
        *self.0.0.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        Writing(self.clone())
    }

    /// Until none is under way, or `within` passes — whether they all finished.
    pub fn wait(&self, within: std::time::Duration) -> bool {
        let (lock, cv) = &*self.0;
        let n = lock.lock().unwrap_or_else(|p| p.into_inner());
        let (n, _) = cv.wait_timeout_while(n, within, |n| *n > 0).unwrap_or_else(|p| p.into_inner());
        *n == 0
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let (lock, cv) = &*self.0.0;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
        cv.notify_all();
    }
}

impl Editor {
    /// Before the process ends (quitting, `:restart`'s exec): the writes still under way land (`Writes`) —
    /// false: some didn't within `EXIT_WAIT`.
    pub fn writes_flush(&self) -> bool {
        self.events.writes().wait(EXIT_WAIT)
    }

    /// What a quit says it didn't write (`flushed`: `writes_flush`'s answer) — every store's writes not landed,
    /// cut off past the wait or failed (trust's, what projects keep, the sessions'), and a take-back of a session's
    /// given before the kept sessions were back.
    pub fn unwritten(&self, flushed: bool) -> Vec<String> {
        let stores: Vec<std::path::PathBuf> = [self.trust_path.clone(), self.allow_path.clone()]
            .into_iter()
            .flatten()
            .chain(self.sessions_path())
            .collect();
        let stores: Vec<&std::path::Path> = stores.iter().map(std::path::PathBuf::as_path).collect();
        let before = self.session_store.taken.iter().map(|(_, w)| format!("the sessions kept without {w}"));
        let said = crate::storewrite::pending_said(&stores).into_iter().chain(before);
        said.map(|said| match flushed {
            true => format!("tarae: not written: {said}"),
            false => format!(
                "tarae: not written before quitting ({} waited): {said}",
                crate::term::run_duration(EXIT_WAIT)
            ),
        })
        .collect()
    }
}

/// Lets `x` go on a worker — a big file's parse trees take milliseconds to free (`editor.rs`'s, perf
/// profile: 4.6 ms the main tree, 1.1 ms its layers — and those go at every parse). No result comes back.
#[track_caller]
pub fn drop_later<T: Send + 'static>(x: T) {
    #[cfg(test)]
    DROPPED_LATER.with(|n| n.set(n.get() + 1));
    run(Location::caller(), Box::new(move || drop(x)));
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
/// In a simulation it's the step's own while it lives (`sim::helper`): refused once it outlives that step.
#[track_caller]
pub fn thread<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    check_thread_name(name);
    #[cfg(test)]
    let f = {
        let helper = crate::sim::helper(Location::caller());
        move || {
            if let Some(h) = &helper {
                h.enter();
            }
            f()
        }
    };
    thread::Builder::new().name(name.into()).spawn(f).expect("failed to spawn thread")
}

/// A scoped thread of tarae's, by name, as `thread`. A scope's own `spawn` starts it unnamed: it shows its
/// creator's name — `tarae` when that's the main thread or a thread still naming itself — and the e2e test
/// (`every_thread_tarae_starts_has_a_name`) caught such ones now and then (1 run in 5 on Debian).
pub fn scoped<'scope, T: Send + 'scope>(
    s: &'scope thread::Scope<'scope, '_>,
    name: &str,
    f: impl FnOnce() -> T + Send + 'scope,
) -> thread::ScopedJoinHandle<'scope, T> {
    check_thread_name(name);
    thread::Builder::new().name(name.into()).spawn_scoped(s, f).expect("failed to spawn thread")
}

/// One copy of the thread-spawn machinery for every job, not one per job type. `site`: where it was spawned —
/// what a simulation's schedule names it by.
fn run(site: &'static Location<'static>, job: Box<dyn FnOnce() + Send>) {
    #[cfg(not(test))]
    {
        let _ = site;
        thread("tarae-job", job);
    }
    // Tests: the worker carries the test's name (libtest names its threads after them), so a failure on
    // it says which test it ran for
    #[cfg(test)]
    if let Some(job) = crate::sim::spawn_job(site, job) {
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
        ran.recv_timeout(crate::test_results::PATIENCE).expect("it ran");
        assert!(q.recv_timeout(Duration::from_millis(100)).is_none(), "nothing came back");
        q.jobs().spawn(|| |_: &mut Editor| {});
        assert!(q.recv_timeout(crate::test_results::PATIENCE).is_some(), "a spawned job's result does");
    }

    /// Waiting ends when the last write under way does — on another thread, or panicking.
    #[test]
    fn waits_for_the_writes_under_way() {
        let writes = Writes::default();
        assert!(writes.wait(Duration::ZERO), "nothing under way");
        let (a, b) = (writes.start(), writes.start());
        assert!(!writes.wait(Duration::from_millis(10)));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(a);
        });
        let _ = std::thread::spawn(move || {
            let _b = b;
            panic!("a worker that panics still counts down");
        })
        .join();
        assert!(writes.wait(Duration::from_secs(5)));
    }

    /// A `background` job holds the process's end until it's done; a `detached` one doesn't.
    #[test]
    fn the_end_waits_for_background_work_not_detached() {
        let q = Queue::default();
        let (go, wait) = mpsc::channel::<()>();
        q.jobs().detached(move || {
            let _ = wait.recv_timeout(crate::test_results::PATIENCE);
        });
        assert!(q.writes().wait(Duration::ZERO), "a detached job held it");
        let (go2, wait2) = mpsc::channel::<()>();
        q.jobs().background(move || {
            let _ = wait2.recv_timeout(crate::test_results::PATIENCE);
        });
        assert!(!q.writes().wait(Duration::from_millis(20)), "a background job didn't hold it");
        let _ = (go.send(()), go2.send(()));
        assert!(q.writes().wait(Duration::from_secs(5)));
    }

    /// Every thread tarae starts has a name: `thread(…)`, or a `Builder` given one — the e2e test
    /// (`every_thread_tarae_starts_has_a_name`, Linux) sees only the threads its scenario starts. A thread
    /// started from the main thread without one is called `tarae` like it; from another, that one's name —
    /// but only once that one has named itself, so a scope's own `spawn` counts as unnamed too (`scoped`
    /// names one: unnamed, they made the e2e test flake). And every name given as `thread("…")` or
    /// `scoped(s, "…")` fits `THREAD_NAME_MAX` — the assert only sees threads that start.
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
            // (the names `thread::scope(|s| …)` gives its scope in this file: `s.spawn(` is an unnamed thread)
            let scopes: Vec<String> = code
                .match_indices("thread::scope(|")
                .filter_map(|(j, m)| {
                    code[j + m.len()..].split('|').next().map(|s| format!("{}.spawn(", s.trim()))
                })
                .collect();
            for (i, line) in code.lines().enumerate() {
                let at = || format!("{}:{}", f.strip_prefix(&src).unwrap().display(), i + 1);
                let from = code.lines().skip(i).take(4).collect::<String>();
                let builder = line.contains("thread::Builder::new()") && !from.contains(".name(");
                let scope_spawn = scopes.iter().any(|s| line.contains(s.as_str()));
                if line.contains("thread::spawn(") || builder || scope_spawn {
                    unnamed.push(at());
                }
                let given = line.match_indices("thread(\"").chain(line.match_indices("scoped(s, \""));
                for (_, rest) in given.map(|(j, m)| line.split_at(j + m.len())) {
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

    /// A job's work waits through `event::sleep` — in a simulation the clock moves instead of the thread
    /// sleeping (`sim.rs`). What still calls `thread::sleep` is on this list, which only shrinks.
    #[test]
    fn every_sleep_in_the_code_goes_through_event_sleep() {
        // (called, or passed as a value)
        let now = crate::sim::lint::counts(&["thread::sleep"], &["event.rs"]);
        crate::sim::lint::only_shrinks("thread::sleep", &now, RAW_SLEEPS);
    }

    /// `thread::sleep` calls left, per file (paths under `src/`).
    const RAW_SLEEPS: &[(&str, usize)] = &[
        ("attach.rs", 3),
        ("builds.rs", 1),
        ("cards.rs", 1),
        ("dap.rs", 4),
        ("disk.rs", 2),
        ("editor.rs", 1),
        ("follow.rs", 1),
        ("fswatch.rs", 1),
        ("git.rs", 1),
        ("gitmenu.rs", 1),
        ("llm.rs", 1),
        ("lsp_editor.rs", 13),
        ("nav.rs", 5),
        ("plugin/cli.rs", 1),
        ("plugin/events.rs", 2),
        ("plugin/io.rs", 1),
        ("procs.rs", 1),
        ("reaper.rs", 1),
        ("rebase.rs", 1),
        ("refactor.rs", 1),
        ("restart.rs", 2),
        ("sandbox.rs", 4),
        ("sandbox/keeper.rs", 1),
        ("sandbox/proxy.rs", 1),
        ("saver.rs", 1),
        ("session.rs", 1),
        ("share.rs", 5),
        ("task.rs", 1),
        ("testing.rs", 1),
        ("tools.rs", 1),
        ("usage.rs", 1),
    ];
}
