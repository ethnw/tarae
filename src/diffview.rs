//! `:diff` — the buffer against another text, drawn the way the code review draws it (the removed lines in
//! place on a red wash, added and changed ones on a green one, `]g` / `[g` between changes), while you keep
//! editing. Against: nothing = the last commit (HEAD), a file = that file, anything else = a git revision
//! (`:diff main`, `:diff HEAD~3`). `:diff-off` ends it (the gutter goes back to comparing with the index).
//! `tarae --diff OLD NEW` opens NEW against OLD — `git difftool -x 'tarae --diff'`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::document::DocId;
use crate::editor::Editor;

/// The text to compare `path` with: `against` empty = HEAD's version, an existing file (`as_file`: what
/// `against` names as a file, from the editor's root) = its text, else the version at that git revision in
/// its repository as `places` finds it — `gitrepo::for_file_in`. (On a worker thread.)
pub(crate) fn base_text(
    places: &crate::gitrepo::Places,
    path: &Path,
    against: &str,
    as_file: PathBuf,
) -> Result<(String, String), String> {
    // A file — or `/dev/null` (git difftool's side for a new file): anything readable that isn't a folder
    if !against.is_empty() && as_file.exists() && !as_file.is_dir() {
        let text = std::fs::read_to_string(&as_file).map_err(|e| format!("{against}: {e}"))?;
        let name = as_file.file_name().map_or(against.to_string(), |n| n.to_string_lossy().into_owned());
        return Ok((text, name));
    }
    let (repo, rel) = crate::gitrepo::for_file_in(places, path).map_err(|n| n.said())?;
    let rev = if against.is_empty() { "HEAD" } else { against };
    // Not in that revision: everything is new
    let text = repo.run(&["show", &format!("{rev}:{rel}")], None).or_else(|e| {
        repo.run(&["rev-parse", "--verify", "--quiet", rev], None).map(|_| String::new()).map_err(|_| e)
    })?;
    Ok((text, rev.to_string()))
}

impl Editor {
    /// `:diff [against]` for the current buffer — the base read and the diff worked out on a worker.
    pub fn diff_against(&mut self, against: &str) -> Result<(), String> {
        let doc = self.doc();
        let Some(path) = doc.path.clone() else {
            return Err("save the buffer first — a diff needs a file".into());
        };
        if self.session.code_review.as_ref().is_some_and(|r| r.docs.contains(&doc.id)) {
            return Err("the review is showing this file's changes — space v q ends it".into());
        }
        let (id, version, text) = (doc.id, doc.version(), doc.text.clone());
        let (as_file, against) = (self.path_of(Path::new(against)), against.to_string());
        let places = self.git_places(&path);
        self.events.jobs().spawn(move || {
            let result = base_text(&places, &path, &against, as_file).map(|(base, label)| {
                let hunks = crate::git::hunks(&base, &text.to_string());
                (base, label, hunks)
            });
            move |ed: &mut Editor| match result {
                Ok((base, label, hunks)) => ed.diff_show(id, version, base, label, hunks),
                Err(e) => ed.set_error(format!("diff: {e}")),
            }
        });
        Ok(())
    }

    pub(crate) fn diff_show(
        &mut self,
        id: DocId,
        version: u64,
        base: String,
        label: String,
        hunks: Vec<crate::git::Hunk>,
    ) {
        let Some(d) = self.docs.iter_mut().find(|d| d.id == id) else { return };
        let n = hunks.len();
        d.set_review_base(Some(ropey::Rope::from_str(&base)));
        d.git_base = Some(Arc::new(base));
        d.diff_of = Some(label.clone());
        // Edited (or still loading) since: the diff is worked out again from the new text
        if d.version() == version && !d.loading {
            d.git_hunks = hunks;
            d.git_version = version;
            self.diff_landed(id, version);
        } else {
            d.git_version = u64::MAX;
        }
        self.base_parse(id);
        // The cursor goes to the first change only in the buffer diffed, if it's the one showing
        if self.doc().id == id
            && let Some(h) = self.doc().git_hunks.first().cloned()
        {
            self.select_hunk(&h);
            self.align_view(crate::viewalign::Place::Center);
        }
        match n {
            0 => self.set_status(format!("no difference from {label}")),
            _ => self.set_status(format!(
                "against {label}: {n} change{} — ]g walks them, :diff-off ends it",
                if n == 1 { "" } else { "s" }
            )),
        }
    }

    /// `:diff-off` — back to the plain buffer (its gutter compares with the index again).
    pub fn diff_off(&mut self) -> Result<(), String> {
        let d = self.doc_mut();
        if d.diff_of.take().is_none() {
            return Err("no diff here".into());
        }
        d.set_review_base(None);
        d.git_base = None;
        d.git_hunks.clear();
        let id = d.id;
        self.git_load_base(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_from_a_file() {
        let dir = std::env::temp_dir().join(format!("tarae-diffview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.txt"), "a\nb\n").unwrap();
        let old = dir.join("old.txt");
        let at = crate::gitrepo::Places::only(&dir);
        let (text, label) =
            base_text(&at, &dir.join("new.txt"), &old.to_string_lossy(), old.clone()).unwrap();
        assert_eq!((text.as_str(), label.as_str()), ("a\nb\n", "old.txt"));
        // (named relative to the editor's root: the file there)
        assert_eq!(base_text(&at, &dir.join("new.txt"), "old.txt", old.clone()).unwrap().0, "a\nb\n");
        // git difftool's side for a new file
        assert_eq!(base_text(&at, &dir.join("new.txt"), "/dev/null", "/dev/null".into()).unwrap().0, "");
        std::fs::remove_dir_all(&dir).ok();
    }
}
