//! `cargo xtask linux [--env NAME=VALUE]… [--userns[=masked] | --docker-default] [--image NAME] [--volume tarae-compat-…:/PATH] [--platform linux/<arch>] [clippy|test [--repeat N] [cargo test's arguments] | test --installed … | test --as-user … | test --keep … | test --kept …]` — tarae's checks on Linux (CI runs there too: `-D dead-code` on code only
//! macOS calls, a root container's `#` prompt …), in Docker on the pinned toolchain (`rust:<major.minor>`, and
//! `rust-toolchain.toml`'s exact version in it). Whichever Docker the CLI reaches: `DOCKER_CONTEXT` or `DOCKER_HOST`, else the
//! current context (here the colima VM: `colima start -p tarae-test`, `DOCKER_CONTEXT=colima-tarae-test`).
//!
//! The working tree is mounted **read-only**; the checks run in a clone made inside the container, with the
//! uncommitted changes applied and the untracked files copied — nothing the container runs can write the real
//! tree or its `.git`. A Docker on another machine (a `DOCKER_HOST` socket forwarded from a cloud VM) can't
//! see the tree (`sees_tree`): the checkout as it is goes in on stdin instead, as a tar (`streamed`), made a
//! repository of one commit inside. Caches live in Docker volumes, per pinned version (target, rustup; the
//! registry shared).
//!
//! Docker not reachable: it says `skipped — …` and exits 0, so a `:preflight` step with `note = "skipped"`
//! reads it as a pass with a note. Not in tarae's own `.tarae.toml` — a step of yours (`.tarae.local.toml`).
//!
//! The target volume is the checkout's own (`target_volume`, labelled with its path and this machine's id):
//! every checkout's clone is /work/tarae, so one shared target let a session's tests run another session's
//! build (its e2e ran another's `target/debug/tarae`). Each run drops this machine's volumes whose checkout is
//! gone — its whole repository deleted too (`prunable`); `cargo xtask linux prune` does that now, and drops
//! the old shared `tarae-target-<pin>` too — tarae's `session.ended` runs it when a session's checkout is
//! removed. It lists the targets of checkouts that landed too (`landed`: nothing changed, the branch in the
//! main branch — sessions brought in but not ended held 34 GB of colima's 30 GB disk, 2026-10-17), dropped only
//! with `--yes`, each said. "This machine" is tarae's own id for it (`machine_id`: random, made once in tarae's data folder,
//! copied into the repository's git folder) — a Docker other machines use too (`DOCKER_HOST`) holds their
//! checkouts' targets, whose paths aren't here: another id's volume is never judged.
//!
//! A volume carries the id when the run that made it had one: a run outside tarae's sandbox always (the data
//! folder's, which it also copies to the git folder, `GIT_COPY`); a run inside it — which can't read the data
//! folder but can read the git folder — once a run outside has made that copy for this repository (any
//! `cargo xtask linux` or `prune` of yours, or `session.ended`'s). A sandboxed run before then says so and
//! makes its volume without one. A volume without the id — made so, or made before ids — is judged by its
//! checkout's path alone, as this machine's, for as long as it lives (`docker volume create` leaves an existing
//! volume's labels as they are, so it's never relabelled) — but only with `prune --mine` (`local`): nothing a
//! Docker says tells that it's this machine's alone (a unix socket may be forwarded from another machine, a
//! host's name collides), so otherwise only volumes carrying this machine's id are dropped. `prune` says how
//! many it judged by path, or kept. `prune --all` lists every volume and image of the repository's but live
//! checkouts' targets (an open session's in the UI state, or a folder that's there), and drops them only with
//! `--yes`.
//! After the checks, past `VOLUMES_SAY` (30 GB), a line says what tarae's volumes take — the checkouts'
//! targets, which ends and `prune` free, apart from the shared ones (colima's disk filled at 40 GB, 2026-10-06,
//! with nothing said); a Docker that can't measure them is said so. The line says the disk's use too (the
//! container's `df -P /target`, read off its output), and comes past `DISK_SAY` (80 %) however little they take.
//!
//! `--platform linux/amd64` runs them on another architecture, its target and rustup volumes its own — on
//! colima's aarch64 VM under qemu-user: slow, and a build was seen to stall (2026-10-05); a Docker on a real
//! x86_64 machine (`DOCKER_HOST`) needs no `--platform`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, root};

#[derive(Clone, Debug, PartialEq)]
enum What {
    Clippy,
    /// `cargo test` with these arguments (a filter, `--bin tarae`, `-- --test-threads=1` …), run `repeat` times
    /// in the one container — stopping at the first that fails (a flaky test looked for).
    Test {
        args: Vec<String>,
        repeat: u32,
    },
    /// `test --installed`: tarae's own tests (`--bin tarae`) built, the binary put at `/usr/bin/tarae` (root's, as
    /// its `.deb` puts it) and run from there with these arguments (the test binary's: a filter, `--exact` …) —
    /// where an AppArmor profile on the Docker's machine attaches to tarae by its path (`--userns`: the container
    /// unconfined, so the machine's profiles apply by path inside it; step 6h).
    Installed {
        args: Vec<String>,
    },
    /// `test --keep`: tarae's own tests built with these arguments (cargo test's: `--profile`, `--bin tarae`) and
    /// the binary kept in the checkout's target volume (`KEPT`) — for `--kept` runs after it (`cargo xtask
    /// compat`: one build a round, every stack's image runs it). Each container clones the tree anew, so cargo
    /// would build the crate again in each (new mtimes).
    Keep {
        args: Vec<String>,
    },
    /// `test --kept`: the binary `--keep` kept, run with these arguments (the test binary's) — no cargo, no build,
    /// so without the container's `CARGO_TARGET_DIR` (the target volume): the tests' own cargo runs (compat's
    /// rust stack) build where a user's would, not into tarae's target; none kept yet, it fails saying so.
    Kept {
        args: Vec<String>,
    },
    /// `test --as-user`: tarae's own tests (`--bin tarae`) built as root, then run as a user of the container's own
    /// (`AS_USER`, made there) through `setpriv` with these arguments (the test binary's), the `--env` variables
    /// kept — what runs as tarae does on a desktop, where root's ways differ (7d: root can't map itself into a run's
    /// own user namespace, so its keys are closed by their folders' rules).
    AsUser {
        args: Vec<String>,
    },
    Both,
}

/// The user `test --as-user` runs the tests as, made in the container.
const AS_USER: &str = "tarae-test";

const USAGE: &str = "usage: cargo xtask linux [--env NAME=VALUE]… [--userns[=masked] | --docker-default] [--image NAME] [--volume tarae-compat-…:/PATH] [--platform linux/<arch>] [clippy | test [--repeat N] [cargo test's arguments] | test --installed [the test binary's arguments] | test --as-user [the test binary's arguments] | test --keep [cargo test's arguments] | test --kept [the test binary's arguments]]";

/// What `--userns` lets the container do, so the sandbox's tier tests reach a real session keeper
/// (`src/sandbox/tier.rs`): Docker's default seccomp profile refuses new namespaces, its `docker-default`
/// AppArmor profile refuses mounting, and its masked paths make a fresh `/proc` too revealing to mount. Only
/// the container is loosened — never what tarae does; without the option it's Docker's defaults (tier host).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Userns {
    /// `--userns`: all three lifted — tier own where the kernel allows unprivileged user namespaces.
    Full,
    /// `--userns=masked`: `/proc` left masked — tier own without a PID namespace (D12).
    Masked,
}

/// A leading `--userns` or `--userns=masked`, and the arguments after it.
fn userns(args: &[String]) -> Result<(Option<Userns>, &[String])> {
    match args.split_first() {
        Some((u, rest)) if u == "--userns" => Ok((Some(Userns::Full), rest)),
        Some((u, rest)) if u == "--userns=masked" => Ok((Some(Userns::Masked), rest)),
        Some((u, _)) if u.starts_with("--userns") => {
            Err(format!("{u}: --userns or --userns=masked — {USAGE}"))
        }
        _ => Ok((None, args)),
    }
}

/// `--docker-default` (M8b): the container exactly as `docker run` makes one — Docker's default seccomp and
/// AppArmor profiles, and no `--cap-add SYS_PTRACE` either, so `pidfd_getfd` is refused as in a dev container
/// started the usual way: tarae can't serve its runs, tier offline (`expect_offline`). Not with `--userns` (which
/// lifts those profiles). Leading, or right after `--userns`'s place: the flag and the arguments after it.
fn docker_default(args: &[String]) -> (bool, &[String]) {
    match args.split_first() {
        Some((d, rest)) if d == "--docker-default" => (true, rest),
        _ => (false, args),
    }
}

/// `docker run`'s arguments without `--cap-add SYS_PTRACE` (`on`: `--docker-default`) — wherever it is among the
/// options (`--cap-add SYS_PTRACE` or `--cap-add=SYS_PTRACE`), nothing else changed. An error where an option still
/// gives the capability (`--cap-add ALL`, `--privileged`): the run would test the full way while saying it doesn't.
fn without_ptrace(a: Vec<String>, on: bool) -> Result<Vec<String>> {
    if !on {
        return Ok(a);
    }
    // (the options: before the image, `sh`, `-ec` and the script — the last four)
    let options = a.len().saturating_sub(4);
    let (mut out, mut i) = (Vec::with_capacity(a.len()), 0);
    while i < a.len() {
        match (i < options, a[i].as_str(), a.get(i + 1).map(String::as_str)) {
            (true, "--cap-add", Some("SYS_PTRACE")) => i += 2,
            (true, "--cap-add=SYS_PTRACE", _) => i += 1,
            _ => {
                out.push(a[i].clone());
                i += 1;
            }
        }
    }
    // (what Docker takes for it: any case, with or without `CAP_`, or all of them)
    let ptrace = |v: &str| {
        let v = v.to_ascii_uppercase();
        matches!(v.strip_prefix("CAP_").unwrap_or(&v), "SYS_PTRACE" | "ALL")
    };
    let opts = &out[..out.len().saturating_sub(4)];
    let still = opts.iter().enumerate().any(|(i, o)| {
        o == "--privileged"
            || o.strip_prefix("--cap-add=").is_some_and(ptrace)
            || o == "--cap-add" && opts.get(i + 1).is_some_and(|v| ptrace(v))
    });
    if still {
        return Err(format!("--docker-default: an option still gives SYS_PTRACE — {opts:?}"));
    }
    Ok(out)
}

/// A leading `--image NAME`, and the arguments after it: the checks run in that image instead of `rust:<pin>` —
/// one built from it with the tools a measurement runs (step 6e′'s M5: `target/sb/m5/m5-image.sh`). It runs the
/// same script, as root, with the same volumes: its `/usr/local/rustup` and the registry are the volumes'.
fn image_named(args: &[String]) -> Result<(Option<String>, &[String])> {
    match args {
        [i, name, rest @ ..] if i == "--image" => {
            let ok = !name.is_empty()
                && !name.starts_with(['-', '.', '/', ':'])
                && name.chars().all(|c| c.is_ascii_alphanumeric() || "._/:-".contains(c));
            match ok {
                true => Ok((Some(name.clone()), rest)),
                false => Err(format!("--image {name}: an image's name — {USAGE}")),
            }
        }
        [i] if i == "--image" => Err(format!("--image: which one? — {USAGE}")),
        rest => Ok((None, rest)),
    }
}

/// A leading `--volume NAME:PATH`, and the arguments after it: one of compat's volumes (`COMPAT`: a stack's
/// pre-warmed caches, its home) mounted at PATH, an absolute one — never another volume, nor a folder of the
/// machine's.
fn volume_named(args: &[String]) -> Result<(Option<String>, &[String])> {
    match args {
        [v, spec, rest @ ..] if v == "--volume" => {
            let (name, path) = spec.split_once(':').unwrap_or_default();
            let ok = name.len() > COMPAT.len()
                && name.starts_with(COMPAT)
                && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                && path.starts_with('/')
                && !path.contains([':', ','])
                && !path.split('/').any(|p| p == "..");
            match ok {
                true => Ok((Some(spec.clone()), rest)),
                false => Err(format!("--volume {spec}: {COMPAT}…:/an/absolute/path — {USAGE}")),
            }
        }
        [v] if v == "--volume" => Err(format!("--volume: which one? — {USAGE}")),
        rest => Ok((None, rest)),
    }
}

/// `docker run`'s arguments with `-v NAME:PATH` before the image (the last four: image, `sh`, `-ec`, the script)
/// — None: as they are.
fn with_volume(mut a: Vec<String>, volume: Option<&str>) -> Vec<String> {
    if let Some(v) = volume {
        let at = a.len().saturating_sub(4);
        a.splice(at..at, ["-v".to_string(), v.to_string()]);
    }
    a
}

/// `docker run`'s arguments with `image` in the image's place (the fourth from the end: image, `sh`, `-ec`, the
/// script) — None: as they are.
fn with_image(mut a: Vec<String>, image: Option<&str>) -> Vec<String> {
    if let (Some(i), Some(at)) = (image, a.len().checked_sub(4)) {
        a[at] = i.to_string();
    }
    a
}

/// `docker run`'s arguments with `--userns`'s security options after `run --rm --cap-add SYS_PTRACE`.
fn with_userns(mut a: Vec<String>, u: Option<Userns>) -> Vec<String> {
    let opts: &[&str] = match u {
        None => &[],
        Some(Userns::Masked) => &["seccomp=unconfined", "apparmor=unconfined"],
        Some(Userns::Full) => &["seccomp=unconfined", "apparmor=unconfined", "systempaths=unconfined"],
    };
    let set: Vec<String> = opts.iter().flat_map(|o| ["--security-opt".to_string(), o.to_string()]).collect();
    let at = 4.min(a.len());
    a.splice(at..at, set);
    a
}

/// The name of the container for `what` (`checks`, `probe`), from `prefix` — tarae's `TARAE_CONTAINER_PREFIX`
/// for a run of Claude's: tarae stops what's named from it when it stops the run (`src/containers.rs`), where a
/// stopped client left its container running. None: no prefix, or one that isn't a container's name.
fn container_name(prefix: Option<&str>, what: &str) -> Option<String> {
    let p = prefix.filter(|p| {
        !p.is_empty()
            && p.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            && !p.starts_with(['.', '-'])
    })?;
    Some(format!("{p}-{what}"))
}

/// The signals that end the checks' container with xtask: an interrupt (Ctrl-C), a termination (`timeout`, a
/// stopped run of tarae's), the terminal's hangup.
const ENDING: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Where `ENDING`'s handler writes the signal it took (a pipe's write end; -1: none yet).
static SIGNALLED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// `ENDING`'s handler: the signal's number, one byte, on `SIGNALLED`'s pipe — all a handler may do.
extern "C" fn took(sig: libc::c_int) {
    let fd = SIGNALLED.load(std::sync::atomic::Ordering::Relaxed);
    let b = sig as u8;
    // SAFETY: write(2) is async-signal-safe; one byte of the stack's.
    unsafe { libc::write(fd, (&raw const b).cast(), 1) };
}

/// From here on, an `ENDING` signal ends the checks' container (`docker kill <name>`, `--rm` removes it) and then
/// xtask (128 + the signal, as a shell says a killed process) — killed alone, xtask left `docker run`'s client
/// living and the container running: a `timeout` around it ended only xtask, and a pipe after it waited on (6f′).
/// A handler for the whole process (whichever thread takes the signal; a child's exec resets it) writes the signal on
/// a pipe, and a thread of its own reads it and does the rest.
fn stop_on_signal(name: String) {
    let Ok((mut r, w)) = std::io::pipe() else { return };
    SIGNALLED.store(std::os::fd::IntoRawFd::into_raw_fd(w), std::sync::atomic::Ordering::Relaxed);
    std::thread::spawn(move || {
        let mut sig = [0u8];
        if std::io::Read::read_exact(&mut r, &mut sig).is_err() {
            return;
        }
        let sig = i32::from(sig[0]);
        eprintln!("signal {sig}: ending the container {name}");
        let _ = Command::new("docker").args(killing(&name)).stdout(std::process::Stdio::null()).status();
        std::process::exit(128 + sig);
    });
    // SAFETY: a zeroed sigaction with its handler and an empty mask, for signals that may have one.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = took as extern "C" fn(libc::c_int) as libc::sighandler_t;
        act.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut act.sa_mask);
        for s in ENDING {
            libc::sigaction(s, &act, std::ptr::null_mut());
        }
    }
}

/// `docker`'s arguments that end container `name` at once.
fn killing(name: &str) -> [String; 2] {
    ["kill".into(), name.into()]
}

/// `docker run`'s arguments with `--name NAME` after `run` (None: as they are).
fn with_name(mut a: Vec<String>, name: Option<String>) -> Vec<String> {
    if let Some(n) = name {
        let at = 1.min(a.len());
        a.splice(at..at, ["--name".to_string(), n]);
    }
    a
}

/// The leading `--env NAME=VALUE`s — set in the container (`HOME=/work`: the checkout inside home, as on a
/// machine where it's under the user's home) — and the arguments after them.
fn envs(args: &[String]) -> Result<(Vec<String>, &[String])> {
    let mut rest = args;
    let mut set = Vec::new();
    while let [e, v, more @ ..] = rest
        && e == "--env"
    {
        let name = v.split_once('=').map(|(n, _)| n).unwrap_or_default();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("--env {v}: NAME=VALUE — {USAGE}"));
        }
        set.push(v.clone());
        rest = more;
    }
    if rest.first().is_some_and(|e| e == "--env") {
        return Err(format!("--env: which one? — {USAGE}"));
    }
    Ok((set, rest))
}

/// The `--env`s, and under `--docker-default` (`stock`) `TARAE_EXPECT_TIER=offline` where none names a tier: the
/// probe's test then fails where the fallback isn't what's reached, rather than passing on whatever it finds.
fn expect_offline(mut set: Vec<String>, stock: bool) -> Vec<String> {
    if stock && !set.iter().any(|e| e.starts_with("TARAE_EXPECT_TIER=")) {
        set.push("TARAE_EXPECT_TIER=offline".into());
    }
    set
}

/// `docker run`'s arguments with `-e NAME=VALUE` for each of `envs`, before the image (the last four: image,
/// `sh`, `-ec`, the script).
fn with_envs(mut a: Vec<String>, envs: &[String]) -> Vec<String> {
    let at = a.len().saturating_sub(4);
    let set: Vec<String> = envs.iter().flat_map(|e| ["-e".to_string(), e.clone()]).collect();
    a.splice(at..at, set);
    a
}

/// What `cargo xtask linux …`'s arguments ask for: the platform first (`--platform linux/amd64`: Docker's
/// `os/arch`, emulated where the VM's isn't it — None: the Docker host's own), then the checks.
fn ask(args: &[String]) -> Result<(Option<String>, What)> {
    match args {
        [p, platform, rest @ ..] if p == "--platform" => {
            let arch = platform.strip_prefix("linux/").unwrap_or_default();
            if arch.is_empty() || !arch.chars().all(|c| c.is_ascii_alphanumeric() || c == '/') {
                return Err(format!("--platform {platform}: linux/<arch>, as linux/amd64 — {USAGE}"));
            }
            Ok((Some(platform.clone()), what(rest)?))
        }
        [p] if p == "--platform" => Err(format!("--platform: which one? — {USAGE}")),
        rest => Ok((None, what(rest)?)),
    }
}

/// The checks the arguments after the platform ask for.
fn what(args: &[String]) -> Result<What> {
    Ok(match args.split_first() {
        None => What::Both,
        Some((c, [])) if c == "clippy" => What::Clippy,
        Some((t, rest)) if t == "test" => match rest {
            [i, rest @ ..] if i == "--installed" => What::Installed { args: rest.to_vec() },
            [u, rest @ ..] if u == "--as-user" => What::AsUser { args: rest.to_vec() },
            [k, rest @ ..] if k == "--keep" => What::Keep { args: rest.to_vec() },
            [k, rest @ ..] if k == "--kept" => What::Kept { args: rest.to_vec() },
            [r, n, rest @ ..] if r == "--repeat" => What::Test {
                args: rest.to_vec(),
                repeat: n
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(format!("--repeat {n}: a count — {USAGE}"))?,
            },
            rest => What::Test { args: rest.to_vec(), repeat: 1 },
        },
        Some((a, _)) => return Err(format!("{USAGE} — not {a}")),
    })
}

pub fn run(args: &[String]) -> Result<()> {
    let (set, args) = envs(args)?;
    let (stock, args) = docker_default(args);
    let (ns, args) = userns(args)?;
    let (stock_after, args) = docker_default(args);
    let stock = stock || stock_after;
    if stock && ns.is_some() {
        return Err(format!("--docker-default and --userns: one or the other — {USAGE}"));
    }
    let set = expect_offline(set, stock);
    let (named, args) = image_named(args)?;
    let (volume, args) = volume_named(args)?;
    let (platform, what) = ask(args)?;
    let platform = platform.as_deref();
    let root = root();
    let toolchain = root.join("rust-toolchain.toml");
    let pin =
        pin(&std::fs::read_to_string(&toolchain).map_err(|e| format!("{}: {e}", toolchain.display()))?)?;
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    let on = platform.map(|p| format!(", {p}")).unwrap_or_default();
    eprintln!("Linux checks on {} (Rust {pin}{on}) …", image(&pin));
    let machine = machine_id(&root);
    // (a run never judges a volume without a machine id: only `prune --mine` does — `local`)
    prune_volumes(machine.as_deref().ok(), false, false);
    // (made here, not by `docker run`: only a volume made this way carries its labels)
    if let Err(why) = &machine {
        eprintln!("no machine id ({why}): a target volume made now carries none — judged by its path");
    }
    let made =
        Command::new("docker").args(create_args(&root, machine.as_deref().ok(), &pin, platform)).output();
    if !made.as_ref().is_ok_and(|o| o.status.success()) {
        return Err(format!("docker volume create {}: failed", target_volume(&root, &pin, platform)));
    }
    let git = common_git_dir(&root);
    let tree = match sees_tree(&root, &pin, platform) {
        true => Tree::Mounted(git.as_deref()),
        false => {
            eprintln!(
                "this Docker doesn't see {} (another machine's): the checkout goes in on stdin",
                root.display()
            );
            Tree::Streamed
        }
    };
    let mut docker = Command::new("docker");
    // (always a name — tarae's prefix's, else one of this process's own: a signal ends the container by it)
    let name = container_name(std::env::var("TARAE_CONTAINER_PREFIX").ok().as_deref(), "checks")
        .unwrap_or_else(|| format!("tarae-xtask-{}-checks", std::process::id()));
    stop_on_signal(name.clone());
    let name = Some(name);
    let args = with_image(docker_args(&root, tree, &pin, platform, &what), named.as_deref());
    let args = with_userns(with_envs(with_volume(args, volume.as_deref()), &set), ns);
    if let Some(i) = &named {
        eprintln!("--image: the checks run in {i}");
    }
    if let Some(v) = &volume {
        eprintln!("--volume: {v}");
    }
    // (the name last; `--docker-default` takes the capability out)
    docker.args(with_name(without_ptrace(args, stock)?, name));
    if let Some(u) = ns {
        eprintln!("--userns: the container's own seccomp and AppArmor profiles lifted ({u:?})");
    }
    if stock {
        let version = Command::new("docker").args(["version", "--format", "{{.Server.Version}}"]).output();
        let version =
            version.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        eprintln!(
            "--docker-default: Docker {version}'s default seccomp and AppArmor profiles, no SYS_PTRACE — \
             pidfd_getfd refused"
        );
    }
    let tar = match tree {
        Tree::Mounted(_) => None,
        Tree::Streamed => Some(streamed(&root)?),
    };
    if tar.is_some() {
        docker.stdin(std::process::Stdio::piped());
    }
    // (its output passed on as it comes, but the disk's line the script ends with — `DISK_MARK`)
    let mut child =
        docker.stdout(std::process::Stdio::piped()).spawn().map_err(|e| format!("docker: {e}"))?;
    let out = child.stdout.take().expect("piped");
    let seen = std::thread::spawn(move || forwarded(std::io::BufReader::new(out), &mut std::io::stdout()));
    let feed = tar.map(|tar| {
        let mut stdin = child.stdin.take().expect("piped");
        std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &tar))
    });
    let st = child.wait().map_err(|e| format!("docker: {e}"))?;
    if let Some(Ok(Err(e))) = feed.map(|f| f.join()) {
        eprintln!("the checkout didn't all go in: {e}");
    }
    let seen = seen.join().unwrap_or_default();
    say_sizes(seen.disk);
    if !st.success() {
        return Err(failed_said(&seen.failed));
    }
    Ok(())
}

/// `rust-toolchain.toml`'s `channel`.
pub(crate) fn pin(toolchain: &str) -> Result<String> {
    let t: toml::Table = toolchain.parse().map_err(|e| format!("rust-toolchain.toml: {e}"))?;
    t.get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| "rust-toolchain.toml: no [toolchain] channel".into())
}

/// Whether the Docker `docker` would talk to answers — Err: why not, in a line.
pub(crate) fn reachable(docker: &mut Command) -> std::result::Result<(), String> {
    let out = docker
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("no docker command ({e})"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let why = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it didn't answer");
    Err(why.to_string())
}

/// The line said when there's no Docker to run in — what a preflight note matches.
pub(crate) fn skipped(why: &str) -> String {
    format!(
        "skipped — Docker isn't reachable: {why} (start it — colima start -p tarae-test — or set DOCKER_CONTEXT)"
    )
}

/// The repository's git folder when it isn't the tree's own `.git` — a worktree (every tarae session is
/// one): its `.git` is a file naming a folder under the main checkout's, which the clone inside reads.
pub(crate) fn common_git_dir(root: &Path) -> Option<PathBuf> {
    git_folder(root).filter(|dir| !dir.starts_with(root))
}

/// The repository's common git folder, wherever it is — the one every checkout of it shares.
fn git_folder(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// How the checkout reaches the container.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tree<'a> {
    /// Read-only at /src, with a worktree's git folder (read-only too, at the path its `.git` file names): a
    /// Docker that sees this machine's files (colima's VM shares them).
    Mounted(Option<&'a Path>),
    /// On the container's stdin as a tar (`streamed`): a Docker on another machine sees none of them.
    Streamed,
}

/// Whether the Docker sees the checkout at its own path — a bind `--mount` (unlike `-v`, which makes an empty
/// folder there) refuses a source that isn't there.
fn sees_tree(root: &Path, pin: &str, platform: Option<&str>) -> bool {
    let mut probe = Command::new("docker");
    let name = container_name(std::env::var("TARAE_CONTAINER_PREFIX").ok().as_deref(), "probe");
    probe.args(with_name(vec!["run".into(), "--rm".into()], name));
    if let Some(p) = platform {
        probe.args(["--platform", p]);
    }
    probe.arg("--mount").arg(format!("type=bind,source={},target=/src,readonly", root.display()));
    probe.args([image(pin).as_str(), "test", "-f", "/src/Cargo.toml"]);
    probe.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
    probe.stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The checkout as it is, as a tar for a Docker that can't see it (`Tree::Streamed`): every tracked file with
/// its changes, the untracked ones git doesn't ignore — a deleted one left out, no `.git`, links as links.
fn streamed(root: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--deduplicate"])
        .output()
        .map_err(|e| format!("git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut tar = tar::Builder::new(Vec::new());
    tar.follow_symlinks(false);
    for name in out.stdout.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let rel = Path::new(std::ffi::OsStr::from_bytes(name));
        let path = root.join(rel);
        // (deleted, the deletion not staged yet: the clone leaves it out too)
        if path.symlink_metadata().is_err() {
            continue;
        }
        tar.append_path_with_name(&path, rel).map_err(|e| format!("{}: {e}", rel.display()))?;
    }
    tar.into_inner().map_err(|e| format!("tar: {e}"))
}

/// `docker run …` for the checks: the tree as `tree` brings it, the caches in volumes, the pinned image — on
/// `platform` when one is asked, its target and toolchains in volumes of its own (the crates' sources stay
/// shared).
fn docker_args(root: &Path, tree: Tree, pin: &str, platform: Option<&str>, what: &What) -> Vec<String> {
    // (CAP_SYS_PTRACE: Docker's default filter refuses `pidfd_getfd` without it — the sandbox's supervisor takes a
    // run's socket with it, and its tests would skip themselves)
    let mut a: Vec<String> = ["run", "--rm", "--cap-add", "SYS_PTRACE"].map(String::from).into();
    if let Some(p) = platform {
        a.extend(["--platform".to_string(), p.to_string()]);
    }
    let mount = |a: &mut Vec<String>, v: String| a.extend(["-v".to_string(), v]);
    match tree {
        Tree::Mounted(git) => {
            mount(&mut a, format!("{}:/src:ro", root.display()));
            if let Some(git) = git {
                mount(&mut a, format!("{0}:{0}:ro", git.display()));
            }
        }
        Tree::Streamed => a.push("-i".to_string()),
    }
    let target = target_volume(root, pin, platform);
    mount(&mut a, format!("{target}:/target"));
    // (and again where toolbox mounts the host's root: the sandbox's tests reach a fake home through it — a second
    // mount of one filesystem, which a container without `--userns` can't make itself — `SECOND_MOUNT`)
    mount(&mut a, format!("{target}:{SECOND_TARGET}"));
    a.extend(["-e".to_string(), format!("TARAE_TEST_SECOND_MOUNT={SECOND_MOUNT}")]);
    mount(&mut a, "tarae-registry:/usr/local/cargo/registry".into());
    // (seeded from the image the first time: the file's components and targets are fetched once)
    let of = platform.map(|p| format!("-{}", p.replace('/', "-"))).unwrap_or_default();
    mount(&mut a, format!("tarae-rustup-{pin}{of}:/usr/local/rustup"));
    // (`TARAE_REFUSED_SUITE`: the must-be-refused suite runs — its snapshots are these tiers'; a plain `cargo
    // test` on Linux skips it)
    for e in ["CARGO_TARGET_DIR=/target", "CARGO_TERM_COLOR=never", "TARAE_REFUSED_SUITE=1"] {
        a.extend(["-e".to_string(), e.to_string()]);
    }
    a.extend([image(pin), "sh".into(), "-ec".into(), script(tree, what)]);
    a
}

/// Where the target volume is mounted a second time (`docker_args`).
const SECOND_TARGET: &str = "/run/host/target";

/// The folder the sandbox's tests make their fake homes in, and the same folder through the second mount
/// (`TARAE_TEST_SECOND_MOUNT`, 7e) — made by the script, writable by `--as-user`'s user too.
const SECOND_MOUNT: &str = "/target/tarae-second-mount:/run/host/target/tarae-second-mount";

/// The label naming the checkout a target volume is for.
const CHECKOUT_LABEL: &str = "tarae.checkout";
/// The label naming the machine it was made on (`machine_id`): only there is its checkout's path judged — a
/// Docker other machines use too (`DOCKER_HOST`) holds their checkouts' targets. (Not the host's name: macOS's
/// changes with the network.)
const MACHINE_LABEL: &str = "tarae.machine";

/// The file in tarae's data folder holding this machine's id.
const MACHINE_FILE: &str = "machine-id";

/// Its copy in the repository's common git folder, which every run of it can read — a run in tarae's sandbox
/// too, which can't read the data folder (nor write the git folder: only a run outside it makes the copy).
const GIT_COPY: &str = "tarae-machine-id";

/// This machine's id for tarae — Err: why there's none (no home, a folder this run can't read or write, and
/// no copy in the repository's git folder).
fn machine_id(root: &Path) -> std::result::Result<String, String> {
    machine_id_from(crate::data_dir(), git_folder(root).as_deref())
}

/// The id in the data folder (`MACHINE_FILE`, made there the first time), copied into the git folder
/// (`GIT_COPY`) when the copy isn't it yet — one id for every repository on the machine, so a deleted
/// repository's volumes are still judged as this machine's. The data folder unreadable (tarae's sandbox): the
/// copy, when a run outside it has made one.
fn machine_id_from(
    data: std::result::Result<PathBuf, String>,
    git: Option<&Path>,
) -> std::result::Result<String, String> {
    let copy = git.map(|g| g.join(GIT_COPY));
    match data.and_then(|d| machine_id_in(&d)) {
        Ok(id) => {
            if let Some(copy) = copy.filter(|c| read_id(c).ok().flatten().as_ref() != Some(&id)) {
                // (a copy that can't be written — a sandboxed run reading the data folder after all — is the
                // next run's to make)
                let _ = write_id(&copy, &id, true);
            }
            Ok(id)
        }
        Err(why) => match copy.as_deref().map(read_id) {
            Some(Ok(Some(id))) => Ok(id),
            Some(Ok(None)) => Err(format!("{why}; and no copy yet in the git folder")),
            Some(Err(e)) => Err(format!("{why}; {e}")),
            None => Err(why),
        },
    }
}

/// The id in `file` — None: there's no such file; Err: one that isn't an id, or can't be read.
fn read_id(file: &Path) -> std::result::Result<Option<String>, String> {
    match std::fs::read_to_string(file) {
        Ok(s) if s.trim().len() == 32 && s.trim().chars().all(|c| c.is_ascii_hexdigit()) => {
            Ok(Some(s.trim().to_string()))
        }
        Ok(_) => Err(format!("{} isn't an id", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

/// Writes `id` to `file`, beside it first then into place: renamed over what's there (`replace`), else linked
/// — failing with `AlreadyExists` when another run's landed first.
fn write_id(file: &Path, id: &str, replace: bool) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{id}\n"))?;
    if replace {
        return std::fs::rename(&tmp, file).inspect_err(|_| {
            std::fs::remove_file(&tmp).ok();
        });
    }
    let linked = std::fs::hard_link(&tmp, file);
    std::fs::remove_file(&tmp).ok();
    linked
}

/// The id kept in `dir`, made there when it's not yet: 16 random bytes in hex, written beside and linked into
/// place — two runs making it at once agree on the one that landed. One that isn't an id is left as it is
/// (another made would orphan the volumes labelled with it).
fn machine_id_in(dir: &Path) -> std::result::Result<String, String> {
    use std::io::Read as _;
    let file = dir.join(MACHINE_FILE);
    let read = || read_id(&file);
    if let Some(id) = read()? {
        return Ok(id);
    }
    let made = |e: std::io::Error| format!("making {}: {e}", file.display());
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut r| r.read_exact(&mut bytes)).map_err(made)?;
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    match write_id(&file, &id, false) {
        Ok(()) => Ok(id),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read()?.ok_or_else(|| made(e)),
        Err(e) => Err(made(e)),
    }
}

/// The checkout's own target volume: `tarae-target-<pin>-<hash of its path>` (FNV-1a — the same path gives the
/// same name on every run and Rust version), the platform asked hashed in after it (the host's own: the path
/// alone).
fn target_volume(root: &Path, pin: &str, platform: Option<&str>) -> String {
    let of = platform.map(|p| format!("\0{p}")).unwrap_or_default();
    let hash = (root.to_string_lossy().bytes())
        .chain(of.bytes())
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    format!("tarae-target-{pin}-{hash:016x}")
}

/// `docker volume create …` for it, labelled with the checkout's path and this machine's id when there's one
/// (an existing one is left as it is).
fn create_args(root: &Path, machine: Option<&str>, pin: &str, platform: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["volume", "create", "--label"].map(String::from).into();
    a.push(format!("{CHECKOUT_LABEL}={}", root.display()));
    if let Some(machine) = machine {
        a.extend(["--label".to_string(), format!("{MACHINE_LABEL}={machine}")]);
    }
    a.push(target_volume(root, pin, platform));
    a
}

/// A target volume as `docker volume ls` lists it.
#[derive(Debug, PartialEq)]
struct Volume {
    name: String,
    /// Its checkout's path — empty when unlabelled
    checkout: String,
    /// The machine it was made on (`machine_id`) — empty on one made before that label, or where the id
    /// couldn't be read
    machine: String,
}

/// The `--format` that lists `Volume`s.
fn ls_format() -> String {
    format!("{{{{.Name}}}}\t{{{{.Label \"{CHECKOUT_LABEL}\"}}}}\t{{{{.Label \"{MACHINE_LABEL}\"}}}}")
}

/// `docker volume ls`'s rows for the target volumes.
fn volumes(listed: &str) -> Vec<Volume> {
    listed
        .lines()
        .filter_map(|l| {
            let mut cols = l.split('\t').map(str::trim);
            let name = cols.next()?;
            let (checkout, machine) = (cols.next().unwrap_or(""), cols.next().unwrap_or(""));
            name.starts_with("tarae-target-").then(|| Volume {
                name: name.to_string(),
                checkout: checkout.to_string(),
                machine: machine.to_string(),
            })
        })
        .collect()
}

/// Whose a labelled target volume is, for `prunable`.
#[derive(Debug, PartialEq)]
enum Whose {
    /// Made with this machine's id
    Ours,
    /// Made before the id (or without it): judged by path, as this machine's
    NoId,
    /// Another machine's id — or one this run can't compare, having none: never judged
    Other,
}

fn whose(v: &Volume, ours: Option<&str>) -> Whose {
    match v.machine.as_str() {
        "" => Whose::NoId,
        m if ours == Some(m) => Whose::Ours,
        _ => Whose::Other,
    }
}

/// What the Docker says of itself (`docker info`): the name of the host it runs on, and its id — None: it
/// didn't say. Said beside `--mine`, never judged by.
fn docker_self(docker: &std::ffi::OsStr) -> Option<(String, String)> {
    let out = Command::new(docker)
        .args(["info", "--format", "{{.Name}}\t{{.ID}}"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let said = String::from_utf8_lossy(&out.stdout);
    let (name, id) = said.trim().split_once('\t')?;
    Some((name.to_string(), id.to_string()))
}

/// Whether volumes without a machine id, and the images, are this machine's: only when you say so (`mine`,
/// `--mine`). Nothing else tells it — a unix socket may be forwarded from another machine (frictions16:
/// deb12's, through ssh), and a host's name collides (`ubuntu`, `debian`, `localhost` — the reviewer's
/// finding: a same-named Docker elsewhere had every volume without an id judged ours). Err: what's said
/// instead, naming the Docker (`docker`: its host's name and id) so `--mine` is said of a known one.
fn local(mine: bool, docker: Option<(&str, &str)>) -> std::result::Result<(), String> {
    if mine {
        return Ok(());
    }
    Err(match docker {
        Some((name, id)) => format!("no machine id: kept — --mine takes them (this Docker: {name}, {id})"),
        None => "no machine id: kept — --mine takes them".into(),
    })
}

/// `local` for the Docker the CLI reaches.
fn judged_local(mine: bool) -> std::result::Result<(), String> {
    let docker = docker_self("docker".as_ref());
    local(mine, docker.as_ref().map(|(n, i)| (n.as_str(), i.as_str())))
}

/// The checkouts of the sessions the tarae running this has open, as its UI state lists them (`tarae state
/// --json`'s answer): asked of the socket tarae names in `TARAE_UI` — set for its terminal and your tasks —
/// with the request `tarae state` sends. No binary is run: a `tarae` on PATH older than `state` took it for a
/// file to open and started an editor (frictions16, `~/.cargo/bin/tarae` of 09-29). Err why not: not run from
/// a tarae, or it didn't answer — then a checkout is live by its folder alone. (Claude's runs never get
/// `TARAE_UI`.)
fn tarae_sessions(ui: Option<std::ffi::OsString>) -> std::result::Result<Vec<PathBuf>, String> {
    use std::io::{BufRead, Write};
    let sock =
        ui.ok_or("not run from a tarae — its terminal or a task of yours has TARAE_UI, the socket to ask")?;
    let said = |e: std::io::Error| format!("{}: {e}", Path::new(&sock).display());
    let mut s = std::os::unix::net::UnixStream::connect(&sock).map_err(said)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).map_err(said)?;
    writeln!(s, "{}", serde_json::json!({ "get": "state", "timeout_ms": 2000 })).map_err(said)?;
    let mut line = String::new();
    std::io::BufReader::new(&s).read_line(&mut line).map_err(said)?;
    let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| format!("its answer: {e}"))?;
    if let Some(e) = v["error"].as_str() {
        return Err(e.to_string());
    }
    Ok(sessions_in(&v))
}

/// The sessions' checkouts in `tarae state --json`'s answer.
fn sessions_in(v: &serde_json::Value) -> Vec<PathBuf> {
    v["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["root"].as_str())
        .map(PathBuf::from)
        .collect()
}

/// Why a target volume's checkout is live — `prune --all` keeps it: a session of a running tarae has it open
/// (`sessions`), or its folder is there (`there`: a worktree under tarae's worktrees, any checkout). None: gone.
fn live(checkout: &str, sessions: &[PathBuf], there: impl Fn(&Path) -> bool) -> Option<&'static str> {
    let at = Path::new(checkout);
    if checkout.is_empty() {
        return None;
    }
    if sessions.iter().any(|s| s == at) {
        return Some("a session tarae has open");
    }
    there(at).then_some("its folder is there")
}

/// The target volumes to drop: this machine's (`ours`) whose checkout is gone (`is_dir` says) — its
/// repository too or not — and, on a Docker that's this machine's alone (`local`), those without an id judged
/// so too; with `shared`, the one every checkout shared before (`tarae-target-<pin>`, no label). An unlabelled
/// one of another form isn't ours to judge, nor another machine's (through a shared Docker), nor any with an
/// id when this run has none: kept.
fn prunable(
    volumes: &[Volume],
    ours: Option<&str>,
    local: bool,
    is_dir: impl Fn(&Path) -> bool,
    shared: bool,
) -> Vec<String> {
    let old_form = |n: &str| {
        n.strip_prefix("tarae-target-").is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    volumes
        .iter()
        .filter(|v| match v.checkout.is_empty() {
            true => shared && old_form(&v.name),
            false => {
                let judged = match whose(v, ours) {
                    Whose::Ours => true,
                    Whose::NoId => local,
                    Whose::Other => false,
                };
                judged && !is_dir(Path::new(&v.checkout))
            }
        })
        .map(|v| v.name.clone())
        .collect()
}

/// What `prune` says besides each volume dropped: the ones another machine made (kept), the ones without an id
/// (judged by path on a Docker of this machine's, else kept), this run having no id of its own.
fn prune_notes(
    volumes: &[Volume],
    ours: std::result::Result<&str, &str>,
    local: std::result::Result<(), &str>,
    gone: &[String],
) -> Vec<String> {
    let left = |w: Whose| {
        let kept = |v: &&Volume| !v.checkout.is_empty() && !gone.contains(&v.name);
        volumes.iter().filter(kept).filter(|v| whose(v, ours.ok()) == w).count()
    };
    let s = |n: usize| if n == 1 { "" } else { "s" };
    let mut notes = Vec::new();
    match (ours, left(Whose::Other)) {
        (_, 0) => {}
        (Ok(_), n) => {
            notes.push(format!("kept {n} target volume{} another machine made — theirs to judge", s(n)))
        }
        (Err(why), n) => notes
            .push(format!("no machine id ({why}): kept {n} target volume{} made with one, not judged", s(n))),
    }
    match (local, left(Whose::NoId)) {
        (_, 0) => {}
        (Ok(()), n) => notes.push(format!(
            "{n} target volume{} made before machine ids carry none: judged by their checkout's path alone, \
             as this machine's (not relabelled)",
            s(n)
        )),
        (Err(why), n) => notes.push(format!("{n} target volume{} with {why}", s(n))),
    }
    notes
}

/// Past this, a run says what tarae's volumes take — before a full disk does.
const VOLUMES_SAY: u64 = 30_000_000_000;

/// Past this share of the disk used (the Docker machine's, where the target volume is), a run says what
/// tarae's volumes take however little that is — a disk filled by others' fills for tarae too.
const DISK_SAY: u32 = 80;

/// The line the checks' script ends with (`script`'s trap, on any exit): `df -P /target`'s row after it.
const DISK_MARK: &str = "tarae-xtask-disk: ";

/// The disk the target volume is on, as the container's `df -P` saw it: its share used (`Capacity`), bytes used
/// and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Disk {
    pct: u32,
    used: u64,
    size: u64,
}

/// `df -P`'s row (filesystem, 1024-blocks, used, available, capacity, mounted on — one row under `-P`).
pub(crate) fn disk(row: &str) -> Option<Disk> {
    let f: Vec<&str> = row.split_whitespace().collect();
    let [_, size, used, _, pct, ..] = f[..] else { return None };
    let kb = |n: &str| n.parse::<u64>().ok().map(|n| n * 1024);
    Some(Disk { pct: pct.strip_suffix('%')?.parse().ok()?, used: kb(used)?, size: kb(size)? })
}

/// What the container's output said besides passing through (`forwarded`).
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    /// The disk (the last `DISK_MARK` line).
    disk: Option<Disk>,
    /// The test binaries that failed, as cargo names them (`--bin tarae`, `--test e2e`), each once, in order.
    failed: Vec<String>,
}

/// cargo's line for each test binary that failed (`--no-fail-fast` goes on past it): `error: test failed, to
/// rerun pass `--test e2e``.
const RERUN: &str = "error: test failed, to rerun pass `";

/// `from` (the container's output) written to `to` as it comes, byte for byte, but the line after `DISK_MARK`,
/// read as the disk; cargo's failed test binaries noted on the way (`RERUN`).
fn forwarded(mut from: impl std::io::BufRead, to: &mut impl std::io::Write) -> Seen {
    let (mut seen, mut line) = (Seen::default(), Vec::new());
    loop {
        line.clear();
        if from.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
            return seen;
        }
        let text = std::str::from_utf8(&line).ok();
        if let Some(row) = text.and_then(|l| l.strip_prefix(DISK_MARK)) {
            seen.disk = disk(row.trim_end());
            continue;
        }
        if let Some(b) = text.and_then(|l| l.trim_end().strip_prefix(RERUN)).and_then(|r| r.strip_suffix('`'))
            && !seen.failed.iter().any(|f| f == b)
        {
            seen.failed.push(b.to_string());
        }
        let _ = to.write_all(&line);
        let _ = to.flush();
    }
}

/// How the run ended, said last: which test binaries failed when cargo named them.
fn failed_said(failed: &[String]) -> String {
    match failed {
        [] => "the Linux checks failed (above)".into(),
        f => format!(
            "the Linux checks failed — {} test {} failed (every one ran): {}",
            f.len(),
            if f.len() == 1 { "binary" } else { "binaries" },
            f.join(", ")
        ),
    }
}

/// `the disk 82 % used (48 of 59 GB)`
pub(crate) fn disk_said(d: Disk) -> String {
    format!("the disk {} % used ({:.0} of {:.0} GB)", d.pct, d.used as f64 / 1e9, d.size as f64 / 1e9)
}

/// Docker's size as it prints it (`7.21GB`, `606.3MB`, `0B` — powers of 1000) in bytes.
pub(crate) fn bytes(size: &str) -> Option<u64> {
    let at = size.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (n, unit) = size.split_at(at);
    let times = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((n.parse::<f64>().ok()? * times) as u64)
}

/// `docker system df -v`'s volumes (name, size, labels a line) — tarae's only: (name, bytes, checkout).
fn sizes(listed: &str) -> Vec<(String, u64, String)> {
    listed
        .lines()
        .filter_map(|l| {
            let mut cols = l.split('\t').map(str::trim);
            let (name, size) = (cols.next()?, bytes(cols.next()?)?);
            let labels = cols.next().unwrap_or("");
            let checkout =
                labels.split(',').find_map(|kv| kv.strip_prefix(&format!("{CHECKOUT_LABEL}="))).unwrap_or("");
            name.starts_with("tarae-").then(|| (name.to_string(), size, checkout.to_string()))
        })
        .collect()
}

/// `8.0 GB`
pub(crate) fn gb(n: u64) -> String {
    format!("{:.1} GB", n as f64 / 1e9)
}

/// The line saying what tarae's volumes take, once past `over` — or the disk (`disk`, when the container said
/// it) past `DISK_SAY`, however little they take; the disk's use said with it: the checkouts' targets — what a
/// session's end and `prune` free, with how — apart from the shared ones (registry, toolchains, trace's target),
/// which only `prune --all` frees; each part's total and its three biggest (a target by its checkout's folder
/// name).
fn size_line(volumes: &[(String, u64, String)], over: u64, disk: Option<Disk>) -> Option<String> {
    let total: u64 = volumes.iter().map(|v| v.1).sum();
    let full = disk.is_some_and(|d| d.pct >= DISK_SAY);
    if total < over && !full {
        return None;
    }
    let past = if total >= over { format!(" (past {})", gb(over)) } else { String::new() };
    let disk = disk.map(|d| format!(" — {}", disk_said(d))).unwrap_or_default();
    let (targets, shared): (Vec<_>, Vec<_>) = volumes.iter().partition(|v| v.0.starts_with("tarae-target-"));
    let part = |mut vs: Vec<&(String, u64, String)>| {
        vs.sort_by_key(|v| std::cmp::Reverse(v.1));
        let named: Vec<String> = vs
            .iter()
            .take(3)
            .map(|(name, size, checkout)| match Path::new(checkout).file_name() {
                Some(f) => format!("{}'s {}", f.to_string_lossy(), gb(*size)),
                None => format!("{name} {}", gb(*size)),
            })
            .collect();
        let more = vs.len().saturating_sub(3);
        let more = if more > 0 { format!(" and {more} more") } else { String::new() };
        format!("{} ({}{more})", gb(vs.iter().map(|v| v.1).sum()), named.join(", "))
    };
    let mut parts = Vec::new();
    if !targets.is_empty() {
        parts.push(format!(
            "checkouts' targets {} — a gone checkout's goes with `cargo xtask linux prune` (and each run), a \
             session's with its end where `session.ended` runs it",
            part(targets)
        ));
    }
    if !shared.is_empty() {
        parts.push(format!(
            "shared {} — only `cargo xtask linux prune --all` frees these (every gone target too; it lists, \
             --yes drops)",
            part(shared)
        ));
    }
    Some(format!("tarae's Docker volumes take {}{past}{disk}: {}", gb(total), parts.join("; ")))
}

/// `docker system df -v`'s volumes as `sizes` reads them: name, size, labels a line.
const DF_FORMAT: &str = "{{range .Volumes}}{{.Name}}\t{{.Size}}\t{{.Labels}}{{println}}{{end}}";

/// What `docker` prints for `DF_FORMAT` — Err: why it didn't, in a line.
pub(crate) fn df(docker: &mut Command) -> std::result::Result<String, String> {
    let out = docker
        .args(["system", "df", "-v", "--format", DF_FORMAT])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it didn't answer");
        return Err(format!("docker system df: {why}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What tarae's volumes take, said once past `VOLUMES_SAY` or the disk (`disk`: the container's) past
/// `DISK_SAY` — and said when it can't be measured, with the disk past `DISK_SAY` all the same.
fn say_sizes(disk: Option<Disk>) {
    match df(&mut Command::new("docker")) {
        Ok(listed) => {
            if let Some(line) = size_line(&sizes(&listed), VOLUMES_SAY, disk) {
                eprintln!("{line}");
            }
        }
        Err(why) => match disk.filter(|d| d.pct >= DISK_SAY) {
            Some(d) => eprintln!("{} — couldn't measure tarae's volumes ({why})", disk_said(d)),
            None => eprintln!("couldn't measure the volumes ({why})"),
        },
    }
}

/// `cargo xtask linux prune [--yes] [--mine]`: the target volumes no checkout uses, the old shared one too — and
/// those of checkouts that landed (`landed`), listed, dropped only with `--yes`.
pub fn prune(mine: bool, yes: bool) -> Result<()> {
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    let machine = machine_id(&root());
    let local = judged_local(mine);
    let (listed, gone) = prune_volumes(machine.as_deref().ok(), local.is_ok(), true);
    if gone.is_empty() {
        println!("no gone checkout's target volume to drop");
    }
    let (landed, untold) =
        landed_targets(&listed, machine.as_deref().ok(), local.is_ok(), &gone, &root(), landed);
    for line in untold {
        println!("{line}");
    }
    if !landed.is_empty() {
        println!(
            "{} the target volumes of checkouts that landed:",
            if yes { "dropping" } else { "would drop" }
        );
        for (name, checkout, why) in &landed {
            println!("  volume {name}  ({checkout} — {why})");
        }
        match yes {
            false => println!(
                "run again with --yes to drop these {} — a checkout's next run builds again",
                landed.len()
            ),
            true => {
                for (name, checkout, _) in &landed {
                    match Command::new("docker").args(["volume", "rm", name]).output() {
                        Ok(o) if o.status.success() => eprintln!("dropped {name} ({checkout} landed)"),
                        _ => eprintln!("kept {name} for now: Docker didn't drop it (a run using it?)"),
                    }
                }
            }
        }
    }
    let notes = prune_notes(
        &listed,
        machine.as_deref().map_err(String::as_str),
        local.as_ref().map(|_| ()).map_err(String::as_str),
        &gone,
    );
    for note in notes {
        println!("{note}");
    }
    // (the images only on a Docker of this machine's alone, as `prune --all` judges them)
    match &local {
        Ok(()) => crate::compat::drop_stale(&root()),
        Err(why) => println!("compat's images with {why}"),
    }
    // (no container ran: the disk unknown)
    say_sizes(None);
    Ok(())
}

/// The images this repository builds: `trace.rs`'s (`tarae-trace:<minor>-<hash>`), and compat's (`COMPAT`).
/// `rust:<minor>` isn't ours: pulled, maybe another project's too.
const OWN_IMAGES: [&str; 1] = ["tarae-trace"];

/// What `cargo xtask compat` makes starts with this: its images (`tarae-compat-node:1.99-<hash>`, one per recipe
/// in `compat/linux/`) and a volume per stack (`tarae-compat-go-cache`, the stack's home). Its own as
/// `OWN_IMAGES` are: `prune --all` lists them, `prune` drops an image its recipe no longer builds.
pub(crate) const COMPAT: &str = "tarae-compat-";

/// One thing `prune --all` drops.
#[derive(Debug, PartialEq)]
struct Dropped {
    /// `volume` or `image`
    kind: &'static str,
    name: String,
    size: u64,
    /// Its checkout, for a target volume (else empty)
    checkout: String,
}

/// A label's value in `docker system df`'s `k=v,k=v` labels (empty: none).
fn label<'a>(labels: &'a str, key: &str) -> &'a str {
    labels.split(',').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')).unwrap_or("")
}

/// What `prune --all` drops of `listed` (`docker system df -v`'s volumes, `DF_FORMAT`) and `images` (`IMAGES_FORMAT`):
/// every volume named `tarae-` that's this machine's — made with its id (`ours`), or without one on a Docker
/// that's this machine's own (`local`: by what it says of itself, or `--mine`) — but a live checkout's target
/// (`live`: an open session's, a folder that's there — kept, said why: removing a working session's target
/// under it was the wrong default), and the images this repository builds (`OWN_IMAGES`, local only: a shared
/// Docker's may be another machine's runs'). The rest is made again by the next run (the registry and
/// toolchains fetched again). Never another's: a volume or image of any other name, another machine's id, one
/// with an id when this run has none. What's dropped, and the lines saying what was kept as live.
fn everything(
    listed: &str,
    images: &str,
    ours: Option<&str>,
    local: bool,
    live: impl Fn(&str) -> Option<&'static str>,
) -> (Vec<Dropped>, Vec<String>) {
    let mut kept = Vec::new();
    let mut v: Vec<Dropped> = listed
        .lines()
        .filter_map(|l| {
            let mut cols = l.split('\t').map(str::trim);
            let (name, size) = (cols.next()?, bytes(cols.next()?)?);
            let labels = cols.next().unwrap_or("");
            let machine = label(labels, MACHINE_LABEL);
            let mine = match machine {
                "" => local,
                m => ours == Some(m),
            };
            if !(name.starts_with("tarae-") && mine) {
                return None;
            }
            let checkout = label(labels, CHECKOUT_LABEL);
            if let Some(why) = live(checkout) {
                let of = Path::new(checkout).file_name().map_or(checkout.into(), |n| n.to_string_lossy());
                kept.push(format!("  kept: live ({of} — {why}) volume {name}  {}", gb(size)));
                return None;
            }
            Some(Dropped { kind: "volume", name: name.to_string(), size, checkout: checkout.to_string() })
        })
        .collect();
    v.extend(images.lines().filter(|_| local).filter_map(|l| {
        let mut cols = l.split('\t').map(str::trim);
        let (name, size) = (cols.next()?, bytes(cols.next()?)?);
        let repo = name.rsplit_once(':').map_or(name, |(r, _)| r);
        (OWN_IMAGES.contains(&repo) || repo.starts_with(COMPAT)).then(|| Dropped {
            kind: "image",
            name: name.to_string(),
            size,
            checkout: String::new(),
        })
    }));
    (v, kept)
}

/// `docker image ls`'s rows as `everything` reads them: `repository:tag`, size.
const IMAGES_FORMAT: &str = "{{.Repository}}:{{.Tag}}\t{{.Size}}";

/// The list `prune --all` says before it drops anything: a line each, its size (a target with its checkout),
/// and the total.
fn dropped_list(all: &[Dropped]) -> Vec<String> {
    let mut lines: Vec<String> = all
        .iter()
        .map(|d| match d.checkout.is_empty() {
            true => format!("  {} {}  {}", d.kind, d.name, gb(d.size)),
            false => format!("  {} {}  {}  ({})", d.kind, d.name, gb(d.size), d.checkout),
        })
        .collect();
    lines.push(format!("  {} in all", gb(all.iter().map(|d| d.size).sum())));
    lines
}

/// `cargo xtask linux prune --all [--yes] [--mine]`: every volume and image of this repository's on this
/// machine but live checkouts' targets, listed with their sizes — and dropped only with `--yes`, after
/// you've read the list (the reviewer: it dropped tens of GB right after printing them, no chance to stop).
/// One in use (a run going) is kept, said.
pub fn prune_all(yes: bool, mine: bool) -> Result<()> {
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    prune_all_with("docker".as_ref(), machine_id(&root()), yes, mine, std::env::var_os("TARAE_UI"))
}

/// `prune_all` with the `docker` to run (a test's stand-in records what it's asked), this machine's id
/// (`machine_id` — made where it isn't yet: a test passes none), and the UI-state socket to ask for open
/// sessions (`tarae_sessions`).
fn prune_all_with(
    docker: &std::ffi::OsStr,
    machine: std::result::Result<String, String>,
    yes: bool,
    mine: bool,
    ui: Option<std::ffi::OsString>,
) -> Result<()> {
    let docker_is = docker_self(docker);
    let local = local(mine, docker_is.as_ref().map(|(n, i)| (n.as_str(), i.as_str())));
    let listed = df(&mut Command::new(docker))?;
    let images = Command::new(docker)
        .args(["image", "ls", "--format", IMAGES_FORMAT])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let sessions = tarae_sessions(ui);
    if let Err(why) = &sessions {
        println!("tarae's open sessions not asked ({why}): a checkout is live by its folder alone");
    }
    let open = sessions.unwrap_or_default();
    let (all, kept) = everything(&listed, &images, machine.as_deref().ok(), local.is_ok(), |c| {
        live(c, &open, checkout_there)
    });
    if let Err(why) = &local {
        println!("volumes and images with {why}");
    }
    for line in &kept {
        println!("{line}");
    }
    if all.is_empty() {
        println!("nothing of this repository's to drop");
        return Ok(());
    }
    println!("{} this repository's own:", if yes { "dropping" } else { "would drop" });
    for line in dropped_list(&all) {
        println!("{line}");
    }
    if !yes {
        let total = gb(all.iter().map(|d| d.size).sum());
        println!("run again with --yes to drop these {}, {total}", all.len());
        return Ok(());
    }
    for d in &all {
        let rm = Command::new(docker).args([d.kind, "rm", d.name.as_str()]).output();
        match rm {
            Ok(o) if o.status.success() => eprintln!("dropped {} {}", d.kind, d.name),
            _ => eprintln!("kept {} {} for now: Docker didn't drop it (in use?)", d.kind, d.name),
        }
    }
    Ok(())
}

/// `cargo xtask linux prune [--all] [--yes] [--mine]` — `--mine`: this Docker's volumes without a machine id
/// (and the images) are this machine's (`local`); `--yes`: what it lists is dropped (`--all`'s, or the landed
/// checkouts' targets).
pub fn prune_cmd(args: &[String]) -> Result<()> {
    let (all, yes, mine) = prune_args(args)?;
    match all {
        true => prune_all(yes, mine),
        false => prune(mine, yes),
    }
}

/// `prune_cmd`'s words: `--all`, `--yes`, `--mine` — any other a usage error.
fn prune_args(args: &[String]) -> std::result::Result<(bool, bool, bool), String> {
    const USAGE: &str = "usage: cargo xtask linux prune [--all] [--yes] [--mine]";
    if args.iter().any(|a| !["--all", "--yes", "--mine"].contains(&a.as_str())) {
        return Err(USAGE.into());
    }
    let has = |f: &str| args.iter().any(|a| a == f);
    Ok((has("--all"), has("--yes"), has("--mine")))
}

/// A checkout whose work is all in its base: its target volume is only a build to make again.
#[derive(Debug, PartialEq)]
enum Landed {
    /// Landed: why, in words (`s1 is in main, nothing changed`)
    In(String),
    /// Not: work of its own there, or the base's own checkout
    Not,
    /// Can't tell (git couldn't read it — tarae's sandbox reads no sibling checkout): kept, said why
    Unknown(String),
}

/// Whether `checkout` landed, as `:session-end` reads nothing to lose (`worktree::Ending::quiet`): nothing
/// changed (`git status`, untracked files too) and its HEAD in the base — the repository's main branch, as
/// tarae's sessions find it (`origin/HEAD`'s, else `main`, else `master`). The base's own checkout never (its
/// target is the base's); a squash merge's branch reads as not landed (its commits aren't the base's).
fn landed(checkout: &Path) -> Landed {
    // (read only: no optional locks — status would refresh the index — and no fsmonitor started)
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .output()
    };
    let text = |args: &[&str]| match git(args) {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("git failed").to_string()),
        Err(e) => Err(format!("git: {e}")),
    };
    let local = |b: &str| text(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_ok();
    let remote = text(&["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        .ok()
        .and_then(|r| r.strip_prefix("origin/").map(str::to_string));
    let Some(base) = remote.into_iter().chain(["main".into(), "master".into()]).find(|b| local(b)) else {
        return Landed::Unknown("no main branch there (origin/HEAD's, main or master)".into());
    };
    let branch = text(&["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    if branch.as_deref() == Some(base.as_str()) {
        return Landed::Not;
    }
    match text(&["status", "--porcelain", "--untracked-files=all"]) {
        Ok(s) if !s.is_empty() => return Landed::Not,
        Ok(_) => {}
        Err(why) => return Landed::Unknown(why),
    }
    match git(&["merge-base", "--is-ancestor", "HEAD", &format!("refs/heads/{base}")])
        .map(|o| o.status.code())
    {
        Ok(Some(0)) => {
            let of = branch.unwrap_or_else(|| "its HEAD".into());
            Landed::In(format!("{of} is in {base}, nothing changed"))
        }
        Ok(Some(1)) => Landed::Not,
        Ok(_) => Landed::Unknown(format!("git merge-base couldn't compare it with {base}")),
        Err(e) => Landed::Unknown(format!("git: {e}")),
    }
}

/// The target volumes of checkouts that landed (`landed`): this machine's, as `prunable` judges whose, their
/// checkout there, not dropped already (`gone`), never the checkout running this (`this`). Each (name, checkout,
/// why it landed) — and a line for each checkout it couldn't tell of (kept).
fn landed_targets(
    volumes: &[Volume],
    ours: Option<&str>,
    local: bool,
    gone: &[String],
    this: &Path,
    landed: impl Fn(&Path) -> Landed,
) -> (Vec<(String, String, String)>, Vec<String>) {
    let (mut found, mut untold) = (Vec::new(), Vec::new());
    for v in volumes {
        let judged = match whose(v, ours) {
            Whose::Ours => true,
            Whose::NoId => local,
            Whose::Other => false,
        };
        let at = Path::new(&v.checkout);
        if !judged || v.checkout.is_empty() || gone.contains(&v.name) || at == this {
            continue;
        }
        match landed(at) {
            Landed::In(why) => found.push((v.name.clone(), v.checkout.clone(), why)),
            Landed::Not => {}
            Landed::Unknown(why) => {
                untold.push(format!("kept {}: can't tell whether {} landed ({why})", v.name, v.checkout))
            }
        }
    }
    (found, untold)
}

/// Whether a volume's checkout is there — or can't be told: only a folder that's surely not there is gone. A
/// run that can't see it (tarae's sandbox reads no sibling checkout: `Operation not permitted`) keeps its
/// volume — measured 2026-10-06: two live sessions' targets dropped as "gone" from inside the sandbox.
fn checkout_there(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(m) => m.is_dir(),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Drops what `prunable` says for this machine (`ours`, on a Docker of its own or not: `local`), each said;
/// one still in use (a run going) stays. What was listed, and what went.
fn prune_volumes(ours: Option<&str>, local: bool, shared: bool) -> (Vec<Volume>, Vec<String>) {
    let Ok(out) = Command::new("docker")
        .args(["volume", "ls", "--filter", "name=tarae-target-", "--format", &ls_format()])
        .output()
    else {
        return (Vec::new(), Vec::new());
    };
    let listed = volumes(&String::from_utf8_lossy(&out.stdout));
    let mut gone = Vec::new();
    for name in prunable(&listed, ours, local, checkout_there, shared) {
        let rm = Command::new("docker").args(["volume", "rm", &name]).output();
        match rm {
            Ok(o) if o.status.success() => {
                gone.push(name.clone());
                let of =
                    listed.iter().find(|v| v.name == name).map(|v| v.checkout.as_str()).unwrap_or_default();
                match of.is_empty() {
                    true => eprintln!("dropped {name} (the target every checkout shared)"),
                    false => eprintln!("dropped {name} ({of} is gone)"),
                }
            }
            _ => eprintln!("kept {name} for now: Docker didn't drop it (a run using it?)"),
        }
    }
    (listed, gone)
}

/// The image for `pin`: `rust:<major.minor>` — rustup in it then takes `rust-toolchain.toml`'s exact version
/// (installing it, into the rustup volume, when the image's patch release differs). The exact tag would be a
/// pull of its own for every patch release, for the same compiler.
pub(crate) fn image(pin: &str) -> String {
    let minor: Vec<&str> = pin.split('.').take(2).collect();
    format!("rust:{}", minor.join("."))
}

/// What runs in the container: the clone (with what isn't committed yet) — or, streamed, the checkout as it is
/// made a repository of one commit — then the checks.
fn script(tree: Tree, what: &What) -> String {
    // (home made first: `--env HOME=/work` names one the clone hasn't made yet)
    // (on any exit, the checks' failing too: the target volume's disk, for `forwarded` — an EXIT trap keeps
    // the script's own exit status)
    let mut s = format!("trap 'df -P /target 2>/dev/null | sed -n \"2s/^/{DISK_MARK}/p\"' EXIT\n");
    // (one stream: cargo's "to rerun pass" lines are stderr's, and `forwarded` reads them for the summary)
    s.push_str("exec 2>&1\n");
    s.push_str(
        "mkdir -p \"$HOME\"\n\
         git config --global --add safe.directory '*'\n\
         mkdir -p /target/tarae-second-mount && chmod 1777 /target/tarae-second-mount\n",
    );
    s.push_str(match tree {
        Tree::Mounted(_) => {
            "git clone -q /src /work/tarae && cd /work/tarae\n\
             git -C /src diff --binary HEAD | git apply --allow-empty\n\
             git -C /src ls-files --others --exclude-standard -z | (cd /src && tar --null -T - -cf -) | tar -xf -\n"
        }
        Tree::Streamed => {
            "mkdir -p /work/tarae && cd /work/tarae\n\
             tar --no-same-owner -xf -\n\
             git init -q && git add -A\n\
             git -c user.name=xtask -c user.email=xtask@localhost commit -q --no-verify -m 'the checkout as it is'\n"
        }
    });
    s.push_str("echo \"toolchain: $(rustc --version)\"\n");
    if !matches!(
        what,
        What::Test { .. }
            | What::Installed { .. }
            | What::AsUser { .. }
            | What::Keep { .. }
            | What::Kept { .. }
    ) {
        // (every package: netlink/ is Linux code, linted only here)
        s.push_str("cargo clippy --locked --workspace --all-targets -- -D warnings\n");
        s.push_str(
            "cargo clippy --locked --workspace --all-targets --features bundled-grammars -- -D warnings\n",
        );
    }
    match what {
        What::Clippy => {}
        What::Installed { args } => {
            s.push_str(&format!(
                "exe=$(cargo test --locked --bin tarae --no-run 2>&1 | tee /dev/stderr | sed -n '{EXE_SED}')\n\
                 test -n \"$exe\" || {{ echo 'cargo named no test binary'; exit 1; }}\n\
                 install -o root -g root -m 0755 \"$exe\" /usr/bin/tarae\n\
                 echo \"the tests run as /usr/bin/tarae ($(stat -c '%U:%G %a' /usr/bin/tarae))\"\n",
            ));
            let args: Vec<String> = args.iter().map(|a| quoted(a)).collect();
            s.push_str(&format!("CARGO_MANIFEST_DIR=/work/tarae /usr/bin/tarae {}\n", args.join(" ")));
        }
        What::Keep { args } => {
            let args: Vec<String> = args.iter().map(|a| quoted(a)).collect();
            s.push_str(&format!(
                "exe=$(cargo test --locked --no-run {} 2>&1 | tee /dev/stderr | sed -n '{EXE_SED}')\n\
                 test -n \"$exe\" || {{ echo 'cargo named no test binary'; exit 1; }}\n\
                 mkdir -p \"$(dirname {KEPT})\" && cp \"$exe\" {KEPT}.new && mv {KEPT}.new {KEPT}\n\
                 echo \"kept $exe as {KEPT}\"\n",
                args.join(" ")
            ));
        }
        What::Kept { args } => {
            let args: Vec<String> = args.iter().map(|a| quoted(a)).collect();
            s.push_str(&format!(
                "test -x {KEPT} || {{ echo 'no test binary kept yet: `test --keep` first'; exit 1; }}\n\
                 env -u CARGO_TARGET_DIR CARGO_MANIFEST_DIR=/work/tarae {KEPT} {}\n",
                args.join(" ")
            ));
        }
        What::AsUser { args } => {
            // (the binary copied where the user reads it; the clone — a throwaway, where tests write under `target/` —
            // given to the user; its uid and gid set by setpriv, every capability gone with root's uid — the
            // environment as `--env` gave it, but its home)
            s.push_str(&format!(
                "exe=$(cargo test --locked --bin tarae --no-run 2>&1 | tee /dev/stderr | sed -n '{EXE_SED}')\n\
                 test -n \"$exe\" || {{ echo 'cargo named no test binary'; exit 1; }}\n\
                 id -u {AS_USER} >/dev/null 2>&1 || useradd --create-home {AS_USER}\n\
                 chown -R {AS_USER}: /work/tarae\n\
                 install -o root -g root -m 0755 \"$exe\" /usr/local/bin/tarae-tests\n\
                 echo \"the tests run as {AS_USER} ($(id {AS_USER}))\"\n",
            ));
            let args: Vec<String> = args.iter().map(|a| quoted(a)).collect();
            s.push_str(&format!(
                "HOME=/home/{AS_USER} USER={AS_USER} CARGO_MANIFEST_DIR=/work/tarae \
                 setpriv --reuid={AS_USER} --regid={AS_USER} --init-groups /usr/local/bin/tarae-tests {}\n",
                args.join(" ")
            ));
        }
        // (`--no-fail-fast`: every test binary runs — a unit test failing hid e2e's, f22 o)
        What::Both => s.push_str("cargo test --locked --no-fail-fast --workspace\n"),
        What::Test { args, repeat } => {
            let test = std::iter::once("cargo test --locked --no-fail-fast".to_string())
                .chain(args.iter().map(|a| quoted(a)))
                .collect::<Vec<_>>()
                .join(" ");
            match repeat {
                1 => s.push_str(&format!("{test}\n")),
                n => s.push_str(&format!(
                    "n=0\nwhile [ $n -lt {n} ]; do n=$((n+1)); echo \"run $n of {n}\"; \
                     {test} || {{ echo \"failed on run $n of {n}\"; exit 1; }}; done\n\
                     echo \"all {n} runs passed\"\n"
                )),
            }
        }
    }
    s
}

/// Where `test --keep` keeps the test binary: in the checkout's own target volume, so only its checkout's runs
/// find it.
const KEPT: &str = "/target/kept/tarae-tests";

/// The binary `cargo test --no-run` built for tarae's own tests, from its line `Executable unittests src/main.rs
/// (<path>)` — a `sed -n` expression.
const EXE_SED: &str = r"s/^ *Executable unittests src\/main.rs (\(.*\))$/\1/p";

/// `a` as one word for sh, whatever is in it.
pub(crate) fn quoted(a: &str) -> String {
    format!("'{}'", a.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scene of `a_signal_ends_the_checks_container`, in a process of its own: as `run` ends —
    /// `stop_on_signal` for the container, then `docker run --name …` waited for — with the stand-in `docker` its
    /// parent put first on PATH.
    #[test]
    #[ignore = "run by a_signal_ends_the_checks_container in a process of its own"]
    fn signalled_scene() {
        let Ok(name) = std::env::var("XTASK_SCENE_NAME") else { return };
        stop_on_signal(name.clone());
        let st = Command::new("docker").args(["run", "--rm", "--name", &name, "rust:1"]).status();
        println!("scene: docker run ended unsignalled: {st:?}");
    }

    /// A signal to xtask — `timeout`'s SIGTERM, Ctrl-C's SIGINT — ends the checks' container by its name (`docker
    /// kill`), then xtask itself, 128 + the signal; killed alone, xtask left `docker run`'s client and the container
    /// going (6f′). A stand-in `docker` (a script in the test's own folder, first on PATH) logs its calls; its `run`
    /// waits until its `kill` comes. The scene runs in a process of its own (signals, handlers: not this binary's).
    #[test]
    fn a_signal_ends_the_checks_container() {
        let dir = std::env::temp_dir().join(format!("xtask-signal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("docker");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"$*\" >> \"$FAKE_DIR/log\"\ncase \"$1\" in\n  run) sleep 30 & echo $! > \"$FAKE_DIR/run.pid\"; \
             wait ;;\n  kill) kill \"$(cat \"$FAKE_DIR/run.pid\")\" ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        let log = dir.join("log");
        for sig in [libc::SIGTERM, libc::SIGINT] {
            let name = format!("xtask-test-{sig}-checks");
            let _ = std::fs::remove_file(&log);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "linux::tests::signalled_scene", "--ignored", "--nocapture"])
                .env("PATH", &path)
                .env("FAKE_DIR", &dir)
                .env("XTASK_SCENE_NAME", &name)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let logged = || std::fs::read_to_string(&log).unwrap_or_default();
            let by = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !logged().contains(&format!("run --rm --name {name}")) {
                assert!(std::time::Instant::now() < by, "the scene's docker run never came: {}", logged());
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // SAFETY: a signal to a child of ours.
            unsafe { libc::kill(child.id() as i32, sig) };
            let by = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let st = loop {
                if let Some(st) = child.try_wait().unwrap() {
                    break st;
                }
                if std::time::Instant::now() > by {
                    let _ = child.kill();
                    panic!("signal {sig}: the scene didn't end within 10 s — {}", logged());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            assert_eq!(st.code(), Some(128 + sig), "signal {sig}: ended so — {}", logged());
            assert!(
                logged().lines().any(|l| l == format!("kill {name}")),
                "the container ended by name: {}",
                logged()
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--userns` lifts the container's own seccomp, AppArmor and masked paths (tier own), `--userns=masked`
    /// keeps `/proc` masked (own, no PID namespace), and without it the container is Docker's defaults: no `--security-opt`.
    /// Its options go after `--cap-add SYS_PTRACE`, before the image, with the rest kept.
    #[test]
    fn userns_loosens_only_the_container() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let full = args(&["--userns", "test", "sandbox"]);
        assert_eq!(userns(&full).unwrap(), (Some(Userns::Full), &full[1..]));
        assert_eq!(userns(&args(&["--userns=masked", "clippy"])).unwrap().0, Some(Userns::Masked));
        let none = args(&["test"]);
        assert_eq!(userns(&none).unwrap(), (None, &none[..]));
        assert!(userns(&args(&["--userns=all"])).is_err(), "only the two");
        let base = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Both);
        let opts = |a: &[String]| -> Vec<String> {
            a.windows(2).filter(|w| w[0] == "--security-opt").map(|w| w[1].clone()).collect()
        };
        assert_eq!(opts(&base), Vec::<String>::new(), "Docker's defaults: {base:?}");
        assert_eq!(with_userns(base.clone(), None), base);
        let full = with_userns(base.clone(), Some(Userns::Full));
        assert_eq!(opts(&full), ["seccomp=unconfined", "apparmor=unconfined", "systempaths=unconfined"]);
        assert_eq!(
            opts(&with_userns(base.clone(), Some(Userns::Masked))),
            ["seccomp=unconfined", "apparmor=unconfined"]
        );
        assert_eq!(&full[..4], ["run", "--rm", "--cap-add", "SYS_PTRACE"]);
        assert_eq!(full[full.len() - 4..], base[base.len() - 4..], "the image and its script last");
        assert_eq!(full.len(), base.len() + 6);
        assert!(!full.iter().any(|a| a == "--privileged" || a.contains("SYS_ADMIN")), "{full:?}");
    }

    /// `--docker-default` (6f′'s M8b): the container as `docker run` makes one — `--cap-add SYS_PTRACE` taken out
    /// and nothing else changed (no `--security-opt`, the image and its script as they were), leading or in
    /// `--userns`'s place; without it the capability stays.
    /// `--docker-default` expects tier offline unless an `--env` names the tier; without it nothing's added.
    #[test]
    fn docker_default_expects_offline_unless_told_otherwise() {
        let s = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(expect_offline(s(&["A=1"]), true), s(&["A=1", "TARAE_EXPECT_TIER=offline"]));
        assert_eq!(expect_offline(s(&["TARAE_EXPECT_TIER=host"]), true), s(&["TARAE_EXPECT_TIER=host"]));
        assert_eq!(expect_offline(s(&["A=1"]), false), s(&["A=1"]));
    }

    #[test]
    fn docker_default_takes_only_sys_ptrace_away() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let on = args(&["--docker-default", "test", "sandbox"]);
        assert_eq!(docker_default(&on), (true, &on[1..]));
        let off = args(&["test"]);
        assert_eq!(docker_default(&off), (false, &off[..]));
        let base = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Both);
        assert_eq!(without_ptrace(base.clone(), false), Ok(base.clone()));
        let stock = without_ptrace(base.clone(), true).unwrap();
        assert_eq!(&stock[..2], ["run", "--rm"]);
        assert_eq!((stock.len(), &stock[2..]), (base.len() - 2, &base[4..]), "only the capability gone");
        assert!(!stock.iter().any(|a| a.contains("SYS_PTRACE") || a == "--security-opt"), "{stock:?}");
    }

    /// `--docker-default` takes the capability by value — wherever it stands among the options, in either form —
    /// and fails, saying so, where an option still gives it: a run that kept it would test the full way under a
    /// line saying `pidfd_getfd` is refused (the review: it took it only from one place, silently).
    #[test]
    fn docker_default_fails_where_sys_ptrace_stays() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let tail = ["rust:1", "sh", "-ec", "echo SYS_PTRACE"];
        let at = |opts: &[&str]| args(&[&["run", "--rm"], opts, &tail[..]].concat());
        // (moved: after other options; and the `=` form)
        let moved = at(&["-e", "X=1", "--cap-add", "SYS_PTRACE", "-v", "a:b"]);
        assert_eq!(without_ptrace(moved, true), Ok(at(&["-e", "X=1", "-v", "a:b"])), "the script left alone");
        assert_eq!(without_ptrace(at(&["--cap-add=SYS_PTRACE"]), true), Ok(at(&[])));
        for kept in [
            &["--cap-add", "ALL"][..],
            &["--cap-add", "cap_sys_ptrace"],
            &["--cap-add=all"],
            &["--privileged"],
        ] {
            let e = without_ptrace(at(kept), true).unwrap_err();
            assert!(e.contains("still gives SYS_PTRACE"), "{kept:?}: {e}");
        }
    }

    #[test]
    fn the_pinned_toolchain_and_its_image() {
        assert_eq!(pin("[toolchain]\nchannel = \"1.99.0\"\ncomponents = [\"clippy\"]\n").unwrap(), "1.99.0");
        assert!(pin("[toolchain]\nprofile = \"minimal\"\n").unwrap_err().contains("no [toolchain] channel"));
        // (this repository's own pin reads)
        let ours = std::fs::read_to_string(root().join("rust-toolchain.toml")).unwrap();
        assert!(pin(&ours).unwrap().starts_with("1."));
    }

    /// The tree goes in read-only, the run is in a clone made inside (the changes and untracked files
    /// brought over), on the pinned image, with caches per version — and `clippy`/`test` run only theirs.
    #[test]
    fn the_tree_is_read_only_and_the_checks_run_in_a_clone() {
        let a = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Both);
        assert!(a.windows(2).any(|w| w == ["-v", "/r/tarae:/src:ro"]), "{a:?}");
        assert!(a.windows(2).any(|w| w == ["--cap-add", "SYS_PTRACE"]), "the supervisor's tests run: {a:?}");
        assert!(
            !a.iter().any(|v| v.starts_with("/r/tarae:") && !v.ends_with(":ro")),
            "no writable mount: {a:?}"
        );
        assert!(!a.contains(&"--platform".to_string()), "the host's own: {a:?}");
        let target = format!("{}:/target", target_volume(Path::new("/r/tarae"), "1.99.0", None));
        assert!(a.contains(&target) && a.contains(&"rust:1.99".to_string()), "{a:?}");
        assert_eq!((image("1.100.2"), image("1.99")), ("rust:1.100".to_string(), "rust:1.99".to_string()));
        let s = a.last().unwrap();
        assert!(s.contains("git clone -q /src /work/tarae && cd /work/tarae"), "{s}");
        // (`--binary`: a changed or removed binary file, a fuzz seed's, applies too)
        assert!(
            s.contains("git -C /src diff --binary HEAD | git apply") && s.contains("ls-files --others"),
            "{s}"
        );
        assert!(
            s.contains("--features bundled-grammars -- -D warnings")
                && s.contains("cargo test --locked --no-fail-fast --workspace\n"),
            "xtask's tests too: {s}"
        );
        let clippy = script(Tree::Mounted(None), &What::Clippy);
        assert!(clippy.contains("cargo clippy") && !clippy.contains("cargo test"));
        let test = script(Tree::Mounted(None), &What::Test { args: Vec::new(), repeat: 1 });
        assert!(test.contains("cargo test") && !test.contains("cargo clippy"));
    }

    /// `test`'s arguments go to `cargo test` as words, whatever is in them; `--repeat N` runs it N times in
    /// the one container and stops at the first that fails.
    #[test]
    fn test_takes_cargo_tests_arguments_and_a_repeat() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let once = script(Tree::Mounted(None), &what(&args(&["test", "recovery", "--", "it's"])).unwrap());
        assert!(once.contains("cargo test --locked --no-fail-fast 'recovery' '--' 'it'\\''s'\n"), "{once}");
        assert!(!once.contains("while"), "{once}");
        let looped =
            script(Tree::Mounted(None), &what(&args(&["test", "--repeat", "20", "recovery"])).unwrap());
        assert!(
            looped.contains("while [ $n -lt 20 ]")
                && looped.contains("cargo test --locked --no-fail-fast 'recovery' ||")
        );
        assert!(looped.contains("exit 1") && looped.contains("all 20 runs passed"), "{looped}");
        assert_eq!(what(&args(&["test"])), Ok(What::Test { args: Vec::new(), repeat: 1 }));
        assert!(what(&args(&["test", "--repeat", "0"])).unwrap_err().contains("--repeat 0: a count"));
        assert!(what(&args(&["clippy", "x"])).unwrap_err().starts_with("usage:"));
    }

    /// `test --installed` (6h): tarae's own tests run from `/usr/bin/tarae` — the binary cargo built, put there
    /// root's, as the `.deb` does — so a profile on the Docker's machine attaches to it by path; the arguments are
    /// the test binary's, as words. Neither clippy nor `cargo test` itself runs the tests.
    #[test]
    fn installed_runs_the_tests_from_the_packaged_path() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let w = what(&args(&["test", "--installed", "sandbox::", "--exact", "it's"])).unwrap();
        assert_eq!(w, What::Installed { args: args(&["sandbox::", "--exact", "it's"]) });
        let s = script(Tree::Mounted(None), &w);
        assert!(s.contains("cargo test --locked --bin tarae --no-run"), "built, not run: {s}");
        assert!(s.contains("install -o root -g root -m 0755 \"$exe\" /usr/bin/tarae\n"), "{s}");
        assert!(s.contains("/usr/bin/tarae 'sandbox::' '--exact' 'it'\\''s'\n"), "run from there: {s}");
        assert!(!s.contains("cargo clippy") && !s.contains("cargo test --locked\n"), "{s}");
        // (cargo's lines through the script's sed: the binary's path alone, not the other targets')
        assert!(s.contains(&format!("sed -n '{EXE_SED}'")), "{s}");
        let said = "   Compiling tarae v0.1.0 (/work/tarae)\n    Finished `test` profile\n  \
                    Executable unittests src/main.rs (/target/debug/deps/tarae-0123abcd)\n  \
                    Executable tests/e2e.rs (/target/debug/deps/e2e-4567)\n";
        let mut sed = Command::new("sed")
            .args(["-n", EXE_SED])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut sed.stdin.take().unwrap(), said.as_bytes()).unwrap();
        let out = sed.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "/target/debug/deps/tarae-0123abcd\n");
    }

    /// `test --as-user` (7d): tarae's own tests built as root, then run as a user made in the container — through
    /// `setpriv`, which leaves the environment (`--env`'s) as it is but its home — with the test binary's arguments
    /// as words. Never as root, never through `cargo test`.
    #[test]
    fn as_user_runs_the_tests_as_a_user_of_the_containers() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let w = what(&args(&["test", "--as-user", "sandbox::", "--exact", "it's"])).unwrap();
        assert_eq!(w, What::AsUser { args: args(&["sandbox::", "--exact", "it's"]) });
        let s = script(Tree::Mounted(None), &w);
        assert!(s.contains("cargo test --locked --bin tarae --no-run"), "built, not run: {s}");
        assert!(s.contains(&format!("useradd --create-home {AS_USER}\n")), "{s}");
        assert!(s.contains(&format!("chown -R {AS_USER}: /work/tarae\n")), "the clone the user's: {s}");
        let run = format!(
            "setpriv --reuid={AS_USER} --regid={AS_USER} --init-groups /usr/local/bin/tarae-tests \
             'sandbox::' '--exact' 'it'\\''s'\n"
        );
        assert!(s.contains(&run) && s.contains(&format!("HOME=/home/{AS_USER} ")), "run as the user: {s}");
        assert!(
            !s.contains("--reset-env") && !s.contains("--keep-groups"),
            "--env's kept, root's groups not: {s}"
        );
        assert!(!s.contains("cargo clippy") && !s.contains("cargo test --locked\n"), "{s}");
    }

    /// `--platform linux/<arch>` first: `docker run` on it, with a target and toolchains of its own (an x86_64
    /// build in the aarch64 one would be rebuilt both ways each time); the crates' sources stay shared. What
    /// comes after it is read as without it; a platform missing, or not Linux, is refused.
    #[test]
    fn a_platform_runs_on_its_own_target_and_toolchains() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (p, w) = ask(&args(&["--platform", "linux/amd64", "test", "--repeat", "3", "reads"])).unwrap();
        assert_eq!(p.as_deref(), Some("linux/amd64"));
        assert_eq!(w, What::Test { args: args(&["reads"]), repeat: 3 });
        assert_eq!(
            ask(&args(&["--platform", "linux/arm/v7"])).unwrap(),
            (Some("linux/arm/v7".into()), What::Both)
        );
        assert_eq!(ask(&args(&["clippy"])).unwrap(), (None, What::Clippy));
        assert!(ask(&args(&["--platform"])).unwrap_err().contains("which one?"));
        for bad in ["amd64", "linux/", "windows/amd64", "linux/amd64;rm"] {
            let e = ask(&args(&["--platform", bad, "clippy"])).unwrap_err();
            assert!(e.contains(&format!("--platform {bad}: linux/<arch>")), "{e}");
        }
        assert!(
            ask(&args(&["test", "--platform", "linux/amd64"])).is_ok_and(|(p, _)| p.is_none()),
            "only first"
        );
        let r = Path::new("/r/tarae");
        let a = docker_args(r, Tree::Mounted(None), "1.99.0", Some("linux/amd64"), &What::Both);
        assert!(a.windows(2).any(|w| w == ["--platform", "linux/amd64"]), "{a:?}");
        assert!(a.windows(2).any(|w| w == ["--cap-add", "SYS_PTRACE"]), "the cap too: {a:?}");
        let image = a.iter().position(|v| v == "rust:1.99").unwrap();
        assert!(
            a.iter().position(|v| v == "--platform").is_some_and(|p| p < image),
            "before the image: {a:?}"
        );
        let amd = target_volume(r, "1.99.0", Some("linux/amd64"));
        assert_ne!(amd, target_volume(r, "1.99.0", None));
        assert_ne!(amd, target_volume(r, "1.99.0", Some("linux/arm64")));
        assert_eq!(amd.len(), target_volume(r, "1.99.0", None).len(), "the same form: pruned alike");
        assert!(a.contains(&format!("{amd}:/target")), "{a:?}");
        // (and again, the sandbox's tests reaching a fake home through it — the folder made where both see it)
        assert!(a.contains(&format!("{amd}:/run/host/target")), "{a:?}");
        let second = format!("TARAE_TEST_SECOND_MOUNT={SECOND_MOUNT}");
        assert!(a.windows(2).any(|w| w == ["-e", second.as_str()]), "{a:?}");
        let (one, two) = SECOND_MOUNT.split_once(':').unwrap();
        assert_eq!(one.strip_prefix("/target"), two.strip_prefix(SECOND_TARGET), "one folder, two ways");
        assert!(a.last().unwrap().contains(&format!("chmod 1777 {one}\n")), "{a:?}");
        assert!(a.contains(&"tarae-rustup-1.99.0-linux-amd64:/usr/local/rustup".to_string()), "{a:?}");
        assert!(a.contains(&"tarae-registry:/usr/local/cargo/registry".to_string()), "{a:?}");
        assert_eq!(
            create_args(r, None, "1.99.0", Some("linux/amd64"))[3..],
            ["tarae.checkout=/r/tarae", amd.as_str()]
        );
    }

    /// `--env NAME=VALUE` first, as often as needed: each a `-e` of `docker run`'s, before the image — the
    /// rest read as without them; a name that isn't one, or a missing value, is refused.
    #[test]
    fn envs_go_into_the_container() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let given = args(&["--env", "HOME=/work", "--env", "A_1=x=y", "test", "--bin", "tarae"]);
        let (set, rest) = envs(&given).unwrap();
        assert_eq!(
            (set.clone(), rest.to_vec()),
            (args(&["HOME=/work", "A_1=x=y"]), args(&["test", "--bin", "tarae"]))
        );
        let a = with_envs(
            docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Clippy),
            &set,
        );
        let image = a.iter().position(|v| v == "rust:1.99").unwrap();
        assert_eq!(a[image - 4..image], args(&["-e", "HOME=/work", "-e", "A_1=x=y"]), "{a:?}");
        assert_eq!(a[image + 1..image + 3], args(&["sh", "-ec"]));
        assert_eq!(envs(&args(&["clippy"])).unwrap(), (Vec::new(), &args(&["clippy"])[..]));
        for bad in [&["--env", "=x"][..], &["--env", "A B=x"], &["--env", "noequals"], &["--env"]] {
            assert!(envs(&args(bad)).is_err_and(|e| e.contains("--env")), "{bad:?}");
        }
    }

    /// `--image NAME` takes the pinned image's place and nothing else's: the script, the volumes and `--env`'s
    /// values are as without it — the rest read as without it; no name, or one Docker would take for an option or
    /// a path, is refused.
    #[test]
    fn an_image_named_takes_only_the_images_place() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let given = args(&["--image", "tarae-m5:1", "test", "--bin", "tarae"]);
        let (named, rest) = image_named(&given).unwrap();
        assert_eq!((named.as_deref(), rest), (Some("tarae-m5:1"), &args(&["test", "--bin", "tarae"])[..]));
        let plain = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Clippy);
        let named = with_envs(with_image(plain.clone(), named.as_deref()), &args(&["A=rust:1.99"]));
        let at = named.iter().position(|v| v == "tarae-m5:1").unwrap();
        assert_eq!(named[at + 1..at + 3], args(&["sh", "-ec"]), "{named:?}");
        assert_eq!(named[at - 2..at], args(&["-e", "A=rust:1.99"]), "{named:?}");
        let mut back = named.clone();
        back.splice(at - 2..at, []);
        back[at - 2] = "rust:1.99".into();
        assert_eq!(back, plain);
        assert_eq!(with_image(plain.clone(), None), plain);
        assert_eq!(image_named(&args(&["clippy"])).unwrap(), (None, &args(&["clippy"])[..]));
        for bad in [&["--image"][..], &["--image", "--rm"], &["--image", "/x"], &["--image", "a b"]] {
            assert!(image_named(&args(bad)).is_err_and(|e| e.contains("--image")), "{bad:?}");
        }
    }

    /// `test --keep` builds once and keeps the binary in the target volume; `test --kept` runs it with no cargo at
    /// all (each container's clone is new, so cargo would build the crate again) — none kept: it says so.
    #[test]
    fn a_kept_binary_is_built_once_and_run_without_cargo() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let keep = what(&args(&["test", "--keep", "--profile", "compat-tests", "--bin", "tarae"])).unwrap();
        assert_eq!(keep, What::Keep { args: args(&["--profile", "compat-tests", "--bin", "tarae"]) });
        let s = script(Tree::Mounted(None), &keep);
        assert!(s.contains("cargo test --locked --no-run '--profile' 'compat-tests' '--bin' 'tarae'"), "{s}");
        assert!(s.contains(&format!("mv {KEPT}.new {KEPT}")) && !s.contains("clippy"), "{s}");
        let kept = what(&args(&["test", "--kept", "--ignored", "--exact", "x"])).unwrap();
        let s = script(Tree::Mounted(None), &kept);
        assert!(
            s.contains(&format!(
                "env -u CARGO_TARGET_DIR CARGO_MANIFEST_DIR=/work/tarae {KEPT} '--ignored' '--exact' 'x'"
            )),
            "{s}"
        );
        assert!(
            s.contains("`test --keep` first") && !s.contains("cargo test") && !s.contains("clippy"),
            "{s}"
        );
    }

    /// `--volume` mounts one of compat's volumes, before the image, and nothing else: another volume, a folder of
    /// the machine's, a relative path or one with `..` is refused.
    #[test]
    fn a_volume_named_is_compats_and_mounted_before_the_image() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let given = args(&["--volume", "tarae-compat-go-cache:/root", "test"]);
        let (v, rest) = volume_named(&given).unwrap();
        assert_eq!((v.as_deref(), rest), (Some("tarae-compat-go-cache:/root"), &args(&["test"])[..]));
        let plain = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Clippy);
        let with = with_volume(plain.clone(), v.as_deref());
        let at = with.iter().position(|a| a == "rust:1.99").unwrap();
        assert_eq!(with[at - 2..at], args(&["-v", "tarae-compat-go-cache:/root"]), "{with:?}");
        assert_eq!(with_volume(plain.clone(), None), plain);
        assert_eq!(volume_named(&args(&["test"])).unwrap(), (None, &args(&["test"])[..]));
        for bad in [
            &["--volume"][..],
            &["--volume", "tarae-registry:/root"],
            &["--volume", "/:/root"],
            &["--volume", "tarae-compat-:/root"],
            &["--volume", "tarae-compat-go-cache:root"],
            &["--volume", "tarae-compat-go-cache:/root/../etc"],
            &["--volume", "tarae-compat-go-cache:/root:ro"],
            &["--volume", "tarae-compat-go-cache"],
        ] {
            assert!(volume_named(&args(bad)).is_err_and(|e| e.contains("--volume")), "{bad:?}");
        }
    }

    /// A worktree (a tarae session): the main checkout's git folder goes in read-only, at the path its `.git`
    /// file names — and only then. Found from a real worktree of a temp repository.
    #[test]
    fn a_worktrees_git_folder_goes_in_read_only() {
        let a = docker_args(
            Path::new("/w/s"),
            Tree::Mounted(Some(Path::new("/r/tarae/.git"))),
            "1.99.0",
            None,
            &What::Clippy,
        );
        assert!(a.windows(2).any(|w| w == ["-v", "/r/tarae/.git:/r/tarae/.git:ro"]), "{a:?}");
        assert!(!a.iter().any(|v| v.contains(':') && v.starts_with('/') && !v.ends_with(":ro")), "{a:?}");
        let dir = std::env::temp_dir().join(format!("tarae-xtask-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let (main, wt) = (dir.join("main"), dir.join("wt"));
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        assert_eq!(common_git_dir(&main), None, "a plain checkout's own .git is in the tree");
        assert_eq!(common_git_dir(&wt), Some(main.join(".git")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Docker that can't see the checkout (another machine's) gets it on stdin: nothing of this machine's is
    /// mounted, and the container makes its repository from what it reads.
    #[test]
    fn a_tree_the_docker_cant_see_goes_in_on_stdin() {
        let a = docker_args(Path::new("/r/tarae"), Tree::Streamed, "1.99.0", None, &What::Clippy);
        assert!(a.contains(&"-i".to_string()), "{a:?}");
        assert!(!a.iter().any(|v| v.starts_with("/r/tarae")), "no mount of the checkout: {a:?}");
        let s = a.last().unwrap();
        assert!(s.contains("tar --no-same-owner -xf -\n") && s.contains("git init -q && git add -A"), "{s}");
        assert!(!s.contains("/src"), "{s}");
        let mounted = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Clippy);
        assert!(!mounted.contains(&"-i".to_string()), "a mounted tree reads no stdin: {mounted:?}");
    }

    /// What goes in on stdin is the checkout as the mounted clone makes it: tracked files with their changes,
    /// untracked ones git doesn't ignore, a link as a link — not a deleted file, an ignored one, nor `.git`.
    #[test]
    fn the_streamed_tree_is_the_checkout_as_it_is() {
        let dir = std::env::temp_dir().join(format!("tarae-xtask-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        git(&["init", "-q"]);
        write(".gitignore", "target/\n");
        write("src/kept.rs", "committed");
        write("changed.rs", "committed");
        write("gone.rs", "committed");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        write("changed.rs", "changed since");
        std::fs::remove_file(dir.join("gone.rs")).unwrap();
        write("new.rs", "untracked");
        std::os::unix::fs::symlink("src/kept.rs", dir.join("link.rs")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        write("target/ignored", "x");
        let bytes = streamed(&dir).unwrap();
        let mut entries: Vec<(String, String)> = tar::Archive::new(&bytes[..])
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let name = e.path().unwrap().display().to_string();
                let body = match e.link_name().unwrap() {
                    Some(to) => format!("-> {}", to.display()),
                    None => {
                        let mut s = String::new();
                        std::io::Read::read_to_string(&mut e, &mut s).unwrap();
                        s
                    }
                };
                (name, body)
            })
            .collect();
        entries.sort();
        let want = [
            (".gitignore", "target/\n"),
            ("changed.rs", "changed since"),
            ("link.rs", "-> src/kept.rs"),
            ("new.rs", "untracked"),
            ("src/kept.rs", "committed"),
        ]
        .map(|(n, b)| (n.to_string(), b.to_string()));
        assert_eq!(entries, want);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each checkout builds in a target of its own (one shared let a session's tests run another's build),
    /// named the same on every run, labelled with its path; registry and rustup stay shared.
    #[test]
    fn each_checkout_has_its_own_target() {
        let (a, b) = (Path::new("/w/tarae/s1"), Path::new("/w/tarae/s2"));
        let name = target_volume(a, "1.99.0", None);
        assert_eq!(name, target_volume(a, "1.99.0", None), "the same on every run");
        assert_ne!(name, target_volume(b, "1.99.0", None), "another checkout, another target");
        assert_ne!(name, target_volume(a, "1.100.0", None), "and per pin");
        assert!(name.starts_with("tarae-target-1.99.0-") && name.len() == "tarae-target-1.99.0-".len() + 16);
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)), "a Docker volume name");
        let made = create_args(a, Some("0123abcd"), "1.99.0", None);
        assert_eq!(
            made,
            [
                "volume",
                "create",
                "--label",
                "tarae.checkout=/w/tarae/s1",
                "--label",
                "tarae.machine=0123abcd",
                name.as_str()
            ]
        );
        assert_eq!(create_args(a, None, "1.99.0", None)[3..], ["tarae.checkout=/w/tarae/s1", name.as_str()]);
        let args =
            docker_args(a, Tree::Mounted(None), "1.99.0", None, &What::Test { args: Vec::new(), repeat: 1 });
        assert!(args.contains(&format!("{name}:/target")), "{args:?}");
        assert!(args.contains(&"tarae-registry:/usr/local/cargo/registry".to_string()), "{args:?}");
        assert!(args.contains(&"tarae-rustup-1.99.0:/usr/local/rustup".to_string()), "{args:?}");
    }

    /// What's dropped: this machine's target whose checkout is gone — its whole repository deleted too — and
    /// one made before machine ids, judged by its path alone; the old shared one only when asked (`prune`);
    /// never a live checkout's, another machine's (through a shared Docker), any with an id when this run has
    /// none, an unlabelled one of another form, nor a volume that isn't a target. `prune` says what it kept
    /// and why.
    #[test]
    fn a_gone_checkouts_target_is_dropped() {
        let listed = volumes(
            "tarae-target-1.99.0-aaaa\t/w/live\tme\n\
             tarae-target-1.99.0-bbbb\t/w/gone\tme\n\
             tarae-target-1.99.0-cccc\t/w/old-label\t\n\
             tarae-target-1.99.0-dddd\t/elsewhere/wt\tthem\n\
             tarae-target-1.99.0-eeee\t/deleted/repo/wt\tme\n\
             tarae-target-1.99.0\t\t\n\
             tarae-target-mine\t\t\n\
             tarae-registry\t\t\n\
             tarae-trace-target-1.99.0\t\t\n\
             k8s-data\t/w/gone\tme\n",
        );
        assert_eq!(listed.len(), 7, "only target volumes: {listed:?}");
        // (/deleted/repo is gone whole — its git folder with it: this machine's all the same)
        let here = |p: &Path| p == Path::new("/w/live");
        let each_run = ["tarae-target-1.99.0-bbbb", "tarae-target-1.99.0-cccc", "tarae-target-1.99.0-eeee"];
        assert_eq!(prunable(&listed, Some("me"), true, here, false), each_run, "each run");
        let mut and_shared = each_run.to_vec();
        and_shared.push("tarae-target-1.99.0");
        assert_eq!(
            prunable(&listed, Some("me"), true, here, true),
            and_shared,
            "prune: the old shared one too"
        );
        assert!(
            prunable(&listed, Some("me"), true, |_| true, false).is_empty(),
            "every checkout there: nothing"
        );
        let none_here = prunable(&listed, Some("me"), true, |_| false, false);
        assert!(!none_here.contains(&"tarae-target-1.99.0-dddd".to_string()), "another's: {none_here:?}");
        assert_eq!(
            prunable(&listed, None, true, here, false),
            ["tarae-target-1.99.0-cccc"],
            "no id of its own: only those without one are judged"
        );
        let old =
            Volume { name: "tarae-target-1.99.0".into(), checkout: String::new(), machine: String::new() };
        assert_eq!(volumes("tarae-target-1.99.0\n"), [old]);
        // (the `--format` names both labels, in the order `volumes` reads them)
        assert_eq!(ls_format(), "{{.Name}}\t{{.Label \"tarae.checkout\"}}\t{{.Label \"tarae.machine\"}}");
        // What prune says besides: another's kept, those without an id judged by path — gone ones not counted
        // (cccc's drop failed here: still there, said)
        let gone = ["tarae-target-1.99.0-bbbb".to_string(), "tarae-target-1.99.0-eeee".to_string()];
        assert_eq!(
            prune_notes(&listed, Ok("me"), Ok(()), &gone),
            [
                "kept 1 target volume another machine made — theirs to judge",
                "1 target volume made before machine ids carry none: judged by their checkout's path alone, \
                 as this machine's (not relabelled)"
            ]
        );
        assert_eq!(prune_notes(&listed, Ok("me"), Ok(()), &gone[..0]).len(), 2);
        let all_gone: Vec<String> = and_shared.iter().map(|s| s.to_string()).collect();
        assert_eq!(prune_notes(&listed, Ok("me"), Ok(()), &all_gone).len(), 1, "the dropped one not counted");
        assert_eq!(
            prune_notes(&listed, Err("denied"), Ok(()), &[]),
            [
                "no machine id (denied): kept 4 target volumes made with one, not judged".to_string(),
                "1 target volume made before machine ids carry none: judged by their checkout's path alone, \
                 as this machine's (not relabelled)"
                    .to_string()
            ]
        );
        assert!(prune_notes(&listed[..1], Ok("me"), Ok(()), &[]).is_empty(), "nothing kept: nothing said");
    }

    /// A run of Claude's names its containers from tarae's prefix (`TARAE_CONTAINER_PREFIX`), so stopping the
    /// run stops them by name (`src/containers.rs`); without one — or one that isn't a container's name —
    /// they're unnamed, as before.
    #[test]
    fn a_run_names_its_containers_from_taraes_prefix() {
        let a = docker_args(Path::new("/r/tarae"), Tree::Mounted(None), "1.99.0", None, &What::Clippy);
        let named = with_name(a.clone(), container_name(Some("tarae-1-s0-r7"), "checks"));
        assert_eq!(named[..3], ["run", "--name", "tarae-1-s0-r7-checks"]);
        assert_eq!(named[3..], a[1..], "the rest as it was");
        assert_eq!(with_name(a.clone(), container_name(None, "checks")), a, "no prefix: unnamed");
        for bad in ["", "-x", "a b", "a/b", "a;rm"] {
            assert_eq!(container_name(Some(bad), "checks"), None, "{bad:?}");
        }
    }

    /// Volumes without a machine id (and the images) are this machine's only when `--mine` says so — nothing a
    /// Docker says tells it: a unix socket may be forwarded from another machine (frictions16: deb12's, over
    /// ssh, was taken for this machine's alone), and a host's name collides (the reviewer: `ubuntu`,
    /// `debian`, `localhost` — a same-named Docker elsewhere had every such volume judged ours). Without
    /// `--mine` they're kept, said so, naming the Docker.
    #[test]
    fn only_mine_judges_volumes_without_an_id() {
        // (this machine's own name, as a Docker running here — or a same-named one elsewhere — would say it)
        let mut buf = [0u8; 256];
        // SAFETY: a buffer of its own length; the name ends with a NUL.
        assert_eq!(unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) }, 0);
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let here = String::from_utf8_lossy(&buf[..end]).into_owned();
        for name in [here.as_str(), "ubuntu", "debian", "localhost", "deb12"] {
            let e = local(false, Some((name, "EF01:2"))).unwrap_err();
            assert_eq!(e, format!("no machine id: kept — --mine takes them (this Docker: {name}, EF01:2)"));
        }
        assert_eq!(local(false, None).unwrap_err(), "no machine id: kept — --mine takes them");
        assert_eq!(local(true, Some(("deb12", "EF01:2"))), Ok(()), "--mine says they are");
        let e = local(false, Some(("deb12", "EF01:2"))).unwrap_err();
        let listed = volumes(
            "tarae-target-1.99.0-aaaa\t/w/gone\tme\n\
             tarae-target-1.99.0-cccc\t/w/old-label\t\n\
             tarae-target-1.99.0-dddd\t/elsewhere/wt\tthem\n",
        );
        let gone_here = |_: &Path| false;
        assert_eq!(prunable(&listed, Some("me"), false, gone_here, false), ["tarae-target-1.99.0-aaaa"]);
        assert!(prunable(&listed, None, false, gone_here, true).is_empty(), "no id of its own: nothing");
        assert_eq!(
            prunable(&listed, Some("me"), true, gone_here, false),
            ["tarae-target-1.99.0-aaaa", "tarae-target-1.99.0-cccc"],
            "--mine: the one without an id too"
        );
        assert_eq!(
            prune_notes(&listed, Ok("me"), Err(&e), &["tarae-target-1.99.0-aaaa".to_string()])[1],
            "1 target volume with no machine id: kept — --mine takes them (this Docker: deb12, EF01:2)"
        );
    }

    /// `prune --all` only lists — what it would drop and how much — unless `--yes`: no `docker volume rm` or
    /// `image rm` runs without it (the reviewer: it dropped right after printing, tens of GB with no chance to
    /// stop). A stand-in `docker` here answers and writes down what it's asked.
    /// `prune` offers the targets of checkouts that landed: this machine's (one without an id with `--mine`),
    /// never another machine's, the checkout running it, nor one dropped as gone; one it can't tell of is kept, said.
    #[test]
    fn landed_checkouts_targets_are_offered_and_only_ours() {
        let v = |name: &str, checkout: &str, machine: &str| Volume {
            name: name.into(),
            checkout: checkout.into(),
            machine: machine.into(),
        };
        let volumes = [
            v("tarae-target-1-a", "/w/landed", "m1"),
            v("tarae-target-1-b", "/w/working", "m1"),
            v("tarae-target-1-c", "/w/unread", "m1"),
            v("tarae-target-1-d", "/w/landed", "other"),
            v("tarae-target-1-e", "/w/landed-no-id", ""),
            v("tarae-target-1-f", "/w/this", "m1"),
            v("tarae-target-1-g", "/w/gone", "m1"),
            v("tarae-target-1.99", "", ""),
        ];
        let landed = |p: &Path| match p.to_str().unwrap() {
            "/w/landed" | "/w/landed-no-id" | "/w/this" | "/w/gone" => Landed::In("s is in main".into()),
            "/w/unread" => Landed::Unknown("Operation not permitted".into()),
            _ => Landed::Not,
        };
        let gone = ["tarae-target-1-g".to_string()];
        let names = |local: bool| {
            let (found, untold) =
                landed_targets(&volumes, Some("m1"), local, &gone, Path::new("/w/this"), landed);
            (found.into_iter().map(|f| f.0).collect::<Vec<_>>(), untold)
        };
        let (found, untold) = names(false);
        assert_eq!(
            found,
            ["tarae-target-1-a"],
            "ours, landed — not another machine's, this checkout's, a gone one's"
        );
        assert_eq!(
            untold,
            ["kept tarae-target-1-c: can't tell whether /w/unread landed (Operation not permitted)"]
        );
        assert_eq!(names(true).0, ["tarae-target-1-a", "tarae-target-1-e"], "--mine: one without an id too");
    }

    /// `landed` on a real repository of the test's own (never this one): a branch with nothing of its own, and one
    /// merged, landed; one with a commit, or a file not committed, not; the base's own checkout never.
    #[test]
    fn a_checkout_landed_when_its_work_is_all_in_main() {
        let dir = std::env::temp_dir().join(format!(
            "tarae-xtask-landed-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        // (made new — never one that's there, cleared)
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.hooksPath=/dev/null"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let main = dir.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let wt = |name: &str| {
            let at = dir.join(name);
            git(&main, &["worktree", "add", "-q", "-b", name, at.to_str().unwrap()]);
            at
        };
        let (fresh, merged, ahead, dirty) = (wt("fresh"), wt("merged"), wt("ahead"), wt("dirty"));
        git(&merged, &["commit", "-q", "--allow-empty", "-m", "merged's"]);
        git(&main, &["merge", "-q", "--ff-only", "merged"]);
        git(&ahead, &["commit", "-q", "--allow-empty", "-m", "its own"]);
        std::fs::write(dirty.join("notes.md"), "not committed\n").unwrap();
        assert_eq!(landed(&fresh), Landed::In("fresh is in main, nothing changed".into()));
        assert_eq!(landed(&merged), Landed::In("merged is in main, nothing changed".into()));
        assert_eq!(landed(&ahead), Landed::Not, "a commit of its own");
        assert_eq!(landed(&dirty), Landed::Not, "a file not committed");
        assert_eq!(landed(&main), Landed::Not, "the base's own checkout");
        assert!(matches!(landed(&dir.join("none")), Landed::Unknown(_)), "no checkout there");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_all_drops_nothing_without_yes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tarae-xtask-standin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (docker, log) = (dir.join("docker"), dir.join("asked"));
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1\" in\n\
             version) echo 27.0 ;;\n\
             info) printf 'standin\\tID:1\\n' ;;\n\
             system) printf 'tarae-registry\\t800MB\\t\\n' ;;\n\
             image) [ \"$2\" = ls ] && printf 'tarae-trace:1.99-0a1b2c3d\\t1.9GB\\n' ;;\n\
             esac\nexit 0\n",
            log.display()
        );
        std::fs::write(&docker, script).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let asked = || std::fs::read_to_string(&log).unwrap_or_default();
        let drops = |a: &str| a.lines().filter(|l| l.contains(" rm ") || l.starts_with("rmi")).count();
        let none = || Err("a test's: none".to_string());
        prune_all_with(docker.as_os_str(), none(), false, true, None).unwrap();
        assert!(asked().contains("system df"), "it listed: {}", asked());
        assert_eq!(drops(&asked()), 0, "nothing dropped without --yes: {}", asked());
        prune_all_with(docker.as_os_str(), none(), true, true, None).unwrap();
        let a = asked();
        assert!(
            a.contains("volume rm tarae-registry") && a.contains("image rm tarae-trace:1.99-0a1b2c3d"),
            "{a}"
        );
        assert_eq!(drops(&a), 2, "{a}");
        let args = |a: &[&str]| prune_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(args(&["--yes"]), Ok((false, true, false)), "--yes alone: the landed checkouts' targets");
        assert!(args(&["--yes", "--force"]).unwrap_err().starts_with("usage:"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `prune --all` drops only this repository's own: volumes named `tarae-` that are this machine's (its id,
    /// or none on a Docker of its own) — gone checkouts' targets, toolchains, the registry, trace's — and the
    /// images it builds; never another machine's volume, another project's (`k8s-data`, `rust:1.99`), nor,
    /// on a shared Docker, one without an id or any image. A live checkout's target is kept, said which way
    /// it's live — a folder that's there, or a session tarae has open: frictions16's list had every session's,
    /// tproxy6's mid-work among them. It says each with its size, and the total, first.
    #[test]
    fn prune_all_drops_only_this_repositorys_own() {
        let listed = "tarae-target-1.99.0-aaaa\t7.2GB\ttarae.checkout=/w/gone,tarae.machine=me\n\
                      tarae-target-1.99.0-bbbb\t4.2GB\ttarae.checkout=/w/wt/tproxy6,tarae.machine=me\n\
                      tarae-target-1.99.0-eeee\t2GB\ttarae.checkout=/w/wt/frictions16,tarae.machine=me\n\
                      tarae-target-1.99.0-dddd\t6GB\ttarae.checkout=/elsewhere/wt,tarae.machine=them\n\
                      tarae-target-1.98.0-cccc\t5GB\ttarae.checkout=/w/old\n\
                      tarae-rustup-1.99.0\t1.5GB\t\n\
                      tarae-registry\t800MB\t\n\
                      tarae-trace-target-1.99.0\t2GB\t\n\
                      tarae-compat-go-cache\t300MB\ttarae.compat=go\n\
                      k8s-data\t3GB\ttarae.machine=me\n";
        let images = "tarae-trace:1.99-0a1b2c3d\t1.9GB\n\
                      rust:1.99\t1.5GB\n\
                      tarae-compat-go:1.99-0123456789ab\t2GB\n\
                      tarae-traced:x\t1MB\n";
        let names = |d: &[Dropped]| d.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        // (tproxy6 a session tarae has open; frictions16's folder there; the rest gone)
        let open = [PathBuf::from("/w/wt/tproxy6")];
        let there = |p: &Path| p == Path::new("/w/wt/frictions16");
        let is_live = |c: &str| live(c, &open, there);
        let (here, kept) = everything(listed, images, Some("me"), true, is_live);
        assert_eq!(
            names(&here),
            [
                "tarae-target-1.99.0-aaaa",
                "tarae-target-1.98.0-cccc",
                "tarae-rustup-1.99.0",
                "tarae-registry",
                "tarae-trace-target-1.99.0",
                "tarae-compat-go-cache",
                "tarae-trace:1.99-0a1b2c3d",
                "tarae-compat-go:1.99-0123456789ab"
            ]
        );
        assert_eq!(
            kept,
            [
                "  kept: live (tproxy6 — a session tarae has open) volume tarae-target-1.99.0-bbbb  4.2 GB",
                "  kept: live (frictions16 — its folder is there) volume tarae-target-1.99.0-eeee  2.0 GB"
            ]
        );
        // A Docker other machines use too: only what carries this machine's id, no image
        let (shared, _) = everything(listed, images, Some("me"), false, is_live);
        assert_eq!(names(&shared), ["tarae-target-1.99.0-aaaa"]);
        // No id of its own: nothing with one
        let (none, _) = everything(listed, images, None, true, is_live);
        assert!(!names(&none).contains(&"tarae-target-1.99.0-aaaa".into()));
        // Said first: each with its size (a target with its checkout), and the total
        let said = dropped_list(&here);
        assert_eq!(said[0], "  volume tarae-target-1.99.0-aaaa  7.2 GB  (/w/gone)");
        assert_eq!(said[6], "  image tarae-trace:1.99-0a1b2c3d  1.9 GB");
        assert_eq!(said.last().unwrap(), "  20.7 GB in all");
        // The open sessions: asked of the socket `TARAE_UI` names, as `tarae state` asks (a stand-in answers
        // here) — never by running a `tarae`; not run from one, or one answering an error: said, none
        assert!(tarae_sessions(None).unwrap_err().contains("not run from a tarae"));
        let dir = std::env::temp_dir().join(format!("tarae-xtask-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("ui.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let answers = [
            r#"{"sessions":[{"id":"s0","root":"/w/wt/tproxy6"},{"id":"s1"}]}"#,
            r#"{"error":"unknown request"}"#,
        ];
        let served = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for answer in answers {
                use std::io::{BufRead, Write};
                let (mut s, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(&s).read_line(&mut line).unwrap();
                asked.push(line);
                writeln!(s, "{answer}").unwrap();
            }
            asked
        });
        assert_eq!(tarae_sessions(Some(sock.clone().into())), Ok(open.to_vec()));
        assert_eq!(tarae_sessions(Some(sock.into())), Err("unknown request".to_string()));
        let asked = served.join().unwrap();
        let req: serde_json::Value = serde_json::from_str(&asked[0]).unwrap();
        assert_eq!(req["get"], "state", "the request `tarae state` sends");
        std::fs::remove_dir_all(&dir).ok();
        assert!(prune_cmd(&["--list".to_string()]).unwrap_err().starts_with("usage:"), "--list: gone");
        // The names are the ones this repository builds its images under
        let trace = std::fs::read_to_string(root().join("xtask/src/trace.rs")).unwrap();
        assert!(trace.contains("format!(\"tarae-trace:{}-{:08x}\""), "trace.rs's image name");
        assert!(prune_cmd(&["--some".to_string()]).unwrap_err().starts_with("usage:"));
    }

    /// The template `say_sizes` asks for, run against the Docker that's there (skipped without one): Docker
    /// takes it, and every row it prints is one `sizes` reads — else the line would never be said. A Docker
    /// that refuses it is said why.
    #[test]
    fn docker_takes_the_size_template() {
        if reachable(&mut Command::new("docker")).is_err() {
            return;
        }
        let listed = df(&mut Command::new("docker")).unwrap();
        for row in listed.lines().filter(|l| !l.trim().is_empty()) {
            let cols: Vec<&str> = row.split('\t').collect();
            assert!(cols.len() == 3 && !cols[0].is_empty() && bytes(cols[1].trim()).is_some(), "{row:?}");
        }
        let tarae = listed.lines().filter(|l| l.starts_with("tarae-")).count();
        assert_eq!(sizes(&listed).len(), tarae, "{listed}");
        let mut refused = Command::new("docker");
        refused.env("DOCKER_HOST", "unix:///nonexistent/tarae-test.sock").env_remove("DOCKER_CONTEXT");
        assert!(df(&mut refused).unwrap_err().starts_with("docker system df: "));
    }

    /// The machine id: made once in the folder, the same after; one that isn't an id is left as it is (never
    /// replaced — the volumes labelled with it would be orphaned); a folder it can't be made in says why.
    #[test]
    fn the_machine_id_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("tarae-xtask-machine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = machine_id_in(&dir.join("tarae")).unwrap();
        assert!(id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        assert_eq!(machine_id_in(&dir.join("tarae")).unwrap(), id, "the same on every run");
        let left: Vec<_> =
            std::fs::read_dir(dir.join("tarae")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, [MACHINE_FILE], "no temp file left");
        assert_ne!(machine_id_in(&dir.join("other")).unwrap(), id, "another machine, another id");
        std::fs::write(dir.join("tarae").join(MACHINE_FILE), "not an id\n").unwrap();
        assert!(machine_id_in(&dir.join("tarae")).unwrap_err().contains("isn't an id"));
        assert_eq!(std::fs::read_to_string(dir.join("tarae").join(MACHINE_FILE)).unwrap(), "not an id\n");
        // (a file where its folder would be: it can't be read nor made there, said with the file's path)
        std::fs::write(dir.join("file"), "").unwrap();
        let e = machine_id_in(&dir.join("file").join("tarae")).unwrap_err();
        assert!(e.contains(&format!("file/tarae/{MACHINE_FILE}")), "{e}");
        // (a folder it can't write in — a sandboxed run's: why, nothing made; not as root — a Linux container
        // — whom the mode doesn't stop: a probe written says so)
        use std::os::unix::fs::PermissionsExt as _;
        let ro = dir.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(ro.join("probe"), "").is_err() {
            let e = machine_id_in(&ro).unwrap_err();
            assert!(e.starts_with("making ") && !ro.join(MACHINE_FILE).exists(), "{e}");
        }
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A checkout is gone only when it surely isn't there: a folder is there, nothing or a file where it was is
    /// gone, and one this run can't see (a folder above it it may not enter — tarae's sandbox's sibling
    /// checkouts) is there for all it can tell: its volume stays. (Not as root, whom the mode doesn't stop.)
    #[test]
    fn a_checkout_it_cant_see_isnt_gone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("xtask-there-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("live")).unwrap();
        std::fs::create_dir_all(dir.join("shut/wt")).unwrap();
        std::fs::write(dir.join("file"), "").unwrap();
        assert!(checkout_there(&dir.join("live")));
        assert!(!checkout_there(&dir.join("deleted")) && !checkout_there(&dir.join("file")));
        std::fs::set_permissions(dir.join("shut"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::metadata(dir.join("shut/wt")).is_err() {
            assert!(checkout_there(&dir.join("shut/wt")), "can't see it: not gone");
        }
        std::fs::set_permissions(dir.join("shut"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A run in tarae's sandbox can't read the data folder but can read the repository's git folder: a run
    /// outside it copies the id there, and the sandboxed one labels its volumes with that copy — the same id,
    /// so its volumes stay this machine's. No copy yet: no id, said why. A copy that isn't the data folder's
    /// (a git folder brought from another machine) is put right by the next run outside.
    #[test]
    fn a_sandboxed_run_labels_with_the_git_folders_copy() {
        let dir = std::env::temp_dir().join(format!("tarae-xtask-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (data, git) = (dir.join("data"), dir.join("repo.git"));
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(dir.join("file"), "").unwrap();
        // (unreadable as the sandbox makes it, for root too: a file where its folder would be)
        let sandboxed = || Ok(dir.join("file").join("tarae"));
        let r = dir.join("w/s1");
        let e = machine_id_from(sandboxed(), Some(&git)).unwrap_err();
        assert!(e.contains("no copy yet in the git folder"), "{e}");
        assert_eq!(
            create_args(&r, machine_id_from(sandboxed(), Some(&git)).ok().as_deref(), "1.99.0", None).len(),
            5
        );
        let id = machine_id_from(Ok(data.clone()), Some(&git)).unwrap();
        assert_eq!(read_id(&git.join(GIT_COPY)), Ok(Some(id.clone())), "copied by a run outside");
        let theirs = machine_id_from(sandboxed(), Some(&git)).unwrap();
        assert_eq!(theirs, id, "the sandboxed run's is the machine's");
        let made = create_args(&r, Some(&theirs), "1.99.0", None);
        assert!(made.contains(&format!("{MACHINE_LABEL}={id}")), "labelled: {made:?}");
        std::fs::write(git.join(GIT_COPY), format!("{}\n", "0".repeat(32))).unwrap();
        assert_eq!(machine_id_from(Ok(data.clone()), Some(&git)).unwrap(), id);
        assert_eq!(read_id(&git.join(GIT_COPY)), Ok(Some(id.clone())), "put right");
        let left: Vec<_> = std::fs::read_dir(&git).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, [GIT_COPY], "no temp file left");
        assert!(machine_id_from(sandboxed(), None).is_err(), "no git folder: no copy");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Past 30 GB a run says what tarae's volumes take — the checkouts' targets (what ends and `prune` free,
    /// said so) apart from the shared ones (only `prune --all` frees them), each part's total and three
    /// biggest, a target by its checkout's folder — counting tarae's volumes only (k8s's and others' aren't
    /// ours); under it, nothing.
    #[test]
    fn the_volumes_size_is_said_past_a_line() {
        assert_eq!(
            [bytes("7.21GB"), bytes("606.3MB"), bytes("0B"), bytes("1.5kB"), bytes("2TB")],
            [Some(7_210_000_000), Some(606_300_000), Some(0), Some(1500), Some(2_000_000_000_000)]
        );
        assert_eq!((bytes("7.21GiB"), bytes("N/A"), bytes("")), (None, None, None));
        let df = "tarae-target-1.99.0-aaaa\t12GB\ttarae.checkout=/w/wt/landlock,tarae.machine=me\n\
                  tarae-trace-target-1.99.0\t9.5GB\t\n\
                  tarae-target-1.99.0-bbbb\t7.034GB\ttarae.checkout=/w/wt/corepack6\n\
                  tarae-registry\t147.4MB\t\n\
                  k8s-pvc-data\t40GB\tapp=shop\n\
                  tarae-rustup-1.99.0\t721.6MB\t\n";
        let listed = sizes(df);
        assert_eq!(listed.len(), 5, "tarae's only: {listed:?}");
        assert_eq!(listed[0], ("tarae-target-1.99.0-aaaa".into(), 12_000_000_000, "/w/wt/landlock".into()));
        assert_eq!(size_line(&listed, VOLUMES_SAY, None), None, "29.4 GB: under the line");
        let line = size_line(&listed, 29_000_000_000, None).expect("past it");
        let (targets, shared) = line.split_once("; ").expect("two parts");
        assert!(
            targets.starts_with(
                "tarae's Docker volumes take 29.4 GB (past 29.0 GB): checkouts' targets 19.0 GB \
                 (landlock's 12.0 GB, corepack6's 7.0 GB) — "
            ),
            "{line}"
        );
        assert!(targets.contains("cargo xtask linux prune") && targets.contains("session.ended"), "{line}");
        assert_eq!(
            shared,
            "shared 10.4 GB (tarae-trace-target-1.99.0 9.5 GB, tarae-rustup-1.99.0 0.7 GB, tarae-registry \
             0.1 GB) — only `cargo xtask linux prune --all` frees these (every gone target too; it lists, \
             --yes drops)",
            "what frees them: everything, said as that"
        );
        // (only shared ones: only that advice; more than three: counted)
        let shared_only = sizes("tarae-trace-target-1.99.0\t40GB\t\n");
        let line = size_line(&shared_only, VOLUMES_SAY, None).unwrap();
        assert!(!line.contains("session.ended") && line.contains("prune --all"), "{line}");
        let many: Vec<_> =
            (0..5).map(|i| (format!("tarae-target-1.99.0-{i}"), 7_000_000_000, String::new())).collect();
        assert!(size_line(&many, VOLUMES_SAY, None).unwrap().contains(" and 2 more)"));
        assert_eq!(VOLUMES_SAY, 30_000_000_000);
    }

    /// f22 l: the checks' script ends saying the target volume's disk (`df -P`, on any exit), read off the
    /// container's output (every other byte passed on as it came); the volumes' line says its use, and comes
    /// past `DISK_SAY` (80 %) under 30 GB too. Under both: nothing.
    #[test]
    fn the_disks_use_is_said_with_the_volumes_and_past_80_percent() {
        let s = script(Tree::Mounted(None), &What::Clippy);
        assert!(
            s.starts_with("trap 'df -P /target 2>/dev/null | sed -n \"2s/^/tarae-xtask-disk: /p\"' EXIT\n"),
            "{s}"
        );
        let out = "toolchain: rustc 1.99.0\n\
                   tarae-xtask-disk: /dev/vdb1 61255492 47984064 10133136 83% /target\n\
                   test result: ok\n\
                   no newline at its end";
        let mut passed = Vec::new();
        let d = forwarded(out.as_bytes(), &mut passed).disk.expect("the disk");
        assert_eq!(
            String::from_utf8(passed).unwrap(),
            "toolchain: rustc 1.99.0\ntest result: ok\nno newline at its end"
        );
        assert_eq!(d, Disk { pct: 83, used: 47_984_064 * 1024, size: 61_255_492 * 1024 });
        assert_eq!(disk("overlay 100 50"), None, "a row cut short");
        assert_eq!(forwarded("plain\n".as_bytes(), &mut Vec::new()), Seen::default());
        // (past 30 GB: its use with it)
        let listed = sizes("tarae-target-1.99.0-aaaa\t31GB\ttarae.checkout=/w/wt/landlock\n");
        let low = Disk { pct: 52, ..d };
        let line = size_line(&listed, VOLUMES_SAY, Some(low)).unwrap();
        assert!(
            line.starts_with(
                "tarae's Docker volumes take 31.0 GB (past 30.0 GB) — the disk 52 % used (49 of 63 GB): "
            ),
            "{line}"
        );
        // (under 30 GB: only past 80 %)
        let small = sizes("tarae-target-1.99.0-aaaa\t12GB\ttarae.checkout=/w/wt/landlock\n");
        assert_eq!(size_line(&small, VOLUMES_SAY, Some(low)), None);
        assert_eq!(size_line(&small, VOLUMES_SAY, None), None);
        let line = size_line(&small, VOLUMES_SAY, Some(d)).expect("past 80 %");
        assert!(line.starts_with("tarae's Docker volumes take 12.0 GB — the disk 83 % used (49 of 63 GB): checkouts' targets 12.0 GB (landlock's 12.0 GB)"), "{line}");
        assert_eq!(DISK_SAY, 80);
    }

    /// f22 o: every test binary runs (`--no-fail-fast`, stderr folded into what's read), and the run's last
    /// line names the ones that failed, each once, as cargo does — their lines passed on as they came.
    #[test]
    fn every_test_binary_runs_and_the_failed_ones_are_named() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for w in [What::Both, what(&args(&["test", "--bin", "tarae"])).unwrap()] {
            let s = script(Tree::Mounted(None), &w);
            assert!(s.contains("cargo test --locked --no-fail-fast") && s.contains("\nexec 2>&1\n"), "{s}");
        }
        let out = "test result: FAILED. 2200 passed; 2 failed\n\
                   error: test failed, to rerun pass `--bin tarae`\n\
                   test result: FAILED. 40 passed; 1 failed\n\
                   error: test failed, to rerun pass `--test e2e`\n\
                   error: test failed, to rerun pass `--bin tarae`\n\
                   error: 2 targets failed:\n    `--bin tarae`\n    `--test e2e`\n";
        let mut passed = Vec::new();
        let seen = forwarded(out.as_bytes(), &mut passed);
        assert_eq!(String::from_utf8(passed).unwrap(), out, "passed on whole");
        assert_eq!(seen.failed, ["--bin tarae", "--test e2e"]);
        assert_eq!(
            failed_said(&seen.failed),
            "the Linux checks failed — 2 test binaries failed (every one ran): --bin tarae, --test e2e"
        );
        assert_eq!(
            failed_said(&["--test e2e".into()]),
            "the Linux checks failed — 1 test binary failed (every one ran): --test e2e"
        );
        assert_eq!(failed_said(&[]), "the Linux checks failed (above)");
    }

    /// No Docker to talk to: said in a line a preflight note can match, not an error.
    #[test]
    fn no_docker_is_a_skip() {
        let mut docker = Command::new("docker");
        docker.env("DOCKER_HOST", "unix:///nonexistent/tarae-test.sock").env_remove("DOCKER_CONTEXT");
        let line = match reachable(&mut docker) {
            Ok(()) => panic!("a Docker answered at a socket that isn't there"),
            Err(why) => skipped(&why),
        };
        assert!(line.starts_with("skipped — Docker isn't reachable: "), "{line}");
        let line = skipped(&reachable(&mut Command::new("tarae-no-such-docker")).unwrap_err());
        assert!(line.starts_with("skipped — Docker isn't reachable: no docker command"), "{line}");
    }
}
