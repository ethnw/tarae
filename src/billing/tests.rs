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
/// `init`, then the result only once interrupted, as Claude Code ends a turn it's told to stop). `turn.limit`:
/// lines it says before the result (a `rate_limit_event`, an assistant message carrying `error`);
/// `turn.refused`: the result is an error with its words;
/// `turn.exit`: told to stop, it exits instead — the turn without a result; `turn.die`: it exits after
/// `turn.limit`'s lines, unasked.
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
    *tarae-stop*) [ -f turn.exit ] && exit 3; printf '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"","session_id":"%s"}\n' "$id";;
    *'"type":"user"'*) [ -f turn.hold ] || {
      printf '{"type":"system","subtype":"init","session_id":"%s","apiKeySource":"%s","model":"m"}\n' "$id" "$tsrc"
      [ -f turn.limit ] && cat turn.limit
      [ -f turn.die ] && exit 4
      if [ -f turn.refused ]; then
        printf '{"type":"result","subtype":"success","is_error":true,"result":"%s","session_id":"%s"}\n' "$(cat turn.refused)" "$id"
      elif [ ! -f turn.wait ]; then
        printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"%s"}\n' "$id"
      fi
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
        let dir = crate::scratch::Scratch::new(&format!("billing-{name}")).into_path();
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
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

    /// An editor in the project, its claude the stand-in, its key helper the stand-in's (and found, `query`),
    /// keeping its chats and sessions in `state/`.
    fn editor(&self) -> Editor {
        let mut ed = Editor::with_root(crate::config::Config::default(), self.root.clone());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), STAND_IN.into(), "stand-in".into()];
        ed.api_key_helper = Some(self.helper.clone());
        ed.api_key_query = query(true);
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

/// A stand-in for the Keychain query (`api_check`): one that finds the key, or one that doesn't — `security`'s
/// exit status for no such item.
fn query(found: bool) -> Option<Vec<String>> {
    Some(["sh", "-c", if found { "exit 0" } else { "exit 44" }].map(String::from).to_vec())
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

/// `:session-billing api` — on the key once the key check (`api_check`, on a worker) has found it.
fn on_key(ed: &mut Editor) {
    ed.session_billing("api").unwrap();
    until(ed, "the session on the key", |ed| ed.session.billing == Billing::Api);
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
    // (what tarae keeps of billing in the state folder too — the weakness said, then kept)
    ed.billing_kept.path = Some(place.state.join("billing.json"));
    let log = crate::log::capture();
    // (a key of tarae's environment, passed on by "keep" — but not to an API session's claude)
    ed.config.llm.keep_env_key = true;
    ed.claude_env = Some(crate::llm::Env::from([("ANTHROPIC_API_KEY".into(), "tarae-fake-key-env".into())]));
    on_key(&mut ed);
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
        has("/chats/") && has("/sessions/") && has("billing.json")
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
    on_key(&mut ed);
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
    on_key(&mut ed);
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
    on_key(&mut ed);
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
    on_key(&mut ed);
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

// ── Switching, and the plan's limit (step 5's rest) ─────────────────────────

/// The session's conversation cached as a stream would say it: its last request carried `tokens` on `model`, an
/// hour-long write `ago_ms` ago — warm for an hour from then.
fn cache(ed: &mut Editor, model: &str, tokens: u64, ago_ms: u64) {
    let now = crate::usage::now_ms();
    let u = &mut ed.session.usage;
    u.requesting(now - ago_ms);
    let counts =
        crate::usage::Counts { input: Some(10), write: Some(tokens - 10), read: Some(0), output: Some(5) };
    let id = Some(format!("msg-{ago_ms}-{tokens}-{now}"));
    let ttl = Some(crate::usage::Ttl::Hour1);
    u.seen(crate::usage::Seen { id, model: model.into(), counts, ttl, at_ms: now - ago_ms });
}

/// The front card drawn, and in sight long enough for a key to answer it.
fn settled(ed: &mut Editor) {
    crate::term::render(ed, &mut Vec::new(), 140, 44).unwrap();
    let o = (ed.offers[0].id, ed.offers[0].rev);
    ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE)));
}

/// A card's answer — `C-q` first when the chat has the keys (it keeps them as a card comes: frictions26).
fn key(ed: &mut Editor, code: crate::key::Code) {
    if !ed.offers.is_empty() && !ed.card_has_keys() {
        ed.handle_key("C-q".parse().unwrap());
    }
    ed.handle_key(crate::key::Key::plain(code));
}

fn toasts(ed: &Editor) -> String {
    ed.toasts.iter().map(|t| t.text.clone()).collect::<Vec<_>>().join("\n")
}

/// An editor whose chat had a turn on the plan (conversation `conv-1`), its cache warm: 150k tokens of Opus 5.5
/// written a minute ago — and what tarae keeps of billing in the test's state folder.
fn talking(place: &Place) -> Editor {
    let mut ed = place.editor();
    ed.billing_kept.path = Some(place.state.join("billing.json"));
    crate::chat::open(&mut ed);
    send(&mut ed, "hello");
    until(&mut ed, "the first turn", |ed| idle(ed) && ed.session.usage.key_source.is_some());
    cache(&mut ed, "claude-opus-5-5", 150_000, 60_000);
    ed
}

/// `$` in the session list on a session with a conversation going asks first — what its next message costs by its
/// cache (warm: the context at the key's hour-long write, and when it goes cold), and the key's weakness the
/// first time — `n` keeps it, `y` switches and resumes the conversation; the list's row says `api` then. Back to
/// the plan the card says tokens. A session with nothing said yet switches at once, and the weakness, said once
/// ever (kept), is a warning there.
#[test]
fn dollar_in_the_list_asks_first_on_a_conversation_saying_what_it_costs() {
    let place = Place::new("dollar");
    let mut ed = talking(&place);
    ed.session_list();
    ed.session_list_key('$');
    until(&mut ed, "the card, once the key is found", |ed| !ed.offers.is_empty());
    let o = ed.offers.first().expect("asked first").clone();
    assert!(matches!(o.what, crate::offer::What::BillingSwitch { to: Billing::Api, .. }), "{:?}", o.what);
    let q = &o.question;
    assert!(q.contains("The next message rewrites ≈ $1.20 of cached context (150k tokens)"), "{q}");
    assert!(q.contains("The cache is warm until") && q.contains("costs nothing extra"), "{q}");
    let lines: Vec<&str> = q.split('\n').collect();
    let warned = o.warning.clone().expect("the weakness in the warning colour");
    assert_eq!(lines[warned].concat(), WEAKNESS);
    assert_eq!(o.answers(false), [('y', "Switch"), ('n', "Keep it")]);
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Esc);
    assert_eq!(ed.offers.len(), 1, "Esc never answers it");
    key(&mut ed, crate::key::Code::Char('n'));
    assert!(ed.offers.is_empty());
    assert_eq!(ed.session.billing, Billing::Subscription, "n keeps it");
    assert_eq!(place.starts().len(), 1, "nothing started again");
    ed.session_list();
    ed.session_list_key('$');
    until(&mut ed, "the card again", |ed| !ed.offers.is_empty());
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Char('y'));
    assert_eq!(ed.session.billing, Billing::Api);
    until(&mut ed, "the conversation resumed on the key", |_| place.starts().len() == 2);
    assert_eq!(place.starts()[1].0.as_deref(), Some("conv-1"));
    assert!(!toasts(&ed).contains(WEAKNESS), "the card said it — not again as a toast");
    ed.session_list();
    let row = ed.picker.as_ref().unwrap().items()[0].hint.clone();
    assert!(row.split("  ").any(|c| c.trim() == "api"), "the list's row says who pays: {row}");
    // Back to the plan: tokens, toward its limits
    ed.session_list_key('$');
    let back = &ed.offers[0];
    let q = &back.question;
    assert!(q.contains("rewrites ≈ 150k tokens of cached context, toward your plan's limits"), "{q}");
    assert!(back.warning.is_none(), "the weakness is about the key");
    ed.offer_dismiss();
    // A session with nothing said yet: at once — the weakness said once, as a warning, and kept
    let fresh = Place::new("dollar-fresh");
    let mut ed = fresh.editor();
    let kept = fresh.state.join("billing.json");
    ed.billing_kept.path = Some(kept.clone());
    ed.session_list();
    ed.session_list_key('$');
    until(&mut ed, "on the key", |ed| ed.session.billing == Billing::Api);
    assert!(ed.offers.is_empty(), "nothing to cost: no card");
    assert!(toasts(&ed).contains(WEAKNESS), "{}", toasts(&ed));
    until(&mut ed, "the weakness kept as said", |_| Kept::read(Some(kept.clone())).weakness_told);
    let mut again = fresh.editor();
    again.billing_kept = Kept::read(Some(kept.clone()));
    on_key(&mut again);
    assert!(!toasts(&again).contains(WEAKNESS), "said once ever");
}

/// After a switch of billing, model or effort: the toast says what the next message costs, and so does the line
/// over the chat's input — `next message rewrites ≈ $1.20 (billing changed) · switching back costs nothing` —
/// until the next message goes; switching back takes it away. An effort change says what the model's row says:
/// Opus 5.5 may rewrite (unchecked), Fable 5.1 keeps the cache. A cold cache costs nothing extra.
#[test]
fn a_switch_says_what_the_next_message_costs_until_it_goes() {
    let place = Place::new("line");
    let mut ed = talking(&place);
    let info = |value: &str, resolved: &str| crate::llm::ModelInfo {
        value: value.into(),
        name: value.into(),
        about: String::new(),
        resolved: resolved.into(),
        efforts: vec!["low".into(), "high".into()],
    };
    let models = vec![
        info("opus", "claude-opus-5-5"),
        info("sonnet", "claude-sonnet-5-5"),
        info("fable", "claude-fable-5-1"),
    ];
    ed.session.chat.as_mut().unwrap().models = models.clone();
    ed.config.llm.model = Some("opus".into());
    on_key(&mut ed);
    let said = "bills api — Claude runs on your API key from now — next message rewrites ≈ $1.20";
    assert!(toasts(&ed).contains(said), "{}", toasts(&ed));
    let line = ed.switch_line();
    assert_eq!(
        line.as_deref(),
        Some("next message rewrites ≈ $1.20 (billing changed) · switching back costs nothing")
    );
    until(&mut ed, "the chat on the key", |_| place.starts().len() == 2);
    ed.session.chat.as_mut().unwrap().models = models;
    // The model too: Sonnet 5.5's prices ($4/MTok) now — what changed said, still since the last message
    crate::models::set(&mut ed, "sonnet").unwrap();
    assert!(toasts(&ed).contains("next message rewrites ≈ $0.60"), "{}", toasts(&ed));
    let line = ed.switch_line();
    let want = "next message rewrites ≈ $0.60 (billing and model changed) · switching back costs nothing";
    assert_eq!(line.as_deref(), Some(want));
    // Back as it was: nothing to say
    crate::models::set(&mut ed, "opus").unwrap();
    ed.session_billing("subscription").unwrap();
    assert_eq!(ed.switch_line(), None, "back where it was");
    // Effort, as the model's row says: Opus 5.5's is unchecked
    crate::models::set_effort(&mut ed, "low").unwrap();
    let line = ed.switch_line();
    let want = "next message may rewrite ≈ 150k tokens (effort changed) · switching back costs nothing";
    assert_eq!(line.as_deref(), Some(want));
    // The next message goes: the line with it
    send(&mut ed, "go");
    assert_eq!(ed.switch_line(), None, "sent: the cost is that message's");
    until(&mut ed, "the turn", idle);
    // Fable 5.1 keeps its cache over an effort change
    crate::models::set(&mut ed, "fable").unwrap();
    send(&mut ed, "on fable");
    until(&mut ed, "the turn", idle);
    cache(&mut ed, "claude-fable-5-1", 150_000, 1_000);
    crate::models::set_effort(&mut ed, "high").unwrap();
    assert_eq!(ed.switch_line().as_deref(), Some("next message keeps the cache (effort changed)"));
    send(&mut ed, "again");
    until(&mut ed, "the turn", idle);
    // A cold cache: nothing extra
    cache(&mut ed, "claude-fable-5-1", 150_000, 2 * crate::usage::Ttl::Hour1.ms());
    on_key(&mut ed);
    let line = ed.switch_line().unwrap();
    assert!(
        line.starts_with("next message costs nothing extra — cold since ")
            && line.ends_with("(billing changed)"),
        "{line}"
    );
}

/// The plan's weekly window refuses the chat's claude (its `rate_limit_event`, `status: rejected`): the limit
/// card, once for that window's period — what the first message on the key costs, `y` this session · `a` new
/// sessions and the reviewer too until the reset · `n` waits; Esc never answers. `a` is kept, and lapses at the
/// reset. The 5-hour window: a toast, once — never the key. A turn ending on Claude Code's own limit words is a
/// refusal too.
#[test]
fn a_weekly_refusal_asks_once_and_a_reaches_new_sessions_until_the_reset() {
    let place = Place::new("limit");
    let mut ed = talking(&place);
    let resets = crate::usage::now_ms() / 1000 + 3 * 86_400;
    let event = |window: &str| {
        json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": window,
               "resetsAt": resets, "utilization": 1.0, "isUsingOverage": false}})
        .to_string()
            + "\n"
    };
    std::fs::write(place.root.join("turn.limit"), event("five_hour")).unwrap();
    send(&mut ed, "one");
    until(&mut ed, "the turn", idle);
    assert!(toasts(&ed).contains("5-hour limit · resets"), "{}", toasts(&ed));
    assert!(ed.offers.is_empty(), "the 5-hour window never offers the key");
    ed.toasts.clear();
    send(&mut ed, "two");
    until(&mut ed, "the turn", idle);
    assert!(!toasts(&ed).contains("5-hour limit"), "said once a period");
    // The weekly window: the card
    std::fs::write(place.root.join("turn.limit"), event("seven_day")).unwrap();
    send(&mut ed, "three");
    until(&mut ed, "the turn and its card", |ed| idle(ed) && !ed.offers.is_empty());
    let o = ed.offers.first().expect("the limit card").clone();
    assert!(o.title.starts_with("Weekly limit reached · resets "), "{}", o.title);
    assert!(o.question.contains("The first message rewrites ≈ $1.20 of cached context"), "{}", o.question);
    assert_eq!(o.answers(false), [('y', "This session"), ('a', "New sessions too"), ('n', "Wait")]);
    assert!(o.goes_with_session(), "its session's");
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Esc);
    assert_eq!(ed.offers.len(), 1, "Esc never answers it");
    key(&mut ed, crate::key::Code::Char('a'));
    assert!(ed.offers.is_empty(), "answered");
    until(&mut ed, "this session on the key", |ed| ed.session.billing == Billing::Api);
    until(&mut ed, "resumed on the key", |_| {
        place
            .starts()
            .last()
            .is_some_and(|s| s.0.as_deref() == Some("conv-1") && s.1["apiKeyHelper"] == json!(place.helper))
    });
    let other = place.dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let id = ed.session_add(other);
    assert_eq!(ed.sessions.iter().find(|l| l.id == id).unwrap().billing, Billing::Api, "a new session");
    ed.config.llm.reviewer_billing = Some(Billing::Subscription);
    assert_eq!(ed.reviewer_billing(), Billing::Api, "the reviewer too");
    let kept = place.state.join("billing.json");
    until(&mut ed, "kept", |_| Kept::read(Some(kept.clone())).api_until_ms == Some(resets * 1000));
    // Past the reset it lapses by itself
    ed.billing_kept.api_until_ms = Some(crate::usage::now_ms() - 1);
    assert_eq!(ed.new_session_billing(), Billing::Subscription);
    assert_eq!(ed.reviewer_billing(), Billing::Subscription, "llm.reviewer-billing again");
    // The same refusal again, back on the plan: not asked again for that period
    ed.session_billing("subscription").unwrap();
    send(&mut ed, "four");
    until(&mut ed, "the turn", idle);
    assert!(ed.offers.is_empty(), "asked once a period");
    // Claude Code's own words end a turn: refused too — a new period's card
    ed.session.limit_asked = None;
    std::fs::remove_file(place.root.join("turn.limit")).unwrap();
    std::fs::write(place.root.join("turn.refused"), "You've hit your weekly limit · resets Mon 12:00am")
        .unwrap();
    send(&mut ed, "five");
    until(&mut ed, "the turn and its card", |ed| idle(ed) && !ed.offers.is_empty());
    let o = ed.offers.first().expect("the words' card");
    assert!(matches!(&o.what, crate::offer::What::Limit(a) if a.window == "seven_day"), "{:?}", o.what);
}

/// The limit card's `y` while Claude still answers: the key pays once the turn is over — the chat goes on it
/// then, the conversation resumed. A reset not known: no `a` (until when?). `:session-billing api` meanwhile is
/// refused as it's typed.
#[test]
fn the_limit_cards_yes_mid_turn_waits_for_the_turns_end() {
    let place = Place::new("limit-mid");
    let mut ed = talking(&place);
    std::fs::write(place.root.join("turn.wait"), "").unwrap();
    send(&mut ed, "long");
    // (`:session-billing api` meanwhile: refused at once, before any key check)
    let refused = ed.session_billing("api").map_err(|e| e.contains("answering"));
    assert_eq!(refused, Err(true), "refused as it's typed, not later as a toast");
    yes_mid_turn(&mut ed);
    crate::chat::stop(&mut ed);
    until(&mut ed, "the turn's end", |ed| idle(ed) && ed.session.billing == Billing::Api);
    until(&mut ed, "resumed on the key", |_| {
        place.starts().last().is_some_and(|s| s.0.as_deref() == Some("conv-1"))
    });
}

/// The limit card for a weekly refusal (no reset known), up once the key is found, and its `y` while Claude still
/// answers: on the key once the turn is over, said so.
fn yes_mid_turn(ed: &mut Editor) {
    ed.limit_hit("seven_day", None);
    until(ed, "the limit card", |ed| !ed.offers.is_empty());
    assert!(!ed.offers[0].limit_all(), "no reset known: no `a`");
    ed.offer_accept();
    until(ed, "the answer", |ed| toasts(ed).contains("Your API key pays once this turn is over"));
    assert_eq!(ed.session.billing, Billing::Subscription, "not mid-turn");
}

/// The limit card's `y` mid-turn, then claude exits before the turn's result: the turn is over all the same, and
/// the key pays from the next message. A new conversation started instead drops the switch that waited — the
/// next message goes on the plan, as before.
#[test]
fn a_switch_waiting_for_the_turn_goes_on_when_claude_exits_and_not_into_a_new_conversation() {
    let place = Place::new("limit-exit");
    let mut ed = talking(&place);
    std::fs::write(place.root.join("turn.wait"), "").unwrap();
    send(&mut ed, "cut off");
    yes_mid_turn(&mut ed);
    // (told to stop, the stand-in exits — no result ends the turn)
    std::fs::write(place.root.join("turn.exit"), "").unwrap();
    crate::chat::stop(&mut ed);
    until(&mut ed, "the exit ends the turn on the key", |ed| idle(ed) && ed.session.billing == Billing::Api);
    assert_eq!(ed.session.billing_next, None);
    // A new conversation instead: the switch waiting for the old one's turn goes with it
    let place = Place::new("limit-reset");
    let mut ed = talking(&place);
    std::fs::write(place.root.join("turn.wait"), "").unwrap();
    send(&mut ed, "long");
    yes_mid_turn(&mut ed);
    crate::chat::reset(&mut ed);
    assert_eq!(ed.session.billing_next, None, "dropped with the conversation");
    std::fs::remove_file(place.root.join("turn.wait")).unwrap();
    send(&mut ed, "fresh");
    until(&mut ed, "the new conversation's turn", |ed| idle(ed) && place.read("stdin").contains("fresh"));
    assert_eq!(ed.session.billing, Billing::Subscription, "still the plan");
    assert_eq!(place.starts().last().unwrap().1["apiKeyHelper"], Value::Null, "{:?}", place.starts());
}

/// A new session's card asks who pays near the plan's weekly limit (80 %): the plan first, your key first from
/// 95 % — `$` switches. Below, it asks nothing; the 5-hour window only says to wait; an Opus session counts the
/// weekly Opus window too; nothing known asks nothing; `a` on the limit card makes it the key's, said.
#[test]
fn a_new_session_asks_who_pays_near_the_weekly_limit() {
    let place = Place::new("ask-new");
    let mut ed = place.editor();
    // (as `api_check` leaves it where the key is found — `:session-new` looks before its card)
    ed.api_ready = Some(Ok(()));
    let now = crate::usage::now_ms();
    let week = now + 3 * 86_400_000;
    let read = |window: &str, percent: u32, resets: u64| crate::limits::Reading {
        window: window.into(),
        percent: Some(percent),
        resets_at_ms: Some(resets),
        status: None,
        seen_at_ms: now,
        from: crate::limits::Source::Usage,
    };
    let set = |ed: &mut Editor, rs: Vec<crate::limits::Reading>| {
        ed.limits = crate::limits::Limits::default();
        ed.limits.seen(crate::limits::Report { readings: rs, ..Default::default() });
    };
    assert_eq!(ed.new_session_ask(), (Billing::Subscription, None, None), "nothing known: nothing asked");
    set(&mut ed, vec![read("seven_day", 79, week)]);
    assert_eq!(ed.new_session_ask().1, None, "under 80 %");
    set(&mut ed, vec![read("seven_day", 84, week)]);
    let (b, ask, _) = ed.new_session_ask();
    assert_eq!(b, Billing::Subscription);
    assert!(ask.as_deref().is_some_and(|a| a.starts_with("weekly 84 % · resets")), "{ask:?}");
    set(&mut ed, vec![read("seven_day", 96, week)]);
    assert_eq!(ed.new_session_ask().0, Billing::Api, "your key first from 95 %");
    // `llm.billing = "api"`: the key from the start — near the limit too, nothing asked
    ed.config.llm.billing = Billing::Api;
    for p in [84, 96] {
        set(&mut ed, vec![read("seven_day", p, week)]);
        assert_eq!(ed.new_session_ask(), (Billing::Api, None, None), "llm.billing = api at {p} %");
    }
    ed.config.llm.billing = Billing::Subscription;
    // The 5-hour window: said, never asked
    set(&mut ed, vec![read("seven_day", 50, week), read("five_hour", 91, now + 3_600_000)]);
    let (b, ask, note) = ed.new_session_ask();
    assert_eq!((b, ask), (Billing::Subscription, None));
    assert!(note.as_deref().is_some_and(|n| n.contains("5-hour 91 %") && n.contains("wait")), "{note:?}");
    // An Opus session: the weekly Opus window counts
    set(&mut ed, vec![read("seven_day", 50, week), read("seven_day_opus", 90, week)]);
    ed.config.llm.model = Some("opus".into());
    assert!(ed.new_session_ask().1.is_some_and(|a| a.starts_with("weekly opus 90 %")));
    // The card: `$` switches, the weakness said while the key would pay
    set(&mut ed, vec![read("seven_day", 96, week)]);
    let mut m = making(&place);
    (m.billing, m.billing_ask, m.billing_note) = ed.new_session_ask();
    let card = ed.session_card(&m);
    let q = &card.question;
    assert!(
        q.contains("The plan's weekly 96 %")
            && q.contains("Its Claude runs on your API key — $ puts it on your plan"),
        "{q}"
    );
    assert!(card.warning.is_some(), "the weakness");
    assert!(card.answers(false).contains(&('$', "Bill plan")));
    ed.push_offer(card);
    ed.offer_session_billing();
    let o = &ed.offers[0];
    assert!(
        o.question.contains("Its Claude runs on your plan — $ puts it on your API key"),
        "{}",
        o.question
    );
    assert!(o.warning.is_none() && o.answers(false).contains(&('$', "Bill API")));
    // `a` on the limit card: the key's, said — no question
    ed.billing_kept.api_until_ms = Some(week);
    let (b, ask, note) = ed.new_session_ask();
    assert_eq!((b, ask), (Billing::Api, None));
    assert!(note.unwrap().contains("until"));
}

/// Where the key can't be used (a stand-in Keychain query that doesn't find it), nothing offers or keeps it: a
/// weekly refusal says to wait and how to add the key — no card; a new session near the limit isn't asked, and
/// `a` kept from before reaches neither it (its line says why) nor the reviewer; `$` in the list and
/// `:session-billing api` say why and stay on the plan. A key gone between the limit card and its `a`: nothing switched, nothing kept.
#[test]
fn where_the_key_cant_be_used_nothing_offers_or_keeps_it() {
    let place = Place::new("no-key");
    let mut ed = place.editor();
    ed.api_key_query = query(false);
    let kept = place.state.join("billing.json");
    ed.billing_kept.path = Some(kept.clone());
    let add =
        "add it in a terminal: security add-generic-password -a \"$USER\" -s tarae.anthropic-api-key -w";
    let now = crate::usage::now_ms();
    let week = now + 3 * 86_400_000;
    ed.limit_hit("seven_day", Some(week));
    until(&mut ed, "the refusal said", |ed| toasts(ed).contains(add));
    assert!(toasts(&ed).contains("weekly limit · resets"), "{}", toasts(&ed));
    assert!(ed.offers.is_empty(), "no limit card");
    // A new session near the limit: not asked — and `a` kept from before doesn't reach it, said
    let reading = crate::limits::Reading {
        window: "seven_day".into(),
        percent: Some(96),
        resets_at_ms: Some(week),
        status: None,
        seen_at_ms: now,
        from: crate::limits::Source::Usage,
    };
    ed.limits.seen(crate::limits::Report { readings: vec![reading], ..Default::default() });
    assert_eq!(ed.new_session_ask(), (Billing::Subscription, None, None), "96 % and no key: not asked");
    ed.billing_kept.api_until_ms = Some(week);
    let (b, ask, note) = ed.new_session_ask();
    assert_eq!((b, ask), (Billing::Subscription, None));
    assert!(note.as_deref().is_some_and(|n| n.contains(add)), "{note:?}");
    assert_eq!(ed.new_session_billing(), Billing::Subscription, "a new session");
    assert_eq!(ed.reviewer_billing(), Billing::Subscription, "the reviewer");
    ed.billing_kept.api_until_ms = None;
    // `$` in the list
    ed.toasts.clear();
    ed.session_list();
    ed.session_list_key('$');
    until(&mut ed, "`$` refused", |ed| toasts(ed).contains(add));
    assert_eq!(ed.session.billing, Billing::Subscription);
    // `:session-billing api` the same: said why and how to add the key, nothing changed
    ed.toasts.clear();
    ed.session_billing("api").unwrap();
    until(&mut ed, "`:session-billing api` refused", |ed| toasts(ed).contains(add));
    assert_eq!(ed.session.billing, Billing::Subscription, "nothing changed");
    // The key there for the card, gone by its `a`
    ed.api_key_query = query(true);
    ed.limit_hit("seven_day", Some(week + 1000));
    until(&mut ed, "the limit card", |ed| !ed.offers.is_empty());
    ed.api_key_query = query(false);
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Char('a'));
    until(&mut ed, "the answer refused", |ed| toasts(ed).contains("Still on your plan"));
    assert_eq!(ed.session.billing, Billing::Subscription);
    assert_eq!(ed.billing_kept.api_until_ms, None, "nothing kept");
    assert!(!kept.exists(), "nothing written");
}

/// What's kept of billing is written by the last save asked of its file: one finishing late is skipped — but only
/// against its own file's; another file's newer save (another state folder, as tests side by side have) never
/// skips it.
#[test]
fn a_late_save_is_skipped_only_against_its_own_file() {
    let (a, b) = (Path::new("/state-a/billing.json"), Path::new("/state-b/billing.json"));
    let mut written = Vec::new();
    assert!(newest(&mut written, b, 2), "b's save 2 first");
    assert!(newest(&mut written, a, 1), "a's save 1 after it: another file's — written");
    assert!(newest(&mut written, a, 3));
    assert!(!newest(&mut written, a, 1), "a's own older save, finishing late: skipped");
    assert!(newest(&mut written, b, 4));
}

/// What a create card is made from, by hand (no git asked): a branch `feat` from main.
fn making(place: &Place) -> crate::worktree::Making {
    crate::worktree::Making {
        top: place.root.clone(),
        branch: "feat".into(),
        base: "main".into(),
        base_sha: "0123456789abcdef".into(),
        path: place.dir.join("feat"),
        files: (1 << 20, true),
        big: Vec::new(),
        builds: None,
        build_own: None,
        copies: Vec::new(),
        others: Vec::new(),
        billing: Billing::Subscription,
        billing_ask: None,
        billing_note: None,
    }
}

/// `r` in `:procs` starts the chat's claude again on the same conversation, as it says — with `--resume`, not a
/// new conversation on claude's side.
#[test]
fn restarting_claude_from_procs_resumes_its_conversation() {
    let place = Place::new("procs-r");
    let mut ed = talking(&place);
    crate::chat::restart_process(&mut ed);
    until(&mut ed, "claude again", |_| place.starts().len() == 2);
    assert_eq!(place.starts()[1].0.as_deref(), Some("conv-1"), "{:?}", place.starts());
}

// ── The key, out of money ───────────────────────────────────────────────────

// What claude says when a request fails — the shapes from the sources (`target/sb/apibill-src/`): an assistant
// message carrying `error` (`sdk-ts-sdk.d.ts` `SDKAssistantMessage`, `SDKAssistantMessageError`), then the turn's
// failed `result` (`api_error_status`); the words from Claude Code's errors reference (`Credit balance is too
// low`, `Request rejected (429)`) and the platform's rate limits page (the spend cap's message). Made up: the ids,
// the dates, and the `API Error: <status>` before the API's message.

const CREDITS: &str = "Credit balance is too low";
const SPEND_CAP: &str = "API Error: 429 You have reached your API usage limits: your organization has crossed its \
                         monthly API usage threshold, set based on your organization's API tier. You will regain \
                         access on 2026-11-01 at 00:00 UTC.";
const THROTTLE: &str = "API Error: Request rejected (429) · this may be a temporary capacity issue. If it persists, \
                        check https://status.claude.com.";

/// An assistant message carrying `error` — Claude Code's stand-in answer to a failed request.
fn assistant_error(category: &str, text: &str) -> Value {
    json!({"type": "assistant", "error": category, "parent_tool_use_id": null, "session_id": "conv-1",
           "message": {"id": "msg-err-1", "model": "<synthetic>", "role": "assistant",
                       "content": [{"type": "text", "text": text}]}})
}

/// A turn's failed result.
fn failed(text: &str, status: u64) -> Value {
    json!({"type": "result", "subtype": "success", "is_error": true, "result": text, "api_error_status": status,
           "session_id": "conv-1"})
}

/// The stand-in's next turns fail with `text`: an assistant `error` of `category` first, then the result.
fn failing(place: &Place, category: &str, text: &str) {
    std::fs::write(place.root.join("turn.limit"), assistant_error(category, text).to_string() + "\n")
        .unwrap();
    std::fs::write(place.root.join("turn.refused"), text).unwrap();
}

/// Money, told from a throttle: `billing_error` or Claude Code's credit words are the credits; the spend cap is
/// its own words — a 429 `rate_limit` like a throttle's, which says none of them; a limit you set below the cap (a
/// 400) is one too. The day the API names, as said; a line that isn't an error is nothing, whatever its words.
#[test]
fn money_is_told_from_a_throttle_by_what_claude_reports() {
    assert_eq!(refusal(&failed(CREDITS, 400)), Some(Money::Credits));
    assert_eq!(refusal(&assistant_error("billing_error", "anything")), Some(Money::Credits));
    let cap = Money::Spend { until: Some("2026-11-01".into()) };
    assert_eq!(refusal(&failed(SPEND_CAP, 429)), Some(cap.clone()));
    assert_eq!(refusal(&assistant_error("rate_limit", SPEND_CAP)), Some(cap.clone()));
    assert_eq!(cap.said(), "API spend limit reached · until the 1st");
    assert_eq!(refusal(&assistant_error("rate_limit", THROTTLE)), None, "a throttle");
    assert_eq!(refusal(&failed(THROTTLE, 429)), None, "a throttle");
    let set = "API Error: 400 You have reached your specified API usage limits. You will regain access on \
               2026-10-15 at 00:00 UTC.";
    assert_eq!(refusal(&failed(set, 400)).unwrap().said(), "API spend limit reached · until 2026-10-15");
    let no_day = "API Error: 429 {\"error_code\":\"enforced_spend_limit_reached\"}";
    assert_eq!(refusal(&failed(no_day, 429)).unwrap().said(), "API spend limit reached");
    assert_eq!(Money::Credits.said(), "API credits ran out");
    // (not an error: Claude's own words about credits, a result that went well)
    let words = json!({"type": "assistant", "message": {"content": [{"type": "text", "text": CREDITS}]}});
    assert_eq!(refusal(&words), None);
    assert_eq!(refusal(&json!({"type": "result", "is_error": false, "result": CREDITS})), None);
}

/// An API session's claude refused for its credits: the chat says `API credits ran out` in place of claude's
/// words, and a card offers the plan back — `y` this session · `n` stay, Esc never answers; `y` resumes the same
/// conversation on the plan, the line over the input saying what the next message costs, as for any switch. A
/// throttle's 429 before it: claude's words only, nothing offered.
#[test]
fn credits_running_out_offers_the_plan_back() {
    let place = Place::new("credits");
    let mut ed = talking(&place);
    on_key(&mut ed);
    until(&mut ed, "the chat on the key", |_| place.starts().len() == 2);
    failing(&place, "rate_limit", THROTTLE);
    send(&mut ed, "one");
    until(&mut ed, "the turn", idle);
    assert_eq!(notes(&ed).last().map(String::as_str), Some(THROTTLE));
    assert!(ed.offers.is_empty(), "a throttle offers nothing");
    failing(&place, "billing_error", CREDITS);
    send(&mut ed, "two");
    until(&mut ed, "the turn and its card", |ed| idle(ed) && !ed.offers.is_empty());
    assert_eq!(notes(&ed).last().map(String::as_str), Some("API credits ran out"));
    assert!(!notes(&ed).iter().any(|n| n == CREDITS), "tarae's words, not claude's: {:?}", notes(&ed));
    let o = ed.offers[0].clone();
    assert!(o.title.ends_with(": API credits ran out"), "{}", o.title);
    assert!(o.question.contains("Continue this conversation on your plan?"), "{}", o.question);
    assert!(o.question.contains("The next message"), "{}", o.question);
    assert_eq!(o.answers(false), [('y', "This session"), ('n', "Stay")]);
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Esc);
    assert_eq!(ed.offers.len(), 1, "Esc never answers it");
    key(&mut ed, crate::key::Code::Char('y'));
    assert_eq!(ed.session.billing, Billing::Subscription);
    until(&mut ed, "the conversation resumed on the plan", |_| place.starts().len() == 3);
    let (resume, settings) = place.starts()[2].clone();
    assert_eq!(resume.as_deref(), Some("conv-1"));
    assert_eq!(settings["apiKeyHelper"], Value::Null, "no key's helper: {settings}");
    let line = ed.switch_line().unwrap_or_default();
    assert!(line.starts_with("next message") && line.contains("(billing changed)"), "{line}");
}

/// The spend cap: `API spend limit reached · until the 1st`. While the plan refuses the session too (its weekly
/// window `rejected`) that's said beside it and nothing's offered; once it doesn't, the card — one a session
/// however many turns fail, and why; `n` stays on the key. A subscription session refused for money ran on a key it
/// shouldn't have (`checked`'s): claude's words only.
#[test]
fn a_spend_cap_offers_the_plan_only_where_the_plan_takes_it() {
    let place = Place::new("spend");
    let mut ed = talking(&place);
    failing(&place, "billing_error", CREDITS);
    send(&mut ed, "on the plan");
    until(&mut ed, "the turn", idle);
    assert_eq!(notes(&ed).last().map(String::as_str), Some(CREDITS), "a subscription's: claude's words");
    assert!(ed.offers.is_empty());
    on_key(&mut ed);
    until(&mut ed, "the chat on the key", |_| place.starts().len() == 2);
    failing(&place, "rate_limit", SPEND_CAP);
    let now = crate::usage::now_ms();
    let refused = crate::limits::Reading {
        window: "seven_day".into(),
        percent: Some(100),
        resets_at_ms: Some(now + 3 * 86_400_000),
        status: Some(crate::limits::Status::Rejected),
        seen_at_ms: now,
        from: crate::limits::Source::Event,
    };
    ed.limits.seen(crate::limits::Report { readings: vec![refused], ..Default::default() });
    send(&mut ed, "one");
    until(&mut ed, "the turn", idle);
    let note = notes(&ed).last().cloned().unwrap_or_default();
    assert!(
        note.starts_with(
            "API spend limit reached · until the 1st — and the plan refuses it too (weekly limit"
        ),
        "{note}"
    );
    assert!(ed.offers.is_empty(), "the plan refuses too: nothing to offer");
    // The plan takes it again: the card, one however many turns fail
    ed.limits = crate::limits::Limits::default();
    send(&mut ed, "two");
    until(&mut ed, "the turn and its card", |ed| idle(ed) && !ed.offers.is_empty());
    assert!(
        ed.offers[0].title.ends_with(": API spend limit reached · until the 1st"),
        "{}",
        ed.offers[0].title
    );
    // (a card of the same title is one anyway — `push_offer`; the credits' title isn't the spend limit's)
    failing(&place, "billing_error", CREDITS);
    send(&mut ed, "three");
    until(&mut ed, "the turn", idle);
    assert_eq!(notes(&ed).last().map(String::as_str), Some("API credits ran out"));
    assert_eq!(ed.offers.len(), 1, "one card a session");
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Char('n'));
    assert!(ed.offers.is_empty());
    assert_eq!(ed.session.billing, Billing::Api, "n stays on the key");
}

/// A refusal for money counts only for the turn it ends, on its error: a turn that's answered after all, or one
/// you stopped, says what it says and offers nothing; a turn cut off — claude exiting, a new chat — leaves nothing
/// behind: the next one, failing for another reason, says its own error.
#[test]
fn a_money_refusal_counts_only_for_the_turn_it_ends() {
    let place = Place::new("money-turn");
    let mut ed = talking(&place);
    on_key(&mut ed);
    until(&mut ed, "the chat on the key", |_| place.starts().len() == 2);
    let error_line = assistant_error("billing_error", CREDITS).to_string() + "\n";
    std::fs::write(place.root.join("turn.limit"), &error_line).unwrap();
    // Answered after all
    send(&mut ed, "answered");
    until(&mut ed, "the turn", idle);
    assert!(notes(&ed).last().is_some_and(|n| n.ends_with('s')), "the turn's time: {:?}", notes(&ed));
    assert!(ed.offers.is_empty(), "an answered turn offers nothing");
    // Stopped
    std::fs::write(place.root.join("turn.wait"), "").unwrap();
    send(&mut ed, "stopped");
    until(&mut ed, "the refusal read", |ed| ed.session.chat.as_ref().unwrap().money.is_some());
    crate::chat::stop(&mut ed);
    until(&mut ed, "the turn", idle);
    assert_eq!(notes(&ed).last().map(String::as_str), Some("stopped"));
    assert!(ed.offers.is_empty(), "a turn you stopped offers nothing");
    // Cut off: claude exits after the error, the turn without its end
    std::fs::remove_file(place.root.join("turn.wait")).unwrap();
    std::fs::write(place.root.join("turn.die"), "").unwrap();
    send(&mut ed, "cut off");
    until(&mut ed, "claude gone", |ed| idle(ed) && !ed.session.chat.as_ref().unwrap().running());
    std::fs::remove_file(place.root.join("turn.die")).unwrap();
    std::fs::remove_file(place.root.join("turn.limit")).unwrap();
    std::fs::write(place.root.join("turn.refused"), "Overloaded").unwrap();
    send(&mut ed, "next");
    until(&mut ed, "the next turn", |ed| {
        idle(ed) && notes(ed).iter().any(|n| n == "Overloaded" || n == "API credits ran out")
    });
    assert_eq!(notes(&ed).last().map(String::as_str), Some("Overloaded"), "{:?}", notes(&ed));
    assert!(ed.offers.is_empty(), "nothing left over from the turn claude's exit cut off");
    // Cut off by a new chat: the refusal read, then `C-l` before the turn's end
    std::fs::remove_file(place.root.join("turn.refused")).unwrap();
    std::fs::write(place.root.join("turn.limit"), &error_line).unwrap();
    std::fs::write(place.root.join("turn.wait"), "").unwrap();
    send(&mut ed, "before the new chat");
    until(&mut ed, "the refusal read", |ed| ed.session.chat.as_ref().unwrap().money.is_some());
    crate::chat::reset(&mut ed);
    std::fs::remove_file(place.root.join("turn.wait")).unwrap();
    std::fs::remove_file(place.root.join("turn.limit")).unwrap();
    std::fs::write(place.root.join("turn.refused"), "Overloaded").unwrap();
    send(&mut ed, "in the new chat");
    until(&mut ed, "the new chat's turn", |ed| {
        idle(ed) && notes(ed).iter().any(|n| n == "Overloaded" || n == "API credits ran out")
    });
    assert_eq!(notes(&ed).last().map(String::as_str), Some("Overloaded"), "{:?}", notes(&ed));
    assert!(ed.offers.is_empty(), "nothing left over from the turn the new chat cut off");
}

/// One card a session for one cause, also while the session is behind (#211: each refused turn of a session behind
/// parked another identical card — the check looked only at the front's cards).
#[test]
fn a_session_behind_refused_twice_for_money_has_one_card() {
    let place = Place::new("money-behind");
    let mut ed = talking(&place);
    on_key(&mut ed);
    until(&mut ed, "the chat on the key", |_| place.starts().len() == 2);
    let sid = ed.session.id;
    std::fs::create_dir_all(place.dir.join("other")).unwrap();
    let other = ed.session_add(place.dir.join("other"));
    assert!(ed.session_switch(other));
    failing(&place, "billing_error", CREDITS);
    let parked = |ed: &Editor| {
        let s = ed.sessions.iter().find(|s| s.id == sid).unwrap();
        s.parked.iter().filter(|o| matches!(o.what, crate::offer::What::BillingSwitch { .. })).count()
    };
    let busy_behind =
        |ed: &Editor| ed.sessions.iter().find(|s| s.id == sid).unwrap().chat.as_ref().unwrap().busy();
    for text in ["one", "two"] {
        ed.with_session(sid, |ed| send(ed, text));
        until(&mut ed, "the turn behind", |ed| !busy_behind(ed) && parked(ed) > 0);
    }
    assert!(place.read("stdin").contains("two"), "the second turn went");
    assert_eq!(parked(&ed), 1, "one card for the session behind");
    assert!(ed.offers.is_empty(), "nothing up over the session in front");
}

/// A message queued during a turn refused for money isn't sent into the same refusal: it waits, said, and goes
/// once the session's billing changes — `y` on the card.
#[test]
fn a_message_queued_behind_a_refusal_for_money_waits_for_the_billing() {
    let place = Place::new("money-queued");
    let mut ed = talking(&place);
    on_key(&mut ed);
    until(&mut ed, "the chat on the key", |_| place.starts().len() == 2);
    failing(&place, "billing_error", CREDITS);
    send(&mut ed, "one");
    send(&mut ed, "queued two");
    assert_eq!(
        ed.session.chat.as_ref().unwrap().queued.as_deref(),
        Some("queued two"),
        "Claude was answering"
    );
    until(&mut ed, "the turn and its card", |ed| idle(ed) && !ed.offers.is_empty());
    assert!(!place.read("stdin").contains("queued two"), "not sent into the refusal");
    assert_eq!(ed.session.chat.as_ref().unwrap().queued.as_deref(), Some("queued two"));
    assert!(notes(&ed).iter().any(|n| n.starts_with("Your queued message waits")), "{:?}", notes(&ed));
    std::fs::remove_file(place.root.join("turn.limit")).unwrap();
    std::fs::remove_file(place.root.join("turn.refused")).unwrap();
    settled(&mut ed);
    key(&mut ed, crate::key::Code::Char('y'));
    assert_eq!(ed.session.billing, Billing::Subscription);
    until(&mut ed, "the queued message sent on the plan", |_| place.read("stdin").contains("queued two"));
    assert_eq!(ed.session.chat.as_ref().unwrap().queued, None);
}
