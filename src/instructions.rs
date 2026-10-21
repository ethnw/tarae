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
    /// The main checkout's changes that touch the session's own (left as the session has them on `y`) — or,
    /// carried out, the session's that touch the main checkout's (left as it has them).
    pub left: usize,
    /// The other direction (frictions29 #79): the session's own changes since its copy, for the main checkout
    /// at this root — asked as its bring's card comes, or once it's ended (read before its checkout went: `y`
    /// carries what the card showed).
    pub out: Option<PathBuf>,
    /// Carried out once its session ended (`session_remove`): what the card showed is what `y` carries, its `n` is
    /// final — by this flag, never by its path (a session opened there again since: the review of frictions36).
    pub ended: bool,
}

impl Both {
    /// What `y` writes: the main checkout's changes over the session's own — with no base, the main checkout's.
    fn merged(&self) -> String {
        match (&self.base, &self.out) {
            (Some(b), Some(_)) => crate::checkpoint::rebase(b, &self.here, &self.main).0,
            (Some(b), None) => crate::checkpoint::rebase(b, &self.main, &self.here).0,
            (None, _) => self.main.clone(),
        }
    }

    /// The card.
    pub fn card(&self) -> Offer {
        let rel = &self.rel;
        if self.out.is_some() {
            let from = self.base.as_deref().unwrap_or(&self.here);
            let (mut added, mut out) = (0, 0);
            for (_, _, old, new) in crate::editdiff::line_changes(from, &self.here) {
                (added, out) = (added + new.lines().count(), out + old.lines().count());
            }
            let lines = match (added, out) {
                (a, 0) => crate::nav::count(a, "line") + " added",
                (0, o) => crate::nav::count(o, "line") + " taken out",
                (a, o) => format!("{} added, {o} taken out", crate::nav::count(a, "line")),
            };
            let left = match self.left {
                0 => String::new(),
                n => format!(
                    " {n} of them touch lines the main checkout changed since: those stay as it has them."
                ),
            };
            // (the changes themselves, on the card at full strength: every session's Claude reads what `y` writes —
            // and a run can write this copy; a long one is drawn cut, and its `y` waits until `v` is read to its end:
            // the review of frictions36)
            let mut shown = String::new();
            for (_, _, old, new) in crate::editdiff::line_changes(from, &self.here) {
                shown.extend(old.lines().map(|l| format!("\n  - {l}")));
                shown.extend(new.lines().map(|l| format!("\n  + {l}")));
            }
            let q = format!(
                "This session changed its {rel} since its copy ({lines}). y carries those changes into the main \
                 checkout's {rel}, v shows them, n leaves it as it is.{left}{shown}\n\nEvery session's Claude reads \
                 it from its next start."
            );
            return Offer::new(
                format!("{rel}: this session's changes for the main checkout"),
                &q,
                "Carrying them into the main checkout's…",
                format!("Couldn't update the main checkout's {rel}"),
                What::Instructions(Box::new(self.clone())),
            );
        }
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
        let (mut out, to) = match (&self.base, &self.out) {
            (_, Some(_)) => (
                format!(
                    "This session's {} since its copy — what y carries into the main checkout's:\n",
                    self.rel
                ),
                &self.here,
            ),
            (Some(_), None) => {
                (format!("The main checkout's {} since this session's copy:\n", self.rel), &self.main)
            }
            (None, None) => (format!("This session's {} → the main checkout's:\n", self.rel), &self.main),
        };
        for (start, _, old, new) in crate::editdiff::line_changes(from, to) {
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
                out: None,
                ended: false,
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
                    out: None,
                    ended: false,
                });
            }
        }
    }
    found
}

/// The other direction (frictions29 #79: carried to the main checkout by hand, a `cp` refused): the session's
/// own changes to its copies since their base, each for the main checkout's — a worker. Only a copy with a base
/// (whose change it is can be told) and changed here; one the main checkout's is the same as, nothing.
pub fn look_out(root: &Path, main: &Path) -> Vec<Both> {
    let Some(dir) = bases(root) else { return Vec::new() };
    let untracked = crate::worktree::instructions(main);
    let mut out = Vec::new();
    for rel in crate::worktree::INSTRUCTIONS.iter().filter(|r| untracked.iter().any(|u| u == *r)) {
        let (Some(main_text), Some(here)) = (plain(main, rel), plain(root, rel)) else { continue };
        let Ok(base) = std::fs::read_to_string(dir.join(rel)) else { continue };
        if here == base || here == main_text {
            continue;
        }
        let (_, left) = crate::checkpoint::rebase(&base, &here, &main_text);
        let out_to = Some(main.to_path_buf());
        let (root, rel) = (root.to_path_buf(), rel.to_string());
        out.push(Both {
            root,
            rel,
            base: Some(base),
            here,
            main: main_text,
            left,
            out: out_to,
            ended: false,
        });
    }
    out
}

/// Session `root`'s changes to its instruction files, asked for the main checkout (`look_out`) — as its bring's
/// card comes (an end looks on its own worker, before its checkout goes: `look_out_of`, `session_remove`). A card
/// each (one a file), on the session's card stack.
pub fn carry_out(ed: &mut Editor, root: PathBuf) {
    ed.events.jobs().spawn(move || {
        let found = look_out_of(&root);
        move |ed: &mut Editor| {
            if found.is_empty() {
                ed.unchanged = true;
            }
            for b in found {
                push_card(ed, b);
            }
        }
    });
}

/// `look_out` for checkout `root` against its repository's main checkout (a worker; none: it is that one).
pub fn look_out_of(root: &Path) -> Vec<Both> {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let main = crate::gitrepo::repo(&real)
        .ok()
        .and_then(|r| crate::worktree::main_checkout(&r.common_dir, &real))
        .filter(|m| *m != real);
    main.map(|main| look_out(&real, &main)).unwrap_or_default()
}

/// `b`'s card, in place of one up for the same file and direction — an ended session's (`Both.ended`): its `n`
/// drops them, never asked again, said on the card.
pub(crate) fn push_card(ed: &mut Editor, b: Both) {
    let ended = b.ended;
    let mine = |o: &Offer| matches!(&o.what, What::Instructions(x) if x.root == b.root && x.rel == b.rel && x.out == b.out);
    ed.offers.retain(|o| !mine(o));
    ed.session.parked.retain(|o| !mine(o));
    let mut card = b.card();
    if ended {
        card.question = card.question.replacen(
            "n leaves it as it is.",
            "n leaves it as it is — the session has ended: they're dropped.",
            1,
        );
    }
    ed.push_offer(card);
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
        push_card(ed, b);
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
    let theirs = b.out.as_ref().map(|m| m.join(&b.rel));
    if take && ed.docs.iter().any(|d| d.is_modified() && d.path.is_some() && d.path == theirs) {
        return ed.set_warning(format!(
            "the main checkout's {rel} has unsaved changes — save it, then bring again"
        ));
    }
    let out = b.out.is_some();
    // (Ok: whether its session had ended — its checkout gone)
    let work = move || -> Result<bool, String> {
        // (a session ended: what the card showed is what's carried — read as the end began; whatever is at its path
        // now, another session's maybe, is no part of it)
        let gone = b.out.is_some() && b.ended;
        if !gone {
            let here = plain(&b.root, &b.rel).ok_or("it isn't a plain file now — left as it is")?;
            if here != b.here {
                return Err("it changed since the card — asked again at claude's next start".into());
            }
        }
        // (into the main checkout's — still as the card read it; this session's copy then its base: the main
        // checkout's other changes come back here as untouched ones do)
        if let Some(main) = &b.out {
            if !take {
                return Ok(gone);
            }
            let now = plain(main, &b.rel);
            // (an ended session's changes are never asked again: merged into the main checkout's as it is now —
            // the review of frictions36; a session's still there is asked again at its next bring)
            let merged = match (now, gone, &b.base) {
                (Some(now), _, _) if now == b.main => b.merged(),
                (Some(now), true, Some(base)) => crate::checkpoint::rebase(base, &b.here, &now).0,
                _ => {
                    return Err(
                        "the main checkout's changed since the card — asked again at the next bring".into()
                    );
                }
            };
            crate::beneath::Dir::root(main)
                .and_then(|d| d.replace(OsStr::new(&b.rel), merged.as_bytes()))
                .map_err(|e| e.to_string())?;
            if gone {
                return Ok(true);
            }
            let dir = bases(&b.root).ok_or("no git folder")?;
            return std::fs::write(dir.join(&b.rel), &b.here).map(|_| false).map_err(|e| e.to_string());
        }
        if take {
            crate::beneath::Dir::root(&b.root)
                .and_then(|d| d.replace(OsStr::new(&b.rel), b.merged().as_bytes()))
                .map_err(|e| e.to_string())?;
        }
        let dir = bases(&b.root).ok_or("no git folder")?;
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(&b.rel), &b.main))
            .map(|_| false)
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
            match out {
                true => answered_out(ed, rel, done, take),
                false => answered(ed, rel, done.map(|_| ()), take),
            }
            again(ed);
        }
    });
}

/// A carry-out card's answer, said — `n` for an ended session's (`Ok(true)`) is final: never asked again.
fn answered_out(ed: &mut Editor, rel: String, done: Result<bool, String>, take: bool) {
    match (done, take) {
        (Ok(_), true) => ed.set_success(format!(
            "the main checkout's {rel} takes this session's changes — every session's Claude reads it from its next start"
        )),
        (Ok(true), false) => ed.set_status(format!(
            "the main checkout's {rel} left as it is — the ended session's changes to it are dropped"
        )),
        (Ok(false), false) => ed.set_status(format!(
            "the main checkout's {rel} left as it is — asked again at the next bring"
        )),
        (Err(e), _) => ed.set_warning(format!("{rel}: {e}")),
    }
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
