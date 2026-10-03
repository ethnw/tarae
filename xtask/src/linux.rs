//! `cargo xtask linux [clippy|test]` — tarae's checks on Linux (CI runs there too: `-D dead-code` on code only
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

use std::path::Path;
use std::process::Command;

use crate::{Result, root};

#[derive(Clone, Copy, Debug, PartialEq)]
enum What {
    Clippy,
    Test,
    Both,
}

pub fn run(args: &[String]) -> Result<()> {
    let what = match args.first().map(String::as_str) {
        None => What::Both,
        Some("clippy") => What::Clippy,
        Some("test") => What::Test,
        Some(a) => return Err(format!("usage: cargo xtask linux [clippy|test] — not {a}")),
    };
    let root = root();
    let toolchain = root.join("rust-toolchain.toml");
    let pin =
        pin(&std::fs::read_to_string(&toolchain).map_err(|e| format!("{}: {e}", toolchain.display()))?)?;
    if let Err(why) = reachable(&mut Command::new("docker")) {
        println!("{}", skipped(&why));
        return Ok(());
    }
    eprintln!("Linux checks on {} (Rust {pin}) …", image(&pin));
    let st = Command::new("docker")
        .args(docker_args(&root, &pin, what))
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

/// `docker run …` for the checks: the tree read-only at /src, the caches in volumes, the pinned image.
fn docker_args(root: &Path, pin: &str, what: What) -> Vec<String> {
    let mut a: Vec<String> = ["run", "--rm"].map(String::from).into();
    let mount = |a: &mut Vec<String>, v: String| a.extend(["-v".to_string(), v]);
    mount(&mut a, format!("{}:/src:ro", root.display()));
    mount(&mut a, format!("tarae-target-{pin}:/target"));
    mount(&mut a, "tarae-registry:/usr/local/cargo/registry".into());
    // (seeded from the image the first time: the file's components and targets are fetched once)
    mount(&mut a, format!("tarae-rustup-{pin}:/usr/local/rustup"));
    for e in ["CARGO_TARGET_DIR=/target", "CARGO_TERM_COLOR=never"] {
        a.extend(["-e".to_string(), e.to_string()]);
    }
    a.extend([image(pin), "sh".into(), "-ec".into(), script(what)]);
    a
}

/// The image for `pin`: `rust:<major.minor>` — rustup in it then takes `rust-toolchain.toml`'s exact version
/// (installing it, into the rustup volume, when the image's patch release differs). The exact tag would be a
/// pull of its own for every patch release, for the same compiler.
fn image(pin: &str) -> String {
    let minor: Vec<&str> = pin.split('.').take(2).collect();
    format!("rust:{}", minor.join("."))
}

/// What runs in the container: the clone (with what isn't committed yet), then the checks.
fn script(what: What) -> String {
    let mut s = String::from(
        "git config --global --add safe.directory '*'\n\
         git clone -q /src /work/tarae && cd /work/tarae\n\
         git -C /src diff HEAD | git apply --allow-empty\n\
         git -C /src ls-files --others --exclude-standard -z | (cd /src && tar --null -T - -cf -) | tar -xf -\n\
         echo \"toolchain: $(rustc --version)\"\n",
    );
    if what != What::Test {
        s.push_str("cargo clippy --locked --all-targets -- -D warnings\n");
        s.push_str("cargo clippy --locked --all-targets --features bundled-grammars -- -D warnings\n");
    }
    if what != What::Clippy {
        s.push_str("cargo test --locked\n");
    }
    s
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
        let a = docker_args(Path::new("/r/tarae"), "1.99.0", What::Both);
        assert!(a.windows(2).any(|w| w == ["-v", "/r/tarae:/src:ro"]), "{a:?}");
        assert!(
            !a.iter().any(|v| v.starts_with("/r/tarae:") && !v.ends_with(":ro")),
            "no writable mount: {a:?}"
        );
        assert!(
            a.contains(&"tarae-target-1.99.0:/target".to_string()) && a.contains(&"rust:1.99".to_string())
        );
        assert_eq!((image("1.100.2"), image("1.99")), ("rust:1.100".to_string(), "rust:1.99".to_string()));
        let s = a.last().unwrap();
        assert!(s.contains("git clone -q /src /work/tarae && cd /work/tarae"), "{s}");
        assert!(s.contains("git -C /src diff HEAD | git apply") && s.contains("ls-files --others"), "{s}");
        assert!(
            s.contains("--features bundled-grammars -- -D warnings") && s.contains("cargo test --locked")
        );
        let clippy = script(What::Clippy);
        assert!(clippy.contains("cargo clippy") && !clippy.contains("cargo test"));
        let test = script(What::Test);
        assert!(test.contains("cargo test") && !test.contains("cargo clippy"));
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
