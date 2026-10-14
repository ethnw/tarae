//! `cargo xtask perf` (`cargo perf`) — tarae's performance as numbers kept over time, not only pass or fail.
//!
//! Builds the budgets (`term.rs` `perf_budget_*`) and the real-use scenarios (`tests/perf.rs`: startup, a key
//! in a big file, scrolling, idle CPU, memory) in the `perf` profile (release, with line tables), runs them
//! `RUNS` times one test at a time, and takes each number's median. Every test prints `PERF <name> <value>
//! budget=<value> [noise=<value>]`; the medians go to `target/perf/history.tsv` (date, commit — `+` when the
//! tree has changes — name, value, budget) and are compared with the median of the same number's last
//! `HISTORY` runs: more than `SLOWER` slower *and* by more than the number's own noise is said (a warning,
//! not a failure). The noise is the number's, in its own unit — what it moves between quiet runs (a
//! worst-of-N frame jumps a tenth of a millisecond: `DEFAULT_NOISE`), not a share of its budget: a loose
//! budget (startup's 500 ms) would hide a real slowdown (5.7 → 10 ms).
//!
//! The release binary's size is measured too (`SIZES`, against a budget), and a jump in it fails: more than
//! `SIZE_JUMP` above the median of its last runs on this branch (`size_jump`) — the budget leaves room for
//! the roadmap's features, so alone it wouldn't see a dependency's +0.5 MB. `--accept-jump` keeps one that's
//! meant.
//!
//! Only on a quiet machine: with a load of `BUSY` or more it waits up to `WAIT` for it to fall (a fat-LTO
//! build leaves the load up), then says "not measured (machine busy)" and keeps nothing — numbers from a
//! busy machine (or a sandboxed parallel run) would only be noise. A budget broken on a quiet machine is a
//! failure.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::{Result, root};

/// Runs whose median is kept.
const RUNS: usize = 3;
/// The 1-minute load at or above which nothing is measured.
const BUSY: f64 = 5.0;
/// Past runs a number is compared with (their median).
const HISTORY: usize = 5;
/// Slower than the past median by this much is said…
const SLOWER: f64 = 1.15;
/// …when the difference is also more than the number's noise — unless it says its own (`noise=`), this, in
/// milliseconds: above the worst-of-N frame numbers' jumps between quiet runs (+0.13 ms measured).
const DEFAULT_NOISE: f64 = 0.25;
/// How long a busy machine is waited for (`TARAE_PERF_WAIT` seconds instead).
const WAIT: u64 = 90;

/// One measured number.
#[derive(Clone, Debug, PartialEq)]
struct Perf {
    name: String,
    value: f64,
    budget: f64,
    /// What it moves between quiet runs, in its own unit.
    noise: f64,
}

/// Scenarios run once, not `RUNS` times — a memory number, not a jittery timing, and the longest by far (the
/// long editing: 48 s a run, measured 2026-10-03).
const ONCE: [&str; 1] = ["perf_e2e_memory_over_long_editing"];

/// What run `i` (from 0) asks of the perf tests: all of them in the first, the `ONCE` ones left out after.
fn filters(i: usize) -> Vec<&'static str> {
    let mut f = vec!["--quiet", "--", "perf_", "--test-threads=1"];
    if i > 0 {
        for t in ONCE {
            f.extend(["--skip", t]);
        }
    }
    f
}

pub fn run(args: &[String]) -> Result<()> {
    let all_sizes = args.iter().any(|a| a == "--sizes");
    let root = root();
    if let Some(load) = wait_quiet() {
        println!("not measured (machine busy: load {load:.1}, under {BUSY} needed)");
        return Ok(());
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let test = |extra: &[&str]| {
        let mut c = Command::new(&cargo);
        // (the dev grammars: the syntax budgets highlight Rust)
        c.args([
            "test",
            "--profile",
            "perf",
            "--features",
            "dev-grammars",
            "--bin",
            "tarae",
            "--test",
            "perf",
        ])
        .args(extra)
        .current_dir(&root);
        c
    };
    eprintln!("building the perf profile …");
    let st = test(&["--no-run", "--quiet"]).status().map_err(|e| format!("cargo: {e}"))?;
    if !st.success() {
        return Err("building the perf profile failed".into());
    }
    let mut runs: Vec<Vec<Perf>> = Vec::new();
    for i in 0..RUNS {
        if let Some(load) = wait_quiet() {
            println!("not measured (machine busy: load {load:.1} before run {})", i + 1);
            return Ok(());
        }
        eprintln!("run {} of {RUNS} …", i + 1);
        let out = test(&filters(i)).output().map_err(|e| format!("cargo: {e}"))?;
        let text =
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            return Err(format!("run {} of {RUNS} failed:\n{}", i + 1, failures(&text)));
        }
        runs.push(parse(&text));
    }
    let mut now = medians(&runs);
    if now.is_empty() {
        return Err("no PERF lines — did the tests run in the perf profile?".into());
    }
    now.extend(sizes(&cargo, &root, all_sizes)?);
    let file = root.join("target/perf/history.tsv");
    let past = std::fs::read_to_string(&file).unwrap_or_default();
    let history = read_history(&past);
    let width = now.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let mut slower = 0;
    for p in &now {
        let was = history.get(&p.name).map(|v| median(v.iter().rev().take(HISTORY).copied().collect()));
        let vs = match was {
            Some(was) if is_slower(p.value, was, p.noise) => {
                slower += 1;
                format!("  SLOWER than {was:.3} (+{:.0}%)", (p.value / was - 1.0) * 100.0)
            }
            Some(was) if was > 0.0 => format!("  ({:+.0}% vs {was:.3})", (p.value / was - 1.0) * 100.0),
            _ => String::new(),
        };
        // A budget as it is (`{}`: 16 → "16", 9.5 → "9.5" — `{:.0}` said 10 for the size's 9.5)
        println!("{:width$}  {:>10.3}  budget {:>6}{vs}", p.name, p.value, p.budget);
    }
    // A jump in the release size fails whatever room the budget has — and keeps nothing, as a broken budget
    // doesn't: kept, three runs again would make it the median. One that's meant is kept with `--accept-jump`.
    if let Some(size) = now.iter().find(|p| p.name == SIZE_JUMP.0) {
        let mut known: BTreeMap<String, bool> = BTreeMap::new();
        let has = |c: &str| *known.entry(c.to_string()).or_insert_with(|| head_has(&root, c));
        let jump = size_jump(size.value, &kept(&past, SIZE_JUMP.0), has);
        let said = jump_said(SIZE_JUMP.0, size.value, &jump);
        match jump {
            Jump::Over { .. } if !args.iter().any(|a| a == "--accept-jump") => return Err(said),
            Jump::Over { .. } => println!("{said} — kept (--accept-jump)"),
            _ => println!("{said}"),
        }
    }
    let (date, commit) = (today(), commit(&root));
    let mut add = String::new();
    if past.is_empty() {
        add.push_str("date\tcommit\tname\tvalue\tbudget\n");
    }
    for p in &now {
        add.push_str(&format!("{date}\t{commit}\t{}\t{:.3}\t{}\n", p.name, p.value, p.budget));
    }
    std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut kept = past;
    kept.push_str(&add);
    std::fs::write(&file, kept).map_err(|e| format!("{}: {e}", file.display()))?;
    println!(
        "{} numbers, median of {RUNS} runs, kept in {} ({commit}){}",
        now.len(),
        file.display(),
        if slower > 0 { format!(" — {slower} slower than before") } else { String::new() }
    );
    Ok(())
}

/// The release binary's size in MB (10⁶ bytes, as Finder and `ls -lh` say): `(name, cargo flags, target dir,
/// budget)`. The bundled one builds in a target dir of its own, so `target/release/tarae` — what `:restart`
/// runs — stays the default build. Sizes don't move between runs: the noise is a few edits' worth.
/// 9.5 (2026-10-04, ~8.5 then): feature batches added 0.03–0.24 MB each, so it has room for the roadmap
/// (queues, UI state, the RA swap) and still trips on a dependency's +0.5 MB (ripgrep's crates were).
/// 10 (2026-10-05, 9.41 then): the user's call — a size pass comes at the roadmap's end.
/// 12 (2026-10-09, 11.05 then; ~10.95 with std's sorts back in `sorting` — target/sb/size-2026-10.md): the
/// user's call — features grew it ~0.4 MB a day, no one cause; the size pass stays at the roadmap's end, and a
/// dependency's jump is `SIZE_JUMP`'s to catch, not the budget's.
/// Bundled 17 (2026-10-14, 16.84 then): the user's call — the bundled budget follows the default's 12 plus the
/// grammars' ~5 MB (it stayed at 15 while the default went 9.5 → 10 → 12).
const SIZES: [(&str, &[&str], &str, f64); 2] = [
    ("release_size_mb", &[], "target", 12.0),
    ("release_size_bundled_mb", &["--features", "bundled-grammars"], "target/bundled-size", 17.0),
];
const SIZE_NOISE: f64 = 0.05;

/// The number a jump fails (`size_jump`), and by how much over its past median (MB): a dependency's +0.5 MB
/// (ripgrep's crates were) trips it; a batch of features (0.03–0.24 MB measured) doesn't. Steady growth of
/// more than a third of it per recorded run trips it too — the median is three runs back.
const SIZE_JUMP: (&str, f64) = ("release_size_mb", 0.3);

/// One kept run of a number (`target/perf/history.tsv`): when, at which commit (`+`: with changes), its value.
#[derive(Clone, Debug, PartialEq)]
struct Kept {
    date: String,
    commit: String,
    value: f64,
}

/// What `size_jump` found.
#[derive(Clone, Debug, PartialEq)]
enum Jump {
    /// No kept run this branch has: nothing to compare with.
    NoHistory,
    /// Within `SIZE_JUMP` of the median of `runs` (newest first).
    Within { median: f64, runs: Vec<Kept> },
    /// More than `SIZE_JUMP` above it, by `by`.
    Over { median: f64, by: f64, runs: Vec<Kept> },
}

/// The release size `now` against the median of the last `HISTORY` kept runs (`rows`, oldest first) whose
/// commit HEAD has (`has`: that commit is HEAD or comes before it) — a checkout's history mixes branches (a
/// session's branch brought to main, an experiment, a rebase), and another branch's size says nothing of
/// this one's step. Fewer such runs than `HISTORY`: the median of those there are (sizes don't jitter: one is
/// enough to see a jump); none: nothing to compare.
fn size_jump(now: f64, rows: &[Kept], mut has: impl FnMut(&str) -> bool) -> Jump {
    let runs: Vec<Kept> = rows.iter().rev().filter(|r| has(&r.commit)).take(HISTORY).cloned().collect();
    if runs.is_empty() {
        return Jump::NoHistory;
    }
    let median = median(runs.iter().map(|r| r.value).collect());
    // (kept to three places: 11.3 − 11.0 is 0.30000000000000071 — "more than" by a thousandth)
    match now - median {
        by if by > SIZE_JUMP.1 + 1e-6 => Jump::Over { median, by, runs },
        _ => Jump::Within { median, runs },
    }
}

/// How a jump is said: the size, by how much over which median, and the runs it's the median of.
fn jump_said(name: &str, now: f64, j: &Jump) -> String {
    let runs = |runs: &[Kept]| {
        let each: Vec<String> =
            runs.iter().map(|r| format!("{} {} {:.3}", r.date, r.commit, r.value)).collect();
        format!("{} run{} ({})", runs.len(), if runs.len() == 1 { "" } else { "s" }, each.join(", "))
    };
    match j {
        Jump::NoHistory => format!("{name}: no kept run on this branch to compare a jump with"),
        Jump::Within { median, runs: r } => {
            format!("{name}: {:+.3} MB vs {median:.3}, the median of its last {}", now - median, runs(r))
        }
        Jump::Over { median, by, runs: r } => format!(
            "{name}: {now:.3} MB is {by:.3} MB above {median:.3}, the median of its last {} — more than the \
             {} MB a jump may be (a dependency?). If it's meant: `cargo perf --accept-jump` keeps it",
            runs(r),
            SIZE_JUMP.1
        ),
    }
}

/// `name`'s kept runs in `tsv`, oldest first.
fn kept(tsv: &str, name: &str) -> Vec<Kept> {
    tsv.lines()
        .skip_while(|l| l.starts_with("date\t"))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let value = f.get(3)?.parse().ok()?;
            (f.get(2) == Some(&name)).then(|| Kept { date: f[0].into(), commit: f[1].into(), value })
        })
        .collect()
}

/// Whether HEAD has `commit` (a kept run's: `+` — with changes — is the commit's all the same). `?` (no git)
/// or a commit git no longer knows: no.
fn head_has(root: &Path, commit: &str) -> bool {
    let commit = commit.trim_end_matches('+');
    commit != "?"
        && Command::new("git")
            .args(["merge-base", "--is-ancestor", commit, "HEAD"])
            .current_dir(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// What the bundled binary has beyond the default one (which every run measures): which grammars are in
/// it, their sources, how they're built, and the release profile. Its size is measured again only when
/// these changed since it last was (`BUNDLED_KEY`), or with `--sizes` — a release build of ~80 s saved
/// otherwise. The packs' `pack.toml`s (their `[grammar.<name>]`) count too.
const BUNDLED_INPUTS: [&str; 2] = ["runtime/grammars.tar.gz", "build.rs"];
const BUNDLED_KEY: &str = "target/perf/bundled-size.key";

/// FNV-1a of `BUNDLED_INPUTS`, the packs and `Cargo.toml`'s `[profile.release]` section.
fn bundled_key(root: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes.iter().chain([0u8].iter()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for f in BUNDLED_INPUTS {
        eat(&std::fs::read(root.join(f)).unwrap_or_default());
    }
    let mut packs: Vec<_> = std::fs::read_dir(root.join("runtime/packs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("pack.toml"))
        .collect();
    packs.sort();
    for p in packs {
        eat(&std::fs::read(p).unwrap_or_default());
    }
    eat(release_profile(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default()).as_bytes());
    format!("{h:016x}")
}

/// A Cargo.toml's `[profile.release]` section (its header to the next table).
fn release_profile(toml: &str) -> String {
    let mut lines = toml.lines().skip_while(|l| l.trim() != "[profile.release]");
    let head = lines.next().unwrap_or_default();
    let body: Vec<&str> = lines.take_while(|l| !l.trim_start().starts_with('[')).collect();
    format!("{head}\n{}", body.join("\n"))
}

/// The `SIZES` binaries built (`cargo build --release`) and measured — the bundled one only when `all`, or
/// when what makes it differ changed since it last was (`bundled_key`).
fn sizes(cargo: &std::ffi::OsStr, root: &Path, all: bool) -> Result<Vec<Perf>> {
    let key = bundled_key(root);
    let kept = std::fs::read_to_string(root.join(BUNDLED_KEY)).unwrap_or_default();
    let bundled = all || kept.trim() != key;
    let mut out = Vec::new();
    for (name, flags, dir, budget) in SIZES {
        if !flags.is_empty() && !bundled {
            eprintln!(
                "{name}: not built — its grammars and the release profile are as when it was measured (--sizes builds it)"
            );
            continue;
        }
        eprintln!("building the release binary {} …", flags.join(" "));
        let st = Command::new(cargo)
            .args(["build", "--release", "--quiet", "--target-dir", dir])
            .args(flags)
            .current_dir(root)
            .status()
            .map_err(|e| format!("cargo: {e}"))?;
        if !st.success() {
            return Err(format!("building the release binary {} failed", flags.join(" ")));
        }
        let bin = root.join(dir).join("release/tarae");
        let len = std::fs::metadata(&bin).map_err(|e| format!("{}: {e}", bin.display()))?.len();
        let mb = len as f64 / 1e6;
        if mb >= budget {
            return Err(format!("{name}: {mb:.2} MB (budget {budget})"));
        }
        out.push(Perf { name: name.to_string(), value: mb, budget, noise: SIZE_NOISE });
        if !flags.is_empty() {
            let file = root.join(BUNDLED_KEY);
            std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&file, &key).map_err(|e| format!("{}: {e}", file.display()))?;
        }
    }
    Ok(out)
}

/// What failed, from libtest's output: each failed test with its panic's message — a budget's number, or
/// what a scenario waited for (the screen dump after it left out: a 40-line tail once held only that) — and
/// the load now. Without a failures section, the output's last lines.
fn failures(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(name) = l.strip_prefix("---- ").and_then(|r| r.strip_suffix(" stdout ----")) else {
            continue;
        };
        let section = lines[i + 1..].iter().take_while(|l| !l.starts_with("---- ") && **l != "failures:");
        let why = section
            .skip_while(|l| !l.contains("panicked at"))
            .nth(1)
            .map_or("(no panic message)", |w| w.trim());
        out.push(format!("  {name}: {why}"));
    }
    if out.is_empty() {
        let tail: Vec<&str> = lines.iter().rev().take(20).rev().copied().collect();
        return tail.join("\n");
    }
    let load = busy().map_or(format!("under {BUSY}"), |l| format!("{l:.1}"));
    format!("{}\n(load now: {load})", out.join("\n"))
}

/// None once the machine is quiet — waiting up to `WAIT` for it, saying so; else the load it stayed at.
fn wait_quiet() -> Option<f64> {
    let mut load = busy()?;
    let wait = std::env::var("TARAE_PERF_WAIT").ok().and_then(|w| w.parse().ok()).unwrap_or(WAIT);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    eprintln!("waiting up to {wait}s for the machine to quiet down (load {load:.1}) …");
    while std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_secs(5).min(end - std::time::Instant::now()));
        load = busy()?;
    }
    Some(load)
}

/// The 1-minute load when it's `BUSY` or more. `TARAE_PERF_LOAD` stands in for it (to see the busy path).
fn busy() -> Option<f64> {
    let load = match std::env::var("TARAE_PERF_LOAD").ok().and_then(|l| l.parse().ok()) {
        Some(l) => l,
        None => {
            let mut load = [0f64; 3];
            // SAFETY: fills up to 3 values into the array given.
            let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
            if n < 1 {
                return None;
            }
            load[0]
        }
    };
    (load >= BUSY).then_some(load)
}

/// The `PERF <name> <value> budget=<value> [noise=<value>]` lines in a run's output.
fn parse(text: &str) -> Vec<Perf> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.trim().strip_prefix("PERF ")?.split_whitespace();
            let name = w.next()?.to_string();
            let value = w.next()?.parse().ok()?;
            let budget = w.next()?.strip_prefix("budget=")?.parse().ok()?;
            let noise = match w.next() {
                Some(n) => n.strip_prefix("noise=")?.parse().ok()?,
                None => DEFAULT_NOISE,
            };
            Some(Perf { name, value, budget, noise })
        })
        .collect()
}

/// Each number's median over the runs (in name order).
fn medians(runs: &[Vec<Perf>]) -> Vec<Perf> {
    let mut by: BTreeMap<&str, (Vec<f64>, &Perf)> = BTreeMap::new();
    for p in runs.iter().flatten() {
        by.entry(&p.name).or_insert((Vec::new(), p)).0.push(p.value);
    }
    by.into_iter().map(|(_, (v, p))| Perf { value: median(v), ..p.clone() }).collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// The kept values of each number, oldest first.
fn read_history(tsv: &str) -> BTreeMap<String, Vec<f64>> {
    let mut out: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for l in tsv.lines().skip_while(|l| l.starts_with("date\t")) {
        let f: Vec<&str> = l.split('\t').collect();
        if let (Some(name), Some(Ok(v))) = (f.get(2), f.get(3).map(|v| v.parse::<f64>())) {
            out.entry(name.to_string()).or_default().push(v);
        }
    }
    out
}

/// More than `SLOWER` above the past median, and by more than the number's noise.
fn is_slower(now: f64, was: f64, noise: f64) -> bool {
    now > was * SLOWER && now - was > noise
}

/// `abc1234`, `+` when the tree has changes (tracked files only; read without taking git's index lock).
fn commit(root: &Path) -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let head = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "?".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    if dirty { format!("{head}+") } else { head }
}

/// Today's date (UTC) as `YYYY-MM-DD`.
fn today() -> String {
    let secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = civil(secs as i64 / 86_400);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day) — Howard Hinnant's `civil_from_days`.
pub(crate) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled binary is built again when its grammar list or the release profile changed — not for
    /// another profile, a dependency, or nothing at all.
    #[test]
    fn the_bundled_size_waits_for_what_makes_it_differ() {
        let dir = std::env::temp_dir().join(format!("tarae-xtask-key-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("runtime/packs/a")).unwrap();
        let cargo =
            "[dependencies]\nx = \"1\"\n\n[profile.release]\nlto = true\n\n[profile.shot]\nlto = false\n";
        std::fs::write(dir.join("Cargo.toml"), cargo).unwrap();
        let pack = dir.join("runtime/packs/a/pack.toml");
        std::fs::write(&pack, "[grammar.a]\nbundle = true\n").unwrap();
        let k = bundled_key(&dir);
        assert_eq!(bundled_key(&dir), k, "nothing changed");
        std::fs::write(
            dir.join("Cargo.toml"),
            cargo.replace("x = \"1\"", "x = \"2\"").replace("lto = false", ""),
        )
        .unwrap();
        assert_eq!(bundled_key(&dir), k, "a dependency, another profile");
        std::fs::write(dir.join("Cargo.toml"), cargo.replace("lto = true", "lto = \"thin\"")).unwrap();
        let k2 = bundled_key(&dir);
        assert_ne!(k2, k, "the release profile");
        std::fs::write(&pack, "[grammar.a]\nbundle = false\n").unwrap();
        assert_ne!(bundled_key(&dir), k2, "the grammar list");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A failed run names each failed test and why — a broken budget, a scenario's wait — not the screen
    /// dump that followed the wait's message.
    #[test]
    fn a_failed_run_says_which_test_and_why() {
        let screen: String = (0..40).map(|r| format!("{r:>3} │ row\n")).collect();
        let out = format!(
            "PERF a 1 budget=16\nrunning 3 tests\ntest perf_a ... ok\ntest perf_b ... FAILED\n\
             test perf_c ... FAILED\n\nfailures:\n\n---- perf_b stdout ----\nPERF b 30 budget=16\n\n\
             thread 'perf_b' (1) panicked at tests/perf.rs:34:5:\nb: 30.000 (budget 16)\n\
             note: run with `RUST_BACKTRACE=1`\n\n---- perf_c stdout ----\n\n\
             thread 'perf_c' (2) panicked at tests/harness/mod.rs:458:17:\n\
             timed out after 10s waiting for the answer — tarae still running\n{screen}\n\
             failures:\n    perf_b\n    perf_c\n\ntest result: FAILED. 1 passed; 2 failed\n\
             error: test failed, to rerun pass `--test perf`\n"
        );
        let said = failures(&out);
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines[0], "  perf_b: b: 30.000 (budget 16)", "{said}");
        assert_eq!(lines[1], "  perf_c: timed out after 10s waiting for the answer — tarae still running");
        assert!(lines[2].starts_with("(load now: "), "{said}");
        assert_eq!(lines.len(), 3, "no screen dump: {said}");
        // (no failures section — a build error: its last lines)
        assert_eq!(failures("a\nerror: could not compile"), "a\nerror: could not compile");
    }

    /// The long editing runs in the first run only; a number measured once is that run's value.
    #[test]
    fn a_once_only_number_is_its_one_value() {
        assert!(!filters(0).contains(&"--skip"));
        for i in 1..RUNS {
            let f = filters(i);
            assert!(f.windows(2).any(|w| w == ["--skip", "perf_e2e_memory_over_long_editing"]), "{f:?}");
        }
        let p = |name: &str, value: f64| Perf { name: name.into(), value, budget: 100.0, noise: 1.0 };
        let runs = vec![vec![p("a", 1.0), p("long", 80.0)], vec![p("a", 3.0)], vec![p("a", 2.0)]];
        let m = medians(&runs);
        assert_eq!(
            m.iter().map(|p| (p.name.as_str(), p.value)).collect::<Vec<_>>(),
            [("a", 2.0), ("long", 80.0)]
        );
    }

    #[test]
    fn lines_medians_history_and_slower() {
        let out = "running 2 tests\nPERF a 1.500 budget=16\nnoise PERF b 1 budget=2\nPERF b 3.0 budget=100\nPERF bad x budget=1\n";
        let n = DEFAULT_NOISE;
        assert_eq!(
            parse(out),
            [
                Perf { name: "a".into(), value: 1.5, budget: 16.0, noise: n },
                Perf { name: "b".into(), value: 3.0, budget: 100.0, noise: n }
            ]
        );
        assert_eq!(parse("PERF m 37.2 budget=400 noise=2")[0].noise, 2.0, "its own, in its unit");
        assert!(parse("PERF m 37.2 budget=400 nois=2").is_empty());
        let runs = vec![
            parse("PERF a 3 budget=16 noise=0.5"),
            parse("PERF a 1 budget=16 noise=0.5"),
            parse("PERF a 2 budget=16 noise=0.5"),
        ];
        assert_eq!(medians(&runs)[0], Perf { name: "a".into(), value: 2.0, budget: 16.0, noise: 0.5 });
        let tsv = "date\tcommit\tname\tvalue\tbudget\n2026-10-02\tabc\ta\t2.000\t16\n2026-10-03\tdef+\ta\t4.000\t16\n";
        assert_eq!(read_history(tsv)["a"], [2.0, 4.0]);
        assert!(is_slower(12.0, 10.0, n), "+20%, 2 ms");
        assert!(!is_slower(11.0, 10.0, n), "+10%");
        // Measured between two quiet runs: worst-of-N frames jumping +40% by a tenth of a millisecond
        assert!(!is_slower(0.45, 0.32, n) && !is_slower(0.30, 0.21, n), "noise");
        // A loose budget doesn't hide a real slowdown: startup 5.7 → 10 ms against a 500 ms budget
        assert!(is_slower(10.0, 5.7, 0.5));
        assert!(is_slower(43.0, 37.0, 2.0) && !is_slower(38.5, 37.0, 2.0), "memory, in MB");
        assert_eq!((civil(0), civil(20_363)), ((1970, 1, 1), (2025, 10, 2)));
    }

    /// Kept runs of the release size at `values` (oldest first), on commits `c0`, `c1` … — all this branch's.
    fn sizes_kept(values: &[f64]) -> Vec<Kept> {
        let row = |(i, v): (usize, &f64)| Kept {
            date: format!("2026-10-{:02}", i + 1),
            commit: format!("c{i}"),
            value: *v,
        };
        values.iter().enumerate().map(row).collect()
    }

    /// A dependency's +0.4 MB in one step fails, whatever room the budget has — said with how much, over
    /// which median, and the runs it's the median of (newest first). "More than" 0.3: 0.3 itself passes
    /// (as kept, to three places — 11.3 − 11.0 is 0.30000000000000071 in floating point).
    #[test]
    fn a_jump_in_the_release_size_fails_naming_the_runs() {
        let rows = sizes_kept(&[10.90, 10.95, 11.00, 11.00, 11.05]);
        let j = size_jump(11.40, &rows, |_| true);
        let Jump::Over { median, by, runs } = &j else { panic!("a jump of 0.4 MB passed: {j:?}") };
        assert_eq!(*median, 11.00);
        assert!((by - 0.40).abs() < 1e-9, "{by}");
        assert_eq!(
            runs.iter().map(|r| r.commit.as_str()).collect::<Vec<_>>(),
            ["c4", "c3", "c2", "c1", "c0"]
        );
        let said = jump_said("release_size_mb", 11.40, &j);
        assert!(said.starts_with("release_size_mb: 11.400 MB is 0.400 MB above 11.000, the median of its last 5 runs (2026-10-05 c4 11.050, 2026-10-04 c3 11.000,"), "{said}");
        assert!(said.contains("--accept-jump"), "{said}");
        assert!(matches!(size_jump(11.31, &rows, |_| true), Jump::Over { .. }), "just over");
        assert!(matches!(size_jump(11.30, &rows, |_| true), Jump::Within { .. }), "0.3 itself");
    }

    /// Features adding a little each run never trip it — only the last `HISTORY` runs count, so a long
    /// climb (20 runs, +0.95 MB in all) is judged by its last steps, not against where it started.
    #[test]
    fn slow_growth_in_the_release_size_passes() {
        let rows = sizes_kept(&[10.80, 10.85, 10.90, 10.95, 11.00]);
        assert_eq!(
            size_jump(11.05, &rows, |_| true),
            Jump::Within { median: 10.90, runs: rows.iter().rev().cloned().collect() }
        );
        let climb: Vec<f64> = (0..20).map(|i| 10.0 + 0.05 * f64::from(i)).collect();
        let j = size_jump(11.00, &sizes_kept(&climb), |_| true);
        assert!(matches!(j, Jump::Within { median, .. } if (median - 10.85).abs() < 1e-9), "{j:?}");
    }

    /// A checkout's history mixes branches: only runs at commits HEAD has count (`+`, with changes, is the
    /// commit's) — another branch's smaller binary isn't this one's jump. Fewer than `HISTORY` of them: the
    /// median of those there are; none: nothing to compare, said. The rows read back from the file as kept.
    #[test]
    fn a_size_jump_compares_this_branchs_runs_only() {
        let tsv = "date\tcommit\tname\tvalue\tbudget\n\
                   2026-10-01\ta\trelease_size_mb\t11.000\t12\n2026-10-01\ta\tstartup_ms\t6.500\t500\n\
                   2026-10-02\tx\trelease_size_mb\t9.000\t12\n2026-10-03\tx\trelease_size_mb\t9.000\t12\n\
                   2026-10-04\tb+\trelease_size_mb\t11.050\t12\n2026-10-05\ty\trelease_size_mb\t9.000\t12\n";
        let rows = kept(tsv, "release_size_mb");
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[3], Kept { date: "2026-10-04".into(), commit: "b+".into(), value: 11.05 });
        let ours = |c: &str| ["a", "b"].contains(&c.trim_end_matches('+'));
        let j = size_jump(11.10, &rows, ours);
        let Jump::Within { median, runs } = &j else { panic!("the other branch's 9.0 MB counted: {j:?}") };
        assert_eq!((*median, runs.len()), (11.05, 2), "the two this branch has");
        assert!(
            jump_said("release_size_mb", 11.10, &j)
                .contains("its last 2 runs (2026-10-04 b+ 11.050, 2026-10-01 a 11.000)")
        );
        assert!(matches!(size_jump(11.50, &rows, ours), Jump::Over { .. }), "against 11.05: a jump still");
        assert_eq!(size_jump(11.10, &rows, |_| false), Jump::NoHistory);
        assert_eq!(
            jump_said("release_size_mb", 11.10, &Jump::NoHistory),
            "release_size_mb: no kept run on this branch to compare a jump with"
        );
    }
}
