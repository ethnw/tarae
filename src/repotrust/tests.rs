use super::*;
use crate::trust::{State, Store};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// A checkout of the test's own where all three wait: its `.tarae.toml` sets what runs commands, its
/// `.claude/settings.json` a helper command, its git config `core.fsmonitor` (`fsmonitor` = what it says) — each
/// a marker in the test's folder, never run (held until trusted). The chat's claude is a stand-in that writes what
/// it's sent to `stdin` there. The editor up with the project's card; the rest not read yet.
fn checkout(name: &str, fsmonitor: Option<&str>) -> (Editor, PathBuf) {
    let dir = crate::scratch::Scratch::new(&format!("repotrust-{name}")).into_path();
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(dir.join("user.gitconfig"), "").unwrap();
    crate::gitrepo::global_config_for(&dir, &dir.join("user.gitconfig"));
    std::fs::write(root.join(".tarae.toml"), crate::config::tests::RISKY_PROJECT).unwrap();
    std::fs::write(root.join(".claude/settings.json"), r#"{"apiKeyHelper": "printf marker-key"}"#).unwrap();
    git(&dir, &["init", "-q"]);
    let monitor = dir.join("fsmonitor").display().to_string();
    git(&dir, &["config", "core.fsmonitor", fsmonitor.unwrap_or(&monitor)]);
    let root = std::fs::canonicalize(&root).unwrap();
    let (mut c, _) = crate::config::load_from(None, Some(root.join(".tarae.toml")), &Store::default());
    // (the stand-in in the config either answer gives too)
    let script = "printf '%s\\n' \"$0\" \"$@\" > argv; exec cat >> stdin".to_string();
    let p = c.project.as_mut().unwrap();
    for llm in [&mut c.editor.llm, &mut p.with.0.llm, &mut p.without.0.llm] {
        llm.command = "sh".into();
        llm.args = vec!["-c".into(), script.clone()];
    }
    let mut ed = Editor::with_root(c, root.clone());
    ed.trust_path = Some(dir.join("trust.json"));
    ed.claude_settings_gate = true;
    (ed, root)
}

/// Plain git in the test's repository (`dir`/repo), with its own user config.
fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir.join("repo"))
        .env("GIT_CONFIG_GLOBAL", dir.join("user.gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}");
}

fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(ed) {
        let cards: Vec<&str> = ed.offers.iter().map(|o| o.kind()).collect();
        assert!(Instant::now() < end, "timed out waiting for {what} — cards {cards:?}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

fn kinds(ed: &Editor) -> Vec<&'static str> {
    ed.offers.iter().map(|o| o.kind()).collect()
}

fn parts(o: &Offer) -> Vec<&'static str> {
    match &o.what {
        What::RepoTrust { parts, .. } => parts.iter().map(|p| p.kind()).collect(),
        _ => Vec::new(),
    }
}

fn claude_card(ed: &Editor) -> bool {
    ed.offers.iter().any(|o| match &o.what {
        What::ClaudeSettings { .. } => true,
        What::RepoTrust { parts, .. } => parts.iter().any(|p| matches!(p.what, What::ClaudeSettings { .. })),
        _ => false,
    })
}

/// The chat opened, its settings read (their card joins), then the repository resolved as git's watch does (its
/// card joins): one card.
fn all_three(ed: &mut Editor, root: &Path) {
    crate::chat::open(ed);
    until(ed, "Claude's part", claude_card);
    let r = crate::gitrepo::repo(root).unwrap();
    assert!(ed.git_trust_check(&r), "git's part asked");
}

fn kept(ed: &Editor, file: &Path, hash: crate::trust::Digest) -> State {
    Store::read(ed.trust_path.as_deref()).state(file, hash)
}

/// The three found one after another, each on its own worker: one card for the repository, each part under its
/// heading with the lines its own card shows, saying faintly which came last; Claude's level says its title only.
/// The chat's claude started meanwhile, held back, and a message reaches it while the card waits.
#[test]
fn the_three_waiting_together_are_one_card_and_the_chat_doesnt_wait() {
    let (mut ed, root) = checkout("one", None);
    assert_eq!(kinds(&ed), ["project-trust"], "tarae's part up at once");
    all_three(&mut ed, &root);
    assert_eq!(kinds(&ed), ["repo-trust"], "one card");
    let o = &ed.offers[0];
    assert_eq!(parts(o), ["project-trust", "claude-settings", "git-trust"]);
    assert_eq!((o.title.as_str(), o.rev), (TITLE, 2), "two parts joined it");
    let lines: Vec<&str> = o.question.split('\n').collect();
    assert!(
        lines[0].starts_with("repo's settings run commands as you") && lines[0].ends_with("· git joined")
    );
    assert!(lines.contains(&"tarae · .tarae.toml"), "{lines:?}");
    assert!(lines.contains(&"  editor.restart-build = \"touch marker\""), "{lines:?}");
    assert!(lines.contains(&"Claude · it started with these held back"), "{lines:?}");
    assert!(lines.contains(&"  .claude/settings.json: apiKeyHelper = printf marker-key"), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("git · .git/config — they run on every status")), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("  core.fsmonitor = ")), "{lines:?}");
    // (every part's lines decided, the first line and the end faint)
    let decided = o.decided_lines();
    let monitor = lines.iter().position(|l| l.starts_with("  core.fsmonitor = ")).unwrap();
    assert!(decided.start == 2 && decided.contains(&monitor) && lines[decided.end].is_empty(), "{lines:?}");
    assert_eq!(o.answers(false), [('y', "Trust"), ('v', "Details"), ('n', "Hold back")]);
    assert!(o.weighty() && o.goes_with_session(), "settled before y; with its session (Claude's part)");
    // Claude sees it by its title
    let s = ed.ui_state_claude();
    let card = s.cards.iter().find(|c| c.kind == "repo-trust").unwrap();
    assert_eq!(card.asks, TITLE);
    assert!(!s.text_claude(false).contains("marker"), "none of its lines");
    // The chat didn't wait: its claude runs held back, and gets what's typed
    until(&mut ed, "claude", |ed| ed.session.chat.as_ref().is_some_and(|c| c.running()));
    ed.session.chat.as_mut().unwrap().input = "hello while it waits".into();
    crate::chat::send(&mut ed);
    until(&mut ed, "the message", |_| {
        std::fs::read_to_string(root.join("stdin")).is_ok_and(|s| s.contains("hello while it waits"))
    });
    assert_eq!(kinds(&ed), ["repo-trust"], "still unanswered");
    // `v`: each part's own card whole, and what holding Claude's back does
    ed.offer_view();
    let all = ed.doc().text.to_string();
    for title in [crate::trust::TITLE, crate::claudetrust::TITLE, crate::gittrust::TITLE] {
        assert!(all.contains(&format!("── {title}")), "{all}");
    }
    assert!(all.contains("Held back, Claude runs with these emptied"), "{all}");
}

/// `y` trusts each part, each kept in its own store's entry; a later change to one asks about that part alone, on
/// its own card as ever.
#[test]
fn yes_trusts_each_part_in_its_own_store_and_a_change_asks_its_part_alone() {
    let (mut ed, root) = checkout("yes", None);
    all_three(&mut ed, &root);
    let What::RepoTrust { parts, .. } = ed.offers[0].what.clone() else { panic!("{:?}", kinds(&ed)) };
    ed.offer_accept();
    assert!(ed.offers.is_empty());
    let said = ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ");
    assert!(said.contains("Trusted — this repository's settings apply: tarae's "), "{said}");
    assert!(said.contains("Claude's 1, git's 1"), "{said}");
    assert_eq!(ed.config.restart_build, "touch marker", "tarae's part applies");
    let seen = ed.session.chat.as_ref().and_then(|c| c.settings.seen.clone()).unwrap();
    assert_eq!(seen.state, State::Trusted);
    for p in &parts {
        let (file, hash) = match &p.what {
            What::ProjectTrust { hash } => (root.join(".tarae.toml"), *hash),
            What::ClaudeSettings { hash } => (seen.file.clone(), *hash),
            What::GitTrust(a) => {
                assert!(crate::gitrepo::answered_since(&a.config, a.digest));
                (a.config.clone(), a.digest)
            }
            _ => unreachable!(),
        };
        until(&mut ed, "each answer kept", |ed| kept(ed, &file, hash) == State::Trusted);
    }
    // Claude's settings changed: its own card alone, word for word as ever
    std::fs::write(root.join(".claude/settings.json"), r#"{"apiKeyHelper": "printf other-marker"}"#).unwrap();
    until(&mut ed, "Claude's card again", claude_card);
    assert_eq!(kinds(&ed), ["claude-settings"], "that part alone");
    let seen = ed.session.chat.as_ref().and_then(|c| c.settings.seen.clone()).unwrap();
    assert_eq!(ed.offers[0].question, crate::claudetrust::question(&seen).0);
    assert_eq!(ed.offers[0].title, crate::claudetrust::TITLE);
    // tarae's changed (Claude's answered): its own card alone too
    ed.offer_dismiss();
    let more = crate::config::tests::RISKY_PROJECT.replace("touch marker", "touch marker2");
    std::fs::write(root.join(".tarae.toml"), more).unwrap();
    let (c, _) = crate::config::load_from(None, Some(root.join(".tarae.toml")), &Store::default());
    ed.project_trust_set(c.project);
    assert_eq!(kinds(&ed), ["project-trust"]);
    assert_eq!(ed.offers[0].title, crate::trust::TITLE);
}

/// Piece E: `:granted` lists each part trusted — for every checkout of the repository — and `d` takes each back:
/// what it let run follows at once (tarae's settings held back, the chat's claude started again with Claude's held
/// back, tarae's git holding the repository's commands), and the next look asks (the store has no answer).
#[test]
fn granted_takes_each_part_back_and_what_it_let_run_follows() {
    use crate::granted::Taken;
    use crate::trust::Kind;
    let (mut ed, root) = checkout("granted", None);
    all_three(&mut ed, &root);
    let What::RepoTrust { parts, .. } = ed.offers[0].what.clone() else { panic!("{:?}", kinds(&ed)) };
    ed.offer_accept();
    let seen = ed.session.chat.as_ref().and_then(|c| c.settings.seen.clone()).unwrap();
    let kept_at: Vec<(PathBuf, crate::trust::Digest)> = parts
        .iter()
        .map(|p| match &p.what {
            What::ProjectTrust { hash } => (root.join(".tarae.toml"), *hash),
            What::ClaudeSettings { hash } => (seen.file.clone(), *hash),
            What::GitTrust(a) => (a.config.clone(), a.digest),
            _ => unreachable!(),
        })
        .collect();
    for (file, hash) in &kept_at {
        until(&mut ed, "each answer kept", |ed| kept(ed, file, *hash) == State::Trusted);
    }
    let held = "\"apiKeyHelper\":\"\"";
    let argv = || std::fs::read_to_string(root.join("argv")).unwrap_or_default();
    until(&mut ed, "claude with them", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| c.running()) && !argv().contains(held)
    });
    assert_eq!(ed.config.restart_build, "touch marker");
    ed.granted();
    until(&mut ed, "the list", |ed| ed.picker.as_ref().is_some_and(|p| p.kind == crate::granted::KIND));
    let labels: Vec<String> = ed.picker.as_ref().unwrap().items().iter().map(|i| i.label.clone()).collect();
    for l in [
        "tarae's project settings · every checkout",
        ".tarae.toml",
        "Claude's settings · every checkout",
        ".claude/settings.json",
        "git's own commands · every checkout",
        ".git/config",
    ] {
        assert!(labels.iter().any(|x| x == l), "{l} in {labels:?}");
    }
    for kind in [Kind::Tarae, Kind::Claude, Kind::Git] {
        let p = ed.picker.as_mut().unwrap();
        p.select_where(
            |a| matches!(a, crate::picker::Action::Granted(Some((Taken::Trust(k, _), Some('y')))) if *k == kind),
        );
        ed.handle_event(crate::event::Event::Key(crate::key::Key::plain(crate::key::Code::Char('d'))));
        let (file, hash) = &kept_at[kind as usize];
        // (the store written, not only the mark `Store::state` reads meanwhile)
        let place = crate::trust::place_of(file);
        until(&mut ed, "taken back", |ed| {
            !crate::trust::is_taking(&place) && kept(ed, file, *hash) == State::Ask
        });
    }
    until(&mut ed, "the list again", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.items().iter().any(|i| i.label == ".git/config"))
    });
    assert_eq!(ed.config.restart_build, "", "tarae's part held back at once");
    let seen = ed.session.chat.as_ref().and_then(|c| c.settings.seen.clone()).unwrap();
    assert_eq!(seen.state, State::Ask);
    until(&mut ed, "claude started again held back", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| c.running()) && argv().contains(held)
    });
    let r = crate::gitrepo::repo(&root).unwrap();
    assert!(r.asks, "git's commands wait for the card again");
    assert!(!root.parent().unwrap().join("fsmonitor").exists(), "never run");
}

/// Pieces E and J: what another checkout of the repository keeps as its own (a place in it, a key file) isn't used
/// here, and the list counts it on its last row, faint; what it kept that names no checkout is every checkout's,
/// listed here.
#[test]
fn answers_kept_for_another_checkout_are_counted_not_used() {
    let (mut ed, root) = checkout("elsewhere", None);
    let dir = root.parent().unwrap().to_path_buf();
    std::fs::write(dir.join("user.gitconfig"), "[user]\nname = t\nemail = t@t\n").unwrap();
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "x"]);
    git(&dir, &["worktree", "add", "-q", "../wt"]);
    let wt = std::fs::canonicalize(dir.join("wt")).unwrap();
    let store = dir.join("sandbox.json");
    ed.allow_path = Some(store.clone());
    let other = crate::sandbox::profile_path(&wt);
    crate::allow::keep(&store, &other, |a| {
        a.add_read("/opt/tarae-stand-in".into());
        a.add_write(other.join("out"));
        a.add(&crate::allow::Grant::Key(other.join(".env")));
    })
    .unwrap();
    ed.granted();
    until(&mut ed, "the list", |ed| ed.picker.as_ref().is_some_and(|p| p.kind == crate::granted::KIND));
    let items = ed.picker.as_ref().unwrap().items();
    let last = items.last().unwrap();
    assert_eq!(last.label, "2 more kept for other checkouts — not used here");
    assert_eq!(last.action, crate::picker::Action::Heading, "faint, never the row in hand");
    assert!(items.iter().any(|i| i.label == "read /opt/tarae-stand-in"), "every checkout's");
    assert!(
        !items.iter().any(|i| i.label.contains(".env") || i.label.contains("wt/out")),
        "not this checkout's"
    );
}

/// `n` holds each part back, each kept so; the toast names the commands that ask again.
#[test]
fn no_holds_each_part_back() {
    let (mut ed, root) = checkout("no", None);
    all_three(&mut ed, &root);
    let What::RepoTrust { parts, .. } = ed.offers[0].what.clone() else { panic!("{:?}", kinds(&ed)) };
    ed.offer_dismiss();
    assert!(ed.offers.is_empty());
    let said = ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ");
    assert!(
        said.contains("Held back — :project-trust, :claude-settings, :git-trust to look again"),
        "{said}"
    );
    assert_eq!(ed.config.restart_build, "");
    let What::GitTrust(a) = &parts[2].what else { panic!() };
    until(&mut ed, "git's kept", |ed| kept(ed, &a.config, a.digest) == State::Ignored);
    let What::ProjectTrust { hash } = parts[0].what else { panic!() };
    until(&mut ed, "tarae's kept", |ed| kept(ed, &root.join(".tarae.toml"), hash) == State::Ignored);
}

/// A part joining changes the card: it settles again before `y` counts, and what it said about a key pressed
/// before isn't carried over.
#[test]
fn a_part_joining_settles_the_card_again() {
    let (mut ed, root) = checkout("settle", None);
    crate::chat::open(&mut ed);
    until(&mut ed, "Claude's part", claude_card);
    let o = &ed.offers[0];
    assert_eq!(parts(o), ["project-trust", "claude-settings"]);
    assert!(o.question.split('\n').next().unwrap().ends_with("· Claude joined"), "{}", o.question);
    ed.offer_seen.set(Some((o.id, o.rev, Instant::now() - crate::offer::SETTLE)));
    assert!(ed.offer_settled());
    ed.offer_refused("y came as the card did — not taken; press it again");
    assert!(ed.offer_said_now().is_some());
    let r = crate::gitrepo::repo(&root).unwrap();
    ed.git_trust_check(&r);
    assert_eq!(parts(&ed.offers[0]), ["project-trust", "claude-settings", "git-trust"]);
    assert!(!ed.offer_settled(), "changed: in sight again before y counts");
    assert_eq!(ed.offer_said_now(), None, "nothing carried over");
}

/// A git value too long to show is never trusted: the card says so in the warning colour, and `y` trusts the rest
/// and holds git's back.
#[test]
fn a_git_value_too_long_to_show_is_held_back_on_yes() {
    let long = format!("/bin/{}", "x".repeat(crate::trust::SHOWN_MAX));
    let (mut ed, root) = checkout("long", Some(&long));
    all_three(&mut ed, &root);
    let o = &ed.offers[0];
    let lines: Vec<&str> = o.question.split('\n').collect();
    let warned: Vec<&str> = o.warnings.iter().flat_map(|r| r.clone()).map(|i| lines[i]).collect();
    assert_eq!(warned, ["  a value is too long to show — y holds git's part back"]);
    let What::RepoTrust { parts, .. } = o.what.clone() else { panic!() };
    ed.offer_accept();
    let said = ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ");
    assert!(
        said.contains("git's held back: a value too long to show") && !said.contains("git's 1"),
        "{said}"
    );
    assert_eq!(ed.config.restart_build, "touch marker", "the rest trusted");
    let What::GitTrust(a) = &parts[2].what else { panic!() };
    until(&mut ed, "git's kept held back", |ed| kept(ed, &a.config, a.digest) == State::Ignored);
}

/// A part asked alone by its command leaves the repository's card; the one part left is its own card again.
#[test]
fn a_command_asks_its_part_alone() {
    let (mut ed, root) = checkout("alone", None);
    all_three(&mut ed, &root);
    crate::typed::execute(&mut ed, "project-trust").unwrap();
    assert_eq!(kinds(&ed), ["repo-trust", "project-trust"]);
    assert_eq!(parts(&ed.offers[0]), ["claude-settings", "git-trust"]);
    assert_eq!(ed.offers[1].title, crate::trust::TITLE);
    crate::typed::execute(&mut ed, "claude-settings").unwrap();
    assert_eq!(kinds(&ed), ["git-trust", "project-trust", "claude-settings"], "git's own card again");
    let r = crate::gitrepo::repo(&root).unwrap();
    let own = crate::gittrust::Ask::of(&r).card(&ed.session.root);
    assert_eq!((ed.offers[0].title.as_str(), &ed.offers[0].question), (own.title.as_str(), &own.question));
}

/// `:git-trust` typed: the checkout's repository's card, alone — also after an answer, and out of the
/// repository's card while it's there.
#[test]
fn git_trust_typed_asks_about_the_repository_again() {
    let (mut ed, root) = checkout("typed", None);
    all_three(&mut ed, &root);
    ed.offer_dismiss();
    assert!(ed.offers.is_empty());
    feed(&mut ed, ":git-trust<ret>");
    until(&mut ed, "git's card", |ed| !ed.offers.is_empty());
    assert_eq!(kinds(&ed), ["git-trust"]);
    assert_eq!(ed.offers[0].title, crate::gittrust::TITLE);
    // Asked while the repository's card is up: it leaves it
    let (mut ed, root) = checkout("typed-up", None);
    all_three(&mut ed, &root);
    feed(&mut ed, ":git-trust<ret>");
    until(&mut ed, "git's card", |ed| kinds(ed).contains(&"git-trust"));
    assert_eq!(kinds(&ed), ["repo-trust", "git-trust"]);
    assert_eq!(parts(&ed.offers[0]), ["project-trust", "claude-settings"]);
}

/// A part of another checkout (a submodule's git, say) isn't gathered: its own card beside it.
#[test]
fn another_checkouts_part_stays_its_own_card() {
    let (mut ed, root) = checkout("other", None);
    let r = crate::gitrepo::repo(&root).unwrap();
    let mut ask = crate::gittrust::Ask::of(&r);
    ask.place = root.join("sub");
    ask.config = root.join(".git/modules/sub/config");
    ed.trust_offer(ask.card(&root));
    assert_eq!(kinds(&ed), ["project-trust", "git-trust"]);
}

/// Keys to the code (the chat opened with them).
fn feed(ed: &mut Editor, keys: &str) {
    ed.session.leave(crate::session::At::Chat);
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: crate::key::Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => crate::key::Key::plain(crate::key::Code::Char(c)),
        };
        ed.handle_key(key);
    }
}
