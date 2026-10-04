//! Syncing with disk (like IntelliJ) — notice when a file changes elsewhere (git checkout, formatter, agent):
//! - If I changed nothing, **quietly reload**. No wholesale swap — only the changed range, as a transaction —
//!   cursor, scroll and undo stay (`u` even goes back to before the disk change).
//! - If I changed something, it's a conflict — notification + header mark, `:reload` (theirs) / `:w!` (mine).
//! - Auto-save: when the terminal loses focus (`editor.auto-save = "focus"`, default); with `"idle"`, also
//!   after 2 s without input. Files in conflict are never auto-saved.
//!
//! Watching = a worker thread waits for the open files to change (inotify/kqueue — `fswatch`), then stats
//! them (the main loop never waits on disk); plus a stat at once on regaining focus.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use ropey::Rope;

use crate::document::DocId;
use crate::editor::Editor;
use crate::transaction::{Change, Transaction};

/// Fingerprint of a disk file: (mtime, size).
pub type Stamp = (SystemTime, u64);

const IDLE_SAVE: Duration = Duration::from_secs(2);
/// Files larger than this aren't reloaded automatically (notification only).
const RELOAD_MAX: u64 = 16 << 20;
/// How long auto-save waits for Claude to write an edit it was allowed (it writes within milliseconds; a
/// claude that died meanwhile shouldn't keep the file unsaved for long).
const AGENT_WRITE_WAIT: Duration = Duration::from_secs(10);

pub fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// `text` is `body` and a newline — what Claude Code writes of an accepted edit lacks the last one.
pub fn is_plus_newline(text: &Rope, body: ropey::RopeSlice) -> bool {
    let n = body.len_bytes();
    text.len_bytes() == n + 1 && text.byte(n) == b'\n' && text.byte_slice(..n) == body
}

/// Byte `i` is in the middle of a UTF-8 char.
pub fn is_cont(text: &Rope, i: usize) -> bool {
    i < text.len_bytes() && text.byte(i) & 0xC0 == 0x80
}

/// The smallest single change turning old text into new (middle minus common prefix/suffix, on char
/// boundaries). None if equal.
pub fn minimal_change(old: &Rope, new: &Rope) -> Option<Change> {
    let (ol, nl) = (old.len_bytes(), new.len_bytes());
    let mut pre = common_run(old.chunks(), new.chunks(), usize::MAX, false);
    if pre == ol && pre == nl {
        return None;
    }
    while pre > 0 && (is_cont(old, pre) || is_cont(new, pre)) {
        pre -= 1;
    }
    let max_suf = (ol - pre).min(nl - pre);
    let mut suf = common_run(chunks_back(old), chunks_back(new), max_suf, true);
    while suf > 0 && (is_cont(old, ol - suf) || is_cont(new, nl - suf)) {
        suf -= 1;
    }
    Some(Change { from: pre, to: ol - suf, insert: new.byte_slice(pre..nl - suf).to_string() })
}

/// `text`'s chunks, last first.
fn chunks_back(text: &Rope) -> impl Iterator<Item = &str> {
    let mut chunks = text.chunks_at_byte(text.len_bytes()).0;
    std::iter::from_fn(move || chunks.prev())
}

/// How many bytes two texts, given as chunks, have in common from their start — or from their end
/// (`from_end`: chunks come last first) — up to `max`. By memcmp a chunk at a time, and chunks at the same
/// address (an undo snapshot shares most of its text with the next) are equal without looking.
fn common_run<'a>(
    mut a: impl Iterator<Item = &'a str>,
    mut b: impl Iterator<Item = &'a str>,
    max: usize,
    from_end: bool,
) -> usize {
    let (mut x, mut y): (&[u8], &[u8]) = (&[], &[]);
    let mut n = 0;
    while n < max {
        if x.is_empty() {
            let Some(c) = a.next() else { break };
            x = c.as_bytes();
        }
        if y.is_empty() {
            let Some(c) = b.next() else { break };
            y = c.as_bytes();
        }
        let k = x.len().min(y.len()).min(max - n);
        let (p, q) = if from_end { (&x[x.len() - k..], &y[y.len() - k..]) } else { (&x[..k], &y[..k]) };
        if std::ptr::eq(p, q) || p == q {
            n += k;
            (x, y) = if from_end { (&x[..x.len() - k], &y[..y.len() - k]) } else { (&x[k..], &y[k..]) };
            continue;
        }
        let pairs = p.iter().zip(q);
        n += if from_end {
            pairs.rev().take_while(|(i, j)| i == j).count()
        } else {
            pairs.take_while(|(i, j)| i == j).count()
        };
        break;
    }
    n
}

/// Replace `path` without a window where it's half-written: temp file in the same directory (permissions
/// of the old file) → fsync → rename. Symlinks are followed (the target is replaced, the link stays).
/// If the directory isn't writable, falls back to writing in place.
pub fn write_atomic(
    path: &Path,
    fill: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    let mut target = path.to_path_buf();
    for _ in 0..40 {
        let Ok(link) = std::fs::read_link(&target) else { break };
        target = match target.parent() {
            Some(dir) => dir.join(link),
            None => link,
        };
    }
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Numbered so back-to-back writes of one file (worker threads) don't collide
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tarae-{}-{n}.tmp", std::process::id()));
    let f = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
        Ok(f) => f,
        Err(_) => {
            let mut w = BufWriter::new(File::create(&target)?);
            fill(&mut w)?;
            return w.into_inner().map_err(|e| e.into_error())?.sync_all();
        }
    };
    let result = (|| {
        if let Ok(m) = std::fs::metadata(&target) {
            f.set_permissions(m.permissions())?;
        }
        let mut w = BufWriter::new(f);
        fill(&mut w)?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// What the worker thread returns: document, fingerprint it compared against, current fingerprint, content
/// (if changed and worth reading).
type Stat = (DocId, Option<Stamp>, Option<Stamp>, Option<Rope>);

impl Editor {
    /// Start watching (once at startup — it keeps itself going after that).
    pub fn disk_watch(&mut self) {
        self.disk_poll(true);
    }

    /// On every event: a file opened, closed, loaded or renamed since the waiting round began — a new round
    /// that watches it.
    pub fn disk_sync(&mut self) {
        let Some((_, watched)) = &self.disk_round else { return };
        let now = self.docs.iter().filter(|d| watchable(d)).filter_map(|d| Some((d.id, d.path.as_ref()?)));
        if !now.eq(watched.iter().map(|(id, p)| (*id, p))) {
            self.disk_poll(true);
        }
    }

    /// A disk round: stat the open files — at once, or (`wait`) once they may have changed. It continues
    /// the watch unless a newer round took over meanwhile (and ends its wait) — so an immediate check never
    /// leaves a second watch running beside the first.
    fn disk_poll(&mut self, wait: bool) {
        self.disk_poll_gen += 1;
        let generation = self.disk_poll_gen;
        let files: Vec<(DocId, PathBuf, Option<Stamp>)> = self
            .docs
            .iter()
            .filter(|d| watchable(d))
            .filter_map(|d| Some((d.id, d.path.clone()?, d.disk)))
            .collect();
        let waker = crate::fswatch::Waker::new();
        let watched = files.iter().map(|(id, p, _)| (*id, p.clone())).collect();
        if let Some((old, _)) = self.disk_round.replace((waker.clone(), watched)) {
            old.wake();
        }
        // (every session's files: applied in the one in front — never swapped into the session that happened
        // to start the round, whose name its toasts then carried, #59)
        self.events.shared_jobs().spawn(move || {
            let stats: Vec<Stat> = loop {
                // Armed before the stat: a change made while it runs still ends the wait
                let mut watcher = wait.then(|| crate::fswatch::Watcher::with_waker(waker.clone()));
                if let Some(w) = watcher.as_mut() {
                    files.iter().for_each(|(_, p, _)| w.file(p));
                }
                let stats: Vec<Stat> = files
                    .iter()
                    .filter_map(|(id, path, known)| {
                        let now = stamp(path);
                        if now == *known {
                            return None;
                        }
                        let body = now
                            .filter(|(_, len)| *len <= RELOAD_MAX)
                            .and_then(|_| std::fs::read_to_string(path).ok())
                            .map(|body| Rope::from_str(&body)); // (here, not on the main loop — ~1 ms a MB)
                        Some((*id, *known, now, body))
                    })
                    .collect();
                // (a change that isn't in the stats — a neighbour's, or a folder that appeared: look again)
                if !stats.is_empty() || watcher.is_none_or(|mut w| w.wait() != crate::fswatch::Woke::Changed)
                {
                    break stats;
                }
            };
            move |ed: &mut Editor| {
                // (a backstop round that found nothing is nothing to draw)
                ed.unchanged = stats.is_empty();
                ed.on_disk(stats);
                if ed.disk_poll_gen == generation {
                    ed.disk_poll(true);
                }
            }
        });
    }

    fn on_disk(&mut self, stats: Vec<Stat>) {
        // Buffers that are the file on disk again after a write from outside (Claude's edit landing, a
        // checkout): saved, as far as the language servers go — rust-analyzer checks on a save (held while
        // a burst of writes goes on, `lsp_saved_outside`)
        let mut saved = Vec::new();
        let mut said: Vec<(DocId, String, bool)> = Vec::new();
        for (id, known, now, body) in stats {
            let Some(doc) = self.docs.iter_mut().find(|d| d.id == id) else { continue };
            // Saved (or reloaded) since the stat started — the change it saw may be our own write; or a save
            // of ours is being written right now
            if doc.disk != known || doc.saving > 0 {
                continue;
            }
            // Whatever it is, the write we were waiting for (if any) has happened
            doc.agent_writes = None;
            let name = doc.display_name_short();
            // The log follows along like `tail -f`: reloaded without a toast, the cursor kept at the end
            let log = crate::log::is_log(doc.path.as_deref());
            match (now, body) {
                // Deleted (or unreadable) — notify only once
                (None, _) => {
                    if doc.disk.is_some() {
                        doc.disk = None;
                        said.push((id, format!("{name} was deleted on disk — :w to write it back"), true));
                    }
                }
                (Some(stamp), Some(body)) if !doc.is_modified() => {
                    let before = doc.text.len_lines();
                    let head = doc.selection().primary().cursor(&doc.text);
                    let at_end = crate::movement::line_of(&doc.text, head) + 1
                        >= crate::movement::last_line(&doc.text);
                    let changed = reload_text(doc, &body);
                    doc.disk = Some(stamp);
                    doc.disk_conflict = false;
                    if changed && !log {
                        saved.push(id);
                    }
                    if log {
                        if changed && at_end {
                            let last = crate::movement::last_line(&doc.text);
                            let line = last.saturating_sub(usize::from(doc.text.line(last).len_chars() == 0));
                            let pos = crate::movement::line_start(&doc.text, line);
                            doc.set_selection(crate::selection::Selection::point(pos));
                        }
                    } else if changed {
                        let delta = doc.text.len_lines() as isize - before as isize;
                        let lines = match delta {
                            0 => String::new(),
                            d => format!(" · {}{} lines", if d > 0 { "+" } else { "" }, d),
                        };
                        said.push((id, format!("{name} reloaded from disk{lines}"), false));
                    }
                }
                // Written outside = my buffer (Claude writing an edit I accepted, etc.) — counts as saved
                (Some(stamp), Some(body)) if doc.text == body => {
                    doc.disk = Some(stamp);
                    doc.disk_conflict = false;
                    doc.mark_saved();
                    saved.push(id);
                }
                // Only the trailing newline is missing — Claude Code does that when writing accepted content
                // (measured). Write back the buffer (= accepted content)
                (Some(stamp), Some(body))
                    if doc.is_modified() && is_plus_newline(&doc.text, body.slice(..)) =>
                {
                    doc.disk = Some(stamp);
                    match doc.save_as(true) {
                        Ok(_) => saved.push(id),
                        Err(e) => self.set_error(format!("{name}: {e:#}")),
                    }
                }
                (Some(stamp), _) => {
                    doc.disk = Some(stamp);
                    if !doc.disk_conflict {
                        doc.disk_conflict = true;
                        said.push((id, changed_while_editing(&name), true));
                    }
                }
            }
        }
        for (id, msg, warn) in said {
            self.disk_said(id, msg, warn);
        }
        for id in saved {
            self.lsp_saved_outside(id);
        }
    }

    /// What became of document `id` on disk, said for its session (#59): with several sessions, named after
    /// it; shown only when it's the one in front — a session behind's goes to `:messages`, and its tab, the
    /// chip and its row in the list get a `•` (#6, `Session.reloaded`).
    fn disk_said(&mut self, id: DocId, msg: String, warn: bool) {
        if self.sessions.is_empty() {
            return if warn { self.set_warning(msg) } else { self.set_status(msg) };
        }
        let front = self.session.docs.contains(&id);
        let mut behind = self.sessions.iter_mut().find(|l| l.docs.contains(&id) && !front);
        if let Some(l) = behind.as_deref_mut() {
            l.reloaded += 1;
        }
        let label = behind.as_deref().map_or_else(|| self.session.label(), |l| l.label()).to_string();
        let msg = format!("{label} · {msg}");
        let kind = if warn { crate::editor::ToastKind::Warning } else { crate::editor::ToastKind::Info };
        match (behind.is_some(), warn) {
            (true, _) => self.recorded(kind, msg),
            (false, true) => self.set_warning(msg),
            (false, false) => self.set_status(msg),
        }
    }

    /// What `path` is on disk, read elsewhere (`body` at `stamp` — the end of a turn whose edits went in
    /// unseen, `checkpoint::gather`): an open buffer without unsaved edits takes it now, as the watch would
    /// a moment later. One with unsaved edits is left to the watch (it says the file changed).
    pub fn disk_take(&mut self, path: &Path, body: &Rope, stamp: Option<Stamp>) {
        let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_deref() == Some(path)) else { return };
        if doc.is_modified() || doc.loading || stamp.is_none() || doc.disk == stamp {
            return;
        }
        reload_text(doc, body);
        doc.disk = stamp;
        doc.disk_conflict = false;
        doc.agent_writes = None;
    }

    /// `:reload` — take the disk content (my changes are discarded but recoverable with undo).
    pub fn reload_current(&mut self) -> Result<(), String> {
        let doc = self.doc_mut();
        let path = doc.path.clone().ok_or("no file name")?;
        let body = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        reload_text(doc, &Rope::from_str(&body));
        doc.disk = stamp(&path);
        doc.disk_conflict = false;
        let name = doc.display_name_short();
        self.set_status(format!("{name} reloaded from disk"));
        Ok(())
    }

    /// Terminal focus: auto-save on loss, check disk right away on regain.
    pub fn on_focus(&mut self, gained: bool) {
        if gained {
            // Check now — so changes show the moment focus returns (this round carries the watch on)
            self.disk_poll(false);
            self.git_reload_bases();
        } else if self.config.auto_save != "off" {
            self.auto_save("");
        }
    }

    /// When input stops (idle mode) — called on every event.
    pub fn auto_save_idle(&mut self) {
        if self.config.auto_save != "idle" || !self.docs.iter().any(|d| d.is_modified() && d.path.is_some()) {
            return;
        }
        self.idle_save_gen += 1;
        let generation = self.idle_save_gen;
        self.events.jobs().spawn(move || {
            std::thread::sleep(IDLE_SAVE);
            move |ed: &mut Editor| {
                if ed.idle_save_gen == generation {
                    ed.auto_save(" (idle)");
                }
            }
        });
    }

    /// Save all modified files (skipping conflicts, loading, and unnamed ones) — in the background: "Saved"
    /// and what follows a save come when they're written (`saver.rs`). Before running code on disk (tests, a
    /// task), start it once they're written with `after_saves`.
    pub(crate) fn auto_save(&mut self, why: &'static str) {
        let (mut failed, mut conflicts) = (Vec::new(), Vec::new());
        for i in 0..self.docs.len() {
            let d = &self.docs[i];
            if !d.is_modified() || d.path.is_none() || d.loading || d.disk_conflict {
                continue;
            }
            if d.agent_writes.is_some_and(|(until, _)| std::time::Instant::now() < until) {
                continue; // Claude writes it
            }
            let (id, name) = (d.id, d.display_name_short());
            // A config file that doesn't parse stays in the buffer (#19: a half-edited ~/.cargo/config.toml
            // broke every cargo at once) — said once per version of it; `:w` writes it anyway
            let held =
                d.path.as_deref().and_then(|p| unparsed_config(p, &d.text)).map(|why| (why, d.version()));
            if let Some((why, version)) = held {
                if !self.auto_held.contains(&(id, version)) {
                    self.auto_held.retain(|(doc, _)| *doc != id);
                    self.auto_held.push((id, version));
                    self.set_warning(format!(
                        "{name} not auto-saved: it doesn't parse ({why}) — :w writes it anyway"
                    ));
                }
                continue;
            }
            match self.save_in_background(id, false, crate::saver::Then::Auto(why)) {
                Ok(()) => {}
                // Changed on disk to something else — the same conflict the watch reports, not a failure
                Err(_) if self.docs[i].disk_conflict => conflicts.push(name),
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }
        for name in conflicts {
            self.set_warning(changed_while_editing(&name));
        }
        if !failed.is_empty() {
            self.set_error(format!("auto-save failed — {}", failed.join("; ")));
        }
    }
}

/// Whether the disk watch looks at this document: not while it loads, nor while a save of it is being
/// written — its file is changing because of us, against a fingerprint that save will replace (a round
/// comparing with the old one would end at once, over and over, until the save is done). The save's end
/// changes the watched set, and `disk_sync` starts a round with the new fingerprint.
fn watchable(d: &crate::document::Document) -> bool {
    !d.loading && d.saving == 0
}

/// Config files whose readers take them as soon as they're on disk — cargo, tarae, npm, Claude Code — and
/// break at once on a syntax error, by name (or the folder it's in). JSON with comments (`tsconfig.json`,
/// VS Code's) isn't among them: a strict parser would call it broken. YAML neither: no parser for it here.
const TOML_CONFIGS: [&str; 11] = [
    "Cargo.toml",
    "rust-toolchain.toml",
    "pyproject.toml",
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    ".clippy.toml",
    "deny.toml",
    ".tarae.toml",
    ".tarae.local.toml",
    "Pipfile",
];
const JSON_CONFIGS: [&str; 3] = ["package.json", "composer.json", ".claude.json"];

/// The most text a parse check reads (a config this big is something else).
const CONFIG_CHECK_MAX: usize = 512 << 10;

/// Why `text` doesn't parse, when `path` is a known config file (`TOML_CONFIGS`, `JSON_CONFIGS`, cargo's
/// `.cargo/config(.toml)`, tarae's own `tarae/*.toml`, Claude Code's `.claude/settings*.json`) — its
/// parser's first line. None for anything else, or one that parses.
pub fn unparsed_config(path: &Path, text: &ropey::Rope) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let dir = path.parent().and_then(Path::file_name).and_then(|d| d.to_str()).unwrap_or_default();
    let toml = TOML_CONFIGS.contains(&name)
        || (dir == ".cargo" && matches!(name, "config.toml" | "config" | "credentials.toml"))
        || (dir == "tarae" && name.ends_with(".toml"))
        || path.parent().and_then(Path::parent).is_some_and(|p| p.ends_with("tarae") && dir == "themes");
    let json = JSON_CONFIGS.contains(&name)
        || (dir == ".claude" && name.starts_with("settings") && name.ends_with(".json"));
    if (!toml && !json) || text.len_bytes() > CONFIG_CHECK_MAX {
        return None;
    }
    let s = text.to_string();
    let err = match toml {
        true => toml::from_str::<toml::Table>(&s).err().map(|e| e.message().to_string()),
        false => serde_json::from_str::<serde_json::Value>(&s).err().map(|e| e.to_string()),
    }?;
    Some(err.lines().next().unwrap_or("a syntax error").trim().to_string())
}

fn changed_while_editing(name: &str) -> String {
    format!("{name} changed on disk while you were editing — :reload takes theirs, :w! keeps yours")
}

impl crate::document::Document {
    /// Claude is about to write this buffer's text to the file — see `agent_writes`.
    pub fn agent_will_write(&mut self) {
        self.agent_writes = Some((std::time::Instant::now() + AGENT_WRITE_WAIT, self.version()));
    }

    /// The buffer differs from disk only by an accepted edit Claude is writing (nothing typed since) — its
    /// next edit goes on top of that, as on disk once the write lands.
    pub fn unsaved_is_claudes(&self) -> bool {
        self.agent_writes.is_some_and(|(until, v)| v == self.version() && std::time::Instant::now() < until)
    }
}

/// Replace the document with new content — changed range only, marked as saved. true if it changed.
pub(crate) fn reload_text(doc: &mut crate::document::Document, body: &Rope) -> bool {
    let Some(change) = minimal_change(&doc.text, body) else {
        doc.mark_saved();
        return false;
    };
    let before = doc.snapshot();
    let tx = Transaction::new(vec![change]);
    let sel = tx.map_selection(doc.selection());
    doc.apply_with(&tx, sel);
    doc.commit_undo(before); // u = back to before the disk change
    doc.mark_saved();
    true
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

    /// One immediate watch round — processes events until it's applied (it then re-arms the watch). The
    /// round it ended may land first.
    fn poll_once(ed: &mut Editor) {
        ed.disk_poll(false);
        let round = ed.disk_poll_gen;
        while ed.disk_poll_gen == round {
            let ev = ed.events.recv_timeout(Duration::from_secs(5)).expect("disk stat");
            ed.handle_event(ev);
        }
    }

    /// A Rust file open with a stand-in server (`cat` into `sent.log`), up: writes from outside, and the
    /// didSaves it was sent.
    struct Saving {
        ed: Editor,
        dir: PathBuf,
        file: PathBuf,
        id: crate::document::DocId,
    }

    impl Saving {
        fn new(name: &str) -> Saving {
            let dir = std::env::temp_dir().join(format!("tarae-disk-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("main.rs");
            std::fs::write(&file, "fn a() {}\n").unwrap();
            std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
            let cfg = format!(
                "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n[lang.rust]\nlsp = [\"fake\"]\n",
                dir.join("sent.log").display()
            );
            let (config, _) = crate::config::parse(&cfg);
            let mut ed = Editor::new(config);
            ed.open(&file).unwrap();
            ed.session.root = dir.clone();
            let (id, cid) = (ed.doc().id, ed.doc().lsp.client.expect("attached"));
            ed.on_lsp_message(cid, serde_json::json!({ "id": 0, "result": { "capabilities": {} } }));
            let file = std::fs::canonicalize(&file).unwrap();
            Saving { ed, dir, file, id }
        }

        /// `body` written to the file from outside, as the disk watch sees it.
        fn outside(&mut self, body: &str) {
            std::fs::write(&self.file, body).unwrap();
            let known = self.ed.doc().disk;
            self.ed.on_disk(vec![(self.id, known, stamp(&self.file), Some(body.into()))]);
        }

        /// The editor's events for `ms` (its timers land).
        fn pump(&mut self, ms: u64) {
            let end = std::time::Instant::now() + Duration::from_millis(ms);
            while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
                if let Some(ev) = self.ed.events.recv_timeout(left.min(Duration::from_millis(10))) {
                    self.ed.handle_event(ev);
                }
            }
        }

        /// The didSaves sent so far, after a fixed while for the writer thread — for checking no more went
        /// out (a slow runner only makes that easier). For one that should have, `saves_reach`.
        fn saves(&self) -> usize {
            std::thread::sleep(Duration::from_millis(150)); // (the writer thread)
            self.saves_now()
        }

        fn saves_now(&self) -> usize {
            let sent = std::fs::read_to_string(self.dir.join("sent.log")).unwrap_or_default();
            sent.matches("textDocument/didSave").count()
        }

        /// The didSaves sent, once there are `n` — waited for, not a fixed while (the writer thread, and
        /// with `pump` the editor's timers too, can take longer on a slow runner); after 10 s, what there is.
        /// Without `pump` no event is handled: what's counted went out before any.
        fn saves_reach(&mut self, n: usize, pump: bool) -> usize {
            let end = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let count = self.saves_now();
                if count >= n || std::time::Instant::now() >= end {
                    return count;
                }
                match pump {
                    true => self.pump(10),
                    false => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }
    }

    /// A write from outside that the buffer takes — reloaded, or Claude writing the edit it already holds —
    /// is a save to the servers too (rust-analyzer checks on a save; `diagnostics` waits on that check), held
    /// until the file has been quiet: a burst of four writes is one. A write the buffer doesn't take (a
    /// conflict) is none; the user's own `:w` goes at once, and a held one goes with it. (The burst's writes
    /// are 30 ms apart over longer than `SAVE_QUIET` — so the quiet restarts with each; a try where a gap
    /// stretched to half of it (a stalled runner) wasn't one burst — tried again.)
    #[test]
    fn a_burst_of_writes_from_outside_is_one_save() {
        const TRIES: u32 = 5;
        let mut s = Saving::new("burst");
        s.outside("fn b() {}\n");
        assert_eq!(s.ed.doc().text.to_string(), "fn b() {}\n");
        assert_eq!(s.saves(), 0, "held for quiet");
        assert_eq!(s.saves_reach(1, true), 1, "reloaded, then quiet");
        for attempt in 1.. {
            let before = s.saves();
            let mut gap = Duration::ZERO;
            for body in ["fn c() {}\n", "fn d() {}\n", "fn e() {}\n", "fn f() {}\n"] {
                let t = std::time::Instant::now();
                s.outside(body);
                s.pump(30);
                gap = gap.max(t.elapsed());
            }
            if gap >= crate::lsp_editor::SAVE_QUIET / 2 {
                assert!(attempt < TRIES, "a write {gap:?} after the last in each of {TRIES} tries");
                // (what that try sent — mid-burst and after it — all out before the next counts)
                s.saves_reach(before + 1, true);
                s.pump(500);
                continue;
            }
            assert_eq!(s.saves(), before, "not during the burst");
            assert_eq!(s.saves_reach(before + 1, true), before + 1, "one for the burst");
            break;
        }
        let n = s.saves();
        feed(&mut s.ed, "ggic<esc>");
        let mine = s.ed.doc().text.to_string();
        s.outside(&mine);
        assert!(!s.ed.doc().is_modified());
        assert_eq!(s.saves_reach(n + 1, true), n + 1, "the buffer's text written");
        // (an edit right after a save sends the save again — `lsp_editor::RESAVE_WITHIN`; this one comes later)
        std::thread::sleep(Duration::from_millis(150));
        feed(&mut s.ed, "ggid<esc>");
        s.outside("something else\n");
        assert!(s.ed.doc().disk_conflict);
        s.pump(300);
        assert_eq!(s.saves(), n + 1, "a conflict is no save");
        // The user's :w: at once — a held one doesn't follow it
        crate::typed::execute(&mut s.ed, "reload").unwrap();
        s.outside("fn g() {}\n");
        crate::typed::execute(&mut s.ed, "w!").unwrap();
        // (its didSave once the write is done — the saver's worker, an event)
        assert_eq!(s.saves_reach(n + 2, true), n + 2, ":w is sent");
        s.pump(300);
        assert_eq!(s.saves(), n + 2, "and the held one went with it");
        std::fs::remove_dir_all(&s.dir).ok();
    }

    /// `diagnostics` sends the held saves first — the check they start is what it waits for.
    #[test]
    fn diagnostics_sends_the_held_saves_first() {
        let mut s = Saving::new("diags");
        s.outside("fn b() {}\n");
        assert_eq!(s.saves(), 0, "held");
        let args = serde_json::json!({ "paths": ["main.rs"], "wait": 1 });
        crate::nav::diagnostics(&mut s.ed, crate::tools::Call::test(1), &args).unwrap();
        assert_eq!(s.saves_reach(1, false), 1, "sent as it's asked, before any wait");
        s.pump(300);
        assert_eq!(s.saves(), 1, "and not again");
        std::fs::remove_dir_all(&s.dir).ok();
    }

    /// While a command or tests of Claude's build, a held save waits for its end — a check then would hold
    /// the build dir's lock against it.
    #[test]
    fn a_write_during_claudes_run_is_saved_after_it() {
        let mut s = Saving::new("run");
        let task = crate::task::Task { name: "cargo test".into(), cmd: "true".into(), cwd: s.dir.clone() };
        let mut run = crate::task::Run::for_test(task);
        (run.state, run.by_claude) = (crate::task::State::Running, true);
        s.ed.session.task_run = Some(run);
        s.outside("fn b() {}\n");
        s.pump(400);
        assert_eq!(s.saves(), 0, "not while it builds");
        // Its end comes as an event, as a real run's does
        s.ed.events.jobs().spawn(|| {
            |ed: &mut Editor| {
                if let Some(r) = ed.session.task_run.as_mut() {
                    r.state = crate::task::State::Passed;
                }
            }
        });
        assert_eq!(s.saves_reach(1, true), 1, "at its end");
        // The user's own run doesn't hold it
        s.ed.session.task_run.as_mut().unwrap().state = crate::task::State::Running;
        s.ed.session.task_run.as_mut().unwrap().by_claude = false;
        s.outside("fn c() {}\n");
        assert_eq!(s.saves_reach(2, true), 2);
        std::fs::remove_dir_all(&s.dir).ok();
    }

    /// The open log follows new lines quietly — a toast would be logged, change the file, and loop.
    #[test]
    fn open_log_follows_without_toasts() {
        let Some(log) = crate::log::path() else { return };
        let dir = std::env::temp_dir().join(format!("tarae-logfollow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.log");
        std::fs::write(&file, "a\nb\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&file).unwrap();
        let id = ed.doc().id;
        feed(&mut ed, "ge");
        ed.doc_mut().path = Some(log); // stands in for the log (tests never write the real one)
        std::fs::write(&file, "a\nb\nc\nd\n").unwrap();
        let known = ed.doc().disk;
        ed.on_disk(vec![(id, known, stamp(&file), Some("a\nb\nc\nd\n".into()))]);
        assert_eq!(ed.doc().text.to_string(), "a\nb\nc\nd\n");
        assert!(ed.status.is_none() && ed.toasts.is_empty(), "no toast");
        let head = ed.doc().selection().primary().head;
        assert_eq!(ed.doc().text.byte_to_line(head), 3, "followed to the new last line");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #59: with several sessions, a reload is said for the session whose buffer it is — named after it,
    /// and shown only when that's the one in front; a session behind's goes to `:messages` (it showed in
    /// front with no name, once as another session's).
    #[test]
    fn a_reload_is_said_for_its_own_session() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-sessions-{}", std::process::id()));
        for s in ["front", "back"] {
            std::fs::create_dir_all(dir.join(s)).unwrap();
            std::fs::write(dir.join(s).join(format!("{s}.txt")), "one\n").unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.join("front");
        let back = ed.session_open(&dir.join("back")).unwrap();
        ed.open(&dir.join("back/back.txt")).unwrap();
        let back_doc = ed.doc().id;
        assert!(ed.session_switch(0));
        ed.open(&dir.join("front/front.txt")).unwrap();
        let front_doc = ed.doc().id;
        let stats: Vec<Stat> = [(front_doc, "front"), (back_doc, "back")]
            .iter()
            .map(|&(id, s)| {
                let path = dir.join(s).join(format!("{s}.txt"));
                std::fs::write(&path, "one\ntwo\n").unwrap();
                let known = ed.docs.iter().find(|d| d.id == id).unwrap().disk;
                (id, known, stamp(&path), Some(Rope::from_str("one\ntwo\n")))
            })
            .collect();
        ed.toasts.clear();
        ed.on_disk(stats);
        let shown: Vec<String> = ed.toasts.iter().map(|t| t.text.clone()).collect();
        let front = ed.session.label().to_string();
        assert_eq!(shown, [format!("{front} · front.txt reloaded from disk · +1 lines")]);
        let back_label = ed.sessions.iter().find(|l| l.id == back).unwrap().label().to_string();
        assert!(
            ed.messages
                .iter()
                .any(|m| m.2 == format!("{back_label} · back.txt reloaded from disk · +1 lines")),
            "the session behind's, in :messages: {:?}",
            ed.messages.iter().map(|m| &m.2).collect::<Vec<_>>()
        );
        // …and marked — the chip, its row in the list — until it comes to the front (#6)
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 24).unwrap();
            let mut parser = vt100::Parser::new(24, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        assert_eq!(ed.sessions.iter().find(|l| l.id == back).unwrap().reloaded, 1);
        assert_eq!(ed.session.reloaded, 0, "the front's was said");
        // (the status line: the screen's last row)
        let chip = |ed: &mut Editor| screen(ed).lines().nth(23).unwrap_or_default().to_string();
        assert!(chip(&mut ed).contains(&format!("{front} •1")), "{}", chip(&mut ed));
        ed.session_list();
        let s = screen(&mut ed);
        let row = s.lines().find(|l| l.contains(&back_label)).unwrap_or_default();
        assert!(row.contains("• 1 reloaded"), "{s}");
        ed.picker = None;
        assert!(ed.session_switch(back));
        assert!(ed.session_switch(0));
        assert!(!chip(&mut ed).contains('•'), "seen: {}", chip(&mut ed));
        // A round started while the other session was in front lands in the one in front now — said there,
        // named after it (not swapped into the one that started it)
        assert!(ed.session_switch(back));
        std::fs::write(dir.join("front/front.txt"), "one\ntwo\nthree\n").unwrap();
        ed.disk_poll(false);
        assert!(ed.session_switch(0));
        ed.toasts.clear();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let reloaded = |ed: &Editor| ed.docs.iter().any(|d| d.id == front_doc && d.text.len_lines() == 4);
        while !reloaded(&ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
        let shown: Vec<String> = ed.toasts.iter().map(|t| t.text.clone()).collect();
        assert_eq!(shown, [format!("{front} · front.txt reloaded from disk · +1 lines")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn external_changes_reload_conflict_and_autosave() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        feed(&mut ed, "jj"); // cursor = 'c'
        // A line added at the top from outside → nothing changed here, so reload; cursor follows 'c'
        std::fs::write(&path, "x\na\nb\nc\n").unwrap();
        poll_once(&mut ed);
        assert_eq!(ed.doc().text.to_string(), "x\na\nb\nc\n");
        assert_eq!(ed.doc().selection().primary().cursor(&ed.doc().text), 6);
        assert!(!ed.doc().is_modified(), "matches disk");
        assert!(ed.toasts.iter().any(|t| t.text.contains("reloaded")));
        // u = back to before the disk change (now differs from disk)
        feed(&mut ed, "u");
        assert_eq!(ed.doc().text.to_string(), "a\nb\nc\n");
        assert!(ed.doc().is_modified());
        // Changed here and disk changed too → conflict (don't overwrite)
        std::fs::write(&path, "theirs\n").unwrap();
        poll_once(&mut ed);
        assert!(ed.doc().disk_conflict);
        assert_eq!(ed.doc().text.to_string(), "a\nb\nc\n", "my text untouched");
        feed(&mut ed, ":w<ret>");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n", ":w refused during conflict");
        feed(&mut ed, ":reload<ret>");
        assert_eq!(ed.doc().text.to_string(), "theirs\n");
        assert!(!ed.doc().disk_conflict && !ed.doc().is_modified());
        // Losing focus auto-saves
        feed(&mut ed, "ggimine <esc>");
        ed.on_focus(false);
        ed.flush_saves(); // (saves are written in the background)
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine theirs\n");
        assert!(!ed.doc().is_modified());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #19: auto-save holds a known config file that doesn't parse — a half-edited `.cargo/config.toml` broke
    /// every cargo at once — and says so, once per change; `:w` writes it anyway, and once it parses again
    /// auto-save writes it. Other files, and configs that parse, save as ever. (Its own temp folder.)
    #[test]
    fn autosave_holds_a_config_that_doesnt_parse() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-config-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        let path = dir.join(".cargo/config.toml");
        std::fs::write(&path, "[build]\njobs = 4\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        // Half way through an edit: a key with no value yet
        feed(&mut ed, "geoincremental = <esc>");
        let held = |ed: &Editor| {
            ed.toasts.iter().filter(|t| t.text.contains("not auto-saved: it doesn't parse")).count()
        };
        ed.on_focus(false);
        ed.flush_saves();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[build]\njobs = 4\n",
            "held: the disk as it was"
        );
        assert!(
            ed.doc().is_modified() && held(&ed) == 1,
            "said: {:?}",
            ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        let said = ed.toasts.iter().find(|t| t.text.contains("doesn't parse")).unwrap().text.clone();
        assert!(
            said.starts_with("config.toml not auto-saved") && said.ends_with(":w writes it anyway"),
            "{said}"
        );
        ed.on_focus(false);
        assert_eq!(held(&ed), 1, "said once for the same text");
        // Finished: it parses, and auto-save writes it
        feed(&mut ed, "Afalse<esc>");
        ed.on_focus(false);
        ed.flush_saves();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[build]\njobs = 4\nincremental = false\n");
        // Broken again: `:w` writes it anyway
        feed(&mut ed, "ojobs = <esc>:w<ret>");
        ed.flush_saves();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("jobs = \n") && !ed.doc().is_modified());
        // Which files: by name and folder; JSON too; anything else (or JSON with comments) never
        let text = |s: &str| ropey::Rope::from_str(s);
        let at = |p: &str| dir.join(p);
        assert!(unparsed_config(&at("Cargo.toml"), &text("[package\n")).is_some());
        assert!(unparsed_config(&at("Cargo.toml"), &text("[package]\nname = \"x\"\n")).is_none());
        assert!(unparsed_config(&at("package.json"), &text("{\"a\": 1,}")).is_some());
        assert!(unparsed_config(&at(".claude/settings.local.json"), &text("{")).is_some());
        assert!(unparsed_config(&at("x/tarae/config.toml"), &text("theme = ")).is_some());
        assert!(unparsed_config(&at(".tarae.local.toml"), &text("[x")).is_some());
        for (p, s) in [("notes.toml", "[x"), ("tsconfig.json", "{ // a comment\n}"), ("a.rs", "fn (")] {
            assert!(unparsed_config(&at(p), &text(s)).is_none(), "{p}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude writes the edit I just accepted, and focus leaves (off to Claude's pane) before the watch has
    /// seen the write: the disk holds my buffer, so it's a save — not "auto-save failed" (measured, often).
    #[test]
    fn autosave_right_after_an_agent_write_is_a_save() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        feed(&mut ed, "wcTWOO<esc>"); // the accepted edit, in the buffer
        std::fs::write(&path, "TWOO").unwrap(); // Claude writes it (without the trailing newline)
        ed.on_focus(false);
        ed.flush_saves(); // (saves are written in the background)
        assert!(
            !ed.toasts.iter().any(|t| t.kind == ToastKind::Error),
            "no failure toast: {:?}",
            ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TWOO\n", "saved, the newline back");
        assert!(!ed.doc().is_modified() && !ed.doc().disk_conflict);
        // :w in the same moment — the same
        feed(&mut ed, "ggwcTHREE<esc>");
        std::fs::write(&path, "THREE\n").unwrap();
        feed(&mut ed, ":w<ret>");
        ed.flush_saves();
        assert!(!ed.doc().is_modified() && !ed.doc().disk_conflict);
        // A real conflict (the disk differs from my buffer) is the watch's warning, not an auto-save error
        feed(&mut ed, "ggcmine<esc>");
        std::fs::write(&path, "theirs, longer\n").unwrap();
        ed.on_focus(false);
        assert!(ed.doc().disk_conflict);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs, longer\n", "theirs not overwritten");
        assert!(!ed.toasts.iter().any(|t| t.kind == ToastKind::Error));
        assert!(ed.toasts.iter().any(|t| t.text.contains("changed on disk while you were editing")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// While Claude's write of an accepted edit is pending, auto-save leaves the file alone; the write
    /// landing (seen by the watch) ends the wait, and so does running out of time.
    #[test]
    fn autosave_waits_for_claudes_write() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-agentwait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        feed(&mut ed, "wcTWOO<esc>");
        ed.doc_mut().agent_will_write();
        ed.on_focus(false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n", "left to Claude");
        std::fs::write(&path, "TWOO\n").unwrap();
        poll_once(&mut ed);
        assert!(!ed.doc().is_modified() && ed.doc().agent_writes.is_none(), "its write is the save");
        // A claude that never writes: after the wait, auto-save does it
        feed(&mut ed, "ggwcTHREE<esc>");
        let v = ed.doc().version();
        ed.doc_mut().agent_writes = Some((std::time::Instant::now() - Duration::from_millis(1), v));
        ed.on_focus(false);
        ed.flush_saves();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "THREE\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every focus regain checks the disk at once — without adding a watch that never ends (they piled up:
    /// one more stat round every 0.5 s, and one more wakeup, per focus switch).
    #[test]
    fn focus_switches_keep_one_disk_watch() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "a\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        ed.disk_watch();
        for _ in 0..5 {
            ed.on_focus(true);
        }
        // Run the watch for a while: count the rounds still going once the immediate checks are done
        let end = std::time::Instant::now() + Duration::from_millis(1600);
        let mut rounds = 0;
        while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
            let Some(ev) = ed.events.recv_timeout(left) else { break };
            ed.handle_event(ev);
            rounds += 1;
        }
        // The first round (ended by the first check) + 5 immediate ones — then the one watch waits for a change
        assert!(rounds <= 7, "{rounds} disk rounds in 1.6 s");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file opened after the watch started is watched too — its change shows long before the backstop.
    #[test]
    fn files_opened_later_are_watched() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-later-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&b, "b\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&a).unwrap();
        ed.disk_watch();
        ed.open(&b).unwrap();
        ed.disk_sync(); // (the main loop does this after every event)
        std::thread::sleep(Duration::from_millis(100)); // the new round arms its watch
        std::fs::write(&b, "b changed\n").unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(3);
        while ed.doc().text != "b changed\n" {
            let left = end.checked_duration_since(std::time::Instant::now()).expect("b reloaded in time");
            if let Some(ev) = ed.events.recv_timeout(left) {
                ed.handle_event(ev);
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Agent writing the same content as my buffer is "saved", not a conflict — if only the trailing
    /// newline is missing, write it back.
    #[test]
    fn agent_writes_matching_buffer_are_saves() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        feed(&mut ed, "wcTWOO<esc>"); // length must change so the fingerprint (size, mtime) surely differs
        assert!(ed.doc().is_modified());
        assert_eq!(ed.doc().text.to_string(), "TWOO\n");
        std::fs::write(&path, "TWOO\n").unwrap();
        poll_once(&mut ed);
        assert!(!ed.doc().is_modified() && !ed.doc().disk_conflict, "same content = saved");
        feed(&mut ed, "ggwcTHREEE<esc>");
        // Without trailing newline — put in place whole: the watch is waiting now, and could catch a write in
        // place half done (truncated, not yet written) on a loaded machine
        std::fs::write(dir.join("tmp"), "THREEE").unwrap();
        std::fs::rename(dir.join("tmp"), &path).unwrap();
        poll_once(&mut ed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "THREEE\n", "newline written back");
        assert!(!ed.doc().is_modified() && !ed.doc().disk_conflict);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file being saved isn't watched until the save is done — then it is again, with the fingerprint the
    /// save left (a round comparing with the old one would end at once, over and over).
    #[test]
    fn a_file_being_saved_leaves_the_watch_until_its_save_is_done() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-unwatched-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        ed.disk_watch();
        let id = ed.doc().id;
        let watched = |ed: &Editor| ed.disk_round.as_ref().unwrap().1.iter().any(|(i, _)| *i == id);
        assert!(watched(&ed));
        ed.doc_mut().saving = 1; // (as `save_in_background` leaves it until the save is done)
        ed.disk_sync();
        assert!(!watched(&ed), "watched while being saved");
        ed.doc_mut().saving = 0;
        ed.disk_sync();
        assert!(watched(&ed), "watched again once saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stat round that lands while a save of ours is being written leaves that file alone — the change it
    /// saw is the save, not someone else's (the save's own end sets the new fingerprint).
    #[test]
    fn the_watch_leaves_a_file_being_saved_alone() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-saving-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        feed(&mut ed, "cTWO<esc>");
        let (id, known) = (ed.doc().id, ed.doc().disk);
        ed.doc_mut().saving = 1;
        std::fs::write(&path, "something else, longer\n").unwrap();
        let body = Rope::from_str("something else, longer\n");
        ed.on_disk(vec![(id, known, stamp(&path), Some(body))]);
        assert!(!ed.doc().disk_conflict, "taken for someone else's change");
        assert_eq!(ed.doc().text.to_string(), "TWOne\n");
        ed.doc_mut().saving = 0;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A watch round that started before `:w` must not report our own write as a conflict.
    #[test]
    fn own_save_during_watch_is_not_a_conflict() {
        let dir = std::env::temp_dir().join(format!("tarae-disk-own-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&path).unwrap();
        ed.disk_poll(true); // captured the old stamp; stats after the save below
        feed(&mut ed, "cTWO<esc>:w<ret>iX<esc>");
        ed.flush_saves();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TWOne\n");
        // Handle whatever arrives for a while (the round re-arms itself, so don't wait for it to stop)
        let until = std::time::Instant::now() + Duration::from_millis(700);
        while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
            if let Some(ev) = ed.events.recv_timeout(left) {
                ed.handle_event(ev);
            }
        }
        assert!(!ed.doc().disk_conflict, "own save seen as an outside change");
        feed(&mut ed, ":w<ret>");
        ed.flush_saves();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ed.doc().text.to_string(), ":w not refused");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_atomic_follows_symlinks_and_keeps_permissions() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tarae-disk-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (real, link) = (dir.join("real.sh"), dir.join("link.sh"));
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("real.sh", &link).unwrap();
        write_atomic(&link, |w| w.write_all(b"new")).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "link kept");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o755);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "no temp file left: {names:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn minimal_change_keeps_common_ends() {
        let c = |a: &str, b: &str| minimal_change(&Rope::from_str(a), &Rope::from_str(b));
        let x = c("fn a() {\n    1\n}\n", "fn a() {\n    2\n}\n").unwrap();
        assert_eq!((x.from, x.to, x.insert.as_str()), (13, 14, "2"));
        assert!(c("same", "same").is_none());
        // Char boundary: "타래" → "타리" replaces the whole second char
        let x = c("타래", "타리").unwrap();
        assert_eq!((x.from, x.to, x.insert.as_str()), (3, 6, "리"));
        // Overlapping prefix/suffix (aaa → aa)
        let x = c("aaa", "aa").unwrap();
        assert_eq!((x.to - x.from, x.insert.as_str()), (1, ""));
        let x = c("", "new").unwrap();
        assert_eq!((x.from, x.to, x.insert.as_str()), (0, 0, "new"));
    }

    /// Compared a chunk at a time, the change is the one a byte-by-byte walk finds — texts of many chunks,
    /// edits near either end or at a chunk seam, multi-byte chars, and an undo snapshot sharing its text.
    #[test]
    fn minimal_change_by_chunks_matches_a_byte_walk() {
        let walk = |old: &Rope, new: &Rope| {
            let (ol, nl) = (old.len_bytes(), new.len_bytes());
            let mut pre = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
            if pre == ol && pre == nl {
                return None;
            }
            while pre > 0 && (is_cont(old, pre) || is_cont(new, pre)) {
                pre -= 1;
            }
            let max_suf = (ol - pre).min(nl - pre);
            let (mut a, mut b) = (old.bytes_at(ol), new.bytes_at(nl));
            let mut suf = 0;
            while suf < max_suf && a.prev().is_some_and(|x| b.prev() == Some(x)) {
                suf += 1;
            }
            while suf > 0 && (is_cont(old, ol - suf) || is_cont(new, nl - suf)) {
                suf -= 1;
            }
            Some((pre, ol - suf, new.byte_slice(pre..nl - suf).to_string()))
        };
        let base: String = (0..3000).map(|i| format!("line {i} 타래 ab\n")).collect();
        let old = Rope::from_str(&base);
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..200 {
            let mut new = old.clone(); // shares its leaves, as an undo snapshot does
            for _ in 0..1 + next(3) {
                let at = new.char_to_byte(next(new.len_chars() + 1));
                let at_char = new.byte_to_char(at);
                match next(3) {
                    0 => new.insert(at_char, ["x", "타", "\n", "aa"][next(4)]),
                    1 if at_char < new.len_chars() => {
                        new.remove(at_char..(at_char + 1 + next(5)).min(new.len_chars()))
                    }
                    _ => {}
                }
            }
            let got = minimal_change(&old, &new).map(|c| (c.from, c.to, c.insert));
            assert_eq!(got, walk(&old, &new));
            let fresh = Rope::from_str(&new.to_string()); // the same text in chunks of its own
            assert_eq!(minimal_change(&old, &fresh).map(|c| (c.from, c.to, c.insert)), walk(&old, &fresh));
        }
    }
}
