//! `cargo xtask compat [stack …] [--lane gate|quick|net] [--linux a+|c|all] [--dry-run]` — the "must work"
//! compatibility suite (`target/sb/compat-plan.md` §4–§5). tarae's tests built under `perf` (a debug supervisor
//! serving every connect would skew the times), then the ignored driver (`compat::tests::driver`, `src/compat.rs`)
//! run here and, asked, on Linux tiers in Docker through `cargo xtask linux` — A+ with `--userns`, C under Docker's
//! own profiles (Ubuntu 24.04's restriction's tier), in `tarae-compat-<stack>` where that image is built (piece 4),
//! else the toolchain image (a stack's missing tools are skipped cells). The driver makes each stack's project from
//! its fixture (`compat/<stack>.toml`, `compat/<fixture>/`), runs its `prewarm` outside the sandbox, then each entry
//! of the lane as Claude's run (`quick`: the entries marked so).
//!
//! Its `COMPAT` lines make the table — `target/compat/<date>-<os>.md`, one per stack, commands down and OS/tier
//! across, `pending` entries' design in a column of its own — and a row each in `target/compat/history.tsv`,
//! against which what changed since is said. A ✗ cell fails the task (a `:preflight` step's verdict).
//! `--dry-run` prints every command it would run, the prewarm included, and runs none. `--lane net` is piece 5's.

use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::linux::quoted;
use crate::{Result, root};

const USAGE: &str =
    "usage: cargo xtask compat [stack …] [--lane gate|quick|net] [--linux a+|c|all] [--dry-run]";

/// The manifests and fixtures, and where the tables and their history go, from the checkout's root.
const MANIFESTS: &str = "compat";
const OUT: &str = "target/compat";
const HISTORY: &str = "target/compat/history.tsv";
const HISTORY_HEAD: &str = "date\tcommit\tlane\tcolumn\tstack\tentry\tcell\n";

/// `cargo test` building tarae's own tests under `perf`, and the driver's arguments after it.
const TEST: [&str; 5] = ["test", "--profile", "perf", "--bin", "tarae"];
const DRIVE: [&str; 6] =
    ["--", "--ignored", "--exact", "compat::tests::driver", "--nocapture", "--test-threads=1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    /// Every entry, the network closed and the caches warmed: what must work.
    Gate,
    /// The entries marked `quick` (build and test).
    Quick,
    /// Installs through tarae's proxy, hosts granted: piece 5.
    Net,
}

impl Lane {
    fn word(self) -> &'static str {
        match self {
            Lane::Gate => "gate",
            Lane::Quick => "quick",
            Lane::Net => "net",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    APlus,
    C,
}

impl Tier {
    fn column(self) -> &'static str {
        match self {
            Tier::APlus => "Linux A+",
            Tier::C => "Linux C",
        }
    }

    /// `cargo xtask linux`'s arguments for it.
    fn args(self) -> &'static [&'static str] {
        match self {
            Tier::APlus => &["--userns"],
            Tier::C => &[],
        }
    }
}

#[derive(Debug, PartialEq)]
struct Ask {
    stacks: Vec<String>,
    lane: Lane,
    linux: Vec<Tier>,
    dry: bool,
}

fn ask(args: &[String]) -> Result<Ask> {
    let mut a = Ask { stacks: Vec::new(), lane: Lane::Gate, linux: Vec::new(), dry: false };
    let mut it = args.iter().map(String::as_str);
    while let Some(x) = it.next() {
        match x {
            "--lane" => {
                a.lane = match it.next() {
                    Some("gate") => Lane::Gate,
                    Some("quick") => Lane::Quick,
                    Some("net") => Lane::Net,
                    l => return Err(format!("--lane {}: gate, quick or net — {USAGE}", l.unwrap_or("?"))),
                }
            }
            "--linux" => {
                a.linux = match it.next() {
                    Some("a+") => vec![Tier::APlus],
                    Some("c") => vec![Tier::C],
                    Some("all") => vec![Tier::APlus, Tier::C],
                    t => return Err(format!("--linux {}: a+, c or all — {USAGE}", t.unwrap_or("?"))),
                }
            }
            "--dry-run" => a.dry = true,
            s if s.starts_with('-') => return Err(format!("{s}? — {USAGE}")),
            s if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => {
                a.stacks.push(s.into())
            }
            s => return Err(format!("{s}: a stack's name — {USAGE}")),
        }
    }
    Ok(a)
}

/// What the xtask reads of a manifest — the harness reads it whole and checks it (`src/compat.rs`): what's done
/// before its entries, to say it, and its entries, for the table's rows.
#[derive(Debug, PartialEq)]
struct Stack {
    name: String,
    fixture: Option<String>,
    prewarm: Vec<String>,
    rows: Vec<Row>,
}

#[derive(Debug, PartialEq)]
struct Row {
    name: String,
    quick: bool,
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
        pending: s(e, "pending"),
    });
    let prewarm = t.get("prewarm").and_then(|p| p.as_array()).map(Vec::as_slice).unwrap_or_default();
    Ok(Stack {
        name: name.into(),
        fixture: s(&t, "fixture"),
        prewarm: prewarm.iter().filter_map(|p| p.as_str().map(String::from)).collect(),
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
    s.rows.iter().filter(move |r| lane != Lane::Quick || r.quick)
}

/// The driver's variables: which stacks, which lane.
fn envs(stacks: &[Stack], lane: Lane) -> [(&'static str, String); 2] {
    let names: Vec<&str> = stacks.iter().map(|s| s.name.as_str()).collect();
    [("TARAE_COMPAT_STACKS", names.join(",")), ("TARAE_COMPAT_LANE", lane.word().into())]
}

/// The image a stack's Linux runs take where it's built (piece 4).
fn image_of(stack: &str) -> String {
    format!("tarae-compat-{stack}")
}

/// `cargo xtask linux`'s arguments for one stack on `tier`: the driver's variables, the tier, the image, the driver.
fn linux_args(s: &Stack, lane: Lane, tier: Tier, image: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    for (k, v) in envs(std::slice::from_ref(s), lane) {
        a.extend(["--env".into(), format!("{k}={v}")]);
    }
    a.extend(tier.args().iter().map(|t| t.to_string()));
    if let Some(i) = image {
        a.extend(["--image".into(), i.into()]);
    }
    a.extend(TEST.iter().chain(&DRIVE).map(|t| t.to_string()));
    a
}

/// What a run does, in order, as commands — what `--dry-run` prints.
fn plan(stacks: &[Stack], lane: Lane, linux: &[Tier], cargo: &str) -> Vec<String> {
    let mut out = vec![format!("{cargo} {} --no-run --quiet", TEST.join(" "))];
    for s in stacks {
        let from = match &s.fixture {
            Some(f) => format!("a copy of {MANIFESTS}/{f}/"),
            None => "an empty project".into(),
        };
        for p in &s.prewarm {
            out.push(format!("# {}: the driver first runs, outside the sandbox, in {from}:", s.name));
            out.push(format!("sh -c {}", quoted(p)));
        }
    }
    let set: Vec<String> = envs(stacks, lane).iter().map(|(k, v)| format!("{k}={v}")).collect();
    out.push(format!("{} {cargo} {} {}", set.join(" "), TEST.join(" "), DRIVE.join(" ")));
    for t in linux {
        for s in stacks {
            let i = image_of(&s.name);
            out.push(format!("# {}, {}: --image {i} where it's built, else none", s.name, t.column()));
            let args: Vec<String> = linux_args(s, lane, *t, Some(&i)).iter().map(|a| quoted(a)).collect();
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
    /// One line of its output: a cell taken; else said as it comes.
    fn line(&mut self, l: &str) {
        let parts: Option<Vec<&str>> = l.strip_prefix("COMPAT\t").map(|r| r.splitn(3, '\t').collect());
        match parts.as_deref() {
            Some([stack, entry, cell]) => {
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
    let stacks = stacks(&root.join(MANIFESTS), &a.stacks)?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    if a.dry {
        for l in plan(&stacks, a.lane, &a.linux, &cargo) {
            println!("{l}");
        }
        return Ok(());
    }
    eprintln!("building tarae's tests under perf …");
    let mut build = Command::new(&cargo);
    build.args(TEST).args(["--no-run", "--quiet"]).current_dir(&root);
    if !build.status().map_err(|e| format!("cargo: {e}"))?.success() {
        return Err("building tarae's tests under perf failed".into());
    }
    let mut cells = Cells::new();
    let mut columns = vec![here()];
    let mut c = Command::new(&cargo);
    c.args(TEST).args(DRIVE).current_dir(&root).envs(envs(&stacks, a.lane));
    fill(&mut cells, here(), &stacks, a.lane, &driven(c)?);
    let docker = match a.linux.is_empty() {
        true => Ok(()),
        false => crate::linux::reachable(&mut Command::new("docker")),
    };
    let xtask = std::env::current_exe().map_err(|e| format!("this xtask: {e}"))?;
    for t in &a.linux {
        columns.push(t.column());
        for s in &stacks {
            let d = match &docker {
                Err(why) => {
                    Driven { skipped: Some(format!("Docker isn't reachable: {why}")), ..Driven::default() }
                }
                Ok(()) => {
                    let i = image_of(&s.name);
                    let built =
                        Command::new("docker").args(["image", "inspect", "--format", ".", &i]).output();
                    let image = built.is_ok_and(|o| o.status.success()).then_some(i);
                    eprintln!(
                        "{}, {}: in {}",
                        s.name,
                        t.column(),
                        image.as_deref().unwrap_or("the toolchain's image")
                    );
                    let mut c = Command::new(&xtask);
                    c.arg("linux").args(linux_args(s, a.lane, *t, image.as_deref())).current_dir(&root);
                    driven(c)?
                }
            };
            fill(&mut cells, t.column(), std::slice::from_ref(s), a.lane, &d);
        }
    }
    let (date, commit) = (crate::perf::today(), crate::perf::commit(&root));
    let file = root.join(HISTORY);
    let past = std::fs::read_to_string(&file).unwrap_or_default();
    let head = format!("{date}, {commit}, the {} lane", a.lane.word());
    let md = table(&head, &columns, &stacks, a.lane, &cells, &past);
    let out = root.join(OUT).join(format!("{date}-{}.md", std::env::consts::OS));
    std::fs::create_dir_all(root.join(OUT)).map_err(|e| e.to_string())?;
    std::fs::write(&out, &md).map_err(|e| format!("{}: {e}", out.display()))?;
    let kept = history(&past, (&date, &commit, a.lane), &cells);
    std::fs::write(&file, kept).map_err(|e| format!("{}: {e}", file.display()))?;
    println!("{md}\nwritten to {}, kept in {}", out.display(), file.display());
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
"#;

    #[test]
    fn the_arguments_are_stacks_a_lane_tiers_and_a_dry_run() {
        let a = ask(&args("node-npm go --lane quick --linux all --dry-run")).unwrap();
        assert_eq!(
            a,
            Ask {
                stacks: vec!["node-npm".into(), "go".into()],
                lane: Lane::Quick,
                linux: vec![Tier::APlus, Tier::C],
                dry: true
            }
        );
        let a = ask(&[]).unwrap();
        assert_eq!((a.lane, a.linux.len(), a.dry), (Lane::Gate, 0, false));
        assert_eq!(ask(&args("--linux c")).unwrap().linux, vec![Tier::C]);
        assert_eq!(ask(&args("--lane net")).unwrap().lane, Lane::Net);
        for (bad, why) in [
            ("--lane", "--lane ?"),
            ("--lane fast", "gate, quick or net"),
            ("--linux b", "a+, c or all"),
            ("--repeat", "--repeat?"),
            ("../node", "a stack's name"),
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
        let p = plan(&s, Lane::Gate, &[Tier::APlus, Tier::C], "cargo");
        let drive = "-- --ignored --exact compat::tests::driver --nocapture --test-threads=1";
        assert_eq!(
            p,
            [
                "cargo test --profile perf --bin tarae --no-run --quiet".to_string(),
                "# node-npm: the driver first runs, outside the sandbox, in a copy of compat/node-npm/:"
                    .into(),
                "sh -c 'npm ci'".into(),
                "# node-npm: the driver first runs, outside the sandbox, in a copy of compat/node-npm/:"
                    .into(),
                r#"sh -c 'echo '\''it'\'''\''s warm'\'''"#.into(),
                format!(
                    "TARAE_COMPAT_STACKS=node-npm TARAE_COMPAT_LANE=gate cargo test --profile perf --bin tarae {drive}"
                ),
                "# node-npm, Linux A+: --image tarae-compat-node-npm where it's built, else none".into(),
                "cargo xtask linux '--env' 'TARAE_COMPAT_STACKS=node-npm' '--env' 'TARAE_COMPAT_LANE=gate' \
                 '--userns' '--image' 'tarae-compat-node-npm' 'test' '--profile' 'perf' '--bin' 'tarae' '--' \
                 '--ignored' '--exact' 'compat::tests::driver' '--nocapture' '--test-threads=1'"
                    .into(),
                "# node-npm, Linux C: --image tarae-compat-node-npm where it's built, else none".into(),
                "cargo xtask linux '--env' 'TARAE_COMPAT_STACKS=node-npm' '--env' 'TARAE_COMPAT_LANE=gate' \
                 '--image' 'tarae-compat-node-npm' 'test' '--profile' 'perf' '--bin' 'tarae' '--' \
                 '--ignored' '--exact' 'compat::tests::driver' '--nocapture' '--test-threads=1'"
                    .into(),
            ]
        );
    }

    #[test]
    fn cells_come_from_the_drivers_lines_and_a_missing_one_says_why() {
        let s = [stack("node-npm", NODE).unwrap()];
        let mut d = Driven { ok: true, ..Driven::default() };
        for l in ["running 1 test", "COMPAT\tnode-npm\ttest\t✓ 1.7×", "COMPAT\tnode-npm\t(manifest)\t✗ x\ty"]
        {
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
        fill(&mut cells, "Linux C", &s, Lane::Quick, &skipped);
        assert_eq!(of(&cells, "Linux C", "test"), "– skipped: Docker isn't reachable: no socket");
        assert!(
            !cells.contains_key(&("Linux C".into(), "node-npm".into(), "lint".into())),
            "not in the lane"
        );
        fill(&mut cells, "Linux A+", &s, Lane::Quick, &Driven::default());
        assert_eq!(of(&cells, "Linux A+", "test"), "✗ harness (its output above)");
    }

    #[test]
    fn the_table_is_commands_by_columns_pending_apart_and_what_changed_since() {
        let s = [stack("node-npm", NODE).unwrap()];
        let mut cells = Cells::new();
        for (c, e, cell) in [
            ("macOS", "test", "✓"),
            ("macOS", "lint", "✗ card"),
            ("macOS", "add", "⊘"),
            ("Linux C", "test", "✓ 1.7×"),
            ("Linux C", "lint", "– skipped: a | b"),
        ] {
            cells.insert((c.into(), "node-npm".into(), e.into()), cell.into());
        }
        let past = history("", ("2026-10-14", "abc", Lane::Gate), &{
            let mut was = cells.clone();
            was.insert(("macOS".into(), "node-npm".into(), "lint".into()), "✓".into());
            was
        });
        let t = table("2026-10-15, def, the gate lane", &["macOS", "Linux C"], &s, Lane::Gate, &cells, &past);
        assert_eq!(
            t,
            "# compat — 2026-10-15, def, the gate lane\n\n\
             ## node-npm\n\n\
             | command | macOS | Linux C | pending |\n\
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
}
