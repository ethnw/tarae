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

/// A stand-in SSH agent at `path`: a unix socket that takes each connection and closes it, counting them — so
/// `ssh-add -l` fails reading its answer (exit 1), where a refused connect is "Could not open a connection" (2).
#[cfg(target_os = "macos")]
struct Agent {
    path: PathBuf,
    taken: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl Agent {
    fn at(path: &Path) -> Agent {
        use std::sync::atomic::Ordering::SeqCst;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        let taken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, s) = (Arc::clone(&taken), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            for c in listener.incoming() {
                if s.load(SeqCst) {
                    return;
                }
                t.fetch_add(1, SeqCst);
                drop(c);
            }
        });
        Agent { path: path.to_path_buf(), taken, stop, thread: Some(thread) }
    }

    fn taken(&self) -> usize {
        self.taken.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(target_os = "macos")]
impl Drop for Agent {
    fn drop(&mut self) {
        // (woken by one last connection of the test's own, then gone)
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if std::os::unix::net::UnixStream::connect(&self.path).is_ok() {
            self.thread.take().map(|t| t.join());
        }
        std::fs::remove_file(&self.path).ok();
    }
}

/// A folder in `/private/tmp` named as launchd names its agent's (`com.apple.launchd.*`) — made new by the test
/// (a name taken is never reused nor removed), only it removed when dropped: emptied of what the test made first
/// (its `Agent` is dropped before it).
#[cfg(target_os = "macos")]
struct LaunchdLike(PathBuf);

#[cfg(target_os = "macos")]
impl LaunchdLike {
    fn made() -> LaunchdLike {
        use std::os::unix::fs::DirBuilderExt;
        for n in 0.. {
            let at = PathBuf::from(format!(
                "/private/tmp/com.apple.launchd.tarae-test-{}-{n}",
                std::process::id()
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&at) {
                Ok(()) => return LaunchdLike(at),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("{}: {e}", at.display()),
            }
        }
        unreachable!()
    }
}

#[cfg(target_os = "macos")]
impl Drop for LaunchdLike {
    fn drop(&mut self) {
        std::fs::remove_dir(&self.0).ok();
    }
}

/// Which of the named sockets (each a variable of that name holding its path) `ssh-add -l` reaches under `text`
/// with `p`'s parameters: name → connected.
#[cfg(target_os = "macos")]
fn agents_reached(h: &Home, p: &Profile, text: &str, socks: &[(&str, &Path)]) -> HashMap<String, bool> {
    let script: String = socks
        .iter()
        .map(|(n, _)| {
            format!(
                "SSH_AUTH_SOCK=\"${n}\" /usr/bin/ssh-add -l >/dev/null 2>&1; if [ $? = 2 ]; then echo \"{n} \
                 refused\"; else echo \"{n} ok\"; fi\n"
            )
        })
        .collect();
    let mut c = Command::new(super::super::SANDBOX_EXEC);
    c.args(["-p", text]);
    for (k, v) in p.params() {
        c.args(["-D".to_string(), format!("{k}={v}")]);
    }
    c.args(["/bin/sh", "-c", &script]).current_dir(&h.root).env_remove("SSH_AUTH_SOCK");
    c.envs(socks.iter().map(|(n, s)| (*n, *s)));
    outcomes(&c.output().unwrap().stdout)
}

/// claude can't reach your SSH agent (G20): its profile refuses connecting to the socket `SSH_AUTH_SOCK` led to
/// when tarae started — by its real path, so through a link too — and to launchd's agent sockets (a stand-in in
/// `/private/tmp/com.apple.launchd.*/Listeners`, older macOS's place: newer ones' `/private/var/run` is root's —
/// `launchds_agent_sockets_are_refused_in_both_places`), each a stand-in agent here that counts what reaches it; another
/// socket stays reachable (the profile is files only). No `SSH_AUTH_SOCK` when tarae started: launchd's still
/// refused. Seen broken: the rule taken out, every agent is reached.
#[test]
#[cfg(target_os = "macos")]
fn claude_cant_reach_your_ssh_agent() {
    if crate::sandbox::skipped_inside("sandbox::claude::tests::claude_cant_reach_your_ssh_agent") {
        return;
    }
    let h = Home::new("agent");
    let agent = Agent::at(&h.base.join("agent/a.sock"));
    std::os::unix::fs::symlink(h.base.join("agent"), h.base.join("link")).unwrap();
    let through_link = h.base.join("link/a.sock");
    let folder = LaunchdLike::made();
    let launchd = Agent::at(&folder.0.join("Listeners"));
    let other = Agent::at(&h.base.join("other.sock"));
    let socks = [
        ("agent", through_link.as_path()),
        ("launchd", launchd.path.as_path()),
        ("other", other.path.as_path()),
    ];
    // (a connection is counted once the stand-in's thread took it: waited for, by the last one the probe made)
    let counted = |want_other: usize| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while other.taken() < want_other && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        (agent.taken(), launchd.taken(), other.taken())
    };
    let mut here = h.here();
    here.env.insert("SSH_AUTH_SOCK".into(), through_link.clone().into());
    let p = Profile::new(&h.root, &h.home, false, &here, "tarae-claude.test.");
    assert_eq!(p.agent.as_deref(), Some(profile_path(&agent.path).as_path()), "its real path");
    // (the probe reaches a socket outside the profile: else "refused" would mean nothing)
    let seen = agents_reached(&h, &p, "(version 1)\n(allow default)\n", &socks);
    let all = |ok: bool| socks.iter().map(|(n, _)| (n.to_string(), ok)).collect::<HashMap<_, _>>();
    assert_eq!(seen, all(true), "unconfined, the probe reaches each");
    assert_eq!(counted(1), (1, 1, 1), "each stand-in counted its connection");
    let seen = agents_reached(&h, &p, &p.text(), &socks);
    let expect = HashMap::from([("agent".into(), false), ("launchd".into(), false), ("other".into(), true)]);
    assert_eq!(seen, expect);
    assert_eq!(counted(2), (1, 1, 2), "what the agents saw");
    // No `SSH_AUTH_SOCK` when tarae started: no parameter, launchd's still refused
    let unset = Profile::new(&h.root, &h.home, false, &h.here(), "tarae-claude.test.");
    assert_eq!(unset.agent, None);
    let seen = agents_reached(&h, &unset, &unset.text(), &socks);
    assert_eq!(seen.get("launchd"), Some(&false));
    assert_eq!(seen.get("agent"), Some(&true), "no path given, none refused");
    // Broken once: the rule taken out — every agent reached
    let rule = "(deny network-outbound (require-all (remote unix-socket)";
    let open: String = p.text().lines().filter(|l| !l.starts_with(rule)).map(|l| format!("{l}\n")).collect();
    assert_ne!(open, p.text(), "the agents' deny is there to take out");
    assert_eq!(agents_reached(&h, &p, &open, &socks), all(true), "the rule taken out");
}

/// The regex in the agents' rule (`(regex #"…")` on the profile's `network-outbound` deny).
fn agents_regex(text: &str) -> String {
    let rule = text.lines().find(|l| l.starts_with("(deny network-outbound")).expect("the agents' rule");
    let from = rule.find("(regex #\"").expect("a regex in it") + "(regex #\"".len();
    rule[from..].split('"').next().unwrap().to_string()
}

/// The profile's rule refuses launchd's agent sockets where each macOS puts them: `/private/var/run/` (27.0.1, the
/// live check's) and `/private/tmp/` (older) — and nothing beside them. The rule's own regex, read from the
/// profile's text, as the `regex` crate reads it — not Seatbelt: root's `/private/var/run` can't hold a test's
/// stand-in, so Seatbelt's own answer there is only the live check's (`target/sb/v1b-claude-mac.sh`, which probes
/// launchd's real agent under the profile without `AGENT`). Seen broken: the `/private/tmp`-only form misses
/// 27.0.1's.
#[test]
fn launchds_agent_sockets_are_refused_in_both_places() {
    let h = Home::new("launchd");
    let text = h.profile(false).text();
    let re = regex::Regex::new(&agents_regex(&text)).unwrap();
    for at in [
        "/private/var/run/com.apple.launchd.87nSqGsHm3/Listeners",
        "/private/tmp/com.apple.launchd.AbCdEf1234/Listeners",
    ] {
        assert!(re.is_match(at), "refused: {at}");
    }
    for at in [
        "/private/var/run/com.apple.launchd.x/Listeners/y",
        "/private/var/runner/com.apple.launchd.x/Listeners",
        "/private/var/run/com.apple.launchdx/Listeners",
        "/private/var/run/a/com.apple.launchd.x/Listeners",
        "/var/run/com.apple.launchd.x/Listeners",
        "/private/tmp/other.sock",
    ] {
        assert!(!re.is_match(at), "not launchd's: {at}");
    }
}

/// A home where claude never ran (no `~/.claude` — a new user, e2e's home): tarae makes `~/.claude` too, and the
/// profile is made — claude starts confined on day one, not unconfined with a note. A `~/.claude` that's a link
/// (dotfiles) is followed where it leads, as before.
#[test]
fn a_home_where_claude_never_ran_gets_its_folder_made() {
    let h = Home::new("never-ran");
    let claude = h.home.join(".claude");
    std::fs::remove_dir_all(&claude).unwrap();
    let p = h.profile(false);
    p.own_project_made().expect("made from home");
    assert!(claude.join("projects").join(project_folder(&h.root)).is_dir() && !claude.is_symlink());
    // ~/.claude a link to a folder of yours: made where it leads
    let mine = h.base.join("dotfiles-claude");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::remove_dir_all(&claude).unwrap();
    std::os::unix::fs::symlink(&mine, &claude).unwrap();
    h.profile(false).own_project_made().expect("through its real path");
    assert!(mine.join("projects").join(project_folder(&h.root)).is_dir());
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
/// one `set --` line the script sources. `V1D_NO_AGENT=1`: without the `AGENT` parameter, so launchd's agent
/// sockets are refused by the rule's regex alone (G20). Writes only that file.
#[test]
#[ignore]
#[cfg(target_os = "macos")]
fn the_live_checks_profile() {
    let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("{n} unset"));
    let (root, out, tag) = (var("V1D_ROOT"), var("V1D_OUT"), var("V1D_TAG"));
    let writes = std::env::var("V1D_WRITES").is_ok_and(|w| w == "1");
    let p = for_chat(Path::new(&root), writes).expect("Seatbelt here");
    let agent = if std::env::var("V1D_NO_AGENT").is_ok_and(|w| w == "1") { None } else { p.agent.clone() };
    let p = Profile { tag, agent, ..p };
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
