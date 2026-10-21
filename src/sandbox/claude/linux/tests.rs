use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::sandbox::{Here, Os, profile_path};

/// A stand-in home outside the temp folders (a temp folder holding home is split around it, so it takes no new
/// entry: never so for a real one): a fake key, your Claude's own, tarae's state, git's credentials, a shell's
/// startup file, a key at its top, and a project with protected names and a file named like a key — or (`moved`)
/// tarae's state and git's credentials outside home, where `XDG_STATE_HOME` and `XDG_CONFIG_HOME` put them. Removed
/// when dropped.
struct Fixture {
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
    moved: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

/// A folder made new in `dir` for the test (0700, `<stem>-<pid>-<n>`): never one that was there — a name taken, a
/// link there too, goes to the next. Only it is removed after.
fn made_new(dir: &Path, stem: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    (0..)
        .map(|n| dir.join(format!("{stem}-{}-{n}", std::process::id())))
        .find(|at| match std::fs::DirBuilder::new().mode(0o700).create(at) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => panic!("{}: {e}", at.display()),
        })
        .unwrap()
}

/// Every token here is a fake one, in the test's own folder.
const FILES: [(&str, &str); 13] = [
    (".ssh/id_x", "not a key\n"),
    (".config/git/credentials", "https://u:tarae-fake-token@example.invalid\n"),
    (".env", "API_KEY=tarae-fake-token\n"),
    (".claude/settings.json", "{}\n"),
    (".claude/.credentials.json", "{\"token\":\"tarae-fake-token\"}\n"),
    (".claude.json", "{}\n"),
    (".bashrc", ""),
    (".local/state/tarae/sessions/s.json", "{}\n"),
    ("code/other/.env", "API_KEY=tarae-fake-token\n"),
    ("work/proj/Cargo.toml", "[package]\nname = \"p\"\n"),
    ("work/proj/.env", "API_KEY=tarae-fake-token\n"),
    ("work/proj/.git/hooks/.keep", ""),
    ("work/proj/src/a.rs", "fn a() {}\n"),
];

/// …and outside home, in a `moved` one (below its folder).
const MOVED: [(&str, &str); 2] = [
    ("state/tarae/sessions/s.json", "{}\n"),
    ("config/git/credentials", "https://u:tarae-fake-token@example.invalid\n"),
];

impl Fixture {
    fn new(name: &str) -> Fixture {
        Fixture::made(name, false)
    }

    fn moved(name: &str) -> Fixture {
        Fixture::made(name, true)
    }

    /// Both, for a test that holds for each.
    fn both(name: &str) -> [Fixture; 2] {
        [Fixture::new(name), Fixture::moved(&format!("{name}-moved"))]
    }

    fn made(name: &str, moved: bool) -> Fixture {
        let tmp = std::env::temp_dir();
        let tmp = if tmp.starts_with("/tmp") { PathBuf::from("/var/tmp") } else { tmp };
        let base = profile_path(&made_new(&tmp, &format!("tarae-claude-linux-{name}")));
        let home = base.join("home");
        for (rel, text) in FILES {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        for (rel, text) in MOVED.iter().filter(|_| moved) {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let root = home.join("work/proj");
        Fixture { base, home, root, moved }
    }

    /// What `Here` says on Linux: its home, no `TMPDIR` — and where moved, the state and config homes.
    fn here(&self) -> Here {
        let mut env = HashMap::from([("HOME".to_string(), self.home.clone().into())]);
        if self.moved {
            env.insert("XDG_STATE_HOME".into(), self.base.join("state").into());
            env.insert("XDG_CONFIG_HOME".into(), self.base.join("config").into());
        }
        Here { env, tmp: None, uid: 1000, os: Os::Linux, ..Here::default() }
    }

    /// tarae's state (kept), and git's config folder (its credentials kept).
    fn state(&self) -> PathBuf {
        if self.moved { self.base.join("state/tarae") } else { self.home.join(".local/state/tarae") }
    }

    fn config(&self) -> PathBuf {
        if self.moved { self.base.join("config") } else { self.home.join(".config") }
    }

    fn profile(&self, writes_project: bool) -> Profile {
        Profile::new(&self.root, &self.home, writes_project, &self.here(), "tl-test")
    }

    /// Its protected names and keys, as `linux::there` finds them (checked there on Linux).
    fn protected(&self) -> Vec<PathBuf> {
        vec![self.root.join(".git"), self.root.join("Cargo.toml")]
    }

    fn keys(&self) -> Vec<PathBuf> {
        vec![self.root.join(".env")]
    }

    /// claude's folder (your `~/.claude`) and its lock folders — where moved, `$XDG_STATE_HOME`'s too.
    fn cfg(&self) -> PathBuf {
        self.home.join(".claude")
    }

    fn locks(&self) -> Vec<PathBuf> {
        let mut locks = vec![self.home.join(".local/state/claude")];
        locks.extend(self.moved.then(|| self.base.join("state/claude")));
        crate::sorting::sort(&mut locks);
        locks
    }

    /// What's kept from claude's reads in either: keys, tarae's state, git's credentials, the keys at home's top
    /// and the project's.
    fn kept(&self) -> Vec<PathBuf> {
        let home = [".ssh/id_x", ".env"];
        let mut v: Vec<PathBuf> = home.iter().map(|r| self.home.join(r)).collect();
        v.extend([self.state().join("sessions/s.json"), self.config().join("git/credentials")]);
        v.push(self.root.join(".env"));
        v
    }
}

/// `p` with what its rules were made around (`Rules::protected`) — its rules written down, never made.
fn with_protected(p: Profile, protected: Vec<PathBuf>) -> Profile {
    let rules = Rules {
        protected,
        #[cfg(target_os = "linux")]
        ruleset: crate::landlock::Ruleset::written(3).unwrap(),
        #[cfg(target_os = "linux")]
        audit: Err("not watched".into()),
    };
    Profile { rules: Some(std::sync::Arc::new(rules)), ..p }
}

#[test]
fn claudes_grants_read_all_but_whats_kept_and_write_its_own() {
    for f in Fixture::both("grants") {
        claudes_grants_in(&f);
    }
}

fn claudes_grants_in(f: &Fixture) {
    for writes in [false, true] {
        let p = f.profile(writes);
        let g = p.grants(&f.protected(), &f.keys());
        let at = |path: &Path| -> Vec<&Grant> { g.iter().filter(|x| x.path == path).collect() };
        // Everything read but home, the terminals and what's kept outside home
        let [all] = at(Path::new("/"))[..] else { panic!("{g:#?}") };
        assert_eq!(all.right, Right::Read);
        assert_eq!(all.holes[..3], [f.home.clone(), "/dev/pts".into(), "/dev/tty".into()]);
        // In home all but what's kept: keys, tarae's state, git's credentials — wherever those are — and the keys
        // at its top and the project's; your Claude's own read (its settings, CLAUDE.md, its login)
        let [home] = at(&f.home)[..] else { panic!("{g:#?}") };
        assert_eq!(home.right, Right::Read);
        let holes: Vec<&PathBuf> = all.holes.iter().chain(&home.holes).collect();
        assert!(home.holes.contains(&f.home.join(".ssh")), "{:#?}", home.holes);
        for own in [f.home.join(".claude"), f.home.join(".claude.json")] {
            assert!(!home.holes.iter().any(|h| own.starts_with(h)), "{}: {:#?}", own.display(), home.holes);
        }
        for kept in [f.state(), f.config().join("git/credentials")] {
            let (outside, inside) = (all.holes.contains(&kept), home.holes.contains(&kept));
            assert!(outside != inside && outside == f.moved, "{}: {holes:#?}", kept.display());
        }
        assert_eq!(home.keys, [f.root.join(".env"), f.home.join(".env")]);
        // Its own folders whole — `~/.claude` too: its login is refreshed there — and `~/.claude.json` as a file
        for own in std::iter::once(f.cfg()).chain(f.locks()) {
            let [g] = at(&own)[..] else { panic!("{}: {g:#?}", own.display()) };
            assert_eq!((g.right, g.holes.len()), (Right::All, 0));
        }
        let [json] = at(&f.home.join(".claude.json"))[..] else { panic!("{g:#?}") };
        assert_eq!(json.right, Right::File);
        // The project: nothing in the default arm; around its protected names in the others (N1)
        let project: Vec<&Grant> = g.iter().filter(|x| x.path.starts_with(&f.root)).collect();
        match writes {
            false => assert!(project.is_empty(), "{project:#?}"),
            true => {
                let [p] = project[..] else { panic!("{project:#?}") };
                assert_eq!(
                    (p.right, &p.holes[..], &p.keys[..]),
                    (Right::All, &f.protected()[..], &f.keys()[..])
                );
            }
        }
        // Nothing else of home is written
        let writes_home: Vec<&Path> = g
            .iter()
            .filter(|x| !matches!(x.right, Right::Read | Right::ReadFile) && x.path.starts_with(&f.home))
            .map(|x| x.path.as_path())
            .collect();
        let own: Vec<PathBuf> = [f.cfg(), f.home.join(".claude.json")].into_iter().chain(f.locks()).collect();
        assert!(
            writes_home.iter().all(|w| own.iter().any(|o| o == w) || writes && *w == f.root),
            "{writes_home:?}"
        );
    }
}

#[test]
fn claudes_rules_in_rust_are_its_grants() {
    for f in Fixture::both("rust") {
        claudes_rules_in_rust_in(&f);
    }
}

fn claudes_rules_in_rust_in(f: &Fixture) {
    for writes in [false, true] {
        let p = with_protected(f.profile(writes), f.protected());
        let reads = |rel: &str| p.reads_allow(&f.home.join(rel));
        let write = |rel: &str| p.writes_allow(&f.home.join(rel));
        // Kept from its reads, in home or outside it: keys, tarae's state, git's credentials, the keys at home's top
        // and the project's
        for kept in f.kept() {
            assert!(!p.reads_allow(&kept), "{}", kept.display());
            // (a key in the project is written where the project is: its folder's rule, all but reading files)
            assert!(kept.starts_with(&f.root) || !p.writes_allow(&kept), "{}", kept.display());
        }
        assert!(reads(".claude/settings.json") && reads(".claude.json") && reads("work/proj/src/a.rs"));
        // (the gap: a key deeper in home than its top — the module's note)
        assert!(reads("code/other/.env"));
        // Its writes: `~/.claude` whole — its login refreshed by a lock folder at its top and the store renamed
        // over; what your own Claude Code runs later there too (G25) —, `~/.claude.json` in place, its lock folders;
        // nothing else in home: not what a shell runs, nor `~/.claude.json`'s temp file or lock beside it
        for own in
            [".claude/.oauth_refresh.lock", ".claude/.credentials.json.tmp.1.ab", ".claude/settings.json"]
        {
            assert!(write(own), "{own}");
        }
        assert!(write(".claude.json") && f.locks().iter().all(|l| p.writes_allow(&l.join("locks/x"))));
        for kept in [".bashrc", ".claude.json.tmp.1.ab", ".claude.json.lock", ".claude.lock"] {
            assert!(!write(kept), "{kept}");
        }
        // The project per arm, never its protected names; a file made where one is: refused (N1)
        assert_eq!(write("work/proj/src/a.rs"), writes);
        assert_eq!(write("work/proj/src/a.rs.tmp.1.ab"), writes);
        assert!(!write("work/proj/Cargo.toml") && !write("work/proj/.git/hooks/x"));
        assert!(!write("work/proj/README.md.tmp.1.ab"), "the top holds .git: no new entry");
        // Temp and the devices but the terminals
        assert!(p.writes_allow(&profile_path(Path::new(crate::sandbox::TMP)).join("cc-socks/1.sock")));
        assert!(p.writes_allow(Path::new("/dev/null")) && p.reads_allow(Path::new("/etc/hosts")));
        for terminal in ["/dev/pts/0", "/dev/tty"] {
            assert!(
                !p.reads_allow(Path::new(terminal)) && !p.writes_allow(Path::new(terminal)),
                "{terminal}"
            );
        }
    }
}

/// A claude program in a place the rules keep (here `~/.ssh`; the old local install in `~/.claude/local/` is read
/// now) is told, not started under rules that refuse running it: `for_chat` leaves claude unconfined then, said.
#[test]
fn a_claude_program_its_rules_keep_is_told() {
    let f = Fixture::new("program");
    let (local, native) = (f.home.join(".ssh/bin"), f.home.join(".claude/local"));
    for dir in [&local, &native] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("claude"), "#!/bin/sh\n").unwrap();
    }
    let on = |dir: &Path| {
        let mut here = f.here();
        here.env.insert("PATH".into(), dir.as_os_str().to_owned());
        here
    };
    let p = f.profile(false);
    assert_eq!(p.program_kept("claude", &on(&local)), Some(local.join("claude")));
    assert_eq!(p.program_kept("claude", &on(&native)), None);
    // (a link into the kept place is where it leads)
    std::os::unix::fs::symlink(local.join("claude"), f.root.join("claude-link")).unwrap();
    assert_eq!(p.program_kept("./claude-link", &f.here()), Some(local.join("claude")));
    assert_eq!(p.program_kept("no-such-claude", &on(&native)), None);
}

#[test]
fn a_refusal_in_landlocks_words_is_said() {
    let f = Fixture::new("said");
    let h = f.home.display();
    let p = with_protected(f.profile(true), f.protected());
    let said = |text: String| p.said_in_result(&text).map(|d| (d.op, d.target));
    let target = |rel: &str| f.home.join(rel).display().to_string();
    // A read of a key: closed both ways
    let key = said(format!("EACCES: permission denied, open '{h}/.ssh/id_x'"));
    assert_eq!(key, Some((crate::sandbox::EITHER.into(), target(".ssh/id_x"))));
    // Edit's file beside the one it changes, at the project's top (it holds .git): a write
    let edit = said(format!("EACCES: permission denied, open '{h}/work/proj/README.md.tmp.7.ab'"));
    assert_eq!(edit, Some(("file-write*".into(), target("work/proj/README.md.tmp.7.ab"))));
    // A move across folders: the one of its two paths the rules refuse
    let tmp = profile_path(Path::new(crate::sandbox::TMP));
    let moved = said(format!(
        "EXDEV: cross-device link not permitted, rename '{}/x' -> '{h}/work/proj/Cargo.toml'",
        tmp.display()
    ));
    assert_eq!(moved, Some(("file-write*".into(), target("work/proj/Cargo.toml"))));
    // What the rules allow, or no refusal at all: nothing
    assert_eq!(said(format!("EACCES: permission denied, open '{h}/work/proj/src/a.rs.tmp.1.ab'")), None);
    assert_eq!(said(format!("ENOENT: no such file or directory, open '{h}/.ssh/id_x'")), None);
}

/// The grants as Landlock rules (written down, not made): no rule reaches what's kept, claude's folder whole.
#[cfg(target_os = "linux")]
#[test]
fn claudes_landlock_rules_reach_nothing_kept() {
    for f in Fixture::both("rules") {
        claudes_landlock_rules_in(&f);
    }
}

#[cfg(target_os = "linux")]
fn claudes_landlock_rules_in(f: &Fixture) {
    f.profile(false).own_made().unwrap();
    // (what `there` finds is what the grants are made around)
    let there = crate::sandbox::linux::there(&f.root, &[], &[]);
    let mut protected = there.protected.clone();
    crate::sorting::sort(&mut protected);
    assert_eq!((protected, there.keys.clone()), (f.protected(), f.keys()));
    for writes in [false, true] {
        let p = f.profile(writes);
        let r = rules_in(crate::landlock::Ruleset::written(6).unwrap(), &p.grants(&f.protected(), &f.keys()))
            .unwrap();
        // (no rule at or below what's kept, in home or outside it; none on home, tarae's state or the terminals)
        let closed: Vec<PathBuf> = [".ssh", ".env", "work/proj/.env"]
            .iter()
            .map(|c| f.home.join(c))
            .chain([f.state().join("sessions"), f.config().join("git/credentials"), "/dev/pts".into()])
            .collect();
        for (path, _) in &r.added {
            assert!(!closed.iter().any(|c| path.starts_with(c)), "{}", path.display());
            let whole = [f.home.clone(), f.state(), "/dev/tty".into()];
            assert!(!whole.contains(path), "{}", path.display());
        }
        // claude's folder has two rules, which Landlock takes together: home's read (it isn't kept) and its own
        // grant, all — REFER in it. Moving a file between folders is what runs C–R were measured with (the stand-in
        // confiner's "all"; R saw only the refresh's rename within one folder, not that no other crosses one), and
        // it gives a hostile claude nothing: in `~/.claude` it writes every name directly anyway, and across grants
        // the kernel wants the move's rights on both sides and no rights gained — nothing comes out of a kept place
        // (no rule there) nor lands on a protected name (a hole)
        let cfg = r.added.iter().filter(|(path, _)| *path == f.cfg()).fold(0, |all, (_, access)| all | access);
        assert_ne!(cfg, 0, "claude's folder");
        assert_eq!(cfg & crate::landlock::REFER, crate::landlock::REFER);
        let written = |path: &Path| {
            r.added.iter().any(|(at, access)| path.starts_with(at) && access & crate::landlock::MAKE_REG != 0)
        };
        assert_eq!(written(&f.root.join("src/a.rs")), writes);
        assert!(!written(&f.root.join(".git/hooks/x")) && !written(&f.root.join("Cargo.toml")));
    }
}

/// The kernel's ABI for a test under real rules, or None after saying it skips itself.
#[cfg(target_os = "linux")]
fn kernel_abi(test: &str) -> Option<u32> {
    match crate::landlock::abi() {
        Ok(n) if n >= MIN_ABI => Some(n),
        found => {
            let line = format!(
                "{}sandbox::claude::linux::tests::{test} — no Landlock claude is confined under here ({found:?})\n",
                crate::test_results::SELF_SKIP
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            None
        }
    }
}

/// What the stand-in claude does, each a line "<name> ok" or "<name> refused" in its marker file: what claude was
/// seen doing (`target/sb/v1-linux-proposal.md` §1), and what it must never.
#[cfg(target_os = "linux")]
const STAND_IN: &str = r#"
set -u
t() { name=$1; shift; if "$@" >/dev/null 2>&1; then echo "$name ok"; else echo "$name refused"; fi >> "$OUT"; }
t "its refresh's lock folder" sh -c 'mkdir "$CFG/.oauth_refresh.lock" && rmdir "$CFG/.oauth_refresh.lock"'
t "its login's temp copy" sh -c 'echo "{}" > "$CFG/.credentials.json.tmp.1.ab"'
t "its login renamed over" mv "$CFG/.credentials.json.tmp.1.ab" "$CFG/.credentials.json"
t "its config written in place" sh -c 'echo "{}" > "$HOME/.claude.json"'
t "its config's temp copy in home" sh -c 'echo "{}" > "$HOME/.claude.json.tmp.1.ab"'
t "its config's lock in home" mkdir "$HOME/.claude.json.lock"
t "the legacy refresh lock in home" mkdir "$HOME/.claude.lock"
t "its version lock" sh -c 'mkdir -p "$LOCKS/locks" && : > "$LOCKS/locks/2.1.296.lock"'
t "a rename through its folder's descriptor" sh -c 'exec 3< "$CFG" && : > "$CFG/b.tmp" && mv /proc/self/fd/3/b.tmp /proc/self/fd/3/b'
t "mkdir on home" sh -c 'mkdir "$HOME" 2>&1 | grep -q "File exists"'
t "read the project" cat "$P/src/a.rs"
t "read a key" cat "$HOME/.ssh/id_x"
t "read its login" cat "$HOME/.claude/.credentials.json"
t "read a key-named file in the project" cat "$P/.env"
t "read tarae's state" cat "$STATE/sessions/s.json"
t "read git's credentials" cat "$GITCFG/git/credentials"
t "write a shell's startup file" sh -c 'echo x >> "$HOME/.bashrc"'
t "write your Claude settings" sh -c 'echo x > "$HOME/.claude/settings.json"'
t "write the project" sh -c 'echo x >> "$P/src/a.rs"'
"#;

/// A stand-in claude in `sh` under the real rules does what claude was seen doing (run R) — its login's refresh
/// (the lock folder at `~/.claude`'s top, the store's temp copy renamed over), `~/.claude.json` written in place,
/// its socket's bind, the fswatch probe, a rename through `/proc/self/fd/N`, mkdir on home — and is refused
/// `~/.claude.json`'s temp copy and lock in home and the legacy refresh lock (all three tolerated by claude), a fake
/// key, a write to a shell's startup file, tarae's state and git's credentials — in home, and moved outside it.
/// Your Claude settings it writes (G25: Landlock grants `~/.claude` whole). The bind and the probe from the confined thread itself (no program
/// here makes a socket); the rest by markers in its temp folder.
#[cfg(target_os = "linux")]
#[test]
fn a_stand_in_claude_does_what_claude_does_and_no_more() {
    let Some(abi) = kernel_abi("a_stand_in_claude_does_what_claude_does_and_no_more") else { return };
    for f in Fixture::both("enforced") {
        stand_in(&f, abi);
    }
}

#[cfg(target_os = "linux")]
fn stand_in(f: &Fixture, abi: u32) {
    let p = f.profile(false);
    p.own_made().unwrap();
    // Its temp folder: markers, the socket, the probe (in the temp folder claude writes, apart from home)
    let t = made_new(Path::new("/tmp"), &format!("tarae-claude-enforced-{}", f.moved));
    let out = t.join("marks");
    let grants = p.grants(&f.protected(), &f.keys());
    let rules = rules_in(crate::landlock::Ruleset::files(abi).unwrap(), &grants).unwrap();
    let (sock, probe) = (t.join("cc-socks/1.sock"), t.join("claude-1000/fswatch-probe-x"));
    let started = crate::landlock::confined(
        &rules,
        || Ok(()),
        |()| {
            // (claude's own calls, in its process: its socket bound, the probe made and watched)
            std::fs::create_dir_all(sock.parent().unwrap())?;
            let bound = std::os::unix::net::UnixListener::bind(&sock).map(drop);
            std::fs::create_dir_all(&probe)?;
            std::fs::write(probe.join("probe"), "")?;
            // SAFETY: a new inotify descriptor of ours, closed here; the path lives for the call.
            let watched = unsafe {
                let fd = libc::inotify_init1(libc::IN_CLOEXEC);
                let c = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes()).unwrap();
                let w = libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_CREATE | libc::IN_MODIFY);
                libc::close(fd);
                w
            };
            let child = std::process::Command::new("/bin/sh")
                .args(["-c", STAND_IN])
                .env("HOME", &f.home)
                .env("CFG", f.cfg())
                .env("LOCKS", &f.locks()[0])
                .env("STATE", f.state())
                .env("GITCFG", f.config())
                .env("P", &f.root)
                .env("OUT", &out)
                .output();
            Ok((bound, watched, child?))
        },
    );
    let (bound, watched, child) = started.unwrap();
    let marks = std::fs::read_to_string(&out).unwrap_or_default();
    std::fs::remove_dir_all(&t).ok();
    assert!(bound.is_ok(), "its socket's bind: {bound:?}");
    assert!(watched >= 0, "the fswatch probe's watch");
    let mut want = String::new();
    for (name, ok) in [
        ("its refresh's lock folder", true),
        ("its login's temp copy", true),
        ("its login renamed over", true),
        ("its config written in place", true),
        ("its config's temp copy in home", false),
        ("its config's lock in home", false),
        ("the legacy refresh lock in home", false),
        ("its version lock", true),
        ("a rename through its folder's descriptor", true),
        ("mkdir on home", true),
        ("read the project", true),
        ("read a key", false),
        ("read its login", true),
        ("read a key-named file in the project", false),
        ("read tarae's state", false),
        ("read git's credentials", false),
        ("write a shell's startup file", false),
        ("write your Claude settings", true),
        ("write the project", false),
    ] {
        want += &format!("{name} {}\n", if ok { "ok" } else { "refused" });
    }
    assert_eq!(marks, want, "moved: {}, stderr: {}", f.moved, String::from_utf8_lossy(&child.stderr));
    // (nothing it was refused changed)
    assert_eq!(std::fs::read_to_string(f.home.join(".bashrc")).unwrap(), "");
    assert!(!f.home.join(".claude.lock").exists() && !f.home.join(".claude.json.lock").exists());
}

/// The path tarae starts claude by (`claude::spawn` → `linux::spawn`): its confining thread is named by the
/// profile's tag (what Landlock's domain record names — `audit_of`) and confined; claude is confined, with
/// `no_new_privs`; the thread that called it isn't; and a profile without its rules starts nothing.
#[cfg(target_os = "linux")]
#[test]
fn claude_is_started_confined_by_its_tagged_thread() {
    let Some(abi) = kernel_abi("claude_is_started_confined_by_its_tagged_thread") else { return };
    let f = Fixture::new("spawn");
    let p = f.profile(false);
    p.own_made().unwrap();
    let ruleset =
        rules_in(crate::landlock::Ruleset::files(abi).unwrap(), &p.grants(&f.protected(), &f.keys()))
            .unwrap();
    let rules = Rules { protected: f.protected(), ruleset, audit: Err("not watched".into()) };
    let p = Profile { rules: Some(std::sync::Arc::new(rules)), ..p };
    let key = f.home.join(".ssh/id_x");
    // The confining thread
    let (comm, refused) =
        confined(&p, || Ok((crate::landlock::comm()?, std::fs::read(&key).is_err()))).unwrap().unwrap();
    assert_eq!((comm.as_str(), refused), (p.tag.as_str(), true));
    // claude: its rules and its filter
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args([
        "-c",
        "grep -E '^(NoNewPrivs|Seccomp|Seccomp_filters):' /proc/self/status; \
         cat \"$K\" >/dev/null 2>&1 && echo read",
    ])
    .env("K", &key)
    .stdout(std::process::Stdio::piped());
    let (child, unconfined) = crate::sandbox::claude::spawn(Some(&p), false, &mut cmd).unwrap();
    let said = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
    assert_eq!(unconfined, None);
    assert!(
        said.contains("NoNewPrivs:\t1") && said.contains("Seccomp:\t2") && !said.contains("read"),
        "{said}"
    );
    // The caller, as it was
    assert!(std::fs::read(&key).is_ok());
    // claude's filter itself, not only a filter: under a container's own profile (Docker's default) the caller
    // already has one and `no_new_privs`, so the two lines above hold without claude's — its filters are counted
    // instead, one more than the caller's (`Seccomp_filters`, Linux 5.9; Landlock ABI 3 came with 6.2, so only a
    // backport lacks it — then this part says it skips)
    let filters = |status: &str| {
        let n = status.lines().find_map(|l| l.strip_prefix("Seccomp_filters:"));
        n.and_then(|n| n.trim().parse::<u32>().ok())
    };
    let caller = filters(&std::fs::read_to_string("/proc/thread-self/status").unwrap());
    match (caller, filters(&said)) {
        (Some(caller), Some(child)) => assert_eq!(child, caller + 1, "claude's filter on its own: {said}"),
        _ => {
            let line = format!(
                "{}sandbox::claude::linux::tests::claude_is_started_confined_by_its_tagged_thread — its filter \
                 uncounted: no Seccomp_filters in /proc here (a kernel before 5.9)\n",
                crate::test_results::SELF_SKIP
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        }
    }
}

/// Rules that don't go on (here no rules, and a ruleset only written down: the kernel refuses it) start nothing from
/// the confining thread: claude starts from the caller's, as you, without `no_new_privs` or a filter, and says why
/// — or, `required`, doesn't start.
#[cfg(target_os = "linux")]
#[test]
fn a_claude_whose_rules_dont_go_on_starts_as_you_from_taraes_thread() {
    let f = Fixture::new("apply");
    let written = with_protected(f.profile(false), f.protected());
    let bare = Profile { rules: None, ..written.clone() };
    // (as the caller is: a container's own filter, if any, is everyone's)
    let status = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| l.starts_with("NoNewPrivs:") || l.starts_with("Seccomp_filters:"))
            .map(String::from)
            .collect()
    };
    let own = status(&std::fs::read_to_string("/proc/thread-self/status").unwrap());
    for p in [&written, &bare] {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "cat /proc/self/status"]).stdout(std::process::Stdio::piped());
        let (child, why) = crate::sandbox::claude::spawn(Some(p), false, &mut cmd).unwrap();
        let said = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
        assert_eq!(status(&said), own, "as the caller: no no_new_privs, no filter of claude's");
        let why = why.expect("said");
        assert!(why.contains("rules didn't go on") || why.contains("weren't made"), "{why}");
        let e = crate::sandbox::claude::spawn(Some(p), true, &mut cmd).expect_err("required: not started");
        assert!(e.to_string().contains("llm.claude-sandbox-required is on"), "{e}");
    }
}

/// claude's own places are made where nothing is — `~/.claude` 0700, `~/.claude.json` `{}` 0600, the lock folders —
/// and left as they are where they're there (a link to a folder too: dotfiles); never through a link: one leading
/// nowhere makes nothing where it leads, and is refused (claude runs as you, said).
#[test]
fn claudes_own_files_are_made_through_no_link() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("own");
    std::fs::remove_dir_all(f.home.join(".claude")).unwrap();
    std::fs::remove_file(f.home.join(".claude.json")).unwrap();
    let p = f.profile(false);
    p.own_made().unwrap();
    let mode = |at: &Path| std::fs::symlink_metadata(at).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&f.home.join(".claude")), 0o700);
    assert_eq!(
        (mode(&f.home.join(".claude.json")), std::fs::read_to_string(f.home.join(".claude.json")).unwrap()),
        (0o600, "{}".into())
    );
    assert!(f.locks().iter().all(|l| l.is_dir()));
    // Made again: what's there is kept
    std::fs::write(f.home.join(".claude.json"), "{\"yours\":1}").unwrap();
    p.own_made().unwrap();
    assert_eq!(std::fs::read_to_string(f.home.join(".claude.json")).unwrap(), "{\"yours\":1}");
    // A link to nowhere at either: nothing made where it leads, refused
    for name in [".claude", ".claude.json"] {
        let f = Fixture::new(&format!("own-link{name}"));
        let marker = f.base.join("marker");
        std::fs::remove_dir_all(f.home.join(name))
            .or_else(|_| std::fs::remove_file(f.home.join(name)))
            .unwrap();
        std::os::unix::fs::symlink(&marker, f.home.join(name)).unwrap();
        let e = f.profile(false).own_made().expect_err("refused");
        assert!(e.contains(name), "{e}");
        assert!(std::fs::symlink_metadata(&marker).is_err(), "{name}: nothing made through it");
        assert!(
            std::fs::symlink_metadata(f.home.join(name)).unwrap().file_type().is_symlink(),
            "left as it is"
        );
    }
    // A link to a folder (dotfiles): used as it is
    let f = Fixture::new("own-dotfiles");
    let dotfiles = f.base.join("dotfiles-claude");
    std::fs::rename(f.home.join(".claude"), &dotfiles).unwrap();
    std::os::unix::fs::symlink(&dotfiles, f.home.join(".claude")).unwrap();
    f.profile(false).own_made().unwrap();
    assert!(dotfiles.join("settings.json").is_file());
}

/// claude's watch hears the domain its confining thread made — tarae's process, the thread named by the tag — and
/// not another's (a run's thread, another claude's).
#[cfg(target_os = "linux")]
#[test]
fn claudes_watch_hears_its_own_domain() {
    let f = Fixture::new("audit");
    let p = f.profile(false);
    let domain = |d: &str, comm: &str| {
        format!(
            "audit: type=1424 audit(1:30): domain={d} status=allocated mode=enforcing pid={} uid=1000 \
             exe=\"/usr/bin/tarae\" comm=\"{comm}\"",
            std::process::id()
        )
    };
    let access =
        |d: &str| format!("audit: type=1423 audit(1:31): domain={d} blockers=fs.read_file path=\"/k\" ino=9");
    let mut a = audit_of(&p);
    assert!(a.line(&domain("2b", "tl-other")).is_empty() && a.line(&access("2b")).is_empty(), "another's");
    assert!(a.line(&domain("1a", &p.tag)).is_empty());
    let d = a.line(&access("1a"));
    assert_eq!(d.iter().map(|d| d.target.as_str()).collect::<Vec<_>>(), ["/k"]);
}
