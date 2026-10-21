//! A session's instruction files follow the main checkout's (frictions29 #27: a session started before a rule
//! landed worked without it). A new checkout gets copies of the git-ignored `CLAUDE.md`, `CLAUDE.local.md`,
//! `AGENTS.md` (`worktree::instructions`) and keeps each as copied — its base — in its own git folder (per
//! checkout, gone with it, where no run of Claude's writes). When the session's claude starts, and after its
//! rebase, a worker compares the three: a copy untouched since its base takes the main checkout's (said, and
//! told to Claude with the next message); one changed on both sides is a card (`What::Instructions`: `y` takes
//! the main checkout's changes where they don't touch the session's own — `checkpoint::rebase` — `v` shows
//! them, `n` keeps it); a copy made before bases were kept that differs is asked the same way, once. The
//! session's own changes are never written over, nor a buffer with unsaved changes. `.claude/settings*.json`
//! aren't followed: they go through `claudetrust.rs`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::offer::{Offer, What};

/// The folder in a checkout's git folder that holds its instruction files as copied.
const BASES: &str = "tarae-instructions";

/// The bases' folder of the checkout at `root` (git runs the first time: a worker).
fn bases(root: &Path) -> Option<PathBuf> {
    crate::gitrepo::repo(root).ok().map(|r| r.git_dir.join(BASES))
}

/// The new checkout `path`'s copies from `top` (`copied`, relative) kept as their bases — the three files only.
/// On a worker (`worktree::make`).
pub fn keep_bases(top: &Path, path: &Path, copied: &[String]) {
    let Some(dir) = bases(path) else { return };
    for rel in copied.iter().filter(|r| crate::worktree::INSTRUCTIONS.contains(&r.as_str())) {
        if let Some(text) = plain(top, rel) {
            let _ = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(rel), text));
        }
    }
}

/// A file changed in the session and in the main checkout since its copy (or no base to tell by).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Both {
    /// The session's checkout, the file (relative).
    pub root: PathBuf,
    pub rel: String,
    /// As copied — None: copied before bases were kept.
    pub base: Option<String>,
    /// The session's now, the main checkout's now.
    pub here: String,
    pub main: String,
    /// The main checkout's changes that touch the session's own (left as the session has them on `y`).
    pub left: usize,
}

impl Both {
    /// What `y` writes: the main checkout's changes over the session's own — with no base, the main checkout's.
    fn merged(&self) -> String {
        match &self.base {
            Some(b) => crate::checkpoint::rebase(b, &self.main, &self.here).0,
            None => self.main.clone(),
        }
    }

    /// The card.
    pub fn card(&self) -> Offer {
        let rel = &self.rel;
        let q = match &self.base {
            Some(_) => {
                let left = match self.left {
                    0 => "None of them touch this session's own changes.".to_string(),
                    1 => "1 of them touches lines this session changed: it stays as it is here.".to_string(),
                    n => {
                        format!("{n} of them touch lines this session changed: those stay as they are here.")
                    }
                };
                format!(
                    "The main checkout's {rel} changed since this session's copy, and so did this one. y takes the \
                     main checkout's changes here. {left}\n\nClaude reads it from its next start."
                )
            }
            None => format!(
                "This session's {rel} differs from the main checkout's, and whose change it is can't be told (it was \
                 copied before tarae kept what it copied). y takes the main checkout's whole — this one's own lines \
                 go; n keeps this one, and only later changes are asked.\n\nClaude reads it from its next start."
            ),
        };
        Offer::new(
            format!("{rel}: the main checkout's changed"),
            &q,
            "Taking the main checkout's…",
            format!("Couldn't update {rel}"),
            What::Instructions(Box::new(self.clone())),
        )
    }

    /// `v`: the main checkout's changes since the copy (with no base: this one's against it), line by line.
    pub fn view_text(&self) -> String {
        let from = self.base.as_deref().unwrap_or(&self.here);
        let mut out = match self.base {
            Some(_) => format!("The main checkout's {} since this session's copy:\n", self.rel),
            None => format!("This session's {} → the main checkout's:\n", self.rel),
        };
        for (start, _, old, new) in crate::editdiff::line_changes(from, &self.main) {
            out += &format!("\n@@ line {}\n", from[..start].matches('\n').count() + 1);
            out.extend(old.lines().map(|l| format!("- {l}\n")));
            out.extend(new.lines().map(|l| format!("+ {l}\n")));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    /// The card counts the main checkout's changes that touch the session's own in words that agree (the review
    /// of frictions31: `nav::count` made "2 of thems").
    #[test]
    fn the_card_counts_changes_that_touch_the_sessions_own() {
        let q = |left| {
            let b = super::Both {
                root: "/stand-in".into(),
                rel: "CLAUDE.md".into(),
                base: Some("a\n".into()),
                here: "a\nb\n".into(),
                main: "a\nc\n".into(),
                left,
            };
            b.card().question
        };
        assert!(q(0).contains("None of them touch this session's own changes."), "{}", q(0));
        assert!(
            q(1).contains("1 of them touches lines this session changed: it stays as it is here."),
            "{}",
            q(1)
        );
        assert!(
            q(2).contains("2 of them touch lines this session changed: those stay as they are here."),
            "{}",
            q(2)
        );
    }
}

/// What a look found: the files taken from the main checkout, the ones changed on both sides, what failed.
#[derive(Debug, Default)]
pub struct Found {
    pub updated: Vec<String>,
    pub both: Vec<Both>,
    pub failed: Vec<String>,
}

/// The session's checkout `root` against the main checkout `main` (a worker): `dirty`, buffers with unsaved
/// changes, are left as they are.
/// Checkout `root`'s file `rel` as text — read through no link (`beneath.rs`): None when it isn't there, is a
/// link or isn't a plain file (the review of frictions31: Claude can put a link where CLAUDE.md was, and tarae,
/// outside the sandbox, would have read what it points at and written it into the checkout).
fn plain(root: &Path, rel: &str) -> Option<String> {
    let bytes = crate::beneath::Dir::root(root).and_then(|d| d.read(OsStr::new(rel))).ok()??;
    String::from_utf8(bytes).ok()
}

pub fn look(root: &Path, main: &Path, dirty: &[PathBuf]) -> Found {
    let mut found = Found::default();
    let Some(dir) = bases(root) else { return found };
    // (only what git doesn't carry: a tracked one comes with the checkout)
    let untracked = crate::worktree::instructions(main);
    for rel in crate::worktree::INSTRUCTIONS.iter().filter(|r| untracked.iter().any(|u| u == *r)) {
        let (Some(main_text), Some(here)) = (plain(main, rel), plain(root, rel)) else { continue };
        if dirty.iter().any(|d| *d == root.join(rel)) {
            continue;
        }
        // (the base is in the checkout's git folder, which no run writes)
        let base = std::fs::read_to_string(dir.join(rel)).ok();
        let keep =
            |text: &str| std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(rel), text));
        if here == main_text {
            if base.as_deref() != Some(main_text.as_str()) {
                let _ = keep(&main_text);
            }
            continue;
        }
        match base {
            // Untouched here: the main checkout's taken (through no link — `beneath.rs`)
            Some(b) if b == here => {
                let wrote = crate::beneath::Dir::root(root)
                    .and_then(|d| d.replace(OsStr::new(rel), main_text.as_bytes()))
                    .and_then(|_| keep(&main_text));
                match wrote {
                    Ok(()) => found.updated.push(rel.to_string()),
                    Err(e) => found.failed.push(format!("{rel}: {e}")),
                }
            }
            // Changed only here: the session's own
            Some(b) if b == main_text => {}
            base => {
                let left = base.as_deref().map_or(0, |b| crate::checkpoint::rebase(b, &main_text, &here).1);
                found.both.push(Both {
                    root: root.to_path_buf(),
                    rel: rel.to_string(),
                    base,
                    here,
                    main: main_text,
                    left,
                });
            }
        }
    }
    found
}

/// A session's look under way, and another asked for meanwhile — one at a time, so two never write over each
/// other (or over what the session wrote since the first read). A card's answer writes too: a look waits for the
/// answers being written, and one out while a card was answered may have read the file before the answer wrote
/// it — its cards would ask again what was just answered (seen under load: the stale card still up long after).
#[derive(Debug, Default)]
pub struct Look {
    under_way: bool,
    again: bool,
    /// Cards answered so far (`answer`)
    answered: u64,
    /// Answers being written
    answering: usize,
    /// Tests: the next answer writes once this says so
    #[cfg(test)]
    pub held: Option<std::sync::mpsc::Receiver<()>>,
}

impl Look {
    /// Whether a look or an answer is under way (or a look asked again after it) — tests wait for none.
    #[cfg(test)]
    pub fn under_way(&self) -> bool {
        self.under_way || self.again || self.answering > 0
    }

    /// Whether a look is out (tests).
    #[cfg(test)]
    pub fn looking(&self) -> bool {
        self.under_way
    }
}

/// The front session's checkout, a session's own (not the main checkout), looked at on a worker — as its
/// claude starts (`chat::ensure_proc`) and after its rebase (`rebase.rs`).
pub fn follow(ed: &mut Editor) {
    if ed.session.id == 0 {
        return;
    }
    let state = &mut ed.session.instructions;
    if state.under_way || state.answering > 0 {
        state.again = true;
        return;
    }
    state.under_way = true;
    let asked = state.answered;
    let root = ed.session.root.clone();
    let dirty: Vec<PathBuf> =
        ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
    ed.events.jobs().spawn(move || {
        let real = std::fs::canonicalize(&root).unwrap_or(root);
        let main = crate::gitrepo::repo(&real)
            .ok()
            .and_then(|r| crate::worktree::main_checkout(&r.common_dir, &real));
        let found = main.map(|main| look(&real, &main, &dirty)).unwrap_or_default();
        move |ed: &mut Editor| {
            let (state, mut found) = (&mut ed.session.instructions, found);
            state.under_way = false;
            // (answered meanwhile: what it read may be from before the answer's write — looked at again after it)
            if state.answered != asked {
                found.both.clear();
                state.again = true;
            }
            said(ed, found);
            again(ed);
        }
    });
}

/// A look asked meanwhile, once nothing's being written.
fn again(ed: &mut Editor) {
    let state = &mut ed.session.instructions;
    if state.answering == 0 && std::mem::take(&mut state.again) {
        follow(ed);
    }
}

/// What a look found, said: the files taken — a toast, and Claude told with the next message; a card for each
/// changed on both sides (one a file).
fn said(ed: &mut Editor, found: Found) {
    let Found { updated, both, failed } = found;
    if updated.is_empty() && both.is_empty() && failed.is_empty() {
        ed.unchanged = true;
        return;
    }
    if !updated.is_empty() {
        let names = updated.join(", ");
        ed.set_success(format!(
            "{names} updated from the main checkout — Claude reads it from its next start"
        ));
        ed.session.checkpoints.tell.push(format!(
            "The user's instruction files {names} were updated here from the main checkout (this session's copy \
             was untouched); what you were started with is older — read them again before you rely on them."
        ));
    }
    if !failed.is_empty() {
        ed.set_error(format!("instruction files not updated: {}", failed.join("; ")));
    }
    for b in both {
        let mine = |o: &Offer| matches!(&o.what, What::Instructions(x) if x.root == b.root && x.rel == b.rel);
        ed.offers.retain(|o| !mine(o));
        ed.session.parked.retain(|o| !mine(o));
        let card = b.card();
        ed.push_offer(card);
    }
}

/// The card's answer: `y` writes the merge (`take`), `n` keeps the session's — either way the base is the main
/// checkout's now, so only its later changes are asked. On a worker; a file changed since the card is left and
/// said.
pub fn answer(ed: &mut Editor, b: Both, take: bool) {
    let path = b.root.join(&b.rel);
    let rel = b.rel.clone();
    if ed.docs.iter().any(|d| d.is_modified() && d.path.as_deref() == Some(path.as_path())) {
        return ed.set_warning(format!("{rel} has unsaved changes — asked again at claude's next start"));
    }
    let work = move || -> Result<(), String> {
        let here = plain(&b.root, &b.rel).ok_or("it isn't a plain file now — left as it is")?;
        if here != b.here {
            return Err("it changed since the card — asked again at claude's next start".into());
        }
        if take {
            crate::beneath::Dir::root(&b.root)
                .and_then(|d| d.replace(OsStr::new(&b.rel), b.merged().as_bytes()))
                .map_err(|e| e.to_string())?;
        }
        let dir = bases(&b.root).ok_or("no git folder")?;
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(&b.rel), &b.main))
            .map_err(|e| e.to_string())
    };
    let state = &mut ed.session.instructions;
    (state.answered, state.answering) = (state.answered + 1, state.answering + 1);
    #[cfg(test)]
    let held = state.held.take();
    ed.events.jobs().spawn(move || {
        #[cfg(test)]
        if let Some(held) = held {
            let _ = held.recv();
        }
        let done = work();
        move |ed: &mut Editor| {
            ed.session.instructions.answering -= 1;
            answered(ed, rel, done, take);
            again(ed);
        }
    });
}

/// An answer's write, said.
fn answered(ed: &mut Editor, rel: String, done: Result<(), String>, take: bool) {
    match (done, take) {
        (Ok(()), true) => {
            ed.set_success(format!(
                "{rel} takes the main checkout's changes — Claude reads it from its next start"
            ));
            ed.session.checkpoints.tell.push(format!(
                "The user took the main checkout's changes to {rel} into this session's; what you were started \
                 with is older — read it again before you rely on it."
            ));
        }
        (Ok(()), false) => {
            ed.set_status(format!("{rel} kept as it is — only the main checkout's later changes are asked"))
        }
        (Err(e), _) => ed.set_warning(format!("{rel}: {e}")),
    }
}
