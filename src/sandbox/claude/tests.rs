use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::sandbox::Os;

/// A stand-in home in the test's own temp folder: a fake key, claude's own, what runs later, and a project in it
/// with a protected name, `.git` and a file named like a key. Removed when dropped.
struct Home {
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
}

impl Drop for Home {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

const FILES: [(&str, &str); 15] = [
    (".ssh/id_x", "not a key\n"),
    (".claude/settings.json", "{}\n"),
    (".claude/plugins/cache/m/p/1.0.0/.in_use/.keep", ""),
    (".claude/plugins/synced/a_b/.keep", ""),
    (".claude/projects/-other/.keep", ""),
    (".claude/local/.keep", ""),
    (".claude/ide/.keep", ""),
    (".claude/shell-snapshots/.keep", ""),
    (".claude.json", "{}\n"),
    (".config/tarae/config.toml", ""),
    (".zshrc", ""),
    (".local/bin/.keep", ""),
    ("work/proj/build.rs", "fn main() {}\n"),
    ("work/proj/.env", "API_KEY=tarae-fake-token\n"),
    ("work/proj/.git/hooks/.keep", ""),
];

impl Home {
    fn new(name: &str) -> Home {
        let tmp = profile_path(&std::env::temp_dir());
        let base = tmp.join(format!("tarae-claude-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        for (rel, text) in FILES {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        std::fs::create_dir_all(home.join("work/proj/src")).unwrap();
        let root = home.join("work/proj");
        // (this project's conversations, and its auto-memory, as claude makes them)
        std::fs::create_dir_all(home.join(".claude/projects").join(project_folder(&root)).join("memory"))
            .unwrap();
        Home { base, home, root }
    }

    /// What `Here` says of it: its home, the real temp folder.
    fn here(&self) -> Here {
        let env = HashMap::from([("HOME".to_string(), self.home.clone().into())]);
        Here { env, tmp: Some(profile_path(&std::env::temp_dir())), uid: 501, os: Os::Mac, ..Here::default() }
    }

    fn profile(&self, writes_project: bool) -> Profile {
        Profile::new(&self.root, &self.home, writes_project, &self.here(), "tarae-claude.test.")
    }
}

/// What the stand-in claude tries, each a line "<name> ok" or "<name> refused": (name, path, a write or not). A
/// path `mv:` moves it aside, into temp — last, as it would leave no `~/.claude` for the others.
#[cfg(target_os = "macos")]
const TRIES: [(&str, &str, bool); 27] = [
    ("read a key folder", "$H/.ssh/id_x", false),
    ("read a key-named file in the project", "$P/.env", false),
    ("read its own config", "$H/.claude.json", false),
    ("read the project", "$P/build.rs", false),
    ("write its own folder", "$H/.claude/sessions.json", true),
    ("write its config's temp copy", "$H/.claude.json.tmp.1.ab", true),
    ("write a plugin's in-use mark", "$H/.claude/plugins/cache/m/p/1.0.0/.in_use/1", true),
    ("write your Claude settings", "$H/.claude/settings.json", true),
    ("write a Claude hook", "$H/.claude/Hooks/x.sh", true),
    ("write the synced marketplace list", "$H/.claude/plugins/synced/a_b/.marketplaces.json", true),
    ("write anything else synced", "$H/.claude/plugins/synced/a_b/hooks.json", true),
    ("write a project's auto-memory", "$H/.claude/projects/$E/memory/MEMORY.md", true),
    ("write its own conversation", "$H/.claude/projects/$E/1.jsonl", true),
    ("write another project's conversation", "$H/.claude/projects/-other/1.jsonl", true),
    ("write the local install's claude", "$H/.claude/local/claude", true),
    ("write an IDE lock", "$H/.claude/ide/1.lock", true),
    ("write a shell snapshot", "$H/.claude/shell-snapshots/s.sh", true),
    ("write tarae's config", "$H/.config/tarae/config.toml", true),
    ("write a shell's startup file", "$H/.zshrc", true),
    ("write a folder of programs", "$H/.local/bin/x", true),
    ("write the project", "$P/src/a.rs", true),
    ("write a protected name", "$P/build.rs", true),
    ("write a git hook", "$P/.git/hooks/x", true),
    ("write temp", "$T/tarae-claude-probe", true),
    ("move this project's folder aside", "mv:$H/.claude/projects/$E", true),
    ("move the projects folder aside", "mv:$H/.claude/projects", true),
    ("move its folder aside", "mv:$H/.claude", true),
];

/// Where `mv:` moves it: `<ASIDE>-<its index in TRIES>` in temp.
#[cfg(target_os = "macos")]
const ASIDE: &str = "tarae-claude-aside";

/// Where the try `name` moves its folder (`ASIDE`).
#[cfg(target_os = "macos")]
fn aside(tmp: &Path, name: &str) -> PathBuf {
    tmp.join(format!("{ASIDE}-{}", TRIES.iter().position(|t| t.0 == name).unwrap()))
}

/// The stand-in's script: each try in `TRIES`, `$H` home, `$P` the project, `$T` the temp folder's own spot.
#[cfg(target_os = "macos")]
fn script() -> String {
    let mut s = String::new();
    for (i, (name, path, write)) in TRIES.into_iter().enumerate() {
        let op = match (path.strip_prefix("mv:"), write) {
            (Some(path), _) => format!("mv \"{path}\" \"$T/{ASIDE}-{i}\""),
            (None, true) => format!("echo x >>\"{path}\""),
            (None, false) => format!("cat \"{path}\" >/dev/null"),
        };
        s.push_str(&format!(
            "if {op} 2>/dev/null; then echo \"{name} ok\"; else echo \"{name} refused\"; fi\n"
        ));
    }
    s
}

/// The paths `TRIES` names, as the stand-in sees them.
#[cfg(target_os = "macos")]
fn tried(h: &Home, tmp: &Path) -> Vec<(&'static str, PathBuf, bool)> {
    let (home, root) = (h.home.display().to_string(), h.root.display().to_string());
    let at = |p: &str| {
        let p = p.trim_start_matches("mv:").replace("$E", &project_folder(&h.root));
        let p = p.replace("$H", &home).replace("$P", &root).replace("$T", &tmp.display().to_string());
        profile_path(Path::new(&p))
    };
    TRIES.iter().map(|(n, p, w)| (*n, at(p), *w)).collect()
}

/// What the stand-in said it could do: name → ok.
#[cfg(target_os = "macos")]
fn outcomes(out: &[u8]) -> HashMap<String, bool> {
    let out = String::from_utf8_lossy(out);
    let parse = |l: &str| {
        let ok = l.strip_suffix(" ok").map(|n| (n.to_string(), true));
        ok.or_else(|| l.strip_suffix(" refused").map(|n| (n.to_string(), false)))
    };
    out.lines().filter_map(parse).collect()
}

/// The stand-in started as the chat starts claude (`llm::spawn` under the profile, in the project) — its
/// outcomes.
#[cfg(target_os = "macos")]
fn as_claude(h: &Home, p: &Profile, tmp: &Path) -> HashMap<String, bool> {
    let cfg = crate::llm::LlmConfig {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), script()],
        ..Default::default()
    };
    let env = HashMap::from([
        ("H".to_string(), h.home.display().to_string()),
        ("P".to_string(), h.root.display().to_string()),
        ("T".to_string(), tmp.display().to_string()),
        ("E".to_string(), project_folder(&h.root)),
    ]);
    let cred = crate::billing::Credential::Subscription;
    let child = crate::llm::spawn(&cfg, &[], &h.root, Some(&env), &cred, Some(p)).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    outcomes(&out.stdout)
}

/// The stand-in under `text` as a profile (a broken one) with `p`'s parameters, straight from `sandbox-exec`.
#[cfg(target_os = "macos")]
fn under_text(h: &Home, p: &Profile, text: &str, tmp: &Path) -> HashMap<String, bool> {
    let mut c = Command::new(super::super::SANDBOX_EXEC);
    c.args(["-p", text]);
    for (k, v) in p.params() {
        c.args(["-D".to_string(), format!("{k}={v}")]);
    }
    c.args(["/bin/sh", "-c", &script()]).current_dir(&h.root);
    c.env("H", &h.home).env("P", &h.root).env("T", tmp).env("E", project_folder(&h.root));
    outcomes(&c.output().unwrap().stdout)
}

/// The claude process under its own profile (V1d), as the chat starts it: it can't read a key — a key folder in
/// home, a file named like a key in the project — nor write what runs later or loosens tarae (tarae's config, a
/// shell's startup file, a folder of programs — its own updater's link among them), nor the project in the default
/// arm (tarae's `edit` writes it); it reads the project and its own, and writes its own (`~/.claude/`, the temp
/// copy renamed over `~/.claude.json`) and temp. With Edit and Write (the other arms) it writes the project, but
/// a protected name or `.git`. The kernel's answer and the profile's rules in Rust (`reads_allow`,
/// `writes_allow`) agree on every one. Seen broken: the key folder not kept reads it; the write allowlist
/// widened to home writes `.zshrc`.
#[test]
#[cfg(target_os = "macos")]
fn claude_cant_read_a_key() {
    if crate::sandbox::skipped_inside("sandbox::claude::tests::claude_cant_read_a_key") {
        return;
    }
    let h = Home::new("key");
    let tmp = h.base.parent().unwrap().to_path_buf();
    let tried = tried(&h, &tmp);
    let expect = |writes_project: bool, name: &str| match name {
        "read the project"
        | "read its own config"
        | "write its own folder"
        | "write its config's temp copy"
        | "write a plugin's in-use mark"
        | "write the synced marketplace list"
        | "write its own conversation" => true,
        "write temp" => true,
        "write the project" => writes_project,
        _ => false,
    };
    for writes_project in [false, true] {
        let p = h.profile(writes_project);
        let seen = as_claude(&h, &p, &tmp);
        for (name, path, write) in &tried {
            let ok = seen.get(*name).copied();
            assert_eq!(ok, Some(expect(writes_project, name)), "{name} (project writable: {writes_project})");
            let rule = if *write { p.writes_allow(path) } else { p.reads_allow(path) };
            assert_eq!(rule, expect(writes_project, name), "the rule in Rust: {name} ({})", path.display());
        }
        std::fs::remove_file(tmp.join("tarae-claude-probe")).ok();
    }
    // A project that holds home, Edit and Write on: home's deny stays over it — nothing in home but claude's own
    let around = Profile::new(&h.home, &h.home, true, &h.here(), "tarae-claude.test.");
    let seen = as_claude(&h, &around, &tmp);
    for (name, path, write) in &tried {
        let rule = if *write { around.writes_allow(path) } else { around.reads_allow(path) };
        assert_eq!(
            seen.get(*name).copied(),
            Some(rule),
            "holding home: {name} — the kernel and the rule in Rust"
        );
    }
    for name in [
        "write a shell's startup file",
        "write the project",
        "write tarae's config",
        "write a folder of programs",
    ] {
        assert_eq!(seen.get(name), Some(&false), "holding home: {name}");
    }
    assert_eq!(seen.get("write its own folder"), Some(&true), "holding home: claude's own");
    std::fs::remove_file(tmp.join("tarae-claude-probe")).ok();
    // Broken once, each: the key folder not kept, the write allowlist widened to home
    let p = h.profile(false);
    let unkept = Profile { kept: Vec::new(), ..p.clone() };
    assert_eq!(under_text(&h, &unkept, &unkept.text(), &tmp).get("read a key folder"), Some(&true));
    let home_deny = "(deny file-write* (subpath (param \"HOME\"))";
    let widened: String =
        p.text().lines().filter(|l| !l.starts_with(home_deny)).map(|l| format!("{l}\n")).collect();
    assert_ne!(widened, p.text(), "the home's deny is there to take out");
    assert_eq!(under_text(&h, &p, &widened, &tmp).get("write a shell's startup file"), Some(&true));
    std::fs::remove_file(tmp.join("tarae-claude-probe")).ok();
    // …and each folder claude's own allow covers whose children a deny names: its own path not kept, it moves
    // aside whole (put back after) — `~/.claude`, `projects/`, this project's
    let claude = h.home.join(".claude");
    let folders = [
        ("O0", "move its folder aside", claude.clone()),
        ("PJ", "move the projects folder aside", claude.join("projects")),
        ("OP", "move this project's folder aside", claude.join("projects").join(project_folder(&h.root))),
    ];
    for (param, name, back) in folders {
        let deny = format!("(deny file-write* (literal (param \"{param}\"))");
        let movable: String =
            p.text().lines().filter(|l| !l.starts_with(&deny)).map(|l| format!("{l}\n")).collect();
        assert_ne!(movable, p.text(), "{param}'s own deny is there to take out");
        assert_eq!(under_text(&h, &p, &movable, &tmp).get(name), Some(&true), "{name}, {param} not kept");
        std::fs::rename(aside(&tmp, name), &back).unwrap();
        std::fs::remove_file(tmp.join("tarae-claude-probe")).ok();
    }
}

/// This project's folder of conversations is made by tarae before claude starts (claude can't: its path is
/// denied) — through no link: a link in the way is neither gone through nor removed, and it says so.
#[test]
fn this_projects_folder_is_made_through_no_link() {
    let h = Home::new("project-made");
    let p = h.profile(false);
    let projects = h.home.join(".claude/projects");
    std::fs::remove_dir_all(&projects).unwrap();
    p.own_project_made().unwrap();
    assert!(projects.join(project_folder(&h.root)).is_dir(), "made");
    p.own_project_made().unwrap();
    // a link in the way: left as it is, nothing made where it leads
    let elsewhere = h.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::remove_dir_all(&projects).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &projects).unwrap();
    let e = p.own_project_made().expect_err("a link in the way");
    assert!(e.contains("a link or no folder there"), "{e}");
    assert!(projects.is_symlink() && std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

/// Not a check: the live one's profile (`target/sb/v1b-claude-mac.sh` with `V1B_ENFORCE=1`, outside tarae's
/// sandbox) — the chat's own (`for_chat`: your home, as `Here` says it) for the project `V1D_ROOT`, the arm
/// `V1D_WRITES` ("1": Edit and Write's), tagged `V1D_TAG`, written to `V1D_OUT` as `sandbox-exec`'s arguments in
/// one `set --` line the script sources. Writes only that file.
#[test]
#[ignore]
#[cfg(target_os = "macos")]
fn the_live_checks_profile() {
    let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("{n} unset"));
    let (root, out, tag) = (var("V1D_ROOT"), var("V1D_OUT"), var("V1D_TAG"));
    let writes = std::env::var("V1D_WRITES").is_ok_and(|w| w == "1");
    let p = for_chat(Path::new(&root), writes).expect("Seatbelt here");
    let p = Profile { tag, ..p };
    let words: Vec<String> = p.args().iter().map(|a| crate::packs::sh_quote(a)).collect();
    std::fs::write(out, format!("set -- {}\n", words.join(" "))).unwrap();
}

/// A refusal claude's tool says (Node's `EPERM: operation not permitted, open '…'`, as its Edit said it live) is
/// the profile's only where its rules refuse that path — said as writing, reading, or either; an EPERM elsewhere (a
/// writable temp file: something else refused it) or another error isn't.
#[test]
fn a_refusal_claudes_tool_says_is_the_profiles_where_its_rules_refuse() {
    let h = Home::new("said-in-result");
    let p = h.profile(false);
    let said =
        |path: &Path| p.said_in_result(&format!("EPERM: operation not permitted, open '{}'", path.display()));
    let edit = said(&h.root.join("src/a.rs.tmp.93827.3ad8")).expect("the project, read-only in this arm");
    assert_eq!((edit.op.as_str(), edit.plain().starts_with("writing ")), ("file-write*", true), "{edit:?}");
    let key = said(&h.home.join(".ssh/id_x")).expect("a key folder: neither read nor written");
    assert_eq!(key.op, crate::sandbox::EITHER);
    assert!(said(&profile_path(&std::env::temp_dir()).join("x")).is_none(), "temp: not the profile's");
    assert!(p.said_in_result("ENOENT: no such file or directory, open '/x'").is_none());
    assert!(
        h.profile(true)
            .said_in_result(&format!("EPERM: operation not permitted, open '{}/src/a.rs'", h.root.display()))
            .is_none()
    );
}

/// In its own folder claude writes what it wrote in V1b — conversations, sessions, plugins' in-use marks, their
/// lists' locks, the synced list and its round's time — but never what your own Claude Code runs or obeys later:
/// settings, memory (`CLAUDE.md`, a project's auto-memory), commands, agents, skills, hooks, output styles,
/// plugins themselves, anything else under `plugins/synced/` — in any case.
#[test]
fn claudes_own_folder_keeps_what_your_claude_runs_later() {
    let h = Home::new("runs-later");
    let p = h.profile(false);
    let c = profile_path(&h.home.join(".claude"));
    for ok in [
        "sessions/1.json",
        "plugins/cache/m/p/1.0.0/.in_use/23307.tmp.1",
        "plugins/known_marketplaces.json.lock",
        "plugins/synced/a_b/.marketplaces.json",
        "plugins/synced/a_b/.marketplaces.json.tmp.1eac9c06",
        "plugins/synced/a_b/.last-complete-round",
    ] {
        assert!(p.writes_allow(&c.join(ok)), "{ok}");
    }
    // this project's conversations, but its auto-memory; another project's none
    let own = c.join("projects").join(project_folder(&p.root));
    assert!(p.writes_allow(&own.join("1.jsonl")) && p.writes_allow(&own.join("memoryless.jsonl")));
    assert!(
        !p.writes_allow(&own.join("memory/MEMORY.md")) && !p.writes_allow(&c.join("projects/-x/1.jsonl"))
    );
    assert!(!p.writes_allow(&c), "~/.claude itself: not moved aside nor made again");
    assert!(
        !p.writes_allow(&c.join("projects")) && !p.writes_allow(&own),
        "projects/ and this project's: never moved whole"
    );
    for kept in [
        "settings.json",
        "Settings.Local.json",
        "CLAUDE.md",
        "commands/x.md",
        "agents/x.md",
        "skills/x/SKILL.md",
        "hooks/x.sh",
        "output-styles/x.md",
        "plugins/installed_plugins.json",
        "plugins/cache/m/p/1.0.0/hooks/hooks.json",
        "plugins/.lock",
        "plugins/synced/a_b/hooks/hooks.json",
        "plugins/synced/a_b/x/plugin.json",
        "projects/-x/memory",
        "projects/-x/memory/MEMORY.md",
        "Projects/-x/Memory/note.md",
        "local/claude",
        "ide/1.lock",
        "Shell-Snapshots/s.sh",
    ] {
        assert!(!p.writes_allow(&c.join(kept)), "{kept}");
    }
}

/// A project that holds home (home itself, or above), Edit and Write on, opens nothing in home but claude's own:
/// home's deny comes again after the project's allow, claude's own after that — your shell's startup files,
/// tarae's config, a folder of programs, `~/.ssh` stay closed; outside home the project is writable.
#[test]
fn a_project_holding_home_opens_no_home() {
    let h = Home::new("holding-home");
    let p = Profile::new(&h.home, &h.home, true, &h.here(), "tarae-claude.test.");
    for kept in
        [".zshrc", ".config/tarae/config.toml", ".local/bin/x", ".ssh/authorized_keys", "work/proj/src/a.rs"]
    {
        assert!(!p.writes_allow(&h.home.join(kept)), "{kept}");
    }
    assert!(p.writes_allow(&profile_path(&h.home.join(".claude")).join("sessions/1.json")), "claude's own");
    let text = p.text();
    let at = |s: &str| text.rfind(s).unwrap_or_else(|| panic!("{s} in {text}"));
    let (project, home, own) = (
        at("(allow file-write* (subpath (param \"ROOT\")))"),
        at("(deny file-write* (subpath (param \"HOME\"))"),
        at("(allow file-write* (subpath (param \"O0\"))"),
    );
    assert!(project < home && home < own, "the project's allow, home's deny again, claude's own");
}

/// `~/.claude.json` and what claude writes beside it (V1b) are its own — a name that only starts so isn't.
#[test]
fn claudes_config_and_its_copies_are_its_own() {
    for own in [".claude.json", ".claude.json.lock", ".claude.json.tmp.23307.c944", ".claude.json.backup"] {
        assert!(own_file(Path::new(own)), "{own}");
    }
    for not in [".claude.jsonx", ".claude.json.tmp.", ".claude.json.evil", ".claude.js"] {
        assert!(!own_file(Path::new(not)), "{not}");
    }
}

/// What the profile keeps from claude, a yes on a card couldn't open: the read is answered at once, saying why
/// and what the user can do instead — a project's file or claude's own goes on as before. Not the system's
/// certificates, though named like keys (`*.pem`): every TLS connection reads them.
#[test]
fn a_read_claudes_profile_refuses_is_said_not_asked() {
    let h = Home::new("refused-read");
    let p = h.profile(false);
    let at = |rel: &str| crate::reads::Look {
        tool: "Read".into(),
        at: Some(h.home.join(rel)),
        pattern: String::new(),
        filter: String::new(),
        given: rel.into(),
        file: true,
        keys: Vec::new(),
    };
    let said = crate::reads::kept_from_claude(&p, &at(".ssh/id_x")).expect("a key folder");
    assert!(said.contains("~/") || said.contains(".ssh/id_x"), "{said}");
    assert!(said.contains("llm.claude-sandbox") && said.contains("C-o"), "{said}");
    assert!(crate::reads::kept_from_claude(&p, &at("work/proj/.env")).is_some(), "named like a key");
    assert!(crate::reads::kept_from_claude(&p, &at("work/proj/build.rs")).is_none(), "the project");
    assert!(crate::reads::kept_from_claude(&p, &at(".claude/settings.json")).is_none(), "its own");
    assert!(p.reads_allow(Path::new("/private/etc/ssl/cert.pem")), "the system's certificates");
}
