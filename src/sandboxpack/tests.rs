use std::path::{Path, PathBuf};

use super::*;

/// The table from these packs' texts as built-in ones, and these as a runtime folder's.
fn table(builtin: &[(&str, &str)], yours: &[(&str, &str)]) -> Packs {
    let builtin: Vec<(String, Result<Table, String>)> = builtin
        .iter()
        .map(|(p, text)| (p.to_string(), crate::packs::read_pack(p, text, "built in")))
        .collect();
    let yours: Vec<(String, String)> = yours.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect();
    build(&builtin, &yours)
}

/// tarae's own table: what its packs say.
fn ours() -> Packs {
    build(crate::packs::builtin_manifests(), &[])
}

/// Every built-in pack's `[sandbox]` and `[claude]` read whole: nothing left out (a mistake in tarae's pack).
#[test]
fn every_built_in_table_reads_clean() {
    let t = ours();
    assert_eq!(t.problems, Vec::<String>::new());
    assert!(!t.reads.is_empty() && !t.run_allow.is_empty() && !t.settings.is_empty() && !t.expand.is_empty());
    // (each home named once, and each place's variable is a home or the core's)
    let names: Vec<&str> = t.homes.iter().map(|h| h.name.as_str()).collect();
    let vars = Vars::new(&t, Path::new("/h"), &|_| None, Os::Mac);
    let mut all: Vec<&String> =
        t.reads.iter().chain(&t.kept).chain(&t.write_files).chain(&t.code_trees).chain(&t.bins).collect();
    all.extend(t.quiet_writes.iter().chain(t.expand.iter().map(|e| &e.first_word_in)));
    for p in all {
        assert!(vars.place(p).is_some(), "{p} (homes: {names:?})");
    }
}

/// What a pack's `pass` may hold: names and families of the toolchain's own — never a bare `*` or a family
/// too short to mean one tool, the loader's (`LD_*`, `DYLD_*`, a family reaching them), nor a name only a
/// secret has; each wrong one left out and said, the rest kept. And the built-in packs pass what was measured:
/// cargo's, Go's by name (not `GO*`: `GOOGLE_*` is no tool's), the C toolchain's — and the managers' in the
/// grammar-less `tools` pack.
#[test]
fn what_a_pass_may_hold() {
    let t = table(
        &[(
            "x",
            r#"[sandbox]
pass = ["X_*", "XFLAGS", "*", "X*", "LD_LIBRARY_PATH", "DYLD_*", "LD*", "NPM_TOKEN", "MY_API_KEY", "A-B", "LDFLAGS"]"#,
        )],
        &[],
    );
    assert_eq!(t.pass, ["X_*", "XFLAGS", "LDFLAGS"]);
    let said = t.problems.join("\n");
    for (e, why) in [
        ("*", "isn't a variable's name"),
        ("X*", "isn't a variable's name"),
        ("LD_LIBRARY_PATH", "is the loader's"),
        ("DYLD_*", "is the loader's"),
        ("LD*", "is the loader's"),
        ("NPM_TOKEN", "names a secret"),
        ("MY_API_KEY", "names a secret"),
        ("A-B", "isn't a variable's name"),
    ] {
        assert!(said.contains(&format!("`{e}` {why}")), "{e}: {said}");
    }
    let ours = ours();
    for e in
        ["CARGO_*", "RUST*", "GOFLAGS", "CGO_*", "CC", "PKG_CONFIG_PATH", "MISE_*", "npm_config_*", "PYTHON*"]
    {
        assert!(ours.pass.contains(&e.to_string()), "{e}: {:?}", ours.pass);
    }
    assert!(!ours.pass.iter().any(|e| e.starts_with("GO") && e.ends_with('*')), "{:?}", ours.pass);
    assert!(!ours.pass.iter().any(|e| e.starts_with("DIRENV")), "direnv's diff holds the values themselves");
}

/// A runtime folder's pack widens nothing: every key that's the built-in packs' only, set to what would open
/// most, is left out — the table is tarae's own — and each is said.
#[test]
fn a_runtime_pack_widens_nothing() {
    let hostile = r#"
[sandbox]
reads = ["~/.ssh", "~/Documents"]
reads-above = [".ssh/id_rsa"]
write-files = ["~/.zshrc"]
ask = [{ when = ["Cargo.toml"], run = ["sh", "-c", "touch marker"], json = ["x"] }]
builds-said = { who = "x", target = "x", build = "x" }
env = { RUSTC_WRAPPER = "/tmp/w", MINE = "{run}" }
pass = ["AWS_*", "GITHUB_TOKEN"]
quiet-writes = ["~/"]
refused-port = { process = "x", said = "run sh outside" }
kept = ["~/mine"]
bins = ["~/mine-bin"]
[sandbox.homes]
cargo = { env = "MY_HOME", else = "~/Documents" }
[sandbox.caches]
home = "cargo"
dirs = ["."]
[sandbox.run-home.x]
env = "X_HOME"
[[sandbox.settings]]
tool = "ssh"
var = "SSH_X"
files = ["~/.ssh/id_rsa"]
copy = "key"
[claude]
run-allow = ["sh -c", "rm -rf"]
code-trees = ["~/*"]
builders = ["mybuild"]
said = { rank = 0, run = "anything at all" }
[claude.expand."$(id)"]
run = ["id"]
first-word-in = "~/"
"#;
    let mine = table(&[], &[("rust", hostile)]);
    let base = ours();
    let with = build(crate::packs::builtin_manifests(), &[("rust".into(), hostile.into())]);
    assert_eq!(mine.reads, Vec::<String>::new(), "a runtime pack alone sets no read");
    assert_eq!((&with.homes, &with.reads, &with.reads_above), (&base.homes, &base.reads, &base.reads_above));
    assert_eq!((&with.caches, &with.write_files, &with.ask), (&base.caches, &base.write_files, &base.ask));
    assert_eq!(
        (&with.run_homes, &with.env, &with.quiet_writes),
        (&base.run_homes, &base.env, &base.quiet_writes)
    );
    assert_eq!((&with.settings, &with.refused_port), (&base.settings, &base.refused_port));
    assert_eq!(
        (&with.builds_said, &with.run_allow, &with.code_trees),
        (&base.builds_said, &base.run_allow, &base.code_trees)
    );
    assert_eq!((&with.expand, &with.said, &with.pass), (&base.expand, &base.said, &base.pass));
    for key in [
        "sandbox.pass",
        "sandbox.homes",
        "sandbox.reads",
        "sandbox.reads-above",
        "sandbox.caches",
        "sandbox.write-files",
        "sandbox.ask",
        "sandbox.builds-said",
        "sandbox.run-home",
        "sandbox.env",
        "sandbox.quiet-writes",
        "sandbox.settings",
        "sandbox.refused-port",
        "claude.run-allow",
        "claude.code-trees",
        "claude.expand",
        "claude.said",
    ] {
        let said = format!("your rust pack: `{key}` is read from tarae's own packs only");
        assert!(with.problems.contains(&said), "{key}: {:?}", with.problems);
    }
    // …what narrows it adds to
    assert!(with.kept.contains(&"~/mine".to_string()) && with.kept.len() == base.kept.len() + 1);
    assert!(with.bins.contains(&"~/mine-bin".to_string()) && with.bins.len() == base.bins.len() + 1);
    assert!(with.builders.contains(&"mybuild".to_string()) && with.builders.len() == base.builders.len() + 1);
    // …and the language table says the same at the start
    let langs =
        crate::languages::build(crate::packs::builtin_manifests(), &[("rust".into(), hostile.into())]);
    assert!(langs.said.iter().any(|s| s.contains("`sandbox.reads` is read from tarae's own packs only")));
    assert!(!langs.said.iter().any(|s| s.contains("`sandbox.kept`")), "{:?}", langs.said);
}

/// A runtime folder's pack over a built-in one through `packs::over`: its own keys by their reach.
#[test]
fn a_pack_of_ones_own_counts_key_by_key() {
    let mut b: Table =
        "[sandbox]\nkept = [\"~/a\"]\nreads = [\"~/r\"]\n[claude]\nbuilders = [\"b\"]\n".parse().unwrap();
    let u = "[sandbox]\nkept = [\"~/k\"]\nreads = [\"~/.ssh\"]\n[claude]\nbuilders = [\"c\"]\nrun-allow = [\"rm\"]\n";
    crate::packs::over(&mut b, crate::packs::read_pack("x", u, "yours").unwrap());
    assert_eq!(b["sandbox"]["kept"].as_array().unwrap().len(), 2, "added to: {b:?}");
    assert_eq!(b["sandbox"]["reads"].as_array().unwrap().len(), 1, "built-in only");
    assert_eq!(b["claude"]["builders"].as_array().unwrap().len(), 2);
    assert!(b["claude"].get("run-allow").is_none());
    let m = crate::packs::merged("new", None, Some(u.into())).unwrap().unwrap();
    assert!(m["sandbox"].get("reads").is_none() && m["sandbox"].get("kept").is_some(), "{m:?}");
    let e = crate::packs::read_pack("x", "[sandbox]\nwhatever = 1\n", "yours").unwrap_err();
    assert!(e.contains("has no `sandbox.whatever`"), "{e}");
}

/// A built-in pack's entry that would widen past what the core allows is left out, and said.
#[test]
fn a_built_in_entry_past_the_core_is_left_out() {
    let cases: [(&str, &str); 16] = [
        ("[sandbox]\nreads = [\"{nope}/x\"]\n", "no home"),
        ("[sandbox.homes]\nh = { env = \"H\", else = \"{user-config}/h\" }\n", "a place from home"),
        ("[sandbox]\nreads = [\"~/\"]\n", "home itself"),
        ("[sandbox]\nreads = [\"/etc\"]\n", "isn't from home"),
        ("[sandbox]\nreads = [\"~/a/../../x\"]\n", "`..`"),
        ("[sandbox]\nreads = [\"~/.ssh\"]\n", "keys"),
        ("[sandbox]\nreads = [\"~/.config\"]\n", "keys"),
        ("[sandbox]\nreads = [\"~/.local/state/tarae/x\"]\n", "keys"),
        ("[sandbox]\nreads = [\"~/a/*\"]\n", "`*`"),
        ("[sandbox]\nwrite-files = [\"~/.cargo/registry/x\"]\n", "nothing is written"),
        ("[sandbox]\nenv = { RUSTC_WRAPPER = \"/x\" }\n", "may not set"),
        ("[sandbox.homes]\nh = { env = \"HOME\", else = \"~/.h\" }\n", "isn't a variable a home"),
        ("[claude]\nrun-allow = [\"sh -c\"]\n", "runs whatever follows"),
        ("[claude]\nrun-allow = [\"cargo $(x)\"]\n", "plain words"),
        ("[claude.expand.\"$(id; rm x)\"]\nrun = [\"id\"]\nfirst-word-in = \"~/x\"\n", "plain words in a"),
        (
            "[[sandbox.settings]]\ntool = \"s\"\nvar = \"S\"\nfiles = [\"~/.ssh/config\"]\ncopy = \"c\"\n",
            "keys",
        ),
    ];
    for (text, why) in cases {
        let t = table(&[("x", text)], &[]);
        assert!(
            t.problems.iter().any(|p| p.starts_with("the x pack") && p.contains(why)),
            "{text}: {:?}",
            t.problems
        );
        assert!(t.reads.is_empty() && t.write_files.is_empty() && t.env.is_empty() && t.run_allow.is_empty());
        assert!(t.expand.is_empty() && t.settings.is_empty() && t.homes.is_empty(), "{text}: {t:?}");
    }
    // (a tool's own settings file kept for its tokens is one — its copy stands in)
    let npm = "[[sandbox.settings]]\ntool = \"n\"\nvar = \"N\"\nfiles = [\"~/.npmrc\"]\ncopy = \"c\"\n";
    assert_eq!(table(&[("x", npm)], &[]).problems, Vec::<String>::new());
    // (a home's place is judged where its `else` is, not as `~/.<name>` — named in the pack after its use)
    let moved = "[sandbox]\nwrite-files = [\"{h}/registry/x\", \"{h}/bin-cache/x\"]\n\
                 [sandbox.homes]\nh = { env = \"H\", else = \"~/.cargo\" }\n";
    let t = table(&[("x", moved)], &[]);
    assert_eq!(t.write_files, ["{h}/bin-cache/x"]);
    assert!(t.problems.iter().any(|p| p.contains("`{h}/registry/x` is where nothing is written")), "{t:?}");
    // (one wrong place of a list goes alone)
    let t = table(&[("x", "[sandbox]\nreads = [\"~/.npm\", \"~/.ssh\"]\n")], &[]);
    assert_eq!(t.reads, ["~/.npm"]);
}

/// An `ask`'s keys by what they're for — folders written, shared caches (each said by a `[sandbox.caches]
/// asked`), what no run reads — a run home kept across a project's runs, and where programs land (`bins`); a
/// cache said on one side only is left out, and said.
#[test]
fn asked_caches_kept_homes_and_bins_read() {
    let good = r#"
[sandbox]
ask = [{ when = ["t.mod"], run = ["t", "env"], caches = ["MODS"], kept = ["BUILD"] }]
bins = ["{t}/bin"]
[sandbox.homes]
t = { env = "T_HOME", else = "~/t" }
[sandbox.caches]
asked = "MODS"
rule = "t's modules"
[sandbox.run-home.t-build]
env = "T_BUILD"
keep = true
when = ["t.mod"]
"#;
    let t = table(&[("t", good)], &[]);
    assert_eq!(t.problems, Vec::<String>::new());
    let [a] = t.ask.as_slice() else { panic!("{:?}", t.ask) };
    assert_eq!(
        (&a.json[..], &a.caches[..], &a.kept[..]),
        (&[][..], &["MODS".to_string()][..], &["BUILD".to_string()][..])
    );
    assert_eq!((t.caches[0].asked.as_deref(), t.caches[0].home.as_str()), (Some("MODS"), ""));
    assert_eq!((t.run_homes[0].keep, &t.run_homes[0].when[..]), (true, &["t.mod".to_string()][..]));
    assert_eq!(t.bins, ["{t}/bin"]);
    // (a cache with no words, words with no cache, both a home and an asked one, a bin that's home)
    let cases = [
        (
            "[sandbox]\nask = [{ when = [\"m\"], run = [\"t\"], caches = [\"X\"] }]\n",
            "has no `[sandbox.caches] asked`",
        ),
        ("[sandbox.caches]\nasked = \"X\"\n", "names no `ask`'s cache"),
        ("[sandbox.caches]\nasked = \"X\"\nhome = \"t\"\n", "or the folder an `ask` named"),
        ("[sandbox]\nbins = [\"~/\"]\n", "home itself"),
        (
            "[sandbox.caches]\nhome = \"t\"\ndirs = [\"r\"]\nplaces = [\"~/r\"]\n",
            "say where an `asked` cache is",
        ),
        ("[sandbox.caches]\nasked = \"X\"\nvar = \"HOME\"\n", "isn't a variable a cache's place"),
        ("[sandbox.caches]\nasked = \"X\"\nplaces = [\"~/\"]\n", "home itself"),
        ("[sandbox.run-home.x]\nenv = \"X\"\nwhen = [\"../m\"]\n", "names files in the project"),
    ];
    for (text, why) in cases {
        let t = table(&[("x", text)], &[]);
        assert!(t.problems.iter().any(|p| p.contains(why)), "{text}: {:?}", t.problems);
        assert!(t.caches.is_empty() && t.bins.is_empty() && t.run_homes.is_empty(), "{text}: {t:?}");
        assert!(t.ask.iter().all(|a| a.caches.is_empty()), "{text}: {:?}", t.ask);
    }
}

/// Places for a home: a home's variable when it's an absolute path (a list home's first entry), else its
/// default; each system's own
/// folders; a kept place at both a home's places; a quiet write from home; `{a}` filled with what's there.
#[test]
fn places_for_a_home() {
    let t = ours();
    let home = Path::new("/u");
    let set = |name: &str| match name {
        "CARGO_HOME" => Some(PathBuf::from("/opt/cargo")),
        "GRADLE_USER_HOME" => Some(PathBuf::from("relative/gradle")),
        "XDG_CONFIG_HOME" => Some(PathBuf::from("/x/config")),
        _ => None,
    };
    let mac = Vars::new(&t, home, &set, Os::Mac);
    let linux = Vars::new(&t, home, &set, Os::Linux);
    assert_eq!(mac.place("{cargo}/registry/src"), Some(PathBuf::from("/opt/cargo/registry/src")));
    assert_eq!(
        mac.place("{gradle}/daemon"),
        Some(PathBuf::from("/u/.gradle/daemon")),
        "not absolute: the default"
    );
    assert_eq!(mac.place("{user-config}/go"), Some(PathBuf::from("/u/Library/Application Support/go")));
    assert_eq!(linux.place("{user-config}/go"), Some(PathBuf::from("/x/config/go")));
    assert_eq!(linux.place("{user-cache}/go-build"), Some(PathBuf::from("/u/.cache/go-build")));
    assert_eq!(mac.place("{run}/x"), None, "no run");
    assert_eq!(mac.place("{nope}/x"), None);
    assert_eq!(
        mac.places_both("{cargo}/credentials"),
        [PathBuf::from("/u/.cargo/credentials"), PathBuf::from("/opt/cargo/credentials")]
    );
    assert_eq!(mac.places_both("{gradle}/daemon"), [PathBuf::from("/u/.gradle/daemon")]);
    assert!(mac.is_home("{cargo}") && !mac.is_home("{cargo}/x") && !mac.is_home("{user-config}"));
    // A list home (`GOPATH=/a:/b`): its places in the first entry, as Go puts them, the others homes alone; a
    // home not said to be a list is taken whole (`CARGO_HOME=/c:/d`: no absolute folder of that name, none split)
    let listed = |name: &str| match name {
        "GOPATH" => Some(PathBuf::from("/a:/b:relative:/c")),
        "CARGO_HOME" => Some(PathBuf::from("/c:/d")),
        _ => None,
    };
    let v = Vars::new(&t, home, &listed, Os::Mac);
    assert_eq!(v.place("{go}/pkg/mod"), Some(PathBuf::from("/a/pkg/mod")));
    assert_eq!(v.places_both("{go}/bin"), [PathBuf::from("/u/go/bin"), PathBuf::from("/a/bin")]);
    assert_eq!(v.listed, [PathBuf::from("/b"), PathBuf::from("/c")]);
    assert_eq!(v.place("{cargo}/registry"), Some(PathBuf::from("/c:/d/registry")));
    let relative_first = |name: &str| (name == "GOPATH").then(|| PathBuf::from("go:/b"));
    let v = Vars::new(&t, home, &relative_first, Os::Mac);
    assert_eq!((v.place("{go}"), &v.listed[..]), (Some(PathBuf::from("/u/go")), &[PathBuf::from("/b")][..]));
    // `{a}`, `{b}`: each a folder that's there, the same name for the same letter
    let dir = std::env::temp_dir().join(format!("tarae-filled-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    for d in ["dists/g-8/h1", "dists/g-8/h2", "dists/g-9/h3"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("dists/not-a-folder"), "").unwrap();
    // (never through a link: a folder linked in, a lock file that's a link — a rule would land where they point)
    std::fs::create_dir_all(dir.join("elsewhere/h4")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("dists/g-link")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere/h4"), dir.join("dists/g-9/h-link")).unwrap();
    std::fs::create_dir_all(dir.join("dists/g-7/h5")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere/x"), dir.join("dists/g-7/h5/g-7.zip.lck")).unwrap();
    let got = filled(&dir.join("dists/{a}/{b}/{a}.zip.lck"));
    let want: Vec<PathBuf> = ["g-8/h1/g-8.zip.lck", "g-8/h2/g-8.zip.lck", "g-9/h3/g-9.zip.lck"]
        .iter()
        .map(|p| dir.join("dists").join(p))
        .collect();
    assert_eq!(got, want);
    std::fs::remove_dir_all(&dir).ok();
}

/// The words a pack says in Claude's prompt come in `rank` order, whatever the packs' names.
#[test]
fn the_packs_words_come_by_rank() {
    let t = ours();
    let tests: Vec<&str> = t.said.iter().filter_map(|s| s.tests.as_deref()).collect();
    assert_eq!(tests, ["Rust (cargo test)", "Go", "Python (pytest)", "Java (Gradle/Maven)"]);
    let protect: Vec<&str> = t.said.iter().filter_map(|s| s.protect.as_deref()).collect();
    assert_eq!(protect, ["build.rs, Cargo.toml, .cargo", "package.json", "Makefile"]);
}
