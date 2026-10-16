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
entry = [{ run = "mybuild go", files = ["tools/"] }]
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
    let mine = Entry { run: vec!["mybuild".into(), "go".into()], files: vec!["tools/".into()] };
    assert!(with.entry.contains(&mine) && with.entry.len() == base.entry.len() + 1, "{:?}", with.entry);
    // …and the language table says the same at the start
    let langs =
        crate::languages::build(crate::packs::builtin_manifests(), &[("rust".into(), hostile.into())]);
    assert!(langs.said.iter().any(|s| s.contains("`sandbox.reads` is read from tarae's own packs only")));
    assert!(!langs.said.iter().any(|s| s.contains("`sandbox.kept`")), "{:?}", langs.said);
}

/// An `entry` names a command by its words and files in the project by plain paths — one that names a file
/// outside it (`..`, a root), a pattern, or no file, or isn't a command's words, is left out and said; the
/// rest of the list is kept.
#[test]
fn an_entry_names_files_in_the_project() {
    let pack = r#"
[claude]
entry = [
  { run = "cargo xtask", files = ["xtask/"] },
  { run = "make", files = ["../up"] },
  { run = "make", files = ["/etc/x"] },
  { run = "make", files = ["src/*"] },
  { run = "make", files = [] },
  { run = "$(x)", files = ["a"] },
]
"#;
    let t = table(&[("p", pack)], &[]);
    let ok = Entry { run: vec!["cargo".into(), "xtask".into()], files: vec!["xtask/".into()] };
    assert_eq!(t.entry, [ok]);
    let said = t.problems.join("\n");
    for bad in
        ["`../up` isn't a path in the project", "`/etc/x`", "`src/*`", "`make` names no files", "`$(x)`"]
    {
        assert!(said.contains(bad), "{bad}: {said}");
    }
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

/// A run home a daemon lives from (`daemon = { make, append }` — a kept one) and what only a home of the run's
/// own gets (`fresh`): read as written, their variables among the packs' (no command of Claude's sets them);
/// a daemon on a home that isn't kept, a key they don't take, a variable no pack may set, a folder that's no
/// plain path — each left out, and said. No home may be called `home` (a run home's `{home}`).
#[test]
fn a_run_home_a_daemon_lives_from_reads() {
    let good = r#"
[sandbox.run-home.d]
env = "D_HOME"
keep = true
daemon = { make = ["reg"], append = { D_OPTS = "-Dreg={home}/reg" } }
fresh = { append = { D_OPTS = "-Ddaemon=off" } }
"#;
    let t = table(&[("d", good)], &[]);
    assert_eq!(t.problems, Vec::<String>::new());
    let [h] = t.run_homes.as_slice() else { panic!("{:?}", t.run_homes) };
    let d = h.daemon.as_ref().expect("its daemon");
    assert_eq!(
        (&d.make[..], &d.append[..]),
        (&["reg".to_string()][..], &[pair("D_OPTS", "-Dreg={home}/reg")][..])
    );
    assert_eq!((&h.fresh[..], &h.append[..]), (&[pair("D_OPTS", "-Ddaemon=off")][..], &[][..]));
    assert!(t.env_names().any(|n| n == "D_OPTS"), "{:?}", t.env_names().collect::<Vec<_>>());
    let cases = [
        ("[sandbox.run-home.x]\nenv = \"X\"\ndaemon = {}\n", "is a kept one"),
        ("[sandbox.run-home.x]\nenv = \"X\"\nkeep = true\ndaemon = { set = {} }\n", "daemon"),
        ("[sandbox.run-home.x]\nenv = \"X\"\nfresh = { make = [\"a\"] }\n", "fresh"),
        ("[sandbox.run-home.x]\nenv = \"X\"\nfresh = { append = { PATH = \"/x\" } }\n", "may not set"),
        ("[sandbox.run-home.x]\nenv = \"X\"\nkeep = true\ndaemon = { make = [\"../a\"] }\n", "plain names"),
        ("[sandbox.homes]\nhome = { env = \"X_HOME\", else = \"~/x\" }\n", "named twice"),
    ];
    for (text, why) in cases {
        let t = table(&[("x", text)], &[]);
        assert!(t.problems.iter().any(|p| p.contains(why)), "{text}: {:?}", t.problems);
        assert!(t.run_homes.is_empty() && t.homes.iter().all(|h| h.name != "home"), "{text}: {t:?}");
    }
}

/// What a run with a proxy URL gets from a pack (step 5f) reads as written — a run home's file, variables, a file
/// in the run's folder and what's appended naming it, the registries' hosts — and each wrong entry is left out
/// in the parser's own words, the rest of its key kept: a value of the proxy's only in a file's lines (a variable
/// can end up on a command line), only the proxy's values there, no newline in a line, a file's name and no
/// path, none of the core's proxy variables, `{file}` only in a file's `append`, a registry a host's name.
#[test]
fn an_env_entry_may_wait_for_a_fact_the_core_knows() {
    let t = table(
        &[(
            "t",
            "[sandbox]\nenv = { T_PLAIN = \"1\", T_OPTS = { value = \"-Dv4\", on = \"v4-mapped-refused\" } }\n",
        )],
        &[],
    );
    assert_eq!(t.problems, Vec::<String>::new());
    assert_eq!(t.env, [("T_PLAIN".to_string(), "1".to_string())]);
    assert_eq!(t.env_on, [EnvOn { name: "T_OPTS".into(), value: "-Dv4".into(), on: On::V4MappedRefused }]);
    // (not a name a run gets from the user's environment: where the fact doesn't hold, theirs stays out)
    assert!(!t.env_names().any(|n| n == "T_OPTS"));
    for (text, said) in [
        ("env = { T = { value = \"x\", on = \"macos\" } }", "`on` is no fact tarae knows (`macos`)"),
        ("env = { T = { value = \"x\" } }", "`env.T`"),
        ("env = { T = { value = \"x\", on = \"v4-mapped-refused\", os = \"x\" } }", "has no `os`"),
        ("env = { T = 1 }", "isn't a string, nor `{ value, on }`"),
        (
            "env = { LD_PRELOAD = { value = \"x\", on = \"v4-mapped-refused\" } }",
            "a variable a pack may not set",
        ),
    ] {
        let text = format!("[sandbox]\n{text}\n");
        let t = table(&[("x", text.as_str())], &[]);
        assert!(t.problems.iter().any(|p| p.contains(said)), "{text}: {:?}", t.problems);
        assert!(t.env_on.is_empty(), "{text}");
    }
}

#[test]
fn the_proxy_keys_are_said_in_the_parsers_words() {
    let good = r#"
[sandbox]
registries = ["registry.example.invalid", "*.example.invalid"]
[sandbox.proxy]
env = { T_USE_PROXY = "1" }
append = { T_OPTS = "-Dproxied" }
[sandbox.proxy.files."t-settings.xml"]
lines = ["<host>{proxy.host}</host>", "<password>{proxy.password}</password>"]
append = { T_ARGS = "-s {file}" }
[sandbox.run-home.t]
env = "T_HOME"
proxy = { file = "t.properties", lines = ["port={proxy.port}", "url={proxy.url}"] }
"#;
    let t = table(&[("t", good)], &[]);
    assert_eq!(t.problems, Vec::<String>::new());
    assert_eq!(
        (&t.proxy.env[..], &t.proxy.append[..]),
        (&[pair("T_USE_PROXY", "1")][..], &[pair("T_OPTS", "-Dproxied")][..])
    );
    let [f] = t.proxy.files.as_slice() else { panic!("{:?}", t.proxy) };
    assert_eq!(
        (f.file.as_str(), f.lines.len(), &f.append[..]),
        ("t-settings.xml", 2, &[pair("T_ARGS", "-s {file}")][..])
    );
    let p = t.run_homes[0].proxy.as_ref().expect("the run home's file");
    assert_eq!(
        (p.file.as_str(), &p.lines[..]),
        ("t.properties", &["port={proxy.port}".to_string(), "url={proxy.url}".to_string()][..])
    );
    let hosts: Vec<String> = t.registries.iter().map(|(pack, h)| format!("{pack}: {h}")).collect();
    assert_eq!(hosts, ["t: registry.example.invalid", "t: *.example.invalid"]);
    assert!(t.env_names().any(|n| n == "T_ARGS") && t.env_names().any(|n| n == "T_USE_PROXY"));
    let home = |proxy: &str| format!("[sandbox.run-home.x]\nenv = \"X_HOME\"\nproxy = {proxy}\n");
    let cases = [
        (
            home(r#"{ file = "x", lines = ["s={proxy.secret}"] }"#),
            "`run-home.x.proxy`: `{proxy.secret}` isn't a value tarae fills in (`{proxy.host}`, `{proxy.port}`, \
             `{proxy.user}`, `{proxy.password}`, `{proxy.url}`)",
        ),
        (home(r#"{ file = "../x", lines = [] }"#), "`run-home.x.proxy`: `file` is a file's name in the home"),
        (home(r#"{ file = "a/b", lines = [] }"#), "`run-home.x.proxy`: `file` is a file's name in the home"),
        (home(r#"{ file = "x", lines = ["a=1\nb=2"] }"#), "`run-home.x.proxy`: a line holds no newline"),
        (
            "[sandbox.proxy]\nenv = { X = \"{proxy.password}\" }\n".into(),
            "`env.X` holds `{proxy.password}` — the proxy's values go only in a file's lines",
        ),
        (
            "[sandbox.proxy]\nappend = { X = \"-Dp={proxy.port}\" }\n".into(),
            "`append.X` holds `{proxy.port}` — the proxy's values go only in a file's lines",
        ),
        ("[sandbox.proxy]\nenv = { HTTPS_PROXY = \"x\" }\n".into(), "`HTTPS_PROXY` is the core's own"),
        ("[sandbox.proxy]\nenv = { no_proxy = \"x\" }\n".into(), "`no_proxy` is the core's own"),
        (
            "[sandbox.proxy]\nenv = { RUSTC_WRAPPER = \"x\" }\n".into(),
            "`RUSTC_WRAPPER` is a variable a pack may not set",
        ),
        (
            "[sandbox.proxy]\nenv = { X = \"{file}\" }\n".into(),
            "`{file}` names a `proxy.files` entry's path — only in its `append`",
        ),
        (
            "[sandbox.proxy.files.\"a/b\"]\nlines = []\n".into(),
            "`files.\"a/b\"`: a file's name in the run's folder",
        ),
        ("[sandbox]\nregistries = [\"1.2.3.4\"]\n".into(), "`1.2.3.4` an address — a rule names hosts"),
        ("[sandbox]\nregistries = [\"*\"]\n".into(), "`*` "),
        ("[sandbox.proxy]\nlines = []\n".into(), "`proxy`"),
    ];
    for (text, said) in &cases {
        let t = table(&[("x", text)], &[]);
        assert!(
            t.problems.iter().any(|p| p.starts_with("the x pack") && p.contains(said)),
            "{text}: {:?}",
            t.problems
        );
        assert!(t.proxy == Proxy::default() && t.registries.is_empty(), "{text}: {:?}", t.proxy);
        assert!(t.run_homes.iter().all(|h| h.proxy.is_none()), "{text}: {:?}", t.run_homes);
    }
    // (one wrong entry goes alone: the rest of its key stays)
    let mixed = "[sandbox]\nregistries = [\"a.example.invalid\", \"1.2.3.4\"]\n\
                 [sandbox.proxy]\nenv = { OK_ONE = \"1\", HTTP_PROXY = \"x\" }\n";
    let t = table(&[("x", mixed)], &[]);
    assert_eq!((t.proxy.env.len(), t.registries.len()), (1, 1), "{:?}", t.problems);
}

/// Only tarae's own packs tell a run with a proxy URL anything (step 5f: built-in packs only may widen runs —
/// these carry the run's password into files and set variables its tools obey): a runtime folder's
/// `[sandbox.proxy]`, a run home's `proxy` and `registries` (the card's words) are left out, and said.
#[test]
fn only_tarae_s_packs_set_a_proxy() {
    let mine = r#"
[sandbox]
registries = ["evil.example.invalid"]
[sandbox.proxy]
env = { T_USE_PROXY = "1" }
[sandbox.proxy.files."t.xml"]
lines = ["{proxy.password}"]
[sandbox.run-home.t]
env = "T_HOME"
proxy = { file = "t.properties", lines = ["p={proxy.password}"] }
"#;
    let base = ours();
    let with = build(crate::packs::builtin_manifests(), &[("rust".into(), mine.into())]);
    assert_eq!((&with.proxy, &with.registries), (&base.proxy, &base.registries));
    assert_eq!(with.run_homes, base.run_homes);
    for key in ["sandbox.proxy", "sandbox.registries", "sandbox.run-home"] {
        let said = format!("your rust pack: `{key}` is read from tarae's own packs only");
        assert!(with.problems.contains(&said), "{key}: {:?}", with.problems);
    }
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.into(), v.into())
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
    let scratch = crate::scratch::Scratch::new("filled");
    let dir = scratch.to_path_buf();
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
