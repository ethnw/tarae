//! `tarae plugin new|dev|list` — starting a plugin, building it while it's worked on, and seeing what's
//! installed.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, anyhow, bail};

use super::abi::{Plugin, WIT_BINDGEN};
use crate::wasm::Limits;

const WIT: &str = include_str!("../../plugin-sdk/wit/tarae.wit");
const CARGO_TOML: &str = include_str!("../../plugin-sdk/template/Cargo.toml");
const LIB_RS: &str = include_str!("../../plugin-sdk/template/src/lib.rs");
const TARGET: &str = "wasm32-unknown-unknown";
/// Rust's keywords, which cargo refuses as package names.
const KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let", "loop",
    "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return", "self", "static",
    "struct", "super", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use", "virtual",
    "where", "while", "yield",
];

fn paint(code: &str, s: &str) -> String {
    format!("\x1b[{code}m{s}\x1b[0m")
}

pub fn cli(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("new") => {
            let dir = args.get(1).ok_or_else(|| anyhow!("usage: tarae plugin new <name>"))?;
            new(Path::new(dir))?;
            println!("{} {}", paint("1;32", "✓"), paint("1", &format!("created {dir}")));
            println!("  {}  :words and :title — edit src/lib.rs", paint("2", "·"));
            println!("\n  cd {dir} && tarae plugin dev");
            println!("  {}", paint("2", "(builds on every save; your running tarae reloads it)"));
            if !target_installed() {
                println!("\n  {} rustup target add {TARGET}", paint("33", "first:"));
            }
            Ok(())
        }
        Some("dev") => dev(Path::new(args.get(1).map_or(".", String::as_str))),
        Some("list") | None => list(),
        Some(other) => bail!("unknown plugin command '{other}' — new <name> · dev [dir] · list"),
    }
}

/// A plugin crate from the template, in `dir` (its last component is the name).
fn new(dir: &Path) -> anyhow::Result<()> {
    let name =
        dir.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("no name in {}", dir.display()))?;
    // What cargo takes as a package name
    if !name.starts_with(|c: char| c.is_ascii_alphabetic())
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("a plugin's name starts with a letter, then letters, digits, - and _");
    }
    if KEYWORDS.contains(&name) {
        bail!("“{name}” is a Rust keyword — cargo won't take it as a name");
    }
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    for (path, text) in [
        ("Cargo.toml", CARGO_TOML.replacen("\"NAME\"", &format!("\"{name}\""), 1)),
        ("src/lib.rs", LIB_RS.to_string()),
        ("wit/tarae.wit", WIT.to_string()),
        (".cargo/config.toml", format!("[build]\ntarget = \"{TARGET}\"\n")),
        (".gitignore", "/target\n".to_string()),
    ] {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("in the plugin folder"))?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

fn target_installed() -> bool {
    std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l == TARGET))
}

fn plugins_dir() -> anyhow::Result<std::path::PathBuf> {
    super::dir().ok_or_else(|| anyhow!("no config folder (set HOME or XDG_CONFIG_HOME)"))
}

/// What's installed, and whether each loads.
fn list() -> anyhow::Result<()> {
    let dir = plugins_dir()?;
    let found = super::host::scan(&dir);
    if found.is_empty() {
        println!("no plugins in {} — tarae plugin new <name> starts one", dir.display());
    }
    for (name, (path, (_, len))) in found {
        println!(
            "{}  {}",
            paint("1", &name),
            paint("2", &format!("{} KB · {}", len.div_ceil(1024), path.display()))
        );
        let loaded = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| Plugin::new(&b, Limits::default(), 100_000_000))
            .and_then(|mut p| Ok((p.commands(100_000_000)?, p.subscriptions(100_000_000)?)));
        match loaded {
            Ok((commands, subscriptions)) => {
                for c in commands {
                    println!("  :{:<20} {}", super::shown(&c.name), paint("2", &super::shown(&c.doc)));
                }
                if !subscriptions.is_empty() {
                    let events: Vec<&str> = subscriptions.iter().map(|s| s.event.name()).collect();
                    println!("  {}", paint("2", &format!("on {}", events.join(", "))));
                }
            }
            Err(e) => println!("  {} {}", paint("31", "can't load:"), super::shown(&e)),
        }
    }
    Ok(())
}

/// The `.wasm` a `cargo build --message-format json…` built.
fn artifact(json: &str) -> Option<std::path::PathBuf> {
    json.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact")
        .flat_map(|m| m["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|f| f.as_str().map(std::path::PathBuf::from))
        .find(|f| f.extension().is_some_and(|e| e == "wasm"))
}

/// The compiler's messages in that output, as it rendered them.
fn diagnostics(json: &str) -> Vec<String> {
    json.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-message")
        .filter_map(|m| m["message"]["rendered"].as_str().map(str::to_string))
        .collect()
}

/// The package name in a plugin's Cargo.toml.
fn package_name(manifest: &str) -> Option<String> {
    let t: toml::Table = toml::from_str(manifest).ok()?;
    Some(t.get("package")?.get("name")?.as_str()?.to_string())
}

/// Builds the plugin in `dir` whenever its sources change, and puts it in the plugins folder — where a
/// running tarae notices and reloads it.
fn dev(dir: &Path) -> anyhow::Result<()> {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml"))
        .map_err(|_| anyhow!("no Cargo.toml in {} — tarae plugin new <name> starts one", dir.display()))?;
    let name = package_name(&manifest).ok_or_else(|| anyhow!("no [package] name in Cargo.toml"))?;
    if !manifest.contains(&format!("wit-bindgen = \"={WIT_BINDGEN}")) {
        println!("{} this tarae checks plugins built with wit-bindgen {WIT_BINDGEN}", paint("33", "note:"));
    }
    let out = plugins_dir()?;
    std::fs::create_dir_all(&out)?;
    let stamps = || -> Vec<SystemTime> {
        fn walk(p: &Path, out: &mut Vec<SystemTime>) {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    out.push(t);
                }
            }
        }
        let mut v = Vec::new();
        walk(&dir.join("src"), &mut v);
        walk(&dir.join("wit"), &mut v);
        v.extend(std::fs::metadata(dir.join("Cargo.toml")).and_then(|m| m.modified()));
        v
    };
    println!("{} {name} → {}", paint("1;36", "tarae plugin dev"), paint("2", &out.display().to_string()));
    let mut last = Vec::new();
    loop {
        let now = stamps();
        if now != last {
            last = now;
            let started = std::time::Instant::now();
            let build = std::process::Command::new("cargo")
                .args(["build", "--release", "--target", TARGET, "--color", "always"])
                .args(["--message-format", "json-diagnostic-rendered-ansi"])
                .current_dir(dir)
                .output()
                .context("running cargo")?;
            // Where cargo put it (a target dir or lib name of the plugin's own included)
            let wasm = artifact(&String::from_utf8_lossy(&build.stdout));
            if build.status.success()
                && let Some(wasm) = wasm
            {
                // Through a temporary name, so tarae never reads a half-written file
                let dest = out.join(format!("{name}.wasm"));
                let tmp = out.join(format!(".{name}.wasm.tmp"));
                std::fs::copy(&wasm, &tmp)?;
                std::fs::rename(&tmp, &dest)?;
                let kb = std::fs::metadata(&dest).map_or(0, |m| m.len().div_ceil(1024));
                let secs = started.elapsed().as_secs_f32();
                println!(
                    "{} built in {secs:.1}s · {kb} KB  {}",
                    paint("1;32", "✓"),
                    paint("2", "— reloading in tarae")
                );
            } else {
                println!("{} build failed", paint("1;31", "✗"));
                for d in diagnostics(&String::from_utf8_lossy(&build.stdout)) {
                    eprint!("{d}");
                }
                std::io::Write::write_all(&mut std::io::stderr(), &build.stderr)?;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_writes_a_plugin_crate() {
        let scratch = crate::scratch::Scratch::new("plugin-new");
        let root = scratch.to_path_buf();
        let dir = root.join("my-plugin");
        new(&dir).unwrap();
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert_eq!(package_name(&manifest).as_deref(), Some("my-plugin"));
        assert!(
            std::fs::read_to_string(dir.join("wit/tarae.wit")).unwrap().contains("package tarae:plugin@0.2.")
        );
        assert!(std::fs::read_to_string(dir.join(".cargo/config.toml")).unwrap().contains(TARGET));
        assert!(new(&dir).is_err(), "not over an existing folder");
        assert!(new(&root.join("bad name")).is_err());
        assert!(new(&root.join("2fa")).is_err(), "cargo refuses a leading digit");
        assert!(new(&root.join("fn")).is_err(), "and keywords");
        std::fs::remove_dir_all(root).ok();
    }

    /// A plugin `tarae plugin new` starts builds as it is — the template keeps up with the WIT. (Ignored: it
    /// fetches the SDK's crates; CI's network job runs it.)
    #[test]
    #[ignore]
    fn a_new_plugin_builds() {
        let scratch = crate::scratch::Scratch::new("plugin-builds");
        let root = scratch.to_path_buf();
        let dir = root.join("hello");
        new(&dir).unwrap();
        // (for this machine: the wasm target may not be installed — the bindings and the code are the same)
        let host = String::from_utf8(std::process::Command::new("rustc").arg("-vV").output().unwrap().stdout)
            .unwrap();
        let host = host.lines().find_map(|l| l.strip_prefix("host: ")).unwrap().to_string();
        let out = std::process::Command::new("cargo")
            .args(["check", "--quiet", "--target", &host])
            .current_dir(&dir)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        std::fs::remove_dir_all(root).ok();
    }

    /// The template, the demo and the version tarae names in its messages agree, and match the WIT's major.
    #[test]
    fn the_sdk_pins_one_wit_bindgen() {
        let pin = format!("wit-bindgen = \"={WIT_BINDGEN}.");
        assert!(CARGO_TOML.contains(&pin), "template: {pin}");
        assert!(include_str!("../../plugin-sdk/demo/Cargo.toml").contains(&pin), "demo: {pin}");
        assert!(WIT.contains(&format!("package tarae:plugin@{}.", super::super::abi::API)));
    }
}
