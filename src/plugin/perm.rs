//! Permissions: what a plugin may do beyond the buffer. It says what in `permissions()` — programs to run,
//! files to read (patterns), environment variables, the network — and nothing it didn't say is ever done.
//!
//! - **Approval**: a plugin that asks for any is held until you approve the whole list: a card shows it when
//!   the plugin loads (once until quit), and what you approved is kept (`plugin-permissions.json` in the
//!   state folder). If a new version asks for something else, it's asked again. What it asks for while the card
//!   is up waits on the answer (`plugin_hold`): done on yes, refused on not now.
//! - **At the moment of use**: a connection to a host:port it hasn't used before is asked about, once; so is
//!   a file outside its read patterns (a kubeconfig `KUBECONFIG` points elsewhere) — when it asked for some
//!   reading at all. The answers are kept with the approval.
//! - **Paths** are checked as written (`..` taken out) and again once resolved, so neither `..` nor a
//!   symlink leads out of a pattern.
//! - `:plugin-permissions` lists each plugin's; `:plugin-revoke <plugin>` takes them back.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use super::abi::{Action, Connect, HostEvent, Permission};
use super::shown;
use crate::editor::Editor;
use crate::offer::{Offer, What};

/// The most requests held for one plugin while its approval card is up (more are refused at once).
const HELD_MAX: usize = 256;

/// What a plugin asks to do right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Connect to a host:port it hasn't used before.
    Connect(Connect),
    /// Read a file outside its patterns: (request id, the path resolved).
    Read(u64, PathBuf),
}

impl Ask {
    /// The request waiting on the answer.
    fn id(&self) -> u64 {
        match self {
            Ask::Connect(c) => c.id,
            Ask::Read(id, _) => *id,
        }
    }

    /// What's asked — (a host?, the host:port or the file): another connection to that host, or that file
    /// read again, is the same question.
    fn subject(&self) -> (bool, String) {
        match self {
            Ask::Connect(c) => (true, target(c)),
            Ask::Read(_, path) => (false, path.display().to_string()),
        }
    }
}

/// One plugin's approvals, as kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Approval {
    /// The permissions approved, as `key` writes them.
    pub granted: Vec<String>,
    /// host:port it may connect to, and files outside its patterns it may read.
    pub hosts: Vec<String>,
    pub files: Vec<String>,
}

/// A permission as it's kept.
pub(super) fn key(p: &Permission) -> String {
    match p {
        Permission::Run(prog) => format!("run {prog}"),
        Permission::Read(pat) => format!("read {pat}"),
        Permission::Env(name) => format!("env {name}"),
        Permission::Network => "network".into(),
        Permission::Project => "project".into(),
    }
}

/// A permission in words, for the card — what it amounts to, broad reading said so.
fn describe(p: &Permission) -> String {
    match p {
        Permission::Run(prog) => format!("run the program {} (with any arguments, as you)", shown(prog)),
        Permission::Read(pat) => {
            let fixed: String =
                pat.split('/').take_while(|c| !c.contains(['*', '?'])).collect::<Vec<_>>().join("/");
            let warn = match fixed.as_str() {
                "" => " ⚠ every file on this computer",
                "~" => " ⚠ every file in your home folder",
                _ => "",
            };
            format!("read files at {}{warn}", shown(pat))
        }
        Permission::Env(name) => format!("read the environment variable {}", shown(name)),
        Permission::Network => "connect over the network (each new host is asked)".into(),
        Permission::Project => {
            "read the files of the project you opened, and find them by name — not ones named like keys \
             (.env, *.pem …): those it asks for"
                .into()
        }
    }
}

/// Why a declared permission isn't one tarae takes, if it isn't: a program is a plain name, a variable a
/// plain name, a pattern absolute (or from `~/`) with no `.` or `..` in it — so what the card shows is what
/// it covers.
pub(super) fn invalid(p: &Permission) -> Option<String> {
    let plain = |s: &str, extra: &str| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    };
    let why = match p {
        Permission::Run(prog) if prog.len() > 64 || !plain(prog, "._+-") || prog.starts_with('.') => {
            "a program is a plain name (no path)"
        }
        Permission::Env(name)
            if name.len() > 128 || !plain(name, "_") || name.starts_with(|c: char| c.is_ascii_digit()) =>
        {
            "an environment variable's name"
        }
        Permission::Read(pat) if pat.len() > 200 => "a pattern up to 200 characters",
        Permission::Read(pat) if !(pat.starts_with('/') || pat.starts_with("~/")) => {
            "a pattern starts at / or ~/"
        }
        Permission::Read(pat) if pat.chars().any(|c| c.is_control() || super::bidi(c)) => {
            "no control characters"
        }
        Permission::Read(pat) if pat.split('/').skip(1).any(|c| matches!(c, "" | "." | "..")) => {
            "no `.`, `..` or empty parts in a pattern"
        }
        _ => return None,
    };
    Some(format!("its permission “{}” isn't valid: {why}", shown(&key(p))))
}

/// Where approvals are kept (none in tests, unless a test says).
pub(super) fn store_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::config::state_dir().map(|d| d.join("plugin-permissions.json"))
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

/// Every plugin's approvals, from `path`.
pub(super) fn load(path: Option<&Path>) -> BTreeMap<String, Approval> {
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return BTreeMap::new() };
    let Ok(Value::Object(all)) = serde_json::from_str::<Value>(&text) else { return BTreeMap::new() };
    all.into_iter()
        .map(|(plugin, v)| {
            let a =
                Approval { granted: strs(&v["granted"]), hosts: strs(&v["hosts"]), files: strs(&v["files"]) };
            (plugin, a)
        })
        .collect()
}

fn save(path: &Path, all: &BTreeMap<String, Approval>) -> std::io::Result<()> {
    // Inserted, not collected: a Map's `collect` sorts — a copy of the sort for each (~6 KB)
    let mut v = serde_json::Map::new();
    for (p, a) in all {
        v.insert(p.clone(), json!({ "granted": a.granted, "hosts": a.hosts, "files": a.files }));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole, then moved into place: never half a file (and a name of its own: two tareas at once)
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = path.with_extension(format!("json.{}.{nanos}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(v)).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

/// The declared list, as kept (sorted, so the order a plugin gives it in doesn't matter).
fn keys(declared: &[Permission]) -> Vec<String> {
    let mut k: Vec<String> = declared.iter().map(key).collect();
    crate::sorting::sort(&mut k);
    k.dedup();
    k
}

/// `~/x` → home/x; `..` and `.` taken out, lexically. None if it isn't absolute after that.
pub(super) fn expand(path: &str) -> Option<PathBuf> {
    let p = match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None if path == "~" => PathBuf::from(std::env::var_os("HOME")?),
        None => PathBuf::from(path),
    };
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    Some(out)
}

/// `*` and `?` in one name.
fn name_matches(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    // (a star's last place, and where in the name it had got to)
    let (mut i, mut j, mut star) = (0, 0, None::<(usize, usize)>);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// A pattern with the folders before its first wildcard resolved (macOS's /tmp is /private/tmp): what a
/// resolved path is matched against.
fn resolved_pattern(pattern: &str) -> Option<PathBuf> {
    let pat = expand(pattern)?;
    let wild = |c: &Component| c.as_os_str().to_string_lossy().contains(['*', '?']);
    let fixed: PathBuf = pat.components().take_while(|c| !wild(c)).collect();
    let rest: PathBuf = pat.components().skip_while(|c| !wild(c)).collect();
    // (the longest part of it that exists)
    let mut base = fixed.clone();
    let mut tail = Vec::new();
    while !base.exists() {
        tail.push(base.file_name()?.to_owned());
        base.pop();
    }
    let mut out = std::fs::canonicalize(&base).ok()?;
    out.extend(tail.iter().rev());
    Some(out.join(rest))
}

/// Whether `path` is one `pattern` covers (`**`: any folders, none too).
pub(super) fn path_matches(pattern: &str, path: &Path) -> bool {
    expand(pattern).is_some_and(|pat| path_matches_in(&pat, path))
}

/// Whether a resolved `path` is one `pattern` covers, resolved too.
fn real_path_matches(pattern: &str, path: &Path) -> bool {
    resolved_pattern(pattern).is_some_and(|pat| path_matches_in(&pat, path))
}

fn path_matches_in(pat: &Path, path: &Path) -> bool {
    let names =
        |p: &Path| p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>();
    fn go(p: &[String], n: &[String]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some((first, rest)) if first == "**" => (0..=n.len()).any(|k| go(rest, &n[k..])),
            Some((first, rest)) => {
                n.split_first().is_some_and(|(name, more)| name_matches(first, name) && go(rest, more))
            }
        }
    }
    go(&names(pat), &names(path))
}

/// A connection's host:port, as approvals keep it (an IPv6 address in brackets).
pub(super) fn target(c: &Connect) -> String {
    if c.host.contains(':') { format!("[{}]:{}", c.host, c.port) } else { format!("{}:{}", c.host, c.port) }
}

impl Editor {
    /// Changes the approvals: on what's kept now (another tarae may have changed it), then kept.
    fn approvals_change(&mut self, f: impl FnOnce(&mut BTreeMap<String, Approval>)) -> std::io::Result<()> {
        let Some(p) = &mut self.plugins else { return Ok(()) };
        let Some(path) = p.perm_store.clone() else {
            f(&mut p.approvals);
            return Ok(());
        };
        let mut all = load(Some(&path));
        f(&mut all);
        let saved = save(&path, &all);
        p.perm_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        p.approvals = all;
        saved
    }

    /// The approvals as kept now — another tarae may have changed them (a revoke counts everywhere).
    fn approvals_fresh(&mut self) {
        let Some(p) = &mut self.plugins else { return };
        let Some(path) = &p.perm_store else { return };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime != p.perm_mtime {
            p.approvals = load(Some(path));
            p.perm_mtime = mtime;
        }
    }

    /// Its cards (approval, asks) closed: it was revoked, dropped or reloaded — no `y` grants it later.
    pub(super) fn plugin_cards_close(&mut self, plugin: &str) {
        self.offers.retain(|o| match &o.what {
            What::PluginPermissions { plugin: q, .. } | What::PluginAsk { plugin: q, .. } => q != plugin,
            _ => true,
        });
        // (what waited on its approval card: refused — never left unanswered)
        let held = self.plugins.as_mut().and_then(|p| p.held.remove(plugin)).unwrap_or_default();
        let why = format!("its permissions wait for your approval (:plugin-approve {plugin})");
        let no: Vec<HostEvent> = held
            .iter()
            .filter_map(|a| match a {
                Action::Spawn(sp) => Some(sp.id),
                Action::ReadFile(id, _) | Action::ReadEnv(id, _) | Action::Find(id, _) => Some(*id),
                Action::Connect(c) => Some(c.id),
                _ => None,
            })
            .map(|id| HostEvent::Failed(id, why.clone()))
            .collect();
        if !no.is_empty() {
            self.plugin_events(plugin, no);
        }
    }

    /// `:plugin-approve <plugin>` — its approval card again.
    pub fn plugin_approve(&mut self, plugin: &str) -> Result<(), String> {
        let plugin = plugin.trim();
        let p = self.plugins.as_mut().ok_or("no plugins are loaded")?;
        let declared =
            p.permissions.get(plugin).cloned().ok_or_else(|| format!("no plugin {plugin} is loaded"))?;
        if declared.is_empty() {
            return Err(format!("{plugin} asks for nothing beyond the buffer"));
        }
        p.asked.insert(plugin.to_string());
        self.plugin_permissions_card(plugin, declared);
        Ok(())
    }

    /// A plugin loaded, asking for `declared`: held until you approve them (a card asks — once until quit).
    pub(super) fn plugin_permissions_loaded(&mut self, plugin: &str, declared: &[Permission]) {
        if declared.is_empty() || self.plugin_approved(plugin) {
            return;
        }
        let Some(p) = &mut self.plugins else { return };
        if !p.asked.insert(plugin.to_string()) {
            return;
        }
        self.plugin_permissions_card(plugin, declared.to_vec());
    }

    fn plugin_permissions_card(&mut self, plugin: &str, declared: Vec<Permission>) {
        let before = self
            .plugins
            .as_ref()
            .and_then(|p| p.approvals.get(plugin))
            .is_some_and(|a| !a.granted.is_empty());
        let list: Vec<String> = declared.iter().map(|p| format!("· {}", describe(p))).collect();
        let head = if before { "It now asks to:" } else { "It asks to:" };
        let n = list.len();
        self.push_offer(
            Offer::new(
                format!("Plugin {plugin} asks for permissions"),
                &format!("{head}\n{}", list.join("\n")),
                "…",
                format!("Plugin {plugin}"),
                What::PluginPermissions { plugin: plugin.to_string(), permissions: declared },
            )
            .deciding(1..1 + n),
        );
    }

    /// Whether what a plugin asks for is what you approved (nothing to approve counts).
    pub(super) fn plugin_approved(&self, plugin: &str) -> bool {
        let Some(p) = &self.plugins else { return false };
        let declared = p.permissions.get(plugin).map(|d| keys(d)).unwrap_or_default();
        declared.is_empty() || p.approvals.get(plugin).is_some_and(|a| a.granted == declared)
    }

    /// The approval card's answer.
    pub(crate) fn plugin_permissions_answer(
        &mut self,
        plugin: String,
        declared: Vec<Permission>,
        allow: bool,
    ) {
        if !allow {
            self.set_status(format!(
                "Plugin {plugin} does nothing beyond the buffer until you approve — :plugin-approve {plugin}"
            ));
            return self.plugin_held_go(&plugin);
        }
        // Still that plugin, still asking for that
        let now = self.plugins.as_ref().and_then(|p| p.permissions.get(&plugin)).map(|d| keys(d));
        if now != Some(keys(&declared)) {
            return;
        }
        let granted = keys(&declared);
        let r = self.approvals_change(|all| all.entry(plugin.clone()).or_default().granted = granted);
        match r {
            Ok(()) => self.set_success(format!("Plugin {plugin}: approved")),
            Err(e) => self.set_warning(format!("approved until you quit — couldn't keep it: {e}")),
        }
        self.plugin_held_go(&plugin);
    }

    /// Whether `a` waits for the plugin's approval card — it needs a permission, and the card asks for them now
    /// (the first time, it comes up, as when a request is refused). Held, it's done once you approve
    /// (`plugin_held_go`): the plugin goes on by itself, not stuck on a refusal from before.
    pub(super) fn plugin_hold(&mut self, plugin: &str, a: &Action) -> bool {
        let needs = matches!(
            a,
            Action::Spawn(_)
                | Action::ReadFile(..)
                | Action::ReadEnv(..)
                | Action::Connect(_)
                | Action::Find(..)
        );
        let Some(declared) = self.plugins.as_ref().and_then(|p| p.permissions.get(plugin)).cloned() else {
            return false;
        };
        if !needs || declared.is_empty() {
            return false;
        }
        self.approvals_fresh();
        if self.plugin_approved(plugin) {
            return false;
        }
        self.plugin_permissions_loaded(plugin, &declared);
        let card = self.offers.iter().chain(&self.session.parked).any(|o| {
            matches!(&o.what, What::PluginPermissions { plugin: q, .. } if q == plugin) && !o.installing()
        });
        let Some(p) = self.plugins.as_mut().filter(|_| card) else { return false };
        let held = p.held.entry(plugin.to_string()).or_default();
        if held.len() >= HELD_MAX {
            return false;
        }
        held.push(a.clone());
        true
    }

    /// The approval card answered: what waited on it, done now — or, not approved, refused as a request is
    /// then (its card gone, so none is held again).
    fn plugin_held_go(&mut self, plugin: &str) {
        let held = self.plugins.as_mut().and_then(|p| p.held.remove(plugin)).unwrap_or_default();
        if held.is_empty() {
            return;
        }
        let doc = self.doc();
        let seen = super::Seen { doc: doc.id, version: doc.version(), ranges: Vec::new() };
        self.plugin_apply(plugin, &seen, held);
    }

    /// Whether a plugin may, right now, do what needs `need` (declared and approved). Err: why not.
    pub(super) fn plugin_may(&mut self, plugin: &str, need: &Permission) -> Result<(), String> {
        let Some(p) = &self.plugins else { return Err("plugins aren't running".into()) };
        let declared = p.permissions.get(plugin).cloned().unwrap_or_default();
        if !declared.contains(need) {
            return Err(format!("it didn't ask to {} (permissions())", describe(need)));
        }
        self.approvals_fresh();
        if !self.plugin_approved(plugin) {
            // (not asked again on its own — a plugin can't nag; `:plugin-approve` brings the card back)
            self.plugin_permissions_loaded(plugin, &declared);
            return Err(format!("its permissions wait for your approval (:plugin-approve {plugin})"));
        }
        Ok(())
    }

    /// Whether a plugin may read `path` (Ok: the path, resolved); `Err(None)`: ask you first.
    pub(super) fn plugin_may_read(&mut self, plugin: &str, path: &str) -> Result<PathBuf, Option<String>> {
        let Some(lexical) = expand(path) else {
            return Err(Some(format!("“{}”: an absolute path, or one from ~/", shown(path))));
        };
        let Some(p) = &self.plugins else { return Err(Some("plugins aren't running".into())) };
        let declared = p.permissions.get(plugin).cloned().unwrap_or_default();
        let patterns: Vec<String> = declared
            .iter()
            .filter_map(|p| if let Permission::Read(pat) = p { Some(pat.clone()) } else { None })
            .collect();
        let project = declared.contains(&Permission::Project).then(|| self.session.root.clone());
        if patterns.is_empty() && project.is_none() {
            return Err(Some("it didn't ask to read files (permissions())".into()));
        }
        if !self.plugin_approved(plugin) {
            return Err(Some("its permissions wait for your approval".into()));
        }
        // Resolved too (a symlink out of the pattern is out of it); a missing file is the reader's to say
        let real = std::fs::canonicalize(&lexical).unwrap_or(lexical.clone());
        let covered = |at: &Path| patterns.iter().any(|pat| path_matches(pat, at));
        let covered_real = |at: &Path| patterns.iter().any(|pat| real_path_matches(pat, at));
        let allowed_before = |at: &Path| {
            self.plugins
                .as_ref()
                .and_then(|p| p.approvals.get(plugin))
                .is_some_and(|a| a.files.iter().any(|f| Path::new(f) == at))
        };
        // The project's files, if it may read them — where they are, and where they lead; not ones named like
        // keys and tokens (`.env`, `*.pem` — `secretnames`): those are asked about like any file outside it
        let secret = crate::secretnames::named_secret(&lexical) || crate::secretnames::named_secret(&real);
        let in_project =
            !secret && project.is_some_and(|root| lexical.starts_with(&root) && real.starts_with(&root));
        if (covered(&lexical) && covered_real(&real)) || in_project || allowed_before(&real) {
            Ok(real)
        } else {
            Err(None)
        }
    }

    /// Whether a plugin may connect to host:port; `Err(None)`: ask you first.
    pub(super) fn plugin_may_connect(&mut self, plugin: &str, c: &Connect) -> Result<(), Option<String>> {
        self.plugin_may(plugin, &Permission::Network).map_err(Some)?;
        let ok = |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':');
        if c.host.is_empty()
            || c.host.len() > 253
            || !c.host.chars().all(ok)
            || !(1..=65535).contains(&c.port)
        {
            return Err(Some(format!("“{}” port {} isn't a host to connect to", shown(&c.host), c.port)));
        }
        let target = target(c);
        let known = self
            .plugins
            .as_ref()
            .and_then(|p| p.approvals.get(plugin))
            .is_some_and(|a| a.hosts.contains(&target));
        if known { Ok(()) } else { Err(None) }
    }

    /// Asks you about one connection or file — one card a question: a request asking what a card already asks
    /// (another connection to that host) waits on that card's answer.
    pub(super) fn plugin_ask(&mut self, plugin: &str, ask: Ask) {
        let asked =
            self.offers.iter_mut().chain(self.session.parked.iter_mut()).find_map(|o| match &mut o.what {
                What::PluginAsk { plugin: p, asks }
                    if p == plugin && asks.first().map(Ask::subject) == Some(ask.subject()) =>
                {
                    Some(asks)
                }
                _ => None,
            });
        if let Some(asks) = asked {
            asks.push(ask);
            return;
        }
        let (title, question) = match &ask {
            Ask::Connect(c) => (
                format!("Plugin {plugin} wants to connect"),
                format!("to {}\nAllow it, from now on?", target(c)),
            ),
            Ask::Read(_, path) => (
                format!("Plugin {plugin} wants to read a file"),
                format!(
                    "{}\n(outside the files it asked for) Allow it, from now on?",
                    shown(&path.display().to_string())
                ),
            ),
        };
        // (what's decided: the host, the file — not the question after it)
        self.push_offer(
            Offer::new(
                title,
                &question,
                "…",
                format!("Plugin {plugin}"),
                What::PluginAsk { plugin: plugin.to_string(), asks: vec![ask] },
            )
            .deciding(0..1),
        );
    }

    /// Your answer about one connection or file: yes is kept, and the requests waiting on it go on; no fails
    /// them.
    pub(crate) fn plugin_ask_answer(&mut self, plugin: String, asks: Vec<Ask>, allow: bool) {
        // Still loaded and approved (a card left from before a revoke grants nothing)
        let loaded = self.plugins.as_ref().is_some_and(|p| p.permissions.contains_key(&plugin));
        self.approvals_fresh();
        if !loaded || !self.plugin_approved(&plugin) {
            return;
        }
        if !allow {
            let no = asks.iter().map(|a| HostEvent::Failed(a.id(), "you said no".into())).collect();
            return self.plugin_events(&plugin, no);
        }
        let Some((host, entry)) = asks.first().map(Ask::subject) else { return };
        let r = self.approvals_change(|all| {
            let a = all.entry(plugin.clone()).or_default();
            if host { a.hosts.push(entry) } else { a.files.push(entry) }
        });
        if let Err(e) = r {
            self.set_warning(format!("allowed until you quit — couldn't keep it: {e}"));
        }
        for ask in asks {
            match ask {
                Ask::Connect(c) => self.plugin_connect_now(&plugin, c),
                Ask::Read(id, path) => self.plugin_read_now(&plugin, id, path),
            }
        }
    }

    /// `:plugin-permissions` — every plugin's, in a buffer.
    pub fn plugin_permissions_list(&mut self) -> Result<(), String> {
        let p = self.plugins.as_ref().ok_or("no plugins are loaded")?;
        let mut out =
            String::from("# Plugin permissions\n\n`:plugin-revoke <plugin>` takes a plugin's back.\n");
        let mut names: Vec<&String> = p.permissions.keys().collect();
        crate::sorting::sort(&mut names);
        for name in names {
            let declared = &p.permissions[name];
            let a = p.approvals.get(name).cloned().unwrap_or_default();
            let state = if declared.is_empty() {
                "nothing beyond the buffer"
            } else if a.granted == keys(declared) {
                "approved"
            } else {
                "waiting for your approval"
            };
            out.push_str(&format!("\n## {name} — {state}\n\n"));
            for d in declared {
                out.push_str(&format!("- {}\n", describe(d)));
            }
            for h in &a.hosts {
                out.push_str(&format!("- connected to {h}\n"));
            }
            for f in &a.files {
                out.push_str(&format!("- read {f}\n"));
            }
        }
        let id = self.open_virtual("plugin permissions", &out);
        if let Some(spec) = crate::syntax::spec("markdown") {
            self.load_language(id, spec);
        }
        Ok(())
    }

    /// `:plugin-revoke <plugin>` — its approvals, hosts and files forgotten; it asks again next time.
    pub fn plugin_revoke(&mut self, plugin: &str) -> Result<(), String> {
        let plugin = plugin.trim();
        self.approvals_fresh();
        let p = self.plugins.as_mut().ok_or("no plugins are loaded")?;
        if !p.approvals.contains_key(plugin) {
            return Err(format!("{plugin} has nothing approved"));
        }
        // What it runs stops now, and its cards go
        p.stop_all(plugin);
        self.plugin_cards_close(plugin);
        self.approvals_change(|all| {
            all.remove(plugin);
        })
        .map_err(|e| e.to_string())?;
        self.set_success(format!("Plugin {plugin}: permissions taken back"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_cover_what_they_say() {
        let home = std::env::var("HOME").unwrap();
        let p = |s: &str| PathBuf::from(s.replace('~', &home));
        assert!(path_matches("~/.kube/**", &p("~/.kube/config")));
        assert!(path_matches("~/.kube/**", &p("~/.kube/cache/discovery/x.json")));
        assert!(!path_matches("~/.kube/**", &p("~/.ssh/id_rsa")));
        assert!(path_matches("~/.kube/*.yaml", &p("~/.kube/dev.yaml")));
        assert!(!path_matches("~/.kube/*.yaml", &p("~/.kube/a/dev.yaml")), "* stays in one folder");
        assert!(path_matches("/etc/k8s/admin.conf", Path::new("/etc/k8s/admin.conf")));
        assert!(name_matches("k?b*.conf", "kube-admin.conf") && !name_matches("*.conf", "x.yaml"));
        // `..` doesn't climb out
        assert_eq!(expand("~/.kube/../.ssh/id_rsa"), Some(p("~/.ssh/id_rsa")));
        assert!(!path_matches("~/.kube/**", &expand("~/.kube/../.ssh/id_rsa").unwrap()));
        assert_eq!(expand("relative/path"), None);
    }

    #[test]
    fn a_symlink_out_of_a_pattern_is_out_of_it() {
        let d = std::env::temp_dir().join(format!("tarae-perm-{}", std::process::id()));
        std::fs::create_dir_all(d.join("allowed")).unwrap();
        std::fs::write(d.join("allowed/plain"), "x").unwrap();
        std::fs::write(d.join("secret"), "x").unwrap();
        let link = d.join("allowed/link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(d.join("secret"), &link).unwrap();
        let pattern = format!("{}/allowed/**", d.display());
        assert!(path_matches(&pattern, &link), "as written, it's in");
        let real = std::fs::canonicalize(&link).unwrap();
        assert!(!real_path_matches(&pattern, &real), "resolved, it isn't");
        // …while a plain file in it stays in, though the temp folder itself is a symlink on macOS
        let plain = std::fs::canonicalize(d.join("allowed/plain")).unwrap();
        assert!(real_path_matches(&pattern, &plain));
        std::fs::remove_dir_all(&d).ok();
    }
}
