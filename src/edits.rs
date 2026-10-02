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

use serde_json::{Value, json};

use crate::document::DocId;
use crate::editor::Editor;

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
}

/// A `can_use_tool` request: answered now (Some) or after its review (None).
pub fn request(ed: &mut Editor, request_id: &str, req: &Value) -> Option<Value> {
    let tool = req["tool_name"].as_str().unwrap_or_default();
    if !matches!(tool, "Edit" | "Write") {
        return Some(deny(&format!("{tool} isn't available in tarae's chat")));
    }
    let c = ed.chat.as_mut()?;
    // Its words right before this call, this turn — taken now, so each queued edit keeps its own
    // Why: its words right before this call (after the call's own row), not ones from before earlier calls —
    // else the plan's step in hand
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
    let reason = words.filter(|w| !w.is_empty()).or(step).unwrap_or_default();
    c.edits.queue.push_back(Pending {
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input: req["input"].clone(),
        reason,
    });
    next(ed);
    None
}

fn allow(input: Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": input })
}

fn deny(message: &str) -> Value {
    json!({ "behavior": "deny", "message": message })
}

/// Every event: shows the next waiting edit once no review is up.
pub fn next(ed: &mut Editor) {
    if ed.review.is_some() {
        return;
    }
    let Some(p) = ed.chat.as_mut().and_then(|c| c.edits.queue.pop_front()) else { return };
    show(ed, p);
}

fn show(ed: &mut Editor, p: Pending) {
    let s = |k: &str| p.input[k].as_str().unwrap_or_default().to_string();
    let path = crate::chat::resolve(&s("file_path"));
    // (a file Claude creates goes again when its turn is undone)
    let existed = ed.docs.iter().any(|d| d.path.as_deref() == Some(path.as_path())) || path.exists();
    if let Err(e) = ed.open(&path) {
        return answer(ed, &p.request_id, deny(&format!("tarae couldn't open {}: {e:#}", path.display())));
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
        .chat
        .as_ref()
        .and_then(|c| c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::User))
        .map(|m| m.text.lines().next().unwrap_or_default().to_string())
        .unwrap_or_else(|| format!("an edit to {}", crate::chat::shown(&path)));
    let reason = p.reason.clone();
    // The keys go to the review; the file is the user's now (follow won't close it as a preview)
    if let Some(c) = &mut ed.chat {
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
    if let Some(r) = &mut ed.review {
        r.reason = reason;
        // (its turn in the chat is the step)
        r.own_turn = false;
        let g = r.generation;
        if let Some(c) = &mut ed.chat {
            c.edits.review = Some(g);
        }
    }
    crate::follow::settle(ed);
}

/// The review ended: tell claude what to write (or not).
fn done(ed: &mut Editor, p: Pending, doc_id: DocId, base: String, dirty: bool, existed: bool, applied: bool) {
    if let Some(c) = &mut ed.chat {
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

/// The chat turn ended (or claude exited): a review still waiting for it closes, queued ones are dropped.
pub fn turn_over(ed: &mut Editor) {
    let Some(c) = &mut ed.chat else { return };
    c.edits.queue.clear();
    if let Some(g) = c.edits.review.take()
        && ed.review.as_ref().is_some_and(|r| r.generation == g)
    {
        crate::llm::close_review(ed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
