//! `cargo xtask compat [stack …] [--lane gate|quick|asks|net] [--linux own|host|all [--linux-only]] [--dry-run]` — the "must work"
//! compatibility suite (`target/sb/compat-plan.md` §4–§5). tarae's tests built under `perf` (a debug supervisor
//! serving every connect would skew the times), then the ignored driver (`compat::tests::driver`, `src/compat.rs`)
//! run here and, asked, on Linux tiers in Docker through `cargo xtask linux` — own with `--userns`, host under Docker's
//! own profiles (Ubuntu 24.04's restriction's tier) (`--linux-only`: not here). On Linux a stack runs in the image
//! its manifest names (`image`: `compat/linux/<image>.Dockerfile` on `rust:<pin>`, built here when its tag — the
//! recipe's hash — isn't there yet, the tags it built before dropped; none: `rust:<pin>`) with its own volume as
//! home (`tarae-compat-<stack>-cache`: the caches its pre-warm fills, kept between runs). After them a line says
//! what compat's images and volumes take against its 10 GB cap (§7 D4), and the disk. The driver makes each stack's project from
//! its fixture (`compat/<stack>.toml`, `compat/<fixture>/`), runs its `prewarm` outside the sandbox, then each entry
//! of the lane as Claude's run (`quick`: the entries marked so; `asks`: the ones marked `asks`, which try the
//! network — never in the gate or quick).
//!
//! Its `COMPAT` lines make the table — `target/compat/<date>-<os>.md`, one per stack, commands down and OS/tier
//! across, `pending` entries' design in a column of its own — and a row each in `target/compat/history.tsv`,
//! against which what changed since is said. A ✗ cell fails the task (a `:preflight` step's verdict).
//! `--dry-run` prints every command it would run, the prewarm included, and runs none. `--lane net` is piece 5's.

use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::linux::{COMPAT, quoted};
use crate::{Result, root};

const USAGE: &str = "usage: cargo xtask compat [stack …] [--lane gate|quick|asks|net] [--linux own|host|all [--linux-only]] [--dry-run]";

/// The manifests and fixtures, and where the tables and their history go, from the checkout's root.
const MANIFESTS: &str = "compat";
const OUT: &str = "target/compat";
const HISTORY: &str = "target/compat/history.tsv";
const HISTORY_HEAD: &str = "date\tcommit\tlane\tcolumn\tstack\tentry\tcell\n";

/// `cargo test` building tarae's own tests under `compat-tests` (optimized, no LTO: Cargo.toml says why), and the
/// driver's arguments after it.
const TEST: [&str; 5] = ["test", "--profile", "compat-tests", "--bin", "tarae"];

/// The one Linux build of a round: `cargo xtask linux`'s arguments, the binary kept in the checkout's target
/// volume for every stack's `test --kept` (each container's fresh clone would otherwise build the crate again),
/// on `jobs` (`jobs_for`).
fn keep(jobs: u64) -> [String; 8] {
    let jobs = format!("CARGO_BUILD_JOBS={jobs}");
    ["--env", &jobs, "test", "--keep", "--profile", "compat-tests", "--bin", "tarae"].map(String::from)
}

/// The build's jobs on a Docker machine of `cpus` and `memory` bytes (`docker info`'s): a job a CPU and 4 GiB,
/// from 2 (colima's VM: 11.7 GiB, shared with a test cluster) to 16 (one crate's codegen units — more buys
/// nothing). Unknown: 2.
fn jobs_for(cpus: Option<u64>, memory: Option<u64>) -> u64 {
    match (cpus, memory) {
        (Some(c), Some(m)) => c.min(m / (4 << 30)).clamp(2, 16),
        _ => 2,
    }
}

/// `jobs_for` the Docker the CLI reaches.
fn docker_jobs() -> u64 {
    let info = Command::new("docker").args(["info", "--format", "{{.NCPU}} {{.MemTotal}}"]).output();
    let text =
        info.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let mut words = text.as_deref().unwrap_or_default().split_whitespace().map(|w| w.parse::<u64>().ok());
    jobs_for(words.next().flatten(), words.next().flatten())
}
const DRIVE: [&str; 6] =
    ["--", "--ignored", "--exact", "compat::tests::driver", "--nocapture", "--test-threads=1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    /// Every entry but the `asks` lane's, the network closed and the caches warmed: what must work.
    Gate,
    /// The entries marked `quick` (build and test).
    Quick,
    /// The entries marked `asks`: everyday checks that try the network, run with it closed — which ask a host,
    /// how many cards, how long a check waits (piece C1's measurement). Nothing leaves the machine: a test build's
    /// proxy dials nothing past it, and these entries have no unsandboxed baseline.
    Asks,
    /// Installs through tarae's proxy, hosts granted: piece 5.
    Net,
}

impl Lane {
    fn word(self) -> &'static str {
        match self {
            Lane::Gate => "gate",
            Lane::Quick => "quick",
            Lane::Asks => "asks",
            Lane::Net => "net",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Own,
    Host,
}

impl Tier {
    fn column(self) -> &'static str {
        match self {
            Tier::Own => "Linux own",
            Tier::Host => "Linux host",
        }
    }

    /// The tier as tarae's runs say it (`sandbox own`): the driver checks each run was at it (`TARAE_COMPAT_TIER`).
    fn word(self) -> &'static str {
        match self {
            Tier::Own => "own",
            Tier::Host => "host",
        }
    }

    /// `cargo xtask linux`'s arguments for it.
    fn args(self) -> &'static [&'static str] {
        match self {
            Tier::Own => &["--userns"],
            Tier::Host => &[],
        }
    }
}

#[derive(Debug, PartialEq)]
struct Ask {
    stacks: Vec<String>,
    lane: Lane,
    linux: Vec<Tier>,
    /// This machine's column too (`--linux-only`: only the Linux tiers').
    here: bool,
    dry: bool,
}

fn ask(args: &[String]) -> Result<Ask> {
    let mut a = Ask { stacks: Vec::new(), lane: Lane::Gate, linux: Vec::new(), here: true, dry: false };
    let mut it = args.iter().map(String::as_str);
    while let Some(x) = it.next() {
        match x {
            "--lane" => {
                a.lane = match it.next() {
                    Some("gate") => Lane::Gate,
                    Some("quick") => Lane::Quick,
                    Some("asks") => Lane::Asks,
                    Some("net") => Lane::Net,
                    l => {
                        return Err(format!(
                            "--lane {}: gate, quick, asks or net — {USAGE}",
                            l.unwrap_or("?")
                        ));
                    }
                }
            }
            "--linux" => {
                a.linux = match it.next() {
                    Some("own") => vec![Tier::Own],
                    Some("host") => vec![Tier::Host],
                    Some("all") => vec![Tier::Own, Tier::Host],
                    t => return Err(format!("--linux {}: own, host or all — {USAGE}", t.unwrap_or("?"))),
                }
            }
            "--dry-run" => a.dry = true,
            "--linux-only" => a.here = false,
            s if s.starts_with('-') => return Err(format!("{s}? — {USAGE}")),
            s if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => {
                a.stacks.push(s.into())
            }
            s => return Err(format!("{s}: a stack's name — {USAGE}")),
        }
    }
    if !a.here && a.linux.is_empty() {
        return Err(format!("--linux-only: with --linux own, host or all — {USAGE}"));
    }
    Ok(a)
}

/// What the xtask reads of a manifest — the harness reads it whole and checks it (`src/compat.rs`): what's done
/// before its entries, to say it, and its entries, for the table's rows.
#[derive(Debug, PartialEq)]
struct Stack {
    name: String,
    fixture: Option<String>,
    /// The files its manifest writes into the project.
    files: usize,
    prewarm: Vec<String>,
    /// The image its Linux runs take (`compat/linux/<image>.Dockerfile`) — None: `rust:<pin>` itself.
    image: Option<String>,
    rows: Vec<Row>,
}

#[derive(Debug, PartialEq)]
struct Row {
    name: String,
    quick: bool,
    /// Tries the network: the `asks` lane's only.
    asks: bool,
    pending: Option<String>,
}

fn stack(name: &str, text: &str) -> Result<Stack> {
    let t: toml::Table = text.parse().map_err(|e| format!("{name}: {e}"))?;
    let s = |t: &toml::Table, k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
    if s(&t, "stack").as_deref() != Some(name) {
        return Err(format!("{MANIFESTS}/{name}.toml: stack = \"{name}\" expected"));
    }
    let entries = t.get("entry").and_then(|e| e.as_array()).map(Vec::as_slice).unwrap_or_default();
    let rows = entries.iter().filter_map(|e| e.as_table()).map(|e| Row {
        name: s(e, "name").unwrap_or_default(),
        quick: e.get("quick").and_then(|q| q.as_bool()).unwrap_or(false),
        asks: e.get("asks").and_then(|q| q.as_bool()).unwrap_or(false),
        pending: s(e, "pending"),
    });
    let prewarm = t.get("prewarm").and_then(|p| p.as_array()).map(Vec::as_slice).unwrap_or_default();
    Ok(Stack {
        name: name.into(),
        fixture: s(&t, "fixture"),
        files: t.get("files").and_then(|f| f.as_table()).map_or(0, |f| f.len()),
        prewarm: prewarm.iter().filter_map(|p| p.as_str().map(String::from)).collect(),
        image: s(&t, "image"),
        rows: rows.collect(),
    })
}

/// The stacks asked — every manifest in `dir` when none is — in order.
fn stacks(dir: &Path, asked: &[String]) -> Result<Vec<Stack>> {
    let mut all: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            let names =
                rd.flatten().filter_map(|e| Some(e.file_name().to_str()?.strip_suffix(".toml")?.to_string()));
            names.collect()
        })
        .unwrap_or_default();
    all.sort();
    if all.is_empty() {
        return Err(format!("no manifests in {} yet (piece 3 adds the first stacks)", dir.display()));
    }
    let names = if asked.is_empty() { &all[..] } else { asked };
    let one = |n: &String| {
        if !all.contains(n) {
            return Err(format!("no stack {n} — there are: {}", all.join(", ")));
        }
        let p = dir.join(format!("{n}.toml"));
        stack(n, &std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)
    };
    names.iter().map(one).collect()
}

/// A stack's rows in the lane.
fn rows(s: &Stack, lane: Lane) -> impl Iterator<Item = &Row> {
    s.rows.iter().filter(move |r| r.asks == (lane == Lane::Asks) && (lane != Lane::Quick || r.quick))
}

/// The driver's variables: which stacks, which lane.
fn envs(stacks: &[Stack], lane: Lane) -> [(&'static str, String); 2] {
    let names: Vec<&str> = stacks.iter().map(|s| s.name.as_str()).collect();
    [("TARAE_COMPAT_STACKS", names.join(",")), ("TARAE_COMPAT_LANE", lane.word().into())]
}

/// The recipes, from the checkout's root: `<image>.Dockerfile`, each `FROM $BASE` (`rust:<pin>`).
const RECIPES: &str = "compat/linux";

/// The label on what compat makes, naming its recipe or stack.
const LABEL: &str = "tarae.compat";

/// Where a stack's volume goes in its container: the home of root, whom the checks run as — so every
/// toolchain's cache (`~/.npm`, `~/.cache/…`, `~/go`, `~/.gradle`, `~/.m2`) and tarae's own kept run homes are
/// where a user has them, and still there the next run.
const HOME: &str = "/root";

/// compat's share of the Docker disk (§7 D4): said after each Linux run, its images and volumes against it.
const CAP: u64 = 10_000_000_000;

/// A stack's volume: its pre-warmed caches, mounted as its container's home (`HOME`).
fn volume_of(stack: &str) -> String {
    format!("{COMPAT}{stack}-cache")
}

/// The tag `recipe` builds on `base`: `tarae-compat-<image>:<minor>-<hash>`, the hash (FNV-1a) of both — a
/// changed recipe or Rust is a tag of its own, and what the old one built is stale.
fn tag(image: &str, base: &str, recipe: &str) -> String {
    let hash = (base.bytes().chain([0]).chain(recipe.bytes()))
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let minor = base.rsplit(':').next().unwrap_or(base);
    format!("{COMPAT}{image}:{minor}-{:012x}", hash >> 16)
}

/// `docker build`'s arguments for `tag`: the recipe on stdin with no context (it fetches what it needs), `base`
/// its `BASE`, labelled as compat's.
fn build_args(tag: &str, image: &str, base: &str) -> Vec<String> {
    ["build", "-t", tag, "--label", &format!("{LABEL}={image}"), "--build-arg", &format!("BASE={base}"), "-"]
        .map(String::from)
        .into()
}

/// `docker volume create`'s arguments for a stack's volume (one that's there is left as it is).
fn volume_args(stack: &str) -> Vec<String> {
    ["volume", "create", "--label", &format!("{LABEL}={stack}"), &volume_of(stack)].map(String::from).into()
}

/// `cargo xtask linux`'s arguments for one stack on `tier`: the driver's variables, the tier, the image (None:
/// `rust:<pin>`), the stack's volume as home, the driver.
fn linux_args(s: &Stack, lane: Lane, tier: Tier, image: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    for (k, v) in envs(std::slice::from_ref(s), lane) {
        a.extend(["--env".into(), format!("{k}={v}")]);
    }
    a.extend(["--env".into(), format!("TARAE_COMPAT_TIER={}", tier.word())]);
    // (host cards left unanswered, as here: the driver's `TARAE_COMPAT_HOSTS=wait`)
    if let Ok(h) = std::env::var("TARAE_COMPAT_HOSTS") {
        a.extend(["--env".into(), format!("TARAE_COMPAT_HOSTS={h}")]);
    }
    a.extend(tier.args().iter().map(|t| t.to_string()));
    if let Some(i) = image {
        a.extend(["--image".into(), i.into()]);
    }
    a.extend(["--volume".into(), format!("{}:{HOME}", volume_of(&s.name))]);
    // (the binary `KEEP` built, its own arguments: the driver's, without cargo's `--`)
    a.extend(["test", "--kept"].iter().chain(&DRIVE[1..]).map(|t| t.to_string()));
    a
}

/// The base image, `rust:<pin>`, from `rust-toolchain.toml`.
fn base(root: &Path) -> Result<String> {
    let toolchain = root.join("rust-toolchain.toml");
    let text = std::fs::read_to_string(&toolchain).map_err(|e| format!("{}: {e}", toolchain.display()))?;
    Ok(crate::linux::image(&crate::linux::pin(&text)?))
}

/// Each recipe in `RECIPES` and the tag it builds on `base`, by name.
fn recipes(root: &Path, base: &str) -> BTreeMap<String, (String, String)> {
    let rd = std::fs::read_dir(root.join(RECIPES)).into_iter().flatten().flatten();
    rd.filter_map(|e| {
        let name = e.file_name().to_str()?.strip_suffix(".Dockerfile")?.to_string();
        let recipe = std::fs::read_to_string(e.path()).ok()?;
        Some((name.clone(), (tag(&name, base, &recipe), recipe)))
    })
    .collect()
}

/// The image `name`'s recipe builds, built when it isn't yet (its output passed on) — the tags it built before
/// dropped then. Err: why there's none.
fn built(root: &Path, base: &str, name: &str) -> Result<String> {
    let Some((tag, recipe)) = recipes(root, base).remove(name) else {
        return Err(format!("no recipe {RECIPES}/{name}.Dockerfile"));
    };
    let there = Command::new("docker").args(["image", "inspect", "--format", ".", &tag]).output();
    if there.is_ok_and(|o| o.status.success()) {
        return Ok(tag);
    }
    eprintln!("building {tag} from {RECIPES}/{name}.Dockerfile on {base} …");
    let mut c = Command::new("docker");
    c.args(build_args(&tag, name, base)).stdin(Stdio::piped()).stdout(Stdio::inherit());
    let mut child = c.spawn().map_err(|e| format!("docker build: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    std::io::Write::write_all(&mut stdin, recipe.as_bytes()).map_err(|e| format!("docker build: {e}"))?;
    drop(stdin);
    if !child.wait().map_err(|e| format!("docker build: {e}"))?.success() {
        return Err(format!("building {tag} failed (above)"));
    }
    drop_stale(root);
    Ok(tag)
}

/// The images of compat's no recipe builds now, out of `listed` (`repository:tag` a line, every image of compat's):
/// a recipe changed or gone, an older Rust.
fn stale(listed: &str, current: &[String]) -> Vec<String> {
    let ours = listed.lines().map(str::trim).filter(|l| l.starts_with(COMPAT) && !l.ends_with(":<none>"));
    ours.filter(|l| !current.iter().any(|c| c == l)).map(String::from).collect()
}

/// Drops compat's images no recipe builds now (`stale`), each said — `cargo xtask linux prune`'s, and each
/// build's after it.
pub fn drop_stale(root: &Path) {
    let Ok(base) = base(root) else { return };
    let current: Vec<String> = recipes(root, &base).into_values().map(|(t, _)| t).collect();
    let reference = format!("reference={COMPAT}*");
    let ls = Command::new("docker")
        .args(["image", "ls", "--filter", &reference, "--format", "{{.Repository}}:{{.Tag}}"])
        .output();
    let listed = ls.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    for old in stale(&listed, &current) {
        match Command::new("docker").args(["image", "rm", &old]).output() {
            Ok(o) if o.status.success() => eprintln!("dropped image {old} (its recipe builds another now)"),
            _ => eprintln!("kept image {old} for now: Docker didn't drop it (in use?)"),
        }
    }
}

/// The line said after the Linux runs: what compat's images (`images`: `docker system df -v`'s, repository and
/// unique size a line — the shared `rust:<pin>` not counted) and volumes (`volumes`: `linux::df`'s) take against
/// `CAP`, each one's size, and the disk.
fn cap_line(images: &str, volumes: &str, disk: Option<crate::linux::Disk>) -> String {
    use crate::linux::{bytes, gb};
    let ours = |text: &str| -> Vec<(String, u64)> {
        let rows = text.lines().filter_map(|l| {
            let mut cols = l.split('\t').map(str::trim);
            let (name, size) = (cols.next()?, bytes(cols.next()?)?);
            name.starts_with(COMPAT).then(|| (name.strip_prefix(COMPAT).unwrap_or(name).to_string(), size))
        });
        rows.collect()
    };
    let (images, volumes) = (ours(images), ours(volumes));
    let sum = |v: &[(String, u64)]| v.iter().map(|(_, s)| s).sum::<u64>();
    let each =
        |v: &[(String, u64)]| v.iter().map(|(n, s)| format!("{n} {}", gb(*s))).collect::<Vec<_>>().join(", ");
    let total = sum(&images) + sum(&volumes);
    let past = match total > CAP {
        true => {
            " — past it: `cargo xtask linux prune --all` lists them (--yes drops; the next run builds and warms again)"
        }
        false => "",
    };
    let disk = disk.map(|d| format!("; {}", crate::linux::disk_said(d))).unwrap_or_default();
    format!(
        "compat's Docker images and volumes take {} of its {} cap{past}: images {} ({}), volumes {} ({}){disk}",
        gb(total),
        gb(CAP),
        gb(sum(&images)),
        each(&images),
        gb(sum(&volumes)),
        each(&volumes),
    )
}

/// `docker system df -v`'s images as `cap_line` reads them: repository and unique size (the layers no other image
/// shares), a line.
const IMAGES_DF: &str = "{{range .Images}}{{.Repository}}\t{{.UniqueSize}}{{println}}{{end}}";

/// `cap_line` for the Docker the CLI reaches, the disk from a container of `base` (its `/`: the machine's disk,
/// where the volumes are too) — Err: why it couldn't be measured.
fn say_cap(base: &str) -> std::result::Result<String, String> {
    let images = Command::new("docker").args(["system", "df", "-v", "--format", IMAGES_DF]).output();
    let images = images.map_err(|e| e.to_string())?;
    if !images.status.success() {
        return Err(String::from_utf8_lossy(&images.stderr).trim().to_string());
    }
    let volumes = crate::linux::df(&mut Command::new("docker"))?;
    let df = Command::new("docker").args(["run", "--rm", base, "df", "-P", "/"]).output();
    let disk =
        df.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).lines().nth(1).and_then(crate::linux::disk));
    Ok(cap_line(&String::from_utf8_lossy(&images.stdout), &volumes, disk))
}

/// What a run does, in order, as commands — what `--dry-run` prints. `recipes`: each recipe's tag and text by
/// name (`recipes`), on `base`.
fn plan(
    stacks: &[Stack],
    lane: Lane,
    linux: &[Tier],
    (cargo, jobs): (&str, u64),
    base: &str,
    recipes: &BTreeMap<String, (String, String)>,
) -> Vec<String> {
    let mut out = vec![format!("{cargo} {} --no-run --quiet", TEST.join(" "))];
    for s in stacks {
        let from = match (&s.fixture, s.files) {
            (Some(f), 0) => format!("a copy of {MANIFESTS}/{f}/"),
            (Some(f), n) => format!("a copy of {MANIFESTS}/{f}/ and the {n} files its manifest writes"),
            (None, 0) => "an empty project".into(),
            (None, n) => format!("a project of the {n} files {MANIFESTS}/{}.toml writes", s.name),
        };
        for p in &s.prewarm {
            out.push(format!("# {}: the driver first runs, outside the sandbox, in {from}:", s.name));
            out.push(format!("sh -c {}", quoted(p)));
        }
    }
    let set: Vec<String> = envs(stacks, lane).iter().map(|(k, v)| format!("{k}={v}")).collect();
    out.push(format!("{} {cargo} {} {}", set.join(" "), TEST.join(" "), DRIVE.join(" ")));
    if linux.is_empty() {
        return out;
    }
    let mut images: Vec<&str> = stacks.iter().filter_map(|s| s.image.as_deref()).collect();
    images.sort();
    images.dedup();
    for i in &images {
        match recipes.get(*i) {
            Some((tag, _)) => {
                out.push(format!(
                    "# {i}: built where {tag} isn't yet, then the tags its recipe built before dropped"
                ));
                let args: Vec<String> = build_args(tag, i, base).iter().map(|a| quoted(a)).collect();
                out.push(format!("docker {} < {RECIPES}/{i}.Dockerfile", args.join(" ")));
            }
            None => {
                out.push(format!("# {i}: no recipe {RECIPES}/{i}.Dockerfile — its stacks' cells skipped"))
            }
        }
    }
    for s in stacks {
        out.push(format!("docker {}", volume_args(&s.name).join(" ")));
    }
    out.push("# the one build every stack's Linux run takes, kept in the checkout's target volume".into());
    out.push(format!("cargo xtask linux {}", keep(jobs).map(|a| quoted(&a)).join(" ")));
    for t in linux {
        for s in stacks {
            let tag = s.image.as_deref().and_then(|i| recipes.get(i)).map(|(t, _)| t.as_str());
            out.push(format!(
                "# {}, {}: in {}, home {}",
                s.name,
                t.column(),
                tag.unwrap_or(base),
                volume_of(&s.name)
            ));
            let args: Vec<String> = linux_args(s, lane, *t, tag).iter().map(|a| quoted(a)).collect();
            out.push(format!("cargo xtask linux {}", args.join(" ")));
        }
    }
    out
}

/// What a driver's run said: its cells by stack and entry, the line it skipped with (`linux::skipped`), whether
/// it ended well.
#[derive(Debug, Default)]
struct Driven {
    cells: BTreeMap<(String, String), String>,
    skipped: Option<String>,
    ok: bool,
}

impl Driven {
    /// One line of its output: a cell taken — the first one comes after libtest's `test … ... ` on its line,
    /// what's before it said; else said as it comes.
    fn line(&mut self, l: &str) {
        let at = l.find("COMPAT\t");
        let parts: Option<Vec<&str>> = at.map(|i| l[i + "COMPAT\t".len()..].splitn(3, '\t').collect());
        match parts.as_deref() {
            Some([stack, entry, cell]) => {
                let before = l[..at.unwrap_or(0)].trim_end();
                if !before.is_empty() {
                    println!("{before}");
                }
                _ = self.cells.insert((stack.to_string(), entry.to_string()), cell.to_string())
            }
            _ => {
                if let Some(why) = l.strip_prefix("skipped — ") {
                    self.skipped = Some(why.into());
                }
                println!("{l}");
            }
        }
    }
}

fn driven(mut c: Command) -> Result<Driven> {
    let mut child = c.stdout(Stdio::piped()).spawn().map_err(|e| format!("{c:?}: {e}"))?;
    let mut d = Driven::default();
    for l in std::io::BufReader::new(child.stdout.take().expect("piped")).lines() {
        d.line(&l.map_err(|e| format!("the driver's output: {e}"))?);
    }
    d.ok = child.wait().map_err(|e| e.to_string())?.success();
    Ok(d)
}

/// Cells by column, stack and entry.
type Cells = BTreeMap<(String, String, String), String>;

/// A column's cells from a run of `stacks`: each row of the lane the driver's cell, else why there's none — and
/// what it said of no row (a manifest it couldn't read).
fn fill(cells: &mut Cells, column: &str, stacks: &[Stack], lane: Lane, d: &Driven) {
    let none = match (&d.skipped, d.ok) {
        (Some(why), _) => format!("– skipped: {why}"),
        (None, false) => "✗ harness (its output above)".into(),
        (None, true) => "– no result".into(),
    };
    for s in stacks {
        for r in rows(s, lane) {
            let cell = d.cells.get(&(s.name.clone(), r.name.clone())).unwrap_or(&none);
            cells.insert((column.into(), s.name.clone(), r.name.clone()), cell.clone());
        }
    }
    for ((s, e), cell) in &d.cells {
        cells.entry((column.into(), s.clone(), e.clone())).or_insert_with(|| cell.clone());
    }
}

/// The table: a section per stack, its rows in the lane (then any the driver named alone) down, `columns` across,
/// `pending` last where a row is; then what changed since the history's last cell of each.
fn table(head: &str, columns: &[&str], stacks: &[Stack], lane: Lane, cells: &Cells, past: &str) -> String {
    let md = |s: &str| s.replace('|', "\\|");
    let mut t = format!("# compat — {head}\n");
    for s in stacks {
        let pending = rows(s, lane).any(|r| r.pending.is_some());
        let mut names: Vec<(&str, Option<&str>)> =
            rows(s, lane).map(|r| (r.name.as_str(), r.pending.as_deref())).collect();
        for (_, st, e) in cells.keys() {
            if *st == s.name && !names.iter().any(|(n, _)| n == e) {
                names.push((e, None));
            }
        }
        let mut head = vec!["command"];
        head.extend(columns);
        if pending {
            head.push("pending");
        }
        t.push_str(&format!("\n## {}\n\n| {} |\n|{}\n", s.name, head.join(" | "), "---|".repeat(head.len())));
        for (name, p) in names {
            let mut row = vec![md(name)];
            for c in columns {
                let cell = cells.get(&(c.to_string(), s.name.clone(), name.to_string()));
                row.push(md(cell.map_or("–", String::as_str)));
            }
            if pending {
                row.push(p.unwrap_or_default().into());
            }
            t.push_str(&format!("| {} |\n", row.join(" | ")));
        }
    }
    let changed = changes(cells, past);
    if !changed.is_empty() {
        t.push_str("\n## Since the last run\n\n");
        for c in changed {
            t.push_str(&format!("- {}\n", md(&c)));
        }
    }
    t
}

/// Each cell that differs from its last in the history: `stack entry, column: was → now`.
fn changes(cells: &Cells, past: &str) -> Vec<String> {
    let mut last: BTreeMap<(&str, &str, &str), &str> = BTreeMap::new();
    for l in past.lines().skip_while(|l| l.starts_with("date\t")) {
        if let [_, _, _, column, stack, entry, cell] = l.split('\t').collect::<Vec<_>>()[..] {
            last.insert((column, stack, entry), cell);
        }
    }
    let was = |(c, s, e): &(String, String, String)| last.get(&(c.as_str(), s.as_str(), e.as_str())).copied();
    let differs = cells.iter().filter_map(|(k, now)| Some((k, was(k).filter(|w| w != now)?, now)));
    differs.map(|((c, s, e), was, now)| format!("{s} {e}, {c}: {was} → {now}")).collect()
}

/// The history's rows for `cells` (its head first when `past` is empty).
fn history(past: &str, (date, commit, lane): (&str, &str, Lane), cells: &Cells) -> String {
    let mut h = if past.is_empty() { HISTORY_HEAD.to_string() } else { past.to_string() };
    for ((column, stack, entry), cell) in cells {
        h.push_str(&format!("{date}\t{commit}\t{}\t{column}\t{stack}\t{entry}\t{cell}\n", lane.word()));
    }
    h
}

/// This machine's column.
fn here() -> &'static str {
    if cfg!(target_os = "macos") { "macOS" } else { "Linux (here)" }
}

pub fn run(args: &[String]) -> Result<()> {
    let a = ask(args)?;
    if a.lane == Lane::Net {
        println!("--lane net: the network lane is piece 5 (target/sb/compat-plan.md §3 b, §9) — nothing run");
        return Ok(());
    }
    let root = root();
    let mut stacks = stacks(&root.join(MANIFESTS), &a.stacks)?;
    // (a stack with no entry in the lane isn't run: its prewarm would be for nothing)
    stacks.retain(|s| rows(s, a.lane).next().is_some());
    if stacks.is_empty() {
        return Err(format!("no stack asked has an entry in the {} lane", a.lane.word()));
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let base = base(&root)?;
    if a.dry {
        let jobs = if a.linux.is_empty() { 2 } else { docker_jobs() };
        let here = plan(&stacks, a.lane, &[], (&cargo, jobs), &base, &BTreeMap::new());
        let all = plan(&stacks, a.lane, &a.linux, (&cargo, jobs), &base, &recipes(&root, &base));
        // (`--linux-only`: what the plan says past this machine's own)
        for l in all.iter().skip(if a.here { 0 } else { here.len() }) {
            println!("{l}");
        }
        return Ok(());
    }
    let mut cells = Cells::new();
    let mut columns = Vec::new();
    if a.here {
        eprintln!("building tarae's tests under perf …");
        let mut build = Command::new(&cargo);
        build.args(TEST).args(["--no-run", "--quiet"]).current_dir(&root);
        if !build.status().map_err(|e| format!("cargo: {e}"))?.success() {
            return Err("building tarae's tests under perf failed".into());
        }
        columns.push(here());
        let mut c = Command::new(&cargo);
        c.args(TEST).args(DRIVE).current_dir(&root).envs(envs(&stacks, a.lane));
        fill(&mut cells, here(), &stacks, a.lane, &driven(c)?);
    }
    let docker = match a.linux.is_empty() {
        true => Ok(()),
        false => crate::linux::reachable(&mut Command::new("docker")),
    };
    let xtask = std::env::current_exe().map_err(|e| format!("this xtask: {e}"))?;
    // (each image built once and each stack's volume made once, before any tier — Err: the stack's cells skipped
    // with why)
    let mut ready: BTreeMap<String, std::result::Result<Option<String>, String>> = BTreeMap::new();
    if docker.is_ok() && !a.linux.is_empty() {
        let mut images: BTreeMap<String, Result<String>> = BTreeMap::new();
        for s in &stacks {
            let image = match &s.image {
                None => Ok(None),
                Some(i) => {
                    images.entry(i.clone()).or_insert_with(|| built(&root, &base, i)).clone().map(Some)
                }
            };
            let made = Command::new("docker").args(volume_args(&s.name)).output();
            let image = image.and_then(|i| match made {
                Ok(o) if o.status.success() => Ok(i),
                _ => Err(format!("docker volume create {} failed", volume_of(&s.name))),
            });
            ready.insert(s.name.clone(), image);
        }
        let jobs = docker_jobs();
        eprintln!("the one Linux build of tarae's tests, kept for every stack, on {jobs} jobs …");
        let mut build = Command::new(&xtask);
        build.arg("linux").args(keep(jobs)).current_dir(&root);
        if !build.status().map_err(|e| format!("cargo xtask linux: {e}"))?.success() {
            let why = "the Linux build of tarae's tests failed (above)".to_string();
            ready.values_mut().for_each(|r| *r = Err(why.clone()));
        }
    }
    for t in &a.linux {
        columns.push(t.column());
        for s in &stacks {
            let d = match &docker {
                Err(why) => {
                    Driven { skipped: Some(format!("Docker isn't reachable: {why}")), ..Driven::default() }
                }
                Ok(()) => match ready.get(&s.name).cloned().unwrap_or(Ok(None)) {
                    Err(why) => Driven { skipped: Some(why), ..Driven::default() },
                    Ok(image) => {
                        let on = image.as_deref().unwrap_or(&base);
                        eprintln!("{}, {}: in {on}, home {}", s.name, t.column(), volume_of(&s.name));
                        let mut c = Command::new(&xtask);
                        c.arg("linux").args(linux_args(s, a.lane, *t, image.as_deref())).current_dir(&root);
                        driven(c)?
                    }
                },
            };
            fill(&mut cells, t.column(), std::slice::from_ref(s), a.lane, &d);
        }
    }
    let cap = match (&docker, a.linux.is_empty()) {
        (Ok(()), false) => {
            Some(say_cap(&base).unwrap_or_else(|why| format!("compat's share not measured: {why}")))
        }
        _ => None,
    };
    let (date, commit) = (crate::perf::today(), crate::perf::commit(&root));
    let file = root.join(HISTORY);
    let past = std::fs::read_to_string(&file).unwrap_or_default();
    let head = format!("{date}, {commit}, the {} lane", a.lane.word());
    let md = table(&head, &columns, &stacks, a.lane, &cells, &past);
    // (named by its columns: this machine's OS where it ran here, `linux` where Linux tiers did)
    let mut on: Vec<&str> = Vec::new();
    if a.here {
        on.push(std::env::consts::OS);
    }
    if !a.linux.is_empty() && !on.contains(&"linux") {
        on.push("linux");
    }
    let out = root.join(OUT).join(format!("{date}-{}.md", on.join("-")));
    std::fs::create_dir_all(root.join(OUT)).map_err(|e| e.to_string())?;
    std::fs::write(&out, &md).map_err(|e| format!("{}: {e}", out.display()))?;
    let kept = history(&past, (&date, &commit, a.lane), &cells);
    std::fs::write(&file, kept).map_err(|e| format!("{}: {e}", file.display()))?;
    println!("{md}\nwritten to {}, kept in {}", out.display(), file.display());
    if let Some(cap) = cap {
        println!("{cap}");
    }
    match cells.values().filter(|c| c.starts_with('✗')).count() {
        0 => Ok(()),
        n => Err(format!("{n} cells failed (✗)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    const NODE: &str = r#"
stack = "node-npm"
fixture = "node-npm"
image = "node"
prewarm = ["npm ci", "echo 'it''s warm'"]
[[entry]]
name = "test"
run = "npm test"
expect = "pass"
quick = true
[[entry]]
name = "lint"
run = "npx eslint ."
expect = "pass"
[[entry]]
name = "add"
run = "npm install left-pad"
expect = "refused"
pending = "manifest-review"
[[entry]]
name = "audit"
run = "npm audit"
expect = "pass"
asks = true
"#;

    #[test]
    fn the_arguments_are_stacks_a_lane_tiers_and_a_dry_run() {
        let a = ask(&args("node-npm go --lane quick --linux all --dry-run")).unwrap();
        assert_eq!(
            a,
            Ask {
                stacks: vec!["node-npm".into(), "go".into()],
                lane: Lane::Quick,
                linux: vec![Tier::Own, Tier::Host],
                here: true,
                dry: true
            }
        );
        assert!(!ask(&args("--linux host --linux-only")).unwrap().here);
        let a = ask(&[]).unwrap();
        assert_eq!((a.lane, a.linux.len(), a.dry), (Lane::Gate, 0, false));
        assert_eq!(ask(&args("--linux host")).unwrap().linux, vec![Tier::Host]);
        assert_eq!(ask(&args("--lane net")).unwrap().lane, Lane::Net);
        assert_eq!(ask(&args("--lane asks")).unwrap().lane, Lane::Asks);
        for (bad, why) in [
            ("--lane", "--lane ?"),
            ("--lane fast", "gate, quick, asks or net"),
            ("--linux b", "own, host or all"),
            ("--repeat", "--repeat?"),
            ("../node", "a stack's name"),
            ("--linux-only", "--linux-only: with --linux"),
        ] {
            let r = ask(&args(bad));
            assert!(r.as_ref().is_err_and(|e| e.contains(why) && e.contains(USAGE)), "{bad}: {r:?}");
        }
    }

    #[test]
    fn a_manifest_gives_the_rows_and_what_runs_before_them() {
        let s = stack("node-npm", NODE).unwrap();
        assert_eq!(s.fixture.as_deref(), Some("node-npm"));
        assert_eq!(s.prewarm, ["npm ci", "echo 'it''s warm'"]);
        let names = |lane| rows(&s, lane).map(|r| r.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names(Lane::Gate), ["test", "lint", "add"]);
        assert_eq!(names(Lane::Quick), ["test"]);
        assert_eq!(names(Lane::Asks), ["audit"]);
        assert_eq!(s.rows[2].pending.as_deref(), Some("manifest-review"));
        assert!(stack("node", NODE).is_err_and(|e| e.contains("stack = \"node\" expected")));
        let dir = std::env::temp_dir().join(format!("xtask-compat-stacks-{}", std::process::id()));
        let _ = std::fs::create_dir(&dir);
        assert!(stacks(&dir, &[]).is_err_and(|e| e.contains("piece 3")));
        std::fs::write(dir.join("node-npm.toml"), NODE).unwrap();
        assert_eq!(stacks(&dir, &[]).unwrap().len(), 1);
        assert!(stacks(&dir, &["go".into()]).is_err_and(|e| e.contains("no stack go — there are: node-npm")));
        std::fs::remove_file(dir.join("node-npm.toml")).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn the_dry_run_says_every_command_in_order_the_prewarm_too() {
        let s = [stack("node-npm", NODE).unwrap()];
        let tag = "tarae-compat-node:1.99-0123456789ab";
        let recipes = BTreeMap::from([("node".to_string(), (tag.to_string(), "FROM $BASE".to_string()))]);
        let p = plan(&s, Lane::Gate, &[Tier::Own, Tier::Host], ("cargo", 2), "rust:1.99", &recipes);
        let drive = "-- --ignored --exact compat::tests::driver --nocapture --test-threads=1";
        assert_eq!(
            p,
            [
                "cargo test --profile compat-tests --bin tarae --no-run --quiet".to_string(),
                "# node-npm: the driver first runs, outside the sandbox, in a copy of compat/node-npm/:"
                    .into(),
                "sh -c 'npm ci'".into(),
                "# node-npm: the driver first runs, outside the sandbox, in a copy of compat/node-npm/:"
                    .into(),
                r#"sh -c 'echo '\''it'\'''\''s warm'\'''"#.into(),
                format!(
                    "TARAE_COMPAT_STACKS=node-npm TARAE_COMPAT_LANE=gate cargo test --profile compat-tests --bin tarae {drive}"
                ),
                format!("# node: built where {tag} isn't yet, then the tags its recipe built before dropped"),
                format!(
                    "docker 'build' '-t' '{tag}' '--label' 'tarae.compat=node' '--build-arg' 'BASE=rust:1.99' '-' \
                     < compat/linux/node.Dockerfile"
                ),
                "docker volume create --label tarae.compat=node-npm tarae-compat-node-npm-cache".into(),
                "# the one build every stack's Linux run takes, kept in the checkout's target volume".into(),
                "cargo xtask linux '--env' 'CARGO_BUILD_JOBS=2' 'test' '--keep' '--profile' 'compat-tests' '--bin' \
                 'tarae'"
                    .into(),
                format!("# node-npm, Linux own: in {tag}, home tarae-compat-node-npm-cache"),
                format!(
                    "cargo xtask linux '--env' 'TARAE_COMPAT_STACKS=node-npm' '--env' 'TARAE_COMPAT_LANE=gate' \
                     '--env' 'TARAE_COMPAT_TIER=own' '--userns' '--image' '{tag}' '--volume' \
                     'tarae-compat-node-npm-cache:/root' 'test' '--kept' \
                     '--ignored' '--exact' 'compat::tests::driver' '--nocapture' '--test-threads=1'"
                ),
                format!("# node-npm, Linux host: in {tag}, home tarae-compat-node-npm-cache"),
                format!(
                    "cargo xtask linux '--env' 'TARAE_COMPAT_STACKS=node-npm' '--env' 'TARAE_COMPAT_LANE=gate' \
                     '--env' 'TARAE_COMPAT_TIER=host' '--image' '{tag}' '--volume' \
                     'tarae-compat-node-npm-cache:/root' 'test' '--kept' '--ignored' '--exact' 'compat::tests::driver' '--nocapture' '--test-threads=1'"
                ),
            ]
        );
        // (a stack with no image runs on rust:<pin> itself; one whose recipe is missing says so)
        let rust = [
            stack("rust", "stack = \"rust\"").unwrap(),
            stack("go", "stack = \"go\"\nimage = \"go\"").unwrap(),
        ];
        let p = plan(&rust, Lane::Gate, &[Tier::Host], ("cargo", 16), "rust:1.99", &recipes);
        assert!(p.iter().any(|l| l.contains("'CARGO_BUILD_JOBS=16'")), "{p:#?}");
        assert!(
            p.contains(&"# go: no recipe compat/linux/go.Dockerfile — its stacks' cells skipped".to_string())
        );
        assert!(
            p.contains(&"# rust, Linux host: in rust:1.99, home tarae-compat-rust-cache".to_string()),
            "{p:#?}"
        );
        assert!(!p.iter().any(|l| l.contains("'--image'") && l.contains("STACKS=rust")));
        // (a fixture kept in the manifest: its files said)
        let written = format!(
            "{}[files]\n\"a\" = \"x\"\n\"b/c\" = \"y\"\n",
            NODE.replace("fixture = \"node-npm\"\n", "")
        );
        let s = [stack("node-npm", &written).unwrap()];
        assert_eq!(
            plan(&s, Lane::Gate, &[], ("cargo", 2), "rust:1.99", &BTreeMap::new())[1],
            "# node-npm: the driver first runs, outside the sandbox, in a project of the 2 files compat/node-npm.toml writes:"
        );
    }

    #[test]
    fn cells_come_from_the_drivers_lines_and_a_missing_one_says_why() {
        let s = [stack("node-npm", NODE).unwrap()];
        let mut d = Driven { ok: true, ..Driven::default() };
        for l in [
            "running 1 test",
            "test compat::tests::driver ... COMPAT\tnode-npm\ttest\t✓ 1.7×",
            "COMPAT\tnode-npm\t(manifest)\t✗ x\ty",
        ] {
            d.line(l);
        }
        let mut cells = Cells::new();
        fill(&mut cells, "macOS", &s, Lane::Gate, &d);
        let of = |c: &Cells, col: &str, e: &str| {
            c[&(col.to_string(), "node-npm".to_string(), e.to_string())].clone()
        };
        assert_eq!(of(&cells, "macOS", "test"), "✓ 1.7×");
        assert_eq!(of(&cells, "macOS", "lint"), "– no result");
        assert_eq!(of(&cells, "macOS", "(manifest)"), "✗ x\ty", "the cell is the line's rest");
        let mut skipped = Driven::default();
        skipped.line(&format!("skipped — Docker isn't reachable: {}", "no socket"));
        fill(&mut cells, "Linux host", &s, Lane::Quick, &skipped);
        assert_eq!(of(&cells, "Linux host", "test"), "– skipped: Docker isn't reachable: no socket");
        assert!(
            !cells.contains_key(&("Linux host".into(), "node-npm".into(), "lint".into())),
            "not in the lane"
        );
        fill(&mut cells, "Linux own", &s, Lane::Quick, &Driven::default());
        assert_eq!(of(&cells, "Linux own", "test"), "✗ harness (its output above)");
    }

    #[test]
    fn the_table_is_commands_by_columns_pending_apart_and_what_changed_since() {
        let s = [stack("node-npm", NODE).unwrap()];
        let mut cells = Cells::new();
        for (c, e, cell) in [
            ("macOS", "test", "✓"),
            ("macOS", "lint", "✗ card"),
            ("macOS", "add", "⊘"),
            ("Linux host", "test", "✓ 1.7×"),
            ("Linux host", "lint", "– skipped: a | b"),
        ] {
            cells.insert((c.into(), "node-npm".into(), e.into()), cell.into());
        }
        let past = history("", ("2026-10-14", "abc", Lane::Gate), &{
            let mut was = cells.clone();
            was.insert(("macOS".into(), "node-npm".into(), "lint".into()), "✓".into());
            was
        });
        let t =
            table("2026-10-15, def, the gate lane", &["macOS", "Linux host"], &s, Lane::Gate, &cells, &past);
        assert_eq!(
            t,
            "# compat — 2026-10-15, def, the gate lane\n\n\
             ## node-npm\n\n\
             | command | macOS | Linux host | pending |\n\
             |---|---|---|---|\n\
             | test | ✓ | ✓ 1.7× |  |\n\
             | lint | ✗ card | – skipped: a \\| b |  |\n\
             | add | ⊘ | – | manifest-review |\n\n\
             ## Since the last run\n\n\
             - node-npm lint, macOS: ✓ → ✗ card\n"
        );
        let kept = history(&past, ("2026-10-15", "def", Lane::Gate), &cells);
        assert!(kept.starts_with(HISTORY_HEAD) && kept.matches("date\t").count() == 1);
        assert!(kept.ends_with("2026-10-15\tdef\tgate\tmacOS\tnode-npm\ttest\t✓\n"), "{kept}");
        assert!(changes(&cells, &kept).is_empty(), "the last run is what's compared");
        let quick = table("h", &["macOS"], &s, Lane::Quick, &Cells::new(), "");
        assert_eq!(quick, "# compat — h\n\n## node-npm\n\n| command | macOS |\n|---|---|\n| test | – |\n");
    }

    /// The Linux build's jobs follow the Docker machine: a CPU and 4 GiB a job, 2 to 16 — colima's shared VM 2, a
    /// 32-core 128 GB one 16, an unknown one 2.
    #[test]
    fn the_builds_jobs_follow_the_docker_machine() {
        let gib = 1u64 << 30;
        assert_eq!(jobs_for(Some(4), Some(11_700_000_000)), 2);
        assert_eq!(jobs_for(Some(32), Some(128 * gib)), 16);
        assert_eq!(jobs_for(Some(8), Some(64 * gib)), 8);
        assert_eq!(jobs_for(Some(32), Some(24 * gib)), 6);
        assert_eq!(jobs_for(None, Some(128 * gib)), 2);
    }

    /// An image's tag is its recipe's and its base's: either changed, a new tag — and what the old one built is
    /// stale, dropped; no other image is.
    #[test]
    fn a_changed_recipe_is_a_new_tag_and_the_old_ones_image_is_stale() {
        let t = tag("go", "rust:1.99", "FROM $BASE\nRUN a");
        assert!(
            t.starts_with("tarae-compat-go:1.99-") && t.len() == "tarae-compat-go:1.99-".len() + 12,
            "{t}"
        );
        assert_eq!(t, tag("go", "rust:1.99", "FROM $BASE\nRUN a"));
        assert_ne!(t, tag("go", "rust:1.99", "FROM $BASE\nRUN b"));
        assert_ne!(t, tag("go", "rust:1.100", "FROM $BASE\nRUN a"));
        let listed = format!(
            "{t}\ntarae-compat-go:1.99-000000000000\ntarae-compat-node:1.98-111111111111\ntarae-compat-go:<none>\n\
             tarae-trace:1.99-ab\nrust:1.99\n"
        );
        assert_eq!(
            stale(&listed, std::slice::from_ref(&t)),
            ["tarae-compat-go:1.99-000000000000", "tarae-compat-node:1.98-111111111111"]
        );
    }

    /// The cap line counts only compat's images (their own layers) and volumes, each named, and says the way out
    /// past `CAP`.
    #[test]
    fn the_cap_line_sums_compats_own_and_says_past_the_cap() {
        let images = "tarae-compat-go\t412.5MB\nrust\t1.6GB\ntarae-compat-node\t190MB\n";
        let volumes = "tarae-compat-go-cache\t300MB\ttarae.compat=go\ntarae-target-1.99.0-ab\t6.2GB\t\n";
        let disk = crate::linux::disk("overlay 41000000 25000000 14000000 65% /");
        assert_eq!(
            cap_line(images, volumes, disk),
            "compat's Docker images and volumes take 0.9 GB of its 10.0 GB cap: images 0.6 GB (go 0.4 GB, node 0.2 GB), \
             volumes 0.3 GB (go-cache 0.3 GB); the disk 65 % used (26 of 42 GB)"
        );
        let line = cap_line("tarae-compat-java\t9.5GB\n", "tarae-compat-java-gradle-cache\t1GB\t\n", None);
        assert!(
            line.starts_with("compat's Docker images and volumes take 10.5 GB of its 10.0 GB cap — past it"),
            "{line}"
        );
    }
}
