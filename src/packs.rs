//! Language packs: what tarae knows about running a language's tests and a project's tasks, as data —
//! `runtime/packs/<lang>/` holds a `pack.toml` and the queries it names. A pack in a runtime folder
//! (`~/.local/share/tarae/packs/`) wins over the built-in one, table by table (`[test]`, `[tasks]`) and
//! file by file (its queries), so a language can be added or changed without touching tarae.
//!
//! `[test]`:
//! - `query`: a tree-sitter query capturing each test (`@test`) and each group of tests (`@group`: a
//!   module, a class), with its name (`@name`). `@start` moves where a test starts back (to its
//!   attributes). A test pattern's `(#set! kind "…")` gives its tests settings of their own in
//!   `[test.kind.<kind>]`.
//! - `files`: the file names tests live in (`*_test.go`). `cwd`: where they run — `"file-dir"`,
//!   `{ nearest = [...], or = [...] }` (the closest folder above with one of `nearest`, else of `or`, else
//!   the file's), or a template (`"{java-root}"`).
//! - `helpers`: tarae's own helpers the templates need — `cargo`: `{cargo-target}` (`--lib`, `--bin x` or
//!   `--test x`), `{module}` (the file's module path, `a::b::`, or empty), `{module-label}` (`a::b::*` or `all
//!   tests`), `{crate-root}`. `vars`: variables worked out from what's around the file (`Derive`) — the
//!   virtualenv's Python, a build's layout.
//! - How the output is read: `[[test.line]]` rules and `none-ran`, a line at a time as it comes
//!   (`test_results::Rules`); or `parse = "junit"`: the JUnit XML written to `reports` during the run, read
//!   once it ends, failures placed through `sources`.
//! - Templates, for the test at the cursor, the group around the cursor when it's in no test, and the
//!   file: `run`, `run-group`, `run-file` (the command); `debug`, `debug-group`, `debug-file` (the
//!   debugger's launch configuration); `label`, `label-group`, `label-file` (what the result is called).
//!   Their variables: `{name}` (the test), `{groups}` (the groups around it, joined with `group-sep`,
//!   `::` unless set), `{path}` (groups and name), `{groups/.}` and `{path/.}` (joined with what follows
//!   the `/` instead; `|re` after any of them escapes it for a regular expression), `{first-group}` (the file's first group), `{names}` (the file's tests, joined with
//!   `|`), `{cwd}`, `{file}`, `{filename}`, `{rel}` (the file from `{cwd}`), and the helpers'. An argument
//!   that is just a list variable (`"{cargo-target}"`) becomes its items; one that ends up empty is left
//!   out.
//! - Debugging: `{built}` in a `debug` template is what `debug-build` builds when debugging starts (the
//!   shape of `[debug] build`, `debugpack.rs`). Or, instead of a `debug` template: `debug-args`, added to
//!   the command so the test waits for a debugger, and `debug-attach = { wait-for, way }`: the line saying
//!   where it waits (a regular expression with a `port` group), and the way tarae attaches then
//!   (`debugpack::WAYS` — `java`).
//! - `variant-by`: a template naming the `[test.variant.<name>]` whose settings (templates, `reports`,
//!   `sources`, `debug-args`) override these — Java's `gradle` and `maven`. A kind's settings override the
//!   test ones; `file = false` keeps its tests out of a file's run.
//!
//! `[tasks]`: see `tasks`. `[install.<tool>]`: see `Install`. `[server.<name>]`: see `LeaveOut`.
//! `[debug]`, `[debugger.<name>]`: `debugpack.rs`.
//! `[[waits]]`: see `waits`. `[[places]]`: see `places`. `[language.<name>]`, `[grammar.<name>]`,
//! `[server.<name>]`'s `command` and `args`, `config-files`, `outputs`, `checks`, `color-off`: the language
//! table, `languages.rs`. `first-made`: what a Linux run gets made
//! first (`made_beside`, `sandbox/linux.rs`). How a runtime folder's pack counts for each key: `Reach`.
//!
//! Packs are read when a test or the tasks are looked for (the built-in ones parsed once, a runtime folder's
//! read each time), so an edited pack counts at once — but for the language table, read once at the start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use streaming_iterator::StreamingIterator;
use toml::Table;
use tree_sitter::{Language, Query, QueryCursor, Tree};

/// The test at the cursor, the group around it, or the file's tests, resolved from a pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub label: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    /// The debugger's launch configuration, if the pack has one for this.
    pub debug: Option<Value>,
    /// How its output is read a line at a time (`[[test.line]]`).
    pub lines: Option<crate::test_results::Rules>,
    /// What to build before debugging (`{built}`).
    pub build: Option<crate::debugpack::Build>,
    /// Debugging by running this command instead (the test waits for a debugger), then attaching this way.
    pub debug_run: Option<(Vec<String>, AttachWait)>,
    /// `parse = "junit"`: the folders the reports land in, and the source roots.
    pub junit: Option<(Vec<PathBuf>, Vec<PathBuf>)>,
}

/// A pack's `[test]`.
pub struct Tests {
    query: Query,
    files: Vec<String>,
    cwd: Cwd,
    helpers: Vec<String>,
    /// `"junit"`, or empty.
    parse: String,
    lines: Option<crate::test_results::Rules>,
    group_sep: String,
    /// What `{built}` in a `debug` template is, built when debugging starts.
    debug_build: Option<crate::debugpack::BuildTemplate>,
    base: Settings,
    variant_by: Option<String>,
    variants: HashMap<String, Settings>,
    kinds: HashMap<String, Kind>,
    /// `vars`: worked out for the file before the variant is chosen · each variant's, after.
    vars: Vec<Derive>,
    variant_vars: HashMap<String, Vec<Derive>>,
}

/// A `vars` entry of `[test]` or a variant: a variable worked out for the file from what's around it — the
/// pieces a build's layout is said with (Java's: the topmost build file's folder, the module's, Gradle or
/// Maven, the wrapper or the tool). In order; each may use those before it, and the query's `@var.<name>`
/// captures (the text of the first; empty when none).
///
/// - `topmost = [files]` / `nearest = [files]`: the folder holding one, the topmost below the repository or
///   the nearest up — `"roots"`: the file's language's `roots`; none: `else` (a template), or the error
///   `missing`.
/// - `first = [templates]`: the first that's a file, else the first that isn't a path (a program on PATH);
///   `~/` and `${VAR}` expanded, one whose variable isn't set left out (`${VIRTUAL_ENV}/bin/python`).
/// - `parts = "{a}"`, `from = "{b}"`: a's folders below b — `{name}` joins them with `/`, `{name/:}` with `:`.
/// - `if = "{x}"` (not empty), or `if-exists = "{x}/f"`: `then`, else `else` — a template, or a list of them
///   (its empty items left out; an argument that's just this variable becomes its items).
#[derive(Clone, Debug)]
pub(crate) struct Derive {
    name: String,
    how: Derived,
}

#[derive(Clone, Debug)]
enum Derived {
    Folder { topmost: bool, markers: Markers, or: Option<String>, missing: Option<String> },
    First(Vec<String>),
    Parts { path: String, from: String },
    If { test: String, exists: bool, then: Choice, or: Choice },
}

#[derive(Clone, Debug)]
enum Markers {
    /// The file's language's `roots`.
    Roots,
    Files(Vec<String>),
}

/// A template, or a list of them.
#[derive(Clone, Debug)]
enum Choice {
    One(String),
    Many(Vec<String>),
}

impl Derive {
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// `lang`'s pack's `[test]` as the language table's snapshot says it: how its output is read, how a test
/// debugged by running it is attached — None: no pack with tests.
#[cfg(test)]
pub(crate) fn test_summary(lang: &str) -> Option<String> {
    let t = manifest(lang)?.ok()?;
    let t = t.get("test")?.as_table()?;
    let mut said = match crate::test_results::Rules::read(t) {
        Ok(Some(r)) => r.summary(),
        Ok(None) if string(t, "parse").ok()?.as_deref() == Some("junit") => "junit reports".into(),
        Ok(None) => "exit code only".into(),
        Err(e) => format!("broken: {e}"),
    };
    let names = |v: Option<&toml::Value>| -> Result<Vec<String>, String> {
        Ok(derive_rules(v, "")?.iter().map(|d| d.name.clone()).collect())
    };
    if let Ok(vars) = names(t.get("vars"))
        && !vars.is_empty()
    {
        said += &format!(" · vars {}", vars.join(" "));
    }
    if let Some(a) = t.get("debug-attach") {
        said += &match AttachWait::read(a) {
            Ok(a) => format!(" · debug-attach {} on {}", a.way, a.wait_for.as_str()),
            Err(e) => format!(" · debug-attach broken: {e}"),
        };
    }
    Some(said)
}

/// `lang`'s pack's `[[places]]`, each its `match` (and `then`) — for the snapshot.
#[cfg(test)]
pub(crate) fn places_summary(lang: &str) -> Vec<String> {
    let Some(Ok(t)) = manifest(lang) else { return Vec::new() };
    let Some(v) = t.get("places") else { return Vec::new() };
    match places_of(v) {
        Ok(rules) => rules.iter().map(crate::task::PlaceRule::summary).collect(),
        Err(e) => vec![format!("broken: {e}")],
    }
}

/// A `vars` list, read.
pub(crate) fn derive_rules(v: Option<&toml::Value>, at: &str) -> Result<Vec<Derive>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let list = v.as_array().ok_or(format!("`{at}vars` isn't a list"))?;
    let mut out = Vec::new();
    for e in list {
        let t = e.as_table().ok_or(format!("a `{at}vars` entry isn't a table"))?;
        let name = string(t, "name")?.ok_or(format!("a `{at}vars` entry has no `name`"))?;
        let what = format!("`{at}vars` `{name}`");
        let markers = |k: &str| -> Result<Markers, String> {
            match t.get(k) {
                Some(toml::Value::String(s)) if s == "roots" => Ok(Markers::Roots),
                v => Ok(Markers::Files(strings(v, k)?.unwrap_or_default())),
            }
        };
        let one_or_many = |k: &str| -> Result<Choice, String> {
            match t.get(k) {
                None => Ok(Choice::One(String::new())),
                Some(toml::Value::String(s)) => Ok(Choice::One(s.clone())),
                v => Ok(Choice::Many(strings(v, k)?.unwrap_or_default())),
            }
        };
        let how = if t.contains_key("topmost") || t.contains_key("nearest") {
            known(t, &["name", "topmost", "nearest", "else", "missing"], &what)?;
            let topmost = t.contains_key("topmost");
            Derived::Folder {
                topmost,
                markers: markers(if topmost { "topmost" } else { "nearest" })?,
                or: string(t, "else")?,
                missing: string(t, "missing")?,
            }
        } else if t.contains_key("first") {
            known(t, &["name", "first"], &what)?;
            Derived::First(strings(t.get("first"), "first")?.unwrap_or_default())
        } else if t.contains_key("parts") {
            known(t, &["name", "parts", "from"], &what)?;
            let need = |k: &str| string(t, k)?.ok_or(format!("{what} has no `{k}`"));
            Derived::Parts { path: need("parts")?, from: need("from")? }
        } else if t.contains_key("if") || t.contains_key("if-exists") {
            known(t, &["name", "if", "if-exists", "then", "else"], &what)?;
            let exists = t.contains_key("if-exists");
            let test = string(t, if exists { "if-exists" } else { "if" })?.unwrap_or_default();
            Derived::If { test, exists, then: one_or_many("then")?, or: one_or_many("else")? }
        } else {
            return Err(format!("{what}: say how — topmost, nearest, first, parts or if"));
        };
        out.push(Derive { name, how });
    }
    Ok(out)
}

/// `rules` worked out for `path`, into `vars`.
pub(crate) fn derive(rules: &[Derive], path: &Path, vars: &mut HashMap<String, Var>) -> Result<(), String> {
    for r in rules {
        let value = match &r.how {
            Derived::Folder { topmost, markers, or, missing } => {
                let files: Vec<String> = match markers {
                    Markers::Files(f) => f.clone(),
                    Markers::Roots => {
                        let lang = crate::syntax::detect(path).map(|l| l.name.clone()).unwrap_or_default();
                        crate::lsp::markers_for(&lang).to_vec()
                    }
                };
                let found = match topmost {
                    true => crate::lsp::root_by_markers(path, &files),
                    false => up_to(path, |d| files.iter().any(|f| d.join(f).is_file())),
                };
                match (found, or) {
                    (Some(d), _) => Var::One(d.display().to_string()),
                    (None, Some(t)) => Var::One(fill(t, vars)),
                    (None, None) => {
                        return Err(missing
                            .clone()
                            .unwrap_or_else(|| format!("no {} above this file", files.join(" or "))));
                    }
                }
            }
            Derived::First(items) => {
                // (`~/` and `${VAR}` too — one naming a variable that isn't set isn't a choice)
                let items: Vec<String> = items
                    .iter()
                    .filter_map(|t| expand_dir(&fill(t, vars)).map(|p| p.display().to_string()))
                    .collect();
                let file = items.iter().find(|i| i.starts_with('/') && Path::new(i).is_file());
                match file.or_else(|| items.iter().find(|i| !i.contains('/') && !i.is_empty())) {
                    Some(i) => Var::One(i.clone()),
                    None => return Err(format!("`{}`: none of {} is here", r.name, items.join(", "))),
                }
            }
            Derived::Parts { path: p, from } => {
                let (p, from) = (PathBuf::from(fill(p, vars)), PathBuf::from(fill(from, vars)));
                let parts = p
                    .strip_prefix(&from)
                    .map(|rel| rel.iter().map(|c| c.to_string_lossy().into_owned()).collect())
                    .unwrap_or_default();
                Var::Parts(parts, "/".into())
            }
            Derived::If { test, exists, then, or } => {
                let t = fill(test, vars);
                let yes = if *exists { Path::new(&t).exists() } else { !t.is_empty() };
                match if yes { then } else { or } {
                    Choice::One(s) => Var::One(fill(s, vars)),
                    Choice::Many(l) => {
                        Var::Many(l.iter().map(|s| fill(s, vars)).filter(|s| !s.is_empty()).collect())
                    }
                }
            }
        };
        vars.insert(r.name.clone(), value);
    }
    Ok(())
}

enum Cwd {
    Markers { nearest: Vec<String>, or: Vec<String> },
    Template(String),
}

/// One scope's templates (the test's, a group's or the file's).
#[derive(Default, Clone)]
struct Set {
    run: Option<Vec<String>>,
    debug: Option<toml::Value>,
    label: Option<String>,
}

impl Set {
    /// `self`, with what `o` sets instead.
    fn over(&self, o: &Set) -> Set {
        Set {
            run: o.run.clone().or_else(|| self.run.clone()),
            debug: o.debug.clone().or_else(|| self.debug.clone()),
            label: o.label.clone().or_else(|| self.label.clone()),
        }
    }
}

/// What `[test]` sets, and a variant can set instead.
#[derive(Default, Clone)]
struct Settings {
    test: Set,
    group: Set,
    file: Set,
    reports: Option<Vec<String>>,
    sources: Option<Vec<String>>,
    debug_args: Option<Vec<String>>,
    debug_attach: Option<AttachWait>,
}

/// A test run waiting for a debugger (`debug-attach`): the line that says where — `wait-for`, its `port`
/// group the port — and the way tarae attaches then (`debugpack::WAYS`).
#[derive(Clone, Debug)]
pub struct AttachWait {
    wait_for: regex::Regex,
    pub way: String,
}

impl PartialEq for AttachWait {
    fn eq(&self, o: &AttachWait) -> bool {
        (self.wait_for.as_str(), &self.way) == (o.wait_for.as_str(), &o.way)
    }
}

impl Eq for AttachWait {}

impl AttachWait {
    fn read(v: &toml::Value) -> Result<AttachWait, String> {
        let t = v.as_table().ok_or("`debug-attach` is `{ wait-for = '…', way = \"…\" }`")?;
        known(t, &["wait-for", "way"], "`debug-attach`")?;
        let way = string(t, "way")?.ok_or("`debug-attach` has no `way`")?;
        if !crate::debugpack::WAYS.contains(&way.as_str()) {
            let ways = crate::debugpack::WAYS.join(", ");
            return Err(format!("no `debug-attach` way `{way}` (there is: {ways})"));
        }
        let wait_for = string(t, "wait-for")?.ok_or("`debug-attach` has no `wait-for`")?;
        let wait_for = regex::Regex::new(&wait_for).map_err(|e| format!("`debug-attach.wait-for`: {e}"))?;
        if wait_for.capture_names().flatten().all(|g| g != "port") {
            return Err("`debug-attach.wait-for` has no `port` group".into());
        }
        Ok(AttachWait { wait_for, way })
    }

    /// The port `line` says the run waits on, if it's the line.
    pub fn port(&self, line: &str) -> Option<u16> {
        self.wait_for.captures(line)?.name("port")?.as_str().parse().ok()
    }
}

/// The keys `Settings` reads (in `[test]` and its variants).
const SETTINGS: &[&str] = &[
    "run",
    "run-group",
    "run-file",
    "debug",
    "debug-group",
    "debug-file",
    "label",
    "label-group",
    "label-file",
    "reports",
    "sources",
    "debug-args",
    "debug-attach",
];

impl Settings {
    fn read(t: &Table) -> Result<Settings, String> {
        Ok(Settings {
            test: set(t, "")?,
            group: set(t, "-group")?,
            file: set(t, "-file")?,
            reports: strings(t.get("reports"), "reports")?,
            sources: strings(t.get("sources"), "sources")?,
            debug_args: strings(t.get("debug-args"), "debug-args")?,
            debug_attach: t.get("debug-attach").map(AttachWait::read).transpose()?,
        })
    }

    fn over(&self, o: &Settings) -> Settings {
        let or = |a: &Option<Vec<String>>, b: &Option<Vec<String>>| a.clone().or_else(|| b.clone());
        Settings {
            test: self.test.over(&o.test),
            group: self.group.over(&o.group),
            file: self.file.over(&o.file),
            reports: or(&o.reports, &self.reports),
            sources: or(&o.sources, &self.sources),
            debug_args: or(&o.debug_args, &self.debug_args),
            debug_attach: o.debug_attach.clone().or_else(|| self.debug_attach.clone()),
        }
    }
}

struct Kind {
    /// In a run of the file's tests.
    file: bool,
    t: Set,
}

/// Whether `lang` has a pack with tests (before its grammar is loaded — for offering to download it); a
/// pack that can't be read says why.
pub fn has_tests(lang: &str) -> Result<bool, String> {
    Ok(manifest(lang).transpose()?.is_some_and(|t| t.contains_key("test")))
}

/// `lang`'s pack's `[test]`, its query compiled for `language` — `None` if there's no such pack.
pub fn tests(lang: &str, language: &Language) -> Option<Result<Tests, String>> {
    let t = match manifest(lang)? {
        Ok(t) if !t.contains_key("test") => return None,
        Ok(t) => t,
        Err(e) => return Some(Err(e)),
    };
    let read = |f: &str| crate::runtime::pack(&format!("{lang}/{f}"));
    Some(Tests::from_table(&t, language, read).map_err(|e| format!("the {lang} pack: {e}")))
}

/// How a runtime folder's pack counts for a top-level key — every key a pack may have is in `KEYS`, so a
/// new one has to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// Its value is used in place of the built-in one: a table whole (`[test]`, `[tasks]`), or by name —
    /// `[install.<tool>]` and `[grammar.<name>]` entry by entry, `[language.<name>]` and `[server.<name>]`
    /// key by key within the entry.
    Yours,
    /// Added to the built-in packs' and the core's own: nothing can be taken out — a key that narrows (more
    /// guarded).
    Added,
    /// The built-in packs' only, from the binary — a key that widens what Claude's runs may do. A runtime
    /// folder's is said and left out.
    BuiltIn,
}

/// Every top-level key of a `pack.toml`, and how a runtime folder's pack counts for it.
pub(crate) const KEYS: &[(&str, Reach)] = &[
    ("test", Reach::Yours),
    ("tasks", Reach::Yours),
    ("preflight", Reach::Yours),
    ("waits", Reach::Yours),
    // How a run's output says where things are (the list, `]q`)
    ("places", Reach::Yours),
    ("install", Reach::Yours),
    ("server", Reach::Yours),
    ("language", Reach::Yours),
    ("grammar", Reach::Yours),
    // How a language is debugged and its debugger found: the user's own debugging (F5, a test's, attach) —
    // never Claude's runs
    ("debug", Reach::Yours),
    ("debugger", Reach::Yours),
    ("config-files", Reach::Added),
    // Whose places feed the user's list: one of one's own only adds to it
    ("checks", Reach::Added),
    // Variables in every task's and test run's environment — Claude's too: env can change what runs
    ("color-off", Reach::BuiltIn),
    // What a code review grades high and quiet (`risk.rs`): one's own patterns make more changes high, its
    // quiet paths can't lower a high one
    ("review", Reach::Added),
    // Tools' output folders: skipped by walks — the token scan's among them, so one of one's own would hide
    // a token from it
    ("outputs", Reach::BuiltIn),
    // What a first build makes beside the pack's protected names (`target/`, `Cargo.lock`): on Linux made,
    // empty, before a run, where a run can't make a new entry (`sandbox/linux.rs`) — it widens what a run makes
    ("first-made", Reach::BuiltIn),
    // (narrows: becomes `Added` with `sandbox::protected_names`)
    ("protect", Reach::BuiltIn),
    // Tools' build folders: decides what a session's end deletes (`builds.rs`)
    ("builds", Reach::BuiltIn),
];

/// `key`'s reach — None: no pack has that key.
pub(crate) fn reach(key: &str) -> Option<Reach> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, r)| *r)
}

/// Tables of named entries whose entries go key by key (the others' entry by entry).
const KEY_BY_KEY: &[&str] = &["language", "server"];

/// `lang`'s `pack.toml`: the built-in one, with a runtime folder's tables in their place by their `Reach` —
/// so a pack of one's own can add tests without losing the built-in tasks, or the other way.
pub(crate) fn manifest(lang: &str) -> Option<Result<Table, String>> {
    let rel = format!("{lang}/pack.toml");
    let builtin = builtin_manifests().iter().find(|(l, _)| l == lang).map(|(_, t)| t.clone());
    let user = crate::runtime::pack_user(&rel).map(|s| read_pack(lang, &s, "yours"));
    merged_tables(builtin, user)
}

/// Every built-in pack's `pack.toml`, read once (they're in the binary and never change): (pack, table).
pub(crate) fn builtin_manifests() -> &'static [(String, Result<Table, String>)] {
    static READ: std::sync::OnceLock<Vec<(String, Result<Table, String>)>> = std::sync::OnceLock::new();
    READ.get_or_init(|| {
        crate::runtime::pack_manifests_builtin()
            .map(|(lang, text)| (lang.to_string(), read_pack(lang, text, "built in")))
            .collect()
    })
}

/// One `pack.toml`'s text, read: a table whose every top-level key is one a pack may have.
pub(crate) fn read_pack(lang: &str, text: &str, whose: &str) -> Result<Table, String> {
    let t = text.parse::<Table>().map_err(|e| format!("the {lang} pack ({whose}): {}", e.message()))?;
    match t.keys().find(|k| reach(k).is_none()) {
        Some(k) => Err(format!("the {lang} pack ({whose}) has no `{k}`")),
        None => Ok(t),
    }
}

/// The built-in `pack.toml` with the tables of one's own in their place.
#[cfg(test)]
pub(crate) fn merged(
    lang: &str,
    builtin: Option<String>,
    user: Option<String>,
) -> Option<Result<Table, String>> {
    let builtin = builtin.map(|s| read_pack(lang, &s, "built in"));
    merged_tables(builtin, user.map(|s| read_pack(lang, &s, "yours")))
}

fn merged_tables(
    builtin: Option<Result<Table, String>>,
    user: Option<Result<Table, String>>,
) -> Option<Result<Table, String>> {
    Some(match (builtin, user) {
        (None, None) => return None,
        (Some(b), None) => b,
        (None, Some(u)) => u.map(|mut u| {
            u.retain(|k, _| reach(k) != Some(Reach::BuiltIn));
            u
        }),
        (Some(b), Some(u)) => u.and_then(|u| {
            let mut b = b?;
            over(&mut b, u);
            Ok(b)
        }),
    })
}

/// A runtime folder's pack `u` over the built-in `b`, each key by its `Reach`.
pub(crate) fn over(b: &mut Table, u: Table) {
    for (key, mine) in u {
        match reach(&key) {
            Some(Reach::BuiltIn) | None => {}
            Some(Reach::Added) => match b.get_mut(&key) {
                Some(theirs) => add_value(theirs, mine),
                None => {
                    b.insert(key, mine);
                }
            },
            Some(Reach::Yours) => match (mine, b.get_mut(&key)) {
                // By name: one of one's own keeps the built-in others
                (toml::Value::Table(m), Some(toml::Value::Table(t))) if is_by_name(&key) => {
                    for (name, entry) in m {
                        match (entry, t.get_mut(&name)) {
                            (toml::Value::Table(e), Some(toml::Value::Table(te)))
                                if KEY_BY_KEY.contains(&key.as_str()) =>
                            {
                                te.extend(e)
                            }
                            (entry, _) => {
                                t.insert(name, entry);
                            }
                        }
                    }
                }
                (m, _) => {
                    b.insert(key, m);
                }
            },
        }
    }
}

/// Whether a `Yours` table holds entries by name (`[install.<tool>]`), not settings of its own (`[test]`).
fn is_by_name(key: &str) -> bool {
    ["install", "server", "language", "grammar", "debugger"].contains(&key)
}

/// `mine` added to `theirs`: lists joined (what's in both once), tables key by key.
pub(crate) fn add_value(theirs: &mut toml::Value, mine: toml::Value) {
    match (theirs, mine) {
        (toml::Value::Array(t), toml::Value::Array(m)) => {
            for v in m {
                if !t.contains(&v) {
                    t.push(v);
                }
            }
        }
        (toml::Value::Table(t), toml::Value::Table(m)) => {
            for (k, v) in m {
                match t.get_mut(&k) {
                    Some(tv) => add_value(tv, v),
                    None => {
                        t.insert(k, v);
                    }
                }
            }
        }
        // A different shape can't take anything out: the built-in one stays
        _ => {}
    }
}

/// A task a pack suggests: its name and command.
pub type Suggested = (String, String);

/// The tasks the packs suggest for a project at `root`: each pack's `[tasks]` whose `when` file `root`
/// has, by `rank` (then pack name). A pack that can't be read is logged and skipped.
///
/// `[tasks]`: `when` — the files that say it's this kind of project (any); `rank` — where its tasks go in
/// the list, lower first (50 unless set); `[[tasks.task]]` — `name` and `cmd` (run by `sh -c`), `needs`
/// (a program that must be on `PATH`), and `each = { json = "package.json", keys = "scripts" }`: a task for
/// each key of that object in that file, as `{key}` in `name` and `cmd` (quoted for `sh` there).
pub fn tasks(root: &Path) -> Vec<Suggested> {
    let mut found = Vec::new();
    for lang in crate::runtime::pack_langs() {
        let t = match manifest(&lang) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                crate::log::line("tarae", "warning", &e);
                continue;
            }
            None => continue,
        };
        let Some(tasks) = t.get("tasks") else { continue };
        match tasks_of(tasks, root) {
            Ok(Some((rank, list))) => found.push((rank, lang, list)),
            Ok(None) => {}
            Err(e) => crate::log::line("tarae", "warning", &format!("the {lang} pack: [tasks]: {e}")),
        }
    }
    crate::sorting::by(&mut found, |a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    found.into_iter().flat_map(|(_, _, list)| list).collect()
}

/// One pack's `[tasks]` for `root` — `None` if `root` isn't that kind of project.
fn tasks_of(v: &toml::Value, root: &Path) -> Result<Option<(i64, Vec<Suggested>)>, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    known(t, &["when", "rank", "task"], "[tasks]")?;
    let when = strings(t.get("when"), "when")?.unwrap_or_default();
    if !when.iter().any(|f| root.join(f).is_file()) {
        return Ok(None);
    }
    let rank = t.get("rank").map_or(Ok(50), |r| r.as_integer().ok_or("`rank` isn't a number"))?;
    let mut out = Vec::new();
    let empty = Vec::new();
    let list = match t.get("task") {
        Some(v) => v.as_array().ok_or("`task` isn't a list ([[tasks.task]])")?,
        None => &empty,
    };
    for task in list {
        let task = task.as_table().ok_or("a task isn't a table")?;
        known(task, &["name", "cmd", "needs", "each"], "[[tasks.task]]")?;
        let name = string(task, "name")?.ok_or("a task has no `name`")?;
        let cmd = string(task, "cmd")?.ok_or(format!("`{name}` has no `cmd`"))?;
        if string(task, "needs")?.is_some_and(|n| !on_path(&n)) {
            continue;
        }
        match task.get("each") {
            None => out.push((name, cmd)),
            Some(each) => {
                let e = each.as_table().ok_or(format!("`{name}`: `each` isn't a table"))?;
                known(e, &["json", "keys"], "`each`")?;
                let file = string(e, "json")?.ok_or(format!("`{name}`: `each` has no `json`"))?;
                let key = string(e, "keys")?.ok_or(format!("`{name}`: `each` has no `keys`"))?;
                let Ok(src) = std::fs::read_to_string(root.join(&file)) else { continue };
                let Ok(json) = serde_json::from_str::<Value>(&src) else { continue };
                // A key is the project's to name: quoted for `sh -c`
                for k in json[key.as_str()].as_object().into_iter().flat_map(|o| o.keys()) {
                    out.push((name.replace("{key}", k), cmd.replace("{key}", &sh_quote(k))));
                }
            }
        }
    }
    Ok(Some((rank, out)))
}

/// `:preflight`'s default steps for a project at `root` (`preflight.rs`), when it lists none of its own:
/// each pack's `[[preflight]]` whose `when` file `root` has, by `rank` (then pack name) — several kinds of
/// project, their steps together. Each step says where it came from (`rust pack`). A pack that can't be
/// read is logged and skipped. The commands are tarae's, what they run (tests, build scripts) the project's:
/// the first run in a project says so on a card (`preflight::DEFAULTS_SAY`). Only where the language's own
/// toolchain defines the check — Rust, Go, Java's installed Gradle and Maven; the other packs' pack.toml says
/// why they have none.
///
/// `[[preflight]]`: `when` — the files that say it's this kind of project (any); `unless` — files that say
/// the project runs it its own way (a build wrapper: the repository's own script, run only from the
/// project's trusted `[preflight]`), with `held` saying so when nothing else runs; `rank` (50 unless set);
/// `steps = [{ name, cmd, note? }]` as in `.tarae.toml`.
pub fn preflight(root: &Path) -> Defaults {
    let (mut found, mut held) = (Vec::new(), Vec::new());
    for lang in crate::runtime::pack_langs() {
        let Some(Ok(t)) = manifest(&lang) else { continue };
        let Some(list) = t.get("preflight") else { continue };
        match preflight_of(list, root, &lang, &mut held) {
            Ok(sets) => found.extend(sets.into_iter().map(|(rank, steps)| (rank, lang.clone(), steps))),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {lang} pack: [[preflight]]: {e}")),
        }
    }
    crate::sorting::by(&mut found, |a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    Defaults { steps: found.into_iter().flat_map(|(_, _, steps)| steps).collect(), held }
}

/// The packs' default preflight steps for a project, and why some were held back (`[[preflight]] held`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Defaults {
    pub steps: Vec<crate::preflight::Step>,
    pub held: Vec<String>,
}

/// What every built-in pack lists as running later outside the sandbox, or setting how things run
/// (`protect = [...]`: build scripts, manifests, wrappers) — every pack's, whatever the project's languages
/// (a Rust project can hold a `pom.xml`). Names, matched anywhere in the project in any case; one of several
/// parts (`gradle/wrapper`) matches them in a row. Built-in packs only, from the binary: a runtime folder's
/// pack can't take a name out (`llm.review-ask` adds yours). A name that can't be one (empty, absolute, `.`,
/// `..`) is a mistake in tarae's pack — left out and logged. `sandbox::protected_names` adds the core's.
pub fn protect() -> Vec<String> {
    let mut out = Vec::new();
    for (lang, text) in crate::runtime::pack_manifests_builtin() {
        match protect_of(text) {
            Ok(names) => out.extend(names),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {lang} pack: protect: {e}")),
        }
    }
    out
}

/// One pack's `[review]` (`risk.rs`): the languages it's for (its `[language.<name>]`s, else its own name),
/// the line patterns that make a change high (`risky`), the paths that are tests or docs (`quiet`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Review {
    pub pack: String,
    pub langs: Vec<String>,
    pub risky: Vec<String>,
    pub quiet: Vec<String>,
}

/// Every pack's `[review]` — a runtime folder's adds to the built-in one's (`Reach::Added`). A pack that
/// can't be read is logged and left out.
pub fn review() -> Vec<Review> {
    let mut out = Vec::new();
    for pack in crate::runtime::pack_langs() {
        let t = match manifest(&pack) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                crate::log::line("tarae", "warning", &e);
                continue;
            }
            None => continue,
        };
        let Some(r) = t.get("review") else { continue };
        let langs = match t.get("language").and_then(toml::Value::as_table) {
            Some(l) => l.keys().cloned().collect(),
            None => vec![pack.clone()],
        };
        let read = || -> Result<Review, String> {
            let r = r.as_table().ok_or("`review` isn't a table")?;
            known(r, &["risky", "quiet"], "[review]")?;
            Ok(Review {
                pack: pack.clone(),
                langs,
                risky: strings(r.get("risky"), "risky")?.unwrap_or_default(),
                quiet: strings(r.get("quiet"), "quiet")?.unwrap_or_default(),
            })
        };
        match read() {
            Ok(r) => out.push(r),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {pack} pack: {e}")),
        }
    }
    out
}

/// What one built-in pack says a project's first build makes beside its protected names (`protect`) — its
/// `first-made`: folders (`target/`, written with a `/`) and files written in place (`Cargo.lock`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MadeBeside {
    pub pack: String,
    pub protect: Vec<String>,
    pub folders: Vec<String>,
    pub files: Vec<String>,
}

/// Every built-in pack's `MadeBeside` that makes something — from the binary only: a runtime folder's pack
/// can't add one (`Reach::BuiltIn`). A pack that doesn't read is logged and left out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // (Landlock's — `sandbox/linux.rs`)
pub fn made_beside() -> Vec<MadeBeside> {
    let mut out = Vec::new();
    for (pack, text) in crate::runtime::pack_manifests_builtin() {
        match made_beside_of(pack, text) {
            Ok(m) if !m.protect.is_empty() && !(m.folders.is_empty() && m.files.is_empty()) => out.push(m),
            Ok(_) => {}
            Err(e) => crate::log::line("tarae", "warning", &format!("the {pack} pack: {e}")),
        }
    }
    out
}

/// One pack's `MadeBeside`, from its `pack.toml`'s text.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // (Landlock's — `sandbox/linux.rs`)
pub(crate) fn made_beside_of(pack: &str, text: &str) -> Result<MadeBeside, String> {
    let t = text.parse::<Table>().map_err(|e| e.message().to_string())?;
    let mut m = MadeBeside { pack: pack.to_string(), protect: protect_of(text)?, ..Default::default() };
    for name in strings(t.get("first-made"), "first-made")?.unwrap_or_default() {
        match name.strip_suffix('/') {
            Some(folder) => m.folders.push(folder.to_string()),
            None => m.files.push(name),
        }
    }
    Ok(m)
}

/// One pack's `protect` names (none: no `protect`).
fn protect_of(text: &str) -> Result<Vec<String>, String> {
    let t = text.parse::<Table>().map_err(|e| e.message().to_string())?;
    let names = strings(t.get("protect"), "protect")?.unwrap_or_default();
    let mut out = Vec::new();
    for name in names {
        let parts: Vec<&str> = name.split('/').collect();
        if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
            crate::log::line("tarae", "warning", &format!("protect: `{name}` isn't a name or names"));
            continue;
        }
        out.push(name);
    }
    Ok(out)
}

/// One pack's `[[preflight]]` entries that `root` is a project of: (rank, steps); those its `unless` holds
/// back say why in `held`.
fn preflight_of(
    v: &toml::Value,
    root: &Path,
    lang: &str,
    held: &mut Vec<String>,
) -> Result<Vec<(i64, Vec<crate::preflight::Step>)>, String> {
    let list = v.as_array().ok_or("isn't a list ([[preflight]])")?;
    let mut out = Vec::new();
    for entry in list {
        let t = entry.as_table().ok_or("an entry isn't a table")?;
        known(t, &["when", "unless", "held", "rank", "steps"], "[[preflight]]")?;
        let when = strings(t.get("when"), "when")?.unwrap_or_default();
        if !when.iter().any(|f| root.join(f).is_file()) {
            continue;
        }
        let unless = strings(t.get("unless"), "unless")?.unwrap_or_default();
        if unless.iter().any(|f| root.join(f).is_file()) {
            held.extend(string(t, "held")?);
            continue;
        }
        let rank = t.get("rank").map_or(Ok(50), |r| r.as_integer().ok_or("`rank` isn't a number"))?;
        let mut warnings = Vec::new();
        let steps = crate::preflight::parse_config(entry, &format!("{lang} pack"), &mut warnings);
        if let Some(w) = warnings.first() {
            return Err(w.clone());
        }
        out.push((rank, steps));
    }
    Ok(out)
}

/// What runs' output says they wait on (`waits.rs`) — every pack's `[[waits]]`, whatever the project: a
/// line only a cargo prints doesn't come from anything else. A pack that can't be read, or an entry that
/// doesn't hold, is logged and skipped.
///
/// `[[waits]]`: `line` — a regular expression the waiting line matches, its named groups (`(?<what>…)`)
/// put into the texts as `{what}`; `for` — what the run waits for, as its panel says it (`waiting for
/// cargo's lock`); `report` — as Claude's report says it (`Waited 16.0s for cargo's build directory lock`),
/// `for` unless set.
pub fn waits() -> Vec<crate::waits::Rule> {
    let mut out = Vec::new();
    for lang in crate::runtime::pack_langs() {
        let t = match manifest(&lang) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                crate::log::line("tarae", "warning", &e);
                continue;
            }
            None => continue,
        };
        let Some(list) = t.get("waits") else { continue };
        match waits_of(list) {
            Ok(rules) => out.extend(rules),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {lang} pack: [[waits]]: {e}")),
        }
    }
    out
}

/// The forms of place runs' output has beyond the one every tool shares (`task::Matcher`) — every pack's
/// `[[places]]` (`task::PlaceRule`), whatever the project: a task runs any tool. Read on a run's waiter
/// thread (a pack of one's own is a file). A pack that can't be read, or an entry that doesn't hold, is
/// logged and left out.
pub fn places() -> Vec<crate::task::PlaceRule> {
    let mut out = Vec::new();
    for lang in crate::runtime::pack_langs() {
        let Some(Ok(t)) = manifest(&lang) else { continue };
        let Some(list) = t.get("places") else { continue };
        match places_of(list) {
            Ok(rules) => out.extend(rules),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {lang} pack: [[places]]: {e}")),
        }
    }
    out
}

pub(crate) fn places_of(v: &toml::Value) -> Result<Vec<crate::task::PlaceRule>, String> {
    let list = v.as_array().ok_or("isn't a list ([[places]])")?;
    let entry = |e: &toml::Value| crate::task::PlaceRule::read(e.as_table().ok_or("an entry isn't a table")?);
    list.iter().map(entry).collect()
}

fn waits_of(v: &toml::Value) -> Result<Vec<crate::waits::Rule>, String> {
    let list = v.as_array().ok_or("isn't a list ([[waits]])")?;
    let mut out = Vec::new();
    for entry in list {
        let t = entry.as_table().ok_or("an entry isn't a table")?;
        known(t, &["line", "for", "report"], "[[waits]]")?;
        let line = string(t, "line")?.ok_or("an entry has no `line`")?;
        let line = regex::Regex::new(&line).map_err(|e| format!("`line`: {e}"))?;
        let said = string(t, "for")?.ok_or("an entry has no `for`")?;
        out.push(crate::waits::Rule::new(line, said, string(t, "report")?));
    }
    Ok(out)
}

/// Whether `cmd` is a program on `PATH`.
pub fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

/// How to install a tool a language needs (a language server, a debugger) — a pack's `[install.<tool>]`:
///
/// - `what`: what it is ("Go's language server")
/// - `ways`: how to install it, in order — each `{ run = [...], needs = "go", bin = ["~/go/bin"] }`, taken if
///   `needs` (else `run`'s program) is on `PATH`. `bin`: where it lands when that isn't on `PATH` (`~` and
///   `${VAR}` expanded), so tarae finds it there. `project = true`: run in the file's folder, not the home
///   one (rustup: the project's toolchain). Or a `download` way — tarae fetches a release itself
///   (`fetch.rs`, `download_way`), taken if what it needs is on `PATH`.
/// - `hint`: what to tell when no way fits ("comes with Xcode's command line tools").
/// - `missing-says`: what the program prints when it's there but not installed (a rustup proxy whose
///   component isn't): a line holding one of these offers it as if it weren't there (`missing_says`).
/// - `look-for`: a tool that's a file, not a program (java-debug's jar: `name-*.jar`, the newest by its
///   version) — in its download's folder, then the `look-in` folders (other editors' copies; a `*` per part).
///
/// The tool is named as it's run (`gopls`, `basedpyright-langserver`). tarae only offers: `y` runs `run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub tool: String,
    pub what: String,
    /// The first way this machine can take, when it runs something.
    pub run: Option<Vec<String>>,
    /// … or when it's a download.
    pub download: Option<crate::fetch::Download>,
    /// That way runs in the file's folder (`project = true`: rustup, for the project's toolchain) — else in
    /// the home folder, where a project's settings (an `.npmrc`) don't reach.
    pub in_project: bool,
    pub hint: Option<String>,
    /// Every way as it's said, fitting here or not — what to say when none does.
    pub every: Vec<String>,
}

impl Install {
    /// Where no way fits: `what` isn't installed, and how one gets it (every way, then the hint).
    pub fn no_way(&self) -> String {
        let mut how = self.every.clone();
        how.extend(self.hint.clone());
        match how.is_empty() {
            true => format!("{} isn't installed", self.what),
            false => format!("{} isn't installed: {}", self.what, how.join(", ")),
        }
    }
}

/// `tool`'s `[install.<tool>]`, from whichever pack has it (a pack that can't be read is logged).
pub fn install(tool: &str) -> Option<Install> {
    install_in(tool, install_tables()).map(|i| i.pick(on_path))
}

/// What `tool` prints when it's there but not installed (`[install.<tool>] missing-says`) — none: it doesn't.
/// Reads the packs: a worker's (a server's stderr thread).
pub fn missing_says(tool: &str) -> Vec<String> {
    install_in(tool, install_tables()).map(|r| r.missing_says).unwrap_or_default()
}

/// Where `tool` is when it isn't on `PATH`: in a `bin` of one of its ways (`go install` → `~/go/bin`), or
/// where a `download` way put it (`fetched_dir`) — or for a file that isn't a program (`look-for`), the newest
/// there or in `look-in`. Disk: a worker's, or once (java-debug's jar at F5 — `java::debug_bundle`).
pub fn installed_at(tool: &str) -> Option<PathBuf> {
    let found = install_in(tool, install_tables())?;
    let fetched = fetched_dir(tool);
    found_in(tool, &found.bins, fetched.as_deref()).or_else(|| {
        let dirs: Vec<PathBuf> =
            fetched.into_iter().chain(found.look_in.iter().filter_map(|d| expand_dir(d))).collect();
        newest_in(found.look_for.as_deref()?, &dirs)
    })
}

/// The first of `bins`' `tool`, then the program a download put in `fetched`, that's there.
fn found_in(tool: &str, bins: &[PathBuf], fetched: Option<&Path>) -> Option<PathBuf> {
    bins.iter().map(|d| d.join(tool)).find(|p| p.is_file()).or_else(|| crate::fetch::installed(fetched?))
}

/// Where a `download` way puts a tool: `<data>/<tool>`.
pub fn fetched_dir(tool: &str) -> Option<PathBuf> {
    Some(crate::runtime::data_dir()?.join(tool))
}

/// Every pack's `[install]`.
fn install_tables() -> Vec<(String, Table)> {
    tables_of("install")
}

/// Every pack's `[<key>]` (a pack that can't be read is logged).
pub(crate) fn tables_of(key: &str) -> Vec<(String, Table)> {
    let mut out = Vec::new();
    for lang in crate::runtime::pack_langs() {
        match manifest(&lang) {
            Some(Ok(t)) => {
                if let Some(i) = t.get(key).and_then(|v| v.as_table()) {
                    out.push((lang, i.clone()));
                }
            }
            Some(Err(e)) => crate::log::line("tarae", "warning", &e),
            None => {}
        }
    }
    out
}

/// How a language server is told to leave alone what isn't the project — a pack's `[server.<name>]`
/// `leave-out` (the server named as tarae names it: `rust-analyzer`, `jdtls`, `pyright`, `gopls`):
///
/// - `init = "files.exclude"`: a dotted path in its `initializationOptions`; or `config =
///   "python.analysis.exclude"`: in the settings `workspace/configuration` answers, by section.
/// - `each`: one folder as the setting takes it, `{rel}` being the folder from the server's root
///   (`"-{rel}"`) — or a list of them, each put in (jdtls matches the folder holding a build file, so
///   `"**/{rel}"` and `"**/{rel}/**"` both: measured, `**/ext/clone/**` alone still imported its pom.xml).
/// - `keep`: the setting's own defaults — giving it a list replaces them, so they go first.
///
/// The folders are `lsp::nested_checkouts`'s: agents' worktrees and checkouts inside the project (not
/// submodules). Nothing is set when there are none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaveOut {
    pub place: Place,
    pub each: Vec<String>,
    pub keep: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// A dotted path in `initializationOptions`.
    Init(String),
    /// A dotted path in the settings answered by section.
    Config(String),
}

/// `server`'s `leave-out`, from whichever pack has it (a broken one is logged and skipped).
pub fn leave_out(server: &str) -> Option<LeaveOut> {
    leave_out_in(server, tables_of("server"))
}

fn leave_out_in(server: &str, tables: Vec<(String, Table)>) -> Option<LeaveOut> {
    tables.into_iter().find_map(|(lang, t)| {
        let v = t.get(server)?;
        server_table(v)
            .inspect_err(|e| {
                crate::log::line("tarae", "warning", &format!("the {lang} pack: [server.{server}]: {e}"))
            })
            .ok()
            .flatten()
    })
}

fn server_table(v: &toml::Value) -> Result<Option<LeaveOut>, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    // (the others: the language table's — `languages.rs`)
    known(t, crate::languages::SERVER_KEYS, "it")?;
    let Some(l) = t.get("leave-out") else { return Ok(None) };
    let l = l.as_table().ok_or("`leave-out` isn't a table")?;
    known(l, &["init", "config", "each", "keep"], "`leave-out`")?;
    let place = match (string(l, "init")?, string(l, "config")?) {
        (Some(p), None) => Place::Init(p),
        (None, Some(p)) => Place::Config(p),
        _ => return Err("`leave-out` takes one of `init` or `config`".into()),
    };
    let each = match l.get("each") {
        Some(toml::Value::String(s)) => vec![s.clone()],
        v @ Some(_) => strings(v, "each")?.unwrap_or_default(),
        None => return Err("`leave-out` has no `each`".into()),
    };
    if each.is_empty() || each.iter().any(|e| !e.contains("{rel}")) {
        return Err("`each` has no `{rel}`".into());
    }
    let keep = strings(l.get("keep"), "keep")?.unwrap_or_default();
    Ok(Some(LeaveOut { place, each, keep }))
}

/// A tool's `[install.<tool>]`, read: every way, and every folder it may land in.
#[derive(Debug)]
struct Recipe {
    tool: String,
    what: String,
    ways: Vec<Way>,
    bins: Vec<PathBuf>,
    hint: Option<String>,
    missing_says: Vec<String>,
    /// `look-for`: a file that isn't a program (java-debug's jar, `name-*.jar`), the newest by the version in
    /// its name — in its download's folder, then the `look-in` folders (other editors' copies; one `*` per
    /// part of a path).
    look_for: Option<String>,
    look_in: Vec<String>,
}

#[derive(Debug)]
enum Way {
    Run { run: Vec<String>, needs: String, project: bool },
    Download(Box<crate::fetch::Download>),
}

impl Recipe {
    /// The first way whose program `has` (a download: whose tools it has).
    fn pick(self, has: impl Fn(&str) -> bool) -> Install {
        let every = self
            .ways
            .iter()
            .map(|w| match w {
                Way::Run { run, .. } => crate::install::shown(run),
                Way::Download(d) => d.shown(),
            })
            .collect();
        let way = self.ways.into_iter().find(|w| match w {
            Way::Run { needs, .. } => has(needs),
            Way::Download(d) => d.fits(&has),
        });
        let (mut run, mut download, mut in_project) = (None, None, false);
        match way {
            Some(Way::Run { run: r, project, .. }) => (run, in_project) = (Some(r), project),
            Some(Way::Download(d)) => download = Some(*d),
            None => {}
        }
        Install { tool: self.tool, what: self.what, run, download, in_project, hint: self.hint, every }
    }
}

/// The first pack's `[install.<tool>]` in `tables` ((pack, its `[install]`)); a broken one is logged and
/// skipped.
fn install_in(tool: &str, tables: Vec<(String, Table)>) -> Option<Recipe> {
    tables.into_iter().find_map(|(lang, t)| {
        let v = t.get(tool)?;
        recipe(tool, v)
            .inspect_err(|e| {
                crate::log::line("tarae", "warning", &format!("the {lang} pack: [install.{tool}]: {e}"))
            })
            .ok()
    })
}

fn recipe(tool: &str, v: &toml::Value) -> Result<Recipe, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    known(t, &["what", "ways", "hint", "missing-says", "look-for", "look-in"], "it")?;
    let what = string(t, "what")?.ok_or("has no `what`")?;
    let mut ways = Vec::new();
    let mut bins = Vec::new();
    for w in
        t.get("ways").map_or(Ok(&[][..]), |w| w.as_array().map(Vec::as_slice).ok_or("`ways` isn't a list"))?
    {
        let w = w.as_table().ok_or("a way isn't a table")?;
        if w.contains_key("download") {
            ways.push(Way::Download(Box::new(download_way(w)?)));
            continue;
        }
        known(w, &["run", "needs", "bin", "project"], "a way")?;
        let run = strings(w.get("run"), "run")?.filter(|r| !r.is_empty()).ok_or("a way has no `run`")?;
        let needs = string(w, "needs")?.unwrap_or_else(|| run[0].clone());
        bins.extend(strings(w.get("bin"), "bin")?.unwrap_or_default().iter().filter_map(|d| expand_dir(d)));
        let project = match w.get("project") {
            None => false,
            Some(p) => p.as_bool().ok_or("`project` is true or false")?,
        };
        ways.push(Way::Run { run, needs, project });
    }
    let missing_says = strings(t.get("missing-says"), "missing-says")?.unwrap_or_default();
    let look_for = string(t, "look-for")?;
    if look_for.as_ref().is_some_and(|f| f.contains('/') || f.matches('*').count() != 1) {
        return Err("`look-for` is a file name with one `*` (where its version is)".into());
    }
    let look_in = strings(t.get("look-in"), "look-in")?.unwrap_or_default();
    if !look_in.is_empty() && look_for.is_none() {
        return Err("`look-in` needs `look-for` (the file to look for there)".into());
    }
    Ok(Recipe {
        tool: tool.to_string(),
        what,
        ways,
        bins,
        hint: string(t, "hint")?,
        missing_says,
        look_for,
        look_in,
    })
}

/// The newest file named as `pattern` (one `*`: its version) in `dirs` — each a path whose parts may hold one
/// `*` (`~/.vscode/extensions/vscjava.vscode-java-debug-*/server`), `~` and `${VAR}` expanded.
fn newest_in(pattern: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let (pre, post) = pattern.split_once('*')?;
    let version = |p: &Path| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let v = n.get(pre.len()..n.len().saturating_sub(post.len())).unwrap_or_default();
        v.split(|c: char| !c.is_ascii_digit())
            .filter(|x| !x.is_empty())
            .map(|x| x.parse().unwrap_or(0))
            .collect::<Vec<u64>>()
    };
    dirs.iter()
        .flat_map(|d| globbed(d))
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| glob(pattern, n)) && p.is_file())
        .max_by_key(|p| version(p))
}

/// The folders a path with `*`s in its parts names (`a/b-*/c` → every `a/b-…/c` there is).
fn globbed(path: &Path) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for part in path.components() {
        let part = part.as_os_str().to_string_lossy();
        if !part.contains('*') {
            found.iter_mut().for_each(|f| f.push(&*part));
            continue;
        }
        found = found
            .iter()
            .filter_map(|f| std::fs::read_dir(f).ok())
            .flatten()
            .flatten()
            .filter(|e| glob(&part, &e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
    }
    found
}

/// A `download` way (docs/languages.md, "Downloads"): where the file is, how its version and name are found,
/// where its checksum is, how it's unpacked and where the program is in it; what it needs and runs on.
fn download_way(w: &Table) -> Result<crate::fetch::Download, String> {
    use crate::fetch::{Download, Name, Pick, Runtime, Sum, Unpack, Version, check_template};
    known(
        w,
        &["download", "version", "file", "sha256", "size", "unpack", "program", "needs", "runtime", "rename"],
        "a download",
    )?;
    let template = |key: &str| -> Result<String, String> {
        let t = string(w, key)?.ok_or_else(|| format!("a download has no `{key}`"))?;
        check_template(&t)?;
        Ok(t)
    };
    let pick = |v: &toml::Value, key: &str| -> Result<Pick, String> {
        let t = v.as_table().ok_or_else(|| format!("`{key}` isn't a table"))?;
        known(t, &["from", "pick"], &format!("`{key}`"))?;
        let from = string(t, "from")?.ok_or_else(|| format!("`{key}` has no `from`"))?;
        let pick = string(t, "pick")?.ok_or_else(|| format!("`{key}` has no `pick`"))?;
        check_template(&from)?;
        regex::Regex::new(&pick).map_err(|e| format!("`{key}.pick`: {e}"))?;
        Ok(Pick { from, pick })
    };
    let url = template("download")?;
    // A version pinned (a newer one is a pack change), or how the newest is found
    let version = match w.get("version").ok_or("a download has no `version`")? {
        toml::Value::String(v) => Version::Pinned(v.clone()),
        v => Version::Newest(pick(v, "version")?),
    };
    // A SHA-256 pinned (64 hex digits), or where the published one is
    let sha256 = match string(w, "sha256")?.ok_or("a download has no `sha256`")? {
        h if crate::fetch::is_sha256(&h) => match version {
            Version::Pinned(_) => Sum::Pinned(h),
            Version::Newest(_) => return Err("a pinned `sha256` needs a pinned `version`".into()),
        },
        at => {
            check_template(&at)?;
            Sum::Published(at)
        }
    };
    let size = match w.get("size") {
        None => None,
        Some(v) => {
            Some(v.as_integer().and_then(|n| u64::try_from(n).ok()).ok_or("`size` is a number of bytes")?)
        }
    };
    let file = match w.get("file") {
        Some(toml::Value::String(s)) => {
            check_template(s)?;
            Name::Is(s.clone())
        }
        Some(v) => Name::Pick(pick(v, "file")?),
        None => return Err("a download has no `file`".into()),
    };
    let unpack = match string(w, "unpack")?.as_deref() {
        Some("tar.gz") => Unpack::TarGz,
        Some("file") | None => Unpack::File,
        Some(u) => return Err(format!("`unpack` is \"tar.gz\" or \"file\", not {u:?}")),
    };
    let needs = match w.get("needs") {
        Some(toml::Value::String(s)) => vec![s.clone()],
        v => strings(v, "needs")?.unwrap_or_default(),
    };
    let runtime = match w.get("runtime") {
        None => None,
        Some(v) => {
            let t = v.as_table().ok_or("`runtime` isn't a table")?;
            known(t, &["program", "home", "ask", "version", "at-least", "get"], "`runtime`")?;
            let need = |k: &str| string(t, k)?.ok_or_else(|| format!("`runtime` has no `{k}`"));
            let version = need("version")?;
            regex::Regex::new(&version).map_err(|e| format!("`runtime.version`: {e}"))?;
            let at_least =
                t.get("at-least").and_then(|v| v.as_integer()).ok_or("`runtime` has no `at-least` number")?;
            Some(Runtime {
                program: need("program")?,
                home: string(t, "home")?,
                ask: strings(t.get("ask"), "ask")?.unwrap_or_default(),
                version,
                at_least: u64::try_from(at_least).map_err(|_| "`at-least` is a version, not below 0")?,
                get: need("get")?,
            })
        }
    };
    let rename = match w.get("rename") {
        None => Vec::new(),
        Some(v) => v
            .as_table()
            .ok_or("`rename` isn't a table")?
            .iter()
            .map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())).ok_or("`rename`'s values are names"))
            .collect::<Result<_, _>>()?,
    };
    Ok(Download {
        url,
        version,
        file,
        sha256,
        size,
        unpack,
        program: template("program")?,
        needs,
        runtime,
        rename,
    })
}

/// `~/go/bin`, `${GOPATH}/bin` → a folder (None if a variable isn't set). A variable holding a list of
/// folders (`GOPATH=/a:/b`) gives its first, where tools install to.
pub(crate) fn expand_dir(d: &str) -> Option<PathBuf> {
    let home = || std::env::var("HOME").ok().filter(|h| !h.is_empty());
    let d = match d.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home()?),
        None => d.to_string(),
    };
    let mut out = String::new();
    let mut rest = d.as_str();
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find('}')? + i;
        let v = std::env::var_os(&rest[i + 2..end])?;
        let first = std::env::split_paths(&v).next().filter(|p| !p.as_os_str().is_empty())?;
        out.push_str(&first.to_string_lossy());
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(PathBuf::from(out))
}

/// `it's` → `'it'\''s'`: one word to `sh`, whatever it holds.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A key `t` doesn't know is a mistake to say, not to skip (`runn = [...]`).
pub(crate) fn known(t: &Table, keys: &[&str], what: &str) -> Result<(), String> {
    match t.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(format!("{what} has no `{k}`")),
        None => Ok(()),
    }
}

pub(crate) fn strings(v: Option<&toml::Value>, what: &str) -> Result<Option<Vec<String>>, String> {
    let Some(v) = v else { return Ok(None) };
    let arr = v.as_array().ok_or(format!("`{what}` isn't a list"))?;
    let each = |x: &toml::Value| x.as_str().map(str::to_string).ok_or(format!("`{what}` holds a non-string"));
    arr.iter().map(each).collect::<Result<_, _>>().map(Some)
}

pub(crate) fn string(t: &Table, key: &str) -> Result<Option<String>, String> {
    match t.get(key) {
        None => Ok(None),
        Some(v) => v.as_str().map(|s| Some(s.to_string())).ok_or(format!("`{key}` isn't a string")),
    }
}

/// The templates of one scope: `run{sfx}`, `debug{sfx}`, `label{sfx}`.
fn set(t: &Table, sfx: &str) -> Result<Set, String> {
    Ok(Set {
        run: strings(t.get(&format!("run{sfx}")), &format!("run{sfx}"))?,
        debug: t.get(&format!("debug{sfx}")).cloned(),
        label: string(t, &format!("label{sfx}"))?,
    })
}

const HELPERS: &[&str] = &["cargo"];

impl Tests {
    /// Reads a pack's `[test]` from its `pack.toml`; `file` reads the pack's other files (its query).
    #[cfg(test)]
    pub fn parse(
        toml: &str,
        language: &Language,
        file: impl Fn(&str) -> Option<String>,
    ) -> Result<Tests, String> {
        let root: Table = toml.parse().map_err(|e: toml::de::Error| e.message().to_string())?;
        Tests::from_table(&root, language, file)
    }

    fn from_table(
        root: &Table,
        language: &Language,
        file: impl Fn(&str) -> Option<String>,
    ) -> Result<Tests, String> {
        let t = root.get("test").and_then(|v| v.as_table()).ok_or("no [test]")?;
        let own = [
            "query",
            "files",
            "cwd",
            "helpers",
            "parse",
            "group-sep",
            "variant-by",
            "variant",
            "kind",
            "debug-build",
            "vars",
            "line",
            "none-ran",
        ];
        known(t, &[&own[..], SETTINGS].concat(), "[test]")?;
        let name = string(t, "query")?.ok_or("[test] has no `query`")?;
        let src = file(&name).ok_or(format!("its query `{name}` is missing"))?;
        let query = Query::new(language, &src).map_err(|e| format!("{name}: {e}"))?;
        if query.capture_index_for_name("name").is_none()
            || (query.capture_index_for_name("test").is_none()
                && query.capture_index_for_name("group").is_none())
        {
            return Err(format!("{name} captures no @test (or @group) and @name"));
        }
        let file_dir = || Cwd::Markers { nearest: Vec::new(), or: Vec::new() };
        let cwd = match t.get("cwd") {
            None => file_dir(),
            Some(v) if v.as_str() == Some("file-dir") => file_dir(),
            Some(toml::Value::String(s)) => Cwd::Template(s.clone()),
            Some(v) => {
                let c = v.as_table().ok_or("`cwd` is \"file-dir\", { nearest = [...] } or a template")?;
                known(c, &["nearest", "or"], "`cwd`")?;
                Cwd::Markers {
                    nearest: strings(c.get("nearest"), "cwd.nearest")?.unwrap_or_default(),
                    or: strings(c.get("or"), "cwd.or")?.unwrap_or_default(),
                }
            }
        };
        let helpers = strings(t.get("helpers"), "helpers")?.unwrap_or_default();
        if let Some(h) = helpers.iter().find(|h| !HELPERS.contains(&h.as_str())) {
            return Err(format!("no helper `{h}` (there are: {})", HELPERS.join(", ")));
        }
        let mut kinds = HashMap::new();
        if let Some(ks) = t.get("kind") {
            for (k, v) in ks.as_table().ok_or("[test.kind] isn't a table")? {
                let kt = v.as_table().ok_or(format!("[test.kind.{k}] isn't a table"))?;
                known(kt, &["file", "run", "debug", "label"], &format!("[test.kind.{k}]"))?;
                let file = match kt.get("file") {
                    None => true,
                    Some(f) => f.as_bool().ok_or(format!("[test.kind.{k}]: `file` is true or false"))?,
                };
                kinds.insert(k.clone(), Kind { file, t: set(kt, "")? });
            }
        }
        let mut variants = HashMap::new();
        let mut variant_vars = HashMap::new();
        if let Some(vs) = t.get("variant") {
            for (k, v) in vs.as_table().ok_or("[test.variant] isn't a table")? {
                let vt = v.as_table().ok_or(format!("[test.variant.{k}] isn't a table"))?;
                known(vt, &[SETTINGS, &["vars"]].concat(), &format!("[test.variant.{k}]"))?;
                variants.insert(k.clone(), Settings::read(vt)?);
                variant_vars
                    .insert(k.clone(), derive_rules(vt.get("vars"), &format!("[test.variant.{k}] "))?);
            }
        }
        let parse = string(t, "parse")?.unwrap_or_default();
        if !parse.is_empty() && parse != "junit" {
            return Err(format!(
                "no `parse = \"{parse}\"` (there is: junit — output read a line at a time is [[test.line]])"
            ));
        }
        let lines = crate::test_results::Rules::read(t)?;
        if lines.is_some() && parse == "junit" {
            return Err("`parse = \"junit\"` or [[test.line]], not both".into());
        }
        let base = Settings::read(t)?;
        let variant_by = string(t, "variant-by")?;
        // What a run can end up with: the base alone, or with a variant over it
        let each: Vec<Settings> = match &variant_by {
            Some(_) if !variants.is_empty() => variants.values().map(|v| base.over(v)).collect(),
            _ => vec![base.clone()],
        };
        if parse == "junit" && each.iter().any(|s| s.reports.is_none()) {
            return Err("`parse = \"junit\"` needs `reports`".into());
        }
        if each.iter().any(|s| s.debug_args.is_some() && s.debug_attach.is_none()) {
            return Err("`debug-args` needs `debug-attach` (how to attach once the test waits)".into());
        }
        let debug_build = match t.get("debug-build") {
            None => None,
            Some(b) => Some(crate::debugpack::build_of(b, false).map_err(|e| format!("`debug-build`: {e}"))?),
        };
        let built = {
            let mut sets =
                each.iter().flat_map(|s| [&s.test, &s.group, &s.file]).chain(kinds.values().map(|k| &k.t));
            sets.any(|s| s.debug.as_ref().is_some_and(builds))
        };
        if debug_build.is_none() && built {
            return Err("`{built}` in a `debug` template needs `debug-build`".into());
        }
        Ok(Tests {
            query,
            files: strings(t.get("files"), "files")?.unwrap_or_default(),
            cwd,
            helpers,
            parse,
            lines,
            debug_build,
            group_sep: string(t, "group-sep")?.unwrap_or_else(|| "::".into()),
            base,
            variant_by,
            variants,
            kinds,
            vars: derive_rules(t.get("vars"), "")?,
            variant_vars,
        })
    }

    /// The query's `@var.<name>` captures in the file: the first one's text each — empty when none.
    fn captured(&self, tree: &Tree, src: &str) -> HashMap<String, Var> {
        let names = self.query.capture_names();
        let mut out: HashMap<String, Var> = names
            .iter()
            .filter_map(|n| n.strip_prefix("var."))
            .map(|n| (n.to_string(), Var::One(String::new())))
            .collect();
        if out.is_empty() {
            return out;
        }
        let mut qc = QueryCursor::new();
        let mut matches = qc.matches(&self.query, tree.root_node(), src.as_bytes());
        while let Some(m) = matches.next() {
            for c in m.captures() {
                let Some(name) = names[c.index as usize].strip_prefix("var.") else { continue };
                if matches!(out.get(name), Some(Var::One(s)) if s.is_empty())
                    && let Ok(text) = c.node.utf8_text(src.as_bytes())
                {
                    out.insert(name.to_string(), Var::One(text.to_string()));
                }
            }
        }
        out
    }

    /// The test at `cursor` (byte) in `path` — else the group around it, else the file's tests; with
    /// `whole_file`, the file's.
    pub fn find(
        &self,
        path: &Path,
        src: &str,
        tree: &Tree,
        cursor: usize,
        whole_file: bool,
    ) -> Result<Found, String> {
        let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !self.files.is_empty() && !self.files.iter().any(|g| glob(g, &filename)) {
            return Err(format!("tests live in {} files", self.files.join(" or ")));
        }
        let dir = path.parent().unwrap_or(path).to_path_buf();
        let near = |marks: &[String]| {
            (!marks.is_empty()).then(|| up_to(path, |d| marks.iter().any(|m| d.join(m).exists()))).flatten()
        };
        // (a template's comes from the variables)
        let first = match &self.cwd {
            Cwd::Markers { nearest, or } => near(nearest).or_else(|| near(or)).unwrap_or(dir),
            Cwd::Template(_) => dir,
        };
        let mut vars = self.helper_vars(path, tree)?;
        vars.extend(self.captured(tree, src));
        // (where it runs, when that's found by markers: `vars` may use it — `{cwd}/.venv/bin/python`)
        if let Cwd::Markers { .. } = &self.cwd {
            vars.insert("cwd".into(), Var::One(first.display().to_string()));
        }
        derive(&self.vars, path, &mut vars)?;
        let cwd = match &self.cwd {
            Cwd::Template(t) => {
                let c = fill(t, &vars);
                if c.is_empty() || c.contains('{') {
                    return Err(format!("`cwd = \"{t}\"`: no helper gives what it names"));
                }
                PathBuf::from(c)
            }
            Cwd::Markers { .. } => first,
        };
        let rel = path.strip_prefix(&cwd).unwrap_or(path).display().to_string();
        for (k, v) in [
            ("cwd", cwd.display().to_string()),
            ("file", path.display().to_string()),
            ("filename", filename.clone()),
            ("rel", rel),
        ] {
            vars.insert(k.into(), Var::One(v));
        }
        let variant = self.variant_by.as_ref().map(|v| fill(v, &vars));
        let settings = match variant.as_ref().map(|v| (v, self.variants.get(v))) {
            Some((v, Some(o))) => {
                derive(self.variant_vars.get(v).map_or(&[][..], Vec::as_slice), path, &mut vars)?;
                self.base.over(o)
            }
            Some((v, None)) => return Err(format!("no [test.variant.{v}]")),
            None => self.base.clone(),
        };

        // Every test and group in the file
        let items = self.items(tree, src);
        let (tests, groups): (Vec<&Item>, Vec<&Item>) = items.iter().partition(|i| i.test);
        let around = |r: &std::ops::Range<usize>, pos: usize| r.start <= pos && pos <= r.end;
        // The outermost test around the cursor (a helper function inside a test isn't one)
        let test =
            tests.iter().filter(|t| around(&t.range, cursor)).max_by_key(|t| t.range.end - t.range.start);
        let groups_of = |pos: usize| {
            let mut g: Vec<&&Item> = groups.iter().filter(|g| around(&g.range, pos)).collect();
            crate::sorting::by_key(&mut g, |g| g.range.start);
            g.iter().map(|g| g.name.clone()).collect::<Vec<_>>()
        };
        let first_group = groups.iter().min_by_key(|g| g.range.start).map(|g| g.name.clone());
        vars.insert("first-group".into(), Var::One(first_group.clone().unwrap_or_default()));
        let sep = &self.group_sep;

        let kind = |k: &Option<String>| k.as_ref().and_then(|k| self.kinds.get(k));
        let (set, label_default) = match (test, whole_file) {
            (Some(t), false) => {
                let gs = groups_of(t.range.start);
                let path_ = gs.iter().cloned().chain([t.name.clone()]).collect();
                vars.insert("name".into(), Var::One(t.name.clone()));
                vars.insert("groups".into(), Var::Parts(gs, sep.clone()));
                vars.insert("path".into(), Var::Parts(path_, sep.clone()));
                let set = match kind(&t.kind) {
                    Some(k) => settings.test.over(&k.t),
                    None => settings.test.clone(),
                };
                (set, "{name}")
            }
            _ => {
                let gs = if whole_file { Vec::new() } else { groups_of(cursor) };
                let mut names: Vec<String> = Vec::new();
                for t in tests.iter().filter(|t| kind(&t.kind).is_none_or(|k| k.file)) {
                    if !names.contains(&t.name) {
                        names.push(t.name.clone());
                    }
                }
                let none = names.is_empty();
                vars.insert("names".into(), Var::Any(names));
                if gs.is_empty() || settings.group.run.is_none() {
                    if (none && uses(&settings.file, "{names"))
                        || (first_group.is_none() && uses(&settings.file, "{first-group"))
                    {
                        return Err("no tests in this file".into());
                    }
                    (settings.file.clone(), "{filename}")
                } else {
                    vars.insert("groups".into(), Var::Parts(gs.clone(), sep.clone()));
                    vars.insert("path".into(), Var::Parts(gs, sep.clone()));
                    (settings.group.clone(), "{groups}")
                }
            }
        };
        let run =
            set.run.as_ref().ok_or("the pack has no command for this (`run`, `run-group`, `run-file`)")?;
        let build = match (&set.debug, &self.debug_build) {
            (Some(d), Some(b)) if builds(d) => Some(b.fill(&vars)),
            _ => None,
        };
        let argv = args(run, &vars);
        // A `debug` template of this scope's own comes first
        let debug_run = match (&set.debug, &settings.debug_args, &settings.debug_attach) {
            (None, Some(extra), Some(attach)) => {
                Some((argv.iter().cloned().chain(args(extra, &vars)).collect(), attach.clone()))
            }
            _ => None,
        };
        // Relative to where the test runs
        let paths = |v: &Option<Vec<String>>| -> Vec<PathBuf> {
            v.iter().flatten().map(|p| cwd.join(fill(p, &vars))).collect()
        };
        let junit = (self.parse == "junit").then(|| {
            let sources = paths(&settings.sources);
            (paths(&settings.reports), if sources.is_empty() { vec![cwd.clone()] } else { sources })
        });
        Ok(Found {
            label: fill(set.label.as_deref().unwrap_or(label_default), &vars),
            cwd,
            argv,
            debug: set.debug.as_ref().map(|d| json(d, &vars)),
            lines: self.lines.clone(),
            build,
            debug_run,
            junit,
        })
    }

    /// Where the file's tests are (byte ranges): each test, and each group holding one (a test module's
    /// helpers are its tests' code) — a change wholly inside one is a test's (`risk.rs`).
    pub fn test_spans(&self, tree: &Tree, src: &str) -> Vec<std::ops::Range<usize>> {
        let items = self.items(tree, src);
        let tests: Vec<std::ops::Range<usize>> =
            items.iter().filter(|i| i.test).map(|i| i.range.clone()).collect();
        let holds = |g: &std::ops::Range<usize>| tests.iter().any(|t| g.start <= t.start && t.end <= g.end);
        let groups = items.iter().filter(|i| !i.test && holds(&i.range)).map(|i| i.range.clone());
        groups.chain(tests.iter().cloned()).collect()
    }

    fn items(&self, tree: &Tree, src: &str) -> Vec<Item> {
        let idx = |n: &str| self.query.capture_index_for_name(n);
        let (test_i, group_i, name_i, start_i) = (idx("test"), idx("group"), idx("name"), idx("start"));
        let mut out = Vec::new();
        let mut qc = QueryCursor::new();
        let mut matches = qc.matches(&self.query, tree.root_node(), src.as_bytes());
        while let Some(m) = matches.next() {
            let find = |i: Option<u32>| m.captures().iter().find(|c| Some(c.index) == i).map(|c| c.node);
            let (node, test) = match (find(test_i), find(group_i)) {
                (Some(n), _) => (n, true),
                (None, Some(n)) => (n, false),
                _ => continue,
            };
            let Some(name) = find(name_i).and_then(|n| n.utf8_text(src.as_bytes()).ok()) else { continue };
            let start = find(start_i).map_or(node.start_byte(), |s| s.start_byte().min(node.start_byte()));
            let kind = self
                .query
                .property_settings(m.pattern_index)
                .iter()
                .find(|p| &*p.key == "kind")
                .and_then(|p| p.value.as_deref().map(str::to_string));
            out.push(Item { range: start..node.end_byte(), name: name.to_string(), kind, test });
        }
        // One node matched by several patterns (with its attributes, without): once, from its earliest start
        crate::sorting::by(&mut out, |a, b| {
            (a.test, a.range.end, &a.name, a.range.start).cmp(&(b.test, b.range.end, &b.name, b.range.start))
        });
        out.dedup_by(|b, a| a.test == b.test && a.range.end == b.range.end && a.name == b.name);
        out
    }

    /// The variables the pack's helpers give, for this file.
    fn helper_vars(&self, path: &Path, tree: &Tree) -> Result<HashMap<String, Var>, String> {
        let mut vars = HashMap::new();
        for h in &self.helpers {
            match h.as_str() {
                "cargo" => {
                    let c = crate::testing::rust_crate(path, &tree.language())?;
                    let module =
                        if c.mods.is_empty() { String::new() } else { format!("{}::", c.mods.join("::")) };
                    let label = if c.mods.is_empty() {
                        "all tests".to_string()
                    } else {
                        format!("{}::*", c.mods.join("::"))
                    };
                    vars.insert("cargo-target".into(), Var::Many(c.flag));
                    vars.insert("module".into(), Var::One(module));
                    vars.insert("module-label".into(), Var::One(label));
                    vars.insert("crate-root".into(), Var::One(c.crate_root.display().to_string()));
                }
                _ => unreachable!("checked when the pack was read"),
            }
        }
        Ok(vars)
    }
}

/// Whether a `debug` template runs what's built first (`{built}`).
fn builds(debug: &toml::Value) -> bool {
    debug.to_string().contains("{built}")
}

/// Whether a scope's command uses a variable (so it needs something there).
fn uses(s: &Set, var: &str) -> bool {
    s.run.as_ref().is_some_and(|r| r.iter().any(|a| a.contains(var)))
}

/// A test or group in the file.
struct Item {
    range: std::ops::Range<usize>,
    name: String,
    kind: Option<String>,
    test: bool,
}

/// A template variable: a string; a list (an argument that's just this variable becomes its items); parts,
/// joined with their separator — or, as `{var/sep}`, with `sep`; or alternatives, joined with `|`.
#[derive(Clone, Debug)]
pub(crate) enum Var {
    One(String),
    Many(Vec<String>),
    Parts(Vec<String>, String),
    /// Alternatives, joined with `|` (`{names}`).
    Any(Vec<String>),
    /// A JSON value: a template string that is just this variable (`"{pid}"`) becomes the value itself (a
    /// number stays one); inside a longer string, its text.
    Json(Value),
}

/// `*_test.go`-style: at most one `*`.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post)
        }
        None => pattern == name,
    }
}

fn up_to(start: &Path, found: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut dir = start.parent()?.to_path_buf();
    loop {
        if found(&dir) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `{var}`s filled in: a list variable joined with spaces; parts with their separator, or with what follows
/// a `/` (`{groups/.}`). `|re` escapes the value for a regular expression (`{path|re}`). A `{…}` that names
/// no variable stays as it is. What a variable holds isn't filled in again.
pub(crate) fn fill(s: &str, vars: &HashMap<String, Var>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let token =
            rest[i + 1..].find('}').and_then(|end| Some((expand(&rest[i + 1..i + 1 + end], vars)?, end)));
        match token {
            Some((value, end)) => {
                out.push_str(&value);
                rest = &rest[i + end + 2..];
            }
            None => {
                out.push('{');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// One `{…}`'s value (without its braces): `name`, `name/sep`, and either with `|re`.
fn expand(token: &str, vars: &HashMap<String, Var>) -> Option<String> {
    let (token, re) = token.strip_suffix("|re").map_or((token, false), |t| (t, true));
    let (name, with) = token.split_once('/').map_or((token, None), |(n, w)| (n, Some(w)));
    let esc = |v: String| if re { regex_escape(&v) } else { v };
    Some(match (vars.get(name)?, with) {
        (Var::One(v), None) => esc(v.clone()),
        (Var::Many(l), None) => esc(l.join(" ")),
        (Var::Parts(p, sep), None) => esc(p.join(sep)),
        (Var::Parts(p, _), Some(with)) => esc(p.join(with)),
        // Each escaped on its own: `A|B` stays a choice
        (Var::Any(a), None) => a.iter().map(|x| esc(x.clone())).collect::<Vec<_>>().join("|"),
        (Var::Json(Value::String(s)), None) => esc(s.clone()),
        (Var::Json(v), None) => esc(v.to_string()),
        _ => return None,
    })
}

/// `a(1+1)?` → `a\(1\+1\)\?`: what a regular expression (Go's, JavaScript's, Python's) reads as itself.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A command's arguments: `"{list}"` becomes the list's items, and arguments that end up empty are left out.
pub(crate) fn args(template: &[String], vars: &HashMap<String, Var>) -> Vec<String> {
    let mut out = Vec::new();
    for a in template {
        if let Some(Var::Many(items)) =
            a.strip_prefix('{').and_then(|x| x.strip_suffix('}')).and_then(|k| vars.get(k))
        {
            out.extend(items.iter().cloned());
            continue;
        }
        let a = fill(a, vars);
        if !a.is_empty() {
            out.push(a);
        }
    }
    out
}

/// A TOML template as JSON, its strings filled in (list arguments as in `args`; a string that is just a
/// `Var::Json` is its value).
pub(crate) fn json(v: &toml::Value, vars: &HashMap<String, Var>) -> Value {
    match v {
        toml::Value::String(s) => {
            let lone = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')).and_then(|k| vars.get(k));
            match lone {
                Some(Var::Json(v)) => v.clone(),
                _ => Value::String(fill(s, vars)),
            }
        }
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) if a.iter().all(|x| x.is_str()) => {
            let strs: Vec<String> = a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
            Value::Array(args(&strs, vars).into_iter().map(Value::String).collect())
        }
        toml::Value::Array(a) => Value::Array(a.iter().map(|x| json(x, vars)).collect()),
        toml::Value::Table(t) => {
            // Inserted, not collected: a Map's `collect` sorts — a copy of the sort for each (~6 KB)
            let mut m = serde_json::Map::new();
            for (k, x) in t {
                m.insert(k.clone(), json(x, vars));
            }
            Value::Object(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_and_templates() {
        assert!(
            glob("*_test.go", "parse_test.go")
                && !glob("*_test.go", "parse.go")
                && !glob("*_test.go", "_test.g")
        );
        assert!(glob("test_*.py", "test_x.py") && glob("conftest.py", "conftest.py"));
        let vars: HashMap<String, Var> = [
            ("name".to_string(), Var::One("TestA".into())),
            ("cwd".to_string(), Var::One("/p".into())),
            ("empty".to_string(), Var::One(String::new())),
            ("flags".to_string(), Var::Many(vec!["--test".into(), "api".into()])),
            ("groups".to_string(), Var::Parts(vec!["Outer".into(), "Inner".into()], "$".into())),
        ]
        .into();
        assert_eq!(fill("^{name}$ in {cwd} {other}", &vars), "^TestA$ in /p {other}");
        assert_eq!(
            fill("{groups} {groups/.} {groups/::} {groups/}", &vars),
            "Outer$Inner Outer.Inner Outer::Inner OuterInner"
        );
        assert_eq!(fill("^{groups|re} > {groups/.|re}$", &vars), r"^Outer\$Inner > Outer\.Inner$");
        assert_eq!(fill("{ {name} {nope} {name", &vars), "{ TestA {nope} {name", "only variables are filled");
        let mut vars = vars;
        vars.insert("names".into(), Var::Any(vec!["a(1)".into(), "b".into()]));
        assert_eq!(fill("^({names|re})$ {names}", &vars), r"^(a\(1\)|b)$ a(1)|b", "each alternative escaped");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let t = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            args(&t(&["cargo", "{flags}", "--", "{empty}", "{name}"]), &vars),
            ["cargo", "--test", "api", "--", "TestA"]
        );
        let v: toml::Value =
            toml::from_str::<Table>("x = { a = [\"{name}\", \"{empty}\"], n = 1 }").unwrap()["x"].clone();
        assert_eq!(json(&v, &vars), serde_json::json!({ "a": ["TestA"], "n": 1 }));
    }

    #[test]
    fn tasks_from_a_pack() {
        let d = std::env::temp_dir().join(format!("tarae-pack-tasks-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"lint":"eslint .","build":"vite build"}}"#)
            .unwrap();
        let pack: Table = r#"
            [tasks]
            when = ["deno.json", "package.json"]
            rank = 5
            [[tasks.task]]
            name = "check"
            cmd = "tsc"
            [[tasks.task]]
            name = "never"
            cmd = "x"
            needs = "no-such-program-tarae"
            [[tasks.task]]
            name = "npm run {key}"
            cmd = "npm run --silent {key}"
            each = { json = "package.json", keys = "scripts" }
        "#
        .parse()
        .unwrap();
        let (rank, list) = tasks_of(&pack["tasks"], &d).unwrap().unwrap();
        let names: Vec<&str> = list.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!((rank, names), (5, vec!["check", "npm run build", "npm run lint"]), "`needs` left out");
        assert_eq!(list[2].1, "npm run --silent 'lint'", "quoted: the project names it");
        assert_eq!(tasks_of(&pack["tasks"], &d.join("elsewhere")).unwrap(), None, "no `when` file");
        let bad: Table = r#"
            [tasks]
            when = ["package.json"]
            [[tasks.task]]
            name = "x"
        "#
        .parse()
        .unwrap();
        assert!(tasks_of(&bad["tasks"], &d).unwrap_err().contains("no `cmd`"));
        // The built-in packs' tasks, likeliest first
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("Makefile"), "").unwrap();
        let names: Vec<String> = tasks(&d).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.first().map(String::as_str), Some("cargo check"));
        assert_eq!(names.last().map(String::as_str), Some("make"));
        assert!(names.contains(&"npm run lint".to_string()));
        std::fs::remove_dir_all(&d).ok();
    }

    /// `:preflight`'s defaults come by the files that mark a project — several kinds, their steps together
    /// by rank, each saying its pack. Only where the language's own toolchain defines the check (cargo, go,
    /// an installed Gradle or Maven): none for JavaScript and TypeScript (many runners, a project's own tsc),
    /// Python (no single standard), nor a Java project with its own wrapper (a script from the repository —
    /// held back, saying how to list it); a folder that's no kind of project, none.
    #[test]
    fn preflight_defaults_by_the_project_files() {
        let dir = std::env::temp_dir().join(format!("tarae-pack-preflight-{}", std::process::id()));
        let found = |files: &[&str]| {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for f in files {
                std::fs::write(dir.join(f), "").unwrap();
            }
            preflight(&dir)
        };
        let steps = |files: &[&str]| {
            found(files).steps.iter().map(|s| format!("{}: {}", s.from, s.name)).collect::<Vec<_>>()
        };
        let rust = ["rust pack: fmt", "rust pack: clippy", "rust pack: test"];
        assert_eq!(steps(&["Cargo.toml"]), rust);
        let both = steps(&["go.mod", "Cargo.toml"]);
        assert_eq!(both[..3], rust, "rust first (rank 10)");
        assert_eq!(both[3..], ["go pack: gofmt", "go pack: go vet", "go pack: go test"]);
        assert_eq!(steps(&["Cargo.toml", "package.json", "tsconfig.json"]), rust, "only rust's beside them");
        for none in ["package.json", "tsconfig.json", "pyproject.toml", "notes.txt"] {
            assert!(steps(&[none]).is_empty(), "no defaults for {none}");
        }
        // Java: the installed tool's tests — the wrapper only from the project's own [preflight]
        for gradle in ["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"] {
            assert_eq!(steps(&[gradle]), ["java pack: gradle test"], "{gradle}");
        }
        let gradle = found(&["build.gradle"]).steps;
        assert_eq!(gradle[0].cmd, "gradle test --console=plain");
        assert_eq!(steps(&["pom.xml"]), ["java pack: mvn test"]);
        assert_eq!(found(&["pom.xml"]).steps[0].cmd, "mvn -B test");
        for (files, wrapper) in
            [(["build.gradle.kts", "gradlew"], "./gradlew"), (["pom.xml", "mvnw"], "./mvnw")]
        {
            let d = found(&files);
            assert!(d.steps.is_empty(), "{files:?}: {:?}", d.steps);
            let why = d.held.join(" ");
            assert!(why.contains(&format!("builds with its own {wrapper}")), "{why}");
            assert!(why.contains(&format!("cmd = \"{wrapper} ")) && why.contains("once you trust"), "{why}");
        }
        assert!(found(&["Cargo.toml"]).held.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pack's `[[preflight]]` read whole: `unless` holds an entry back and its `held` says why; an unknown
    /// key is refused, as everywhere in a pack.
    #[test]
    fn a_preflight_entry_held_back_by_a_file() {
        let dir = std::env::temp_dir().join(format!("tarae-pack-unless-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.build"), "").unwrap();
        let v: toml::Value = toml::from_str(
            "[[preflight]]\nwhen = [\"a.build\"]\nunless = [\"wrap\"]\nheld = \"its own wrap\"\n\
             steps = [{ name = \"t\", cmd = \"b test\" }]",
        )
        .unwrap();
        let mut held = Vec::new();
        let ran = preflight_of(&v["preflight"], &dir, "x", &mut held).unwrap();
        assert_eq!((ran.len(), held.len()), (1, 0), "no wrapper: it runs");
        std::fs::write(dir.join("wrap"), "").unwrap();
        let ran = preflight_of(&v["preflight"], &dir, "x", &mut held).unwrap();
        assert!(ran.is_empty() && held == ["its own wrap"], "{held:?}");
        let odd: toml::Value =
            toml::from_str("[[preflight]]\nwhen = [\"a.build\"]\nunles = [\"wrap\"]").unwrap();
        assert!(preflight_of(&odd["preflight"], &dir, "x", &mut held).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ones_own_install_recipe_keeps_the_others() {
        let builtin = "[install.gopls]\nwhat = \"a\"\n[install.dlv]\nwhat = \"b\"";
        let user = "[install.gopls]\nwhat = \"mine\"";
        let t = merged("go", Some(builtin.into()), Some(user.into())).unwrap().unwrap();
        assert_eq!(t["install"]["gopls"]["what"].as_str(), Some("mine"));
        assert_eq!(t["install"]["dlv"]["what"].as_str(), Some("b"), "the built-in dlv stays");
    }

    #[test]
    fn ones_own_pack_replaces_tables_not_the_pack() {
        let builtin = "[test]\nquery = \"a.scm\"\n[tasks]\nwhen = [\"x\"]";
        let user = "[test]\nquery = \"mine.scm\"";
        let t = merged("ts", Some(builtin.into()), Some(user.into())).unwrap().unwrap();
        assert_eq!(t["test"]["query"].as_str(), Some("mine.scm"));
        assert!(t.contains_key("tasks"), "the built-in tasks stay");
        let t = merged("new", None, Some(user.into())).unwrap().unwrap();
        assert!(t.contains_key("test") && !t.contains_key("tasks"));
        assert!(
            merged("ts", Some(builtin.into()), Some("[test".into())).unwrap().unwrap_err().contains("yours")
        );
        assert!(merged("none", None, None).is_none());
    }

    /// A tool off PATH is found in its ways' folders, then where a download put its program (`<data>/<tool>`,
    /// the program it recorded).
    #[test]
    fn a_tool_off_path_is_found_where_it_landed() {
        let d = std::env::temp_dir().join(format!("tarae-found-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let fetched = d.join("data/tool");
        std::fs::create_dir_all(fetched.join("bin")).unwrap();
        let bins = [d.join("go/bin")];
        assert_eq!(found_in("tool", &bins, Some(&fetched)), None, "not there yet");
        std::fs::write(fetched.join("bin/tool"), "").unwrap();
        assert_eq!(found_in("tool", &bins, Some(&fetched)), None, "unpacked by hand, not by a download");
        std::fs::write(fetched.join(".tarae-program"), "bin/tool").unwrap();
        assert_eq!(found_in("tool", &bins, Some(&fetched)), Some(fetched.join("bin/tool")));
        std::fs::write(d.join("outside"), "").unwrap();
        std::fs::write(fetched.join(".tarae-program"), "../../outside").unwrap();
        assert_eq!(found_in("tool", &bins, Some(&fetched)), None, "only inside its folder");
        std::fs::create_dir_all(&bins[0]).unwrap();
        std::fs::write(bins[0].join("tool"), "").unwrap();
        assert_eq!(
            found_in("tool", &bins, Some(&fetched)),
            Some(bins[0].join("tool")),
            "a way's folder first"
        );
        assert_eq!(fetched_dir("tool"), crate::runtime::data_dir().map(|d| d.join("tool")));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A `download` way as a pack writes it, and what it's refused for (said, and the recipe skipped).
    #[test]
    fn download_ways_read() {
        let t: Table = r#"
            [tool]
            what = "a tool"
            [[tool.ways]]
            download = "https://x.org/{version}/tool_{os}_{arch}"
            version = { from = "https://api.x.org/latest", pick = '"tag_name":\s*"([^"]+)"' }
            file = "tool_{os}_{arch}"
            sha256 = "https://x.org/{version}/sha256sums.txt"
            program = "tool"
            rename = { x86_64 = "amd64", aarch64 = "arm64" }
        "#
        .parse()
        .unwrap();
        let d = match &recipe("tool", &t["tool"]).unwrap().ways[0] {
            Way::Download(d) => (**d).clone(),
            w => panic!("{w:?}"),
        };
        assert_eq!(d.unpack, crate::fetch::Unpack::File, "the program itself unless it says");
        assert_eq!(d.file, crate::fetch::Name::Is("tool_{os}_{arch}".into()));
        assert_eq!(d.rename.len(), 2);
        assert!(d.runtime.is_none() && d.needs.is_empty());
        let broken = |change: &str| {
            let base = r#"download = "https://x/{version}"
                version = { from = "https://x/", pick = '(\d+)' }
                file = "f"
                sha256 = "https://x/{version}.sha256"
                program = "f""#;
            let toml = format!("[tool]\nwhat = \"a tool\"\n[[tool.ways]]\n{base}\n{change}\n");
            let t: Table = toml.parse().unwrap_or_else(|e| panic!("{change}: {e}"));
            recipe("tool", &t["tool"]).map(|_| ()).unwrap_err()
        };
        assert!(broken("unpack = \"zip\"").contains("`unpack`"));
        assert!(broken("needs = 3").contains("needs"));
        assert!(broken("mirror = \"x\"").contains("mirror"), "an unknown key");
        assert!(broken("runtime = { program = \"java\" }").contains("runtime"));
        assert!(broken("rename = { x86_64 = 1 }").contains("rename"));
        let t: Table =
            "[tool]\nwhat = \"t\"\n[[tool.ways]]\ndownload = \"https://x/{verison}\"\n".parse().unwrap();
        assert!(recipe("tool", &t["tool"]).unwrap_err().contains("no such placeholder"));
        let t: Table = "[tool]\nwhat = \"t\"\n[[tool.ways]]\ndownload = \"https://x/\"\nversion = { from = \"https://x/\", pick = \"(\" }\nfile = \"f\"\nsha256 = \"s\"\nprogram = \"f\"\n".parse().unwrap();
        assert!(recipe("tool", &t["tool"]).unwrap_err().contains("version.pick"));
    }

    /// The Java pack's jdtls: Homebrew first, else Eclipse's newest milestone, checked, on Java 21.
    #[test]
    fn the_java_packs_jdtls_reads() {
        let recipe = || install_in("jdtls", install_tables()).expect("the Java pack has [install.jdtls]");
        assert_eq!(recipe().ways.len(), 2);
        let brew = recipe().pick(|_| true);
        assert_eq!(
            (brew.run.as_deref(), brew.download.is_some()),
            (Some(&["brew".to_string(), "install".into(), "jdtls".into()][..]), false)
        );
        let linux = recipe().pick(|p| p != "brew");
        let d = linux.download.expect("the download where there's no brew");
        assert_eq!(d.host(), "download.eclipse.org");
        assert!(
            d.url.contains("/jdtls/milestones/{version}/{file}"),
            "a release, not the nightly: {}",
            d.url
        );
        assert_eq!(
            d.sha256,
            crate::fetch::Sum::Published(
                "https://download.eclipse.org/jdtls/milestones/{version}/{file}.sha256".into()
            )
        );
        assert_eq!(
            (d.unpack, d.program.as_str(), d.needs.as_slice()),
            (crate::fetch::Unpack::TarGz, "bin/jdtls", &["python3".to_string()][..])
        );
        let rt = d.runtime.expect("it runs on Java");
        assert_eq!((rt.program.as_str(), rt.home.as_deref(), rt.at_least), ("java", Some("JAVA_HOME"), 21));
        let none = recipe().pick(|p| p != "brew" && p != "python3");
        assert!(none.run.is_none() && none.download.is_none(), "no python3: no way fits");
        assert!(
            none.no_way().contains("a download from download.eclipse.org (with curl or wget, tar, python3)"),
            "{}",
            none.no_way()
        );
    }

    /// The Java pack's java-debug: the release it pins (Maven Central publishes only SHA-1 for it, so the
    /// SHA-256 is the pack's), downloaded as the jar it is — and found as other editors left it.
    #[test]
    fn the_java_packs_java_debug_is_pinned() {
        let r = install_in("java-debug", install_tables()).expect("the Java pack has [install.java-debug]");
        assert_eq!(r.look_for.as_deref(), Some("com.microsoft.java.debug.plugin-*.jar"));
        assert!(r.look_in.iter().any(|d| d.contains("vscjava.vscode-java-debug-*/server")));
        let d = r.pick(|_| true).download.expect("a download");
        use crate::fetch::{Name, Sum, Unpack, Version};
        assert_eq!(d.version, Version::Pinned("0.53.1".into()));
        assert_eq!(
            d.sha256,
            Sum::Pinned("4f4778d452a6a0665536f43ce4e32403a24be6593336b80dc85a322912859e24".into())
        );
        assert_eq!(d.size, Some(2960556));
        assert_eq!(d.file, Name::Is("com.microsoft.java.debug.plugin-{version}.jar".into()));
        assert_eq!((d.unpack, d.program.as_str()), (Unpack::File, "{file}"));
        assert_eq!(
            d.url,
            "https://repo1.maven.org/maven2/com/microsoft/java/com.microsoft.java.debug.plugin/{version}/{file}"
        );
        // A pinned hash with the newest release isn't a pin
        let t: Table = format!(
            "[t]\nwhat = \"t\"\n[[t.ways]]\ndownload = \"https://x/{{file}}\"\nfile = \"f\"\nprogram = \"f\"\n\
             sha256 = \"{}\"\nversion = {{ from = \"https://x/\", pick = \"(\\\\d+)\" }}\n",
            "a".repeat(64)
        )
        .parse()
        .unwrap();
        assert!(recipe("t", &t["t"]).unwrap_err().contains("needs a pinned `version`"));
    }

    /// `[test] vars` read: each says how; a key that doesn't belong to its way is a mistake said.
    #[test]
    fn test_vars_say_how() {
        let read = |s: &str| {
            let t: Table = format!("vars = [{s}]").parse().unwrap();
            derive_rules(t.get("vars"), "").map(|r| r.len())
        };
        assert_eq!(read("{ name = \"a\", topmost = \"roots\" }, { name = \"b\", first = [\"x\"] }"), Ok(2));
        assert!(read("{ name = \"a\" }").unwrap_err().contains("say how"));
        assert!(read("{ topmost = [\"x\"] }").unwrap_err().contains("has no `name`"));
        assert!(
            read("{ name = \"a\", first = [\"x\"], else = \"y\" }").unwrap_err().contains("has no `else`")
        );
        assert!(read("{ name = \"a\", parts = \"{x}\" }").unwrap_err().contains("has no `from`"));
    }

    /// `look-for` in the download's folder and `look-in` folders (a `*` in a part of the path): the newest by
    /// the version in its name.
    #[test]
    fn a_file_is_found_newest_where_editors_left_it() {
        let t = std::env::temp_dir().join(format!("tarae-look-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        for (dir, file) in [
            ("ours", "p-0.9.0.jar"),
            ("ext/vscjava.debug-0.50/server", "p-0.50.0.jar"),
            ("ext/vscjava.debug-0.53/server", "p-0.53.1.jar"),
            ("ext/other-1/server", "p-9.9.9.jar"),
            ("ext/vscjava.debug-0.53/server", "notes.txt"),
        ] {
            std::fs::create_dir_all(t.join(dir)).unwrap();
            std::fs::write(t.join(dir).join(file), "PK").unwrap();
        }
        let dirs = [t.join("ours"), t.join("ext/vscjava.debug-*/server"), t.join("nowhere/*/x")];
        assert_eq!(newest_in("p-*.jar", &dirs), Some(t.join("ext/vscjava.debug-0.53/server/p-0.53.1.jar")));
        assert_eq!(newest_in("p-*.jar", &dirs[..1]), Some(t.join("ours/p-0.9.0.jar")));
        assert_eq!(newest_in("q-*.jar", &dirs), None);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn install_recipes() {
        let t: Table = r#"
            [gopls]
            what = "Go's language server"
            ways = [
                { run = ["no-such-installer", "gopls"] },
                { run = ["go", "install", "x/gopls@latest"], bin = ["~/go/bin", "${TARAE_NO_SUCH_VAR}/bin"] },
            ]
            [rust-analyzer]
            what = "Rust's"
            ways = [{ run = ["rustup", "component", "add", "rust-analyzer"], project = true }]
            [lldb-dap]
            what = "the debugger"
            hint = "comes with Xcode"
            [bad]
            what = "x"
            ways = [{ needs = "go" }]
        "#
        .parse()
        .unwrap();
        let tables = || vec![("go".to_string(), t.clone())];
        let r = install_in("gopls", tables()).unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(r.bins, [PathBuf::from(format!("{home}/go/bin"))], "an unset variable drops that folder");
        let i = r.pick(|p| p == "go");
        assert_eq!(
            i.run.as_deref(),
            Some(&["go".to_string(), "install".into(), "x/gopls@latest".into()][..])
        );
        let none = install_in("gopls", tables()).unwrap().pick(|_| false);
        assert_eq!(none.run, None, "no way fits");
        // What's said then: every way, then the hint
        assert_eq!(
            none.no_way(),
            "Go's language server isn't installed: no-such-installer gopls, go install x/gopls@latest"
        );
        let hint = install_in("lldb-dap", tables()).unwrap().pick(|_| true);
        assert_eq!(hint.no_way(), "the debugger isn't installed: comes with Xcode");
        assert_eq!((hint.run, hint.hint.as_deref()), (None, Some("comes with Xcode")));
        assert!(install_in("bad", tables()).is_none(), "a way without `run` is said (logged) and skipped");
        assert!(install_in("nothing", tables()).is_none());
        let ra = install_in("rust-analyzer", tables()).unwrap().pick(|_| true);
        assert!(ra.in_project, "rustup in the project, for its toolchain");
        assert!(
            !install_in("gopls", tables()).unwrap().pick(|_| true).in_project,
            "the rest in the home folder"
        );
        assert_eq!(expand_dir("${HOME}/x"), Some(PathBuf::from(format!("{home}/x"))));
        // SAFETY: the variable is this test's own
        unsafe { std::env::set_var("TARAE_TEST_GOPATH", "/a/go:/b/go") };
        assert_eq!(
            expand_dir("${TARAE_TEST_GOPATH}/bin"),
            Some(PathBuf::from("/a/go/bin")),
            "a list: its first"
        );
        unsafe { std::env::set_var("TARAE_TEST_LOOP", "${TARAE_TEST_LOOP}") };
        assert_eq!(expand_dir("${TARAE_TEST_LOOP}"), Some(PathBuf::from("${TARAE_TEST_LOOP}")), "not again");
    }

    /// `[server.<name>] leave-out`: the built-in ones read; a broken one says what's wrong (logged, skipped);
    /// one of one's own replaces that server's, not the others'.
    #[test]
    fn servers_say_how_to_leave_out_folders() {
        for s in ["rust-analyzer", "jdtls", "pyright", "basedpyright", "gopls"] {
            assert!(leave_out(s).is_some(), "{s}");
        }
        assert_eq!(
            leave_out("pyright").map(|l| l.place),
            Some(Place::Config("python.analysis.exclude".into()))
        );
        assert!(leave_out("taplo").is_none() && leave_out("marksman").is_none());
        let t: Table = r#"
            [a]
            leave-out = { init = "x.y", each = "-{rel}", keep = ["-k"] }
            [list]
            leave-out = { config = "x", each = ["**/{rel}", "**/{rel}/**"] }
            [listnorel]
            leave-out = { config = "x", each = ["**/{rel}", "**"] }
            [both]
            leave-out = { init = "x", config = "y", each = "{rel}" }
            [norel]
            leave-out = { init = "x", each = "*" }
            [typo]
            leave-out = { init = "x", each = "{rel}", keeps = [] }
            [nothing]
        "#
        .parse()
        .unwrap();
        let one = |k: &str| server_table(&t[k]);
        assert_eq!(
            one("a"),
            Ok(Some(LeaveOut {
                place: Place::Init("x.y".into()),
                each: vec!["-{rel}".into()],
                keep: vec!["-k".into()]
            }))
        );
        assert_eq!(one("list").unwrap().unwrap().each, ["**/{rel}", "**/{rel}/**"]);
        assert!(one("listnorel").unwrap_err().contains("{rel}"), "every one of them");
        assert!(one("both").unwrap_err().contains("one of"));
        assert!(one("norel").unwrap_err().contains("{rel}"));
        assert!(one("typo").unwrap_err().contains("keeps"));
        assert_eq!(one("nothing"), Ok(None));
        assert!(leave_out_in("both", vec![("x".into(), t.clone())]).is_none(), "said and skipped");
        let builtin = "[server.gopls]\nleave-out = { init = \"a\", each = \"{rel}\" }\n[server.x]\n";
        let user = "[server.gopls]\nleave-out = { init = \"mine\", each = \"{rel}\" }";
        let m = merged("go", Some(builtin.into()), Some(user.into())).unwrap().unwrap();
        assert_eq!(m["server"]["gopls"]["leave-out"]["init"].as_str(), Some("mine"));
        assert!(m["server"].get("x").is_some(), "the other built-in one stays");
    }

    #[test]
    fn the_built_in_packs_are_embedded() {
        assert!(["go", "python", "rust", "java"].iter().all(|l| has_tests(l) == Ok(true)));
        assert_eq!(has_tests("markdown"), Ok(false));
        assert_eq!(has_tests("typescript"), Ok(false), "tasks only");
        assert!(builtin_manifests().len() >= 52);
        for (pack, t) in builtin_manifests() {
            assert!(t.is_ok(), "the {pack} pack: {t:?}");
        }
    }

    /// Every top-level key a pack may have says how a runtime folder's pack counts for it; one that doesn't
    /// is refused when read, whoever's pack it is.
    #[test]
    fn every_top_level_pack_key_has_a_reach() {
        for (i, (k, _)) in KEYS.iter().enumerate() {
            assert!(!KEYS[i + 1..].iter().any(|(o, _)| o == k), "{k} twice");
        }
        assert!(read_pack("x", "[tset]\n", "yours").unwrap_err().contains("has no `tset`"));
        assert!(read_pack("x", "whatever = 1\n", "built in").unwrap_err().contains("built in"));
        assert_eq!(reach("outputs"), Some(Reach::BuiltIn));
        assert_eq!(reach("first-made"), Some(Reach::BuiltIn));
        assert_eq!(reach("config-files"), Some(Reach::Added));
        assert_eq!(reach("language"), Some(Reach::Yours));
        assert_eq!(reach("checks"), Some(Reach::Added));
        assert_eq!(reach("color-off"), Some(Reach::BuiltIn), "it's in Claude's runs' environment");
        assert_eq!(reach("places"), Some(Reach::Yours));
    }

    /// `first-made`: a name ending in `/` is a folder, the others files — read from the built-in packs only (a
    /// runtime folder's widens what a run makes: left out, as `outputs`).
    #[test]
    fn first_made_is_folders_and_files_from_the_built_in_packs() {
        let m = made_beside_of("x", "protect = [\"M.toml\"]\nfirst-made = [\"out/\", \"M.lock\"]\n").unwrap();
        assert_eq!((m.folders, m.files), (vec!["out".to_string()], vec!["M.lock".to_string()]));
        let rust = made_beside().into_iter().find(|m| m.pack == "rust").expect("the rust pack makes some");
        assert_eq!((rust.folders, rust.files), (vec!["target".to_string()], vec!["Cargo.lock".to_string()]));
        let mut b: Table = "protect = [\"M.toml\"]\nfirst-made = [\"out/\"]\n".parse().unwrap();
        over(&mut b, "first-made = [\"src/\", \"x\"]\n".parse().unwrap());
        assert_eq!(b["first-made"].as_array().unwrap().len(), 1, "a pack of yours adds none: {b:?}");
    }

    /// A runtime folder's pack over a built-in one, key by key as each one's reach says: whole tables
    /// replaced, named entries by name (languages and servers key by key), lists that narrow added to,
    /// what widens left out.
    #[test]
    fn a_runtime_pack_counts_by_reach() {
        let builtin = "outputs = [\"target\"]\nconfig-files = { toml = [\"A.toml\"] }\n\
                       [language.l]\nextensions = [\"l\"]\ncomment-token = \"#\"\n\
                       [server.s]\nargs = [\"x\"]\nleave-out = { init = \"a\", each = \"{rel}\" }\n\
                       [install.s]\nwhat = \"a\"\nhint = \"h\"\n";
        let user = "outputs = [\"src\"]\nconfig-files = { toml = [\"B.toml\"], json = [\"b.json\"] }\n\
                    [language.l]\nextensions = [\"l\", \"ll\"]\n\
                    [server.s]\ncommand = \"/opt/s\"\n\
                    [install.s]\nwhat = \"mine\"\n";
        let m = merged("p", Some(builtin.into()), Some(user.into())).unwrap().unwrap();
        assert_eq!(m["outputs"].as_array().unwrap().len(), 1, "built-in only");
        let toml: Vec<&str> =
            m["config-files"]["toml"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(toml, ["A.toml", "B.toml"], "added to");
        assert_eq!(m["config-files"]["json"].as_array().unwrap().len(), 1);
        assert_eq!(m["language"]["l"]["comment-token"].as_str(), Some("#"), "key by key");
        assert_eq!(m["language"]["l"]["extensions"].as_array().unwrap().len(), 2);
        assert_eq!(m["server"]["s"]["command"].as_str(), Some("/opt/s"));
        assert!(m["server"]["s"].get("leave-out").is_some(), "key by key");
        assert_eq!(m["install"]["s"]["what"].as_str(), Some("mine"));
        assert!(m["install"]["s"].get("hint").is_none(), "a recipe is replaced whole");
        // A pack of one's own with no built-in one beside it: what widens is left out all the same
        let m = merged("new", None, Some(user.into())).unwrap().unwrap();
        assert!(!m.contains_key("outputs") && m.contains_key("config-files"));
    }

    /// A Java test debugged by running it: attached where the JVM says it waits, by the Java pack's pattern —
    /// a port alone or after a host; a pattern with no port, a way tarae hasn't, are said.
    #[test]
    fn a_test_run_waiting_for_a_debugger_says_where() {
        let java = merged("java", crate::runtime::pack_builtin("java/pack.toml"), None).unwrap().unwrap();
        let a = AttachWait::read(&java["test"]["debug-attach"]).unwrap();
        assert_eq!(a.way, "java");
        let says = |at: &str| a.port(&format!("Listening for transport dt_socket at address: {at}"));
        assert_eq!(
            (says("5005"), says("*:5006"), says("localhost:5007")),
            (Some(5005), Some(5006), Some(5007))
        );
        assert_eq!(a.port("Listening for transport dt_shmem at address: 5005"), None);
        assert_eq!(a.port("[INFO] Running demo.AppTest"), None);
        let bad = |s: &str| AttachWait::read(&s.parse::<Table>().unwrap().into()).unwrap_err();
        assert!(bad("wait-for = 'at (\\d+)'\nway = 'java'").contains("no `port` group"));
        assert!(bad("wait-for = 'at (?<port>\\d+)'\nway = 'gdb'").contains("way `gdb`"));
        assert!(bad("way = 'java'").contains("no `wait-for`"));
    }

    /// `first` takes `~/` and `${VAR}`: a variable that isn't set leaves its choice out. The Python pack's
    /// `{python}`: the project's virtualenv's when it has one, else python3 (`$VIRTUAL_ENV` is the process's —
    /// its expansion is `CARGO_MANIFEST_DIR`'s here, which cargo sets).
    #[test]
    fn vars_take_the_first_there_and_expand_variables() {
        let first = |items: &str, vars: &mut HashMap<String, Var>| {
            let t: Table = format!("vars = [{{ name = \"x\", first = [{items}] }}]").parse().unwrap();
            derive(&derive_rules(t.get("vars"), "").unwrap(), Path::new("/"), vars).map(
                |_| match &vars["x"] {
                    Var::One(s) => s.clone(),
                    v => panic!("{v:?}"),
                },
            )
        };
        let mut vars = HashMap::new();
        let manifest = format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
        let picked = first(
            "\"${TARAE_NO_SUCH_VARIABLE_SET}/x\", \"${CARGO_MANIFEST_DIR}/Cargo.toml\", \"python3\"",
            &mut vars,
        );
        assert_eq!(picked, Ok(manifest));
        assert_eq!(first("\"${TARAE_NO_SUCH_VARIABLE_SET}\", \"tool\"", &mut vars), Ok("tool".into()));
        assert!(first("\"${TARAE_NO_SUCH_VARIABLE_SET}/x\"", &mut vars).is_err(), "none there");
        // The Python pack's, for a project with a .venv and one without
        let d = std::env::temp_dir().join(format!("tarae-pack-venv-{}", std::process::id()));
        std::fs::create_dir_all(d.join(".venv/bin")).unwrap();
        let python =
            merged("python", crate::runtime::pack_builtin("python/pack.toml"), None).unwrap().unwrap();
        for at in ["test", "debug"] {
            let rules = derive_rules(python[at].get("vars"), "").unwrap();
            let python_in = |cwd: &Path| {
                let mut vars = HashMap::from([("cwd".to_string(), Var::One(cwd.display().to_string()))]);
                derive(&rules, cwd, &mut vars).unwrap();
                fill("{python}", &vars)
            };
            // (an activated virtualenv's, where the tests run in one)
            let active = std::env::var_os("VIRTUAL_ENV").map(|v| Path::new(&v).join("bin/python"));
            let otherwise =
                active.filter(|p| p.is_file()).map_or("python3".into(), |p| p.display().to_string());
            assert_eq!(python_in(&d), otherwise, "[{at}] no python in .venv yet");
            std::fs::write(d.join(".venv/bin/python"), "").unwrap();
            assert_eq!(python_in(&d), d.join(".venv/bin/python").display().to_string(), "[{at}]");
            std::fs::remove_file(d.join(".venv/bin/python")).unwrap();
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `[[places]]`: a pack's rules read, a pack of one's own replaces its list; mistakes are said.
    #[test]
    fn places_of_ones_own_replace_the_packs() {
        let user = "[[places]]\nmatch = '^at (?<place>\\S+)'\n";
        let m = merged("python", crate::runtime::pack_builtin("python/pack.toml"), Some(user.into()));
        let rules = places_of(&m.unwrap().unwrap()["places"]).unwrap();
        assert_eq!(
            rules.iter().map(crate::task::PlaceRule::summary).collect::<Vec<_>>(),
            ["^at (?<place>\\S+)"]
        );
        let bad = |s: &str| places_of(&s.parse::<Table>().unwrap()["places"]).unwrap_err();
        assert!(bad("[[places]]\nmatch = '^x'").contains("no place"));
        assert!(bad("[[places]]\nmatch = '^(?<file>x)'").contains("no group `file`"));
        assert!(bad("[[places]]\nmatch = '^e'\nthen = '^x'").contains("`then` has no place"));
        assert!(bad("[[places]]\nmatch = '(?<place>x)'\nsays = 'x'").contains("has no `says`"));
    }
}
