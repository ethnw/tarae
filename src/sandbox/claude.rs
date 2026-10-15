//! The claude process itself under a Seatbelt profile of its own (V1d, decision 17: files only — the network,
//! the programs it starts and the system's services as they are, but your SSH agent's socket: G20). `(allow default)` and denies where the harm is
//! (`target/sb/v1d-plan.md`, from what claude 2.1.296 touched — `target/sb/v1b-mac.log`): it reads no place that
//! holds keys (but the keychain: `security` reads its login there), no file named like a key in home or the
//! project, and nothing of tarae's state but the log; it writes only its own (`~/.claude/`, `~/.claude.json` and
//! what it writes beside it, `~/.local/state/claude/`), the temp folders and the devices — and the project only in
//! the edit arms that give it Claude Code's Edit and Write, never `.git` nor a protected name there (what runs
//! later). So a bug in its permission path, or a feature acting by itself, can't write what runs later or loosens
//! tarae (`~/.config/tarae`, `~/.zshrc`, `~/.local/bin` — its own updater's link too: decision A), nor read
//! `~/.ssh`. The deny lists are the runs' (`kept_reads`, `key_files`, `protected_writes`), never a copy; paths
//! only as `-D` parameters. Linux: the same pieces as Landlock rules, claude's folder tarae's (`linux`,
//! `claudehome.rs` — `~/.claude.json`'s atomic write would need home itself writable: the threat model's G1).

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use super::{
    CLAUDES, Denial, Here, TMP, Watch, devices_rule, kept_reads, key_files, profile_path, protected_writes,
    user_temp, vars_in,
};

/// Claude's own folders in home, written (V1b): its settings, conversations, sessions and plugins' marks, and its
/// program's version locks.
const OWN_DIRS: [&str; 2] = [".claude", ".local/state/claude"];

/// `~/.claude.json` and what claude writes beside it (V1b): its lock, the copy renamed over it, its backups.
const OWN_FILE: &str = r"\.claude\.json(\.lock|\.tmp\.[^/]+|\.backup[^/]*)?";

/// What in `~/.claude/` your own Claude Code — outside tarae, unconfined, its hooks on — runs or obeys later, so
/// never claude's to write from here: its settings (hooks, permissions, env, apiKeyHelper), memory, commands,
/// agents, skills, hooks, output styles, plugins. In any case (APFS). V1b saw claude 2.1.296 write none of them
/// but plugins' marks (`PLUGIN_MARKS`). A denylist, not an allowlist of what claude writes: a refusal of claude's
/// own (not a tool's) goes unseen — the kernel never reported one live (V1d), and no tool result says it — so an
/// allowlist waits for a way to see them (the plan's follow-up). Also: the local installer's claude program your
/// shell's alias runs (`local/`), the IDE locks your later claude connects by (`ide/`), and the shell snapshots it
/// sources (`shell-snapshots/` — whether one it didn't make is sourced can't be told from here; V1b saw none
/// written: tarae's chat has no Bash).
const RUNS_LATER: [&str; 12] = [
    "local",
    "ide",
    "shell-snapshots",
    "settings.json",
    "settings.local.json",
    "CLAUDE.md",
    "commands",
    "agents",
    "skills",
    "hooks",
    "output-styles",
    "plugins",
];

/// …and in `projects/`, every project's but this one's (`project_folder`) — a conversation resumed later is obeyed
/// as its history — and in this one's its auto-memory (`memory/`): your own Claude Code reads it later as
/// instructions, as it does `CLAUDE.md` (V1b saw claude make the folder at its start — a new project's now isn't).
const MEMORY: [&str; 2] = ["projects", "memory"];

/// The folder claude keeps a project's conversations in, below `~/.claude/projects/`: its path with every
/// character but a letter or digit a `-` (V1b: `/private/var/…/tarae-v1b.oqL1bjVY/proj` →
/// `-private-var-…-tarae-v1b-oqL1bjVY-proj`).
pub fn project_folder(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// …but what claude writes in `plugins/` (V1b), each by name: a plugin's in-use mark (`cache/…/.in_use/<pid>`, and
/// its temp copy), the lists' locks (`known_marketplaces*.json.lock`), and in each `synced/<account>/` the synced
/// marketplace list and the time of its last round — a list of where plugins come from, read, not run (installing
/// from one is your act); nothing else there, a marketplace's code or hooks, is claude's to write. A regex below
/// `~/.claude/`.
const PLUGIN_MARKS: &str = r"plugins/(cache/(.*/)?\.in_use/[^/]+|[^/]+\.lock|synced/[^/]+/(\.marketplaces\.json(\.tmp\.[^/]+)?|\.last-complete-round))";

/// Kept from runs (`allow::SECRETS`) but claude's to read: `/usr/bin/security` reads its login from the login
/// keychain there (V1b) — Seatbelt can't say "only Claude Code's item" (the threat model says so).
const KEYCHAINS: &str = "Library/Keychains";

/// The program that says what Seatbelt refused, by the profile's tag.
const LOG: &str = "/usr/bin/log";

/// launchd's SSH agent sockets (macOS gives every login one, `SSH_AUTH_SOCK` names it) — a fixed form, no one's
/// path in it. Where depends on the macOS: `/private/tmp/` on the older ones (as widely reported, through 14 at
/// least), `/private/var/run/` on 27.0.1 (measured, the live check) — when it moved isn't known here, so both.
const LAUNCHD_AGENTS: &str = r"^/private/(var/run|tmp)/com\.apple\.launchd\.[^/]+/Listeners$";

/// Claude's profile for one process: where it runs, whose home, which arm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The project (real) — claude's cwd.
    pub root: PathBuf,
    /// Home (real).
    pub home: PathBuf,
    /// The project writable, but `.git` and the protected names: the edit arms with Claude Code's Edit and Write
    /// (`edittool::Arm::Both`, `Native`). Else read-only: tarae's `edit` writes it, never claude.
    pub writes_project: bool,
    /// The temp folders (real): `/private/tmp` (V1b: `/tmp/claude-<uid>/`, its socket in `/tmp/cc-socks/`) and
    /// `$TMPDIR`'s parent, as runs'.
    pub temps: Vec<PathBuf>,
    /// Never read (real): the runs' `kept_reads` less claude's own (`CLAUDES`) and the keychain (`KEYCHAINS`) —
    /// where claude's folder is tarae's (`place`), with them, and with tarae's state that folder is in.
    pub kept: Vec<PathBuf>,
    /// tarae's log, in tarae's state but read: Claude reads it unasked (#45).
    pub log: Option<PathBuf>,
    /// Your SSH agent's socket (real): where `SSH_AUTH_SOCK` led when tarae started — never connected to (G20).
    pub agent: Option<PathBuf>,
    /// What each deny says (`with message`): the refusals' watch finds them by it (`watch`). Linux: the name of
    /// the thread that confines claude (`landlock::thread_name`), which Landlock's audit names.
    pub tag: String,
    /// Linux: claude's folder, tarae's (`claudehome::Place`) — written whole, your `~/.claude` kept (`linux`).
    /// None: macOS, claude's own in home (`OWN_DIRS`).
    pub place: Option<crate::claudehome::Place>,
    /// Linux: its Landlock rules, made on a worker (`linux::for_chat`). None: macOS, or a stand-in's.
    pub rules: Option<Arc<linux::Rules>>,
}

impl Profile {
    /// The profile for claude in `root` with `home` (both made real here), as `here` says the toolchains' places
    /// and the temp folder. A few `stat`s (`profile_path`), as the runs' policy.
    pub fn new(root: &Path, home: &Path, writes_project: bool, here: &Here, tag: &str) -> Profile {
        let (root, home) = (profile_path(root), profile_path(home));
        let vars = vars_in(&home, here);
        let place = crate::claudehome::Place::of(here.os, &|n| here.var(n)).ok().flatten();
        let own: Vec<PathBuf> = match place {
            Some(_) => Vec::new(),
            None => CLAUDES.iter().chain(&[KEYCHAINS]).map(|c| profile_path(&home.join(c))).collect(),
        };
        let mut kept: Vec<PathBuf> =
            kept_reads(&home, &vars).into_iter().filter(|k| !own.contains(k)).collect();
        // (claude's folder is in tarae's state, kept but by its own rule — `linux`)
        kept.extend(place.as_ref().map(|p| profile_path(&p.state)));
        crate::sorting::sort(&mut kept);
        kept.dedup();
        let mut temps = vec![profile_path(Path::new(TMP))];
        temps.extend(here.tmp.as_deref().map(|t| user_temp(&profile_path(t))));
        crate::sorting::sort(&mut temps);
        temps.dedup();
        let log = crate::log::path().map(|l| profile_path(&l));
        let agent = here.path("SSH_AUTH_SOCK").filter(|a| a.is_absolute()).map(|a| profile_path(&a));
        let tag = tag.to_string();
        Profile { root, home, writes_project, temps, kept, log, agent, tag, place, rules: None }
    }

    /// Claude's own folders (`OWN_DIRS`, real).
    fn own_dirs(&self) -> Vec<PathBuf> {
        OWN_DIRS.iter().map(|d| profile_path(&self.home.join(d))).collect()
    }

    /// The kept places (by index) that hold the project, and the others — as runs' (`Policy::kept_split`).
    fn kept_split(&self) -> (Vec<usize>, Vec<usize>) {
        (0..self.kept.len()).partition(|&i| self.root.starts_with(&self.kept[i]))
    }

    /// The Seatbelt profile. Paths aren't in it: they're `-D` parameters (`params`). The later of two rules on a
    /// path wins (measured, `sandbox.rs`'s head), so the order is the rule.
    pub fn text(&self) -> String {
        let deny = |rule: &str| format!("(deny {rule} (with message \"{}\"))\n", self.tag);
        let each =
            |letter: &str, n: usize| (0..n).map(|i| format!(" (subpath (param \"{letter}{i}\"))")).collect();
        let parts = self.home.components().filter(|c| matches!(c, Component::Normal(_))).count();
        let own_file = format!(
            "(require-all (subpath (param \"HOME\")) (regex #\"^{}/{OWN_FILE}$\"))",
            "/[^/]+".repeat(parts)
        );
        let own: String = each("O", OWN_DIRS.len());
        let mut p = String::from("(version 1)\n(allow default)\n");
        // Writes: none, but the temp folders and the devices every program writes…
        p.push_str(&deny("file-write*"));
        let temps: String = each("W", self.temps.len());
        p.push_str(&format!("(allow file-write*{temps})\n"));
        p.push_str(&devices_rule());
        // …nothing in home — after the temp folders, so a home in one (a test's) stays closed — but claude's own
        p.push_str(&deny("file-write* (subpath (param \"HOME\"))"));
        // (in its folder, not what your own Claude Code runs later — but the plugins' marks it writes; by its own
        // real path's depth: a `~/.claude` that's a link elsewhere is matched where it leads)
        let in_own = format!("^{}/", "/[^/]+".repeat(parts_of(&self.own_dirs()[0])));
        let later = RUNS_LATER.iter().map(|n| super::any_case(n)).collect::<Vec<_>>().join("|");
        let mut own_rule = format!("(allow file-write*{own} {own_file})\n");
        // (`~/.claude` itself: never moved aside, removed or made again — a link or a new folder in its place
        // would hold settings and hooks of claude's making; claude writes inside it)
        own_rule.push_str(&deny("file-write* (literal (param \"O0\"))"));
        own_rule.push_str(&deny(&format!(
            "file-write* (require-all (subpath (param \"O0\")) (regex #\"{in_own}({later})(/|$)\"))"
        )));
        let (projects, memory) = (super::any_case(MEMORY[0]), super::any_case(MEMORY[1]));
        own_rule.push_str(&deny(&format!(
            "file-write* (require-all (subpath (param \"O0\")) (regex #\"{in_own}{projects}/\"))"
        )));
        own_rule.push_str("(allow file-write* (subpath (param \"OP\")))\n");
        // (`projects/` and this project's folder themselves: never moved — a folder renamed whole carries what's in
        // it past the denies on its children (Seatbelt checks a rename's two paths), a memory or another project's
        // conversations filled in elsewhere. tarae makes this project's beforehand — `own_project_made`)
        own_rule.push_str(&deny("file-write* (literal (param \"PJ\"))"));
        own_rule.push_str(&deny("file-write* (literal (param \"OP\"))"));
        own_rule.push_str(&deny(&format!(
            "file-write* (require-all (subpath (param \"O0\")) (regex #\"{in_own}{projects}/[^/]+/{memory}(/|$)\"))"
        )));
        own_rule.push_str(&format!(
            "(allow file-write* (require-all (subpath (param \"O0\")) (regex #\"{in_own}{PLUGIN_MARKS}$\")))\n"
        ));
        // …and the project as the arm says (a checkout under `~/.claude/worktrees` stays the project: after
        // claude's own). Where the project holds home (home itself or above), home's deny comes again over it —
        // the project is writable only outside home — and claude's own after that, or claude can't start
        let holds_home = self.home.starts_with(&self.root);
        if !holds_home {
            p.push_str(&own_rule);
        }
        match self.writes_project {
            false => p.push_str(&deny("file-write* (subpath (param \"ROOT\"))")),
            true => {
                p.push_str("(allow file-write* (subpath (param \"ROOT\")))\n");
                p.push_str(&deny(&format!("file-write* {}", protected_writes(&self.root))));
            }
        }
        if holds_home {
            p.push_str(&deny("file-write* (subpath (param \"HOME\"))"));
            p.push_str(&own_rule);
        }
        // Reads: never the places that hold keys, tarae's state, the toolchains' credentials (`kept_reads`, less
        // claude's own and the keychain) — but the project, where one holds it, as runs'…
        let (around, other) = self.kept_split();
        let kept = |v: &[usize]| v.iter().map(|i| format!(" (subpath (param \"X{i}\"))")).collect::<String>();
        if !other.is_empty() {
            p.push_str(&deny(&format!("file-read-data file-read-xattr{}", kept(&other))));
        }
        if !around.is_empty() {
            p.push_str(&deny(&format!(
                "file-read-data file-read-xattr (require-all (require-any{}) (require-not (subpath (param \
                 \"ROOT\"))))",
                kept(&around)
            )));
        }
        // …nor a file named like a key in home or the project, but claude's own (its sessions' `*.key`). Not
        // elsewhere: the system's certificates are `*.pem` (`/etc/ssl/cert.pem`, read by every TLS connection)
        p.push_str(&deny(&format!(
            "file-read-data file-read-xattr (require-all (require-any (subpath (param \"HOME\")) (subpath \
             (param \"ROOT\"))) {} (require-not (require-any{own} {own_file})))",
            key_files("/")
        )));
        // …and tarae's log, last: Claude reads it
        if self.log.is_some() {
            p.push_str("(allow file-read-data file-read-xattr (literal (param \"LOG\")))\n");
        }
        // Connecting: never to your SSH agent (G20) — it signs with your keys for whoever reaches its socket. The
        // one `SSH_AUTH_SOCK` led to (claude isn't given the variable, but a path is found elsewhere) and launchd's
        let agent = if self.agent.is_some() { " (literal (param \"AGENT\"))" } else { "" };
        p.push_str(&deny(&format!(
            "network-outbound (require-all (remote unix-socket) (require-any{agent} (regex #\"{LAUNCHD_AGENTS}\")))"
        )));
        p
    }

    /// The profile's parameters (name, value).
    pub fn params(&self) -> Vec<(String, String)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut v = vec![("ROOT".to_string(), s(&self.root)), ("HOME".to_string(), s(&self.home))];
        v.extend(self.own_dirs().iter().enumerate().map(|(i, p)| (format!("O{i}"), s(p))));
        v.push(("PJ".to_string(), s(&self.own_dirs()[0].join(MEMORY[0]))));
        v.push(("OP".to_string(), s(&self.own_project())));
        v.extend(self.temps.iter().enumerate().map(|(i, p)| (format!("W{i}"), s(p))));
        v.extend(self.kept.iter().enumerate().map(|(i, p)| (format!("X{i}"), s(p))));
        v.extend(self.log.iter().map(|l| ("LOG".to_string(), s(l))));
        v.extend(self.agent.iter().map(|a| ("AGENT".to_string(), s(a))));
        v
    }

    /// `sandbox-exec`'s arguments before claude's program.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["-p".to_string(), self.text()];
        for (k, v) in self.params() {
            a.push("-D".into());
            a.push(format!("{k}={v}"));
        }
        a
    }

    /// Whether the profile lets claude read `real` — its read rules, in Rust: what a Read the user would allow
    /// still can't open (`reads.rs` says so instead of asking).
    pub fn reads_allow(&self, real: &Path) -> bool {
        if let Some(place) = &self.place {
            return self.reads_allow_in(place, real);
        }
        if self.log.as_deref() == Some(real) {
            return true;
        }
        let own = self.own_dirs().iter().any(|d| real.starts_with(d))
            || real.parent() == Some(self.home.as_path()) && own_file(real);
        let kept = self
            .kept
            .iter()
            .any(|k| real.starts_with(k) && !(self.root.starts_with(k) && real.starts_with(&self.root)));
        let key = (real.starts_with(&self.home) || real.starts_with(&self.root))
            && !own
            && crate::secretnames::named_secret(real)
            && !real.iter().any(|p| super::outputs().iter().any(|o| p == o.as_str()));
        !kept && !key
    }

    /// A refusal claude's tool result says — Node's words: `EPERM: operation not permitted, open '<path>'` (Seatbelt,
    /// claude's Edit, V1d's live check), `EACCES: permission denied, …` (Landlock), `EXDEV: cross-device link not
    /// permitted, rename '<from>' -> '<to>'` (Landlock's move across folders) — where the profile's rules refuse
    /// that path (the first of a move's two they refuse); its operation as they say (reading, writing, or either
    /// when both are refused). The kernel's own report didn't come for it (only the probes' did; Landlock's only
    /// from ABI 7), so the chat says it from here too. A few `stat`s (the path made real).
    pub fn said_in_result(&self, text: &str) -> Option<Denial> {
        let rest = REFUSED.iter().find_map(|r| Some(&text[text.find(r)? + r.len()..]))?;
        let line = rest.lines().next().unwrap_or_default();
        line.split('\'').skip(1).step_by(2).take(2).find_map(|path| {
            let real = profile_path(Path::new(path));
            let op = match (self.reads_allow(&real), self.writes_allow(&real)) {
                (true, true) => return None,
                (false, false) => super::EITHER,
                (false, true) => "file-read-data",
                (true, false) => "file-write*",
            };
            Some(Denial { process: "claude".into(), op: op.into(), target: real.display().to_string() })
        })
    }

    /// Whether the profile lets claude write `real` — its write rules, in Rust.
    pub fn writes_allow(&self, real: &Path) -> bool {
        if let Some(place) = &self.place {
            return self.writes_allow_in(place, real);
        }
        // (the profile's order: the later rule on a path wins)
        let in_home = real.starts_with(&self.home);
        let mut ok = !in_home
            && (self.temps.iter().any(|t| real.starts_with(t))
                || super::DEVICE_PLACES.iter().any(|d| real.starts_with(d)));
        let in_own = self.own_dirs().iter().any(|d| real.starts_with(d))
            || real.parent() == Some(self.home.as_path()) && own_file(real);
        let own_ok = !self.runs_later(real)
            && real != self.own_dirs()[0]
            && real != self.own_dirs()[0].join(MEMORY[0])
            && real != self.own_project();
        let holds_home = self.home.starts_with(&self.root);
        if in_own && !holds_home {
            ok = own_ok;
        }
        if real.starts_with(&self.root) {
            ok = self.writes_project && !super::protected_in(&self.root, real);
        }
        if holds_home && in_home {
            ok = in_own && own_ok;
        }
        ok
    }

    /// This project's folder of conversations, `~/.claude/projects/<project_folder>` (real).
    fn own_project(&self) -> PathBuf {
        self.own_dirs()[0].join(MEMORY[0]).join(project_folder(&self.root))
    }

    /// This project's folder of conversations (`own_project`) made where it isn't — claude can't make it under its
    /// profile (its path is denied, so it can't be moved whole). From claude's folder (real) down, each step made
    /// (`mkdirat`, 0700) and opened following no link (`open_dir_at`): a link in the way is never gone through nor
    /// removed — Err, and claude runs unconfined, said. No `~/.claude` yet (claude never run — a new user, e2e's
    /// home): made too, from home's own folder, as the steps below it; one that's there, a link to elsewhere
    /// included (dotfiles), is started from where it leads (`own_dirs` made it real).
    pub fn own_project_made(&self) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        let own = self.home.join(OWN_DIRS[0]);
        let missing =
            std::fs::symlink_metadata(&own).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        let (dir, mut names) = match missing {
            true => (self.home.clone(), vec![OWN_DIRS[0].to_string()]),
            false => (self.own_dirs()[0].clone(), Vec::new()),
        };
        names.extend([MEMORY[0].to_string(), project_folder(&self.root)]);
        let said =
            |at: &Path, e: std::io::Error| format!("{}: {e} — a link or no folder there", at.display());
        let c = super::c_name(dir.as_os_str())?;
        let mut fd = super::open_dir_at(libc::AT_FDCWD, &c).map_err(|e| said(&dir, e))?;
        let mut at = dir.clone();
        for name in names {
            let c = super::c_name(std::ffi::OsStr::new(&name))?;
            at.push(&name);
            // SAFETY: a folder descriptor of ours and a name, live for the call (one there already: EEXIST).
            unsafe { libc::mkdirat(fd.as_raw_fd(), c.as_ptr(), 0o700) };
            fd = super::open_dir_at(fd.as_raw_fd(), &c).map_err(|e| said(&at, e))?;
        }
        Ok(())
    }
}

impl Profile {
    /// Whether `real` is in `~/.claude/` and what your own Claude Code runs or obeys later (`RUNS_LATER`, a
    /// project's auto-memory — `MEMORY`), not a plugin's mark claude writes (`PLUGIN_MARKS`) — the profile's rule,
    /// in Rust.
    fn runs_later(&self, real: &Path) -> bool {
        let dir = &self.own_dirs()[0];
        let Ok(rel) = real.strip_prefix(dir) else { return false };
        let parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        // (another project's conversations; this one's auto-memory)
        if let [p, project, rest @ ..] = parts.as_slice()
            && p.eq_ignore_ascii_case(MEMORY[0])
        {
            let memory = rest.first().is_some_and(|m| m.eq_ignore_ascii_case(MEMORY[1]));
            return *project != project_folder(&self.root) || memory;
        }
        let Some(first) = parts.first() else { return false };
        if !RUNS_LATER.iter().any(|n| n.eq_ignore_ascii_case(first)) {
            return false;
        }
        let mark = match parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["plugins", f] => f.ends_with(".lock") && f.len() > ".lock".len(),
            ["plugins", "synced", _, f] => {
                *f == ".last-complete-round"
                    || *f == ".marketplaces.json"
                    || f.strip_prefix(".marketplaces.json.tmp.").is_some_and(|t| !t.is_empty())
            }
            ["plugins", "cache", .., ".in_use", _] => true,
            _ => false,
        };
        !mark
    }
}

/// How a refusal starts in Node's words (`said_in_result`): Seatbelt's, Landlock's, Landlock's move across folders.
pub const REFUSED: [&str; 3] = [
    "EPERM: operation not permitted, ",
    "EACCES: permission denied, ",
    "EXDEV: cross-device link not permitted, ",
];

/// How many named parts a path has (`/a/b`: 2) — a regex's depth, no path in it.
fn parts_of(p: &Path) -> usize {
    p.components().filter(|c| matches!(c, Component::Normal(_))).count()
}

/// The chat's claude's profile in `root` (`writes_project`: the arm) — or why it runs unconfined: no Seatbelt
/// here (tarae itself in a sandbox: they don't nest), no home; Linux: `linux::for_chat`'s. Blocks the first time
/// (`probe`; Linux: the project walked, the audit probed): a worker's. `command`: claude's program (`llm.command`)
/// — on Linux one in a place the rules keep can't run under them (`linux::for_chat`).
pub fn for_chat(root: &Path, writes_project: bool, command: &str) -> Result<Profile, String> {
    #[cfg(target_os = "linux")]
    {
        let here = Here::now();
        let home = here.path("HOME").ok_or("no HOME to keep it to")?;
        linux::for_chat(root, &home, writes_project, command, &here)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        for_chat_seatbelt(root, writes_project)
    }
}

#[cfg(not(target_os = "linux"))]
fn for_chat_seatbelt(root: &Path, writes_project: bool) -> Result<Profile, String> {
    if super::inside() {
        return Err(super::INSIDE.into());
    }
    if let super::Availability::Missing(why) = super::probe() {
        return Err(why);
    }
    let here = Here::now();
    let home = here.path("HOME").ok_or("no HOME to keep it to")?;
    let p = Profile::new(root, &home, writes_project, &here, &tag());
    p.own_project_made().map_err(|e| format!("claude's folder for this project couldn't be made: {e}"))?;
    Ok(p)
}

/// Whether `path`'s name is `~/.claude.json`'s or one claude writes beside it (`OWN_FILE`).
fn own_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    let Some(rest) = name.strip_prefix(".claude.json") else { return false };
    rest.is_empty()
        || rest == ".lock"
        || rest.strip_prefix(".tmp.").is_some_and(|r| !r.is_empty())
        || rest.starts_with(".backup")
}

/// A new tag for a claude process's profile — one no other process's contains (`log stream` matches by
/// `CONTAINS`: the dot after the number keeps `….1.` out of `….10.`). Linux: its confining thread's name, which
/// Landlock's audit names (`landlock::thread_name`, this tarae's alone).
pub fn tag() -> String {
    #[cfg(target_os = "linux")]
    {
        crate::landlock::thread_name()
    }
    #[cfg(not(target_os = "linux"))]
    {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("tarae-claude.{}.{n}.", std::process::id())
    }
}

/// `claude`'s command under `profile`: `sandbox-exec`, which execs claude — the pid is claude's
/// (`Editor::claude_pids`). Linux: claude's own, confined as it starts (`spawn`).
pub fn command(profile: &Profile, program: &str) -> Command {
    #[cfg(target_os = "linux")]
    {
        let _ = profile;
        Command::new(program)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut c = Command::new(super::SANDBOX_EXEC);
        c.args(profile.args()).arg(program);
        c
    }
}

/// `cmd` (`command`'s) started under `profile` — Linux: from a thread confined to its rules (`linux::spawn`);
/// macOS: `sandbox-exec` is in it already. None: as you.
pub fn spawn(profile: Option<&Profile>, cmd: &mut Command) -> std::io::Result<std::process::Child> {
    match profile {
        #[cfg(target_os = "linux")]
        Some(p) => linux::spawn(p, cmd),
        _ => cmd.spawn(),
    }
}

/// What the profile refuses claude, watched by its tag (`log stream`, as runs' `Watch::start`) from now until
/// claude's process `pid` is gone — each refusal handed to `tell` as it comes. Started before claude is, but not
/// waited for (`WATCH_READY`): a refusal in claude's first moments may go unseen (the live check lists them all).
/// Linux: Landlock's audit records of the domain its confining thread made (named by the tag), where they're
/// readable (ABI 7, `linux::audit_readable`) — else nothing comes, and the tool results say what they say.
pub fn watch(profile: &Profile, tell: impl Fn(&Denial) + Send + 'static) -> Arc<Watch> {
    let w = Watch::quiet(&profile.tag);
    #[cfg(target_os = "linux")]
    {
        let Some(source) = profile.rules.as_ref().and_then(|r| r.audit.as_ref().ok()) else { return w };
        let mut audit = linux::audit_of(profile);
        w.read(source.stream(), move |line| {
            let d = audit.line(line);
            d.iter().for_each(&tell);
            d
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let tag = profile.tag.clone();
        let predicate = format!("sender == \"Sandbox\" AND eventMessage CONTAINS \"{tag}\"");
        let mut c = Command::new(LOG);
        c.args(["stream", "--style", "ndjson", "--predicate", &predicate]);
        w.read(c, move |line| {
            let d: Vec<Denial> = super::parse_denial(line, &tag).into_iter().collect();
            d.iter().for_each(&tell);
            d
        });
    }
    w
}

pub mod linux;

#[cfg(test)]
mod tests;
