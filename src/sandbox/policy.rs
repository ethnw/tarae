//! What a sandboxed command may do (`Policy`), and how each system is told it: Seatbelt's profile and its
//! parameters, Landlock's report lines, what a run's output says it was refused, and what the result says.

use super::*;

/// The name a tool's refusal gives bare after the tool's own name — macOS's `touch: Makefile: Operation not permitted`,
/// `mkdir: .github: Permission denied` (`<program>: <name>: <refusal>`) — as a path from `root` (where the run is),
/// folded; `named_in` reads the other forms. A name with no program before it (`Makefile: Operation not permitted`)
/// names nothing. frictions29 30: the kernel's report of a protected name refused was lost (1 in 10, measured), and
/// this line, all the output said, was read as no cause.
fn program_named_in(line: &str, root: &Path) -> Option<PathBuf> {
    let low = line.to_ascii_lowercase();
    let refusals = [": permission denied", ": operation not permitted", ": read-only file system"];
    let at = refusals.iter().find_map(|r| low.find(r))?;
    let (program, name) = line[..at].trim().split_once(": ")?;
    let program_like =
        !program.is_empty() && program.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
    let bare = !name.is_empty()
        && !name.starts_with(['/', '~'])
        && !name.contains(char::is_whitespace)
        && !name.contains(['\'', '"', '‘', '’']);
    if !program_like || !bare {
        return None;
    }
    let mut out = PathBuf::new();
    for c in root.join(name).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

/// What a run's rule for the files named like keys says of one (`Policy::key_rule`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyRule {
    /// Closed by its name below this place — the project's root, or a place outside it a grant or a default
    /// opened (`key_places`): only the user's opening of that file (`Grant::Key`) lets a run read it.
    Closed(PathBuf),
    /// macOS: named like a key in home where nothing opened it — every place a grant opens closes the names like
    /// keys in it, so a read of a folder of it wouldn't open it: only `Grant::Key` does, its read too.
    ClosedOnceRead,
}

/// A policy's key rule taken apart from it (`Policy::key_rules`): what's decided after its run, on a worker —
/// the run's panel (`blocked::choices`), `also`'s card — goes by the rule the run had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRules {
    root: PathBuf,
    keys: Option<Vec<PathBuf>>,
    places: Vec<PathBuf>,
    spared: Vec<PathBuf>,
    supervised: bool,
    reads: Reads,
    run: Option<PathBuf>,
    write: Vec<PathBuf>,
    files: Vec<PathBuf>,
    covered: Vec<PathBuf>,
}

impl KeyRules {
    /// `Policy::key_rule`.
    pub fn rule(&self, real: &Path) -> Option<KeyRule> {
        self.view().rule(real)
    }

    fn view(&self) -> KeyView<'_> {
        KeyView {
            root: &self.root,
            keys: self.keys.as_deref(),
            places: &self.places,
            spared: &self.spared,
            supervised: self.supervised,
            reads: &self.reads,
            run: self.run.as_deref(),
            write: &self.write,
            files: &self.files,
            covered: &self.covered,
        }
    }

    /// Whether anything below the folder `at` (real, outside the project) may be closed by this rule or read from
    /// a copy: a key place it holds or is in, a part of home runs don't read, a setting with a copy. False: below it
    /// runs read every file named like a key — a toolchain's folder, an install (`reads::decide_with` doesn't walk
    /// it).
    pub fn may_close_below(&self, at: &Path) -> bool {
        let v = self.view();
        let in_home = self.reads.home.as_ref().is_some_and(|h| at.starts_with(h) || h.starts_with(at));
        self.keys.is_some()
            && !self.supervised
            && (self.places.iter().any(|k| k.starts_with(at) || at.starts_with(k))
                || (in_home && !v.reads_place(at))
                || self.reads.copies.hidden.iter().any(|(f, _)| f.starts_with(at)))
    }

    /// Whether only the user's opening of that file (`Grant::Key`) lets a run read `real`.
    pub fn closes(&self, real: &Path) -> bool {
        self.rule(real).is_some()
    }

    /// Whether runs read a copy of `real` in its place (a setting holding a token, `tokens::Copies`): the file
    /// itself never — Claude's Read asks for it as for a key (`reads::key_closed`).
    pub fn copied(&self, real: &Path) -> bool {
        self.reads.copies.instead_of(real).is_some()
    }

    /// The rule as designed, whatever the system enforces: macOS's profile's (Linux's Landlock only grants, so it
    /// closes outside the project only what tier own's mounts hide — D-i — and tiers host and offline none) — what
    /// Claude's reads go by (`reads::key_closed`).
    pub fn designed(self) -> KeyRules {
        KeyRules { supervised: false, ..self }
    }
}

/// What the key rule reads of a policy (`Policy::key_view`, `KeyRules`) — the rule written once.
struct KeyView<'a> {
    root: &'a Path,
    keys: Option<&'a [PathBuf]>,
    places: &'a [PathBuf],
    spared: &'a [PathBuf],
    supervised: bool,
    reads: &'a Reads,
    run: Option<&'a Path>,
    write: &'a [PathBuf],
    files: &'a [PathBuf],
    /// Linux tier own: the files named like keys outside the project its run's mounts hid (`linux::Outside`, D-i) —
    /// closed, the rest there not.
    covered: &'a [PathBuf],
}

impl KeyView<'_> {
    fn rule(&self, real: &Path) -> Option<KeyRule> {
        // (`keys` None: closed nowhere — a test's policy; a file the user opened; what no run reads anyway)
        let opened = self.keys?;
        // (what no run reads is the kept places' rule; a setting holding a token, read from a copy, the copies' —
        // its report says so, and no key grant opens it)
        if opened.iter().any(|f| f == real)
            || self.reads.kept_holds(self.root, real)
            || self.reads.copies.instead_of(real).is_some()
        {
            return None;
        }
        if real.starts_with(self.root) {
            return key_named_in(self.root, real).then(|| KeyRule::Closed(self.root.to_path_buf()));
        }
        // (Linux: Landlock only grants — outside the project only what its run's mounts hid is closed (tier own,
        // D-i); the run's own folder, its `TMPDIR`, only it writes: what's there is its own)
        if self.supervised && self.covered.iter().any(|c| c == real) {
            return self.places.iter().find(|k| real.starts_with(k)).map(|k| KeyRule::Closed(k.clone()));
        }
        if self.supervised
            || self.spared.iter().any(|s| real.starts_with(s))
            || self.run.is_some_and(|d| real.starts_with(d))
        {
            return None;
        }
        if let Some(k) = self.places.iter().find(|k| key_named_below(k, real)) {
            return Some(KeyRule::Closed(k.clone()));
        }
        // (in no place that closes it: read as any file there, or — in home, nothing opening it — any folder of it
        // opened would close it)
        let unread = self.reads.home.as_ref().is_some_and(|h| real.starts_with(h)) && !self.reads_place(real);
        let named = self.places.iter().all(|k| !real.starts_with(k)) && key_named_below(Path::new("/"), real);
        (unread && named).then_some(KeyRule::ClosedOnceRead)
    }

    /// Whether the profile's reads in home let a run read `real`, its names aside: the project, where it writes,
    /// what's allowed, a file the user opened (macOS: `Y<n>`, after the deny of home).
    fn reads_place(&self, real: &Path) -> bool {
        match &self.reads.home {
            Some(home) if real.starts_with(home) => {
                real.starts_with(self.root)
                    || self.run.is_some_and(|d| real.starts_with(d))
                    || self.write.iter().chain(&self.reads.allow).any(|w| real.starts_with(w))
                    || self.files.iter().any(|f| f == real)
                    || (!self.supervised && self.keys.is_some_and(|k| k.iter().any(|f| f == real)))
            }
            _ => true,
        }
    }
}

/// What a sandboxed command may do. Every path is real (canonical).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The project: writable but for `protect` and the names `protected_names` gives.
    pub root: PathBuf,
    /// Inside `root`, kept as they are (`llm.sandbox-protect`).
    pub protect: Vec<PathBuf>,
    /// Folders writable besides the project (temp, the toolchain's, `llm.sandbox-write`).
    pub write: Vec<PathBuf>,
    /// Single files writable — the caches' own files (cargo's locks and last-use database), the network closed
    /// even those inside `caches`, and the packs' `write-files`.
    pub files: Vec<PathBuf>,
    /// Folders inside writable ones kept as they are: the terminal multiplexers' sockets (`mux_dirs`) — a
    /// command could otherwise delete or replace the multiplexer the user works in.
    pub keep: Vec<PathBuf>,
    /// The tools' shared caches (the packs' `caches` — cargo's crates and git checkouts) kept as they are while
    /// the network is closed: every project's build outside the sandbox uses what's there, and with no
    /// download a build has nothing new to put there. Only `files` are written in them. What a build needs
    /// that isn't there fails here, and the report says why (`caches_said`).
    pub caches: Vec<PathBuf>,
    /// How each tool's caches are said, while they're kept, and the attribute it sets on them.
    pub caches_said: Vec<CachesSaid>,
    /// Beyond this machine, through tarae's proxy: `llm.sandbox-network` on, or each host asked (`asks_hosts`) —
    /// false, nothing goes beyond (a policy made by hand: tests).
    pub network: bool,
    /// The network closed (`llm.sandbox-network` off): beyond goes through the proxy with an empty list, every
    /// host asked on a card (piece C) — no host the project keeps, the caches written only under a grant for
    /// their pack's registry (`open_granted_caches`).
    pub asks_hosts: bool,
    /// Ports on this machine it may connect to (`llm.sandbox-ports`): the network closed, the only ones;
    /// open, the risky ones listed here stay reachable.
    pub ports: Vec<u16>,
    /// Every port on this machine, the risky ones too (`llm.sandbox-localhost`).
    pub localhost: bool,
    /// This run's own temp folder — its `TMPDIR`, and with the project the only place it may make or reach
    /// a unix socket (`RunDir`).
    pub run: Option<std::sync::Arc<RunDir>>,
    /// The folder the run runs in, real — what a name its output gives relative is from (`from`). None: the
    /// project's root.
    pub cwd: Option<PathBuf>,
    /// Where runs' own folders are made (`runs_root`, real) — every run's, so where runs write
    /// (`write_places`); a run's rules open only its own.
    pub runs: Option<PathBuf>,
    /// This run's tag: every deny carries it (`(with message …)`), so the kernel's report of a denial says
    /// whose it is — by the time it arrives the process is mostly gone (measured), so its pid can't.
    pub tag: Option<String>,
    /// macOS, the network open: this run's own way to tarae's proxy (`proxy::RunPort`) — its port the only one
    /// beyond the opened ones its profile opens. None with the network closed, or where it couldn't be made:
    /// then nothing goes beyond this machine (`loopback_only` says so). None on Linux (the supervisor hands its
    /// connections over); made for a macOS `Here` on any system, so both systems' policies pin alike (`golden`).
    pub proxy: Option<std::sync::Arc<proxy::RunPort>>,
    /// Linux: its supervisor hands its connections to tarae's proxy — reached at `route::PROXY`, no credential (the
    /// supervisor knows the run). Made for the `Here` a policy is made for (`policy_in`), so both systems'
    /// policies pin alike on either (`golden`); `for_project`: this system's.
    pub supervised: bool,
    /// The hosts its network reaches through the proxy, as its rules there hold them (`llm.sandbox-hosts`, and
    /// what the user answered for the project and for this run alone — `tools::with_allowed`). Not part of its key
    /// (`policy_key`): an answer mustn't end what an earlier run left (a daemon) — the proxy's newer answers go
    /// before these.
    pub egress: proxy::egress::Egress,
    /// The run, as the proxy and the main loop name it (`next_run`; a copy of the policy is the same run's).
    pub run_id: u64,
    /// Its session, as the proxy and the main loop name it (`proxy::session_of` its root).
    pub session: u64,
    /// What the sandbox denied this run, watched while it goes (`Watch`).
    pub watch: Option<std::sync::Arc<Watch>>,
    /// What the user let Claude's commands do besides (`allow.rs`) — as the result says it.
    pub granted: Vec<String>,
    /// Unix sockets outside the project it may connect to, each that one file (`allow::Grant::Socket`).
    pub sockets: Vec<PathBuf>,
    /// macOS: the terminals (`TERMINALS`) open to it, every one — the project's grant (`allow::Grant::Terminals`).
    pub terminals: bool,
    /// Files named like keys inside the project (`key_named_in` — `.env`, `*.pem`, as data) are closed to it, but
    /// these the user opened, each that file (`allow::Grant::Key`; real paths) — macOS by the profile (7a), Linux
    /// by Landlock's rules around them (`linux::there`, 7b). None: closed to it nowhere (a test's policy).
    pub keys: Option<Vec<PathBuf>>,
    /// The places outside the project a grant or a default opened — the repository's other checkouts, what the
    /// user let Claude read (`Env::key_places`), the temp folders — where the files named like keys are closed
    /// as in the project (macOS's profile; Landlock only grants, so on Linux only tier own's mounts hide them there,
    /// found before each run — `linux::outside_keys`, D-i). Never in the
    /// project (its own rule — a place holding it leaves it out), never wider: a CA bundle elsewhere is read. Real
    /// paths.
    pub key_places: Vec<PathBuf>,
    /// Inside those, the places whose files stay read whatever their names: a toolchain's installs (the packs'
    /// `installs` — pyenv's, conda's: their CA bundles, their packages' test keys). Not the user's `$TMPDIR`:
    /// every program of theirs writes there (frictions29 round 3) — the run's own is its `RunDir`. Real paths.
    pub key_spared: Vec<PathBuf>,
    /// The folders the checkout's build tool named that stay closed, and why (`asked_dir_verdict`) — said with
    /// the result and on the panel — and who named them (`builds-said`: `cargo`).
    pub refused: Vec<(PathBuf, String)>,
    pub named_by: String,
    /// The folders the packs' `ask` named that stay closed, and why (`ask_dir_verdict` — Go's caches).
    pub refused_asked: Vec<(PathBuf, String)>,
    /// Where it may read (card model P2).
    pub reads: Reads,
    /// What the run's environment gets from the packs: the variables that point a tool at a home of the run's
    /// own (`run_homes_made` — Gradle's), and their `env` (npm's log folder in the run's).
    pub env: Vec<(String, String)>,
    /// The packs' tools' stores on the project's filesystem (`sandbox::disk_stores_in`, compat B12): those told,
    /// each with the variables that tell its tool — in `env` for a run — and those passed over, with why (the
    /// run's report says both). None told sits where a run may write (`DiskStores::written_passed`).
    pub disk_stores: super::DiskStores,
    /// What the run gets of tarae's own environment (`runenv.rs`) — it starts from nothing else — and the names
    /// of what it doesn't.
    pub run_env: runenv::RunEnv,
    /// The users' own homes of the tools that get one of the run's (kept as they are), as Claude is told.
    pub run_homes: Vec<RunHomeSaid>,
    /// The kept run homes its runs get (`with_kept`; held — `policy_in`).
    pub kept: Vec<Kept>,
    /// Linux: the resolvers `resolv.conf` names, reached on port 53 with the network open (D-c, `route`).
    #[cfg(target_os = "linux")]
    pub resolvers: Vec<std::net::SocketAddr>,
    /// Linux: this machine's own addresses (its LAN's, its bridges' — `docker0`'s), read when the policy is made:
    /// `route` takes them as this machine, reached on its opened ports only. None where they couldn't be read:
    /// every address beyond loopback is refused (never opened by a port, never out).
    #[cfg(target_os = "linux")]
    pub own_addresses: Option<Vec<std::net::IpAddr>>,
    /// Tests (Linux): how long the supervisor waits once a socket of tarae's is in a run's place, before the call
    /// that swapped it goes on — the window another thread of the run's calls into.
    #[cfg(all(test, target_os = "linux"))]
    pub swap_pause: Option<std::time::Duration>,
    /// Tests (Linux): its protected names closed by their folders' rules even where its run could have them sealed
    /// by mounts (`linux::Covers::seals`) — for the tests of that rule, in any tier.
    #[cfg(all(test, target_os = "linux"))]
    pub seals_off: bool,
    /// Linux: the policy as Landlock's rules (`linux::confine`, made on the worker) — Err why it couldn't be
    /// (the run then doesn't start: `spawn`); None before it's made (`policy` makes it).
    #[cfg(target_os = "linux")]
    pub confine: Option<Result<linux::Confine, String>>,
    /// Linux, tier own: whether the run has its own private `/dev/shm` tmpfs.
    pub own_shm: bool,
}

/// The ioctl commands a run may use on a terminal (macOS's numbers, the same on every Mac — written here, not
/// read from the build's libc, so both systems' policies pin alike: `golden`; checked against libc on macOS):
/// what a program on a pty asks of it — its size, its modes (raw mode: `TIOCSETA*` — tarae's own e2e and shots
/// need them), its line discipline read (`stty` asks it before anything, friction 47; never set), its process
/// group, the bytes waiting, close-on-exec. Not `TIOCSTI` (macOS refuses it anyway — measured), nor making one
/// (`TIOCPTYGRANT`, `TIOCPTYUNLK`, `TIOCPTYGNAME`: the broker makes it, outside).
pub const TTY_IOCTLS: &[(&str, u64)] = &[
    ("TIOCGWINSZ", 0x4008_7468),
    ("TIOCSWINSZ", 0x8008_7467),
    ("TIOCSCTTY", 0x2000_7461),
    ("TIOCGETA", 0x4048_7413),
    ("TIOCSETA", 0x8048_7414),
    ("TIOCSETAW", 0x8048_7415),
    ("TIOCSETAF", 0x8048_7416),
    ("TIOCGETD", 0x4004_741a),
    ("TIOCGPGRP", 0x4004_7477),
    ("TIOCSPGRP", 0x8004_7476),
    ("TIOCFLUSH", 0x8004_7410),
    ("TIOCDRAIN", 0x2000_745e),
    ("TIOCOUTQ", 0x4004_7473),
    ("FIONREAD", 0x4004_667f),
    ("FIONBIO", 0x8004_667e),
    ("FIOCLEX", 0x2000_6601),
    ("FIONCLEX", 0x2000_6602),
];

impl Policy {
    /// Whether its run gets a pty broker (macOS, `ptybroker`): it has a folder of its own, and no socket of the
    /// user's is granted it — one could hand it a descriptor of the user's terminal, and its profile then has no
    /// ioctl on terminals (`TTY_IOCTLS`): such a run asks the terminals card as before.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // (macOS's — its test runs everywhere)
    pub fn brokers_ptys(&self) -> bool {
        self.run.is_some() && self.sockets.is_empty()
    }

    /// Piece C, each host asked: the caches of a pack one of whose `registries` (`registries`: by pack) names a
    /// host this run's rules carry the user's yes for (`egress.answered` — this run's, this conversation's, this
    /// session's) — written as with the network open, for this run: its rules are made once, so a yes given while
    /// it runs opens them from the next run on. The network open: they are already.
    pub fn open_granted_caches(&mut self, registries: &[(String, proxy::egress::Pattern)]) {
        if !self.asks_hosts {
            return;
        }
        // (a registry at its own port, else HTTPS's — as the card names it: `hosts::registry_of`; a `*.domain`
        // answered is the registry as the pack writes it, the card's suggestion)
        let answered = &self.egress.answered;
        let granted = |pack: &str| {
            registries.iter().filter(|(of, _)| of == pack).any(|(_, r)| {
                answered.iter().any(|(h, p)| match h.starts_with("*.") {
                    true => r.to_string() == *h,
                    false => r.matches(h, *p, 443),
                })
            })
        };
        let (open, kept): (Vec<CachesSaid>, Vec<CachesSaid>) =
            std::mem::take(&mut self.caches_said).into_iter().partition(|c| granted(&c.pack));
        self.caches_said = kept;
        for c in open {
            self.caches.retain(|d| !c.dirs.contains(d));
            for d in c.dirs {
                if !self.write.contains(&d) {
                    self.write.push(d);
                }
            }
        }
    }

    /// Its run's proxy URL — the one thing the packs' proxy lines and the proxy variables hang on (step 5f): a
    /// port and token of the run's own where it has one (`proxy`: macOS), else, where its supervisor hands its
    /// connections over (`supervised`: Linux), that address (`route::PROXY` — no credential: the supervisor
    /// knows the run). None: the network closed, or no way to the proxy was made — nothing goes beyond this
    /// machine.
    pub fn proxy_url(&self) -> Option<proxy::ProxyUrl> {
        match (&self.proxy, self.supervised) {
            _ if !self.network => None,
            (Some(r), _) => Some(r.url()),
            (None, true) => Some(proxy::ProxyUrl {
                host: route::PROXY.ip().to_string(),
                port: route::PROXY.port(),
                user: None,
                password: None,
                per_run: false,
            }),
            (None, false) => None,
        }
    }

    /// The Seatbelt profile: denied by default (macdeny, as srt's), then what every run may do (`BASE`) and these
    /// rules. Paths aren't in it: they're `-D` parameters (`params`), so no path can change what it says. Every
    /// deny carries the run's tag (`tag`) — the only text from outside, and tarae's own.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
    pub fn profile(&self) -> String {
        let with = self.tag.as_ref().map(|t| format!(" (with message \"{t}\")")).unwrap_or_default();
        let deny = |rule: &str| format!("(deny {rule}{with})\n");
        // The network open goes beyond this machine only through tarae's proxy, at the run's own port (step 5d:
        // another run's isn't reached — M3d); the port is a number tarae made, never text from outside
        let proxied = self.proxy.as_ref().filter(|_| self.network).map(|r| r.port);
        // Denied by default, as srt's profile is (macdeny): every refusal carries the run's tag, so the watch
        // sees it. Then what every run may do (`BASE`), and trust's service where the run goes beyond this
        // machine as it starts — a host the user says yes to during the run (its card) gets it from the next run,
        // as a registry's caches do
        let e = &self.egress;
        let beyond = proxied.is_some() && (e.any || !e.hosts.is_empty() || !e.answered.is_empty());
        let mut p = String::from("(version 1)\n");
        p.push_str(&deny("default"));
        for (rule, _) in BASE {
            p.push_str(&format!("(allow {rule})\n"));
        }
        if beyond {
            p.push_str(&format!("(allow {TRUSTD})\n"));
        }
        // Reads are open but where the rules below close them (srt's too). Writes are where these allow them
        // (a hard link needs these write rules to allow its source too — Seatbelt's own
        // `forbidden-link-priv<file-write*>`, measured 2026-10-04, `target/sb/p1-links2.sh` — so a run can't
        // link a file from outside into the project, nor give a kept one a second name to write through: no
        // `file-link` rule of its own is needed)
        p.push_str("(allow file-read*)\n");
        let protect: String =
            (0..self.protect.len()).map(|i| format!(" (require-not (subpath (param \"P{i}\")))")).collect();
        p.push_str(&format!("(allow file-write* (require-all (subpath (param \"ROOT\")){protect}))\n"));
        let run = if self.run.is_some() { " (subpath (param \"RUN\"))" } else { "" };
        let dirs: String = (0..self.write.len()).map(|i| format!(" (subpath (param \"W{i}\"))")).collect();
        if !run.is_empty() || !dirs.is_empty() {
            p.push_str(&format!("(allow file-write*{run}{dirs})\n"));
        }
        // Reads in home (card model P2): the project, where it writes, and what's allowed (`Reads`). A hard
        // link's source must be readable too — Seatbelt's own `forbidden-link-priv<file-read-data>`, measured
        // 2026-10-05 with `link(2)` itself, `target/sb/p2-links2.sh` — so nothing a run can't read comes into
        // the project by a link, though its folder be writable: a `file-link` rule would add nothing (its
        // places would be these write rules') and costs every build a denial per object file rustc links
        if self.reads.home.is_some() {
            let files: String =
                (0..self.files.len()).map(|i| format!(" (literal (param \"F{i}\"))")).collect();
            let reads: String =
                (0..self.reads.allow.len()).map(|i| format!(" (subpath (param \"R{i}\"))")).collect();
            p.push_str(&deny("file-read-data file-read-xattr (subpath (param \"HOME\"))"));
            p.push_str(&format!(
                "(allow file-read-data file-read-xattr (subpath (param \"ROOT\")){run}{dirs}{files}{reads})\n"
            ));
            // …and the global preferences (locale, languages) Apple's libraries read in every tool — cargo,
            // rustup, the JVM, Node, curl: CoreFoundation reads their plists itself, cfprefsd being out of reach (it
            // hands out every domain the profile lets it: `user-preference-read`) — this domain only (macdeny)
            p.push_str(&format!(
                "(allow file-read-data (require-all (subpath (param \"HOME\")) {GLOBAL_PREFERENCES}))\n"
            ));
        }
        // After the allows, so these win. The names a run keeps off, below the project's root however deep
        // (`protected_writes`)
        p.push_str(&deny(&format!("file-write* {}", protected_writes(&self.root))));
        // …the files named like keys (`secretnames`, data — 7a): not read, below the root however deep, but a
        // template or a public key (its `!` lines), what's below tools' output (a package's test keys), and each
        // file the user opened (`keys`)
        if let Some(keys) = &self.keys {
            p.push_str(&deny(&format!(
                "file-read-data file-read-xattr (require-all (subpath (param \"ROOT\")) {})",
                key_files(&below(&self.root))
            )));
            // …and below the places outside it a grant or a default opened (frictions29), alike: one rule for
            // the places as deep as each other (the regex counts a place's parts, as `below` does the root's)
            let mut deep: Vec<(String, String)> = Vec::new();
            for (i, place) in self.key_places.iter().enumerate().filter(|_| !self.supervised) {
                let (at, sub) = (below(place), format!(" (subpath (param \"KP{i}\"))"));
                match deep.iter_mut().find(|(b, _)| *b == at) {
                    Some((_, subs)) => subs.push_str(&sub),
                    None => deep.push((at, sub)),
                }
            }
            // (never in the project: a place holding it — a folder granted above it, the temp folder it's in —
            // leaves it to the root's rule, whose exceptions are counted from the root; nor in what's spared — an
            // install — nor the run's own folder, which only it writes; below an install's packages there a file
            // is the package's)
            let installs = (0..self.key_spared.len()).map(|i| format!(" (subpath (param \"KI{i}\"))"));
            let spared: String = std::iter::once(run.to_string()).chain(installs).collect();
            let not = match spared.is_empty() {
                true => "(require-not (subpath (param \"ROOT\")))".to_string(),
                false => format!("(require-not (require-any (subpath (param \"ROOT\")){spared}))"),
            };
            for (at, subs) in deep {
                p.push_str(&deny(&format!(
                    "file-read-data file-read-xattr (require-all (require-any{subs}) {not} {})",
                    key_files_outside(&at)
                )));
            }
            let open: String = (0..keys.len()).map(|i| format!(" (literal (param \"Y{i}\"))")).collect();
            if !open.is_empty() {
                p.push_str(&format!("(allow file-read-data file-read-xattr{open})\n"));
            }
        }
        // …the multiplexers' socket folders (they sit inside writable temp ones), cargo's caches
        let keep: String = (0..self.keep.len()).map(|i| format!(" (subpath (param \"K{i}\"))")).collect();
        let caches: String = (0..self.caches.len()).map(|i| format!(" (subpath (param \"C{i}\"))")).collect();
        if !keep.is_empty() || !caches.is_empty() {
            p.push_str(&deny(&format!("file-write*{keep}{caches}")));
        }
        // …but cargo's own files, each that one file (its backup tags sit in the caches)
        let files: String = (0..self.files.len()).map(|i| format!(" (literal (param \"F{i}\"))")).collect();
        if !files.is_empty() {
            p.push_str(&format!("(allow file-write*{files})\n"));
        }
        // …and the mark cargo puts on its caches at every run, on its own folders themselves
        let mut xattrs: Vec<(&str, String)> = Vec::new();
        for c in &self.caches_said {
            let Some(x) = c.xattr.as_deref() else { continue };
            let at = (0..self.caches.len()).filter(|&i| c.dirs.contains(&self.caches[i]));
            let at: String = at.map(|i| format!(" (literal (param \"C{i}\"))")).collect();
            match xattrs.iter_mut().find(|(y, _)| *y == x) {
                Some((_, all)) => all.push_str(&at),
                None => xattrs.push((x, at)),
            }
        }
        for (x, caches) in xattrs.iter().filter(|(_, c)| !c.is_empty()) {
            p.push_str(&format!(
                "(allow file-write-xattr (require-all (require-any{caches}) (xattr \"{x}\")))\n"
            ));
        }
        p.push_str(&devices_rule());
        // The terminals: closed, read and write, and no pty made, but where the project's grant opens them
        // (`TERMINALS`, `/dev/ptmx`, srt's `allowPty`). After every file allow, so none opens them. Their ioctl
        // stays: it needs a descriptor, and a run has none of a terminal but the ptys its broker hands in
        // (`ptybroker` — always on, the user's decision 2026-10-19; Seatbelt checks an ioctl at every call, by
        // path: without it a pty handed in can't be sized or made a controlling terminal — measured)
        match self.terminals {
            true => p.push_str(&format!(
                "(allow pseudo-tty)\n(allow file-write* file-ioctl (literal \"/dev/ptmx\") (regex #\"{TERMINALS}\"))\n"
            )),
            false => {
                p.push_str(&deny(&format!("file-read-data file-write* (regex #\"{TERMINALS}\")")));
                // (and the masters', `/dev/ptmx` by its path: the e2e harness sizes its pty through it) — only
                // the commands a program on a pty uses (`TTY_IOCTLS`: measured, Seatbelt narrows by command), and
                // none for a run with a socket of the user's granted: a descriptor of one of the user's terminals
                // could come in over it (the review of terminals A) — that run gets no broker and asks the
                // terminals card as before
                if self.sockets.is_empty() {
                    let commands: String =
                        TTY_IOCTLS.iter().map(|(_, n)| format!(" (ioctl-command #x{n:x})")).collect();
                    p.push_str(&format!(
                        "(allow file-ioctl (require-all (require-any (literal \"/dev/ptmx\") (regex #\"{TERMINALS}\")) \
                         (require-any{commands})))\n"
                    ));
                }
            }
        }
        let open: String =
            self.ports.iter().chain(&proxied).map(|n| format!(" (remote ip \"localhost:{n}\")")).collect();
        // Listening stays open (a test's own server); connecting, only where it's allowed
        p.push_str("(allow network-bind network-inbound (local ip \"localhost:*\"))\n");
        if self.localhost {
            p.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        } else if !open.is_empty() {
            p.push_str(&format!("(allow network-outbound{open})\n"));
        }
        // Unix sockets — made or reached — only in the project and the run's own folder (`RunDir`), the
        // network open or not: the ones elsewhere are servers outside (tmux, zellij, ssh-agent). Last, so
        // these win (the later rule does — measured)
        let here = format!("(require-any (subpath (param \"ROOT\")){run})");
        p.push_str(&deny("network-bind network-inbound (local unix-socket)"));
        p.push_str(&deny("network-outbound (remote unix-socket)"));
        p.push_str(&format!(
            "(allow network-bind network-inbound (require-all (local unix-socket) {here}))\n\
             (allow network-outbound (require-all (remote unix-socket) {here}))\n"
        ));
        // …and the sockets the user granted: each that one file, connecting only (#119)
        let sockets: String =
            (0..self.sockets.len()).map(|i| format!(" (literal (param \"S{i}\"))")).collect();
        if !sockets.is_empty() {
            p.push_str(&format!(
                "(allow network-outbound (require-all (remote unix-socket) (require-any{sockets})))\n"
            ));
        }
        for rule in NO_WAY_OUT {
            p.push_str(&deny(rule));
        }
        p.push_str(&deny(&kept_services()));
        // Signals and processes' details only within the run's own (F2): `same-sandbox` is this `sandbox-exec`'s
        // processes — another run under the same profile isn't (spike M7.3, `target/sb/f2-spike.sh`). Its child,
        // group and grandchild stay signalable (`make -j`, a test runner ending its workers); tarae, another
        // session's run, the user's shell don't. (Arguments and environment aren't kept by these: spike 2)
        // (each denied here though the profile denies by default: its default doesn't cover a process's details —
        // `proc_pidinfo` of tarae's was let through under it, measured: `apart::two_sessions_cant_reach_each_
        // others_processes`)
        for rule in OWN_PROCESSES {
            p.push_str(&deny(rule));
            p.push_str(&format!("(allow {rule} (target same-sandbox))\n"));
        }
        // The settings that hold a token, each that file: the tools read their copies (`tokens.rs`)
        let hidden: String =
            (0..self.reads.copies.hidden.len()).map(|i| format!(" (literal (param \"T{i}\"))")).collect();
        if !hidden.is_empty() {
            p.push_str(&deny(&format!("file-read-data file-read-xattr{hidden}")));
        }
        // Last, so nothing granted opens them: what no run reads — but the project, when one holds it (a
        // checkout under `~/.claude/worktrees`), and tarae's log. (Their metadata stays open, as everywhere:
        // tarae's own tests look at its state folder — measured; a hard link needs the data's read anyway)
        let (around, other) = self.kept_split();
        let each = |v: &[usize]| v.iter().map(|i| format!(" (subpath (param \"X{i}\"))")).collect::<String>();
        if !other.is_empty() {
            p.push_str(&deny(&format!("file-read-data file-read-xattr{}", each(&other))));
        }
        if !around.is_empty() {
            p.push_str(&deny(&format!(
                "file-read-data file-read-xattr (require-all (require-any{}) (require-not (subpath (param \
                 \"ROOT\"))))",
                each(&around)
            )));
        }
        if self.reads.log.is_some() && !self.reads.kept.is_empty() {
            p.push_str("(allow file-read-data file-read-xattr (literal (param \"LOG\")))\n");
        }
        p
    }

    /// Linux, a served run: why it reaches nothing but this machine's loopback, where it doesn't — this
    /// machine's own addresses couldn't be read when the policy was made (`own_addresses`: `route` refuses every
    /// other address). macOS, the network open: the run has no port of its own to tarae's proxy, or the proxy
    /// can't tell this machine's own addresses from the network's. None elsewhere.
    pub fn loopback_only(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            self.own_addresses.is_none().then(|| {
                "tarae couldn't read this machine's own addresses, so it can't tell them from the network's and \
                 refuses every other address"
                    .into()
            })
        }
        #[cfg(not(target_os = "linux"))]
        match &self.proxy {
            _ if !self.network => None,
            None => Some("tarae couldn't open this run a port to its proxy, so nothing goes beyond it".into()),
            Some(r) if r.own_addresses.is_none() => Some(
                "tarae couldn't read this machine's own addresses, so its proxy can't tell them from the network's \
                 and reaches no address"
                    .into(),
            ),
            Some(_) => None,
        }
    }

    /// Linux: the tier its runs go at, `partial` below Landlock ABI 3 (`linux::Confine::tier`, its guarantees' —
    /// empty for a probe's rules); None elsewhere (Seatbelt has no tiers).
    pub fn tier_said(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        return Some(match &self.confine {
            Some(Ok(c)) => {
                format!("{}{}", c.tier(), if c.abi < 3 { " partial" } else { "" }).trim().to_string()
            }
            _ => String::new(),
        });
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// Linux: how the report says the network where it differs from macOS's words — tarae's supervisor
    /// serving: routed (`None`: `rules` says it); not serving here: no socket at all,
    /// this machine's ports and the project's unix sockets included, and why — and what Landlock can't keep
    /// (`linux.rs`). None elsewhere.
    pub fn linux_said(&self) -> Option<(Option<String>, String)> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            let asked = self.network || self.localhost || !self.ports.is_empty() || !self.sockets.is_empty();
            let net = c.serve.as_ref().err().map(|why| {
                format!(
                    ", and make no socket: no network, not even this machine's ports, and no unix socket — not \
                     even in the project or its own folder, which macOS allows (tarae can't serve them here: \
                     {why}){}",
                    if asked {
                        " — what llm.sandbox-network, -localhost, -ports or a socket grant open, too"
                    } else {
                        ""
                    }
                )
            });
            // (Q4: tarae's AppArmor profile would lend its runs what Ubuntu's restriction keeps from programs)
            let userns = match c.no_userns {
                true => {
                    " It can't make a user namespace (unshare, clone and clone3 are refused): tarae's AppArmor \
                     profile would let it, which Ubuntu keeps from other programs — a tool that needs one \
                     (rootless podman, bwrap, a browser's own sandbox) fails here; tell the user."
                }
                false => "",
            };
            let notes = format!(
                " On Linux a protected name it makes where none is yet isn't refused (the user is asked about \
                 it before your next run), and the terminal multiplexers' folders in /tmp stay \
                 writable.{userns}{}{}{}{}",
                linux::made_said(&c.made, &self.root),
                linux::kept_said(&c.kept, &self.root),
                linux::unread_said(&c.unread, &self.root),
                linux::covers_said(c)
            );
            return Some((net, notes));
        }
        None
    }

    /// What its runs are kept apart from and share (9a): Landlock's from the run's keeper (none for a probe's
    /// rules); Seatbelt's for a policy made for macOS. A policy made for Linux (`supervised`: `policy_computed`,
    /// `for_project`) whose rules aren't made — `confine` None, or Err: its run never starts (`spawn_as`) — says
    /// nothing, never Seatbelt's words. On Linux a policy made for macOS is only a test's (`Here.os`: the golden's).
    pub(super) fn guarantees(&self) -> Option<super::guarantees::Guarantees> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return c.guarantees.clone();
        }
        debug_assert!(
            cfg!(test) || cfg!(not(target_os = "linux")) || self.supervised,
            "a policy made for macOS on Linux"
        );
        (!self.supervised).then(|| super::seatbelt_guarantees(self))
    }

    /// Linux without Landlock's audit log: what a run was refused in files is read from its own error lines
    /// only, though its watch hears what the supervisor refused.
    pub(super) fn files_unwatched(&self) -> bool {
        #[cfg(target_os = "linux")]
        return matches!(&self.confine, Some(Ok(c)) if c.audit.is_err());
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// Linux: why what a run refused was read from its own error lines — why there's no audit log, or that
    /// the log didn't have it (Landlock's own rule for new entries isn't the policy's; a signal).
    pub(super) fn output_read_why(&self) -> String {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return match &c.audit {
                Ok(_) => "Landlock's audit log didn't have it".into(),
                Err(why) => why.clone(),
            };
        }
        String::new()
    }

    /// Linux: where Landlock's audit log is read here (`linux::AuditSource::said`) — None without one, and
    /// elsewhere.
    pub(super) fn audit_log(&self) -> Option<&'static str> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return c.audit.as_ref().ok().map(linux::AuditSource::said);
        }
        None
    }

    /// The report's words on what only the run's output says was refused (`o`, from `said_in_output`): tarae
    /// has no record of it, so it's the output's — the sandbox's report of a denial gets lost now and then, but
    /// such a line may be the command's own (a hit quoting the phrase, a refusal inside a container or a VM).
    /// `also`: after what tarae saw blocked. Empty when the output says nothing.
    pub(super) fn output_says(&self, o: &Blocked, also: bool) -> String {
        if o.total == 0 {
            return String::new();
        }
        let (what, was, it, these) = match o.distinct {
            1 => ("one thing".to_string(), "was", "it", "that's"),
            n => (format!("{n} things"), "were", "them", "those are"),
        };
        let mut s = format!(
            "Its output {}says {what} {was} refused where the sandbox's rules keep a command out — tarae has no \
             record of {it}, so {these} the output's words, not a refusal seen: ",
            if also { "also " } else { "" },
        );
        s.push_str(match self.denials_from() {
            // (Linux without the audit log: the error lines are all there is to go by)
            Some(_) => "likely the sandbox's, though a line may be the command's own:\n",
            None => {
                "the sandbox's report of a denial gets lost now and then, but the line may be the command's \
                 own (a hit quoting the phrase, a refusal inside a container or VM):\n"
            }
        });
        for d in &o.seen {
            s.push_str(&format!("  {}\n", d.reported()));
        }
        if o.distinct > o.seen.len() {
            s.push_str(&format!("  … and {} more\n", o.distinct - o.seen.len()));
        }
        if self.denials_from().is_some() {
            s.push_str(&format!(
                "(Read from its own error lines — {}, judged by the sandbox's rules.)\n",
                self.output_read_why()
            ));
        }
        s
    }

    /// Linux: where what a run refused is told from — Landlock's audit log, or the run's own error lines and
    /// why. None elsewhere (macOS's log, always).
    pub fn denials_from(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return Some(match &c.audit {
                Ok(_) => "from Landlock's audit log".into(),
                Err(why) => format!("from its own error lines — {why}"),
            });
        }
        None
    }

    /// The kept places (`Reads::kept`, by index) that hold the project, and the others.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
    pub(super) fn kept_split(&self) -> (Vec<usize>, Vec<usize>) {
        (0..self.reads.kept.len()).partition(|&i| self.root.starts_with(&self.reads.kept[i]))
    }

    /// Whether the profile lets a run read `real` — its rules, in Rust.
    pub fn reads_allow(&self, real: &Path) -> bool {
        let r = &self.reads;
        // (the log: opened after everything, when anything is kept)
        if r.log.as_deref() == Some(real) && !r.kept.is_empty() {
            return true;
        }
        if r.kept_holds(&self.root, real) || r.copies.instead_of(real).is_some() {
            return false;
        }
        if is_terminal(real) {
            return self.terminals;
        }
        let k = self.key_view();
        !matches!(k.rule(real), Some(KeyRule::Closed(_))) && k.reads_place(real)
    }

    /// What the rule for the files named like keys says of `real` (frictions29 A: one rule — the profile's,
    /// `reads_allow`, the report, the run's panel and `also` go by it). None: a run reads it as any file there.
    pub fn key_rule(&self, real: &Path) -> Option<KeyRule> {
        self.key_view().rule(real)
    }

    /// The key rule taken apart from the policy, for what's decided after its run on a worker (`KeyRules`).
    pub fn key_rules(&self) -> KeyRules {
        KeyRules {
            root: self.root.clone(),
            keys: self.keys.clone(),
            places: self.key_places.clone(),
            spared: self.key_spared.clone(),
            supervised: self.supervised,
            reads: self.reads.clone(),
            run: self.run.as_ref().map(|d| d.path().to_path_buf()),
            write: self.write.clone(),
            files: self.files.clone(),
            covered: self.covered().to_vec(),
        }
    }

    /// Linux: every file named like a key its run's mounts hid — the project's (7d) and those outside it (D-i) —
    /// which the run can't write, remove or rename (`sandbox::hidden_keys_said`). Real paths; none elsewhere.
    pub fn hidden_keys(&self) -> Vec<PathBuf> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return c.covers.keys.iter().chain(&c.covers.loose).map(|k| k.path.clone()).collect();
        }
        Vec::new()
    }

    /// Linux: the files named like keys outside the project its run's mounts hid (tier own, `linux::Outside`, D-i).
    fn covered(&self) -> &[PathBuf] {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return &c.outside.covered;
        }
        &[]
    }

    fn key_view(&self) -> KeyView<'_> {
        KeyView {
            root: &self.root,
            keys: self.keys.as_deref(),
            places: &self.key_places,
            spared: &self.key_spared,
            supervised: self.supervised,
            reads: &self.reads,
            run: self.run.as_ref().map(|d| d.path()),
            write: &self.write,
            files: &self.files,
            covered: self.covered(),
        }
    }

    /// Every place a run under this policy may write, folders and files — what its rules open, from the fields
    /// the profile (`args`) and Landlock's rules (`linux::confine`) are made from: the project, `write`, `files`,
    /// a run's own folder (any run's: where `RunDir` makes them — `runs`), the devices (`DEVICE_PLACES`).
    /// What's kept inside them (`protect`, `keep`, `caches`) counts as written too. Real paths.
    pub fn write_places(&self) -> Vec<PathBuf> {
        let run = self.run.as_ref().map(|r| r.path().to_path_buf());
        let mut all: Vec<PathBuf> = std::iter::once(self.root.clone())
            .chain(self.write.iter().cloned())
            .chain(self.files.iter().cloned())
            .chain(self.runs.clone())
            .chain(run)
            .chain(DEVICE_PLACES.iter().map(PathBuf::from))
            .collect();
        crate::sorting::sort(&mut all);
        all.dedup();
        all
    }

    /// Whether the profile lets a run write `real` — its rules, in Rust (the devices aside).
    pub fn writes_allow(&self, real: &Path) -> bool {
        if self.files.iter().any(|f| f == real) || is_terminal(real) && self.terminals {
            return true;
        }
        if real.starts_with(&self.root) {
            return !protected_in(&self.root, real) && !self.protect.iter().any(|p| real.starts_with(p));
        }
        let open = (self.own_shm && real.starts_with(Path::new("/dev/shm")))
            || self.run.as_ref().is_some_and(|d| real.starts_with(d.path()))
            || self.write.iter().any(|w| real.starts_with(w));
        open && !self.keep.iter().chain(&self.caches).any(|k| real.starts_with(k))
    }

    /// What the run's output says was refused where the profile denies it — the kernel's own report of a
    /// denial is lost now and then, at random (1 in 10, measured, `target/sb/p2-measured.md` §5): a path on
    /// a line that failed, at or just before a line saying `Operation not permitted` (cargo says the path,
    /// then `Caused by:`, then the error), that this profile doesn't let a run read or write. Those `seen`
    /// already said aren't again. Relative paths are from where the run is (`from`; `paths_in`: no disk).
    /// A name relative to where the run is counts too — `sh`'s `cannot touch 'Makefile'`, Go's `open fx`
    /// (`named_in`): on Linux, with no audit log to read, the output is all there is, and a protected name refused
    /// by its relative name was said nowhere (compat, `touch Makefile`, `go build`: "the report names no cause").
    pub fn said_in_output(&self, output: &[(bool, String)], seen: &[Denial]) -> Vec<Denial> {
        const BEFORE: usize = 3;
        let refused = |l: &str| refusal(l);
        let failed = |l: &str| {
            let l = l.to_ascii_lowercase();
            let words = [
                "not permitted",
                "permission denied",
                "read-only file system",
                "resource busy",
                "fail",
                "error",
                "cannot",
                "can't",
                "couldn't",
                "unable",
            ];
            words.iter().any(|w| l.contains(w))
        };
        let mut near = vec![false; output.len()];
        // (Landlock's own: "Permission denied" at or just after it — cargo says the path, then why)
        let mut denied = vec![false; output.len()];
        for (i, (_, l)) in output.iter().enumerate().filter(|(_, (_, l))| refused(l)) {
            near[i.saturating_sub(BEFORE)..=i].fill(true);
            if l.to_ascii_lowercase().contains("permission denied") {
                denied[i.saturating_sub(BEFORE)..=i].fill(true);
            }
        }
        let mut out: Vec<Denial> = Vec::new();
        let lines = output.iter().zip(near.iter().zip(&denied)).filter(|(_, (n, _))| **n);
        for (l, denied) in lines.map(|((_, l), (_, d))| (l, *d)).filter(|(l, _)| failed(l)) {
            let named = named_in(l, self.from()).into_iter().chain(program_named_in(l, self.from()));
            for real in paths_in(l, self.from()).into_iter().chain(named) {
                let op = match (self.reads_allow(&real), self.writes_allow(&real)) {
                    (true, true) => continue,
                    (true, false) => "file-write*",
                    // (a file named like a key in the project — `key_named_in`)
                    (false, true) => "file-read-data",
                    (false, false) => EITHER,
                };
                if seen.iter().chain(&out).any(|d| d.file() == Some(real.as_path())) {
                    continue;
                }
                let target = real.display().to_string();
                out.push(Denial { process: SEEN_IN_OUTPUT.into(), op: op.into(), target });
            }
            for d in self.landlock_said_in(l, denied) {
                if !seen.iter().chain(&out).any(|s| s.op == d.op && s.target == d.target) {
                    out.push(d);
                }
            }
        }
        let rules = &crate::sandboxpack::get().refused_connect;
        let rules: Vec<_> = rules
            .iter()
            .filter(|r| r.when.as_ref().is_none_or(|w| output.iter().any(|(_, l)| w.is_match(l.trim_end()))))
            .collect();
        for (_, l) in output {
            for d in self.connect_said_in(&rules, l) {
                // (the kernel's names no address: one on the same port is this one)
                let port = |s: &Denial| s.target.rsplit_once(':').map(|(_, p)| p.to_string());
                if !seen.iter().chain(&out).any(|s| s.op == d.op && port(s) == port(&d)) {
                    out.push(d);
                }
            }
        }
        out.truncate(DENIALS_KEPT);
        out
    }

    /// A connect refused as `line` says it (the packs' `refused-connect` whose `when` the output holds: macOS's nc,
    /// Gradle's daemon), as the kernel's report would give it — its program, `network-outbound`,
    /// `remote:<host>:<port>` — where this run's rules refuse it: a port here closed (not every port open, nor that
    /// one), any host beyond this machine (only tarae's proxy is).
    fn connect_said_in(&self, rules: &[&crate::sandboxpack::RefusedConnect], line: &str) -> Vec<Denial> {
        let mut out = Vec::new();
        for r in rules {
            let Some(c) = r.line.captures(line.trim_end()) else { continue };
            let host = match &c["host"] {
                h if h.contains(':') => format!("[{h}]"),
                h => h.to_string(),
            };
            let target = format!("remote:{host}:{}", &c["port"]);
            let process = r.process.clone().unwrap_or_else(|| c["process"].to_string());
            let d = Denial { process, op: "network-outbound".into(), target };
            let open = d.port_here().is_some_and(|n| self.localhost || self.ports.contains(&n));
            if !open {
                out.push(d);
            }
        }
        out
    }

    /// `said` (what the output says, `said_in_output`) put with `seen` (the kernel's): Landlock's record of a
    /// refused new file names only its folder (`file-make-in`) — the file the output names in that folder
    /// takes its place, the kernel's line kept; without one, the folder is said with Landlock's own rule
    /// where that's it (a folder holding protected names: `file-entry-make-in`), else as a write there. What
    /// of `said` is left.
    pub fn merge_made(&self, seen: &mut [Denial], mut said: Vec<Denial>) -> Vec<Denial> {
        for d in seen.iter_mut().filter(|d| d.op == "file-make-in") {
            let folder = PathBuf::from(&d.target);
            let named = said.iter().position(|s| {
                ["file-entry-make", "file-write*", EITHER].contains(&s.op.as_str())
                    && Path::new(&s.target).parent() == Some(folder.as_path())
            });
            match named {
                Some(i) => {
                    let s = said.remove(i);
                    (d.op, d.target) = (s.op, s.target);
                }
                None if self.closed_holds(&folder) => d.op = "file-entry-make-in".into(),
                None => d.op = "file-write*".into(),
            }
        }
        said
    }

    /// Where the run is: its folder (`cwd`), else the project's root.
    pub fn from(&self) -> &Path {
        self.cwd.as_deref().unwrap_or(&self.root)
    }

    /// Linux, tier own: whether the project's protected names are sealed by mounts for the run
    /// (`linux::Covers::seals`).
    pub(super) fn sealed(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return !c.covers.seals.is_empty();
        }
        false
    }

    /// Linux: whether `folder` is one of the project's that take no new entry (`linux::Confine::closed`).
    pub(super) fn closed_holds(&self, folder: &Path) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            return c.closed.iter().any(|f| f == folder);
        }
        let _ = folder;
        false
    }

    /// Linux: what a line of output says Landlock refused though the rules above let it write — making,
    /// removing or moving an entry of a folder that holds a protected name, or one below it
    /// (`linux::Confine::closed`; "Permission denied" on it or, `denied`, just after it as cargo says it —
    /// by a path, or a name from where the run is, as `sh` says it); from ABI 6, a signal to a process
    /// outside the run's own ("Operation not permitted" from `kill`). None elsewhere.
    pub(super) fn landlock_said_in(&self, line: &str, denied: bool) -> Vec<Denial> {
        #[cfg(target_os = "linux")]
        if let Some(Ok(c)) = &self.confine {
            let low = line.to_ascii_lowercase();
            let mut out: Vec<Denial> = Vec::new();
            if denied || low.contains("permission denied") {
                let op = match () {
                    _ if low.contains("remove") || low.contains("unlink") => "file-entry-remove",
                    _ if low.contains("move") || low.contains("rename") => "file-entry-move",
                    _ => "file-entry-make",
                };
                for real in paths_in(line, self.from()).into_iter().chain(named_in(line, self.from())) {
                    let closed = real.parent().is_some_and(|d| c.closed.iter().any(|f| f == d));
                    let target = real.display().to_string();
                    if closed && self.writes_allow(&real) && !out.iter().any(|d| d.target == target) {
                        out.push(Denial { process: SEEN_IN_OUTPUT.into(), op: op.into(), target });
                    }
                }
            }
            if c.abi >= 6 && low.contains("kill") && low.contains("not permitted") {
                let pid = line
                    .split(['(', ')'])
                    .nth(1)
                    .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
                out.push(Denial {
                    process: SEEN_IN_OUTPUT.into(),
                    op: "signal".into(),
                    target: pid.unwrap_or_default().into(),
                });
            }
            return out;
        }
        let _ = (line, denied);
        Vec::new()
    }

    /// The profile's parameters (name, value).
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
    pub fn params(&self) -> Vec<(String, String)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut v = vec![("ROOT".to_string(), s(&self.root))];
        v.extend(self.run.iter().map(|r| ("RUN".to_string(), s(r.path()))));
        v.extend(self.protect.iter().enumerate().map(|(i, p)| (format!("P{i}"), s(p))));
        v.extend(self.write.iter().enumerate().map(|(i, p)| (format!("W{i}"), s(p))));
        v.extend(self.files.iter().enumerate().map(|(i, p)| (format!("F{i}"), s(p))));
        v.extend(self.keep.iter().enumerate().map(|(i, p)| (format!("K{i}"), s(p))));
        v.extend(self.caches.iter().enumerate().map(|(i, p)| (format!("C{i}"), s(p))));
        v.extend(self.sockets.iter().enumerate().map(|(i, p)| (format!("S{i}"), s(p))));
        v.extend(self.keys.iter().flatten().enumerate().map(|(i, p)| (format!("Y{i}"), s(p))));
        let places = self.key_places.iter().filter(|_| self.keys.is_some() && !self.supervised);
        v.extend(places.enumerate().map(|(i, p)| (format!("KP{i}"), s(p))));
        let spared = self.key_spared.iter().filter(|_| self.keys.is_some() && !self.supervised);
        v.extend(spared.enumerate().map(|(i, p)| (format!("KI{i}"), s(p))));
        let r = &self.reads;
        v.extend(r.home.iter().map(|h| ("HOME".to_string(), s(h))));
        v.extend(
            r.home.iter().flat_map(|_| r.allow.iter().enumerate().map(|(i, p)| (format!("R{i}"), s(p)))),
        );
        v.extend(r.kept.iter().enumerate().map(|(i, p)| (format!("X{i}"), s(p))));
        v.extend(r.copies.hidden.iter().enumerate().map(|(i, (p, _))| (format!("T{i}"), s(p))));
        v.extend(r.log.iter().filter(|_| !r.kept.is_empty()).map(|l| ("LOG".to_string(), s(l))));
        v
    }

    /// `sandbox-exec`'s arguments before the command.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
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
        // (`.git`, `.tarae.toml` and the rest are names the profile keeps itself: `protected_names`)
        let mut protect = Vec::new();
        for p in &env.protect {
            let rel = Path::new(p);
            // Inside the project only (`..` would reach out of it)
            if rel.is_relative() && rel.components().all(|c| matches!(c, Component::Normal(_))) {
                protect.push(root.join(rel));
            }
        }
        let mut write = vec![PathBuf::from(TMP)];
        if let Some(t) = &env.tmp {
            write.push(user_temp(t));
        }
        // (the files named like keys closed there as in the project — the temp folders too, read and written by
        // every session's runs; a run's own `TMPDIR` is its `RunDir`, not among them)
        let mut key_places: Vec<PathBuf> = env.key_places.iter().chain(&write).cloned().collect();
        // The folders the packs' `ask` names: the tool's own — but never where nothing is written, a shared cache
        // or a toolchain's home among it; its caches (Go's module cache) as the others, read — each judged but
        // for its own places, which the caches rule keeps (another toolchain's cache or home still refuses it)
        let mut refused_asked = Vec::new();
        let home = env.home.as_deref();
        let open = |d: &Path, which: &str, later: &RunsLater, refused: &mut Vec<(PathBuf, String)>| {
            let verdict = ask_dir_verdict(d, &root, home, later);
            verdict.map_err(|why| refused.push((d.to_path_buf(), format!("{which} — {why}")))).is_ok()
        };
        for (d, which) in &env.dirs {
            if open(d, which, &env.runs_later, &mut refused_asked) {
                write.push(d.clone());
            }
        }
        let mut reads = env.reads.clone();
        let mut caches_said = env.caches.clone();
        for (key, which, c) in &env.asked_caches {
            let but = env.runs_later.but_cache(key);
            if c.dirs.iter().all(|d| open(d, which, &but, &mut refused_asked)) {
                reads.allow.extend(
                    c.dirs.iter().filter(|d| reads.home.as_ref().is_some_and(|h| d.starts_with(h))).cloned(),
                );
                caches_said.push(c.clone());
            }
        }
        // The tools' caches: written as they download and unpack — with the network closed, kept (`caches`)
        let caches: Vec<PathBuf> = caches_said.iter().flat_map(|c| c.dirs.iter().cloned()).collect();
        if env.network {
            write.extend(caches.iter().cloned());
        }
        // The tools' homes kept for the project's runs (Go's build cache)
        write.extend(env.kept_homes.iter().map(|(_, h)| h.clone()));
        // The folders the checkout's build tool names as its config says: only its own (#20)
        let mut refused = Vec::new();
        for (d, which) in &env.asked {
            let (home, own) = (env.home.as_deref(), env.own_build.as_deref());
            match asked_dir_verdict(d, &root, home, &env.builder, own, env.tmp.as_deref()) {
                Ok(true) => write.push(d.clone()),
                Ok(false) => {}
                Err(why) if !refused.iter().any(|(r, _)| r == d) => {
                    refused.push((d.clone(), format!("{}'s {which} — {why}", env.builder.who)))
                }
                Err(_) => {}
            }
        }
        let mut user_writes = Vec::new();
        for w in &env.write {
            let p = match (w.strip_prefix("~/"), &env.home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => root.join(w),
            };
            write.push(profile_path(&p));
            user_writes.push(profile_path(&p));
        }
        // Never the project or what holds it (that would open .git again); inside it, the project's rule
        write.retain(|w| !root.starts_with(w) && !w.starts_with(&root));
        write.dedup();
        // (a folder the user opened for writing — `llm.sandbox-write`, a write grant — is read too: its keys closed;
        // only as it's opened: not one holding the project, which isn't, nor `/`)
        key_places.extend(user_writes.into_iter().filter(|w| write.contains(w)));
        // (a read place holding the project stays — a granted ancestor, the main checkout of a nested worktree: its
        // rule leaves the project to the project's own; one in the project is the project's)
        // (what's spared — an install the packs name — is never one, and is left out of those holding it; only
        // what one holds is kept: the profile's parameters. `$TMPDIR` isn't: every program of the user's writes
        // there — a run's own is its `RunDir`)
        let mut key_spared = env.key_spared.clone();
        key_places.retain(|k| !k.starts_with(&root) && !key_spared.iter().any(|s| k.starts_with(s)));
        crate::sorting::sort(&mut key_places);
        key_places.dedup();
        key_spared.retain(|s| key_places.iter().any(|k| s.starts_with(k)));
        crate::sorting::sort(&mut key_spared);
        key_spared.dedup();
        let mut ports: Vec<u16> = env.ports.clone();
        crate::sorting::sort(&mut ports);
        ports.dedup();
        ports.truncate(PORTS_MAX);
        let session = proxy::session_of(&root);
        Policy {
            root,
            protect,
            write,
            files: env.files.clone(),
            keep: mux_dirs(env),
            caches: if env.network { Vec::new() } else { caches },
            caches_said: if env.network { Vec::new() } else { caches_said },
            network: env.network || env.asks_hosts,
            asks_hosts: !env.network && env.asks_hosts,
            ports,
            localhost: env.localhost,
            run: None,
            cwd: None,
            runs: env.runs.clone(),
            tag: None,
            proxy: None,
            supervised: Os::HOST == Os::Linux,
            // (with the network off no host is reached unasked, whatever the list says — each host asked, the
            // user's yes goes out the way tarae itself does)
            egress: proxy::egress::Egress {
                any: env.network && env.hosts.is_none(),
                hosts: if env.network { env.hosts.clone().unwrap_or_default() } else { Vec::new() },
                closed: !env.network && env.asks_hosts,
                upstream: match env.network || env.asks_hosts {
                    true => env.upstream.clone(),
                    false => Default::default(),
                },
                ..Default::default()
            },
            run_id: next_run(),
            session,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            terminals: false,
            keys: Some(Vec::new()),
            key_places,
            key_spared,
            refused,
            refused_asked,
            named_by: env.builder.who.clone(),
            reads,
            env: Vec::new(),
            disk_stores: env.disk_stores.clone(),
            run_env: env.run_env.clone(),
            run_homes: env.run_homes.clone(),
            kept: Vec::new(),
            #[cfg(target_os = "linux")]
            resolvers: env.resolvers.clone(),
            #[cfg(target_os = "linux")]
            own_addresses: env.own_addresses.clone(),
            #[cfg(all(test, target_os = "linux"))]
            swap_pause: None,
            #[cfg(all(test, target_os = "linux"))]
            seals_off: false,
            #[cfg(target_os = "linux")]
            confine: None,
            own_shm: false,
        }
    }
}

#[cfg(test)]
mod tests;
