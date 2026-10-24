//! Unsaved-buffer recovery — a copy of every buffer with unsaved edits, so a crash, a kill, a power cut or a
//! terminal closed under tarae doesn't take what was typed with it.
//!
//! `$XDG_STATE_HOME/tarae/recovery/<pid>-<start ms>/` is one running tarae's (0700): `lock`, an flock held
//! while it runs — the kernel lets go when the process dies, however it dies, and `exec` closes it — and
//! `<doc id>.txt` per buffer: a JSON header line (its file, that file's stamp as the buffer last read or
//! wrote it, the root tarae started in, when), then the whole text. Not the edits since the save: those
//! need the saved file as it was, and a copy matters most when it isn't.
//!
//! One thread writes them (`Keeper`): a buffer's copy once it's been quiet for `QUIET`, or at the latest
//! `MOST` after its first change not yet kept — never on a key, and nothing comes back to the main loop (no
//! event, no frame). The main loop only tells it what changed (`Editor::recovery_sync`, every event: a few
//! compares per buffer, a rope clone when one moved). A buffer saved, undone to its file, reloaded or
//! closed: its copy goes. A quit removes the folder; a quit by a signal (the terminal went) and
//! `:restart`'s exec keep it, written up to the last key first (`Editor::recovery_end`).
//!
//! At startup a worker looks at the folders whose lock is free — a tarae that's gone — and each copy of
//! this project's that differs from its file is a card (`What::Recover`): `y` restore · `c` compare · `D`
//! discard · `n` not now (asked again at the next start). Never kept (`never`): the places that hold keys,
//! files named like keys, `editor.recovery-skip`; nor files over `MAX_BYTES`.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ropey::Rope;
use serde_json::{Value, json};

use crate::disk::Stamp;
use crate::document::{DocId, Document};
use crate::editor::Editor;
use crate::offer::{Offer, What};

/// A buffer quiet this long has its copy written…
const QUIET: Duration = Duration::from_secs(1);
/// …and one typed into without a pause, this long after its first change not yet kept.
const MOST: Duration = Duration::from_secs(10);
/// Buffers larger than this aren't kept (persistent undo's limit too).
pub const MAX_BYTES: usize = 8 << 20;
/// How long the end of the process waits for the copies to be written, or removed.
const EXIT_WAIT: Duration = Duration::from_secs(5);
const LOCK: &str = "lock";
const COPY: &str = "txt";
/// A compare buffer's title: this, then the file.
const COMPARE: &str = "recovered · ";

/// What decides, besides names like keys' (`secretnames`) and the places that hold keys, that a file is never kept: the user's
/// `editor.recovery-skip`, the sessions' roots its relative entries are from, and home (`~/` entries, and
/// where the key places are).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    pub home: Option<PathBuf>,
    pub skip: Vec<String>,
    pub roots: Vec<PathBuf>,
}

/// Whether the file at `path` is never kept: in a place that holds keys (`places`, real paths — a link into
/// `~/.ssh` counts), named like a key, or in `editor.recovery-skip`. On the keeper (it resolves the path).
fn never(path: &Path, rules: &Rules, places: &[PathBuf]) -> bool {
    let real = crate::sandbox::profile_path(path);
    let named = crate::secretnames::named_secret;
    places.iter().any(|p| real.starts_with(p)) || named(path) || named(&real) || skipped(&real, rules)
}

/// The places that hold keys, as real paths: what Claude's runs never read (`sandbox::kept_reads` —
/// `allow::SECRETS`, Claude's own, the toolchains' credentials wherever their homes are, tarae's state).
fn key_places(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else { return Vec::new() };
    crate::sandbox::kept_reads(home, &crate::sandbox::vars_in(home, &crate::sandbox::Here::now()))
}

/// `editor.recovery-skip`: a name (one `*` at most) matches any part of the path in any case; an entry
/// with a `/` is a path — from a session's root, or from `/` or `~/`.
fn skipped(real: &Path, rules: &Rules) -> bool {
    // (a `/` anywhere makes it a path — `private/` too)
    let entries = rules.skip.iter().map(|s| (s.trim().contains('/'), s.trim().trim_end_matches('/')));
    entries.filter(|(_, s)| !s.is_empty()).any(|(path, s)| {
        if let Some(rest) = s.strip_prefix("~/") {
            let at = rules.home.as_ref().map(|h| crate::sandbox::profile_path(&h.join(rest)));
            return at.is_some_and(|at| real.starts_with(at));
        }
        if s.starts_with('/') {
            return real.starts_with(crate::sandbox::profile_path(Path::new(s)));
        }
        if path {
            return rules.roots.iter().any(|r| real.strip_prefix(r).is_ok_and(|rel| rel.starts_with(s)));
        }
        let s = s.to_lowercase();
        real.iter().any(|p| crate::secretnames::wild(&s, &p.to_string_lossy().to_lowercase()))
    })
}

fn nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// A copy's first line.
fn header(path: &Path, root: &Path, disk: Option<Stamp>, conflict: bool, len: usize) -> String {
    json!({
        "v": 1,
        "path": path.to_string_lossy(),
        "root": root.to_string_lossy(),
        "disk": disk.map(|(t, n)| json!([nanos(t), n])),
        "conflict": conflict,
        "at": nanos(SystemTime::now()),
        "len": len,
    })
    .to_string()
}

/// A copy as read back.
#[derive(Debug)]
struct Copy {
    path: PathBuf,
    root: PathBuf,
    /// The file's stamp as the buffer knew it: (mtime ns, length)
    disk: Option<(u64, u64)>,
    conflict: bool,
    at: SystemTime,
    text: String,
}

/// A copy's file read back — None when it isn't one, or was cut short.
fn read_copy(file: &Path) -> Option<Copy> {
    let bytes = std::fs::read(file).ok()?;
    let nl = memchr::memchr(b'\n', &bytes)?;
    let v: Value = serde_json::from_slice(&bytes[..nl]).ok()?;
    let text = String::from_utf8(bytes[nl + 1..].to_vec()).ok()?;
    if v["v"].as_u64() != Some(1) || v["len"].as_u64()? != text.len() as u64 {
        return None;
    }
    let disk = v["disk"].as_array().and_then(|d| Some((d.first()?.as_u64()?, d.get(1)?.as_u64()?)));
    Some(Copy {
        path: PathBuf::from(v["path"].as_str()?),
        root: PathBuf::from(v["root"].as_str().unwrap_or_default()),
        disk,
        conflict: v["conflict"].as_bool().unwrap_or(false),
        at: UNIX_EPOCH + Duration::from_nanos(v["at"].as_u64().unwrap_or(0)),
        text,
    })
}

/// The copies in a folder.
fn copies(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let is_copy = |p: &Path| p.extension().is_some_and(|e| e == COPY);
    rd.flatten().map(|e| e.path()).filter(|p| is_copy(p)).collect()
}

/// This tarae's folder, made and locked — `run`, or `run.N` if a tarae starting up holds that one's lock
/// (it took it for a gone tarae's, and finds it empty).
fn take_folder(run: &Path) -> std::io::Result<(PathBuf, File)> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut last = None;
    for n in 0..4 {
        let dir = match n {
            0 => run.to_path_buf(),
            n => PathBuf::from(format!("{}.{n}", run.display())),
        };
        let made = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).and_then(|()| {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK))
        });
        match made.map(|f| (f.try_lock(), f)) {
            Ok((Ok(()), f)) => return Ok((dir, f)),
            Ok((Err(std::fs::TryLockError::WouldBlock), _)) => {}
            Ok((Err(std::fs::TryLockError::Error(e)), _)) | Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("every folder it tried was taken")))
}

enum Msg {
    Keep {
        id: DocId,
        path: PathBuf,
        text: Rope,
        disk: Option<Stamp>,
        conflict: bool,
    },
    Drop(DocId),
    Rules(Rules),
    /// Write every copy that waits, now — then answer.
    Flush(Sender<()>),
    /// Remove every copy and the folder — then answer, and end.
    Clear(Sender<()>),
    /// A gone tarae's copy answered: removed — and with the last of its folder's cards, the folder if it's
    /// empty, then its lock let go. (Here, not on a worker of its own: the end of the process waits for the
    /// keeper, so a quit right after `y` can't leave the copy to be offered again.)
    Forget {
        file: PathBuf,
        folder: Option<(PathBuf, File)>,
    },
}

/// A buffer's text waiting for its pause.
struct Waiting {
    path: PathBuf,
    text: Rope,
    disk: Option<Stamp>,
    conflict: bool,
    changed: Instant,
    first: Instant,
}

/// The thread that writes the copies.
struct Keeper {
    run: PathBuf,
    root: PathBuf,
    quiet: Duration,
    /// The folder in use and its lock (taken with the first copy)
    folder: Option<(PathBuf, File)>,
    waiting: std::collections::HashMap<DocId, Waiting>,
    /// Buffers whose copy is on disk
    kept: Vec<DocId>,
    rules: Rules,
    /// `key_places` for `rules.home` (worked out with the first copy)
    places: Option<Vec<PathBuf>>,
    /// A failure said in the log already
    said: bool,
    #[cfg(test)]
    slow: Duration,
}

impl Keeper {
    fn due(&self, w: &Waiting) -> Instant {
        (w.changed + self.quiet).min(w.first + MOST)
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let due = self.waiting.values().map(|w| self.due(w)).min();
            let msg = match due {
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                },
            };
            match msg {
                Some(Msg::Keep { id, path, text, disk, conflict }) => {
                    let changed = Instant::now();
                    let first = self.waiting.get(&id).map_or(changed, |w| w.first);
                    self.waiting.insert(id, Waiting { path, text, disk, conflict, changed, first });
                }
                Some(Msg::Drop(id)) => {
                    self.waiting.remove(&id);
                    self.remove(id);
                }
                Some(Msg::Rules(rules)) => {
                    if rules.home != self.rules.home {
                        self.places = None;
                    }
                    self.rules = rules;
                }
                Some(Msg::Flush(done)) => {
                    let all: Vec<DocId> = self.waiting.keys().copied().collect();
                    all.into_iter().for_each(|id| self.write(id));
                    let _ = done.send(());
                }
                Some(Msg::Clear(done)) => {
                    self.waiting.clear();
                    if let Some((dir, _lock)) = self.folder.take() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    let _ = done.send(());
                    return;
                }
                Some(Msg::Forget { file, folder }) => {
                    let _ = std::fs::remove_file(file);
                    if let Some((dir, _lock)) = folder
                        && copies(&dir).is_empty()
                    {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
                None => {}
            }
            let now = Instant::now();
            let due: Vec<DocId> =
                self.waiting.iter().filter(|(_, w)| self.due(w) <= now).map(|(id, _)| *id).collect();
            due.into_iter().for_each(|id| self.write(id));
        }
    }

    fn write(&mut self, id: DocId) {
        let Some(w) = self.waiting.remove(&id) else { return };
        if self.places.is_none() {
            self.places = Some(key_places(self.rules.home.as_deref()));
        }
        // (a buffer renamed to a key's name since its last copy loses that one too)
        if never(&w.path, &self.rules, self.places.as_deref().unwrap_or_default()) {
            return self.remove(id);
        }
        #[cfg(test)]
        std::thread::sleep(self.slow);
        if self.folder.is_none() {
            match take_folder(&self.run) {
                Ok(f) => self.folder = Some(f),
                Err(e) => return self.failed(format!("no folder for the copies of unsaved buffers: {e}")),
            }
        }
        let Some((dir, _)) = &self.folder else { return };
        let file = dir.join(format!("{id}.{COPY}"));
        let head = header(&w.path, &self.root, w.disk, w.conflict, w.text.len_bytes());
        let written = crate::disk::write_atomic(&file, |f| {
            f.write_all(head.as_bytes())?;
            f.write_all(b"\n")?;
            w.text.write_to(f)
        });
        match written {
            Ok(()) if !self.kept.contains(&id) => self.kept.push(id),
            Ok(()) => {}
            Err(e) => self.failed(format!("{}'s copy not written: {e}", w.path.display())),
        }
    }

    fn remove(&mut self, id: DocId) {
        if let Some((dir, _)) = &self.folder
            && self.kept.contains(&id)
        {
            let _ = std::fs::remove_file(dir.join(format!("{id}.{COPY}")));
            self.kept.retain(|&k| k != id);
        }
    }

    /// A copy that couldn't be written — said in the log, once (a full disk would say it every pause).
    fn failed(&mut self, msg: String) {
        if !std::mem::replace(&mut self.said, true) {
            crate::log::line("recovery", "warn", &msg);
        }
    }
}

/// What the keeper was last told about a buffer.
struct Told {
    id: DocId,
    version: u64,
    disk: Option<Stamp>,
    conflict: bool,
    path: PathBuf,
}

impl Told {
    fn is(&self, d: &Document) -> bool {
        self.version == d.version()
            && self.disk == d.disk
            && self.conflict == d.disk_conflict
            && d.path.as_ref() == Some(&self.path)
    }
}

/// A gone tarae's folder, held (its lock) while cards ask about its copies.
struct Held {
    dir: PathBuf,
    lock: File,
    cards: usize,
}

pub struct Recovery {
    /// `$state/recovery`
    dir: PathBuf,
    /// This tarae's folder in it (made with the first copy)
    run: PathBuf,
    /// Where tarae started — the project a copy is offered in again
    root: PathBuf,
    tx: Option<Sender<Msg>>,
    thread: Option<std::thread::JoinHandle<()>>,
    told: Vec<Told>,
    rules: Rules,
    held: Vec<Held>,
    /// Restores waiting for a big file's text to load
    after_load: Vec<(DocId, Found)>,
    /// Compare buffers: (the file, the buffer)
    compares: Vec<(PathBuf, DocId)>,
    /// The process ends keeping the copies — a signal quit (the terminal went), else a quit removes them.
    pub keep: bool,
    pub quiet: Duration,
    /// Tests: a slow disk's stand-in — each copy takes this long to write · startup looks answered
    #[cfg(test)]
    pub slow: Duration,
    #[cfg(test)]
    pub scans: usize,
}

impl Recovery {
    /// Copies in `dir` (`$state/recovery`), for a tarae started in `root`.
    pub fn new(dir: PathBuf, root: PathBuf) -> Recovery {
        // (numbered too: two editors of one process — tests — in the same millisecond)
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let start = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        Recovery {
            run: dir.join(format!("{}-{start}-{n}", std::process::id())),
            dir,
            root,
            tx: None,
            thread: None,
            told: Vec::new(),
            rules: Rules { home: crate::allow::home(), ..Default::default() },
            held: Vec::new(),
            after_load: Vec::new(),
            compares: Vec::new(),
            keep: false,
            quiet: QUIET,
            #[cfg(test)]
            slow: Duration::ZERO,
            #[cfg(test)]
            scans: 0,
        }
    }

    /// To the keeper — started with the first message.
    fn send(&mut self, msg: Msg) {
        if self.tx.is_none() {
            let (tx, rx) = channel();
            let keeper = Keeper {
                run: self.run.clone(),
                root: self.root.clone(),
                quiet: self.quiet,
                folder: None,
                waiting: Default::default(),
                kept: Vec::new(),
                rules: self.rules.clone(),
                places: None,
                said: false,
                #[cfg(test)]
                slow: self.slow,
            };
            let thread = std::thread::Builder::new().name("tarae-recovery".into()).spawn(|| keeper.run(rx));
            self.thread = thread.ok();
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }

    /// Asks the keeper `msg` and waits for its answer, up to `within` — whether it answered.
    fn ask(&mut self, msg: impl FnOnce(Sender<()>) -> Msg, within: Duration) -> bool {
        let Some(tx) = &self.tx else { return true };
        let (done, answered) = channel();
        tx.send(msg(done)).is_err() || answered.recv_timeout(within).is_ok()
    }

    /// Tests: the process dies — the keeper stops where it is and its lock goes, its copies stay. Gone once
    /// no process has it open: a child another test forked while it was open holds it too, until it execs
    /// (a fork copies every descriptor; close-on-exec closes them only at the exec) — under load long enough
    /// for the next scan to take this tarae for a running one and pass its copies over (seen on Linux: a
    /// `queued::tests` child, not yet exec'd). A real death lets it go the same way; this waits for that.
    #[cfg(test)]
    pub fn killed(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // (the folder the keeper took: `run`, or `run.N` — `take_folder`)
        let folders = (0..4).map(|n| match n {
            0 => self.run.clone(),
            n => PathBuf::from(format!("{}.{n}", self.run.display())),
        });
        let end = Instant::now() + crate::test_results::PATIENCE;
        for lock in folders.map(|d| d.join(LOCK)).filter(|l| l.exists()) {
            let free = || File::open(&lock).is_ok_and(|f| f.try_lock().is_ok());
            while !free() {
                assert!(Instant::now() < end, "{} still held 10 s after the keeper ended", lock.display());
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// Tests: what waits is written now.
    #[cfg(test)]
    pub fn flush(&mut self) -> bool {
        self.ask(Msg::Flush, EXIT_WAIT)
    }

    /// This tarae's folder (made with its first copy).
    #[cfg(test)]
    pub fn folder(&self) -> &Path {
        &self.run
    }
}

/// Whether a buffer's unsaved edits are kept: a file's (not a scratch, a commit message, a class file's
/// source), loaded, not too big. Where it is (`never`) is the keeper's to say.
fn keepable(d: &Document) -> bool {
    d.is_modified()
        && d.path.is_some()
        && !d.loading
        && !d.throwaway
        && d.virtual_uri.is_none()
        && d.text.len_bytes() <= MAX_BYTES
}

/// What became of a copy's file since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disk {
    /// As the buffer knew it.
    Same,
    /// It had changed on disk while the buffer was being edited (a conflict, before tarae ended).
    Conflict,
    /// Changed since — at this time.
    Changed(SystemTime),
    /// Deleted since.
    Gone,
    /// It was a new file, never saved (and still isn't there).
    New,
}

/// A copy newer than its file — one card.
#[derive(Clone, Debug)]
pub struct Found {
    pub path: PathBuf,
    /// The copy's file, and its folder
    file: PathBuf,
    dir: PathBuf,
    pub text: Rope,
    pub at: SystemTime,
    pub disk: Disk,
    /// The file as it is now (empty when there's none)
    pub base: Rope,
    /// Lines the copy has that the file hasn't · the file has that the copy hasn't
    pub added: usize,
    pub removed: usize,
}

#[cfg(test)]
impl Found {
    /// A copy found, in a test: nothing on disk behind it.
    pub(crate) fn for_test(path: PathBuf) -> Found {
        let (file, dir) = (path.join("copy"), path.clone());
        let (text, base) = (Rope::new(), Rope::new());
        Found {
            path,
            file,
            dir,
            text,
            at: SystemTime::UNIX_EPOCH,
            disk: Disk::Same,
            base,
            added: 0,
            removed: 0,
        }
    }
}

/// The copies left by taraes that are gone — those of this project (started in `origin`, or a file under
/// one of `roots`) that differ from their file, the newest per file, their folders held; a copy that's its
/// file already goes, and so does an older copy of a file offered. Another project's are left for a tarae
/// started there. (A worker.)
fn scan(dir: &Path, own: &Path, origin: &Path, roots: &[PathBuf]) -> (Vec<(PathBuf, File)>, Vec<Found>) {
    let (mut held, mut found) = (Vec::new(), Vec::<Found>::new());
    let Ok(rd) = std::fs::read_dir(dir) else { return (held, found) };
    let own = own.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for e in rd.flatten() {
        let folder = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !folder.is_dir() || name == own || name.starts_with(&format!("{own}.")) {
            continue;
        }
        let lock =
            std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(folder.join(LOCK));
        // (its tarae is running)
        let Some(lock) = lock.ok().filter(|l| l.try_lock().is_ok()) else { continue };
        let mut mine = 0;
        for file in copies(&folder) {
            let Some(c) = read_copy(&file) else { continue };
            if c.root != origin && !roots.iter().any(|r| c.path.starts_with(r)) {
                continue;
            }
            let now = crate::disk::stamp(&c.path);
            let base = now.and_then(|_| std::fs::read_to_string(&c.path).ok());
            if base.as_deref() == Some(c.text.as_str()) {
                let _ = std::fs::remove_file(&file);
                continue;
            }
            let disk = match (now, c.disk) {
                (None, None) => Disk::New,
                (None, Some(_)) => Disk::Gone,
                (Some((t, n)), was) if was != Some((nanos(t), n)) => Disk::Changed(t),
                _ if c.conflict => Disk::Conflict,
                _ => Disk::Same,
            };
            let base = base.unwrap_or_default();
            let hunks = crate::git::hunks(&base, &c.text);
            found.push(Found {
                path: c.path,
                file,
                dir: folder.clone(),
                added: hunks.iter().map(|h| h.lines.len()).sum(),
                removed: hunks.iter().map(|h| h.base.len()).sum(),
                text: Rope::from(c.text),
                at: c.at,
                disk,
                base: Rope::from(base),
            });
            mine += 1;
        }
        if copies(&folder).is_empty() {
            let _ = std::fs::remove_dir_all(&folder);
        } else if mine > 0 {
            held.push((folder, lock));
        }
    }
    // One card per file: its newest copy
    crate::sorting::by(&mut found, |a, b| a.path.cmp(&b.path).then(b.at.cmp(&a.at)));
    let mut seen: Option<PathBuf> = None;
    found.retain(|f| {
        let older = seen.as_ref() == Some(&f.path);
        if older {
            let _ = std::fs::remove_file(&f.file);
        }
        seen = Some(f.path.clone());
        !older
    });
    held.retain(|(d, _)| found.iter().any(|f| f.dir == *d));
    (held, found)
}

/// `at 14:32`, or `on 10-03 at 14:32` before today.
fn when(t: SystemTime) -> String {
    let (_, m, d) = crate::log::day(t);
    match crate::log::day(t) == crate::log::day(SystemTime::now()) {
        true => format!("at {}", crate::log::clock(t)),
        false => format!("on {m:02}-{d:02} at {}", crate::log::clock(t)),
    }
}

/// `path` as the card names it: from the root, else whole.
fn shown(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// The card for a copy found.
fn card(f: Found, root: &Path) -> Offer {
    // (when the copy was kept — tarae may have gone on a while after)
    let lines = match (f.added, f.removed) {
        (0, 0) => "spaces or the last newline".to_string(),
        (a, r) => format!("+{a} −{r} lines"),
    };
    let mut q = format!("Not saved when tarae ended: {lines}, kept {}.\n\n", when(f.at));
    let warn = !matches!(f.disk, Disk::Same | Disk::New);
    q.push_str(&match f.disk {
        Disk::Same => "The file on disk is as it was then.".to_string(),
        Disk::New => "It was a new file, never saved.".to_string(),
        Disk::Conflict => "The file changed while they were made — they'd go over it.".to_string(),
        Disk::Changed(t) => format!("The file changed since, {} — they'd go over it.", when(t)),
        Disk::Gone => "The file was deleted since — :w writes it again.".to_string(),
    });
    q.push_str("\nRestored, they stay unsaved — u goes back to the file.");
    let title = format!("Unsaved edits · {}", shown(&f.path, root));
    let o = Offer::new(title, &q, "Restoring…", "Not restored".into(), What::Recover(Box::new(f)));
    if warn { o.warning(2..3) } else { o }
}

/// Why `y` can't restore `f` now — an open buffer of it with edits of its own, or one still loading.
pub fn refuses(ed: &Editor, f: &Found) -> Option<String> {
    let d = ed.docs.iter().find(|d| d.path.as_ref() == Some(&f.path))?;
    match (d.loading, d.is_modified()) {
        (true, _) => Some("it's still loading — y in a moment".into()),
        (_, true) => Some(format!(
            "{} has unsaved edits of yours — save or :reload it first, then y",
            d.display_name_short()
        )),
        _ => None,
    }
}

/// `y`: the file open (or its buffer), the copy's text put in as one change — unsaved, `u` back to the file.
pub fn restore(ed: &mut Editor, card: u64, f: Found) {
    ed.offer_finished(card, Ok(()));
    compare_close(ed, &f.path);
    // (not opened: the copy stays, asked about again at the next start)
    if let Err(e) = ed.open(&f.path) {
        return ed.set_error(format!("not restored — {e:#}"));
    }
    let id = ed.doc().id;
    if ed.doc().loading {
        // (a big file: once its text is in — `recovery_loaded`)
        ed.set_status(format!("restoring {} once it's loaded", ed.doc().display_name_short()));
        if let Some(r) = ed.recovery.as_mut() {
            r.after_load.push((id, f));
        }
        return;
    }
    put(ed, id, &f);
}

/// The copy's text into buffer `id`, as one change — and the copy goes.
fn put(ed: &mut Editor, id: DocId, f: &Found) {
    let Some(doc) = ed.docs.iter_mut().find(|d| d.id == id) else { return };
    if let Some(change) = crate::disk::minimal_change(&doc.text, &f.text) {
        let before = doc.snapshot();
        let tx = crate::transaction::Transaction::new(vec![change]);
        let sel = tx.map_selection(doc.selection());
        doc.apply_with(&tx, sel);
        doc.own.not_yours();
        doc.commit_undo(before);
    }
    let name = doc.display_name_short();
    ed.set_success(format!("Restored {name} — unsaved: :w writes it, u goes back to the file"));
    forget(ed, f);
}

/// `c`: the copy in a buffer of its own, drawn against the file on disk as `:diff` draws; the card stays.
pub fn compare(ed: &mut Editor) {
    let Some(What::Recover(f)) = ed.offers.first().map(|o| &o.what) else { return };
    let (path, text, base) = (f.path.clone(), f.text.clone(), f.base.clone());
    compare_close(ed, &path);
    let id = ed.open_virtual(&format!("{COMPARE}{}", shown(&path, &ed.session.root)), "");
    let Some(doc) = ed.docs.iter_mut().find(|d| d.id == id) else { return };
    doc.text = text.clone();
    let version = doc.version();
    if let Some(spec) = crate::syntax::detect(&path) {
        ed.load_language(id, spec);
    }
    if let Some(r) = ed.recovery.as_mut() {
        r.compares.push((path, id));
    }
    ed.events.jobs().spawn(move || {
        let (base, now) = (base.to_string(), text.to_string());
        let hunks = crate::git::hunks(&base, &now);
        move |ed: &mut Editor| ed.diff_show(id, version, base, "the file on disk".into(), hunks)
    });
}

/// `D`: the copy goes.
pub fn discard(ed: &mut Editor, f: Found) {
    compare_close(ed, &f.path);
    forget(ed, &f);
    ed.set_status(format!("Discarded the unsaved edits of {}", shown(&f.path, &ed.session.root)));
}

/// `n`: the copy stays, asked about at the next start.
pub fn not_now(ed: &mut Editor, f: Found) {
    compare_close(ed, &f.path);
    drop(release(ed, &f.dir));
    let name = shown(&f.path, &ed.session.root);
    ed.set_status(format!("Kept the unsaved edits of {name} — asked again when tarae next starts"));
}

fn compare_close(ed: &mut Editor, path: &Path) {
    let Some(r) = ed.recovery.as_mut() else { return };
    let gone: Vec<DocId> = r.compares.iter().filter(|(p, _)| p == path).map(|(_, d)| *d).collect();
    r.compares.retain(|(p, _)| p != path);
    for d in gone {
        ed.close_doc(d);
    }
}

/// One card less for `dir` — its lock, when that was the last.
fn release(ed: &mut Editor, dir: &Path) -> Option<Held> {
    let r = ed.recovery.as_mut()?;
    let i = r.held.iter().position(|h| h.dir == dir)?;
    r.held[i].cards = r.held[i].cards.saturating_sub(1);
    (r.held[i].cards == 0).then(|| r.held.swap_remove(i))
}

/// The copy removed (by the keeper) — its folder too once it holds none.
fn forget(ed: &mut Editor, f: &Found) {
    let folder = release(ed, &f.dir).map(|h| (h.dir, h.lock));
    if let Some(r) = ed.recovery.as_mut() {
        r.send(Msg::Forget { file: f.file.clone(), folder });
    }
}

impl Editor {
    /// Every event: tells the keeper which buffers changed since — a copy for each with unsaved edits worth
    /// keeping, none for the rest. No disk here, nothing waited for; a rope clone (O(1)) only for a buffer
    /// whose version moved.
    pub(crate) fn recovery_sync(&mut self) {
        let Some(r) = self.recovery.as_mut() else { return };
        let roots = std::iter::once(&self.session).chain(&self.sessions).map(|s| &s.root);
        if !roots.clone().eq(r.rules.roots.iter()) || self.config.recovery_skip != r.rules.skip {
            r.rules.roots = roots.cloned().collect();
            r.rules.skip = self.config.recovery_skip.clone();
            if r.tx.is_some() {
                let rules = r.rules.clone();
                r.send(Msg::Rules(rules));
            }
        }
        let on = self.config.recovery;
        for d in &self.docs {
            let i = r.told.iter().position(|t| t.id == d.id);
            match (on && keepable(d), i) {
                (true, Some(i)) if r.told[i].is(d) => {}
                (true, i) => {
                    let Some(path) = d.path.clone() else { continue };
                    let told = Told {
                        id: d.id,
                        version: d.version(),
                        disk: d.disk,
                        conflict: d.disk_conflict,
                        path,
                    };
                    let (text, conflict) = (d.text.clone(), d.disk_conflict);
                    r.send(Msg::Keep { id: d.id, path: told.path.clone(), text, disk: d.disk, conflict });
                    match i {
                        Some(i) => r.told[i] = told,
                        None => r.told.push(told),
                    }
                }
                (false, Some(i)) => {
                    r.told.swap_remove(i);
                    r.send(Msg::Drop(d.id));
                }
                (false, None) => {}
            }
        }
        // Closed
        if r.told.len() > self.docs.len() || r.told.iter().any(|t| !self.docs.iter().any(|d| d.id == t.id)) {
            let gone: Vec<DocId> =
                r.told.iter().filter(|t| !self.docs.iter().any(|d| d.id == t.id)).map(|t| t.id).collect();
            for id in gone {
                r.told.retain(|t| t.id != id);
                r.send(Msg::Drop(id));
            }
        }
    }

    /// The end of the process: the copies written up to the last key and kept (`keep` — a restart's exec, an
    /// error; or a signal said so: `Recovery.keep`), or removed with the folder (a quit). Waits up to
    /// `EXIT_WAIT` — the loop has ended, nothing is on screen to hold.
    pub fn recovery_end(&mut self, keep: bool) {
        self.recovery_sync();
        let Some(r) = self.recovery.as_mut() else { return };
        if keep || r.keep {
            r.ask(Msg::Flush, EXIT_WAIT);
        } else {
            r.ask(Msg::Clear, EXIT_WAIT);
        }
    }

    /// At startup: the copies gone taraes left for this project, looked for on a worker — a card each.
    pub fn recovery_scan(&mut self) {
        let Some(r) = &self.recovery else { return };
        let (dir, own, origin) = (r.dir.clone(), r.run.clone(), r.root.clone());
        let roots: Vec<PathBuf> =
            std::iter::once(&self.session).chain(&self.sessions).map(|s| s.root.clone()).collect();
        self.events.shared_jobs().spawn(move || {
            let (held, found) = scan(&dir, &own, &origin, &roots);
            move |ed: &mut Editor| ed.recovery_found(held, found)
        });
    }

    fn recovery_found(&mut self, held: Vec<(PathBuf, File)>, found: Vec<Found>) {
        let Some(r) = self.recovery.as_mut() else { return };
        #[cfg(test)]
        {
            r.scans += 1;
        }
        if found.is_empty() {
            self.unchanged = true;
            return;
        }
        for (dir, lock) in held {
            let cards = found.iter().filter(|f| f.dir == dir).count();
            r.held.push(Held { dir, lock, cards });
        }
        let root = self.session.root.clone();
        for f in found {
            self.push_offer(card(f, &root));
        }
    }

    /// A big file's text is in: a restore that waited for it goes on.
    pub(crate) fn recovery_loaded(&mut self, id: DocId) {
        let Some(r) = self.recovery.as_mut() else { return };
        let Some(i) = r.after_load.iter().position(|(d, _)| *d == id) else { return };
        let (_, f) = r.after_load.swap_remove(i);
        put(self, id, &f);
    }
}

#[cfg(test)]
mod tests;
