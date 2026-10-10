//! What a sandboxed command may do (`Policy`), and how each system is told it: Seatbelt's profile and its
//! parameters, Landlock's report lines, what a run's output says it was refused, and what the result says.

use super::*;

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
    /// The network beyond this machine (`llm.sandbox-network`).
    pub network: bool,
    /// Ports on this machine it may connect to (`llm.sandbox-ports`): the network closed, the only ones;
    /// open, the risky ones listed here stay reachable.
    pub ports: Vec<u16>,
    /// Every port on this machine, the risky ones too (`llm.sandbox-localhost`).
    pub localhost: bool,
    /// This run's own temp folder — its `TMPDIR`, and with the project the only place it may make or reach
    /// a unix socket (`RunDir`).
    pub run: Option<std::sync::Arc<RunDir>>,
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
    /// Linux (6e′): its keeper's session network takes its runs' connections and answers their names (TPROXY, A+/A
    /// — `tier::redirects`, as `Here.redirects` says): no proxy URL, so no proxy variable nor the packs' proxy
    /// lines — a tool goes out as it would anywhere. Made for the `Here` (a stand-in's: false), as `supervised`.
    pub redirected: bool,
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
    /// Linux: the policy as Landlock's rules (`linux::confine`, made on the worker) — Err why it couldn't be
    /// (the run then doesn't start: `spawn`); None before it's made (`policy` makes it).
    #[cfg(target_os = "linux")]
    pub confine: Option<Result<linux::Confine, String>>,
}

impl Policy {
    /// Its run's proxy URL — the one thing the packs' proxy lines and the proxy variables hang on (step 5f): a
    /// port and token of the run's own where it has one (`proxy`: macOS), else, where its supervisor hands its
    /// connections over (`supervised`: Linux), that address (`route::PROXY` — no credential: the supervisor
    /// knows the run). None: the network closed, or no way to the proxy was made — nothing goes beyond this
    /// machine; or the session's network takes its connections (`redirected`, 6e′ — M5: the tools go out without
    /// one). (Step 6f: a run's own one in tier C.)
    pub fn proxy_url(&self) -> Option<proxy::ProxyUrl> {
        match (&self.proxy, self.supervised) {
            _ if !self.network || self.redirected => None,
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

    /// The Seatbelt profile. Paths aren't in it: they're `-D` parameters (`params`), so no path can change
    /// what it says. Every deny carries the run's tag (`tag`) — the only text from outside, and tarae's own.
    #[cfg_attr(target_os = "linux", allow(dead_code))] // (Seatbelt's — its tests run everywhere)
    pub fn profile(&self) -> String {
        let with = self.tag.as_ref().map(|t| format!(" (with message \"{t}\")")).unwrap_or_default();
        let deny = |rule: &str| format!("(deny {rule}{with})\n");
        let mut p = String::from("(version 1)\n(allow default)\n");
        // (A hard link needs these write rules to allow its source too — Seatbelt's own
        // `forbidden-link-priv<file-write*>`, measured 2026-10-04, `target/sb/p1-links2.sh` — so a run can't
        // link a file from outside into the project, nor give a kept one a second name to write through: no
        // `file-link` rule of its own is needed)
        p.push_str(&deny("file-write*"));
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
        }
        // After the allows, so these win. The names a run keeps off, below the project's root however deep
        // — the regex starts with as many path parts as the root has (no path in it), so a name in the
        // root's own path (a checkout under `.claude/worktrees`) doesn't close the whole project
        let depth = self.root.components().filter(|c| matches!(c, Component::Normal(_))).count();
        let below = format!("^{}/(.*/)?", "/[^/]+".repeat(depth));
        let outputs = outputs().iter().map(|o| as_written(o)).collect::<Vec<_>>().join("|");
        let (mut named, mut in_output) = (String::new(), String::new());
        for names in name_regexes() {
            named.push_str(&format!(" (regex #\"{below}({names})(/|$)\")"));
            in_output.push_str(&format!(" (regex #\"{below}({outputs})/(.*/)?({names})(/|$)\")"));
        }
        p.push_str(&deny(&format!(
            "file-write* (require-all (subpath (param \"ROOT\")) (require-any{named}) \
             (require-not (require-any{in_output})))"
        )));
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
        // The network open goes beyond this machine only through tarae's proxy, at the run's own port (step 5d:
        // another run's isn't reached — M3d); the port is a number tarae made, never text from outside
        let proxied = self.proxy.as_ref().filter(|_| self.network).map(|r| r.port);
        let open: String =
            self.ports.iter().chain(&proxied).map(|n| format!(" (remote ip \"localhost:{n}\")")).collect();
        // Listening stays open (a test's own server); connecting, only where it's allowed
        p.push_str(&deny("network*"));
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

    /// Linux: the tier its runs go at, `partial` below Landlock ABI 3 (`linux::Confine.tier`, as `confine`
    /// decided it — empty for a probe's rules); None elsewhere (Seatbelt has no tiers).
    pub fn tier_said(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        return Some(match &self.confine {
            Some(Ok(c)) => {
                format!("{}{}", c.tier, if c.abi < 3 { " partial" } else { "" }).trim().to_string()
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
                 writable.{userns}{}{}{}",
                linux::notes(c.abi, c.ns),
                linux::made_said(&c.made, &self.root),
                linux::kept_said(&c.kept, &self.root)
            );
            return Some((net, notes));
        }
        None
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
        match &r.home {
            Some(home) if real.starts_with(home) => {
                real.starts_with(&self.root)
                    || self.run.as_ref().is_some_and(|d| real.starts_with(d.path()))
                    || self.write.iter().chain(&r.allow).any(|w| real.starts_with(w))
                    || self.files.iter().any(|f| f == real)
            }
            _ => true,
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
        if self.files.iter().any(|f| f == real) {
            return true;
        }
        if real.starts_with(&self.root) {
            return !protected_in(&self.root, real) && !self.protect.iter().any(|p| real.starts_with(p));
        }
        let open = self.run.as_ref().is_some_and(|d| real.starts_with(d.path()))
            || self.write.iter().any(|w| real.starts_with(w));
        open && !self.keep.iter().chain(&self.caches).any(|k| real.starts_with(k))
    }

    /// What the run's output says was refused where the profile denies it — the kernel's own report of a
    /// denial is lost now and then, at random (1 in 10, measured, `target/sb/p2-measured.md` §5): a path on
    /// a line that failed, at or just before a line saying `Operation not permitted` (cargo says the path,
    /// then `Caused by:`, then the error), that this profile doesn't let a run read or write. Those `seen`
    /// already said aren't again. Relative paths are from the project (`paths_in`: no disk).
    pub fn said_in_output(&self, output: &[(bool, String)], seen: &[Denial]) -> Vec<Denial> {
        const BEFORE: usize = 3;
        let refused = |l: &str| l.contains("ermi") && refusal(l);
        let failed = |l: &str| {
            let l = l.to_ascii_lowercase();
            let words = [
                "not permitted",
                "permission denied",
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
            for real in paths_in(l, &self.root) {
                let op = match (self.reads_allow(&real), self.writes_allow(&real)) {
                    (true, true) => continue,
                    (true, false) => "file-write*",
                    (false, _) => EITHER,
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
        out.truncate(DENIALS_KEPT);
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
                for real in paths_in(line, &self.root).into_iter().chain(named_in(line, &self.root)) {
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
        for w in &env.write {
            let p = match (w.strip_prefix("~/"), &env.home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => root.join(w),
            };
            write.push(profile_path(&p));
        }
        // Never the project or what holds it (that would open .git again); inside it, the project's rule
        write.retain(|w| !root.starts_with(w) && !w.starts_with(&root));
        write.dedup();
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
            network: env.network,
            ports,
            localhost: env.localhost,
            run: None,
            runs: env.runs.clone(),
            tag: None,
            proxy: None,
            supervised: Os::HOST == Os::Linux,
            redirected: false,
            // (with the network off no host is reached, whatever the list says)
            egress: proxy::egress::Egress {
                any: env.network && env.hosts.is_none(),
                hosts: if env.network { env.hosts.clone().unwrap_or_default() } else { Vec::new() },
                upstream: if env.network { env.upstream.clone() } else { Default::default() },
                ..Default::default()
            },
            run_id: next_run(),
            session,
            watch: None,
            granted: Vec::new(),
            sockets: Vec::new(),
            refused,
            refused_asked,
            named_by: env.builder.who.clone(),
            reads,
            env: Vec::new(),
            run_env: env.run_env.clone(),
            run_homes: env.run_homes.clone(),
            kept: Vec::new(),
            #[cfg(target_os = "linux")]
            resolvers: env.resolvers.clone(),
            #[cfg(target_os = "linux")]
            own_addresses: env.own_addresses.clone(),
            #[cfg(all(test, target_os = "linux"))]
            swap_pause: None,
            #[cfg(target_os = "linux")]
            confine: None,
        }
    }
}
