use super::*;
use crate::llm::Env;
use std::os::unix::fs::PermissionsExt;

/// A home of the test's own in the temp folder (real), empty.
fn home(name: &str) -> PathBuf {
    let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let h = tmp.join(format!("tarae-claudehome-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn env_of(home: &Path) -> Env {
    Env::from([("HOME".to_string(), home.to_string_lossy().into_owned())])
}

fn place(home: &Path) -> Place {
    Place::of(Os::Linux, &var_in(Some(&env_of(home)))).unwrap().expect("Linux has one")
}

/// Claude's folder is made 0700 in tarae's state, and `~/.local/state/claude` beside it — each through no link: a
/// link planted where one goes is refused, never gone through nor removed, and claude doesn't start.
#[test]
fn claudes_folder_is_made_through_no_link() {
    let h = home("made");
    let p = place(&h);
    assert_eq!(p.dir(), h.join(".local/state/tarae/claude"));
    assert_eq!(p.locks, [h.join(".local/state/claude")]);
    p.made().unwrap();
    let mode = std::fs::symlink_metadata(p.dir()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "its own: {mode:o}");
    assert!(h.join(".local/state/claude").is_dir(), "claude's locks' folder");
    p.made().unwrap();
    // A link where the folder goes — to a folder elsewhere
    for (n, at) in [("link", ".local/state/tarae/claude"), ("locks-link", ".local/state/claude")] {
        let h = home(n);
        let elsewhere = h.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(h.join(at).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, h.join(at)).unwrap();
        let e = place(&h).made().expect_err("a link is refused");
        assert!(e.contains(at) && e.contains("a link"), "{e}");
        assert!(std::fs::symlink_metadata(h.join(at)).unwrap().file_type().is_symlink(), "left as it is");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing made through it");
        // (the worker's step keeps it; the spawn says it once, and the next start makes it again)
        let given = env_of(&h);
        assert!(make(Os::Linux, Some(&given)).is_err() && ready(Os::Linux, Some(&given)));
        let e = for_claude(Os::Linux, Some(&given), &[]).expect_err("claude doesn't start");
        assert!(e.contains("a link"), "{e}");
        assert!(!ready(Os::Linux, Some(&given)), "made again before the next claude");
        std::fs::remove_dir_all(&h).ok();
    }
    std::fs::remove_dir_all(&h).ok();
}

/// On Linux every claude tarae starts gets `CLAUDE_CONFIG_DIR` = tarae's folder and no user settings source, in
/// any form it's given; on macOS neither. Where the state folder is moved (`XDG_STATE_HOME`) the folder moves with
/// it, as tarae's state does; with no absolute place claude doesn't start.
#[test]
fn every_claude_on_linux_has_taraes_folder_and_no_user_settings() {
    let h = home("sources");
    let given = env_of(&h);
    let args: Vec<String> =
        ["-p", "--setting-sources", "user,project,local", "--setting-sources=user", "--x", "user"]
            .map(String::from)
            .to_vec();
    assert_eq!(for_claude(Os::Mac, Some(&given), &args).unwrap(), (args.clone(), None));
    assert_eq!(std::fs::read_dir(&h).unwrap().count(), 0, "nothing made on macOS");
    assert!(ready(Os::Mac, Some(&given)) && make(Os::Mac, Some(&given)).is_ok());
    assert_eq!(std::fs::read_dir(&h).unwrap().count(), 0, "nothing to make on macOS");
    make(Os::Linux, Some(&given)).unwrap();
    let (linux, folder) = for_claude(Os::Linux, Some(&given), &args).unwrap();
    assert_eq!(linux, ["-p", "--setting-sources", "project,local", "--setting-sources=", "--x", "user"]);
    assert_eq!(folder, Some(h.join(".local/state/tarae/claude")));
    // (moved state)
    let mut moved = given.clone();
    moved.insert("XDG_STATE_HOME".into(), h.join("st").to_string_lossy().into_owned());
    let p = Place::of(Os::Linux, &var_in(Some(&moved))).unwrap().unwrap();
    assert_eq!(p.dir(), h.join("st/tarae/claude"));
    assert_eq!(p.locks, [h.join(".local/state/claude"), h.join("st/claude")]);
    let relative = Env::from([("HOME".to_string(), "rel".to_string())]);
    assert!(Place::of(Os::Linux, &var_in(Some(&relative))).is_err(), "no absolute place");
    // Your own Claude settings aren't a hold-back's baseline where they aren't loaded
    assert_eq!(crate::claudetrust::user_settings(Os::Linux), None);
    // Through `llm::spawn`, the one way every claude starts (the chat, the reviewer, an ask, a draft)
    let cfg = crate::llm::LlmConfig {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "printf '%s|%s' \"$CLAUDE_CONFIG_DIR\" \"$*\"".into(),
            "x".into(),
            "--setting-sources".into(),
            "user,project,local".into(),
        ],
        ..Default::default()
    };
    let mut theirs = given.clone();
    theirs.insert(VAR.into(), "/yours".into());
    let sub = crate::billing::Credential::Subscription;
    let child = crate::llm::spawn(&cfg, &[], &h, Some(&theirs), &sub, None).unwrap();
    let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    match Os::HOST {
        Os::Linux => {
            assert_eq!(out, format!("{}|--setting-sources project,local", folder.unwrap().display()))
        }
        Os::Mac => assert_eq!(out, "/yours|--setting-sources user,project,local"),
    }
    std::fs::remove_dir_all(&h).ok();
}

/// A spawn does no folder work: it only reads what a worker's step made (`make`) — where none did, claude doesn't
/// start and nothing is made, through `llm::spawn` too; once a worker made it, it starts.
#[test]
fn a_spawn_only_reads_what_a_worker_made() {
    let h = home("reads");
    let given = env_of(&h);
    assert!(!ready(Os::Linux, Some(&given)), "not made yet");
    let e = for_claude(Os::Linux, Some(&given), &[]).expect_err("not made: no start");
    assert!(e.contains("isn't made yet"), "{e}");
    let cfg = crate::llm::LlmConfig {
        command: "sh".into(),
        args: vec!["-c".into(), "true".into()],
        ..Default::default()
    };
    let sub = crate::billing::Credential::Subscription;
    let spawned = crate::llm::spawn(&cfg, &[], &h, Some(&given), &sub, None);
    match Os::HOST {
        Os::Linux => assert!(matches!(spawned, Err(crate::llm::SpawnError::Folder(_))), "{spawned:?}"),
        Os::Mac => assert!(spawned.is_ok(), "macOS has none to wait for"),
    }
    assert_eq!(std::fs::read_dir(&h).unwrap().count(), 0, "nothing made by a spawn");
    // (the worker's step)
    make(Os::Linux, Some(&given)).unwrap();
    assert!(ready(Os::Linux, Some(&given)) && place(&h).dir().is_dir());
    let (_, folder) = for_claude(Os::Linux, Some(&given), &[]).unwrap();
    assert_eq!(folder, Some(place(&h).dir()));
    std::fs::remove_dir_all(&h).ok();
}

/// `:claude-login` is claude's own `auth login` (`claude --help`), by `llm.command`, with tarae's folder on Linux.
#[test]
fn claude_login_is_claudes_own_login_in_taraes_folder() {
    let h = home("login");
    let p = place(&h);
    let (argv, env) = login_command("/opt/claude", Some(&p));
    assert_eq!(argv, ["/opt/claude", "auth", "login"]);
    assert_eq!(env, [(VAR.to_string(), p.dir().to_string_lossy().into_owned())]);
    assert_eq!(login_command("claude", None).1, [], "macOS: your claude's own login");
    // (a stand-in for claude in the terminal panel)
    let mut ed = Editor::new(Default::default());
    ed.config.llm.command = "/bin/echo".into();
    ed.claude_env = Some(env_of(&h));
    crate::typed::execute(&mut ed, "claude-login").unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ed.status.as_ref().is_some_and(|s| s.0.contains("login")) {
        assert!(std::time::Instant::now() < end, "said: {:?}", ed.status);
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
    assert!(ed.status.as_ref().unwrap().0.contains("terminal panel"), "{:?}", ed.status);
    assert_eq!(p.dir().is_dir(), Os::HOST == Os::Linux, "made first on Linux only");
    drop(ed);
    std::fs::remove_dir_all(&h).ok();
}

/// A claude with no login on Linux — Claude Code's words under `-p` — makes the chat say `:claude-login`, once since
/// tarae started; not on macOS (its login is your claude's), not for another failure.
#[test]
fn no_login_says_claude_login_once() {
    let h = home("hint");
    let p = place(&h);
    let mut told = false;
    assert_eq!(login_hint("Overloaded", Some(&p), &mut told), None);
    assert_eq!(login_hint("Not logged in · Please run /login", None, &mut told), None, "macOS");
    let said = login_hint("Not logged in · Please run /login", Some(&p), &mut told).expect("said");
    assert!(said.contains(":claude-login"), "{said}");
    assert_eq!(login_hint("Failed to authenticate. API Error: 401", Some(&p), &mut told), None, "once");
    std::fs::remove_dir_all(&h).ok();
}
