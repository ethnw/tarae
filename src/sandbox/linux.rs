//! The sandbox on Linux: the `Policy` macOS's profile is made from, enforced by Landlock (files —
//! `landlock.rs`) and seccomp (sockets — `seccomp.rs`) on the short-lived thread that starts the run
//! (`spawn`). What differs is only how.
//!
//! Landlock grants; Seatbelt denies after it allows. So what the profile keeps inside a place it allows is a
//! hole here, and the place is granted entry by entry around it (`grant`) — its own entries then can't be
//! made, removed or renamed (the user's decision): reads everywhere but home (home a hole in `/`), in home
//! the project and its places (`Reads`) around what's kept and the token files the tools read copies of;
//! writes in the project around every protected name that's there (`protected_there`) and
//! `llm.sandbox-protect`, in the run's folder, the temp and toolchain folders, the single files cargo and
//! Gradle's wrapper write (those there now), and the devices programs use (`DEVICES`, measured).
//!
//! What Landlock can't keep, said in the report (`linux_notes`): a protected name a run makes anew where
//! nothing is protected yet (a path that isn't there has no rule); the terminal multiplexers' folders in
//! `/tmp` (splitting `/tmp` would refuse every new file there, and Java writes its temp files in `/tmp`
//! whatever `TMPDIR` says); a single file not there yet (cargo's SQLite journal). The network: what a run
//! reaches by address tarae decides, as macOS's profile does (`supervisor.rs`) — where it can't here,
//! seccomp refuses every socket but netlink, this machine's ports and the project's unix sockets too.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Availability, Denial, Policy};
use crate::landlock::{self, Ruleset};

/// Full support from this Landlock ABI (≈ Linux 6.2): it controls truncating (3) and moving across folders
/// (2). Below it the sandbox goes as far as it can and every run asks (the user's decision, 2026-10-05).
pub const FULL: u32 = 3;

/// Reading, listing, running.
const READ: u64 = landlock::EXECUTE | landlock::READ_FILE | landlock::READ_DIR;
/// Writing one file (not making or removing it).
const WRITE_ONE: u64 = landlock::READ_FILE | landlock::WRITE_FILE | landlock::TRUNCATE | landlock::IOCTL_DEV;

/// Devices programs use, and how (measured, `target/sb/measure-devices.sh`: cargo, go, npm, Maven, Gradle, a
/// pty, Python's multiprocessing): `/dev/null` read, written, created-and-truncated and asked `isatty`;
/// terminals for a pty test (`/dev/ptmx`, then its `/dev/pts/N`, with their ioctls); POSIX semaphores and
/// shared memory as files in `/dev/shm` (made and removed — macOS's profile allows them too). Reading the
/// rest of `/dev` (`/dev/urandom`) is the reads' (outside home).
const DEVICES: [(&str, u64); 7] = [
    ("/dev/null", WRITE_ONE),
    ("/dev/zero", WRITE_ONE),
    ("/dev/full", WRITE_ONE),
    ("/dev/tty", WRITE_ONE),
    ("/dev/ptmx", WRITE_ONE),
    ("/dev/pts", landlock::ALL),
    ("/dev/shm", landlock::ALL),
];

/// Whether this system can sandbox Claude's runs, from the kernel's Landlock ABI (`landlock::abi`) and
/// whether seccomp has a filter for this architecture: full from `FULL`, partial (every run asks, saying
/// why) at 1–2, none without Landlock.
pub fn availability(abi: io::Result<u32>, seccomp: bool) -> Availability {
    if !seccomp {
        return Availability::Missing("tarae has no seccomp filter for this architecture".into());
    }
    match abi {
        Ok(n) if n >= FULL => Availability::Ready,
        Ok(1) => Availability::Partial(
            "this kernel's Landlock (ABI 1) can't control truncating files, and refuses every move across \
             folders"
                .into(),
        ),
        Ok(n) => Availability::Partial(format!(
            "this kernel's Landlock (ABI {n}) can't control truncating files — a command could empty any \
             file of yours"
        )),
        Err(e) => Availability::Missing(match e.raw_os_error() {
            Some(libc::ENOSYS) => "this kernel has no Landlock (it came with Linux 5.13)".into(),
            Some(libc::EOPNOTSUPP) => "Landlock is turned off on this system (its boot's lsm= list)".into(),
            _ => format!("Landlock: {e}"),
        }),
    }
}

/// A policy made into Landlock's rules for this kernel's ABI — applied, with the seccomp filter, by the
/// thread that starts each run (`spawn`). `audit`: the denials come from Landlock's audit log (ABI 7+, the
/// kernel's messages readable here — `audit_readable`); else from the run's own error lines. `serve`: what
/// a run reaches by address is tarae's to decide (`supervisor.rs`) — or why it can't be here, and every
/// socket but netlink is refused.
#[derive(Clone)]
pub struct Confine {
    pub abi: u32,
    pub audit: bool,
    pub serve: Result<(), String>,
    rules: Arc<Ruleset>,
}

impl std::fmt::Debug for Confine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Confine(ABI {}, audit {}, serve {:?})", self.abi, self.audit, self.serve)
    }
}

impl PartialEq for Confine {
    fn eq(&self, other: &Self) -> bool {
        self.abi == other.abi
            && self.audit == other.audit
            && self.serve == other.serve
            && Arc::ptr_eq(&self.rules, &other.rules)
    }
}

impl Eq for Confine {}

/// `access` beneath `path`, around `holes`: none at or under it → one rule; `path` itself one (or in one) →
/// nothing; one deeper → each of its entries on its own, and nothing for the folder itself (its own entries
/// can't be made, removed or renamed). A path not there is skipped; a symlink gets no rule (`allow`).
fn grant(r: &mut Ruleset, path: &Path, access: u64, holes: &[PathBuf]) -> io::Result<()> {
    if holes.iter().any(|h| path.starts_with(h)) {
        return Ok(());
    }
    if !holes.iter().any(|h| h.starts_with(path)) {
        return match r.allow(path, access) {
            Ok(_) => Ok(()),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => Ok(()),
            Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        };
    }
    let Ok(entries) = std::fs::read_dir(path) else { return Ok(()) };
    for e in entries.flatten() {
        grant(r, &e.path(), access, holes)?;
    }
    Ok(())
}

/// The protected names (`protected_in`) that are there in the project at `root`, each the path where one
/// starts — not looked for below one, nor in tools' output (`OUTPUTS`, where those names are let through) —
/// and `extra` (`llm.sandbox-protect`) that are there. Links aren't followed.
pub fn protected_there(root: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if super::protected_in(root, &path) {
                out.push(path);
                continue;
            }
            let folder = e.file_type().is_ok_and(|t| t.is_dir());
            if folder && !super::OUTPUTS.iter().any(|o| e.file_name() == *o) {
                walk(root, &path, out);
            }
        }
    }
    let mut out: Vec<PathBuf> = extra.iter().filter(|p| p.symlink_metadata().is_ok()).cloned().collect();
    walk(root, root, &mut out);
    out
}

/// `p` as Landlock's rules for ABI `abi`. Reads everywhere but home, and in it where `p.reads` says around what
/// it keeps (none of that when reads aren't confined); writes as `writes_allow` has them; the devices.
pub fn confine(p: &Policy, abi: u32, audit: bool) -> io::Result<Confine> {
    let mut r = Ruleset::new(abi)?;
    let hidden: Vec<PathBuf> = p.reads.copies.hidden.iter().map(|(f, _)| f.clone()).collect();
    match &p.reads.home {
        Some(home) => {
            grant(&mut r, Path::new("/"), READ, std::slice::from_ref(home))?;
            // In home: the project, where it writes, what's allowed — none inside a kept place (but the
            // project, when one holds it), and around what's kept or read from a copy below them
            let kept = &p.reads.kept;
            let run = p.run.as_ref().map(|d| d.path().to_path_buf());
            let places = std::iter::once(&p.root)
                .chain(run.as_ref())
                .chain(&p.write)
                .chain(&p.files)
                .chain(&p.reads.allow);
            for place in places.filter(|w| w.starts_with(home)) {
                let in_kept = kept.iter().any(|k| place.starts_with(k));
                if in_kept && !place.starts_with(&p.root) {
                    continue;
                }
                let holes: Vec<PathBuf> =
                    kept.iter().chain(&hidden).filter(|h| h.starts_with(place)).cloned().collect();
                grant(&mut r, place, READ, &holes)?;
            }
            if let Some(log) = p.reads.log.as_ref().filter(|_| !kept.is_empty()) {
                grant(&mut r, log, landlock::READ_FILE, &[])?;
            }
        }
        None => grant(&mut r, Path::new("/"), READ, &hidden)?,
    }
    let abs: Vec<PathBuf> = p.protect.clone();
    grant(&mut r, &p.root, landlock::ALL, &protected_there(&p.root, &abs))?;
    if let Some(run) = &p.run {
        grant(&mut r, run.path(), landlock::ALL, &[])?;
    }
    for w in &p.write {
        let holes: Vec<PathBuf> = p.caches.iter().filter(|c| c.starts_with(w)).cloned().collect();
        grant(&mut r, w, landlock::ALL, &holes)?;
    }
    for f in &p.files {
        grant(&mut r, f, WRITE_ONE, &[])?;
    }
    for (d, access) in DEVICES {
        grant(&mut r, Path::new(d), access, &[])?;
    }
    let serve = match super::supervisor::unavailable() {
        None => Ok(()),
        Some(why) => Err(why),
    };
    Ok(Confine { abi, audit, serve, rules: Arc::new(r) })
}

/// `start` (a spawn) on a short-lived thread confined to `c` and under the seccomp filter — what it starts is
/// in the sandbox, the caller isn't (`landlock::confined`). `serve`: the run's policy, when tarae decides
/// what it reaches by address (`supervisor::serve`, started from here: an unconfined thread) — else the
/// plain filter. `owner` hears the thread's id before (a watch reading Landlock's audit log tells its run's
/// domain by it).
pub fn confined<T: Send>(
    c: &Confine,
    serve: Option<Arc<Policy>>,
    owner: impl FnOnce(u32) + Send,
    start: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    use crate::seccomp::Mode;
    let to = match serve.filter(|_| c.serve.is_ok()) {
        Some(p) => Some(super::supervisor::serve(p)?),
        None => None,
    };
    let mode = if to.is_some() { Mode::Served } else { Mode::Plain };
    landlock::confined(
        &c.rules,
        || crate::seccomp::apply(mode),
        |listener| {
            if let (Some(to), Some(l)) = (to, listener) {
                to.send(l).map_err(|_| io::Error::other("tarae's supervisor ended before its run"))?;
            }
            // SAFETY: gettid has no preconditions.
            owner(unsafe { libc::gettid() } as u32);
            start()
        },
    )
}

/// What a run of ABI `abi` can't be kept from, for the report: no signal scope below 6.
pub fn notes(abi: u32) -> String {
    match abi {
        n if n < 6 => format!(
            " This kernel's Landlock (ABI {n}) can't keep a command's signals to its own processes (6 does)."
        ),
        _ => String::new(),
    }
}

/// Whether Landlock's audit records can be read here: ABI 7+, and `journalctl` shows the kernel's messages
/// to this user (one line of them; none means only the user's own journal is readable). Blocks (a worker's),
/// looked once.
pub fn audit_readable(abi: u32) -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    abi >= 7
        && *FOUND.get_or_init(|| {
            std::process::Command::new("journalctl")
                .args(["-k", "-n", "1", "-q", "-o", "cat", "--no-pager"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
        })
}

/// The command a watch reads Landlock's audit records from: the kernel's messages and the audit ones, as
/// they come.
pub fn audit_stream() -> std::process::Command {
    let mut c = std::process::Command::new("journalctl");
    c.args(["-f", "-n", "0", "-q", "-o", "cat", "--no-pager", "_TRANSPORT=kernel", "_TRANSPORT=audit"]);
    c
}

/// A value in an audit record's `key=value` words: quoted, or hex (how audit writes a string with spaces).
fn audit_value(line: &str, key: &str) -> Option<String> {
    let at = line.split(' ').position(|w| w.starts_with(&format!("{key}=")))?;
    let rest: String = line.split(' ').skip(at).collect::<Vec<_>>().join(" ");
    let v = &rest[key.len() + 1..];
    if let Some(q) = v.strip_prefix('"') {
        return q.split_once('"').map(|(s, _)| s.to_string());
    }
    let word = v.split(' ').next().unwrap_or_default();
    let hex = word.len().is_multiple_of(2) && !word.is_empty() && word.chars().all(|c| c.is_ascii_hexdigit());
    if hex && key == "path" {
        let bytes: Option<Vec<u8>> =
            (0..word.len()).step_by(2).map(|i| u8::from_str_radix(&word[i..i + 2], 16).ok()).collect();
        return bytes.map(|b| String::from_utf8_lossy(&b).into_owned());
    }
    Some(word.to_string())
}

/// What one run's Landlock audit records say, read line by line: its domain is the one whose record names
/// the thread that confined it (`owner`); access records of a domain not told yet wait for it.
#[derive(Debug, Default)]
pub struct Audit {
    pub owner: Option<u32>,
    ours: Vec<String>,
    others: Vec<String>,
    waiting: Vec<(String, Denial)>,
}

/// Access records kept waiting for their domain's record, at most.
const AUDIT_WAITING: usize = 64;

impl Audit {
    /// The denials `line` adds: an access record of the run's domain (`LANDLOCK_ACCESS`: `domain=… blockers=…
    /// path=…`), or those waiting for a domain record that turned out the run's (`LANDLOCK_DOMAIN`:
    /// `domain=… status=allocated … pid=…`). Said in the profile's words (`file-write*`, `file-read-data`).
    pub fn line(&mut self, line: &str) -> Vec<Denial> {
        let Some(domain) = audit_value(line, "domain") else { return Vec::new() };
        if let Some(blockers) = audit_value(line, "blockers") {
            let target = audit_value(line, "path").unwrap_or_default();
            let d = Denial { process: "the run".into(), op: audit_op(&blockers).into(), target };
            if self.ours.contains(&domain) {
                return vec![d];
            }
            if !self.others.contains(&domain) && self.waiting.len() < AUDIT_WAITING {
                self.waiting.push((domain, d));
            }
            return Vec::new();
        }
        let pid = audit_value(line, "pid").and_then(|p| p.parse::<u32>().ok());
        if audit_value(line, "status").is_none() || pid.is_none() {
            return Vec::new();
        }
        let mine = self.owner.is_some() && pid == self.owner;
        let (keep, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.waiting).into_iter().partition(|(d, _)| *d == domain);
        self.waiting = rest;
        match mine {
            true => {
                self.ours.push(domain);
                keep.into_iter().map(|(_, d)| d).collect()
            }
            false => {
                self.others.push(domain);
                Vec::new()
            }
        }
    }
}

/// An audit record's blockers (`fs.write_file,fs.make_reg`, `scope.signal`) in the profile's words.
fn audit_op(blockers: &str) -> &'static str {
    let any = |names: &[&str]| blockers.split(',').any(|b| names.contains(&b));
    if any(&["fs.read_file", "fs.read_dir", "fs.execute"]) && !blockers.contains("fs.write") {
        return "file-read-data";
    }
    if blockers.starts_with("fs.") {
        return "file-write*";
    }
    if any(&["scope.signal"]) {
        return "signal";
    }
    "network-outbound"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Env, Guard, Reads, RunDir};

    /// The kernel's ABI, or None after saying the test skips itself (no Landlock here).
    fn kernel_abi(test: &str) -> Option<u32> {
        match landlock::abi() {
            Ok(n) => Some(n),
            Err(e) => {
                let line = format!(
                    "{}sandbox::linux::tests::{test} — this kernel has no Landlock ({e})\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                None
            }
        }
    }

    /// The floor: full from ABI 3, partial (saying why) at 1–2, none without Landlock or a seccomp filter.
    #[test]
    fn availability_follows_the_kernels_abi() {
        let err = |n| Err(io::Error::from_raw_os_error(n));
        assert_eq!(availability(Ok(3), true), Availability::Ready);
        assert_eq!(availability(Ok(8), true), Availability::Ready);
        let partial = |a| match availability(Ok(a), true) {
            Availability::Partial(why) => why,
            other => panic!("ABI {a}: {other:?}"),
        };
        assert!(partial(2).contains("can't control truncating files"), "{}", partial(2));
        assert!(partial(1).contains("refuses every move across folders"), "{}", partial(1));
        let missing = |a: io::Result<u32>, seccomp| match availability(a, seccomp) {
            Availability::Missing(why) => why,
            other => panic!("{other:?}"),
        };
        assert!(missing(err(libc::ENOSYS), true).contains("no Landlock (it came with Linux 5.13)"));
        assert!(missing(err(libc::EOPNOTSUPP), true).contains("turned off"));
        assert!(missing(Ok(8), false).contains("seccomp"));
        // (this kernel, asked as the probe asks)
        if let Ok(n) = landlock::abi() {
            assert_eq!(crate::sandbox::probe_now() == Availability::Ready, n >= FULL);
        }
    }

    /// The protected names that are there, each where it starts — not below one, nor in tools' output — and
    /// `llm.sandbox-protect`'s; a link isn't followed.
    #[test]
    fn protected_names_are_found_where_they_are() {
        let dir = std::env::temp_dir().join(format!("tarae-protected-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in [
            "Cargo.toml",
            ".git/HEAD",
            "src/main.rs",
            "sub/build.rs",
            "app/gradle/wrapper/gradle-wrapper.properties",
            "target/package/x/Cargo.toml",
            "node_modules/p/package.json",
            "kept/x",
        ] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        let outside = dir.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Makefile"), "").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let mut found = protected_there(&dir, &[dir.join("kept"), dir.join("not-there")]);
        found.sort();
        let want: Vec<PathBuf> = [".git", "Cargo.toml", "app/gradle/wrapper", "kept", "sub/build.rs"]
            .iter()
            .map(|f| dir.join(f))
            .collect();
        assert_eq!(found, want);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// A scratch project and home for a real policy, under the build's `target/` (not in `/tmp`: the
    /// policy writes there, and leaves out what holds the project).
    struct Scratch {
        base: PathBuf,
        root: PathBuf,
        home: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let base = std::env::current_dir()
                .unwrap()
                .join(format!("target/tarae-confine-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            for f in [
                "proj/Cargo.toml",
                "proj/.git/HEAD",
                "proj/src/main.rs",
                "proj/sub/build.rs",
                "proj/sub/lib.rs",
                "proj/kept/x",
                "home/.cargo/config.toml",
                "home/.cargo/credentials.toml",
                "home/.ssh/id",
                "home/notes",
            ] {
                std::fs::create_dir_all(base.join(f).parent().unwrap()).unwrap();
                std::fs::write(base.join(f), "x").unwrap();
            }
            let base = std::fs::canonicalize(&base).unwrap();
            Scratch { root: base.join("proj"), home: base.join("home"), base }
        }

        /// The policy a run of Claude's gets here: `kept` protected (`llm.sandbox-protect`), reads in home
        /// only `.cargo` but its credentials and `.ssh`, a folder of its own — made into the rules.
        fn policy(&self) -> Policy {
            let h = &self.home;
            let env = Env {
                root: self.root.clone(),
                home: Some(h.clone()),
                tmp: Some(crate::sandbox::profile_path(&std::env::temp_dir())),
                protect: vec!["kept".into()],
                reads: Reads {
                    home: Some(h.clone()),
                    allow: vec![h.join(".cargo")],
                    kept: vec![h.join(".ssh"), h.join(".cargo/credentials.toml")],
                    ..Default::default()
                },
                ..Default::default()
            };
            let p = Policy { run: RunDir::new().ok(), ..Policy::for_project(&env) };
            crate::sandbox::confined(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// For real: a run started through `sandbox::spawn` keeps to its policy — writes in the project but its
    /// protected names (where they are, and every entry of a folder holding one), `llm.sandbox-protect`, git's
    /// folder; in its own folder, `/tmp`, `/dev/null`; nowhere else. Reads in home only what's allowed, never
    /// what's kept inside it; outside home anything. No port of this machine's that wasn't opened. The thread
    /// that started it writes anywhere.
    #[test]
    fn a_run_keeps_to_its_policy() {
        let Some(_) = kernel_abi("a_run_keeps_to_its_policy") else { return };
        let s = Scratch::new("run");
        let p = s.policy();
        assert!(matches!(p.confine, Some(Ok(_))), "{:?}", p.confine);
        let run = p.run.as_ref().unwrap().path().to_path_buf();
        let script = r#"
            w() { if ( : > "$2" ) 2>/dev/null; then echo "$1 ok"; else echo "$1 no"; fi; }
            r() { if cat "$2" >/dev/null 2>&1; then echo "$1 ok"; else echo "$1 no"; fi; }
            w project "$R/src/new.rs"; w protected "$R/Cargo.toml"; w protected-deep "$R/sub/build.rs"
            w new-at-root "$R/new.txt"; w beside-a-protected-name "$R/sub/new.rs"; w git "$R/.git/HEAD"
            w llm-protect "$R/kept/x"; w run "$RUN/x"; w tmp "/tmp/tarae-confine-$$"; w devnull /dev/null
            w outside "$B/outside"
            r home-allowed "$H/.cargo/config.toml"; r home-kept "$H/.cargo/credentials.toml"
            r home-ssh "$H/.ssh/id"; r home-other "$H/notes"; r outside-home /etc/hostname
            if mv "$R/src/main.rs" "$R/src/moved.rs" 2>/dev/null; then echo move ok; else echo move no; fi
            if rm "$R/sub/build.rs" 2>/dev/null; then echo remove-protected ok; else echo remove-protected no; fi
            if bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT" 2>/dev/null; then echo socket ok; else echo socket no; fi
            rm -f "/tmp/tarae-confine-$$"
        "#;
        // (a port that answers — the test's own listener: unconfined, the run would connect)
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let mut cmd = crate::sandbox::command(Some(&p), "sh", &["-c".into(), script.into()]);
        cmd.env("R", &s.root).env("RUN", &run).env("H", &s.home).env("B", &s.base).env("PORT", port);
        let spawned = crate::sandbox::spawn(Some(&p), &cmd).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(spawned.pid).unwrap();
        let got: Vec<&str> = out.lines().collect();
        let want = [
            "project ok",
            "protected no",
            "protected-deep no",
            "new-at-root no",
            "beside-a-protected-name no",
            "git no",
            "llm-protect no",
            "run ok",
            "tmp ok",
            "devnull ok",
            "outside no",
            "home-allowed ok",
            "home-kept no",
            "home-ssh no",
            "home-other no",
            "outside-home ok",
            "move ok",
            "remove-protected no",
            "socket no",
        ];
        assert_eq!(got, want);
        std::fs::write(s.base.join("outside"), "x").expect("the thread that started it writes anywhere");
    }

    /// What Claude reads of a Linux run: the network as macOS says it where tarae serves it (nothing beyond
    /// this machine, `llm.sandbox-network` or not), else no socket at all and why; the protected names where
    /// they are, what Landlock can't keep, and where what it refused is read from — this kernel's ABI said.
    #[test]
    fn the_report_says_how_linux_differs() {
        let Some(abi) = kernel_abi("the_report_says_how_linux_differs") else { return };
        let s = Scratch::new("report");
        let said = |p: Policy| {
            let r = Guard::In(Box::new(p)).report(&[]);
            r.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let r = said(s.policy());
        match super::super::supervisor::unavailable() {
            None => {
                assert!(
                    r.contains("connect to nothing over the network, this machine's ports included"),
                    "{r}"
                );
                let open = said(Policy { network: true, ..s.policy() });
                assert!(!open.contains("reach the network"), "{open}");
                assert!(
                    open.contains("llm.sandbox-network doesn't open the network beyond this machine"),
                    "{open}"
                );
            }
            Some(why) => {
                assert!(
                    r.contains(
                        "make no socket: no network, not even this machine's ports, and no unix socket"
                    ),
                    "{r}"
                );
                assert!(r.contains("which macOS allows") && r.contains(&why), "{r}");
            }
        }
        assert!(r.contains("where they are now"), "{r}");
        assert!(r.contains("a protected name it makes where none is yet isn't refused"), "{r}");
        assert_eq!(r.contains("can't keep a command's signals"), abi < 6, "{r}");
        if abi < 7 {
            assert!(
                r.contains(&format!("this kernel's Landlock (ABI {abi}) doesn't log what it refuses")),
                "{r}"
            );
        }
    }

    /// Without an audit log, what a run refused is read from its error lines: Landlock's "Permission
    /// denied" on a path the policy keeps, as the profile's words — paths as they are (no `/private`).
    #[test]
    fn the_output_says_what_was_refused() {
        let root = PathBuf::from("/w/p");
        let p = Policy::for_project(&Env { root: root.clone(), ..Default::default() });
        let out = [
            (true, "touch: cannot touch '/w/p/Cargo.toml': Permission denied".to_string()),
            (true, "cat: /tmp/x: No such file or directory".to_string()),
        ];
        let said = p.said_in_output(&out, &[]);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!((said[0].op.as_str(), said[0].target.as_str()), ("file-write*", "/w/p/Cargo.toml"));
        assert_eq!(crate::sandbox::paths_in("cat /tmp/x", &root), [PathBuf::from("/tmp/x")]);
        assert_eq!(crate::sandbox::refused_lines(&out), [out[0].1.clone()]);
    }

    /// Landlock's audit records (the kernel's documented form): the run's domain is the one whose record
    /// names the thread that confined it; access records waiting for it come with it, others' are dropped;
    /// a path in hex is read.
    #[test]
    fn audit_records_are_read_for_their_run() {
        let access = |d: &str, b: &str, path: &str| {
            format!(
                "audit: type=1423 audit(1729738800.268:30): domain={d} blockers={b} path={path} dev=\"vda2\" ino=9"
            )
        };
        let domain = |d: &str, pid: u32| {
            format!(
                "audit: type=1424 audit(1729738800.268:30): domain={d} status=allocated mode=enforcing pid={pid} \
                 uid=0 exe=\"/usr/bin/tarae\" comm=\"tarae-landlock\""
            )
        };
        let mut a = Audit { owner: Some(300), ..Default::default() };
        assert!(
            a.line(&access("1a", "fs.write_file", "\"/w/p/Cargo.toml\"")).is_empty(),
            "waits for its domain"
        );
        let d = a.line(&domain("1a", 300));
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].op.as_str(), d[0].target.as_str()), ("file-write*", "/w/p/Cargo.toml"));
        let d = a.line(&access("1a", "fs.read_file", "2F686F6D652F752F612062"));
        assert_eq!((d[0].op.as_str(), d[0].target.as_str()), ("file-read-data", "/home/u/a b"));
        assert_eq!(
            a.line(&access("1a", "scope.signal", "\"\"")).first().map(|d| d.op.clone()),
            Some("signal".into())
        );
        // Another run's domain
        assert!(a.line(&access("2b", "fs.make_reg", "\"/x\"")).is_empty());
        assert!(a.line(&domain("2b", 301)).is_empty(), "not this run's");
        assert!(a.line(&access("2b", "fs.make_reg", "\"/x\"")).is_empty());
        assert!(a.line("audit: type=1400 apparmor=\"DENIED\"").is_empty());
    }

    /// A watch reads its run's records as they come (a stand-in for `journalctl`: a script that prints the
    /// kernel's lines once the run's thread is told), and stops with it.
    #[test]
    fn a_watch_reads_its_runs_audit_records() {
        let dir = std::env::temp_dir().join(format!("tarae-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join("go");
        let mut stream = std::process::Command::new("sh");
        stream.arg("-c").arg(format!(
            "until [ -e '{}' ]; do sleep 0.01; done; \
             echo 'audit: type=1424 audit(1:2): domain=9f status=allocated mode=enforcing pid=4242 uid=1'; \
             echo 'audit: type=1423 audit(1:2): domain=9f blockers=fs.make_reg path=\"/w/p/build.rs\" ino=1'; \
             exec sleep 30",
            go.display()
        ));
        let w = crate::sandbox::Watch::start_audit_from("t.", stream);
        w.owned_by(4242);
        std::fs::write(&go, "").unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while w.blocked().seen.is_empty() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let b = w.blocked();
        assert_eq!(b.seen.len(), 1, "{b:?}");
        assert_eq!((b.seen[0].op.as_str(), b.seen[0].target.as_str()), ("file-write*", "/w/p/build.rs"));
        let pid = w.pid().unwrap();
        w.finish();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // SAFETY: signal 0 only asks.
        while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // SAFETY: as above.
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0, "the stream stopped with it");
        std::fs::remove_dir_all(&dir).ok();
    }
}
