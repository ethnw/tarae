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
//!
//! A `y` is kept for each file's content (piece F, 2026-10-17): a trust answer in `$state/trust.json` (the
//! repository's, its checkouts share it — `trust::record`), the file's SHA-256 as the version — so a hook
//! answered isn't asked about again until it's rewritten. `n` is this time's only.

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
/// as it is, and its content's SHA-256 (what a `y` keeps).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub rel: String,
    pub kind: &'static str,
    pub likely: bool,
    pub text: Option<String>,
    pub hash: crate::trust::Digest,
}

/// Files of a hooks folder looked at, at most (and folders deep) — a folder past that is asked about whole.
const FILES_MAX: usize = 256;
const DEPTH_MAX: usize = 3;

/// What a hook the walk can't look at is (its `kind`): asked about, never left out — its text None, so the
/// card offers only `n` and no `y` is kept for it (auditfix2 F10); its hash `UNHASHED`.
const UNREADABLE: &str = "can't be read";
const NOT_A_FILE: &str = "not a plain file";
const NOT_LOOKED: &str = "not looked at";
const UNHASHED: crate::trust::Digest = [0; 32];

/// What the card says: the action, against what base, the hooks — of the checkout `top` (where a `y` is kept).
#[derive(Clone, Debug)]
pub struct Ask {
    pub what: String,
    pub base: String,
    pub hooks: Vec<Hook>,
    pub top: PathBuf,
}

impl Ask {
    /// `y` is offered only when every hook could be shown.
    pub fn runnable(&self) -> bool {
        self.hooks.iter().all(|h| h.text.is_some())
    }

    pub(crate) fn card(&self) -> Offer {
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
        if self.runnable() {
            q += match n {
                1 => "\nA yes is kept for this text — a change asks again.",
                _ => "\nA yes is kept for these texts — a change asks again.",
            };
        }
        let warning = n + 1..n + 2;
        for h in &self.hooks {
            match &h.text {
                Some(t) => {
                    let t = t.replace("\r\n", "\n").replace('\t', "    ");
                    q += &format!("\n\n{}:\n{}", h.rel, t.trim_end_matches('\n'));
                }
                None if [UNREADABLE, NOT_A_FILE, NOT_LOOKED].contains(&h.kind) => {
                    q += &format!("\n\n{}: {} — only n is offered", h.rel, h.kind)
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

/// The hooks folder git uses in the checkout `top` (by real path), when that folder is inside it and not
/// under its git folder — the one a Claude could write. With `top` by its real path.
pub fn folder(top: &Path) -> Option<(PathBuf, PathBuf)> {
    let path = |args: &[&str]| -> Option<PathBuf> {
        let p = PathBuf::from(git(top, args, None).ok()?.trim());
        Some(std::fs::canonicalize(&p).unwrap_or(p))
    };
    let top = std::fs::canonicalize(top).unwrap_or_else(|_| top.to_path_buf());
    let dir = path(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?;
    let gits = [
        path(&["rev-parse", "--path-format=absolute", "--git-dir"]),
        path(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ];
    (dir.starts_with(&top) && !gits.iter().flatten().any(|g| dir.starts_with(g))).then_some((top, dir))
}

/// The files of the hooks folder git uses in the checkout `top`, when that folder is inside it, that are
/// untracked or new or changed since `base` — every file, not only the hooks this action runs (`likely` marks
/// those): config decides what an action runs, and a hook runs the folder's other files (on a worker). A hooks
/// folder outside the checkout, or under its git folder: none. One whose content a `y` kept in the trust
/// store at `store` (`Ask::top`'s file, its SHA-256) isn't asked about. What it can't look at — a file it
/// can't read or that isn't a plain one, a folder it can't list or deeper than `DEPTH_MAX`, the files past
/// `FILES_MAX` — is asked about too, never left out: it can't be shown, so only `n` is offered.
pub fn unseen(top: &Path, base: &str, likely: &[&str], store: Option<&Path>) -> Vec<Hook> {
    let Some((top, dir)) = folder(top) else { return Vec::new() };
    let rel_of = |p: &Path| p.strip_prefix(&top).ok().map(|r| r.to_string_lossy().into_owned());
    let mut out = Vec::new();
    // (a folder, or what's past the walk in it: `p` from the checkout's top, then `then`)
    let mut whole = |p: &Path, then: String, kind| {
        let rel = rel_of(p).unwrap_or_default() + &then;
        out.push(Hook { rel, kind, likely: false, text: None, hash: UNHASHED })
    };
    // Every file in it (links not followed — a link's target is read as the file it is)
    let mut files = Vec::new();
    let mut past = 0;
    let mut stack = vec![(dir.clone(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            whole(&d, "/".into(), NOT_LOOKED);
            continue;
        };
        for e in rd {
            let Ok(e) = e else {
                whole(&d, "/".into(), NOT_LOOKED);
                break;
            };
            // (a type it can't tell: as a file — opening it says what it is)
            match e.file_type() {
                Ok(t) if t.is_dir() && depth < DEPTH_MAX => stack.push((e.path(), depth + 1)),
                Ok(t) if t.is_dir() => whole(&e.path(), "/".into(), NOT_LOOKED),
                _ if files.len() < FILES_MAX => files.push(e.path()),
                _ => past += 1,
            }
        }
    }
    if past > 0 {
        whole(&dir, format!("/… {past} more files"), NOT_LOOKED);
    }
    crate::sorting::sort(&mut files);
    let kept = store.map(|s| crate::trust::Store::read(Some(s)));
    for p in files {
        let Some(rel) = rel_of(&p) else { continue };
        // (a hook git runs by its name, at the folder's top)
        let named = p.parent() == Some(dir.as_path())
            && p.file_name().is_some_and(|n| likely.iter().any(|l| n == *l));
        // (opened without waiting — a FIFO has no writer — and read from what was opened)
        let now = match crate::paths::open_read(&p) {
            Err(_) => Err(UNREADABLE),
            Ok(o) if !o.meta.is_file() => Err(NOT_A_FILE),
            Ok(o) => {
                let mut now = Vec::new();
                std::io::Read::read_to_end(&mut &o.file, &mut now).map(|_| now).map_err(|_| UNREADABLE)
            }
        };
        let now = match now {
            Ok(now) => now,
            Err(kind) => {
                out.push(Hook { rel, kind, likely: named, text: None, hash: UNHASHED });
                continue;
            }
        };
        let then = git(top.as_path(), &["show", &format!("{base}:{rel}")], None);
        if then.as_deref().is_ok_and(|t| t.as_bytes() == now.as_slice()) {
            continue;
        }
        let hash = crate::sha256::digest(&now);
        if kept.as_ref().is_some_and(|k| k.state(&p, hash) == crate::trust::State::Trusted) {
            continue;
        }
        let tracked = git(&top, &["ls-files", "--error-unmatch", "--", &rel], None).is_ok();
        let kind = match (tracked, then.is_ok()) {
            (false, _) => "untracked",
            (true, false) => "new",
            (true, true) => "changed",
        };
        let text = String::from_utf8(now).ok().filter(|t| crate::tools::draws_as_itself(t));
        out.push(Hook { rel, kind, likely: named, text, hash });
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
        let (what, session, store) = (what.to_string(), self.session.id, self.trust_path.clone());
        self.events.jobs().spawn(move || {
            let top = crate::gitpanel::top_of(&cwd).ok();
            let hooks =
                top.as_ref().map(|top| unseen(top, &base, names, store.as_deref())).unwrap_or_default();
            move |ed: &mut Editor| {
                let Some(top) = top.filter(|_| !hooks.is_empty()) else {
                    return go(ed, &[]);
                };
                let o = Ask { what, base, hooks, top }.card();
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

    /// The hooks card answered: `run` — each hook's content kept as trusted (on a worker, then its action as
    /// usual: the next action reads the store after it's written); else with no hooks this time, nothing kept.
    pub(crate) fn hooks_answer(&mut self, card: u64, ask: Ask, run: bool) {
        self.offer_finished(card, Ok(()));
        let Some(i) = self.hook_waits.iter().position(|w| w.card == card) else { return };
        let w = self.hook_waits.remove(i);
        let pre: Pre = if run { &[] } else { &crate::gitrepo::NO_HOOKS };
        if !run {
            self.note(format!("{} without hooks this time", ask.what));
        }
        let go = move |ed: &mut Editor| {
            ed.with_session(w.session, |ed| (w.go)(ed, pre));
        };
        let Some(path) = self.trust_path.clone().filter(|_| run) else { return go(self) };
        self.events.jobs().spawn(move || {
            let r: Result<(), String> = ask
                .hooks
                .iter()
                .try_for_each(|h| crate::trust::record(path.clone(), ask.top.join(&h.rel), h.hash, true));
            move |ed: &mut Editor| {
                if let Err(e) = r {
                    ed.set_error(format!("couldn't keep the answer: {e}"));
                }
                go(ed)
            }
        });
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
            hash: [0; 32],
        };
        let ask = Ask {
            what: "commit".into(),
            base: "main".into(),
            hooks: vec![
                hook("pre-commit", "changed", true, Some("#!/bin/sh\nmake lint\n")),
                hook("lib.sh", "untracked", false, Some("#!/bin/sh\n")),
            ],
            top: PathBuf::from("/p"),
        };
        let o = ask.card();
        assert_eq!(o.title, "Hooks before the commit");
        assert!(
            o.question.starts_with(
                "These 2 hooks aren't as main has them:\n  .githooks/pre-commit — changed · this commit runs it\n  \
                 .githooks/lib.sh — untracked\nThey run as you, unsandboxed — a Claude here can write them.\nA yes \
                 is kept for these texts — a change asks again.\n\n"
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
        assert!(!binary.runnable() && q.contains("only n is offered") && !q.contains("A yes is kept"));
    }

    /// git in the test's own repository — no hooks, no signing, a name of its own.
    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
            .args(["-c", "user.email=t@example.invalid", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repository in `s` whose hooks folder is `.githooks` in the checkout (its commands trusted, as the
    /// user's `y` on their card), `files` committed there as they are.
    fn checkout(s: &crate::scratch::Scratch, files: &[(&str, &[u8])]) -> PathBuf {
        let dir = std::fs::canonicalize(s.path()).unwrap();
        for (rel, bytes) in files {
            let p = dir.join(".githooks").join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        git_in(&dir, &["init", "-q"]);
        git_in(&dir, &["add", "-A"]);
        git_in(&dir, &["commit", "-q", "-m", "base"]);
        git_in(&dir, &["config", "core.hooksPath", ".githooks"]);
        let r = crate::gitrepo::repo(&dir).unwrap();
        crate::gitrepo::answered(&r.config_file(), r.local.digest(), true);
        dir
    }

    /// (rel, kind, shown) of each.
    fn said(hooks: &[Hook]) -> Vec<(&str, &str, bool)> {
        hooks.iter().map(|h| (h.rel.as_str(), h.kind, h.text.is_some())).collect()
    }

    /// auditfix2 F10: what the walk can't read is asked about, never left out — a hook made `0111` (git runs
    /// it: `execve` needs no read), and a FIFO (opened without waiting for a writer: the action would never
    /// start). Neither can be shown: only `n`.
    #[test]
    fn a_hook_it_cant_read_is_asked_about() {
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let s = crate::scratch::Scratch::new("hooks-unreadable");
        let dir = checkout(&s, &[("pre-commit", b"#!/bin/sh\nexit 0\n")]);
        assert_eq!(said(&unseen(&dir, "HEAD", COMMIT, None)), [], "as the last commit has it");
        let hook = dir.join(".githooks/pre-commit");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o111)).unwrap();
        // (as root every file reads: nothing to see there)
        if std::fs::File::open(&hook).is_err() {
            let hooks = unseen(&dir, "HEAD", COMMIT, None);
            assert_eq!(said(&hooks), [(".githooks/pre-commit", "can't be read", false)]);
            assert!(!Ask { what: "commit".into(), base: "HEAD".into(), hooks, top: dir.clone() }.runnable());
        }
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fifo = std::ffi::CString::new(dir.join(".githooks/pipe").into_os_string().into_vec()).unwrap();
        // SAFETY: a path of the test's own scratch folder, live for the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let top = dir.clone();
        std::thread::spawn(move || tx.send(unseen(&top, "HEAD", COMMIT, None)));
        let hooks = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("never waits on a FIFO");
        assert_eq!(said(&hooks), [(".githooks/pipe", "not a plain file", false)]);
    }

    /// auditfix2 F10: past what the walk looks at, the rest is asked about whole — 300 files committed as they
    /// are (more than `FILES_MAX`: which ones `read_dir` gives first isn't said), and a folder deeper than
    /// `DEPTH_MAX`.
    #[test]
    fn past_what_is_looked_at_is_asked_about_whole() {
        let s = crate::scratch::Scratch::new("hooks-many");
        let names: Vec<String> = (0..300).map(|i| format!("d{i:03}")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"#!/bin/sh\n"[..])).collect();
        let dir = checkout(&s, &files);
        let hooks = unseen(&dir, "HEAD", COMMIT, None);
        assert_eq!(said(&hooks), [(".githooks/… 44 more files", "not looked at", false)]);
        let s = crate::scratch::Scratch::new("hooks-deep");
        let dir = checkout(&s, &[("pre-commit", b"#!/bin/sh\n"), ("a/b/c/d/lib.sh", b"#!/bin/sh\n")]);
        let hooks = unseen(&dir, "HEAD", COMMIT, None);
        assert_eq!(said(&hooks), [(".githooks/a/b/c/d/", "not looked at", false)]);
        let q = Ask { what: "commit".into(), base: "HEAD".into(), hooks, top: dir.clone() }.card().question;
        assert!(q.contains("\n\n.githooks/a/b/c/d/: not looked at — only n is offered"), "{q}");
    }
}
