//! Claude's own folder on Linux (V1 Linux, decision (a), 2026-10-15): every claude tarae starts there — the chat,
//! the reviewer, an ask, a commit draft — gets `CLAUDE_CONFIG_DIR` = `<tarae's state>/claude`, one per user, and
//! loads no user settings (`--setting-sources` without `user`). macOS is unchanged (its login is a keychain item,
//! `~/.claude` is claude's as it is — `sandbox::claude`).
//!
//! Why: Landlock only grants, and a right on a folder holds for everything below it. claude writes `.claude.json`
//! by a temp file renamed over it and a lock folder beside it, so whatever folder holds that file becomes claude's
//! to replace whole — in home, all of home. With the variable, `.claude.json`, its lock and backups, the login, the
//! conversations all live in this folder (measured, `target/sb/v1-linux-proposal.md` run B); outside it only
//! `~/.local/state/claude/locks/` and the temp folders. So your `~/.claude` and `~/.claude.json` can be closed to
//! tarae's claude whole — and your settings, `CLAUDE.md`, hooks, MCP servers and memory stop applying in it. Its
//! login is its own: `:claude-login`, once. Conversations are apart: one started before the switch continues fresh
//! (`chat.rs`, "no longer has this conversation").
//!
//! Where: in tarae's state folder (`config::state_dir`), the one exception to "tarae's state isn't claude's" (the
//! log is the other, read only). Safe, because: runs never read tarae's state (`sandbox::kept_reads`), so the login
//! here is kept from Claude's commands with no rule of its own (a folder elsewhere would need one, or they'd read
//! it); tarae reads nothing in it — only claude does — and what tarae keeps in its state (answers, sessions, the
//! conversations' rows, the UI socket) is beside it, never below, and a grant on a folder holds below it only;
//! nothing but tarae writes where it is — runs don't write tarae's state, claude writes only below it — and tarae
//! makes it through no link anyway (`Place::made`).

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::sandbox::Os;

/// Claude Code's variable for its folder.
pub const VAR: &str = "CLAUDE_CONFIG_DIR";

/// Its name in tarae's state folder.
const FOLDER: &str = "claude";

/// Claude Code's words when it has no login (its errors reference, under `-p`: "Not logged in · Please run /login",
/// "Failed to authenticate…", "Please run /login · API Error: 401…").
pub const NOT_LOGGED_IN: [&str; 3] = ["Not logged in", "Please run /login", "Failed to authenticate"];

/// Claude's folders on Linux, for one home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// tarae's state folder — `config::state_dir`'s, from the variables given.
    pub state: PathBuf,
    /// Where claude keeps its version locks whatever `CLAUDE_CONFIG_DIR` says: `~/.local/state/claude` (measured,
    /// runs A and B), and `$XDG_STATE_HOME/claude` where that's set elsewhere (not measured — claude may follow it).
    pub locks: Vec<PathBuf>,
}

impl Place {
    /// On `os`, from the variables as `var` gives them (set and not empty): Ok(None) on macOS; Err with no absolute
    /// state folder — claude doesn't start then (not with your `~/.claude` in its place).
    pub fn of(os: Os, var: &dyn Fn(&str) -> Option<String>) -> Result<Option<Place>, String> {
        if os == Os::Mac {
            return Ok(None);
        }
        let var = |n: &str| var(n).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME").filter(|h| h.is_absolute());
        let local = home.as_ref().map(|h| h.join(".local/state"));
        // (as `config::state_dir`: `$XDG_STATE_HOME`, else `~/.local/state`)
        let base = var("XDG_STATE_HOME").or(local.clone());
        let base = base
            .filter(|b| b.is_absolute())
            .ok_or("claude's folder: no absolute XDG_STATE_HOME or HOME to keep it in")?;
        let mut locks: Vec<PathBuf> =
            local.into_iter().chain([base.clone()]).map(|s| s.join(FOLDER)).collect();
        locks.dedup();
        Ok(Some(Place { state: base.join("tarae"), locks }))
    }

    /// `CLAUDE_CONFIG_DIR`.
    pub fn dir(&self) -> PathBuf {
        self.state.join(FOLDER)
    }

    /// Its folders made where they aren't, 0700, through no link: tarae's state folder as tarae makes its own (its
    /// parents as they come — yours), then `claude` in it made and opened following no link (`O_NOFOLLOW`) — a link
    /// there is never gone through nor removed: Err, and claude doesn't start. The same for each lock folder.
    pub fn made(&self) -> Result<(), String> {
        made_in(&self.state, FOLDER, true)?;
        for l in &self.locks {
            let (Some(parent), Some(name)) = (l.parent(), l.file_name()) else { continue };
            made_in(parent, &name.to_string_lossy(), false)?;
        }
        Ok(())
    }
}

/// Folder `name` in `parent` (made where it isn't, 0700) made where it isn't and opened through no link; `own`:
/// tarae's — 0700 whoever made it.
fn made_in(parent: &Path, name: &str, own: bool) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::DirBuilderExt;
    let at = parent.join(name);
    let said = |e: std::io::Error| format!("claude's folder {}: {e}", at.display());
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(said)?;
    let dir = std::fs::File::open(parent).map_err(said)?;
    let c = crate::sandbox::c_name(std::ffi::OsStr::new(name))?;
    // SAFETY: a folder descriptor of ours and a name, live for the call (one there already: EEXIST).
    unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) };
    let fd = crate::sandbox::open_dir_at(dir.as_raw_fd(), &c)
        .map_err(|e| format!("claude's folder {}: {e} — a link or no folder there", at.display()))?;
    // SAFETY: a folder descriptor of ours, live for the call.
    if own && unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } != 0 {
        return Err(said(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// The variables a claude gets: `given`'s (`Editor::claude_env`, a test's stand-in), else the process's — never the
/// process's in tests (their home is yours): a home of the test binary's own in the temp folder.
pub fn var_in(given: Option<&crate::llm::Env>) -> impl Fn(&str) -> Option<String> + '_ {
    move |n: &str| match given.and_then(|g| g.get(n)) {
        Some(v) => Some(v.clone()),
        None if cfg!(test) => (n == "HOME").then(|| {
            let home = std::env::temp_dir().join(format!("tarae-test-home-{}", std::process::id()));
            home.to_string_lossy().into_owned()
        }),
        None => std::env::var(n).ok(),
    }
}

/// `--setting-sources` as claude gets it from tarae where its folder is tarae's (`place`): without `user` — claude's
/// user settings would be this folder, which claude writes itself (a `settings.json` or `CLAUDE.md` a bug left
/// there would be obeyed at the next start), and yours are in `~/.claude`, closed to it. The rest as it is.
pub fn sources(args: &[String], place: Option<&Place>) -> Vec<String> {
    if place.is_none() {
        return args.to_vec();
    }
    let off = |v: &str| v.split(',').filter(|s| s.trim() != "user").collect::<Vec<_>>().join(",");
    let mut out = Vec::with_capacity(args.len());
    let mut next = false;
    for a in args {
        match a.strip_prefix("--setting-sources=") {
            Some(v) => out.push(format!("--setting-sources={}", off(v))),
            None if next => out.push(off(a)),
            None => out.push(a.clone()),
        }
        next = a == "--setting-sources";
    }
    out
}

/// Each folder as a worker made it (`make`), by `CLAUDE_CONFIG_DIR`: Ok kept for as long as tarae runs, Err until
/// a spawn has said it (then made again by the next worker step). The spawn only reads it: no disk on the main
/// thread.
static MADE: std::sync::Mutex<std::collections::BTreeMap<PathBuf, Result<(), String>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn made_list() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<PathBuf, Result<(), String>>> {
    MADE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Claude's folders for `given` made (`Place::made`) and the outcome kept — a worker's: a few calls on the disk.
pub fn make(os: Os, given: Option<&crate::llm::Env>) -> Result<(), String> {
    match Place::of(os, &var_in(given)) {
        Ok(Some(p)) => made_kept(&p),
        _ => Ok(()),
    }
}

fn made_kept(p: &Place) -> Result<(), String> {
    let made = p.made();
    made_list().insert(p.dir(), made.clone());
    made
}

/// Whether a claude for `given` can start without waiting: its folder made (or its failure kept, for the spawn to
/// say) — macOS, or no place to make it in (the spawn says so), nothing to wait for. A lookup.
pub fn ready(os: Os, given: Option<&crate::llm::Env>) -> bool {
    match Place::of(os, &var_in(given)) {
        Ok(Some(p)) => test_home_made(&p) || made_list().contains_key(&p.dir()),
        _ => true,
    }
}

/// In tests the test binary's own home (`var_in`'s) is made where it's first asked: they check at once what
/// follows a spawn. Elsewhere (a test's given home, tarae) false.
fn test_home_made(p: &Place) -> bool {
    let home = std::env::temp_dir().join(format!("tarae-test-home-{}", std::process::id()));
    if !cfg!(test) || !p.state.starts_with(&home) {
        return false;
    }
    if !made_list().contains_key(&p.dir()) {
        let _ = made_kept(p);
    }
    true
}

/// When claude's folder isn't made yet (`ready`): made on a worker, then `then` — the claude that waited its turn
/// (its error said). True: `then` waits; false: ready, go on.
pub fn after_made(
    ed: &mut Editor,
    then: impl FnOnce(&mut Editor) -> Result<(), String> + Send + 'static,
) -> bool {
    if ready(Os::HOST, ed.claude_env.as_ref()) {
        return false;
    }
    let given = ed.claude_env.clone();
    ed.events.jobs().spawn(move || {
        // (a failure is the spawn's to say, when `then` starts it)
        let _ = make(Os::HOST, given.as_ref());
        move |ed: &mut Editor| {
            if let Err(e) = then(ed) {
                ed.set_error(e);
            }
        }
    });
    true
}

/// What a claude tarae starts on `os` gets: `args` with the user source off, and `CLAUDE_CONFIG_DIR` — as a worker
/// made it before (`make`; the callers wait for it: `ready`), only read here. Not made: Err, and nothing is made
/// here; made but failed: its Err, said once — the next start makes it again. macOS: `args` as they are, no folder.
pub fn for_claude(
    os: Os,
    given: Option<&crate::llm::Env>,
    args: &[String],
) -> Result<(Vec<String>, Option<PathBuf>), String> {
    let place = Place::of(os, &var_in(given))?;
    if let Some(p) = &place {
        test_home_made(p);
        let mut made = made_list();
        match made.get(&p.dir()) {
            Some(Ok(())) => {}
            Some(Err(why)) => {
                let why = why.clone();
                made.remove(&p.dir());
                return Err(why);
            }
            None => return Err(format!("claude's folder {} isn't made yet", p.dir().display())),
        }
    }
    Ok((sources(args, place.as_ref()), place.map(|p| p.dir())))
}

/// `:claude-login` — Claude Code's own login, `claude auth login` (`claude --help` → `auth`, "Manage
/// authentication" → `login`, "Sign in to your Anthropic account"; a subscription by default) by `llm.command`, in
/// the terminal panel (it opens a browser and may ask for a code), as you, outside the sandbox. On Linux with
/// tarae's folder (`CLAUDE_CONFIG_DIR`, made first through no link): the login every claude tarae starts uses. On
/// macOS your own claude's login, as `claude` in a terminal.
pub fn login(ed: &mut Editor) -> Result<(), String> {
    let place = Place::of(Os::HOST, &var_in(ed.claude_env.as_ref()))?;
    let (argv, env) = login_command(&ed.config.llm.command, place.as_ref());
    ed.events.jobs().spawn(move || {
        let made = place.as_ref().map_or(Ok(()), made_kept);
        move |ed: &mut Editor| match made {
            Err(e) => ed.set_error(e),
            Ok(()) if !ed.terminal_run(argv, env) => {
                ed.set_warning("the terminal panel runs something — end it, then :claude-login again")
            }
            Ok(()) => {
                ed.set_status("claude's login, in the terminal panel — the next claude tarae starts uses it")
            }
        }
    });
    Ok(())
}

/// `:claude-login`'s program and the variables it adds.
fn login_command(command: &str, place: Option<&Place>) -> (Vec<String>, Vec<(String, String)>) {
    let argv = vec![command.to_string(), "auth".into(), "login".into()];
    let env = place.map(|p| (VAR.to_string(), p.dir().to_string_lossy().into_owned()));
    (argv, env.into_iter().collect())
}

/// What the chat says when a claude failed with `error` for want of a login, where tarae's claudes keep their own
/// (`place`) — once since tarae started (`told`, set here). None: not a login failure, macOS, or said already.
pub fn login_hint(error: &str, place: Option<&Place>, told: &mut bool) -> Option<String> {
    if place.is_none() || *told || !NOT_LOGGED_IN.iter().any(|l| error.contains(l)) {
        return None;
    }
    *told = true;
    Some(
        "claude has no login for tarae yet — on Linux tarae's claudes keep a login of their own, apart from your \
         ~/.claude: :claude-login logs in, once"
            .into(),
    )
}

#[cfg(test)]
mod tests;
