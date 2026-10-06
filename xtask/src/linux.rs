//! `cargo xtask linux [--env NAME=VALUE]… [--platform linux/<arch>] [clippy|test [--repeat N] [cargo test's arguments]]` — tarae's checks on Linux (CI runs there too: `-D dead-code` on code only
//! macOS calls, a root container's `#` prompt …), in Docker on the pinned toolchain (`rust:<major.minor>`, and
//! `rust-toolchain.toml`'s exact version in it). Whichever Docker the CLI reaches: `DOCKER_CONTEXT` or `DOCKER_HOST`, else the
//! current context (here the colima VM: `colima start -p tarae-test`, `DOCKER_CONTEXT=colima-tarae-test`).
//!
//! The working tree is mounted **read-only**; the checks run in a clone made inside the container, with the
//! uncommitted changes applied and the untracked files copied — nothing the container runs can write the real
//! tree or its `.git`. Caches live in Docker volumes, per pinned version (target, rustup; the registry shared).
//!
//! Docker not reachable: it says `skipped — …` and exits 0, so a `:preflight` step with `note = "skipped"`
//! reads it as a pass with a note. Not in tarae's own `.tarae.toml` — a step of yours (`.tarae.local.toml`).
//!
//! The target volume is the checkout's own (`target_volume`, labelled with its path and this machine's id):
//! every checkout's clone is /work/tarae, so one shared target let a session's tests run another session's
//! build (its e2e ran another's `target/debug/tarae`). Each run drops this machine's volumes whose checkout is
//! gone — its whole repository deleted too (`prunable`); `cargo xtask linux prune` does that now, and drops
//! the old shared `tarae-target-<pin>` too — tarae's `session.ended` runs it when a session's checkout is
//! removed. "This machine" is tarae's own id for it (`machine_id`: random, made once in tarae's data folder,
//! copied into the repository's git folder) — a Docker other machines use too (`DOCKER_HOST`) holds their
//! checkouts' targets, whose paths aren't here: another id's volume is never judged.
//!
//! A volume carries the id when the run that made it had one: a run outside tarae's sandbox always (the data
//! folder's, which it also copies to the git folder, `GIT_COPY`); a run inside it — which can't read the data
//! folder but can read the git folder — once a run outside has made that copy for this repository (any
//! `cargo xtask linux` or `prune` of yours, or `session.ended`'s). A sandboxed run before then says so and
//! makes its volume without one. A volume without the id — made so, or made before ids — is judged by its
//! checkout's path alone, as this machine's, for as long as it lives (`docker volume create` leaves an existing
//! volume's labels as they are, so it's never relabelled) — but only on a Docker that's this machine's alone, a
//! unix socket (`local`); one reached over the network (`tcp://`, `ssh://`, from `DOCKER_HOST` or the current
//! context) may hold other machines' volumes without an id, so there only volumes carrying this machine's id
//! are dropped. `prune` says how many it judged by path, or kept.
//! After the checks, past `VOLUMES_SAY` (30 GB), a line says what tarae's volumes take — the checkouts'
//! targets, which ends and `prune` free, apart from the shared ones (colima's disk filled at 40 GB, 2026-10-06,
//! with nothing said); a Docker that can't measure them is said so.
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
    Both,
}

const USAGE: &str = "usage: cargo xtask linux [--env NAME=VALUE]… [--platform linux/<arch>] [clippy | test [--repeat N] [cargo test's arguments]]";

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
    prune_volumes(machine.as_deref().ok(), local(endpoint().as_deref()).is_ok(), false);
    // (made here, not by `docker run`: only a volume made this way carries its labels)
    if let Err(why) = &machine {
        eprintln!("no machine id ({why}): a target volume made now carries none — judged by its path");
    }
    let made =
        Command::new("docker").args(create_args(&root, machine.as_deref().ok(), &pin, platform)).output();
    if !made.as_ref().is_ok_and(|o| o.status.success()) {
        return Err(format!("docker volume create {}: failed", target_volume(&root, &pin, platform)));
    }
    let st = Command::new("docker")
        .args(with_envs(docker_args(&root, common_git_dir(&root).as_deref(), &pin, platform, &what), &set))
        .status()
        .map_err(|e| format!("docker: {e}"))?;
    say_sizes();
    if !st.success() {
        return Err("the Linux checks failed (above)".into());
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

/// `docker run …` for the checks: the tree read-only at /src (a worktree's git folder read-only too, where
/// its `.git` file says), the caches in volumes, the pinned image — on `platform` when one is asked, its
/// target and toolchains in volumes of its own (the crates' sources stay shared).
fn docker_args(
    root: &Path,
    git: Option<&Path>,
    pin: &str,
    platform: Option<&str>,
    what: &What,
) -> Vec<String> {
    // (CAP_SYS_PTRACE: Docker's default filter refuses `pidfd_open`/`pidfd_getfd` without it — the sandbox's
    // supervisor takes a run's socket with them, and its tests would skip themselves)
    let mut a: Vec<String> = ["run", "--rm", "--cap-add", "SYS_PTRACE"].map(String::from).into();
    if let Some(p) = platform {
        a.extend(["--platform".to_string(), p.to_string()]);
    }
    let mount = |a: &mut Vec<String>, v: String| a.extend(["-v".to_string(), v]);
    mount(&mut a, format!("{}:/src:ro", root.display()));
    if let Some(git) = git {
        mount(&mut a, format!("{0}:{0}:ro", git.display()));
    }
    mount(&mut a, format!("{}:/target", target_volume(root, pin, platform)));
    mount(&mut a, "tarae-registry:/usr/local/cargo/registry".into());
    // (seeded from the image the first time: the file's components and targets are fetched once)
    let of = platform.map(|p| format!("-{}", p.replace('/', "-"))).unwrap_or_default();
    mount(&mut a, format!("tarae-rustup-{pin}{of}:/usr/local/rustup"));
    for e in ["CARGO_TARGET_DIR=/target", "CARGO_TERM_COLOR=never"] {
        a.extend(["-e".to_string(), e.to_string()]);
    }
    a.extend([image(pin), "sh".into(), "-ec".into(), script(what)]);
    a
}

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

/// Where the Docker CLI talks to: `DOCKER_HOST`, else the current context's endpoint — None: it didn't say.
fn endpoint() -> Option<String> {
    if let Some(host) = std::env::var("DOCKER_HOST").ok().filter(|h| !h.is_empty()) {
        return Some(host);
    }
    let out = Command::new("docker")
        .args(["context", "inspect", "--format", "{{.Endpoints.docker.Host}}"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|h| !h.is_empty())
}

/// Whether a Docker at `endpoint` is this machine's alone: a unix socket. Err: why it may not be — `tcp://`,
/// `ssh://` reach a Docker other machines may use too, whose volumes without an id aren't ours to judge.
fn local(endpoint: Option<&str>) -> std::result::Result<(), String> {
    match endpoint {
        Some(e) if e.starts_with("unix://") => Ok(()),
        Some(e) => Err(format!("the Docker at {e} may be other machines' too")),
        None => Err("Docker didn't say where it is".into()),
    }
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
        (Err(why), n) => notes.push(format!(
            "kept {n} target volume{} without a machine id: {why} — only those made with this machine's id \
             are judged there",
            s(n)
        )),
    }
    notes
}

/// Past this, a run says what tarae's volumes take — before a full disk does.
const VOLUMES_SAY: u64 = 30_000_000_000;

/// Docker's size as it prints it (`7.21GB`, `606.3MB`, `0B` — powers of 1000) in bytes.
fn bytes(size: &str) -> Option<u64> {
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
fn gb(n: u64) -> String {
    format!("{:.1} GB", n as f64 / 1e9)
}

/// The line saying what tarae's volumes take, once past `over`: the checkouts' targets — what a session's
/// end and `prune` free, with how — apart from the shared ones (registry, toolchains, trace's target), which
/// neither frees; each part's total and its three biggest (a target by its checkout's folder name).
fn size_line(volumes: &[(String, u64, String)], over: u64) -> Option<String> {
    let total: u64 = volumes.iter().map(|v| v.1).sum();
    if total < over {
        return None;
    }
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
        parts.push(format!("shared {} — neither frees these", part(shared)));
    }
    Some(format!("tarae's Docker volumes take {} (past {}): {}", gb(total), gb(over), parts.join("; ")))
}

/// `docker system df -v`'s volumes as `sizes` reads them: name, size, labels a line.
const DF_FORMAT: &str = "{{range .Volumes}}{{.Name}}\t{{.Size}}\t{{.Labels}}{{println}}{{end}}";

/// What `docker` prints for `DF_FORMAT` — Err: why it didn't, in a line.
fn df(docker: &mut Command) -> std::result::Result<String, String> {
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

/// What tarae's volumes take, said once past `VOLUMES_SAY` — and said when it can't be measured.
fn say_sizes() {
    match df(&mut Command::new("docker")) {
        Ok(listed) => {
            if let Some(line) = size_line(&sizes(&listed), VOLUMES_SAY) {
                eprintln!("{line}");
            }
        }
        Err(why) => eprintln!("couldn't measure the volumes ({why})"),
    }
}

/// `cargo xtask linux prune`: the target volumes no checkout uses, the old shared one too.
pub fn prune() -> Result<()> {
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    let machine = machine_id(&root());
    let local = local(endpoint().as_deref());
    let (listed, gone) = prune_volumes(machine.as_deref().ok(), local.is_ok(), true);
    if gone.is_empty() {
        println!("no target volume to drop — each of this machine's is a checkout's that's there");
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
    say_sizes();
    Ok(())
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

/// What runs in the container: the clone (with what isn't committed yet), then the checks.
fn script(what: &What) -> String {
    // (home made first: `--env HOME=/work` names one the clone hasn't made yet)
    let mut s = String::from(
        "mkdir -p \"$HOME\"\n\
         git config --global --add safe.directory '*'\n\
         git clone -q /src /work/tarae && cd /work/tarae\n\
         git -C /src diff HEAD | git apply --allow-empty\n\
         git -C /src ls-files --others --exclude-standard -z | (cd /src && tar --null -T - -cf -) | tar -xf -\n\
         echo \"toolchain: $(rustc --version)\"\n",
    );
    if !matches!(what, What::Test { .. }) {
        s.push_str("cargo clippy --locked --all-targets -- -D warnings\n");
        s.push_str("cargo clippy --locked --all-targets --features bundled-grammars -- -D warnings\n");
    }
    match what {
        What::Clippy => {}
        What::Both => s.push_str("cargo test --locked --workspace\n"),
        What::Test { args, repeat } => {
            let test = std::iter::once("cargo test --locked".to_string())
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

/// `a` as one word for sh, whatever is in it.
fn quoted(a: &str) -> String {
    format!("'{}'", a.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let a = docker_args(Path::new("/r/tarae"), None, "1.99.0", None, &What::Both);
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
        assert!(s.contains("git -C /src diff HEAD | git apply") && s.contains("ls-files --others"), "{s}");
        assert!(
            s.contains("--features bundled-grammars -- -D warnings")
                && s.contains("cargo test --locked --workspace\n"),
            "xtask's tests too: {s}"
        );
        let clippy = script(&What::Clippy);
        assert!(clippy.contains("cargo clippy") && !clippy.contains("cargo test"));
        let test = script(&What::Test { args: Vec::new(), repeat: 1 });
        assert!(test.contains("cargo test") && !test.contains("cargo clippy"));
    }

    /// `test`'s arguments go to `cargo test` as words, whatever is in them; `--repeat N` runs it N times in
    /// the one container and stops at the first that fails.
    #[test]
    fn test_takes_cargo_tests_arguments_and_a_repeat() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let once = script(&what(&args(&["test", "recovery", "--", "it's"])).unwrap());
        assert!(once.contains("cargo test --locked 'recovery' '--' 'it'\\''s'\n"), "{once}");
        assert!(!once.contains("while"), "{once}");
        let looped = script(&what(&args(&["test", "--repeat", "20", "recovery"])).unwrap());
        assert!(
            looped.contains("while [ $n -lt 20 ]") && looped.contains("cargo test --locked 'recovery' ||")
        );
        assert!(looped.contains("exit 1") && looped.contains("all 20 runs passed"), "{looped}");
        assert_eq!(what(&args(&["test"])), Ok(What::Test { args: Vec::new(), repeat: 1 }));
        assert!(what(&args(&["test", "--repeat", "0"])).unwrap_err().contains("--repeat 0: a count"));
        assert!(what(&args(&["clippy", "x"])).unwrap_err().starts_with("usage:"));
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
        let a = docker_args(r, None, "1.99.0", Some("linux/amd64"), &What::Both);
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
        let a = with_envs(docker_args(Path::new("/r/tarae"), None, "1.99.0", None, &What::Clippy), &set);
        let image = a.iter().position(|v| v == "rust:1.99").unwrap();
        assert_eq!(a[image - 4..image], args(&["-e", "HOME=/work", "-e", "A_1=x=y"]), "{a:?}");
        assert_eq!(a[image + 1..image + 3], args(&["sh", "-ec"]));
        assert_eq!(envs(&args(&["clippy"])).unwrap(), (Vec::new(), &args(&["clippy"])[..]));
        for bad in [&["--env", "=x"][..], &["--env", "A B=x"], &["--env", "noequals"], &["--env"]] {
            assert!(envs(&args(bad)).is_err_and(|e| e.contains("--env")), "{bad:?}");
        }
    }

    /// A worktree (a tarae session): the main checkout's git folder goes in read-only, at the path its `.git`
    /// file names — and only then. Found from a real worktree of a temp repository.
    #[test]
    fn a_worktrees_git_folder_goes_in_read_only() {
        let a =
            docker_args(Path::new("/w/s"), Some(Path::new("/r/tarae/.git")), "1.99.0", None, &What::Clippy);
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
        let args = docker_args(a, None, "1.99.0", None, &What::Test { args: Vec::new(), repeat: 1 });
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

    /// A Docker reached over the network (`tcp://`, `ssh://`) may be other machines' too: there only volumes
    /// made with this machine's id are judged — one without an id, gone from here, may be another machine's
    /// checkout that's there — and `prune` says it kept them. A unix socket is this machine's.
    #[test]
    fn a_remote_docker_judges_only_volumes_with_this_machines_id() {
        assert_eq!(local(Some("unix:///Users/me/.colima/tarae-test/docker.sock")), Ok(()));
        for remote in ["tcp://10.0.0.5:2376", "ssh://me@build"] {
            let e = local(Some(remote)).unwrap_err();
            assert!(e.contains(remote) && e.contains("other machines'"), "{e}");
        }
        assert!(local(None).is_err(), "not said where: not judged as ours");
        let listed = volumes(
            "tarae-target-1.99.0-aaaa\t/w/gone\tme\n\
             tarae-target-1.99.0-cccc\t/w/old-label\t\n\
             tarae-target-1.99.0-dddd\t/elsewhere/wt\tthem\n",
        );
        let gone_here = |_: &Path| false;
        assert_eq!(prunable(&listed, Some("me"), false, gone_here, false), ["tarae-target-1.99.0-aaaa"]);
        assert!(prunable(&listed, None, false, gone_here, true).is_empty(), "no id of its own: nothing");
        let why = local(Some("tcp://10.0.0.5:2376")).unwrap_err();
        assert_eq!(
            prune_notes(&listed, Ok("me"), Err(&why), &["tarae-target-1.99.0-aaaa".to_string()]),
            [
                "kept 1 target volume another machine made — theirs to judge".to_string(),
                "kept 1 target volume without a machine id: the Docker at tcp://10.0.0.5:2376 may be other \
                 machines' too — only those made with this machine's id are judged there"
                    .to_string()
            ]
        );
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
    /// the only part with advice) apart from the shared ones (neither frees them), each part's total and three
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
        assert_eq!(size_line(&listed, VOLUMES_SAY), None, "29.4 GB: under the line");
        let line = size_line(&listed, 29_000_000_000).expect("past it");
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
             0.1 GB) — neither frees these",
            "no advice for what nothing frees"
        );
        // (only shared ones: no advice at all; more than three: counted)
        let shared_only = sizes("tarae-trace-target-1.99.0\t40GB\t\n");
        let line = size_line(&shared_only, VOLUMES_SAY).unwrap();
        assert!(!line.contains("prune") && line.contains("neither frees"), "{line}");
        let many: Vec<_> =
            (0..5).map(|i| (format!("tarae-target-1.99.0-{i}"), 7_000_000_000, String::new())).collect();
        assert!(size_line(&many, VOLUMES_SAY).unwrap().contains(" and 2 more)"));
        assert_eq!(VOLUMES_SAY, 30_000_000_000);
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
