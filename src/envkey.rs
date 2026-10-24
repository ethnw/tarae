//! The API key found by failing (sandbox UX piece B, #2, Q9): the chat's claude ended for want of a login while
//! tarae held a key of its environment back (`llm::login_hint`'s case) — in a subscription session, not inside a
//! Claude Code session (there the key is the host's), a card asks whether Claude takes it: `y` until this session
//! ends (`Session.env_key` — claude starts again with it, and the message that failed goes again) · `A` always
//! (`llm.environment-key = "keep"`, written to your own config, never a project's) · `n` not asked again this
//! session. An API session never gets it: its claude never takes a key from the environment (`billing.rs`).

use crate::billing::Billing;
use crate::chat::Role;
use crate::editor::Editor;
use crate::event::SessionId;
use crate::offer::{Offer, What};

pub const TITLE: &str = "Claude isn't logged in";

/// How the card was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Session,
    Always,
    No,
}

/// Whether the session in front's login failure is the card's — a key of `env` held back, and the session's to
/// ask (subscription, not answered yet): the card is up or comes now, and the chat only points at it.
pub fn offered(ed: &Editor, env: &crate::llm::ClaudeEnv) -> bool {
    ed.session.billing == Billing::Subscription
        && ed.session.env_key.is_none()
        && !env.nested
        && !env.keys().is_empty()
}

/// The session in front's chat failed for want of a login: the card, when it's `offered` and none is up.
pub fn ask(ed: &mut Editor) {
    let env = crate::llm::ClaudeEnv::of(ed.claude_env.as_ref(), crate::llm::keeps_key(ed));
    let keys = env.keys();
    if !offered(ed, &env) || ed.session_cards().any(|o| matches!(o.what, What::EnvKey { .. })) {
        return;
    }
    let message = ed.session.chat.as_ref().and_then(|c| c.msgs.iter().rfind(|m| m.role == Role::User));
    let message = message.map(|m| m.text.clone());
    let q = format!(
        "Use {} from tarae's environment for Claude? Its requests then bill that key's API account by the token, \
         not your plan.\n\nClaude starts again with it and your message goes again. A keeps it for every session: \
         llm.environment-key = \"keep\" in your own config.",
        keys.join(" and ")
    );
    let what = What::EnvKey { session: ed.session.id, message };
    ed.push_offer(Offer::new(TITLE.into(), &q, "", String::new(), what));
}

/// The answer on that card, for session `sid`: `message` is the one that failed, sent again on a yes.
pub fn answer(ed: &mut Editor, id: u64, (sid, message): (SessionId, Option<String>), how: Answer) {
    ed.offer_finished(id, Ok(()));
    if how == Answer::Always {
        ed.config.llm.keep_env_key = true;
        ed.overrides.retain(|(p, _)| p != "llm.environment-key");
        // (here, as `:set!` writes — your own config file, the one tarae read at start)
        let kept = ed
            .config_file
            .as_ref()
            .map(|f| crate::config::persist_to(f, "llm.environment-key", &"keep".into()));
        match kept {
            Some(Err(e)) => ed.set_error(format!("llm.environment-key isn't kept for the next start: {e}")),
            None => ed.set_warning("No config file of yours to keep llm.environment-key in — until you quit"),
            Some(Ok(())) => {}
        }
    }
    ed.with_session(sid, |ed| {
        ed.session.env_key = Some(how != Answer::No);
        match how {
            Answer::No => {
                ed.set_status("Claude stays on your Claude Code login — not asked again this session")
            }
            _ => {
                let said =
                    "Claude starts again with the key from tarae's environment, until this session ends";
                crate::chat::start_again(ed, message, said)
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A chat whose stand-in claude has no login unless it's given a key: each message it gets is a line in
    /// `log` — `key=set` or `key=`, and `hello` when it's that message — answered "Not logged in" without the key,
    /// `hi` with it. tarae's environment is a stand-in (`Editor::claude_env`) holding a fake key.
    fn chat(root: &std::path::Path, log: &std::path::Path) -> Editor {
        let mut ed = Editor::with_root(crate::config::Config::default(), root.to_path_buf());
        ed.claude_env =
            Some(crate::llm::Env::from([("ANTHROPIC_API_KEY".into(), "tarae-fake-key-zq7".into())]));
        let failed = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#;
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi"}"#;
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in *'\"type\":\"user\"'*) \
                 h=; case \"$line\" in *hello*) h=hello;; esac; \
                 printf 'key=%s %s\\n' \"${{ANTHROPIC_API_KEY:+set}}\" \"$h\" >> '{log}'; \
                 if [ -n \"$ANTHROPIC_API_KEY\" ]; then printf '%s\\n' '{ok}'; else printf '%s\\n' '{failed}'; fi;; \
                 esac; done",
                log = log.display()
            ),
        ];
        crate::chat::open(&mut ed);
        ed.session.chat.as_mut().unwrap().input = "hello".into();
        crate::chat::send(&mut ed);
        ed
    }

    fn settle_for(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + crate::test_results::PATIENCE;
        while !done(ed) {
            assert!(
                Instant::now() < end,
                "{:?}",
                ed.session.chat.as_ref().map(|c| c.msgs.iter().map(|m| &m.text).collect::<Vec<_>>())
            );
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    fn card_up(ed: &Editor) -> bool {
        ed.offers.iter().any(|o| matches!(o.what, What::EnvKey { .. }))
    }

    /// Answered: the message went again, to a claude that had the key — and one row of it in the chat.
    fn sent_again_with_the_key(ed: &mut Editor, log: &std::path::Path) {
        let answered = |ed: &Editor| ed.session.chat.as_ref().unwrap().msgs.iter().any(|m| m.text == "hi");
        settle_for(ed, answered);
        let lines = std::fs::read_to_string(log).unwrap();
        let lines: Vec<&str> = lines.lines().collect();
        // (the stand-in is the whole environment judged: the same where the tests run inside a Claude Code session)
        assert_eq!(
            lines,
            ["key= hello", "key=set hello"],
            "held back, then the next claude has it, the message again"
        );
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(c.msgs.iter().filter(|m| m.role == Role::User).count(), 1, "one row of it");
    }

    /// "Not logged in" with a key held back: the card — names only — and `y` starts claude again with the key
    /// for this session, the message sent again; nothing written.
    #[test]
    fn yes_on_the_key_card_starts_claude_again_with_the_key() {
        let scratch = crate::scratch::Scratch::new("envkey-yes");
        let root = std::fs::canonicalize(&scratch).unwrap();
        let log = root.join("claude.log");
        let mut ed = chat(&root, &log);
        settle_for(&mut ed, card_up);
        let o = ed.offers.iter().find(|o| matches!(o.what, What::EnvKey { .. })).unwrap();
        assert_eq!(o.title, TITLE);
        assert!(
            o.question.starts_with("Use ANTHROPIC_API_KEY from tarae's environment for Claude?"),
            "{}",
            o.question
        );
        assert!(
            o.question.contains("bill that key's API account by the token") && !o.question.contains("zq7")
        );
        assert_eq!(o.answers(false), vec![('y', "This session"), ('A', "Always"), ('n', "No")]);
        assert!(o.weighty() && o.keeps());
        // (the chat points at the card: the long form — the setting, how to log in — is the card's to say)
        let notes: Vec<&str> =
            ed.session.chat.as_ref().unwrap().msgs.iter().map(|m| m.text.as_str()).collect();
        assert!(
            notes.contains(&"A card asks whether Claude takes ANTHROPIC_API_KEY from tarae's environment."),
            "{notes:?}"
        );
        assert!(!notes.iter().any(|n| n.contains("no login of its own")), "{notes:?}");
        ed.offer_accept();
        sent_again_with_the_key(&mut ed, &log);
        assert_eq!(ed.session.env_key, Some(true));
        assert!(!ed.config.llm.keep_env_key, "this session only");
    }

    /// `A`: as `y`, and `llm.environment-key = "keep"` written to your own config (a stand-in file), what was
    /// there kept.
    #[test]
    fn always_on_the_key_card_writes_your_config() {
        let scratch = crate::scratch::Scratch::new("envkey-always");
        let root = std::fs::canonicalize(&scratch).unwrap();
        let log = root.join("claude.log");
        let config = root.join("config.toml");
        std::fs::write(&config, "# mine\n[llm]\nmodel = \"opus\"\n").unwrap();
        let mut ed = chat(&root, &log);
        ed.config_file = Some(config.clone());
        settle_for(&mut ed, card_up);
        let front = ed.offers.iter().position(|o| matches!(o.what, What::EnvKey { .. })).unwrap();
        let o = ed.offers.remove(front);
        ed.offers.insert(0, o);
        ed.offer_always();
        sent_again_with_the_key(&mut ed, &log);
        let written = std::fs::read_to_string(&config).unwrap();
        assert!(written.contains("environment-key = \"keep\""), "{written}");
        assert!(written.contains("# mine") && written.contains("model = \"opus\""), "{written}");
        assert!(ed.config.llm.keep_env_key);
    }

    /// Only where it's the session's to ask: an API session never takes a key from the environment (no card),
    /// nor does tarae inside a Claude Code session (the host's), nor a session that said `n` — nor one card twice.
    #[test]
    fn the_key_card_comes_only_to_a_subscription_session() {
        let mut ed = Editor::with_root(crate::config::Config::default(), std::env::temp_dir());
        let key = ("ANTHROPIC_API_KEY".to_string(), "tarae-fake-key".to_string());
        ed.claude_env = Some(crate::llm::Env::from([key.clone()]));
        ed.session.billing = Billing::Api;
        ask(&mut ed);
        assert!(!card_up(&ed), "an API session: never");
        ed.session.billing = Billing::Subscription;
        ed.claude_env =
            Some(crate::llm::Env::from([key.clone(), ("CLAUDE_CODE_SESSION_ID".into(), "x".into())]));
        ask(&mut ed);
        assert!(!card_up(&ed), "inside Claude Code: the host's");
        ed.claude_env = Some(crate::llm::Env::from([("PATH".to_string(), "/usr/bin".to_string())]));
        ask(&mut ed);
        assert!(!card_up(&ed), "no key held back");
        ed.claude_env = Some(crate::llm::Env::from([key]));
        ask(&mut ed);
        ask(&mut ed);
        assert_eq!(ed.offers.iter().filter(|o| matches!(o.what, What::EnvKey { .. })).count(), 1, "one card");
        ed.offer_dismiss();
        assert_eq!(ed.session.env_key, Some(false));
        ask(&mut ed);
        assert!(!card_up(&ed), "not asked again this session");
    }
}
