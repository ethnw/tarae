//! Databases in the code: PostgreSQL, MySQL and Redis — what the live database says beside the statements and
//! keys your code holds (db-plan.md). So far: the connections, found where the project already says them —
//! `.tarae/db.toml`, your own git-ignored `.tarae/db.local.toml` (it wins by name), its compose files' database
//! services, the variables tarae started with — and shown in the side panel's `db` tab and the status line, never a
//! password; and the one in use connected to and signed in (`live`), its server's version said.
//!
//! `:db` turns it on (the tab opens the first time) and, once on, asks which to use when there are several;
//! `:db-off` turns it off. A connection whose `paths` cover the file in front is that file's.
wit_bindgen::generate!({ path: "../wit", world: "plugin", additional_derives: [PartialEq] });

mod compose;
mod config;
mod conn;
mod driver;
mod env;
mod glob;
mod live;
mod pgpass;
mod secret;
mod url;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use tarae::plugin::types::{Pick, Side, SideRow, Tone};

use conn::Connection;
use live::{Live, Status};

// ── Ids ────────────────────────────────────────────────────────────────────
// (8, 9 and from 1 000 000: the live connection's — `live.rs`)

const FIND_CONFIG: u64 = 1;
const FIND_COMPOSE: u64 = 2;
const CONFIG: u64 = 3;
const PICK: u64 = 4;
/// The project's top files (to learn its folder); the personal config, read by its path; `find` of it (git-ignored,
/// it isn't found).
const FIND_ROOT: u64 = 5;
const LOCAL: u64 = 6;
const FIND_LOCAL: u64 = 7;
/// Variable `n` of `driver::env_names()`; compose file `n`.
const ENV: u64 = 100;
const COMPOSE: u64 = 1000;
/// The compose files read at most.
const MAX_COMPOSE: usize = 16;

const CONFIG_FILE: &str = ".tarae/db.toml";
const LOCAL_FILE: &str = ".tarae/db.local.toml";

#[derive(Default)]
struct State {
    on: bool,
    /// Requests out, not back yet: what's found is put together once they're all in.
    waiting: BTreeSet<u64>,
    /// The project's folder, once known (a top file's, or `.tarae/db.toml`'s parent's parent).
    root: Option<String>,
    /// The config files (`CONFIG`, `LOCAL`): their paths, what they say.
    config_paths: BTreeMap<u64, String>,
    configs: BTreeMap<u64, Result<config::Read, String>>,
    local_asked: bool,
    compose_paths: Vec<String>,
    compose: BTreeMap<String, Result<Vec<Connection>, String>>,
    vars: BTreeMap<String, String>,
    /// Put together: config's, then compose's, then the environment's.
    connections: Vec<Connection>,
    /// Problems not about one connection: (where, why).
    problems: Vec<(String, String)>,
    /// The one you chose (`:db`, `u` in the tab), by name.
    chosen: Option<String>,
    front: Option<String>,
    side: Option<Side>,
    side_opened: bool,
    badge: Option<(String, Tone)>,
    /// The connection in use, connected to.
    live: Option<Live>,
    /// Tests' fixed random bytes (a nonce a captured exchange used); else each session's are drawn fresh.
    seed: Option<[u8; 32]>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

impl State {
    // ── On and off ──

    fn start(&mut self) -> Vec<Action> {
        let (front, chosen, side_opened, seed) = (self.front.take(), self.chosen.take(), self.side_opened, self.seed);
        *self = State { on: true, front, chosen, side_opened, seed, ..State::default() };
        let mut out = vec![
            Action::Find((FIND_CONFIG, CONFIG_FILE.into())),
            Action::Find((FIND_COMPOSE, "**/*compose*.y*ml".into())),
            Action::Find((FIND_ROOT, "*".into())),
            Action::Find((FIND_LOCAL, LOCAL_FILE.into())),
        ];
        self.waiting.extend([FIND_CONFIG, FIND_COMPOSE, FIND_ROOT, FIND_LOCAL]);
        for (i, name) in driver::env_names().iter().enumerate() {
            self.waiting.insert(ENV + i as u64);
            out.push(Action::ReadEnv((ENV + i as u64, name.to_string())));
        }
        out
    }

    fn stop(&mut self) -> Vec<Action> {
        let mut out = self.live.take().map(|mut l| l.close()).unwrap_or_default();
        let (side_opened, seed) = (self.side_opened, self.seed);
        *self = State { side_opened, seed, ..State::default() };
        out.extend([Action::Side(None), Action::Badge(None)]);
        out
    }

    // ── What came back ──

    fn event(&mut self, ev: HostEvent) -> Vec<Action> {
        if let Some(l) = &mut self.live {
            let fixed = self.seed;
            let mut seed = move || fixed.or_else(fresh_seed);
            if let Some(out) = l.event(&ev, &mut live::Ctx { vars: &self.vars, seed: &mut seed }) {
                return out;
            }
        }
        match ev {
            // (a connection's, after it was closed — the arms below are discovery's)
            HostEvent::Failed((id, _)) | HostEvent::Data((id, _)) | HostEvent::Closed((id, _)) | HostEvent::Connected(id)
                if id >= live::CONNECT =>
            {
                Vec::new()
            }
            HostEvent::Found((FIND_CONFIG, paths)) => {
                self.waiting.remove(&FIND_CONFIG);
                let Some(path) = paths.into_iter().find(|p| p.ends_with(&format!("/{CONFIG_FILE}"))) else {
                    return Vec::new();
                };
                self.root.get_or_insert_with(|| path[..path.len() - CONFIG_FILE.len() - 1].to_string());
                self.config_paths.insert(CONFIG, path.clone());
                self.waiting.insert(CONFIG);
                let mut out = vec![Action::ReadFile((CONFIG, path))];
                out.extend(self.read_local());
                out
            }
            HostEvent::Found((FIND_ROOT, paths)) => {
                self.waiting.remove(&FIND_ROOT);
                if let Some(top) = paths.first().and_then(|p| p.rsplit_once('/')) {
                    self.root.get_or_insert_with(|| top.0.to_string());
                }
                self.read_local()
            }
            HostEvent::Found((FIND_LOCAL, paths)) => {
                self.waiting.remove(&FIND_LOCAL);
                // (`find` leaves out what git ignores: found, it isn't)
                if !paths.is_empty() {
                    self.problems.push((LOCAL_FILE.into(), "isn't git-ignored — it would be committed with the project".into()));
                }
                Vec::new()
            }
            HostEvent::Found((FIND_COMPOSE, paths)) => {
                self.waiting.remove(&FIND_COMPOSE);
                if paths.len() > MAX_COMPOSE {
                    self.problems.push(("compose files".into(), format!("{} found — the first {MAX_COMPOSE} read", paths.len())));
                }
                self.compose_paths = paths.into_iter().take(MAX_COMPOSE).collect();
                (0..self.compose_paths.len())
                    .map(|i| {
                        self.waiting.insert(COMPOSE + i as u64);
                        Action::ReadFile((COMPOSE + i as u64, self.compose_paths[i].clone()))
                    })
                    .collect()
            }
            HostEvent::File((id @ (CONFIG | LOCAL), bytes)) => {
                self.waiting.remove(&id);
                let (path, root) = (self.config_paths.get(&id).cloned().unwrap_or_default(), self.root.clone().unwrap_or_default());
                self.configs.insert(id, config::read(&path, &root, &String::from_utf8_lossy(&bytes)));
                Vec::new()
            }
            HostEvent::File((id, bytes)) if id >= COMPOSE => {
                self.waiting.remove(&id);
                if let Some(path) = self.compose_paths.get((id - COMPOSE) as usize) {
                    self.compose.insert(path.clone(), compose::read(path, &String::from_utf8_lossy(&bytes)));
                }
                Vec::new()
            }
            HostEvent::Env((id, value)) => {
                self.waiting.remove(&id);
                let name = id.checked_sub(ENV).and_then(|i| driver::env_names().get(i as usize).copied());
                if let (Some(name), Some(v)) = (name, value) {
                    self.vars.insert(name.to_string(), v);
                }
                Vec::new()
            }
            HostEvent::Failed((id, why)) => {
                self.waiting.remove(&id);
                let what = match id {
                    FIND_CONFIG | FIND_COMPOSE => "the project's files".to_string(),
                    // (said once, by the two above)
                    FIND_ROOT | FIND_LOCAL => return Vec::new(),
                    CONFIG => CONFIG_FILE.to_string(),
                    // (no personal config is the usual case)
                    LOCAL if why.contains("No such file") => return Vec::new(),
                    LOCAL => LOCAL_FILE.to_string(),
                    i if i >= COMPOSE => self.compose_paths.get((i - COMPOSE) as usize).map_or("a compose file", |p| conn::file_name(p)).to_string(),
                    i if i >= ENV => format!("${}", driver::env_names().get((i - ENV) as usize).unwrap_or(&"?")),
                    _ => return Vec::new(),
                };
                self.problems.push((what, why));
                Vec::new()
            }
            HostEvent::Answer((PICK, Some(name))) => self.use_(name),
            HostEvent::Key((key, _, Some(id))) if key == "u" => match id.strip_prefix("conn:") {
                Some(name) => self.use_(name.to_string()),
                None => vec![Action::Status("not a connection".into())],
            },
            _ => Vec::new(),
        }
    }

    /// The personal config, read by its path — once the project's folder is known (`find` would leave it out:
    /// it's git-ignored).
    fn read_local(&mut self) -> Vec<Action> {
        let Some(root) = self.root.as_ref().filter(|_| !self.local_asked) else { return Vec::new() };
        let path = format!("{root}/{LOCAL_FILE}");
        self.local_asked = true;
        self.config_paths.insert(LOCAL, path.clone());
        self.waiting.insert(LOCAL);
        vec![Action::ReadFile((LOCAL, path))]
    }

    /// A config's `url-env` connections, from the variable now read.
    fn fill_url_env(&self, read: &mut config::Read) {
        for (i, var) in std::mem::take(&mut read.url_env) {
            let c = &mut read.connections[i];
            match self.vars.get(&var).map(|u| env::from_url(&var, u)) {
                Some(Ok(u)) => {
                    (c.kind, c.host, c.port, c.user, c.database) = (u.kind, u.host, u.port, u.user, u.database);
                    c.tls = c.tls.or(u.tls);
                    if matches!(c.password, conn::Password::None) {
                        c.password = u.password;
                    }
                }
                Some(Err(why)) => c.problems.push(why),
                None => c.problems.push(format!("${var} isn't set")),
            }
        }
    }

    /// Everything found, put together (once nothing's still coming).
    fn settle(&mut self) {
        let mut cs: Vec<Connection> = Vec::new();
        let mut problems: Vec<(String, String)> = std::mem::take(&mut self.problems);
        let mut configs = std::mem::take(&mut self.configs);
        // A variable a config names (`url-env`) is that connection, not one more of the environment's
        let mut named = BTreeSet::new();
        for (id, file) in [(CONFIG, CONFIG_FILE), (LOCAL, LOCAL_FILE)] {
            match configs.remove(&id) {
                Some(Ok(mut read)) => {
                    named.extend(read.named.iter().cloned());
                    self.fill_url_env(&mut read);
                    for c in read.connections {
                        // Yours wins over the project's, by name, where it stood
                        match cs.iter_mut().find(|o| o.name == c.name) {
                            Some(o) => *o = c,
                            None => cs.push(c),
                        }
                    }
                }
                Some(Err(why)) => problems.push((file.into(), why)),
                None => {}
            }
        }
        for (path, found) in &self.compose {
            match found {
                Ok(found) => cs.extend(found.iter().cloned()),
                Err(why) => problems.push((conn::file_name(path).to_string(), why.clone())),
            }
        }
        let (from_env, env_problems) = env::connections(&self.vars);
        cs.extend(from_env.into_iter().filter(|c| !matches!(&c.origin, conn::Origin::Env(var) if named.contains(var))));
        problems.extend(env_problems.into_iter().map(|why| ("environment".to_string(), why)));
        // Names are what you choose by: one each (a second gets where it's from)
        let mut names = BTreeSet::new();
        for c in &mut cs {
            if !names.insert(c.name.clone()) {
                c.name = format!("{} ({})", c.name, c.origin.said());
                names.insert(c.name.clone());
            }
        }
        self.connections = cs;
        self.problems = problems;
        if self.chosen.as_ref().is_some_and(|n| !self.connections.iter().any(|c| &c.name == n)) {
            self.chosen = None;
        }
    }

    // ── Which one ──

    /// The file in front's connection: the first whose `paths` cover it, else the one you chose, else the only one.
    fn current(&self) -> Option<&Connection> {
        let scoped = self.front.as_deref().and_then(|f| self.connections.iter().find(|c| c.covers(f)));
        scoped
            .or_else(|| self.chosen.as_ref().and_then(|n| self.connections.iter().find(|c| &c.name == n)))
            .or_else(|| if self.connections.len() == 1 { self.connections.first() } else { None })
    }

    fn use_(&mut self, name: String) -> Vec<Action> {
        if !self.connections.iter().any(|c| c.name == name) {
            return vec![Action::Warning(format!("no connection {name}"))];
        }
        self.chosen = Some(name.clone());
        let mut out = self.show();
        out.push(Action::Status(format!("db: {name}")));
        out
    }

    fn pick(&self) -> Vec<Action> {
        let items = self
            .connections
            .iter()
            .map(|c| {
                // (production first: a long line is cut at its end)
                let prod = if c.production() { " · production" } else { "" };
                (c.name.clone(), format!("{}{prod}  {}  {}", c.name, c.shown(), c.origin.said()))
            })
            .collect();
        vec![Action::Pick(Pick { id: PICK, title: "Use which database?".into(), items })]
    }

    // ── What's shown ──

    /// The live connection follows the one in use: opened when there is one, closed and opened again when it
    /// changes (another chosen, another file in front, found again different), closed when there's none.
    fn follow(&mut self) -> Vec<Action> {
        if !self.waiting.is_empty() {
            return Vec::new();
        }
        let want = self.current().cloned();
        if let (Some(l), Some(c)) = (&self.live, &want)
            && &l.conn == c
        {
            return Vec::new();
        }
        let mut out = self.live.take().map(|mut l| l.close()).unwrap_or_default();
        if let Some(c) = want {
            let mut l = Live::new(c);
            let fixed = self.seed;
            let mut seed = move || fixed.or_else(fresh_seed);
            out.extend(l.open(&mut live::Ctx { vars: &self.vars, seed: &mut seed }));
            self.live = Some(l);
        }
        out
    }

    /// How the connection in use stands, if it's live.
    fn live_status(&self, name: &str) -> Option<&Status> {
        self.live.as_ref().filter(|l| l.conn.name == name).map(|l| &l.status)
    }

    fn show(&mut self) -> Vec<Action> {
        if !self.on {
            return Vec::new();
        }
        let mut out = self.follow();
        let side = self.side_now();
        if self.side.as_ref() != Some(&side) {
            self.side = Some(side.clone());
            // The first time it's on: the side panel opens at its tab
            let open = !std::mem::replace(&mut self.side_opened, true);
            out.push(Action::Side(Some(Side { open, ..side })));
        }
        let badge = self.badge_now();
        if self.badge.as_ref() != Some(&badge) {
            self.badge = Some(badge.clone());
            out.push(Action::Badge(Some(badge)));
        }
        out
    }

    fn badge_now(&self) -> (String, Tone) {
        if !self.waiting.is_empty() {
            return ("db …".into(), Tone::Info);
        }
        // Connected: the server's version beside the name; down: said (the tab says why); else the name alone —
        // a connection being made doesn't make the status line jump
        let state = |c: &Connection| {
            let (mut said, mut fine) = match self.live_status(&c.name) {
                Some(Status::Ready { version }) => (format!(" · {}", with_version(c.kind.driver().short(), version)), true),
                Some(Status::Down { .. }) => (" · not connected".to_string(), false),
                _ => (String::new(), true),
            };
            // Encryption with nobody checked at the other end: said wherever the connection is
            if c.tls() == conn::TlsMode::Unverified {
                (said, fine) = (format!("{said} · TLS not verified"), false);
            }
            (said, fine)
        };
        match self.current() {
            Some(c) if c.production() => (format!("db {}{} · production", c.name, state(c).0), Tone::Error),
            Some(c) => match state(c) {
                (said, true) => (format!("db {}{said}", c.name), Tone::Ok),
                (said, false) => (format!("db {}{said}", c.name), Tone::Warning),
            },
            None if self.connections.is_empty() => ("db · none found".into(), Tone::Warning),
            None => (format!("db · {} found — :db to choose", self.connections.len()), Tone::Info),
        }
    }

    /// The tab: each connection — its name, what it is (`pg · prod · in use`); under it, as whom where; its
    /// password's whereabouts; what's wrong with it — and what couldn't be read. Short lines: the panel is narrow,
    /// and a cut word says nothing.
    fn side_now(&self) -> Side {
        let row = |id: String, depth, text: String, detail: String, tone, at: Option<(String, u64)>| SideRow {
            id,
            depth,
            text,
            detail,
            tone,
            at: at.map(|(p, l)| (p, l, l)),
            note: false,
        };
        let why = |depth, text: String, tone| SideRow { note: true, ..row(String::new(), depth, text, String::new(), tone, None) };
        let mut rows = Vec::new();
        if !self.waiting.is_empty() {
            rows.push(row("finding".into(), 0, "finding the project's databases…".into(), String::new(), Tone::Info, None));
        }
        let current = self.current().map(|c| c.name.clone());
        for c in &self.connections {
            let tone = if !c.problems.is_empty() {
                Tone::Warning
            } else if c.production() {
                Tone::Info
            } else {
                Tone::Ok
            };
            let mut what = vec![c.kind.driver().short()];
            if c.production() {
                what.push("prod");
            }
            if current.as_ref() == Some(&c.name) {
                what.push("in use");
            }
            rows.push(row(format!("conn:{}", c.name), 0, c.name.clone(), what.join(" · "), tone, c.origin.place()));
            rows.push(row(format!("conn:{}:at", c.name), 1, c.target(), String::new(), Tone::Info, None));
            // (where it's set: `enter` goes there, and the picker says it)
            rows.push(row(format!("conn:{}:password", c.name), 1, c.password.said(), String::new(), Tone::Info, None));
            if let conn::Password::Unknown(w) = &c.password {
                rows.push(why(1, w.clone(), Tone::Info));
            }
            // libpq ignores a `.pgpass` others may read; a plugin isn't told a file's mode until API 0.3 (§6 #8)
            if let conn::Password::File(_) = &c.password {
                rows.push(why(1, "its mode can't be checked yet".into(), Tone::Info));
            }
            // Encryption without the certificate checked is had only by naming it — and said every time it's shown
            if c.tls() == conn::TlsMode::Unverified {
                rows.push(why(1, "TLS not verified".into(), Tone::Warning));
            }
            // The one in use: how its connection stands
            let live = |text: &str, tone| row(format!("conn:{}:live", c.name), 1, text.to_string(), String::new(), tone, None);
            match self.live_status(&c.name) {
                Some(Status::Ready { version }) => {
                    rows.push(live(&format!("connected · {}", with_version(c.kind.driver().short(), version)), Tone::Ok));
                }
                Some(Status::Connecting | Status::SigningIn | Status::Reading) => rows.push(live("connecting…", Tone::Info)),
                Some(Status::Down { why: w, again }) => {
                    rows.push(live(if *again { "not connected — trying again" } else { "not connected" }, Tone::Warning));
                    rows.push(why(1, w.clone(), Tone::Warning));
                }
                Some(Status::Cannot(w)) => {
                    rows.push(live("not connected", Tone::Info));
                    rows.push(why(1, w.clone(), Tone::Info));
                }
                None => {}
            }
            rows.extend(c.problems.iter().map(|p| why(1, p.clone(), Tone::Warning)));
        }
        for (i, (what, problem)) in self.problems.iter().enumerate() {
            rows.push(row(format!("problem:{i}"), 0, what.clone(), String::new(), Tone::Warning, None));
            rows.push(why(1, problem.clone(), Tone::Warning));
        }
        if self.waiting.is_empty() && self.connections.is_empty() {
            rows.push(row("none".into(), 0, "no databases found".into(), String::new(), Tone::Info, None));
            rows.push(why(1, "a .tarae/db.toml, a compose service or $DATABASE_URL names one".into(), Tone::Info));
        }
        let note = match &current {
            Some(name) => format!("db {name}"),
            None => format!("{} found", self.connections.len()),
        };
        let keys = [("u", "use"), ("ret", "where it's set")];
        Side {
            title: "db".into(),
            note,
            rows,
            keys: keys.iter().map(|(k, d)| (k.to_string(), d.to_string())).collect(),
            open: false,
            focus: false,
        }
    }
}

/// `pg 17.6 (Debian 17.6-1.pgdg120+1)` → `pg 17.6`; a version the server didn't say (a Redis before 6, without
/// HELLO) → `redis`.
fn with_version(short: &str, v: &str) -> String {
    match v.split_whitespace().next() {
        Some(v) => format!("{short} {v}"),
        None => short.to_string(),
    }
}

/// Random bytes from the SDK's generator (stirred by every call's entropy) — none before any call handed entropy
/// over: then nothing is signed in (a fixed nonce, fixed padding would be weaker than not connecting).
fn fresh_seed() -> Option<[u8; 32]> {
    let mut b = [0; 32];
    tarae_plugin::now::fill(&mut b).ok().map(|()| b)
}

// ── The plugin ─────────────────────────────────────────────────────────────

struct Plugin;

impl Guest for Plugin {
    fn commands() -> Vec<Command> {
        vec![
            Command {
                name: "db".into(),
                doc: "Databases: the project's connections — on, then which to use".into(),
                text: false,
            },
            Command { name: "db-off".into(), doc: "Databases: off".into(), text: false },
        ]
    }

    fn subscriptions() -> Vec<Subscription> {
        // (the badge and the tab follow the file in front: a connection's `paths`)
        vec![Subscription { event: Event::Shown, text: false, languages: Vec::new() }]
    }

    fn permissions() -> Vec<Permission> {
        // (each database's host:port is asked about the first time it's connected to)
        let mut p = vec![Permission::Project, Permission::Network];
        p.extend(driver::env_names().iter().map(|n| Permission::Env(n.to_string())));
        p.extend(config::PASSWORD_FILES.iter().map(|f| Permission::Read(f.to_string())));
        p
    }

    fn run(name: String, cx: Context) -> Vec<Action> {
        tarae_plugin::now::set(cx.now.millis, &cx.now.entropy);
        STATE.with_borrow_mut(|s| s.run(&name, cx.buffer.path))
    }

    fn on(event: Event, cx: Context) -> Vec<Action> {
        tarae_plugin::now::set(cx.now.millis, &cx.now.entropy);
        if event != Event::Shown {
            return Vec::new();
        }
        STATE.with_borrow_mut(|s| {
            s.front = cx.buffer.path;
            s.show()
        })
    }

    fn on_events(events: Vec<HostEvent>, now: Now) -> Vec<Action> {
        tarae_plugin::now::set(now.millis, &now.entropy);
        STATE.with_borrow_mut(|s| s.on_events(events))
    }
}

impl State {
    fn run(&mut self, name: &str, front: Option<String>) -> Vec<Action> {
        match name {
            "db-off" => self.stop(),
            "db" if !self.on => {
                self.front = front;
                let mut out = self.start();
                out.extend(self.show());
                out
            }
            "db" if self.waiting.is_empty() && self.connections.len() > 1 => self.pick(),
            "db" => {
                let side = self.side_now();
                self.side = Some(side.clone());
                vec![Action::Side(Some(Side { open: true, focus: true, ..side }))]
            }
            _ => Vec::new(),
        }
    }

    fn on_events(&mut self, events: Vec<HostEvent>) -> Vec<Action> {
        if !self.on {
            return Vec::new();
        }
        let was_waiting = !self.waiting.is_empty();
        let mut out = Vec::new();
        for ev in events {
            out.extend(self.event(ev));
        }
        if was_waiting && self.waiting.is_empty() {
            self.settle();
        }
        out.extend(self.show());
        out
    }
}

export!(Plugin);

#[cfg(test)]
mod tests;
