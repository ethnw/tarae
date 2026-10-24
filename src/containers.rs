//! The containers a run of Claude's starts, stopped with it (the user's call, 2026-10-08 — "Stop it" on a quiet
//! Docker run killed the client and left the container running). No command line of Claude's is rewritten:
//! each run of Claude's gets `TARAE_CONTAINER_PREFIX` (`tarae-<tarae's pid>-s<session>-r<run>`), and a script
//! that starts containers names them from it (`cargo xtask linux`: `<prefix>-checks`).
//!
//! When the run is stopped — "Stop it" on its card, `space m c` / `space x c`, dropped for another run — tarae
//! lists each engine's running containers (`[[containers]]`, the packs' data: its program, how its containers
//! are listed, how one is stopped) on a worker, outside the sandbox. The run's own (its prefix) are stopped by
//! name — never by pid. Others made while it ran (one Claude started by hand, no prefix) aren't guessed at: a
//! card names them (`What::Containers`) — `y` stops them, by name, `n` leaves them running. One carrying another
//! run's prefix (a parallel session's `cargo xtask linux`) is that run's: left alone, said, never on the card. A
//! run that ends by itself leaves its containers as they are (one started detached may be meant to stay).
//!
//! Which engine: the one the run reached (`Reach`) — the engine's settings (`DOCKER_HOST`…, the packs' data)
//! as its environment and its line (`DOCKER_HOST=… cargo xtask linux`) gave them, and only those; none named,
//! the engine's default. Never the user's own config folder (`~/.docker`: its credential helpers would run) —
//! each call gets an empty one of tarae's own (`Folder`): listing and stopping need no credentials.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::editor::Editor;
use crate::readonly::Part;

/// The variable a run of Claude's gets its prefix in.
pub const VAR: &str = "TARAE_CONTAINER_PREFIX";

/// Seconds a container may look made before its run started: the engine's clock (a VM's, another host's) may
/// be behind tarae's.
const SKEW: i64 = 5;

/// A container engine, as the packs say it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Engine {
    pub program: String,
    /// What lists its running containers: a name, a tab, when it was made — a line each.
    pub list: Vec<String>,
    /// What stops one: `{name}` its name.
    pub stop: Vec<String>,
    /// The variables that say which engine a call reaches (`DOCKER_HOST`, `DOCKER_CONTEXT`…): a run's values,
    /// and only those, are what its calls get.
    pub settings: Vec<String>,
    /// The engine's config folder, made an empty one of tarae's for every call.
    pub config: Option<Config>,
}

/// An engine's config folder: its variable, where it is unless a run's variable says (`~/…`), and what of it is
/// copied into tarae's empty one when a run named a setting looked up there (`for`: a context's endpoint — no
/// credentials, no keys).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub var: String,
    pub home: String,
    pub copy: Vec<String>,
    pub copy_for: Vec<String>,
}

impl Engine {
    /// Every variable of a run's that says which engine it reached: the settings and the config folder's.
    fn names(&self) -> impl Iterator<Item = &str> {
        self.settings.iter().map(String::as_str).chain(self.config.as_ref().map(|c| c.var.as_str()))
    }
}

/// The built-in packs' engines (`[[containers]]` — tarae's own only, `packs::Reach::BuiltIn`: what's run
/// here runs outside the sandbox). One that doesn't read is logged and left out.
pub fn engines() -> Vec<Engine> {
    let mut out = Vec::new();
    for (pack, t) in crate::packs::builtin_manifests() {
        let Some(v) = t.as_ref().ok().and_then(|t| t.get("containers")) else { continue };
        match engines_of(v) {
            Ok(e) => out.extend(e),
            Err(e) => crate::log::line("tarae", "warning", &format!("the {pack} pack: {e}")),
        }
    }
    out
}

/// A pack's `[[containers]]`, read.
pub fn engines_of(v: &toml::Value) -> Result<Vec<Engine>, String> {
    let list = v.as_array().ok_or("`containers` isn't a list of tables ([[containers]])")?;
    let mut out = Vec::new();
    for e in list {
        let t = e.as_table().ok_or("a [[containers]] entry isn't a table")?;
        let known = ["program", "list", "stop", "settings", "config"];
        if let Some(k) = t.keys().find(|k| !known.contains(&k.as_str())) {
            return Err(format!("[[containers]] has no `{k}`"));
        }
        let program =
            t.get("program").and_then(toml::Value::as_str).ok_or("[[containers]] needs `program`")?;
        let settings = match t.get("settings") {
            Some(v) => words(v, "settings")?,
            None => Vec::new(),
        };
        let need = |k: &str| t.get(k).ok_or(format!("[[containers]] needs `{k}`"));
        let e = Engine {
            program: program.to_string(),
            list: words(need("list")?, "list")?,
            stop: words(need("stop")?, "stop")?,
            config: t.get("config").map(config_of).transpose()?,
            settings,
        };
        if !e.stop.iter().any(|a| a.contains("{name}")) {
            return Err(format!("{}: `stop` names no {{name}}", e.program));
        }
        if let Some(n) = e.names().find(|n| !crate::readonly::env_core(n)) {
            return Err(format!("{}: `{n}` isn't a variable tarae passes on", e.program));
        }
        out.push(e);
    }
    Ok(out)
}

/// A list of words, `k` naming it in what's said.
fn words(v: &toml::Value, k: &str) -> Result<Vec<String>, String> {
    let v = v.as_array().ok_or(format!("`{k}`: a list of words"))?;
    v.iter().map(|w| w.as_str().map(str::to_string).ok_or(format!("`{k}`: words only"))).collect()
}

/// An engine's `config = { var, home, copy, for }`: `home` from `~/`, `copy` folders below it (no `..`).
fn config_of(v: &toml::Value) -> Result<Config, String> {
    let t = v.as_table().ok_or("`config` isn't a table")?;
    if let Some(k) = t.keys().find(|k| !["var", "home", "copy", "for"].contains(&k.as_str())) {
        return Err(format!("`config` has no `{k}`"));
    }
    let text = |k: &str| t.get(k).and_then(toml::Value::as_str).ok_or(format!("`config` needs `{k}`"));
    let list = |k: &str| t.get(k).map_or(Ok(Vec::new()), |v| words(v, k));
    let c = Config {
        var: text("var")?.to_string(),
        home: text("home")?.to_string(),
        copy: list("copy")?,
        copy_for: list("for")?,
    };
    if !c.home.starts_with("~/") {
        return Err(format!("`config.home` {:?} isn't from `~/`", c.home));
    }
    let below = |p: &str| {
        let p = Path::new(p);
        p.is_relative() && p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
    };
    if let Some(p) = c.copy.iter().find(|p| !below(p)) {
        return Err(format!("`config.copy` {p:?} isn't a folder below it"));
    }
    Ok(c)
}

/// A run's prefix: this tarae (its pid — a tarae started later numbers its runs from 1 again), the session,
/// the run (`run#<id>`).
pub fn prefix(session: crate::event::SessionId, run: u64) -> String {
    format!("tarae-{}-s{session}-r{run}", std::process::id())
}

/// Whether container `name` is the run's of `prefix`: the prefix itself, or it then `-…` (run 1's isn't run
/// 12's).
pub fn ours(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// Whether container `name` carries a run's prefix (`prefix`'s form: `tarae-<pid>-s<n>-r<m>`, then nothing or
/// `-…`) — some run's, this tarae's or another's.
pub fn a_runs(name: &str) -> bool {
    let number = |s: &str, before: &str| -> Option<usize> {
        let s = s.strip_prefix(before)?;
        let n = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        (n > 0).then_some(before.len() + n)
    };
    let mut rest = name;
    for before in ["tarae-", "-s", "-r"] {
        let Some(n) = number(rest, before) else { return false };
        rest = &rest[n..];
    }
    rest.is_empty() || rest.starts_with('-')
}

/// When a container was made, as `{{.CreatedAt}}` says it (`2026-10-08 21:17:03 +0900 KST`, a fraction of a
/// second too) — seconds since the epoch. None: not that form.
pub fn created(s: &str) -> Option<i64> {
    let mut parts = s.split_whitespace();
    let (date, time, zone) = (parts.next()?, parts.next()?, parts.next()?);
    let numbers = |s: &str, sep: char| -> Option<Vec<i64>> { s.split(sep).map(|p| p.parse().ok()).collect() };
    let (d, t) = (numbers(date, '-')?, numbers(time.split('.').next()?, ':')?);
    let (&[y, m, day], &[h, min, sec]) = (d.as_slice(), t.as_slice()) else { return None };
    let (sign, z) = match zone.split_at_checked(1)? {
        ("+", z) => (1, z),
        ("-", z) => (-1, z),
        _ => return None,
    };
    let (zh, zm) = (z.get(..2)?.parse::<i64>().ok()?, z.get(2..4)?.parse::<i64>().ok()?);
    Some(days(y, m, day) * 86_400 + h * 3600 + min * 60 + sec - sign * (zh * 3600 + zm * 60))
}

/// Days from 1970-01-01 to a civil date.
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// What a stopped run's look finds in an engine's list.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Look {
    /// The run's own (its prefix), whenever made: stopped.
    pub own: Vec<String>,
    /// Others made since it started, under no run's prefix: asked about.
    pub meanwhile: Vec<String>,
    /// Made since it started under another run's prefix (a parallel session's `cargo xtask linux`): that run's
    /// — left alone, said, never on the card.
    pub others: Vec<String>,
}

/// An engine's list (`listed`: a name, a tab, when made — a line each) as a run of `prefix` started at `since`
/// (seconds) sees it. One whose time can't be read, not its own, is in none: not guessed at.
pub fn look(listed: &str, prefix: &str, since: i64) -> Look {
    let mut look = Look::default();
    for l in listed.lines() {
        let (name, when) = l.split_once('\t').unwrap_or((l, ""));
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if ours(name, prefix) {
            look.own.push(name.to_string());
        } else if created(when).is_some_and(|c| c >= since - SKEW) {
            match a_runs(name) {
                true => look.others.push(name.to_string()),
                false => look.meanwhile.push(name.to_string()),
            }
        }
    }
    look
}

/// Which engine a run reached, as far as tarae can tell: of every engine's variables (`Engine::names`), its
/// environment's values, and what each part of its line sets in front of its command (`DOCKER_HOST=… cargo
/// xtask linux`, `env DOCKER_HOST=… …`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    env: Vec<(String, String)>,
    parts: Vec<Vec<(String, String)>>,
    /// Its line isn't one `readonly::parse_env` reads (`$…`, backticks): its environment alone says.
    unread: bool,
}

impl Reach {
    /// `line`'s (None: not a shell line — tests run by their words), with `env` its environment.
    pub fn of(line: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Reach {
        let engines = engines();
        let names: Vec<&str> = engines.iter().flat_map(Engine::names).collect();
        let env = names.iter().filter_map(|n| Some((n.to_string(), env(n)?))).collect();
        let (parts, unread) = match line.map(crate::readonly::parse_env) {
            None => (Vec::new(), false),
            Some(None) => (Vec::new(), true),
            Some(Some((parts, _))) => {
                let set = parts.iter().filter_map(|p| match p {
                    Part::Cmd(w) => Some(assignments(w)),
                    Part::Cd(_) => None,
                });
                let named = set.map(|a| a.into_iter().filter(|(n, _)| names.contains(&n.as_str())).collect());
                (named.filter(|a: &Vec<_>| !a.is_empty()).collect(), false)
            }
        };
        Reach { env, parts, unread }
    }

    /// A run's, from the command it's spawned with: what that sets or removes, else tarae's own.
    pub fn of_command(line: Option<&str>, cmd: &Command) -> Reach {
        let set: Vec<(OsString, Option<OsString>)> =
            cmd.get_envs().map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned))).collect();
        Reach::of(line, |n| match set.iter().find(|(k, _)| k == n) {
            Some((_, v)) => v.as_ref().map(|v| v.to_string_lossy().into_owned()),
            None => std::env::var(n).ok(),
        })
    }

    /// The settings `e`'s calls are made with — one for each way the run reached it: each part of its line
    /// that names one of `e`'s, over its environment; none does: its environment alone (nothing: the
    /// engine's default).
    pub fn sets(&self, e: &Engine) -> Vec<Vec<(String, String)>> {
        let mine = |n: &String| e.names().any(|m| m == n);
        let base: Vec<_> = self.env.iter().filter(|(n, _)| mine(n)).cloned().collect();
        let parts: Vec<_> = self.parts.iter().filter(|p| p.iter().any(|(n, _)| mine(n))).collect();
        if parts.is_empty() {
            return vec![sorted(base)];
        }
        let mut out = Vec::new();
        for p in parts {
            let mut s = base.clone();
            for (n, v) in p.iter().filter(|(n, _)| mine(n)) {
                s.retain(|(m, _)| m != n);
                s.push((n.clone(), v.clone()));
            }
            let s = sorted(s);
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

fn sorted(mut v: Vec<(String, String)>) -> Vec<(String, String)> {
    crate::sorting::sort(&mut v);
    v
}

/// A command's leading `NAME=value` words, and `env`'s after them (`env A=1 cmd`) — in order, a later one of a
/// name winning.
fn assignments(w: &[crate::readonly::Word]) -> Vec<(String, String)> {
    let assign = |w: &crate::readonly::Word| {
        let (n, v) = w.text.split_once('=')?;
        let name = n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        name.then(|| (n.to_string(), v.to_string()))
    };
    let mut out: Vec<(String, String)> = w.iter().map_while(assign).collect();
    let rest = &w[out.len()..];
    if rest.first().is_some_and(|x| x.text == "env") {
        out.extend(rest[1..].iter().map_while(assign));
    }
    out
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// What a run of Claude's needs to stop its containers once it's stopped: its prefix, when it started, what
/// it was (for the card), its session's sender (the card goes there), the engines, where their programs are
/// found (PATH), which engine it reached, and home (where an engine's config folder is unless the run says).
pub struct Run {
    pub prefix: String,
    started: SystemTime,
    label: String,
    tx: crate::event::Tx,
    path: Option<OsString>,
    engines: Vec<Engine>,
    reach: Reach,
    home: Option<PathBuf>,
}

impl Run {
    /// A run of Claude's starting now. (Tests: no PATH — no engine is found; a test gives its stand-in's — and
    /// no home: the user's config folder is never read.)
    pub fn new(prefix: String, label: String, tx: crate::event::Tx, reach: Reach) -> Run {
        let path = if cfg!(test) { None } else { std::env::var_os("PATH") };
        let home = if cfg!(test) { None } else { crate::allow::home() };
        Run { prefix, started: SystemTime::now(), label, tx, path, engines: engines(), reach, home }
    }

    /// One whose engines' programs are found on `path` (a test's stand-in folder).
    #[cfg(test)]
    pub fn on(prefix: &str, label: &str, tx: crate::event::Tx, path: &Path) -> Run {
        let mut r = Run::new(prefix.into(), label.into(), tx, Reach::default());
        r.path = Some(path.as_os_str().to_owned());
        r
    }

    /// The same, reaching `reach`, home `home` (a test's stand-in).
    #[cfg(test)]
    pub fn reaching(mut self, reach: Reach, home: Option<&Path>) -> Run {
        (self.reach, self.home) = (reach, home.map(Path::to_path_buf));
        self
    }

    /// A run tarae started, its engines' programs found on `path` (a stand-in's folder, or the live test's
    /// PATH) — the engine it reached kept as its start read it.
    #[cfg(test)]
    pub fn find_on(&mut self, path: &OsStr) {
        self.path = Some(path.to_owned());
    }

    /// The run was stopped: its containers, on a worker (`stop_now`).
    pub fn stopped(self) {
        crate::event::thread("tarae-container", move || {
            self.stop_now();
        });
    }

    /// The run's own containers stopped, by name, on each engine it reached — said in its session, with another
    /// run's left alone — and the others made while it ran asked about there (`What::Containers`). What was
    /// stopped.
    pub fn stop_now(self) -> Vec<String> {
        let since = secs(self.started);
        let (mut stopped, mut others, mut failed) = (Vec::new(), Vec::new(), Vec::new());
        if self.reach.unread {
            let why =
                format!("{}: its line isn't read — its environment alone says which engine", self.prefix);
            crate::log::line("containers", "reach", &why);
        }
        for e in &self.engines {
            let Some(path) = find(&e.program, self.path.as_deref()) else { continue };
            for set in self.reach.sets(e) {
                let at = At { engine: e.clone(), path: path.clone(), set, home: self.home.clone() };
                let look = {
                    let calls = match Calls::of(&at) {
                        Ok(c) => c,
                        Err(why) => {
                            failed.push(format!("{}: {why}", at.named()));
                            continue;
                        }
                    };
                    let look = match calls.run(&e.list) {
                        Ok(listed) => look(&listed, &self.prefix, since),
                        // (the default engine not running is the usual case, nothing to say; one the run named is)
                        Err(why) => {
                            if !at.set.is_empty() {
                                failed.push(format!("{} couldn't list its containers: {why}", at.named()));
                            }
                            continue;
                        }
                    };
                    for name in &look.own {
                        match calls.stop(name) {
                            true => stopped.push(name.clone()),
                            false => failed.push(format!("{} didn't stop {name}", at.named())),
                        }
                    }
                    look
                };
                if !look.own.is_empty() {
                    let line = format!("{} {}: {}", at.named(), self.prefix, look.own.join(" "));
                    crate::log::line("containers", "stop", &line);
                }
                others.extend(look.others);
                if !look.meanwhile.is_empty() {
                    let ask = Ask { at, names: look.meanwhile, run: self.label.clone() };
                    let _ = self
                        .tx
                        .send(crate::event::Event::Job(Box::new(move |ed: &mut Editor| asked(ed, ask))));
                }
            }
        }
        let mut said = Vec::new();
        if !stopped.is_empty() {
            said.push(format!("stopped with it, by name: {}", stopped.join(", ")));
        }
        if !others.is_empty() {
            said.push(format!("another run's, left alone: {}", others.join(", ")));
        }
        let warn = !failed.is_empty();
        said.extend(failed);
        if !said.is_empty() {
            let said = said.join(" · ");
            let _ = self.tx.send(crate::event::Event::Job(Box::new(move |ed: &mut Editor| match warn {
                true => ed.set_warning(said),
                false => ed.set_status(said),
            })));
        }
        stopped
    }
}

/// `program` on `path` (its folders in order) — None: not there.
fn find(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?).map(|d| d.join(program)).find(|p| p.is_file())
}

/// An engine as a run reached it: its program (found on PATH), the run's settings for it, home (where its
/// config folder is unless those say).
#[derive(Clone, Debug, PartialEq)]
pub struct At {
    engine: Engine,
    path: PathBuf,
    set: Vec<(String, String)>,
    home: Option<PathBuf>,
}

impl At {
    /// How it's named to people: `docker`, or `docker (DOCKER_HOST=unix:///…)` — what the run named.
    pub fn named(&self) -> String {
        let set: Vec<String> = self.set.iter().map(|(n, v)| format!("{n}={v}")).collect();
        match set.is_empty() {
            true => self.engine.program.clone(),
            false => format!("{} ({})", self.engine.program, set.join(" ")),
        }
    }
}

/// Calls to an engine as a run reached it: with the run's settings and nothing else of the kind (tarae's own
/// `DOCKER_HOST` isn't the run's), and the engine's config folder an empty one of tarae's (`Folder`) — never
/// the user's, whose credential helpers would run.
struct Calls<'a> {
    at: &'a At,
    folder: Option<Folder>,
}

impl<'a> Calls<'a> {
    fn of(at: &'a At) -> Result<Calls<'a>, String> {
        Ok(Calls { at, folder: Folder::made(at)? })
    }

    /// `args` run on it — its output, or its last line of complaint.
    fn run(&self, args: &[String]) -> Result<String, String> {
        let e = &self.at.engine;
        let mut c = Command::new(&self.at.path);
        c.args(args).stdin(std::process::Stdio::null());
        for n in e.names() {
            c.env_remove(n);
        }
        let config = e.config.as_ref().map(|c| c.var.as_str());
        c.envs(self.at.set.iter().filter(|(n, _)| Some(n.as_str()) != config).map(|(n, v)| (n, v)));
        match (config, &self.folder) {
            (Some(var), Some(f)) => c.env(var, &f.0),
            (Some(_), None) => return Err("no config folder of tarae's own".into()),
            (None, _) => &mut c,
        };
        let out = c.output().map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed").to_string())
    }

    /// Container `name` stopped with the engine's `stop` words — whether it said it did.
    fn stop(&self, name: &str) -> bool {
        let args: Vec<String> = self.at.engine.stop.iter().map(|a| a.replace("{name}", name)).collect();
        self.run(&args).is_ok()
    }
}

/// An engine's config folder for one look: an empty one of tarae's own (0700, in the temp folder), gone with
/// it. When the run named a setting looked up there (`config.for` — a context), what that needs
/// (`config.copy`) is copied in from the run's folder (its variable, else `config.home`): plain files only,
/// none through a link, `COPY_MAX` in all.
struct Folder(PathBuf);

const COPY_MAX: u64 = 1 << 20;

impl Folder {
    fn made(at: &At) -> Result<Option<Folder>, String> {
        use std::os::unix::fs::DirBuilderExt;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let Some(c) = &at.engine.config else { return Ok(None) };
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tarae-engine-{}-{n}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let folder = Folder(dir);
        if at.set.iter().any(|(n, _)| c.copy_for.contains(n)) {
            let theirs = match at.set.iter().find(|(n, _)| *n == c.var) {
                Some((_, v)) => Some(PathBuf::from(v)).filter(|p| p.is_absolute()),
                None => at.home.as_ref().map(|h| h.join(&c.home[2..])),
            };
            let mut left = COPY_MAX;
            for (t, sub) in theirs.iter().flat_map(|t| c.copy.iter().map(move |s| (t, s))) {
                copy_in(&t.join(sub), &folder.0.join(sub), &mut left);
            }
        }
        Ok(Some(folder))
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `from` — a folder or a plain file, never through a link — copied to `to`, up to `left` bytes in all.
fn copy_in(from: &Path, to: &Path, left: &mut u64) {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(m) = std::fs::symlink_metadata(from) else { return };
    if m.is_dir() {
        if std::fs::create_dir_all(to).is_err() {
            return;
        }
        // (in name order: what the budget leaves out is the same on every file system)
        let Ok(rd) = std::fs::read_dir(from) else { return };
        for (name, e) in crate::sorting::listing(rd, usize::MAX).unwrap_or_default() {
            copy_in(&e.path(), &to.join(name), left);
        }
    } else if m.is_file() && m.len() <= *left {
        let open = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(from);
        let Ok(src) = open else { return };
        let Ok(mut dst) = std::fs::OpenOptions::new().write(true).create_new(true).open(to) else { return };
        if let Ok(n) = std::io::copy(&mut src.take(*left), &mut dst) {
            *left -= n;
        }
    }
}

/// Containers made on an engine while a stopped run went, under no run's name: what the card asks about.
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    at: At,
    pub names: Vec<String>,
    /// The run, as its card names it.
    pub run: String,
}

#[cfg(test)]
impl Ask {
    /// A card's ask in a test: on an engine that is no program.
    pub(crate) fn for_test(names: Vec<String>) -> Ask {
        let (list, stop, settings) = (Vec::new(), Vec::new(), Vec::new());
        let engine = Engine { program: "no-such-engine".into(), list, stop, settings, config: None };
        let at = At { engine, path: "/nowhere".into(), set: Vec::new(), home: None };
        Ask { at, names, run: "run#1".into() }
    }
}

/// The card about them (`What::Containers`), in the run's session.
fn asked(ed: &mut Editor, ask: Ask) {
    let list = ask.names.iter().map(|n| format!("    {n}")).collect::<Vec<_>>().join("\n");
    let q = format!(
        "{} was stopped, and the containers it named as its own with it. These started on {} while it ran, \
         under other names — tarae doesn't guess they were its:\n{list}\n\ny stops them ({} by name) · n leaves \
         them running",
        ask.run,
        ask.at.named(),
        ask.at.engine.stop.first().map(String::as_str).unwrap_or("stop")
    );
    let title = match ask.names.len() {
        1 => "Stop a container it may have started?".to_string(),
        n => format!("Stop {n} containers it may have started?"),
    };
    let what = crate::offer::What::Containers(Box::new(ask));
    ed.push_offer(crate::offer::Offer::new(title, &q, "Stopping…", String::new(), what));
}

/// The card's answer: `y` stops them, by name, on a worker — said; `n` leaves them, said.
pub fn answer(ed: &mut Editor, card: u64, ask: Ask, yes: bool) {
    let on = ask.at.named();
    if !yes {
        return ed.set_status(format!("left running on {on}: {}", ask.names.join(", ")));
    }
    ed.offer_finished(card, Ok(()));
    ed.events.jobs().spawn(move || {
        let (done, not): (Vec<String>, Vec<String>) = match Calls::of(&ask.at) {
            Ok(calls) => ask.names.iter().cloned().partition(|n| calls.stop(n)),
            Err(_) => (Vec::new(), ask.names.clone()),
        };
        move |ed: &mut Editor| match not.is_empty() {
            true => ed.set_status(format!("stopped on {on}: {}", done.join(", "))),
            false => {
                ed.set_warning(format!("{on} didn't stop {} — stopped: {}", not.join(", "), done.join(", ")))
            }
        }
    });
}

#[cfg(test)]
mod tests;
