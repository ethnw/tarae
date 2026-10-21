//! The "must work" compatibility suite's harness (compat piece 1, `target/sb/compat-plan.md` §2): one entry — a
//! stack's command, the settings it runs under, what it's expected to do — run exactly as Claude's run is, with no
//! chat: `tools::run` → its card → the queue → the policy → Seatbelt or the session's keeper → the report → the
//! snapshot, the same functions a turn calls. A scripted user answers the cards: `y` on the ones the entry lists,
//! `n` on any other (never a yes to what nobody listed), each one recorded. Then it's judged: its exit, what its
//! report says, the cards it raised, what it made and wrote outside the project, and its time against the same
//! command unsandboxed in a copy.
//!
//! Tests only: the editor is the test build's, with what `cfg(test)` turns off on the run path turned back on
//! (`as_shipped`) — `BRANCHES` lists every `cfg(test)` there and what compat does about it, and a test fails when
//! one is added. Entries come from a manifest (`Manifest::parse`), one per stack.
//!
//! **Manifests are reviewed code.** The baseline runs an entry's command outside the sandbox, with the user's real
//! HOME, and so does `prewarm`: an entry's `run` is never taken from a repository under test. The real-repo lane
//! (piece 5) pins each repository at a commit and its manifest is read before it runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;

use crate::editor::Editor;

/// What an entry is expected to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    /// It succeeds in the sandbox, and the sandbox blocked nothing.
    Pass,
    /// It fails, and its report says each of these (what it was refused, in the report's words).
    Refused(Vec<String>),
}

/// One command of a stack's, as Claude would run it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    /// The command line, as Claude's `run` gets it.
    pub run: String,
    /// Where it runs from, in the project (`run`'s `cwd`).
    pub cwd: Option<String>,
    pub expect: Expect,
    /// The cards it's expected to raise, each `kind` (`run`) or `kind why` (`run not-a-check:npm`) as the card
    /// line names them (`cards::line`): answered `y`. Any other is answered `n` and fails it.
    pub cards: Vec<String>,
    /// Settings it runs under, by their paths (`llm.sandbox-localhost`), as `settings::apply` takes them.
    pub settings: Vec<(String, toml::Value)>,
    /// Places outside the project it's expected to change (absolute, or `~/`): the rest of the watched places
    /// must stay as they were.
    pub writes: Vec<String>,
    /// Expected to be refused only until a design is built (`manifest-review`, D8): its own column.
    pub pending: Option<String>,
    /// Whether to time the same command unsandboxed in a copy of the project.
    pub baseline: bool,
    pub timeout: Duration,
    /// In the quick lane (`cargo xtask compat --lane quick`: build and test) as well as the gate.
    pub quick: bool,
    /// Tries the network: in the asks lane alone (`--lane asks`), never the gate's or quick's. Run with the
    /// network closed, its host cards are what's measured — not failures — and it has no baseline (unsandboxed
    /// it would reach the network).
    pub asks: bool,
}

/// A stack's entries: what it needs on PATH, what's done outside the sandbox first (as the user's own build
/// would), the commands. Reviewed code, kept in tarae's tree: `prewarm` and every entry's baseline run outside the
/// sandbox with the user's real HOME, so neither comes from a repository under test.
///
/// ```toml
/// stack = "node-npm"
/// pack = "javascript"
/// tools = ["node", "npm"]
/// fixture = "node-npm"                # compat/node-npm/, copied in as the project
/// prewarm = ["npm ci"]                # outside the sandbox, as the user's own build
/// watch = ["~/.npm"]                  # outside the project, checked for what it changes
/// writes = ["~/.npm/_logs"]           # what every entry may change there
/// [files]                             # written into the project (after the fixture's copy)
/// "package.json" = """{ … }"""
/// [[entry]]
/// name = "test"
/// run = "npm test"                    # never from the repository under test
/// expect = "pass"                     # or "refused", with said = […]
/// cards = ["run"]
/// settings = { llm = { sandbox-localhost = true } }
/// writes = ["~/.npm/_cacache"]        # outside the project, expected to change
/// # cwd, pending = "manifest-review", baseline = false, timeout = 600, quick = true, asks = true
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub stack: String,
    /// The language pack whose data the stack leans on.
    pub pack: Option<String>,
    pub tools: Vec<String>,
    /// A folder in `compat/` copied in as the project — None: an empty one.
    pub fixture: Option<String>,
    /// Files written into the project, each path in it with its text: a fixture kept in the manifest, so no
    /// `Cargo.toml` or `package.json` of a fixture's is in tarae's own tree for its tools to find.
    pub files: Vec<(String, String)>,
    pub prewarm: Vec<String>,
    /// Places outside the project watched for what the entries change (absolute, or `~/`): the toolchain's homes
    /// (§2.1) — none: a folder of the harness's own. On Linux `[linux]`'s, where it names them (`~/.cache/…` for
    /// `~/Library/Caches/…`).
    pub watch: Vec<String>,
    /// The image its Linux runs take, `compat/linux/<image>.Dockerfile` built on `rust:<pin>` by the xtask — None:
    /// `rust:<pin>` itself.
    pub image: Option<String>,
    pub entries: Vec<Entry>,
}

/// Where the manifests and their fixtures are, from the checkout's root.
pub const MANIFESTS: &str = "compat";

const MANIFEST_KEYS: [&str; 11] =
    ["stack", "pack", "tools", "fixture", "files", "prewarm", "watch", "writes", "entry", "image", "linux"];
/// `[linux]`'s: the places on Linux, in place of the manifest's own.
const LINUX_KEYS: [&str; 2] = ["watch", "writes"];
const ENTRY_KEYS: [&str; 10] =
    ["name", "run", "cwd", "expect", "said", "cards", "settings", "writes", "pending", "baseline"];
const ENTRY_KEYS_MORE: [&str; 3] = ["timeout", "quick", "asks"];

/// An entry's time limit when it names none.
const TIMEOUT: Duration = Duration::from_secs(600);

impl Manifest {
    /// A manifest from its TOML — an unknown key, a setting that isn't one, `said` without `refused` or `pending`
    /// on what's expected to pass refused, so a typo can't make an entry pass.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let t: toml::Table = text.parse().map_err(|e| format!("not TOML: {e}"))?;
        unknown(&t, &MANIFEST_KEYS, "the manifest")?;
        let stack = string(&t, "stack")?.ok_or("no stack")?;
        let mut entries: Vec<Entry> = match t.get("entry") {
            None => Vec::new(),
            Some(toml::Value::Array(a)) => a.iter().map(entry).collect::<Result<_, _>>()?,
            Some(_) => return Err("entry: an array of tables ([[entry]])".into()),
        };
        let linux = match t.get("linux") {
            None => toml::Table::new(),
            Some(toml::Value::Table(l)) => {
                unknown(l, &LINUX_KEYS, "[linux]")?;
                l.clone()
            }
            Some(_) => return Err("linux: a table ([linux])".into()),
        };
        // (both checked on every OS; Linux's taken there where it names them)
        let places = |key: &str| -> Result<Vec<String>, String> {
            let (own, on_linux) =
                (strings(&t, key)?, strings(&linux, key).map_err(|e| format!("[linux] {e}"))?);
            if let Some(w) = own.iter().chain(&on_linux).find(|w| !w.starts_with('/') && !w.starts_with("~/"))
            {
                return Err(format!("{key}: {w} — absolute or ~/"));
            }
            Ok(match cfg!(target_os = "linux") && linux.contains_key(key) {
                true => on_linux,
                false => own,
            })
        };
        // (what every entry may change outside the project: a toolchain's own lock or last-use files)
        let writes = places("writes")?;
        for e in &mut entries {
            e.writes.extend(writes.iter().cloned());
        }
        let mut names = BTreeSet::new();
        if let Some(e) = entries.iter().find(|e: &&Entry| !names.insert(e.name.clone())) {
            return Err(format!("entry {}: twice", e.name));
        }
        let fixture = string(&t, "fixture")?;
        if let Some(f) = fixture.as_deref().filter(|f| !below(f)) {
            return Err(format!("fixture: {f} — a folder in {MANIFESTS}/"));
        }
        let files = match t.get("files") {
            None => Vec::new(),
            Some(toml::Value::Table(f)) => f
                .iter()
                .map(|(p, v)| match v.as_str() {
                    Some(_) if !below(p) || p.split('/').next() == Some(".git") => {
                        Err(format!("files: {p} — a path in the project, not in .git"))
                    }
                    Some(text) => Ok((p.clone(), text.to_string())),
                    None => Err(format!("files: {p} — its text, a string")),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("files: a table of paths and their text".into()),
        };
        let watch = places("watch")?;
        let image = string(&t, "image")?;
        if let Some(i) = image.as_deref().filter(|i| {
            i.is_empty() || !i.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }) {
            return Err(format!("image: {i} — a recipe's name in {MANIFESTS}/linux/ (a-z, 0-9, -)"));
        }
        Ok(Manifest {
            stack,
            pack: string(&t, "pack")?,
            tools: strings(&t, "tools")?,
            fixture,
            files,
            prewarm: strings(&t, "prewarm")?,
            watch,
            image,
            entries,
        })
    }
}

fn entry(v: &toml::Value) -> Result<Entry, String> {
    let t = v.as_table().ok_or("entry: a table")?;
    let name = string(t, "name")?.ok_or("an entry without a name")?;
    let at = |e: String| format!("entry {name}: {e}");
    let keys: Vec<&str> = ENTRY_KEYS.iter().chain(&ENTRY_KEYS_MORE).copied().collect();
    unknown(t, &keys, "it").map_err(at)?;
    let run = string(t, "run").map_err(at)?.ok_or_else(|| at("no run".into()))?;
    let said = strings(t, "said").map_err(at)?;
    let expect = match string(t, "expect").map_err(at)?.as_deref() {
        Some("pass") if said.is_empty() => Expect::Pass,
        Some("pass") => return Err(at("said is for what's refused".into())),
        Some("refused") => Expect::Refused(said),
        Some(other) => return Err(at(format!("expect: pass or refused, not {other}"))),
        None => return Err(at("no expect".into())),
    };
    let pending = string(t, "pending").map_err(at)?;
    if pending.is_some() && expect == Expect::Pass {
        return Err(at("pending: refused until it's built, so expect refused".into()));
    }
    let mut settings = Vec::new();
    if let Some(s) = t.get("settings") {
        let s = s.as_table().ok_or_else(|| at("settings: a table".into()))?;
        flatten(s, "", &mut settings);
    }
    let mut cfg = crate::config::EditorConfig::default();
    for (path, v) in &settings {
        crate::settings::apply(&mut cfg, path, v).map_err(at)?;
    }
    let writes = strings(t, "writes").map_err(at)?;
    if let Some(w) = writes.iter().find(|w| !w.starts_with('/') && !w.starts_with("~/")) {
        return Err(at(format!("writes: {w} — absolute or ~/")));
    }
    let asks = match t.get("asks") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| at("asks: true or false".into()))?,
    };
    let baseline = match t.get("baseline") {
        None => !asks,
        Some(v) => v.as_bool().ok_or_else(|| at("baseline: true or false".into()))?,
    };
    if asks && baseline {
        return Err(at("baseline: none for asks — unsandboxed it would reach the network".into()));
    }
    let timeout = match t.get("timeout") {
        None => TIMEOUT,
        Some(v) => v
            .as_integer()
            .and_then(|n| u64::try_from(n).ok())
            .map(Duration::from_secs)
            .ok_or_else(|| at("timeout: seconds".into()))?,
    };
    let quick = match t.get("quick") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| at("quick: true or false".into()))?,
    };
    if asks && quick {
        return Err(at("quick: an asks entry is in the asks lane alone".into()));
    }
    Ok(Entry {
        cwd: string(t, "cwd").map_err(at)?,
        cards: strings(t, "cards").map_err(at)?,
        name,
        run,
        expect,
        settings,
        writes,
        pending,
        baseline,
        timeout,
        quick,
        asks,
    })
}

/// `[settings]`'s values by their dotted paths — `llm.sandbox-localhost = true` and `"llm.sandbox-localhost" =
/// true` alike.
fn flatten(t: &toml::Table, prefix: &str, out: &mut Vec<(String, toml::Value)>) {
    for (k, v) in t {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v.as_table() {
            Some(inner) if crate::settings::find(&path).is_none() => flatten(inner, &path, out),
            _ => out.push((path, v.clone())),
        }
    }
}

/// Whether `p` is a relative path that stays below where it's joined: no `..`, no root, not empty.
fn below(p: &str) -> bool {
    !p.is_empty() && Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn unknown(t: &toml::Table, known: &[&str], what: &str) -> Result<(), String> {
    match t.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(format!("{what} has no key {k}")),
        None => Ok(()),
    }
}

fn string(t: &toml::Table, key: &str) -> Result<Option<String>, String> {
    match t.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{key}: a string")),
    }
}

fn strings(t: &toml::Table, key: &str) -> Result<Vec<String>, String> {
    match t.get(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(a)) => {
            a.iter().map(|v| v.as_str().map(String::from).ok_or(format!("{key}: strings"))).collect()
        }
        Some(_) => Err(format!("{key}: an array of strings")),
    }
}

/// Where an entry runs: its project (a fixture written, or a checkout), the places watched for what it changes
/// outside it (home in a real run; the test's own folder here), and a folder of the harness's own for the
/// baseline's copy — outside the watched places.
pub struct Place {
    pub project: PathBuf,
    pub watched: Vec<PathBuf>,
    pub scratch: PathBuf,
}

/// Which check an entry failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// How it ended against what was expected (or that it never ran).
    Exit,
    /// What its report says: a denial on a pass, a failure that names no cause, a refusal without its words.
    Report,
    /// A card nobody listed.
    Card,
    /// A change outside the project nobody listed.
    Writes,
    /// A protected name it made (`snapshot::Made`).
    Made,
    /// It didn't run in the sandbox.
    Sandbox,
    /// Not over within its time.
    Timeout,
    /// It failed where a pass was expected, and fails unsandboxed too: the tool's own, not the sandbox's (§5
    /// `not-ours`).
    Outside,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub check: Check,
    pub what: String,
}

impl Failure {
    /// Its class (`target/sb/compat-plan.md` §5) as far as the check tells it — the rest `unexplained`, for a
    /// person (a denial's place would tell `pack-data` from `core-rule`: piece 3's findings first).
    pub fn class(&self) -> &'static str {
        match self.check {
            Check::Report if self.what == NO_CAUSE => "report",
            Check::Timeout => "hang",
            Check::Card => "card",
            Check::Made => "protected-name",
            Check::Sandbox => "not-sandboxed",
            Check::Writes => "writes",
            Check::Outside => "not-ours",
            Check::Exit | Check::Report => "unexplained",
        }
    }
}

/// The classes by rank: a cell names its entry's first. A failure with no cause said ranks first (§5).
const CLASSES: [&str; 8] =
    ["report", "hang", "card", "protected-name", "not-sandboxed", "writes", "not-ours", "unexplained"];

const NO_CAUSE: &str = "it failed, and the report names no cause";

/// How an entry went.
#[derive(Clone, Debug)]
pub struct Verdict {
    pub entry: String,
    pub pending: Option<String>,
    /// How it ended — None: it never ran.
    pub ended: Option<crate::task::State>,
    /// Where it ran, as its chat row says it (`Guard::where_ran`: `sandbox`, `sandbox own`, `outside` …).
    pub ran: Option<String>,
    /// The run's own time (its second run's, warm, where one was timed), and the same command's unsandboxed.
    pub took: Option<Duration>,
    pub baseline: Option<Duration>,
    /// The first run's time, where a second was timed.
    pub cold: Option<Duration>,
    /// What Claude would have read.
    pub answer: String,
    /// Each card it raised, as `kind why → answer`.
    pub cards: Vec<String>,
    pub failed: Vec<Failure>,
    /// The protected names the snapshot found it made, its expected end on Linux (`judge_made`): `⊘ found`.
    pub found: Vec<String>,
    /// An asks entry's host cards, how many came — None: not an asks entry.
    pub asked: Option<usize>,
}

impl Verdict {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }

    /// Past the time it's allowed over its unsandboxed run (`slow`) — flagged, never failed (D9).
    pub fn slow(&self) -> bool {
        self.took.zip(self.baseline).is_some_and(|(t, b)| slow(t, b))
    }

    fn fail(&mut self, check: Check, what: impl Into<String>) {
        self.failed.push(Failure { check, what: what.into() });
    }

    /// Its cell in the table (§5): `✓`, `✓ 1.7×` (slow), `⊘` (refused as expected), `✗ class` (the first by
    /// `CLASSES`), `● 2 hosts` (an asks entry's host cards).
    pub fn cell(&self) -> String {
        let rank = |c: &&str| CLASSES.iter().position(|k| k == c);
        if let Some(class) = self.failed.iter().map(Failure::class).min_by_key(rank) {
            return format!("✗ {class}");
        }
        if let Some(n) = self.asked.filter(|n| *n > 0) {
            return format!("● {n} host{}", if n == 1 { "" } else { "s" });
        }
        if !self.found.is_empty() {
            return "⊘ found".into();
        }
        if self.ended != Some(crate::task::State::Passed) {
            return "⊘".into();
        }
        match self.took.zip(self.baseline) {
            Some((t, b)) if self.slow() => format!("✓ {:.1}×", t.as_secs_f64() / b.as_secs_f64().max(0.001)),
            _ => "✓".into(),
        }
    }
}

/// Slow: half again its unsandboxed time and at least `SLOW_FLOOR` more (a 10 ms command's sandbox start is
/// several times it, and nothing a person waits for), or `SLOW_PAST` more whatever its time.
pub fn slow(took: Duration, baseline: Duration) -> bool {
    let over = took.saturating_sub(baseline);
    (took.as_secs_f64() > baseline.as_secs_f64() * 1.5 && over >= SLOW_FLOOR) || over >= SLOW_PAST
}

const SLOW_FLOOR: Duration = Duration::from_millis(500);
const SLOW_PAST: Duration = Duration::from_secs(5);

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ended = match &self.ended {
            None => "didn't run".to_string(),
            Some(crate::task::State::Passed) => "exit 0".into(),
            Some(crate::task::State::Failed(Some(n))) => format!("exit {n}"),
            Some(s) => format!("{s:?}").to_lowercase(),
        };
        let secs = |d: Duration| crate::term::duration(d.as_secs_f32());
        let cold = self.cold.map(|c| format!(" warm, {} first", secs(c))).unwrap_or_default();
        let took = match (self.took, self.baseline) {
            (Some(t), Some(b)) => {
                let slow = if self.slow() { ", slow" } else { "" };
                format!(" in {}{cold} (unsandboxed {}{slow})", secs(t), secs(b))
            }
            (Some(t), None) => format!(" in {}", secs(t)),
            _ => String::new(),
        };
        let ran = self.ran.as_deref().map(|r| format!(", {r}")).unwrap_or_default();
        let verdict = match (self.ok(), &self.pending) {
            (true, _) if let Some(n) = self.asked.filter(|n| *n > 0) => {
                format!("asked {n} host card{}", if n == 1 { "" } else { "s" })
            }
            (true, _) if !self.found.is_empty() => {
                format!("made {}, found by the snapshot as expected on Linux", self.found.join(", "))
            }
            (true, Some(p)) => format!("refused as expected, pending {p}"),
            (true, None) => "ok".into(),
            (false, _) => format!("FAILED ({})", self.failed.len()),
        };
        writeln!(f, "{}: {verdict} — {ended}{took}{ran}", self.entry)?;
        for c in &self.cards {
            writeln!(f, "  card {c}")?;
        }
        for x in &self.failed {
            writeln!(f, "  {:?}: {}", x.check, x.what)?;
        }
        Ok(())
    }
}

/// The editor as tarae ships it, where `cfg(test)` changes the run path (`BRANCHES`): the sandbox looked for
/// (tests bypass it) and the look landed — as claude's `tools/list` starts it (`tools::sandbox_look`) before its
/// first command; the stand-in chat lists no tools, and while it's `Unknown` every run takes today's gate and
/// its card (`tools::sandboxed_here`) —, the temp folders read unasked, and a policy made from the whole environment — tests'
/// `Here::now` drops the proxy variables. What stays a test's: no stored grants, trust or tokens told (an entry
/// says its settings), runs' folders in the temp folder (`Here::runs`).
pub fn as_shipped(ed: &mut Editor) {
    ed.sandbox = crate::sandbox::probe();
    ed.read_temp = true;
    ed.shellcheck = crate::readonly::Shellcheck::of_process();
    ed.sandbox_here = Some(std::sync::Arc::new(|| {
        let env = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v)));
        crate::sandbox::Here { env: env.collect(), ..crate::sandbox::Here::now() }
    }));
}

/// Whether runs go in the sandbox here — else the test says it skips itself: inside a sandbox already
/// (Claude's `cargo test`: Seatbelt can't start), or none on this system.
pub fn sandboxed_here(test: &str) -> bool {
    #[cfg(target_os = "macos")]
    if crate::sandbox::skipped_inside(test) {
        return false;
    }
    match crate::sandbox::probe() {
        crate::sandbox::Availability::Ready | crate::sandbox::Availability::Partial(_) => true,
        found => {
            let line = format!("{}{test} — no sandbox here ({found:?})\n", crate::test_results::SELF_SKIP);
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            false
        }
    }
}

/// Runs `entry` at `at` as Claude's run, and judges it.
pub fn run(entry: &Entry, at: &Place) -> Verdict {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut v = Verdict {
        entry: entry.name.clone(),
        pending: entry.pending.clone(),
        ended: None,
        ran: None,
        took: None,
        baseline: None,
        cold: None,
        answer: String::new(),
        cards: Vec::new(),
        failed: Vec::new(),
        found: Vec::new(),
        asked: entry.asks.then_some(0),
    };
    // (no checkout, no snapshot: a protected name Landlock let it make would pass unseen)
    if !at.project.join(".git").exists() {
        v.fail(Check::Made, "the project isn't a git checkout (`checkout`): what a run makes can't be seen");
        return v;
    }
    let mut ed = Editor::with_root(crate::config::Config::default(), at.project.clone());
    as_shipped(&mut ed);
    // (the card record kept as tarae keeps it, in the harness's folder rather than tarae's state)
    ed.cards_dir = Some(at.scratch.join(crate::cards::DIR));
    for (path, value) in &entry.settings {
        // (checked when the manifest was read)
        let _ = crate::settings::apply(&mut ed.config, path, value);
    }
    // (the chat its call is of: a stand-in that reads what it's sent and says nothing)
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let Some(c) = ed.session.chat.as_mut() else {
        v.fail(Check::Exit, "the chat didn't open");
        return v;
    };
    c.state = crate::chat::State::Thinking;
    c.focused = false;
    let id = json!(format!("compat-{n}"));
    let call = crate::tools::Call::of(c.generation(), &format!("compat{n}"), id.clone());
    // (what it's let change outside the project, and what's the harness's own)
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut allowed: Vec<PathBuf> = entry.writes.iter().filter_map(|w| place(w, home.as_deref())).collect();
    allowed.extend([at.project.clone(), at.scratch.clone()]);
    let before = Tree::of(&at.watched, &allowed);
    let copy = entry.baseline.then(|| at.scratch.join(format!("baseline-{n}")));
    if let Some(c) = &copy
        && let Err(e) = copy_tree(&at.project, c)
    {
        v.fail(Check::Exit, format!("the project couldn't be copied for its baseline: {e}"));
        return v;
    }
    let mut args = json!({ "command": entry.run });
    if let Some(cwd) = &entry.cwd {
        args["cwd"] = json!(cwd);
    }
    // (the proxy's questions go to a test's sender (`TEST_ASKS`): handed to the editor as its main loop takes
    // them, so each host card comes as it would — piece C, the network closed: each host asked)
    let asks = {
        let session = ed.session.proxy_id();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut to = crate::sandbox::proxy::TEST_ASKS.lock().unwrap_or_else(|e| e.into_inner());
        to.retain(|(s, _)| *s != session);
        to.push((session, tx));
        rx
    };
    let answer = match crate::tools::run(&mut ed, call, &args) {
        Err(e) => Some(Err(e)),
        Ok(()) => answered_by_script(&mut ed, entry, &id, &asks, &mut v),
    };
    // (the record's lines written before it's judged, as tarae waits for them at its end)
    ed.cards_flush();
    // (each entry its own session at the proxy: a host refused for the session (`n`) is refused at once, with no
    // card, for the rest of it — the next entry, in the same project, is asked afresh)
    crate::hosts::session_ended(&ed.session);
    let run = ed.session.task_run.as_ref().filter(|r| r.by_claude && r.task.cmd == entry.run);
    match answer {
        None => {
            let tail = run.map(|r| tail(&r.output)).unwrap_or_default();
            v.fail(
                Check::Timeout,
                format!("not over in {}: {tail}", crate::term::duration(entry.timeout.as_secs_f32())),
            );
            ed.task_close();
            return v;
        }
        Some(Ok(text)) | Some(Err(text)) => v.answer = text,
    }
    let Some(r) = run.filter(|r| r.state != crate::task::State::Running) else {
        v.fail(Check::Exit, format!("it didn't run: {}", v.answer.lines().next().unwrap_or_default()));
        return v;
    };
    v.ended = Some(r.state.clone());
    v.took = r.took;
    judge_report(&mut v, entry, r);
    // (an asks entry tries the network: its host cards, answered `n` or left, are what's measured — the exit
    // they caused isn't its failure; a report that names no cause still is)
    if v.asked.is_some_and(|n| n > 0) {
        v.failed.retain(|f| f.check != Check::Exit);
    }
    let made: Vec<String> = ed.session.made_protected.iter().map(|m| m.path.clone()).collect();
    judge_made(&mut v, entry, &made, cfg!(target_os = "linux"));
    // (before the baseline: what the same command writes outside the sandbox is the user's own build's)
    let after = Tree::of(&at.watched, &allowed);
    for (path, how) in before.changes(&after) {
        v.fail(
            Check::Writes,
            format!("{how} {} — outside the project, and not in its writes", path.display()),
        );
    }
    // (§5 `not-ours`: a pass expected that failed, and fails unsandboxed too in the copy made before it — the
    // tool's own: its exit, and the report's silence on it, aren't the sandbox's; what the sandbox blocked still is)
    if let Some(c) = &copy
        && entry.expect == Expect::Pass
        && v.ended.as_ref().is_some_and(|s| *s != crate::task::State::Passed)
        && let Some(how) = fails_outside(&entry.run, &c.join(entry.cwd.as_deref().unwrap_or("")))
    {
        v.failed.retain(|f| f.check != Check::Exit && !(f.check == Check::Report && f.what == NO_CAUSE));
        v.fail(Check::Outside, format!("it fails unsandboxed too — {how}"));
    }
    // (only an entry expected to pass is timed: one expected refused that ran — `⊘ found` on Linux — made its name
    // the first time, and its second run, finding nothing new, would say "it succeeded" again)
    if let Some(c) = &copy
        && v.ended == Some(crate::task::State::Passed)
        && entry.expect == Expect::Pass
    {
        // (timed warm, both sides — §2.1, D9: a project's first run in the sandbox fills the run homes kept for
        // it, the user's own caches are warm already; how the second run ended and what it was denied are as
        // real as the first's — what it changes outside, the first's again, was judged)
        let warm = crate::compat::run(&Entry { baseline: false, ..entry.clone() }, at);
        if warm.ended == Some(crate::task::State::Passed) {
            v.cold = v.took;
            v.took = warm.took;
        }
        let again: Vec<Failure> = (warm.failed.into_iter())
            .filter(|f| matches!(f.check, Check::Exit | Check::Report) && !v.failed.contains(f))
            .collect();
        for f in again {
            v.fail(f.check, format!("(its second run) {}", f.what));
        }
        v.baseline = baseline(&entry.run, &c.join(entry.cwd.as_deref().unwrap_or("")));
    }
    v
}

/// Events pumped until `id` is answered (None: not within the entry's time), the scripted user answering each
/// card once as it comes to the front — a host's card too (`asks`: the proxy's questions), but with
/// `TARAE_COMPAT_HOSTS=wait` a host card unlisted is left unanswered, as a user away would: the proxy holds the
/// connection its time (`Limits::hold`), then refuses it. Each host card says when it came and how many times it
/// was asked (the tool's retries join it).
fn answered_by_script(
    ed: &mut Editor,
    entry: &Entry,
    id: &serde_json::Value,
    asks: &std::sync::mpsc::Receiver<crate::sandbox::proxy::FromProxy>,
    v: &mut Verdict,
) -> Option<Result<String, String>> {
    let answered =
        || crate::tools::ANSWERED.with(|a| a.borrow().iter().find(|(i, _)| i == id).map(|(_, r)| r.clone()));
    let start = Instant::now();
    let end = start + entry.timeout;
    let wait = std::env::var("TARAE_COMPAT_HOSTS").is_ok_and(|w| w == "wait");
    let mut seen = BTreeSet::new();
    // (each host card's words by its `host:port`, and the questions the proxy asked about each)
    let (mut hosts, mut questions) = (Vec::<(String, String)>::new(), BTreeMap::<String, usize>::new());
    let host_cards = |v: &mut Verdict, hosts: &[(String, String)], questions: &BTreeMap<String, usize>| {
        for (at, said) in hosts {
            v.cards.push(format!("{said} (asked {}×)", questions.get(at).copied().unwrap_or(0)));
        }
    };
    loop {
        for a in asks.try_iter() {
            let crate::sandbox::proxy::FromProxy::Ask { host, port, .. } = &a;
            *questions.entry(format!("{host}:{port}")).or_default() += 1;
            crate::hosts::asked(ed, a);
        }
        if let Some(r) = answered() {
            host_cards(v, &hosts, &questions);
            return Some(r);
        }
        if Instant::now() > end {
            host_cards(v, &hosts, &questions);
            return None;
        }
        if let Some(o) = ed.offers.first().filter(|o| !o.installing() && !seen.contains(&o.id)) {
            seen.insert(o.id);
            let kind = o.kind();
            let said = match &o.asked {
                Some(a) => format!("{kind} {}", a.why.word()),
                None => kind.to_string(),
            };
            let listed = entry.cards.iter().any(|c| c == kind || *c == said);
            let host = o.host().map(|a| {
                let after = crate::term::duration(start.elapsed().as_secs_f32());
                let at = format!("{}:{}", a.host, a.port);
                (at.clone(), format!("{said} {at} by {}, after {after}", a.process))
            });
            let answer = match (listed, &host) {
                (true, _) => {
                    ed.offer_accept();
                    "y"
                }
                (false, Some(_)) if wait => "left unanswered",
                (false, _) => {
                    ed.offer_dismiss();
                    "n"
                }
            };
            // (an asks entry's host cards are what it measures)
            if !listed && !(entry.asks && host.is_some()) {
                v.fail(Check::Card, format!("{said} — nobody listed it"));
            }
            if let (Some(n), Some(_)) = (v.asked.as_mut(), &host) {
                *n += 1;
            }
            match host {
                Some((at, h)) => hosts.push((at, format!("{h} → {answer}"))),
                None => v.cards.push(format!("{said} → {answer}")),
            }
            continue;
        }
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
            ed.handle_event(ev);
        }
    }
}

/// The report against what was expected: how it ended, where it ran, what it was refused.
fn judge_report(v: &mut Verdict, entry: &Entry, r: &crate::task::Run) {
    let Some(g) = r.guard.as_ref() else {
        v.fail(Check::Sandbox, "it ran with no guard at all");
        return;
    };
    let (mark, _, words) = g.where_ran();
    v.ran = Some(mark.clone());
    if !mark.starts_with("sandbox") {
        v.fail(Check::Sandbox, words);
    }
    let passed = r.state == crate::task::State::Passed;
    let report = g.report_with(&r.output, crate::sandbox::Told::default(), passed);
    let seen = g.watch().map(|w| w.blocked().seen).unwrap_or_default();
    let said = match g {
        crate::sandbox::Guard::In(p) => p.said_in_output(&r.output, &seen),
        _ => Vec::new(),
    };
    // (in Seatbelt's words too where the plain ones name no target: `opening apps` is a mach service; an asks
    // entry's hosts refused on their cards are what it measures; what every run is refused by design is never an
    // entry's cause — the tools go on without it, macdeny measured)
    let denials: Vec<String> = seen
        .iter()
        .chain(&said)
        .filter(|d| !(entry.asks && d.op == "network-host") && !d.by_design())
        .map(|d| match d.plain() == d.shown() {
            true => d.reported(),
            false => format!("{} — {}", d.reported(), d.shown()),
        })
        .collect();
    match &entry.expect {
        Expect::Pass => {
            if !passed {
                v.fail(Check::Exit, format!("{:?}: {}", r.state, tail(&r.output)));
            }
            if report.blocked {
                // (a line each: one line of them all is cut where it's shown)
                for d in denials.iter().take(DENIALS_SAID) {
                    v.fail(Check::Report, format!("the sandbox blocked: {d}"));
                }
                if denials.len() > DENIALS_SAID {
                    let more = denials.len() - DENIALS_SAID;
                    v.fail(Check::Report, format!("the sandbox blocked {more} more"));
                }
            } else if !passed {
                v.fail(Check::Report, NO_CAUSE);
            }
        }
        Expect::Refused(words) => {
            if passed {
                v.fail(Check::Exit, "it succeeded");
            } else if !report.blocked {
                v.fail(Check::Report, NO_CAUSE);
            }
            // (its words where it says what happened — not the rules, which name every protected name)
            let said = report.text.replace(&g.rules(), "");
            for w in words.iter().filter(|w| !said.contains(w.as_str())) {
                v.fail(Check::Report, format!("the report doesn't say {w}"));
            }
        }
    }
}

/// The protected names the snapshot found the run made (`made`) against what was expected. Under Landlock
/// (`landlock`) a name made where none was can't be refused (G23): the snapshot finds it after the run and its
/// card (`ProtectedMade`) stands before what runs there next. So an entry expected refused that ran and made only
/// names it says (`said`: the path or its file name), each found, ended as expected there — `⊘ found`, its "it succeeded" and
/// "the report doesn't say" taken back. Anything else made is a failure: on macOS, or by an entry expected to pass.
fn judge_made(v: &mut Verdict, entry: &Entry, made: &[String], landlock: bool) {
    let named = |m: &String, words: &[String]| {
        let name = Path::new(m).file_name().and_then(|n| n.to_str()).unwrap_or(m);
        words.iter().any(|w| w == m || w == name)
    };
    match &entry.expect {
        Expect::Refused(words)
            if landlock
                && v.ended == Some(crate::task::State::Passed)
                && !made.is_empty()
                && made.iter().all(|m| named(m, words)) =>
        {
            v.failed.retain(|f| match f.check {
                Check::Exit => f.what != "it succeeded",
                Check::Report => !f.what.starts_with("the report doesn't say "),
                _ => true,
            });
            v.found = made.to_vec();
        }
        _ => {
            for m in made {
                v.fail(Check::Made, format!("it made {m}, a protected name"));
            }
        }
    }
}

/// The distinct denials a passing entry's verdict names, a line each.
const DENIALS_SAID: usize = 12;

/// The same command unsandboxed, in the copy made before the run, from `dir` — its time, when it ran: run twice,
/// the second timed (warm, as the sandboxed run it's against).
fn baseline(cmd: &str, dir: &Path) -> Option<Duration> {
    let once = || {
        let mut c = crate::nofile::command("sh", Some(dir));
        c.args(["-c", cmd]).stdin(std::process::Stdio::null());
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let at = Instant::now();
        c.status().ok().map(|_| at.elapsed())
    };
    once()?;
    once()
}

/// The same command unsandboxed, once, in the copy made before the run, from `dir` — how it failed (`exit 3:
/// its last lines`); None when it passed or couldn't start.
fn fails_outside(cmd: &str, dir: &Path) -> Option<String> {
    let mut c = crate::nofile::command("sh", Some(dir));
    c.args(["-c", cmd]).stdin(std::process::Stdio::null());
    let out = c.output().ok()?;
    if out.status.success() {
        return None;
    }
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let lines: Vec<(bool, String)> = text.lines().map(|l| (false, l.to_string())).collect();
    let code = out.status.code().map_or("a signal".to_string(), |n| format!("exit {n}"));
    Some(format!("{code}: {}", tail(&lines)))
}

/// The last lines of a run's output, on one line.
fn tail(output: &[(bool, String)]) -> String {
    let last: Vec<&str> = output.iter().rev().take(3).map(|(_, l)| l.as_str()).collect();
    last.into_iter().rev().collect::<Vec<_>>().join(" / ")
}

/// `writes`' place: absolute, or `~/` in `home`.
fn place(w: &str, home: Option<&Path>) -> Option<PathBuf> {
    match w.strip_prefix("~/") {
        Some(rest) => home.map(|h| h.join(rest)),
        None => Some(PathBuf::from(w)),
    }
}

/// What's under some folders, each entry's size, time and kind — links not followed, `skip` (and what's in it)
/// left out, at most `TREE_MAX` entries.
#[derive(Debug, Default, PartialEq)]
pub struct Tree {
    entries: BTreeMap<PathBuf, (u64, Option<SystemTime>, char)>,
    /// Past `TREE_MAX`: what wasn't looked at can't be told.
    pub cut: bool,
}

const TREE_MAX: usize = 200_000;

impl Tree {
    pub fn of(roots: &[PathBuf], skip: &[PathBuf]) -> Tree {
        let mut t = Tree::default();
        let mut next: Vec<PathBuf> = roots.to_vec();
        while let Some(dir) = next.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let path = e.path();
                if skip.iter().any(|s| path.starts_with(s)) {
                    continue;
                }
                let Ok(m) = std::fs::symlink_metadata(&path) else { continue };
                if t.entries.len() >= TREE_MAX {
                    t.cut = true;
                    return t;
                }
                let kind = if m.is_dir() {
                    'd'
                } else if m.is_symlink() {
                    'l'
                } else {
                    'f'
                };
                if m.is_dir() {
                    next.push(path.clone());
                }
                // (a folder's time changes with what's in it: what's in it is said instead)
                let time = if m.is_dir() { None } else { m.modified().ok() };
                t.entries.insert(path, (if m.is_dir() { 0 } else { m.len() }, time, kind));
            }
        }
        t
    }

    /// What changed from `self` to `after`: each path with how — `made`, `changed`, `removed` — and that it
    /// couldn't all be looked at.
    pub fn changes(&self, after: &Tree) -> Vec<(PathBuf, &'static str)> {
        let mut out: Vec<(PathBuf, &'static str)> = Vec::new();
        for (p, a) in &after.entries {
            match self.entries.get(p) {
                None => out.push((p.clone(), "made")),
                Some(b) if b != a => out.push((p.clone(), "changed")),
                Some(_) => {}
            }
        }
        out.extend(
            self.entries.keys().filter(|p| !after.entries.contains_key(*p)).map(|p| (p.clone(), "removed")),
        );
        if self.cut || after.cut {
            out.push((PathBuf::from(format!("(past {TREE_MAX} entries)")), "not looked at:"));
        }
        out
    }
}

/// A fixture's project made a git checkout of its own (one there already is kept), what's in it committed — so
/// the snapshot runs after each run as it does in Claude's checkouts: Landlock can't refuse a protected name
/// made where none was (G23), only the snapshot finds it (`Session.made_protected`). git with no config but its
/// own, no hooks. Never a repository under test: its clone's history is its own.
pub fn checkout(project: &Path) -> Result<(), String> {
    // (git's repository variables from where the tests run — a hook, a git-driven script — would point it at
    // another repository, tarae's own included: none, and no climbing past the project)
    const REPOSITORY_VARIABLES: [&str; 6] = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
    ];
    let above = project.parent().unwrap_or(project);
    let git = |args: &[&str]| {
        crate::gitrepo::not_in_this_repository(project, args);
        let mut c = std::process::Command::new("git");
        for v in REPOSITORY_VARIABLES {
            c.env_remove(v);
        }
        let out = c
            .args([
                "-c",
                "user.name=compat",
                "-c",
                "user.email=compat@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(project)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", above)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("git: {e}"))?;
        match out.status.success() {
            true => Ok(()),
            false => Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())),
        }
    };
    if !project.join(".git").exists() {
        git(&["init", "-q"])?;
    }
    git(&["add", "-A"])?;
    git(&["commit", "-q", "--allow-empty", "--no-verify", "-m", "the fixture"])
}

/// `from`'s tree copied to `to` (made), links as links.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let m = std::fs::symlink_metadata(&src)?;
        if m.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if m.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// The files of the run path: from Claude's `run` to its answer — the card, the queue, the task, the policy and
/// its enforcement, the report, the snapshot. A `cfg(test)` in them (but in a test module) is in `BRANCHES`.
const RUN_PATH: [&str; 13] = [
    "src/tools.rs",
    "src/queued.rs",
    "src/task.rs",
    "src/snapshot.rs",
    "src/readonly.rs",
    "src/allow.rs",
    "src/blocked.rs",
    "src/cardwarn.rs",
    "src/cards.rs",
    "src/tokens.rs",
    "src/spawn.rs",
    "src/nofile.rs",
    "src/sandbox.rs",
];

/// Every `cfg(test)` on the run path (`RUN_PATH`, `sandbox/` whole, the editor's `with_root`), by file and the
/// item it's on or in — and what compat does about it. A new one fails `every_test_branch_on_the_run_path_is_listed`
/// until it's here: compat runs the test build, so a branch it doesn't know of would change what it measures.
const BRANCHES: &[(&str, &str, &str)] = &[
    ("src/cards.rs", r#"in fn logged: LOGGED.with(|l| l.borrow_mut().push(format!("{leve"#, RECORD),
    (
        "src/editor.rs",
        "in fn with_root: allow_path",
        "stays off: no stored grants — an entry says its settings",
    ),
    ("src/editor.rs", "in fn with_root: api_key_query", NOT_RUNS),
    (
        "src/editor.rs",
        "in fn with_root: base_trees",
        "a commit's tree's folder in temp (`run`'s `base`): another folder",
    ),
    ("src/editor.rs", "in fn with_root: billing_kept", NOT_RUNS),
    ("src/editor.rs", "in fn with_root: builds", "new sessions' checkouts: compat runs in one"),
    (
        "src/editor.rs",
        "in fn with_root: cards_dir",
        "given (`run`): the card record kept in the harness's folder, a line per card or run refused",
    ),
    (
        "src/editor.rs",
        "in fn with_root: calls_dir",
        "stays off: no call-order record written — a turn's tool names, never read by a run",
    ),
    ("src/editor.rs", "in fn with_root: caps_home", HOOK),
    ("src/editor.rs", "in fn with_root: chats_dir", NOT_RUNS),
    ("src/editor.rs", "in fn with_root: claude_env", CLAUDES),
    ("src/editor.rs", "in fn with_root: claude_settings_gate", CLAUDES),
    ("src/editor.rs", "in fn with_root: claude_update", CLAUDES),
    ("src/editor.rs", "in fn with_root: claude_user_settings", CLAUDES),
    (
        "src/editor.rs",
        "in fn with_root: config_file",
        "where `:set` writes: an entry's settings are applied in memory",
    ),
    ("src/editor.rs", "in fn with_root: dir", "new sessions' checkouts: compat runs in one"),
    ("src/editor.rs", "in fn with_root: features", NOT_RUNS),
    ("src/editor.rs", "in fn with_root: git_auto", NOT_RUNS),
    (
        "src/editor.rs",
        "in fn with_root: offer_popups",
        "grammar and install offers: kept off, nothing offered for download",
    ),
    (
        "src/editor.rs",
        "in fn with_root: read_temp",
        "turned on (`as_shipped`): reads of the temp folders go unasked",
    ),
    (
        "src/editor.rs",
        "in fn with_root: read_outside",
        "not a run's: only Claude's Read and tarae's read and search tools go by it (`reads::Gate`)",
    ),
    ("src/editor.rs", "in fn with_root: recovery", NOT_RUNS),
    (
        "src/editor.rs",
        "in fn with_root: sandbox",
        "looked for (`as_shipped`, `sandbox::probe`): tests bypass the sandbox",
    ),
    (
        "src/editor.rs",
        "in fn with_root: shellcheck",
        "turned on (`as_shipped`): home's shellcheck rc files and SHELLCHECK_OPTS as the process has them",
    ),
    ("src/editor.rs", "in fn with_root: sizes", NOT_RUNS),
    ("src/editor.rs", "in fn with_root: sizes_path", NOT_RUNS),
    ("src/editor.rs", "in fn with_root: tokens_said", "stays off: token holders told of again at each run"),
    (
        "src/editor.rs",
        "in fn with_root: trust_path",
        "stays off: no trust store — an entry's project has no .tarae.toml",
    ),
    (
        "src/editor.rs",
        "in fn with_root: turns_dir",
        "checkpoints kept in memory only: the snapshot still runs",
    ),
    ("src/sandbox.rs", "fn asked: a test build's twin", ASKED),
    ("src/sandbox.rs", "in fn now: asked", ASKED),
    ("src/sandbox.rs", "in fn now: env", "drops the proxy variables: `as_shipped`'s `Here` takes them all"),
    ("src/sandbox.rs", "in fn now: runs", RUNS),
    ("src/sandbox.rs", "in fn run_folder: (given, _) = (false, here);", RUNS),
    ("src/sandbox.rs", "in fn run_folder: given", RUNS),
    ("src/sandbox.rs", "in fn runs_root: if let Some(runs) = &here.runs {", RUNS),
    ("src/sandbox.rs", "in fn spawn_as: held", HOOK),
    ("src/sandbox.rs", "in fn spawn_as: if let Some(held) = held {", HOOK),
    ("src/sandbox/keeper.rs", "in fn child: if exec.leak {", SEAM),
    (
        "src/sandbox/keeper.rs",
        "in fn child: tests",
        "a flag checked in the forked child (no allocation): the same code",
    ),
    ("src/sandbox/keeper.rs", "in fn heard: Msg", RECORD),
    ("src/sandbox/keeper.rs", "in fn heard: s.exits.push(status);", RECORD),
    (
        "src/sandbox/keeper.rs",
        "in fn keeper_argv: args",
        "the keeper is the test binary run again: the same code, another entry",
    ),
    (
        "src/sandbox/keeper.rs",
        "in fn keeper_env: match cfg!(test) {",
        "`TARAE_KEEPER=1`: the test binary starts as the keeper",
    ),
    ("src/sandbox/keeper.rs", "in fn number: if let Some(info) = s.fdinfo {", HOOK),
    ("src/sandbox/keeper.rs", "in fn number: if let Some(d) = s.number_after {", HOOK),
    ("src/sandbox/keeper.rs", "in fn of: leak", SEAM),
    ("src/sandbox/keeper.rs", "in fn ready: if let Some(began) = slow {", HOOK),
    ("src/sandbox/keeper.rs", "in fn ready: slow", HOOK),
    ("src/sandbox/keeper.rs", "in fn start_with: covers", SEAM),
    ("src/sandbox/linux.rs", "in fn confine_in: granted", SEAM),
    ("src/sandbox/linux.rs", "in fn confine_in: ways", SEAM),
    ("src/sandbox/linux.rs", "in fn confine_in: ways_in", SEAM),
    ("src/sandbox/policy.rs", "in fn for_project: swap_pause", HOOK),
    (
        "src/sandbox/policy.rs",
        r#"in fn guarantees: cfg!(test) || cfg!(not(target_os = "linux")) || se"#,
        "a debug assertion relaxed: tests make macOS's policies on Linux",
    ),
    ("src/sandbox/proxy.rs", "in fn get: asks", ASKS),
    ("src/sandbox/proxy.rs", "in fn get: asks", ASKS),
    ("src/sandbox/proxy.rs", "in fn get: resolve", NAMES),
    ("src/sandbox/proxy.rs", "in fn get: resolve", NAMES),
    ("src/sandbox/proxy.rs", "in fn run: In", RECORD),
    ("src/sandbox/proxy.rs", "in fn started: beyond", BEYOND),
    ("src/sandbox/proxy.rs", "in fn started: beyond", BEYOND),
    (
        "src/sandbox/proxy/egress.rs",
        "in fn dial: if !a.ip().is_loopback()",
        "nothing dialled past this machine but with TARAE_MEASURE_REACHES_NETWORK: the network lane's (piece 5)",
    ),
    ("src/sandbox/proxy/egress.rs", "in fn refuses: beyond", BEYOND),
    ("src/sandbox/proxy/egress.rs", "in fn refuses: beyond", BEYOND),
    ("src/sandbox/proxy/relay.rs", "in fn relay_with: if let Ok(peer) = peer {", RECORD),
    ("src/sandbox/proxy/relay.rs", "in fn relay_with: if let Ok(peer) = peer {", RECORD),
    ("src/sandbox/proxy/relay.rs", "in fn relay_with: peer", RECORD),
    ("src/sandbox/supervisor.rs", "in fn kept_apart: if APART_OFF.load(Relaxed) == self.session {", HOOK),
    ("src/sandbox/supervisor.rs", "in fn routed: if p.resolvers.iter().any(|r| r.ip().to_canonical(", RECORD),
    ("src/sandbox/supervisor.rs", "in fn swapped_for: if let Some(pause) = p.swap_pause {", HOOK),
    ("src/sandbox/tier.rs", r#"in fn get: if std::env::var_os("TARAE_TEST_NO_NS").is_some() "#, SEAM),
    ("src/sandbox/tier.rs", r#"in fn get: if std::env::var_os("TARAE_TEST_NO_PID").is_some()"#, SEAM),
    (
        "src/tools.rs",
        "in fn answer: ANSWERED.with(|a| a.borrow_mut().push((call.id.clo",
        "the answer kept: compat reads it there",
    ),
    ("src/tools.rs", "in fn guard_and_go_riding: if let (For::Claude(_), Job::Command(c, _)) = (&wh", RECORD),
];

/// What `BRANCHES` says most often.
const RECORD: &str = "a test's record of what happened: kept, never read by the run";
const HOOK: &str = "a test's hook, set by a test for its own session or root: none here";
const SEAM: &str = "a seam behind a TARAE_TEST_… variable: off unless set, and compat sets none";
const NOT_RUNS: &str = "not on the run path";
const CLAUDES: &str = "the claude process's, not a run's: the chat here is a stand-in `sh`";
const ASKED: &str = "a record of the variables a policy reads; the shipped twin does nothing";
const RUNS: &str = "runs' folders in the temp folder (`Here::runs`), not tarae's cache: the same RunDir, another \
                    parent, and the check that every run may write there skipped";
const ASKS: &str = "the proxy's questions go to a test's sender (`TEST_ASKS`): the driver's, which hands them to its \
                    editor as the main loop does (`answered_by_script`) — each host card comes";
const NAMES: &str = "names resolved from `TEST_NAMES`/`TEST_RECORDS` only, never the network — the network lane \
                     needs a seam (piece 5)";
const BEYOND: &str = "macOS: addresses a test stands for beyond this machine (`TEST_BEYOND`): none";

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder of the test's own, made new (0700; another name where one is there already — never
    /// one found and emptied): `project/` (a file in it), `outside/` (watched), `harness/`. Removed when it goes,
    /// and only it.
    struct Scratch {
        dir: PathBuf,
        place: Place,
    }

    impl Scratch {
        fn new(test: &str) -> Scratch {
            Scratch::new_in(&std::env::temp_dir(), test)
        }

        /// In `temp`, a temp folder.
        fn new_in(temp: &Path, test: &str) -> Scratch {
            use std::os::unix::fs::DirBuilderExt;
            let base = temp.join(format!("tarae-compat-{test}-{}", std::process::id()));
            let dir = (0..)
                .map(|n| PathBuf::from(format!("{}-{n}", base.display())))
                .find(|d| match std::fs::DirBuilder::new().mode(0o700).create(d) {
                    Ok(()) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                    Err(e) => panic!("{}: {e}", d.display()),
                })
                .unwrap();
            let dir = std::fs::canonicalize(&dir).unwrap();
            for d in ["project", "outside", "harness"] {
                std::fs::DirBuilder::new().mode(0o700).create(dir.join(d)).unwrap();
            }
            std::fs::write(dir.join("project/README"), "a fixture\n").unwrap();
            // (a folder holding no protected name: on Linux a run can make one there — the snapshot's case)
            std::fs::DirBuilder::new().mode(0o700).create(dir.join("project/sub")).unwrap();
            std::fs::write(dir.join("project/sub/a.txt"), "a file\n").unwrap();
            checkout(&dir.join("project")).unwrap();
            let place = Place {
                project: dir.join("project"),
                watched: vec![dir.join("outside")],
                scratch: dir.join("harness"),
            };
            Scratch { dir, place }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn entry(text: &str) -> Entry {
        let m = Manifest::parse(&format!("stack = \"stand-in\"\n[[entry]]\n{text}")).unwrap();
        m.entries.into_iter().next().unwrap()
    }

    fn failed(v: &Verdict, check: Check) -> Vec<String> {
        v.failed.iter().filter(|f| f.check == check).map(|f| f.what.clone()).collect()
    }

    #[test]
    fn a_manifest_reads_its_entries_and_refuses_what_could_make_one_pass_by_mistake() {
        let m = Manifest::parse(
            r#"
stack = "node-npm"
pack = "javascript"
tools = ["node", "npm"]
fixture = "node-npm"
prewarm = ["npm ci"]
watch = ["~/.npm"]
writes = ["~/.npm/_logs"]
[files]
"package.json" = "{}"
"src/a.js" = """
export {};
"""
[[entry]]
name = "test"
run = "npm test"
expect = "pass"
cards = ["run"]
quick = true
settings = { llm = { sandbox-localhost = true } }
[[entry]]
name = "add"
run = "npm install left-pad"
expect = "refused"
said = ["package.json"]
pending = "manifest-review"
writes = ["~/.npm"]
baseline = false
timeout = 60
"#,
        )
        .unwrap();
        assert_eq!((m.stack.as_str(), m.pack.as_deref()), ("node-npm", Some("javascript")));
        assert_eq!((m.tools.len(), m.prewarm.len(), m.entries.len()), (2, 1, 2));
        let (t, add) = (&m.entries[0], &m.entries[1]);
        assert_eq!(t.expect, Expect::Pass);
        assert_eq!(t.settings, vec![("llm.sandbox-localhost".to_string(), toml::Value::Boolean(true))]);
        assert!(t.baseline && t.timeout == TIMEOUT && t.quick && !m.entries[1].quick);
        assert_eq!((m.fixture.as_deref(), &m.watch[..]), (Some("node-npm"), &["~/.npm".to_string()][..]));
        assert_eq!(add.expect, Expect::Refused(vec!["package.json".into()]));
        assert_eq!(add.pending.as_deref(), Some("manifest-review"));
        assert!(!add.baseline && add.timeout == Duration::from_secs(60));
        // (an asks entry has no baseline, and isn't quick)
        let asks = entry("name = \"a\"\nrun = \"npm audit\"\nexpect = \"pass\"\nasks = true");
        assert!(asks.asks && !asks.baseline && !t.asks);
        let files: Vec<(&str, &str)> = m.files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        assert_eq!(files, [("package.json", "{}"), ("src/a.js", "export {};\n")]);
        // (the stack's writes are every entry's, after its own)
        assert_eq!(t.writes, ["~/.npm/_logs"]);
        assert_eq!(add.writes, ["~/.npm", "~/.npm/_logs"]);
        let refused =
            |e: &str| Manifest::parse(&format!("stack = \"s\"\n[[entry]]\nname = \"e\"\nrun = \"x\"\n{e}"));
        for (e, why) in [
            ("expect = \"pass\"\ncard = [\"run\"]", "has no key card"),
            ("expect = \"pass\"\nsaid = [\"x\"]", "said is for what's refused"),
            ("expect = \"pass\"\npending = \"manifest-review\"", "so expect refused"),
            ("expect = \"passes\"", "expect: pass or refused"),
            ("", "no expect"),
            ("expect = \"pass\"\nsettings = { llm = { sandbox-localhst = true } }", "unknown setting"),
            ("expect = \"pass\"\nwrites = [\"cache\"]", "absolute or ~/"),
            ("expect = \"pass\"\nasks = true\nbaseline = true", "would reach the network"),
            ("expect = \"pass\"\nasks = true\nquick = true", "the asks lane alone"),
            ("expect = \"pass\"\nasks = 1", "asks: true or false"),
        ] {
            let r = refused(e);
            assert!(r.as_ref().is_err_and(|r| r.contains(why)), "{e}: {r:?}");
        }
        assert!(Manifest::parse("stak = \"s\"").is_err_and(|e| e.contains("no key stak")));
        for (top, why) in [
            ("fixture = \"../x\"", "a folder in compat/"),
            ("fixture = \"/x\"", "a folder in compat/"),
            ("watch = [\"cache\"]", "absolute or ~/"),
            ("writes = [\"cache\"]", "absolute or ~/"),
            ("files = { \"../a\" = \"x\" }", "a path in the project"),
            ("files = { \"/a\" = \"x\" }", "a path in the project"),
            ("files = { \".git/config\" = \"x\" }", "not in .git"),
            ("files = { \"a\" = 1 }", "its text, a string"),
            ("files = [\"a\"]", "a table of paths"),
            ("image = \"../node\"", "a recipe's name"),
            ("image = \"\"", "a recipe's name"),
            ("linux = [\"x\"]", "a table"),
            ("[linux]\nwach = []", "[linux] has no key wach"),
            ("[linux]\nwatch = [\"cache\"]", "absolute or ~/"),
            ("[linux]\nwrites = [\"cache\"]", "absolute or ~/"),
        ] {
            let r = Manifest::parse(&format!("stack = \"s\"\n{top}"));
            assert!(r.as_ref().is_err_and(|r| r.contains(why)), "{top}: {r:?}");
        }
        // (`[linux]`'s places stand for the manifest's own there, the others' where it names none)
        let m = Manifest::parse(
            "stack = \"s\"\nimage = \"go\"\nwatch = [\"~/Library/Caches/go-build\"]\nwrites = [\"~/w\"]\n\
             [linux]\nwatch = [\"~/.cache/go-build\"]\n[[entry]]\nname = \"e\"\nrun = \"x\"\nexpect = \"pass\"",
        )
        .unwrap();
        let watch = if cfg!(target_os = "linux") { "~/.cache/go-build" } else { "~/Library/Caches/go-build" };
        assert_eq!(
            (m.image.as_deref(), &m.watch[..], &m.entries[0].writes[..]),
            (Some("go"), &[watch.to_string()][..], &["~/w".to_string()][..])
        );
    }

    #[test]
    fn every_manifest_in_compat_reads_and_has_a_refusal() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(MANIFESTS);
        let mut read = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let Some(stack) = e.file_name().to_str().and_then(|n| n.strip_suffix(".toml")).map(String::from)
            else {
                continue;
            };
            let m = Manifest::parse(&std::fs::read_to_string(e.path()).unwrap())
                .unwrap_or_else(|e| panic!("{MANIFESTS}/{stack}.toml: {e}"));
            assert_eq!(m.stack, stack, "its stack is its file's name");
            assert!(!m.tools.is_empty() && !m.watch.is_empty(), "{stack}: its tools and its homes");
            if let Some(i) = &m.image {
                assert!(
                    dir.join("linux").join(format!("{i}.Dockerfile")).is_file(),
                    "{stack}: no recipe {i}"
                );
            }
            // (§9, piece 2: a protected name made where none is — the case only the snapshot catches on Linux)
            assert!(
                m.entries.iter().any(|e| matches!(e.expect, Expect::Refused(_)) && e.run.contains("sub")),
                "{stack}: an entry that makes a protected name in sub/"
            );
            read.push(stack);
        }
        assert!(read.len() >= 5, "the first stacks: {read:?}");
    }

    #[test]
    fn slow_is_half_again_and_more_than_a_moment_or_five_seconds_more() {
        let ms = Duration::from_millis;
        assert!(!slow(ms(60), ms(10)), "a moment's sandbox start isn't slow");
        assert!(slow(ms(1600), ms(1000)));
        assert!(!slow(ms(1400), ms(1000)));
        assert!(slow(ms(66_000), ms(60_000)), "five seconds more");
    }

    #[test]
    fn a_tree_says_what_was_made_changed_and_removed_outside_what_it_skips() {
        let s = Scratch::new("tree");
        let at = &s.place;
        let out = &at.watched[0];
        std::fs::write(out.join("kept"), "1").unwrap();
        std::fs::write(out.join("gone"), "1").unwrap();
        std::fs::create_dir_all(out.join("allowed")).unwrap();
        let skip = [out.join("allowed")];
        let before = Tree::of(&at.watched, &skip);
        std::fs::write(out.join("kept"), "12").unwrap();
        std::fs::remove_file(out.join("gone")).unwrap();
        std::fs::write(out.join("new"), "").unwrap();
        std::fs::write(out.join("allowed/x"), "").unwrap();
        let mut c = before.changes(&Tree::of(&at.watched, &skip));
        c.sort();
        assert_eq!(
            c,
            vec![(out.join("gone"), "removed"), (out.join("kept"), "changed"), (out.join("new"), "made")]
        );
    }

    /// A card nobody listed fails the entry, answered `n` — what it asked never runs. (No sandbox needed: the
    /// card comes before the run's guard — with `llm.sandbox` off, where a command has today's run card; in the
    /// sandbox it would run, P3.)
    #[test]
    fn a_card_nobody_listed_fails_the_entry_and_its_command_never_runs() {
        let s = Scratch::new("card");
        let at = &s.place;
        let e = entry(
            "name = \"made\"\nrun = \"touch made-here\"\nexpect = \"pass\"\ntimeout = 20\nsettings = { llm = { sandbox = false } }",
        );
        let v = run(&e, at);
        assert_eq!(
            failed(&v, Check::Card),
            vec!["run not-a-check:touch made-here — nobody listed it"],
            "{v}"
        );
        assert_eq!(v.cards, vec!["run not-a-check:touch made-here → n"], "{v}");
        assert!(failed(&v, Check::Exit).iter().any(|f| f.starts_with("it didn't run")), "{v}");
        assert!(!at.project.join("made-here").exists(), "{v}");
        // Its card is in the record, as tarae keeps it
        let record = crate::cards::read_record(&at.scratch.join(crate::cards::DIR));
        let said: Vec<_> = record.iter().map(|e| (&e["kind"], &e["answer"], &e["what"])).collect();
        assert_eq!(said, [(&json!("run"), &json!("n"), &json!("touch made-here"))], "{record:?}");
    }

    /// The settings an entry's run gets (`settings::apply`, as `run` applies them).
    fn llm_of(e: &Entry) -> crate::llm::LlmConfig {
        let mut cfg = crate::config::EditorConfig::default();
        for (path, v) in &e.settings {
            crate::settings::apply(&mut cfg, path, v).unwrap();
        }
        cfg.llm
    }

    /// What a run changes outside the project, where the sandbox let it write, fails the entry — only the
    /// harness's look before and after finds it — unless the entry lists the place; what its baseline writes
    /// there doesn't. Where the sandbox lets it
    /// write, from the policy (`writes_allow` is the profile's rule): not the temp folders here — the project is in
    /// one, and a policy never opens what holds the project (`Policy::for_project`) — but a folder
    /// `llm.sandbox-write` opens.
    #[test]
    fn a_write_outside_the_project_fails_the_entry_unless_it_lists_the_place() {
        let s = Scratch::new("writes");
        let (at, outside) = (&s.place, &s.place.watched[0]);
        let marker = outside.join("marker");
        let cmd = format!("touch {}", marker.display());
        let opened = format!("settings = {{ llm = {{ sandbox-write = [\"{}\"] }} }}", outside.display());
        let here = crate::sandbox::Here::now();
        let allows =
            |e: &Entry| crate::sandbox::policy_in(&at.project, &llm_of(e), &[], &here).writes_allow(&marker);
        assert!(!allows(&entry(&e_text(&cmd))), "the temp folder holding the project isn't opened");
        let e = entry(&format!("{}\n{opened}", e_text(&cmd)));
        assert!(allows(&e), "llm.sandbox-write opens it");
        if !sandboxed_here(
            "compat::tests::a_write_outside_the_project_fails_the_entry_unless_it_lists_the_place",
        ) {
            return;
        }
        let v = run(&e, at);
        assert_eq!(v.ended, Some(crate::task::State::Passed), "{v}");
        assert!(v.ran.as_deref().is_some_and(|r| r.starts_with("sandbox")), "{v}");
        let want = format!("made {} — outside the project, and not in its writes", marker.display());
        assert_eq!(failed(&v, Check::Writes), vec![want], "{v}");
        assert!(v.baseline.is_some(), "timed unsandboxed too: {v}");
        std::fs::remove_file(&marker).unwrap();
        let listed = format!("{}\n{opened}\nwrites = [\"{}\"]", e_text(&cmd), outside.display());
        let v = run(&entry(&listed), at);
        assert!(v.ok(), "{v}");
        // (what only its unsandboxed baseline writes there is the user's own build's, not the entry's)
        std::fs::remove_file(&marker).unwrap();
        let baseline_only = format!("case $PWD in */baseline-*) touch {};; esac", marker.display());
        let v = run(&entry(&e_text(&baseline_only)), at);
        assert!(v.ok() && v.baseline.is_some(), "{v}");
        assert!(marker.exists(), "the baseline wrote it: {v}");
    }

    /// Piece C: a closed run's connection reaches tarae's proxy, and its question comes to the driver's editor as
    /// the host card — unlisted, answered `n` and failing the entry, said with what asked. The `n` ends the
    /// connection that waits on it at once, and the run's next try of the host is refused for the session with no
    /// card and no wait: the run is over well within the proxy's hold. (A made-up name: the proxy asks before any
    /// lookup, and refused, nothing is looked up; no baseline — that would run it outside.)
    #[test]
    fn a_closed_runs_host_comes_to_the_driver_as_its_card() {
        let test = "compat::tests::a_closed_runs_host_comes_to_the_driver_as_its_card";
        if !sandboxed_here(test) || !on_path("curl") {
            return;
        }
        // (tier offline — runs not served, `pidfd_getfd` refused under Docker's default profile: no socket reaches
        // the proxy, so no card to see)
        #[cfg(target_os = "linux")]
        if let Some(why) = crate::sandbox::supervisor::unavailable() {
            let line = format!("{}{test} — runs not served here: {why}\n", crate::test_results::SELF_SKIP);
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            return;
        }
        let hold = crate::sandbox::proxy::Limits::default().hold;
        let curl = "curl -s --max-time 30 -o /dev/null https://compat-c1.example.invalid/";
        let run_ = format!("{curl}; {curl}");
        let text = format!(
            "name = \"out\"\nrun = \"{run_}\"\nexpect = \"pass\"\ncards = [\"run\"]\ntimeout = 60\nbaseline = false"
        );
        let s = Scratch::new("hostcard");
        let v = run(&entry(&text), &s.place);
        // (by the run's process as the proxy names it: `sh` on macOS, which knows no more)
        let hosts: Vec<&String> =
            v.cards.iter().filter(|c| c.contains("compat-c1.example.invalid:443 by ")).collect();
        assert_eq!(hosts.len(), 1, "one card, the second try refused without one: {v}");
        assert!(hosts[0].contains("→ n (asked 1×)"), "the host card, answered n: {v}");
        assert!(failed(&v, Check::Card).iter().any(|f| f.starts_with("host")), "{v}");
        assert_ne!(v.ended, Some(crate::task::State::Passed), "refused: {v}");
        assert!(v.took.is_some_and(|t| t < hold / 4), "refused at once, not after the {hold:?} hold: {v}");
        // (the asks lane's entry: the host card is what it measures — counted, not failed, nor its exit; a project
        // of its own, so the session's refusal above isn't its)
        let text = format!(
            "name = \"out\"\nrun = \"{run_}\"\nexpect = \"pass\"\ncards = [\"run\"]\ntimeout = 60\nasks = true"
        );
        let s = Scratch::new("hostcard-asks");
        let v = run(&entry(&text), &s.place);
        assert_eq!((v.asked, v.cell()), (Some(1), "● 1 host".to_string()), "{v}");
        assert!(v.to_string().contains("out: asked 1 host card"), "{v}");
        assert!(v.took.is_some_and(|t| t < hold / 4), "{v}");
    }

    fn e_text(cmd: &str) -> String {
        format!("name = \"out\"\nrun = \"{cmd}\"\nexpect = \"pass\"\ncards = [\"run\"]\ntimeout = 60")
    }

    /// A protected name, judged by what happened: refused, its cause is in the report (a pass expected fails
    /// naming it; a refusal expected passes only by its words); made — Landlock can't refuse a name made where
    /// none was (G23) — the snapshot finds it: an entry expected refused that says the name ends `⊘ found`
    /// (`judge_made`), any other fails as made. Two
    /// places: the root, which holds `.git` (Landlock grants it entry by entry: refused everywhere), and `sub/`,
    /// which holds no protected name (made on Linux: the case the snapshot is there for).
    #[test]
    fn a_protected_name_is_refused_with_its_cause_or_found_made() {
        if !sandboxed_here("compat::tests::a_protected_name_is_refused_with_its_cause_or_found_made") {
            return;
        }
        let s = Scratch::new("denial");
        let at = &s.place;
        let mut made = Vec::new();
        for name in ["Makefile", "sub/Makefile"] {
            let path = at.project.join(name);
            let refused = format!(
                "name = \"make\"\nrun = \"touch {name}\"\nexpect = \"refused\"\nsaid = [\"{name}\"]\n\
                 pending = \"manifest-review\"\ncards = [\"run\"]\ntimeout = 60"
            );
            let unsaid = refused.replace(&format!("said = [\"{name}\"]"), "said = [\"not in any report\"]");
            for (text, expected) in
                [(e_text(&format!("touch {name}")), "pass"), (refused, "refused"), (unsaid, "")]
            {
                let v = run(&entry(&text), at);
                let at_ = format!("{name}, {expected:?}: {v}");
                if path.exists() {
                    if expected == "refused" {
                        assert_eq!(
                            (v.cell(), &v.found[..]),
                            ("⊘ found".into(), &[name.to_string()][..]),
                            "{at_}"
                        );
                        assert!(v.to_string().contains("found by the snapshot"), "{at_}");
                    } else {
                        assert!(
                            failed(&v, Check::Made).iter().any(|f| f.contains(name)),
                            "made, unsaid: {at_}"
                        );
                        assert_eq!(v.cell(), "✗ protected-name", "{at_}");
                    }
                    std::fs::remove_file(&path).unwrap();
                    made.push(name);
                    continue;
                }
                match expected {
                    "pass" => {
                        assert!(!failed(&v, Check::Exit).is_empty(), "{at_}");
                        assert!(
                            failed(&v, Check::Report).iter().any(|f| f.contains(name)),
                            "its cause: {at_}"
                        );
                        assert_eq!(v.baseline, None, "a failed run isn't timed against its baseline: {at_}");
                    }
                    "refused" => {
                        assert!(v.ok(), "{at_}");
                        assert!(
                            v.to_string().contains("refused as expected, pending manifest-review"),
                            "{at_}"
                        );
                    }
                    _ => assert_eq!(
                        failed(&v, Check::Report),
                        vec!["the report doesn't say not in any report"],
                        "{at_}"
                    ),
                }
            }
        }
        // (both outcomes seen where they happen: Seatbelt refuses the name anywhere; Landlock only in a folder
        // holding a protected name — in `sub/` it's made, and only the snapshot catches it)
        match cfg!(target_os = "linux") {
            true => assert_eq!(made, ["sub/Makefile"; 3]),
            false => assert!(made.is_empty(), "{made:?}"),
        }
    }

    /// Without a checkout the snapshot can't see what a run made: the harness refuses to judge it.
    #[test]
    fn a_project_that_isnt_a_checkout_isnt_judged() {
        let s = Scratch::new("nogit");
        std::fs::remove_dir_all(s.place.project.join(".git")).unwrap();
        let v = run(&entry(&e_text("true")), &s.place);
        assert_eq!(v.cell(), "✗ protected-name", "{v}");
        assert!(failed(&v, Check::Made)[0].contains("isn't a git checkout"), "{v}");
    }

    /// A failure the report names no cause of is the worst outcome (Claude would work around it): said so. (It
    /// fails only in the sandbox: one its unsandboxed baseline shares is the tool's own — `not-ours`.)
    #[test]
    fn a_failure_with_no_cause_in_the_report_is_said() {
        if !sandboxed_here("compat::tests::a_failure_with_no_cause_in_the_report_is_said") {
            return;
        }
        let s = Scratch::new("nocause");
        let v = run(&entry(&e_text("case $PWD in */baseline-*) ;; *) exit 3;; esac")), &s.place);
        assert_eq!(v.ended, Some(crate::task::State::Failed(Some(3))), "{v}");
        assert_eq!(failed(&v, Check::Report), vec!["it failed, and the report names no cause"], "{v}");
        assert_eq!(v.cell(), "✗ report", "{v}");
    }

    #[test]
    fn a_cell_names_the_first_class_or_how_it_passed() {
        let mut v = Verdict {
            entry: "e".into(),
            pending: None,
            ended: Some(crate::task::State::Passed),
            ran: None,
            took: Some(Duration::from_millis(1700)),
            baseline: Some(Duration::from_millis(1000)),
            cold: None,
            answer: String::new(),
            cards: Vec::new(),
            failed: Vec::new(),
            found: Vec::new(),
            asked: None,
        };
        assert_eq!(v.cell(), "✓ 1.7×");
        v.took = Some(Duration::from_millis(1100));
        assert_eq!(v.cell(), "✓");
        v.ended = Some(crate::task::State::Failed(Some(1)));
        assert_eq!(v.cell(), "⊘", "refused, as expected");
        v.fail(Check::Exit, "it didn't run");
        v.fail(Check::Writes, "made x");
        v.fail(Check::Card, "run — nobody listed it");
        assert_eq!(v.cell(), "✗ card");
        v.fail(Check::Report, NO_CAUSE);
        assert_eq!(v.cell(), "✗ report");
        assert_eq!(line("s", "e\tx", "✗ a\nb"), "COMPAT\ts\te x\t✗ a b");
        // (failing unsandboxed too: not the sandbox's — before what it was denied, after what it did outside)
        v.failed.clear();
        v.fail(Check::Report, "the sandbox blocked: opening apps");
        v.fail(Check::Outside, "it fails unsandboxed too — exit 3: x");
        assert_eq!(v.cell(), "✗ not-ours");
        v.fail(Check::Writes, "made x");
        assert_eq!(v.cell(), "✗ writes");
        // (an asks entry: its host cards, after any failure)
        v.asked = Some(2);
        assert_eq!(v.cell(), "✗ writes");
        v.failed.clear();
        assert_eq!(v.cell(), "● 2 hosts");
        v.asked = Some(0);
        assert_eq!(v.cell(), "⊘", "it asked nothing");
    }

    /// A pass expected that fails in the sandbox and unsandboxed alike is the tool's own (§5 `not-ours`): its exit
    /// and the report's silence aren't failed, the baseline's words are said; one that fails only in the sandbox
    /// stays a failure that names no cause.
    #[test]
    fn a_failure_the_baseline_shares_is_not_ours() {
        if !sandboxed_here("compat::tests::a_failure_the_baseline_shares_is_not_ours") {
            return;
        }
        let s = Scratch::new("notours");
        let at = &s.place;
        let v = run(&entry(&e_text("echo broken >&2; exit 3")), at);
        assert_eq!(v.cell(), "✗ not-ours", "{v}");
        assert_eq!(failed(&v, Check::Outside), ["it fails unsandboxed too — exit 3: broken"], "{v}");
        assert!(failed(&v, Check::Exit).is_empty() && failed(&v, Check::Report).is_empty(), "{v}");
        let sandbox_only = "case $PWD in */baseline-*) ;; *) exit 3;; esac";
        let v = run(&entry(&e_text(sandbox_only)), at);
        assert_eq!(v.cell(), "✗ report", "{v}");
        assert!(failed(&v, Check::Outside).is_empty(), "{v}");
    }

    /// A protected name made: under Landlock, by an entry expected refused that says it, the expected end (`⊘
    /// found`, its "succeeded" taken back); on macOS, by an entry expected to pass, or a name it doesn't say, a
    /// failure.
    #[test]
    fn a_name_made_under_landlock_is_found_only_where_refused_was_expected() {
        let judged = |text: &str, made: &[&str], landlock: bool| {
            let e = entry(text);
            let mut v = Verdict {
                entry: "e".into(),
                pending: None,
                ended: Some(crate::task::State::Passed),
                ran: None,
                took: None,
                baseline: None,
                cold: None,
                answer: String::new(),
                cards: Vec::new(),
                failed: Vec::new(),
                found: Vec::new(),
                asked: None,
            };
            // (as `judge_report` leaves a refusal expected that passed)
            if matches!(e.expect, Expect::Refused(_)) {
                v.fail(Check::Exit, "it succeeded");
                v.fail(Check::Report, "the report doesn't say Makefile");
            }
            let made: Vec<String> = made.iter().map(|m| m.to_string()).collect();
            judge_made(&mut v, &e, &made, landlock);
            v.cell()
        };
        let refused = "name = \"m\"\nrun = \"x\"\nexpect = \"refused\"\nsaid = [\"Makefile\"]";
        assert_eq!(judged(refused, &["sub/Makefile"], true), "⊘ found");
        assert_eq!(judged(refused, &["sub/Makefile"], false), "✗ protected-name");
        assert_eq!(judged(refused, &["sub/Makefile", "sub/build.rs"], true), "✗ protected-name");
        assert_eq!(judged(refused, &[], true), "✗ unexplained", "nothing made, nothing refused");
        assert_eq!(judged(&e_text("x"), &["sub/Makefile"], true), "✗ protected-name");
    }

    /// The driver `cargo xtask compat` runs (ignored: it needs the stacks' toolchains, and runs their `prewarm`
    /// outside the sandbox): each stack of `TARAE_COMPAT_STACKS` (comma-separated, `compat/<stack>.toml`), its
    /// entries in `TARAE_COMPAT_LANE` (`quick`: the quick ones; `asks`: the asks ones; else all but those), each
    /// verdict on stderr and its cell on
    /// stdout as a `COMPAT` line (`line`) for the xtask's table — on a Linux tier's column (`TARAE_COMPAT_TIER`:
    /// `own`, `host`) a run at another tier ends the stack there, said. Without the variable it does nothing.
    #[test]
    #[ignore]
    fn driver() {
        let Some(stacks) = std::env::var_os("TARAE_COMPAT_STACKS") else { return };
        let lane = std::env::var("TARAE_COMPAT_LANE").unwrap_or_default();
        let root = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
        let root = root.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        for stack in stacks.to_string_lossy().split(',').filter(|s| !s.is_empty()) {
            for (entry, cell) in drive(&root, stack, &lane) {
                println!("{}", line(stack, &entry, &cell));
            }
        }
    }

    /// A cell for the xtask: `COMPAT`, the stack, the entry and the cell, tab-separated, on one line.
    fn line(stack: &str, entry: &str, cell: &str) -> String {
        let one = |s: &str| s.replace(['\t', '\n'], " ");
        format!("COMPAT\t{}\t{}\t{}", one(stack), one(entry), one(cell))
    }

    /// One stack's entries in the lane, each with its cell (`Verdict::cell`) — or `– why` where none could run:
    /// a tool not on PATH, no sandbox here, its fixture or a prewarm failed.
    fn drive(root: &Path, stack: &str, lane: &str) -> Vec<(String, String)> {
        let text = std::fs::read_to_string(root.join(MANIFESTS).join(format!("{stack}.toml")));
        let m = match text.map_err(|e| e.to_string()).and_then(|t| Manifest::parse(&t)) {
            Ok(m) => m,
            Err(e) => return vec![("(manifest)".into(), format!("✗ manifest: {e}"))],
        };
        let entries: Vec<&Entry> =
            m.entries.iter().filter(|e| e.asks == (lane == "asks") && (lane != "quick" || e.quick)).collect();
        let skip = |why: &str| -> Vec<(String, String)> {
            entries.iter().map(|e| (e.name.clone(), format!("– {why}"))).collect()
        };
        if let Some(t) = m.tools.iter().find(|t| !on_path(t)) {
            return skip(&format!("no {t} on PATH"));
        }
        if !sandboxed_here("compat::tests::driver") {
            return skip("no sandbox here");
        }
        // (macOS: not in $TMPDIR — a policy never opens what holds the project, and a user's project isn't in
        // it: a JVM's `$TMPDIR/hsperfdata_<user>` was refused only because the fixture was there; `/private/tmp`
        // is closed instead)
        let temp = match cfg!(target_os = "macos") {
            true => PathBuf::from("/private/tmp"),
            false => std::env::temp_dir(),
        };
        let s = Scratch::new_in(&temp, stack);
        if let Some(f) = &m.fixture
            && let Err(e) = copy_tree(&root.join(MANIFESTS).join(f), &s.place.project)
        {
            return skip(&format!("fixture {f}: {e}"));
        }
        for (f, text) in &m.files {
            let path = s.place.project.join(f);
            let parent = path.parent().map_or(Ok(()), std::fs::create_dir_all);
            if let Err(e) = parent.and_then(|()| std::fs::write(&path, text)) {
                return skip(&format!("file {f}: {e}"));
            }
        }
        if let Err(e) = checkout(&s.place.project) {
            return skip(&format!("the fixture's checkout: {e}"));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let at = Place {
            project: s.place.project.clone(),
            watched: match m.watch.is_empty() {
                true => s.place.watched.clone(),
                false => m.watch.iter().filter_map(|w| place(w, home.as_deref())).collect(),
            },
            scratch: s.place.scratch.clone(),
        };
        for cmd in &m.prewarm {
            eprintln!("{stack}: prewarm, outside the sandbox: {cmd}");
            let mut c = crate::nofile::command("sh", Some(&at.project));
            c.args(["-c", cmd]).stdin(std::process::Stdio::null());
            match c.output() {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    let last = err.lines().last().unwrap_or_default();
                    return skip(&format!("prewarm failed ({}): {cmd}: {last}", o.status));
                }
                Err(e) => return skip(&format!("prewarm didn't start: {cmd}: {e}")),
            }
        }
        // (a Linux column's tier, `TARAE_COMPAT_TIER`: a run at another isn't that column's — the rest of the stack
        // skipped, said where it ran)
        let tier = std::env::var("TARAE_COMPAT_TIER").ok().filter(|t| !t.is_empty());
        let mut elsewhere: Option<String> = None;
        let cells = entries.iter().map(|e| {
            if let Some(why) = &elsewhere {
                return (e.name.clone(), why.clone());
            }
            let v = run(e, &at);
            eprint!("{stack} {v}");
            match (&tier, &v.ran) {
                (Some(t), Some(ran)) if *ran != format!("sandbox {t}") => {
                    let why = format!("– {ran} here, not {t}");
                    elsewhere = Some(why.clone());
                    (e.name.clone(), why)
                }
                _ => (e.name.clone(), v.cell()),
            }
        });
        cells.collect()
    }

    /// Whether `tool` is an executable file in a folder of PATH.
    fn on_path(tool: &str) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path).any(|d| {
            std::fs::metadata(d.join(tool)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }

    /// Where `cfg(test)` is on the run path, found in the source: each `(file, item)` — an attribute's item, or
    /// the function an expression is in — once per time it's there. Test modules (inline from `mod tests {`, or
    /// files declared under `#[cfg(test)]`) left out.
    fn test_branches(root: &Path) -> Vec<(String, String)> {
        let mut files: Vec<(PathBuf, Option<&str>)> = RUN_PATH.iter().map(|f| (root.join(f), None)).collect();
        files.push((root.join("src/editor.rs"), Some("with_root")));
        let mut dirs = vec![root.join("src/sandbox")];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                match p.is_dir() {
                    true => dirs.push(p),
                    false if p.extension().is_some_and(|x| x == "rs") => files.push((p, None)),
                    false => {}
                }
            }
        }
        // (files whose module is declared under `#[cfg(test)]` — a test module, whole)
        let mut test_files = BTreeSet::new();
        for (f, _) in &files {
            let text = std::fs::read_to_string(f).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let dir = match f.file_stem().is_some_and(|s| s == "mod") {
                true => f.parent().unwrap().to_path_buf(),
                false => f.with_extension(""),
            };
            for w in lines.windows(2) {
                if w[0].trim().starts_with("#[cfg(") && w[0].contains("test") {
                    let decl = w[1].trim().trim_start_matches("pub ").trim_start_matches("pub(crate) ");
                    if let Some(m) = decl.strip_prefix("mod ").and_then(|m| m.strip_suffix(';')) {
                        test_files.insert(dir.join(format!("{m}.rs")));
                        test_files.insert(dir.join(m).join("mod.rs"));
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (f, only) in files.iter().filter(|(f, _)| !test_files.contains(f)) {
            let text = std::fs::read_to_string(f).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let name = f.strip_prefix(root).unwrap().display().to_string();
            // (the item a line is in: the last `fn`, `struct`, `enum`, `impl` begun)
            let mut ctx = String::new();
            // (items only a test build has, and items only the shipped build has: a name in both is a twin)
            let (mut added, mut shipped) = (BTreeSet::new(), Vec::new());
            for (i, l) in lines.iter().enumerate() {
                let t = l.trim();
                if unpub(t) == "mod tests {" {
                    break;
                }
                if let Some(item) = item_name(t)
                    .filter(|i| ["fn ", "struct ", "enum ", "impl "].iter().any(|k| i.starts_with(k)))
                {
                    ctx = item;
                }
                let is_test = ["cfg!(test)", "cfg(test)", "cfg(not(test))", "cfg(all(test", "cfg(any(test"]
                    .iter()
                    .any(|c| t.contains(c));
                if !is_test || t.starts_with("//") {
                    continue;
                }
                // (an attribute: what it's on, past other attributes and doc lines)
                let on = match t.starts_with("#[") {
                    true => lines[i + 1..]
                        .iter()
                        .map(|l| l.trim())
                        .find(|l| !l.starts_with("#[") && !l.starts_with("//"))
                        .unwrap_or_default(),
                    false => t,
                };
                if unpub(on).starts_with("mod ") {
                    continue;
                }
                // (an item only a test build has is only reached from test code — not a branch; one only the
                // shipped build has is; what a function does under `cfg(test)` is; a field or variant a test
                // build adds is data, set by the branches)
                match item_name(on).filter(|_| t.starts_with("#[")) {
                    Some(item) if t.contains("not(test)") => shipped.push(item),
                    Some(item) => _ = added.insert(item),
                    None if ctx.starts_with("fn ") && only.is_none_or(|o| ctx == format!("fn {o}")) => {
                        out.push((name.clone(), format!("in {ctx}: {}", what(on))))
                    }
                    None => {}
                }
            }
            for item in shipped {
                let how = if added.contains(&item) { "a test build's twin" } else { "not in a test build" };
                out.push((name.clone(), format!("{item}: {how}")));
            }
        }
        out.sort();
        out
    }

    fn fn_name(t: &str) -> Option<String> {
        let at = t.find("fn ")?;
        let before = &t[..at];
        if !before
            .split_whitespace()
            .all(|w| ["pub", "pub(crate)", "pub(super)", "const", "unsafe", "async", "extern"].contains(&w))
        {
            return None;
        }
        let rest = &t[at + 3..];
        Some(rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect())
    }

    /// An item's kind and name, from its first line: `fn report`, `static ANSWERED`, `impl Call` — None: not an
    /// item's first line.
    fn item_name(t: &str) -> Option<String> {
        let t = unpub(t);
        if let Some(f) = fn_name(t) {
            return Some(format!("fn {f}"));
        }
        for kind in ["struct ", "enum ", "const ", "static ", "impl", "use ", "type ", "thread_local!"] {
            if let Some(rest) = t.strip_prefix(kind) {
                let name: String = rest
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                    .collect();
                return Some(format!("{}{name}", kind.trim_end().to_string() + " ").trim_end().to_string());
            }
        }
        None
    }

    /// `t` without its visibility (`pub`, `pub(crate)`, `pub(super)` …).
    fn unpub(t: &str) -> &str {
        match t.strip_prefix("pub") {
            Some(rest) if rest.starts_with(' ') => rest.trim_start(),
            Some(rest) if rest.starts_with('(') => rest.split_once(") ").map_or(t, |(_, r)| r),
            _ => t,
        }
    }

    /// What a line inside an item names: a field's or a `let`'s name, else its start.
    fn what(t: &str) -> String {
        let t = unpub(t).trim_start_matches("let ");
        let name: String = t.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        match t[name.len()..].trim_start().chars().next() {
            Some(':' | '=') if !name.is_empty() => name,
            _ => t.chars().take(50).collect(),
        }
    }

    #[test]
    fn every_test_branch_on_the_run_path_is_listed() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let found = test_branches(root);
        let listed: Vec<(String, String)> =
            BRANCHES.iter().map(|(f, i, _)| (f.to_string(), i.to_string())).collect();
        let mut listed_sorted = listed.clone();
        listed_sorted.sort();
        let new: Vec<String> = found
            .iter()
            .filter(|f| !listed.contains(f))
            .map(|(f, i)| format!("    (\"{f}\", \"{i}\", \"…\"),"))
            .collect();
        let gone: Vec<String> =
            listed.iter().filter(|l| !found.contains(l)).map(|(f, i)| format!("{f}: {i}")).collect();
        assert!(
            new.is_empty() && gone.is_empty() && found == listed_sorted,
            "cfg(test) on the run path, not in compat::BRANCHES (say what compat does about each):\n{}\n\
             listed, not there any more:\n{}",
            new.join("\n"),
            gone.join("\n")
        );
        assert!(
            BRANCHES.iter().all(|(_, _, what)| !what.is_empty() && *what != "…"),
            "each says what compat does"
        );
    }
}
