//! Saving off the main loop, as Helix does. A save is checked where it's asked (still loading, no name,
//! changed on disk), then the text — a rope clone, no copy — goes to one saver thread that writes it the way
//! `:w` always has (temp file, F_FULLFSYNC on macOS, rename: `disk::write_atomic`) while typing goes on.
//! Measured on macOS: a 270 KB file held the main loop 9 ms, nearly all of it the flush — `:wa` or an
//! auto-save of ten files, 90 ms.
//!
//! One thread, first in first out: a later save of a file never lands before an earlier one. When the
//! thread runs out of work the main loop hears of it and finishes those saves at once (saved version, disk
//! stamp, "Saved", the language server's didSave …) — one message for a batch. Quitting waits for what's
//! still being written (`Editor::flush_saves`), as Helix's quit commands do.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
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
    done: Vec<Done>,
}

pub struct Saver {
    events: Sender<Event>,
    /// The thread, started with the first save
    tx: Option<Sender<Job>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

impl Saver {
    pub fn new(events: Sender<Event>) -> Saver {
        Saver { events, tx: None, shared: Arc::default() }
    }

    fn push(&mut self, job: Job) {
        let shared = self.shared.clone();
        let events = self.events.clone();
        let tx = self.tx.get_or_insert_with(|| {
            let (tx, rx) = channel::<Job>();
            std::thread::spawn(move || {
                while let Ok(job) = rx.recv() {
                    let result = crate::disk::write_atomic(&job.path, |w| job.text.write_to(w))
                        .map(|()| crate::disk::stamp(&job.path))
                        .map_err(|e| format!("writing {}: {e}", job.path.display()));
                    let (lock, cv) = &*shared;
                    let idle = {
                        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
                        let Job { id, path, version, then, .. } = job;
                        s.done.push(Done { id, path, version, then, result });
                        s.pending -= 1;
                        s.pending == 0
                    };
                    cv.notify_all();
                    // Out of work: the main loop finishes the batch (one message for `:wa`)
                    if idle && events.send(Event::Job(Box::new(|ed: &mut Editor| ed.saves_done()))).is_err() {
                        return;
                    }
                }
            });
            tx
        });
        self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).pending += 1;
        let _ = tx.send(job);
    }

    /// Saves written since last asked.
    pub fn take_done(&self) -> Vec<Done> {
        std::mem::take(&mut self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).done)
    }

    /// Until nothing is left to write, or `within` passes — whether it's all written.
    pub fn wait(&self, within: Duration) -> bool {
        let (lock, cv) = &*self.shared;
        let s = lock.lock().unwrap_or_else(|p| p.into_inner());
        let (s, _) = cv.wait_timeout_while(s, within, |s| s.pending > 0).unwrap_or_else(|p| p.into_inner());
        s.pending == 0
    }
}

/// How long quitting waits for saves still being written (a network drive can be slow; a hung one isn't
/// waited for forever — the quit is refused, the saves go on).
const QUIT_WAIT: Duration = Duration::from_secs(10);

impl Editor {
    /// Save document `id` in the background. Err: it can't be saved (nothing was asked) — the same
    /// errors a save always gave, at once.
    pub fn save_in_background(&mut self, id: DocId, force: bool, then: Then) -> Result<(), String> {
        let doc = self.docs.iter_mut().find(|d| d.id == id).ok_or("the buffer was closed")?;
        let path = doc.check_save(force).map_err(|e| format!("{e:#}"))?;
        doc.saving += 1;
        let job = Job { id, path, text: doc.text.clone(), version: doc.version(), then };
        self.saver.push(job);
        Ok(())
    }

    /// Finish the saves written so far (the saver's wake-up, `flush_saves`).
    pub fn saves_done(&mut self) {
        let done = self.saver.take_done();
        if done.is_empty() {
            return;
        }
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
                    let msg =
                        format!("Saved {} · {} lines", doc.display_name(&self.root), doc.text.len_lines());
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

    fn opened(name: &str) -> (Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-saver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
