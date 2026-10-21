//! Deterministic simulation (roadmap §9.2, `target/sb/detsim-plan.md`) — test builds only, nothing of it in a
//! release. Every job keeps its own thread and its own code, but only one thread of a simulation runs at a
//! time: the *baton*. The driver — the test's thread, the editor's main loop — holds it between steps and picks
//! the next by seed: the scenario's next input, a job resumed (just spawned, or its sleep over), a result
//! landed (`handle_event`, as the loop does), or the simulated clock moved to the next wake. A job yields when
//! it ends or sleeps (`event::sleep`); its results wait in the simulation's inbox, in order per sending thread
//! (one thread's sends stay in order, as on a channel) and freely between threads.
//!
//! What the simulation doesn't model is refused, never left to run real: a thread `event::thread` starts is
//! the step's own only while it lives inside that step (joined before the thread that started it yields), and
//! never sends to the loop or sleeps; a send from any thread that isn't the simulation's fails the run.
//!
//! Every decision comes from one stream of choices (`Choices`), made from a seed or read back from a file a
//! failed run wrote (`TARAE_SIM_REPLAY`); past a replayed stream's end every choice is 0 — the oldest first,
//! no wait. A run is checked against itself: the same seed twice gives the same trace.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use crate::editor::Editor;
use crate::event::Event;

mod applies;
mod board;
mod brings;
mod hosts;
mod instructions;
#[cfg(not(target_os = "linux"))]
mod ptybroker;
mod rebasemsg;
mod reviews;
mod sessions;
mod shuttle;
mod stuckstop;
mod watchers;

/// Where a job was spawned (`#[track_caller]` through `Jobs::spawn`, `Editor::step` …) — a schedule's steps
/// are named by it.
pub type Site = &'static Location<'static>;

/// How long one thread may hold the baton — a job's work between two yields — before the run fails: a wait
/// the simulation doesn't see.
const BATON_MAX: Duration = Duration::from_secs(10);

/// How long a thread's yield waits for the threads it started (`event::thread`) to end. A joined one has
/// ended already; a detached one doing finite work (a reaper, a tidy-up) ends within it. One still alive after
/// it is refused — so whether a detached thread happened to finish before the yield never decides the run.
const HELPERS_WAIT: Duration = Duration::from_secs(5);

/// Steps after the last input within which a run must go quiet (nothing to run, land or wake).
const QUIET_WITHIN: usize = 2000;

/// The simulated wall clock starts at 2026-10-18 00:00 UTC.
const EPOCH_S: u64 = 1_792_281_600;

/// Trace lines a failure shows (the last ones).
const TRACE_SHOWN: usize = 40;

/// A simulation, as the threads in it hold it.
#[derive(Clone)]
pub struct Handle(Arc<Shared>);

struct Shared {
    state: Mutex<State>,
    turn: Condvar,
    /// The simulated clock's `Instant` at time 0.
    base: Instant,
}

#[derive(Default)]
struct State {
    /// Simulated time since the start.
    now: Duration,
    /// Jobs by id − 1 (0 is the driver).
    jobs: Vec<Job>,
    /// Who runs: 0 the driver, else a job's id.
    holder: usize,
    /// Results waiting, by the thread that sent them (0: the driver), each with where it was sent from.
    inbox: BTreeMap<usize, VecDeque<(Site, Event)>>,
    /// Threads `event::thread` started that are still alive: (who started them, where, serial).
    helpers: Vec<(usize, Site, u64)>,
    serial: u64,
    /// The first failure (a refusal, a job's panic) — the driver fails the run with it after the step.
    failed: Option<String>,
    /// Over: threads still waiting for their turn go without running.
    ended: bool,
}

struct Job {
    site: Site,
    run: Run,
}

#[derive(Clone, Copy, PartialEq)]
enum Run {
    /// Ready to run from this time on (spawned: now; sleeping: when it wakes).
    Waits(Duration),
    Runs,
    Done,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Role {
    Driver,
    Job(usize),
    /// A thread `event::thread` started from the driver (0) or a job.
    Helper(usize),
}

/// A job's thread unwinding because the simulation ended before its turn.
struct Ended;

thread_local! {
    static CURRENT: RefCell<Option<(Handle, Role)>> = const { RefCell::new(None) };
}

fn current_role() -> Option<(Handle, Role)> {
    CURRENT.with(|c| c.borrow().clone())
}

fn set_role(h: &Handle, role: Role) {
    CURRENT.with(|c| *c.borrow_mut() = Some((h.clone(), role)));
}

impl Handle {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The run fails with `msg` (the first one stands), said by the driver after the step.
    fn fail(&self, msg: String) {
        let mut s = self.lock();
        s.failed.get_or_insert(msg);
        drop(s);
        self.0.turn.notify_all();
    }

    /// At `owner`'s yield: the threads it started have ended, waited for (`HELPERS_WAIT`); one still alive
    /// after that is refused.
    fn helpers_end<'a>(&'a self, mut s: MutexGuard<'a, State>, owner: usize) -> MutexGuard<'a, State> {
        let until = Instant::now() + HELPERS_WAIT;
        while let Some(&(_, site, _)) = s.helpers.iter().find(|h| h.0 == owner) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                s.failed.get_or_insert(format!(
                    "a thread the simulation doesn't model: started at {site}, it outlives the step that started it"
                ));
                break;
            }
            s = self.0.turn.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        s
    }

    /// On job `id`'s thread: waits for the baton (unwinds with `Ended` once the simulation is over).
    fn wait_turn(&self, id: usize) {
        let mut s = self.lock();
        while s.holder != id && !s.ended {
            s = self.0.turn.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if s.holder != id {
            drop(s);
            std::panic::resume_unwind(Box::new(Ended));
        }
    }

    /// Job `id`'s thread: its turn, its work, the baton back.
    fn job(&self, id: usize, site: Site, job: Box<dyn FnOnce() + Send>) {
        set_role(self, Role::Job(id));
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.wait_turn(id);
            job()
        }));
        if let Err(p) = ran
            && !p.is::<Ended>()
        {
            self.fail(format!("job {site} panicked: {}", panic_said(&*p)));
        }
        let mut s = self.helpers_end(self.lock(), id);
        s.jobs[id - 1].run = Run::Done;
        if s.holder == id {
            s.holder = 0;
        }
        drop(s);
        self.0.turn.notify_all();
    }
}

/// `f`, its panic caught: what it said.
fn caught<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| panic_said(&*p))
}

fn panic_said(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".into())
}

/// The simulation this thread is part of, if any.
pub fn current() -> Option<Handle> {
    current_role().map(|c| c.0)
}

/// The simulated clock (`clock::now`), on a thread of a simulation.
pub fn now() -> Option<Instant> {
    let (h, _) = current_role()?;
    let now = h.lock().now;
    Some(h.0.base + now)
}

/// The simulated wall clock (`clock::system`).
pub fn system() -> Option<SystemTime> {
    let (h, _) = current_role()?;
    let now = h.lock().now;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(EPOCH_S) + now)
}

/// `job` as a thread of the simulation (`event::run`) — given back outside one, for the caller to start.
pub fn spawn_job(site: Site, job: Box<dyn FnOnce() + Send>) -> Option<Box<dyn FnOnce() + Send>> {
    let Some((h, role)) = current_role() else { return Some(job) };
    if let Role::Helper(_) = role {
        h.fail(format!("a thread the simulation doesn't model starts a job ({site})"));
        return None;
    }
    let id = {
        let mut s = h.lock();
        if s.ended {
            return None;
        }
        let at = s.now;
        s.jobs.push(Job { site, run: Run::Waits(at) });
        s.jobs.len()
    };
    // (named after the test, as `event::run`'s workers are in tests)
    let name = std::thread::current().name().map(str::to_string);
    let worker = name.map_or_else(std::thread::Builder::new, |n| std::thread::Builder::new().name(n));
    let on = h.clone();
    worker.spawn(move || on.job(id, site, job)).expect("failed to spawn thread");
    None
}

/// `event::sleep` on a thread of a simulation (false outside one): a job yields until the simulated clock
/// reaches its wake; the driver's own wait moves the clock. A helper's sleep fails the run — and is a real
/// one (false), or a helper polling in a loop would spin until its step ends.
pub fn sleep(d: Duration) -> bool {
    let Some((h, role)) = current_role() else { return false };
    match role {
        Role::Driver => h.lock().now += d,
        Role::Helper(_) => {
            h.fail("a thread the simulation doesn't model sleeps".into());
            return false;
        }
        Role::Job(id) => {
            let mut s = h.helpers_end(h.lock(), id);
            let at = s.now + d;
            s.jobs[id - 1].run = Run::Waits(at);
            s.holder = 0;
            drop(s);
            h.0.turn.notify_all();
            h.wait_turn(id);
        }
    }
    true
}

/// A `Tx`'s send in a simulation (`event::Tx::send`): into the inbox, after what this thread sent before.
#[track_caller]
pub fn send(h: &Handle, ev: Event) {
    let role = current_role().filter(|(cur, _)| Arc::ptr_eq(&cur.0, &h.0)).map(|c| c.1);
    let mut s = h.lock();
    if s.ended {
        return;
    }
    let from = match role {
        Some(Role::Driver) => 0,
        Some(Role::Job(id)) => id,
        _ => {
            let name = std::thread::current().name().unwrap_or("unnamed").to_string();
            s.failed
                .get_or_insert(format!("a thread the simulation doesn't model ({name}) sends to the loop"));
            drop(s);
            h.0.turn.notify_all();
            return;
        }
    };
    let site = if from == 0 { Location::caller() } else { s.jobs[from - 1].site };
    s.inbox.entry(from).or_default().push_back((site, ev));
}

/// A send in a simulation through a sender made outside it (`Tx` of a `Queue` made before `Sim::new`): the
/// simulation would run the job and its result never land — the run fails.
pub fn stray() {
    if let Some((h, _)) = current_role() {
        h.fail("a result sent through a queue made outside the simulation (an editor made before `Sim::new`?) — it would never land".into());
    }
}

/// A thread `event::thread` starts in a simulation — the step's own while it lives; `enter` on it.
pub struct Helper {
    h: Handle,
    owner: usize,
    serial: u64,
}

/// The helper for a thread about to start at `site` (`event::thread`), None outside a simulation.
pub fn helper(site: Site) -> Option<Helper> {
    let (h, role) = current_role()?;
    let owner = match role {
        Role::Driver => 0,
        Role::Job(id) | Role::Helper(id) => id,
    };
    let mut s = h.lock();
    s.serial += 1;
    let serial = s.serial;
    s.helpers.push((owner, site, serial));
    drop(s);
    Some(Helper { h, owner, serial })
}

impl Helper {
    /// On the helper's own thread, first thing.
    pub fn enter(&self) {
        set_role(&self.h, Role::Helper(self.owner));
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.h.lock().helpers.retain(|h| h.2 != self.serial);
        // (its starter may be waiting for it to end: `helpers_end`)
        self.h.0.turn.notify_all();
    }
}

/// The stream every decision of a run comes from: made from a seed, or read back (`replay`).
pub struct Choices {
    rng: crate::mutate::Rng,
    given: Option<Vec<u32>>,
    at: usize,
    made: Vec<u32>,
    /// What it was made from: the seed, or the file replayed.
    pub from: String,
}

impl Choices {
    pub fn seeded(seed: u64) -> Choices {
        // (xorshift stays at 0 from 0; the seeds next to each other start apart)
        let rng = crate::mutate::Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        Choices { rng, given: None, at: 0, made: Vec::new(), from: format!("seed {seed}") }
    }

    /// The choices a failed run wrote (one number a line); past their end, every choice is 0.
    pub fn replay(path: &Path) -> Choices {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let given = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
        let rng = crate::mutate::Rng(1);
        Choices { rng, given: Some(given), at: 0, made: Vec::new(), from: path.display().to_string() }
    }

    /// One of `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        let v = match &self.given {
            Some(g) => {
                let v = g.get(self.at).copied().unwrap_or(0) as usize;
                self.at += 1;
                if v < n { v } else { 0 }
            }
            None => self.rng.below(n),
        };
        self.made.push(v as u32);
        v
    }

    pub fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[self.below(of.len())]
    }

    /// An index of `weights`, each as likely as its weight.
    pub fn weighted(&mut self, weights: &[usize]) -> usize {
        let mut at = self.below(weights.iter().sum());
        for (i, w) in weights.iter().enumerate() {
            if at < *w {
                return i;
            }
            at -= w;
        }
        0
    }
}

#[derive(Clone, Copy, Debug)]
enum Next {
    Input,
    Resume(usize),
    Land(usize),
    Wake,
}

/// A simulation on the test's thread, its driver. Dropped: over — threads still waiting go without running.
pub struct Sim {
    h: Handle,
    pub choices: Choices,
    trace: Vec<String>,
    name: String,
    deadline: Instant,
    /// Steps taken, inputs among them.
    pub steps: usize,
    /// Results land as the main loop takes them — a batch of one, then a frame of this size when one is due
    /// (`Sim::batch`); None: `handle_event` alone, the scenario draws its own frames.
    pub frames: Option<(u16, u16)>,
}

impl Sim {
    /// A simulation of scenario `name`, this thread its driver; the run fails past `limit` of real time.
    pub fn new(name: &str, choices: Choices, limit: Duration) -> Sim {
        let h =
            Handle(Arc::new(Shared { state: Mutex::default(), turn: Condvar::new(), base: Instant::now() }));
        CURRENT.with(|c| {
            assert!(c.borrow().is_none(), "a simulation already runs on this thread");
            *c.borrow_mut() = Some((h.clone(), Role::Driver));
        });
        let deadline = Instant::now() + limit;
        Sim { h, choices, trace: Vec::new(), name: name.into(), deadline, steps: 0, frames: None }
    }

    /// `ev` handled as the main loop handles a batch of it alone (`term::event_loop`): then a frame, when one
    /// is due (`term::batch_done`) — at `frames`' size, else none.
    pub fn batch(&self, ed: &mut Editor, ev: Event) {
        let unchanged = crate::term::handle_noting(ed, ev);
        if crate::term::batch_done(ed, unchanged)
            && let Some((w, h)) = self.frames
        {
            crate::term::render(ed, &mut Vec::new(), w, h).unwrap();
        }
    }

    pub fn trace(&self) -> &[String] {
        &self.trace
    }

    /// The simulated clock moves on by `d` (the user took that long).
    pub fn wait(&mut self, d: Duration) {
        self.h.lock().now += d;
    }

    /// Takes steps — `inputs` of the scenario's (`input` makes one and says it) among the jobs' and their
    /// results', each picked by the stream — until it's quiet after the last input; `check` after every step
    /// (Err fails the run), `print` the state a trace line ends with.
    pub fn walk(
        &mut self,
        ed: &mut Editor,
        inputs: usize,
        mut input: impl FnMut(&mut Sim, &mut Editor) -> String,
        check: impl Fn(&Editor) -> Result<(), String>,
        print: impl Fn(&Editor) -> String,
    ) {
        let (mut left, mut after) = (inputs, 0);
        loop {
            if Instant::now() > self.deadline {
                self.fail_now("past its time limit".into());
            }
            let options = self.options(left > 0);
            if options.is_empty() {
                break;
            }
            if left == 0 {
                after += 1;
                if after > QUIET_WITHIN {
                    let pending = self.pending();
                    self.fail_now(format!("not quiet {QUIET_WITHIN} steps after the last input: {pending}"));
                }
            }
            let next = options[self.choices.below(options.len())];
            // (a panic on the loop — the scenario's input, a result landing, `print`, `check` — fails the run as a
            // job's own does: through `fail_now`, with the trace and the choices for a replay)
            let said = match next {
                Next::Input => {
                    left -= 1;
                    match caught(|| input(self, ed)) {
                        Ok(said) => said,
                        Err(p) => self.fail_now(format!("the scenario's input panicked: {p}")),
                    }
                }
                Next::Resume(id) => format!("job {} runs", self.resume(id)),
                Next::Land(from) => {
                    let (site, ev) =
                        self.h.lock().inbox.get_mut(&from).and_then(VecDeque::pop_front).unwrap();
                    let land = |ed: &mut Editor| match self.frames {
                        Some(_) => self.batch(ed, ev),
                        None => ed.handle_event(ev),
                    };
                    if let Err(p) = caught(|| land(ed)) {
                        self.trace.push(format!("lands from {site}"));
                        self.fail_now(format!("a result from {site} panicked as it landed: {p}"));
                    }
                    format!("lands from {site}")
                }
                Next::Wake => {
                    let mut s = self.h.lock();
                    let wake = next_wake(&s).unwrap_or(s.now);
                    let d = wake - s.now;
                    s.now = wake;
                    format!("+{} ms", d.as_millis())
                }
            };
            self.steps += 1;
            let failed = self.h.helpers_end(self.h.lock(), 0).failed.clone();
            if let Some(why) = failed {
                self.trace.push(said);
                self.fail_now(why);
            }
            let shown = match caught(|| print(ed)) {
                Ok(shown) => shown,
                Err(p) => {
                    self.trace.push(said);
                    self.fail_now(format!("print panicked: {p}"));
                }
            };
            self.trace.push(format!("{said} · {shown}"));
            match caught(|| check(ed)) {
                Ok(Ok(())) => {}
                Ok(Err(why)) => self.fail_now(format!("invariant: {why}")),
                Err(p) => self.fail_now(format!("the check panicked: {p}")),
            }
        }
    }

    fn options(&self, inputs: bool) -> Vec<Next> {
        let s = self.h.lock();
        let mut v = Vec::new();
        if inputs {
            v.push(Next::Input);
        }
        for (i, j) in s.jobs.iter().enumerate() {
            if matches!(j.run, Run::Waits(at) if at <= s.now) {
                v.push(Next::Resume(i + 1));
            }
        }
        v.extend(s.inbox.iter().filter(|q| !q.1.is_empty()).map(|q| Next::Land(*q.0)));
        if next_wake(&s).is_some() {
            v.push(Next::Wake);
        }
        v
    }

    /// What's left to run, land or wake — for a failure's message.
    fn pending(&self) -> String {
        let s = self.h.lock();
        let mut v: Vec<String> = s
            .jobs
            .iter()
            .filter_map(|j| match j.run {
                Run::Waits(at) => Some(format!("job {} at {} ms", j.site, at.as_millis())),
                _ => None,
            })
            .collect();
        v.extend(s.inbox.values().flatten().map(|(site, _)| format!("a result from {site}")));
        v.join(", ")
    }

    /// Job `id` runs until it yields; its site.
    fn resume(&mut self, id: usize) -> Site {
        let mut s = self.h.lock();
        s.jobs[id - 1].run = Run::Runs;
        s.holder = id;
        let site = s.jobs[id - 1].site;
        self.h.0.turn.notify_all();
        let until = Instant::now() + BATON_MAX;
        while s.holder != 0 {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(s);
                let why = format!(
                    "job {site} held the baton past {BATON_MAX:?} — a wait the simulation doesn't see"
                );
                self.fail_now(why);
            }
            s = self.h.0.turn.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        site
    }

    /// The run fails: its choices written for a replay, the trace's end and how to replay it said.
    fn fail_now(&mut self, why: String) -> ! {
        let file = self.choices_file();
        let body: String = self.choices.made.iter().map(|c| format!("{c}\n")).collect();
        let kept = std::fs::create_dir_all(file.parent().unwrap()).and_then(|_| std::fs::write(&file, body));
        let replay = match kept {
            Ok(()) => format!("replay: TARAE_SIM_REPLAY={} cargo test {}", file.display(), self.name),
            Err(e) => format!("(its choices not written: {e})"),
        };
        let from = self.trace.len().saturating_sub(TRACE_SHOWN);
        let trace: String =
            self.trace[from..].iter().enumerate().map(|(i, l)| format!("{:>5} {l}\n", from + i)).collect();
        panic!(
            "simulation {} ({}) failed after {} steps: {why}\n{trace}{replay}",
            self.name,
            self.choices.from,
            self.trace.len()
        );
    }

    fn choices_file(&self) -> PathBuf {
        let tag: String = self.choices.from.chars().filter(char::is_ascii_alphanumeric).collect();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/sim");
        dir.join(format!("{}-{}.choices", self.name, tag.trim_start_matches("seed")))
    }
}

fn next_wake(s: &State) -> Option<Duration> {
    s.jobs
        .iter()
        .filter_map(|j| match j.run {
            Run::Waits(at) if at > s.now => Some(at),
            _ => None,
        })
        .min()
}

impl Drop for Sim {
    fn drop(&mut self) {
        CURRENT.with(|c| c.borrow_mut().take());
        let mut s = self.h.lock();
        s.ended = true;
        s.holder = 0;
        drop(s);
        self.h.0.turn.notify_all();
    }
}

/// A scenario's seeds run twice each, the two traces the same — else the first step they part at fails it.
/// `TARAE_SIM_REPLAY` runs that file's choices once instead; `TARAE_SIM_SEEDS=N` runs seeds `0..N` (the long
/// runs), saying the steps a second; `TARAE_SIM_TRACE=1` prints each seed's trace (with `--nocapture`).
pub fn seeds(name: &str, fixed: &[u64], run: impl Fn(Choices) -> Vec<String>) {
    if let Ok(file) = std::env::var("TARAE_SIM_REPLAY") {
        let trace = run(Choices::replay(Path::new(&file)));
        eprintln!("simulation {name}, {file}:\n{}", trace.join("\n"));
        return;
    }
    let many: Option<u64> = std::env::var("TARAE_SIM_SEEDS").ok().and_then(|n| n.parse().ok());
    let seeds: Vec<u64> = many.map_or_else(|| fixed.to_vec(), |n| (0..n).collect());
    let began = Instant::now();
    let mut steps = 0;
    for &seed in &seeds {
        let one = run(Choices::seeded(seed));
        if std::env::var_os("TARAE_SIM_TRACE").is_some() {
            eprintln!("simulation {name}, seed {seed}:\n{}", one.join("\n"));
        }
        let two = run(Choices::seeded(seed));
        if let Some(i) = (0..one.len().max(two.len())).find(|&i| one.get(i) != two.get(i)) {
            panic!(
                "simulation {name} (seed {seed}) isn't deterministic: step {i}\n  first:  {}\n  second: {}\n(before it: {})",
                one.get(i).map_or("(ended)", String::as_str),
                two.get(i).map_or("(ended)", String::as_str),
                i.checked_sub(1).and_then(|j| one.get(j)).map_or("(the start)", String::as_str)
            );
        }
        steps += one.len() * 2;
    }
    if many.is_some() {
        let secs = began.elapsed().as_secs_f64();
        eprintln!(
            "simulation {name}: {} seeds, {steps} steps in {secs:.1} s, {:.0} steps/s",
            seeds.len(),
            steps as f64 / secs
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's real time at most, in these tests.
    const LIMIT: Duration = Duration::from_secs(30);

    /// `n` jobs spawned at once, each result noting its number as it lands: the order they landed in, and
    /// the run's trace.
    fn landed(choices: Choices, n: usize) -> (Vec<usize>, Vec<String>) {
        let mut sim = Sim::new("landed", choices, LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..n {
            let order = order.clone();
            ed.events.jobs().spawn(move || move |_: &mut Editor| order.lock().unwrap().push(i));
        }
        sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
        let trace = sim.trace().to_vec();
        drop(sim);
        let order = order.lock().unwrap().clone();
        (order, trace)
    }

    /// Which result lands first is the seed's: the same seed, the same order (and trace); seeds apart, orders
    /// apart; every result lands once.
    #[test]
    fn the_seed_decides_which_result_lands_next() {
        let mut orders = std::collections::BTreeSet::new();
        for seed in 0..12 {
            let (one, trace) = landed(Choices::seeded(seed), 4);
            assert_eq!(landed(Choices::seeded(seed), 4), (one.clone(), trace), "seed {seed}");
            let mut all = one.clone();
            all.sort();
            assert_eq!(all, [0, 1, 2, 3], "seed {seed}: each once");
            orders.insert(one);
        }
        assert!(orders.len() > 3, "orders by seed: {orders:?}");
    }

    /// A recorded stream replays the same run; past its end every choice is 0 — the oldest first.
    #[test]
    fn a_recorded_stream_replays_the_run() {
        let mut choices = Choices::seeded(5);
        let firsts: Vec<usize> = (0..6).map(|_| choices.below(4)).collect();
        let dir = crate::scratch::Scratch::new("sim-replay");
        let file = dir.join("5.choices");
        std::fs::write(&file, choices.made.iter().map(|c| format!("{c}\n")).collect::<String>()).unwrap();
        let mut again = Choices::replay(&file);
        assert_eq!((0..6).map(|_| again.below(4)).collect::<Vec<_>>(), firsts);
        assert_eq!((again.below(4), again.below(9)), (0, 0), "past its end");
        let mut empty = Choices::replay(&dir.join("5.choices"));
        empty.given = Some(Vec::new());
        assert_eq!(landed(empty, 3).0, [0, 1, 2], "all zeros: in the order they were spawned");
    }

    /// A job's sleep moves the simulated clock, not the thread: its result lands once the clock is past it,
    /// and `clock::now` on both sides of it says exactly that long.
    #[test]
    fn a_jobs_sleep_is_the_simulated_clocks() {
        let mut sim = Sim::new("sleep", Choices::seeded(1), LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        let took = Arc::new(Mutex::new(None));
        let at = crate::clock::now();
        let seen = took.clone();
        ed.events.jobs().spawn(move || {
            let t = crate::clock::now();
            crate::event::sleep(Duration::from_secs(600));
            let slept = crate::clock::since(t);
            move |_: &mut Editor| *seen.lock().unwrap() = Some(slept)
        });
        let began = Instant::now();
        sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
        assert_eq!(*took.lock().unwrap(), Some(Duration::from_secs(600)));
        assert_eq!(crate::clock::since(at), Duration::from_secs(600));
        assert!(began.elapsed() < Duration::from_secs(5), "no real sleep");
        assert!(sim.trace().iter().any(|l| l.starts_with("+600000 ms")), "{:?}", sim.trace());
        drop(sim);
        assert!(crate::clock::since(at) < Duration::from_secs(600), "the real clock again");
    }

    /// A thread started in a job that's still alive `HELPERS_WAIT` after the job yields isn't the simulation's
    /// to schedule: the run fails, naming where it was started.
    #[test]
    #[should_panic(expected = "outlives the step that started it")]
    fn a_thread_that_outlives_its_step_is_refused() {
        let mut sim = Sim::new("outlives", Choices::seeded(1), LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        // (it waits on the test's sender — dropped as the test unwinds, so it ends)
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        ed.events.jobs().background(move || {
            crate::event::thread("tarae-sim-left", move || rx.recv_timeout(Duration::from_secs(60)));
        });
        sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
        drop(tx);
    }

    /// A thread that lives within its step is the step's own — joined, or left to end on its own a moment
    /// later (a reaper): allowed, whichever ended first.
    #[test]
    fn a_thread_that_lives_within_its_step_is_its_own() {
        let mut sim = Sim::new("joined", Choices::seeded(1), LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        let got = Arc::new(Mutex::new(0));
        let seen = got.clone();
        ed.events.jobs().spawn(move || {
            let n = crate::event::thread("tarae-sim-own", || 41).join().unwrap() + 1;
            move |_: &mut Editor| *seen.lock().unwrap() = n
        });
        ed.events.jobs().background(|| {
            crate::event::thread("tarae-sim-late", || std::thread::sleep(Duration::from_millis(200)));
        });
        sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
        assert_eq!(*got.lock().unwrap(), 42);
    }

    /// A thread that isn't the simulation's sending to the loop fails the run.
    #[test]
    #[should_panic(expected = "(tarae-sim-out) sends to the loop")]
    fn a_send_from_outside_the_simulation_is_refused() {
        let mut sim = Sim::new("outside", Choices::seeded(1), LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        let tx = ed.events.sender();
        let out = std::thread::Builder::new().name("tarae-sim-out".into());
        let sent = out.spawn(move || tx.send(Event::Job(Box::new(|_: &mut Editor| {})))).unwrap();
        sent.join().unwrap().unwrap();
        sim.walk(&mut ed, 1, |_, _| "a step".into(), |_| Ok(()), |_| String::new());
    }

    /// A job's panic fails the run with where the job was spawned.
    #[test]
    #[should_panic(expected = "panicked: the job's own")]
    fn a_jobs_panic_fails_the_run() {
        let mut sim = Sim::new("panic", Choices::seeded(1), LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        ed.events.jobs().background(|| panic!("the job's own"));
        sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
    }

    /// Review note 9: an editor made before its simulation — its queue outside it — would have its jobs run by
    /// the simulation and their results sent where nothing reads them, the run quiet: the first such result fails
    /// it instead.
    #[test]
    #[should_panic(expected = "a result sent through a queue made outside the simulation")]
    fn a_queue_made_outside_the_simulation_is_refused() {
        let mut ed = Editor::new(crate::config::Config::default());
        let mut sim = Sim::new("outside_queue", Choices::seeded(1), LIMIT);
        ed.events.jobs().spawn(|| |_: &mut Editor| {});
        sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
    }

    /// Second review 4: a panic on the loop — here a job's result as it lands — fails the run as a job's own
    /// panic does: its seed and trace said, its choices written for a replay.
    #[test]
    fn a_panic_on_the_loop_fails_the_run_with_its_replay() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/sim/loop_panic-3.choices");
        std::fs::remove_file(&file).ok();
        let failed = std::panic::catch_unwind(|| {
            let mut sim = Sim::new("loop_panic", Choices::seeded(3), LIMIT);
            let mut ed = Editor::new(crate::config::Config::default());
            ed.events.jobs().spawn(|| |_: &mut Editor| panic!("the result's own"));
            sim.walk(&mut ed, 0, |_, _| String::new(), |_| Ok(()), |_| String::new());
        });
        let said = failed.err().map(|p| panic_said(&*p)).unwrap_or_default();
        assert!(said.starts_with("simulation loop_panic (seed 3) failed after "), "{said}");
        assert!(
            said.contains("panicked as it landed: the result's own") && said.contains("lands from "),
            "{said}"
        );
        assert!(said.contains("replay: TARAE_SIM_REPLAY=") && file.exists(), "{said}");
    }

    /// Second review 5 (the reviewer's note on threads the loop starts): one the loop lets go that ends a
    /// moment later — a reaper, `tarae-end` — is waited for (`HELPERS_WAIT`) before the step is judged. Most
    /// here end at once, every eighth 30 ms later; without the wait, the late ones fail the run every time, and
    /// whether the ones ending at once did hung on the OS's timing.
    #[test]
    fn a_thread_the_loop_lets_go_is_waited_for() {
        let mut sim = Sim::new("let_go", Choices::seeded(2), LIMIT);
        let mut ed = Editor::new(crate::config::Config::default());
        let input = |sim: &mut Sim, _: &mut Editor| {
            let late = sim.steps.is_multiple_of(8);
            crate::event::thread("tarae-sim-go", move || {
                if late {
                    std::thread::sleep(Duration::from_millis(30));
                }
            });
            "a thread let go".to_string()
        };
        sim.walk(&mut ed, 40, input, |_| Ok(()), |_| String::new());
        assert_eq!(sim.steps, 40);
    }

    fn tree(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
    }

    /// Second review 1: a test-only declaration (`#[cfg(test)] mod compat;`) hides nothing after it — what a
    /// non-test build compiles is counted to the file's end — and a test-only item of any form isn't counted (a
    /// module, a function, a field, a statement), nor comments and literals (a `}` in one doesn't end an item).
    #[test]
    fn the_lint_counts_what_a_non_test_build_compiles() {
        let main = "mod a;\n#[cfg(test)]\nmod compat;\nfn f() { let t = Instant::now(); }\n\
                    #[cfg(test)]\nfn helper() { Instant::now(); }\n\
                    struct S {\n    #[cfg(test)]\n    t: Instant,\n    u: u8\n}\n\
                    fn g() {\n    #[cfg(test)]\n    let _ = Instant::now();\n    // Instant::now()\n    \
                    let s = \"Instant::now()\";\n    let r = r#\"Instant::now() \"#;\n    SystemTime::now();\n}\n\
                    #[cfg(test)]\nmod tests {\n    fn x() { Instant::now(); let s = \"}\"; let c = '}'; }\n}\n\
                    fn h<'a>(_: &'a u8) { Instant::now(); }\n";
        let files = tree(&[("main.rs", main), ("a.rs", ""), ("compat.rs", "fn c() { Instant::now(); }")]);
        let n = lint::count_tree(&files, &["Instant::now", "SystemTime::now"], &[]);
        assert_eq!(n, BTreeMap::from([("main.rs".to_string(), 3)]), "f, g's SystemTime, h");
    }

    /// Second review 2: a module declared after a test-only one is the build's own (`nav.rs` after
    /// `#[cfg(test)] mod mutate;`); one declared under a test-only attribute, by its `#[path]`, or by a test-only
    /// file at all, is the tests'.
    #[test]
    fn test_only_files_are_the_ones_test_items_declare() {
        let read = "fn x() { Instant::now(); }\n";
        let main = "#[cfg(test)]\nmod mutate;\nmod nav;\n#[cfg(all(test, target_os = \"macos\"))]\nmod apart;\nmod term;\n";
        let term =
            "fn t() {}\n#[cfg(test)]\n#[path = \"term_snapshots.rs\"]\nmod snapshot_tests;\nmod after;\n";
        let files = tree(&[
            ("main.rs", main),
            ("nav.rs", read),
            ("mutate.rs", "mod deeper;\n"),
            ("mutate/deeper.rs", read),
            ("apart.rs", read),
            ("term.rs", term),
            ("term_snapshots.rs", read),
            ("term/after.rs", read),
        ]);
        let n = lint::count_tree(&files, &["Instant::now"], &[]);
        let counted: Vec<&str> = n.keys().map(String::as_str).collect();
        assert_eq!(counted, ["nav.rs", "term/after.rs"]);
    }
}

/// Source-walking checks that keep code on the simulation's seams (`clock`, `event::sleep`).
pub mod lint {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    /// How often each of `patterns` appears per file of tarae's code (under `src/`, keyed by the path from
    /// there) — in what a non-test build compiles; `except` left out.
    pub fn counts(patterns: &[&str], except: &[&str]) -> BTreeMap<String, usize> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths = Vec::new();
        walk(&src, &mut paths);
        let files: Vec<(String, String)> = paths
            .iter()
            .map(|p| {
                (p.strip_prefix(&src).unwrap().display().to_string(), std::fs::read_to_string(p).unwrap())
            })
            .collect();
        count_tree(&files, patterns, except)
    }

    /// `counts` over `files` (each its path from `src/` and its text): not in comments or literals, nor in a
    /// test-only item or file.
    pub fn count_tree(
        files: &[(String, String)],
        patterns: &[&str],
        except: &[&str],
    ) -> BTreeMap<String, usize> {
        let split: BTreeMap<&str, Split> = files.iter().map(|(p, t)| (p.as_str(), split(t))).collect();
        let tests = test_only(&split);
        let mut out = BTreeMap::new();
        for (path, s) in &split {
            if except.contains(path) || tests.contains(*path) {
                continue;
            }
            let n: usize = patterns.iter().map(|p| s.code.matches(p).count()).sum();
            if n > 0 {
                out.insert(path.to_string(), n);
            }
        }
        out
    }

    /// A file's text as the two builds see it.
    #[derive(Default)]
    struct Split {
        /// What a non-test build compiles: comments out, literals emptied, test-only items out.
        code: String,
        /// The same with its literals (a `#[path = "…"]`'s).
        decls: String,
        /// Its test-only items, literals kept.
        tests: String,
        /// Test-only whole (`#![cfg(test)]`).
        whole: bool,
    }

    /// A test-only attribute: `#[cfg(test)]`, `#[cfg(all(test, …))]`.
    fn cfg_test(at: &str) -> bool {
        at.starts_with("#[cfg(test") || at.starts_with("#[cfg(all(test")
    }

    /// `text` split (`Split`). A test-only item is whatever follows its attribute up to its end, whatever it is:
    /// a module or function (its block), a declaration or statement (its `;`), a field (its `,`, or the block
    /// around it closing).
    fn split(text: &str) -> Split {
        let mut s =
            Split { whole: text.lines().any(|l| l.trim().starts_with("#![cfg(test")), ..Split::default() };
        // (in a test-only item: its bracket depth)
        let mut item: Option<i32> = None;
        let (mut i, mut prev) = (0, ' ');
        while i < text.len() {
            let rest = &text[i..];
            if let Some((n, comment)) = literal(rest, prev) {
                match (item.is_some(), comment) {
                    (_, true) => {}
                    (true, false) => s.tests.push_str(&rest[..n]),
                    (false, false) => {
                        s.code.push_str("\"\"");
                        s.decls.push_str(&rest[..n]);
                    }
                }
                (i, prev) = (i + n, '"');
                continue;
            }
            let c = rest.chars().next().unwrap_or(' ');
            (i, prev) = (i + c.len_utf8(), c);
            if item.is_none() && cfg_test(rest) {
                item = Some(0);
            }
            if let Some(depth) = item {
                // (the block around it closes: it ended unterminated — a last field)
                if !(c == '}' && depth == 0) {
                    s.tests.push(c);
                    item = match c {
                        '(' | '[' | '{' => Some(depth + 1),
                        ')' | ']' => Some(depth - 1),
                        '}' if depth == 1 => None,
                        '}' => Some(depth - 1),
                        ';' | ',' if depth == 0 => None,
                        _ => Some(depth),
                    };
                    continue;
                }
                item = None;
            }
            s.code.push(c);
            s.decls.push(c);
        }
        s
    }

    /// A comment or literal at `rest`'s start (`prev`: the character before it): its length, and whether it's a
    /// comment. A lifetime (`'a`) is neither.
    fn literal(rest: &str, prev: char) -> Option<(usize, bool)> {
        let b = rest.as_bytes();
        if rest.starts_with("//") {
            return Some((rest.find('\n').unwrap_or(b.len()), true));
        }
        if rest.starts_with("/*") {
            let (mut depth, mut j) = (0, 0);
            while j < b.len() {
                if rest[j..].starts_with("/*") {
                    (depth, j) = (depth + 1, j + 2);
                } else if rest[j..].starts_with("*/") {
                    (depth, j) = (depth - 1, j + 2);
                    if depth == 0 {
                        return Some((j, true));
                    }
                } else {
                    j += 1;
                }
            }
            return Some((b.len(), true));
        }
        let ident = prev.is_alphanumeric() || prev == '_';
        // (raw strings: r"…", r#"…"#, br"…")
        let raw = match () {
            _ if ident => None,
            _ if rest.starts_with("br") => Some(2),
            _ if rest.starts_with('r') => Some(1),
            _ => None,
        };
        if let Some(k) = raw {
            let hashes = b[k..].iter().take_while(|&&x| x == b'#').count();
            if b.get(k + hashes) == Some(&b'"') {
                let close = format!("\"{}", "#".repeat(hashes));
                let from = k + hashes + 1;
                return Some((rest[from..].find(&close).map_or(b.len(), |e| from + e + close.len()), false));
            }
        }
        let (k, quote) = match (b.first(), b.get(1)) {
            (Some(b'b'), Some(&q @ (b'"' | b'\''))) if !ident => (1, q),
            (Some(&q @ (b'"' | b'\'')), _) => (0, q),
            _ => return None,
        };
        if quote == b'"' {
            let mut j = k + 1;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 2,
                    b'"' => return Some((j + 1, false)),
                    _ => j += 1,
                }
            }
            return Some((b.len(), false));
        }
        // (a character: 'x', '\'', '\u{1F600}')
        if b.get(k + 1) == Some(&b'\\') {
            let end = rest.get(k + 3..)?.find('\'')?;
            return Some((k + 3 + end + 1, false));
        }
        let c = rest.get(k + 1..)?.chars().next()?;
        let after = k + 1 + c.len_utf8();
        (b.get(after) == Some(&b'\'')).then_some((after + 1, false))
    }

    /// The files only a test build has: those a test-only item declares (`#[cfg(test)] mod x;`, with its
    /// `#[path = "…"]` or not), those a file of these declares at all, test-only wholes — to a fixpoint.
    fn test_only(split: &BTreeMap<&str, Split>) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = split.iter().filter(|s| s.1.whole).map(|s| s.0.to_string()).collect();
        loop {
            let before = out.len();
            for (path, s) in split {
                let all;
                let text = match out.contains(*path) {
                    true => {
                        all = format!("{}{}", s.decls, s.tests);
                        &all
                    }
                    false => &s.tests,
                };
                out.extend(declared(path, text).into_iter().filter(|f| split.contains_key(f.as_str())));
            }
            if out.len() == before {
                return out;
            }
        }
    }

    /// The files `text` (of the file at `path`) declares as modules, as Rust finds them: `mod x;` in its
    /// module's folder (`x.rs`, `x/mod.rs`), `#[path = "p"] mod x;` from its own folder.
    fn declared(path: &str, text: &str) -> Vec<String> {
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let stem = file.trim_end_matches(".rs");
        let join = |a: &str, b: &str| if a.is_empty() { b.to_string() } else { format!("{a}/{b}") };
        let module = match stem == "mod" || (stem == "main" && dir.is_empty()) {
            true => dir.to_string(),
            false => join(dir, stem),
        };
        let mut out = Vec::new();
        for (at, _) in text.match_indices("mod ") {
            if text[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let rest = &text[at + 4..];
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if name.is_empty() || !rest[name.len()..].trim_start().starts_with(';') {
                continue;
            }
            let before = &text[..at];
            let attrs = &before[before.rfind([';', '{', '}']).map_or(0, |j| j + 1)..];
            match attrs.split_once("#[path = \"").and_then(|(_, p)| p.split_once('"')) {
                Some((p, _)) => out.push(normal(&join(dir, p))),
                None => out
                    .extend([join(&module, &format!("{name}.rs")), join(&module, &format!("{name}/mod.rs"))]),
            }
        }
        out
    }

    /// `a/b/../c` as `a/c`.
    fn normal(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for p in path.split('/') {
            match p {
                ".." => {
                    parts.pop();
                }
                "." | "" => {}
                p => parts.push(p),
            }
        }
        parts.join("/")
    }

    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// `now` (per file) against `kept`, a list that only shrinks: a file over its count fails, one under it fails
    /// until its count is lowered, a file off the list fails — and the message prints the table as it is now.
    pub fn only_shrinks(what: &str, now: &BTreeMap<String, usize>, kept: &[(&str, usize)]) {
        let kept: BTreeMap<&str, usize> = kept.iter().copied().collect();
        let mut wrong = Vec::new();
        for (f, &n) in now {
            match kept.get(f.as_str()) {
                None => wrong.push(format!("{f}: {n}, not on the list")),
                Some(&k) if n > k => wrong.push(format!("{f}: {n}, more than its {k}")),
                Some(&k) if n < k => wrong.push(format!("{f}: {n} — lower its {k} to {n}")),
                _ => {}
            }
        }
        for f in kept.keys().filter(|f| !now.contains_key(**f)) {
            wrong.push(format!("{f}: none left — take it off the list"));
        }
        let table: String = now.iter().map(|(f, n)| format!("    (\"{f}\", {n}),\n")).collect();
        assert!(wrong.is_empty(), "{what}:\n{}\n\nthe table as it is now:\n{table}", wrong.join("\n"));
    }
}
