use super::*;
use crate::gitrepo::{Places, Repo, answered, global_config_for, hooked, repo};
use std::process::Command;
use std::sync::Arc;

/// A repository of a test's own, its own config set by plain git (what's tested is what tarae's git does
/// with it): `*.x` filtered by `fx`, diffed by `dx`, merged by `mx`; `*.p` by the process filter `fp`; `*.d`
/// diffed by the command of `dc`; a branch `side` whose `a.x` conflicts with main's; a bare remote `l`.
/// Every command it sets is a marker script that only touches a file in the fixture's own folder. git there
/// reads a user config of the fixture's own (`user.gitconfig`), no system config.
struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    marks: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("tarae-held-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (repo, marks) = (dir.join("repo"), dir.join("marks"));
        std::fs::create_dir_all(&marks).unwrap();
        std::fs::write(dir.join("user.gitconfig"), "").unwrap();
        global_config_for(&dir, &dir.join("user.gitconfig"));
        let f = Fixture { dir, repo, marks };
        let w = |p: &str, t: &str| std::fs::write(f.repo.join(p), t).unwrap();
        w(".gitattributes", "*.x filter=fx diff=dx merge=mx\n*.p filter=fp\n*.d diff=dc\n");
        for p in ["a.x", "b.p", "c.d", "e.n"] {
            w(p, "one\n");
        }
        f.git(&["init", "-q", "-b", "main"]);
        for (k, v) in [("user.name", "t"), ("user.email", "t@t"), ("commit.gpgSign", "false")] {
            f.git(&["config", k, v]);
        }
        f.git(&["add", "-A"]);
        f.git(&["commit", "-qm", "one"]);
        f.git(&["checkout", "-qb", "side"]);
        w("a.x", "side\n");
        f.git(&["commit", "-qam", "side"]);
        f.git(&["checkout", "-q", "main"]);
        for p in ["a.x", "c.d", "e.n"] {
            w(p, "main\n");
        }
        f.git(&["commit", "-qam", "main"]);
        let ok =
            Command::new("git").args(["init", "-q", "--bare"]).arg(f.dir.join("other.git")).output().unwrap();
        assert!(ok.status.success());
        f.git(&["remote", "add", "l", f.dir.join("other.git").to_str().unwrap()]);
        f
    }

    /// Plain git in the repository (setting it up).
    fn git(&self, args: &[&str]) {
        self.git_at(&self.repo, args);
    }

    /// Plain git in `dir`, with the fixture's user config.
    fn git_at(&self, dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", self.dir.join("user.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A marker script `name`: touches its mark, then `how` — pass (stdin to stdout), file (cats its first
    /// argument), ok (exit 0), fail (exit 1).
    fn marker(&self, name: &str, how: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let body = match how {
            "pass" => "cat",
            "file" => "if [ -f \"$1\" ]; then cat \"$1\"; fi",
            "ok" => "exit 0",
            _ => "exit 1",
        };
        let path = self.dir.join(format!("bin-{name}"));
        std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n{body}\n", self.marks.join(name).display()))
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    /// The repository's own config: `key` = `value`.
    fn set(&self, key: &str, value: &str) {
        self.git(&["config", key, value]);
    }

    /// The user's own (the fixture's user config): `key` = `value`.
    fn set_user(&self, key: &str, value: &str) {
        let f = self.dir.join("user.gitconfig");
        let ok = Command::new("git").args(["config", "--file"]).arg(&f).args([key, value]).output().unwrap();
        assert!(ok.status.success());
    }

    /// A repository `lib` beside it (`x.txt`), added at `lib` as the submodule `name` and committed.
    fn submodule(&self, name: &str) {
        let lib = self.dir.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("x.txt"), "lib's\n").unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-qm", "lib"]] {
            self.git_at(&lib, &[&["-c", "user.name=t", "-c", "user.email=t@t"][..], args].concat());
        }
        let url = lib.to_str().unwrap();
        self.git(&["-c", "protocol.file.allow=always", "submodule", "add", "-q", "--name", name, url, "lib"]);
        self.git(&["commit", "-qm", "lib"]);
    }

    /// The user's own `key` gone again.
    fn unset_user(&self, key: &str) {
        let f = self.dir.join("user.gitconfig");
        let ok =
            Command::new("git").args(["config", "--file"]).arg(&f).args(["--unset", key]).output().unwrap();
        assert!(ok.status.success());
    }

    fn ran(&self, name: &str) -> bool {
        self.marks.join(name).exists()
    }

    fn clear(&self) {
        for e in std::fs::read_dir(&self.marks).unwrap().flatten() {
            std::fs::remove_file(e.path()).ok();
        }
    }

    fn touch(&self, file: &str) {
        let p = self.repo.join(file);
        let mut text = std::fs::read_to_string(&p).unwrap_or_default();
        text.push_str("more\n");
        std::fs::write(p, text).unwrap();
    }

    /// The rebase's own line (`Repo::shell_line`) and then `rest`, run as its task runs it: by `sh`, with the
    /// line's environment (and the fixture's user config).
    fn run_line(&self, rest: &str) {
        let line = self.resolved().shell_line();
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("{} {rest}", line.text))
            .envs(line.env)
            .env("GIT_CONFIG_GLOBAL", self.dir.join("user.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.repo)
            .output();
    }

    /// The repository as tarae resolves it now.
    fn resolved(&self) -> Arc<Repo> {
        repo(&self.repo).unwrap()
    }

    /// The card answered `y` for what it finds now.
    fn trust(&self) {
        let r = self.resolved();
        answered(&r.config_file(), r.local.digest(), true);
    }

    /// `call` with the repository's commands held — `mark` doesn't appear — then trusted: it does (so this
    /// test sees it run when it isn't held).
    fn held_then_trusted(&self, mark: &str, call: impl Fn(&Fixture)) {
        let r = self.resolved();
        assert!(r.held.is_some() && r.asks, "{mark}: found and asking — {:?}", r.local.found);
        self.clear();
        call(self);
        assert!(!self.ran(mark), "{mark} ran while held");
        self.trust();
        assert!(self.resolved().held.is_none(), "{mark}: trusted, nothing held");
        self.clear();
        call(self);
        assert!(self.ran(mark), "{mark} trusted: it runs (or the test can't see it)");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// Past `RACY` since the test's last change: a resolve now is kept.
fn settled() {
    std::thread::sleep(crate::gitrepo::RACY + std::time::Duration::from_millis(100));
}

/// What `h` sets, as `key=value` (each given to git whole — `Hold::settings`).
fn said(h: &Hold) -> Vec<String> {
    h.config.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

// ── The table ─────────────────────────────────────────────────────────────────

#[test]
fn keys_match_as_git_lists_them() {
    assert!(guard("core.fsmonitor").is_some());
    assert!(guard("core.hookspath").is_some(), "git lists names lowercase");
    assert!(guard("filter.lfs.clean").is_some() && guard("filter.My.Driver.clean").is_some());
    assert!(guard("credential.https://example.com.helper").is_some(), "a subsection with dots");
    assert!(guard("credential.helper").is_some());
    assert!(guard("filter..clean").is_none() && guard("filter.clean").is_none());
    assert!(guard("core.editor").is_none() && guard("core.pager").is_none(), "never reached: not held");
    assert!(guard("commit.gpgsign").is_none() && guard("log.showsignature").is_none(), "switches: not held");
    assert_eq!(protocol_default("protocol.ext.allow"), "never");
    assert_eq!(protocol_default("protocol.https.allow"), "always");
    assert_eq!(protocol_default("protocol.file.allow"), "user");
}

/// What git allows for `protocol` (`transport.c`) from config scopes in order, a later value winning: the
/// protocol's own key, else `protocol.allow`, else git's table.
fn allowed(scopes: &[&[(String, String)]], protocol: &str) -> String {
    let key = format!("protocol.{protocol}.allow");
    let last =
        |key: &str| scopes.iter().flat_map(|s| s.iter()).rfind(|(k, _)| k == key).map(|(_, v)| v.clone());
    last(&key).or_else(|| last("protocol.allow")).unwrap_or_else(|| protocol_default(&key).to_string())
}

/// While the repository's protocol keys are held, every protocol is allowed exactly as with none of them
/// set — whatever the user's own config says: `protocol.allow` held as `user` alone would open `ext::`, and
/// `file` and git's table as well.
#[test]
fn a_held_protocol_is_no_looser_than_none_set() {
    let none = std::env::temp_dir().join("tarae-no-hooks-here");
    std::fs::create_dir_all(&none).ok();
    let users: [&[(&str, &str)]; 4] = [
        &[],
        &[("protocol.allow", "never")],
        &[("protocol.ext.allow", "always")],
        &[("protocol.file.allow", "never"), ("protocol.allow", "always")],
    ];
    let repos: [&[(&str, &str)]; 4] = [
        &[("protocol.allow", "always")],
        &[("protocol.ext.allow", "always"), ("protocol.file.allow", "always")],
        &[("protocol.allow", "always"), ("protocol.ext.allow", "user")],
        &[("protocol.ext.allow", "always"), ("protocol.allow", "always"), ("protocol.https.allow", "never")],
    ];
    let pairs = |s: &[(&str, &str)]| -> Vec<(String, String)> {
        s.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    };
    for user in users {
        for repo in repos {
            let entry = |scope: &str, (k, v): &(&str, &str)| Entry {
                scope: scope.into(),
                origin: None,
                key: k.to_string(),
                value: Some(v.to_string()),
            };
            let entries: Vec<Entry> = user
                .iter()
                .map(|e| entry("global", e))
                .chain(repo.iter().map(|e| entry("local", e)))
                .collect();
            let held: Vec<(String, String)> = find(&entries, &none, &none.join("HEAD")).hold.config;
            for p in ["ext", "file", "git", "http", "https", "ssh", "other"] {
                assert_eq!(
                    allowed(&[&pairs(user), &pairs(repo), &held], p),
                    allowed(&[&pairs(user)], p),
                    "{p} — user {user:?}, repository {repo:?}: held {held:?}"
                );
            }
            let mut keys: Vec<&String> = held.iter().map(|(k, _)| k).collect();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), held.len(), "each key once: {held:?}");
        }
    }
}

#[test]
fn flags_go_right_after_the_subcommand() {
    let h = Hold {
        flags: vec![("--no-textconv", &["blame", "show"]), ("--upload-pack=git-upload-pack", &["fetch"])],
        ..Hold::default()
    };
    assert_eq!(h.args(&["blame", "--porcelain", "a"]), ["blame", "--no-textconv", "--porcelain", "a"]);
    assert_eq!(
        h.args(&["-c", "core.hooksPath=/dev/null", "fetch", "l"]),
        ["-c", "core.hooksPath=/dev/null", "fetch", "--upload-pack=git-upload-pack", "l"]
    );
    assert_eq!(h.args(&["status"]), ["status"]);
    assert!(h.args(&[]).is_empty());
}

/// The listing's format as measured (`target/sb/gittrust-zformat.sh`, stand-in configs, fake values): scope,
/// origin, key LF value, each NUL-ended; a key with no `=` has no LF; a value may hold a LF; the repository's
/// own config named relative to where git ran; Apple's git lists Xcode's built-in config as `unknown`.
const MEASURED: &str = "unknown\0file:/Applications/Xcode.app/Contents/Developer/usr/share/git-core/gitconfig\0\
credential.helper\nosxkeychain\0system\0file:/t/system\0core.sshcommand\nfake-ssh\0global\0file:/t/global\0\
credential.helper\nfake-helper\0local\0file:.git/config\0core.fsmonitor\nfake-monitor\0local\0file:/t/inc.cfg\0\
filter.fi.clean\nfake-included\0local\0file:.git/config\0alias.bare\0local\0file:.git/config\0alias.multi\na\nb\0";

#[test]
fn the_measured_listing_parses() {
    let e = parse(MEASURED);
    assert_eq!(e.len(), 7);
    assert_eq!(
        e[0],
        Entry {
            scope: "unknown".into(),
            origin: Some("/Applications/Xcode.app/Contents/Developer/usr/share/git-core/gitconfig".into()),
            key: "credential.helper".into(),
            value: Some("osxkeychain".into()),
        }
    );
    assert_eq!(
        (e[3].origin.as_deref(), e[3].key.as_str()),
        (Some(Path::new(".git/config")), "core.fsmonitor")
    );
    assert_eq!((e[5].key.as_str(), e[5].value.as_deref()), ("alias.bare", None), "no `=`: no value");
    assert_eq!(e[6].value.as_deref(), Some("a\nb"), "a value's own LF");
    // What's found and held: the repository's own (an include's too); git's built-in config is the user's —
    // its credential helper comes back after the list is emptied, with the global one
    let none = std::env::temp_dir().join("tarae-no-hooks-here");
    std::fs::create_dir_all(&none).ok();
    let l = find(&e, &none, &none.join("HEAD"));
    let keys: Vec<&str> = l.found.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["core.fsmonitor", "filter.fi.clean"]);
    assert_eq!(l.files, [PathBuf::from(".git/config"), PathBuf::from("/t/inc.cfg")], "as git named them");
    let mut e2 = e.clone();
    e2.push(Entry {
        scope: "local".into(),
        origin: Some(".git/config".into()),
        key: "credential.helper".into(),
        value: Some("fake-own".into()),
    });
    let held: Vec<String> = said(&find(&e2, &none, &none.join("HEAD")).hold)
        .into_iter()
        .filter(|c| c.starts_with("credential.helper"))
        .collect();
    assert_eq!(
        held,
        ["credential.helper=", "credential.helper=osxkeychain", "credential.helper=fake-helper"]
    );
}

/// The listing as git writes it: an include in the repository's config counts as its own (scope `local`,
/// the included file its origin); the user's own is `global`.
#[test]
fn the_listing_is_read_with_its_includes_and_scopes() {
    let f = Fixture::new("listing");
    let inc = f.dir.join("inc.cfg");
    std::fs::write(&inc, format!("[filter \"fi\"]\n\tclean = {}\n", f.marker("included", "pass"))).unwrap();
    f.set("include.path", inc.to_str().unwrap());
    f.set_user("core.sshCommand", "ssh -v");
    let r = f.resolved();
    assert!(r.local.found.iter().any(|(k, _)| k == "filter.fi.clean"), "{:?}", r.local.found);
    assert!(r.local.files.contains(&inc), "the include's file is watched: {:?}", r.local.files);
    assert!(!r.local.found.iter().any(|(k, _)| k == "core.sshcommand"), "the user's own isn't found");
    assert!(said(r.held.as_ref().unwrap()).contains(&"filter.fi.clean=".to_string()));
    assert!(!f.ran("included"), "listing the config ran nothing");
}

/// What's held gets the user's own value back where theirs has the key; lists are emptied, then theirs again.
#[test]
fn the_users_own_values_come_back() {
    let f = Fixture::new("users");
    f.set("core.sshCommand", &f.marker("ssh", "fail"));
    f.set_user("core.sshCommand", "ssh -4");
    f.set("credential.helper", &f.marker("helper", "ok"));
    f.set_user("credential.helper", "store");
    f.set("filter.lfs.clean", &f.marker("lfs", "pass"));
    f.set_user("filter.lfs.clean", "git-lfs clean -- %f");
    let h = said(&f.resolved().held.clone().unwrap());
    assert!(h.contains(&"core.sshcommand=ssh -4".to_string()), "{h:?}");
    let helpers: Vec<&String> = h.iter().filter(|c| c.starts_with("credential.helper=")).collect();
    assert_eq!(helpers, ["credential.helper=", "credential.helper=store"]);
    assert!(h.contains(&"filter.lfs.clean=git-lfs clean -- %f".to_string()));
    assert!(!h.iter().any(|c| c.starts_with("filter.lfs.required")), "the user's own filter stays required");
    // (the files they're from, as the resolve keeps them: absolute — git names them so with `--git-dir` given
    // absolute; found by discovery it says `.git/config`, which a stamp couldn't use)
    let files = f.resolved().local.files.clone();
    assert!(
        files.contains(&f.repo.join(".git/config")) && files.iter().all(|p| p.is_absolute()),
        "{files:?}"
    );
}

/// Answers: `n` keeps them held and doesn't ask again; a value changed since asks again (the config file's
/// time resolves the repository again).
#[test]
fn an_answer_is_for_what_was_found() {
    let f = Fixture::new("answers");
    f.set("core.fsmonitor", &f.marker("fsmonitor", "fail"));
    let r = f.resolved();
    assert!(r.asks && r.held.is_some());
    answered(&r.config_file(), r.local.digest(), false);
    let r = f.resolved();
    assert!(!r.asks && r.held.is_some(), "n: held, not asked again");
    std::thread::sleep(std::time::Duration::from_millis(1100)); // (the config file's time moves by the second)
    f.set("core.fsmonitor", &f.marker("fsmonitor2", "fail"));
    let r = f.resolved();
    assert!(r.asks && r.held.is_some(), "changed since: asks again");
}

/// git in the fixture's repository, its git folder named as the resolve names it: what it lists.
fn listed(f: &Fixture, args: &[&str]) -> Vec<Entry> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(f.repo.join(".git"))
        .args(args)
        .current_dir(&f.repo)
        .env("GIT_CONFIG_GLOBAL", f.dir.join("user.gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// The resolve's narrow listing (`listing_args`) is the whole one cut to the guarded keys and the includes —
/// every row of `GUARDED`, subsections by case, both scopes, an include's — and nothing else of the user's.
#[test]
fn the_narrow_listing_is_the_full_one_cut_to_the_guarded_keys() {
    let f = Fixture::new("narrow");
    let inc = f.dir.join("inc.cfg");
    std::fs::write(&inc, "[filter \"Inc\"]\n\tclean = x\n[alias]\n\tinc = y\n").unwrap();
    for g in GUARDED {
        for sub in ["Sub.Sec", "sub.sec"] {
            f.set(&g.key.replace('*', sub), "x");
        }
        f.set_user(&g.key.replace('*', "User"), "u");
    }
    f.set("include.path", inc.to_str().unwrap());
    f.set("includeIf.gitdir:/nowhere/.path", "../never.cfg");
    f.set("alias.x", "y");
    f.set("core.pager", "less");
    f.set_user("url.https://fake-token@example.invalid/.insteadOf", "https://example.invalid/");
    f.set_user("http.extraHeader", "Authorization: fake-token");
    let full = listed(&f, &["config", "--list", "--show-scope", "--show-origin", "--includes", "-z"]);
    let args = listing_args();
    let narrow = listed(&f, &args.iter().map(String::as_str).collect::<Vec<_>>());
    let cut: Vec<Entry> = full.into_iter().filter(|e| guard(&e.key).is_some() || include(&e.key)).collect();
    assert_eq!(narrow, cut);
    let keys: Vec<&str> = narrow.iter().map(|e| e.key.as_str()).collect();
    for g in GUARDED {
        let of = |scope: &str| {
            let row = |e: &&Entry| guard(&e.key).is_some_and(|h| h.key == g.key);
            narrow.iter().filter(row).filter(|e| e.scope == scope && e.origin.as_ref() != Some(&inc)).count()
        };
        let subs = if g.key.contains('*') { 2 } else { 1 };
        assert_eq!((of("local"), of("global")), (subs, 1), "{}: {keys:?}", g.key);
    }
    for k in ["filter.Inc.clean", "include.path", "includeif.gitdir:/nowhere/.path"] {
        assert!(keys.contains(&k), "{k}: {keys:?}");
    }
    assert!(!narrow.iter().any(|e| e.value.as_deref().is_some_and(|v| v.contains("fake-token"))));
}

/// An include's file decides what's found though it holds nothing yet: a tracked file in the work tree
/// (`include.path = ../.gitconfig`, which a run can write) that gets a command later is read again.
#[test]
fn an_included_file_is_watched_before_it_holds_a_command() {
    let f = Fixture::new("incwatch");
    std::fs::write(f.repo.join(".gitconfig"), "[alias]\n\tx = y\n").unwrap();
    f.set("include.path", "../.gitconfig");
    let r = f.resolved();
    assert!(r.held.is_none() && r.stamps.iter().any(|(p, _)| *p == f.repo.join(".git/../.gitconfig")));
    std::thread::sleep(std::time::Duration::from_millis(1100)); // (a file's time moves by the second)
    let monitor = f.marker("fsmonitor", "fail");
    std::fs::write(f.repo.join(".gitconfig"), format!("[core]\n\tfsmonitor = {monitor}\n")).unwrap();
    f.held_then_trusted("fsmonitor", |f| {
        f.touch("e.n");
        let _ = crate::gitpanel::status(&f.repo, None);
    });
}

/// An include of a file that isn't there yet: its coming is seen.
#[test]
fn an_include_not_there_yet_is_watched() {
    let f = Fixture::new("incmissing");
    f.set("include.path", "../later.gitconfig");
    assert!(f.resolved().held.is_none());
    let monitor = f.marker("fsmonitor", "fail");
    std::fs::write(f.repo.join("later.gitconfig"), format!("[core]\n\tfsmonitor = {monitor}\n")).unwrap();
    f.held_then_trusted("fsmonitor", |f| {
        f.touch("e.n");
        let _ = crate::gitpanel::status(&f.repo, None);
    });
}

/// An `onbranch:` include that comes into force after a switch: `HEAD` is watched.
#[test]
fn an_onbranch_include_is_read_again_after_a_switch() {
    let f = Fixture::new("onbranch");
    let monitor = f.marker("fsmonitor", "fail");
    std::fs::write(f.dir.join("side.gitconfig"), format!("[core]\n\tfsmonitor = {monitor}\n")).unwrap();
    f.set("includeIf.onbranch:side.path", f.dir.join("side.gitconfig").to_str().unwrap());
    assert!(f.resolved().held.is_none(), "on main: not in force");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    f.git(&["checkout", "-q", "side"]);
    f.held_then_trusted("fsmonitor", |f| {
        f.touch("e.n");
        let _ = crate::gitpanel::status(&f.repo, None);
    });
}

/// Subsections differ by case: `filter.FX` and `filter.fx` are two drivers, each held — and the user's own
/// `filter.FX` comes back for that one only.
#[test]
fn keys_that_differ_by_case_are_each_held() {
    let f = Fixture::new("case");
    f.set("filter.FX.clean", &f.marker("upper", "pass"));
    f.set("filter.fx.clean", &f.marker("clean", "pass"));
    f.set_user("filter.FX.clean", "cat");
    let h = said(&f.resolved().held.clone().unwrap());
    for c in ["filter.FX.clean=cat", "filter.fx.clean=", "filter.fx.required=false"] {
        assert!(h.contains(&c.to_string()), "{c}: {h:?}");
    }
    assert!(!h.contains(&"filter.FX.required=false".to_string()), "the user's own stays required");
    f.held_then_trusted("clean", |f| {
        f.touch("a.x");
        let _ = crate::snapshot::tree(&f.repo);
    });
}

// Keys whose subsection holds a `=` stay held: `-c filter.x=y.clean=` would set `filter.x` and leave the
// repository's own `x=y` filter running (review, 2026-10-06) — each key here split at its first `=` is a key
// git takes, so the old `-c` ran them (checked). The drivers named through `.git/info/attributes`, which wins
// over the fixture's own and no reset takes back.

fn named_with_an_equals_sign(f: &Fixture) {
    std::fs::create_dir_all(f.repo.join(".git/info")).unwrap();
    std::fs::write(f.repo.join(".git/info/attributes"), "*.x filter=x=y merge=m=x\n").unwrap();
}

#[test]
fn a_filter_named_with_an_equals_sign_stays_held() {
    let f = Fixture::new("eqfilter");
    named_with_an_equals_sign(&f);
    f.set("filter.x=y.clean", &f.marker("clean", "pass"));
    f.set("filter.x=y.required", "true");
    let h = said(&f.resolved().held.clone().unwrap());
    for c in ["filter.x=y.clean=", "filter.x=y.required=false"] {
        assert!(h.contains(&c.to_string()), "{c}: {h:?}");
    }
    f.held_then_trusted("clean", |f| {
        f.touch("a.x");
        assert!(crate::snapshot::tree(&f.repo).is_ok());
    });
}

/// (the rebase's line)
#[test]
fn a_merge_driver_named_with_an_equals_sign_stays_held() {
    let f = Fixture::new("eqmerge");
    named_with_an_equals_sign(&f);
    let out = Command::new("git").args(["rev-parse", "main"]).current_dir(&f.repo).output().unwrap();
    let tip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    f.set("merge.m=x.driver", &f.marker("merge", "ok"));
    f.held_then_trusted("merge", |f| {
        f.run_line("merge -q side");
        f.git(&["reset", "-q", "--hard", &tip]);
    });
}

/// (a host with a `=`: git matches it — `credential.https://cred.a` is what `-c` set)
#[test]
fn a_credential_helper_for_a_url_with_an_equals_sign_stays_held() {
    let f = Fixture::new("eqcred");
    f.set("credential.https://cred.a=b.helper", &f.marker("helper", "ok"));
    f.held_then_trusted("helper", |f| {
        let cmd = f.resolved().hooked_command(&["credential", "fill"]).unwrap();
        let _ = crate::gitrepo::ran(&cmd, Some(b"protocol=https\nhost=cred.a=b\n\n"));
    });
}

/// The user's own `GIT_CONFIG_*` (the environment tarae runs in) stay as they are, and count as theirs: git
/// reads them first, the held settings after (measured — `--get-all` lists in the order read); a list a
/// hold empties gets their value back; another key of theirs is still read.
#[test]
fn the_users_own_git_config_environment_stays() {
    let f = Fixture::new("userenv");
    let (own, theirs) = (f.marker("helper", "ok"), f.marker("helper-env", "ok"));
    f.set("credential.helper", &own);
    let env = [
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "credential.helper"),
        ("GIT_CONFIG_VALUE_0", theirs.as_str()),
        ("GIT_CONFIG_KEY_1", "tarae.mark"),
        ("GIT_CONFIG_VALUE_1", "theirs"),
    ];
    crate::gitrepo::env_for(&f.dir, &env);
    let r = f.resolved();
    assert!(r.held.is_some() && r.asks);
    let helpers = r.run(&["config", "--get-all", "credential.helper"], None).unwrap();
    assert_eq!(helpers.lines().collect::<Vec<_>>(), [own.as_str(), &theirs, "", &theirs]);
    assert_eq!(r.run(&["config", "--get", "tarae.mark"], None).unwrap().trim(), "theirs");
    let cmd = r.hooked_command(&["credential", "fill"]).unwrap();
    let _ = crate::gitrepo::ran(&cmd, Some(b"protocol=https\nhost=example.com\n\n"));
    assert!(!f.ran("helper") && f.ran("helper-env"), "held: theirs ran, the repository's didn't");
}

/// A change that lands while the repository is resolved — after its config was listed — is seen by the
/// next run: what decides it is stamped before it's read (review, 2026-10-06: stamped after, a run's
/// `core.fsmonitor` written then would never be found).
#[test]
fn a_change_while_its_resolved_is_seen_next_time() {
    let f = Fixture::new("whileread");
    let (repo, user, monitor) = (f.repo.clone(), f.dir.join("user.gitconfig"), f.marker("fsmonitor", "fail"));
    crate::gitrepo::after_listing(&f.repo.join(".git"), move || {
        std::thread::sleep(std::time::Duration::from_millis(1100)); // (a file's time moves by the second)
        let ok = Command::new("git")
            .args(["config", "core.fsmonitor", &monitor])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", user)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(ok.status.success());
    });
    let first = f.resolved();
    assert!(first.local.found.is_empty(), "listed before the change");
    let r = f.resolved();
    assert!(!Arc::ptr_eq(&first, &r), "the change is seen: resolved again");
    assert!(r.held.is_some() && r.asks, "{:?}", r.local.found);
}

// ── The files a value runs ──────────────────────────────────────────────────

/// `path` made a marker script of the fixture's (`how` as `Fixture::marker`), executable.
fn script_at(f: &Fixture, path: &Path, mark: &str, how: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(f.marker(mark, how), path).unwrap();
}

/// `path` with a line more (its text changed — after a pause: a file's time moves by the second).
fn edit(path: &Path) {
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut text = std::fs::read_to_string(path).unwrap();
    text.push_str("# changed\n");
    std::fs::write(path, text).unwrap();
}

/// A status as the git tab reads it, a file touched first (what an fsmonitor runs on).
fn status(f: &Fixture) {
    f.touch("e.n");
    let _ = crate::gitpanel::status(&f.repo, None);
}

/// A script in the work tree a value names (`core.fsmonitor = tools/mon.sh`, a filter's `./scripts/x %f`):
/// its text is part of what's trusted — edited after a `y` (a run of Claude's can write the work tree),
/// held and asked again; an absolute path too, wherever it is (a temp folder, here); a bare word is `PATH`'s.
#[test]
fn a_script_a_value_names_is_trusted_as_it_was() {
    let f = Fixture::new("script");
    let (monitor, clean) = (f.repo.join("tools/mon.sh"), f.repo.join("scripts/clean.sh"));
    script_at(&f, &monitor, "fsmonitor", "fail");
    script_at(&f, &clean, "clean", "pass");
    f.set("core.fsmonitor", "tools/mon.sh");
    f.set("filter.fx.clean", "./scripts/clean.sh %f");
    let textconv = f.marker("textconv", "file");
    f.set("diff.dx.textconv", &textconv);
    f.set("merge.mx.driver", "cat %A");
    let r = f.resolved();
    let covered: Vec<&PathBuf> = r.local.covers.iter().filter(|(_, d)| d.is_some()).map(|(p, _)| p).collect();
    let expected = [&monitor, &f.repo.join("./scripts/clean.sh"), &PathBuf::from(&textconv)];
    assert_eq!(covered, expected, "{:?}", r.local.covers);
    assert_eq!(r.local.covers.len(), 3, "a bare word isn't a file here: {:?}", r.local.covers);
    f.held_then_trusted("fsmonitor", status);
    edit(&monitor);
    let r = f.resolved();
    assert!(r.held.is_some() && r.asks, "edited since the answer: held, asked again");
    f.clear();
    status(&f);
    assert!(!f.ran("fsmonitor"), "held");
    // (the filter's script the same way)
    f.trust();
    edit(&clean);
    assert!(f.resolved().asks, "the filter's script edited: asked again");
}

/// A script a value names that isn't there yet: covered as not there — made after a `y`, asked again.
#[test]
fn a_script_made_after_the_answer_asks_again() {
    let f = Fixture::new("scriptlater");
    f.set("core.fsmonitor", "tools/later.sh");
    let r = f.resolved();
    assert_eq!(r.local.covers, [(f.repo.join("tools/later.sh"), None)]);
    f.trust();
    assert!(f.resolved().held.is_none());
    script_at(&f, &f.repo.join("tools/later.sh"), "fsmonitor", "fail");
    let r = f.resolved();
    assert!(r.held.is_some() && r.asks, "made since: asked again");
    f.clear();
    status(&f);
    assert!(!f.ran("fsmonitor"));
}

/// `core.hooksPath` naming a folder in the work tree: the setting is what's trusted, its hooks are the hooks
/// card's (`hooks::unseen`) — one edited, one more, one made executable after a `y` doesn't bring this card
/// again (one card for one edit).
#[test]
fn a_hooks_folder_in_the_work_tree_is_the_hooks_cards() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new("hooksin");
    let hooks = f.repo.join(".githooks");
    script_at(&f, &hooks.join("pre-commit"), "hook", "ok");
    f.set("core.hooksPath", ".githooks");
    let commit = |f: &Fixture| drop(hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]));
    f.held_then_trusted("hook", commit);
    assert!(f.resolved().local.covers.is_empty(), "{:?}", f.resolved().local.covers);
    edit(&hooks.join("pre-commit"));
    std::fs::write(hooks.join("pre-push"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(hooks.join("pre-push"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let r = f.resolved();
    assert!(!r.asks && r.held.is_none(), "trusted still: the hooks card asks about them");
    // (the setting itself changed: asked again)
    std::thread::sleep(std::time::Duration::from_millis(1100));
    f.set("core.hooksPath", "./.githooks");
    assert!(f.resolved().asks);
}

/// `core.hooksPath` naming a folder outside the work tree (no hooks card looks there): each hook in it is
/// part of what's trusted — one edited, or one more put there, after a `y` asks again; one made executable
/// later too.
#[test]
fn a_hooks_folder_outside_the_work_tree_is_trusted_as_it_was() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new("hooksout");
    let hooks = f.dir.join("hooks");
    script_at(&f, &hooks.join("pre-commit"), "hook", "ok");
    f.set("core.hooksPath", hooks.to_str().unwrap());
    let commit = |f: &Fixture| drop(hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]));
    f.held_then_trusted("hook", commit);
    edit(&hooks.join("pre-commit"));
    assert!(f.resolved().asks, "a hook edited: asked again");
    f.clear();
    commit(&f);
    assert!(!f.ran("hook"), "held");
    f.trust();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    script_at(&f, &hooks.join("post-commit"), "post", "ok");
    assert!(f.resolved().asks, "a hook more: asked again");
    f.trust();
    let later = hooks.join("pre-push");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&later, "#!/bin/sh\nexit 0\n").unwrap();
    let r = f.resolved();
    assert!(!r.asks && r.local.files.contains(&later), "not executable: nothing more runs, but it's watched");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::set_permissions(&later, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(f.resolved().asks, "made executable: asked again");
}

/// Where a file's times move coarsely (by the second here, standing in for Linux's tick), a change in the
/// tick the resolve's stamp was taken in leaves the stamp as it was: that resolve isn't kept — the next
/// sees the change — and once the files have settled, one is kept again.
#[test]
fn a_change_in_the_same_tick_as_the_resolve_is_seen() {
    let f = Fixture::new("racy");
    crate::gitrepo::coarse_times_under(&f.dir);
    let script = f.repo.join("tools/mon.sh");
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    // (the change and both resolves within one second of the clock, from the start of one — tried again
    // when a busy machine takes longer: else the test shows nothing; 20 ms past the second's start, as a file's
    // time is the kernel's coarse clock, a tick behind the one `now` reads — at the start it's the second before)
    for tries in 1.. {
        let into = std::time::Duration::from_nanos(u64::from(now().subsec_nanos()));
        std::thread::sleep(std::time::Duration::from_millis(1020) - into);
        script_at(&f, &script, "fsmonitor", "fail");
        f.set("core.fsmonitor", &format!("tools/mon.sh {tries}"));
        let first = f.resolved();
        std::fs::write(&script, format!("#!/bin/sh\nexit 1\n# changed {tries}\n")).unwrap();
        let r = f.resolved();
        if now().as_secs() != first_second(&first) {
            assert!(tries < 5, "never within one second");
            continue;
        }
        assert_ne!(r.local.covers, first.local.covers, "the change in the same tick is seen");
        break;
    }
    settled();
    let kept = f.resolved();
    assert!(Arc::ptr_eq(&kept, &f.resolved()), "settled: kept");
}

/// The second `r`'s config file was stamped in (whole: coarse).
fn first_second(r: &Repo) -> u64 {
    let (_, s) = r.stamps.iter().find(|(p, _)| *p == r.config_file()).unwrap();
    s.unwrap()[0].0 as u64
}

// ── Each key, through what tarae runs ───────────────────────────────────────

/// `git blame` as the gutter's line end asks it (`gitmenu::blame_of`).
fn blame(f: &Fixture) {
    let (r, rel) = crate::gitrepo::for_file(&f.repo, &f.repo.join("a.x")).unwrap();
    let _ = r.run(&["blame", "--porcelain", "--contents", "-", "--", &rel], Some("main\nmore\n"));
}

#[test]
fn fsmonitor_is_held_on_status_and_snapshots() {
    let f = Fixture::new("fsmonitor");
    f.set("core.fsmonitor", &f.marker("fsmonitor", "fail"));
    f.held_then_trusted("fsmonitor", |f| {
        f.touch("e.n");
        let _ = crate::gitpanel::status(&f.repo, None);
        let _ = crate::snapshot::tree(&f.repo);
    });
}

#[test]
fn a_clean_filter_is_held_when_a_snapshot_stores_files() {
    let f = Fixture::new("clean");
    f.set("filter.fx.clean", &f.marker("clean", "pass"));
    f.set("filter.fx.required", "true");
    f.held_then_trusted("clean", |f| {
        f.touch("a.x");
        assert!(crate::snapshot::tree(&f.repo).is_ok(), "held, a required filter doesn't fail the snapshot");
    });
}

#[test]
fn a_smudge_filter_is_held_when_a_file_is_written_out() {
    let f = Fixture::new("smudge");
    f.set("filter.fx.smudge", &f.marker("smudge", "pass"));
    f.held_then_trusted("smudge", |f| {
        std::fs::remove_file(f.repo.join("a.x")).ok();
        // (the git tab's discard)
        let _ = crate::gitrepo::git(&f.repo, &["restore", "--", "a.x"], None);
    });
}

#[test]
fn a_process_filter_is_held() {
    let f = Fixture::new("process");
    f.set("filter.fp.process", &f.marker("process", "pass"));
    f.held_then_trusted("process", |f| {
        f.touch("b.p");
        let _ = crate::snapshot::tree(&f.repo);
    });
}

#[test]
fn a_textconv_is_held_on_blame_and_a_commits_preview() {
    let f = Fixture::new("textconv");
    f.set("diff.dx.textconv", &f.marker("textconv", "file"));
    f.held_then_trusted("textconv", |f| {
        blame(f);
        let _ = crate::gitpanel::show(&f.repo, "HEAD");
    });
    // (held, blame and the preview still work)
    f.set("diff.dx.textconv", &f.marker("textconv2", "file"));
    let r = f.resolved();
    let (r2, rel) = crate::gitrepo::for_file(&f.repo, &f.repo.join("a.x")).unwrap();
    assert!(r.held.is_some());
    assert!(r2.run(&["blame", "--porcelain", "--", &rel], None).is_ok());
    assert!(crate::gitpanel::show(&f.repo, "HEAD").is_ok());
}

/// `git diff --cached` as the commit draft's fallback asks it (`gitpanel::commit_prompt` — amending a first
/// commit); `show`/`log` run an external diff only with `--ext-diff`.
fn staged_diff(f: &Fixture) {
    f.touch("c.d");
    f.touch("e.n");
    f.git(&["add", "c.d", "e.n"]);
    let _ = crate::gitrepo::git(&f.repo, &["diff", "--no-color", "--cached"], None);
}

#[test]
fn a_diff_command_is_held_on_the_staged_diff() {
    let f = Fixture::new("diffcmd");
    f.set("diff.dc.command", &f.marker("diffcmd", "ok"));
    f.held_then_trusted("diffcmd", staged_diff);
}

#[test]
fn an_external_diff_is_held_on_the_staged_diff() {
    let f = Fixture::new("external");
    f.set("diff.external", &f.marker("external", "ok"));
    f.held_then_trusted("external", staged_diff);
}

#[test]
fn a_merge_driver_is_held_in_the_rebases_line() {
    let f = Fixture::new("merge");
    let out = Command::new("git").args(["rev-parse", "main"]).current_dir(&f.repo).output().unwrap();
    let tip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    f.set("merge.mx.driver", &f.marker("merge", "ok"));
    f.held_then_trusted("merge", |f| {
        f.run_line("merge -q side");
        f.git(&["reset", "-q", "--hard", &tip]);
    });
    // Held: git's own conflict, its markers in the file
    f.set("merge.mx.driver", &f.marker("merge2", "ok"));
    f.run_line("merge -q side");
    let a = std::fs::read_to_string(f.repo.join("a.x")).unwrap();
    assert!(!f.ran("merge2") && a.contains("<<<<<<<") && a.contains(">>>>>>> side"), "{a}");
}

#[test]
fn a_hooks_path_is_held_on_the_git_tabs_commit() {
    let f = Fixture::new("hookspath");
    let hooks = f.dir.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::copy(f.marker("hook", "ok"), hooks.join("pre-commit")).unwrap();
    f.set("core.hooksPath", hooks.to_str().unwrap());
    f.held_then_trusted("hook", |f| {
        let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    });
}

#[test]
fn hooks_in_the_git_folder_are_held_on_the_git_tabs_commit() {
    let f = Fixture::new("githooks");
    std::fs::copy(f.marker("hook", "ok"), f.repo.join(".git/hooks/pre-commit")).unwrap();
    let r = f.resolved();
    assert!(r.local.found.iter().any(|(k, _)| k == "hook pre-commit"), "{:?}", r.local.found);
    assert!(!r.local.found.iter().any(|(k, _)| k.ends_with(".sample")), "samples aren't hooks");
    f.held_then_trusted("hook", |f| {
        let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    });
}

/// A hook that's a link to a script elsewhere: git follows it, so it's found (where it leads said) and held.
#[test]
fn a_linked_hook_in_the_git_folder_is_held() {
    let f = Fixture::new("linkhook");
    std::os::unix::fs::symlink(f.marker("hook", "ok"), f.repo.join(".git/hooks/pre-commit")).unwrap();
    let r = f.resolved();
    let found = r.local.found.iter().find(|(k, _)| k == "hook pre-commit");
    assert!(found.is_some_and(|(_, v)| v.contains("→") && v.contains("bin-hook")), "{:?}", r.local.found);
    f.held_then_trusted("hook", |f| {
        let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    });
}

/// A hook that isn't executable doesn't run and isn't asked about, but it's watched: made executable after
/// the resolve — its change time moves, its modification time doesn't — it's held on the next run.
#[test]
fn a_hook_made_executable_later_is_held() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new("chmodhook");
    let hook = f.repo.join(".git/hooks/pre-commit");
    std::fs::copy(f.marker("hook", "ok"), &hook).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = f.resolved();
    assert!(r.held.is_none() && r.local.found.is_empty(), "not executable: nothing runs");
    assert!(r.local.files.contains(&hook), "but watched: {:?}", r.local.files);
    std::thread::sleep(std::time::Duration::from_millis(1100)); // (a file's time moves by the second)
    let modified = || std::fs::metadata(&hook).unwrap().modified().unwrap();
    let before = modified();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(modified(), before, "chmod leaves the modification time");
    f.held_then_trusted("hook", |f| {
        let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    });
}

#[test]
fn an_ssh_command_is_held_and_the_users_own_comes_back() {
    let f = Fixture::new("ssh");
    f.set("remote.s.url", "ssh://127.0.0.1:1/x");
    f.set("core.sshCommand", &f.marker("ssh", "fail"));
    f.set_user("core.sshCommand", &f.marker("ssh-users", "fail"));
    f.held_then_trusted("ssh", |f| {
        let _ = hooked(&f.repo, &["fetch", "-q", "s"]);
    });
    // (held: the user's own ran instead)
    f.set("core.sshCommand", &f.marker("ssh2", "fail"));
    f.clear();
    let _ = hooked(&f.repo, &["fetch", "-q", "s"]);
    assert!(!f.ran("ssh2") && f.ran("ssh-users"));
}

#[test]
fn a_git_proxy_is_held() {
    let f = Fixture::new("gitproxy");
    f.set("remote.g.url", "git://127.0.0.1:1/x");
    f.set("core.gitProxy", &f.marker("gitproxy", "fail"));
    f.held_then_trusted("gitproxy", |f| {
        let _ = hooked(&f.repo, &["fetch", "-q", "g"]);
    });
}

/// `git credential fill` as git asks it on a push to an https remote.
fn credential(f: &Fixture) {
    let cmd = f.resolved().hooked_command(&["credential", "fill"]).unwrap();
    let _ = crate::gitrepo::ran(&cmd, Some(b"protocol=https\nhost=example.com\n\n"));
}

#[test]
fn a_credential_helper_is_held() {
    let f = Fixture::new("credhelper");
    f.set("credential.helper", &f.marker("helper", "ok"));
    f.held_then_trusted("helper", credential);
}

#[test]
fn an_askpass_is_held() {
    let f = Fixture::new("askpass");
    f.set("core.askPass", &f.marker("askpass", "ok"));
    f.held_then_trusted("askpass", credential);
}

#[test]
fn an_upload_pack_is_held_on_fetch_and_a_receive_pack_on_push() {
    let f = Fixture::new("packs");
    f.set("remote.l.uploadpack", &f.marker("uploadpack", "fail"));
    f.set("remote.l.receivepack", &f.marker("receivepack", "fail"));
    f.held_then_trusted("uploadpack", |f| {
        let _ = hooked(&f.repo, &["fetch", "-q", "l"]);
    });
    let f = Fixture::new("packs2");
    f.set("remote.l.receivepack", &f.marker("receivepack", "fail"));
    f.held_then_trusted("receivepack", |f| {
        let _ = hooked(&f.repo, &["push", "-q", "l", "HEAD:refs/heads/x"]);
    });
}

#[test]
fn the_ext_protocol_is_held() {
    let f = Fixture::new("ext");
    f.set("protocol.ext.allow", "always");
    f.set("remote.e.url", &format!("ext::{} %S", f.marker("ext", "fail")));
    f.held_then_trusted("ext", |f| {
        let _ = hooked(&f.repo, &["fetch", "-q", "e"]);
    });
}

/// `protocol.allow=always` alone opens `ext::`: held, git's own `never` for it is back.
#[test]
fn the_ext_protocol_is_held_under_protocol_allow() {
    let f = Fixture::new("extall");
    f.set("protocol.allow", "always");
    f.set("remote.e.url", &format!("ext::{} %S", f.marker("ext", "fail")));
    let h = said(&f.resolved().held.clone().unwrap());
    assert!(h.contains(&"protocol.ext.allow=never".to_string()), "{h:?}");
    f.held_then_trusted("ext", |f| {
        let _ = hooked(&f.repo, &["fetch", "-q", "e"]);
    });
}

#[test]
fn an_alternate_refs_command_is_held() {
    let f = Fixture::new("altrefs");
    let other = f.dir.join("alt.git");
    let ok = Command::new("git").args(["clone", "-q", "--bare"]).arg(&f.repo).arg(&other).output().unwrap();
    assert!(ok.status.success());
    std::fs::write(
        f.repo.join(".git/objects/info/alternates"),
        format!("{}\n", other.join("objects").display()),
    )
    .unwrap();
    f.set("core.alternateRefsCommand", &f.marker("altrefs", "ok"));
    f.held_then_trusted("altrefs", |f| {
        let _ = hooked(&f.repo, &["push", "-q", "l", "HEAD:refs/heads/x"]);
        let _ = hooked(&f.repo, &["fetch", "-q", "l"]);
    });
}

#[test]
fn a_gpg_program_is_held_on_a_signed_commit_and_the_users_own_comes_back() {
    let f = Fixture::new("gpg");
    f.set("commit.gpgSign", "true");
    f.set("gpg.program", &f.marker("gpg", "fail"));
    f.set_user("gpg.program", &f.marker("gpg-users", "fail"));
    f.held_then_trusted("gpg", |f| {
        let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    });
    f.set("gpg.program", &f.marker("gpg2", "fail"));
    f.clear();
    let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    assert!(!f.ran("gpg2") && f.ran("gpg-users"), "held: the user's own program instead");
}

#[test]
fn an_ssh_signing_key_command_is_held() {
    let f = Fixture::new("sshkey");
    f.set("commit.gpgSign", "true");
    f.set("gpg.format", "ssh");
    f.set_user("gpg.ssh.program", &f.marker("ssh-keygen", "fail"));
    f.set("gpg.ssh.defaultKeyCommand", &f.marker("keycmd", "fail"));
    f.held_then_trusted("keycmd", |f| {
        let _ = hooked(&f.repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
    });
}

/// `remote.<n>.vcs` names a helper `git-remote-<vcs>` that has to be on PATH (no test can put one there
/// without the test binary's environment): what holds it is checked — the remote's helper emptied.
#[test]
fn a_remote_helper_is_held() {
    let f = Fixture::new("vcs");
    f.set("remote.v.url", "x::y");
    f.set("remote.v.vcs", "tarae-test-helper");
    let r = f.resolved();
    assert!(said(r.held.as_ref().unwrap()).contains(&"remote.v.vcs=".to_string()));
}

/// Keys tarae's git never reaches (`gittrust.md`): set, and trusted (nothing held), none runs on the calls
/// tarae makes — a call that does is a new row for `GUARDED`.
#[test]
fn keys_never_reached_run_on_none_of_taraes_calls() {
    let f = Fixture::new("never");
    for (key, mark) in [
        ("core.pager", "pager"),
        ("pager.log", "pagerlog"),
        ("core.editor", "editor"),
        ("sequence.editor", "seqeditor"),
        ("trailer.t.command", "trailer"),
        ("difftool.d.cmd", "difftool"),
        ("mergetool.m.cmd", "mergetool"),
    ] {
        f.set(key, &f.marker(mark, "fail"));
    }
    f.set("diff.tool", "d");
    f.set("merge.tool", "m");
    assert!(f.resolved().held.is_none(), "none of them is held");
    f.touch("a.x");
    let _ = crate::gitpanel::status(&f.repo, None);
    let _ = crate::gitpanel::show(&f.repo, "HEAD");
    let _ = crate::snapshot::tree(&f.repo);
    blame(&f);
    let msg = f.dir.join("msg");
    std::fs::write(&msg, "x\n").unwrap();
    let _ =
        hooked(&f.repo, &["commit", "-q", "--allow-empty", "--cleanup=strip", "-F", msg.to_str().unwrap()]);
    let _ = crate::gitrepo::git(&f.repo, &["log", "-3", "--format=%h%x1f%s"], None);
    let ran: Vec<String> = std::fs::read_dir(&f.marks)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(ran.is_empty(), "ran: {ran:?}");
}

/// A submodule's own config: held on its files' git by its own answer, not the repository's around it — and
/// its resolve kept (a gutter read again runs no `git config`) until its config changes.
#[test]
fn a_submodules_own_commands_are_held() {
    let f = Fixture::new("submodule");
    f.submodule("lib");
    let file = f.repo.join("lib/x.txt");
    let places = Places::only(&f.repo);
    let sub = |f: &Fixture| crate::gitrepo::for_file(&f.repo, &file).unwrap().0;
    settled();
    let first = sub(&f);
    assert_eq!(first.git_dir, f.repo.join(".git/modules/lib"));
    assert!(first.held.is_none());
    assert_eq!(crate::git::index_blob(&places, &file).unwrap().as_deref(), Some("lib's\n"));
    assert!(Arc::ptr_eq(&first, &sub(&f)), "kept: no `git config` again");

    std::thread::sleep(std::time::Duration::from_millis(1100)); // (a file's time moves by the second)
    f.git(&["config", "--file", ".git/modules/lib/config", "core.fsmonitor", &f.marker("fsmonitor", "fail")]);
    let r = sub(&f);
    assert!(!Arc::ptr_eq(&first, &r), "its config changed: resolved again");
    assert!(r.held.is_some() && r.asks, "{:?}", r.local.found);
    let parent = f.resolved();
    assert!(parent.local.found.is_empty(), "the repository around it runs nothing of its own");
    assert!(parent.held.as_ref().is_some_and(|h| !h.exclude.is_empty()), "but keeps out of it");
    let status = |f: &Fixture| {
        f.touch("lib/x.txt");
        let _ = sub(f).run(&["status", "--porcelain"], None);
        let _ = crate::git::index_blob(&places, &file);
    };
    f.clear();
    status(&f);
    assert!(!f.ran("fsmonitor"), "ran while held");
    answered(&r.config_file(), r.local.digest(), true);
    assert!(sub(&f).held.is_none(), "trusted, nothing held");
    f.clear();
    status(&f);
    assert!(f.ran("fsmonitor"), "trusted: it runs (or the test can't see it)");
}

/// What `held_submodule_runs` does: its name, whether git looks into the submodule there once it's trusted
/// (measured on git 2.54 — `false`: held all the same, in case a git does), the call.
type Op = (&'static str, bool, Box<dyn Fn(&Fixture)>);

/// Every parent run tarae makes that git can take into a submodule.
fn held_submodule_runs() -> Vec<Op> {
    let git =
        |args: &'static [&'static str]| move |f: &Fixture| drop(crate::gitrepo::git(&f.repo, args, None));
    // (the user's own config asks to recurse, on that call only)
    let users = |key: &'static str, call: Box<dyn Fn(&Fixture)>| {
        Box::new(move |f: &Fixture| {
            f.set_user(key, "true");
            call(f);
            f.unset_user(key);
        })
    };
    vec![
        ("the git tab's status", true, Box::new(|f| drop(crate::gitpanel::status(&f.repo, None)))),
        (
            "a review's diff",
            true,
            Box::new(git(&["diff-index", "-p", "--no-color", "--no-ext-diff", "HEAD"])),
        ),
        ("a diff", true, Box::new(git(&["diff", "--numstat"]))),
        ("a run's snapshot", true, Box::new(|f| drop(crate::snapshot::tree(&f.repo)))),
        (
            "a commit with nothing staged",
            true,
            Box::new(|f| drop(hooked(&f.repo, &["commit", "-q", "-m", "x"]))),
        ),
        ("staging everything", true, Box::new(git(&["add", "--all"]))),
        ("unstaging", false, Box::new(git(&["reset", "-q"]))),
        ("a fast-forward", false, Box::new(git(&["merge", "--ff-only", "-q", "HEAD"]))),
        ("the rebase's line", false, Box::new(|f| f.run_line("rebase -q -f HEAD~1"))),
        (
            "a fetch, the user's config recursing",
            true,
            users("fetch.recurseSubmodules", Box::new(|f| drop(hooked(&f.repo, &["fetch", "-q", "l"])))),
        ),
        (
            "a checkout and back, the user's config recursing",
            false,
            users(
                "submodule.recurse",
                Box::new(|f| {
                    let _ = hooked(&f.repo, &["checkout", "-q", "pre"]);
                    let _ = hooked(&f.repo, &["checkout", "-q", "main"]);
                }),
            ),
        ),
        (
            "a stash",
            true,
            Box::new(|f| {
                f.touch("e.n");
                drop(hooked(&f.repo, &["stash", "push", "--include-untracked"]));
            }),
        ),
        ("a stash taken back", true, Box::new(|f| drop(hooked(&f.repo, &["stash", "pop"])))),
    ]
}

/// A submodule's own commands held: the repository around it keeps out of it on every run that would look
/// in — though its `.gitmodules` says `ignore = none` (which `diff.ignoreSubmodules` loses to) and its name
/// holds a `=` (which `-c` would split at) — and the git tab says why its changes don't show. After `y`
/// they show, and the runs look in again (so this test sees them when they do).
#[test]
fn a_held_submodule_is_kept_out_of_the_parents_runs() {
    let f = Fixture::new("keepout");
    f.git(&["tag", "pre"]);
    f.submodule("li=b");
    f.git(&["config", "--file", ".gitmodules", "submodule.li=b.ignore", "none"]);
    f.git(&["commit", "-qam", "ignore none"]);
    let own = |k: &str, v: &str| f.git(&["config", "--file", ".git/modules/li=b/config", k, v]);
    own("core.fsmonitor", &f.marker("fsmonitor", "fail"));
    own("remote.origin.uploadpack", &f.marker("uploadpack", "fail"));
    let parent = f.resolved();
    assert!(parent.local.found.is_empty() && !parent.asks, "nothing of its own");
    let [sub] = &parent.subs[..] else { panic!("{:?}", parent.subs) };
    assert!(sub.rel == "lib" && sub.repo.held.is_some() && sub.repo.asks);
    assert!(said(parent.held.as_ref().unwrap()).contains(&"submodule.li=b.ignore=dirty".to_string()));
    assert_eq!(parent.asking().len(), 1, "its card");

    // The git tab: its row, and why its changes don't show
    f.touch("lib/x.txt");
    let st = crate::gitpanel::status(&f.repo, None).unwrap();
    assert_eq!(st.held, [("lib".to_string(), true)]);
    let rows = crate::sidebar::git_rows(&st);
    let at = rows.iter().position(|l| l.label == "lib").expect("its row");
    let notes: Vec<&str> = rows[at + 1..at + 3].iter().map(|l| l.label.as_str()).collect();
    assert_eq!(notes, crate::sidebar::HELD_ASKS);

    let ran = |f: &Fixture| f.ran("fsmonitor") || f.ran("uploadpack");
    for (what, _, call) in held_submodule_runs() {
        f.touch("lib/x.txt");
        f.clear();
        call(&f);
        assert!(!ran(&f), "{what}: looked into the held submodule");
    }

    // After `y`: its changes show, and git looks in again
    let sub = f.resolved().subs[0].repo.clone();
    answered(&sub.config_file(), sub.local.digest(), true);
    assert!(f.resolved().held.is_none(), "nothing held any more");
    f.git(&["reset", "-q", "--hard"]);
    f.touch("lib/x.txt");
    f.clear();
    let st = crate::gitpanel::status(&f.repo, None).unwrap();
    assert!(st.held.is_empty() && st.unstaged.iter().any(|e| e.rel == "lib"), "{:?}", st.unstaged);
    for (what, looks, call) in held_submodule_runs() {
        f.touch("lib/x.txt");
        f.clear();
        call(&f);
        if looks {
            assert!(ran(&f), "{what}: trusted, it looks in (or this test can't see it)");
        }
    }
}

/// A submodule's git folder that isn't checked out now — a checkout of a commit without it, then back,
/// would check it out — held too: the repository around it keeps out of it.
#[test]
fn a_held_submodule_not_checked_out_is_kept_out_of() {
    let f = Fixture::new("dormant");
    f.git(&["tag", "pre"]);
    f.submodule("lib");
    f.git(&["config", "--file", ".git/modules/lib/config", "core.fsmonitor", &f.marker("fsmonitor", "fail")]);
    f.git(&["checkout", "-q", "pre"]);
    std::fs::remove_dir_all(f.repo.join("lib")).unwrap();
    let parent = f.resolved();
    assert!(parent.subs.is_empty() && parent.dormant.len() == 1, "{:?}", parent.dormant);
    assert!(parent.held.is_some() && parent.asking().len() == 1, "kept out of, its card asked");
    f.set_user("submodule.recurse", "true");
    f.clear();
    let _ = hooked(&f.repo, &["checkout", "-q", "main"]);
    assert!(!f.ran("fsmonitor"), "held: not checked out into");
    assert!(!f.repo.join("lib/x.txt").exists());
    // (trusted, the same checkout goes into it — so the test sees it when it does)
    f.unset_user("submodule.recurse");
    f.git(&["checkout", "-q", "pre"]);
    std::fs::remove_dir_all(f.repo.join("lib")).ok();
    let dormant = f.resolved().dormant[0].clone();
    answered(&dormant.config_file(), dormant.local.digest(), true);
    f.set_user("submodule.recurse", "true");
    f.clear();
    let _ = hooked(&f.repo, &["checkout", "-q", "main"]);
    assert!(f.repo.join("lib/x.txt").exists(), "trusted: checked out");
}

/// A submodule added after the repository was resolved — its own commands set — is seen by the next run:
/// `.gitmodules` and `modules/` are watched. A stage alone (the index written) keeps the resolve.
#[test]
fn a_submodule_added_after_the_resolve_is_caught() {
    let f = Fixture::new("addedsub");
    settled();
    let first = f.resolved();
    assert!(first.subs.is_empty() && first.held.is_none());
    f.touch("e.n");
    f.git(&["add", "e.n"]);
    assert!(Arc::ptr_eq(&first, &f.resolved()), "a stage: kept");
    std::thread::sleep(std::time::Duration::from_millis(1100)); // (a file's time moves by the second)
    f.submodule("lib");
    f.git(&["config", "--file", ".git/modules/lib/config", "core.fsmonitor", &f.marker("fsmonitor", "fail")]);
    let r = f.resolved();
    assert!(!Arc::ptr_eq(&first, &r), "resolved again");
    let [sub] = &r.subs[..] else { panic!("{:?}", r.subs) };
    assert!(sub.repo.held.is_some() && sub.repo.asks, "{:?}", sub.repo.local.found);
    assert!(r.held.as_ref().is_some_and(|h| !h.exclude.is_empty()), "kept out of");
    f.touch("lib/x.txt");
    f.clear();
    let _ = crate::gitpanel::status(&f.repo, None);
    assert!(!f.ran("fsmonitor"));
}

/// A submodule not checked out whose own git folder holds a submodule's of its own (`modules/lib/modules/
/// inner`): that one's commands found and held too — a recursive checkout bringing `lib` back would check
/// `inner` out with it — and the repository around them keeps out of both until it's trusted.
#[test]
fn a_nested_submodule_of_one_not_checked_out_is_held() {
    let f = Fixture::new("nested");
    f.git(&["tag", "pre"]);
    // (`inner` inside `lib`, then `lib` here, both checked out)
    let inner = f.dir.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("y.txt"), "inner's\n").unwrap();
    let id = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "protocol.file.allow=always"];
    for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-qm", "inner"]] {
        f.git_at(&inner, &[&id[..], args].concat());
    }
    let lib = f.dir.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("x.txt"), "lib's\n").unwrap();
    f.git_at(&lib, &[&id[..], &["init", "-q", "-b", "main"]].concat());
    f.git_at(&lib, &[&id[..], &["submodule", "add", "-q", inner.to_str().unwrap(), "inner"]].concat());
    f.git_at(&lib, &[&id[..], &["add", "-A"]].concat());
    f.git_at(&lib, &[&id[..], &["commit", "-qm", "lib"]].concat());
    f.git(&[&id[..], &["submodule", "add", "-q", lib.to_str().unwrap(), "lib"]].concat());
    f.git(&[&id[..], &["submodule", "update", "-q", "--init", "--recursive"]].concat());
    f.git(&["commit", "-qm", "lib"]);
    let nested = ".git/modules/lib/modules/inner/config";
    f.git(&["config", "--file", nested, "core.fsmonitor", &f.marker("fsmonitor", "fail")]);
    // Not checked out: `lib` gone with the checkout of a commit before it
    f.git(&["checkout", "-q", "pre"]);
    std::fs::remove_dir_all(f.repo.join("lib")).unwrap();
    let parent = f.resolved();
    let [lib_repo] = &parent.dormant[..] else { panic!("{:?}", parent.dormant) };
    assert!(lib_repo.local.found.is_empty() && !lib_repo.asks, "lib runs nothing of its own");
    let [inner_repo] = &lib_repo.dormant[..] else { panic!("{:?}", lib_repo.dormant) };
    assert!(inner_repo.held.is_some() && inner_repo.asks, "{:?}", inner_repo.local.found);
    assert!(lib_repo.held.is_some() && parent.held.is_some(), "kept out of, all the way up");
    let asking: Vec<PathBuf> = parent.asking().iter().map(|r| r.git_dir.clone()).collect();
    assert_eq!(asking, [f.repo.join(".git/modules/lib/modules/inner")], "its card");
    // (a change to it is seen: its config stamped)
    assert!(parent.stamps.iter().any(|(p, _)| *p == f.repo.join(nested)));
    f.set_user("submodule.recurse", "true");
    f.clear();
    let _ = hooked(&f.repo, &["checkout", "-q", "main"]);
    assert!(!f.ran("fsmonitor") && !f.repo.join("lib/inner/y.txt").exists(), "held: not checked out into");
    // (trusted, the same checkout goes into both — so the test sees it when it does)
    f.unset_user("submodule.recurse");
    f.git(&["checkout", "-q", "pre"]);
    std::fs::remove_dir_all(f.repo.join("lib")).ok();
    let inner_repo = f.resolved().dormant[0].dormant[0].clone();
    answered(&inner_repo.config_file(), inner_repo.local.digest(), true);
    assert!(f.resolved().held.is_none(), "trusted: nothing held");
    f.set_user("submodule.recurse", "true");
    let _ = hooked(&f.repo, &["checkout", "-q", "main"]);
    assert!(f.repo.join("lib/inner/y.txt").exists(), "trusted: checked out, the nested one too");
}

/// The rebase's line holds everything every run holds — the network's keys too (a credential helper, ssh,
/// a proxy, the protocols) — named in the line, no value as text: each in its environment, the user's own
/// put back (a credential helper holding a token here) as well. Run, the repository's helper doesn't run.
#[test]
fn the_rebases_line_holds_the_network_keys_and_shows_no_value() {
    let f = Fixture::new("rebaseline");
    f.set("credential.helper", &f.marker("helper", "ok"));
    f.set_user("credential.helper", "store --file /x/tarae-fake-token");
    f.set("core.sshCommand", &f.marker("ssh", "fail"));
    f.set_user("core.sshCommand", "ssh -i /x/tarae-fake-key");
    f.set("core.gitProxy", &f.marker("proxy", "fail"));
    f.set("protocol.allow", "always");
    f.set("filter.fx.clean", &f.marker("clean", "pass"));
    f.set_user("filter.fx.clean", "cat # the-users-filter");
    let r = f.resolved();
    let hold = r.held.clone().unwrap();
    let line = r.shell_line();
    for not in ["tarae-fake", "the-users-filter", "bin-"] {
        assert!(!line.text.contains(not), "{not} in the line: {}", line.text);
    }
    let names: Vec<&str> = line.text.split(' ').filter_map(|w| w.strip_prefix("'--config-env=")).collect();
    let keys: Vec<&str> = names.iter().filter_map(|n| n.rsplit_once('=').map(|(k, _)| k)).collect();
    let held: Vec<&str> = hold.config.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, held, "every held setting, in order: {}", line.text);
    for k in ["credential.helper", "core.sshcommand", "protocol.allow", "protocol.ext.allow"] {
        assert!(keys.contains(&k), "{k}: {keys:?}");
    }
    let env = |k: &str| line.env.iter().filter(|(n, _)| n == k).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
    assert_eq!(env("GIT_PROXY_COMMAND"), ["false"]);
    assert_eq!(env("GIT_NO_LAZY_FETCH"), ["1"]);
    let values: Vec<&str> = line.env.iter().map(|(_, v)| v.as_str()).collect();
    assert!(values.contains(&"store --file /x/tarae-fake-token"), "the user's own, in the environment");
    // (run: the repository's own helper held, as on every run)
    let ask = format!("printf 'protocol=https\\nhost=example.com\\n\\n' | {} credential fill", line.text);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(ask)
        .envs(line.env.clone())
        .env("GIT_CONFIG_GLOBAL", f.dir.join("user.gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(&f.repo)
        .output();
    assert!(!f.ran("helper"), "the repository's credential helper ran from the rebase's line");
}

/// A partial clone missing an object the rebase needs: the rebase's git doesn't fetch it from the remote
/// (`NO_LAZY_FETCH`) — it fails, saying what's missing, and the object is still missing after.
#[test]
fn the_rebases_line_never_fetches_a_missing_object() {
    let f = Fixture::new("partial");
    f.set("uploadpack.allowFilter", "true");
    let clone = f.dir.join("clone");
    let url = format!("file://{}", f.repo.display());
    let args = ["-c", "protocol.file.allow=always", "clone", "-q", "--filter=blob:none", "-b", "main", &url];
    f.git_at(&f.dir, &[&args[..], &[clone.to_str().unwrap()]].concat());
    // (side's own `a.x` was never fetched — git asked without fetching it says so)
    let missing = |f: &Fixture| {
        let out = Command::new("git")
            .args(["cat-file", "-e", "origin/side:a.x"])
            .current_dir(&clone)
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_CONFIG_GLOBAL", f.dir.join("user.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        !out.status.success()
    };
    assert!(missing(&f), "the stand-in: side's blob not in the clone");
    let line = repo(&clone).unwrap().shell_line();
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{} -c protocol.file.allow=always rebase -q origin/side", line.text))
        .envs(line.env)
        .env("GIT_CONFIG_GLOBAL", f.dir.join("user.gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(&clone)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && said.contains("lazy fetch"), "{said}");
    assert!(missing(&f), "not fetched");
}

/// Ending a session whose checkout has a submodule checked out (`worktree::remove`, through the git folder
/// alone — which finds no submodules): git refuses to remove it, and `--force` (changes the card said go)
/// runs no status of it — so nothing of the submodule's runs, held or not.
#[test]
fn ending_a_checkout_with_a_submodule_looks_into_none() {
    let f = Fixture::new("endsub");
    f.submodule("lib");
    let wt = f.dir.join("wt");
    f.git(&["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "main"]);
    f.git_at(&wt, &["-c", "protocol.file.allow=always", "submodule", "update", "-q", "--init"]);
    let config = f.repo.join(".git/worktrees/wt/modules/lib/config");
    f.git(&["config", "--file", config.to_str().unwrap(), "core.fsmonitor", &f.marker("fsmonitor", "fail")]);
    std::fs::write(wt.join("lib/x.txt"), "changed\n").unwrap();
    f.clear();
    let common = Repo::git_folder(&f.repo.join(".git"));
    let e = common.run_no_hooks(&["worktree", "remove", wt.to_str().unwrap()]).unwrap_err();
    assert!(e.contains("submodules"), "{e}");
    assert!(common.run_no_hooks(&["worktree", "remove", "--force", wt.to_str().unwrap()]).is_ok());
    assert!(!f.ran("fsmonitor") && !wt.exists());
}

/// `Hold::args`: `add` gets the held submodules' exclusions after its own pathspecs — `:/` first when it
/// named none (the whole tree, from wherever it runs); the flags only where a subcommand takes them.
#[test]
fn a_held_submodule_is_left_out_of_add() {
    let mut h = Hold::default();
    h.keep_out(&["a=b".into()], &["lib".into()]);
    let ex = ":(exclude,top,literal)lib";
    assert_eq!(h.args(&["add", "--all"]), ["add", "--all", "--", ":/", ex]);
    assert_eq!(h.args(&["add", "-A", "--", "."]), ["add", "-A", "--", ".", ex]);
    assert_eq!(h.args(&["add", "--all", "--", "src/a.rs"]), ["add", "--all", "--", "src/a.rs", ex]);
    assert_eq!(h.args(&["add", "-u", "--"]), ["add", "-u", "--", ":/", ex]);
    assert_eq!(h.args(&["status", "-z"]), ["status", "--ignore-submodules=dirty", "-z"]);
    assert_eq!(h.args(&["fetch", "l"]), ["fetch", "--no-recurse-submodules", "l"]);
    assert_eq!(h.args(&["commit", "-m", "x"]), ["commit", "-m", "x"]);
    assert_eq!(h.config.last(), Some(&("submodule.a=b.ignore".to_string(), "dirty".to_string())));
    // (git splits `--config-env` at its last `=`: the name whole)
    let (arg, var, value) = h.settings().pop().unwrap();
    assert_eq!(arg, "--config-env=submodule.a=b.ignore=TARAE_GIT_HELD_6");
    assert_eq!((var.as_str(), value), ("TARAE_GIT_HELD_6", "dirty"));
    assert!(h.env.is_empty());
}

/// The repository's git folder named alone (`Repo::git_folder` — ending a session: `worktree remove`, whose
/// own status of the checkout reads its index): its config held as a checkout's.
#[test]
fn a_git_folder_named_alone_has_its_commands_held() {
    let f = Fixture::new("gitfolder");
    f.set("core.fsmonitor", &f.marker("fsmonitor", "fail"));
    let side = f.dir.join("side");
    let add = |f: &Fixture| f.git(&["worktree", "add", "-q", side.to_str().unwrap(), "side"]);
    let remove = |f: &Fixture| {
        let common = Repo::git_folder(&f.repo.join(".git"));
        assert_eq!(common.held.is_some(), common.asks);
        let _ = common.run_no_hooks(&["worktree", "remove", side.to_str().unwrap()]);
    };
    add(&f);
    f.held_then_trusted("fsmonitor", |f| {
        if !side.exists() {
            add(f);
        }
        f.clear();
        remove(f);
    });
}

/// A file outside the sessions, in a repository found from its folder (runs not sandboxed here): its own
/// commands held as a session's.
#[test]
fn a_file_outside_the_sessions_has_its_repositorys_commands_held() {
    let f = Fixture::new("outside");
    f.set("diff.dx.textconv", &f.marker("textconv", "file"));
    let elsewhere = f.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let places = Places::only(&elsewhere);
    let (r, rel) = crate::gitrepo::for_file_in(&places, &f.repo.join("a.x")).unwrap();
    assert!(r.held.is_some() && r.asks);
    let _ = r.run(&["blame", "--porcelain", "--", &rel], None);
    assert!(!f.ran("textconv"));
}
