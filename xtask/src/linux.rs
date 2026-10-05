//! `cargo xtask linux [clippy|test [--repeat N] [cargo test's arguments]]` — tarae's checks on Linux (CI runs there too: `-D dead-code` on code only
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
//! The target volume is the checkout's own (`target_volume`, labelled with its path): every checkout's clone is
//! /work/tarae, so one shared target let a session's tests run another session's build (its e2e ran another's
//! `target/debug/tarae`). Each run drops the volumes of checkouts that are gone; `cargo xtask linux prune` does
//! that now, and drops the old shared `tarae-target-<pin>` too.

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

const USAGE: &str = "usage: cargo xtask linux [clippy | test [--repeat N] [cargo test's arguments]]";

/// What `cargo xtask linux …`'s arguments ask for.
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
    let what = what(args)?;
    let root = root();
    let toolchain = root.join("rust-toolchain.toml");
    let pin =
        pin(&std::fs::read_to_string(&toolchain).map_err(|e| format!("{}: {e}", toolchain.display()))?)?;
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    eprintln!("Linux checks on {} (Rust {pin}) …", image(&pin));
    prune_volumes(false);
    // (made here, not by `docker run`: only a volume made this way carries its checkout's label)
    let made = Command::new("docker").args(create_args(&root, &pin)).output();
    if !made.as_ref().is_ok_and(|o| o.status.success()) {
        return Err(format!("docker volume create {}: failed", target_volume(&root, &pin)));
    }
    let st = Command::new("docker")
        .args(docker_args(&root, common_git_dir(&root).as_deref(), &pin, &what))
        .status()
        .map_err(|e| format!("docker: {e}"))?;
    if !st.success() {
        return Err("the Linux checks failed (above)".into());
    }
    Ok(())
}

/// `rust-toolchain.toml`'s `channel`.
fn pin(toolchain: &str) -> Result<String> {
    let t: toml::Table = toolchain.parse().map_err(|e| format!("rust-toolchain.toml: {e}"))?;
    t.get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| "rust-toolchain.toml: no [toolchain] channel".into())
}

/// Whether the Docker `docker` would talk to answers — Err: why not, in a line.
fn reachable(docker: &mut Command) -> std::result::Result<(), String> {
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
fn skipped(why: &str) -> String {
    format!(
        "skipped — Docker isn't reachable: {why} (start it — colima start -p tarae-test — or set DOCKER_CONTEXT)"
    )
}

/// The repository's git folder when it isn't the tree's own `.git` — a worktree (every tarae session is
/// one): its `.git` is a file naming a folder under the main checkout's, which the clone inside reads.
fn common_git_dir(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    (!dir.starts_with(root)).then_some(dir)
}

/// `docker run …` for the checks: the tree read-only at /src (a worktree's git folder read-only too, where
/// its `.git` file says), the caches in volumes, the pinned image.
fn docker_args(root: &Path, git: Option<&Path>, pin: &str, what: &What) -> Vec<String> {
    let mut a: Vec<String> = ["run", "--rm"].map(String::from).into();
    let mount = |a: &mut Vec<String>, v: String| a.extend(["-v".to_string(), v]);
    mount(&mut a, format!("{}:/src:ro", root.display()));
    if let Some(git) = git {
        mount(&mut a, format!("{0}:{0}:ro", git.display()));
    }
    mount(&mut a, format!("{}:/target", target_volume(root, pin)));
    mount(&mut a, "tarae-registry:/usr/local/cargo/registry".into());
    // (seeded from the image the first time: the file's components and targets are fetched once)
    mount(&mut a, format!("tarae-rustup-{pin}:/usr/local/rustup"));
    for e in ["CARGO_TARGET_DIR=/target", "CARGO_TERM_COLOR=never"] {
        a.extend(["-e".to_string(), e.to_string()]);
    }
    a.extend([image(pin), "sh".into(), "-ec".into(), script(what)]);
    a
}

/// The label naming the checkout a target volume is for.
const CHECKOUT_LABEL: &str = "tarae.checkout";

/// The checkout's own target volume: `tarae-target-<pin>-<hash of its path>` (FNV-1a — the same path gives the
/// same name on every run and Rust version).
fn target_volume(root: &Path, pin: &str) -> String {
    let hash = root
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    format!("tarae-target-{pin}-{hash:016x}")
}

/// `docker volume create …` for it, labelled with the checkout's path (an existing one is left as it is).
fn create_args(root: &Path, pin: &str) -> Vec<String> {
    let label = format!("{CHECKOUT_LABEL}={}", root.display());
    ["volume", "create", "--label", &label, &target_volume(root, pin)].map(String::from).into()
}

/// `docker volume ls`'s rows for the target volumes: (name, its checkout's path — empty when unlabelled).
fn volumes(listed: &str) -> Vec<(String, String)> {
    listed
        .lines()
        .filter_map(|l| {
            let (name, path) = l.split_once('\t').unwrap_or((l, ""));
            name.starts_with("tarae-target-").then(|| (name.to_string(), path.trim().to_string()))
        })
        .collect()
}

/// The target volumes to drop: those whose checkout is gone (`is_dir` says) — and with `shared`, the one every
/// checkout shared before (`tarae-target-<pin>`, no label). An unlabelled one of another form isn't ours to
/// judge: kept.
fn prunable(volumes: &[(String, String)], is_dir: impl Fn(&Path) -> bool, shared: bool) -> Vec<String> {
    let old_form = |n: &str| {
        n.strip_prefix("tarae-target-").is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    volumes
        .iter()
        .filter(|(name, path)| match path.is_empty() {
            true => shared && old_form(name),
            false => !is_dir(Path::new(path)),
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// `cargo xtask linux prune`: the target volumes no checkout uses, the old shared one too.
pub fn prune() -> Result<()> {
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    if prune_volumes(true) == 0 {
        println!("no target volume to drop — every one is a checkout's that's there");
    }
    Ok(())
}

/// Drops what `prunable` says, each said; one still in use (a run going) stays. How many went.
fn prune_volumes(shared: bool) -> usize {
    let format = format!("{{{{.Name}}}}\t{{{{.Label \"{CHECKOUT_LABEL}\"}}}}");
    let Ok(out) = Command::new("docker")
        .args(["volume", "ls", "--filter", "name=tarae-target-", "--format", &format])
        .output()
    else {
        return 0;
    };
    let listed = volumes(&String::from_utf8_lossy(&out.stdout));
    let mut gone = 0;
    for name in prunable(&listed, Path::is_dir, shared) {
        let rm = Command::new("docker").args(["volume", "rm", &name]).output();
        match rm {
            Ok(o) if o.status.success() => {
                gone += 1;
                let of = listed.iter().find(|(n, _)| *n == name).map(|(_, p)| p.as_str()).unwrap_or_default();
                match of.is_empty() {
                    true => eprintln!("dropped {name} (the target every checkout shared)"),
                    false => eprintln!("dropped {name} ({of} is gone)"),
                }
            }
            _ => eprintln!("kept {name} for now: Docker didn't drop it (a run using it?)"),
        }
    }
    gone
}

/// The image for `pin`: `rust:<major.minor>` — rustup in it then takes `rust-toolchain.toml`'s exact version
/// (installing it, into the rustup volume, when the image's patch release differs). The exact tag would be a
/// pull of its own for every patch release, for the same compiler.
fn image(pin: &str) -> String {
    let minor: Vec<&str> = pin.split('.').take(2).collect();
    format!("rust:{}", minor.join("."))
}

/// What runs in the container: the clone (with what isn't committed yet), then the checks.
fn script(what: &What) -> String {
    let mut s = String::from(
        "git config --global --add safe.directory '*'\n\
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
        What::Both => s.push_str("cargo test --locked\n"),
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
        let a = docker_args(Path::new("/r/tarae"), None, "1.99.0", &What::Both);
        assert!(a.windows(2).any(|w| w == ["-v", "/r/tarae:/src:ro"]), "{a:?}");
        assert!(
            !a.iter().any(|v| v.starts_with("/r/tarae:") && !v.ends_with(":ro")),
            "no writable mount: {a:?}"
        );
        let target = format!("{}:/target", target_volume(Path::new("/r/tarae"), "1.99.0"));
        assert!(a.contains(&target) && a.contains(&"rust:1.99".to_string()), "{a:?}");
        assert_eq!((image("1.100.2"), image("1.99")), ("rust:1.100".to_string(), "rust:1.99".to_string()));
        let s = a.last().unwrap();
        assert!(s.contains("git clone -q /src /work/tarae && cd /work/tarae"), "{s}");
        assert!(s.contains("git -C /src diff HEAD | git apply") && s.contains("ls-files --others"), "{s}");
        assert!(
            s.contains("--features bundled-grammars -- -D warnings") && s.contains("cargo test --locked")
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

    /// A worktree (a tarae session): the main checkout's git folder goes in read-only, at the path its `.git`
    /// file names — and only then. Found from a real worktree of a temp repository.
    #[test]
    fn a_worktrees_git_folder_goes_in_read_only() {
        let a = docker_args(Path::new("/w/s"), Some(Path::new("/r/tarae/.git")), "1.99.0", &What::Clippy);
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
        let name = target_volume(a, "1.99.0");
        assert_eq!(name, target_volume(a, "1.99.0"), "the same on every run");
        assert_ne!(name, target_volume(b, "1.99.0"), "another checkout, another target");
        assert_ne!(name, target_volume(a, "1.100.0"), "and per pin");
        assert!(name.starts_with("tarae-target-1.99.0-") && name.len() == "tarae-target-1.99.0-".len() + 16);
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)), "a Docker volume name");
        let made = create_args(a, "1.99.0");
        assert_eq!(made, ["volume", "create", "--label", "tarae.checkout=/w/tarae/s1", name.as_str()]);
        let args = docker_args(a, None, "1.99.0", What::Test);
        assert!(args.contains(&format!("{name}:/target")), "{args:?}");
        assert!(args.contains(&"tarae-registry:/usr/local/cargo/registry".to_string()), "{args:?}");
        assert!(args.contains(&"tarae-rustup-1.99.0:/usr/local/rustup".to_string()), "{args:?}");
    }

    /// What's dropped: a target whose checkout is gone; the old shared one only when asked (`prune`); never a
    /// live checkout's, nor an unlabelled one of another form.
    #[test]
    fn a_gone_checkouts_target_is_dropped() {
        let listed = volumes(
            "tarae-target-1.99.0-aaaa\t/w/live\n\
             tarae-target-1.99.0-bbbb\t/w/gone\n\
             tarae-target-1.99.0\t\n\
             tarae-target-mine\t\n\
             tarae-registry\t\n",
        );
        assert_eq!(listed.len(), 4, "only target volumes: {listed:?}");
        let live = |p: &Path| p == Path::new("/w/live");
        assert_eq!(prunable(&listed, live, false), ["tarae-target-1.99.0-bbbb"], "each run");
        assert_eq!(
            prunable(&listed, live, true),
            ["tarae-target-1.99.0-bbbb", "tarae-target-1.99.0"],
            "prune: the old shared one too"
        );
        assert!(prunable(&listed, |_| true, false).is_empty(), "every checkout there: nothing");
        assert_eq!(volumes("tarae-target-1.99.0\n"), [("tarae-target-1.99.0".to_string(), String::new())]);
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
