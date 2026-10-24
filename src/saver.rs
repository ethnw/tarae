//! Saving off the main loop, as Helix does. A save is checked where it's asked (still loading, no name,
//! changed on disk), then the text — a rope clone, no copy — goes to the saver's queue and a job of its own
//! writes it the way `:w` always has (temp file, F_FULLFSYNC on macOS, rename: `disk::write_atomic`) while
//! typing goes on. Measured on macOS: a 270 KB file held the main loop 9 ms, nearly all of it the flush —
//! `:wa` or an auto-save of ten files, 90 ms.
//!
//! One save written at a time, first in first out: each job takes the oldest left under the saver's write lock,
//! so a later save of a file never lands before an earlier one. When nothing is left to write the main loop
//! hears of it and finishes those saves at once (saved version, disk stamp, "Saved", the language server's
//! didSave …) — one message for a batch. Quitting waits for what's still being written
//! (`Editor::flush_saves`), as Helix's quit commands do. (Jobs, not a thread of its own: a simulation schedules
//! them as its own — `sim/writing.rs`'s review answered with something applied saves.)

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ropey::Rope;

use crate::disk::Stamp;
use crate::document::DocId;
use crate::editor::Editor;
use crate::event::Event;

/// What finishes a save on the main loop.
#[derive(Clone, Debug, PartialEq)]
pub enum Then {
    /// `:w`, `:wa`, format-on-save: "Saved …", and the file's followers (server, undo file, git, plugins).
    Command,
    /// Auto-save — "Saved … (idle)" and the same followers but git (as before).
    Auto(&'static str),
}

struct Job {
    id: DocId,
    path: PathBuf,
    text: Rope,
    version: u64,
    then: Then,
    /// A new file of Claude's: its missing folders are made first, inside this project (`Document::make_folders`).
    folders: Option<PathBuf>,
    /// A save Claude caused (its change accepted, its own write that never came): written beneath this folder
    /// through no link (`write_beneath`), its missing folders made the same way.
    beneath: Option<PathBuf>,
}

/// `text` to `path` as Claude's changes are written (`refactor::write_file`, `beneath.rs`): through descriptors
/// walked from `root`'s real path one name at a time, no link followed, its missing folders made on the way —
/// a file there replaced whole (made beside it, renamed over it; a link there refused), else made. Never a path
/// walked again after the check: a folder swapped for a link meanwhile isn't gone through. `between` runs once
/// the folder is open, before a byte is written (tests: what a run could swap then).
pub(crate) fn write_beneath(
    root: &Path,
    path: &Path,
    text: &Rope,
    between: &dyn Fn(),
) -> Result<Option<Stamp>, String> {
    use crate::beneath::Dir;
    let said = |e: &dyn std::fmt::Display| format!("writing {}: {e}", path.display());
    let base = Dir::root(root).map_err(|e| said(&e))?;
    let rel = path.strip_prefix(&base.at).map_err(|_| said(&format!("not in {}", base.at.display())))?;
    let name = rel.file_name().ok_or_else(|| said(&"no file named"))?;
    let dir = base.folder(rel.parent().unwrap_or(Path::new("")), true).map_err(|e| said(&e))?;
    let there = match dir.stamp(name) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(said(&e)),
    };
    between();
    let mut bytes = Vec::with_capacity(text.len_bytes());
    text.write_to(&mut bytes).map_err(|e| said(&e))?;
    let written = if there { dir.replace(name, &bytes) } else { dir.create(name, &bytes) };
    written.map(Some).map_err(|e| said(&e))
}

/// The folders `path` needs, for a new file of Claude's (`Document::make_folders`): made when they're inside
/// `root`, else why not — never outside the project, where tarae doesn't write unasked.
fn make_folders(root: &Path, path: &Path) -> Result<(), String> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty() && !d.exists()) else { return Ok(()) };
    if crate::paths::inside(root, dir).is_none() {
        return Err(format!(
            "writing {}: its folder isn't there, and tarae makes folders only inside the project",
            path.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))
}

/// A written (or failed) save, for the main loop.
pub struct Done {
    pub id: DocId,
    pub path: PathBuf,
    pub version: u64,
    pub then: Then,
    /// The file's new fingerprint — or why it couldn't be written
    pub result: Result<Option<Stamp>, String>,
}

#[derive(Default)]
struct Shared {
    /// Asked but not written yet
    pending: usize,
    /// Saves asked and written since the start — a run waits for the count asked before it (`after_saves`)
    asked: u64,
    written: u64,
    done: Vec<Done>,
}

/// What waits for the saves asked before it (`Editor::after_saves`), in the session that asked.
struct Wait {
    asked: u64,
    session: crate::event::SessionId,
    /// The user's input it was asked under (`Queue.asked`): `then` runs as asked by it, as a job's result
    /// does — what it makes (`:restart`'s card) is still the user's (#205).
    input: Option<u64>,
    then: Box<dyn FnOnce(&mut Editor)>,
}

/// Saves asked and not taken yet, oldest first — and whether a drainer is on them.
#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    draining: bool,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `job` written as `:w` always has (or beneath its folder through no link), its outcome kept for the main loop —
/// whether nothing is left to write now. `_slow`: tests' slow disk.
fn write_one(job: Job, shared: &(Mutex<Shared>, Condvar), _slow: Duration) -> bool {
    let write = |w: &mut std::io::BufWriter<std::fs::File>| {
        // (inside the write: its temp file is there meanwhile, as on a slow disk)
        #[cfg(test)]
        std::thread::sleep(_slow);
        job.text.write_to(w)
    };
    let result = match job.beneath.as_deref() {
        Some(root) => write_beneath(root, &job.path, &job.text, &|| {
            #[cfg(test)]
            std::thread::sleep(_slow);
        }),
        None => job.folders.as_deref().map_or(Ok(()), |root| make_folders(root, &job.path)).and_then(|()| {
            crate::disk::write_atomic(&job.path, write)
                .map(|()| crate::disk::stamp(&job.path))
                .map_err(|e| format!("writing {}: {e}", job.path.display()))
        }),
    };
    let (lock, cv) = shared;
    let idle = {
        let mut s = locked(lock);
        let Job { id, path, version, then, .. } = job;
        s.done.push(Done { id, path, version, then, result });
        s.pending -= 1;
        s.written += 1;
        s.pending == 0
    };
    cv.notify_all();
    idle
}

pub struct Saver {
    events: crate::event::Tx,
    /// Saves asked and not written yet — one drainer job at a time takes them, oldest first, each under `writing`
    queue: Arc<Mutex<Queue>>,
    writing: Arc<Mutex<()>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    /// Oldest first
    waits: Vec<Wait>,
    /// Tests: a slow disk's stand-in — each write takes this long, its temp file there meanwhile (set before
    /// the first save)
    #[cfg(test)]
    pub slow: Duration,
}

impl Saver {
    pub fn new(events: crate::event::Tx) -> Saver {
        Saver {
            events,
            queue: Arc::default(),
            writing: Arc::default(),
            shared: Arc::default(),
            waits: Vec::new(),
            #[cfg(test)]
            slow: Duration::ZERO,
        }
    }

    /// `job` queued; a drainer started if none is on the queue — one at a time (`:wa` of ten files is one job, not
    /// ten threads), a `background` job: kept work, the process's end waits for it.
    fn push(&mut self, job: Job, jobs: &crate::event::Jobs) {
        {
            let mut s = locked(&self.shared.0);
            s.pending += 1;
            s.asked += 1;
        }
        {
            let mut q = locked(&self.queue);
            q.jobs.push_back(job);
            if std::mem::replace(&mut q.draining, true) {
                return;
            }
        }
        let (queue, writing, shared) = (self.queue.clone(), self.writing.clone(), self.shared.clone());
        let events = self.events.clone();
        #[cfg(test)]
        let slow = self.slow;
        #[cfg(not(test))]
        let slow = Duration::ZERO;
        jobs.background(move || {
            loop {
                let idle = {
                    // (taken under the write lock: a later save of a file never lands before an earlier one, whoever
                    // writes it — this job or a flush, `wait`)
                    let _one = locked(&writing);
                    let job = {
                        let mut q = locked(&queue);
                        match q.jobs.pop_front() {
                            Some(job) => job,
                            None => {
                                q.draining = false;
                                return;
                            }
                        }
                    };
                    write_one(job, &shared, slow)
                };
                // Out of work: the main loop finishes the batch (one message for `:wa`), and starts what waited for it
                if idle {
                    let _ = events.send(Event::Job(Box::new(|ed: &mut Editor| ed.saves_woke())));
                }
            }
        });
    }

    /// Saves written since last asked.
    pub fn take_done(&self) -> Vec<Done> {
        std::mem::take(&mut self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).done)
    }

    /// (saves asked so far, written so far)
    fn counts(&self) -> (u64, u64) {
        let s = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        (s.asked, s.written)
    }

    /// Until nothing is left to write, or `within` passes — whether it's all written. What's still queued while no
    /// write is under way is written here, oldest first: a quit or a save of the file itself doesn't wait on a job
    /// (in a simulation, one only it would run); one under way (a slow disk) is waited for, `within` at most.
    pub fn wait(&self, within: Duration) -> bool {
        #[cfg(test)]
        let slow = self.slow;
        #[cfg(not(test))]
        let slow = Duration::ZERO;
        loop {
            let _one = match self.writing.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => break,
            };
            let Some(job) = locked(&self.queue).jobs.pop_front() else { break };
            write_one(job, &self.shared, slow);
        }
        let (lock, cv) = &*self.shared;
        let s = lock.lock().unwrap_or_else(|p| p.into_inner());
        let (s, _) = cv.wait_timeout_while(s, within, |s| s.pending > 0).unwrap_or_else(|p| p.into_inner());
        s.pending == 0
    }
}

/// How long quitting waits for saves still being written (a network drive can be slow; a hung one isn't
/// waited for forever — the quit is refused, the saves go on).
const QUIT_WAIT: Duration = Duration::from_secs(10);

/// A run waiting this long for its saves says so (`after_saves`) — a usual save is a few ms, never seen.
const SAY_WAITING: Duration = Duration::from_secs(1);
const WAITING: &str = "saves-before-run";

impl Editor {
    /// Save document `id` in the background. Err: it can't be saved (nothing was asked) — the same
    /// errors a save always gave, at once.
    pub fn save_in_background(&mut self, id: DocId, force: bool, then: Then) -> Result<(), String> {
        self.save_job(id, force, then, None)
    }

    /// `save_in_background` for a save Claude caused — its change accepted in the review, its own write that
    /// never came: written beneath `root` (the project) through no link
    /// (`write_beneath`; `space u` writes with it in place — `checkpoint::file`). Your own saves (`:w`,
    /// auto-save) write by path as always — a link to a file elsewhere is yours to follow.
    pub fn save_beneath(&mut self, id: DocId, root: PathBuf, then: Then) -> Result<(), String> {
        self.save_job(id, false, then, Some(root))
    }

    pub(crate) fn save_job(
        &mut self,
        id: DocId,
        force: bool,
        then: Then,
        beneath: Option<PathBuf>,
    ) -> Result<(), String> {
        // (into a checkout being moved onto its base: written once it lands — `keepclose::hold_save`)
        if crate::keepclose::hold_save(self, id, force, &then, &beneath) {
            return Ok(());
        }
        let doc = self.docs.iter_mut().find(|d| d.id == id).ok_or("the buffer was closed")?;
        let path = doc.check_save(force).map_err(|e| format!("{e:#}"))?;
        doc.saving += 1;
        let folders = doc.make_folders.clone();
        let job = Job { id, path, text: doc.text.clone(), version: doc.version(), then, folders, beneath };
        let jobs = self.events.jobs();
        self.saver.push(job, &jobs);
        Ok(())
    }

    /// The saver's wake-up, an event of its own: `saves_done`, and nothing drawn when nothing of it is on
    /// screen (the saves taken by a flush before it, what waited for them started in a session behind).
    fn saves_woke(&mut self) {
        if self.saves_done() {
            self.unchanged = true;
        }
    }

    /// Finish the saves written so far (the saver's wake-up, `flush_saves`), then start what waited for them.
    /// Whether nothing of it is on screen (no save finished, no step went, every wait started behind).
    pub fn saves_done(&mut self) -> bool {
        let done = self.saver.take_done();
        let mut quiet = done.is_empty();
        if !done.is_empty() {
            // (a file the session's reviewer read, saved: what changed since is looked at again — the rail's
            // `read … at`, whether a board read a review's ▲ changes)
            let read = self
                .session
                .reviewer
                .as_ref()
                .is_some_and(|r| done.iter().any(|d| r.digests.iter().any(|(p, _)| d.path == *p)));
            self.saves_finish(done);
            if read {
                crate::reviewer::since_sync_shown(self);
            }
        }
        let (_, written) = self.saver.counts();
        let (ready, waiting) =
            std::mem::take(&mut self.saver.waits).into_iter().partition(|w| w.asked <= written);
        self.saver.waits = waiting;
        if self.saver.waits.is_empty() {
            let toasts = self.toasts.len();
            self.step_done(WAITING);
            quiet &= self.toasts.len() == toasts;
        }
        for w in ready {
            let was = self.events.set_asked(w.input);
            quiet &= self.with_session(w.session, w.then);
            self.events.set_asked(was);
        }
        quiet
    }

    /// `then` once the saves asked so far are on disk — what runs the code on disk (a task, tests, the
    /// preflight) asks it after `auto_save`. Not a wait: the saver's idle event starts it (in the session
    /// that asked), and with nothing being written it runs at once. Saves keep their order, and so do
    /// these. A slow disk once held the screen here (`flush_saves`, up to 10 s); now a step says it.
    pub fn after_saves(&mut self, then: impl FnOnce(&mut Editor) + 'static) {
        let (asked, written) = self.saver.counts();
        if written >= asked {
            // (what's written finished first — its marks, its "Saved" — and earlier waits go first)
            self.saves_done();
            return then(self);
        }
        let first = self.saver.waits.is_empty();
        let input = self.events.asked();
        self.saver.waits.push(Wait { asked, session: self.session.id, input, then: Box::new(then) });
        if first {
            self.events.jobs().spawn(|| {
                std::thread::sleep(SAY_WAITING);
                |ed: &mut Editor| {
                    if !ed.saver.waits.is_empty() {
                        ed.set_step(WAITING, "still writing files — going on once they're saved");
                    }
                }
            });
        }
    }

    fn saves_finish(&mut self, done: Vec<Done>) {
        let (mut saved, mut auto, mut failed) = (Vec::new(), Vec::new(), Vec::new());
        let mut why = "";
        for d in done {
            let Some(doc) = self.docs.iter_mut().find(|doc| doc.id == d.id) else { continue };
            doc.saving = doc.saving.saturating_sub(1);
            // (renamed meanwhile with `:w new-name`: that save wrote the old name — its stamp isn't this file's)
            let same_file = doc.path.as_ref() == Some(&d.path);
            match d.result {
                Ok(stamp) => {
                    if same_file {
                        doc.saved_at(d.version, stamp);
                    }
                    // (a run under way no longer runs what's there — `queued::join`)
                    self.wrote(&d.path);
                    match d.then {
                        Then::Command => saved.push(d.id),
                        Then::Auto(w) => {
                            why = w;
                            auto.push(d.id);
                        }
                    }
                }
                Err(e) => failed.push(e),
            }
        }
        for &id in saved.iter().chain(&auto) {
            self.lsp_did_save(id);
            self.undo_persist(id);
            self.plugin_event(crate::plugin::PluginEvent::Saved, id);
        }
        for &id in &saved {
            self.git_saved(id);
            self.rebase_saved(id);
        }
        if !failed.is_empty() {
            self.set_error(format!("not saved — {}", failed.join("; ")));
        }
        let name =
            |ed: &Editor, id: DocId| ed.docs.iter().find(|d| d.id == id).map(|d| d.display_name_short());
        match (saved.as_slice(), auto.as_slice()) {
            ([], []) => {}
            ([one], []) => {
                if let Some(doc) = self.docs.iter().find(|d| d.id == *one) {
                    let msg = format!(
                        "Saved {} · {} lines",
                        doc.display_name(&self.session.root),
                        doc.text.len_lines()
                    );
                    self.set_success(msg);
                }
            }
            ([], [one]) => {
                if let Some(n) = name(self, *one) {
                    self.set_success(format!("Saved {n}{why}"));
                }
            }
            (s, []) => self.set_success(format!("Saved {} files", s.len())),
            (s, a) => self.set_success(format!("Saved {} files{why}", s.len() + a.len())),
        }
    }

    /// Wait for the saves still being written, then finish them — before quitting, or before a save that
    /// writes the file itself (`Document::save_as`) where an older background save must not land after it.
    /// false: some are still being written after a long wait.
    pub fn flush_saves(&mut self) -> bool {
        let all = self.saver.wait(QUIT_WAIT);
        self.saves_done();
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::editor::ToastKind;
    use crate::key::Key;

    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key: Key = if c == '<' {
                chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap()
            } else {
                c.to_string().parse().unwrap()
            };
            ed.handle_key(key);
        }
    }

    /// A flush (a quit, a save of the file itself) never waits on a job: what's still queued it writes itself,
    /// oldest first — in a simulation the drainer is a job only the simulation runs, and a flush waiting on it would
    /// wait out `QUIT_WAIT` with nothing written. Several saves are one drainer job, not a thread each.
    #[test]
    fn a_flush_writes_what_is_queued_itself() {
        let dir = crate::scratch::Scratch::new("saver-flush-sim");
        let paths: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("{i}.txt"))).collect();
        for p in &paths {
            std::fs::write(p, "one\n").unwrap();
        }
        let sim =
            crate::sim::Sim::new("saver_flush", crate::sim::Choices::seeded(1), Duration::from_secs(30));
        let mut ed = Editor::new(Config::default());
        for p in &paths {
            ed.open(p).unwrap();
            feed(&mut ed, "ix<esc>");
            let id = ed.doc().id;
            ed.save_in_background(id, false, Then::Command).unwrap();
        }
        {
            let q = locked(&ed.saver.queue);
            assert!(q.draining && q.jobs.len() == 3, "one drainer for the three, none run yet");
        }
        let began = std::time::Instant::now();
        assert!(ed.flush_saves(), "all written");
        assert!(began.elapsed() < Duration::from_secs(2), "not waited out");
        for p in &paths {
            assert_eq!(std::fs::read_to_string(p).unwrap(), "xone\n", "{}", p.display());
        }
        assert!(ed.docs.iter().all(|d| !d.is_modified()), "finished: saved");
        drop(ed);
        drop(sim);
    }

    fn opened(name: &str) -> (Editor, PathBuf) {
        let dir = crate::scratch::Scratch::new(&format!("saver-{name}")).into_path();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        (ed, path)
    }

    fn toasts(ed: &Editor, kind: ToastKind) -> Vec<String> {
        ed.toasts.iter().filter(|t| t.kind == kind).map(|t| t.text.clone()).collect()
    }

    /// `:w` hands the text over and returns: what's typed meanwhile isn't in that save, and the buffer is
    /// "saved" as of the version that was written.
    #[test]
    fn a_save_is_written_in_the_background() {
        let (mut ed, path) = opened("bg");
        feed(&mut ed, "cTWO<esc>:w<ret>iX<esc>");
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TWOne\n");
        assert_eq!(ed.doc().text.to_string(), "TWOXne\n");
        assert!(ed.doc().is_modified(), "typed after the save");
        assert_eq!(ed.doc().saving, 0);
        assert!(
            toasts(&ed, ToastKind::Success).iter().any(|t| t.starts_with("Saved ") && t.contains("a.txt"))
        );
        // Undo back to what was written: that's saved
        feed(&mut ed, "u");
        assert!(!ed.doc().is_modified());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A save Claude caused goes through the project's folders by descriptor (`write_beneath`): `sub/` swapped
    /// for a link to outside after its folder was opened — what a sandboxed run could do while the save waits —
    /// and the outside file is untouched, the write lands in the folder that was opened; a link already there
    /// is refused. Through `save_beneath` the same: the outside file stays as it was. Your own `:w` follows.
    #[test]
    fn a_save_claude_caused_never_goes_through_a_link_swapped_in() {
        let scratch = crate::scratch::Scratch::new("saver-beneath");
        let base = scratch.to_path_buf();
        let (dir, outside) = (base.join("p"), base.join("outside"));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let (dir, outside) = (std::fs::canonicalize(&dir).unwrap(), std::fs::canonicalize(&outside).unwrap());
        std::fs::write(dir.join("sub/a.txt"), "one\n").unwrap();
        std::fs::write(outside.join("a.txt"), "theirs\n").unwrap();
        let swap = || {
            std::fs::rename(dir.join("sub"), dir.join("sub.aside")).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("sub")).unwrap();
        };
        let text = Rope::from_str("two\n");
        write_beneath(&dir, &dir.join("sub/a.txt"), &text, &swap).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.join("a.txt")).unwrap(),
            "theirs\n",
            "never through the link"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub.aside/a.txt")).unwrap(),
            "two\n",
            "in the folder opened"
        );
        // The link there before the write: refused
        let e = write_beneath(&dir, &dir.join("sub/a.txt"), &text, &|| {}).unwrap_err();
        assert!(e.contains("a link"), "{e}");
        // As the review's save: the same
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.clone();
        ed.open(&dir.join("sub.aside/a.txt")).unwrap();
        std::fs::remove_file(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("sub")).unwrap();
        ed.doc_mut().path = Some(dir.join("sub/a.txt"));
        ed.doc_mut().disk = crate::disk::stamp(&outside.join("a.txt"));
        feed(&mut ed, "cTHREE<esc>");
        let id = ed.doc().id;
        ed.save_beneath(id, dir.clone(), Then::Auto(" · Claude's change")).unwrap();
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(outside.join("a.txt")).unwrap(), "theirs\n");
        assert!(toasts(&ed, ToastKind::Error).iter().any(|t| t.contains("a link")), "said");
        std::fs::remove_dir_all(&base).ok();
    }

    /// The save tarae makes when Claude's write of an accepted edit never came (`agent_write_missed`): in the
    /// project it goes through the folders by descriptor — `sub/` swapped for a link to outside meanwhile and
    /// the outside file is untouched, said; a file outside the project (accepted on its review) is saved by
    /// its path as before.
    #[test]
    fn the_save_for_a_missed_write_never_goes_through_a_link() {
        let scratch = crate::scratch::Scratch::new("saver-missed");
        let base = scratch.to_path_buf();
        let (dir, outside, apart) = (base.join("p"), base.join("outside"), base.join("apart"));
        for d in [dir.join("sub"), outside.clone(), apart.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        let real = |p: &Path| std::fs::canonicalize(p).unwrap();
        let (dir, outside, apart) = (real(&dir), real(&outside), real(&apart));
        std::fs::write(dir.join("sub/a.txt"), "one\n").unwrap();
        std::fs::write(outside.join("a.txt"), "theirs\n").unwrap();
        std::fs::write(apart.join("b.txt"), "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.clone();
        // Claude's accepted edit, its write never come — and the folder swapped for a link meanwhile
        let missed = |ed: &mut Editor, path: &Path| {
            ed.open(path).unwrap();
            feed(ed, "cTWO<esc>");
            ed.doc_mut().agent_will_write();
            (ed.doc().id, ed.doc().version())
        };
        let (id, version) = missed(&mut ed, &dir.join("sub/a.txt"));
        std::fs::rename(dir.join("sub"), dir.join("sub.aside")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("sub")).unwrap();
        // (its fingerprint the outside file's: a conflict would stop it before the write — what's tried is the write)
        ed.doc_mut().disk = crate::disk::stamp(&outside.join("a.txt"));
        ed.agent_write_missed(id, version);
        assert!(ed.flush_saves());
        assert_eq!(
            std::fs::read_to_string(outside.join("a.txt")).unwrap(),
            "theirs\n",
            "never through the link"
        );
        assert_eq!(std::fs::read_to_string(dir.join("sub.aside/a.txt")).unwrap(), "one\n");
        assert!(toasts(&ed, ToastKind::Error).iter().any(|t| t.contains("a link")), "said");
        // Outside the project: by its path, as before
        let (id, version) = missed(&mut ed, &apart.join("b.txt"));
        ed.agent_write_missed(id, version);
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(apart.join("b.txt")).unwrap(), "TWOne\n");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Saves in a row land in the order asked — the file ends with the last — and none of them is taken for
    /// someone else changing the file (each checks the disk while the one before is being written).
    #[test]
    fn saves_land_in_order() {
        let (mut ed, path) = opened("order");
        for i in 0..20 {
            feed(&mut ed, &format!("A{}<esc>:w<ret>", i % 10));
        }
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ed.doc().text.to_string());
        assert!(!ed.doc().is_modified() && !ed.doc().disk_conflict);
        assert!(toasts(&ed, ToastKind::Error).is_empty(), "{:?}", toasts(&ed, ToastKind::Error));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A write that fails says so; the buffer stays unsaved.
    #[test]
    fn a_failed_write_is_reported() {
        let (mut ed, path) = opened("fail");
        ed.doc_mut().path = Some(path.parent().unwrap().join("gone/deeper/a.txt"));
        feed(&mut ed, "cTWO<esc>:w<ret>");
        assert!(ed.flush_saves());
        assert!(ed.doc().is_modified());
        assert_eq!(ed.doc().saving, 0);
        let errors = toasts(&ed, ToastKind::Error);
        assert!(errors.iter().any(|t| t.starts_with("not saved")), "{errors:?}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// `:w` then `:bc` in one go (a key bound to both): the close doesn't take the save still being written for
    /// unsaved changes.
    #[test]
    fn close_right_after_a_save() {
        let (mut ed, path) = opened("close");
        let other = path.parent().unwrap().join("b.txt");
        std::fs::write(&other, "two\n").unwrap();
        ed.open(&other).unwrap();
        feed(&mut ed, "cB<esc>:w<ret>:bc<ret>");
        assert!(toasts(&ed, ToastKind::Error).is_empty(), "{:?}", toasts(&ed, ToastKind::Error));
        assert!(ed.docs.iter().all(|d| d.path.as_ref() != Some(&other)), "closed");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "Bwo\n");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// `:wq` quits once the save is on disk.
    #[test]
    fn write_quit_waits_for_the_write() {
        let (mut ed, path) = opened("wq");
        feed(&mut ed, "cTWO<esc>:wq<ret>");
        assert!(ed.should_quit, "{:?}", toasts(&ed, ToastKind::Error));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TWOne\n");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Events until `done` (5 s at most).
    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + crate::test_results::PATIENCE;
        while !done(ed) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    /// A run starts once the saves before it are written — the saver's idle event, not a wait on the main
    /// loop (a slow disk held the screen up to 10 s) — and it reads what was saved.
    #[test]
    fn a_run_starts_once_its_saves_are_written() {
        let (mut ed, path) = opened("run");
        ed.saver.slow = Duration::from_millis(800);
        feed(&mut ed, "cTWO<esc>");
        let dir = path.parent().unwrap().to_path_buf();
        let task = crate::task::Task { name: "cat".into(), cmd: "cat a.txt".into(), cwd: dir.clone() };
        let t = std::time::Instant::now();
        ed.task_start(task);
        let took = t.elapsed();
        eprintln!("run asked with a save being written: {:.2} ms on the main loop", took.as_secs_f64() * 1e3);
        assert!(took < Duration::from_millis(300), "the main loop waited {took:?} for the save");
        assert!(ed.session.task_run.is_none(), "started before its save was written");
        settle_until(&mut ed, |ed| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        });
        let r = ed.session.task_run.as_ref().unwrap();
        assert_eq!(r.state, crate::task::State::Passed);
        let out: Vec<String> = r.output.iter().map(|(_, l)| l.into_owned()).collect();
        assert_eq!(out, ["TWOne"], "it read the saved file");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A run asked in one session starts in that session, though another is in front once its saves are
    /// written.
    #[test]
    fn a_run_waiting_for_its_saves_stays_its_sessions() {
        let (mut ed, path) = opened("session");
        let dir = path.parent().unwrap().to_path_buf();
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let first = ed.session.id;
        let second = ed.session_add(other.canonicalize().unwrap());
        ed.saver.slow = Duration::from_millis(300);
        feed(&mut ed, "cTWO<esc>");
        ed.task_start(crate::task::Task { name: "cat".into(), cmd: "cat a.txt".into(), cwd: dir.clone() });
        assert!(ed.session_switch(second));
        let ended = |ed: &Editor| {
            let s = std::iter::once(&ed.session).chain(&ed.sessions).find(|s| s.id == first).unwrap();
            s.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        };
        settle_until(&mut ed, ended);
        assert_eq!(ed.session.id, second);
        assert!(ed.session.task_run.is_none(), "not the session in front's");
        let s = ed.sessions.iter().find(|s| s.id == first).unwrap();
        let out: Vec<String> =
            s.task_run.as_ref().unwrap().output.iter().map(|(_, l)| l.into_owned()).collect();
        assert_eq!(out, ["TWOne"]);
        std::fs::remove_dir_all(dir).ok();
    }

    /// The saver's wake-up is an event of its own: when nothing of it is on screen — its saves taken by a
    /// flush before it, what waited for them started in a session behind — it draws nothing; a wait started in
    /// front, or the "still writing" step going, is drawn.
    #[test]
    fn the_wake_up_for_a_session_behind_draws_nothing() {
        let mut ed = Editor::new(Config::default());
        let back = ed.session_add(std::env::temp_dir());
        let ran = std::rc::Rc::new(std::cell::Cell::new(None));
        let woke = |ed: &mut Editor, session, step: bool| {
            let r = ran.clone();
            let then = Box::new(move |ed: &mut Editor| r.set(Some(ed.session.id)));
            ed.saver.waits.push(Wait { asked: 0, session, input: None, then });
            if step {
                ed.set_step(WAITING, "still writing files");
            }
            ed.unchanged = false;
            ed.handle_event(Event::Job(Box::new(|ed: &mut Editor| ed.saves_woke())));
            std::mem::take(&mut ed.unchanged)
        };
        assert!(woke(&mut ed, back, false), "started behind: nothing drawn");
        assert_eq!(ran.take(), Some(back), "in its session");
        let front = ed.session.id;
        assert!(!woke(&mut ed, front, false), "started in front: drawn");
        assert_eq!(ran.take(), Some(front));
        assert!(!woke(&mut ed, back, true), "the step went: drawn");
        assert!(ed.toasts.is_empty() && ran.take() == Some(back));
    }

    /// A save that takes long (a network drive): after a second the run says it's waiting for it — and that
    /// goes once it starts.
    #[test]
    fn a_run_waiting_long_for_its_saves_says_so() {
        let (mut ed, path) = opened("long");
        let dir = path.parent().unwrap().to_path_buf();
        ed.saver.slow = SAY_WAITING + Duration::from_millis(400);
        feed(&mut ed, "cTWO<esc>");
        ed.task_start(crate::task::Task { name: "cat".into(), cmd: "cat a.txt".into(), cwd: dir.clone() });
        let said = |ed: &Editor| ed.toasts.iter().any(|t| t.text.starts_with("still writing files"));
        settle_until(&mut ed, said);
        assert!(ed.session.task_run.is_none());
        settle_until(&mut ed, |ed| ed.session.task_run.is_some());
        assert!(!said(&ed), "gone once it started");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Auto-save (focus leaving) saves every modified file in the background, one message for them all.
    #[test]
    fn auto_save_is_one_batch() {
        let (mut ed, path) = opened("auto");
        let other = path.parent().unwrap().join("b.txt");
        std::fs::write(&other, "two\n").unwrap();
        feed(&mut ed, "cA<esc>");
        ed.open(&other).unwrap();
        feed(&mut ed, "cB<esc>");
        ed.on_focus(false);
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Ane\n");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "Bwo\n");
        assert!(ed.docs.iter().all(|d| !d.is_modified()));
        assert!(toasts(&ed, ToastKind::Success).contains(&"Saved 2 files".to_string()));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
