//! Claude compacting its conversation — near the context limit by itself, or asked (`/compact`): claude writes
//! a `system` `compact_boundary` line (measured 2026-10-05, Claude Code 2.1.288):
//!
//! ```json
//! {"type":"system","subtype":"compact_boundary","session_id":"…","uuid":"…",
//!  "compact_metadata":{"trigger":"manual","pre_tokens":6577,"post_tokens":795,
//!                      "cumulative_dropped_tokens":5782,"duration_ms":12261},"logical_parent_uuid":"…"}
//! ```
//!
//! `trigger` is `auto` or `manual`; `pre_tokens` the context before, `post_tokens` after (the summary and
//! what's kept). Before it come `status: "compacting"`, `status: null` with `compact_result: "success"` once
//! it's over, and a second `init` of the same conversation; after it, the summary as a user message. A
//! `/compact` turn's own `result` reports 0 tokens and the summary's cost.
//!
//! While it compacts the chat says `compacting…` (it took 12 s for a tiny conversation). What Claude worked
//! out before may be only in the summary now, so it's said: a note in the chat where it happened, the
//! session's count in `tarae state`, a toast when its chat isn't in sight (a session behind — named, its row
//! in the list marked until it's in front — or the front's zoomed away), and the context figure follows the
//! drop (`Usage::compacted`).

use serde_json::{Value, json};

use crate::editor::Editor;

/// One compaction, as its `compact_boundary` line says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compaction {
    /// `auto` · `manual` — another word kept as claude says it.
    pub trigger: String,
    /// The context before · after (None: not said).
    pub pre_tokens: Option<u64>,
    pub post_tokens: Option<u64>,
    /// When it was read (unix ms).
    pub at_ms: u64,
}

impl Compaction {
    /// From a stream line, read at `at_ms` — None: not a `compact_boundary`.
    pub fn of(ev: &Value, at_ms: u64) -> Option<Compaction> {
        if ev["type"] != "system" || ev["subtype"] != "compact_boundary" {
            return None;
        }
        let m = &ev["compact_metadata"];
        Some(Compaction {
            trigger: m["trigger"].as_str().unwrap_or_default().to_string(),
            pre_tokens: m["pre_tokens"].as_u64(),
            post_tokens: m["post_tokens"].as_u64(),
            at_ms,
        })
    }

    pub fn automatic(&self) -> bool {
        self.trigger == "auto"
    }

    /// `automatic, 812k tokens before` · `manual` (no count said).
    pub fn how(&self) -> String {
        let trigger = match self.trigger.as_str() {
            "auto" => "automatic",
            "" => "trigger not reported",
            t => t,
        };
        match self.pre_tokens {
            Some(n) => format!("{trigger}, {} tokens before", crate::usage::tokens(n)),
            None => trigger.to_string(),
        }
    }

    /// The chat's note where it happened.
    pub fn note(&self) -> String {
        format!("{NOTE} ({}) — what it worked out before may be only in its summary now", self.how())
    }
}

const NOTE: &str = "Claude compacted this conversation";

/// A chat note that's a compaction's — told by its words, so a conversation kept and continued draws it the
/// same.
pub fn is_note(text: &str) -> bool {
    text.starts_with(NOTE)
}

/// What a stream line says of compacting: it began (`status: "compacting"`), it's over (`status: null` with
/// `compact_result`), and the compaction itself (`compact_boundary`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    Started,
    Ended,
    Boundary(Compaction),
}

impl Ev {
    /// From a stream line, read at `at_ms` — None: nothing of compacting.
    pub fn of(ev: &Value, at_ms: u64) -> Option<Ev> {
        if ev["type"] != "system" {
            return None;
        }
        match ev["subtype"].as_str()? {
            "status" if ev["status"] == "compacting" => Some(Ev::Started),
            "status" if ev.get("compact_result").is_some() => Some(Ev::Ended),
            "compact_boundary" => Compaction::of(ev, at_ms).map(Ev::Boundary),
            _ => None,
        }
    }
}

/// A session's compactions since tarae started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compacted {
    pub count: usize,
    pub last: Option<Compaction>,
    /// One came while the session was behind, and it hasn't been in front since — its row in the list says so.
    pub unseen: bool,
    /// Its Claude said it's compacting, not yet that it's over — since when.
    pub since: Option<std::time::Instant>,
}

/// Session `l`'s Claude is compacting right now: said so where "thinking" would be. (One said in an earlier
/// turn and never ended — the process gone mid-way — isn't this turn's.)
pub fn under_way(l: &crate::session::Session) -> bool {
    let Some(since) = l.compacted.since else { return false };
    l.chat.as_ref().is_some_and(|c| c.busy() && since >= c.started)
}

/// A stream line of compacting (`Ev`), in session `ed.session`.
pub fn on_event(ed: &mut Editor, e: Ev) {
    let was = under_way(&ed.session);
    match e {
        Ev::Started => ed.session.compacted.since = Some(std::time::Instant::now()),
        Ev::Ended => ed.session.compacted.since = None,
        Ev::Boundary(k) => {
            ed.session.compacted.since = None;
            return on_compacted(ed, k);
        }
    }
    // (behind, nothing of it is on screen; a line saying again what's so changes nothing)
    if ed.background() || was == under_way(&ed.session) {
        ed.unchanged = true;
    }
}

impl Compacted {
    /// As `tarae state` says it.
    pub fn json(&self) -> Value {
        let last = self.last.as_ref().map(|k| {
            json!({ "at_ms": k.at_ms, "trigger": k.trigger, "pre_tokens": k.pre_tokens,
                    "post_tokens": k.post_tokens })
        });
        json!({ "count": self.count, "last": last, "unseen": self.unseen })
    }

    /// The text form's: `2× · last 14:32 automatic, 812k tokens before`.
    pub fn text(&self) -> String {
        match &self.last {
            Some(k) => format!("{}× · last {} {}", self.count, crate::usage::until(k.at_ms), k.how()),
            None => format!("{}×", self.count),
        }
    }
}

/// Session `ed.session`'s Claude compacted its conversation: a note in its chat where it happened, counted,
/// the context figure follows; behind, said (named) and its row in the list says so until it's in front.
pub fn on_compacted(ed: &mut Editor, k: Compaction) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Note, k.note(), ""));
        // (the sandbox's rules and guarantees a run told may be gone from what it kept: the next run says them)
        c.rules_said = (None, Default::default());
    }
    ed.session.usage.compacted(&k);
    let how = k.how();
    let automatic = k.automatic();
    let behind = ed.background();
    let s = &mut ed.session.compacted;
    s.count += 1;
    s.last = Some(k);
    if behind {
        s.unseen = true;
    }
    // Behind (`said_by_session` puts its name first), or in front with the chat out of sight past the turn —
    // as a turn's end is said (`chat::out_of_sight`: not when following minimized it, the panel's back with
    // the answer and the note in it). Came by itself, what it worked out may be gone: a warning
    let msg = match (behind, crate::chat::out_of_sight(ed)) {
        (true, _) => Some(format!("Claude compacted its conversation ({how})")),
        (false, true) => Some(format!("Claude compacted its conversation ({how}) — space l shows it")),
        (false, false) => None,
    };
    match (msg, automatic) {
        (Some(msg), true) => ed.set_warning(msg),
        (Some(msg), false) => ed.set_status(msg),
        (None, _) => {}
    }
    crate::uistate::watch::usage(ed, "compacted");
    crate::usage::view_sync(ed);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::*;
    use crate::chat::Role;
    use crate::config::Config;
    use crate::key::{Code, Key};

    /// The line the installed claude wrote for `/compact` (2.1.288 — `target/sb/compact.md`), whole.
    const BOUNDARY: &str = r#"{"type":"system","subtype":"compact_boundary","session_id":"49a87b48-0524-43f7-b9cd-d4cfd845af0f","uuid":"13f7e73c-ed56-428c-807b-814e5afcb60c","compact_metadata":{"trigger":"manual","pre_tokens":6577,"post_tokens":795,"cumulative_dropped_tokens":5782,"duration_ms":12261},"logical_parent_uuid":"24536454-416b-41ff-9e95-26e1174c4ace"}"#;

    /// The same line as claude writes it near the context limit: `auto`, 812k before, 30k kept.
    fn automatic() -> Value {
        let line =
            BOUNDARY.replace("\"manual\"", "\"auto\"").replace("6577", "812000").replace("795", "30000");
        serde_json::from_str(&line).unwrap()
    }

    fn manual() -> Value {
        serde_json::from_str(BOUNDARY).unwrap()
    }

    const MODEL: &str = "claude-haiku-4-5-20251001";

    /// A request going out with `context` tokens in it (read from the cache but 10).
    fn request(id: &str, context: u64) -> Value {
        json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": id, "model": MODEL,
            "usage": {"input_tokens": 10, "cache_read_input_tokens": context - 10, "output_tokens": 1}}}})
    }

    fn text(t: &str) -> Value {
        json!({"type": "stream_event", "event": {"type": "content_block_delta",
            "delta": {"type": "text_delta", "text": t}}})
    }

    /// A turn's result: its usage (None: claude's `/compact` turn's zeros), the conversation's cost so far.
    fn result(usage: Option<Value>, total: f64) -> Value {
        let zeros = json!({"input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            "output_tokens": 0});
        json!({"type": "result", "result": "", "session_id": "c1", "total_cost_usd": total,
            "usage": usage.unwrap_or(zeros), "modelUsage": {MODEL: {"contextWindow": 1_000_000}}})
    }

    /// A fake claude that answers its `n`th message with the lines of `turns[n]` (a string: a command of its
    /// own, `sleep 1`).
    fn claude_answers(ed: &mut Editor, turns: &[Vec<Value>]) {
        let mut cases = String::new();
        for (i, lines) in turns.iter().enumerate() {
            let body: Vec<String> = lines
                .iter()
                .map(|l| match l.as_str() {
                    Some(cmd) => cmd.to_string(),
                    None => format!("printf '%s\\n' '{l}'"),
                })
                .collect();
            cases.push_str(&format!("{}) {};; ", i + 1, body.join("; ")));
        }
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "n=0; while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
                 n=$((n+1)); case $n in {cases}esac; done"
            ),
        ];
    }

    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key = match c {
                '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
                c => Key::plain(Code::Char(c)),
            };
            ed.handle_key(key);
        }
    }

    /// Events handled until session `id`'s Claude is idle again.
    fn turn_ends(ed: &mut Editor, id: crate::event::SessionId) {
        let end = Instant::now() + Duration::from_secs(10);
        let busy = |ed: &Editor| {
            std::iter::once(&ed.session)
                .chain(&ed.sessions)
                .find(|l| l.id == id)
                .and_then(|l| l.chat.as_ref())
                .is_some_and(|c| c.busy())
        };
        while busy(ed) {
            assert!(Instant::now() < end, "its turn ends");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    /// Events handled one at a time until `done` holds after one.
    fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(ed) {
            assert!(Instant::now() < end, "{what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    /// The measured lines around a compaction: `status: "compacting"` · `status: null` with `compact_result`.
    fn started() -> Value {
        json!({"type": "system", "subtype": "status", "status": "compacting", "session_id": "c1",
               "uuid": "e0bb7715-cbb6-4575-b503-cc518f9d96f1"})
    }

    fn ended() -> Value {
        json!({"type": "system", "subtype": "status", "status": null, "compact_result": "success",
               "session_id": "c1", "uuid": "cb55325e-2353-4ca8-a6f5-0ea977eef5ba"})
    }

    /// An editor in a folder of its own (`name`, made empty).
    fn editor(name: &str) -> (Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-compaction-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("front")).unwrap();
        std::fs::create_dir_all(dir.join("back")).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.join("front");
        ed.session.name = "front".into();
        (ed, dir)
    }

    fn rows(ed: &mut Editor, w: u16) -> Vec<String> {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, w, 40).unwrap();
        let mut vt = vt100::Parser::new(40, w, 0);
        vt.process(&bytes);
        vt.screen().rows(0, w).collect()
    }

    fn cleanup(dir: &Path) {
        std::fs::remove_dir_all(dir).ok();
    }

    /// The measured line reads as what it says; the lines around it (`status: compacting`, the second
    /// `init`) aren't one.
    #[test]
    fn the_measured_line_reads() {
        let k = Compaction::of(&manual(), 7).expect("a compaction");
        assert_eq!(
            k,
            Compaction { trigger: "manual".into(), pre_tokens: Some(6577), post_tokens: Some(795), at_ms: 7 }
        );
        assert!(!k.automatic() && Compaction::of(&automatic(), 0).unwrap().automatic());
        assert_eq!(k.how(), "manual, 6.6k tokens before");
        let status =
            json!({"type": "system", "subtype": "status", "status": "compacting", "session_id": "c"});
        let init = json!({"type": "system", "subtype": "init", "session_id": "c", "model": MODEL});
        assert_eq!((Compaction::of(&status, 0), Compaction::of(&init, 0)), (None, None));
        // (no counts said: none guessed)
        let bare =
            json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"trigger": "auto"}});
        assert_eq!(Compaction::of(&bare, 0).unwrap().how(), "automatic");
    }

    /// Claude compacts mid-turn near the limit, then is asked to (`/compact`): each is a note in the chat
    /// where it happened — between the words before and after it — saying how and how big the context was;
    /// the context figure follows the drop (to what the compaction kept, then the next request's), and
    /// `/compact`'s turn says `compacted` where its counts would be 0s.
    #[test]
    fn a_compaction_is_said_in_the_chat_where_it_happened() {
        let (mut ed, dir) = editor("note");
        let init = json!({"type": "system", "subtype": "init", "session_id": "c1", "model": MODEL});
        let used = json!({"input_tokens": 20, "cache_read_input_tokens": 842_000, "output_tokens": 9});
        claude_answers(
            &mut ed,
            &[
                vec![
                    init,
                    request("m1", 812_000),
                    text("before"),
                    automatic(),
                    request("m2", 31_000),
                    text("after"),
                    result(Some(used), 1.0),
                ],
                vec![manual(), result(None, 1.2)],
            ],
        );
        feed(&mut ed, "<space>lgo<ret>");
        turn_ends(&mut ed, 0);
        let said: Vec<(Role, String)> = {
            let c = ed.session.chat.as_ref().unwrap();
            c.msgs.iter().map(|m| (m.role, m.text.clone())).collect()
        };
        let auto = "Claude compacted this conversation (automatic, 812k tokens before) — what it worked out \
                    before may be only in its summary now";
        assert_eq!(
            said[..4].iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
            ["go", "before", auto, "after"]
        );
        assert_eq!(said[2].0, Role::Note);
        // Drawn as a beat of its own: a row of air above it, marked `≈` as the session list marks it
        let shown = rows(&mut ed, 160);
        let at = shown.iter().position(|r| r.contains("≈ Claude compacted")).expect("drawn");
        assert!(shown[at - 2].contains("before") && !shown[at - 1].contains("before"), "{shown:#?}");
        // (the request after it says what the context holds)
        assert_eq!(ed.session.usage.context(), Some((31_000, Some(1_000_000))));
        assert_eq!(ed.session.usage.turns[0].compacted.as_deref(), Some("auto"));
        // Asked: said so, and the context is what it kept until a request says
        feed(&mut ed, "/compact<ret>");
        turn_ends(&mut ed, 0);
        let c = ed.session.chat.as_ref().unwrap();
        let manual = "Claude compacted this conversation (manual, 6.6k tokens before) — what it worked out \
                      before may be only in its summary now";
        let at = c.msgs.iter().rposition(|m| m.text == "/compact").expect("asked");
        assert_eq!(c.msgs.get(at + 1).map(|m| (m.role, m.text.as_str())), Some((Role::Note, manual)));
        assert_eq!(ed.session.usage.context(), Some((795, Some(1_000_000))));
        let wide = rows(&mut ed, 240);
        let footer = wide.iter().find(|r| r.contains("ctx ")).unwrap_or_else(|| panic!("{wide:#?}"));
        assert!(footer.contains("ctx 0% of 1M") && footer.contains("compacted"), "{footer:?}");
        assert!(!footer.contains("read 0"), "{footer:?}");
        cleanup(&dir);
    }

    /// While claude compacts (`status: "compacting"` until `compact_result` or the compaction itself), the
    /// chat says `compacting…` where it says `thinking…`, and its title `compacting`; following, the card
    /// beside the code says it too. Begun in an earlier turn and never ended (the process gone mid-way), it
    /// isn't the next turn's.
    #[test]
    fn while_claude_compacts_the_chat_says_so() {
        let (mut ed, dir) = editor("compacting");
        let sleep = || Value::String("sleep 1".into());
        claude_answers(
            &mut ed,
            &[
                vec![started(), sleep(), ended(), sleep(), automatic(), result(None, 0.1)],
                vec![started(), sleep(), result(None, 0.2)],
                vec![sleep(), result(None, 0.3)],
            ],
        );
        feed(&mut ed, "<space>l<esc>");
        ed.session.chat.as_mut().unwrap().follow.on = false;
        let said = |rows: &[String], w: &str| rows.iter().any(|r| r.contains(w));
        // Begun: said in the chat, title and body
        feed(&mut ed, "<space>lone<ret>");
        until(&mut ed, "it begins", |ed| under_way(&ed.session));
        let shown = rows(&mut ed, 160);
        assert!(said(&shown, "compacting…") && said(&shown, " compacting "), "{shown:#?}");
        assert!(!said(&shown, "thinking"), "{shown:#?}");
        // Over (`compact_result`): thinking again
        until(&mut ed, "it ends", |ed| ed.session.compacted.since.is_none());
        let shown = rows(&mut ed, 160);
        assert!(!said(&shown, "compacting") && said(&shown, "thinking…"), "{shown:#?}");
        turn_ends(&mut ed, 0);
        // Following: the card beside the code says it (code: the start screen has no card)
        let notes = dir.join("front/notes.txt");
        std::fs::write(&notes, "one\ntwo\n").unwrap();
        ed.open(&notes).unwrap();
        ed.session.chat.as_mut().unwrap().follow.on = true;
        feed(&mut ed, "two<ret>");
        until(&mut ed, "it begins", |ed| under_way(&ed.session));
        let shown = rows(&mut ed, 160);
        assert!(said(&shown, "compacting"), "{shown:#?}");
        turn_ends(&mut ed, 0);
        assert!(ed.session.compacted.since.is_some(), "never said to end");
        // The next turn isn't compacting
        feed(&mut ed, "three<ret>");
        assert!(ed.session.chat.as_ref().unwrap().busy() && !under_way(&ed.session));
        let shown = rows(&mut ed, 160);
        assert!(!said(&shown, "compacting"), "{shown:#?}");
        turn_ends(&mut ed, 0);
        cleanup(&dir);
    }

    /// The front session compacting with its chat out of sight — the code zoomed (`C-w z`): a toast says
    /// so, and where to see it. With the chat in sight its note is enough, and so when following only
    /// minimized it while Claude worked (the panel's back with the answer, the note in it).
    #[test]
    fn the_front_out_of_sight_is_told_of_a_compaction() {
        use crate::editor::ToastKind;
        let (mut ed, dir) = editor("front");
        let turn = vec![automatic(), result(None, 0.1)];
        claude_answers(&mut ed, &[turn.clone(), turn.clone(), turn]);
        let told = |ed: &Editor| -> Vec<(String, ToastKind)> {
            ed.toasts
                .iter()
                .filter(|t| t.text.starts_with("Claude compacted"))
                .map(|t| (t.text.clone(), t.kind))
                .collect()
        };
        feed(&mut ed, "<space>l<esc>");
        ed.session.chat.as_mut().unwrap().follow.on = false;
        // In sight
        feed(&mut ed, "<space>lone<ret>");
        rows(&mut ed, 160);
        assert!(ed.screen.chat_x.is_some());
        turn_ends(&mut ed, 0);
        assert_eq!(told(&ed), [], "{:?}", ed.toasts);
        // Minimized by following while Claude works (code in sight, so the card is drawn)
        let notes = dir.join("front/notes.txt");
        std::fs::write(&notes, "one\n").unwrap();
        ed.open(&notes).unwrap();
        ed.session.chat.as_mut().unwrap().follow.on = true;
        feed(&mut ed, "<space>lthree<ret>");
        rows(&mut ed, 160);
        assert!(ed.screen.chat_x.is_none(), "minimized while it works");
        turn_ends(&mut ed, 0);
        assert_eq!(told(&ed), [], "{:?}", ed.toasts);
        ed.session.chat.as_mut().unwrap().follow.on = false;
        // The code zoomed: said
        feed(&mut ed, "<space>ltwo<ret><esc>");
        rows(&mut ed, 160);
        feed(&mut ed, "<C-w>z");
        rows(&mut ed, 160);
        assert!(ed.zoomed().is_some() && ed.screen.chat_x.is_none());
        turn_ends(&mut ed, 0);
        let msg = "Claude compacted its conversation (automatic, 812k tokens before) — space l shows it";
        assert_eq!(told(&ed), [(msg.to_string(), ToastKind::Warning)], "{:?}", ed.toasts);
        cleanup(&dir);
    }

    /// `tarae state` has each session's compactions: how many, the last one's time, trigger and counts —
    /// the JSON and the text form; Claude's level has its own session's, never another's.
    #[test]
    fn the_state_has_each_sessions_compactions() {
        let (mut ed, dir) = editor("state");
        claude_answers(&mut ed, &[vec![automatic(), result(None, 0.1)], vec![manual(), result(None, 0.2)]]);
        assert_eq!(ed.ui_state().json()["sessions"][0].get("compacted"), None, "never: not said");
        feed(&mut ed, "<space>lone<ret>");
        turn_ends(&mut ed, 0);
        feed(&mut ed, "two<ret>");
        turn_ends(&mut ed, 0);
        let state = ed.ui_state();
        let k = &state.json()["sessions"][0]["compacted"];
        assert_eq!(k["count"], 2, "{k}");
        assert_eq!(k["last"]["trigger"], "manual", "{k}");
        assert_eq!(
            (k["last"]["pre_tokens"].as_u64(), k["last"]["post_tokens"].as_u64()),
            (Some(6577), Some(795))
        );
        let at = k["last"]["at_ms"].as_u64().expect("when");
        assert!(at.abs_diff(crate::usage::now_ms()) < 60_000, "{at}");
        assert_eq!(k["unseen"], false, "in front: seen");
        let text = state.text();
        let clock = crate::usage::until(at);
        assert!(
            text.contains(&format!("compacted s0: 2× · last {clock} manual, 6.6k tokens before")),
            "{text}"
        );
        // Claude's own session's; another's isn't its business
        assert_eq!(ed.ui_state_claude().sessions[0].compacted.as_ref().map(|c| c.count), Some(2));
        let back = ed.session_open(&dir.join("back")).unwrap();
        assert!(ed.ui_state_claude().sessions.iter().all(|s| s.compacted.is_none()), "s0 is another's now");
        assert_eq!(ed.session.id, back);
        cleanup(&dir);
    }

    /// A session behind whose Claude compacts: a toast names it (a warning when it came by itself — what it
    /// worked out may be gone), once; its row in the list says `≈ compacted` until it's been in front.
    #[test]
    fn a_session_behind_that_compacts_is_said_and_listed_until_seen() {
        use crate::editor::ToastKind;
        let (mut ed, dir) = editor("behind");
        claude_answers(&mut ed, &[vec![text("working"), automatic(), result(None, 0.1)]]);
        let back = ed.session_open(&dir.join("back")).unwrap();
        feed(&mut ed, "<space>lgo<ret><esc>");
        assert!(ed.session_switch(0));
        turn_ends(&mut ed, back);
        let msg = "back · Claude compacted its conversation (automatic, 812k tokens before)";
        let toasts: Vec<_> = ed.toasts.iter().filter(|t| t.text == msg).collect();
        assert_eq!(toasts.len(), 1, "{:?}", ed.toasts);
        assert_eq!((toasts[0].kind, toasts[0].of), (ToastKind::Warning, Some(back)));
        let listed = |ed: &mut Editor| {
            ed.session_list();
            let p = ed.picker.as_ref().expect("the list");
            let row = p.items().iter().find(|i| i.label == "back").map(|i| i.hint.clone()).unwrap();
            feed(ed, "<esc>");
            row
        };
        let row = listed(&mut ed);
        assert!(row.contains("≈ compacted"), "{row:?}");
        assert_eq!(ed.ui_state().json()["sessions"][1]["compacted"]["unseen"], true);
        // Seen once it's in front
        assert!(ed.session_switch(back) && ed.session_switch(0));
        let row = listed(&mut ed);
        assert!(!row.contains("compacted"), "{row:?}");
        assert_eq!(ed.ui_state().json()["sessions"][1]["compacted"]["count"], 1, "still counted");
        cleanup(&dir);
    }
}
