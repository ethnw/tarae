//! What the sandbox makes of a stand-in home, pinned (`src/snapshots/sandbox_*.txt`, `TARAE_BLESS=1` rewrites
//! them): the profile and its parameters, the policy each system's rules are made from, Landlock's rules, where
//! a run writes, what Claude reads in a run's report, and the words its prompt says of the sandbox. Made from a
//! home of the test's own (`Fixture`) and what `Here` says of it — never the user's — so the same text comes
//! out on any machine; what isn't the fixture's (the run's folder, the temp folders, tarae's state) is named
//! by a `$NAME`.

use std::path::{Path, PathBuf};

use super::*;

/// A stand-in home with what the toolchains keep there, a project in it, and what `Here` says of them.
struct Fixture {
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
    here: Here,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

/// Every token here is a fake one (`tarae-fake-token`), in the test's own folder.
const FILES: [(&str, &str); 16] = [
    ("Library/Application Support/go/env", "GOPROXY=https://u:tarae-fake-token@example.invalid\n"),
    (".config/go/env", "GOPROXY=https://u:tarae-fake-token@example.invalid\n"),
    (".cargo/registry/src/index/x-1.0/lib.rs", ""),
    (".cargo/git/checkouts/y/z", ""),
    (".cargo/config.toml", "[registries.x]\ntoken = \"tarae-fake-token-cargo-0000\"\n"),
    (".cargo/credentials.toml", "[registry]\ntoken = \"tarae-fake-token-cred-0000\"\n"),
    (".rustup/toolchains/stable-x/lib/rustlib/src/rust/library/core/lib.rs", ""),
    (".gradle/wrapper/dists/gradle-8.10-bin/abc123/gradle-8.10-bin.zip", ""),
    (".gradle/caches/modules-2/files-2.1/x", ""),
    (".gradle/daemon/8.10/registry.bin", ""),
    (".m2/repository/x", ""),
    (".npmrc", "//registry.npmjs.org/:_authToken=tarae-fake-token-npm-00000000\n"),
    (
        ".gitconfig",
        "[user]\n\tname = x\n[http]\n\textraHeader = Authorization: Bearer tarae-fake-token-git-0000\n",
    ),
    (".config/pip/pip.conf", "[global]\nindex-url = https://u:tarae-fake-token@example.invalid/simple\n"),
    (".ssh/id_x", "not a key\n"),
    ("work/.cargo/config.toml", "[build]\njobs = 2\n"),
];

const FOLDERS: [&str; 7] = [
    "Library/Application Support/go/telemetry",
    "Library/Caches/go-build",
    ".cache/go-build",
    "gocache",
    "go/pkg/mod",
    ".cargo/build/ab/cdef0123",
    "elsewhere",
];

impl Fixture {
    /// A fresh one (`name` keeps tests apart), the homes at their defaults; `env` adds to what `Here` says.
    fn new(name: &str, os: Os, env: &[(&str, &str)]) -> Fixture {
        // (never in the shared temp folder — Linux's is `/tmp`: a run writes all of it, so the fixture's own
        // places would fold into it)
        let tmp = std::env::temp_dir();
        let tmp = if tmp.starts_with(TMP) { PathBuf::from("/var/tmp") } else { tmp };
        let base = tmp.join(format!("tarae-golden-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let home = base.join("home");
        let root = home.join("work/proj");
        for (rel, text) in FILES {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        for rel in FOLDERS {
            std::fs::create_dir_all(home.join(rel)).unwrap();
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        std::fs::write(root.join("go.mod"), "module p\n").unwrap();
        for d in ["cargo-moved/registry", "gradle-moved/daemon", "rustup-moved/toolchains"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let mut vars: HashMap<String, std::ffi::OsString> = HashMap::new();
        vars.insert("HOME".into(), home.clone().into());
        for (k, v) in env {
            let v = v.replace("$BASE", &base.display().to_string());
            vars.insert(k.to_string(), v.into());
        }
        let here = Here { env: vars, tmp: Some(base.join("tmp")), uid: 501, os, ..Here::default() };
        // What the toolchain would have said (no cargo or go is run): Go's module cache, the user's Go build
        // cache moved into home (kept from runs), cargo's target dir in the checkout, a folder in home it names
        // (kept closed — #20), and this checkout's own build dir
        let own = home.join(".cargo/build/ab/cdef0123");
        let named = vec![
            (root.join("target"), "target-dir".into()),
            (home.join("elsewhere"), "target-dir".into()),
            (own.clone(), "build-dir".into()),
        ];
        let found = ToolDirs {
            other: Vec::new(),
            caches: vec![(home.join("go/pkg/mod"), "GOMODCACHE".into(), "go's GOMODCACHE".into())],
            kept: vec![home.join("gocache")],
            named,
            own_build: Some(own),
        };
        toolchain_found_in(&root, found, &here);
        Fixture { base, home, root, here }
    }

    /// `text` with what isn't the fixture's own named: the run's folder and tag, the project, the stand-in home,
    /// the fixture's folder, the shared temp folder, tarae's state.
    fn named(&self, text: &str, run: Option<&RunDir>) -> String {
        let mut names: Vec<(String, &str)> = vec![
            // (the project's kept homes are named by its path: the fixture's, a new one each time)
            (project_key(&self.root), "$KEY"),
            (self.root.display().to_string(), "$ROOT"),
            (self.home.display().to_string(), "$HOME"),
            (self.base.display().to_string(), "$BASE"),
            (TMP.to_string(), "$TMP"),
        ];
        if let Some(r) = run {
            names.push((r.path().display().to_string(), "$RUN"));
            names.push((r.tag(), "$TAG"));
        }
        if let Some(s) = crate::config::state_dir() {
            names.push((profile_path(&s).display().to_string(), "$STATE"));
        }
        // (each as the report shows it too: a fixture in a temp folder under home — a sandboxed run's own,
        // in tarae's cache since F1 — is said `~/…`, `home_shown`)
        let shown: Vec<(String, &str)> = names
            .iter()
            .map(|(from, to)| (home_shown(from), *to))
            .filter(|(from, _)| from.starts_with('~'))
            .collect();
        names.extend(shown);
        // (longest first: the project is in the home, the home in the fixture's folder)
        names.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        let mut out = text.to_string();
        for (from, to) in names {
            out = replace_whole(&out, &from, to);
        }
        out
    }

    /// A run's (`run_policy_in`: its own folder in the stand-in home's cache).
    fn policy(&self, llm: &crate::llm::LlmConfig) -> Policy {
        run_policy_in(&self.root, llm, &[], &self.here).expect("a run's policy")
    }
}

/// `text` with `from` as `to` where it's a whole path's start — not inside a name (`$BASE/tmp` keeps its
/// `/tmp`, `TMP` on Linux) — and ends with a part (`/tmp` isn't `/tmpfoo`'s).
fn replace_whole(text: &str, from: &str, to: &str) -> String {
    let in_name = |c: char| c.is_alphanumeric() || "_.-$".contains(c);
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(from) {
        out.push_str(&rest[..i]);
        let before = out.chars().next_back();
        let after = rest[i + from.len()..].chars().next();
        if before.is_some_and(in_name) || after.is_some_and(|c| in_name(c) && c != '.') {
            out.push_str(from);
        } else {
            out.push_str(to);
        }
        rest = &rest[i + from.len()..];
    }
    out.push_str(rest);
    out
}

/// Compared with `src/snapshots/<name>`, or written there with `TARAE_BLESS=1`.
fn pinned(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots").join(name);
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    if expected != actual {
        let first = expected.lines().zip(actual.lines()).position(|(a, b)| a != b);
        panic!(
            "{} differs (first at line {}) — if intended: TARAE_BLESS=1 cargo test sandbox::golden\n\n{actual}",
            path.display(),
            first.map_or("past the shorter's end".into(), |l| (l + 1).to_string()),
        );
    }
}

/// Paths, sorted, one a line.
fn lines(f: &Fixture, run: Option<&RunDir>, paths: impl IntoIterator<Item = String>) -> String {
    let mut v: Vec<String> = paths.into_iter().map(|p| f.named(&p, run)).collect();
    v.sort();
    v.iter().map(|p| format!("    {p}\n")).collect()
}

fn shown(p: &Path) -> String {
    p.display().to_string()
}

/// The profile byte for byte, its parameters as a set per letter (the profile names them by index, so their
/// order is the packs' reading order's), and the policy's fields every system's rules are made from.
fn policy_said(f: &Fixture, p: &Policy) -> String {
    let run = p.run.as_deref();
    let mut s = String::from("profile:\n");
    // (the names' regexes start with as many path parts as the project's path has: the machine's)
    let depth = p.root.components().filter(|c| matches!(c, Component::Normal(_))).count();
    let mut profile = p.profile().replace(&format!("^{}/", "/[^/]+".repeat(depth)), "^$ROOT-PARTS/");
    // (macOS, the network open: the run's own port to tarae's proxy, a new one each time)
    if let Some(r) = &p.proxy {
        profile = profile.replace(&format!("\"localhost:{}\"", r.port), "\"localhost:$PROXY\"");
    }
    s.push_str(&f.named(&profile, run));
    s.push_str("params:\n");
    let mut by: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (k, v) in p.params() {
        let letter: String = k.chars().take_while(|c| !c.is_ascii_digit()).collect();
        by.entry(letter).or_default().push(v);
    }
    for (letter, values) in by {
        s.push_str(&format!("  {letter}:\n{}", lines(f, run, values)));
    }
    let r = &p.reads;
    let fields: [(&str, Vec<String>); 15] = [
        ("write", p.write.iter().map(|w| shown(w)).collect()),
        ("files", p.files.iter().map(|w| shown(w)).collect()),
        ("keep", p.keep.iter().map(|w| shown(w)).collect()),
        ("caches", p.caches.iter().map(|w| shown(w)).collect()),
        ("refused", p.refused.iter().map(|(d, why)| format!("{} — {why}", shown(d))).collect()),
        ("reads.home", r.home.iter().map(|w| shown(w)).collect()),
        ("reads.allow", r.allow.iter().map(|w| shown(w)).collect()),
        ("reads.kept", r.kept.iter().map(|w| shown(w)).collect()),
        ("reads.log", r.log.iter().map(|w| shown(w)).collect()),
        ("copies.env", r.copies.env.iter().map(|(v, c)| format!("{v}={}", shown(c))).collect()),
        ("copies.hidden", r.copies.hidden.iter().map(|(h, v)| format!("{} (${v})", shown(h))).collect()),
        ("copies.left_out", r.copies.left_out.iter().map(|(h, v)| format!("{} (${v})", shown(h))).collect()),
        ("copies.told", r.copies.told.iter().map(|(w, _)| shown(w)).collect()),
        ("run env", p.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        ("tool homes", p.run_homes.iter().map(|h| format!("{} {}", h.name, shown(&h.user))).collect()),
    ];
    for (name, values) in fields {
        s.push_str(&format!("{name}:\n{}", lines(f, run, values)));
    }
    s
}

/// A scene of `the_policy_is_pinned`: its name, what `Here` adds, the settings.
type Scene<'a> = (&'a str, &'a [(&'a str, &'a str)], crate::llm::LlmConfig);

/// The profile and the policy, each system's, from the stand-in home: its homes at their defaults, the
/// homes moved by their variables, and the network open with a port (cargo's caches written then).
#[test]
fn the_policy_is_pinned() {
    let moved = [
        ("CARGO_HOME", "$BASE/cargo-moved"),
        ("RUSTUP_HOME", "$BASE/rustup-moved"),
        ("GRADLE_USER_HOME", "$BASE/gradle-moved"),
        ("GRADLE_OPTS", "-Xmx1g"),
    ];
    let open = crate::llm::LlmConfig { sandbox_network: true, sandbox_ports: vec![9], ..Default::default() };
    let mut out = String::new();
    for os in [Os::Mac, Os::Linux] {
        let scenes: [Scene; 3] = [
            ("homes at their defaults", &[], Default::default()),
            ("homes moved", &moved, Default::default()),
            ("network open, port 9", &[], open.clone()),
        ];
        for (i, (scene, env, llm)) in scenes.into_iter().enumerate() {
            let f = Fixture::new(&format!("policy-{os:?}-{i}"), os, env);
            out.push_str(&format!("══ {os:?}: {scene}\n"));
            out.push_str(&policy_said(&f, &f.policy(&llm)));
        }
    }
    pinned("sandbox_policy.txt", &out);
}

/// Where a run may write (`write_places` — gitescape's git launcher reads it), runs' folders among it (in
/// tarae's cache, as the home says: `runs`), but the devices, which no pack names (each said present).
#[test]
fn the_write_places_are_pinned() {
    let f = Fixture::new("write-places", Os::HOST, &[]);
    let llm = crate::llm::LlmConfig::default();
    let all = Policy::for_project(&env_in(&f.root, &llm, &[], &f.here).0).write_places();
    assert!(DEVICE_PLACES.iter().all(|d| all.iter().any(|w| w == Path::new(d))), "{all:?}");
    let ours = all.iter().filter(|w| !DEVICE_PLACES.iter().any(|d| *w == Path::new(d)));
    pinned("sandbox_write_places.txt", &lines(&f, None, ours.map(|w| shown(w))));
}

/// What Claude reads with a run's result: every kind of denial the report explains, and what its panel line
/// says — then a run that was denied nothing, and its panel line (the folder the checkout's cargo config named).
#[test]
fn the_report_is_pinned() {
    // (macOS's: a token file's place is the system's — the report names it)
    let f = Fixture::new("report", Os::Mac, &[]);
    let p = f.policy(&crate::llm::LlmConfig::default());
    let run = p.run.clone();
    let home = &f.home;
    let file = |op: &str, p: PathBuf| Denial { process: "x".into(), op: op.into(), target: shown(&p) };
    let telemetry = |rel: &str| quiet_writes()[0].join(rel).display().to_string();
    let denials = [
        file("file-write-create", home.join(".cargo/registry/src/index/x-1.0/new.rs")),
        file("file-write-create", home.join(".gradle/caches/transforms-4/x")),
        Denial { process: "java".into(), op: "network-outbound".into(), target: "remote:*:9".into() },
        file("file-read-data", home.join(".gitconfig")),
        file("file-read-data", home.join(".cargo/credentials.toml")),
        // (Go's telemetry: quiet at its place for the process's home on this system, as `quiet` reads it —
        // the same tail in the project is named)
        Denial { process: "x".into(), op: "file-write-create".into(), target: telemetry("local/x.count") },
        file("file-write-create", f.root.join("Library/Application Support/go/telemetry/local/x.count")),
        file("file-write-data", f.root.join("Cargo.toml")),
        file("file-write-create", f.root.join(".git/index.lock")),
        file("file-write-create", PathBuf::from("/opt/tarae-golden/x")),
        // (3c: a pty the run made — `script`'s other end — or one of the user's)
        file("file-write-data", PathBuf::from("/dev/ttys012")),
    ];
    let watch = Watch::quiet("golden.");
    for d in denials {
        watch.record(d);
    }
    let guard = Guard::In(Box::new(Policy { watch: Some(watch), ..p.clone() }));
    let mut out = String::from("══ denied\n");
    out.push_str(&guard.report(&[]));
    let line = panel_line(Some(&guard), false, std::time::Instant::now());
    out.push_str(&format!("\npanel: {line:?}\n"));
    // What only the run's output says was refused: the output's words, not the sandbox's — a grep's hit
    // quoting the phrase in the project says nothing; cargo's error on its cache is said, apart
    let output: Vec<(bool, String)> = [
        format!("{}/src/a.rs:3: // Operation not permitted", shown(&f.root)),
        "error: failed to download `serde v1.0.0`".into(),
        format!(
            "  failed to create directory `{}`",
            shown(&home.join(".cargo/registry/src/index/serde-1.0.0"))
        ),
        "Caused by:".into(),
        "  Operation not permitted (os error 1)".into(),
    ]
    .into_iter()
    .map(|l| (true, l))
    .collect();
    let unrecorded = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("golden-out.")), ..p.clone() }));
    out.push_str("══ said only in the output\n");
    out.push_str(&unrecorded.report(&output));
    let line = panel_line(Some(&unrecorded), false, std::time::Instant::now());
    out.push_str(&format!("\npanel: {line:?}\n"));
    // Both: what tarae saw blocked, then what the output also says
    let seen = Watch::quiet("golden-both.");
    seen.record(file("file-write-data", f.root.join("Cargo.toml")));
    let both = Guard::In(Box::new(Policy { watch: Some(seen), ..p.clone() }));
    out.push_str("══ denied, and said in the output\n");
    out.push_str(&both.report(&output));
    // (Linux, step 8: where sessions share this machine's loopback, another session's listener on a port the policy
    // opens, and a bind joining another session's port — a supervised policy's: macOS's reaches either)
    let theirs = Watch::quiet("golden-theirs.");
    for (process, op, target) in [
        ("node", SESSION_OP, "remote:127.0.0.1:5173"),
        ("vite", SESSION_OP, "local:0.0.0.0:5173"),
        // (whose a port is couldn't be told: refused, never said to be another session's)
        ("curl", UNTOLD_OP, "remote:127.0.0.1:8080"),
    ] {
        theirs.record(Denial { process: process.into(), op: op.into(), target: target.into() });
    }
    let apart =
        Guard::In(Box::new(Policy { watch: Some(theirs), ports: vec![5173], supervised: true, ..p.clone() }));
    out.push_str("\n══ another session's port\n");
    out.push_str(&apart.report(&[]));
    out.push_str("\n══ denied nothing\n");
    let calm = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("golden-calm.")), ..p }));
    out.push_str(&calm.report(&[]));
    let line = panel_line(Some(&calm), false, std::time::Instant::now());
    out.push_str(&format!("\npanel: {line:?}\n"));
    // 9b: the run's guarantees after the rules — with the conversation's first run; a later quiet run says neither,
    // a later failed one the rules again but not them. Seatbelt's (the policy's own), then each tier's (its keeper's,
    // given); the rules said once above
    use super::guarantees::{Found, Guarantees, Ns};
    let rules = calm.rules();
    let none = Some("no user namespaces: kernel.unprivileged_userns_clone is 0".to_string());
    let no_pid =
        Some("no PID namespace: /proc can't be mounted fresh here (Docker's masked /proc)".to_string());
    let (none_brief, no_pid_brief) = (
        Some("user namespaces are turned off".to_string()),
        Some("/proc is masked, no PID namespace".to_string()),
    );
    let tiers: [(&str, Option<Found>); 7] = [
        ("Seatbelt", None),
        (
            "A+, ABI 11",
            Some(Found { abi: 11, network: false, ..tier_found(Ns::session(true, true), true, false, true) }),
        ),
        (
            "A, ABI 6",
            Some(Found {
                short: no_pid.clone(),
                brief: no_pid_brief.clone(),
                ..tier_found(Ns::session(true, false), true, false, false)
            }),
        ),
        ("B+, ABI 6", Some(tier_found(Ns::session(true, true), false, false, false))),
        (
            "C, ABI 4",
            Some(Found {
                abi: 4,
                short: none.clone(),
                brief: none_brief.clone(),
                ..tier_found(Ns::NONE, true, false, false)
            }),
        ),
        (
            "C by copies, ABI 11",
            Some(Found {
                abi: 11,
                short: none.clone(),
                brief: none_brief.clone(),
                ..tier_found(Ns::NONE, true, true, false)
            }),
        ),
        ("D, ABI 3", Some(Found { abi: 3, short: none, ..tier_found(Ns::NONE, false, false, false) })),
    ];
    for (label, found) in tiers {
        let g = found.map(Guarantees::landlock).or_else(|| calm.guarantees());
        let said = |known: Told| calm.report_as(&[], known, g.clone()).text.replace(&rules, "[the rules]");
        let first = calm.report_as(&[], Told::default(), g.clone());
        let later = Told { rules: Some(first.rules), guarantees: first.guarantees };
        out.push_str(&format!("\n══ {label}: the conversation's first run\n{}", said(Told::default())));
        out.push_str(&format!("\n── a later run\n{}", said(later)));
        out.push_str(&format!("\n── a later run that failed\n{}\n", said(Told { rules: None, ..later })));
    }
    pinned("sandbox_report.txt", &f.named(&out, run.as_deref()));
}

/// A keeper's `Found` at Landlock ABI 6, `served` or not (Yama's 3), by `copies`, its runs going out by TPROXY or
/// not (why not: no `nft_tproxy`, or no network namespace of its own), the network open; nothing else short.
fn tier_found(
    ns: super::guarantees::Ns,
    served: bool,
    copies: bool,
    tproxy: bool,
) -> super::guarantees::Found {
    let net = ns.run_net(served, copies);
    super::guarantees::Found {
        ns,
        served: if served { Ok(()) } else { Err("Yama's ptrace_scope is 3".into()) },
        copies,
        abi: 6,
        ptys: ns.serves_ptys(net).then_some(16),
        tproxy: match (tproxy, ns.netns()) {
            (true, _) => Ok(()),
            (false, true) => Err("this kernel has no nft_tproxy".into()),
            (false, false) => Err("no network namespace of its own".into()),
        },
        lo_missing: Vec::new(),
        short: None,
        brief: None,
        hint: None,
        network: true,
    }
}

/// 9c, D7 (decision 12, N1): a run's `TARAE_SANDBOX_STATUS` is its tier's letter and Landlock's ABI — by copies still
/// its tier's letter, a partial ABI just its number — or Seatbelt's `seatbelt`.
#[test]
fn the_status_is_the_tier_and_the_abi() {
    use super::guarantees::{Found, Guarantees, Ns};
    let status = |f: Found| Guarantees::landlock(f).status();
    assert_eq!(status(Found { abi: 8, ..tier_found(Ns::session(true, true), true, false, true) }), "A+:8");
    assert_eq!(
        status(tier_found(Ns::session(false, true), true, true, false)),
        "A+:6",
        "by copies: still A+"
    );
    assert_eq!(status(Found { abi: 4, ..tier_found(Ns::NONE, true, false, false) }), "C:4", "colima's");
    assert_eq!(status(Found { abi: 2, ..tier_found(Ns::session(true, false), false, false, false) }), "B:2");
    assert_eq!(Guarantees::seatbelt(true, true).status(), "seatbelt");
}

/// 9b, decision 11: Claude's report says the run's tier — what its sandbox keeps from other sessions' commands, what
/// it shares, and the notes on that (N2) — with the conversation's first run, not with the next, and again once a
/// policy change gives its runs another keeper: the network opened, the session's keeper has no network namespace of
/// its own, its loopback this machine's (`tools::rules_told` keeps what was told; a failed run's rules come again).
#[test]
fn the_report_says_the_tier_once() {
    use super::guarantees::{Found, Guarantees, Ns};
    let f = Fixture::new("tier-once", Os::Mac, &[]);
    let p = f.policy(&crate::llm::LlmConfig::default());
    let calm = Guard::In(Box::new(Policy { watch: Some(Watch::quiet("tier-once.")), ..p }));
    let closed = Found { network: false, ..tier_found(Ns::session(true, true), true, false, true) };
    let open = tier_found(Ns::session(false, true), true, false, false);
    let (closed, open) = (Guarantees::landlock(closed), Guarantees::landlock(open));
    assert_ne!(closed.told(), open.told(), "another keeper's namespaces, other words");
    let mut known = Told::default();
    let mut run = |g: &Guarantees, failed: bool| {
        let asked = Told { rules: known.rules.filter(|_| !failed), ..known };
        let r = calm.report_as(&[], asked, Some(g.clone()));
        known = Told { rules: Some(r.rules), guarantees: r.guarantees };
        r.text.contains(&g.told())
    };
    assert!(run(&closed, false), "the conversation's first run says it");
    assert!(!run(&closed, false), "the next doesn't");
    assert!(!run(&closed, true), "nor a failed one, which says the rules again");
    assert!(run(&open, false), "another keeper's: said again");
    assert!(!run(&open, false), "and then not");
}

/// 9a: what each tier's runs are kept apart from and share, in every place's words (`guarantees.rs`) — every tier,
/// Landlock ABI 2 to 11, each network way (TPROXY, tarae's proxy by its variable, by copies — with the session's user
/// namespace too —, no socket), and Seatbelt's, ports and the network each open or not. Beside the golden, what each
/// must share whatever the words, and (9f) the profile's hint where Ubuntu's restriction is why C or D has no
/// namespaces — its tier up (A+, B+) in the panel's words, then whole —, written out per scene for ABI 2, 3 to 5 and 6 on — every ABI around a boundary
/// tried (7, 9 and 11 add nothing a run is kept from) — and nothing else: what's shared is said shared everywhere, the
/// rest kept.
#[test]
fn the_guarantees_say_each_tier() {
    use super::guarantees::{Found, Guarantees, Hint, Ns, Part, Tier};
    use Part::*;
    let found = tier_found;
    // (9f: Ubuntu's restriction and no profile on tarae — the `.deb`'s place, so its step is short)
    let packaged =
        super::apparmor::Place { path: super::apparmor::PACKAGED.into(), root_owned: Ok(()), packaged: true };
    let ubuntu = |served: bool| Found {
        short: Some("no user namespaces: Ubuntu's restriction".to_string()),
        brief: Some("AppArmor blocks user namespaces".to_string()),
        hint: Some(Hint::of(Tier::of(Ns::NONE, served), &packaged)),
        ..found(Ns::NONE, served, false, false)
    };
    let no_pid =
        Some("no PID namespace: /proc can't be mounted fresh here (Docker's masked /proc)".to_string());
    let none = Some("no user namespaces: kernel.unprivileged_userns_clone is 0".to_string());
    let (none_brief, no_pid_brief) = (
        Some("user namespaces are turned off".to_string()),
        Some("/proc is masked, no PID namespace".to_string()),
    );
    let a_plus = || found(Ns::session(true, true), true, false, true);
    let a = |copies: bool, tproxy: bool| Found {
        short: no_pid.clone(),
        brief: no_pid_brief.clone(),
        ..found(Ns::session(!copies, false), true, copies, tproxy)
    };
    // (what each shares at ABI 2 — no truncating rule: files; no signal scope —, at 3 to 5 — still no signal scope:
    // signals where other processes are in sight —, and from 6; every ABI around a boundary tried, 3, 5 and 6 too)
    // (its label, its tier, what its keeper is, what it shares at ABI 2, 3 to 5 and from 6)
    type Scene = (&'static str, Option<Tier>, Found, [&'static [Part]; 3]);
    let scenes: Vec<Scene> = vec![
        ("A+, TPROXY", Some(Tier::APlus), a_plus(), [&[Files], &[], &[]]),
        (
            "A+, tarae's proxy",
            Some(Tier::APlus),
            found(Ns::session(true, true), true, false, false),
            [&[Files], &[], &[]],
        ),
        (
            "A+ by copies: the session's user namespace, no network one",
            Some(Tier::APlus),
            found(Ns::session(false, true), true, true, false),
            [&[Files], &[], &[]],
        ),
        (
            "A+, the project in /dev/shm",
            Some(Tier::APlus),
            found(Ns { shm: false, ..Ns::session(true, true) }, true, false, true),
            [&[Files, Shm], &[Shm], &[Shm]],
        ),
        (
            "A+, its lo without an IPv6 address",
            Some(Tier::APlus),
            Found { lo_missing: vec!["IPv6"], ..a_plus() },
            [&[Files], &[], &[]],
        ),
        (
            "A+, the network closed",
            Some(Tier::APlus),
            Found { network: false, ..a_plus() },
            [&[Files], &[], &[]],
        ),
        (
            "A, TPROXY",
            Some(Tier::A),
            a(false, true),
            [&[Files, Processes, Signals], &[Processes, Signals], &[Processes]],
        ),
        (
            "A, tarae's proxy",
            Some(Tier::A),
            a(false, false),
            [&[Files, Processes, Signals], &[Processes, Signals], &[Processes]],
        ),
        (
            "A by copies",
            Some(Tier::A),
            a(true, false),
            [&[Files, Processes, Signals], &[Processes, Signals], &[Processes]],
        ),
        ("B+", Some(Tier::BPlus), found(Ns::session(true, true), false, false, false), [&[Files], &[], &[]]),
        (
            "B",
            Some(Tier::B),
            Found {
                short: no_pid.clone(),
                brief: no_pid_brief.clone(),
                ..found(Ns::session(true, false), false, false, false)
            },
            [&[Files, Processes, Signals], &[Processes, Signals], &[Processes]],
        ),
        (
            "C, tarae's proxy",
            Some(Tier::C),
            Found { short: none.clone(), brief: none_brief.clone(), ..found(Ns::NONE, true, false, false) },
            [&[Files, Processes, Signals, Ipc, Shm], &[Processes, Signals, Ipc, Shm], &[Processes, Ipc, Shm]],
        ),
        (
            "C by copies",
            Some(Tier::C),
            Found { short: none.clone(), brief: none_brief.clone(), ..found(Ns::NONE, true, true, false) },
            [&[Files, Processes, Signals, Ipc, Shm], &[Processes, Signals, Ipc, Shm], &[Processes, Ipc, Shm]],
        ),
        (
            "C, Ubuntu's restriction, no profile",
            Some(Tier::C),
            ubuntu(true),
            [&[Files, Processes, Signals, Ipc, Shm], &[Processes, Signals, Ipc, Shm], &[Processes, Ipc, Shm]],
        ),
        (
            "D",
            Some(Tier::D),
            Found { short: none.clone(), brief: none_brief.clone(), ..found(Ns::NONE, false, false, false) },
            [
                &[Files, Processes, Signals, Ipc, Shm, Ptys],
                &[Processes, Signals, Ipc, Shm, Ptys],
                &[Processes, Ipc, Shm, Ptys],
            ],
        ),
        (
            "D, Ubuntu's restriction, no profile",
            Some(Tier::D),
            ubuntu(false),
            [
                &[Files, Processes, Signals, Ipc, Shm, Ptys],
                &[Processes, Signals, Ipc, Shm, Ptys],
                &[Processes, Ipc, Shm, Ptys],
            ],
        ),
    ];
    let mut all: Vec<(String, Option<Tier>, Guarantees, Vec<Part>)> = Vec::new();
    for (label, tier, f, [at_2, to_5, from_6]) in scenes {
        let abis =
            [(2, at_2), (3, to_5), (4, to_5), (5, to_5), (6, from_6), (7, from_6), (9, from_6), (11, from_6)];
        for (abi, shared) in abis {
            let g = Guarantees::landlock(Found { abi, ..f.clone() });
            all.push((format!("{label} · ABI {abi}"), tier, g, shared.to_vec()));
        }
    }
    // (ports and the network apart: ports opened with the network closed is llm.sandbox-localhost alone, as this
    // repository has it; the network open reaches this machine's ports only through tarae's proxy, none opened)
    for (label, ports, network) in [
        ("Seatbelt, no port open, the network closed", false, false),
        ("Seatbelt, ports open, the network closed", true, false),
        ("Seatbelt, no port open, the network open", false, true),
        ("Seatbelt, ports and the network open", true, true),
    ] {
        let mut shared = vec![Processes, Ipc, Shm];
        if ports {
            shared.push(Loopback);
        }
        all.push((label.to_string(), None, Guarantees::seatbelt(ports, network), shared));
    }
    let mut out = String::new();
    let mut wrong = Vec::new();
    for (label, tier, g, mut want) in all {
        let (line, sentence, said) = (g.line(), g.sentence(), g.shared_said());
        out += &format!("══ {label}\nline: {line}\nsentence: {sentence}\n");
        out += &format!(
            "why: {}\nbrief: {}\nhint: {}\nreport:{}\n",
            g.why.as_deref().unwrap_or("—"),
            g.brief.as_deref().unwrap_or("—"),
            g.hint_brief().map_or("—".into(), |b| format!("{b} — {}", g.hint.as_ref().unwrap().said)),
            if said.is_empty() { " —" } else { &said }
        );
        if g.tier() != tier {
            wrong.push(format!("{label}: tier {:?}", g.tier()));
        }
        let order =
            |p: &Part| [Files, Processes, Signals, Ipc, Shm, Ptys, Loopback].iter().position(|q| q == p);
        want.sort_by_key(order);
        if g.shared() != want {
            wrong.push(format!("{label}: shares {:?}, not {want:?}", g.shared()));
        }
        // (each where every place says it: the sentence's half, the line's list, the report)
        let (kept, shared) = sentence.split_once("; shared with them — ").unwrap();
        let listed = line.rsplit_once(" · shared ").unwrap().1;
        for r in &g.rows {
            let name = g.name(r.part);
            let (here, there) = if r.kept { (kept, shared) } else { (shared, kept) };
            if !here.contains(name) || there.contains(name) || listed.contains(name) == r.kept {
                wrong.push(format!("{label}: {name} not said {}", if r.kept { "kept" } else { "shared" }));
            }
        }
        // (and below ABI 6 the report says signals whatever the tier: its session's own other commands are reached)
        let below_6 = matches!(g.by, super::guarantees::By::Landlock { abi, .. } if abi < 6);
        if said.is_empty() != (want.is_empty() && !below_6) {
            wrong.push(format!("{label}: the report says {said:?} of {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    pinned("sandbox_guarantees.txt", &out);
}

/// The words Claude's prompt and tools say of the sandbox, the checks and the toolchains.
#[test]
fn claudes_words_are_pinned() {
    let desc = |v: serde_json::Value| v["description"].as_str().unwrap().to_string();
    let system = &crate::chat::system();
    let from = system.find("Both run in a sandbox").unwrap();
    let to = from + system[from..].find("never work around it.").unwrap() + "never work around it.".len();
    let out = format!(
        "══ run\n{}\n══ run_tests\n{}\n══ the chat's prompt\n{}\n",
        desc(crate::tools::run_schema()),
        desc(crate::tools::run_tests_schema()),
        &system[from..to]
    );
    pinned("claude_sandbox_words.txt", &out);
}

/// The packs' table (`sandboxpack.rs`) says what the core's own lists say, each as its readers take it — the
/// two side by side until the readers move to the table (then the pinned texts above hold it).
#[test]
fn the_packs_say_what_the_core_said() {
    let t = crate::sandboxpack::get();
    assert_eq!(t.problems, Vec::<String>::new());
    // (what a run reads, never reads and writes in home, the caches: the policy pinned above)
    // Go's telemetry, at its place in home on each system (Linux's where `XDG_CONFIG_HOME` puts it)
    let quiet = |os: Os, env: &[(&str, &str)]| {
        let env = env.iter().map(|(k, v)| (k.to_string(), v.into())).collect();
        quiet_places(&vars_in(Path::new("/h"), &Here { env, os, ..Here::default() }))
    };
    let xdg = [("XDG_CONFIG_HOME", "/x/conf")];
    assert_eq!(quiet(Os::Mac, &xdg), [PathBuf::from("/h/Library/Application Support/go/telemetry")]);
    assert_eq!(quiet(Os::Linux, &[]), [PathBuf::from("/h/.config/go/telemetry")]);
    assert_eq!(quiet(Os::Linux, &xdg), [PathBuf::from("/x/conf/go/telemetry")]);
    // What Claude runs unasked, git's read-only ones being the core's
    let mut now: Vec<&str> = crate::tools::run_allow().collect();
    now.sort();
    let was = [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
        "cargo metadata",
        "cargo test",
        "cargo tree",
        "git diff",
        "git log",
        "git show",
        "git status",
        "go build",
        "go test",
        "go vet",
        "gofmt -l",
    ];
    assert_eq!(now, was);
    // What builds a project, with what no pack names
    let mut now: Vec<&str> = t.builders.iter().map(String::as_str).collect();
    now.sort();
    let was = [
        "cargo", "cmake", "dotnet", "gmake", "go", "gradle", "just", "make", "mvn", "nox", "npm", "npx",
        "pnpm", "pytest", "rustc", "swift", "tox", "yarn", "zig",
    ];
    assert_eq!(now, was);
    // What a command runs that its words don't name: the card for it again says whether that changed
    let entry: Vec<(String, &[String])> = t.entry.iter().map(|e| (e.run.join(" "), &e.files[..])).collect();
    assert_eq!(entry, [("cargo xtask".to_string(), &["xtask/".to_string()][..])]);
    // The one `$(…)` a line may hold
    let [e] = t.expand.as_slice() else { panic!("{:?}", t.expand) };
    assert_eq!(
        (e.word.as_str(), &e.run[..]),
        ("$(rustc --print sysroot)", &["rustup", "show", "active-toolchain"].map(String::from)[..])
    );
    // (the settings that can hold a token: the policy pinned above, `tokens.rs`'s tests)
}

/// Landlock's rules for the stand-in home's policy, in the fixture's folder and `/dev` (outside them they're
/// the machine's) — what Linux enforces, from the same policy — for the ABIs that change what's handled:
/// each one's handled rights, then its rules. Written down, not made: any kernel pins every ABI. Its project
/// holds files named like keys (7b): one at the top, one deep, one opened (`allow::Grant::Key`) and an exception
/// — no rule for the closed ones, their folders all but reading files.
#[cfg(target_os = "linux")]
#[test]
fn landlocks_rules_are_pinned() {
    use crate::landlock;
    let f = Fixture::new("landlock", Os::Linux, &[]);
    for k in [".env", ".env.example", "src/tls/server.pem", "src/tls/opened.key"] {
        std::fs::create_dir_all(f.root.join(k).parent().unwrap()).unwrap();
        std::fs::write(f.root.join(k), "tarae-fake-token").unwrap();
    }
    let mut p = f.policy(&crate::llm::LlmConfig::default());
    p.keys = Some(vec![f.root.join("src/tls/opened.key")]);
    let run = p.run.as_deref();
    let ours = |path: &Path| {
        path.starts_with(&f.base)
            || run.is_some_and(|r| path.starts_with(r.path()))
            || path == Path::new(TMP)
            || path.starts_with("/dev")
    };
    let mut out = String::new();
    for abi in [3, 4, 6, 9, 10] {
        let r = landlock::Ruleset::written(abi).unwrap();
        let served = super::tier::RunNet::Served;
        let c =
            linux::confine_in(r, &p, Err("not watched".into()), false, Ok(()), served, Default::default())
                .unwrap();
        let (fs, net, scopes) = (landlock::handled(abi), landlock::handled_net(abi), landlock::scopes(abi));
        out += &format!("ABI {abi}: handles files {fs:#07x} · network {net:#x} · scopes {scopes:#x}\n");
        let mut rules: Vec<String> = c
            .granted()
            .iter()
            .filter(|(path, _)| ours(path))
            .map(|(path, access)| format!("    {:#06x} {}\n", access, f.named(&shown(path), run)))
            .collect();
        // (a set: sorted as shown — in path order the run's folder, numbered by the runs before, moves about)
        rules.sort();
        out.extend(rules);
    }
    pinned("sandbox_landlock.txt", &out);
    // 7d: where the run's child has a mount namespace of its own, the closed keys are hidden by mounts (`Covers`)
    // and every folder of the project is one rule — the project's rules and the files mounted over, one ABI
    let r = landlock::Ruleset::written(6).unwrap();
    // (its mount table: the project on the one mount of its filesystem)
    let table = Some("22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n".to_string());
    let own = linux::Own { ptys: None, mounts: Some(Ok(())), table };
    let served = super::tier::RunNet::Served;
    let c = linux::confine_in(r, &p, Err("not watched".into()), false, Ok(()), served, own).unwrap();
    let mut out = String::from("ABI 6, its own mount namespace — the project's rules:\n");
    let mut rules: Vec<String> = c
        .granted()
        .iter()
        .filter(|(path, _)| path.starts_with(&f.root))
        .map(|(path, access)| format!("    {:#06x} {}\n", access, f.named(&shown(path), run)))
        .collect();
    rules.sort();
    out.extend(rules);
    out += "hidden by mounts:\n";
    let mut hidden: Vec<String> =
        c.covers.keys.iter().map(|k| format!("    {}\n", f.named(&shown(&k.path), run))).collect();
    hidden.sort();
    out.extend(hidden);
    pinned("sandbox_landlock_covered.txt", &out);
}

/// The rules of a run with its own ptys (3b, `sandbox::ptys`), pinned by shape: what this machine's `/dev` holds
/// is read entry by entry around `/dev/pts` (one line, `/dev/<each entry but pts>`), the pool's ptys by inode
/// (`$PTY1`, `$PTY2`, in number order); nothing else of `/dev/pts` but its `ptmx`.
#[cfg(target_os = "linux")]
#[test]
fn landlocks_rules_with_ptys_are_pinned() {
    use crate::landlock;
    let f = Fixture::new("landlock-ptys", Os::Linux, &[]);
    let p = f.policy(&crate::llm::LlmConfig::default());
    let pool = super::ptys::Ptys::open(2);
    let mut numbers = Vec::new();
    pool.each(|_, n| {
        numbers.push(n);
        Ok(())
    })
    .unwrap();
    if numbers.len() < 2 {
        eprintln!("landlocks_rules_with_ptys_are_pinned: skipped — this system gave {} ptys", numbers.len());
        return;
    }
    numbers.sort();
    let mut out = String::new();
    for abi in [4, 6] {
        let r = landlock::Ruleset::written(abi).unwrap();
        let served = super::tier::RunNet::Served;
        let own = linux::Own { ptys: Some(&pool), ..Default::default() };
        let c = linux::confine_in(r, &p, Err("not watched".into()), false, Ok(()), served, own).unwrap();
        let read = (landlock::EXECUTE | landlock::READ_FILE | landlock::READ_DIR) & landlock::handled(abi);
        out += &format!("ABI {abi}:\n");
        let mut rules: Vec<String> = c
            .granted()
            .iter()
            .filter(|(path, _)| path.starts_with("/dev"))
            .map(|(path, access)| {
                let shown =
                    match numbers.iter().position(|n| *path == Path::new("/dev/pts").join(n.to_string())) {
                        Some(i) => format!("/dev/pts/$PTY{}", i + 1),
                        // (a file's read has no `READ_DIR`: two lines, files' and folders')
                        None if path.parent() == Some(Path::new("/dev")) && *access & !read == 0 => {
                            "/dev/<each entry but pts>".into()
                        }
                        None => path.display().to_string(),
                    };
                format!("    {access:#06x} {shown}\n")
            })
            .collect();
        rules.sort();
        rules.dedup();
        out.extend(rules);
    }
    pinned("sandbox_landlock_ptys.txt", &out);
}
