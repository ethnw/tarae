//! The layout kept across quit and start (panes P2b, `target/sb/panes-plan.md` §5.1): each checkout's pane tree —
//! the shares you set, the surfaces' panes, the code's panes by the files they show, the one last focused — in
//! `$state/layouts.json`, keyed by the checkout's real path (a session's own checkout its own key). Read once
//! before the first frame (a small file); written on a worker each time it changes (a split, a close, a swap, a
//! turn, a size, another file in a pane — `Editor::layout_keep`), the newest write winning, so a crash keeps it
//! too; each write replaces only its own checkout's key, under a lock (taraes in other checkouts keep theirs). Not
//! while only the start screen's scratch is open: a look at it keeps the last one (as `restore.json`'s files). It comes back with the files (`r` on the start screen — `restore_files`) where it still holds: each code
//! pane's file there (one gone takes its pane with it), one pane of each surface. No scroll: `restore.json` keeps
//! each file's spot.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::editor::Editor;

pub fn path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("layouts.json"))
}

/// The file at `path` (missing or unreadable: none kept).
pub fn read(path: Option<&Path>) -> Map<String, Value> {
    let text = path.and_then(|p| std::fs::read_to_string(p).ok());
    text.and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Session `l`'s layout as kept, the pane last focused showing `current` — None while none of its code panes shows
/// a file.
pub fn of(
    l: &crate::session::Session,
    docs: &[crate::document::Document],
    current: crate::document::DocId,
) -> Option<Value> {
    let path = |id| docs.iter().find(|d| d.id == id).and_then(|d| d.path.as_ref()).and_then(|p| p.to_str());
    let doc = |v: &crate::editor::View| if v.id == l.focus { current } else { v.doc };
    let views: Vec<Value> = l.views.iter().map(|v| json!({ "id": v.id, "path": path(doc(v)) })).collect();
    if views.iter().all(|v| v["path"].is_null()) {
        return None;
    }
    let panes: Vec<Value> = l.panes.iter().map(|p| json!({ "id": p.id, "shows": p.shows.name() })).collect();
    Some(
        json!({ "split": crate::restart::split_json(&l.split), "panes": panes, "views": views, "focus": l.focus }),
    )
}

/// The writes' numbers, one counter for the process: every editor's (a test's, a simulation's run again) newer than
/// the last — so an editor started later is never taken for an older write.
static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Written on a worker: checkout `key`'s layout into the file — read, its key replaced and written while the lock
/// beside it is held, so taraes in other checkouts keep theirs; the last one asked for wins (an older write of the
/// same key that finishes later is skipped).
pub fn save(path: &Path, key: &str, layout: Value, seq: u64) {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::Mutex;
    static LAST: Mutex<Vec<(PathBuf, String, u64)>> = Mutex::new(Vec::new());
    let Ok(mut last) = LAST.lock() else { return };
    match last.iter_mut().find(|(p, k, _)| p == path && k == key) {
        Some((.., n)) if seq < *n => return,
        Some((.., n)) => *n = seq,
        None => last.push((path.to_path_buf(), key.to_string(), seq)),
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // (the file itself is replaced by each write — a new inode — so the lock is a file beside it)
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_extension("lock"));
    let Ok(lock) = lock else { return };
    if lock.lock().is_err() {
        return;
    }
    let mut all = read(Some(path));
    all.insert(key.to_string(), layout);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, Value::Object(all).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

impl Editor {
    /// Every key and event: the front session's layout, kept again once it changed (a worker's write — nothing
    /// back to the loop, no frame).
    pub fn layout_keep(&mut self) {
        let Some(path) = self.layouts_path.clone() else { return };
        let Some(now) = of(&self.session, &self.docs, self.doc().id) else { return };
        let Some(key) = self.session.root.to_str().map(str::to_string) else { return };
        if self.layouts.get(&key) == Some(&now) {
            return;
        }
        self.layouts.insert(key.clone(), now.clone());
        self.layouts_seq += 1;
        let seq = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.events.jobs().background(move || save(&path, &key, now, seq));
    }

    /// After `restore_files`: the layout kept for this checkout, laid over the files reopened — where it holds
    /// (`restart::layout`'s rule: the tree exactly its code panes and one pane of each surface). A pane whose file
    /// isn't open now goes, its neighbours taking the room. Said when it came back with more than one pane.
    pub fn layout_restore(&mut self) {
        let Some(key) = self.session.root.to_str() else { return };
        let Some(kept) = self.layouts.get(key).cloned() else { return };
        let docs: Vec<Option<crate::document::DocId>> = self.docs.iter().map(|d| Some(d.id)).collect();
        let index =
            |p: &str| self.docs.iter().position(|d| d.path.as_deref().and_then(Path::to_str) == Some(p));
        let Some(mut split) = crate::restart::split_of(&kept["split"]) else { return };
        let mut views = Vec::new();
        for v in kept["views"].as_array().into_iter().flatten() {
            let Some(id) = v["id"].as_u64() else { continue };
            match v["path"].as_str().and_then(index) {
                Some(i) => views
                    .push(json!({ "id": id, "doc": i, "top": self.docs[i].top, "left": self.docs[i].left })),
                // (its file gone: so is its pane — the last one left stays, showing what's open)
                None if !split.remove(id) => views.push(json!({ "id": id, "doc": self.current })),
                None => {}
            }
        }
        let mut focus = kept["focus"].as_u64();
        if !views.iter().any(|v| v["id"].as_u64() == focus) {
            focus = views.first().and_then(|v| v["id"].as_u64());
        }
        let current = views.iter().find(|v| v["id"].as_u64() == focus).map(|v| v["doc"].clone());
        let n = views.len();
        let v = json!({
            "current": current, "views": views, "panes": kept["panes"], "split": crate::restart::split_json(&split),
            "focus": focus,
        });
        let before = self.session.split.clone();
        crate::restart::layout(self, &v, &docs);
        if self.session.split != before && n > 1 {
            self.note(format!("Your {n} panes from last time are back"));
        }
    }
}

#[cfg(test)]
mod tests;
