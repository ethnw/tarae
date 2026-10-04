//! Claude's edits from the chat — every `Edit`/`Write` it makes is a permission request (claude runs with
//! `--permission-prompt-tool stdio`; those tools aren't pre-allowed) that tarae shows as the in-buffer review
//! (y/n per change) before anything is written.
//!
//! - claude sends `control_request {subtype: "can_use_tool", tool_name, input}` and waits; we answer
//!   `{behavior: "allow", updatedInput}` (claude writes the file) or `{behavior: "deny", message}` (measured with
//!   the real CLI).
//! - The review applies the accepted changes to the buffer; the answer then makes claude write exactly that:
//!   all of it → its own input; part of it → `updatedInput` rewritten (an Edit becomes one unique region swap,
//!   a Write the accepted text). The buffer already matches, so disk sync takes the write as a save.
//! - A buffer with unsaved edits differs from disk: the change stays in the buffer and claude is told so.
//! - One review at a time: more requests wait (`next`, every event); the turn ending closes a pending one.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::document::DocId;
use crate::editor::Editor;
use crate::offer::{Offer, What};

/// A permission request waiting for (or in) its review.
pub struct Pending {
    pub request_id: String,
    pub tool: String,
    pub input: Value,
    /// What Claude thought or said just before asking (the review shows it: why this edit).
    pub reason: String,
}

#[derive(Default)]
pub struct Edits {
    pub queue: VecDeque<Pending>,
    /// The review showing one of ours (its `llm_generation`).
    pub review: Option<u64>,
    /// Review at the end (`space v a`, agreed on its card): edits under this folder go in as Claude makes
    /// them, and the turn is reviewed when it ends.
    pub at_end: Option<PathBuf>,
    /// Claude's renames and replaces landing (`refactor.rs`): the front one's files reviewed one at a time.
    pub sets: VecDeque<crate::refactor::Landing>,
    /// An edit waiting while you type has been said (once until it's shown).
    pub waiting_told: bool,
}

// ── Review at the end ───────────────────────────────────────────────────────

const AT_END_TITLE: &str = "Review Claude's edits at the end?";

/// What the card asks — what turning it on means.
const AT_END_ASK: &str = "Claude's edits go into your files as it makes them — before you see them — and the \
checks it runs (cargo test, cargo build) run that code.\n\nWhen its turn ends you review all of it; space u \
takes the turn back. Build scripts, settings and files outside the project still ask first.";

/// What Claude hears with its next message when it's turned on · off.
const TELL_ON: &str = "From now on in this conversation your Edit and Write calls are applied as you make them, \
without the user seeing each one: they review all of your changes when your turn ends. Edits outside the project, \
to build scripts and settings, and to files with unsaved edits still go to the user first.";
const TELL_OFF: &str =
    "Your Edit and Write calls are shown to the user one at a time again, before they're written.";

/// Whether the chat's edits are reviewed at the end of its turn.
pub fn at_end(ed: &Editor) -> bool {
    ed.session.chat.as_ref().is_some_and(|c| c.edits.at_end.is_some())
}

/// `space v a` · `:review-at-end [on|off]` (`want`; None flips it): on asks first, on its card.
pub fn at_end_toggle(ed: &mut Editor, want: Option<bool>) {
    let on = at_end(ed);
    let want = want.unwrap_or(!on);
    ed.offers.retain(|o| !matches!(o.what, What::ReviewAtEnd));
    if want == on {
        return ed.set_status(match on {
            true => "review at the end is on — space v a turns it off",
            false => "Claude's edits come one at a time",
        });
    }
    if !want {
        return at_end_set(ed, false);
    }
    let card = Offer::new(AT_END_TITLE.into(), AT_END_ASK, "", String::new(), What::ReviewAtEnd);
    ed.push_offer(card);
}

/// A new conversation (the chat opened, `C-l`, `:chat-new`): review at the end as `llm.review-at-end`
/// says — on, Claude hears it with its first message.
pub fn at_end_new_chat(ed: &mut Editor) {
    let on = ed.config.llm.review_at_end;
    let root = ed.session.root.clone();
    let Some(c) = &mut ed.session.chat else { return };
    c.edits.at_end = on.then_some(root);
    let tell = &mut ed.session.checkpoints.tell;
    tell.retain(|t| t != TELL_ON && t != TELL_OFF);
    if on {
        tell.push(TELL_ON.to_string());
    }
}

/// `y` on the card (on) · turned off: for this chat, the project folder it was agreed for — and for the
/// chats after it (`llm.review-at-end`, kept in the config file).
pub fn at_end_set(ed: &mut Editor, on: bool) {
    // (opened as it was set — it's told below)
    if ed.session.chat.is_none() {
        crate::chat::open(ed);
        if let Some(c) = &mut ed.session.chat {
            c.focused = false;
            c.unfolded = false;
        }
    }
    ed.config.llm.review_at_end = on;
    ed.overrides.retain(|(p, _)| p != "llm.review-at-end");
    // (here, as `:set!` writes: on a worker each, a quick on-off could land off-on — and the config watch
    // would take that back — or cross a `:set!` of the same file)
    let kept = ed.config_file.as_ref().map(|f| crate::config::persist_to(f, "llm.review-at-end", &on.into()));
    if let Some(Err(e)) = kept {
        ed.set_error(format!("review at the end isn't kept for the next chats: {e}"));
    }
    let root = ed.session.root.clone();
    let Some(c) = &mut ed.session.chat else { return };
    c.edits.at_end = on.then_some(root);
    let row = match on {
        true => "review at the end · space v a turns it off",
        false => "Claude's edits come one at a time again",
    };
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Note, row, ""));
    // Claude hears the change with its next message — unless it never heard of the other
    let (say, unsay) = if on { (TELL_ON, TELL_OFF) } else { (TELL_OFF, TELL_ON) };
    let tell = &mut ed.session.checkpoints.tell;
    match tell.iter().position(|t| t == unsay) {
        Some(i) => {
            tell.remove(i);
        }
        None => tell.push(say.to_string()),
    }
    match on {
        true => ed.set_success(
            "Claude's edits go in as it works — you review the turn when it ends (new chats too)",
        ),
        false => ed.set_status(row),
    }
}

/// A `can_use_tool` request: answered now (Some) or after its review (None).
pub fn request(ed: &mut Editor, request_id: &str, req: &Value) -> Option<Value> {
    let tool = req["tool_name"].as_str().unwrap_or_default();
    if crate::reads::TOOLS.contains(&tool) {
        return crate::reads::request(ed, request_id, req);
    }
    if !matches!(tool, "Edit" | "Write") {
        return Some(deny(&format!("{tool} isn't available in tarae's chat")));
    }
    let c = ed.session.chat.as_mut()?;
    // Its words right before this call, this turn — taken now, so each queued edit keeps its own
    let reason = reason(c);
    let p = Pending {
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input: req["input"].clone(),
        reason,
    };
    match c.edits.at_end.clone() {
        Some(root) => unasked(ed, root, p),
        None => {
            c.edits.queue.push_back(p);
            next(ed);
        }
    }
    None
}

/// Why Claude makes the change it's calling for now (its review shows it): its words right before this call
/// (after the call's own row), not ones from before earlier calls — else the plan's step in hand.
pub fn reason(c: &crate::chat::Chat) -> String {
    use crate::chat::Role;
    let turn = c.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
    let before: Vec<_> = c.msgs[turn..].iter().rev().skip_while(|m| m.role == Role::Tool).collect();
    let words = before
        .iter()
        .take_while(|m| m.role != Role::Tool)
        .find(|m| matches!(m.role, Role::Thought | Role::Assistant))
        .map(|m| crate::chat::displayed(&m.text).split_whitespace().collect::<Vec<_>>().join(" "));
    let step = c.plan.as_ref().and_then(|p| {
        let i = p.current()?;
        Some(format!("step {}/{}: {}", i + 1, p.steps.len(), p.steps[i].text))
    });
    words.filter(|w| !w.is_empty()).or(step).unwrap_or_default()
}

/// Review at the end: whether the edit goes in unasked is worked out on a worker — where its path really
/// leads, and the file's text before it — then it's allowed as claude asked (claude writes it; the watch
/// brings an open buffer along), or shown as a review after all.
fn unasked(ed: &mut Editor, root: PathBuf, p: Pending) {
    let path = crate::chat::resolve(&ed.session.root, p.input["file_path"].as_str().unwrap_or_default());
    let ask = ed.config.llm.review_ask.clone();
    let generation = ed.session.chat.as_ref().map(crate::chat::Chat::generation);
    ed.events.jobs().spawn(move || {
        // (Some: it goes in — the real path, and what's there now: None = no file)
        let found = inside(&root, &path)
            .filter(|real| !real.strip_prefix(&root).is_ok_and(|rel| listed(rel, &ask)))
            .and_then(|real| Some((before_text(&real)?, real)));
        move |ed: &mut Editor| {
            // (a new chat, claude restarted: no one waits for this answer)
            if ed.session.chat.as_ref().map(crate::chat::Chat::generation) != generation {
                return;
            }
            // Unsaved edits of the user's: disk isn't what they see — the usual review
            let dirty = |real: &PathBuf| {
                ed.docs
                    .iter()
                    .any(|d| d.path.as_ref() == Some(real) && d.is_modified() && !d.unsaved_is_claudes())
            };
            match found {
                Some((before, real)) if at_end(ed) && !dirty(&real) => {
                    // (follow shows the edit against what it was, once it's written)
                    crate::follow::before_edit(ed, real.clone(), before.clone());
                    // (allowed once what it was is on disk)
                    let before = before.map(ropey::Rope::from);
                    crate::checkpoint::record_unseen(ed, real, before, move |ed: &mut Editor| {
                        if ed.session.chat.as_ref().map(crate::chat::Chat::generation) == generation {
                            answer(ed, &p.request_id, allow(p.input));
                        }
                    });
                }
                _ => {
                    if let Some(c) = &mut ed.session.chat {
                        c.edits.queue.push_back(p);
                    }
                    next(ed);
                }
            }
        }
    });
}

/// Where `path` really leads — its deepest existing folder resolved (links followed, `..` gone through) —
/// if that's inside `root` (resolved itself).
pub fn inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    real(path).filter(|real| real.starts_with(&root))
}

/// Where `path` really leads: its deepest existing folder resolved (links followed, `..` gone through),
/// the names below it as they are — None if one of those is `.`/`..` (it would climb unseen).
pub fn real(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut at = path.to_path_buf();
    let real = loop {
        if let Ok(real) = std::fs::canonicalize(&at) {
            break real;
        }
        rest.push(at.file_name()?.to_os_string());
        at = at.parent()?.to_path_buf();
    };
    // What doesn't exist yet: plain names only (a `..` there would climb out unseen)
    let mut real = real;
    for name in rest.iter().rev() {
        if name == ".." || name == "." {
            return None;
        }
        real.push(name);
    }
    Some(real)
}

/// Whether an edit to `rel` (from the project's root) still asks first: anything in `.git`, or an entry of
/// `llm.review-ask` — a name matches any part of the path, a path (with `/`) its start.
pub fn listed(rel: &Path, ask: &[String]) -> bool {
    let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
    parts.iter().any(|p| *p == ".git")
        || ask
            .iter()
            .map(|a| a.trim().trim_end_matches('/'))
            .filter(|a| !a.is_empty())
            .any(|a| if a.contains('/') { rel.starts_with(a) } else { parts.iter().any(|p| *p == a) })
}

/// The file's text before an edit (None inside: no such file) — None when it can't be told (not text, not
/// readable): then it's a review.
fn before_text(path: &Path) -> Option<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).ok().map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

fn allow(input: Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": input })
}

fn deny(message: &str) -> Value {
    json!({ "behavior": "deny", "message": message })
}

/// Every event: shows the next waiting edit once no review is up — not while you're typing (insert mode,
/// the command line, a picker): it waits, said once, and comes up when you stop (#54: it took the screen,
/// and the next keys answered it).
pub fn next(ed: &mut Editor) {
    // (a session behind: its edits wait in their queue until it's in front)
    if ed.session.review.is_some() || ed.background() {
        return;
    }
    let typing = ed.mode == crate::editor::Mode::Insert || ed.prompt.is_some() || ed.picker.is_some();
    let Some(c) = ed.session.chat.as_mut() else { return };
    if typing {
        let Some(p) = c.edits.queue.front().filter(|_| !c.edits.waiting_told) else { return };
        c.edits.waiting_told = true;
        let path = crate::chat::resolve(&ed.session.root, p.input["file_path"].as_str().unwrap_or_default());
        let shown = crate::chat::shown(&ed.session.root, &path);
        return ed
            .set_status(format!("Claude's edit to {shown} waits — it opens when you stop typing (esc)"));
    }
    c.edits.waiting_told = false;
    let Some(p) = c.edits.queue.pop_front() else { return };
    show(ed, p);
}

fn show(ed: &mut Editor, p: Pending) {
    let s = |k: &str| p.input[k].as_str().unwrap_or_default().to_string();
    let path = crate::chat::resolve(&ed.session.root, &s("file_path"));
    // (a file Claude creates goes again when its turn is undone)
    let open_before = ed.docs.iter().any(|d| d.path.as_deref() == Some(path.as_path()));
    let existed = open_before || path.exists();
    if let Err(e) = ed.open(&path) {
        return answer(ed, &p.request_id, deny(&format!("tarae couldn't open {}: {e:#}", path.display())));
    }
    // Outside the project (#37) or in its build output (#46), and opened for this: it goes when the turn
    // ends, unless you take it up
    let root = ed.session.root.clone();
    let doc = ed.doc_mut();
    if !open_before && doc.path.as_ref().is_some_and(|p| aside_path(&root, p)) {
        doc.aside = true;
    }
    if ed.doc().loading {
        return answer(
            ed,
            &p.request_id,
            deny("the file is still loading in the editor — try again in a moment"),
        );
    }
    let doc = ed.doc();
    // (unsaved only by Claude's write of its previous edit — on its way, the watch may not have seen it yet)
    let dirty = doc.is_modified() && !doc.unsaved_is_claudes();
    let (doc_id, base) = (doc.id, doc.text.to_string());
    let proposed = match p.tool.as_str() {
        "Write" => s("content"),
        _ => {
            let (old, new) = (s("old_string"), s("new_string"));
            let all = p.input["replace_all"].as_bool() == Some(true);
            let n = if old.is_empty() { 0 } else { base.matches(old.as_str()).count() };
            if n == 0 || (n > 1 && !all) {
                // Not as the user has it (or ambiguous): claude's own checks answer on disk — unless the
                // buffer has unsaved edits, where disk isn't what the user sees
                let r = if dirty {
                    deny(
                        "old_string isn't in the file as the user has it open (it has unsaved edits) — Read it again",
                    )
                } else {
                    allow(p.input.clone())
                };
                return answer(ed, &p.request_id, r);
            }
            if all { base.replace(&old, &new) } else { base.replacen(&old, &new, 1) }
        }
    };
    let changes = crate::agent::line_changes(&base, &proposed);
    if changes.is_empty() {
        return answer(ed, &p.request_id, allow(p.input.clone()));
    }
    // The header quotes what you asked for (the file is in the path bar above)
    let title = ed
        .session
        .chat
        .as_ref()
        .and_then(|c| c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::User))
        .map(|m| m.text.lines().next().unwrap_or_default().to_string())
        .unwrap_or_else(|| format!("an edit to {}", crate::chat::shown(&ed.session.root, &path)));
    let reason = p.reason.clone();
    // The keys go to the review; the file is the user's now (follow won't close it as a preview)
    if let Some(c) = &mut ed.session.chat {
        c.focused = false;
        c.follow.keep(doc_id);
    }
    crate::llm::review_with(
        ed,
        doc_id,
        &title,
        changes,
        Some(Box::new(move |ed: &mut Editor, applied: bool| {
            done(ed, p, doc_id, base, dirty, existed, applied)
        })),
    );
    if let Some(r) = &mut ed.session.review {
        r.reason = reason;
        // (its turn in the chat is the step)
        r.own_turn = false;
        let g = r.generation;
        if let Some(c) = &mut ed.session.chat {
            c.edits.review = Some(g);
        }
    }
    crate::follow::settle(ed);
}

/// The review ended: tell claude what to write (or not).
fn done(ed: &mut Editor, p: Pending, doc_id: DocId, base: String, dirty: bool, existed: bool, applied: bool) {
    if let Some(c) = &mut ed.session.chat {
        c.edits.review = None;
    }
    if applied {
        crate::checkpoint::record(ed, doc_id, existed.then(|| ropey::Rope::from_str(&base)));
    }
    let applied_note = "The user accepted this change and tarae applied it to their open buffer, which has \
unsaved edits — the file on disk changes when they save. Treat the edit as done.";
    let r = if !applied {
        deny("The user declined this change in the editor.")
    } else if dirty {
        deny(applied_note)
    } else {
        let now = ed.docs.iter().find(|d| d.id == doc_id).map(|d| d.text.to_string()).unwrap_or_default();
        let path = p.input["file_path"].clone();
        match p.tool.as_str() {
            "Write" => allow(json!({ "file_path": path, "content": now })),
            _ => match region(&base, &now) {
                Some((old, new)) => allow(
                    json!({ "file_path": path, "old_string": old, "new_string": new, "replace_all": false }),
                ),
                None => deny(applied_note),
            },
        }
    };
    if r["behavior"] == "allow"
        && let Some(doc) = ed.docs.iter_mut().find(|d| d.id == doc_id)
    {
        doc.agent_will_write();
        // (its replay stop: what you accepted, against what it was — once Claude has written it)
        let real = doc.path.clone();
        if let Some(real) = real {
            crate::follow::before_edit(ed, real, existed.then_some(base));
        }
    }
    answer(ed, &p.request_id, r);
    crate::follow::settle(ed);
}

/// `base` → `now` as one Edit: the changed span, grown by whole lines until it occurs once in `base`.
pub fn region(base: &str, now: &str) -> Option<(String, String)> {
    if base == now {
        return None;
    }
    // Common prefix and suffix, in bytes but ending on character boundaries in both (한 and 셋 share a byte)
    let mut pre = base.bytes().zip(now.bytes()).take_while(|(a, b)| a == b).count();
    while !(base.is_char_boundary(pre) && now.is_char_boundary(pre)) {
        pre -= 1;
    }
    let max_suf = base.len().min(now.len()) - pre;
    let mut suf = base.bytes().rev().zip(now.bytes().rev()).take_while(|(a, b)| a == b).count().min(max_suf);
    while !(base.is_char_boundary(base.len() - suf) && now.is_char_boundary(now.len() - suf)) {
        suf -= 1;
    }
    // Whole lines: start after the last newline before the change, end at the next newline after it
    let mut a = base[..pre].rfind('\n').map_or(0, |i| i + 1);
    let mut end = base.len() - suf;
    end = base[end..].find('\n').map_or(base.len(), |i| end + i);
    loop {
        let old = &base[a..end];
        if !old.is_empty() && base.matches(old).count() == 1 {
            let tail = base.len() - end;
            return Some((old.to_string(), now[a..now.len() - tail].to_string()));
        }
        if a == 0 && end == base.len() {
            return None;
        }
        a = base[..a.saturating_sub(1)].rfind('\n').map_or(0, |i| i + 1);
        end = if end < base.len() {
            base[end + 1..].find('\n').map_or(base.len(), |i| end + 1 + i)
        } else {
            end
        };
    }
}

fn answer(ed: &mut Editor, request_id: &str, response: Value) {
    crate::chat::respond(ed, request_id, response);
}

/// The chat turn ended (or claude exited): a review still waiting for it closes, queued ones are dropped,
/// and the files outside the project its reviews opened go (`Document::aside` — not ones with unsaved
/// edits of yours, nor ones you worked in since; an accepted edit Claude is still writing is its own).
pub fn turn_over(ed: &mut Editor) {
    let Some(c) = &mut ed.session.chat else { return };
    c.edits.queue.clear();
    if let Some(g) = c.edits.review.take()
        && ed.session.review.as_ref().is_some_and(|r| r.generation == g)
    {
        crate::llm::close_review(ed);
    }
    let aside: Vec<_> = ed
        .docs
        .iter()
        .filter(|d| d.aside && (!d.is_modified() || d.unsaved_is_claudes()))
        .map(|d| d.id)
        .collect();
    for id in aside {
        ed.close_doc(id);
    }
}

/// The code pane before a key, click or paste: its session, document, version and selection — None while what
/// you press goes to a review (its answer moves and edits the file, but isn't you going there).
pub struct Before(Option<(crate::event::SessionId, DocId, u64, crate::selection::Selection)>);

/// Before every key, click or paste (`taken_up` after it).
pub fn before_input(ed: &Editor) -> Before {
    let d = ed.doc();
    Before((ed.session.review.is_none()).then(|| (ed.session.id, d.id, d.version(), d.selection().clone())))
}

/// After every key, click or paste: the file in the code pane, if Claude's turn opened it aside, is yours
/// now if you went there — switched to it (the file you were in still open, in the same session), moved in
/// it or changed it. Not a key that went elsewhere (a review's answer, the chat's input, `space l`, a
/// picker that closed again: #46 — such files stayed, each keeping a language server), nor one that only
/// brought it to the front by closing yours (`:bc`, `C-x` in the buffer list) or switching sessions.
pub fn taken_up(ed: &mut Editor, before: Before) {
    let Before(Some((session, id, version, sel))) = before else { return };
    let d = ed.doc();
    if !d.aside || ed.session.review.is_some() {
        return;
    }
    let switched = d.id != id && ed.session.id == session && ed.docs.iter().any(|d| d.id == id);
    if switched || (d.id == id && (d.version() != version || *d.selection() != sel)) {
        ed.doc_mut().aside = false;
    }
}

/// Whether a file Claude's turn opens is aside — it goes when the turn ends unless you take it up: one
/// outside the project, or in its build output (`target/` — test fixtures, sandboxes; #46).
pub fn aside_path(root: &Path, path: &Path) -> bool {
    !path.starts_with(root) || path.starts_with(root.join("target"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review at the end lets in only edits that land inside the project — where the path really leads,
    /// links followed and `..` gone through — and not to `.git` or a listed name (anywhere) or path (from
    /// the root).
    #[test]
    fn which_edits_go_in_unasked() {
        let dir = std::env::temp_dir().join(format!("tarae-edits-inside-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("tarae-edits-outside-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("away")).ok();
        let at = |p: &str| inside(&root, &root.join(p));
        assert_eq!(at("src/main.rs"), Some(root.join("src/main.rs")));
        assert_eq!(
            at("src/new/deeper/file.rs"),
            Some(root.join("src/new/deeper/file.rs")),
            "folders to come"
        );
        assert_eq!(at("src/../src/lib.rs"), Some(root.join("src/lib.rs")));
        assert_eq!(at("../elsewhere.rs"), None);
        assert_eq!(at("away/x.rs"), None, "a link out of the project");
        assert_eq!(inside(&root, Path::new("/etc/hosts")), None);
        // (the root as given: /tmp → /private/tmp on macOS)
        assert_eq!(inside(&root, &dir.join("src/main.rs")), Some(root.join("src/main.rs")));
        let ask: Vec<String> =
            ["build.rs", ".github", "src/secret/", "Cargo.toml"].iter().map(|s| s.to_string()).collect();
        let listed = |p: &str| listed(Path::new(p), &ask);
        assert!(!listed("src/main.rs"));
        assert!(listed(".git/hooks/pre-commit"), ".git always asks");
        assert!(listed("crates/a/build.rs"), "a name, anywhere");
        assert!(listed(".github/workflows/ci.yml"));
        assert!(listed("src/secret/key.rs"), "a path, from the root");
        assert!(!listed("lib/src/secret/key.rs"));
        assert!(!listed("src/build.rs.bak"));
        assert!(listed("Cargo.toml") && listed("crates/a/Cargo.toml"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn region_is_one_unique_span_of_whole_lines() {
        let base = "fn a() {}\nlet x = 1;\nfn b() {}\n";
        let now = "fn a() {}\nlet x = 2;\nfn b() {}\n";
        assert_eq!(region(base, now), Some(("let x = 1;".into(), "let x = 2;".into())));
        // The changed line alone isn't unique → grows to its neighbours
        let base = "a\nx\nb\nx\nc\n";
        let now = "a\nx\nb\ny\nc\n";
        assert_eq!(region(base, now), Some(("b\nx\nc".into(), "b\ny\nc".into())));
        // Insertion at the end; nothing changed
        assert_eq!(region("a\n", "a\nb\n"), Some(("a\n".into(), "a\nb\n".into())));
        assert_eq!(region("same", "same"), None);
        // Applying the region swap to base gives now
        for (b, n) in [("x\ny\nz\n", "x\nY\nz\nw\n"), ("한글\n둘\n", "한글\n셋\n"), ("p\np\n", "p\nq\n")]
        {
            let (old, new) = region(b, n).unwrap();
            assert_eq!(b.replacen(&old, &new, 1), n, "{b:?} → {n:?}");
        }
    }
}
