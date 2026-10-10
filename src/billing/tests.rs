//! Billing's tests. No real claude, Keychain or key: a stand-in claude in `sh` (`stand_in`) that does what Claude
//! Code does with an `apiKeyHelper` — runs it, and says `apiKeyHelper` as its credential — and a stand-in helper,
//! a script in the test's own folder printing `tarae-fake-key-<nonce>`. The stand-in writes what it got (its
//! `--settings`, its `--resume`, its environment's names, the key) in the project's folder, the test's own.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::editor::Editor;

/// A stand-in claude: each start appends `<resume or -> <its --settings>` to `starts`, writes its environment's
/// names to `env.names`; it runs the `apiKeyHelper` its `--settings` names, as Claude Code does, the key to
/// `got.key`, and says `apiKeyHelper` as its credential then (else `none`) — or what `init.source` (its answer to
/// `initialize`) and `turn.source` (each turn's `init`) say. Every line it's sent goes to `stdin`; a message is
/// answered with a turn's `init` and its result — unless `turn.hold` is there (nothing), or `turn.wait` (the
/// `init`, then the result only once interrupted, as Claude Code ends a turn it's told to stop).
const STAND_IN: &str = r#"s=''; r=-
while [ $# -gt 0 ]; do case "$1" in --settings) s="$2";; --resume) r="$2";; esac; shift; done
printf '%s %s\n' "$r" "$s" >> starts
awk 'BEGIN { for (k in ENVIRON) print k }' > env.tmp && mv env.tmp env.names
h=$(printf '%s' "$s" | sed -n 's/.*"apiKeyHelper":"\([^"]*\)".*/\1/p')
src=none
if [ -n "$h" ]; then sh -c "$h" > got.key && src=apiKeyHelper; fi
isrc=$src; tsrc=$src
[ -f init.source ] && isrc=$(cat init.source)
[ -f turn.source ] && tsrc=$(cat turn.source)
id=conv-1; [ "$r" != - ] && id=$r
while read -r line; do
  printf '%s\n' "$line" >> stdin
  case "$line" in
    *tarae-init*) printf '{"type":"control_response","response":{"subtype":"success","request_id":"tarae-init","response":{"models":[],"account":{"apiKeySource":"%s"}}}}\n' "$isrc";;
    *tarae-stop*) printf '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"","session_id":"%s"}\n' "$id";;
    *'"type":"user"'*) [ -f turn.hold ] || {
      printf '{"type":"system","subtype":"init","session_id":"%s","apiKeySource":"%s","model":"m"}\n' "$id" "$tsrc"
      [ -f turn.wait ] || printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"%s"}\n' "$id"
    };;
  esac
done"#;

/// The test's own folder (real path): `project/` (a git repository, the session's checkout), `state/` (where
/// tarae keeps things), the stand-in helper `helper.sh` beside them — and the key it prints.
struct Place {
    dir: PathBuf,
    root: PathBuf,
    state: PathBuf,
    helper: String,
    key: String,
}

impl Place {
    fn new(name: &str) -> Place {
        let dir = std::env::temp_dir().join(format!("tarae-billing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let root = dir.join("project");
        let git = std::process::Command::new("git").args(["init", "-q"]).current_dir(&root).output().unwrap();
        assert!(git.status.success());
        // (a nonce per run: what's found later is this run's, and nothing of tarae's own holds it)
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let key = format!("tarae-fake-key-{:x}{:x}", std::process::id(), nanos);
        std::fs::write(dir.join("helper.sh"), format!("printf {key}\n")).unwrap();
        let helper = format!("sh {}", dir.join("helper.sh").display());
        Place { state: dir.join("state"), root, dir, helper, key }
    }

    /// An editor in the project, its claude the stand-in, its key helper the stand-in's, keeping its chats and
    /// sessions in `state/`.
    fn editor(&self) -> Editor {
        let mut ed = Editor::with_root(crate::config::Config::default(), self.root.clone());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), STAND_IN.into(), "stand-in".into()];
        ed.api_key_helper = Some(self.helper.clone());
        ed.chats_dir = Some(self.state.join("chats"));
        ed.session_store.dir = Some(self.state.join("sessions"));
        ed.session_store.restored = true;
        ed
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    /// Each start of the stand-in: its `--resume` (None: none) and its `--settings` (Null: none).
    fn starts(&self) -> Vec<(Option<String>, Value)> {
        let parse = |l: &str| {
            let (r, s) = l.split_once(' ').unwrap();
            let resume = (r != "-").then(|| r.to_string());
            (resume, serde_json::from_str(s).unwrap_or(Value::Null))
        };
        self.read("starts").lines().map(parse).collect()
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done(ed) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

fn send(ed: &mut Editor, text: &str) {
    ed.session.chat.as_mut().unwrap().input = text.into();
    crate::chat::send(ed);
}

fn notes(ed: &Editor) -> Vec<String> {
    let c = ed.session.chat.as_ref().unwrap();
    c.msgs.iter().filter(|m| m.role == crate::chat::Role::Note).map(|m| m.text.clone()).collect()
}

fn idle(ed: &Editor) -> bool {
    ed.session.chat.as_ref().is_some_and(|c| !c.busy())
}

/// Every file under `dir`, read whole.
fn files_in(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_in(&p));
        } else if let Ok(b) = std::fs::read(&p) {
            out.push((p, b));
        }
    }
    out
}

/// An API session's claude gets the key only through its helper: tarae's helper in the chat's own `--settings`
/// (its `ask`/`deny` rules and hooks off kept beside it), the hour-long cache, no key from tarae's environment
/// even with `llm.environment-key = "keep"` — and the key reaches claude while nothing tarae keeps or says holds
/// it: its state folder (the chat, the session list), its log (the card log too), `tarae state`, its messages, the
/// chat's rows. The session says `api` in `tarae state`, and is kept as one.
#[test]
fn an_api_sessions_key_goes_through_its_helper_and_never_through_tarae() {
    let place = Place::new("api");
    let mut ed = place.editor();
    let log = crate::log::capture();
    // (a key of tarae's environment, passed on by "keep" — but not to an API session's claude)
    ed.config.llm.keep_env_key = true;
    ed.claude_env = Some(crate::llm::Env::from([("ANTHROPIC_API_KEY".into(), "tarae-fake-key-env".into())]));
    ed.session_billing("api").unwrap();
    crate::chat::open(&mut ed);
    send(&mut ed, "hello");
    until(&mut ed, "the turn's end", |ed| idle(ed) && ed.session.usage.key_source.is_some());
    assert_eq!(place.read("got.key"), place.key, "the helper's key reached claude");
    let starts = place.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    let settings = &starts[0].1;
    assert_eq!(settings["apiKeyHelper"], json!(place.helper), "tarae's helper: {settings}");
    assert_eq!(settings["promptCacheTtl"], "1h", "the chat's cache lives an hour");
    let chat: Value = serde_json::from_str(crate::chat::CHAT_SETTINGS).unwrap();
    for k in ["permissions", "disableAllHooks"] {
        assert_eq!(settings[k], chat[k], "{k}: the chat's own settings kept beside the helper");
    }
    let names = place.read("env.names");
    assert!(
        !names.lines().any(|n| n == "ANTHROPIC_API_KEY"),
        "no key from the environment: it'd outrank the helper"
    );
    assert_eq!(ed.session.usage.key_source.as_deref(), Some("apiKeyHelper"));
    assert!(!notes(&ed).iter().any(|n| n.starts_with("Stopped:")), "{:?}", notes(&ed));
    let state = ed.ui_state();
    assert_eq!(state.json()["sessions"][0]["billing"], "api");
    // (what's kept is written on workers: the chat and the session list there before looking)
    until(&mut ed, "the chat and the sessions kept", |_| {
        let files = files_in(&place.state);
        let has = |part: &str| files.iter().any(|(p, _)| p.to_string_lossy().contains(part));
        has("/chats/") && has("/sessions/")
    });
    let sessions = files_in(&place.state.join("sessions"));
    assert!(String::from_utf8_lossy(&sessions[0].1).contains(r#""billing":"api""#), "kept as an API session");
    // The key, nowhere of tarae's
    let nonce = place.key.trim_start_matches("tarae-fake-key-");
    for (path, bytes) in files_in(&place.state) {
        assert!(!String::from_utf8_lossy(&bytes).contains(nonce), "the key in {}", path.display());
    }
    assert!(!log.lines().iter().any(|l| l.contains(nonce)), "the key in the log");
    assert!(
        !state.json().to_string().contains(nonce) && !state.text().contains(nonce),
        "the key in tarae state"
    );
    assert!(!ed.messages.iter().any(|(_, _, m)| m.contains(nonce)), "the key in :messages");
    let rows = ed.session.chat.as_ref().unwrap().msgs.iter().map(|m| m.text.clone()).collect::<Vec<_>>();
    assert!(!rows.iter().any(|t| t.contains(nonce)), "the key in the chat");
}

/// `:session-billing` on a running chat: its claude stops and the next resumes the same conversation on the new
/// billing — the helper going in or out of its `--settings` — and the ask's spare goes and comes back on it (no
/// hour-long cache there: one turn). Refused while Claude answers; an unknown word refused, nothing changed.
#[test]
fn switching_billing_resumes_the_same_conversation_and_the_spare_follows() {
    let place = Place::new("switch");
    let mut ed = place.editor();
    crate::llm::prewarm(&mut ed);
    crate::chat::open(&mut ed);
    send(&mut ed, "hello");
    until(&mut ed, "the first turn", |ed| idle(ed) && ed.session.usage.key_source.is_some());
    assert_eq!(ed.session.chat.as_ref().unwrap().conversation.as_deref(), Some("conv-1"));
    let before = place.starts();
    assert_eq!(before.len(), 2, "the spare and the chat: {before:?}");
    assert!(before.iter().all(|(r, s)| r.is_none() && s.get("apiKeyHelper").is_none()), "{before:?}");
    assert!(ed.session_billing("dollars").is_err());
    ed.session_billing("api").unwrap();
    until(&mut ed, "the chat's and the spare's new claude", |_| place.starts().len() == 4);
    let after = &place.starts()[2..];
    let chat = after.iter().find(|(r, _)| r.as_deref() == Some("conv-1")).expect("the chat resumed conv-1");
    assert_eq!(chat.1["apiKeyHelper"], json!(place.helper));
    assert_eq!(chat.1["promptCacheTtl"], "1h");
    let spare = after.iter().find(|(r, _)| r.is_none()).expect("the spare again");
    assert_eq!(spare.1["apiKeyHelper"], json!(place.helper), "the spare on the key too");
    assert!(spare.1.get("promptCacheTtl").is_none(), "the ask's: one turn, no hour-long cache");
    assert_eq!(ed.session.chat.as_ref().unwrap().billing, Some(Billing::Api));
    // The next turn goes on in the same conversation
    send(&mut ed, "again");
    until(&mut ed, "the turn on the key", |ed| {
        idle(ed) && ed.session.usage.key_source.as_deref() == Some("apiKeyHelper")
    });
    assert_eq!(ed.session.chat.as_ref().unwrap().conversation.as_deref(), Some("conv-1"));
    // While Claude answers: refused, as it was
    std::fs::write(place.root.join("turn.hold"), "").unwrap();
    send(&mut ed, "a long one");
    assert!(ed.session.chat.as_ref().unwrap().busy());
    assert!(ed.session_billing("subscription").unwrap_err().contains("answering"));
    assert_eq!(ed.session.billing, Billing::Api, "unchanged");
}

/// claude says it runs on another credential than its session's billing: said in the chat with what it
/// reported, and it stops — mid-turn the turn is interrupted and the claude goes at its end; as it starts (its
/// answer to `initialize`) it goes at once. Both ways: a key on a subscription session, the plan on an API one.
#[test]
fn a_credential_other_than_its_billings_stops_claude_and_says_so() {
    let place = Place::new("wrong");
    let mut ed = place.editor();
    let log = crate::log::capture();
    ed.session_billing("api").unwrap();
    std::fs::write(place.root.join("turn.source"), "ANTHROPIC_API_KEY").unwrap();
    // (its turn ends only once interrupted)
    std::fs::write(place.root.join("turn.wait"), "").unwrap();
    crate::chat::open(&mut ed);
    send(&mut ed, "hello");
    let said = |ed: &Editor, src: &str| {
        notes(ed).iter().any(|n| n.contains(&format!("reported its credential as {src}")))
    };
    until(&mut ed, "the note", |ed| said(ed, "ANTHROPIC_API_KEY"));
    until(&mut ed, "the turn interrupted, and its claude gone at its end", |ed| {
        idle(ed) && !ed.session.chat.as_ref().unwrap().running()
    });
    let note = notes(&ed).into_iter().find(|n| n.starts_with("Stopped:")).unwrap();
    assert!(note.contains("bills api") && note.contains("env block"), "{note}");
    assert!(place.read("stdin").contains("tarae-stop"), "the turn was interrupted");
    assert_eq!(notes(&ed).last().map(String::as_str), Some("stopped"), "{:?}", notes(&ed));
    assert!(log.lines().iter().any(|l| l.contains("reported its credential as ANTHROPIC_API_KEY")));
    // A subscription session whose claude says it runs on a helper, as it starts: gone before any turn
    ed.session.billing = Billing::Subscription;
    std::fs::write(place.root.join("init.source"), "apiKeyHelper").unwrap();
    crate::chat::reset(&mut ed);
    until(&mut ed, "the note", |ed| said(ed, "apiKeyHelper"));
    assert!(!ed.session.chat.as_ref().unwrap().running(), "stopped as it started");
    let note = notes(&ed).into_iter().rfind(|n| n.starts_with("Stopped:")).unwrap();
    assert!(note.contains("bills subscription") && note.contains("your own Claude settings"), "{note}");
}

/// The ask's claude is checked too: on the session's billing (its spare's `--settings` carry the helper), and one
/// saying another credential is stopped — nothing applied, said why.
#[test]
fn the_asks_claude_is_billed_and_checked_as_the_chats() {
    let place = Place::new("ask");
    let mut ed = place.editor();
    ed.session_billing("api").unwrap();
    std::fs::write(place.root.join("turn.source"), "none").unwrap();
    crate::llm::ask(&mut ed, "rename it").unwrap();
    until(&mut ed, "the ask stopped", |ed| ed.session.review.is_none());
    assert_eq!(place.starts()[0].1["apiKeyHelper"], json!(place.helper));
    let warned = ed.toasts.iter().any(|t| t.text.contains("reported its credential as none"));
    assert!(warned, "{:?}", ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>());
}

/// Billing is the session's: kept in what a restart carries (and the session list), back as it was; a new
/// session takes `llm.billing`; one kept before billing existed takes it too.
#[test]
fn billing_is_kept_with_its_session_and_a_new_one_takes_the_setting() {
    let place = Place::new("kept");
    let mut ed = place.editor();
    ed.session_billing("api").unwrap();
    let carried = crate::worktree::snapshot(&ed);
    let mut back = place.editor();
    assert_eq!(back.session.billing, Billing::Subscription);
    crate::worktree::restore(&mut back, &carried);
    assert_eq!(back.session.billing, Billing::Api, "back after a restart");
    let other = place.dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    back.config.llm.billing = Billing::Subscription;
    let id = back.session_add(other.clone());
    assert_eq!(back.sessions.iter().find(|l| l.id == id).unwrap().billing, Billing::Subscription);
    back.config.llm.billing = Billing::Api;
    let id = back.session_add(place.dir.clone());
    assert_eq!(back.sessions.iter().find(|l| l.id == id).unwrap().billing, Billing::Api, "llm.billing");
    // (`tarae state` says it: each session's, and in its line a key session's `api` — the plan's nothing)
    let state = back.ui_state();
    let billed: Vec<&str> = state.sessions.iter().map(|s| s.billing).collect();
    assert_eq!(billed, ["api", "subscription", "api"]);
    let line = state.text().lines().find(|l| l.starts_with("sessions:")).unwrap().to_string();
    assert_eq!(line.matches(" · api · ").count(), 2, "{line}");
    // (a list kept by an older build: no billing — the setting's)
    let mut old = place.editor();
    old.config.llm.billing = Billing::Api;
    old.session.billing = Billing::Api;
    let v = json!({"sessions": [{"root": place.root, "name": "project"}], "front": place.root});
    crate::worktree::restore(&mut old, &v);
    assert_eq!(old.session.billing, Billing::Api);
}

/// The helper is the Keychain's `security` reading tarae's item for you — your login name on its command line as
/// it is, or refused; and only on macOS (elsewhere: said why, nothing started).
#[test]
fn the_helper_reads_the_keychain_item_and_only_on_macos() {
    assert_eq!(
        helper_for(true, "wq").unwrap(),
        "/usr/bin/security find-generic-password -a wq -s tarae.anthropic-api-key -w"
    );
    assert_eq!(
        helper_for(true, "first.last_2-x").unwrap(),
        "/usr/bin/security find-generic-password -a first.last_2-x -s tarae.anthropic-api-key -w"
    );
    for odd in ["", "-w", "a b", "a;rm", "$(x)", "é"] {
        assert!(helper_for(true, odd).is_err(), "{odd:?}");
    }
    assert!(helper_for(false, "wq").unwrap_err().contains("macOS Keychain"));
}

/// The helper joins the one `--settings` object the chat passes: `claudetrust` holds every command key up front
/// as `""` — an API session's helper takes `apiKeyHelper`'s place, the rest stays as held; a subscription's
/// arguments are untouched. No `--settings` yet: one made. One naming a file: refused (tarae can't add to it
/// unread).
#[test]
fn the_helper_is_merged_into_the_chats_settings_never_beside_them() {
    let seen = crate::claudetrust::Seen {
        scan: None,
        state: crate::trust::State::Trusted,
        file: PathBuf::new(),
        base: Default::default(),
    };
    let (held, _) = crate::claudetrust::settings(crate::chat::CHAT_SETTINGS, &seen);
    let held_json: Value = serde_json::from_str(&held).unwrap();
    assert_eq!(held_json["apiKeyHelper"], "", "held up front");
    let given: Vec<String> = vec!["-p".into(), "--settings".into(), held.clone()];
    let api = |hour_cache| Credential::Api { helper: "the-helper".into(), hour_cache };
    let out = args(&given, &api(true)).unwrap();
    assert_eq!(out.iter().filter(|a| *a == "--settings").count(), 1, "one --settings: {out:?}");
    let got: Value = serde_json::from_str(out.last().unwrap()).unwrap();
    assert_eq!(got["apiKeyHelper"], "the-helper");
    assert_eq!(got["promptCacheTtl"], "1h");
    for (k, v) in held_json.as_object().unwrap().iter().filter(|(k, _)| *k != "apiKeyHelper") {
        assert_eq!(&got[k], v, "{k} as held");
    }
    let one_turn: Value = serde_json::from_str(args(&given, &api(false)).unwrap().last().unwrap()).unwrap();
    assert!(one_turn.get("promptCacheTtl").is_none());
    assert_eq!(args(&given, &Credential::Subscription).unwrap(), given, "the plan: untouched");
    let made = args(&["-p".into()], &api(false)).unwrap();
    assert_eq!(made[1..], ["--settings".to_string(), r#"{"apiKeyHelper":"the-helper"}"#.to_string()]);
    let file: Vec<String> = vec!["--settings".into(), "/some/settings.json".into()];
    assert!(args(&file, &api(true)).unwrap_err().contains("as a file"));
}

/// What's checked: an API session's claude must say `apiKeyHelper`, a subscription's `none` — or, with your key
/// kept (`llm.environment-key = "keep"`), that key. Nothing said: nothing to check.
#[test]
fn each_billing_has_the_one_credential_it_accepts() {
    let check = |b, keep, src| checked(b, keep, Role::Chat, Some(src)).is_none();
    assert!(check(Billing::Api, false, "apiKeyHelper"));
    for src in ["none", "ANTHROPIC_API_KEY", "/login managed key"] {
        assert!(!check(Billing::Api, false, src) && !check(Billing::Api, true, src), "{src}");
    }
    assert!(check(Billing::Subscription, false, "none"));
    assert!(!check(Billing::Subscription, false, "ANTHROPIC_API_KEY"));
    assert!(check(Billing::Subscription, true, "ANTHROPIC_API_KEY"), "your key, kept on purpose");
    assert!(!check(Billing::Subscription, true, "apiKeyHelper"));
    assert_eq!(checked(Billing::Api, false, Role::Chat, None), None);
}
