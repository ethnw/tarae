//! The claude process itself under Landlock and a seccomp filter on Linux (V1 Linux part 2, 2026-10-15; on your own
//! `~/.claude` and on by default since claudeown, 2026-10-16 — `target/sb/claudeown-plan.md`). The same pieces as
//! macOS's profile (`Profile`: `kept_reads`, the protected names, the files named like keys, the temp folders, the
//! devices), made into grants (`Profile::grants` — data, checked on every system) and those into Landlock rules as
//! the runs' are (`sandbox::linux::grant_around`, around holes and keys). Made on a worker (`for_chat`), applied on a
//! short-lived thread that then starts claude (`spawn`; `no_new_privs` with it) — Landlock and seccomp confine the
//! calling thread and what it starts.
//!
//! Reads: `/` with home, the terminals (another one's input, tarae's own) and every kept place outside home as holes;
//! home entry by entry around what's kept — the runs' `kept_reads` but claude's own (`CLAUDES`), tarae's state —
//! and around the files named like keys at its top; tarae's log as a file. Home-wide, not the runs' narrow home: a
//! Read you allow on a card still reaches the file (a running process can't be given new rules). The project's
//! files named like keys are left out wherever it is. Writes: `~/.claude` whole (its login is refreshed there: a
//! lock folder made at its top, the store renamed over — a grant narrower than the folder refused the lock, and a
//! confined claude then failed every turn once its token was refused: run R), `~/.claude.json` as a file (its temp
//! file and lock beside it in home are refused; claude then writes it in place — measured), the version-lock
//! folders whole, the temp folders, the devices but the terminals (claude starts no pty under `-p`, and isn't setsid: `/dev/tty` is tarae's own terminal —
//! a write there is on your screen, its ioctl types keys into tarae); the project only in the arms with Claude Code's
//! Edit and Write, as runs have it (N1): entry by entry around the protected names there, so a folder holding one
//! (the top: `.git`) takes no new entry: a new file there is refused, and so is the temp file claude's Edit and Write
//! make beside the one they change — claude 2.1.296 then writes a file already there in place (measured, the live
//! check's run C), so an edit there works; the chat says what's refused (`said_in_result`). Unix sockets: Landlock's
//! files-only ruleset (`landlock::Ruleset::files`) leaves them be, and the seccomp filter refuses making one
//! (`seccomp::Claude`: `socket(AF_UNIX)` — glibc's nscd look-up is claude's only one, and it goes on; `socketpair`
//! and the internet its own) — no path it finds reaches the session's D-Bus, tmux or an agent (G24); the scopes
//! from ABI 6 (abstract sockets, signals outside its domain); its refusals read from Landlock's audit from ABI 7, as
//! runs'. Below `MIN_ABI`, rules the kernel refuses or that don't go on, claude runs as you, said — or not at all,
//! with `llm.claude-sandbox-required` (`claude::start_as`); so does a claude whose program is in a place the rules
//! keep (`program_kept`).
//!
//! What Landlock can't say (`LIMITS`, the threat model's §2): a name inside a granted folder (`~/.claude` is all or
//! nothing: what your own Claude Code runs later there — settings, hooks, `CLAUDE.md`, commands — is claude's to
//! write, where macOS's profile keeps it, G25); a file named like a key deeper in home than its top (macOS's regex
//! has no such gap; claude's Read still asks tarae, `reads.rs`); what's made in home after the rules (closed until
//! claude's next start: the chat makes each claude's as it starts); home's own listing (no rule can hold home
//! without what's kept in it).

// (the grants are data on every system — their tests and golden run on macOS too; only Linux makes rules of them)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::{OWN_DIRS, Profile, profile_path};

/// Below this Landlock ABI claude runs unconfined, said: before 3 truncating any file stays open (the runs' line —
/// `sandbox::linux::FULL`).
pub const MIN_ABI: u32 = 3;

/// What the chat says once since tarae started, when its claude runs confined on Linux: what the rules can't hold —
/// unix sockets plainly, a way out of them (decision 17: files only).
pub const LIMITS: &str = "claude itself runs under Landlock and makes no unix socket: it reads all but what's \
                          kept (keys, tokens, tarae's state) and writes only your ~/.claude and ~/.claude.json, the \
                          temp folders and the devices. Landlock grants ~/.claude whole — claude refreshes your \
                          login there — so what your own Claude Code runs later in it (settings, hooks, CLAUDE.md, \
                          commands, agents, skills, plugins) is this claude's to write, where macOS keeps them from \
                          it. Nor can Landlock keep from it a file named like a key deeper in home than its top. \
                          What's made after it started is as it was until its next start: in home closed to it, a \
                          key or protected name in the project open to it";

/// …and in the arms with Claude Code's Edit and Write.
pub const LIMITS_EDIT: &str = " — a folder holding a protected name (the project's top holds .git) takes no new \
                               file from its Write; a file already there is changed in place (claude 2.1.296 falls \
                               back to that when its temp file beside it is refused). tarae's edit makes new files \
                               there";

/// What a grant gives — Landlock's rights on Linux (`bits`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Right {
    /// Reading, listing, running.
    Read,
    /// Reading one file.
    ReadFile,
    /// Reading, writing and truncating one file, not replacing it: `~/.claude.json` (its folder is home).
    File,
    /// Every right (but unix sockets by path, never handled): claude's folders, the temp folders, the project (N1).
    All,
    /// Reading and writing a device file, not making or removing it (the runs' `WRITE_ONE`).
    Device,
}

impl Right {
    /// In a few words, for the golden.
    pub fn said(self) -> &'static str {
        match self {
            Right::Read => "read",
            Right::ReadFile => "read the file",
            Right::File => "write the file",
            Right::All => "all",
            Right::Device => "device",
        }
    }
}

/// One grant of claude's rules: `right` beneath `path` (none where it isn't there), around `holes` (no rule at or
/// below one) and `keys` (files named like keys: no rule, their folders all but reading files).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub path: PathBuf,
    pub right: Right,
    pub holes: Vec<PathBuf>,
    pub keys: Vec<PathBuf>,
}

/// Device files claude writes: the runs' (`sandbox::linux::DEVICES`, measured) but the terminals (`TERMINALS`).
const DEVICES: [(&str, Right); 4] = [
    ("/dev/null", Right::Device),
    ("/dev/zero", Right::Device),
    ("/dev/full", Right::Device),
    ("/dev/shm", Right::All),
];

/// The terminals, holes in claude's reads and never written: `/dev/pts` is every terminal's input, and `/dev/tty` is
/// tarae's own — claude isn't setsid (runs are), so a write there lands on your screen and an ioctl (`TIOCSTI`,
/// where the kernel still allows it) types keys into tarae, a card's `y` too. claude starts no pty under `-p`.
const TERMINALS: [&str; 2] = ["/dev/pts", "/dev/tty"];

/// Claude's rules as made for this kernel, with what they were made around — one claude's, made as it starts: the
/// chat makes the next claude's again (`chat.rs`'s `confine_ready`), the reviewer each review's.
#[derive(Debug)]
pub struct Rules {
    /// The protected names in the project when they were made (`linux::there`): a folder on the way to one takes
    /// no new entry (N1) — what `writes_allow` says of a file made there.
    pub protected: Vec<PathBuf>,
    #[cfg(target_os = "linux")]
    pub ruleset: crate::landlock::Ruleset,
    /// Where its refusals are read (`linux::audit_readable`: ABI 7 and a readable log), or why nowhere.
    #[cfg(target_os = "linux")]
    pub audit: Result<crate::sandbox::linux::AuditSource, String>,
}

// (one made once: the same rules only as the same value)
impl PartialEq for Rules {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for Rules {}

/// `~/.claude.json`'s name in home.
const OWN_FILE: &str = ".claude.json";

impl Profile {
    /// claude's own places, written (real): `~/.claude` whole, `~/.claude.json` as a file, the version-lock folders.
    fn own_places(&self) -> Vec<(PathBuf, Right)> {
        let mut own = vec![
            (profile_path(&self.home.join(OWN_DIRS[0])), Right::All),
            (profile_path(&self.home.join(OWN_FILE)), Right::File),
        ];
        own.extend(self.locks.iter().map(|l| (l.clone(), Right::All)));
        own
    }

    /// Claude's grants, in order, given what's in the project now: `protected` (where each protected name starts —
    /// `linux::there`) and `keys` (its files named like keys). None for macOS's profile (Seatbelt's text).
    pub fn grants(&self, protected: &[PathBuf], keys: &[PathBuf]) -> Vec<Grant> {
        if self.os != crate::sandbox::Os::Linux {
            return Vec::new();
        }
        let (home, root) = (&self.home, &self.root);
        let keys: Vec<PathBuf> = keys.iter().cloned().chain(self.home_keys()).collect();
        let grant = |path: &Path, right: Right, holes: Vec<PathBuf>| Grant {
            path: path.to_path_buf(),
            right,
            holes,
            keys: keys.iter().filter(|k| k.starts_with(path)).cloned().collect(),
        };
        let broken = broken();
        let kept = if broken == "open" { Vec::new() } else { self.kept.clone() };
        // Reads: all but home, the terminals and what's kept outside home (`$XDG_STATE_HOME`'s tarae state, a moved
        // toolchain home); in home all but what's kept — the project by its own rule when a kept place holds it,
        // around what's kept below it
        let mut holes = vec![home.clone()];
        holes.extend(TERMINALS.iter().map(PathBuf::from));
        holes.extend(kept.iter().filter(|k| !k.starts_with(home)).cloned());
        let mut out = vec![grant(Path::new("/"), Right::Read, holes)];
        out.push(grant(home, Right::Read, kept.iter().filter(|k| k.starts_with(home)).cloned().collect()));
        if kept.iter().any(|k| root.starts_with(k)) {
            let below = self.kept.iter().filter(|k| k.starts_with(root) && *k != root).cloned().collect();
            out.push(grant(root, Right::Read, below));
        }
        out.extend(self.log.iter().map(|l| grant(l, Right::ReadFile, Vec::new())));
        // Writes: claude's own, the temp folders, the devices — none reaching into home or the project but their own
        // (a temp folder holding either: a test's home)
        let places = self.own_places().into_iter();
        let temps = self.temps.iter().map(|t| (t.clone(), Right::All));
        let devices = DEVICES.iter().map(|(d, r)| (PathBuf::from(d), *r));
        for (path, right) in places.chain(temps).chain(devices) {
            if broken == "closed" && matches!(right, Right::All | Right::File) {
                continue;
            }
            let holes = self.write_holes(&path, protected);
            out.push(grant(&path, right, holes));
        }
        // The project in the arms with Edit and Write (N1): around its protected names, never into home
        if self.writes_project {
            let mut holes: Vec<PathBuf> = protected.to_vec();
            holes.extend(home.starts_with(root).then(|| home.clone()));
            out.push(grant(root, Right::All, holes));
        }
        out
    }

    /// What a write grant at `path` (claude's own, a temp folder, a device) leaves out: home, where it holds it but
    /// isn't in it; what's kept below it (a state folder in `/tmp`); the project, where it holds it — whole in the
    /// default arm, its protected names in the others.
    fn write_holes(&self, path: &Path, protected: &[PathBuf]) -> Vec<PathBuf> {
        let mut holes: Vec<PathBuf> =
            self.kept.iter().filter(|k| k.starts_with(path) && *k != path).cloned().collect();
        if self.home.starts_with(path) && self.home != path {
            holes.push(self.home.clone());
        }
        if self.root.starts_with(path) {
            match self.writes_project {
                false => holes.push(self.root.clone()),
                true => holes.extend(protected.iter().filter(|p| p.starts_with(path)).cloned()),
            }
        }
        holes
    }

    /// The files named like keys at home's top (`.netrc`, `.env`): what home's grant can leave out without walking
    /// it — deeper ones it can't (the module's note).
    fn home_keys(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.home) else { return Vec::new() };
        let mut keys: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| crate::secretnames::is_secret(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        crate::sorting::sort(&mut keys);
        keys
    }

    /// `reads_allow` on Linux: its grants' reads, in Rust.
    pub(super) fn reads_allow_in(&self, real: &Path) -> bool {
        if self.log.as_deref() == Some(real) || self.own_places().iter().any(|(d, _)| real.starts_with(d)) {
            return true;
        }
        let key = super::super::key_named_in(&self.root, real)
            || real.parent() == Some(self.home.as_path()) && crate::secretnames::named_secret(real);
        if key || TERMINALS.iter().any(|t| real.starts_with(t)) {
            return false;
        }
        broken() == "open"
            || !self
                .kept
                .iter()
                .any(|k| real.starts_with(k) && !(self.root.starts_with(k) && real.starts_with(&self.root)))
    }

    /// `writes_allow` on Linux: its grants' writes, in Rust — a file not there yet in a folder on the way to a
    /// protected name too (N1: such a folder takes no new entry).
    pub(super) fn writes_allow_in(&self, real: &Path) -> bool {
        if self.own_places().iter().any(|(d, _)| real.starts_with(d)) {
            return true;
        }
        let in_home = real.starts_with(&self.home);
        if real.starts_with(&self.root) && !(in_home && self.home.starts_with(&self.root)) {
            return self.writes_project
                && (broken() == "open"
                    || !super::super::protected_in(&self.root, real) && !self.made_where_protected(real));
        }
        !in_home
            && (self.temps.iter().any(|t| real.starts_with(t))
                || DEVICES.iter().any(|(d, _)| real.starts_with(d)))
    }

    /// claude's own places made where nothing is, before its rules are (a grant needs its path there): `~/.claude`
    /// (0700) and `~/.claude.json` (`{}`, 0600) from home's own folder, the version-lock folders (their parents as
    /// they come — yours). Never through a link: `mkdirat` and `O_CREAT | O_EXCL` make nothing where a link is,
    /// leading anywhere or nowhere. What's there is used as it is, wherever it leads (dotfiles), but one that leads
    /// to no folder or file is refused — Err, and claude runs as you, said (`start_as`). A worker's.
    pub fn own_made(&self) -> Result<(), String> {
        made_at(&self.home, OWN_DIRS[0], None)?;
        made_at(&self.home, OWN_FILE, Some(b"{}"))?;
        for l in &self.locks {
            let (Some(parent), Some(name)) = (l.parent(), l.file_name()) else { continue };
            made_at(parent, &name.to_string_lossy(), None)?;
        }
        Ok(())
    }

    /// Where `command` (`llm.command`) starts from when its rules keep that place: it couldn't be run under them — claude runs unconfined then, said,
    /// rather than failing to start. Found as `Command` finds it (a path from `root`, else on `PATH` as `here` says
    /// it), links followed. None: allowed, or not found (the spawn says so).
    pub fn program_kept(&self, command: &str, here: &super::Here) -> Option<PathBuf> {
        let found = match command.contains('/') {
            true => Some(self.root.join(command)),
            false => {
                let path = here.var("PATH")?;
                std::env::split_paths(&path).map(|d| d.join(command)).find(|p| p.is_file())
            }
        };
        let real = std::fs::canonicalize(found?).ok()?;
        (!self.reads_allow(&real)).then_some(real)
    }

    /// Whether `real` isn't there yet and its folder is on the way to a protected name (`Rules::protected`).
    fn made_where_protected(&self, real: &Path) -> bool {
        let Some(dir) = real.parent() else { return false };
        let Some(rules) = &self.rules else { return false };
        real.symlink_metadata().is_err() && rules.protected.iter().any(|p| p.starts_with(dir) && p != dir)
    }
}

/// `name` in `dir` made where nothing is — a folder (0700), or (`file`) a file of that text (0600) — through no link:
/// `mkdirat` and `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` from `dir`'s descriptor (`dir` made first where it isn't,
/// 0700). Then it must be one, followed where it leads.
fn made_at(dir: &Path, name: &str, file: Option<&[u8]>) -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::DirBuilderExt;
    let at = dir.join(name);
    let said = |e: std::io::Error| format!("claude's {}: {e}", at.display());
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(said)?;
    let d = std::fs::File::open(dir).map_err(said)?;
    let c = crate::sandbox::c_name(std::ffi::OsStr::new(name))?;
    match file {
        // SAFETY: a folder descriptor of ours and a name, live for the call (one there already: EEXIST).
        None => unsafe {
            libc::mkdirat(d.as_raw_fd(), c.as_ptr(), 0o700);
        },
        Some(text) => {
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: as above; a descriptor it returns is ours alone, owned by the `File` at once.
            let fd = unsafe { libc::openat(d.as_raw_fd(), c.as_ptr(), flags, 0o600 as libc::c_uint) };
            if fd >= 0 {
                // SAFETY: a new descriptor of ours (above).
                let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
                f.write_all(text).map_err(said)?;
            }
        }
    }
    let there =
        std::fs::metadata(&at).map_err(|e| format!("claude's {}: {e} — a link to nowhere?", at.display()))?;
    match (file.is_some(), there.is_file(), there.is_dir()) {
        (true, true, _) | (false, _, true) => Ok(()),
        _ => Err(format!(
            "claude's {}: not a {}",
            at.display(),
            if file.is_some() { "file" } else { "folder" }
        )),
    }
}

/// The break-once (tests): `open` — nothing kept in home, the project's protected names written; `closed` — none of
/// claude's own places nor temp granted; `cache` — the chat keeps one claude's rules for the next (`chat.rs`'s
/// `confine_ready`). Else "".
pub(crate) fn broken() -> String {
    #[cfg(test)]
    {
        std::env::var("TARAE_TEST_CLAUDE_BREAK").unwrap_or_default()
    }
    #[cfg(not(test))]
    {
        String::new()
    }
}

/// Landlock's rights for a grant's.
#[cfg(target_os = "linux")]
fn bits(right: Right) -> u64 {
    use crate::landlock as l;
    match right {
        Right::Read => l::EXECUTE | l::READ_FILE | l::READ_DIR,
        Right::ReadFile => l::READ_FILE,
        Right::File => l::READ_FILE | l::WRITE_FILE | l::TRUNCATE,
        Right::All => l::ALL,
        Right::Device => l::READ_FILE | l::WRITE_FILE | l::TRUNCATE | l::IOCTL_DEV,
    }
}

/// What claude's watch reads Landlock's audit with (`claude::watch`): its domain is the one the confining thread made
/// — tarae's process, the thread named by the tag (`spawn`). (The break-once `open`: no owner, nothing heard.)
#[cfg(target_os = "linux")]
pub fn audit_of(p: &Profile) -> crate::sandbox::linux::Audit {
    let owner = crate::sandbox::linux::Owner { pid: std::process::id(), comm: p.tag.clone() };
    let mut audit = crate::sandbox::linux::Audit::default();
    audit.owner = (broken() != "open").then_some(owner);
    audit
}

/// `grants` as rules added to `r` — each as the runs' are, around its holes and keys (`grant_around`).
#[cfg(target_os = "linux")]
pub fn rules_in(
    mut r: crate::landlock::Ruleset,
    grants: &[Grant],
) -> std::io::Result<crate::landlock::Ruleset> {
    // (the folders a file made in can't be read until the next start: the runs' report says those; claude's
    // folders hold no key, so only the project's can, and `LIMITS` says it)
    let mut unread = Vec::new();
    for g in grants {
        crate::sandbox::linux::grant_around(&mut r, &g.path, bits(g.right), &g.holes, &g.keys, &mut unread)?;
    }
    Ok(r)
}

/// The chat's (and the reviewer's) claude's profile on Linux, for `root` and `home` as `here` says them — or why
/// it runs unconfined: no Landlock, one below `MIN_ABI`, claude's own places not made, rules the kernel refused. Walks
/// the project once (`linux::there`) and probes the audit once (`audit_readable`, ≤ ½ s): a worker's. `command`:
/// claude's program (`llm.command`) — one the rules keep can't run under them (`program_kept`).
#[cfg(target_os = "linux")]
pub fn for_chat(
    root: &Path,
    home: &Path,
    writes_project: bool,
    command: &str,
    here: &super::Here,
) -> Result<Profile, String> {
    let abi = crate::landlock::abi().map_err(|e| format!("this kernel has no Landlock ({e})"))?;
    if abi < MIN_ABI {
        return Err(format!(
            "this kernel's Landlock (ABI {abi}) can't control truncating files — claude could empty any file of yours"
        ));
    }
    let p = Profile::new(root, home, writes_project, here, &super::tag());
    if let Some(program) = p.program_kept(command, here) {
        return Err(format!(
            "its program {} is in a place its rules keep closed (your own Claude Code's) — Claude Code's native \
             install (`claude install`) puts it where they don't",
            program.display()
        ));
    }
    p.own_made()?;
    let there = crate::sandbox::linux::there(&p.root, &[], &[]);
    let grants = p.grants(&there.protected, &there.keys);
    let said = |e: std::io::Error| format!("its Landlock rules couldn't be made: {e}");
    let ruleset = rules_in(crate::landlock::Ruleset::files(abi).map_err(said)?, &grants).map_err(said)?;
    let audit = crate::sandbox::linux::audit_readable(abi);
    let rules = Rules { protected: there.protected, ruleset, audit };
    Ok(Profile { rules: Some(std::sync::Arc::new(rules)), ..p })
}

/// `cmd` started confined to `p`'s rules and claude's filter (`confined`): the child takes the thread's
/// credentials, and nothing else of tarae's is confined. Ok(Err(why)): they didn't go on, and nothing was started —
/// the caller says what then (`claude::spawn`), from its own thread, never the half-confined one.
#[cfg(target_os = "linux")]
pub fn spawn(
    p: &Profile,
    cmd: &mut std::process::Command,
) -> std::io::Result<Result<std::process::Child, String>> {
    confined(p, || cmd.spawn())
}

/// `start` on a short-lived thread named by `p`'s tag (Landlock's audit names the thread that confined it —
/// `audit_of`), which confines itself first: `p`'s rules, then claude's seccomp filter (`seccomp::Claude`;
/// `no_new_privs` with them). Ok(Err(why)): either didn't go on, `start` not called. Waits for `start` only.
#[cfg(target_os = "linux")]
pub fn confined<T: Send>(
    p: &Profile,
    start: impl FnOnce() -> std::io::Result<T> + Send,
) -> std::io::Result<Result<T, String>> {
    let Some(rules) = &p.rules else { return Ok(Err("its Landlock rules weren't made".into())) };
    let filter = match crate::seccomp::Claude::new() {
        Ok(f) => f,
        Err(e) => return Ok(Err(format!("its seccomp filter: {e}"))),
    };
    std::thread::scope(|s| {
        let t = std::thread::Builder::new().name(p.tag.clone()).spawn_scoped(s, || {
            if let Err(e) = rules.ruleset.apply() {
                return Ok(Err(format!("its Landlock rules didn't go on: {e}")));
            }
            if let Err(e) = filter.apply() {
                return Ok(Err(format!("its seccomp filter didn't go on: {e}")));
            }
            start().map(Ok)
        })?;
        t.join().unwrap_or_else(|_| Err(std::io::Error::other("claude's confining thread panicked")))
    })
}

/// The hidden subcommand that starts claude as tarae does, confined (the live check, `target/sb/v1b-linux.sh` run
/// C): `tarae --claude-confined [--writes] <claude> [args…]`, in the project.
#[cfg(target_os = "linux")]
pub const FLAG: &str = "--claude-confined";

/// `FLAG`'s: its profile for the project here (claude's own places made; the arm with Edit and Write with
/// `--writes`), its command as `llm::spawn` makes it (the keys in the environment kept: the live check's stub takes
/// a fake one), this thread confined — rules and filter —, then exec'd. Never returns.
#[cfg(target_os = "linux")]
pub fn main() -> ! {
    use std::os::unix::process::CommandExt;
    fn fail(e: String) -> ! {
        eprintln!("tarae {FLAG}: {e}");
        std::process::exit(126)
    }
    let mut args: Vec<String> = std::env::args().skip(2).collect();
    let writes = args.first().is_some_and(|a| a == "--writes");
    if writes {
        args.remove(0);
    }
    let Some((program, rest)) = args.split_first() else {
        fail(format!("usage: tarae {FLAG} [--writes] <claude> [args…]"))
    };
    let root = std::env::current_dir().unwrap_or_else(|e| fail(e.to_string()));
    let p = super::for_chat(&root, writes, program)
        .unwrap_or_else(|e| fail(format!("claude would run unconfined: {e}")));
    let cfg = crate::llm::LlmConfig {
        command: program.clone(),
        args: rest.to_vec(),
        keep_env_key: true,
        claude_sandbox: true,
        ..Default::default()
    };
    let sub = crate::billing::Credential::Subscription;
    let mut cmd = crate::llm::command(&cfg, &[], &root, None, &sub, Some(&p))
        .unwrap_or_else(|e| fail(e.said(program, true)));
    if let Some(r) = &p.rules {
        r.ruleset.apply().unwrap_or_else(|e| fail(format!("its rules: {e}")));
    }
    let filter = crate::seccomp::Claude::new().unwrap_or_else(|e| fail(format!("its filter: {e}")));
    filter.apply().unwrap_or_else(|e| fail(format!("its filter: {e}")));
    // (the pipes are the caller's: stdin, stdout and stderr as this process has them)
    let e = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .exec();
    fail(format!("{program}: {e}"))
}

#[cfg(test)]
mod tests;
