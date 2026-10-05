//! `cargo xtask trace [offcpu|syscalls|pty] [scenario …]` · `cargo xtask trace test` — tarae measured with
//! eBPF on Linux (the `trace/` crate, outside the workspace). Everything happens in one container on the Docker the CLI
//! reaches (`linux`'s rules: colima `tarae-test` here): the tool and tarae are built there — tarae with frame
//! pointers, in the `perf` profile — and tarae is driven there by `tests/perf.rs`'s scenarios under a pty, so
//! the tool sees its threads in its own pid namespace (no `--pid=host`).
//!
//! Privileges: `CAP_BPF` + `CAP_PERFMON` (load and attach tracing programs, stack maps) and `CAP_SYSLOG`
//! (kallsyms' addresses — kptr_restrict is 1 — for kernel frames); no `--privileged`, no host `/sys`. Mounts:
//! the tree read-only, volumes for the registry, rustup, a target of its own (frame pointers would thrash
//! `linux`'s) and the kept checkout (`SYNC`). Results leave as a tar on the container's stdout (everything
//! else goes to stderr) and land in `target/trace/<date>-<time>/`: `<probe>-<scenario>.json` (and `.folded`).
//! The image (`tarae-trace:<major.minor>-<hash of its Dockerfile>`) is built when missing.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::linux::{common_git_dir, image, pin, reachable, skipped};
use crate::{Result, root};

/// The probes, and the scenarios each traces when none is named: a key and scrolling in a 200k-line file;
/// for the terminal's writes, scrolling with a fast reader and with a slow one.
const PROBES: [(&str, [&str; 2]); 3] = [
    ("offcpu", ["perf_e2e_key_in_a_big_file", "perf_e2e_scrolling"]),
    ("syscalls", ["perf_e2e_key_in_a_big_file", "perf_e2e_scrolling"]),
    ("pty", ["perf_e2e_scrolling", "perf_e2e_scrolling_slow_terminal"]),
];
/// What the image adds to the Rust one: clang for the BPF C, bpftool for vmlinux.h, libbpf's headers, what
/// libbpf-sys builds libbpf with, libc's debug symbols (Debian strips them: its frames would be offsets),
/// and rsync for the kept checkout (`SYNC`).
const PACKAGES: &str = "clang llvm bpftool libbpf-dev libelf-dev zlib1g-dev pkg-config make libc6-dbg rsync";
/// The tree at /work/tarae, kept in a volume between runs and brought up to date by content: a fresh clone
/// of the read-only tree (what isn't committed applied, untracked files copied) synced over it with
/// `rsync --checksum`, which leaves files whose bytes didn't change alone — their mtimes stay, so cargo
/// rebuilds only what changed (a fresh clone each run relinked all of tarae: ~110 s).
const SYNC: &str = "git config --global --add safe.directory '*'\n\
     rm -rf /tmp/new && git clone -q --depth 1 file:///src /tmp/new && cd /tmp/new\n\
     git -C /src diff HEAD | git apply --allow-empty\n\
     git -C /src ls-files --others --exclude-standard -z | (cd /src && tar --null -T - -cf -) | tar -xf -\n\
     mkdir -p /work/tarae && rsync -rlc --delete --exclude=/target /tmp/new/ /work/tarae/ && cd /work/tarae\n\
     echo \"toolchain: $(rustc --version)\"\n";
/// The most one scenario is traced: a scenario that never ends (a tarae that doesn't quit) still gives its
/// trace back, and the next one runs.
const FOR: &str = "5m";
/// The capabilities the container gets on top of Docker's defaults.
const CAPS: [&str; 3] = ["BPF", "PERFMON", "SYSLOG"];

#[derive(Clone, Debug, PartialEq)]
enum What {
    /// The tool's own tests (the fixtures with known waits, calls and writes).
    Test,
    /// clippy on the tool, warnings as errors — it only builds on Linux, so its check is here too.
    Clippy,
    /// A probe over these perf scenarios, with these options of the probe's (`--stacks mprotect`).
    Probe(&'static str, Vec<String>, Vec<String>),
}

fn what(args: &[String]) -> Result<What> {
    let usage =
        "usage: cargo xtask trace [offcpu|syscalls [--stacks SYSCALL,…]|pty] [scenario …] | test | clippy";
    let first = args.first().map(String::as_str);
    match (first, args.len()) {
        (Some("test"), 1) => return Ok(What::Test),
        (Some("clippy"), 1) => return Ok(What::Clippy),
        (Some("test" | "clippy"), _) => return Err(usage.into()),
        _ => {}
    }
    let (probe, defaults, mut rest) = match PROBES.iter().find(|(p, _)| Some(*p) == first) {
        Some((p, d)) => (*p, d, &args[1..]),
        None => (PROBES[0].0, &PROBES[0].1, args),
    };
    let mut options = Vec::new();
    if probe == "syscalls" && rest.first().map(String::as_str) == Some("--stacks") {
        let list = rest.get(1).ok_or(usage)?;
        // (words in the container's script too)
        if list.is_empty()
            || !list.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_,".contains(&b))
        {
            return Err(format!("{usage} — --stacks takes syscall names, not {list:?}"));
        }
        options = vec!["--stacks".to_string(), list.clone()];
        rest = &rest[2..];
    }
    for s in rest {
        // (they go into the container's script as words)
        if !s.starts_with("perf_") || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(format!("{usage} — a scenario is a perf_… test of tests/perf.rs, not {s:?}"));
        }
    }
    let rest = if rest.is_empty() { defaults.map(String::from).to_vec() } else { rest.to_vec() };
    Ok(What::Probe(probe, rest, options))
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
    let image = ensure_image(&pin)?;
    eprintln!("tracing in {image} — caps {}, the tree read-only …", CAPS.join(" "));
    let out = Command::new("docker")
        .args(docker_args(&root, common_git_dir(&root).as_deref(), &pin, &image, &what))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    let dir = root.join("target/trace").join(stamp());
    if !out.stdout.is_empty() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        tar::Archive::new(out.stdout.as_slice()).unpack(&dir).map_err(|e| format!("the results: {e}"))?;
        eprintln!("results in {}", dir.display());
    }
    if !out.status.success() {
        return Err("the trace run failed (above)".into());
    }
    Ok(())
}

/// The image's name — its Dockerfile's hash in the tag, so a change to it is a new image — built if missing.
fn ensure_image(pin: &str) -> Result<String> {
    let file = dockerfile(pin);
    let name = format!("tarae-trace:{}-{:08x}", &image(pin)["rust:".len()..], fnv(&file) as u32);
    let have = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", &name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if have {
        return Ok(name);
    }
    eprintln!("building the image {name} (once) …");
    let mut child = Command::new("docker")
        .args(["build", "-q", "-t", &name, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("docker build: {e}"))?;
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), file.as_bytes())
        .map_err(|e| format!("docker build: {e}"))?;
    if !child.wait().map_err(|e| format!("docker build: {e}"))?.success() {
        return Err(format!("building {name} failed (above)"));
    }
    Ok(name)
}

fn dockerfile(pin: &str) -> String {
    format!(
        "FROM {}\nRUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
         --no-install-recommends {PACKAGES} >/dev/null && rm -rf /var/lib/apt/lists/*\n",
        image(pin)
    )
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Why `trace/` can't be in tarae's workspace — `Err` says how it got in. Its build.rs needs clang and
/// bpftool, which CI (`cargo test --workspace` on ubuntu-latest), `:preflight` and `cargo xtask linux` don't
/// have: so it declares a workspace of its own, and no member or default member of the root's names it.
#[cfg(test)]
fn trace_outside_the_workspace(root: &str, trace: &str) -> Result<()> {
    let root: toml::Table = root.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
    let trace: toml::Table = trace.parse().map_err(|e| format!("trace/Cargo.toml: {e}"))?;
    if !trace.contains_key("workspace") {
        return Err("trace/Cargo.toml has no [workspace] of its own".into());
    }
    // (cargo's member globs: `*` within a path segment)
    let matches = |pattern: &str| {
        let parts: Vec<&str> = pattern.trim_end_matches('/').split('*').collect();
        let mut rest = "trace";
        for (i, p) in parts.iter().enumerate() {
            match rest.find(p) {
                Some(at) if i > 0 || at == 0 => rest = &rest[at + p.len()..],
                _ => return false,
            }
        }
        rest.is_empty() || parts.last() == Some(&"")
    };
    let ws = root.get("workspace").and_then(|w| w.as_table());
    for key in ["members", "default-members"] {
        let listed = ws.and_then(|w| w.get(key)).and_then(|m| m.as_array()).cloned().unwrap_or_default();
        if let Some(p) = listed.iter().filter_map(|m| m.as_str()).find(|p| matches(p)) {
            return Err(format!("the root's [workspace] {key} has {p:?}, which takes in trace/"));
        }
    }
    Ok(())
}

/// `2026-10-05-143012` (UTC).
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = crate::perf::civil(secs / 86_400);
    let t = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02}-{:02}{:02}{:02}", t / 3600, t / 60 % 60, t % 60)
}

/// `docker run …`: the capabilities BPF needs and no more, the tree read-only, volumes for the rest.
fn docker_args(root: &Path, git: Option<&Path>, pin: &str, image: &str, what: &What) -> Vec<String> {
    let mut a: Vec<String> = ["run", "--rm"].map(String::from).into();
    for cap in CAPS {
        a.extend(["--cap-add".into(), cap.into()]);
    }
    let mount = |a: &mut Vec<String>, v: String| a.extend(["-v".to_string(), v]);
    mount(&mut a, format!("{}:/src:ro", root.display()));
    if let Some(git) = git {
        mount(&mut a, format!("{0}:{0}:ro", git.display()));
    }
    mount(&mut a, format!("tarae-trace-target-{pin}:/target"));
    mount(&mut a, format!("tarae-trace-work-{pin}:/work"));
    mount(&mut a, "tarae-registry:/usr/local/cargo/registry".into());
    mount(&mut a, format!("tarae-rustup-{pin}:/usr/local/rustup"));
    for e in ["CARGO_TARGET_DIR=/target", "CARGO_TERM_COLOR=never"] {
        a.extend(["-e".to_string(), e.to_string()]);
    }
    a.extend([image.to_string(), "sh".into(), "-ec".into(), script(what)]);
    a
}

/// What runs in the container: stdout kept for the results' tar (fd 3), the clone, the tool built with
/// frame pointers like tarae, then the tool's tests or each scenario under the probe. A failed test or
/// scenario still sends what the others left; each step says how long it took.
fn script(what: &What) -> String {
    let mut s = String::from("exec 3>&1 1>&2\nfail=0\n");
    s.push_str(SYNC);
    s.push_str(
        "export RUSTFLAGS='-C force-frame-pointers=yes'\n\
         mkdir -p /out\n\
         took() { t=$(date +%s); r=0; \"$@\" || r=$?; echo \"took $(( $(date +%s) - t )) s: $*\"; return $r; }\n\
         cd trace\n\
         took cargo build --release --locked\n\
         cp Cargo.lock /out/trace-Cargo.lock\n",
    );
    match what {
        // (every test binary runs: one failing doesn't hide the others')
        What::Test => {
            s.push_str("took cargo test --release --locked --no-fail-fast -- --nocapture || fail=1\n")
        }
        What::Clippy => {
            s.push_str("took cargo clippy --release --locked --all-targets -- -D warnings || fail=1\n")
        }
        What::Probe(probe, scenarios, options) => {
            s.push_str("cd /work/tarae\ntook cargo test --locked --profile perf --test perf --no-run\n");
            let options: String = options.iter().map(|o| format!(" {o}")).collect();
            for sc in scenarios {
                s.push_str(&format!(
                    "took /target/release/tarae-trace {probe}{options} --comm tarae --name {sc} --out /out \
                     --for {FOR} -- cargo test --locked --profile perf --test perf -- --exact {sc} --nocapture \
                     || fail=1\n"
                ));
            }
        }
    }
    s.push_str("tar -C /out -cf - . >&3\nexit $fail\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No scenario named: the default ones; named ones must be perf tests' names (they become script words).
    #[test]
    fn scenarios_and_test() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let keys = s(&["perf_e2e_key_in_a_big_file", "perf_e2e_scrolling"]);
        assert_eq!(what(&[]).unwrap(), What::Probe("offcpu", keys.clone(), vec![]));
        assert_eq!(what(&s(&["offcpu"])).unwrap(), What::Probe("offcpu", keys.clone(), vec![]));
        assert_eq!(what(&s(&["syscalls"])).unwrap(), What::Probe("syscalls", keys.clone(), vec![]));
        assert_eq!(
            what(&s(&["pty"])).unwrap(),
            What::Probe("pty", s(&["perf_e2e_scrolling", "perf_e2e_scrolling_slow_terminal"]), vec![])
        );
        assert_eq!(
            what(&s(&["perf_e2e_idle_cpu"])).unwrap(),
            What::Probe("offcpu", s(&["perf_e2e_idle_cpu"]), vec![])
        );
        // The syscall probe's stacks: syscall names only (they become script words), its probe only
        assert_eq!(
            what(&s(&["syscalls", "--stacks", "mprotect,munmap", "perf_e2e_scrolling"])).unwrap(),
            What::Probe("syscalls", s(&["perf_e2e_scrolling"]), s(&["--stacks", "mprotect,munmap"]))
        );
        assert_eq!(
            what(&s(&["syscalls", "--stacks", "mprotect"])).unwrap(),
            What::Probe("syscalls", keys, s(&["--stacks", "mprotect"]))
        );
        assert!(what(&s(&["syscalls", "--stacks", "x;rm"])).is_err());
        assert!(what(&s(&["syscalls", "--stacks"])).is_err());
        assert!(what(&s(&["pty", "--stacks", "write"])).is_err(), "not the pty probe's");
        assert_eq!(what(&s(&["test"])).unwrap(), What::Test);
        assert_eq!(what(&s(&["clippy"])).unwrap(), What::Clippy);
        assert!(what(&s(&["clippy", "x"])).is_err());
        assert!(
            script(&What::Clippy).contains("cargo clippy --release --locked --all-targets -- -D warnings")
        );
        assert!(what(&s(&["perf_x; rm -rf /"])).is_err());
        assert!(what(&s(&["offcpu", "scrolling"])).is_err());
        assert!(what(&s(&["pty", "nope"])).is_err());
        assert!(what(&s(&["test", "x"])).is_err());
    }

    /// The capabilities BPF needs and no more; the tree read-only and no other host folder; a target volume
    /// of its own; stdout kept for the results.
    #[test]
    fn the_container_gets_bpf_caps_and_a_read_only_tree() {
        let w = What::Probe("offcpu", vec!["perf_e2e_scrolling".into()], vec![]);
        let a = docker_args(Path::new("/r/tarae"), Some(Path::new("/r/.git")), "1.99.0", "img", &w);
        let caps: Vec<&str> = a.windows(2).filter(|w| w[0] == "--cap-add").map(|w| w[1].as_str()).collect();
        assert_eq!(caps, ["BPF", "PERFMON", "SYSLOG"]);
        for no in ["--privileged", "--pid", "--pid=host", "SYS_ADMIN", "--security-opt"] {
            assert!(!a.iter().any(|v| v == no || v.contains(no)), "{no}: {a:?}");
        }
        let host: Vec<&String> = a.iter().filter(|v| v.starts_with('/') && v.contains(':')).collect();
        assert_eq!(host, ["/r/tarae:/src:ro", "/r/.git:/r/.git:ro"]);
        assert!(a.contains(&"tarae-trace-target-1.99.0:/target".to_string()), "{a:?}");
        let s = a.last().unwrap();
        assert!(s.starts_with("exec 3>&1 1>&2\n") && s.ends_with("tar -C /out -cf - . >&3\nexit $fail\n"));
        assert!(
            s.contains("--name perf_e2e_scrolling --out /out --for 5m -- cargo test --locked --profile perf"),
            "a bound on every scenario: {s}"
        );
        assert!(s.contains("force-frame-pointers=yes"));
        // (the checkout is kept in a volume and synced by content: unchanged files keep their mtimes)
        assert!(a.contains(&"tarae-trace-work-1.99.0:/work".to_string()), "{a:?}");
        assert!(s.contains("rsync -rlc --delete --exclude=/target /tmp/new/ /work/tarae/"), "{s}");
        // (a step that fails fails the run — `took` passes its status on)
        assert!(s.contains("\"$@\" || r=$?;") && s.contains("return $r; }"), "{s}");
        assert!(
            script(&What::Test).contains("cargo test --release") && !script(&What::Test).contains("perf")
        );
    }

    /// `trace/` stays out of tarae's workspace — `--workspace` (CI, preflight, `xtask linux`) never builds it
    /// or runs its build.rs, which needs clang and bpftool. This repository's manifests, then the ways in.
    #[test]
    fn trace_stays_out_of_the_workspace() {
        let read = |p: &str| std::fs::read_to_string(root().join(p)).unwrap();
        trace_outside_the_workspace(&read("Cargo.toml"), &read("trace/Cargo.toml")).unwrap();
        let own = "[package]\nname = \"tarae-trace\"\n\n[workspace]\n";
        let root = |ws: &str| format!("[package]\nname = \"tarae\"\n\n[workspace]\n{ws}\n");
        for (ws, why) in [
            ("members = [\"xtask\", \"trace\"]", "members has \"trace\""),
            ("members = [\"*\"]", "members has \"*\""),
            ("members = [\"xtask\", \"tr*\"]", "members has \"tr*\""),
            ("members = [\"xtask\"]\ndefault-members = [\"trace/\"]", "default-members has \"trace/\""),
        ] {
            let e = trace_outside_the_workspace(&root(ws), own).unwrap_err();
            assert!(e.contains(why), "{ws}: {e}");
        }
        for fine in ["members = [\"xtask\"]", "members = [\"xtask\", \"plugin-sdk/*\", \"tracer\"]", ""] {
            assert_eq!(trace_outside_the_workspace(&root(fine), own), Ok(()), "{fine}");
        }
        let e = trace_outside_the_workspace(&root("members = [\"xtask\"]"), "[package]\nname = \"t\"\n");
        assert!(e.unwrap_err().contains("no [workspace] of its own"));
    }

    /// Another package list is another image.
    #[test]
    fn the_image_follows_its_dockerfile() {
        assert!(
            dockerfile("1.99.0").starts_with("FROM rust:1.99\n") && dockerfile("1.99.0").contains("bpftool")
        );
        assert_ne!(fnv(&dockerfile("1.99.0")), fnv(&dockerfile("1.100.0")));
        assert_eq!(stamp().len(), "2026-10-05-143012".len());
    }
}
