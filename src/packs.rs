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
//! - `helpers`: tarae's own helpers the templates need, each giving some variables:
//!   - `python`: `{python}`, the project's virtualenv's Python, else `python3`
//!   - `cargo`: `{cargo-target}` (`--lib`, `--bin x` or `--test x`), `{module}` (the file's module path,
//!     `a::b::`, or empty), `{module-label}` (`a::b::*` or `all tests`)
//!   - `java`: `{java-root}` (the topmost build file's folder), `{java-module-dir}` (the nearest one's),
//!     `{java-build}` (`gradle` or `maven`), `{java-tool}` (the wrapper if there is one), `{java-task}`
//!     (`:a:b:test`), `{java-pl}` (`-pl a/b`, or nothing), `{package}` (`com.x.`, or empty)
//! - `parse`: how the output is read — a parser tarae has (`go`, `rust`, `python`), or `junit`: the JUnit
//!   XML written to `reports` during the run, read once it ends, failures placed through `sources`.
//! - Templates, for the test at the cursor, the group around the cursor when it's in no test, and the
//!   file: `run`, `run-group`, `run-file` (the command); `debug`, `debug-group`, `debug-file` (the
//!   debugger's launch configuration); `label`, `label-group`, `label-file` (what the result is called).
//!   Their variables: `{name}` (the test), `{groups}` (the groups around it, joined with `group-sep`,
//!   `::` unless set), `{path}` (groups and name), `{groups/.}` and `{path/.}` (joined with what follows
//!   the `/` instead; `|re` after any of them escapes it for a regular expression), `{first-group}` (the file's first group), `{names}` (the file's tests, joined with
//!   `|`), `{cwd}`, `{file}`, `{filename}`, `{rel}` (the file from `{cwd}`), and the helpers'. An argument
//!   that is just a list variable (`"{cargo-target}"`) becomes its items; one that ends up empty is left
//!   out.
//! - Debugging: `{cargo-test-binary}` in a `debug` template is the test binary, built (`cargo test
//!   --no-run`) when debugging starts. Or, instead of a `debug` template: `debug-args`, added to the
//!   command so the test waits for a debugger, and `debug-attach = "java"`, which attaches java-debug once
//!   the JVM says where.
//! - `variant-by`: a template naming the `[test.variant.<name>]` whose settings (templates, `reports`,
//!   `sources`, `debug-args`) override these — Java's `gradle` and `maven`. A kind's settings override the
//!   test ones; `file = false` keeps its tests out of a file's run.
//!
//! `[tasks]`: see `tasks`. `[install.<tool>]`: see `Install`. `[server.<name>]`: see `LeaveOut`.
//!
//! Packs are read when a test or the tasks are looked for (a few small files), so an edited pack counts at
//! once.

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
    /// The output parser's name.
    pub parse: String,
    /// What to build before debugging (`{cargo-test-binary}`).
    pub build: Option<Build>,
    /// Debugging by running this command instead (the test waits for a debugger), then attaching this way.
    pub debug_run: Option<(Vec<String>, String)>,
    /// `parse = "junit"`: the folders the reports land in, and the source roots.
    pub junit: Option<(Vec<PathBuf>, Vec<PathBuf>)>,
}

/// Built when debugging starts, its path put into the launch configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Build {
    /// `cargo test --no-run` with these target flags, picking the binary whose root file is `crate_root`.
    CargoTestBinary { flag: Vec<String>, crate_root: PathBuf },
}

/// A pack's `[test]`.
pub struct Tests {
    query: Query,
    files: Vec<String>,
    cwd: Cwd,
    helpers: Vec<String>,
    parse: String,
    group_sep: String,
    base: Settings,
    variant_by: Option<String>,
    variants: HashMap<String, Settings>,
    kinds: HashMap<String, Kind>,
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
    debug_attach: Option<String>,
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
            debug_attach: match string(t, "debug-attach")? {
                Some(a) if a != "java" => {
                    return Err(format!("no `debug-attach = \"{a}\"` (there is: java)"));
                }
                a => a,
            },
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

/// `lang`'s `pack.toml`: the built-in one, with each table a runtime folder's sets (`[test]`, `[tasks]`) in
/// its place — so a pack of one's own can add tests without losing the built-in tasks, or the other way.
fn manifest(lang: &str) -> Option<Result<Table, String>> {
    let rel = format!("{lang}/pack.toml");
    merged(lang, crate::runtime::pack_builtin(&rel), crate::runtime::pack_user(&rel))
}

/// The built-in `pack.toml` with the tables of one's own in their place (`[install]` and `[server]`: tool by
/// tool).
fn merged(lang: &str, builtin: Option<String>, user: Option<String>) -> Option<Result<Table, String>> {
    let read = |s: String, whose: &str| {
        s.parse::<Table>().map_err(|e| format!("the {lang} pack ({whose}): {}", e.message()))
    };
    let builtin = builtin.map(|s| read(s, "built in"));
    let user = user.map(|s| read(s, "yours"));
    Some(match (builtin, user) {
        (None, None) => return None,
        (Some(b), None) => b,
        (None, Some(u)) => u,
        (Some(b), Some(u)) => u.and_then(|mut u| {
            let mut b = b?;
            // `[install]` and `[server]` go tool by tool: one of one's own keeps the built-in others
            let mine: Vec<_> = BY_TOOL.iter().map(|k| (*k, u.remove(*k))).collect();
            b.extend(u);
            for (key, mine) in mine {
                match (mine, b.get_mut(key)) {
                    (Some(toml::Value::Table(m)), Some(toml::Value::Table(t))) => t.extend(m),
                    (Some(m), _) => {
                        b.insert(key.into(), m);
                    }
                    (None, _) => {}
                }
            }
            Ok(b)
        }),
    })
}

/// A pack's tables of tools, merged with one's own tool by tool.
const BY_TOOL: &[&str] = &["install", "server"];

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
    found.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
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
/// the first run in a project says so on a card (`preflight::DEFAULTS_SAY`). Never a project's own wrapper.
///
/// `[[preflight]]`: `when` — the files that say it's this kind of project (any); `rank` (50 unless set);
/// `steps = [{ name, cmd, note? }]` as in `.tarae.toml`.
pub fn preflight(root: &Path) -> Vec<crate::preflight::Step> {
    let mut found = Vec::new();
    for lang in crate::runtime::pack_langs() {
        let Some(Ok(t)) = manifest(&lang) else { continue };
        let Some(list) = t.get("preflight") else { continue };
        match preflight_of(list, root, &lang) {
            Ok(sets) => found.extend(sets.into_iter().map(|(rank, steps)| (rank, lang.clone(), steps))),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {lang} pack: [[preflight]]: {e}")),
        }
    }
    found.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    found.into_iter().flat_map(|(_, _, steps)| steps).collect()
}

/// One pack's `[[preflight]]` entries that `root` is a project of: (rank, steps).
fn preflight_of(
    v: &toml::Value,
    root: &Path,
    lang: &str,
) -> Result<Vec<(i64, Vec<crate::preflight::Step>)>, String> {
    let list = v.as_array().ok_or("isn't a list ([[preflight]])")?;
    let mut out = Vec::new();
    for entry in list {
        let t = entry.as_table().ok_or("an entry isn't a table")?;
        known(t, &["when", "rank", "steps"], "[[preflight]]")?;
        let when = strings(t.get("when"), "when")?.unwrap_or_default();
        if !when.iter().any(|f| root.join(f).is_file()) {
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
///   one (rustup: the project's toolchain).
/// - `hint`: what to tell when no way fits ("comes with Xcode's command line tools").
///
/// The tool is named as it's run (`gopls`, `basedpyright-langserver`). tarae only offers: `y` runs `run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub tool: String,
    pub what: String,
    /// The first way this machine can take.
    pub run: Option<Vec<String>>,
    /// That way runs in the file's folder (`project = true`: rustup, for the project's toolchain) — else in
    /// the home folder, where a project's settings (an `.npmrc`) don't reach.
    pub in_project: bool,
    pub hint: Option<String>,
}

/// `tool`'s `[install.<tool>]`, from whichever pack has it (a pack that can't be read is logged).
pub fn install(tool: &str) -> Option<Install> {
    install_in(tool, install_tables()).map(|i| i.pick(on_path))
}

/// Where `tool` is when it isn't on `PATH`: in a `bin` of one of its ways (`go install` → `~/go/bin`).
pub fn installed_at(tool: &str) -> Option<PathBuf> {
    let found = install_in(tool, install_tables())?;
    found.bins.iter().map(|d| d.join(tool)).find(|p| p.is_file())
}

/// Every pack's `[install]`.
fn install_tables() -> Vec<(String, Table)> {
    tables_of("install")
}

/// Every pack's `[<key>]` (a pack that can't be read is logged).
fn tables_of(key: &str) -> Vec<(String, Table)> {
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
    known(t, &["leave-out"], "it")?;
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
    /// (run, needs, in the project)
    ways: Vec<(Vec<String>, String, bool)>,
    bins: Vec<PathBuf>,
    hint: Option<String>,
}

impl Recipe {
    /// The first way whose program `has`.
    fn pick(self, has: impl Fn(&str) -> bool) -> Install {
        let way = self.ways.into_iter().find(|(_, needs, _)| has(needs));
        let in_project = way.as_ref().is_some_and(|w| w.2);
        Install { tool: self.tool, what: self.what, run: way.map(|w| w.0), in_project, hint: self.hint }
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
    known(t, &["what", "ways", "hint"], "it")?;
    let what = string(t, "what")?.ok_or("has no `what`")?;
    let mut ways = Vec::new();
    let mut bins = Vec::new();
    for w in
        t.get("ways").map_or(Ok(&[][..]), |w| w.as_array().map(Vec::as_slice).ok_or("`ways` isn't a list"))?
    {
        let w = w.as_table().ok_or("a way isn't a table")?;
        known(w, &["run", "needs", "bin", "project"], "a way")?;
        let run = strings(w.get("run"), "run")?.filter(|r| !r.is_empty()).ok_or("a way has no `run`")?;
        let needs = string(w, "needs")?.unwrap_or_else(|| run[0].clone());
        bins.extend(strings(w.get("bin"), "bin")?.unwrap_or_default().iter().filter_map(|d| expand_dir(d)));
        let project = match w.get("project") {
            None => false,
            Some(p) => p.as_bool().ok_or("`project` is true or false")?,
        };
        ways.push((run, needs, project));
    }
    Ok(Recipe { tool: tool.to_string(), what, ways, bins, hint: string(t, "hint")? })
}

/// `~/go/bin`, `${GOPATH}/bin` → a folder (None if a variable isn't set). A variable holding a list of
/// folders (`GOPATH=/a:/b`) gives its first, where tools install to.
fn expand_dir(d: &str) -> Option<PathBuf> {
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
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A key `t` doesn't know is a mistake to say, not to skip (`runn = [...]`).
fn known(t: &Table, keys: &[&str], what: &str) -> Result<(), String> {
    match t.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(format!("{what} has no `{k}`")),
        None => Ok(()),
    }
}

fn strings(v: Option<&toml::Value>, what: &str) -> Result<Option<Vec<String>>, String> {
    let Some(v) = v else { return Ok(None) };
    let arr = v.as_array().ok_or(format!("`{what}` isn't a list"))?;
    let each = |x: &toml::Value| x.as_str().map(str::to_string).ok_or(format!("`{what}` holds a non-string"));
    arr.iter().map(each).collect::<Result<_, _>>().map(Some)
}

fn string(t: &Table, key: &str) -> Result<Option<String>, String> {
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

const HELPERS: &[&str] = &["python", "cargo", "java"];
const PARSERS: &[&str] = &["go", "rust", "python", "junit"];

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
        let own = ["query", "files", "cwd", "helpers", "parse", "group-sep", "variant-by", "variant", "kind"];
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
        if let Some(vs) = t.get("variant") {
            for (k, v) in vs.as_table().ok_or("[test.variant] isn't a table")? {
                let vt = v.as_table().ok_or(format!("[test.variant.{k}] isn't a table"))?;
                known(vt, SETTINGS, &format!("[test.variant.{k}]"))?;
                variants.insert(k.clone(), Settings::read(vt)?);
            }
        }
        let parse = string(t, "parse")?.unwrap_or_default();
        if !parse.is_empty() && !PARSERS.contains(&parse.as_str()) {
            return Err(format!("no `parse = \"{parse}\"` (there are: {})", PARSERS.join(", ")));
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
        Ok(Tests {
            query,
            files: strings(t.get("files"), "files")?.unwrap_or_default(),
            cwd,
            helpers,
            parse,
            group_sep: string(t, "group-sep")?.unwrap_or_else(|| "::".into()),
            base,
            variant_by,
            variants,
            kinds,
        })
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
        // (a template's comes from the helpers', which get the file's folder)
        let first = match &self.cwd {
            Cwd::Markers { nearest, or } => near(nearest).or_else(|| near(or)).unwrap_or(dir),
            Cwd::Template(_) => dir,
        };
        let mut vars = self.helper_vars(path, &first, src, tree)?;
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
            Some((_, Some(o))) => self.base.over(o),
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
            g.sort_by_key(|g| g.range.start);
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
        let build = match &set.debug {
            Some(d) if d.to_string().contains("{cargo-test-binary}") => {
                let flag = match vars.get("cargo-target") {
                    Some(Var::Many(v)) => v.clone(),
                    _ => return Err("{cargo-test-binary} needs the `cargo` helper".into()),
                };
                let crate_root = match vars.get("crate-root") {
                    Some(Var::One(p)) => PathBuf::from(p),
                    _ => return Err("{cargo-test-binary} needs the `cargo` helper".into()),
                };
                Some(Build::CargoTestBinary { flag, crate_root })
            }
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
            parse: self.parse.clone(),
            build,
            debug_run,
            junit,
        })
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
        out.sort_by(|a, b| {
            (a.test, a.range.end, &a.name, a.range.start).cmp(&(b.test, b.range.end, &b.name, b.range.start))
        });
        out.dedup_by(|b, a| a.test == b.test && a.range.end == b.range.end && a.name == b.name);
        out
    }

    /// The variables the pack's helpers give, for this file.
    fn helper_vars(
        &self,
        path: &Path,
        cwd: &Path,
        src: &str,
        tree: &Tree,
    ) -> Result<HashMap<String, Var>, String> {
        let mut vars = HashMap::new();
        for h in &self.helpers {
            match h.as_str() {
                "python" => {
                    vars.insert("python".into(), Var::One(crate::testing::py_interpreter(cwd)));
                }
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
                "java" => {
                    let j = java(path, src, tree)?;
                    let one = |s: String| Var::One(s);
                    let task = if j.module.is_empty() { "test".into() } else { format!("{}:test", j.module) };
                    let pl = if j.gradle || j.module.is_empty() {
                        Vec::new()
                    } else {
                        vec!["-pl".into(), j.module]
                    };
                    vars.extend([
                        ("java-root".into(), one(j.root.display().to_string())),
                        ("java-module-dir".into(), one(j.module_dir.display().to_string())),
                        ("java-build".into(), one(if j.gradle { "gradle" } else { "maven" }.into())),
                        ("java-tool".into(), one(j.tool)),
                        ("java-task".into(), one(task)),
                        ("java-pl".into(), Var::Many(pl)),
                        ("package".into(), one(j.package)),
                    ]);
                }
                _ => unreachable!("checked when the pack was read"),
            }
        }
        Ok(vars)
    }
}

/// Whether a scope's command uses a variable (so it needs something there).
fn uses(s: &Set, var: &str) -> bool {
    s.run.as_ref().is_some_and(|r| r.iter().any(|a| a.contains(var)))
}

/// A Java file's build: run from the topmost build file (the top of a multi-module build), in the module
/// with the nearest one.
struct Java {
    root: PathBuf,
    module_dir: PathBuf,
    gradle: bool,
    tool: String,
    /// `:a:b` (Gradle) or `a/b` (Maven), empty at the top.
    module: String,
    /// `com.x.`, or empty.
    package: String,
}

fn java(path: &Path, src: &str, tree: &Tree) -> Result<Java, String> {
    let root = crate::java::project_root(path).ok_or("no pom.xml or build.gradle above this file")?;
    let module_dir =
        up_to(path, |d| crate::java::BUILD_FILES.iter().any(|f| d.join(f).is_file())).unwrap_or(root.clone());
    let rel: Vec<String> = module_dir
        .strip_prefix(&root)
        .map(|r| r.iter().map(|c| c.to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let gradle = !root.join("pom.xml").is_file();
    let tool = match (gradle, root.join("gradlew").is_file(), root.join("mvnw").is_file()) {
        (true, true, _) => root.join("gradlew").display().to_string(),
        (true, false, _) => "gradle".into(),
        (false, _, true) => root.join("mvnw").display().to_string(),
        (false, _, false) => "mvn".into(),
    };
    let module = match (gradle, rel.is_empty()) {
        (_, true) => String::new(),
        (true, false) => format!(":{}", rel.join(":")),
        (false, false) => rel.join("/"),
    };
    // `package a.b;` — from the tree, so not one in a comment or a string
    let root_node = tree.root_node();
    let mut walk = root_node.walk();
    let package = root_node
        .named_children(&mut walk)
        .find(|n| n.kind() == "package_declaration")
        .and_then(|p| {
            let mut w = p.walk();
            let name =
                p.named_children(&mut w).find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"));
            name.and_then(|n| n.utf8_text(src.as_bytes()).ok()).map(|p| format!("{p}."))
        })
        .unwrap_or_default();
    Ok(Java { root, module_dir, gradle, tool, module, package })
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
enum Var {
    One(String),
    Many(Vec<String>),
    Parts(Vec<String>, String),
    /// Alternatives, joined with `|` (`{names}`).
    Any(Vec<String>),
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
fn fill(s: &str, vars: &HashMap<String, Var>) -> String {
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
fn args(template: &[String], vars: &HashMap<String, Var>) -> Vec<String> {
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

/// A TOML template as JSON, its strings filled in (list arguments as in `args`).
fn json(v: &toml::Value, vars: &HashMap<String, Var>) -> Value {
    match v {
        toml::Value::String(s) => Value::String(fill(s, vars)),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) if a.iter().all(|x| x.is_str()) => {
            let strs: Vec<String> = a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
            Value::Array(args(&strs, vars).into_iter().map(Value::String).collect())
        }
        toml::Value::Array(a) => Value::Array(a.iter().map(|x| json(x, vars)).collect()),
        toml::Value::Table(t) => Value::Object(t.iter().map(|(k, x)| (k.clone(), json(x, vars))).collect()),
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
    /// by rank, each saying its pack; Python has none (no single standard); a folder that's no kind of
    /// project, none.
    #[test]
    fn preflight_defaults_by_the_project_files() {
        let dir = std::env::temp_dir().join(format!("tarae-pack-preflight-{}", std::process::id()));
        let steps = |files: &[&str]| {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for f in files {
                std::fs::write(dir.join(f), "").unwrap();
            }
            preflight(&dir).iter().map(|s| format!("{}: {}", s.from, s.name)).collect::<Vec<_>>()
        };
        let rust = ["rust pack: fmt", "rust pack: clippy", "rust pack: test"];
        assert_eq!(steps(&["Cargo.toml"]), rust);
        let both = steps(&["go.mod", "Cargo.toml"]);
        assert_eq!(both[..3], rust, "rust first (rank 10)");
        assert_eq!(both[3..], ["go pack: gofmt", "go pack: go vet", "go pack: go test"]);
        assert_eq!(
            steps(&["tsconfig.json", "package.json"]),
            ["typescript pack: tsc", "javascript pack: npm test"]
        );
        assert_eq!(steps(&["pom.xml"]), ["java pack: mvn verify"]);
        assert_eq!(steps(&["build.gradle.kts"]), ["java pack: gradle check"]);
        assert!(steps(&["pyproject.toml"]).is_empty(), "no Python defaults");
        assert!(steps(&["notes.txt"]).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The Java defaults run the mvn or gradle on your PATH — never the project's own wrapper (a script from
    /// the repository: a project's `[preflight]` runs it, once trusted). Without the tool: said so, nothing
    /// run. Harmless stand-ins: a `./mvnw`/`./gradlew` that would leave a marker, a fake `mvn`/`gradle` in
    /// the test's own folder.
    #[test]
    fn the_java_defaults_never_run_the_projects_wrapper() {
        let dir = std::env::temp_dir().join(format!("tarae-pack-wrapper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |path: &Path, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        for (marker, tool, wrapper) in [("pom.xml", "mvn", "mvnw"), ("build.gradle", "gradle", "gradlew")] {
            for f in ["pom.xml", "build.gradle"] {
                let _ = std::fs::remove_file(dir.join(f));
            }
            std::fs::write(dir.join(marker), "").unwrap();
            script(&dir.join(wrapper), &format!("touch '{}'", dir.join("wrapper-ran").display()));
            let step = preflight(&dir).into_iter().find(|s| s.from == "java pack").expect("a java step");
            let run = || {
                std::process::Command::new("sh")
                    .args(["-c", &step.cmd])
                    .current_dir(&dir)
                    .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                    .output()
                    .unwrap()
            };
            // No tool on PATH: said, and the wrapper isn't the way out
            let out = run();
            let said = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(!out.status.success(), "{said}");
            assert!(
                said.contains(&format!(
                    "{tool} isn't on PATH — tarae's defaults don't run a project's own ./{wrapper}"
                )),
                "{said}"
            );
            assert!(!dir.join("wrapper-ran").exists(), "./{wrapper} didn't run");
            // The one on PATH runs
            script(&bin.join(tool), &format!("echo \"{tool} $*\" > '{}'", dir.join("tool-ran").display()));
            assert!(run().status.success());
            let ran = std::fs::read_to_string(dir.join("tool-ran")).unwrap();
            assert!(
                ran.starts_with(&format!("{tool} ")) && (ran.contains("verify") || ran.contains("check")),
                "{ran}"
            );
            assert!(!dir.join("wrapper-ran").exists(), "./{wrapper} didn't run");
            std::fs::remove_file(bin.join(tool)).unwrap();
        }
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
        assert_eq!(install_in("gopls", tables()).unwrap().pick(|_| false).run, None, "no way fits");
        let hint = install_in("lldb-dap", tables()).unwrap().pick(|_| true);
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
    }
}
