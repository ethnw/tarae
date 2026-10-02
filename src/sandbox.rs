//! Claude's commands in a sandbox. What Claude runs — `run` and `run_tests` (`tools.rs`) — goes through
//! macOS's Seatbelt (`/usr/bin/sandbox-exec`) with a profile made here (`Policy::profile`): it may write
//! in the project (but `.git`, `.tarae.toml` and `llm.sandbox-protect`), the temp folders and the
//! toolchain's own folders (cargo's target and build dirs, its registry; Go's caches), reach nothing
//! over the network but this machine, and make or reach unix sockets only in the project and its own
//! temp folder (`RunDir`, its `TMPDIR`). What the user runs themselves (`space m m`, `space x x`, installs,
//! `:restart`) isn't sandboxed.
//!
//! Measured on macOS 27 (2026-10-01, `target/sb/` scripts): the later of two rules on a path wins and
//! `require-not` works · a sandboxed process can't apply a different profile (exit 71, `sandbox_apply:
//! Operation not permitted` — so `probe` says so inside one) · `launchctl submit` is refused, Apple Events
//! and LaunchServices are blocked by the rules below · localhost connects (by name too), other addresses
//! fail at once, names don't resolve · tarae's own `cargo test` passes inside, its build dir being
//! `~/.cargo/build/shared` (`build.build-dir`) — which is why the toolchain's dirs come from `cargo
//! metadata`, not a guess. Paths are matched as real paths (`/tmp` = `/private/tmp`): all are canonical.
//!
//! Where there's no sandbox (not macOS, or tarae itself runs inside one) Claude's commands don't run
//! quietly without one: each asks, or the user agrees for the conversation, and the chat's title says so.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Set in a sandboxed command's environment (tarae's own tests read it: a sandbox can't start inside one).
pub const MARK: &str = "TARAE_SANDBOXED";

/// Whether this system can sandbox a command.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Availability {
    /// Not looked yet (`probe` runs on the first command's worker).
    #[default]
    Unknown,
    Ready,
    /// Why there's none.
    Missing(String),
    /// Tests only: Claude's commands run as before — the sandbox's own tests set the others.
    Bypass,
}

/// How a command of Claude's runs.
#[derive(Clone, Debug)]
pub enum Guard {
    In(Policy),
    /// No sandbox here (why) — the user agreed.
    Without(String),
    /// `llm.sandbox` is off.
    Off,
}

impl Guard {
    /// The line Claude reads with the command's result.
    pub fn report(&self) -> String {
        match self {
            Guard::In(p) => format!(
                "In tarae's sandbox: it could write only in the project (not .git or .tarae.toml), temp \
                 folders and the toolchain's own{} — \"Operation not permitted\" is likely the sandbox.",
                if p.network { "" } else { ", and reach nothing over the network but this machine" }
            ),
            Guard::Without(why) => format!("Ran without a sandbox ({why}) — the user agreed."),
            Guard::Off => "Ran without a sandbox — the user turned it off (llm.sandbox).".into(),
        }
    }
}

/// What a sandboxed command may do. Every path is real (canonical).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The project: writable but for `protect`.
    pub root: PathBuf,
    /// Inside `root`, kept as they are (`.git`, `.tarae.toml`, `llm.sandbox-protect`).
    pub protect: Vec<PathBuf>,
    /// Folders writable besides the project (temp, the toolchain's, `llm.sandbox-write`).
    pub write: Vec<PathBuf>,
    /// Single files writable (cargo's lock and cache databases).
    pub files: Vec<PathBuf>,
    /// The network beyond this machine (`llm.sandbox-network`).
    pub network: bool,
    /// This run's own temp folder — its `TMPDIR`, and with the project the only place it may make or reach
    /// a unix socket (`RunDir`).
    pub run: Option<std::sync::Arc<RunDir>>,
}

/// A run's own temp folder. Unix sockets anywhere else stay out of reach: in `/private/tmp` and `$TMPDIR`
/// live tmux's, zellij's and ssh-agent's — servers outside the sandbox that run what they're told
/// (reproduced: `tmux -S /private/tmp/tmux-501/default run-shell "touch ~/marker"` from inside made the
/// marker when temp sockets were allowed). Removed, on a thread of its own, once the run is gone.
#[derive(Debug, PartialEq, Eq)]
pub struct RunDir(PathBuf);

impl RunDir {
    /// A new folder under `$TMPDIR` (real path; short — a socket's path has 104 bytes at most), only ours.
    pub fn new() -> std::io::Result<std::sync::Arc<RunDir>> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = real(&std::env::temp_dir());
        let dir = base.join(format!("tarae-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(std::sync::Arc::new(RunDir(dir)))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let dir = std::mem::take(&mut self.0);
        std::thread::spawn(move || std::fs::remove_dir_all(dir));
    }
}

/// Device files every program writes to (`/dev/null`, terminals — a pty test hangs without them: measured).
const DEVICES: &str = "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
(literal \"/dev/ptmx\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/ttys[0-9]+$\") (subpath \"/dev/fd\"))\n";

/// Ways to get something run outside: Apple Events to other apps, LaunchServices (`open`).
const NO_WAY_OUT: &str = "(deny appleevent-send)\n(deny mach-lookup (global-name \
\"com.apple.coreservices.launchservicesd\") (global-name-regex #\"^com\\.apple\\.lsd\\.\"))\n";

impl Policy {
    /// The Seatbelt profile. Paths aren't in it: they're `-D` parameters (`params`), so no path can change
    /// what it says.
    pub fn profile(&self) -> String {
        let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        let protect: String =
            (0..self.protect.len()).map(|i| format!(" (require-not (subpath (param \"P{i}\")))")).collect();
        p.push_str(&format!("(allow file-write* (require-all (subpath (param \"ROOT\")){protect}))\n"));
        let run = if self.run.is_some() { " (subpath (param \"RUN\"))" } else { "" };
        let dirs: String = (0..self.write.len()).map(|i| format!(" (subpath (param \"W{i}\"))")).collect();
        let files: String = (0..self.files.len()).map(|i| format!(" (literal (param \"F{i}\"))")).collect();
        if !run.is_empty() || !dirs.is_empty() || !files.is_empty() {
            p.push_str(&format!("(allow file-write*{run}{dirs}{files})\n"));
        }
        p.push_str(DEVICES);
        if !self.network {
            p.push_str("(deny network*)\n");
            p.push_str("(allow network-bind network-inbound (local ip \"localhost:*\"))\n");
            p.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        }
        // Unix sockets — made or reached — only in the project and the run's own folder (`RunDir`), the
        // network open or not: the ones elsewhere are servers outside (tmux, zellij, ssh-agent). Last, so
        // these win (the later rule does — measured)
        let here = format!("(require-any (subpath (param \"ROOT\")){run})");
        p.push_str("(deny network-bind network-inbound (local unix-socket))\n");
        p.push_str("(deny network-outbound (remote unix-socket))\n");
        p.push_str(&format!(
            "(allow network-bind network-inbound (require-all (local unix-socket) {here}))\n\
             (allow network-outbound (require-all (remote unix-socket) {here}))\n"
        ));
        p.push_str(NO_WAY_OUT);
        p
    }

    /// The profile's parameters (name, value).
    pub fn params(&self) -> Vec<(String, String)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut v = vec![("ROOT".to_string(), s(&self.root))];
        v.extend(self.run.iter().map(|r| ("RUN".to_string(), s(r.path()))));
        v.extend(self.protect.iter().enumerate().map(|(i, p)| (format!("P{i}"), s(p))));
        v.extend(self.write.iter().enumerate().map(|(i, p)| (format!("W{i}"), s(p))));
        v.extend(self.files.iter().enumerate().map(|(i, p)| (format!("F{i}"), s(p))));
        v
    }

    /// `sandbox-exec`'s arguments before the command.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["-p".to_string(), self.profile()];
        for (k, v) in self.params() {
            a.push("-D".into());
            a.push(format!("{k}={v}"));
        }
        a
    }

    /// The policy for a project, from what's around it (`Env`, gathered on a worker — or made up in tests).
    pub fn for_project(env: &Env) -> Policy {
        let root = env.root.clone();
        let mut protect = vec![root.join(".git"), root.join(".tarae.toml")];
        for p in &env.protect {
            let rel = Path::new(p);
            // Inside the project only (`..` would reach out of it)
            if rel.is_relative() && rel.components().all(|c| matches!(c, Component::Normal(_))) {
                protect.push(root.join(rel));
            }
        }
        let mut write = vec![PathBuf::from("/private/tmp")];
        if let Some(t) = &env.tmp {
            write.push(user_temp(t));
        }
        if let Some(ch) = &env.cargo_home {
            write.extend([ch.join("registry"), ch.join("git")]);
        }
        write.extend(env.dirs.iter().cloned());
        for w in &env.write {
            let p = match (w.strip_prefix("~/"), &env.home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => root.join(w),
            };
            write.push(real(&p));
        }
        // Never the project or what holds it (that would open .git again); inside it, the project's rule
        write.retain(|w| !root.starts_with(w) && !w.starts_with(&root));
        write.dedup();
        let files = match &env.cargo_home {
            Some(ch) => [".package-cache", ".package-cache-mutate", ".global-cache"]
                .iter()
                .flat_map(|f| {
                    let db = *f == ".global-cache";
                    let extra: &[&str] = if db { &["", "-journal", "-wal", "-shm"] } else { &[""] };
                    extra.iter().map(move |e| ch.join(format!("{f}{e}"))).collect::<Vec<_>>()
                })
                .collect(),
            None => Vec::new(),
        };
        Policy { root, protect, write, files, network: env.network, run: None }
    }
}

/// What a project's policy is made from.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub root: PathBuf,
    pub home: Option<PathBuf>,
    /// `$TMPDIR`, real.
    pub tmp: Option<PathBuf>,
    /// `$CARGO_HOME` (or `~/.cargo`), real — when it exists.
    pub cargo_home: Option<PathBuf>,
    /// The toolchain's own folders (`toolchain_dirs`).
    pub dirs: Vec<PathBuf>,
    /// `llm.sandbox-write` · `llm.sandbox-protect` · `llm.sandbox-network`.
    pub write: Vec<String>,
    pub protect: Vec<String>,
    pub network: bool,
}

/// The real path; for one that isn't there yet, its folder's real path joined with its name.
pub fn real(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) if dir != p => real(dir).join(name),
        _ => p.to_path_buf(),
    }
}

/// The user's own temp space: `$TMPDIR` is `/private/var/folders/xx/yyy/T` — its parent holds `C` (caches
/// some tools use) too.
fn user_temp(tmp: &Path) -> PathBuf {
    match tmp.parent() {
        Some(parent) if tmp.starts_with("/private/var/folders") && tmp.file_name() == Some("T".as_ref()) => {
            parent.to_path_buf()
        }
        _ => tmp.to_path_buf(),
    }
}

// ── On a worker ─────────────────────────────────────────────────────────────

/// Whether commands can be sandboxed here — looked once per run of tarae (blocks: a worker's).
pub fn probe() -> Availability {
    static FOUND: OnceLock<Availability> = OnceLock::new();
    FOUND.get_or_init(probe_now).clone()
}

fn probe_now() -> Availability {
    if !cfg!(target_os = "macos") {
        return Availability::Missing("this system has no sandbox tarae can use yet".into());
    }
    let out = Command::new(SANDBOX_EXEC)
        .args([
            "-p",
            "(version 1)(allow default)(deny file-write* (literal \"/tarae-probe\"))",
            "/usr/bin/true",
        ])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Availability::Ready,
        Ok(o) => Availability::Missing(missing_why(&String::from_utf8_lossy(&o.stderr))),
        Err(e) => Availability::Missing(format!("{SANDBOX_EXEC}: {e}")),
    }
}

/// Why `sandbox-exec` couldn't start one, from what it said.
pub fn missing_why(stderr: &str) -> String {
    if stderr.contains("sandbox_apply") {
        "tarae itself runs in a sandbox, and another can't start inside it".into()
    } else {
        format!("sandbox-exec: {}", stderr.trim())
    }
}

/// The policy for the project at `root` with these settings — the toolchain's folders found (blocks).
pub fn policy(root: &Path, write: &[String], protect: &[String], network: bool) -> Policy {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")))
        .and_then(|p| std::fs::canonicalize(p).ok());
    let tmp = std::fs::canonicalize(std::env::temp_dir()).ok();
    let mut env = Env {
        root: real(root),
        home,
        tmp,
        cargo_home,
        dirs: Vec::new(),
        write: write.to_vec(),
        protect: protect.to_vec(),
        network,
    };
    // The toolchain is asked inside the sandbox it's for (it reads only)
    env.dirs = toolchain_dirs(&Policy::for_project(&env));
    // (no folder of its own: no sockets but the project's, and `TMPDIR` as it was)
    Policy { run: RunDir::new().ok(), ..Policy::for_project(&env) }
}

/// Folders the project's toolchain writes outside it: cargo's target and build dirs (`cargo metadata` —
/// they can be anywhere: `CARGO_TARGET_DIR`, `build.build-dir` in a cargo config), Go's caches. Kept per
/// project until a file that sets them changes.
fn toolchain_dirs(base: &Policy) -> Vec<PathBuf> {
    type Seen = HashMap<PathBuf, (Vec<Option<SystemTime>>, Vec<PathBuf>)>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let root = &base.root;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let stamp_of = [
        root.join("Cargo.toml"),
        root.join(".cargo/config.toml"),
        home.join(".cargo/config.toml"),
        root.join("go.mod"),
    ];
    let stamp: Vec<Option<SystemTime>> =
        stamp_of.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect();
    let seen = SEEN.get_or_init(Default::default);
    if let Some((s, dirs)) = seen.lock().ok().and_then(|m| m.get(root).cloned())
        && s == stamp
    {
        return dirs;
    }
    let mut dirs = Vec::new();
    if root.join("Cargo.toml").is_file()
        && let Some(json) =
            output_in(base, "cargo", &["metadata", "--no-deps", "--offline", "--format-version", "1"])
    {
        for key in ["target_directory", "build_directory"] {
            if let Some(d) = json[key].as_str() {
                dirs.push(real(Path::new(d)));
            }
        }
    }
    if root.join("go.mod").is_file()
        && let Some(json) = output_in(base, "go", &["env", "-json", "GOCACHE", "GOMODCACHE"])
    {
        for key in ["GOCACHE", "GOMODCACHE"] {
            if let Some(d) = json[key].as_str().filter(|d| !d.is_empty()) {
                dirs.push(real(Path::new(d)));
            }
        }
    }
    if let Ok(mut m) = seen.lock() {
        m.insert(root.clone(), (stamp, dirs.clone()));
    }
    dirs
}

/// A program's JSON output, run in the project inside `policy`.
fn output_in(policy: &Policy, program: &str, args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new(SANDBOX_EXEC)
        .args(policy.args())
        .arg(program)
        .args(args)
        .current_dir(&policy.root)
        .env(MARK, "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    serde_json::from_slice(&out.stdout).ok()
}

/// `program args` inside `policy` — or as it is, without one.
pub fn command(policy: Option<&Policy>, program: &str, args: &[String]) -> Command {
    match policy {
        Some(p) => {
            let mut c = Command::new(SANDBOX_EXEC);
            c.args(p.args()).arg(program).args(args).env(MARK, "1");
            if let Some(run) = &p.run {
                c.env("TMPDIR", run.path());
            }
            c
        }
        None => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(root: &Path) -> Env {
        Env {
            root: root.to_path_buf(),
            home: Some("/Users/u".into()),
            tmp: Some("/private/var/folders/ab/cd/T".into()),
            cargo_home: Some("/Users/u/.cargo".into()),
            dirs: vec!["/Users/u/.cargo/build/shared".into(), root.join("target")],
            write: vec!["~/Library/Caches/thing".into(), "/".into(), "/Users".into()],
            protect: vec!["secrets".into(), "../outside".into(), "/etc".into()],
            network: false,
        }
    }

    /// The project but its own guarded files, temp, cargo's caches and dirs — not cargo's `bin` (on PATH:
    /// what lands there runs later outside), not the project's ancestors (they'd open `.git` again).
    #[test]
    fn a_projects_policy() {
        let root = Path::new("/Users/u/p");
        let p = Policy::for_project(&env(root));
        assert_eq!(
            p.protect,
            [root.join(".git"), root.join(".tarae.toml"), root.join("secrets")],
            "only paths inside the project"
        );
        let w: Vec<&str> = p.write.iter().map(|w| w.to_str().unwrap()).collect();
        assert_eq!(
            w,
            [
                "/private/tmp",
                "/private/var/folders/ab/cd",
                "/Users/u/.cargo/registry",
                "/Users/u/.cargo/git",
                "/Users/u/.cargo/build/shared",
                "/Users/u/Library/Caches/thing",
            ]
        );
        assert!(p.files.contains(&PathBuf::from("/Users/u/.cargo/.package-cache")));
        assert!(p.files.contains(&PathBuf::from("/Users/u/.cargo/.global-cache-wal")));
        assert!(!p.write.iter().chain(&p.files).any(|w| w.starts_with("/Users/u/.cargo/bin")));
    }

    /// Paths are parameters, never text in the profile — a path can't change what it says.
    #[test]
    fn the_profile_names_paths_only_through_parameters() {
        let root = Path::new("/Users/u/a\"b (c)");
        let p = Policy::for_project(&env(root));
        let text = p.profile();
        assert!(!text.contains("/Users/u"), "{text}");
        assert!(text.contains("(require-not (subpath (param \"P0\")))"));
        assert!(text.contains("(subpath (param \"W5\"))") && !text.contains("W6"));
        assert!(text.contains("(deny network*)") && text.contains("(deny appleevent-send)"));
        let params = p.params();
        assert_eq!(params[0], ("ROOT".into(), "/Users/u/a\"b (c)".into()));
        assert!(params.iter().any(|(k, v)| k == "P0" && v.ends_with("/.git")));
        let open = Policy { network: true, ..p };
        let text = open.profile();
        assert!(!text.contains("(deny network*)"), "{text}");
        assert!(
            text.contains("(deny network-outbound (remote unix-socket))"),
            "sockets stay guarded: {text}"
        );
        assert!(!text.contains("RUN"), "no folder of its own: only the project's sockets");
    }

    /// What `sandbox-exec` says inside another sandbox (measured) reads as that.
    #[test]
    fn a_sandbox_inside_one_says_so() {
        assert!(
            missing_why("sandbox-exec: sandbox_apply: Operation not permitted\n")
                .contains("runs in a sandbox")
        );
    }

    /// For real, on macOS — with harmless markers in a temp folder: writes land in the project and nowhere
    /// else, `.git` and `.tarae.toml` stay as they are, a symlink out doesn't help, localhost answers and
    /// the rest of the network doesn't, and a pseudo-terminal opens. Inside a sandbox already (Claude's
    /// own `cargo test`) none can start: the probe says so, and that's what's checked.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_keeps_writes_in_the_project() {
        if std::env::var_os(MARK).is_some() {
            assert!(matches!(probe_now(), Availability::Missing(w) if w.contains("runs in a sandbox")));
            eprintln!("(in a sandbox already: the Seatbelt checks run outside one — cargo test, CI)");
            return;
        }
        assert_eq!(probe_now(), Availability::Ready);
        let t = std::env::temp_dir().join(format!("tarae-sb-{}", std::process::id()));
        let (root, out) = (t.join("project"), t.join("outside"));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let (root, out) = (real(&root), real(&out));
        std::os::unix::fs::symlink(&out, root.join("link")).ok();
        // Not the usual temp folders: here, the outside folder is in one
        let p = Policy {
            root: root.clone(),
            protect: vec![root.join(".git"), root.join(".tarae.toml")],
            write: Vec::new(),
            files: Vec::new(),
            network: false,
            run: None,
        };
        let sh = |script: &str| {
            let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
            c.current_dir(&root).stdin(Stdio::null()).output().unwrap()
        };
        assert!(sh("touch inside").status.success());
        assert!(root.join("inside").exists());
        for (target, what) in [
            (out.join("marker"), "outside"),
            (root.join(".git/marker"), ".git"),
            (root.join(".tarae.toml"), ".tarae.toml"),
            (root.join("link/through"), "a symlink out"),
        ] {
            let r = sh(&format!("touch '{}'", target.display()));
            assert!(!r.status.success() && !target.exists(), "{what}: written");
            assert!(String::from_utf8_lossy(&r.stderr).contains("Operation not permitted"), "{what}");
        }
        // This machine answers; TEST-NET-1 (never routed) is refused at once, not timed out
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(sh(&format!("/usr/bin/nc -z -w 2 127.0.0.1 {port}")).status.success());
        let started = std::time::Instant::now();
        assert!(!sh("/usr/bin/nc -z -w 5 192.0.2.1 9").status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "refused, not timed out");
        // A pseudo-terminal opens (without /dev/ptmx a pty test hangs instead of failing: measured)
        assert!(sh("/usr/bin/script -q /dev/null /usr/bin/true").status.success());
        std::fs::remove_dir_all(&t).ok();
    }

    /// For real, on macOS: a server outside the run listening on a unix socket — in `/private/tmp` or
    /// `$TMPDIR`, where tmux's and zellij's live — can't be reached from inside, the network closed or
    /// open (a tmux server there ran a command for a sandboxed one: reproduced). The run's own folder is
    /// its `TMPDIR`, and a socket made there is reached; the project's are too. Harmless: the listeners
    /// are this test's own, and nothing is sent but "hi".
    #[cfg(target_os = "macos")]
    #[test]
    fn unix_sockets_only_in_the_run_and_the_project() {
        use std::os::unix::net::UnixListener;
        if std::env::var_os(MARK).is_some() {
            eprintln!("(in a sandbox already: the Seatbelt checks run outside one — cargo test, CI)");
            return;
        }
        let pid = std::process::id();
        let t = real(&std::env::temp_dir()).join(format!("tarae-sock-{pid}"));
        let root = t.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let in_tmp = PathBuf::from(format!("/private/tmp/tarae-sock-{pid}"));
        std::fs::create_dir_all(&in_tmp).unwrap();
        let outside = [in_tmp.join("s"), t.join("s")];
        for network in [false, true] {
            let run = RunDir::new().unwrap();
            let p = Policy {
                root: root.clone(),
                protect: vec![root.join(".git")],
                write: vec!["/private/tmp".into(), user_temp(&real(&std::env::temp_dir()))],
                files: Vec::new(),
                network,
                run: Some(run.clone()),
            };
            let sh = |script: &str| {
                let mut c = command(Some(&p), "/bin/sh", &["-c".into(), script.into()]);
                let out = c.current_dir(&root).stdin(Stdio::null()).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            let reached = |path: &Path| {
                std::fs::remove_file(path).ok();
                let l = UnixListener::bind(path).unwrap();
                l.set_nonblocking(true).unwrap();
                sh(&format!("echo hi | /usr/bin/nc -w 2 -U '{}'", path.display()));
                l.accept().is_ok()
            };
            for path in &outside {
                assert!(!reached(path), "{}: reached from inside (network open: {network})", path.display());
            }
            assert!(reached(&root.join("s")), "the project's socket (network open: {network})");
            // Its own folder: its TMPDIR, where a socket is made and reached from inside
            assert_eq!(sh("echo \"$TMPDIR\""), run.path().display().to_string());
            let got = sh("/usr/bin/nc -w 2 -lU \"$TMPDIR/s\" > \"$TMPDIR/got\" & sleep 0.5; \
                 echo hi | /usr/bin/nc -w 2 -U \"$TMPDIR/s\"; wait; cat \"$TMPDIR/got\"");
            assert_eq!(got, "hi", "a socket in its own folder (network open: {network})");
        }
        std::fs::remove_dir_all(&t).ok();
        std::fs::remove_dir_all(&in_tmp).ok();
    }
}
