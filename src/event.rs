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
    /// A job's result for a lane (`Tx` makes these): applied in that lane (`Editor::with_lane`).
    For(LaneId, Apply),
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

/// Which lane a result belongs to (`lane.rs`).
pub type LaneId = u64;

/// The queue the editor holds. The sending side is cloned and handed out to threads.
pub struct Queue {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    /// The lane in front — what `sender` and `jobs` tag their results with (set on a switch, and while a
    /// background lane's result is applied).
    lane: std::cell::Cell<LaneId>,
}

impl Default for Queue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, lane: Default::default() }
    }
}

impl Queue {
    /// A sender whose results are applied in the lane in front now, wherever the user is when they land.
    pub fn sender(&self) -> Tx {
        Tx { tx: self.tx.clone(), lane: Some(self.lane.get()) }
    }

    /// A sender whose results are applied as they come, in whatever lane is in front then — for what every
    /// lane shares (language servers, the saver, input, the config watch, plugins, the debugger).
    pub fn shared_sender(&self) -> Tx {
        Tx { tx: self.tx.clone(), lane: None }
    }

    pub fn jobs(&self) -> Jobs {
        Jobs { tx: self.sender() }
    }

    /// Jobs whose results are applied in whatever lane is in front when they land — work about lanes
    /// themselves (making, ending one: `sessions.rs`), never swapped into the lane it's about.
    pub fn shared_jobs(&self) -> Jobs {
        Jobs { tx: self.shared_sender() }
    }

    /// Results made from now on are for `lane`.
    pub fn set_lane(&self, lane: LaneId) {
        self.lane.set(lane);
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

/// The sending side: a job's result sent through it is applied in the lane it was made for (`None`: the
/// one in front when it lands).
#[derive(Clone)]
pub struct Tx {
    tx: Sender<Event>,
    lane: Option<LaneId>,
}

impl Tx {
    pub fn send(&self, ev: Event) -> Result<(), mpsc::SendError<Event>> {
        self.tx.send(match (ev, self.lane) {
            (Event::Job(apply), Some(lane)) => Event::For(lane, apply),
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
}

/// One copy of the thread-spawn machinery for every job, not one per job type.
fn run(job: Box<dyn FnOnce() + Send>) {
    #[cfg(not(test))]
    thread::spawn(job);
    // Tests: the worker carries the test's name (libtest names its threads after them), so a failure on
    // it says which test it ran for
    #[cfg(test)]
    {
        let name = thread::current().name().map(str::to_string);
        let worker = name.map_or_else(thread::Builder::new, |n| thread::Builder::new().name(n));
        worker.spawn(job).expect("failed to spawn thread");
    }
}
