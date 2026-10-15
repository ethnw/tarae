//! What a warning holds for, read from its data: a command's words (after `NAME=value` and a wrapper), the
//! scripts it runs, what's set around it, its options, the checked forms that excuse it, the core's facts.

use std::path::PathBuf;

use super::*;

/// The warnings a `[[claude.warn]]` text says (as one pack's, `tools pack`) — none of them wrong.
fn warns(text: &str) -> Vec<Warn> {
    let t: toml::Table = text.parse().unwrap();
    let mut problems = Vec::new();
    let w = read(&t["warn"], "tools pack", &mut problems);
    assert!(problems.is_empty(), "{problems:?}");
    w
}

/// What `cmd` (running `scripts`) is warned of, nothing set in its environment unless `env` says so.
#[derive(Default)]
struct Judge<'a> {
    env: &'a [&'a str],
    scripts: Vec<(String, String)>,
    reads: Vec<PathBuf>,
    outside: bool,
    script_changed: bool,
}

impl Judge<'_> {
    fn ids(&self, w: &[Warn], cmd: &str) -> Vec<String> {
        self.judged(w, cmd).into_iter().map(|w| w.id).collect()
    }

    fn judged(&self, w: &[Warn], cmd: &str) -> Vec<Warned> {
        let env = |n: &str| self.env.contains(&n);
        // (the packs' own — `timeout`, `env`, `nice` …)
        let wrappers = &crate::sandboxpack::get().wrappers;
        let in_outputs = |p: &str| p.starts_with("target/") && !p.contains("..");
        judge(
            w,
            &Facts {
                cmd,
                scripts: &self.scripts,
                env: &env,
                wrappers,
                reads: &self.reads,
                script_changed: self.script_changed,
                edit_protected: false,
                outside: self.outside,
                in_outputs: &in_outputs,
            },
        )
    }
}

const DOCKER: &str = r#"
[[warn]]
id = "docker-own-config"
run = ["docker", "docker compose"]
missing = [
  { env = "DOCKER_CONFIG", say = "no DOCKER_CONFIG" },
  { env = "DOCKER_HOST", say = "no DOCKER_HOST" },
  { wrapper = ["timeout", "gtimeout"], say = "no timeout" },
]
say = "Docker: {missing}."
"#;

/// A command is its leading words — after `NAME=value` and a wrapper (`timeout -k 5 600 docker …`), never a word
/// another command says (`echo docker`); each of `missing` it lacks is named, and none lacking is no warning:
/// set in front of it, by an earlier `export`, around the script it's in, or in the run's environment.
#[test]
fn what_a_command_lacks_is_named() {
    let w = warns(DOCKER);
    let j = Judge::default();
    let said = j.judged(&w, "docker run --rm alpine");
    assert_eq!(said[0].said, "Docker: no DOCKER_CONFIG; no DOCKER_HOST; no timeout.");
    assert_eq!(said[0].line(), "Docker: no DOCKER_CONFIG; no DOCKER_HOST; no timeout. (tools pack)");
    assert!(j.ids(&w, "DOCKER_CONFIG=/x DOCKER_HOST=unix:///y timeout 600 docker run alpine").is_empty());
    assert!(j.ids(&w, "DOCKER_CONFIG=/x DOCKER_HOST=y timeout -k 5 600 docker compose up").is_empty());
    let said = j.judged(&w, "DOCKER_HOST=y timeout 600 docker run alpine");
    assert_eq!(said[0].said, "Docker: no DOCKER_CONFIG.");
    assert!(
        j.ids(&w, "export DOCKER_CONFIG=/x DOCKER_HOST=y; timeout 60 docker ps").is_empty(),
        "set before it"
    );
    let said = j.judged(
        &w,
        "DOCKER_CONFIG=/x DOCKER_HOST=y timeout 5 true; DOCKER_CONFIG=/x DOCKER_HOST=y docker ps",
    );
    assert_eq!(said[0].said, "Docker: no timeout.", "an earlier command's wrapper wraps nothing of this one");
    let set = Judge { env: &["DOCKER_CONFIG", "DOCKER_HOST"], ..Judge::default() };
    assert!(set.ids(&w, "timeout 60 docker ps").is_empty(), "in the run's environment");
    for not in ["echo docker run", "dockerd", "grep docker notes.txt", "cat docker-compose.yml"] {
        assert!(j.ids(&w, not).is_empty(), "{not}");
    }
    // (any command of the line — after a pipe, in an `sh -c`, after `&&`)
    for cmd in ["cd x && docker ps", "true | docker ps", "sh -c 'docker ps'", "if docker ps; then :; fi"] {
        assert_eq!(j.ids(&w, cmd), ["docker-own-config"], "{cmd}");
    }
}

/// What runs in a script runs too: a script's commands are judged, with what the line sets and wraps around
/// it (`DOCKER_HOST=… timeout 600 sh run.sh`) counted.
#[test]
fn a_scripts_commands_are_judged_with_the_line_around_them() {
    let w = warns(DOCKER);
    let j =
        Judge { scripts: vec![("run.sh".into(), "set -eu\ndocker build .\n".into())], ..Judge::default() };
    assert_eq!(j.ids(&w, "sh run.sh"), ["docker-own-config"]);
    assert!(j.ids(&w, "DOCKER_CONFIG=/x DOCKER_HOST=y timeout 600 sh run.sh").is_empty());
    let set_inside = Judge {
        scripts: vec![(
            "run.sh".into(),
            "export DOCKER_CONFIG=/x\nDOCKER_HOST=y timeout 60 docker build .\n".into(),
        )],
        ..Judge::default()
    };
    assert!(set_inside.ids(&w, "sh run.sh").is_empty(), "set in the script before it");
    // (a comment says nothing)
    let comment =
        Judge { scripts: vec![("c.sh".into(), "# docker build .\necho hi\n".into())], ..Judge::default() };
    assert!(comment.ids(&w, "sh c.sh").is_empty());
}

/// `options`, read as options are: a short one in a cluster (`-af`), a long one as itself or with `=`, BSD's
/// dashless form only as the first word (`ps e`, `ps auxe`) — never a longer option that starts the same
/// (`--fullname`), nor an option's value (`ps -o user`: `user` has an `e`, and it's `-o`'s); none of them: no
/// warning.
#[test]
fn an_option_counts_in_a_cluster() {
    let w = warns(
        r#"
[[warn]]
id = "process-details"
run = ["pgrep", "lsof"]
options = ["-f", "-a", "-p", "--full"]
say = "It reads other processes' details."
[[warn]]
id = "ps-env"
run = ["ps"]
options = ["-E", "e"]
option-values = ["-o", "-p", "-u"]
say = "It reads other processes' environments."
"#,
    );
    let j = Judge::default();
    for cmd in ["pgrep -f tarae", "pgrep -af x", "pgrep --full x", "lsof -p 12", "lsof -nP -p 12"] {
        assert_eq!(j.ids(&w, cmd), ["process-details"], "{cmd}");
    }
    for cmd in ["ps -E", "ps e", "ps auxe", "ps -xE", "ps -p 12 -E"] {
        assert_eq!(j.ids(&w, cmd), ["ps-env"], "{cmd}");
    }
    for cmd in [
        "pgrep tarae",
        "lsof -i :80",
        "ps aux",
        "ps -ef",
        "pgrep --fullname x",
        "ps -o user -p 12",
        "ps -p 12 -o pid,state",
        "ps -u alice",
        "ps aux user",
    ] {
        assert!(j.ids(&w, cmd).is_empty(), "{cmd}");
    }
}

/// The bash pack's `rm -r`: excused only in the checked-mktemp form (D5) — `set -eu`; one fresh `mktemp -d
/// /private/tmp/<prefix>.*` folder; checked non-empty and by `case` against that prefix — those before the `rm`,
/// in that order — and the `rm -rf` of that one variable, or a path in it with no `..`: or for a build's scratch
/// below a pack's `outputs` folder. A form with any part off, the variable or the prefix another in one of them,
/// a guard after the `rm`, a path that leaves the folder (`"$T/.."`), warns.
#[test]
fn rm_r_is_excused_only_in_the_checked_form() {
    let packs = crate::sandboxpack::get();
    let w: Vec<Warn> = packs.warn.iter().filter(|w| w.id == "rm-r-unchecked").cloned().collect();
    assert_eq!(w.len(), 1, "the bash pack's");
    let j = Judge::default();
    let guarded = |v: &str, pre: &str, case_pre: &str, set: &str, rm: &str| {
        format!(
            "{set}\nT=$(mktemp -d /private/tmp/{pre}.XXXXXX)\n[ -n \"${v}\" ]\ncase \"${v}\" in /private/tmp/{case_pre}.*) ;; \
             *) exit 1;; esac\necho work > \"$T/out\"\n{rm}\n"
        )
    };
    let form =
        |v: &str, pre: &str, case_pre: &str, set: &str| guarded(v, pre, case_pre, set, "rm -rf \"$T\"");
    let script = |text: String| Judge { scripts: vec![("scratch.sh".into(), text)], ..Judge::default() };
    assert!(script(form("T", "brk", "brk", "set -eu")).ids(&w, "sh scratch.sh").is_empty(), "the form");
    assert!(script(form("T", "brk", "brk", "set -euo pipefail")).ids(&w, "sh scratch.sh").is_empty());
    let inside = guarded("T", "brk", "brk", "set -eu", "rm -rf \"$T/out/old\"");
    assert!(script(inside).ids(&w, "sh scratch.sh").is_empty(), "a path in the folder");
    // (the guards after the `rm`: the same lines, in the wrong place)
    let after = "set -eu\nT=$(mktemp -d /private/tmp/brk.XXXXXX)\nrm -rf \"$T\"\n[ -n \"$T\" ]\n\
                 case \"$T\" in /private/tmp/brk.*) ;; *) exit 1;; esac\n";
    for (why, text) in [
        ("no set -eu", form("T", "brk", "brk", "set -e")),
        ("another variable checked", form("U", "brk", "brk", "set -eu")),
        ("another prefix checked", form("T", "brk", "other", "set -eu")),
        ("out of the folder", guarded("T", "brk", "brk", "set -eu", "rm -rf \"$T/..\"")),
        ("out of the folder, further", guarded("T", "brk", "brk", "set -eu", "rm -rf \"$T/../x\"")),
        ("out of the folder, deep", guarded("T", "brk", "brk", "set -eu", "rm -rf \"$T/a/../../x\"")),
        ("the guards after the rm", after.to_string()),
    ] {
        assert_eq!(script(text).ids(&w, "sh scratch.sh"), ["rm-r-unchecked"], "{why}");
    }
    // On the line itself, one-liner
    let line = "set -eu; T=$(mktemp -d /private/tmp/x.XXXX); [ -n \"$T\" ] && case \"$T\" in /private/tmp/x.*) rm -rf \"$T\";; esac";
    assert!(j.ids(&w, line).is_empty(), "{line}");
    for cmd in ["rm -rf /tmp/x", "rm -r src", "rm -fr \"$HOME/x\"", "rm --recursive x", "cd /; rm -rf *"] {
        assert_eq!(j.ids(&w, cmd), ["rm-r-unchecked"], "{cmd}");
    }
    // A build's scratch: excused; anything else in the same `rm`, not
    assert!(j.ids(&w, "rm -rf target/sb/step6c").is_empty());
    assert_eq!(j.ids(&w, "rm -rf target/sb src"), ["rm-r-unchecked"]);
    assert_eq!(j.ids(&w, "rm -rf target/../src"), ["rm-r-unchecked"]);
    // Not recursive: nothing
    assert!(j.ids(&w, "rm -f a.txt").is_empty());
}

/// The core's facts, worded by data: what it reads outside the project (`{paths}`), a script changed since it
/// was seen, outside the sandbox — and a `run` with a fact needs both.
#[test]
fn a_fact_is_worded_by_its_entry() {
    let w = warns(
        r#"
[[warn]]
id = "reads-outside"
fact = "reads-outside"
say = "What it reads goes to Claude: {paths}."
[[warn]]
id = "script-changed"
fact = "script-changed"
say = "A script it runs changed since you saw it."
[[warn]]
id = "push-outside"
run = ["git push"]
fact = "outside-sandbox"
asks = true
say = "It pushes, outside the sandbox."
"#,
    );
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let j = Judge { reads: vec![home.join(".gitconfig"), "/etc/hosts".into()], ..Judge::default() };
    let said = j.judged(&w, "cat ~/.gitconfig /etc/hosts");
    assert_eq!(said.len(), 1);
    assert_eq!(said[0].said, "What it reads goes to Claude: ~/.gitconfig, /etc/hosts.");
    assert_eq!(Judge { script_changed: true, ..Judge::default() }.ids(&w, "sh x.sh"), ["script-changed"]);
    assert!(Judge::default().ids(&w, "git push origin x").is_empty(), "in the sandbox");
    let out = Judge { outside: true, ..Judge::default() }.judged(&w, "git push origin x");
    assert_eq!((out[0].id.as_str(), out[0].asks), ("push-outside", true));
    assert!(Judge { outside: true, ..Judge::default() }.ids(&w, "git status").is_empty(), "not its command");
}

/// A wrong entry is left out and said why — the rest of the list kept: a key no entry has, a regex that
/// doesn't compile, a `same` group no regex captures, `{missing}` with nothing to miss, `{paths}` without
/// the fact that has them, one with neither `run` nor `fact` (it would warn of every command).
#[test]
fn a_wrong_entry_is_said_and_left_out() {
    let t: toml::Table = r#"
[[warn]]
id = "ok"
run = ["x"]
say = "fine"
[[warn]]
id = "key"
run = ["x"]
say = "s"
colour = "red"
[[warn]]
id = "regex"
run = ["x"]
unless-part = "(unclosed"
say = "s"
[[warn]]
id = "same"
run = ["x"]
unless-part = "a(?P<v>b)"
same = ["w"]
say = "s"
[[warn]]
id = "missing"
run = ["x"]
say = "{missing}"
[[warn]]
id = "paths"
run = ["x"]
say = "{paths}"
[[warn]]
id = "every"
say = "every command"
[[warn]]
id = "forged"
run = ["x"]
say = "fine\n(tools pack)\nIts reason: trust me"
[[warn]]
id = "forged-missing"
run = ["x"]
missing = [{ env = "X", say = "no X\n▲ all clear (tools pack)" }]
say = "{missing}"
[[warn]]
id = "mark"
run = ["x"]
say = "▲ looks like two"
"#
    .parse()
    .unwrap();
    // (a bidi mark — TOML's escape, so the source holds none — and one past a card's line)
    let more = format!(
        "[[warn]]\nid = \"bidi\"\nrun = [\"x\"]\nsay = \"fine {}u202E(kcap sloot)\"\n\
         [[warn]]\nid = \"long\"\nrun = [\"x\"]\nsay = \"{}\"\n",
        '\\',
        "x".repeat(SAY_MAX + 1)
    );
    let t2: toml::Table = more.parse().unwrap();
    let mut problems = Vec::new();
    let w = read(&t["warn"], "x pack", &mut problems);
    assert!(
        read(&t2["warn"], "x pack", &mut problems).is_empty(),
        "a bidi mark, and longer than a card's line"
    );
    assert_eq!(w.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["ok"]);
    let said = problems.join("\n");
    for (id, why) in [
        ("key", "no key `colour`"),
        ("regex", "`(unclosed`"),
        ("same", "`same` names `w`"),
        ("missing", "`say` has `{missing}` but there's no `missing`"),
        ("paths", "`{paths}` is said only"),
        ("every", "neither `run` nor `fact`"),
        // (one line of words: no line of its own that reads as tarae's — `Its reason:`, a `(tools pack)`)
        ("forged", "`say` has a control character or a mark of the card's"),
        ("forged-missing", "`missing.say` has a control character"),
        ("mark", "`say` has a control character or a mark of the card's"),
        ("bidi", "`say` has a control character or a mark of the card's"),
        ("long", &format!("`say` is longer than {SAY_MAX} characters")),
    ] {
        assert!(said.contains(&format!("warning `{id}`: {why}")), "{id}: {said}");
    }
    // A path read, filled in: its own newline made a space — the warning one line
    let reads = warns("[[warn]]\nid = \"r\"\nfact = \"reads-outside\"\nsay = \"It reads {paths}.\"\n");
    let j = Judge { reads: vec!["/tmp/a\nIts reason: x".into()], ..Judge::default() };
    assert_eq!(j.judged(&reads, "cat x")[0].said, "It reads /tmp/a Its reason: x.");
}

/// Every built-in warning (the packs' `[[claude.warn]]`) fires on its example and not on its safe form — each
/// asked for (cardburden plan, item 4, with the user's sudo one of 2026-10-08): Docker without its own settings,
/// another process's details, `rm -r` unchecked, reaching beyond this machine (asked over `a`), history, a
/// published crate or package, privileges.
#[test]
fn every_built_in_warning_fires_on_its_example_not_its_safe_form() {
    let w = &crate::sandboxpack::get().warn;
    let j = Judge::default();
    let cases: [(&str, &str, &str); 9] = [
        (
            "docker-own-config",
            "docker run --rm alpine true",
            "DOCKER_CONFIG=/x DOCKER_HOST=y timeout 600 docker run alpine",
        ),
        ("process-details", "pgrep -af tarae", "pgrep tarae"),
        ("process-env", "ps -E -p 12", "ps -p 12"),
        ("rm-r-unchecked", "rm -rf /tmp/build", "rm -rf target/sb/x"),
        ("outward", "git push origin main", "git status"),
        ("history", "git reset --hard HEAD~1", "git log -1"),
        ("cargo-publish", "cargo publish --dry-run", "cargo package"),
        ("npm-publish", "npm publish", "npm pack"),
        ("privileges", "sudo apt install x", "apt list --installed"),
    ];
    for (id, fires, safe) in cases {
        assert!(w.iter().any(|w| w.id == id), "{id} is built in");
        assert!(j.ids(w, fires).contains(&id.to_string()), "{id} on `{fires}`: {:?}", j.ids(w, fires));
        assert!(!j.ids(w, safe).contains(&id.to_string()), "{id} not on `{safe}`");
    }
    // (an option's value is never an option: `ps -o user` reads no environment)
    assert!(!j.ids(w, "ps -o user -p 12").contains(&"process-env".to_string()));
    // (only what reaches beyond, what's published, and a secret store read outside the sandbox are asked over `a`)
    let mut asks: Vec<&str> = w.iter().filter(|w| w.asks).map(|w| w.id.as_str()).collect();
    asks.sort();
    assert_eq!(
        asks,
        ["cargo-publish", "keychain-dump", "keychain-read", "npm-publish", "outward", "secret-service-read"]
    );
    // The facts' words: what a read goes to, a script changed, a protected file edited
    for (id, facts) in [
        ("reads-outside", Judge { reads: vec!["/etc/hosts".into()], ..Judge::default() }),
        ("script-changed", Judge { script_changed: true, ..Judge::default() }),
    ] {
        assert_eq!(facts.ids(w, "cat x"), [id]);
    }
    let privileges = j.judged(w, "sudo true");
    assert!(
        privileges[0].said.contains("no terminal for a password")
            && privileges[0].said.contains("tarae's terminal"),
        "says why and what to do: {}",
        privileges[0].said
    );
    assert_eq!(privileges[0].whose, "tools pack");
}

/// The secret stores tarae's API key is kept in (apibill D3): outside the sandbox, a read of a password from the
/// keychain (`-w`, `-g`), a dump of it, a Secret Service lookup warns — and asks each time, whatever `a` said.
/// Inside the sandbox it can't reach them: no warning. The keychain's attributes alone (no `-w`) aren't the
/// password; an option's value is never an option (`-s -w` names a service called `-w`).
#[test]
fn a_secret_store_read_outside_the_sandbox_warns_and_asks_each_time() {
    let w = &crate::sandboxpack::get().warn;
    let (outside, inside) = (Judge { outside: true, ..Judge::default() }, Judge::default());
    let read = "security find-generic-password -a me -s tarae.anthropic-api-key -w";
    for (id, cmd) in [
        ("keychain-read", read),
        ("keychain-read", "security find-internet-password -g -s example.com"),
        ("keychain-dump", "security dump-keychain"),
        ("secret-service-read", "secret-tool lookup service tarae-anthropic-api-key"),
        ("secret-service-read", "secret-tool search --all service x"),
    ] {
        assert!(outside.ids(w, cmd).contains(&id.to_string()), "{id} outside on `{cmd}`");
        assert!(!inside.ids(w, cmd).contains(&id.to_string()), "{id}: not in the sandbox, `{cmd}`");
        assert!(w.iter().find(|x| x.id == id).unwrap().asks, "{id} asks over `a`");
    }
    for cmd in [
        "security find-generic-password -a me -s tarae.anthropic-api-key",
        "security find-generic-password -s -w",
    ] {
        assert!(!outside.ids(w, cmd).contains(&"keychain-read".to_string()), "not a password read: `{cmd}`");
    }
    // (in a script it runs, as in its line; and its words say what's at stake)
    let script =
        Judge { outside: true, scripts: vec![("k.sh".into(), format!("{read}\n"))], ..Judge::default() };
    let said = script.judged(w, "sh k.sh");
    let k = said.iter().find(|x| x.id == "keychain-read").expect("in the script");
    assert!(k.said.contains("tarae's API key is there") && k.whose == "tools pack", "{k:?}");
}

/// An editor in a temp folder of its own (`name`) with Claude mid-turn (a stand-in claude that reads and says
/// nothing), `project` its `[[cards.warn]]` (as a `.tarae.toml` would say them) — and its chat's generation.
fn chat_in(name: &str, project: &str) -> (crate::editor::Editor, u64, PathBuf) {
    let dir = crate::scratch::Scratch::new(&format!("cardwarn-{name}")).into_path();
    let (config, warnings) = crate::config::parse(project);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut ed = crate::editor::Editor::new(config);
    ed.session.root = dir.clone();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    (c.state, c.focused) = (crate::chat::State::Thinking, false);
    let generation = c.generation();
    (ed, generation, dir)
}

fn pump_until(ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool, what: &str) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !done(ed) {
        assert!(std::time::Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

fn answered(n: u64) -> Option<Result<String, String>> {
    crate::tools::ANSWERED
        .with(|a| a.borrow().iter().find(|(id, _)| *id == serde_json::json!(n)).map(|(_, r)| r.clone()))
}

/// Claude's `run` of `cmd` (call `n`), its tool row in the chat as claude's stream would make it — answered
/// at once (None) or waiting for its card (its id).
fn ask(ed: &mut crate::editor::Editor, generation: u64, n: u64, cmd: &str) -> Option<u64> {
    let c = ed.session.chat.as_mut().unwrap();
    let id = format!("toolu_{n}");
    let input = serde_json::json!({ "command": cmd });
    c.calls.push(crate::chat::Called {
        id: id.clone(),
        name: "mcp__tarae__run".into(),
        input: input.clone(),
    });
    let look =
        crate::chat::Look { verb: "run", what: cmd.into(), pattern: true, range: String::new(), link: None };
    c.msgs.push(crate::chat::Msg {
        look: Some(look),
        call: id,
        ..crate::chat::Msg::new(crate::chat::Role::Tool, "", "")
    });
    let msg = serde_json::json!({"jsonrpc": "2.0", "id": n, "method": "tools/call",
        "params": {"name": "run", "arguments": input}});
    assert!(crate::tools::mcp(ed, generation, &format!("r{n}"), &msg).is_none(), "it waits");
    pump_until(ed, &|ed| !ed.offers.is_empty() || answered(n).is_some(), "its card, or its end");
    ed.offers.first().map(|o| o.id)
}

/// The card: what it warns of right under the command, in the warning color, saying whose — the packs'
/// (`tools pack`) and the project's (`.tarae.toml`) — its `card` line naming them (`warn=`); a card the same as
/// one answered before still says them (item 3). It answers nothing. (Every answer `n`: nothing runs.)
#[test]
fn a_card_says_its_warnings_under_what_it_asks() {
    let project = r#"
[[cards.warn]]
id = "xtask-linux-docker"
run = ["cargo xtask linux"]
missing = [{ wrapper = ["timeout"], say = "no timeout" }]
say = "cargo xtask linux runs Docker: {missing}."
"#;
    let (mut ed, generation, dir) = chat_in("card", project);
    let id = ask(&mut ed, generation, 1, "docker ps && cargo xtask linux clippy").expect("its card");
    let o = &ed.offers[0];
    let lines: Vec<&str> = o.question.split('\n').collect();
    assert_eq!(lines[0], "docker ps && cargo xtask linux clippy");
    assert!(
        lines[2].starts_with("▲ Docker: no DOCKER_CONFIG") && lines[2].ends_with("(tools pack)"),
        "right under the command: {lines:#?}"
    );
    assert_eq!(lines[3], "▲ cargo xtask linux runs Docker: no timeout. (config.toml)");
    assert_eq!(o.warnings, vec![2..4], "in the warning color");
    assert!(answered(1).is_none(), "never answered for the user");
    ed.offer_dismiss();
    pump_until(&mut ed, &|_| answered(1).is_some(), "told no");
    let line =
        crate::cards::logged_lines("card ").into_iter().find(|l| l.contains(&format!(" · card#{id} · ")));
    let line = line.expect("its card line");
    assert!(line.contains(" · warn=docker-own-config,xtask-linux-docker · n · "), "{line}");
    // The same again: `Same as …` first, and its warnings still under the command
    pump_until(&mut ed, &|ed| ed.session.answered.iter().any(|a| a.card == id), "the no kept");
    ask(&mut ed, generation, 2, "docker ps && cargo xtask linux clippy").expect("its card");
    let q = ed.offers[0].question.clone();
    assert!(
        q.starts_with(&format!("Same as card#{id}")) && q.contains("\n▲ Docker: no DOCKER_CONFIG"),
        "{q}"
    );
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}

/// tarae's own `.tarae.toml` warns of `cargo xtask linux` without Docker's own settings — and not of step 6c's
/// line, which set all three (the plan's count: none of 6c's Docker runs would have been warned of).
#[test]
fn taraes_own_project_warns_of_xtask_linux_bare() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.tarae.toml");
    let (config, warnings) = crate::config::parse(&std::fs::read_to_string(path).unwrap());
    assert!(warnings.is_empty(), "{warnings:?}");
    let w = &config.editor.cards_warn;
    let j = Judge::default();
    assert_eq!(j.ids(w, "cargo xtask linux clippy"), ["xtask-linux-docker"]);
    let six_c =
        "DOCKER_CONFIG=/tmp/dc DOCKER_HOST=unix:///x/docker.sock timeout 1800 cargo xtask linux clippy";
    assert!(j.ids(w, six_c).is_empty(), "{six_c}");
}

/// A project's `.tarae.toml` — a cloned repository's, read with no trust asked — only adds warnings: an id a
/// pack has, or your own config, is left out and said, and the pack's and yours still fire as they say (an
/// `asks` one still asks); one of its own ids comes in.
#[test]
fn a_projects_file_only_adds_warnings() {
    let scratch = crate::scratch::Scratch::new("cardwarn-layers");
    let dir = scratch.to_path_buf();
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let user = dir.join("config.toml");
    std::fs::write(
        &user,
        "[[cards.warn]]\nid = \"mine\"\nrun = [\"deploy\"]\nasks = true\nsay = \"It deploys.\"\n",
    )
    .unwrap();
    let project = dir.join("proj/.tarae.toml");
    std::fs::write(
        &project,
        r#"
[[cards.warn]]
id = "outward"
run = ["nope"]
say = "nothing to see"
[[cards.warn]]
id = "mine"
run = ["nope"]
say = "nothing to see"
[[cards.warn]]
id = "theirs"
run = ["thing"]
say = "Its own."
"#,
    )
    .unwrap();
    let (c, warnings) = crate::config::load_from(Some(user), Some(project), &crate::trust::Store::default());
    let said = warnings.join("\n");
    assert!(said.contains("cards.warn: left out: id `outward` is the tools pack's"), "{said}");
    assert!(said.contains("cards.warn: left out: id `mine` is the config.toml's"), "{said}");
    let ids: Vec<&str> = c.editor.cards_warn.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["mine", "theirs"]);
    let packs = &crate::sandboxpack::get().warn;
    let j = Judge::default();
    let all: Vec<Warn> = packs.iter().chain(&c.editor.cards_warn).cloned().collect();
    let judged = j.judged(&all, "git push origin x && deploy && thing");
    let got: Vec<(&str, &str, bool)> =
        judged.iter().map(|w| (w.id.as_str(), w.whose.as_str(), w.asks)).collect();
    assert_eq!(
        got,
        [("outward", "tools pack", true), ("mine", "config.toml", true), ("theirs", ".tarae.toml", false)]
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Warnings only add: a runtime folder's pack can add one, never take a built-in one out nor reword it (the
/// built-in's words win for its id) — nor can a project's.
#[test]
fn a_built_in_warning_stays_whatever_is_added() {
    let yours = r#"
[[claude.warn]]
id = "outward"
run = ["git push"]
say = "nothing to see"
[[claude.warn]]
id = "mine"
run = ["deploy"]
say = "It deploys."
"#;
    let packs =
        crate::sandboxpack::build(crate::packs::builtin_manifests(), &[("tools".into(), yours.into())]);
    let j = Judge::default();
    let said = j.judged(&packs.warn, "git push origin x && deploy");
    let said: Vec<(&str, &str, &str)> =
        said.iter().map(|w| (w.id.as_str(), w.whose.as_str(), w.said.as_str())).collect();
    assert_eq!(said[0].0, "outward");
    assert_eq!(said[0].1, "tools pack", "the built-in's");
    assert!(said[0].2.starts_with("It reaches beyond this machine"), "{:?}", said[0]);
    assert_eq!(said[1], ("mine", "your tools pack", "It deploys."));
}

/// D2: a warning doesn't take `a` back — the same command runs unasked after it, its warning said on its chat
/// row and in the log (`warned`) — but one whose data `asks` does, an `a` given before it asked (your config
/// changed since): its card comes again, saying why. (The runs: `touch` in the test's own temp folder.)
#[test]
fn only_a_warning_that_asks_takes_a_back() {
    for (asks, k) in [(false, 10), (true, 20)] {
        // (each pass its own calls: what was answered is kept per thread)
        let (first, second) = (k + 1, k + 2);
        let project = "[[cards.warn]]\nid = \"touches\"\nrun = [\"touch\"]\nsay = \"It touches a file.\"\n";
        let (mut ed, generation, dir) = chat_in(&format!("asks{asks}"), project);
        // (no sandbox inside the tests' own: the guard is "off" — the run's record all the same)
        ed.config.llm.sandbox = false;
        let cmd = "touch made.txt";
        ask(&mut ed, generation, first, cmd).expect("a first card");
        // (`a`: this exact command again unasked, this chat)
        ed.offer_for_chat();
        pump_until(&mut ed, &|_| answered(first).is_some(), "it ran");
        assert!(dir.join("made.txt").exists(), "{:?}", answered(first));
        // (its row: where it ran, and what it warned of — the card's)
        let row_of = |ed: &crate::editor::Editor, n: u64| {
            let c = ed.session.chat.as_ref().unwrap();
            c.msgs
                .iter()
                .find(|m| m.call == format!("toolu_{n}"))
                .and_then(|m| m.ran.clone())
                .expect("marked")
        };
        let row = row_of(&ed, first);
        assert_eq!(row.warned.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["touches"]);
        assert_eq!((row.mark.as_str(), row.past), ("sandbox off", true));
        // (the second pass: the warning asks now — your config changed after the `a`)
        ed.config.cards_warn[0].asks = asks;
        let again = ask(&mut ed, generation, second, cmd);
        match asks {
            false => {
                assert!(again.is_none(), "`a` holds: it ran unasked");
                pump_until(&mut ed, &|_| answered(second).is_some(), "it ran");
                let row = row_of(&ed, second);
                assert_eq!(row.warned.len(), 1, "said on its row");
                let logged = crate::cards::logged_lines("warned ");
                assert!(logged.iter().any(|l| l.contains(" · touch made.txt · warn=touches")), "{logged:?}");
            }
            true => {
                let q = ed.offers.first().map(|o| o.question.clone()).expect("asked again");
                assert!(
                    q.contains("▲ It touches a file. (config.toml)")
                        && q.contains("`a` let this command run"),
                    "{q}"
                );
                ed.offer_dismiss();
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// D2, from a script: `sh deploy.sh` whose script pushes — the `outward` warning, which asks, read from the
/// script for its card — gets no `a` there: `a` would let the same run through unasked next time, the push with
/// it. (Answered `n`: nothing runs.)
#[test]
fn a_card_whose_script_asks_offers_no_a() {
    let (mut ed, generation, dir) = chat_in("scriptasks", "");
    std::fs::write(dir.join("deploy.sh"), "set -eu\ncargo build\ngit push origin main\n").unwrap();
    let id = ask(&mut ed, generation, 1, "sh deploy.sh").expect("its card");
    let o = &ed.offers[0];
    assert!(o.question.contains("\n▲ It reaches beyond this machine"), "{}", o.question);
    assert!(!o.question.contains("a — this exact command"), "{}", o.question);
    assert!(!o.answers(false).iter().any(|(k, _)| *k == 'a'), "{:?}", o.answers(false));
    // (`a` pressed anyway: nothing — the card waits)
    ed.offer_for_chat();
    assert!(ed.offers.first().is_some_and(|o| o.id == id) && answered(1).is_none());
    // The same line without the push: `a` there
    std::fs::write(dir.join("deploy.sh"), "set -eu\ncargo build\n").unwrap();
    ed.offer_dismiss();
    pump_until(&mut ed, &|_| answered(1).is_some(), "told no");
    ask(&mut ed, generation, 2, "sh deploy.sh").expect("its card");
    assert!(ed.offers[0].answers(false).iter().any(|(k, _)| *k == 'a'), "{:?}", ed.offers[0].answers(false));
    ed.offer_dismiss();
    std::fs::remove_dir_all(&dir).ok();
}

/// A command split as a shell splits it — quotes kept together, `#` a comment, a `$(…)` its own command — and
/// a path that's a build's scratch only below the project's `outputs` folder.
#[test]
fn a_line_is_split_as_a_shell_splits_it() {
    let words = |t: &str| commands(t).into_iter().map(|c| c.words.join(" ")).collect::<Vec<_>>();
    assert_eq!(words("a 'b c' \"d;e\"; f | g && h"), ["a b c d;e", "f", "g", "h"]);
    assert_eq!(words("T=$(mktemp -d x) # gone\nrm -rf \"$T\""), ["T=", "mktemp -d x", "rm -rf $T"]);
    assert_eq!(commands("rm -rf \"$T\"")[0].raw, "rm -rf \"$T\"", "as written, for `unless-part`");
    let root = std::path::Path::new("/p");
    let out = ["target".to_string()];
    assert!(in_outputs(root, root, "target/sb/x", &out));
    assert!(in_outputs(root, &root.join("target"), "sb/x", &out), "from where it runs");
    for not in ["src", "../target/x", "/p/target/x", "target/*", "$T", "~/target"] {
        assert!(!in_outputs(root, root, not, &out), "{not}");
    }
}
