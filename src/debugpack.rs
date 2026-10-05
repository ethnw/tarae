//! A language's debugger, as its pack says (`runtime/packs/<lang>/pack.toml`) — the protocol and the flow are
//! `dap.rs`'s and `attach.rs`'s; what's about one language or one debugger is data here.
//!
//! `[debug]` (the pack's language; a pack of one's own replaces it whole):
//! - `debugger = "<name>"`: the `[debugger.<name>]` it runs — or `way = "java"`, a way tarae has in code
//!   (java-debug through jdtls, `java.rs`).
//! - `helpers = ["python"]`: `{python}` for the templates (the helpers that need no test).
//! - `launch`: `F5` on a file — the launch request's arguments. `{file}`, `{filename}`, `{dir}` (the file's
//!   folder), `{cwd}` (the project's root — where the debugger runs), `{built}`, the helpers'.
//! - `build = { when, otherwise, run, env, pick, take, error, none }`: built before launching (on a worker) —
//!   `run`'s output read as JSON lines, the last one `pick` holds gives `{built}` from its field `take`; a
//!   failed build says the first line `error.pick` holds, by its first `error.text` field there (else its
//!   stderr's last lines). `when`: files the project's root has one of, else `otherwise` is said; `none`: said
//!   when it built but nothing was picked.
//! - `attach-pid` (`{pid}`, `{program}`), `attach-remote` (`{host}`, `{port}`, `{program}`, `{remote-root}`):
//!   the attach request's arguments for the debugger started to attach — by pid, or to a remote stub at
//!   host:port; `attach-tcp`: connect straight to host:port (the program's own debug server speaks DAP).
//!   `{cwd}` in all; `{program}` is `[[attach]] program` from the project's root.
//!
//! `[debugger.<name>]` (any pack, by name — the name is also its `[install.<name>]`, offered when it's missing):
//! - `find`: in order — `"lldb-dap"` a program on `PATH` (`{22..14}`: each number, in that order); `{ program =
//!   "python3" }` the same; `{ file = [...] }` the first that exists (`{cwd}`, `~`, `${VAR}`); `{ ask = [...] }`
//!   the path a command prints. Each may have a `run` of its own. Then where its install recipe puts it.
//! - `run`: the command, `{found}` being what was found (`["{found}"]` unless set). `listen = ["-l",
//!   "127.0.0.1:{port}"]`: it talks over TCP, on a port tarae picks. `adapter-id`: what `initialize` calls it.
//! - `slow-start = { after, os, say }`: what to say when it hasn't started after `after` seconds (on `os` only).
//! - `quiet`: its own output lines dropped (regular expressions; not the program's stdout and stderr);
//!   `own-first`: a first stdout line that's its own, shown dim.
//!
//! In a template, a key whose value names a variable with nothing in it (no `remote-root` given) is left out
//! — its value whole (`sourceMap = [["{remote-root}", "{cwd}"]]`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use serde_json::Value;
use toml::Table;

use crate::packs::{Var, known, string, strings};

/// The ways tarae has in code (`way = …`).
const WAYS: &[&str] = &["java"];
/// The helpers a `[debug]` may name: those that need no test's tree.
const HELPERS: &[&str] = &["python"];

/// A language's `[debug]`.
#[derive(Debug, Clone)]
pub struct DebugSpec {
    pub lang: String,
    pub with: With,
    helpers: Vec<String>,
    launch: Option<toml::Value>,
    build: Option<BuildTemplate>,
    attach_pid: Option<toml::Value>,
    attach_remote: Option<toml::Value>,
    attach_tcp: Option<toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum With {
    /// A `[debugger.<name>]`.
    Debugger(String),
    /// A way tarae has in code.
    Way(String),
}

/// `lang`'s `[debug]` — None: its pack has none (or there's no pack).
pub fn read(lang: &str) -> Option<Result<DebugSpec, String>> {
    let t = match crate::packs::manifest(lang)? {
        Ok(t) => t,
        Err(e) => return Some(Err(e)),
    };
    let d = t.get("debug")?;
    Some(debug_of(lang, d).map_err(|e| format!("the {lang} pack: [debug]: {e}")))
}

/// `lang`'s `[debug]`, or why there's none to use. Reads the packs: a worker's.
pub fn of(lang: &str) -> Result<DebugSpec, String> {
    match read(lang) {
        Some(d) => d,
        None if lang.is_empty() => Err("no debugger for this file".into()),
        None => Err(format!("no debugger for {lang}")),
    }
}

fn debug_of(lang: &str, v: &toml::Value) -> Result<DebugSpec, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    known(
        t,
        &["debugger", "way", "helpers", "launch", "build", "attach-pid", "attach-remote", "attach-tcp"],
        "it",
    )?;
    let with = match (string(t, "debugger")?, string(t, "way")?) {
        (Some(d), None) => With::Debugger(d),
        (None, Some(w)) if WAYS.contains(&w.as_str()) => With::Way(w),
        (None, Some(w)) => return Err(format!("no `way = \"{w}\"` (there is: {})", WAYS.join(", "))),
        _ => return Err("takes one of `debugger` or `way`".into()),
    };
    let helpers = strings(t.get("helpers"), "helpers")?.unwrap_or_default();
    if let Some(h) = helpers.iter().find(|h| !HELPERS.contains(&h.as_str())) {
        return Err(format!("no helper `{h}` here (there is: {})", HELPERS.join(", ")));
    }
    let template = |k: &str| match t.get(k) {
        None => Ok(None),
        Some(v @ toml::Value::Table(_)) => Ok(Some(v.clone())),
        Some(_) => Err(format!("`{k}` isn't a table")),
    };
    let build = t.get("build").map(|b| build_of(b, true).map_err(|e| format!("`build`: {e}"))).transpose()?;
    let launch = template("launch")?;
    if build.is_some() && launch.is_none() {
        return Err("`build` with no `launch`".into());
    }
    Ok(DebugSpec {
        lang: lang.to_string(),
        with,
        helpers,
        launch,
        build,
        attach_pid: template("attach-pid")?,
        attach_remote: template("attach-remote")?,
        attach_tcp: template("attach-tcp")?,
    })
}

/// How `F5` goes on, once the pack is read.
pub enum Launch {
    /// The way tarae has in code (`WAYS`: Java's).
    Way,
    /// The launch request's arguments.
    Ready(Value),
    /// Built first; then `Pending::with` what was built.
    Build(Box<Build>, Pending),
}

/// A launch template waiting for `{built}`.
pub struct Pending {
    template: toml::Value,
    vars: HashMap<String, Var>,
}

impl Pending {
    pub fn with(mut self, built: &Path) -> Value {
        self.vars.insert("built".into(), Var::One(built.display().to_string()));
        filled(&self.template, &self.vars)
    }
}

/// How an attach goes on.
#[derive(Debug)]
pub enum Attach {
    /// The way tarae has in code (`WAYS`: Java's).
    Way,
    /// Straight to host:port, with these arguments — that debugger's (its output rules, its id).
    Tcp(Value, Box<Debugger>),
    /// The debugger started to attach, with these arguments.
    Started(Value),
}

impl DebugSpec {
    /// `F5` on `file` in the project at `root` (a real path): what to do. Reads the disk (the helpers, the
    /// build's `when`): a worker's.
    pub fn launch(&self, file: &Path, root: &Path) -> Result<Launch, String> {
        if let With::Way(_) = &self.with {
            return Ok(Launch::Way);
        }
        let Some(template) = &self.launch else {
            return Err(format!(
                "the {} pack doesn't say how to launch a file — attach to the program (space G a)",
                self.lang
            ));
        };
        let mut vars = self.vars(root);
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = file.parent().map_or_else(|| root.to_path_buf(), |d| d.canonicalize().unwrap_or(d.into()));
        for (k, v) in [
            ("file", file.display().to_string()),
            ("filename", name(file)),
            ("dir", dir.display().to_string()),
        ] {
            vars.insert(k.into(), Var::One(v));
        }
        match &self.build {
            None => Ok(Launch::Ready(filled(template, &vars))),
            Some(b) => {
                if !b.when.is_empty() && !b.when.iter().any(|f| root.join(f).is_file()) {
                    return Err(b
                        .otherwise
                        .clone()
                        .unwrap_or_else(|| format!("no {} here", b.when.join(" or "))));
                }
                Ok(Launch::Build(Box::new(b.fill(&vars)), Pending { template: template.clone(), vars }))
            }
        }
    }

    /// Attaching in the project at `root`: to a process by `pid`, else to `host:port`. `program` (from `root`)
    /// and `remote_root` as `[[attach]]` gives them.
    pub fn attach(
        &self,
        root: &Path,
        pid: Option<u32>,
        host: &str,
        port: Option<u16>,
        program: Option<&str>,
        remote_root: Option<&str>,
    ) -> Result<Attach, String> {
        let name = match &self.with {
            With::Way(_) => return Ok(Attach::Way),
            With::Debugger(d) => d,
        };
        let mut vars = self.vars(root);
        let mut unset = Vec::new();
        let mut put = |k: &str, v: Option<Var>| match v {
            Some(v) => drop(vars.insert(k.into(), v)),
            None => unset.push(k.to_string()),
        };
        put("program", program.map(|p| Var::One(root.join(p).display().to_string())));
        put("remote-root", remote_root.map(|r| Var::One(r.to_string())));
        put("pid", pid.map(|p| Var::Json(p.into())));
        put("host", Some(Var::One(host.to_string())));
        put("port", port.map(|p| Var::Json(p.into())));
        let with = |t: &toml::Value| filled(&without(t, &unset), &vars);
        match (pid, port, &self.attach_pid, &self.attach_tcp, &self.attach_remote) {
            (Some(_), _, Some(t), ..) => Ok(Attach::Started(with(t))),
            (Some(_), ..) => Err(format!("{name} can't attach to a process here (the {} pack)", self.lang)),
            (None, Some(_), _, Some(t), _) => Ok(Attach::Tcp(with(t), Box::new(debugger(name)?))),
            (None, Some(_), _, None, Some(t)) => Ok(Attach::Started(with(t))),
            (None, Some(_), ..) => Err(format!("{name} can't attach to host:port (the {} pack)", self.lang)),
            (None, None, ..) => Err("needs a pid or host:port".into()),
        }
    }

    /// The variables every template has: `{cwd}`, and the helpers'.
    fn vars(&self, root: &Path) -> HashMap<String, Var> {
        let mut vars = HashMap::new();
        vars.insert("cwd".into(), Var::One(root.display().to_string()));
        for h in &self.helpers {
            // (the `[test]` helper of that name, `packs.rs`)
            if h == "python" {
                vars.insert("python".into(), Var::One(crate::testing::py_interpreter(root)));
            }
        }
        vars
    }

    /// One line for the language table's snapshot.
    #[cfg(test)]
    pub(crate) fn summary(&self) -> String {
        let mut s = match &self.with {
            With::Debugger(d) => d.clone(),
            With::Way(w) => format!("way {w}"),
        };
        if let Some(l) = &self.launch {
            s += &format!(" · launch {}", l.to_string().replace('\n', " ").trim());
        }
        if let Some(b) = &self.build {
            s += &format!(" · build {} when {}", b.run.join(" "), b.when.join(" "));
        }
        for (k, t) in [("pid", &self.attach_pid), ("remote", &self.attach_remote), ("tcp", &self.attach_tcp)]
        {
            if let Some(t) = t {
                s += &format!(" · attach-{k} {}", t.to_string().replace('\n', " ").trim());
            }
        }
        s
    }
}

/// A template filled in.
fn filled(t: &toml::Value, vars: &HashMap<String, Var>) -> Value {
    crate::packs::json(t, vars)
}

/// The template `t` without its keys whose value names one of `unset` anywhere in it (`sourceMap` with no
/// remote root).
fn without(t: &toml::Value, unset: &[String]) -> toml::Value {
    match t {
        toml::Value::Table(m) => {
            let mut out = Table::new();
            for (k, v) in m.iter().filter(|(_, v)| !names(v, unset)) {
                out.insert(k.clone(), v.clone());
            }
            toml::Value::Table(out)
        }
        v => v.clone(),
    }
}

/// Whether `v` names one of `vars` (`{x}`, `{x/…}`, `{x|re}`) somewhere.
fn names(v: &toml::Value, vars: &[String]) -> bool {
    match v {
        toml::Value::String(s) => {
            vars.iter().any(|n| ["}", "/", "|"].iter().any(|end| s.contains(&format!("{{{n}{end}"))))
        }
        toml::Value::Array(a) => a.iter().any(|x| names(x, vars)),
        toml::Value::Table(m) => m.values().any(|x| names(x, vars)),
        _ => false,
    }
}

// ── Debuggers ────────────────────────────────────────────────────────────

/// A `[debugger.<name>]`.
#[derive(Debug, Clone)]
pub struct Debugger {
    pub name: String,
    /// What `initialize` calls it (`adapterID`).
    pub id: String,
    find: Vec<Find>,
    run: Vec<String>,
    listen: Option<Vec<String>>,
    pub slow_start: Option<SlowStart>,
    pub quiet: Quiet,
}

#[derive(Debug, Clone)]
struct Find {
    how: Look,
    run: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
enum Look {
    Program(String),
    File(Vec<String>),
    Ask(Vec<String>),
}

/// What to say when the debugger hasn't started after a while.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowStart {
    pub after: std::time::Duration,
    pub os: Option<String>,
    pub say: String,
}

impl SlowStart {
    /// Whether it's said on this system.
    pub fn here(&self) -> bool {
        self.os.as_deref().is_none_or(|os| os == std::env::consts::OS)
    }
}

/// The debugger's own output lines: dropped, or shown dim.
#[derive(Debug, Clone, Default)]
pub struct Quiet {
    drop: Vec<Regex>,
    own_first: Option<Regex>,
}

impl Quiet {
    /// Which category an output line shows as — the debugger's own chatter dimmed (`console`), what's of no
    /// use dropped (None). `first`: the debug run's first line.
    pub fn category<'a>(&self, cat: &'a str, line: &str, first: bool) -> Option<&'a str> {
        let program = cat == "stdout" || cat == "stderr";
        if !program && self.drop.iter().any(|r| r.is_match(line)) {
            return None;
        }
        if first && cat == "stdout" && self.own_first.as_ref().is_some_and(|r| r.is_match(line)) {
            return Some("console");
        }
        Some(cat)
    }
}

/// `[debugger.<name>]` from whichever pack has it. Reads the packs: a worker's.
pub fn debugger(name: &str) -> Result<Debugger, String> {
    debugger_in(name, crate::packs::tables_of("debugger"))
}

fn debugger_in(name: &str, tables: Vec<(String, Table)>) -> Result<Debugger, String> {
    let (pack, v) = tables
        .into_iter()
        .find_map(|(pack, t)| Some((pack, t.get(name)?.clone())))
        .ok_or_else(|| format!("no [debugger.{name}] in any pack"))?;
    debugger_of(name, &v).map_err(|e| format!("the {pack} pack: [debugger.{name}]: {e}"))
}

fn debugger_of(name: &str, v: &toml::Value) -> Result<Debugger, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    known(t, &["find", "run", "listen", "adapter-id", "slow-start", "quiet", "own-first"], "it")?;
    let mut find = Vec::new();
    for f in t.get("find").and_then(|f| f.as_array()).ok_or("has no `find` list")? {
        match f {
            toml::Value::String(p) => {
                find.extend(numbered(p)?.into_iter().map(|p| Find { how: Look::Program(p), run: None }))
            }
            toml::Value::Table(e) => {
                known(e, &["program", "file", "ask", "run"], "a `find` entry")?;
                let run = strings(e.get("run"), "run")?;
                let how = match (
                    string(e, "program")?,
                    strings(e.get("file"), "file")?,
                    strings(e.get("ask"), "ask")?,
                ) {
                    (Some(p), None, None) => Look::Program(p),
                    (None, Some(f), None) if !f.is_empty() => Look::File(f),
                    (None, None, Some(a)) if !a.is_empty() => Look::Ask(a),
                    _ => return Err("a `find` entry takes one of `program`, `file` or `ask`".into()),
                };
                find.push(Find { how, run });
            }
            _ => return Err("`find` holds names and tables".into()),
        }
    }
    if find.is_empty() {
        return Err("`find` is empty".into());
    }
    let run = strings(t.get("run"), "run")?.unwrap_or_else(|| vec!["{found}".into()]);
    if run.is_empty() {
        return Err("`run` is empty".into());
    }
    let listen = strings(t.get("listen"), "listen")?;
    if listen.as_ref().is_some_and(|l| !l.iter().any(|a| a.contains("{port}"))) {
        return Err("`listen` has no `{port}`".into());
    }
    let slow_start = match t.get("slow-start") {
        None => None,
        Some(s) => {
            let s = s.as_table().ok_or("`slow-start` isn't a table")?;
            known(s, &["after", "os", "say"], "`slow-start`")?;
            let after =
                s.get("after").and_then(|a| a.as_integer()).ok_or("`slow-start` has no `after` (seconds)")?;
            Some(SlowStart {
                after: std::time::Duration::from_secs(
                    u64::try_from(after).map_err(|_| "`after` is below 0")?,
                ),
                os: string(s, "os")?,
                say: string(s, "say")?.ok_or("`slow-start` has no `say`")?,
            })
        }
    };
    let regex = |r: &str, k: &str| Regex::new(r).map_err(|e| format!("`{k}`: {e}"));
    let dropped = strings(t.get("quiet"), "quiet")?
        .unwrap_or_default()
        .iter()
        .map(|r| regex(r, "quiet"))
        .collect::<Result<_, _>>()?;
    let own_first = string(t, "own-first")?.map(|r| regex(&r, "own-first")).transpose()?;
    Ok(Debugger {
        name: name.to_string(),
        id: string(t, "adapter-id")?.unwrap_or_else(|| name.to_string()),
        find,
        run,
        listen,
        slow_start,
        quiet: Quiet { drop: dropped, own_first },
    })
}

/// `lldb-dap-{22..14}` → `lldb-dap-22` … `lldb-dap-14` (one range at most; else the name as it is).
fn numbered(name: &str) -> Result<Vec<String>, String> {
    let Some((pre, rest)) = name.split_once('{') else { return Ok(vec![name.to_string()]) };
    let Some((range, post)) = rest.split_once('}') else { return Ok(vec![name.to_string()]) };
    let Some((a, b)) = range.split_once("..") else { return Ok(vec![name.to_string()]) };
    let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) else {
        return Err(format!("`{name}`: a range is {{from..to}}, in numbers"));
    };
    if a.abs_diff(b) > 100 {
        return Err(format!("`{name}`: over 100 names"));
    }
    let each: Vec<u32> = if a <= b { (a..=b).collect() } else { (b..=a).rev().collect() };
    Ok(each.iter().map(|n| format!("{pre}{n}{post}")).collect())
}

impl Debugger {
    /// Its command, from the first `find` that finds it (`path`: the `PATH` to look in; `installed`: where its
    /// install recipe puts a tool) — None: it isn't here. Looks at the disk and may run `ask`: a worker's.
    pub fn command(
        &self,
        cwd: &Path,
        path: Option<&OsString>,
        installed: impl Fn(&str) -> Option<PathBuf>,
    ) -> Option<Vec<String>> {
        let on_path =
            |n: &str| path.and_then(|p| std::env::split_paths(p).map(|d| d.join(n)).find(|f| f.is_file()));
        let mut vars = HashMap::new();
        vars.insert("cwd".to_string(), Var::One(cwd.display().to_string()));
        let found = self.find.iter().find_map(|f| {
            let at = match &f.how {
                Look::Program(p) => on_path(&crate::packs::fill(p, &vars)),
                Look::File(files) => files
                    .iter()
                    .filter_map(|f| crate::packs::expand_dir(&crate::packs::fill(f, &vars)))
                    .find(|f| f.is_file()),
                Look::Ask(cmd) => ask(cmd, path),
            }?;
            Some((at, f.run.as_ref().unwrap_or(&self.run)))
        });
        let (at, run) = found.or_else(|| Some((installed(&self.name)?, &self.run)))?;
        vars.insert("found".into(), Var::One(at.display().to_string()));
        Some(crate::packs::args(run, &vars))
    }

    /// `listen`'s arguments for `port` — None: it talks over stdio.
    pub fn listen(&self, port: u16) -> Option<Vec<String>> {
        let vars = HashMap::from([("port".to_string(), Var::One(port.to_string()))]);
        self.listen.as_ref().map(|l| crate::packs::args(l, &vars))
    }

    pub fn tcp(&self) -> bool {
        self.listen.is_some()
    }

    /// One line for the language table's snapshot.
    #[cfg(test)]
    pub(crate) fn summary(&self) -> String {
        let find: Vec<String> = self
            .find
            .iter()
            .map(|f| {
                let how = match &f.how {
                    Look::Program(p) => p.clone(),
                    Look::File(fs) => format!("file:{}", fs.join("|")),
                    Look::Ask(a) => format!("ask:{}", a.join(" ")),
                };
                match &f.run {
                    Some(r) => format!("{how}→{}", r.join(" ")),
                    None => how,
                }
            })
            .collect();
        let mut s =
            format!("{} (id {}): {} · run {}", self.name, self.id, find.join(" "), self.run.join(" "));
        if let Some(l) = &self.listen {
            s += &format!(" · listen {}", l.join(" "));
        }
        if let Some(w) = &self.slow_start {
            s += &format!(" · slow-start {}s {}", w.after.as_secs(), w.os.as_deref().unwrap_or("any"));
        }
        for r in &self.quiet.drop {
            s += &format!(" · quiet {}", r.as_str());
        }
        if let Some(r) = &self.quiet.own_first {
            s += &format!(" · own-first {}", r.as_str());
        }
        s
    }
}

/// The path `cmd` prints (its first line), run with `path` as its `PATH`.
fn ask(cmd: &[String], path: Option<&OsString>) -> Option<PathBuf> {
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stderr(Stdio::null());
    match path {
        Some(p) => c.env("PATH", p),
        None => c.env_remove("PATH"),
    };
    let out = c.output().ok().filter(|o| o.status.success())?;
    let first = String::from_utf8(out.stdout).ok()?.lines().next()?.trim().to_string();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// How `resolve` says the debugger isn't there.
pub const NO_ADAPTER: &str = "no debug adapter";

/// Why a debug run didn't start — and the tool to offer, when it's that one isn't installed.
#[derive(Debug)]
pub struct Failed {
    pub said: String,
    pub offer: Option<String>,
}

impl From<String> for Failed {
    fn from(said: String) -> Failed {
        Failed { said, offer: None }
    }
}

/// `lang`'s debugger and its command for a run in `cwd` — or what's missing. Reads the packs, looks at the
/// disk: a worker's.
pub fn resolve(lang: &str, cwd: &Path) -> Result<(Debugger, Vec<String>), Failed> {
    let name = match of(lang)?.with {
        With::Debugger(d) => d,
        With::Way(w) => return Err(format!("{lang} debugs its own way ({w})").into()),
    };
    let d = debugger(&name)?;
    let path = std::env::var_os("PATH");
    match d.command(cwd, path.as_ref(), crate::packs::installed_at) {
        Some(cmd) => Ok((d, cmd)),
        None => {
            let how = crate::packs::install(&name)
                .map_or_else(|| format!("{name} isn't installed"), |i| i.no_way());
            Err(Failed { said: format!("{NO_ADAPTER} for {lang} — {how}"), offer: Some(name) })
        }
    }
}

// ── Building first ───────────────────────────────────────────────────────

/// A `build` (or `[test] debug-build`) as the pack says it, its templates not filled yet.
#[derive(Debug, Clone)]
pub struct BuildTemplate {
    when: Vec<String>,
    otherwise: Option<String>,
    run: Vec<String>,
    env: Vec<(String, String)>,
    pick: Fields<toml::Value>,
    take: String,
    error: Option<OnError<toml::Value>>,
    none: Option<String>,
}

/// Fields (dotted paths) and the values they hold.
pub type Fields<V> = Vec<(String, V)>;

/// A failed build's line: the one whose fields hold `pick`, its words in the first of `text` it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnError<V> {
    pub pick: Fields<V>,
    pub text: Vec<String>,
}

/// A build to run: the command, and how its JSON lines are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub run: Vec<String>,
    pub env: Vec<(String, String)>,
    /// A line is the one when each field holds its value.
    pub pick: Fields<Value>,
    /// The picked line's field naming what was built.
    pub take: String,
    pub error: Option<OnError<Value>>,
    pub none: String,
}

/// A `build` table; `when` = whether it may say `when` and `otherwise` (`[debug]`'s, not a test's).
pub(crate) fn build_of(v: &toml::Value, when: bool) -> Result<BuildTemplate, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    let mut keys = vec!["run", "env", "pick", "take", "error", "none"];
    if when {
        keys.extend(["when", "otherwise"]);
    }
    known(t, &keys, "it")?;
    let run = strings(t.get("run"), "run")?.filter(|r| !r.is_empty()).ok_or("has no `run`")?;
    let fields = |v: Option<&toml::Value>, k: &str| -> Result<Fields<toml::Value>, String> {
        let t = v.and_then(|v| v.as_table()).ok_or(format!("has no `{k}` table"))?;
        Ok(t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    };
    let env = match t.get("env") {
        None => Vec::new(),
        Some(e) => e
            .as_table()
            .ok_or("`env` isn't a table")?
            .iter()
            .map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())).ok_or("`env`'s values are strings"))
            .collect::<Result<_, _>>()?,
    };
    let error = match t.get("error") {
        None => None,
        Some(e) => {
            let et = e.as_table().ok_or("`error` isn't a table")?;
            known(et, &["pick", "text"], "`error`")?;
            let text = strings(et.get("text"), "error.text")?
                .filter(|t| !t.is_empty())
                .ok_or("`error` has no `text`")?;
            Some(OnError { pick: fields(et.get("pick"), "error.pick")?, text })
        }
    };
    Ok(BuildTemplate {
        when: strings(t.get("when"), "when")?.unwrap_or_default(),
        otherwise: string(t, "otherwise")?,
        run,
        env,
        pick: fields(t.get("pick"), "pick")?,
        take: string(t, "take")?.ok_or("has no `take`")?,
        error,
        none: string(t, "none")?,
    })
}

impl BuildTemplate {
    /// Its templates filled in.
    pub(crate) fn fill(&self, vars: &HashMap<String, Var>) -> Build {
        let pick =
            |p: &[(String, toml::Value)]| p.iter().map(|(k, v)| (k.clone(), filled(v, vars))).collect();
        Build {
            run: crate::packs::args(&self.run, vars),
            env: self.env.iter().map(|(k, v)| (k.clone(), crate::packs::fill(v, vars))).collect(),
            pick: pick(&self.pick),
            take: self.take.clone(),
            error: self.error.as_ref().map(|e| OnError { pick: pick(&e.pick), text: e.text.clone() }),
            none: self.none.clone().unwrap_or_else(|| "nothing built to debug".into()),
        }
    }
}

/// `v`'s field at a dotted path (`target.kind`) — Null where there's none.
fn field<'a>(v: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(v, |v, k| &v[k])
}

/// Whether `got` holds `want`: equal; a list holding it; absolute paths naming the same file.
fn holds(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Array(a), w) if !w.is_array() => a.iter().any(|x| holds(x, w)),
        (Value::String(g), Value::String(w)) if g != w && Path::new(w).is_absolute() => {
            let real = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
            Path::new(g).is_absolute() && real(g) == real(w)
        }
        (g, w) => g == w,
    }
}

fn picks(v: &Value, pick: &[(String, Value)]) -> bool {
    pick.iter().all(|(k, want)| holds(field(v, k), want))
}

/// Runs the build in `cwd` and gives what it built — on failure, its first error as the pack says to read
/// it (else its stderr's last lines). On a worker.
pub fn run_build(b: &Build, cwd: &Path) -> Result<PathBuf, String> {
    let out = Command::new(&b.run[0])
        .args(&b.run[1..])
        .current_dir(cwd)
        .envs(b.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", b.run[0]))?;
    let mut built = None;
    let mut first_error = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if picks(&v, &b.pick) {
            if let Some(p) = field(&v, &b.take).as_str() {
                built = Some(PathBuf::from(p));
            }
        } else if let Some(e) = &b.error
            && first_error.is_none()
            && picks(&v, &e.pick)
        {
            first_error = e.text.iter().find_map(|t| field(&v, t).as_str()).map(str::to_string);
        }
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why: Vec<&str> = match &first_error {
            Some(e) => e.trim_end().lines().take(8).collect(),
            None => {
                let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
                tail.into_iter().rev().collect()
            }
        };
        return Err(crate::dap::with_reason("build failed", &why));
    }
    built.ok_or_else(|| b.none.clone())
}

/// A launch configuration with `{built}` put in.
pub fn with_built(v: Value, built: &Path) -> Value {
    let built = built.display().to_string();
    put_built(v, &built)
}

fn put_built(v: Value, built: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{built}", built)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| put_built(x, built)).collect()),
        // In place, not collected: a Map's `collect` sorts — a copy of the sort for each (~6 KB)
        Value::Object(mut o) => {
            for x in o.values_mut() {
                *x = put_built(x.take(), built);
            }
            Value::Object(o)
        }
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A temp folder of the test's own, as a real path.
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarae-debugpack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn started(a: Attach) -> Value {
        match a {
            Attach::Started(v) => v,
            other => panic!("not the debugger started to attach: {other:?}"),
        }
    }

    fn tcp(a: Attach) -> (Value, String) {
        match a {
            Attach::Tcp(v, d) => (v, d.name),
            other => panic!("not straight over TCP: {other:?}"),
        }
    }

    /// What the code said per language before it was data — the same launch and attach arguments from the
    /// built-in packs.
    #[test]
    fn launch_and_attach_say_what_they_said() {
        let root = tmp("say");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        let file = root.join("pkg/main.go");
        let r = root.display().to_string();

        // Rust: built first in a Cargo project — then the binary built
        let rust = of("rust").unwrap();
        assert_eq!(rust.with, With::Debugger("lldb-dap".into()));
        let Err(e) = rust.launch(&root.join("src/main.rs"), &root) else { panic!("no Cargo.toml here") };
        assert_eq!(e, "open a Cargo project");
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let Ok(Launch::Build(build, pending)) = rust.launch(&root.join("src/main.rs"), &root) else {
            panic!("rust builds first")
        };
        assert_eq!(build.run, ["cargo", "build", "--message-format=json"]);
        assert_eq!(build.env, [("CARGO_TERM_COLOR".to_string(), "never".to_string())]);
        assert_eq!(build.take, "executable");
        assert_eq!(build.none, "no binary target to debug");
        let artifact =
            json!({ "reason": "compiler-artifact", "target": { "kind": ["bin"] }, "executable": "/x" });
        assert!(picks(&artifact, &build.pick), "{:?}", build.pick);
        let lib = json!({ "reason": "compiler-artifact", "target": { "kind": ["lib"] } });
        assert!(!picks(&lib, &build.pick));
        let error = build.error.clone().unwrap();
        let message =
            json!({ "reason": "compiler-message", "message": { "level": "error", "rendered": "E" } });
        assert!(picks(&message, &error.pick) && error.text[0] == "message.rendered");
        assert_eq!(
            pending.with(Path::new("/t/app")),
            json!({ "program": "/t/app", "args": [], "cwd": r, "stopOnEntry": false })
        );

        // Go: the file's package
        let Ok(Launch::Ready(go)) = of("go").unwrap().launch(&file, &root) else { panic!("go launches") };
        let dir = root.join("pkg").display().to_string();
        assert_eq!(go, json!({ "mode": "debug", "program": dir, "cwd": r, "outputMode": "remote" }));

        // Python: the file, with the project's virtualenv's Python
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::fs::write(root.join(".venv/bin/python"), "").unwrap();
        let py = root.join("app.py");
        let Ok(Launch::Ready(p)) = of("python").unwrap().launch(&py, &root) else {
            panic!("python launches")
        };
        let python = root.join(".venv/bin/python").display().to_string();
        assert_eq!(
            p,
            json!({ "program": py, "python": python, "console": "internalConsole", "cwd": r, "justMyCode": true })
        );

        // Java's way is code; C and C++ attach only
        assert!(matches!(of("java").unwrap().launch(&root.join("A.java"), &root), Ok(Launch::Way)));
        for c in ["c", "cpp"] {
            let e = of(c).unwrap().launch(&root.join("a.c"), &root).err().unwrap();
            assert!(e.contains("attach to the program (space G a)"), "{e}");
        }
        assert_eq!(of("markdown").err().unwrap(), "no debugger for markdown");

        // Attach: lldb-dap by pid, or to a gdb-remote stub (program and source map only when given)
        let app = root.join("target/debug/app").display().to_string();
        for lang in ["rust", "c", "cpp"] {
            let d = of(lang).unwrap();
            let a = |pid, port, program, remote| {
                started(d.attach(&root, pid, "10.0.0.5", port, program, remote).unwrap())
            };
            assert_eq!(a(Some(42), None, None, None), json!({ "pid": 42, "cwd": r }), "{lang}");
            assert_eq!(
                a(Some(42), None, Some("target/debug/app"), None),
                json!({ "pid": 42, "program": app, "cwd": r })
            );
            assert_eq!(
                a(None, Some(1234), None, None),
                json!({ "attachCommands": ["gdb-remote 10.0.0.5:1234"], "cwd": r })
            );
            assert_eq!(
                a(None, Some(1234), Some("target/debug/app"), Some("/app")),
                json!({ "attachCommands": ["gdb-remote 10.0.0.5:1234"], "program": app,
                        "sourceMap": [["/app", r]], "cwd": r })
            );
            assert_eq!(
                d.attach(&root, None, "h", None, None, None).err().unwrap(),
                "needs a pid or host:port"
            );
        }
        // dlv and debugpy: by pid through the debugger started here, else straight to the program's server
        let go = of("go").unwrap();
        assert_eq!(
            started(go.attach(&root, Some(7), "h", None, None, None).unwrap()),
            json!({ "mode": "local", "processId": 7, "cwd": r })
        );
        assert_eq!(
            tcp(go.attach(&root, None, "h", Some(2345), None, None).unwrap()),
            (json!({ "mode": "remote" }), "dlv".into())
        );
        assert_eq!(
            tcp(go.attach(&root, None, "h", Some(2345), None, Some("/src")).unwrap()).0,
            json!({ "mode": "remote", "substitutePath": [{ "from": r, "to": "/src" }] })
        );
        let python = of("python").unwrap();
        assert_eq!(
            started(python.attach(&root, Some(7), "h", None, None, None).unwrap()),
            json!({ "processId": 7, "justMyCode": false, "cwd": r })
        );
        assert_eq!(
            tcp(python.attach(&root, None, "h", Some(5678), None, None).unwrap()),
            (json!({ "justMyCode": false }), "debugpy".into())
        );
        assert_eq!(
            tcp(python.attach(&root, None, "h", Some(5678), None, Some("/app")).unwrap()).0,
            json!({ "justMyCode": false, "pathMappings": [{ "localRoot": r, "remoteRoot": "/app" }] })
        );
        assert!(matches!(
            of("java").unwrap().attach(&root, None, "h", Some(5005), None, None),
            Ok(Attach::Way)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Each debugger is found as its pack says: lldb-dap by its names (the newest numbered one first), then
    /// what xcrun says; debugpy through the virtualenv's Python, else its launcher; dlv where its install
    /// recipe put it, run as `dlv dap` on a port tarae picks.
    #[test]
    fn adapters_are_found_as_their_packs_say() {
        let d = tmp("find");
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = OsString::from(&bin);
        let touch = |p: &Path| std::fs::write(p, "").unwrap();
        let none = |_: &str| None;
        let s = |p: &Path| p.display().to_string();

        let lldb = debugger("lldb-dap").unwrap();
        assert_eq!(lldb.command(&d, Some(&path), none), None, "nothing here (and no xcrun on this PATH)");
        // xcrun's answer, last — a stand-in that prints a path
        let xcrun = bin.join("xcrun");
        std::fs::write(&xcrun, "#!/bin/sh\n[ \"$1 $2\" = \"-f lldb-dap\" ] && echo /opt/x/lldb-dap\n")
            .unwrap();
        std::fs::set_permissions(&xcrun, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(lldb.command(&d, Some(&path), none), Some(vec!["/opt/x/lldb-dap".to_string()]));
        touch(&bin.join("lldb-vscode-15"));
        touch(&bin.join("lldb-dap-18"));
        assert_eq!(lldb.command(&d, Some(&path), none), Some(vec![s(&bin.join("lldb-dap-18"))]));
        touch(&bin.join("lldb-vscode"));
        assert_eq!(lldb.command(&d, Some(&path), none), Some(vec![s(&bin.join("lldb-vscode"))]));
        assert!(!lldb.tcp());
        assert!(
            lldb.slow_start
                .as_ref()
                .is_some_and(|w| w.after.as_secs() == 8 && w.os.as_deref() == Some("macos"))
        );

        let dlv = debugger("dlv").unwrap();
        assert_eq!(dlv.id, "go");
        assert_eq!(dlv.command(&d, Some(&path), none), None);
        let fetched = |t: &str| (t == "dlv").then(|| PathBuf::from("/home/u/go/bin/dlv"));
        assert_eq!(
            dlv.command(&d, Some(&path), fetched),
            Some(vec!["/home/u/go/bin/dlv".into(), "dap".into()])
        );
        touch(&bin.join("dlv"));
        assert_eq!(dlv.command(&d, Some(&path), fetched), Some(vec![s(&bin.join("dlv")), "dap".into()]));
        assert_eq!(dlv.listen(4711), Some(vec!["-l".into(), "127.0.0.1:4711".into()]));

        let debugpy = debugger("debugpy").unwrap();
        // ($VIRTUAL_ENV, when this test runs inside one, would come before the launcher)
        if std::env::var_os("VIRTUAL_ENV").is_none() {
            assert_eq!(debugpy.command(&d, Some(&path), none), None);
            touch(&bin.join("python3"));
            let py = vec![s(&bin.join("python3")), "-m".into(), "debugpy.adapter".into()];
            assert_eq!(debugpy.command(&d, Some(&path), none), Some(py));
            touch(&bin.join("debugpy-adapter"));
            assert_eq!(debugpy.command(&d, Some(&path), none), Some(vec![s(&bin.join("debugpy-adapter"))]));
        }
        std::fs::create_dir_all(d.join(".venv/bin")).unwrap();
        touch(&d.join(".venv/bin/python"));
        let venv = vec![s(&d.join(".venv/bin/python")), "-m".into(), "debugpy.adapter".into()];
        assert_eq!(debugpy.command(&d, Some(&path), none), Some(venv));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every built-in `[debug]` reads and names a debugger some pack has; every `[debugger]` reads. A
    /// mistake in one is said, not skipped.
    #[test]
    fn every_debug_table_reads() {
        let mut langs = 0;
        for lang in crate::runtime::pack_langs() {
            let Some(d) = read(&lang) else { continue };
            let d = d.unwrap();
            langs += 1;
            if let With::Debugger(name) = &d.with {
                debugger(name).unwrap_or_else(|e| panic!("{lang}: {e}"));
            }
        }
        assert_eq!(langs, 6, "rust c cpp go python java");
        for (pack, t) in crate::packs::tables_of("debugger") {
            for name in t.keys() {
                debugger(name).unwrap_or_else(|e| panic!("{pack}: {e}"));
            }
        }
        let bad = |t: &str| debug_of("x", &toml::Value::Table(t.parse::<Table>().unwrap())).err().unwrap();
        assert!(bad("debuger = \"d\"").contains("no `debuger`"));
        assert!(bad("debugger = \"d\"\nway = \"java\"").contains("one of"));
        assert!(bad("way = \"kotlin\"").contains("no `way = \"kotlin\"`"));
        assert!(bad("debugger = \"d\"\nhelpers = [\"cargo\"]").contains("no helper `cargo`"));
        assert!(bad("debugger = \"d\"\nlaunch = 1").contains("`launch` isn't a table"));
        assert!(
            bad("debugger = \"d\"\n[build]\nrun = [\"b\"]\ntake = \"x\"\npick = {}").contains("no `launch`")
        );
        assert!(
            bad("debugger = \"d\"\nlaunch = {}\n[build]\nrun = [\"b\"]\npick = {}").contains("no `take`")
        );
        let bad = |t: &str| debugger_of("d", &toml::Value::Table(t.parse::<Table>().unwrap())).err().unwrap();
        assert!(bad("run = [\"x\"]").contains("no `find`"));
        assert!(bad("find = [\"x\"]\nlisten = [\"-l\"]").contains("no `{port}`"));
        assert!(bad("find = [\"x\"]\nquiet = [\"(\"]").contains("`quiet`"));
        assert!(bad("find = [\"x-{a..3}\"]").contains("in numbers"));
        assert!(bad("find = [{ program = \"x\", file = [\"y\"] }]").contains("one of"));
        assert_eq!(numbered("a-{3..1}").unwrap(), ["a-3", "a-2", "a-1"]);
        assert_eq!(numbered("{cwd}/x").unwrap(), ["{cwd}/x"]);
    }

    /// A pack of one's own replaces `[debug]` whole and a `[debugger.<name>]` entry by entry — the others
    /// stay.
    #[test]
    fn ones_own_pack_replaces_debug_and_one_debugger() {
        let builtin = crate::runtime::pack_builtin("go/pack.toml");
        let mine = "[debug]\ndebugger = \"mine\"\n[debugger.mine]\nfind = [\"mine\"]\n";
        let m = crate::packs::merged("go", builtin, Some(mine.into())).unwrap().unwrap();
        let d = debug_of("go", &m["debug"]).unwrap();
        assert_eq!(d.with, With::Debugger("mine".into()));
        assert!(d.launch.is_none() && d.attach_tcp.is_none(), "replaced whole");
        let names: Vec<&String> = m["debugger"].as_table().unwrap().keys().collect();
        assert_eq!(names, ["dlv", "mine"]);
    }

    #[test]
    fn a_failed_build_says_its_stderr_when_no_line_is_an_error() {
        let d = tmp("build");
        let b = Build {
            run: vec![
                "sh".into(),
                "-c".into(),
                "echo '{\"x\":1}'; echo one >&2; echo two >&2; exit 3".into(),
            ],
            env: Vec::new(),
            pick: vec![("x".into(), json!(2))],
            take: "x".into(),
            error: None,
            none: "nothing".into(),
        };
        assert_eq!(run_build(&b, &d).unwrap_err(), "build failed\none\ntwo");
        let ok = Build {
            run: vec!["sh".into(), "-c".into(), "echo '{\"x\":1,\"p\":\"/b\"}'".into()],
            ..b.clone()
        };
        assert_eq!(run_build(&ok, &d).unwrap_err(), "nothing", "built, nothing picked");
        let picked = Build { pick: vec![("x".into(), json!(1))], take: "p".into(), ..ok };
        assert_eq!(run_build(&picked, &d).unwrap(), PathBuf::from("/b"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
