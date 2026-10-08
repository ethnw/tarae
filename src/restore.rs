//! Restore — started without arguments, reopen the files last open in this folder at their spots (cursor,
//! scroll); opening a file by name goes to where you last were in it.
//! `$XDG_STATE_HOME/tarae/restore.json` (default `~/.local/state/tarae/restore.json`):
//! `{ "positions": { path: [cursor, top line] }, "folders": { dir: { "files": [...], "current": n } },
//!   "notes": { dir: [note…] } }` (Claude's notes per project — `notes::to_json`).
//! Read once at startup, written once on quit (a small file). Earlier builds wrote `session.json`, its
//! folders under `"sessions"` — read when there's no `restore.json` yet, so nothing is lost on upgrade.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::editor::Editor;
use crate::graphemes;
use crate::selection::Selection;

/// Number of file positions remembered (oldest dropped first).
const KEEP_POSITIONS: usize = 500;
/// The file · what earlier builds called it.
const FILE: &str = "restore.json";
const OLD_FILE: &str = "session.json";

fn file() -> Option<PathBuf> {
    Some(crate::config::state_dir()?.join(FILE))
}

#[derive(Default)]
pub struct State {
    root: Map<String, Value>,
    /// A file was opened this run. Quitting with none open then clears the folder's files (they were
    /// closed on purpose); without it (a look at the start screen) the last ones stay.
    pub opened: bool,
}

impl State {
    pub fn load() -> State {
        match crate::config::state_dir() {
            Some(dir) => State::load_from(&dir),
            None => State::default(),
        }
    }

    /// The file in state folder `dir` — or what an earlier build left there.
    fn load_from(dir: &Path) -> State {
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.as_object().cloned())
        };
        let mut root = read(FILE).or_else(|| read(OLD_FILE)).unwrap_or_default();
        if !root.contains_key("folders")
            && let Some(old) = root.remove("sessions")
        {
            root.insert("folders".into(), old);
        }
        State { root, opened: false }
    }

    #[cfg(test)]
    pub fn from_json(v: Value) -> State {
        State { root: v.as_object().cloned().unwrap_or_default(), opened: false }
    }

    /// A file's last position (cursor byte, top line).
    pub fn position(&self, path: &Path) -> Option<(usize, usize)> {
        let p = self.root.get("positions")?.get(path.to_str()?)?.as_array()?;
        Some((p.first()?.as_u64()? as usize, p.get(1)?.as_u64()? as usize))
    }

    /// Notes saved for this folder (`notes::restore_in` reads them).
    pub fn notes(&self, cwd: &Path) -> Vec<Value> {
        self.root
            .get("notes")
            .and_then(|n| n.get(cwd.to_str()?))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// The changed lines a person looked at in this folder's reviews (`Session.looked` — `risk.rs`), kept as
    /// 32 hex digits each; anything else (an earlier build's FNV numbers) counts for nothing — its changes ask
    /// for a look again.
    pub fn looked(&self, cwd: &Path) -> std::collections::HashSet<crate::risk::Key> {
        let keys = self.root.get("looked").and_then(|n| n.get(cwd.to_str()?)).and_then(Value::as_array);
        keys.into_iter().flatten().filter_map(Value::as_str).filter_map(crate::risk::key_of_hex).collect()
    }

    /// The looked-at lines of the session in `cwd` as they are now (as `set_notes`).
    pub fn set_looked(&mut self, cwd: &Path, looked: &std::collections::HashSet<crate::risk::Key>) {
        let Some(cwd) = cwd.to_str() else { return };
        let saved = self.root.entry("looked").or_insert_with(|| json!({}));
        if let Some(map) = saved.as_object_mut() {
            match looked.is_empty() {
                true => map.remove(cwd),
                false => {
                    let keys: Vec<String> = looked.iter().map(|k| crate::risk::key_hex(*k)).collect();
                    map.insert(cwd.to_string(), json!(keys))
                }
            };
        }
    }

    /// The added lines looked at in this folder's reviews (`Session.looked_lines` — `risk::LookedLine`): by
    /// file, each `[line then, text, how many times the file had it]`.
    pub fn looked_lines(
        &self,
        cwd: &Path,
    ) -> std::collections::HashMap<String, Vec<crate::risk::LookedLine>> {
        let line = |rel: &str, v: &Value| {
            let (at, text) = (v.get(0)?.as_u64()? as usize, v.get(1)?.as_str()?);
            Some(crate::risk::LookedLine::new(rel, at, text, v.get(2)?.as_u64()? as u32))
        };
        self.by_file("looked_lines", cwd, line)
    }

    /// The high-risk changes looked at in this folder's reviews (`Session.looked_orders`): by file, each its
    /// added lines' keys in order (hex) and the lines just above and below it.
    pub fn looked_orders(
        &self,
        cwd: &Path,
    ) -> std::collections::HashMap<String, Vec<crate::risk::LookedOrder>> {
        let order = |_: &str, v: &Value| {
            let keys = v["keys"].as_array()?.iter().map(|k| crate::risk::key_of_hex(k.as_str()?));
            Some(crate::risk::LookedOrder {
                keys: keys.collect::<Option<Vec<_>>>()?,
                above: v["above"].as_str().map(str::to_string),
                below: v["below"].as_str().map(str::to_string),
            })
        };
        self.by_file("looked_orders", cwd, order)
    }

    /// `cwd`'s entry under `name`, each file's items read by `item` (one it can't read left out).
    fn by_file<T>(
        &self,
        name: &str,
        cwd: &Path,
        item: impl Fn(&str, &Value) -> Option<T>,
    ) -> std::collections::HashMap<String, Vec<T>> {
        let files = self.root.get(name).and_then(|n| n.get(cwd.to_str()?)).and_then(Value::as_object);
        files
            .into_iter()
            .flatten()
            .map(|(rel, v)| {
                (rel.clone(), v.as_array().into_iter().flatten().filter_map(|v| item(rel, v)).collect())
            })
            .collect()
    }

    /// The looked-at added lines and changes' orders of the session in `cwd` as they are now (as
    /// `set_looked`).
    pub fn set_looked_lines(
        &mut self,
        cwd: &Path,
        lines: &std::collections::HashMap<String, Vec<crate::risk::LookedLine>>,
        orders: &std::collections::HashMap<String, Vec<crate::risk::LookedOrder>>,
    ) {
        let line = |l: &crate::risk::LookedLine| json!([l.at, l.text, l.count]);
        let order = |o: &crate::risk::LookedOrder| {
            let keys: Vec<String> = o.keys.iter().map(|k| crate::risk::key_hex(*k)).collect();
            json!({ "keys": keys, "above": o.above, "below": o.below })
        };
        self.set_by_file("looked_lines", cwd, lines, line);
        self.set_by_file("looked_orders", cwd, orders, order);
    }

    /// `cwd`'s entry under `name`: each file's items as `item` writes them (none: the entry goes).
    fn set_by_file<T>(
        &mut self,
        name: &str,
        cwd: &Path,
        files: &std::collections::HashMap<String, Vec<T>>,
        item: impl Fn(&T) -> Value,
    ) {
        let Some(cwd) = cwd.to_str() else { return };
        let saved = self.root.entry(name).or_insert_with(|| json!({}));
        if let Some(map) = saved.as_object_mut() {
            match files.is_empty() {
                true => map.remove(cwd),
                false => {
                    let files: Map<String, Value> =
                        files.iter().map(|(rel, v)| (rel.clone(), v.iter().map(&item).collect())).collect();
                    map.insert(cwd.to_string(), Value::Object(files))
                }
            };
        }
    }

    /// The notes of the session in `cwd` as they are now — what `notes` gives from here on (a session ended
    /// and opened again in the same run), and what's written on quit.
    pub fn set_notes(&mut self, cwd: &Path, notes: Vec<Value>) {
        let Some(cwd) = cwd.to_str() else { return };
        let saved = self.root.entry("notes").or_insert_with(|| json!({}));
        if let Some(map) = saved.as_object_mut() {
            match notes.is_empty() {
                true => map.remove(cwd),
                false => map.insert(cwd.to_string(), Value::Array(notes)),
            };
        }
    }

    /// The files last open in this folder: (files, index being viewed).
    pub fn files(&self, cwd: &Path) -> Option<(Vec<PathBuf>, usize)> {
        let s = self.root.get("folders")?.get(cwd.to_str()?)?;
        let files: Vec<PathBuf> =
            s.get("files")?.as_array()?.iter().filter_map(|f| f.as_str().map(PathBuf::from)).collect();
        Some((files, s.get("current").and_then(Value::as_u64).unwrap_or(0) as usize))
    }
}

impl Editor {
    /// Right after opening a document: go to the remembered spot if any (clamped to the end / a cluster
    /// boundary if the file changed meanwhile).
    pub fn restore_position(&mut self, idx: usize) {
        let Some(path) = self.docs.get(idx).and_then(|d| d.path.clone()) else { return };
        let Some((head, top)) = self.restore.position(&path) else { return };
        let doc = &mut self.docs[idx];
        if doc.loading {
            return;
        }
        let head = graphemes::snap(&doc.text, head);
        doc.set_selection(Selection::point(head));
        doc.top = top.min(doc.text.len_lines().saturating_sub(1));
    }

    /// The files last open in this folder, if any of them are still there — the start screen offers them
    /// (`r`). Read once at startup (a stat per file; never while drawing).
    pub fn find_restorable(&mut self) {
        let found = self.restore.files(&self.session.root);
        self.session.restorable = found.map(|(files, _)| files.into_iter().filter(|f| f.is_file()).collect());
        self.session.restorable.take_if(|f| f.is_empty());
    }

    /// The files last open in this folder, opened again (existing ones only). Number of files opened.
    pub fn restore_files(&mut self) -> usize {
        let Some((files, current)) = self.restore.files(&self.session.root) else { return 0 };
        let mut opened = 0;
        // Saved `current` indexes the saved list — missing files shift it. Take the file itself, or the
        // nearest opened one before it (else the first).
        let mut pick = None;
        // (outside the checkouts, a file's server waits until it's shown — #141)
        self.lsp.unseen_later = true;
        for (i, f) in files.iter().enumerate() {
            if !f.is_file() || self.open(f).is_err() {
                continue;
            }
            opened += 1;
            if i <= current || pick.is_none() {
                pick = Some(self.current);
            }
        }
        self.lsp.unseen_later = false;
        if let Some(pick) = pick {
            self.current = pick;
            let name = if opened == 1 { "1 file".to_string() } else { format!("{opened} files") };
            self.set_status(format!("Reopened {name} from last time"));
        }
        opened
    }

    /// On quit: record open files' positions and the files open in this folder.
    pub fn save_restore(&mut self) {
        let Some(f) = file() else { return };
        self.restore_record();
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = Value::Object(self.restore.root.clone()).to_string();
        // (yours only, from its first byte: it holds lines of code looked at in reviews, and notes' text)
        if let Err(e) = crate::disk::write_private(&f, |w| w.write_all(body.as_bytes())) {
            crate::log::line("tarae", "warning", &format!("{}: not written ({e})", f.display()));
        }
    }

    /// What `save_restore` writes, put in `self.restore` (tests read it back from there).
    fn restore_record(&mut self) {
        // (every session's notes, each under its own checkout)
        let notes: Vec<(PathBuf, Vec<Value>)> = self
            .sessions_in_order()
            .iter()
            .map(|l| (l.root.clone(), crate::notes::to_json(&l.notes, &self.docs)))
            .collect();
        for (cwd, notes) in notes {
            self.restore.set_notes(&cwd, notes);
        }
        // (and what was looked at in its reviews — the gate on its bring, `risk.rs`)
        let looked: Vec<(PathBuf, std::collections::HashSet<crate::risk::Key>)> =
            self.sessions_in_order().iter().map(|l| (l.root.clone(), l.looked.clone())).collect();
        for (cwd, looked) in looked {
            self.restore.set_looked(&cwd, &looked);
        }
        type Looked = (
            PathBuf,
            std::collections::HashMap<String, Vec<crate::risk::LookedLine>>,
            std::collections::HashMap<String, Vec<crate::risk::LookedOrder>>,
        );
        let lines: Vec<Looked> = self
            .sessions_in_order()
            .iter()
            .map(|l| (l.root.clone(), l.looked_lines.clone(), l.looked_orders.clone()))
            .collect();
        for (cwd, lines, orders) in lines {
            self.restore.set_looked_lines(&cwd, &lines, &orders);
        }
        let opened = self.restore.opened;
        let folder = self.session.root.to_str().map(str::to_string);
        let root = &mut self.restore.root;
        // Sequence: each entry is [cursor, top line, seq] — on overflow, drop the smallest (oldest) seq first
        let mut seq = root.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let positions = root.entry("positions").or_insert_with(|| json!({}));
        if let Some(map) = positions.as_object_mut() {
            for d in &self.docs {
                let Some(p) = d.path.as_ref().and_then(|p| p.to_str()) else { continue };
                seq += 1;
                map.insert(p.to_string(), json!([d.selection().primary().cursor(&d.text), d.top, seq]));
            }
            if map.len() > KEEP_POSITIONS {
                let mut by_age: Vec<(u64, String)> = map
                    .iter()
                    .map(|(k, v)| (v.get(2).and_then(Value::as_u64).unwrap_or(0), k.clone()))
                    .collect();
                by_age.sort();
                for (_, k) in by_age.into_iter().take(map.len() - KEEP_POSITIONS) {
                    map.remove(&k);
                }
            }
        }
        root.insert("seq".into(), json!(seq));
        if let Some(cwd) = folder.as_deref() {
            let files: Vec<&str> = self.docs.iter().filter_map(|d| d.path.as_ref()?.to_str()).collect();
            let folders = root.entry("folders").or_insert_with(|| json!({}));
            if let Some(map) = folders.as_object_mut() {
                if !files.is_empty() {
                    map.insert(cwd.to_string(), json!({ "files": files, "current": self.current }));
                } else if opened {
                    map.remove(cwd);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_positions_and_folders() {
        let s = State::from_json(json!({
            "positions": { "/a.rs": [12, 3] },
            "folders": { "/proj": { "files": ["/a.rs", "/b.rs"], "current": 1 } }
        }));
        assert_eq!(s.position(Path::new("/a.rs")), Some((12, 3)));
        assert_eq!(s.position(Path::new("/nope")), None);
        let (files, cur) = s.files(Path::new("/proj")).unwrap();
        assert_eq!((files.len(), cur), (2, 1));
    }

    /// What an earlier build wrote (`session.json`, its folders under `"sessions"`) is read while there's no
    /// `restore.json` — and once there is, that one wins.
    #[test]
    fn what_earlier_builds_kept_is_restored() {
        let dir = std::env::temp_dir().join(format!("tarae-restore-old-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let old = json!({ "positions": { "/a.rs": [4, 1] },
            "sessions": { "/proj": { "files": ["/a.rs"], "current": 0 } } });
        std::fs::write(dir.join(OLD_FILE), old.to_string()).unwrap();
        let s = State::load_from(&dir);
        assert_eq!(s.files(Path::new("/proj")), Some((vec![PathBuf::from("/a.rs")], 0)));
        assert_eq!(s.position(Path::new("/a.rs")), Some((4, 1)));
        assert!(!s.root.contains_key("sessions"), "kept under its new name from now on");
        let new = json!({ "folders": { "/proj": { "files": ["/b.rs"], "current": 0 } } });
        std::fs::write(dir.join(FILE), new.to_string()).unwrap();
        let s = State::load_from(&dir);
        assert_eq!(s.files(Path::new("/proj")), Some((vec![PathBuf::from("/b.rs")], 0)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_skips_missing_files_and_snaps_to_clusters() {
        let dir = std::env::temp_dir().join(format!("tarae-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, c) = (dir.join("a.txt"), dir.join("c.txt"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&c, "e\u{301}x\n").unwrap();
        let (a, c) = (std::fs::canonicalize(a).unwrap(), std::fs::canonicalize(c).unwrap());
        // (the files are the editor's root's — here the test's own folder)
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = cwd.clone();
        ed.restore = State::from_json(json!({
            "positions": { c.to_str().unwrap(): [1, 0] }, // on the combining mark
            "folders": { cwd.to_str().unwrap(): {
                "files": [dir.join("gone.txt").to_str().unwrap(), a.to_str().unwrap(), c.to_str().unwrap()],
                "current": 1,
            } },
        }));
        assert_eq!(ed.restore_files(), 2);
        assert_eq!(ed.doc().path.as_deref(), Some(a.as_path()), "saved current = a, not shifted by gone.txt");
        let doc_c = ed.docs.iter().find(|d| d.path.as_deref() == Some(c.as_path())).unwrap();
        assert_eq!(doc_c.selection().primary().head, 0, "cluster start");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The lines looked at in a review (`risk.rs`) outlive a quit, as their keys in hex; a mark kept by an earlier
    /// build (an FNV number) counts for nothing — its change asks for a look again.
    #[test]
    fn looked_at_lines_are_kept_by_their_sha256_key_only() {
        let k = crate::risk::key("src/a.rs", b'+', "    unsafe { b() }");
        let mut s = State::from_json(json!({}));
        let root = Path::new("/proj");
        s.set_looked(root, &std::iter::once(k).collect());
        let kept = State::from_json(Value::Object(s.root.clone()));
        assert_eq!(kept.looked(root), std::iter::once(k).collect());
        // An earlier build's mark of the same line: its FNV key, a number
        let old = State::from_json(json!({ "looked": { "/proj": [12387807294418380025u64, "zz"] } }));
        assert!(old.looked(root).is_empty(), "an old mark is no look");
    }

    /// Wave 3, gap 1: the notes of a session behind (a review's drafts too) were saved only for the front's
    /// folder — gone on every quit and restart. Each session's are kept under its own checkout, and come back
    /// in its session when it does (`sessions_back` → `session_add`), none in another's.
    #[test]
    fn notes_of_a_session_behind_outlive_a_quit() {
        use crate::notes::{By, Note};
        let dir = std::env::temp_dir().join(format!("tarae-restore-notes-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for p in ["front", "back"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
            std::fs::write(dir.join(p).join("a.txt"), "one\ntwo\n").unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (front, back) = (dir.join("front"), dir.join("back"));
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = front.clone();
        crate::notes::add(&mut ed, &front.join("a.txt"), 0..1, By::Author, "in front");
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| {
            crate::notes::add(ed, &back.join("a.txt"), 1..2, By::Author, "behind");
            // (a review's draft, not sent yet)
            let n = ed.session.notes.list.last_mut().unwrap();
            n.thread.push(crate::notes::Entry { by: By::You, text: "my draft".into(), draft: true });
        });
        ed.restore_record();
        let kept = State::from_json(Value::Object(ed.restore.root.clone()));
        drop(ed);
        // The next start: the front's at once (`main.rs`), the one behind as its session comes back
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = front.clone();
        ed.restore = kept;
        ed.session_kept_back(0);
        let id = ed.session_add(back.clone());
        let gists = |l: &[Note]| l.iter().map(|n| n.gist().to_string()).collect::<Vec<_>>();
        assert_eq!(gists(&ed.session.notes.list), ["in front"]);
        let behind = &ed.sessions.iter().find(|l| l.id == id).unwrap().notes.list;
        assert_eq!(gists(behind), ["behind"], "its own, in its own session");
        assert_eq!((behind[0].lines.clone(), behind[0].path.clone()), (1..2, back.join("a.txt")));
        assert!(behind[0].thread.get(1).is_some_and(|e| e.draft && e.text == "my draft"), "the draft too");
        std::fs::remove_dir_all(&dir).ok();
    }
}
