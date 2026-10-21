//! The loops of the readers of what a run says (V2 step 5, `target/sb/v2-fuzz-plan.md` §1 #8): the log's denials
//! (`parse_denial`) and the run's own output (`Policy::said_in_output`, through `paths_in`, `named_in` and
//! `landlock_said_in`). The run picks the words, the paths and its process's name; what's said of them is a
//! denial the line holds, never one the policy wouldn't make.

use std::path::Path;

use super::*;
use crate::mutate::{self, Rng};

/// Lines each loop reads.
const CASES: usize = 20_000;

/// `log stream`'s messages for a run tagged `tag`, as the measurements saw them (`target/sb`, the tests'): a
/// path with a space, a target after which the tag comes on the line, repeats, a mach service, compat's Gradle and
/// Python ones — and a process named like a denial (a run names its program).
fn messages(tag: &str) -> Vec<String> {
    [
        "Sandbox: touch(80432) deny(1) file-write-create /Users/u/a b\n{tag}",
        "Sandbox: nc(9) deny(1) network-outbound 192.0.2.1:9 {tag}",
        "3 duplicate reports for Sandbox: sh(5) deny(1) file-write-data /x\n{tag}",
        "Sandbox: node(5121) deny(1) mach-lookup com.apple.coreservices.launchservicesd\n{tag}",
        "Sandbox: java(301) deny(1) file-write-unlink /Users/u/.gradle/wrapper/dists/gradle-8.10-bin/x/gradle-8.10-bin.zip.ok\n{tag}",
        "Sandbox: python3.13(77) deny(1) file-write-create /opt/homebrew/Cellar/python@3.13/3.13.1/lib/python3.13/__pycache__/os.cpython-313.pyc\n{tag}",
        "Sandbox: a(1) deny(1) x (9) deny(1) file-read-data /private/etc/x {tag}",
    ]
    .iter()
    .map(|m| m.replace("{tag}", tag))
    .collect()
}

/// Whatever a log line holds, a denial read from it is its message's: the run's tag there (on the first line's
/// end or a line of its own), and the process, the operation and the target words of the message's first line —
/// never text from elsewhere in the JSON, nor a tag that only starts like this run's. The messages above
/// mutated, and half of them their JSON too, 20,000 times.
#[test]
fn a_denial_read_from_the_log_is_in_its_message() {
    let tag = "tarae-7-1.";
    let seeds = messages(tag);
    let words = [
        tag,
        "tarae-7-10.",
        "Sandbox: ",
        ") deny(",
        " deny(1) ",
        "(12)",
        "\n",
        "\\n",
        "\"",
        "file-write-create ",
    ];
    let mut rng = Rng(0x5eed_0add_7e55_0101);
    let mut read = 0;
    for _ in 0..CASES {
        let mut m = rng.pick(&seeds).clone();
        mutate::text(&mut rng, &mut m, &words);
        let mut line = serde_json::json!({ "eventMessage": m, "processID": 0 }).to_string();
        if rng.below(2) == 0 {
            mutate::text(&mut rng, &mut line, &words);
        }
        let Some(d) = parse_denial(&line, tag) else { continue };
        read += 1;
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let msg = v["eventMessage"].as_str().unwrap();
        let first = msg.split('\n').next().unwrap_or_default();
        let tagged = first.trim_end().ends_with(&format!(" {tag}")) || msg.lines().any(|l| l.trim() == tag);
        assert!(tagged, "{d:?}: no tag {tag} in {msg:?}");
        for (what, w) in
            [("process", format!("{}(", d.process)), ("op", d.op.clone()), ("target", d.target.clone())]
        {
            assert!(first.contains(&w), "{d:?}: its {what} isn't in {first:?}");
        }
        assert!(first.contains("Sandbox: ") && first.contains(") deny("), "{d:?} of {first:?}");
    }
    assert!(read > 3000, "{read} read");
}

/// The project the output's runs ran in.
const ROOT: &str = "/Users/u/p";

/// What the measurements' runs printed (`target/sb`: compat, P2, Linux C): refusals in each tool's words, by an
/// absolute path, one from the project, from home, a name in quotes — and lines about paths the rules allow.
const OUTPUT: [&str; 14] = [
    "touch: cannot touch 'Makefile': Operation not permitted",
    "sh: 1: cannot create build.rs: Permission denied",
    "cp: cannot create regular file ‘sub/.envrc’: Operation not permitted",
    "touch: cannot touch 'src/a.rs': Operation not permitted",
    "error: failed to open: /Users/u/.cargo/registry/cache/x/.package-cache",
    "Caused by:",
    "  Operation not permitted (os error 1)",
    "mkdir: /Users/u/other/x: Operation not permitted",
    "rm: cannot remove './target/../Cargo.toml': Permission denied",
    "cat: ~/.ssh/id_x: Operation not permitted",
    "bash: kill: (4242) - Operation not permitted",
    "error: could not write `/private/tmp/x`: Operation not permitted",
    "warning: /Users/u/p/src/main.rs: unable to read, Permission denied",
    "   Compiling p v0.1.0 (/Users/u/p)",
];

/// Whatever a run prints, the denials read from its output are the policy's: a write it refuses said as one
/// (`file-write*` only where it lets the run read, `file-read-data` only where it lets it write, both where
/// neither), never one at a path it allows both ways; Landlock's entry refusals only in a folder that takes no new
/// entry; a signal's target a process number or nothing. Each is at a path one of the lines names (its last part
/// on a line — `..` folded, `~` and relative paths made whole: `~/` alone home, `../` a folder above the project). One to four of the lines above, mutated, 20,000
/// times.
#[test]
fn the_output_reader_says_no_denial_the_policy_wouldnt_make() {
    let p = Policy::for_project(&tests::env(Path::new(ROOT)));
    let words = [
        "Operation not permitted",
        "Permission denied",
        "cannot create ",
        ": ",
        "'",
        "/Users/u/p/",
        "../",
        "./",
        "~/",
        "Cargo.toml",
        "/dev/null",
    ];
    let mut rng = Rng(0x5eed_0add_7e55_0102);
    let (mut said, mut allowed) = (0, 0);
    for _ in 0..CASES {
        let out: Vec<(bool, String)> = (0..1 + rng.below(4))
            .map(|_| {
                let mut l = rng.pick(&OUTPUT).to_string();
                mutate::text(&mut rng, &mut l, &words);
                (true, l)
            })
            .collect();
        allowed += usize::from(out.iter().any(|(_, l)| l.contains("src/a.rs")));
        for d in p.said_in_output(&out, &[]) {
            said += 1;
            assert_eq!(d.process, SEEN_IN_OUTPUT, "{d:?}");
            let real = Path::new(&d.target);
            let (reads, writes) = (p.reads_allow(real), p.writes_allow(real));
            let policys = match d.op.as_str() {
                "file-write*" => reads && !writes,
                "file-read-data" => !reads && writes,
                EITHER => !reads && !writes,
                "file-entry-make" | "file-entry-remove" | "file-entry-move" => {
                    writes && real.parent().is_some_and(|f| p.closed_holds(f))
                }
                "signal" => d.target.bytes().all(|b| b.is_ascii_digit()),
                _ => false,
            };
            assert!(policys, "{d:?} (reads {reads}, writes {writes}) from {out:?}");
            // (`~/` alone is home, which no line spells but as `~`; `../` from the project a folder above it)
            let home = std::env::var_os("HOME").is_some_and(|h| real == Path::new(&h));
            let above = Path::new(ROOT).starts_with(real);
            if d.op != "signal"
                && let Some(name) = real.file_name()
            {
                let name = name.to_string_lossy();
                let on = |l: &str| l.contains(&*name) || home && l.contains('~') || above && l.contains("..");
                assert!(out.iter().any(|(_, l)| on(l)), "{d:?}: {name} on no line of {out:?}");
            }
        }
    }
    assert!(said > 2000 && allowed > 1000, "{said} said, {allowed} outputs naming an allowed path");
}
