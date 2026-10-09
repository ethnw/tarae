//! The user's own git in a checkout, with hooks a Claude could have written (review, 2026-10-04): a
//! `core.hooksPath` inside the working tree (`.githooks`) is a folder Claude can write, and the git tab's
//! commit, amend, push, pull, fetch, switch, stash and branch work run its hooks with all the user's
//! permissions, outside the sandbox. Before such an action, on a worker: when the hooks folder is inside the
//! checkout, every file in it that is untracked, or new or changed since the session's base — not only the
//! hooks this action is likely to run (config and a pull's merge or rebase run others, and a hook runs the
//! folder's other files; the likely ones are marked) — if any, a card names them and shows them (as a script written unseen is shown — cut to fit, `v` for all of it, `y` waiting until
//! it's been read): `y` runs the action as usual · `n` runs it with no hooks this time (`gitrepo::NO_HOOKS`) ·
//! Esc back, unanswered, like every card. Committed hooks the base has as they are run as before; hooks under
//! `.git` (no Claude writes there) are never asked about. tarae's own steps never run hooks at all.

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::event::SessionId;
use crate::gitrepo::git;
use crate::offer::{Offer, What};

/// The hooks an action is likely to run — only said on the card (`runs on this pull`): what is asked about is
/// every file in the hooks folder, since config and a pull's merge or rebase run others (pre-merge-commit,
/// pre-rebase, post-rewrite …) and a hook can run the folder's other files (review, 2026-10-04).
pub const COMMIT: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "reference-transaction",
    "pre-auto-gc",
];
pub const AMEND: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-rewrite",
    "reference-transaction",
    "pre-auto-gc",
];
pub const PUSH: &[&str] = &["pre-push", "reference-transaction"];
pub const PULL: &[&str] = &["post-merge", "post-checkout", "reference-transaction", "pre-auto-gc"];
pub const FETCH: &[&str] = &["reference-transaction", "pre-auto-gc"];
pub const SWITCH: &[&str] = &["post-checkout", "reference-transaction"];
pub const STASH: &[&str] = &["post-checkout", "reference-transaction"];
pub const REFS: &[&str] = &["reference-transaction"];

/// The args put before an action's own: none (its hooks run) · `NO_HOOKS`.
pub type Pre = &'static [&'static str];

/// `pre` then `args`, as git takes them.
pub fn with<'a>(pre: Pre, args: &[&'a str]) -> Vec<&'a str> {
    pre.iter().copied().chain(args.iter().copied()).collect()
}

/// A file of the hooks folder the card asks about: where it is (relative to the checkout), how it differs from
/// the base, whether the action is likely to run it (a hook by its name), and its text when a card can show it
/// as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub rel: String,
    pub kind: &'static str,
    pub likely: bool,
    pub text: Option<String>,
}

/// Files of a hooks folder looked at, at most (and folders deep) — a folder past that is asked about whole.
const FILES_MAX: usize = 256;
const DEPTH_MAX: usize = 3;

/// What the card says: the action, against what base, the hooks.
#[derive(Clone, Debug)]
pub struct Ask {
    pub what: String,
    pub base: String,
    pub hooks: Vec<Hook>,
}

impl Ask {
    /// `y` is offered only when every hook could be shown.
    pub fn runnable(&self) -> bool {
        self.hooks.iter().all(|h| h.text.is_some())
    }

    fn card(&self) -> Offer {
        let n = self.hooks.len();
        // (short: the card often shares the code's width with the chat)
        // (the checkout tarae started in compares with its last commit — said so, not as `HEAD`)
        let base = if self.base == "HEAD" { "the last commit" } else { self.base.as_str() };
        let mut q = match n {
            1 => format!("This hook isn't as {base} has it:"),
            n => format!("These {n} hooks aren't as {base} has them:"),
        };
        for h in &self.hooks {
            let likely = if h.likely { format!(" · this {} runs it", self.what) } else { String::new() };
            q += &format!("\n  {} — {}{likely}", h.rel, h.kind);
        }
        q += match n {
            1 => "\nIt runs as you, unsandboxed — a Claude here can write it.",
            _ => "\nThey run as you, unsandboxed — a Claude here can write them.",
        };
        let warning = n + 1..n + 2;
        for h in &self.hooks {
            match &h.text {
                Some(t) => {
                    let t = t.replace("\r\n", "\n").replace('\t', "    ");
                    q += &format!("\n\n{}:\n{}", h.rel, t.trim_end_matches('\n'));
                }
                None => {
                    q += &format!(
                        "\n\n{}: not text this card can show as it is (binary, or characters that draw as something \
                     else) — only n is offered",
                        h.rel
                    )
                }
            }
        }
        Offer::new(
            format!("Hooks before the {}", self.what),
            &q,
            "Going on…",
            format!("Couldn't {}", self.what),
            What::Hooks(Box::new(self.clone())),
        )
        .deciding(0..n + 1)
        .warning(warning)
    }
}

/// A git-tab action, given the args to put before its git's own.
type Go = Box<dyn FnOnce(&mut Editor, Pre) + Send>;

/// A git-tab action waiting on its card: the card, the session it's in, the action.
pub struct Wait {
    card: u64,
    session: SessionId,
    go: Go,
}

/// The files of the hooks folder git uses in the checkout `top`, when that folder is inside it, that are
/// untracked or new or changed since `base` — every file, not only the hooks this action runs (`likely` marks
/// those): config decides what an action runs, and a hook runs the folder's other files (on a worker). A hooks
/// folder outside the checkout, or under its git folder: none.
pub fn unseen(top: &Path, base: &str, likely: &[&str]) -> Vec<Hook> {
    let path = |args: &[&str]| -> Option<PathBuf> {
        let p = PathBuf::from(git(top, args, None).ok()?.trim());
        Some(std::fs::canonicalize(&p).unwrap_or(p))
    };
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let top = real(top);
    let Some(dir) = path(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"]) else {
        return Vec::new();
    };
    let gits = [
        path(&["rev-parse", "--path-format=absolute", "--git-dir"]),
        path(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ];
    if !dir.starts_with(&top) || gits.iter().flatten().any(|g| dir.starts_with(g)) {
        return Vec::new();
    }
    // Every file in it (links not followed — a link's target is read as the file it is)
    let mut files = Vec::new();
    let mut stack = vec![(dir.clone(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() && depth < DEPTH_MAX {
                stack.push((e.path(), depth + 1));
            } else if !t.is_dir() && files.len() < FILES_MAX {
                files.push(e.path());
            }
        }
    }
    crate::sorting::sort(&mut files);
    let mut out = Vec::new();
    for p in files {
        let Ok(rel) = p.strip_prefix(&top).map(|r| r.to_string_lossy().into_owned()) else { continue };
        let Ok(now) = std::fs::read(&p) else { continue };
        let then = git(top.as_path(), &["show", &format!("{base}:{rel}")], None);
        if then.as_deref().is_ok_and(|t| t.as_bytes() == now.as_slice()) {
            continue;
        }
        let tracked = git(&top, &["ls-files", "--error-unmatch", "--", &rel], None).is_ok();
        let kind = match (tracked, then.is_ok()) {
            (false, _) => "untracked",
            (true, false) => "new",
            (true, true) => "changed",
        };
        let text = String::from_utf8(now).ok().filter(|t| crate::tools::draws_as_itself(t));
        // (a hook git runs by its name, at the folder's top)
        let named = p.parent() == Some(dir.as_path())
            && p.file_name().is_some_and(|n| likely.iter().any(|l| n == *l));
        out.push(Hook { rel, kind, likely: named, text });
    }
    // (the ones this action runs first)
    crate::sorting::by_key(&mut out, |h| !h.likely);
    out
}

impl Editor {
    /// A git-tab action (`what`, the hooks it's likely to run `names`) — at once when nothing in the hooks
    /// folder is one to ask about, else after its card: `go` gets the args to put before its own.
    pub(crate) fn git_hooked(
        &mut self,
        what: &str,
        names: &'static [&'static str],
        go: impl FnOnce(&mut Editor, Pre) + Send + 'static,
    ) {
        let cwd = self.session.git_status.as_ref().map_or_else(|| self.git_cwd(), |s| s.top.clone());
        // (against the session's base — else what its checkout has committed)
        let base = self.session.base.clone().unwrap_or_else(|| "HEAD".into());
        let (what, session) = (what.to_string(), self.session.id);
        self.events.jobs().spawn(move || {
            let hooks =
                crate::gitpanel::top_of(&cwd).map(|top| unseen(&top, &base, names)).unwrap_or_default();
            move |ed: &mut Editor| {
                if hooks.is_empty() {
                    return go(ed, &[]);
                }
                let o = Ask { what, base, hooks }.card();
                let title = o.title.clone();
                ed.push_offer(o);
                // (its id — a card of that title up already took this one's text and keeps its id: the action
                // waiting on it before is the one this replaces)
                let Some(card) =
                    ed.offers.iter().chain(&ed.session.parked).find(|o| o.title == title).map(|o| o.id)
                else {
                    return;
                };
                ed.hook_waits.retain(|w| w.card != card);
                ed.hook_waits.push(Wait { card, session, go: Box::new(go) });
            }
        });
    }

    /// The hooks card answered: `run` — its action as usual; else with no hooks this time.
    pub(crate) fn hooks_answer(&mut self, card: u64, ask: Ask, run: bool) {
        self.offer_finished(card, Ok(()));
        let Some(i) = self.hook_waits.iter().position(|w| w.card == card) else { return };
        let w = self.hook_waits.remove(i);
        let pre: Pre = if run { &[] } else { &crate::gitrepo::NO_HOOKS };
        if !run {
            self.note(format!("{} without hooks this time", ask.what));
        }
        self.with_session(w.session, |ed| (w.go)(ed, pre));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_names_each_hook_and_shows_it() {
        let hook = |name: &str, kind, likely, text: Option<&str>| Hook {
            rel: format!(".githooks/{name}"),
            kind,
            likely,
            text: text.map(str::to_string),
        };
        let ask = Ask {
            what: "commit".into(),
            base: "main".into(),
            hooks: vec![
                hook("pre-commit", "changed", true, Some("#!/bin/sh\nmake lint\n")),
                hook("lib.sh", "untracked", false, Some("#!/bin/sh\n")),
            ],
        };
        let o = ask.card();
        assert_eq!(o.title, "Hooks before the commit");
        assert!(
            o.question.starts_with(
                "These 2 hooks aren't as main has them:\n  .githooks/pre-commit — changed · this commit runs it\n  \
                 .githooks/lib.sh — untracked\nThey run as you, unsandboxed — a Claude here can write them.\n\n"
            ),
            "{}",
            o.question
        );
        assert!(o.question.contains("\n\n.githooks/pre-commit:\n#!/bin/sh\nmake lint\n\n"), "{}", o.question);
        assert_eq!((o.decided_lines(), o.warning.clone()), (0..3, Some(3..4)));
        assert!(ask.runnable());
        // One, in the checkout tarae started in (its last commit — not said as `HEAD`); one the card can't show
        let binary = Ask { base: "HEAD".into(), hooks: vec![hook("pre-push", "new", false, None)], ..ask };
        let q = binary.card().question;
        assert!(
            q.starts_with(
                "This hook isn't as the last commit has it:\n  .githooks/pre-push — new\nIt runs as you"
            ),
            "{q}"
        );
        assert!(!binary.runnable() && q.contains("only n is offered"));
    }
}
