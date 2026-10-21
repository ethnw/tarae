use super::*;
use std::time::{Duration, Instant};

const PROJECT: &str = ".claude/settings.json";
const LOCAL: &str = ".claude/settings.local.json";

/// One file's bytes, as `look` gives them to `scan`.
fn file(name: &'static str, text: &str) -> (&'static str, FileRead) {
    (name, Some(Ok(text.as_bytes().to_vec())))
}

fn keys(scan: &Scan) -> Vec<(Kind, &str)> {
    scan.items.iter().map(|i| (i.kind, i.key.as_str())).collect()
}

/// Your own Claude settings are a hold-back's baseline: every claude tarae starts loads them, confined or not, on
/// either system (claude's folder is always yours).
#[test]
fn your_claude_settings_are_the_baseline_where_claude_loads_them() {
    let mut ed = Editor::new(crate::config::Config::default());
    let yours = std::path::PathBuf::from("/home/stand-in/.claude/settings.json");
    ed.claude_user_settings = Some(yours.clone());
    for confined in [false, true] {
        ed.config.llm.claude_sandbox = confined;
        assert_eq!(Places::of(&ed).user, Some(yours.clone()), "confined: {confined}");
    }
}

/// A repository's file can run a command as you (`apiKeyHelper` and its kin), set claude's environment and
/// turn plugins on — each held, by name. Hooks it sets are only shown: the chat's own `--settings` has them off
/// already. A plugin it turns *off* isn't asked about (off is what holding back does).
#[test]
fn what_a_repository_runs_is_held_and_hooks_only_shown() {
    let text = r#"{
        "apiKeyHelper": "printf stand-in-key",
        "awsCredentialExport": "printf stand-in-aws",
        "env": {"ANTHROPIC_BASE_URL": "https://stand-in.invalid", "NODE_OPTIONS": "--stand-in"},
        "enabledPlugins": {"deploy@team": true, "lint@team": false},
        "hooks": {"PreToolUse": []},
        "statusLine": {"type": "command", "command": "printf x"},
        "permissions": {"allow": ["Read"]}
    }"#;
    let scan = scan(&[file(PROJECT, text), (LOCAL, None)]).expect("something to ask about");
    assert_eq!(
        keys(&scan),
        [
            (Kind::Command, "apiKeyHelper"),
            (Kind::Command, "awsCredentialExport"),
            (Kind::Env, "ANTHROPIC_BASE_URL"),
            (Kind::Env, "NODE_OPTIONS"),
            (Kind::Plugin, "deploy@team"),
            (Kind::Allow, "Read"),
        ]
    );
    assert_eq!(scan.held, [(PROJECT, "hooks".to_string()), (PROJECT, "statusLine".to_string())]);
    assert!(scan.unreadable.is_empty());
}

/// No file, or a file that sets nothing that runs, reaches claude's environment or lets a tool run unasked
/// (rules that only deny or ask, a model): no card — the chat starts as it always did.
#[test]
fn nothing_to_ask_about_is_no_card() {
    assert_eq!(scan(&[(PROJECT, None), (LOCAL, None)]), None);
    let calm =
        r#"{"permissions": {"deny": ["Bash"], "ask": ["Read"]}, "model": "sonnet", "hooks": {}, "env": {}}"#;
    assert_eq!(scan(&[file(PROJECT, calm), (LOCAL, None)]), None, "held hooks alone ask nothing");
}

/// Held back (`n`), every held key is laid over in the chat's `--settings` by name — a variable emptied, a
/// helper emptied, a plugin off — and the chat's own settings stay. What a variable held never reaches the
/// command line (it may be a token): only its name does.
#[test]
fn held_back_keys_are_overridden_by_name_never_by_value() {
    let text = r#"{"apiKeyHelper": "printf stand-in-key", "env": {"ANTHROPIC_BASE_URL": "https://stand-in.invalid"},
        "enabledPlugins": {"deploy@team": true}}"#;
    let seen = Seen {
        scan: scan(&[file(PROJECT, text)]),
        state: State::Ignored,
        file: PathBuf::from("/p"),
        base: Baseline::default(),
    };
    let (json, sources) = settings(crate::chat::CHAT_SETTINGS, &seen);
    assert!(!json.contains("stand-in"), "no value of theirs on the command line: {json}");
    let v: Json = serde_json::from_str(&json).unwrap();
    assert_eq!(v["apiKeyHelper"], "");
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "");
    assert_eq!(v["enabledPlugins"]["deploy@team"], false);
    assert_eq!(v["disableAllHooks"], true, "the chat's own settings stay: {json}");
    assert_eq!(v["permissions"]["ask"], serde_json::json!(["Edit", "Write"]));
    assert_eq!(sources, SOURCES, "the files still load — the rest of them applies");
}

/// Trusted (`y`), the files apply as they are — but a command too long to show was never on the card, so it's
/// never trusted: still held back (as `trust.rs` does for `.tarae.toml`).
#[test]
fn trusted_keeps_the_files_but_a_command_too_long_to_show() {
    let long = "x".repeat(crate::trust::SHOWN_MAX + 1);
    let text = format!(r#"{{"apiKeyHelper": "{long}", "awsAuthRefresh": "printf ok", "env": {{"A": "1"}}}}"#);
    let seen = Seen {
        scan: scan(&[file(PROJECT, &text)]),
        state: State::Trusted,
        file: PathBuf::from("/p"),
        base: Baseline::default(),
    };
    let v: Json = serde_json::from_str(&settings(crate::chat::CHAT_SETTINGS, &seen).0).unwrap();
    assert_eq!(v["apiKeyHelper"], "", "too long to show: held back even trusted");
    assert!(v.get("awsAuthRefresh").is_none() && v.get("env").is_none(), "the rest as written: {v}");
    let (q, _) = question(&seen);
    assert!(q.contains("too long to show, so never used") && !q.contains(&long), "{q}");
}

/// A file Claude Code may read some way tarae can't tell — not JSON, not an object, a key of another shape,
/// too big — can't be held back key by key: it's never loaded (its source left out of `--setting-sources`),
/// trusted or not, and the card says so.
#[test]
fn a_file_read_some_way_tarae_cant_tell_is_never_loaded() {
    for local in ["not json", "[1]", r#"{"env": ["A"]}"#, r#"{"apiKeyHelper": 7}"#] {
        let scan = scan(&[file(PROJECT, r#"{"model": "x"}"#), file(LOCAL, local)]).expect(local);
        assert_eq!(scan.unreadable.len(), 1, "{local}");
        for state in [State::Trusted, State::Ignored] {
            let seen = Seen {
                scan: Some(scan.clone()),
                state,
                file: PathBuf::from("/p"),
                base: Baseline::default(),
            };
            assert_eq!(settings(crate::chat::CHAT_SETTINGS, &seen).1, "user,project", "{local} {state:?}");
        }
    }
    let too_big = scan(&[(PROJECT, Some(Err("2000000 bytes — too big to read".into())))]).unwrap();
    let seen = Seen {
        scan: Some(too_big),
        state: State::Trusted,
        file: PathBuf::from("/p"),
        base: Baseline::default(),
    };
    assert_eq!(settings(crate::chat::CHAT_SETTINGS, &seen).1, "user,local");
    assert!(question(&seen).0.contains("too big to read — never used"));
}

/// An answer is about the values too, though the card shows a variable by name only: the same name with
/// another value (`ANTHROPIC_BASE_URL` pointed elsewhere) asks again; the same files read again don't.
#[test]
fn the_answer_is_about_values_the_card_doesnt_show() {
    let at = |url: &str| {
        scan(&[file(PROJECT, &format!(r#"{{"env": {{"ANTHROPIC_BASE_URL": "{url}"}}}}"#))]).unwrap().hash
    };
    assert_eq!(at("https://a.invalid"), at("https://a.invalid"));
    assert_ne!(at("https://a.invalid"), at("https://b.invalid"));
    // (the same key in the other file is another thing to answer)
    let in_local = scan(&[file(LOCAL, r#"{"env": {"ANTHROPIC_BASE_URL": "https://a.invalid"}}"#)]).unwrap();
    assert_ne!(in_local.hash, at("https://a.invalid"));
}

/// The project's files pick the names a hold-back works on, so a hostile `.claude/settings.json` naming your
/// variables (`DATABASE_URL`, `AWS_SESSION_TOKEN`) gets them emptied — never their value from tarae's
/// environment on claude's command line, where your other processes read it. Only `RESTORED` names (`PATH` …)
/// go back to tarae's value: claude needs them, and they aren't secret. A variable your own Claude settings set
/// too: that file isn't loaded, so yours applies — and stays off the command line.
#[test]
fn a_hostile_file_naming_your_variables_gets_them_empty_never_their_value() {
    let root = temp("baseline");
    let user = root.join("your-settings.json");
    std::fs::write(&user, r#"{"env": {"FROM_YOURS": "yours-zq7"}}"#).unwrap();
    // (stand-in values for tarae's environment — the test's own, nothing of the process's)
    let env = |n: &str| match n {
        "PATH" => Some("/usr/bin:/bin".to_string()),
        "DATABASE_URL" => Some("postgres://db.internal.zq7:5432/app".to_string()),
        "AWS_SESSION_TOKEN" => Some("aws-session-zq7".to_string()),
        _ => None,
    };
    let held = |project: &str| {
        std::fs::write(root.join(PROJECT), project).unwrap();
        let seen = Seen { state: State::Ignored, ..look(&root, None, Some(&user), |_| false, env) };
        settings(crate::chat::CHAT_SETTINGS, &seen)
    };
    let (json, sources) = held(
        r#"{"env": {"DATABASE_URL": "theirs", "AWS_SESSION_TOKEN": "theirs", "PATH": "/theirs/bin", "NEW_ONE": "x"}}"#,
    );
    let v: Json = serde_json::from_str(&json).unwrap();
    assert_eq!((&v["env"]["DATABASE_URL"], &v["env"]["AWS_SESSION_TOKEN"]), (&"".into(), &"".into()));
    assert!(!json.contains("db.internal.zq7") && !json.contains("aws-session-zq7"), "never yours: {json}");
    assert_eq!(v["env"]["PATH"], "/usr/bin:/bin", "a RESTORED name: tarae's value, not \"\"");
    assert_eq!(v["env"]["NEW_ONE"], "");
    assert!(!json.contains("theirs"), "{json}");
    assert_eq!(sources, SOURCES, "the files still load");
    // A variable your own settings set too: the project's file isn't loaded — yours applies, off argv
    let (json, sources) = held(r#"{"env": {"FROM_YOURS": "theirs"}}"#);
    assert!(!json.contains("yours-zq7") && !json.contains("FROM_YOURS"), "{json}");
    assert_eq!(sources, "user,local");
    std::fs::remove_dir_all(&root).ok();
}

/// A project's `permissions.allow` would let tools run unasked — `Read` anywhere, past `reads.rs` — in a folder
/// you once trusted in Claude Code (under `-p` in one never trusted it's unused; tarae can't tell which). Held
/// back, each rule becomes an `ask` rule beside the chat's own (an `ask` outranks an `allow` from any file), so
/// tarae answers it. An `additionalDirectories` folder can't be taken back by an override: its file isn't loaded.
#[test]
fn a_projects_allow_rules_ask_and_its_extra_folders_dont_load() {
    let allow = r#"{"permissions": {"allow": ["Read", "Bash(curl *)"]}}"#;
    let seen = |text: &str| Seen {
        scan: scan(&[file(PROJECT, text)]),
        state: State::Ignored,
        file: PathBuf::from("/p"),
        base: Baseline::default(),
    };
    let (json, sources) = settings(crate::chat::CHAT_SETTINGS, &seen(allow));
    let v: Json = serde_json::from_str(&json).unwrap();
    let ask = v["permissions"]["ask"].as_array().unwrap();
    for rule in ["Edit", "Write", "Read", "Bash(curl *)"] {
        assert!(ask.iter().any(|r| r == rule), "{rule} asks: {json}");
    }
    assert_eq!(v["permissions"]["deny"], serde_json::json!(["Skill"]), "the chat's own stay");
    assert_eq!(sources, SOURCES);
    let dirs = r#"{"permissions": {"additionalDirectories": ["/"]}}"#;
    assert_eq!(settings(crate::chat::CHAT_SETTINGS, &seen(dirs)).1, "user,local");
    assert!(question(&seen(allow)).0.contains("lets Read run unasked"));
}

/// Claude Code reloads an edited helper in a running session, so one a `git pull` adds would run before tarae
/// looks: every command key is held up front — set by the files or not — but one the files set and you
/// trusted, and one only you set (yours applies). Set by both and held back: back to yours — unless yours looks
/// like a secret, then their file isn't loaded and yours stays off the command line.
#[test]
fn every_command_key_is_held_up_front_but_trusted_or_yours() {
    let mut base = Baseline::default();
    base.commands.insert("awsAuthRefresh".into(), "aws sso login".into());
    let none = Seen { scan: None, state: State::Trusted, file: PathBuf::from("/p"), base: base.clone() };
    let (over, _) = overrides(&none);
    for key in ["apiKeyHelper", "awsCredentialExport", "gcpAuthRefresh", "otelHeadersHelper"] {
        assert_eq!(over[key], "", "{key}: held before any file sets it");
    }
    assert!(over.get("awsAuthRefresh").is_none(), "only yours: yours applies");
    // Theirs, trusted: theirs applies; the others still held
    let theirs = scan(&[file(PROJECT, r#"{"apiKeyHelper": "printf theirs"}"#)]);
    let trusted = Seen { scan: theirs.clone(), ..none.clone() };
    let (over, _) = overrides(&trusted);
    assert!(over.get("apiKeyHelper").is_none() && over["gcpAuthRefresh"] == "");
    // Theirs held back, yours too: back to yours
    base.commands.insert("apiKeyHelper".into(), "printf mine-zq7".into());
    let held = Seen { scan: theirs.clone(), state: State::Ignored, file: PathBuf::from("/p"), base };
    assert_eq!(overrides(&held).0["apiKeyHelper"], "printf mine-zq7");
    // …unless yours looks like a secret: off the command line, their file left out
    let mut secret = held.clone();
    secret.base.commands.insert("apiKeyHelper".into(), "fetch-key --api-key=fake-zq7".into());
    let (json, sources) = settings(crate::chat::CHAT_SETTINGS, &secret);
    assert!(!json.contains("fake-zq7") && !json.contains("apiKeyHelper"), "{json}");
    assert_eq!(sources, "user,local");
}

/// What tarae passes claude in `--settings`, pinned in `src/snapshots/chat_settings_*.json` — and what live
/// check L6 (`target/sb/apibill-L6.sh`) gives real claude, so it checks exactly this:
/// - `plain`: a chat with no project settings at all — every command key held up front as "";
/// - `held`: L6's stand-in project (a helper, `ANTHROPIC_BASE_URL`) held back (`n`), with nothing to go back to
///   (L6 runs claude without `ANTHROPIC_*`, and no settings of yours are read here);
/// - `trusted`: the same project trusted (`y`) — its helper applies, the other four still held.
///
/// `TARAE_BLESS=1 cargo test chat_settings_l6` rewrites them.
#[test]
fn chat_settings_l6_checks_are_what_tarae_passes() {
    let stand_in =
        r#"{"apiKeyHelper": "the stand-in's helper", "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:9"}}"#;
    let project = scan(&[file(PROJECT, stand_in)]);
    let at = |scan: Option<Scan>, state| {
        let base = baseline(scan.as_ref(), &[], |_| None);
        Seen { scan, state, file: PathBuf::from("/p"), base }
    };
    for (name, seen) in [
        ("plain", at(None, State::Trusted)),
        ("held", at(project.clone(), State::Ignored)),
        ("trusted", at(project.clone(), State::Trusted)),
    ] {
        let (json, sources) = settings(crate::chat::CHAT_SETTINGS, &seen);
        assert_eq!(sources, SOURCES, "{name}: every file loads");
        let path = format!("{}/src/snapshots/chat_settings_{name}.json", env!("CARGO_MANIFEST_DIR"));
        crate::settings::check_generated(&path, &format!("{json}\n"), "chat_settings_l6");
    }
}

/// A temp folder of the test's own.
fn temp(name: &str) -> PathBuf {
    let base = crate::scratch::Scratch::new(&format!("claudetrust-{name}")).into_path();
    std::fs::create_dir_all(base.join(".claude")).unwrap();
    base
}

/// The local file is yours when git says it's untracked and ignored (`config::personal`): not asked about. An
/// answer kept in the store (`trust.rs`'s) is found for the same content, and only for it.
#[test]
fn your_own_local_file_isnt_asked_about_and_answers_are_kept() {
    let root = temp("look");
    std::fs::write(root.join(LOCAL), r#"{"apiKeyHelper": "printf mine"}"#).unwrap();
    assert_eq!(look(&root, None, None, |_| true, |_| None).scan, None, "yours: nothing to ask");
    let seen = look(&root, None, None, |_| false, |_| None);
    assert_eq!(seen.state, State::Ask, "the repository's: asks");
    assert_eq!(seen.file, root.join(PROJECT), "kept under the project file's place");
    let store = root.join("trust.json");
    crate::trust::record(store.clone(), seen.file.clone(), seen.digest().unwrap(), false).unwrap();
    assert_eq!(look(&root, Some(&store), None, |_| false, |_| None).state, State::Ignored);
    std::fs::write(root.join(LOCAL), r#"{"apiKeyHelper": "printf changed"}"#).unwrap();
    assert_eq!(look(&root, Some(&store), None, |_| false, |_| None).state, State::Ask, "changed: asks again");
    std::fs::remove_dir_all(&root).ok();
}

// ── The chat ────────────────────────────────────────────────────────────────

/// A stand-in claude: writes its arguments, one a line, to `argv` and appends what it's sent to `stdin`, in the
/// folder it's started in — its session's root, the test's temp folder — nothing else runs. It never answers, so
/// a message sent leaves its turn under way. (`exec`: one process, as claude is — a `cat` left behind by a killed
/// `sh` would keep its output open, and its exit would never come.)
fn stand_in(ed: &mut Editor) {
    let script = "printf '%s\\n' \"$0\" \"$@\" > argv; exec cat >> stdin".to_string();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), script];
}

/// A project in its own temp folder whose `.claude/settings.json` says `settings`, its chat open with the gate
/// on and the stand-in claude, the card up — and, with `store`, the answers kept in the folder's `trust.json`.
fn asked(name: &str, settings: &str, store: bool) -> (Editor, PathBuf) {
    let root = temp(name);
    std::fs::write(root.join(PROJECT), settings).unwrap();
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    ed.claude_settings_gate = true;
    if store {
        ed.trust_path = Some(root.join("trust.json"));
    }
    stand_in(&mut ed);
    crate::chat::open(&mut ed);
    until(&mut ed, "the card", |ed| card(ed).is_some());
    (ed, root)
}

fn pid(ed: &Editor) -> Option<u32> {
    ed.session.chat.as_ref().and_then(|c| c.pid())
}

fn noted(ed: &Editor, text: &str) -> bool {
    ed.session.chat.as_ref().is_some_and(|c| c.msgs.iter().any(|m| m.text.contains(text)))
}

/// A change while claude runs that needs other overrides but no card — the file now says what was answered
/// before (kept in the store) — restarts claude with what it now needs, said in the chat, no card.
#[test]
fn a_change_answered_before_restarts_claude_without_a_card() {
    let (mut ed, root) = asked("restart", r#"{"env": {"FIRST_ZQ7": "1"}}"#, true);
    ed.offer_dismiss();
    until(&mut ed, "the held-back start", |_| arg_after(&root, "--settings").is_some());
    let first = pid(&ed);
    // The next version, held back before (in this repository's store)
    let next = r#"{"env": {"SECOND_ZQ7": "1"}}"#;
    let hash = scan(&[file(PROJECT, next)]).unwrap().hash;
    crate::trust::record(root.join("trust.json"), root.join(PROJECT), hash, false).unwrap();
    std::fs::remove_file(root.join("argv")).unwrap();
    std::fs::write(root.join(PROJECT), next).unwrap();
    until(&mut ed, "the restart with the new overrides", |ed| {
        running(ed) && pid(ed) != first && arg_after(&root, "--settings").is_some()
    });
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert_eq!(settings["env"]["SECOND_ZQ7"], "", "{settings}");
    assert!(settings["env"].get("FIRST_ZQ7").is_none(), "{settings}");
    assert!(card(&ed).is_none(), "answered before: no card");
    assert!(noted(&ed, "Claude started again with them as answered"), "{}", state(&ed));
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// The case the window is about: no `.claude/` when the chat opens (claude starts as always), then a pull makes
/// `.claude/settings.json` while claude runs — the watch sees the new file (through its folder not there yet),
/// claude starts again held back, and the card asks.
#[test]
fn settings_made_while_claude_runs_restart_it_held_back_and_ask() {
    let root = temp("made-later");
    std::fs::remove_dir_all(root.join(".claude")).unwrap();
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    ed.claude_settings_gate = true;
    stand_in(&mut ed);
    crate::chat::open(&mut ed);
    until(&mut ed, "the plain start", |ed| running(ed) && arg_after(&root, "--settings").is_some());
    assert!(card(&ed).is_none());
    let first = pid(&ed);
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(root.join(PROJECT), r#"{"env": {"MADE_ZQ7": "1"}}"#).unwrap();
    until(&mut ed, "the card for the new file, claude again", |ed| {
        card(ed).is_some() && running(ed) && pid(ed) != first
    });
    until(&mut ed, "its arguments written", |_| {
        arg_after(&root, "--settings").is_some_and(|s| s.contains("MADE_ZQ7"))
    });
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert_eq!(settings["env"]["MADE_ZQ7"], "", "held back: {settings}");
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// `y` with claude running held back and idle: it starts again at once with the files as they are, the
/// conversation going on (`--resume`), said in the chat.
#[test]
fn a_yes_restarts_claude_at_idle_with_resume() {
    let (mut ed, root) = asked("yes-idle", r#"{"env": {"THIRD_ZQ7": "1"}}"#, false);
    until(&mut ed, "the held-back start", |ed| running(ed) && arg_after(&root, "--settings").is_some());
    let first = pid(&ed);
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert_eq!(settings["env"]["THIRD_ZQ7"], "", "held back while the card waits: {settings}");
    // (a conversation going, as claude's `init` names it)
    ed.session.chat.as_mut().unwrap().conversation = Some("conv-zq7".into());
    std::fs::remove_file(root.join("argv")).unwrap();
    ed.offer_accept();
    until(&mut ed, "the trusted start", |ed| running(ed) && pid(ed) != first);
    until(&mut ed, "its arguments written", |_| arg_after(&root, "--settings").is_some());
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert!(settings.get("env").is_none(), "trusted: theirs applies — {settings}");
    assert_eq!(arg_after(&root, "--resume").as_deref(), Some("conv-zq7"), "the same conversation");
    assert!(noted(&ed, "Trusted — Claude starts again with these settings"), "{}", state(&ed));
    assert!(card(&ed).is_none());
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// Trusted and mid-turn, the files change: claude is stopped at once, not at the turn's end, and starts again
/// held back on the same conversation, the card up again — its exit ending the turn unsaid (the chat said why).
#[test]
fn a_change_mid_chat_restarts_it_held_back_at_once() {
    let (mut ed, root) = asked("change", r#"{"env": {"FOURTH_ZQ7": "1"}}"#, false);
    until(&mut ed, "the held-back start", |ed| running(ed) && arg_after(&root, "--settings").is_some());
    ed.session.chat.as_mut().unwrap().conversation = Some("conv-zq7".into());
    let first = pid(&ed);
    ed.offer_accept();
    until(&mut ed, "the trusted start", |ed| running(ed) && pid(ed) != first);
    let trusted = pid(&ed);
    // A turn under way (the stand-in never answers)
    ed.session.chat.as_mut().unwrap().input = "mid-turn from the test".into();
    crate::chat::send(&mut ed);
    until(&mut ed, "the message", |_| {
        std::fs::read_to_string(root.join("stdin")).is_ok_and(|s| s.contains("mid-turn from the test"))
    });
    assert!(ed.session.chat.as_ref().unwrap().busy(), "{}", state(&ed));
    std::fs::remove_file(root.join("argv")).unwrap();
    std::fs::write(root.join(PROJECT), r#"{"env": {"FIFTH_ZQ7": "1"}}"#).unwrap();
    until(&mut ed, "held back again, the card up", |ed| {
        card(ed).is_some() && running(ed) && pid(ed) != trusted && arg_after(&root, "--settings").is_some()
    });
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert_eq!(settings["env"]["FIFTH_ZQ7"], "", "held back: {settings}");
    assert_eq!(arg_after(&root, "--resume").as_deref(), Some("conv-zq7"), "the same conversation");
    assert!(!ed.session.chat.as_ref().unwrap().busy(), "the cut turn is over");
    assert!(noted(&ed, "Claude started again with them held back"), "{}", state(&ed));
    assert!(!noted(&ed, "claude exited"), "said once, as why: {}", state(&ed));
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// The card is its session's: switching to another session parks it there (a key in the other session never
/// answers it — #159's kind), it's up again on coming back, and answered there it starts that session's claude.
/// Another session's chat reading settings that need no card takes away only its own card, never this one.
#[test]
fn the_card_stays_with_its_session() {
    let (mut ed, root) = asked("session-a", r#"{"apiKeyHelper": "printf theirs-zq7"}"#, false);
    let other = temp("session-b");
    let b = ed.session_add(other.clone());
    // (its chat opened behind: its read lands in it — nothing to ask there)
    ed.with_session(b, crate::chat::open);
    until(&mut ed, "the other chat's read", |ed| {
        ed.sessions.iter().any(|s| s.id == b && s.chat.as_ref().is_some_and(|c| c.settings.seen.is_some()))
    });
    assert!(card(&ed).is_some(), "the other session's read leaves this card alone");
    assert!(ed.session_switch(b));
    assert!(card(&ed).is_none(), "not up in the other session");
    let a = ed.sessions.iter().find(|s| s.root == root).map(|s| s.id).unwrap();
    assert!(ed.session_switch(a));
    assert!(card(&ed).is_some(), "up again with its session");
    ed.offer_dismiss();
    until(&mut ed, "this session's claude", |_| arg_after(&root, "--settings").is_some());
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&other).ok();
}

/// Until `done`, handling events as they come (5 s at most).
fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(ed) {
        assert!(Instant::now() < end, "timed out waiting for {what} — {}", state(ed));
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// What a timed-out wait says: the cards, the chat's claude, its gate, its last lines.
fn state(ed: &Editor) -> String {
    let cards: Vec<&str> = ed.offers.iter().map(|o| o.title.as_str()).collect();
    let chat = ed.session.chat.as_ref().map(|c| {
        let last: Vec<&str> = c.msgs.iter().rev().take(3).map(|m| m.text.as_str()).collect();
        format!(
            "running {} · waiting {} · queued {:?} · seen {:?} · last {last:?}",
            c.running(),
            c.settings.waiting(),
            c.queued,
            c.settings.seen.as_ref().map(|s| s.state)
        )
    });
    let toasts: Vec<&str> = ed.toasts.iter().map(|t| t.text.as_str()).collect();
    format!("cards {cards:?} · chat {chat:?} · toasts {toasts:?}")
}

fn running(ed: &Editor) -> bool {
    ed.session.chat.as_ref().is_some_and(|c| c.running())
}

fn card(ed: &Editor) -> Option<Digest> {
    ed.offers.iter().find_map(|o| match o.what {
        crate::offer::What::ClaudeSettings { hash } => Some(hash),
        _ => None,
    })
}

/// The arguments the stand-in was started with — the value after `flag`.
fn arg_after(root: &Path, flag: &str) -> Option<String> {
    let argv = std::fs::read_to_string(root.join("argv")).ok()?;
    let mut it = argv.lines();
    it.find(|l| *l == flag)?;
    it.next().map(String::from)
}

/// The chat's claude doesn't start before the repository's settings are read, nor waits for their card: it starts
/// held back as `n` leaves it — them overridden, no value of theirs on its command line — the card up beside the
/// chat, which keeps the keys, its first line saying so; a message typed reaches claude before any answer. `n`
/// keeps the same claude (it runs held back already), and `:claude-settings` puts the card up again.
#[test]
fn a_chat_starts_held_back_while_its_card_waits() {
    let root = temp("chat");
    let settings_file = root.join(PROJECT);
    std::fs::write(
        &settings_file,
        r#"{"apiKeyHelper": "printf theirs-zq7-key", "env": {"ANTHROPIC_BASE_URL": "https://theirs-zq7.invalid"}}"#,
    )
    .unwrap();
    // (`theirs-zq7`: in nothing of tarae's own — its system prompt says "stand-in")
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    ed.claude_settings_gate = true;
    // (tarae's environment as the test gives it — with the variable, as a real one may have)
    let base_url = ("ANTHROPIC_BASE_URL".to_string(), "https://api.anthropic.com".to_string());
    ed.claude_env = Some(std::collections::HashMap::from([base_url]));
    stand_in(&mut ed);
    crate::chat::open(&mut ed);
    assert!(!running(&ed), "nothing starts before the settings are read");
    until(&mut ed, "the card, and claude beside it", |ed| card(ed).is_some() && running(ed));
    until(&mut ed, "its arguments written", |_| arg_after(&root, "--settings").is_some());
    let q = &ed.offers.iter().find(|o| o.title == TITLE).unwrap().question;
    assert!(q.starts_with("Claude started with these held back"), "{q}");
    assert!(
        q.contains("apiKeyHelper = printf theirs-zq7-key") && q.contains("env ANTHROPIC_BASE_URL"),
        "{q}"
    );
    assert!(!q.contains("theirs-zq7.invalid"), "a variable's value isn't shown: {q}");
    let c = ed.session.chat.as_ref().unwrap();
    assert!(c.focused && !ed.card_has_keys(), "the chat keeps its keys beside the card");
    // Typed before any answer: it goes to claude
    ed.session.chat.as_mut().unwrap().input = "hello from the test".into();
    crate::chat::send(&mut ed);
    until(&mut ed, "the message", |_| {
        std::fs::read_to_string(root.join("stdin")).is_ok_and(|s| s.contains("hello from the test"))
    });
    assert!(card(&ed).is_some(), "still unanswered");
    // Held back: started with each key overridden by name
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert_eq!(settings["apiKeyHelper"], "");
    // (not on `RESTORED`: empty, whatever tarae's environment holds — the test gives one that has it)
    assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "");
    assert_eq!(settings["disableAllHooks"], true);
    // (f22 i: claude's transcript cleanup kept off where it loads the user source — on either system)
    assert_eq!(settings["cleanupPeriodDays"].as_u64(), Some(36_500));
    let argv = std::fs::read_to_string(root.join("argv")).unwrap();
    let theirs: Vec<&str> = argv.lines().filter(|l| l.contains("theirs-zq7")).collect();
    assert!(theirs.is_empty(), "names only, no value of theirs: {theirs:?}");
    // `n`: held back already — the same claude goes on
    let first = pid(&ed);
    ed.offer_dismiss();
    assert!(running(&ed) && pid(&ed) == first, "the same claude: {}", state(&ed));
    // `:claude-settings`: the card again, to look again
    assert!(card(&ed).is_none());
    crate::typed::execute(&mut ed, "claude-settings").unwrap();
    assert!(card(&ed).is_some(), "the card again");
    // Short beside the work: what holding back does is `v`'s, after the card's text
    let q = ed.offers.iter().find(|o| o.title == TITLE).unwrap().question.clone();
    assert!(!q.contains("Held back, Claude runs") && !q.contains("no trust dialog"), "{q}");
    ed.offer_view();
    let all = ed.doc().text.to_string();
    assert!(all.contains(&q), "the card's text: {all}");
    assert!(
        all.contains("no trust dialog") && all.contains("Held back, Claude runs with these emptied"),
        "{all}"
    );
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// Under `-p` a project's `.mcp.json` servers connect unasked: the chat's claude is always told to use only
/// tarae's own — even when `llm.args` leaves the flag out.
#[test]
fn the_chat_asks_for_only_tarae_s_own_mcp_servers() {
    let root = temp("mcp");
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    stand_in(&mut ed);
    assert!(!ed.config.llm.args.iter().any(|a| a == "--strict-mcp-config"), "the args leave it out");
    crate::chat::open(&mut ed);
    until(&mut ed, "its arguments written", |_| {
        std::fs::read_to_string(root.join("argv")).is_ok_and(|a| a.contains("--settings"))
    });
    let argv = std::fs::read_to_string(root.join("argv")).unwrap();
    assert!(argv.lines().any(|l| l == "--strict-mcp-config"), "{argv}");
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// A project skill's `allowed-tools` pre-approves tools with no trust asked, even under `-p` (Claude Code's
/// skills docs). Skills run through the `Skill` tool: the chat's `--tools` leaves it out, and its `--settings`
/// denies it as well — so no skill runs in the chat to grant anything.
#[test]
fn a_project_skill_cant_pre_approve_tools_in_the_chat() {
    let root = temp("skill");
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    stand_in(&mut ed);
    crate::chat::open(&mut ed);
    until(&mut ed, "its arguments written", |_| arg_after(&root, "--settings").is_some());
    let tools = arg_after(&root, "--tools").unwrap();
    assert!(!tools.split(',').any(|t| t == "Skill"), "{tools}");
    let settings: Json = serde_json::from_str(&arg_after(&root, "--settings").unwrap()).unwrap();
    assert!(settings["permissions"]["deny"].as_array().is_some_and(|d| d.iter().any(|t| t == "Skill")));
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}
