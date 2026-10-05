//! Tools' build folders, as their packs say — `[builds]`, read from the built-in packs only (it decides what
//! is deleted; a runtime folder's is ignored and said, `packs::Reach::BuiltIn`): where a checkout builds (the
//! tool asked), whether its build folder is one per checkout (a new session's checkout then builds cold), and
//! at a session's end whether its own build folder goes with it — only when every rule of `Builds::fate` holds.
//!
//! `[builds]`: `name` (as the session card says it), `when` (files a checkout of this tool's has at its top),
//! `ask` (a command run in a checkout, `WAIT` at most, printing JSON), `target` and `build` (its fields — the
//! build folder is the target when the answer has none), `home = { env, else }` (the tool's home: the variable,
//! else `~/…`), `setting = { env, files, home-files, key, per-checkout }` (where the build folder is set: the
//! variable, else the nearest of `files` up from the checkout, else `home-files` in the home; the TOML key; what
//! in its value makes it one per checkout), `remove = { under, depth }` (a session's own build folder may go
//! from exactly `depth` folders below `{home}/{under}`), `keep` (folders in the home never removed, added to
//! the floor).

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use toml::Table;

use crate::packs::{known, string, strings};

/// How long a tool gets to say where a checkout builds.
pub const WAIT: Duration = Duration::from_secs(3);

/// One `[builds]`, read.
#[derive(Clone, Debug)]
pub struct Tool {
    pub name: String,
    pub when: Vec<String>,
    ask: Vec<String>,
    target: String,
    build: Option<String>,
    home_env: Option<String>,
    home_else: Option<String>,
    setting: Option<Setting>,
    remove: Option<Remove>,
    keep: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
struct Setting {
    env: Option<String>,
    files: Vec<String>,
    home_files: Vec<String>,
    key: String,
    per_checkout: Vec<String>,
}

#[derive(Clone, Debug)]
struct Remove {
    under: PathBuf,
    depth: usize,
}

/// `p` as plain names, one or more (`build`, `a/b`): no root, `.`, `..` or empty part.
fn plain(p: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = p.split('/').collect();
    let ok = !p.is_empty() && parts.iter().all(|c| !c.is_empty() && *c != "." && *c != "..");
    ok.then(|| PathBuf::from(p))
}

/// A pack's `[builds]`.
pub fn tool_of(v: &toml::Value) -> Result<Tool, String> {
    let t = v.as_table().ok_or("isn't a table")?;
    known(t, &["name", "when", "ask", "target", "build", "home", "setting", "remove", "keep"], "it")?;
    let need = |k: &str| string(t, k)?.ok_or(format!("has no `{k}`"));
    let list = |k: &str| strings(t.get(k), k).map(Option::unwrap_or_default);
    let when = list("when")?;
    let ask = list("ask")?;
    if when.is_empty() || ask.is_empty() {
        return Err("`when` and `ask` are lists of one or more".into());
    }
    let sub = |k: &str, keys: &[&str]| -> Result<Option<&Table>, String> {
        let Some(v) = t.get(k) else { return Ok(None) };
        let v = v.as_table().ok_or(format!("`{k}` isn't a table"))?;
        known(v, keys, &format!("`{k}`"))?;
        Ok(Some(v))
    };
    let (home_env, home_else) = match sub("home", &["env", "else"])? {
        None => (None, None),
        Some(h) => {
            let e = string(h, "else")?;
            if e.as_deref().is_some_and(|e| !e.starts_with("~/") && !e.starts_with('/')) {
                return Err("`home.else` is `~/…` or a full path".into());
            }
            (string(h, "env")?, e)
        }
    };
    let setting = match sub("setting", &["env", "files", "home-files", "key", "per-checkout"])? {
        None => None,
        Some(s) => Some(Setting {
            env: string(s, "env")?,
            files: strings(s.get("files"), "setting.files")?.unwrap_or_default(),
            home_files: strings(s.get("home-files"), "setting.home-files")?.unwrap_or_default(),
            key: string(s, "key")?.filter(|k| !k.is_empty()).ok_or("`setting` has no `key`")?,
            per_checkout: strings(s.get("per-checkout"), "setting.per-checkout")?.unwrap_or_default(),
        }),
    };
    let remove = match sub("remove", &["under", "depth"])? {
        None => None,
        Some(r) => {
            let under = string(r, "under")?.ok_or("`remove` has no `under`")?;
            let under =
                plain(&under).ok_or(format!("`remove.under` {under:?} isn't a folder's name in the home"))?;
            let depth = r.get("depth").and_then(toml::Value::as_integer).ok_or("`remove` has no `depth`")?;
            if !(1..=4).contains(&depth) {
                return Err(format!("`remove.depth` is 1 to 4, not {depth}"));
            }
            if home_env.is_none() && home_else.is_none() {
                return Err("`remove` needs `home`".into());
            }
            Some(Remove { under, depth: depth as usize })
        }
    };
    let keep = list("keep")?
        .iter()
        .map(|k| plain(k).ok_or(format!("`keep` {k:?} isn't a folder's name in the home")))
        .collect::<Result<_, _>>()?;
    Ok(Tool {
        name: need("name")?,
        when,
        ask,
        target: need("target")?,
        build: string(t, "build")?,
        home_env,
        home_else,
        setting,
        remove,
        keep,
    })
}

impl Tool {
    /// `Cargo` → `cargo`: the tool named inside a sentence.
    fn word(&self) -> String {
        self.name.to_lowercase()
    }

    /// Whether a session's own build folder may go with it.
    pub fn removes(&self) -> bool {
        self.remove.is_some()
    }

    /// Whether a build-folder setting puts each checkout's builds apart (`per-checkout` in it).
    pub fn per_checkout(&self, setting: &str) -> bool {
        self.setting.as_ref().is_some_and(|s| s.per_checkout.iter().any(|m| setting.contains(m.as_str())))
    }

    /// Why the checkout at `root` chose its build folder itself — one of `setting.files` there sets `key`
    /// (parsed, not searched), or can't be read. Its Claude can write those files: a folder it chose is never
    /// removed.
    fn chosen_by(&self, root: &Path) -> Option<String> {
        let s = self.setting.as_ref()?;
        for name in &s.files {
            let path = root.join(name);
            if std::fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let Some(table) =
                std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str::<Table>(&t).ok())
            else {
                return Some(format!("its {name} can't be read"));
            };
            if dotted(&toml::Value::Table(table), &s.key).is_some() {
                let setting = s.key.rsplit('.').next().unwrap_or(&s.key);
                return Some(format!("its own {name} sets {setting}"));
            }
        }
        None
    }

    /// One line for the language table's snapshot.
    #[cfg(test)]
    pub(crate) fn summary(&self) -> String {
        let mut s = format!(
            "{}: when {} · ask {} · {} {}",
            self.name,
            self.when.join(" "),
            self.ask.join(" "),
            self.target,
            self.build.as_deref().unwrap_or("-")
        );
        s += &format!(
            " · home {} else {}",
            self.home_env.as_deref().unwrap_or("-"),
            self.home_else.as_deref().unwrap_or("-")
        );
        if let Some(st) = &self.setting {
            s += &format!(
                " · setting {} in {} / home {} ({}) per checkout {}",
                st.key,
                st.files.join(" "),
                st.home_files.join(" "),
                st.env.as_deref().unwrap_or("-"),
                st.per_checkout.join(" ")
            );
        }
        if let Some(r) = &self.remove {
            s += &format!(" · remove {} depth {}", r.under.display(), r.depth);
        }
        let keep: Vec<String> = self.keep.iter().map(|k| k.display().to_string()).collect();
        s + &format!(" · keep {}", keep.join(" "))
    }
}

/// `v`'s value at a dotted key (`build.build-dir`).
fn dotted<'a>(v: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    key.split('.').try_fold(v, |v, k| v.get(k))
}

/// What a session's end did with its checkout's build folder.
#[derive(Debug, PartialEq)]
pub enum BuildDir {
    /// Nothing to do: not asked, not a tool's project, never built, or inside the checkout (gone with it).
    Nothing,
    /// Removed — what it took.
    Freed((u64, bool)),
    /// Left where it is: why, said whole (`its build dir … stays: …`).
    Left(String),
    /// Removing it failed partway — why.
    Failed(String),
}

/// A build folder to remove: the tool's home (real path), the removal root's names below it (`under`) and the
/// folders below that (`depth` of them).
#[derive(Debug, PartialEq)]
pub struct Doomed {
    home: PathBuf,
    under: PathBuf,
    rel: PathBuf,
}

impl Doomed {
    /// The removal root (`{home}/{under}`).
    pub fn root(&self) -> PathBuf {
        self.home.join(&self.under)
    }

    /// The build folder it is.
    pub fn path(&self) -> PathBuf {
        self.root().join(&self.rel)
    }

    /// Still as it was checked — the home its own real path, every folder from it down to the build folder a
    /// folder, not a link (rules 8 and 10 again) — its path then. Asked right before removing (a link put there
    /// since would be followed).
    fn still(&self) -> Result<PathBuf, String> {
        if std::fs::canonicalize(&self.home).ok().as_deref() != Some(self.home.as_path()) {
            return Err(format!("{} isn't where it was", crate::allow::tilde(&self.home)));
        }
        no_links(&self.home, &self.under.join(&self.rel))
    }
}

/// `base` joined with each of `rel`'s parts in turn, each a folder and not a link (`symlink_metadata`: a link
/// is a link here, never what it points to) — the whole path then.
fn no_links(base: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut p = base.to_path_buf();
    for c in rel.components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a link, not a folder", crate::allow::tilde(&p)));
            }
            Ok(_) => return Err(format!("{} isn't a folder", crate::allow::tilde(&p))),
            Err(e) => return Err(format!("{}: {e}", crate::allow::tilde(&p))),
        }
    }
    Ok(p)
}

/// One inside the other, either way (removing one takes some of the other) — by real paths where they're there.
fn overlap(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (real(a), real(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// The built-in packs' `[builds]`, and what's asked of them: the user's home, tarae's own folders (never
/// removed), the environment; in tests a stand-in program for every `ask` and a home for every tool — never the
/// real tools nor their real homes.
#[derive(Clone, Debug)]
pub struct Builds {
    /// (pack, its `when` as far as it reads, the table read).
    pub entries: Vec<(String, Vec<String>, Result<Tool, String>)>,
    pub program: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub user_home: Option<PathBuf>,
    /// tarae's own state, data and config folders.
    pub ours: Vec<PathBuf>,
    pub env: fn(&str) -> Option<String>,
    pub wait: Duration,
}

impl Builds {
    /// From these packs' tables ((pack, its table read)) — the built-in ones (`real`).
    pub fn of(packs: &[(String, Result<Table, String>)]) -> Vec<(String, Vec<String>, Result<Tool, String>)> {
        let mut out = Vec::new();
        for (pack, t) in packs {
            let Ok(t) = t else { continue };
            let Some(v) = t.get("builds") else { continue };
            let when =
                v.get("when").and_then(|w| strings(Some(w), "when").ok().flatten()).unwrap_or_default();
            out.push((pack.clone(), when, tool_of(v).map_err(|e| format!("the {pack} pack: [builds]: {e}"))));
        }
        out
    }

    /// The built-in packs', the real home, the real tools.
    pub fn real() -> Builds {
        let entries = Builds::of(crate::packs::builtin_manifests());
        for (_, _, t) in &entries {
            if let Err(e) = t {
                crate::log::line("tarae", "warning", e);
            }
        }
        let ours = [
            crate::config::state_dir(),
            crate::runtime::data_dir(),
            crate::config::config_path().and_then(|p| p.parent().map(Path::to_path_buf)),
        ];
        Builds {
            entries,
            program: None,
            home: None,
            user_home: std::env::var_os("HOME").map(PathBuf::from).filter(|h| !h.as_os_str().is_empty()),
            ours: ours.into_iter().flatten().collect(),
            env: |k| std::env::var(k).ok().filter(|v| !v.is_empty()),
            wait: WAIT,
        }
    }

    /// The built-in packs' with a stand-in for every `ask` (`program`), every tool's home and the user's
    /// home — no variables read, nothing of tarae's own.
    #[cfg(test)]
    pub(crate) fn stand_in(program: &Path, home: &Path, user_home: &Path) -> Builds {
        Builds {
            entries: Builds::of(crate::packs::builtin_manifests()),
            program: Some(program.to_path_buf()),
            home: Some(home.to_path_buf()),
            user_home: Some(user_home.to_path_buf()),
            ours: Vec::new(),
            env: |_| None,
            wait: WAIT,
        }
    }

    /// `pack`'s tool (tests).
    #[cfg(test)]
    pub(crate) fn tool(&self, pack: &str) -> &Tool {
        let (_, _, t) = self.entries.iter().find(|(p, ..)| p == pack).expect("that pack's [builds]");
        t.as_ref().expect("it reads")
    }

    /// The tool whose `when` the checkout at `root` has (the first) — Err: its pack's `[builds]` doesn't read.
    pub fn tool_for(&self, root: &Path) -> Option<Result<&Tool, String>> {
        let (_, _, t) =
            self.entries.iter().find(|(_, when, _)| when.iter().any(|f| root.join(f).is_file()))?;
        Some(t.as_ref().map_err(|e| format!("{e} — no build dir removed")))
    }

    /// The tool's home as said: the stand-in, else its variable, else `~/…` from the user's home.
    pub fn home_of(&self, tool: &Tool) -> Option<PathBuf> {
        if let Some(h) = &self.home {
            return Some(h.clone());
        }
        if let Some(v) = tool.home_env.as_deref().and_then(self.env) {
            return Some(PathBuf::from(v));
        }
        let e = tool.home_else.as_deref()?;
        match e.strip_prefix("~/") {
            Some(rest) => Some(self.user_home.as_ref()?.join(rest)),
            None => Some(PathBuf::from(e)),
        }
    }

    /// Where the tool says the checkout at `dir` builds — (target, build folder) as printed, `..` and links and
    /// all — None when it doesn't say within `wait` (or says it failed).
    pub fn ask(&self, tool: &Tool, dir: &Path) -> Option<(PathBuf, PathBuf)> {
        use std::io::Read as _;
        use std::process::{Command, Stdio};
        let program = self.program.clone().unwrap_or_else(|| PathBuf::from(&tool.ask[0]));
        let mut child = Command::new(program)
            .args(&tool.ask[1..])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = out.read_to_end(&mut s);
            s
        });
        let end = Instant::now() + self.wait;
        loop {
            match child.try_wait() {
                Ok(Some(s)) if s.success() => break,
                Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let v: Value = serde_json::from_slice(&reader.join().ok()?).ok()?;
        let target = PathBuf::from(v[tool.target.as_str()].as_str()?);
        let build = tool.build.as_ref().and_then(|b| v[b.as_str()].as_str());
        Some((target.clone(), build.map_or(target, PathBuf::from)))
    }

    /// The build-folder setting as it stands for `top`, unresolved (its per-checkout markers as written) — the
    /// variable, else the nearest of `files` from `top` up, else the home's `home-files`. None when nothing sets
    /// it.
    pub fn setting(&self, tool: &Tool, top: &Path) -> Option<String> {
        let s = tool.setting.as_ref()?;
        if let Some(v) = s.env.as_deref().and_then(self.env) {
            return Some(v);
        }
        let read = |files: &[String], dir: &Path| {
            files.iter().find_map(|f| {
                let text = std::fs::read_to_string(dir.join(f)).ok()?;
                let v: toml::Value = toml::from_str(&text).ok()?;
                dotted(&v, &s.key)?.as_str().map(str::to_string)
            })
        };
        top.ancestors().find_map(|d| read(&s.files, d)).or_else(|| read(&s.home_files, &self.home_of(tool)?))
    }

    /// A session's end: whether the checkout at `root`'s own build folder goes once the checkout is gone —
    /// rule 1 (its tool is a built-in pack's `[builds]` that reads: `entries`), rule 2 (the checkout has its
    /// `when`), then `fate`'s. `git_dir` and `checkouts` (every checkout of the repository, `root` among them)
    /// are never removed; the checkouts other than `root` that are the tool's projects are asked where they
    /// build. `Ok` = what to remove once the checkout is gone (`remove_build` checks it again).
    pub fn decide(&self, root: &Path, git_dir: &Path, checkouts: &[PathBuf]) -> Result<Doomed, BuildDir> {
        let tool = match self.tool_for(root) {
            None => return Err(BuildDir::Nothing),
            Some(Err(why)) => return Err(BuildDir::Left(why)),
            Some(Ok(t)) if !t.removes() => return Err(BuildDir::Nothing),
            Some(Ok(t)) => t,
        };
        let mut floor = vec![git_dir.to_path_buf()];
        floor.extend(checkouts.iter().cloned());
        let others = || {
            checkouts
                .iter()
                .filter(|p| p.as_path() != root && tool.when.iter().any(|f| p.join(f).is_file()))
                .map(|p| (p.clone(), self.ask(tool, p)))
                .collect()
        };
        self.fate(tool, root, self.ask(tool, root), &floor, others)
    }

    /// Whether the checkout at `root`'s own build folder goes once the checkout is gone — only when every rule
    /// holds (any that doesn't: it stays, `Left` says why; `Nothing`: there's nothing to remove):
    ///
    /// 3. `ours`, what the tool said for `root`, is there (target, build folder `B`);
    /// 4. `B` is absolute, with no `.` or `..`;
    /// 5. `B` is outside `root` (inside, it goes with the checkout) and exists;
    /// 6. the checkout didn't choose it (`Tool::chosen_by`);
    /// 7. the tool's home `H` (real) is absolute, not `/`, not the user's home nor above it, not in `root`;
    /// 8. `U` = `H/{under}` is a folder, with no link from `H` down to it;
    /// 9. `B` is exactly `depth` folders below `U` (as spelled, or below `U`'s real path);
    /// 10. no link from `U` down to `B` (and again right before removing — `Doomed::still`);
    /// 11. `B` overlaps no floor place: the user's home or above, its keys (`allow::SECRETS`) and what runs later
    ///     (`allow::NEVER_WRITE`), the tool's `keep`, tarae's own folders, `floor` (the repository's git folder
    ///     and its checkouts);
    /// 12. `B` doesn't overlap the target;
    /// 13. every other checkout (`others`, asked only now: each one and what the tool said there) answered, and
    ///     builds neither in nor around `B`.
    ///
    /// (Rules 1 and 2 are the caller's: a tool read from a built-in pack, one whose `when` the checkout has.)
    pub fn fate(
        &self,
        tool: &Tool,
        root: &Path,
        ours: Option<(PathBuf, PathBuf)>,
        floor: &[PathBuf],
        others: impl FnOnce() -> Vec<(PathBuf, Option<(PathBuf, PathBuf)>)>,
    ) -> Result<Doomed, BuildDir> {
        let tilde = crate::allow::tilde;
        let Some(remove) = &tool.remove else { return Err(BuildDir::Nothing) };
        // 3
        let Some((target, build)) = ours else {
            return Err(BuildDir::Left(format!(
                "{} didn't say where it built: no build dir removed",
                tool.word()
            )));
        };
        let stays =
            |why: String| Err(BuildDir::Left(format!("its build dir {} stays: {why}", tilde(&build))));
        // 4
        if !build.is_absolute()
            || build.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        {
            return stays("its path has `.` or `..` in it".into());
        }
        // 5
        if build.starts_with(root) || std::fs::symlink_metadata(&build).is_err() {
            return Err(BuildDir::Nothing);
        }
        // 6
        if let Some(why) = tool.chosen_by(root) {
            return stays(why);
        }
        // 7
        let Some(home) = self.home_of(tool) else {
            return stays(format!("{} has no home here", tool.word()));
        };
        let Ok(real_home) = std::fs::canonicalize(&home) else {
            return stays(format!("there's no {}", tilde(&home)));
        };
        let user_home =
            self.user_home.as_ref().map(|h| std::fs::canonicalize(h).unwrap_or_else(|_| h.clone()));
        if !real_home.is_absolute()
            || real_home.parent().is_none()
            || user_home.as_ref().is_some_and(|u| u.starts_with(&real_home))
            || real_home.starts_with(root)
        {
            return stays(format!(
                "{}'s home {} isn't one a build dir is removed from",
                tool.word(),
                tilde(&home)
            ));
        }
        // 8
        let builds = match no_links(&real_home, &remove.under) {
            Ok(u) => u,
            Err(why) => return stays(why),
        };
        // 9 (`{home}` as the tool spelled it, or its real path)
        let Some(rel) =
            build.strip_prefix(home.join(&remove.under)).or_else(|_| build.strip_prefix(&builds)).ok()
        else {
            return stays(format!("it isn't in {}", tilde(&builds)));
        };
        if rel.components().count() != remove.depth {
            let mut shape = vec!["xx"; remove.depth - 1];
            shape.push("yyyy");
            return stays(format!(
                "it isn't a checkout's own folder in {} ({})",
                tilde(&builds),
                shape.join("/")
            ));
        }
        // 10 (below `U`: `U` itself is 8's — and both again right before removing, `Doomed::still`)
        let real = match no_links(&builds, rel) {
            Ok(p) => p,
            Err(why) => return stays(why),
        };
        let doomed = Doomed { home: real_home.clone(), under: remove.under.clone(), rel: rel.to_path_buf() };
        // 11
        if user_home.as_ref().is_some_and(|u| u.starts_with(&real)) {
            return stays("your home folder is in it".into());
        }
        let mut places: Vec<PathBuf> = Vec::new();
        if let Some(u) = &user_home {
            places.extend(
                crate::allow::SECRETS.iter().chain(crate::allow::NEVER_WRITE.iter()).map(|s| u.join(s)),
            );
        }
        places.extend(tool.keep.iter().map(|k| real_home.join(k)));
        places.extend(self.ours.iter().cloned());
        places.extend(floor.iter().cloned());
        if let Some(p) = places.iter().find(|p| overlap(&real, p)) {
            return stays(format!("{} is never removed", tilde(p)));
        }
        // 12
        if overlap(&real, &target) {
            return stays(format!("{}'s target dir is there", tool.word()));
        }
        // 13
        for (other, said) in others() {
            match said {
                None => {
                    return stays(format!("{} didn't say where {} builds", tool.word(), tilde(&other)));
                }
                Some((t, b)) if overlap(&real, &b) || overlap(&real, &t) => {
                    return stays(format!("{} builds there too", tilde(&other)));
                }
                Some(_) => {}
            }
        }
        Ok(doomed)
    }
}

/// The doomed build folder removed — checked again first (`Doomed::still`); a link inside it is removed, never
/// followed (`remove_dir_all`, `disk_size`).
pub fn remove_build(d: &Doomed) -> BuildDir {
    let path = match d.still() {
        Ok(p) => p,
        Err(why) => {
            let at = crate::allow::tilde(&d.path());
            return BuildDir::Left(format!("its build dir {at} stays: {why}"));
        }
    };
    let size = crate::worktree::disk_size(&path, Instant::now() + crate::worktree::MEASURE_FOR);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => BuildDir::Freed(size),
        Err(why) => BuildDir::Failed(format!("{}: {why}", crate::allow::tilde(&path))),
    }
}

/// `ask`'s answer by real paths (as far as they're there) — for comparing and saying, never for removing.
pub fn real_dirs((target, build): (PathBuf, PathBuf)) -> (PathBuf, PathBuf) {
    let real = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    (real(target), real(build))
}

/// One test per rule of `Builds::decide`/`fate`: each sets up a build folder only that rule stands between and
/// the removal, and checks it stays (its `marker`) and why it's said. All in the test's own temp folder: a
/// stand-in for cargo (a script printing what `.fake-build` holds), a cargo home and a user's home of its own —
/// never the real cargo, `~/.cargo` or `$HOME`.
#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    struct Place {
        dir: PathBuf,
        /// The tool's home · its `build/` (the removal root) · the user's home · the ended checkout.
        home: PathBuf,
        u: PathBuf,
        user: PathBuf,
        root: PathBuf,
        b: Builds,
    }

    /// A cargo project ending at `dir/wt/x`, the rust pack's `[builds]` with stand-ins.
    fn place(name: &str) -> Place {
        let dir = std::env::temp_dir().join(format!("tarae-builds-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let (home, user, root) = (dir.join("cargo-home"), dir.join("user-home"), dir.join("wt/x"));
        for d in [home.join("build"), user.clone(), root.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let program = dir.join("fake-cargo");
        let script = "#!/bin/sh\n[ -f .fake-sleep ] && sleep \"$(cat .fake-sleep)\"\n\
                      printf '{\"target_directory\":\"%s/target\",\"build_directory\":\"%s\"}' \"$(pwd -P)\" \
                      \"$(cat .fake-build)\"\n[ -f .fake-exit ] && exit \"$(cat .fake-exit)\"\nexit 0\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let b = Builds::stand_in(&program, &home, &user);
        Place { u: home.join("build"), dir, home, user, root, b }
    }

    impl Place {
        /// What the stand-in prints as the checkout's build folder (as cargo prints it: `..` and all).
        fn prints(&self, at: &Path) {
            std::fs::write(self.root.join(".fake-build"), at.display().to_string()).unwrap();
        }

        /// The session's end with `b`: decided, then removed if so.
        fn end_with(&self, b: &Builds, checkouts: &[PathBuf]) -> BuildDir {
            match b.decide(&self.root, &self.dir.join("repo.git"), checkouts) {
                Ok(d) => remove_build(&d),
                Err(said) => said,
            }
        }

        fn end(&self) -> BuildDir {
            self.end_with(&self.b, std::slice::from_ref(&self.root))
        }

        /// The decision alone, before any removal (whose own check again would hide a rule missing here).
        fn decided(&self) -> BuildDir {
            match self.b.decide(&self.root, &self.dir.join("repo.git"), std::slice::from_ref(&self.root)) {
                Ok(d) => panic!("decided to remove {}", d.path().display()),
                Err(said) => said,
            }
        }
    }

    impl Drop for Place {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// `d` made, a `marker` in it — the marker's path.
    fn marked(d: &Path) -> PathBuf {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("marker"), "keep me").unwrap();
        d.join("marker")
    }

    /// It stayed, and why is said.
    #[track_caller]
    fn stays(said: BuildDir, why: &str, marker: &Path) {
        match &said {
            BuildDir::Left(w) if w.contains(why) => {}
            other => panic!("expected it left because {why:?}: {other:?}"),
        }
        assert!(marker.is_file(), "{} removed", marker.display());
    }

    /// The rust pack's `[builds]` with its text changed (`from` → `to`), read as a built-in pack.
    fn rust_changed(from: &str, to: &str) -> Vec<(String, Vec<String>, Result<Tool, String>)> {
        let text = crate::runtime::pack_builtin("rust/pack.toml").unwrap();
        assert!(text.contains(from), "{from}");
        let t = crate::packs::read_pack("rust", &text.replace(from, to), "built in");
        Builds::of(&[("rust".to_string(), t)])
    }

    /// A tool of the test's own: `[builds]` with this `remove` and `keep`.
    fn tool_with(remove: &str, keep: &str) -> Vec<(String, Vec<String>, Result<Tool, String>)> {
        let text = format!(
            "[builds]\nname = \"Cargo\"\nwhen = [\"Cargo.toml\"]\nask = [\"cargo\"]\ntarget = \"target_directory\"\n\
             build = \"build_directory\"\nhome = {{ else = \"~/.cargo\" }}\nremove = {remove}\nkeep = {keep}\n"
        );
        Builds::of(&[("test".to_string(), crate::packs::read_pack("test", &text, "built in"))])
    }

    /// The fixture is right: plainly its own build folder goes, and nothing beside it (a neighbour, the folder
    /// holding it, the home's caches); a link inside it is removed, never followed.
    #[test]
    fn its_own_build_dir_goes_and_nothing_beside_it() {
        let p = place("own");
        let own = p.u.join("ab/cdef");
        marked(&own);
        let (neighbour, above) = (marked(&p.u.join("ab/other")), marked(&p.u.join("zz")));
        let registry = marked(&p.home.join("registry/index"));
        let outside = marked(&p.dir.join("outside-in"));
        symlink(p.dir.join("outside-in"), own.join("out")).unwrap();
        p.prints(&own);
        assert!(matches!(p.end(), BuildDir::Freed(_)), "goes");
        assert!(!own.exists());
        for m in [&neighbour, &above, &registry, &outside] {
            assert!(m.is_file(), "{} stays", m.display());
        }
    }

    /// Rule 1: Only tarae's own packs say: a runtime folder's `[builds]` isn't read (said at the start), and a
    /// built-in one that doesn't read removes nothing — said.
    #[test]
    fn rule_1_only_taraes_own_packs_say_and_a_broken_one_removes_nothing() {
        let p = place("r1");
        let marker = marked(&p.u.join("ab/own"));
        p.prints(&p.u.join("ab/own"));
        let builtin = crate::runtime::pack_builtin("rust/pack.toml");
        let mine = "[builds]\nname = \"Mine\"\nwhen = [\"Cargo.toml\"]\nask = [\"x\"]\ntarget = \"t\"\n\
                    home = { else = \"/\" }\nremove = { under = \"anywhere\", depth = 1 }\n";
        let m = crate::packs::merged("rust", builtin, Some(mine.into())).unwrap().unwrap();
        let of = Builds::of(&[("rust".into(), Ok(m))]);
        assert_eq!(of[0].2.as_ref().unwrap().name, "Cargo", "the built-in one, not the runtime folder's");
        let t = crate::languages::build(crate::packs::builtin_manifests(), &[("rust".into(), mine.into())]);
        assert!(t.said.iter().any(|s| s == "your rust pack: `builds` is read from tarae's own packs only"));
        let mut b = p.b.clone();
        b.entries = rust_changed("depth = 2", "depth = 0");
        let said = p.end_with(&b, std::slice::from_ref(&p.root));
        stays(
            said,
            "the rust pack: [builds]: `remove.depth` is 1 to 4, not 0 — no build dir removed",
            &marker,
        );
    }

    /// Rule 2: A checkout that isn't the tool's project (no `when` file): nothing asked, nothing removed.
    #[test]
    fn rule_2_a_checkout_that_isnt_the_tools_project_removes_nothing() {
        let p = place("r2");
        let marker = marked(&p.u.join("ab/own"));
        p.prints(&p.u.join("ab/own"));
        std::fs::remove_file(p.root.join("Cargo.toml")).unwrap();
        assert_eq!(p.end(), BuildDir::Nothing);
        assert!(marker.is_file());
    }

    /// Rule 3: The tool has to answer — in time, and saying it did (an exit that isn't 0 with JSON is no answer).
    #[test]
    fn rule_3_a_tool_that_didnt_answer_removes_nothing() {
        let mut p = place("r3");
        let marker = marked(&p.u.join("ab/own"));
        p.prints(&p.u.join("ab/own"));
        std::fs::write(p.root.join(".fake-exit"), "1").unwrap();
        stays(p.end(), "cargo didn't say where it built: no build dir removed", &marker);
        std::fs::remove_file(p.root.join(".fake-exit")).unwrap();
        std::fs::write(p.root.join(".fake-sleep"), "2").unwrap();
        p.b.wait = Duration::from_millis(300);
        stays(p.end(), "cargo didn't say where it built", &marker);
    }

    /// Rule 4: A path with `..` in it: `build/ab/..` is two folders below `build/` as spelled — and is `build/`.
    #[test]
    fn rule_4_a_path_with_dot_dot_removes_nothing() {
        let p = place("r4");
        let (inside, beside) = (marked(&p.u.join("ab")), marked(&p.u.join("zz")));
        p.prints(&p.u.join("ab/.."));
        stays(p.end(), "its path has `.` or `..` in it", &inside);
        assert!(beside.is_file());
    }

    /// Rule 5: Inside the checkout it goes with the checkout; missing, there's nothing to do — neither is a refusal.
    #[test]
    fn rule_5_inside_the_checkout_or_missing_is_nothing_to_do() {
        let p = place("r5");
        std::fs::create_dir_all(p.root.join("target")).unwrap();
        p.prints(&p.root.join("target"));
        assert_eq!(p.end(), BuildDir::Nothing);
        p.prints(&p.u.join("ab/never"));
        assert_eq!(p.end(), BuildDir::Nothing);
    }

    /// Rule 6: A build folder the checkout chose (its own config sets it, or can't be read): its Claude can write
    /// that file — never removed.
    #[test]
    fn rule_6_one_the_checkout_chose_removes_nothing() {
        let p = place("r6");
        let marker = marked(&p.u.join("ab/own"));
        p.prints(&p.u.join("ab/own"));
        std::fs::create_dir_all(p.root.join(".cargo")).unwrap();
        std::fs::write(p.root.join(".cargo/config.toml"), "build.\"build\\u002Ddir\" = \"x\"\n").unwrap();
        stays(p.end(), "its own .cargo/config.toml sets build-dir", &marker);
        std::fs::write(p.root.join(".cargo/config"), "[build\n").unwrap();
        std::fs::remove_file(p.root.join(".cargo/config.toml")).unwrap();
        stays(p.end(), "its .cargo/config can't be read", &marker);
    }

    /// Rule 7: A tool home that is the user's home, or above it: nothing below it is a tool's own.
    #[test]
    fn rule_7_a_tool_home_thats_your_home_or_above_removes_nothing() {
        let p = place("r7");
        for (home, case) in [(p.user.clone(), "the user's home"), (p.dir.clone(), "above it")] {
            let marker = marked(&home.join("build/ab/own"));
            std::fs::write(p.root.join(".fake-build"), home.join("build/ab/own").display().to_string())
                .unwrap();
            let mut b = p.b.clone();
            b.home = Some(home.clone());
            let said = p.end_with(&b, std::slice::from_ref(&p.root));
            assert!(
                matches!(&said, BuildDir::Left(w) if w.contains("isn't one a build dir is removed from")),
                "{case}: {said:?}"
            );
            assert!(marker.is_file(), "{case}");
        }
    }

    /// Rule 8: The removal root a link (`build` → elsewhere): not followed. And a `remove.under` that isn't a
    /// folder's name in the home is a pack that doesn't read.
    #[test]
    fn rule_8_a_removal_root_thats_a_link_removes_nothing() {
        let p = place("r8");
        std::fs::remove_dir(&p.u).unwrap();
        let marker = marked(&p.dir.join("elsewhere/ab/own"));
        symlink(p.dir.join("elsewhere"), &p.u).unwrap();
        p.prints(&p.u.join("ab/own"));
        stays(p.decided(), "build is a link, not a folder", &marker);
        stays(p.end(), "build is a link, not a folder", &marker);
        for under in ["\"..\"", "\".\"", "\"\"", "\"/tmp\"", "\"a/../b\"", "\"a//b\""] {
            let read = tool_with(&format!("{{ under = {under}, depth = 2 }}"), "[]");
            let e = read[0].2.as_ref().err().unwrap_or_else(|| panic!("{under} read"));
            assert!(e.contains("isn't a folder's name in the home"), "{under}: {e}");
        }
    }

    /// Rule 9: Exactly `depth` folders below the removal root — and `depth` is 1 to 4.
    #[test]
    fn rule_9_not_exactly_depth_below_removes_nothing() {
        let p = place("r9");
        for (at, case) in [("one", "one level"), ("ab/cd/ef", "three levels")] {
            let marker = marked(&p.u.join(at));
            p.prints(&p.u.join(at));
            let said = p.end();
            assert!(
                matches!(&said, BuildDir::Left(w) if w.contains("isn't a checkout's own folder in") && w.contains("(xx/yyyy)")),
                "{case}: {said:?}"
            );
            assert!(marker.is_file(), "{case}");
        }
        for depth in [0, 5, 9] {
            let read = tool_with(&format!("{{ under = \"build\", depth = {depth} }}"), "[]");
            assert!(read[0].2.as_ref().is_err_and(|e| e.contains("is 1 to 4")), "depth {depth}");
        }
    }

    /// Rule 10: A link below the removal root — in the middle, at the end: never followed.
    #[test]
    fn rule_10_a_link_below_the_removal_root_removes_nothing() {
        let p = place("r10");
        let middle = marked(&p.dir.join("outside-mid/victim"));
        symlink(p.dir.join("outside-mid"), p.u.join("ln")).unwrap();
        p.prints(&p.u.join("ln/victim"));
        stays(p.decided(), "ln is a link, not a folder", &middle);
        stays(p.end(), "ln is a link, not a folder", &middle);
        let end = marked(&p.dir.join("outside-end"));
        std::fs::create_dir_all(p.u.join("ab")).unwrap();
        symlink(p.dir.join("outside-end"), p.u.join("ab/ln")).unwrap();
        p.prints(&p.u.join("ab/ln"));
        stays(p.decided(), "ab/ln is a link, not a folder", &end);
        stays(p.end(), "ab/ln is a link, not a folder", &end);
    }

    /// Rule 11: The floor, whatever a pack says: a checkout of the repository, its git folder, tarae's own folders,
    /// the tool's `keep`, what runs later (`allow::NEVER_WRITE`) and keys (`allow::SECRETS`) in the user's home.
    #[test]
    fn rule_11_the_floor_is_never_removed() {
        let p = place("r11");
        let own = p.u.join("ab/own");
        let marker = marked(&own);
        p.prints(&own);
        // a checkout there · the git folder around it · tarae's own folder in it
        let checkouts = [p.root.clone(), own.clone()];
        stays(p.end_with(&p.b, &checkouts), "is never removed", &marker);
        let said = p.b.decide(&p.root, &p.u.join("ab"), std::slice::from_ref(&p.root));
        stays(said.map(|_| BuildDir::Nothing).unwrap_or_else(|e| e), "is never removed", &marker);
        let mut b = p.b.clone();
        b.ours = vec![own.join("state")];
        stays(p.end_with(&b, std::slice::from_ref(&p.root)), "is never removed", &marker);
        // a pack whose removal root is the home's crates: its `keep` (a home outside the user's)…
        let crates = p.home.join("registry/index");
        let marker = marked(&crates);
        p.prints(&crates);
        let mut b = p.b.clone();
        b.entries = tool_with("{ under = \"registry\", depth = 1 }", "[\"registry\"]");
        stays(p.end_with(&b, std::slice::from_ref(&p.root)), "cargo-home/registry is never removed", &marker);
        // …and with no `keep`, in the user's home: `NEVER_WRITE`
        let crates = p.user.join(".cargo/registry/index");
        let marker = marked(&crates);
        p.prints(&crates);
        b.home = Some(p.user.join(".cargo"));
        b.entries = tool_with("{ under = \"registry\", depth = 1 }", "[]");
        stays(p.end_with(&b, std::slice::from_ref(&p.root)), ".cargo/registry is never removed", &marker);
        // keys: a removal root in ~/.ssh
        let keys = p.user.join(".ssh/keys/id");
        let marker = marked(&keys);
        p.prints(&keys);
        b.home = Some(p.user.join(".ssh"));
        b.entries = tool_with("{ under = \"keys\", depth = 1 }", "[]");
        stays(p.end_with(&b, std::slice::from_ref(&p.root)), ".ssh is never removed", &marker);
    }

    /// Rule 12: The target folder in it, or it in the target.
    #[test]
    fn rule_12_the_target_there_removes_nothing() {
        let p = place("r12");
        let own = p.u.join("ab/own");
        let marker = marked(&own);
        let tool = p.b.tool("rust");
        for target in [own.clone(), own.join("target")] {
            let said = p.b.fate(tool, &p.root, Some((target.clone(), own.clone())), &[], Vec::new);
            stays(
                said.map(|_| BuildDir::Nothing).unwrap_or_else(|e| e),
                "cargo's target dir is there",
                &marker,
            );
        }
    }

    /// Rule 13: Another checkout of the repository builds there (in it, or around it) — or didn't say where it builds.
    #[test]
    fn rule_13_another_checkout_building_there_removes_nothing() {
        let p = place("r13");
        let own = p.u.join("ab/own");
        let marker = marked(&own);
        p.prints(&own);
        let main = p.dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(main.join("Cargo.toml"), "[package]\n").unwrap();
        let checkouts = [p.root.clone(), main.clone()];
        for (b, case) in
            [(own.clone(), "the same"), (own.join("sub"), "in it"), (p.u.join("ab"), "around it")]
        {
            std::fs::write(main.join(".fake-build"), b.display().to_string()).unwrap();
            let said = p.end_with(&p.b, &checkouts);
            assert!(
                matches!(&said, BuildDir::Left(w) if w.ends_with("builds there too")),
                "{case}: {said:?}"
            );
            assert!(marker.is_file(), "{case}");
        }
        std::fs::write(main.join(".fake-exit"), "1").unwrap();
        stays(p.end_with(&p.b, &checkouts), "didn't say where", &marker);
    }
}
