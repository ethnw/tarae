//! The property loop (roadmap §3, from Progent's split): no change made without a card — or your own config —
//! ever widens what a session may do; a take-back narrows at once. Seeded sequences of the ways a session's `Caps`
//! and what the gates read beside it change (the real functions, over a project in a scratch folder: cards'
//! answers, your config and the project's `.tarae.toml` reloaded, chats opened and ended, take-backs — their store's
//! write failing too —, sessions made and switched, a restart, another checkout appearing), each step's `Reach`
//! before and after held to `check`. The stores' writes are held, each landing where the seed puts it among the
//! other steps (`Step::Land`) — one failing, another tarae writing the same file between them (`Step::Other`), a
//! quit cutting them off, `:restart` carrying them — never waited for between steps. Grants are sets of paths,
//! ports, hosts and names, so "wider" is set inclusion per kind, a path covered by one it's inside.
//!
//! `TARAE_CAPS_SEEDS=N` runs N seeds instead of `SEEDS`, `TARAE_CAPS_SEED=s` that one alone.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use toml::Value;

use super::*;
use crate::allow::{Grant, Scope};
use crate::editor::Editor;
use crate::trust::{Kind, State};

/// How a setting in `Reach` widens: `Opens` — on, or more in its list; `Guards` — off, or less in its list;
/// `Where` — any change.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Way {
    Opens,
    Guards,
    Where,
}

/// The settings a session's reach reads (`tools::with_allowed` starts from them; `guard_then_go`, the edit review
/// and the run cards read the rest).
const SETTINGS: [(&str, Way); 12] = [
    ("llm.sandbox", Way::Guards),
    ("llm.sandbox-network", Way::Opens),
    ("llm.sandbox-hosts", Way::Opens),
    ("llm.sandbox-upstream", Way::Where),
    ("llm.sandbox-ports", Way::Opens),
    ("llm.sandbox-localhost", Way::Opens),
    ("llm.sandbox-write", Way::Opens),
    ("llm.sandbox-env", Way::Opens),
    ("llm.sandbox-protect", Way::Guards),
    ("llm.run-allow", Way::Opens),
    ("llm.review-ask", Way::Guards),
    ("llm.review-at-end", Way::Opens),
];

/// The `trust::RISKY` settings that aren't what a session's Claude may do, each with why — with `SETTINGS`, every
/// one of them (`every_risky_setting_is_in_reach_or_said_elsewhere`).
const ELSEWHERE: [(&str, &str); 11] = [
    ("editor.lsp", "language servers, run as you"),
    ("editor.restart-build", "`:restart`'s build, run as you"),
    ("editor.restart-binary", "what `:restart` starts, as you"),
    ("llm.command", "the claude process itself"),
    ("llm.args", "the claude process itself"),
    ("llm.chat-settings", "what claude loads, not what its tools may do"),
    ("llm.screen-terminal", "what Claude's screen shows"),
    ("session.worktrees", "where checkouts are made"),
    ("session.ended", "a command at a session's end, as you"),
    ("session.keep", "files moved at a session's end"),
    ("review.risky", "what a code review holds for a look"),
];

/// What a session may do, as the gates read it — each part from where they read it, so a gate input added there is
/// added here too:
/// - `settings`: `config.llm` with the answers in, through `tools::with_allowed` — what a run's policy is made
///   from (`tools::guard_then_go`) — each `SETTINGS` row as the settings table reads it, `llm.review-at-end`
///   on too where the chat's edit gate has it (`edits::request` reads `Chat.edits.at_end`, not the config);
/// - `read`, `outside`, `keys`: the session's `Caps` as a gate makes it (`Basis::build` — `reads::covered`,
///   `cards::decide`, `tools::in_sandbox`), with `with_allowed`'s reads and keys;
/// - `open_keys`: of the files named like keys looked at, those whose text goes with a message — what `Caps`
///   opens (a kept one counts before the chat's copy is read, or with no chat), and what `reads::key_shut` lets
///   through (the chat's answers and its copy of the kept ones, `Chat.kept_keys`);
/// - `sockets`, `hosts`, `terminals`: `with_allowed`'s, the hosts with `Session.hosts` added as `guard_then_go`
///   adds them;
/// - `again`, `sees`: the chat's `a` on a run's card (`Chat.run_again`, read by `tools.rs`'s gate — Docker's
///   socket rides with one) and on the screen card (`Chat.sees`, `seeing.rs`);
/// - `unsandboxed`: `Session.no_sandbox`'s `s` — checks and reads run unasked without a sandbox.
///
/// And to hold the rest of `check` to: `caps` (made now), `shown` (the session's own, `Session.caps` — `:granted`,
/// the chat's count), `listed` (the answers as `:granted` lists them), `trusted` (the project's settings applied,
/// or the store trusting them for the next reload), `held` (`settings` as the project's part held back gives them,
/// the chat's review at the end aside — None: there's no such part).
#[derive(Clone, Debug)]
struct Reach {
    settings: Vec<Value>,
    read: Vec<PathBuf>,
    outside: bool,
    sockets: Vec<PathBuf>,
    hosts: Vec<(String, u16)>,
    terminals: bool,
    keys: Vec<PathBuf>,
    open_keys: Vec<PathBuf>,
    again: Vec<String>,
    sees: bool,
    unsandboxed: bool,
    caps: Caps,
    shown: Option<Caps>,
    listed: Vec<String>,
    trusted: bool,
    held: Option<Vec<Value>>,
}

impl Reach {
    /// The session in front's — `named`: the files named like keys to look at.
    fn of(ed: &Editor, named: &[PathBuf]) -> Reach {
        let basis = ed.caps_basis();
        let caps = basis.build();
        let w = crate::tools::with_allowed(&ed.config.llm, &caps, None);
        let mut hosts = w.hosts.clone();
        hosts.extend(basis.hosts.iter().filter(|h| !w.hosts.contains(h)).cloned());
        let rows = |cfg: &crate::config::EditorConfig| -> Vec<Value> {
            let llm = crate::tools::with_allowed(&cfg.llm, &caps, None).llm;
            let cfg = crate::config::EditorConfig { llm, ..cfg.clone() };
            SETTINGS.iter().map(|(p, _)| (crate::settings::find(p).expect(p).get)(&cfg)).collect()
        };
        let mut gated = ed.config.clone();
        gated.llm.review_at_end |= crate::edits::at_end(ed);
        let settings = rows(&gated);
        let held = ed.project_trust.as_ref().map(|p| {
            let mut cfg = p.without.0.clone();
            for (path, v) in &ed.overrides {
                let _ = crate::settings::apply(&mut cfg, path, v);
            }
            rows(&cfg)
        });
        let chat = ed.session.chat.as_ref();
        let mut listed: Vec<String> = caps.grants().iter().map(crate::granted::taken_as).collect();
        listed.extend(ed.session.hosts.iter().map(|(h, p)| crate::hosts::listed(h, *p)));
        let trusted = ed.project_trust.as_ref().is_some_and(|p| {
            p.state == State::Trusted
                || crate::trust::Store::read(ed.trust_path.as_deref()).state(&p.file, p.hash)
                    == State::Trusted
        });
        Reach {
            settings,
            read: caps.read.iter().map(|e| e.what.clone()).chain(w.reads).collect(),
            outside: caps.outside,
            sockets: w.sockets,
            hosts,
            terminals: w.terminals,
            keys: caps.keys.iter().map(|e| e.what.clone()).chain(w.keys).collect(),
            // (a key of the session's own project: another's is read outside it, asked on its own card)
            open_keys: (named.iter().filter(|k| k.starts_with(&ed.session.root)))
                .filter(|k| caps.keys.iter().any(|e| e.what == **k) || !crate::reads::key_shut(ed, k, &[]))
                .cloned()
                .collect(),
            again: chat.map(|c| c.run_again.iter().map(|r| format!("{r:?}")).collect()).unwrap_or_default(),
            sees: chat.is_some_and(|c| c.sees),
            unsandboxed: ed.session.no_sandbox == Some(crate::tools::NoSandbox::Unasked),
            shown: ed.session.caps().cloned(),
            caps,
            listed,
            trusted,
            held,
        }
    }

    fn setting(&self, path: &str) -> Option<&Value> {
        SETTINGS.iter().position(|(p, _)| *p == path).map(|i| &self.settings[i])
    }
}

/// What a step took back, that mustn't be there after it.
#[derive(Clone, Debug, PartialEq)]
enum Gone {
    /// An answer, as `:granted` lists it.
    Listed(String),
    /// The project's settings' trust: neither applied nor kept trusted, and the settings as held back.
    Trust,
    /// The no-sandbox card's `s`.
    Unsandboxed,
    /// A file named like a key, its text no longer going with a message.
    Key(PathBuf),
    /// A run's `a` (as `Reach.again` says it).
    Again(String),
    /// The screen card's `a`.
    Sees,
}

/// What a step may widen — for the session at the root it's given with (None: every session), its reach before
/// with this added (`plus`).
#[derive(Clone, Debug)]
enum Effect {
    /// A card's answer, as `tools::with_allowed` lets it (`the_card_allowance_is_what_with_allowed_adds`).
    Grant(Grant),
    Unsandboxed,
    Again(String),
    Sees,
    /// A checkout you made, read by default.
    Read(PathBuf),
    /// These `SETTINGS` rows as the session has them after (a project's settings a card trusted: the config is
    /// every session's).
    Rows(Vec<usize>),
}

/// `b` with `e` (`after`: the session after the step).
fn plus(b: &Reach, after: &Reach, e: &Effect) -> Reach {
    fn row<'a>(r: &'a mut Reach, path: &str) -> &'a mut Value {
        &mut r.settings[SETTINGS.iter().position(|(p, _)| *p == path).unwrap()]
    }
    fn push(r: &mut Reach, path: &str, v: Value) {
        if let Value::Array(a) = row(r, path) {
            a.push(v);
        }
    }
    let mut r = b.clone();
    match e {
        Effect::Grant(Grant::Write(p)) => {
            push(&mut r, "llm.sandbox-write", Value::from(p.display().to_string()))
        }
        Effect::Grant(Grant::Port(n)) => push(&mut r, "llm.sandbox-ports", Value::from(i64::from(*n))),
        Effect::Grant(Grant::Env(n)) => push(&mut r, "llm.sandbox-env", Value::from(n.clone())),
        Effect::Grant(Grant::Localhost) => *row(&mut r, "llm.sandbox-localhost") = Value::Boolean(true),
        Effect::Grant(Grant::Read(p)) | Effect::Read(p) => r.read.push(p.clone()),
        Effect::Grant(Grant::Socket(s)) => r.sockets.push(s.clone()),
        Effect::Grant(Grant::Host(h, p)) => {
            r.hosts.extend(crate::hosts::with_suggested(h, *p).into_iter().map(|h| (h, *p)))
        }
        Effect::Grant(Grant::Terminals) => r.terminals = true,
        Effect::Grant(Grant::Key(k)) => {
            r.keys.push(k.clone());
            r.open_keys.push(k.clone());
        }
        Effect::Unsandboxed => r.unsandboxed = true,
        Effect::Again(a) => r.again.push(a.clone()),
        Effect::Sees => r.sees = true,
        Effect::Rows(rows) => rows.iter().for_each(|i| r.settings[*i] = after.settings[*i].clone()),
    }
    r
}

/// `p` is `q` or inside it.
fn within(p: &Path, q: &Path) -> bool {
    p.starts_with(q) || crate::paths::inside(q, p).is_some()
}

/// The properties of one step for one session, as `wire/check.rs` keeps a reader's — Err says which broke:
/// - **no wider**: nothing in `after` that `before` doesn't cover (a new session's: a session tarae would start
///   there), nor `allowed` (what the step's card or your own act let, for this session), nor `yours` (the settings
///   as your own config alone says them — a project's list takes your list's place: held back again, yours is back);
/// - **taken back**: nothing `gone` names is in `after`;
/// - **never in it**: nothing read where runs never read (`Caps.kept`), no folder written in or holding one where a
///   write runs something later (`Caps.later`);
/// - **in step**: the `Caps` the session keeps is the one made now — a narrowing is in what `:granted` says at once.
fn check(
    before: &Reach,
    after: &Reach,
    allowed: &[Reach],
    gone: &[Gone],
    yours: &[Value],
) -> Result<(), String> {
    let more = wider(before, allowed, after, yours);
    if !more.is_empty() {
        return Err(format!("wider without a card: {}", more.join("; ")));
    }
    for g in gone {
        let still = match g {
            Gone::Listed(what) => after.listed.contains(what),
            Gone::Trust => after.trusted || after.held.as_ref().is_some_and(|h| *h != after.settings),
            Gone::Unsandboxed => after.unsandboxed,
            Gone::Key(k) => after.open_keys.contains(k),
            Gone::Again(a) => after.again.contains(a),
            Gone::Sees => after.sees,
        };
        if still {
            return Err(format!("taken back, still there: {g:?}"));
        }
    }
    let c = &after.caps;
    if let Some(e) = c.read.iter().find(|e| c.kept.iter().any(|k| within(&e.what, k))) {
        return Err(format!("read where runs never read: {}", e.what.display()));
    }
    let later: Vec<&Path> = (c.later.bins.iter().map(PathBuf::as_path))
        .chain(c.later.caches.iter().map(|x| x.at.as_path()))
        .chain(c.later.homes.iter().map(PathBuf::as_path))
        .collect();
    if let Some(e) = c.write.iter().find(|e| later.iter().any(|l| within(&e.what, l) || within(l, &e.what))) {
        return Err(format!("written where it runs later: {}", e.what.display()));
    }
    if after.shown.as_ref().is_some_and(|s| *s != after.caps) {
        return Err(format!(
            "the session's Caps isn't the one made now: {:?} — now {:?}",
            after.shown, after.caps
        ));
    }
    Ok(())
}

/// What `a` has that neither `b`, nor one of `allowed`, nor `yours` (its settings) covers, each said.
fn wider(b: &Reach, allowed: &[Reach], a: &Reach, yours: &[Value]) -> Vec<String> {
    let all: Vec<&Reach> = std::iter::once(b).chain(allowed).collect();
    let any = |f: &dyn Fn(&Reach) -> bool| all.iter().any(|r| f(r));
    let mut out = Vec::new();
    let d = |p: &PathBuf| p.display().to_string();
    let read = |p: &PathBuf| any(&|r| r.read.iter().any(|q| within(p, q)));
    out.extend(a.read.iter().filter(|p| !read(p)).map(|p| format!("read {}", d(p))));
    out.extend((a.outside && !any(&|r| r.outside)).then(|| "outside home read".to_string()));
    let socket = |s: &PathBuf| any(&|r| r.sockets.contains(s));
    out.extend(a.sockets.iter().filter(|s| !socket(s)).map(|s| format!("socket {}", d(s))));
    let host = |h: &(String, u16)| any(&|r| r.hosts.contains(h));
    out.extend(a.hosts.iter().filter(|h| !host(h)).map(|(h, p)| format!("host {h}:{p}")));
    out.extend((a.terminals && !any(&|r| r.terminals)).then(|| "terminals".to_string()));
    let key = |k: &PathBuf| any(&|r| r.keys.contains(k));
    out.extend(a.keys.iter().filter(|k| !key(k)).map(|k| format!("key {}", d(k))));
    let open = |k: &PathBuf| any(&|r| r.open_keys.contains(k));
    out.extend(a.open_keys.iter().filter(|k| !open(k)).map(|k| format!("the text of {}", d(k))));
    let again = |x: &String| any(&|r| r.again.contains(x));
    out.extend(a.again.iter().filter(|x| !again(x)).map(|x| format!("runs again: {x}")));
    out.extend((a.sees && !any(&|r| r.sees)).then(|| "sees the screen".to_string()));
    out.extend((a.unsandboxed && !any(&|r| r.unsandboxed)).then(|| "no sandbox, unasked".to_string()));
    // (every port is open where localhost is)
    let on = Some(&Value::Boolean(true));
    let localhost = any(&|r| r.setting("llm.sandbox-localhost") == on)
        || SETTINGS
            .iter()
            .position(|(p, _)| *p == "llm.sandbox-localhost")
            .is_some_and(|i| yours[i] == on.cloned().unwrap());
    for (i, (path, way)) in SETTINGS.iter().enumerate() {
        let bases: Vec<&Value> = all.iter().map(|r| &r.settings[i]).chain([&yours[i]]).collect();
        if !(*path == "llm.sandbox-ports" && localhost)
            && let Some(w) = value_wider(*way, &bases, &a.settings[i])
        {
            out.push(format!("{path} {w}"));
        }
    }
    out
}

/// Whether setting value `a` is wider than every one of `bases`, said: a switch on (`Opens`) where none is, off
/// (`Guards`) where all are on; an entry none of them covers (`Opens`), or one all of them guard missing.
fn value_wider(way: Way, bases: &[&Value], a: &Value) -> Option<String> {
    let any = |v: &Value| matches!(v.as_str(), Some("*" | "any"));
    let covers = |y: &Value, x: &Value| {
        y == x
            || any(y)
            || matches!((y.as_str(), x.as_str()), (Some(y), Some(x)) if y.starts_with('/')
                && within(Path::new(x), Path::new(y)))
    };
    let items = |v: &Value| v.as_array().cloned().unwrap_or_else(|| vec![v.clone()]);
    let wide = match (way, a) {
        _ if bases.contains(&a) => false,
        (Way::Opens, _) if bases.iter().any(|b| any(b)) => false,
        (Way::Opens, Value::Boolean(a)) => *a && !bases.iter().any(|b| b.as_bool() == Some(true)),
        (Way::Opens, Value::Array(a)) => {
            a.iter().any(|x| !bases.iter().any(|b| items(b).iter().any(|y| covers(y, x))))
        }
        (Way::Guards, Value::Boolean(a)) => !*a && bases.iter().all(|b| b.as_bool() == Some(true)),
        (Way::Guards, Value::Array(a)) => {
            let first = items(bases[0]);
            first.iter().any(|y| bases.iter().all(|b| items(b).contains(y)) && !a.contains(y))
        }
        _ => true,
    };
    wide.then(|| format!("{} → {a}", bases[0]))
}

// ── The world the steps run in ──────────────────────────────────────────────────────────────────────────────

/// Your config file's `[llm]` past the stand-in claude, one of these.
const YOURS: [&str; 3] = ["", "sandbox-ports = [6379]\n", "sandbox-localhost = true\n"];

/// The project's `.tarae.toml`, one of these (None: there's none) — risky ones held back until trusted.
const PROJECTS: [Option<&str>; 4] = [
    None,
    Some("[llm]\nsandbox-ports = [5432]\n"),
    Some("[llm]\nsandbox-network = true\nsandbox-hosts = [\"*\"]\nreview-at-end = true\n"),
    Some("[editor]\nscrolloff = 3\n"),
];

/// A project in a scratch folder, a repository's checkout (an empty repository git reads: `.git` a folder), with
/// the state an editor keeps there: your config file, the trust answers, what's kept for the project, the sessions
/// — and `broken`, `broken_dir`, a store and a folder no write reaches (under a file: as root too). Home is a
/// stand-in holding what runs never read (`.ssh`, `.claude`, tarae's state).
struct World {
    _dir: crate::scratch::Scratch,
    at: PathBuf,
    root: PathBuf,
    home: PathBuf,
    user: PathBuf,
    trust: PathBuf,
    allow: PathBuf,
    sessions: PathBuf,
    broken: PathBuf,
    broken_dir: PathBuf,
    /// What the cards in the steps answer about.
    grants: Vec<Grant>,
    /// What the sandbox denied that a card offers choices for (`allow::read_choices`, `write_choices`) — a write
    /// when true: kept places, a folder of programs, places a card may open.
    denied: Vec<(PathBuf, bool)>,
    /// The files named like keys `Reach` looks at.
    named: Vec<PathBuf>,
    /// Sessions and checkouts made so far.
    made: usize,
    /// The loop's: its stores' writes held, each landing at a step that says so (`Step::Land`).
    held: bool,
}

thread_local! {
    /// The stores this thread's worlds write — `settle` counts the writes queued there whose result hasn't come.
    static STORES_HERE: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The writes queued at this thread's worlds' stores and not landed.
fn queued_here() -> u64 {
    let at = |p: &PathBuf| crate::storewrite::given_turns(p) - crate::storewrite::served(p);
    STORES_HERE.with(|s| s.borrow().iter().map(at).sum())
}

impl Drop for World {
    fn drop(&mut self) {
        let mine = [&self.trust, &self.allow, &self.broken];
        STORES_HERE.with(|s| s.borrow_mut().retain(|p| !mine.contains(&p)));
        mine.iter().for_each(|p| crate::storewrite::cut(p));
    }
}

/// An empty repository git reads at `dir`.
fn repository(dir: &Path) {
    for d in [".git/objects", ".git/refs/heads"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
}

impl World {
    fn new(name: &str) -> World {
        let dir = crate::scratch::Scratch::new(name);
        let at = std::fs::canonicalize(&dir).unwrap();
        let root = at.join("main");
        repository(&root);
        for d in [
            "home/.ssh",
            "home/.claude",
            "home/notes",
            "home/.local/state/tarae",
            "state",
            "out/a/sub",
            "out/b",
            "out/w",
        ] {
            std::fs::create_dir_all(at.join(d)).unwrap();
        }
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(at.join("file"), "").unwrap();
        std::fs::write(at.join("home/notes/todo.txt"), "").unwrap();
        let home = at.join("home");
        // (a folder of programs on PATH, deep enough for a card to offer it were it not refused)
        let bin = std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).find(|d| d.components().count() > 3 && d.is_dir()));
        let mut denied = vec![
            (home.join(".ssh/id_tarae"), false),
            (home.join(".claude/settings.json"), false),
            (home.join(".local/state/tarae/x"), false),
            (home.join("notes/todo.txt"), false),
            (at.join("out/w/f"), true),
        ];
        denied.extend(bin.map(|b| (b.join("tarae-none"), true)));
        let out = at.join("out");
        let grants = vec![
            Grant::Read(out.join("a")),
            Grant::Read(out.join("a/sub")),
            Grant::Read(out.join("b")),
            Grant::Write(out.join("w")),
            Grant::Port(5432),
            Grant::Port(8080),
            Grant::Socket(out.join("s.sock")),
            Grant::Host("example.com".into(), 443),
            Grant::Host("example.org".into(), 8443),
            Grant::Terminals,
            Grant::Localhost,
            Grant::Env("TARAE_CAPS_VAR".into()),
            Grant::Env("TARAE_CAPS_TOKEN".into()),
            Grant::Key(root.join(".env")),
        ];
        let w = World {
            user: at.join("config.toml"),
            trust: at.join("state/trust.json"),
            allow: at.join("state/sandbox.json"),
            sessions: at.join("state/sessions"),
            broken: at.join("file/sandbox.json"),
            broken_dir: at.join("file/sessions"),
            named: vec![root.join(".env")],
            home,
            root,
            at,
            _dir: dir,
            grants,
            denied,
            made: 0,
            held: false,
        };
        STORES_HERE.with(|s| s.borrow_mut().extend([w.trust.clone(), w.allow.clone(), w.broken.clone()]));
        w.yours(0);
        w
    }

    /// Its stores' writes held from now on, each landing at a `Step::Land`.
    fn hold(&mut self) {
        self.held = true;
        [&self.trust, &self.allow].iter().for_each(|p| crate::storewrite::hold(p, Some(0)));
    }

    fn sources(&self) -> crate::config::Sources {
        crate::config::Sources { user: Some(self.user.clone()), trust: Some(self.trust.clone()) }
    }

    /// An editor started in the project, on this state — the kept sessions not back yet (`sessions_restore`).
    fn editor(&self) -> Editor {
        self.editor_at(&self.root)
    }

    /// An editor started on this state with `root` its checkout (a session tarae would start there).
    fn editor_at(&self, root: &Path) -> Editor {
        let (config, _) = crate::config::load(&self.root, &self.sources());
        let mut ed = Editor::with_root(config, root.to_path_buf());
        ed.allow_path = Some(self.allow.clone());
        ed.trust_path = Some(self.trust.clone());
        ed.caps_home = Some(self.home.clone());
        ed.session_store.dir = Some(self.sessions.clone());
        ed
    }

    /// A fresh tarae: this world's take-backs of trust no longer marked (`trust::TAKING` is the process's), its
    /// stores' writes not landed cut off (the process ended) and what it saw there forgotten — held again.
    fn new_process(&self) {
        crate::trust::untake_all(&crate::trust::place_of(&self.root.join(".tarae.toml")));
        for p in [&self.trust, &self.allow, &self.broken] {
            crate::storewrite::cut(p);
        }
        if self.held {
            [&self.trust, &self.allow].iter().for_each(|p| crate::storewrite::hold(p, Some(0)));
        }
    }

    /// Every write held at its stores landed, and their results in.
    fn land_all(&self, ed: &mut Editor, s: &str) {
        [&self.trust, &self.allow].iter().for_each(|p| crate::storewrite::release(p, u64::MAX / 2));
        settle(ed, s, ed.messages.len(), &|_, _| queued_here() == 0);
        if self.held {
            [&self.trust, &self.allow].iter().for_each(|p| crate::storewrite::hold(p, Some(0)));
        }
    }

    /// Your config file says `YOURS[i]`.
    fn yours(&self, i: usize) {
        let claude = "command = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n";
        std::fs::write(&self.user, format!("[llm]\n{claude}{}", YOURS[i])).unwrap();
    }

    /// The project's file says `PROJECTS[i]`.
    fn project(&self, i: usize) {
        let file = self.root.join(".tarae.toml");
        match PROJECTS[i] {
            Some(text) => std::fs::write(file, text).unwrap(),
            None => _ = std::fs::remove_file(file),
        }
    }

    /// `SETTINGS` as your config file alone says them.
    fn yours_said(&self) -> Vec<Value> {
        let (c, _) = crate::config::load_layers(Some(self.user.clone()), None, None, &Default::default());
        SETTINGS.iter().map(|(p, _)| (crate::settings::find(p).expect(p).get)(&c.editor)).collect()
    }

    /// The config read again, as the watch reads it when a file changes.
    fn reload(&self, ed: &mut Editor) {
        let (config, warnings) = crate::config::load(&self.root, &self.sources());
        ed.reload_config(config, warnings);
    }
}

/// The events applied until every session's `Caps` asked has landed and `done` says so (`said`: the messages since
/// `from`).
fn settle(ed: &mut Editor, what: &str, from: usize, done: &dyn Fn(&Editor, &[String]) -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let said: Vec<String> = ed.messages.iter().skip(from).map(|m| m.2.clone()).collect();
        // (and the result of every store write that landed: those held are the steps' to land — a wait met by
        // another write's result put it in a later step)
        let landed = (ed.allow_writing + ed.trust_writing) as u64 == queued_here()
            && std::iter::once(&ed.session)
                .chain(&ed.sessions)
                .all(|s| s.caps_asked == 0 || s.caps.as_ref().is_some_and(|(g, _)| *g == s.caps_asked));
        if landed && done(ed, &said) {
            return;
        }
        assert!(Instant::now() < end, "{what}: never settled — said {said:?}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

fn landed(_: &Editor, _: &[String]) -> bool {
    true
}

/// Every session's `Reach`, by its checkout.
fn reaches(ed: &mut Editor, named: &[PathBuf]) -> Vec<(PathBuf, Reach)> {
    let ids: Vec<_> =
        std::iter::once(&ed.session).chain(&ed.sessions).map(|s| (s.id, s.root.clone())).collect();
    let mut out = Vec::new();
    for (id, root) in ids {
        let mut r = None;
        ed.with_session(id, |ed| r = Some(Reach::of(ed, named)));
        out.push((root, r.unwrap()));
    }
    out
}

// ── The steps ─────────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Step {
    /// A card's `a`/`s`/`A` about `grants[i]`: a read card's (`grant_reads`, `grant_keys`), a host's
    /// (`hosts::granted`), what the sandbox blocked (`allow::grant`).
    Card(usize, Scope),
    /// What the sandbox denied at `denied[i]`: the widest place its card offers (`allow::read_choices`,
    /// `write_choices` — never what runs never read, nor where a write runs something later) answered.
    Denied(usize, Scope),
    /// A file named like a key the project keeps open (`sandbox.json` as an earlier answer left it: a key's
    /// lasts once now, `Caps::storable`), read into the chat's copy.
    KeyKept,
    /// `a` on a run's card: `docker ps` with Docker's socket, as `tools::run_answer` keeps it.
    RunAgain,
    /// `a` on the screen card.
    Sees,
    /// The project trust card's `y` / `n`.
    Trust(bool),
    /// The no-sandbox card's `s`.
    Unsandboxed,
    /// Your config file says `YOURS[i]`, reloaded.
    Yours(usize),
    /// The project's `.tarae.toml` says `PROJECTS[i]`, reloaded — someone else's file.
    Project(usize),
    ChatOpen,
    ChatReset,
    ChatClose,
    /// `C-p`: another conversation continued.
    ChatResume,
    /// `:granted`'s `d` (`:sandbox-disallow`) on one the session's answers — `failing`: which store's write fails.
    TakeBack {
        pick: usize,
        failing: Failing,
    },
    /// `:project-untrust` — `failing`: the store's write fails.
    Untrust {
        failing: bool,
    },
    /// `:granted`'s `d` on the project's trust answer — `failing`: the store's write fails.
    TrustTaken {
        failing: bool,
    },
    /// `:granted`'s `d` on the no-sandbox `s`.
    Sandboxed,
    SessionAdd,
    SessionSwitch(usize),
    /// Quit, and tarae started again on the same state (`sessions_restore`) — `early`: one of the start
    /// session's kept answers taken back before they're back (before then only it is there); `cut`: the quit
    /// gave up waiting, the writes held cut off (said on stderr — what they took back may be back).
    Restart {
        early: Option<usize>,
        cut: bool,
    },
    /// The next `n` writes held at a store land (`trust`: `trust.json`'s, else `sandbox.json`'s) — `fail`: the
    /// first of them fails.
    Land {
        trust: bool,
        n: usize,
        fail: bool,
    },
    /// Another tarae's write lands now: a change of its own (nothing of it this tarae's).
    Other {
        op: OtherOp,
    },
    /// `:restart`: tarae again, its state carried (`restart::snapshot` → `restore`) — "until you quit" holds.
    ReExec,
    /// Another checkout of the repository: made as `git worktree add` makes it (yours), or one way only (planted).
    Checkout {
        linked: bool,
    },
}

/// What another tarae writes (`Step::Other`).
#[derive(Clone, Copy, Debug)]
enum OtherOp {
    /// A yes or a no on the project's file — `current`: its version here, else another.
    Trust { yes: bool, current: bool },
    /// The project's file's trust taken back.
    TrustTaken,
    /// `grants[i]` kept for the project.
    Keep(usize),
    /// `grants[i]` taken back from what the project keeps.
    Take(usize),
}

/// Which store's write a take-back meets failing: none, `sandbox.json`, the sessions'.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Failing {
    No,
    Store,
    Sessions,
}

impl Step {
    fn random(rng: &mut crate::mutate::Rng, w: &World) -> Step {
        let coin = |rng: &mut crate::mutate::Rng| rng.below(2) == 0;
        let scope =
            |rng: &mut crate::mutate::Rng| *rng.pick(&[Scope::ThisChat, Scope::Session, Scope::Always]);
        match rng.below(40) {
            30..=35 => {
                let (trust, n, fail) = (coin(rng), 1 + rng.below(3), rng.below(5) == 0);
                Step::Land { trust, n, fail }
            }
            36..=38 => {
                let op = match rng.below(5) {
                    0 => OtherOp::Trust { yes: true, current: coin(rng) },
                    1 => OtherOp::Trust { yes: false, current: coin(rng) },
                    2 => OtherOp::TrustTaken,
                    3 => OtherOp::Keep(rng.below(w.grants.len())),
                    _ => OtherOp::Take(rng.below(w.grants.len())),
                };
                Step::Other { op }
            }
            39 => Step::Restart { early: None, cut: true },
            0..=5 | 28 => Step::Card(rng.below(w.grants.len()), scope(rng)),
            6 => Step::Denied(rng.below(w.denied.len()), scope(rng)),
            7 => Step::KeyKept,
            8 if coin(rng) => Step::RunAgain,
            8 => Step::Sees,
            9 | 26 => Step::Trust(rng.below(3) != 0),
            10 if coin(rng) => Step::Unsandboxed,
            10 => Step::Sandboxed,
            11 => Step::Yours(rng.below(YOURS.len())),
            12 => Step::Project(rng.below(PROJECTS.len())),
            13 | 14 | 27 => Step::ChatOpen,
            15 if coin(rng) => Step::ChatReset,
            15 => Step::ChatClose,
            16 => Step::ChatResume,
            17 | 18 | 29 => {
                let failing = *rng.pick(&[Failing::Store, Failing::Sessions, Failing::No, Failing::No]);
                Step::TakeBack { pick: rng.below(64), failing }
            }
            19 if coin(rng) => Step::Untrust { failing: coin(rng) },
            19 => Step::TrustTaken { failing: coin(rng) },
            20 if coin(rng) => Step::SessionAdd,
            20 => Step::SessionSwitch(rng.below(4)),
            21 => Step::Restart { early: coin(rng).then(|| rng.below(64)), cut: false },
            22 => Step::ReExec,
            23 => Step::Checkout { linked: coin(rng) },
            _ => Step::Project(1 + rng.below(2)),
        }
    }
}

/// What the loop carries between steps: the project versions a card trusted (a reload of one may widen by that
/// answer), and what was taken back "until you quit", its store's write failed — at the next start it may be back,
/// as said, but no wider than its session was then; the project's trusted settings likewise (their rows).
#[derive(Default)]
struct Ledger {
    trusted: Vec<crate::trust::Digest>,
    /// What failed take-backs took (a checkout's answers they named) — said to come back at the next start.
    failed: Vec<(PathBuf, Vec<Grant>)>,
    /// The versions a failed trust take-back named — trusted again at the next start, as the store still has them.
    failed_versions: Vec<(PathBuf, crate::trust::Digest)>,
    /// Conversations continued so far (`ChatResume`).
    conversations: usize,
    /// The versions another tarae trusted — its card: a reload of one may widen by it.
    others_trusted: Vec<crate::trust::Digest>,
    /// What another tarae kept for the project — its card: from then on any `Caps` made may have it.
    others_kept: Vec<Grant>,
    /// What another tarae took back from what the project keeps, with when (our `given`): an answer of ours
    /// given before it that lands after it isn't kept.
    others_took: Vec<(String, u64)>,
    /// The answers kept for the project a card gave, as the writes carry them (`Grant::shown`).
    answered: Vec<Grant>,
    /// When another tarae said no or took back the project's trust (our `given`): a yes of ours given before it
    /// that lands after it isn't written — held back again here.
    others_untrusted: Vec<u64>,
    /// The given of our last yes on the project's settings.
    last_yes: u64,
}

/// What a step did, for `check`: what it may widen (each for the session at a root — None: every one), what it
/// took back (in the session at `gone_at`, else the one in front before it).
#[derive(Default)]
struct Done {
    allowed: Vec<(Option<PathBuf>, Effect)>,
    gone: Vec<Gone>,
    gone_at: Option<PathBuf>,
}

fn narrows(gone: Vec<Gone>) -> Done {
    Done { gone, ..Done::default() }
}

fn widens(at: Option<PathBuf>, e: Effect) -> Done {
    Done { allowed: vec![(at, e)], ..Done::default() }
}

/// The rows of `SETTINGS` the project's held-back part names.
fn project_rows(ed: &Editor) -> Vec<usize> {
    let Some(p) = &ed.project_trust else { return Vec::new() };
    let named = |path: &str| p.items.iter().any(|(k, _)| k == path);
    SETTINGS.iter().enumerate().filter(|(_, (path, _))| named(path)).map(|(i, _)| i).collect()
}

/// `grants[i]` answered on a card for `scope`, settled.
fn answer(ed: &mut Editor, g: &Grant, scope: Scope, read_card: bool, s: &str) {
    let from = ed.messages.len();
    match g {
        Grant::Host(h, p) => crate::hosts::granted(ed, h, *p, scope),
        Grant::Key(k) => crate::allow::grant_keys(ed, std::slice::from_ref(k), scope),
        Grant::Read(p) if read_card => crate::allow::grant_reads(ed, std::slice::from_ref(p), scope),
        g => crate::allow::grant(ed, g, scope),
    }
    // (kept for the project: written on a worker, the `Caps` made again after — unless the loop holds it)
    let asked = ed.session.caps_asked;
    let held = ed.allow_path.as_deref().is_some_and(crate::storewrite::is_held);
    let kept = !held && !matches!(g, Grant::Host(..)) && scope.min(Caps::storable(g)) == Scope::Always;
    settle(ed, s, from, &|ed, _| !kept || ed.session.caps_asked > asked);
}

/// `step` applied, and settled.
fn apply(w: &mut World, ed: &mut Editor, step: &Step, ledger: &mut Ledger, front: &Reach) -> Done {
    let from = ed.messages.len();
    let s = format!("{step:?}");
    let root = ed.session.root.clone();
    match step {
        Step::Card(i, scope) => {
            let g = w.grants[*i].clone();
            answer(ed, &g, *scope, *i % 2 == 0, &s);
            ledger.answered.push(g.clone());
            widens(Some(root), Effect::Grant(g))
        }
        Step::Denied(i, scope) => {
            let (target, write) = w.denied[*i].clone();
            let caps = ed.caps_basis().build();
            let home = caps.home.clone().unwrap();
            let widest = match write {
                true => crate::allow::write_choices(&target, &home, &caps.root, &caps.later)
                    .pop()
                    .map(Grant::Write),
                false => crate::allow::read_choices(&target, &home, &caps.kept).pop().map(Grant::Read),
            };
            let Some(g) = widest else { return narrows(Vec::new()) };
            answer(ed, &g, *scope, !write, &s);
            ledger.answered.push(g.clone());
            widens(Some(root), Effect::Grant(g))
        }
        Step::KeyKept => {
            let key = w.named[0].clone();
            if let Some(c) = ed.session.chat.as_mut() {
                c.kept_keys = None;
            }
            let asked = ed.session.caps_asked;
            // (in the store as an earlier answer left it, then read as a kept answer's write is)
            let (store, project) = (ed.allow_path.clone().unwrap(), crate::sandbox::profile_path(&root));
            crate::allow::keep(&store, &project, |a| a.add(&Grant::Key(key.clone()))).unwrap();
            crate::reads::kept_keys_load(ed);
            ed.caps_sync();
            settle(ed, &s, from, &|ed, _| {
                ed.session.caps_asked > asked
                    && ed.session.chat.as_ref().is_none_or(|c| c.kept_keys.is_some())
            });
            widens(Some(root), Effect::Grant(Grant::Key(key)))
        }
        Step::RunAgain => {
            let Some(c) = ed.session.chat.as_mut() else { return narrows(Vec::new()) };
            let parts = vec![vec!["docker".to_string(), "ps".to_string()]];
            let again = crate::tools::Again { parts, scripts: Vec::new() };
            let r =
                crate::tools::RunAgain { cwd: root.clone(), socket: Some(w.at.join("docker.sock")), again };
            let said = format!("{r:?}");
            if !c.run_again.contains(&r) {
                c.run_again.push(r);
            }
            widens(Some(root), Effect::Again(said))
        }
        Step::Sees => {
            let Some(c) = ed.session.chat.as_mut() else { return narrows(Vec::new()) };
            c.sees = true;
            widens(Some(root), Effect::Sees)
        }
        Step::Trust(yes) => {
            let Some(p) = ed.project_trust.clone().filter(|p| p.state != State::Trusted) else {
                return narrows(Vec::new());
            };
            ed.project_trust_answered(p.hash, *yes);
            // (its given, from its own write — held: not `storewrite::last()`, which another seed's thread raises — F9)
            let given =
                crate::storewrite::pending(&w.trust).last().and_then(|p| p.carry[0]["given"].as_u64());
            let want = if *yes { State::Trusted } else { State::Ignored };
            let (trust, held) = (w.trust.clone(), w.held);
            settle(ed, &s, from, &|_, _| {
                held || crate::trust::Store::read(Some(&trust)).state(&p.file, p.hash) == want
            });
            ledger.trusted.retain(|h| *h != p.hash);
            if !*yes {
                return narrows(vec![Gone::Trust]);
            }
            // (a yes writes a failed take-back with it: what it took stays taken — `trust::write_over`)
            ledger.failed_versions.clear();
            ledger.trusted.push(p.hash);
            ledger.last_yes = given.unwrap_or(0);
            widens(None, Effect::Rows(project_rows(ed)))
        }
        Step::Unsandboxed => {
            // (the card's `s`, as `tools.rs` keeps it)
            ed.session.no_sandbox = Some(crate::tools::NoSandbox::Unasked);
            widens(Some(root), Effect::Unsandboxed)
        }
        Step::Sandboxed => {
            // (`:granted`'s `d` on it, as `granted_take` takes it)
            ed.session.no_sandbox = None;
            narrows(vec![Gone::Unsandboxed])
        }
        Step::Yours(i) => {
            // (what it widens is in `yours`, read again before the check)
            w.yours(*i);
            w.reload(ed);
            narrows(Vec::new())
        }
        Step::Project(i) => {
            w.project(*i);
            w.reload(ed);
            // (a version a card trusted comes back with that answer — another tarae's card too)
            let carded =
                |h: &crate::trust::Digest| ledger.trusted.contains(h) || ledger.others_trusted.contains(h);
            match ed.project_trust.as_ref().is_some_and(|p| carded(&p.hash)) {
                true => widens(None, Effect::Rows(project_rows(ed))),
                false => narrows(Vec::new()),
            }
        }
        Step::ChatOpen => {
            crate::chat::open(ed);
            settle(ed, &s, from, &landed);
            narrows(Vec::new())
        }
        Step::ChatReset | Step::ChatClose | Step::ChatResume => {
            if ed.session.chat.is_none() {
                return narrows(Vec::new());
            }
            let gone = chat_only(ed);
            match step {
                Step::ChatReset => crate::chat::reset(ed),
                Step::ChatClose => crate::chat::close(ed),
                _ => {
                    ledger.conversations += 1;
                    let id = format!("conversation-{}", ledger.conversations);
                    let (rows, plan, usage) = (Vec::new(), None, None);
                    let saved = crate::chats::Saved {
                        id,
                        title: "earlier".into(),
                        updated: 0,
                        rows,
                        plan,
                        usage,
                        seq: 0,
                    };
                    crate::chat::continue_with(ed, saved);
                }
            }
            settle(ed, &s, from, &landed);
            narrows(gone)
        }
        Step::TakeBack { pick, failing } => {
            let Some(what) =
                (!front.listed.is_empty()).then(|| front.listed[pick % front.listed.len()].clone())
            else {
                return narrows(Vec::new());
            };
            take_back(w, ed, &what, *failing, &s);
            if *failing != Failing::No {
                let took = front.caps.grants().into_iter().filter(|g| crate::allow::takes_back(&what, g));
                ledger.failed.push((root.clone(), took.collect()));
            }
            let key = |k: &&PathBuf| {
                k.starts_with(&root) && crate::allow::takes_back(&what, &Grant::Key(k.to_path_buf()))
            };
            let mut gone: Vec<Gone> = w.named.iter().filter(key).cloned().map(Gone::Key).collect();
            gone.push(Gone::Listed(what));
            narrows(gone)
        }
        Step::Untrust { failing } => {
            let Some(p) = ed.project_trust.clone().filter(|p| p.state == State::Trusted) else {
                return narrows(Vec::new());
            };
            if *failing {
                ed.trust_path = Some(w.broken.clone());
                ledger.failed_versions.push((p.file.clone(), p.hash));
            }
            ed.project_untrust().unwrap();
            let (trust, held) = (w.trust.clone(), w.held);
            settle(ed, &s, from, &|_, said| match failing {
                true => said.iter().any(|m| m.contains("held back until you quit")),
                false => {
                    held || crate::trust::Store::read(Some(&trust)).state(&p.file, p.hash) == State::Ignored
                }
            });
            ed.trust_path = Some(w.trust.clone());
            ledger.trusted.retain(|h| *h != p.hash);
            narrows(vec![Gone::Trust])
        }
        Step::TrustTaken { failing } => {
            if !ed.project_trust.as_ref().is_some_and(|p| p.state == State::Trusted) {
                return narrows(Vec::new());
            }
            if *failing {
                let file = ed.project_trust.as_ref().map(|p| p.file.clone()).unwrap();
                ledger.failed_versions.extend(ledger.trusted.iter().map(|h| (file.clone(), *h)));
            }
            trust_take(w, ed, *failing, &s);
            // (every trusted version of the file forgotten — or held back until you quit)
            ledger.trusted.clear();
            narrows(vec![Gone::Trust])
        }
        Step::SessionAdd => {
            w.made += 1;
            let root = w.at.join(format!("s{}", w.made));
            repository(&root);
            ed.session_add(root);
            settle(ed, &s, from, &landed);
            narrows(Vec::new())
        }
        Step::SessionSwitch(i) => {
            let ids: Vec<_> = std::iter::once(&ed.session).chain(&ed.sessions).map(|s| s.id).collect();
            ed.session_switch(ids[i % ids.len()]);
            settle(ed, &s, from, &landed);
            narrows(Vec::new())
        }
        Step::Restart { early, cut } => restart(w, ed, *early, *cut, ledger, &s),
        Step::ReExec => {
            // (the exec cuts off what's held: carried, and given again by the next tarae)
            let v = crate::restart::snapshot(ed);
            w.new_process();
            // (as `main` starts it: what's carried marked before the config reads the store)
            crate::restart::before_config(&v);
            *ed = w.editor();
            crate::restart::restore(ed, &v);
            settle(ed, &s, 0, &landed);
            // (the next tarae reads the store as it starts: another tarae's yes there — that one's card — applies)
            match ed.project_trust.as_ref().is_some_and(|p| ledger.others_trusted.contains(&p.hash)) {
                true => widens(None, Effect::Rows(project_rows(ed))),
                false => narrows(Vec::new()),
            }
        }
        Step::Land { trust, n, fail } => land(w, ed, *trust, *n, *fail, ledger, &s),
        Step::Other { op } => other(w, ed, *op, ledger, &s),
        Step::Checkout { linked } => {
            w.made += 1;
            let co = w.at.join(format!("co{}", w.made));
            let wt = w.root.join(format!(".git/worktrees/co{}", w.made));
            std::fs::create_dir_all(&co).unwrap();
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::write(wt.join("gitdir"), format!("{}\n", co.join(".git").display())).unwrap();
            std::fs::write(wt.join("commondir"), "../..\n").unwrap();
            // (planted: its `.git` names another folder — git's links go one way)
            let named = if *linked { wt } else { w.at.join("elsewhere") };
            std::fs::write(co.join(".git"), format!("gitdir: {}\n", named.display())).unwrap();
            // (the sessions' `Caps` made again, as the next answer would)
            let ids: Vec<_> = std::iter::once(&ed.session).chain(&ed.sessions).map(|s| s.id).collect();
            ids.into_iter().for_each(|id| ed.caps_sync_of(id));
            settle(ed, &s, from, &landed);
            match linked {
                true => widens(Some(w.root.clone()), Effect::Read(co)),
                false => narrows(Vec::new()),
            }
        }
    }
}

/// What the session's conversation alone lets, as `:granted` lists it, and its `a` on a run's card and on the
/// screen card — gone with the conversation.
fn chat_only(ed: &Editor) -> Vec<Gone> {
    let b = ed.caps_basis();
    let none = Allowed::default();
    let rest = Caps::granted(&b.root, &b.kept().1.all, &b.session, &none).grants();
    let hosts: Vec<String> = ed.session.hosts.iter().map(|(h, p)| crate::hosts::listed(h, *p)).collect();
    let chat = Caps::granted(&b.root, &none, &none, &b.chat).grants();
    let chat = chat.iter().filter(|g| !rest.contains(g)).map(crate::granted::taken_as);
    let mut gone: Vec<Gone> = chat.filter(|g| !hosts.contains(g)).map(Gone::Listed).collect();
    if let Some(c) = &ed.session.chat {
        gone.extend(c.run_again.iter().map(|r| Gone::Again(format!("{r:?}"))));
        gone.extend(c.sees.then_some(Gone::Sees));
    }
    gone
}

/// `:sandbox-disallow what`, settled — `failing`: with that store's write failing (then the store put back and
/// the `Caps` made again, as the next answer would).
fn take_back(w: &World, ed: &mut Editor, what: &str, failing: Failing, s: &str) {
    let root = ed.session.root.clone();
    let (from, taking) = (ed.messages.len(), ed.taking_for(&root).len());
    let had = ed.session.allowed.clone();
    match failing {
        Failing::Store => ed.allow_path = Some(w.broken.clone()),
        Failing::Sessions => ed.session_store.dir = Some(w.broken_dir.clone()),
        Failing::No => {}
    }
    ed.sandbox_disallow(what).unwrap();
    // (the sessions' store is written only once they're back, and only when the session's answers changed)
    let sessions = failing == Failing::Sessions && ed.session_store.restored && ed.session.allowed != had;
    settle(ed, s, from, &|ed, said| match failing {
        Failing::Store => said.iter().any(|m| m.contains("taken back until you quit")),
        Failing::Sessions if sessions => said.iter().any(|m| m.contains("couldn't write the session")),
        // (one that failed before stays taken back: `allow_taking` keeps it; one held lands at its step)
        _ => w.held || ed.taking_for(&root).len() <= taking,
    });
    ed.allow_path = Some(w.allow.clone());
    ed.session_store.dir = Some(w.sessions.clone());
    ed.caps_sync();
    settle(ed, s, from, &landed);
}

/// `:granted`'s `d` on the project's trust answer, settled — `failing`: with the store's write failing (then the
/// store put back).
fn trust_take(w: &World, ed: &mut Editor, failing: bool, s: &str) {
    let from = ed.messages.len();
    let place = crate::trust::place_of(&ed.project_trust.as_ref().unwrap().file);
    if failing {
        ed.trust_path = Some(w.broken.clone());
    }
    ed.trust_take(Kind::Tarae, place.clone());
    settle(ed, s, from, &|_, said| {
        let held = said.iter().any(|m| m.contains("the project's settings are held back"));
        let written = match failing {
            true => said.iter().any(|m| m.contains("held back until you quit")),
            false => w.held || !crate::trust::is_taking(&place),
        };
        held && written
    });
    ed.trust_path = Some(w.trust.clone());
}

/// Quit, and tarae started again on this state: a new process (no take-back of trust held), the kept sessions back
/// from their store (`sessions_restore`) — `early`: one of the start session's answers taken back before they're
/// back. What a failed take-back said comes back now ("until you quit") may: no wider than its session was then.
fn restart(
    w: &World,
    ed: &mut Editor,
    early: Option<usize>,
    cut: bool,
    ledger: &mut Ledger,
    s: &str,
) -> Done {
    let start = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.root == w.root);
    let of: Vec<String> =
        start.map(|l| l.allowed.grants().iter().map(crate::granted::taken_as).collect()).unwrap_or_default();
    let early = early.filter(|_| !of.is_empty()).map(|i| of[i % of.len()].clone());
    // (quit as `main` quits: what's still being written lands first — or, `cut`, the quit gave up on it: what
    // it took back may be back, as said)
    let landed = if cut { Vec::new() } else { landing(&crate::storewrite::pending(&w.allow), ledger) };
    match cut {
        true => {
            for p in crate::storewrite::pending(&w.allow).iter().chain(&crate::storewrite::pending(&w.trust))
            {
                if p.again {
                    not_written(w, ed, p, ledger);
                }
            }
            w.new_process();
        }
        false => w.land_all(ed, s),
    }
    assert!(ed.writes_flush(), "{s}: the quit's writes past their wait");
    w.new_process();
    *ed = w.editor();
    if let Some(what) = &early {
        ed.sandbox_disallow(what).unwrap();
    }
    ed.sessions_restore();
    let held = w.held;
    settle(ed, s, 0, &|ed, _| ed.session_store.restored && (held || ed.allow_taking.is_empty()));
    let mut done = narrows(early.into_iter().map(Gone::Listed).collect());
    done.gone_at = Some(w.root.clone());
    // (only what the failed ones named: one taken back since whose write landed isn't)
    let failed =
        ledger.failed.drain(..).flat_map(|(root, took)| took.into_iter().map(move |g| (root.clone(), g)));
    done.allowed = failed.map(|(root, g)| (Some(root), Effect::Grant(g))).chain(landed).collect();
    // (the versions a failed trust take-back named, the store's still: trusted again — the project's rows now, and
    // a reload of one later widens with that answer)
    let store = crate::trust::Store::read(Some(&w.trust));
    let back: Vec<_> = (ledger.failed_versions.drain(..))
        .filter(|(file, h)| store.state(file, *h) == State::Trusted)
        .map(|(_, h)| h)
        .collect();
    for h in &back {
        if !ledger.trusted.contains(h) {
            ledger.trusted.push(*h);
        }
    }
    // (and a yes a card gave, its write held until the quit)
    let carded = |h: &crate::trust::Digest| {
        back.contains(h) || ledger.others_trusted.contains(h) || (!cut && ledger.trusted.contains(h))
    };
    if ed.project_trust.as_ref().is_some_and(|p| carded(&p.hash)) {
        done.allowed.push((None, Effect::Rows(project_rows(ed))));
    }
    done
}

/// A take-back or no held at a store that won't be written (it fails, or the quit cut it off): what it took back
/// may be back at the next start, as said — its checkout's answers it names, the versions trusted.
fn not_written(w: &World, ed: &Editor, p: &crate::storewrite::Pending, ledger: &mut Ledger) {
    match p.carry["root"].as_str() {
        Some(root) => {
            let root = PathBuf::from(root);
            let names: Vec<&str> =
                p.carry["remove"].as_array().into_iter().flatten().filter_map(|n| n.as_str()).collect();
            let kept = crate::allow::Store::read(Some(&w.allow)).get(&crate::sandbox::profile_path(&root));
            let took =
                kept.grants().into_iter().filter(|g| names.iter().any(|n| crate::allow::takes_back(n, g)));
            ledger.failed.push((root, took.collect()));
        }
        None => {
            let file = w.root.join(".tarae.toml");
            let now = ed.project_trust.as_ref().map(|p| p.hash);
            let versions = ledger.trusted.iter().copied().chain(now).map(|h| (file.clone(), h));
            ledger.failed_versions.extend(versions);
        }
    }
}

/// `Step::Land`: the next `n` writes held at a store land — `fail`: the first fails. What an answer kept for the
/// project lets is the step's to widen, unless another tarae took it back after it was given (it isn't kept).
fn land(w: &World, ed: &mut Editor, trust: bool, n: usize, fail: bool, ledger: &mut Ledger, s: &str) -> Done {
    let path = if trust { w.trust.clone() } else { w.allow.clone() };
    // (those queued: a failed one kept for `:restart` and the quit isn't in line)
    let ahead: Vec<_> = crate::storewrite::pending(&path).into_iter().filter(|p| !p.failed).collect();
    let n = n.min(ahead.len());
    if n == 0 {
        return narrows(Vec::new());
    }
    if fail {
        crate::storewrite::fail_next(&path);
        if ahead[0].again {
            not_written(w, ed, &ahead[0], ledger);
        }
    }
    let target = crate::storewrite::served(&path) + n as u64;
    crate::storewrite::release(&path, n as u64);
    settle(ed, s, ed.messages.len(), &|_, _| crate::storewrite::served(&path) >= target);
    let mut done = narrows(Vec::new());
    if !trust {
        done.allowed = landing(&ahead[usize::from(fail)..n], ledger);
    }
    // (a yes on the project's settings another tarae's no or take-back came after: held back again)
    let yes = |w: &serde_json::Value| w["yes"] == "tarae" && w["given"].as_u64() == Some(ledger.last_yes);
    let beaten =
        |w: &serde_json::Value| ledger.others_untrusted.iter().any(|at| w["given"].as_u64() < Some(*at));
    let writes = ahead[usize::from(fail)..n]
        .iter()
        .filter(|_| trust)
        .flat_map(|p| p.carry.as_array().cloned().unwrap_or_default());
    if writes.into_iter().any(|w| yes(&w) && beaten(&w)) {
        done.gone.push(Gone::Trust);
    }
    done
}

/// What the answers kept for the project among `writes` (sandbox.json's, landing) let — each for its checkout —
/// unless another tarae took it back after it was given (it isn't kept).
fn landing(writes: &[crate::storewrite::Pending], ledger: &Ledger) -> Vec<(Option<PathBuf>, Effect)> {
    let mut out = Vec::new();
    for p in writes {
        let (Some(root), Some(given)) = (p.carry["root"].as_str(), p.carry["given"].as_u64()) else {
            continue;
        };
        for name in p.carry["add"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) {
            let Some(g) = ledger.answered.iter().find(|g| g.shown() == name) else { continue };
            let beaten =
                ledger.others_took.iter().any(|(w, at)| *at > given && crate::allow::takes_back(w, g));
            if !beaten {
                out.push((Some(PathBuf::from(root)), Effect::Grant(g.clone())));
            }
        }
    }
    out
}

/// `Step::Other`: another tarae's write lands now — its card: what it keeps or trusts may widen from then on
/// (`Ledger.others_*`); what it takes back narrows.
fn other(w: &World, ed: &mut Editor, op: OtherOp, ledger: &mut Ledger, s: &str) -> Done {
    let now = crate::storewrite::given();
    let at = now;
    let (file, kind) = (w.root.join(".tarae.toml"), Kind::Tarae);
    let project = crate::sandbox::profile_path(&w.root);
    match op {
        OtherOp::Trust { yes, current } => {
            let hash = match (current, &ed.project_trust) {
                (true, Some(p)) => p.hash,
                _ => [9; 32],
            };
            let seen = std::collections::BTreeMap::new();
            let a =
                crate::trust::Write::Answer { kind, place: None, file, hash, trusted: yes, given: at, seen };
            crate::trust::write_as_other(&w.trust, &a).unwrap();
            ledger.others_trusted.retain(|h| *h != hash);
            match yes {
                true => ledger.others_trusted.push(hash),
                false => {
                    ledger.trusted.retain(|h| *h != hash);
                    ledger.others_untrusted.push(now);
                }
            }
        }
        OtherOp::TrustTaken => {
            let f = crate::trust::Write::Forget { kind, place: crate::trust::place_of(&file), given: at };
            crate::trust::write_as_other(&w.trust, &f).unwrap();
            ledger.trusted.clear();
            ledger.others_trusted.clear();
            ledger.others_untrusted.push(now);
        }
        OtherOp::Keep(i) => {
            let g = w.grants[i].clone();
            crate::allow::write_as_other(&w.allow, &project, &[], std::slice::from_ref(&g)).unwrap();
            ledger.others_kept.push(g);
        }
        OtherOp::Take(i) => {
            let name = crate::granted::taken_as(&w.grants[i]);
            crate::allow::write_as_other(&w.allow, &project, std::slice::from_ref(&name), &[]).unwrap();
            ledger.others_kept.retain(|k| !crate::allow::takes_back(&name, k));
            ledger.others_took.push((name, now));
        }
    }
    // (the sessions' `Caps` made again, as the next answer would)
    let ids: Vec<_> = std::iter::once(&ed.session).chain(&ed.sessions).map(|s| s.id).collect();
    ids.into_iter().for_each(|id| ed.caps_sync_of(id));
    settle(ed, s, ed.messages.len(), &landed);
    narrows(Vec::new())
}

/// `TARAE_CAPS_SEEDS`, else this many seeds — and `FOUND`.
const SEEDS: u64 = 5;
/// Seeds that find a widening once, kept in the default run beside `SEEDS`: the review's majors broken once (picked
/// again 2026-10-18 for `Step::Other` without a clock, `target/sb/capsloop-found.sh`) — 22 review at the end past
/// its trust (no default seed finds it); 42 the key's text after its take-back failed and 23 a read card offering
/// `~/.claude` (`allow::read_open` without its `kept`), as seed 5 does; a run's `a` past its conversation seed 2
/// finds. Pick them again (`TARAE_CAPS_SEEDS=60` with the break) when `Step::random` changes.
const FOUND: [u64; 3] = [22, 23, 42];
/// Steps in each seed's sequence.
const STEPS: usize = 30;
/// Seeds run at once.
const THREADS: usize = 4;

/// One seed's sequence, each step held to `check` for every session there before and after it — Err names the
/// seed, the step and the steps before it.
fn run_seed(seed: u64) -> Result<(), String> {
    let mut rng = crate::mutate::Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let random = (0..STEPS).map(|_| None);
    run_steps(&format!("caps-props-{seed}"), &format!("seed {seed}"), random, &mut |w| {
        Step::random(&mut rng, w)
    })
}

/// `run_seed` over `steps` — each one given, or the next of `random` — said as `said`.
fn run_steps(
    name: &str,
    said: &str,
    steps: impl Iterator<Item = Option<Step>>,
    random: &mut dyn FnMut(&World) -> Step,
) -> Result<(), String> {
    let mut w = World::new(name);
    // (every write held until a step lands it: the loop chooses where each lands among the other steps)
    w.hold();
    let mut ed = w.editor();
    let mut ledger = Ledger::default();
    let mut done: Vec<String> = Vec::new();
    let mut yours = w.yours_said();
    ed.sessions_restore();
    settle(&mut ed, "start", 0, &|ed, _| ed.session_store.restored);
    // (each step's state before and after, said with a failure: what the store, the session and its chat hold, the
    // take-backs under way, each session's `Caps` asked and landed)
    let mut trace: Vec<String> = Vec::new();
    let snap = |ed: &Editor, w: &World| {
        let store = crate::allow::Store::read(Some(&w.allow));
        let sessions = std::iter::once(&ed.session).chain(&ed.sessions).map(|s| {
            let chat = s.chat.as_ref().map(|c| format!("{:?}", c.allowed.grants())).unwrap_or_default();
            format!(
                "[{} {}: kept {:?} session {:?} chat {chat} caps {}/{:?}]",
                s.id,
                s.root.display(),
                store.get(&crate::sandbox::profile_path(&s.root)).grants(),
                s.allowed.grants(),
                s.caps_asked,
                s.caps.as_ref().map(|(g, _)| *g)
            )
        });
        format!(
            "front {} taking {:?} {}",
            ed.session.id,
            ed.allow_taking,
            sessions.collect::<Vec<_>>().join(" ")
        )
    };
    for (n, step) in steps.enumerate() {
        let step = step.unwrap_or_else(|| random(&w));
        let before = reaches(&mut ed, &w.named);
        let front_root = ed.session.root.clone();
        let front = before.iter().find(|(r, _)| *r == front_root).map(|(_, r)| r.clone()).unwrap();
        trace.push(format!("{n} {step:?} before: {}", snap(&ed, &w)));
        let d = apply(&mut w, &mut ed, &step, &mut ledger, &front);
        trace.push(format!("{n} after: {}", snap(&ed, &w)));
        if matches!(step, Step::Yours(_)) {
            yours = w.yours_said();
        }
        // (what a step took back is the front session's, unless it says whose)
        let gone_at = d.gone_at.clone().unwrap_or(front_root);
        // (the project's settings a card trusted, its write maybe still held: what a new session has of them)
        let carded = ed.project_trust.as_ref().is_some_and(|p| {
            p.state == State::Trusted
                && (ledger.trusted.contains(&p.hash) || ledger.others_trusted.contains(&p.hash))
        });
        let rows = carded.then(|| (None, Effect::Rows(project_rows(&ed))));
        for (root, after) in reaches(&mut ed, &w.named) {
            // (a session new since: no wider than one tarae would start there — and the trust a card gave)
            let (fresh, mut extra) = (None::<Reach>, Vec::new());
            let fresh = match before.iter().find(|(r, _)| *r == root) {
                Some(_) => fresh,
                None => {
                    extra.extend(rows.clone());
                    Some(Reach::of(&w.editor_at(&root), &w.named))
                }
            };
            let b = fresh.as_ref().unwrap_or_else(|| &before.iter().find(|(r, _)| *r == root).unwrap().1);
            // (and what another tarae kept for the project, its card, from then on)
            let others = ledger.others_kept.iter().map(|g| (Some(w.root.clone()), Effect::Grant(g.clone())));
            let allowed: Vec<Reach> = (d.allowed.iter().cloned().chain(others).chain(extra))
                .filter(|(at, _)| at.as_ref().is_none_or(|at| *at == root))
                .map(|(_, e)| plus(b, &after, &e))
                .collect();
            let gone = if root == gone_at { d.gone.as_slice() } else { &[] };
            check(b, &after, &allowed, gone, &yours).map_err(|e| {
                let trace = trace.join("\n    ");
                format!(
                    "{said}, step {n} {step:?}, session {}: {e}\n  after {done:?}\n  trace\n    {trace}",
                    root.display()
                )
            })?;
        }
        done.push(format!("{step:?}"));
    }
    Ok(())
}

/// The loop: no change made without a card or your own config widens what a session may do, a take-back narrows at
/// once — every `cargo test`, `SEEDS` seeds (`TARAE_CAPS_SEEDS` more, `TARAE_CAPS_SEED` one).
#[test]
fn no_change_without_a_card_widens_what_a_session_may_do() {
    let var = |n: &str| std::env::var(n).ok().and_then(|v| v.parse::<u64>().ok());
    let seeds: Vec<u64> = match (var("TARAE_CAPS_SEED"), var("TARAE_CAPS_SEEDS")) {
        (Some(one), _) => vec![one],
        (None, Some(n)) => (1..=n).collect(),
        (None, None) => (1..=SEEDS).chain(FOUND).collect(),
    };
    // (each seed its own editor and project: `THREADS` at a time)
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..THREADS.min(seeds.len()) {
            crate::event::scoped(s, "tarae-capsloop", || {
                while let Some(&seed) = seeds.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                    if let Err(e) = run_seed(seed) {
                        failed.lock().unwrap().push(e);
                    }
                }
            });
        }
    });
    let mut failed = failed.into_inner().unwrap();
    failed.sort();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// Sequences no default seed takes: a version whose trust take-back failed is trusted again at the next start — a
/// reload of it after is that answer, not a widening (the verify's two).
#[test]
fn a_failed_trust_take_back_is_trusted_again_at_the_next_start() {
    let restart = Step::Restart { early: None, cut: false };
    let sequences = [
        vec![Step::Project(2), Step::Trust(true), Step::Untrust { failing: true }, restart.clone()]
            .into_iter()
            .chain([Step::Project(1), Step::Project(2)])
            .collect::<Vec<_>>(),
        vec![Step::Project(1), Step::Trust(true), Step::Project(2), Step::Trust(true)]
            .into_iter()
            .chain([Step::TrustTaken { failing: true }, Step::Project(1), restart])
            .collect(),
    ];
    for (i, steps) in sequences.into_iter().enumerate() {
        let given = steps.into_iter().map(Some);
        let said = format!("sequence {i}");
        run_steps(&format!("caps-trust-back-{i}"), &said, given, &mut |_| unreachable!()).unwrap();
    }
}

/// The loop's case for the compare-and-set: another tarae's take-back from a clock 9 h behind lands before this
/// tarae's own yes and `A`, both held — neither is written (the next start, the `Caps` the `A`'s result makes,
/// are held to `check`: written, either would widen with no card for it).
#[test]
fn another_taraes_take_back_before_a_held_answer_keeps_it_out() {
    let land = |trust| Step::Land { trust, n: 3, fail: false };
    let steps = [
        Step::Project(1),
        Step::Trust(true),
        Step::Other { op: OtherOp::TrustTaken },
        land(true),
        Step::Restart { early: None, cut: false },
        Step::Card(5, Scope::Always),
        Step::Other { op: OtherOp::Take(5) },
        land(false),
    ];
    let given = steps.into_iter().map(Some);
    run_steps("caps-cas-loop", "the compare-and-set", given, &mut |_| unreachable!()).unwrap();
}

/// Every setting `trust::RISKY` holds back is in `Reach`, or said to be about something else.
#[test]
fn every_risky_setting_is_in_reach_or_said_elsewhere() {
    let mut ours: Vec<&str> =
        SETTINGS.iter().map(|(p, _)| *p).chain(ELSEWHERE.iter().map(|(p, _)| *p)).collect();
    let mut risky = crate::trust::RISKY.to_vec();
    ours.sort();
    risky.sort();
    assert_eq!(ours, risky);
}

/// Every setting `with_allowed` changes for an answer is one `Reach` reads.
#[test]
fn every_setting_with_allowed_changes_is_in_reach() {
    let w = World::new("caps-props-with-allowed");
    let cfg = crate::config::EditorConfig::default();
    for g in &w.grants {
        let mut a = Allowed::default();
        a.add(g);
        let caps = Caps::granted(&w.root, &Allowed::default(), &Allowed::default(), &a);
        let widened = crate::config::EditorConfig {
            llm: crate::tools::with_allowed(&cfg.llm, &caps, None).llm,
            ..cfg.clone()
        };
        for s in crate::settings::SETTINGS {
            if (s.get)(&cfg) != (s.get)(&widened) {
                assert!(SETTINGS.iter().any(|(p, _)| *p == s.path), "{g:?} changes {}, not in Reach", s.path);
            }
        }
    }
}

// ── The fixes the loop showed, each named ────────────────────────────────────────────────────────────────────

fn granted_now(ed: &Editor) -> Vec<Grant> {
    ed.caps_basis().build().grants()
}

/// A take-back whose store's write fails stays taken back until you quit: the store still keeps the answer, and
/// the next `Caps` (any answer, a chat opened) would have it again — no card asked.
#[test]
fn a_take_back_whose_write_fails_stays_taken_back_until_you_quit() {
    let w = World::new("caps-take-fails");
    let mut ed = w.editor();
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    settle(&mut ed, "kept", 0, &|ed, _| ed.caps_basis().kept().1.all.ports.contains(&5432));
    take_back(&w, &mut ed, "port 5432", Failing::Store, "taken back");
    assert!(!granted_now(&ed).contains(&Grant::Port(5432)), "back after its write failed");
    assert!(ed.messages.iter().any(|m| m.2.contains("taken back until you quit")), "and it's said so");
}

/// `f` true before long, the editor's events applied meanwhile (a background write brings none back).
fn until(ed: &mut Editor, what: &str, f: &dyn Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// An editor whose kept sessions are back.
fn restored(w: &World) -> Editor {
    let mut ed = w.editor();
    ed.sessions_restore();
    settle(&mut ed, "the sessions back", 0, &|ed, _| ed.session_store.restored);
    ed
}

/// Whether the sessions' store names `what` in its list (not among the take-backs kept beside it) — its files, not
/// a write's temp file (`disk::write_atomic`'s `.…tmp`, there through the fsync before the rename: a slow disk's
/// start read nothing yet).
fn sessions_say(w: &World, what: &str) -> bool {
    let files = std::fs::read_dir(&w.sessions).into_iter().flatten().flatten();
    let stored = files.filter(|f| f.file_name().to_string_lossy().ends_with(".json"));
    let list = |t: String| serde_json::from_str::<serde_json::Value>(&t).map(|v| v["sessions"].to_string());
    stored
        .into_iter()
        .any(|f| std::fs::read_to_string(f.path()).map(list).is_ok_and(|l| l.is_ok_and(|l| l.contains(what))))
}

/// A take-back before the kept sessions are back stays taken back: what comes back for the session leaves it out,
/// and the sessions' store is written without it — the start after doesn't bring it back.
#[test]
fn a_take_back_before_the_sessions_are_back_stays_taken_back() {
    let w = World::new("caps-take-early");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    until(&mut ed, "kept with the session", &|| sessions_say(&w, "8080"));
    let mut ed = w.editor();
    ed.sandbox_disallow("port 8080").unwrap();
    ed.sessions_restore();
    settle(&mut ed, "back", 0, &|ed, _| ed.session_store.restored);
    assert!(!ed.session.allowed.grants().contains(&Grant::Port(8080)), "back with the session");
    assert!(!granted_now(&ed).contains(&Grant::Port(8080)));
    // (written on a worker, nothing brought back: the next start is what tells)
    let back = || restored(&w).session.allowed.grants().contains(&Grant::Port(8080));
    until(&mut ed, "back at the next start", &|| !back());
}

/// … and when that write fails, it's said: the start after would bring it back.
#[test]
fn a_take_back_before_the_sessions_are_back_says_when_its_write_fails() {
    let w = World::new("caps-take-early-fails");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    until(&mut ed, "kept with the session", &|| sessions_say(&w, "8080"));
    let mut ed = w.editor();
    ed.sandbox_disallow("port 8080").unwrap();
    // (a quit now names it: nothing written yet)
    let said = ed.unwritten(true);
    assert!(said.iter().any(|s| s.contains("without port 8080")), "{said:?}");
    ed.sessions_restore();
    // (read from the store already; written to one no write reaches)
    ed.session_store.dir = Some(w.broken_dir.clone());
    settle(&mut ed, "said", 0, &|_, said| {
        said.iter().any(|m| m.contains("couldn't write the sessions without port 8080"))
    });
}

/// A session's answer taken back whose sessions' store write fails is said so — it would be back at the next start.
#[test]
fn a_session_answer_whose_write_fails_says_so() {
    let w = World::new("caps-session-fails");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    take_back(&w, &mut ed, "port 8080", Failing::Sessions, "failing");
    assert!(ed.messages.iter().any(|m| m.2.contains("couldn't write the sessions without port 8080")));
}

/// A failed take-back holds its own checkout's answer only: another session's kept answer of that name stays, and
/// that one's take-back doesn't end this one's.
#[test]
fn a_failed_take_back_holds_its_own_checkout_only() {
    let w = World::new("caps-take-own");
    let mut ed = w.editor();
    let port = Grant::Port(5432);
    answer(&mut ed, &port, Scope::Always, false, "kept here");
    let (main, other) = (ed.session.id, w.at.join("s1"));
    repository(&other);
    let s1 = ed.session_add(other);
    ed.session_switch(s1);
    answer(&mut ed, &port, Scope::Always, false, "kept there");
    ed.session_switch(main);
    take_back(&w, &mut ed, "port 5432", Failing::Store, "failing here");
    ed.session_switch(s1);
    assert!(granted_now(&ed).contains(&port), "the other checkout's answer hidden");
    take_back(&w, &mut ed, "port 5432", Failing::No, "taken there");
    ed.session_switch(main);
    assert!(!granted_now(&ed).contains(&port), "the failed one ended by the other's");
}

/// An `A` for an answer whose take-back failed writes both, and ends the take-back: the new answer is the
/// project's.
#[test]
fn an_answer_kept_again_ends_a_failed_take_back() {
    let w = World::new("caps-kept-again");
    let mut ed = w.editor();
    let port = Grant::Port(5432);
    answer(&mut ed, &port, Scope::Always, false, "kept");
    take_back(&w, &mut ed, "port 5432", Failing::Store, "failing");
    assert!(!granted_now(&ed).contains(&port));
    answer(&mut ed, &port, Scope::Always, false, "kept again");
    let caps = ed.caps_basis().build();
    assert!(
        caps.ports.iter().any(|e| e.what == 5432 && e.source == Source::Grant(Scope::Always)),
        "the answer given again hidden"
    );
    assert!(ed.taking_for(&w.root).is_empty());
}

/// A host card's `A` for a host whose take-back failed ends it too, as a card's other `A`s: the answer given again
/// is the project's.
#[test]
fn a_hosts_answer_kept_again_ends_a_failed_take_back() {
    let w = World::new("caps-host-again");
    let mut ed = w.editor();
    let project = crate::sandbox::profile_path(&w.root);
    let kept = || crate::allow::Store::read(Some(&w.allow)).get(&project).hosts.len();
    // (the card's `A`, the network open when it asked)
    let a = |ed: &Editor| crate::hosts::Ask {
        ended: false,
        session: ed.session.proxy_id(),
        run: 0,
        host: "db.example.com".into(),
        port: 5432,
        process: "psql".into(),
        label: "psql".into(),
        users: false,
        left: false,
        open: true,
        hosts: crate::llm::Hosts::List(Vec::new()),
        also: Vec::new(),
    };
    let ask = a(&ed);
    crate::hosts::answer(&mut ed, 0, ask, crate::sandbox::proxy::Scope::Project);
    until(&mut ed, "written", &|| kept() == 1);
    take_back(&w, &mut ed, &crate::hosts::listed("db.example.com", 5432), Failing::Store, "failing");
    assert_eq!(ed.taking_for(&w.root).len(), 1);
    let ask = a(&ed);
    crate::hosts::answer(&mut ed, 0, ask, crate::sandbox::proxy::Scope::Project);
    settle(&mut ed, "written again", 0, &|ed, _| ed.taking_for(&w.root).is_empty());
    assert_eq!(kept(), 1);
}

/// A project trusted just before quitting is kept too: the trust store's write is waited for at the end (a
/// spawned job's — `spawn_kept`), and the next start reads it trusted.
#[test]
fn a_trust_answered_just_before_quitting_is_kept() {
    let w = World::new("caps-quit-trust");
    w.project(1);
    let mut ed = w.editor();
    let p = ed.project_trust.clone().expect("the project's settings held back");
    // (a slow write: the store's lock held a moment by another thread)
    let lock = w.trust.with_extension("lock");
    let (tx, locked) = std::sync::mpsc::channel();
    let slow = crate::event::thread("tarae-trustheld", move || {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock).unwrap();
        f.lock().unwrap();
        tx.send(()).ok();
        std::thread::sleep(Duration::from_millis(300));
    });
    locked.recv().unwrap();
    ed.project_trust_answered(p.hash, true);
    // Quit: the loop has ended, nothing more is applied
    assert!(ed.writes_flush(), "waited past its cap");
    let kept = crate::trust::Store::read(Some(&w.trust)).state(&p.file, p.hash) == State::Trusted;
    slow.join().unwrap();
    assert!(kept, "the answer lost with the process");
    let next = w.editor();
    assert_eq!(next.project_trust.map(|p| p.state), Some(State::Trusted), "not at the next start");
}

/// A project's trust taken back (`:granted`'s `d`) just before quitting stays taken back: the store's forget starts
/// with the `d`, so the process's end waits for it — not from a result the ended loop would never apply.
#[test]
fn a_trust_taken_back_just_before_quitting_stays_taken_back() {
    let w = World::new("caps-quit-untrust");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let p = ed.project_trust.clone().unwrap();
    // (a slow write: the store's lock held a moment by another thread)
    let lock = w.trust.with_extension("lock");
    let (tx, locked) = std::sync::mpsc::channel();
    let slow = crate::event::thread("tarae-trustheld", move || {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock).unwrap();
        f.lock().unwrap();
        tx.send(()).ok();
        std::thread::sleep(Duration::from_millis(300));
    });
    locked.recv().unwrap();
    ed.trust_take(Kind::Tarae, crate::trust::place_of(&p.file));
    // Quit: the loop has ended, nothing more is applied
    assert!(ed.writes_flush(), "waited past its cap");
    // (read while the lock's holder may still hold it: the mark ends only with the forget's write)
    let written = !crate::trust::is_taking(&p.place);
    slow.join().unwrap();
    assert!(written, "the take-back's write wasn't waited for");
    // (the next start: a new process, no take-back marked — the store alone says)
    w.new_process();
    let next = w.editor();
    assert_ne!(next.project_trust.map(|p| p.state), Some(State::Trusted), "trusted again at the next start");
}

/// Every write queued at this thread's worlds' stores landed, its result in.
fn all_landed(ed: &mut Editor, what: &str) {
    settle(ed, what, 0, &|_, _| queued_here() == 0);
}

/// Rule 1, one writer a store: its writes land in the order given, whatever their workers reach first (the first
/// one slow) — an `A` then its take-back leaves it out, a take-back then an `A` keeps it, a yes then
/// `:project-untrust` leaves the project held back.
#[test]
fn a_stores_writes_land_in_the_order_given() {
    let w = World::new("caps-order-given");
    w.project(1);
    let mut ed = w.editor();
    let project = crate::sandbox::profile_path(&w.root);
    let kept = || crate::allow::Store::read(Some(&w.allow)).get(&project).ports.contains(&5432);
    let slow = Duration::from_millis(300);
    crate::storewrite::slow_next(&w.allow, slow);
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    ed.sandbox_disallow("port 5432").unwrap();
    all_landed(&mut ed, "an A, then its take-back");
    assert!(!kept(), "the A's write landed after its take-back's");
    crate::storewrite::slow_next(&w.allow, slow);
    ed.sandbox_disallow("port 5432").unwrap();
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    all_landed(&mut ed, "a take-back, then an A");
    assert!(kept(), "the take-back's write landed after the A's");
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::slow_next(&w.trust, slow);
    ed.project_trust_answered(p.hash, true);
    ed.project_untrust().unwrap();
    all_landed(&mut ed, "a yes, then the untrust");
    let state = crate::trust::Store::read(Some(&w.trust)).state(&p.file, p.hash);
    w.new_process();
    assert_eq!(state, State::Ignored, "the yes's write landed after the untrust's");
}

/// Rule 2, a mark ends with its own write: a yes given before `:project-untrust` and landing first leaves the
/// untrust's mark — and when the untrust's write then fails, it stays held back until you quit.
#[test]
fn a_take_backs_mark_ends_with_its_own_write_only() {
    let w = World::new("caps-mark-own");
    w.project(1);
    let mut ed = w.editor();
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::hold(&w.trust, Some(0));
    ed.project_trust_answered(p.hash, true);
    ed.project_untrust().unwrap();
    let served = crate::storewrite::served(&w.trust);
    crate::storewrite::release(&w.trust, 1);
    settle(&mut ed, "the yes", 0, &|_, _| crate::storewrite::served(&w.trust) > served);
    let marked = crate::trust::is_taking(&p.place);
    crate::storewrite::fail_next(&w.trust);
    crate::storewrite::hold(&w.trust, None);
    settle(&mut ed, "failed", 0, &|_, said| said.iter().any(|m| m.contains("held back until you quit")));
    w.reload(&mut ed);
    let reloaded = ed.config.llm.sandbox_ports.contains(&5432);
    w.new_process();
    assert!(marked, "the yes ended the take-back given after it");
    assert!(!reloaded, "trusted again by a reload");
}

/// Rule 3, a no is written whatever's there: `:project-untrust` of one version while another tarae's later `n` on
/// another version of the place lands first — the untrusted version isn't trusted at the next start.
#[test]
fn a_no_is_written_whatever_another_version_says() {
    let w = World::new("caps-no-versions");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    crate::storewrite::hold(&w.trust, Some(0));
    ed.project_untrust().unwrap();
    let file = w.root.join(".tarae.toml");
    let seen = std::collections::BTreeMap::new();
    let given = crate::trust::given();
    let theirs = crate::trust::Write::Answer {
        kind: Kind::Tarae,
        place: None,
        file,
        hash: [9; 32],
        trusted: false,
        given,
        seen,
    };
    crate::trust::write_as_other(&w.trust, &theirs).unwrap();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the untrust");
    w.new_process();
    let next = w.editor();
    assert_eq!(next.project_trust.map(|p| p.state), Some(State::Ignored), "the untrust skipped");
}

/// R3, a yes is a compare-and-set: another tarae's take-back it hadn't seen lands before it — the yes isn't
/// written and the card asks again; the same for an `A` and sandbox.json's.
#[test]
fn a_yes_after_an_unseen_take_back_isnt_written() {
    let w = World::new("caps-yes-cas");
    w.project(1);
    let mut ed = w.editor();
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::hold(&w.trust, Some(0));
    ed.project_trust_answered(p.hash, true);
    let forget = crate::trust::Write::Forget { kind: Kind::Tarae, place: p.place.clone(), given: 0 };
    crate::trust::write_as_other(&w.trust, &forget).unwrap();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the yes");
    let state = crate::trust::Store::read(Some(&w.trust)).state(&p.file, p.hash);
    let here = ed.project_trust.as_ref().map(|p| p.state);
    let project = crate::sandbox::profile_path(&w.root);
    crate::storewrite::hold(&w.allow, Some(0));
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    crate::allow::write_as_other(&w.allow, &project, &["port 5432".into()], &[]).unwrap();
    crate::storewrite::hold(&w.allow, None);
    all_landed(&mut ed, "the A");
    let kept = crate::allow::Store::read(Some(&w.allow)).get(&project).ports.contains(&5432);
    w.new_process();
    assert_ne!(state, State::Trusted, "the yes written over their take-back");
    assert_eq!(here, Some(State::Ask), "still trusted here");
    assert!(!kept, "the A written over their take-back");
}

/// R3 (verify3 F7): this tarae's own take-back, ahead of its yes in the queue, lands after another tarae's take-back
/// the yes never saw — the own one doesn't hide it: the yes isn't written; the same for an `A`.
#[test]
fn an_own_take_back_ahead_doesnt_hide_an_unseen_one() {
    let w = World::new("caps-own-ahead");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::hold(&w.trust, Some(0));
    ed.trust_take(Kind::Tarae, p.place.clone());
    ed.project_trust_answered(p.hash, true);
    let forget = crate::trust::Write::Forget { kind: Kind::Tarae, place: p.place.clone(), given: 0 };
    crate::trust::write_as_other(&w.trust, &forget).unwrap();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the take-back, the yes");
    let state = crate::trust::Store::read(Some(&w.trust)).state(&p.file, p.hash);
    let project = crate::sandbox::profile_path(&w.root);
    answer(&mut ed, &Grant::Port(5432), Scope::Always, false, "kept");
    crate::storewrite::hold(&w.allow, Some(0));
    ed.sandbox_disallow("port 5432").unwrap();
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    crate::allow::write_as_other(&w.allow, &project, &["port 5432".into()], &[]).unwrap();
    crate::storewrite::hold(&w.allow, None);
    all_landed(&mut ed, "the take-back, the A");
    let kept = crate::allow::Store::read(Some(&w.allow)).get(&project).ports.contains(&5432);
    w.new_process();
    assert_ne!(state, State::Trusted, "the own take-back hid theirs");
    assert!(!kept, "the own take-back hid theirs (sandbox.json)");
}

/// R6 (verify3 F4, N8, N9): a yes after this tarae's failed take-back writes it along — and is still skipped over
/// another tarae's take-back between; an `A` after a failed take-back of another name is checked all the same.
#[test]
fn a_failed_take_back_taken_along_doesnt_switch_the_check_off() {
    let w = World::new("caps-failed-along");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::fail_next(&w.trust);
    ed.trust_take(Kind::Tarae, p.place.clone());
    all_landed(&mut ed, "the failed take-back");
    crate::storewrite::hold(&w.trust, Some(0));
    ed.project_trust_answered(p.hash, true);
    let forget = crate::trust::Write::Forget { kind: Kind::Tarae, place: p.place.clone(), given: 0 };
    crate::trust::write_as_other(&w.trust, &forget).unwrap();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the yes");
    let state = crate::trust::Store::read(Some(&w.trust)).state(&p.file, p.hash);
    let project = crate::sandbox::profile_path(&w.root);
    answer(&mut ed, &Grant::Port(9000), Scope::Always, false, "kept");
    take_back(&w, &mut ed, "port 9000", Failing::Store, "failing");
    crate::storewrite::hold(&w.allow, Some(0));
    crate::allow::grant(&mut ed, &Grant::Port(8080), Scope::Always);
    crate::allow::write_as_other(&w.allow, &project, &["port 8080".into()], &[]).unwrap();
    crate::storewrite::hold(&w.allow, None);
    all_landed(&mut ed, "the A");
    let kept = crate::allow::Store::read(Some(&w.allow)).get(&project);
    w.new_process();
    assert_ne!(state, State::Trusted, "the failed take-back written along hid theirs");
    assert!(!kept.ports.contains(&8080), "another name's take-back hid theirs");
    assert!(!kept.ports.contains(&9000), "the failed take-back wasn't written along");
}

/// R4: the store removed, another tarae's change brings the place back to the very counter the yes had seen —
/// its tag tells it apart: the yes isn't written.
#[test]
fn a_counter_gone_back_is_told_by_its_tag() {
    let w = World::new("caps-gone-back");
    w.project(1);
    let mut ed = w.editor();
    let p = ed.project_trust.clone().unwrap();
    ed.project_trust_answered(p.hash, false);
    all_landed(&mut ed, "the no");
    std::fs::remove_file(&w.trust).unwrap();
    let theirs = crate::trust::Write::Answer {
        kind: Kind::Tarae,
        place: None,
        file: p.file.clone(),
        hash: [9; 32],
        trusted: false,
        given: 0,
        seen: Default::default(),
    };
    crate::trust::write_as_other(&w.trust, &theirs).unwrap();
    let _ = crate::trust::Store::read(Some(&w.trust));
    ed.project_trust_answered(p.hash, true);
    all_landed(&mut ed, "the yes");
    let state = crate::trust::Store::read(Some(&w.trust)).state(&p.file, p.hash);
    w.new_process();
    assert_ne!(state, State::Trusted, "their change at the same counter taken for this tarae's");
}

/// verify3 F5: a store read while a take-back's mark was up holds the place back after the mark ends — that
/// write came after the read.
#[test]
fn a_read_before_a_marks_end_still_holds_it_back() {
    let w = World::new("caps-read-mark");
    let file = w.root.join(".tarae.toml");
    crate::trust::record(w.trust.clone(), Kind::Tarae, file.clone(), [6; 32], true).unwrap();
    let place = crate::trust::place_of(&file);
    crate::trust::taking(place.clone(), 1);
    let read = crate::trust::Store::read(Some(&w.trust));
    crate::trust::taken(&place, 1);
    assert_ne!(read.state(&file, [6; 32]), State::Trusted, "read under the mark, trusted once it ended");
}

/// verify3 F8: a take-back whose write fails stays among the writes not landed — `:restart` carries it, a quit
/// names it — until a later write of this tarae's that wrote it over lands (a take-back of its place).
#[test]
fn a_failed_take_back_is_carried_until_written_over() {
    let w = World::new("caps-failed-kept");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::fail_next(&w.trust);
    ed.trust_take(Kind::Tarae, p.place.clone());
    all_landed(&mut ed, "the failed take-back");
    let carried = crate::restart::snapshot(&mut ed)["store_writes"]["trust"].clone();
    let said = crate::storewrite::pending_said(&[&w.trust]);
    ed.trust_take(Kind::Tarae, p.place.clone());
    all_landed(&mut ed, "the take-back again");
    let after = crate::storewrite::pending(&w.trust).len();
    w.new_process();
    assert_eq!(carried.as_array().map_or(0, Vec::len), 1, "not carried: {carried}");
    assert!(said.iter().any(|s| s.contains("trust taken back")), "{said:?}");
    assert_eq!(after, 0, "kept past a later write of its place");
}

/// verify3 F11: a reload that read the store before a `d` — of any version there — doesn't trust what it read.
#[test]
fn a_reload_read_before_a_take_back_doesnt_trust() {
    let w = World::new("caps-reload-d");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let (early, _) = crate::config::load(&w.root, &w.sources());
    let place = ed.project_trust.as_ref().unwrap().place.clone();
    crate::storewrite::hold(&w.trust, Some(0));
    ed.trust_take(Kind::Tarae, place);
    ed.reload_config(early, Vec::new());
    let ports = ed.config.llm.sandbox_ports.clone();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the take-back");
    w.new_process();
    assert!(!ports.contains(&5432), "trusted again by a reload read before the d");
}

/// verify3 F13, F14: a session's take-back where another repository is found than the one its answers are kept
/// for isn't written there and is said; one over a store a clock ahead wrote is written.
#[test]
fn a_sessions_take_back_is_written_where_kept_or_said() {
    let w = World::new("caps-session-where");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    settle(&mut ed, "kept there", 0, &|ed, _| ed.session_store.repo.is_some());
    let kept = ed.session_store.repo.clone().unwrap();
    let file = crate::chats::file(ed.session_store.dir.as_deref().unwrap(), &kept);
    // (the list, not the take-backs kept beside it)
    let read = |f: &Path| {
        let v =
            std::fs::read_to_string(f).ok().and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok());
        v.map(|v| v["sessions"].to_string()).unwrap_or_default()
    };
    settle(&mut ed, "written", 0, &|_, _| read(&file).contains("8080"));
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    v["seq"] = serde_json::json!(u64::MAX);
    std::fs::write(&file, v.to_string()).unwrap();
    ed.sandbox_disallow("port 8080").unwrap();
    settle(&mut ed, "written", 0, &|_, _| !read(&file).contains("8080"));
    answer(&mut ed, &Grant::Port(8081), Scope::Session, false, "for the session");
    settle(&mut ed, "kept", 0, &|_, _| read(&file).contains("8081"));
    ed.session_store.repo = Some(w.at.join("another/.git"));
    let from = ed.messages.len();
    ed.sandbox_disallow("port 8081").unwrap();
    settle(&mut ed, "said", from, &|_, said| said.iter().any(|m| m.contains("is gone")));
    assert!(read(&file).contains("8081"), "written to the repository found");
}

/// R5 (verify3 F1a): a yes, then `:project-untrust` — while the untrust waits behind it, another tarae reading the
/// store never finds the project trusted.
#[test]
fn a_yes_with_a_later_take_back_queued_isnt_written_true() {
    let w = World::new("caps-later-take");
    w.project(1);
    let mut ed = w.editor();
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::hold(&w.trust, Some(0));
    ed.project_trust_answered(p.hash, true);
    ed.project_untrust().unwrap();
    let served = crate::storewrite::served(&w.trust);
    crate::storewrite::release(&w.trust, 1);
    settle(&mut ed, "the yes", 0, &|_, _| crate::storewrite::served(&w.trust) > served);
    // (as another tarae reads it: no mark of this one's)
    let between = std::fs::read_to_string(&w.trust).unwrap_or_default();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the untrust");
    w.new_process();
    assert!(
        !between.contains("true"),
        "written trusted between the yes and the untrust behind it: {between}"
    );
}

/// Rule 5: what `:restart`'s exec cut off — a trust take-back, one of what the project keeps — is carried and the
/// next tarae writes it; its sequence goes on past what was given (`given_last`); a quit that gives up names them.
#[test]
fn take_backs_not_written_at_restart_are_written_by_the_next_tarae() {
    let w = World::new("caps-restart-pending");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    answer(&mut ed, &Grant::Port(8080), Scope::Always, false, "kept");
    crate::storewrite::hold(&w.trust, Some(0));
    crate::storewrite::hold(&w.allow, Some(0));
    ed.project_untrust().unwrap();
    ed.sandbox_disallow("port 8080").unwrap();
    let said = ed.unwritten(true);
    let last = crate::storewrite::last();
    let mut v = crate::restart::snapshot(&mut ed);
    let carried_last = v["given_last"].as_u64();
    // (the exec: the writes held cut off — and the last tarae's sequence far past this process's, which every
    // test's thread raises: only the carry reaches it — F8)
    w.new_process();
    let far = crate::storewrite::last() + 1_000_000;
    v["given_last"] = serde_json::json!(far);
    crate::restart::before_config(&v);
    let went_on = crate::storewrite::last() >= far;
    // (the config read the store, which still trusts: held by the carried marks until the no lands — N11)
    let mut ed = w.editor();
    let held = ed.project_trust.as_ref().map(|p| p.state);
    crate::restart::restore(&mut ed, &v);
    all_landed(&mut ed, "given again");
    w.new_process();
    let next = w.editor();
    let project = crate::sandbox::profile_path(&w.root);
    let kept = crate::allow::Store::read(Some(&w.allow)).get(&project).ports.contains(&8080);
    w.new_process();
    assert!(
        said.iter().any(|s| s.contains("trust held back")) && said.iter().any(|s| s.contains("port 8080")),
        "{said:?}"
    );
    assert!(carried_last >= Some(last), "{carried_last:?}");
    assert!(went_on, "the sequence didn't go on past the carried one");
    assert_ne!(held, Some(State::Trusted), "trusted after the restart before its no landed");
    assert_eq!(next.project_trust.map(|p| p.state), Some(State::Ignored), "the untrust lost with the exec");
    assert!(!kept, "the take-back lost with the exec");
}

/// N2, N8: a no whose place is worked out at the write (git's, Claude's settings') whose write failed stays past a
/// write at another place that lands — named by a quit, carried by `:restart` placed, and marked there before the
/// config's first read (N1, N7); one carried before its worker placed it is placed as it's marked.
#[test]
fn a_failed_no_placed_at_its_write_stays_and_is_marked_after_a_restart() {
    let w = World::new("caps-no-placed");
    let mut ed = w.editor();
    let (a, b) = (w.root.join(".git/config"), w.at.join("other/.git/config"));
    let answer = |file: &Path, trusted: bool| crate::trust::Write::Answer {
        kind: Kind::Git,
        place: None,
        file: file.to_path_buf(),
        hash: [7; 32],
        trusted,
        given: crate::trust::given(),
        seen: Default::default(),
    };
    crate::storewrite::fail_next(&w.trust);
    crate::trust::queue(&mut ed, w.trust.clone(), vec![answer(&a, false)], |_, _| {});
    all_landed(&mut ed, "the no, failed");
    crate::trust::queue(&mut ed, w.trust.clone(), vec![answer(&b, true)], |_, _| {});
    all_landed(&mut ed, "a yes at another place");
    let said = ed.unwritten(true);
    let v = crate::restart::snapshot(&mut ed);
    w.new_process();
    let place = crate::trust::place_of(&a);
    let marked = |v: &serde_json::Value| {
        crate::restart::before_config(v);
        let marked = crate::trust::is_taking(&place);
        crate::trust::untake_all(&place);
        marked
    };
    let mut early = v.clone();
    early["store_writes"]["trust"][0][0]["place"] = serde_json::Value::Null;
    let (after, unplaced) = (marked(&v), marked(&early));
    assert!(said.iter().any(|s| s.contains("trust held back for")), "dropped by a write elsewhere: {said:?}");
    let carried = &v["store_writes"]["trust"][0][0];
    assert!(carried["place"].is_object(), "carried unplaced: {carried}");
    assert!(after, "not marked after the restart");
    assert!(unplaced, "carried unplaced: not marked");
}

/// F1, N3, N12: a failed take-back stays until a write that wrote it over lands — not a no on another version at
/// its place, nor a yes a take-back given after it superseded; a take-back of the place does.
#[test]
fn a_failed_take_back_stays_past_writes_that_leave_it() {
    let w = World::new("caps-failed-left");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::fail_next(&w.trust);
    ed.project_untrust().unwrap();
    all_landed(&mut ed, "the untrust, failed");
    let at = |hash, trusted, given| crate::trust::Write::Answer {
        kind: Kind::Tarae,
        place: Some(p.place.clone()),
        file: p.file.clone(),
        hash,
        trusted,
        given,
        seen: Default::default(),
    };
    let left = || crate::storewrite::pending(&w.trust).len();
    crate::trust::queue(&mut ed, w.trust.clone(), vec![at([9; 32], false, crate::trust::given())], |_, _| {});
    all_landed(&mut ed, "a no on another version");
    let after_no = left();
    // (a take-back given after the yes, marked: the yes isn't written — its own write never queued here)
    let yes = crate::trust::given();
    let later = ed.trust_taking_given(p.place.clone());
    crate::trust::queue(&mut ed, w.trust.clone(), vec![at(p.hash, true, yes)], |_, _| {});
    all_landed(&mut ed, "a yes superseded");
    crate::trust::taken(&p.place, later);
    let after_yes = left();
    ed.trust_take(Kind::Tarae, p.place.clone());
    all_landed(&mut ed, "a take-back of the place");
    let after_forget = left();
    w.new_process();
    assert_eq!(after_no, 1, "dropped by a no on another version");
    assert_eq!(after_yes, 1, "dropped by a yes not written");
    assert_eq!(after_forget, 0, "kept past a take-back of its place");
}

/// F2, F3, F5: a session's take-back whose sessions' write failed stays among the writes not landed — a quit names
/// it — and `:restart` carries it: the next tarae writes the sessions without it.
#[test]
fn a_sessions_take_back_not_written_is_named_and_carried() {
    let w = World::new("caps-session-carried");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    until(&mut ed, "kept with the session", &|| sessions_say(&w, "8080"));
    crate::storewrite::fail_next(&w.sessions);
    ed.sandbox_disallow("port 8080").unwrap();
    let failed = |_: &Editor, said: &[String]| said.iter().any(|m| m.contains("couldn't write the sessions"));
    settle(&mut ed, "failed", 0, &failed);
    let said = ed.unwritten(true);
    let v = crate::restart::snapshot(&mut ed);
    w.new_process();
    crate::storewrite::cut(&w.sessions);
    let mut ed = w.editor();
    crate::restart::before_config(&v);
    crate::restart::restore(&mut ed, &v);
    until(&mut ed, "written without it by the next tarae", &|| !sessions_say(&w, "8080"));
    assert!(said.iter().any(|s| s.contains("without port 8080")), "not named: {said:?}");
}

/// F7: another tarae's take-back of a session's answer — kept in the sessions' file with its counter — isn't put
/// back by this tarae's list written after it unseen, and goes here too; the answer given again after it is kept.
#[test]
fn another_taraes_session_take_back_isnt_put_back() {
    let w = World::new("caps-session-other");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    until(&mut ed, "kept with the session", &|| sessions_say(&w, "8080"));
    // (the other tarae: the list read, written without it as taken back)
    let common = ed.session_store.repo.clone().unwrap();
    let s = crate::worktree::load(&w.sessions, &common);
    let mut theirs = s.sessions.clone();
    theirs.iter_mut().for_each(|k| k.allowed.remove("port 8080"));
    let took = [(ed.session.root.clone(), "port 8080".to_string())];
    let stored = crate::worktree::Stored { sessions: theirs, front: s.front, rev: s.rev };
    crate::worktree::save(&w.sessions, &common, stored, &took, s.rev).unwrap();
    ed.sessions_keep();
    let gone = |ed: &Editor, _: &[String]| !ed.session.allowed.ports.contains(&8080);
    settle(&mut ed, "gone here", 0, &gone);
    let put_back = sessions_say(&w, "8080");
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "again");
    until(&mut ed, "kept again", &|| sessions_say(&w, "8080"));
    assert!(!put_back, "their take-back put back");
}

/// The same answer another tarae kept first holds: a yes on the version it trusted, an `A` for the grant it kept —
/// not said "changed in another tarae", nor held back again.
#[test]
fn the_same_answer_kept_first_in_another_tarae_holds() {
    let w = World::new("caps-same-answer");
    w.project(1);
    let mut ed = w.editor();
    let p = ed.project_trust.clone().unwrap();
    crate::storewrite::hold(&w.trust, Some(0));
    ed.project_trust_answered(p.hash, true);
    let yes = crate::trust::Write::Answer {
        kind: Kind::Tarae,
        place: Some(p.place.clone()),
        file: p.file.clone(),
        hash: p.hash,
        trusted: true,
        given: 0,
        seen: Default::default(),
    };
    crate::trust::write_as_other(&w.trust, &yes).unwrap();
    crate::storewrite::hold(&w.trust, None);
    all_landed(&mut ed, "the yes");
    let here = ed.project_trust.as_ref().map(|p| p.state);
    let project = crate::sandbox::profile_path(&w.root);
    crate::storewrite::hold(&w.allow, Some(0));
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    crate::allow::write_as_other(&w.allow, &project, &[], &[Grant::Port(5432)]).unwrap();
    crate::storewrite::hold(&w.allow, None);
    all_landed(&mut ed, "the A");
    let warned: Vec<String> =
        ed.messages.iter().map(|m| m.2.clone()).filter(|m| m.contains("another tarae")).collect();
    w.new_process();
    assert_eq!(here, Some(State::Trusted), "held back again");
    assert!(warned.is_empty(), "{warned:?}");
}

/// A sessions' write cut off (the process ended before its turn) writes nothing — as trust's and sandbox.json's.
#[test]
fn a_sessions_write_cut_off_writes_nothing() {
    let w = World::new("caps-session-cut");
    let mut ed = restored(&w);
    crate::storewrite::hold(&w.sessions, Some(0));
    ed.session.allowed.add(&Grant::Port(8080));
    ed.sessions_keep();
    crate::storewrite::cut(&w.sessions);
    assert!(ed.writes_flush(), "the write's worker never ended");
    assert!(!sessions_say(&w, "8080"), "written though cut off");
}

/// F6: the repository a sessions' write landed for is in `:restart`'s state though its result was never applied
/// (the loop had ended) — a folder that became a repository after the start.
#[test]
fn a_sessions_write_records_its_repository_where_it_lands() {
    let w = World::new("caps-landed-repo");
    std::fs::rename(w.root.join(".git"), w.at.join("git-later")).unwrap();
    crate::gitrepo::forget(&w.root);
    let mut ed = restored(&w);
    std::fs::rename(w.at.join("git-later"), w.root.join(".git")).unwrap();
    ed.session.allowed.add(&Grant::Port(8080));
    ed.sessions_keep();
    // (landed, its result not applied)
    let end = Instant::now() + Duration::from_secs(20);
    while crate::storewrite::served(&w.sessions) < crate::storewrite::given_turns(&w.sessions) {
        assert!(Instant::now() < end, "never landed");
        std::thread::yield_now();
    }
    let v = crate::worktree::snapshot(&ed);
    assert!(ed.session_store.repo.is_none(), "its result applied");
    assert!(v["repo"].is_string(), "not recorded: {v}");
}

/// A session's answer kept once a folder that wasn't a repository at the start became one records it: taken back
/// after the repository's gone, the write is said to have failed (verify2 5).
#[test]
fn a_session_answer_kept_after_git_init_records_the_repository() {
    let w = World::new("caps-init-later");
    std::fs::rename(w.root.join(".git"), w.at.join("git-later")).unwrap();
    crate::gitrepo::forget(&w.root);
    let mut ed = restored(&w);
    std::fs::rename(w.at.join("git-later"), w.root.join(".git")).unwrap();
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    settle(&mut ed, "kept there", 0, &|ed, _| ed.session_store.repo.is_some());
    std::fs::rename(w.root.join(".git"), w.root.join(".git-gone")).unwrap();
    crate::gitrepo::forget(&w.root);
    let from = ed.messages.len();
    ed.sandbox_disallow("port 8080").unwrap();
    settle(&mut ed, "said", from, &|_, said| {
        said.iter().any(|m| m.contains("couldn't write the sessions without port 8080"))
    });
}

/// N8: a repository's git trust taken back is held from the `d` on — its answer since tarae started too, before
/// the worker that forgets it.
#[test]
fn a_git_trust_taken_back_is_held_from_the_d_on() {
    let w = World::new("caps-git-held");
    let config = w.root.join(".git/config");
    crate::gitrepo::answered(&config, [3; 32], true);
    let place = crate::trust::place_of(&config);
    crate::trust::taking(place.clone(), 1);
    let state = crate::gitrepo::trust_state_of(&config, [3; 32]);
    crate::trust::taken(&place, 1);
    crate::gitrepo::forget_trust(&place);
    assert_eq!(state, State::Ask, "its answer read before the mark");
}

/// N10, N11, N12: a write carries the place worked out where it was given; one over a store it can't read refuses;
/// a place taken back with no version left goes after `TAKEN_KEPT`, a recent one stays.
#[test]
fn a_trust_write_keeps_its_place_refuses_an_unreadable_store_and_prunes() {
    let w = World::new("caps-trust-write");
    let elsewhere = crate::trust::Place::File(w.at.join("elsewhere/.tarae.toml"));
    let seen = std::collections::BTreeMap::new();
    let a = crate::trust::Write::Answer {
        kind: Kind::Tarae,
        place: Some(elsewhere.clone()),
        file: w.root.join(".tarae.toml"),
        hash: [4; 32],
        trusted: true,
        given: crate::trust::given(),
        seen,
    };
    crate::trust::write_now(&w.trust, &a, false).unwrap();
    let text = std::fs::read_to_string(&w.trust).unwrap();
    assert!(text.contains("elsewhere"), "written under the place worked out again: {text}");
    // Unreadable: not written over
    std::fs::write(&w.trust, "{ not json").unwrap();
    assert!(crate::trust::write_now(&w.trust, &a, false).is_err(), "written over a store it can't read");
    assert_eq!(std::fs::read_to_string(&w.trust).unwrap(), "{ not json");
    // Pruned: taken back 31 days ago, and now
    let new = crate::trust::Place::File(w.at.join("new"));
    let old = serde_json::json!({ "answers": [{
        "kind": "tarae", "file": w.at.join("old"), "versions": [], "rev": [1, 1],
        "taken": crate::allow::now_s() - 31 * 24 * 3600,
    }] });
    std::fs::write(&w.trust, old.to_string()).unwrap();
    crate::trust::forget(&w.trust, Kind::Tarae, &new, crate::trust::given()).unwrap();
    let text = std::fs::read_to_string(&w.trust).unwrap();
    assert!(!text.contains(&w.at.join("old").display().to_string()), "the old one kept: {text}");
    assert!(text.contains(&w.at.join("new").display().to_string()), "the new one dropped: {text}");
    // sandbox.json alike: an unreadable one isn't written over
    std::fs::write(&w.allow, "[").unwrap();
    let c = crate::allow::Change { add: vec![Grant::Port(1)], ..Default::default() };
    assert!(crate::allow::keep_ordered(&w.allow, &w.root, &c, &[], false).is_err());
    assert_eq!(std::fs::read_to_string(&w.allow).unwrap(), "[");
}

/// N13: a config reload that read the store before `:project-untrust`, landing after it, doesn't trust it again.
#[test]
fn a_reload_read_before_an_untrust_doesnt_trust_it_again() {
    let w = World::new("caps-reload-before");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let (early, _) = crate::config::load(&w.root, &w.sources());
    ed.project_untrust().unwrap();
    ed.reload_config(early, Vec::new());
    let ports = ed.config.llm.sandbox_ports.clone();
    all_landed(&mut ed, "the untrust");
    w.new_process();
    assert!(!ports.contains(&5432), "trusted again by a reload read before");
}

/// The reviewer reads what the project keeps less what's being taken back: a kept read place whose take-back
/// failed ("until you quit") isn't read unasked by it either (`reviewer::basis`).
#[test]
fn the_reviewer_leaves_out_a_take_back_whose_write_failed() {
    let w = World::new("caps-reviewer-taking");
    let mut ed = w.editor();
    let place = w.at.join("out/b");
    answer(&mut ed, &Grant::Read(place.clone()), Scope::Always, true, "kept");
    let reads =
        |ed: &Editor| crate::reviewer::basis(ed, &w.root).build().read.iter().any(|e| e.what == place);
    assert!(reads(&ed), "the kept place");
    take_back(&w, &mut ed, &place.display().to_string(), Failing::Store, "failing");
    assert!(!reads(&ed), "the reviewer reads what's taken back");
}

/// `:restart`'s state — made after the loop has ended — carries no take-back an `A`'s write ended meanwhile (its
/// result never applied): the answer just given isn't taken back again until you quit.
#[test]
fn restart_carries_no_take_back_a_landed_write_ended() {
    let w = World::new("caps-restart-landed");
    let mut ed = w.editor();
    let port = Grant::Port(5432);
    answer(&mut ed, &port, Scope::Always, false, "kept");
    take_back(&w, &mut ed, "port 5432", Failing::Store, "failing");
    crate::allow::grant(&mut ed, &port, Scope::Always);
    // (the loop ends: the write lands, its result isn't applied)
    assert!(ed.writes_flush(), "waited past its cap");
    let v = crate::restart::snapshot(&mut ed);
    assert_eq!(v["taking"].as_array().map_or(0, Vec::len), 0, "carried: {}", v["taking"]);
}

/// A session's answer taken back when the repository its sessions came back from isn't found any more: the write
/// is said to have failed — not kept, silently.
#[test]
fn a_session_take_back_without_its_repository_says_so() {
    let w = World::new("caps-session-norepo");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    until(&mut ed, "kept with the session", &|| sessions_say(&w, "8080"));
    std::fs::rename(w.root.join(".git"), w.root.join(".git-gone")).unwrap();
    crate::gitrepo::forget(&w.root);
    let from = ed.messages.len();
    ed.sandbox_disallow("port 8080").unwrap();
    settle(&mut ed, "said", from, &|_, said| {
        said.iter().any(|m| m.contains("couldn't write the sessions without port 8080"))
    });
}

/// … after a `:restart` too: its state carries the repository the sessions are kept for.
#[test]
fn a_session_take_back_without_its_repository_says_so_after_a_restart() {
    let w = World::new("caps-session-norepo-restart");
    let mut ed = restored(&w);
    answer(&mut ed, &Grant::Port(8080), Scope::Session, false, "for the session");
    until(&mut ed, "kept with the session", &|| sessions_say(&w, "8080"));
    let v = crate::restart::snapshot(&mut ed);
    w.new_process();
    let mut ed = w.editor();
    crate::restart::restore(&mut ed, &v);
    settle(&mut ed, "back", 0, &landed);
    assert!(ed.session.allowed.grants().contains(&Grant::Port(8080)), "the session's answer back");
    std::fs::rename(w.root.join(".git"), w.root.join(".git-gone")).unwrap();
    crate::gitrepo::forget(&w.root);
    let from = ed.messages.len();
    ed.sandbox_disallow("port 8080").unwrap();
    settle(&mut ed, "said", from, &|_, said| {
        said.iter().any(|m| m.contains("couldn't write the sessions without port 8080"))
    });
}

/// An `A` given just before quitting is kept: the process's end waits for its write (`Editor::writes_flush`, as
/// `main` calls it), and the next start has it.
#[test]
fn an_answer_given_just_before_quitting_is_kept() {
    let w = World::new("caps-quit-kept");
    let mut ed = w.editor();
    // (a slow write, as a slow disk makes it)
    crate::storewrite::slow_next(&w.allow, Duration::from_millis(300));
    crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Always);
    // Quit: the loop has ended, nothing more is applied
    assert!(ed.writes_flush(), "waited past its cap");
    let project = crate::sandbox::profile_path(&w.root);
    let kept = crate::allow::Store::read(Some(&w.allow)).get(&project).ports.contains(&5432);
    assert!(kept, "the answer lost with the process");
    let next = w.editor();
    assert!(next.caps_basis().build().ports.iter().any(|e| e.what == 5432), "not at the next start");
}

/// An `A`'s write ends the take-backs it carried only: one of that name made while it's under way stays taken back
/// — for good when its own write fails; and the `A` isn't kept at all, that take-back given after it (R5).
#[test]
fn an_answers_write_leaves_a_take_back_made_after_it() {
    let w = World::new("caps-kept-then-taken");
    let mut ed = w.editor();
    let port = Grant::Port(5432);
    answer(&mut ed, &port, Scope::Always, false, "kept");
    take_back(&w, &mut ed, "port 5432", Failing::Store, "failing");
    let from = ed.messages.len();
    // (the `A`'s write held until the take-back after it is asked — which fails)
    crate::storewrite::hold(&w.allow, Some(0));
    crate::allow::grant(&mut ed, &port, Scope::Always);
    ed.allow_path = Some(w.broken.clone());
    ed.sandbox_disallow("port 5432").unwrap();
    ed.allow_path = Some(w.allow.clone());
    crate::storewrite::hold(&w.allow, None);
    // (both results in: the take-back's failure said, the `A`'s ending the one it carried — not the store's file,
    // there before its result reaches the loop)
    settle(&mut ed, "both written", from, &|ed, said| {
        ed.taking_for(&w.root) == ["port 5432"]
            && said.iter().any(|m| m.contains("taken back until you quit"))
    });
    let project = crate::sandbox::profile_path(&w.root);
    assert!(
        !crate::allow::Store::read(Some(&w.allow)).get(&project).ports.contains(&5432),
        "the answer kept, a take-back given after it"
    );
    ed.caps_sync();
    settle(&mut ed, "made again", from, &landed);
    assert!(!granted_now(&ed).contains(&port), "the later take-back ended by the answer's write");
}

/// A key the project kept open, taken back while its write fails, no longer goes with a message — nor once the
/// chat reads the kept ones again.
#[test]
fn a_key_taken_back_stays_out_of_the_message_after_its_write_failed() {
    let mut w = World::new("caps-key-back");
    let mut ed = w.editor();
    crate::chat::open(&mut ed);
    settle(&mut ed, "open", 0, &landed);
    let front = Reach::of(&ed, &w.named);
    apply(&mut w, &mut ed, &Step::KeyKept, &mut Ledger::default(), &front);
    let key = w.named[0].clone();
    assert!(!crate::reads::key_shut(&ed, &key, &[]), "kept open");
    take_back(&w, &mut ed, &key.display().to_string(), Failing::Store, "failing");
    assert!(crate::reads::key_shut(&ed, &key, &[]), "its text still goes with a message");
    crate::chat::close(&mut ed);
    crate::chat::open(&mut ed);
    let read = |ed: &Editor, _: &[String]| ed.session.chat.as_ref().is_some_and(|c| c.kept_keys.is_some());
    settle(&mut ed, "read again", 0, &read);
    assert!(crate::reads::key_shut(&ed, &key, &[]), "open again with the chat");
}

/// A conversation left — a new one, another continued — takes its `a` answers with it: a card's (the gates' `Caps`
/// too), a run's (Docker's socket with it), the screen card's.
#[test]
fn a_conversation_left_takes_its_answers_with_it() {
    let mut w = World::new("caps-chat-left");
    let g = Grant::Read(w.at.join("out/b"));
    for again in [false, true] {
        let mut ed = w.editor();
        crate::chat::open(&mut ed);
        settle(&mut ed, "open", 0, &landed);
        answer(&mut ed, &g, Scope::ThisChat, false, "for the conversation");
        let front = Reach::of(&ed, &w.named);
        for step in [Step::RunAgain, Step::Sees] {
            apply(&mut w, &mut ed, &step, &mut Ledger::default(), &front);
        }
        match again {
            false => crate::chat::reset(&mut ed),
            true => {
                let (id, rows) = ("another".to_string(), Vec::new());
                let saved = crate::chats::Saved {
                    id,
                    title: String::new(),
                    updated: 0,
                    rows,
                    plan: None,
                    usage: None,
                    seq: 0,
                };
                crate::chat::continue_with(&mut ed, saved);
            }
        }
        settle(&mut ed, "left", 0, &landed);
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.run_again.is_empty() && !c.sees, "a run's or the screen's `a` carried over ({again})");
        assert!(!granted_now(&ed).contains(&g), "a card's `a` carried over ({again})");
        assert!(!ed.session.caps().unwrap().grants().contains(&g), "and said ({again})");
    }
}

/// The project's settings answered `y`, the store written.
fn trusted(ed: &mut Editor, w: &World) {
    let p = ed.project_trust.clone().unwrap();
    ed.project_trust_answered(p.hash, true);
    let file = w.trust.clone();
    settle(ed, "trusted", 0, &|_, _| {
        crate::trust::Store::read(Some(&file)).state(&p.file, p.hash) == State::Trusted
    });
}

/// A yes on another version of the project's file doesn't bring back the version taken back (its forget failed):
/// it's written with it.
#[test]
fn a_yes_on_another_version_keeps_the_one_taken_back_held() {
    let w = World::new("caps-trust-versions");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    trust_take(&w, &mut ed, true, "failing");
    w.project(2);
    w.reload(&mut ed);
    trusted(&mut ed, &w);
    w.project(1);
    w.reload(&mut ed);
    assert!(!ed.config.llm.sandbox_ports.contains(&5432), "the version taken back trusted again");
    w.new_process();
}

/// `:project-untrust` whose write fails stays held back — a reload reads the store as not trusting it.
#[test]
fn an_untrust_whose_write_fails_stays_held_back() {
    let w = World::new("caps-untrust-fails");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    ed.trust_path = Some(w.broken.clone());
    ed.project_untrust().unwrap();
    settle(&mut ed, "failing", 0, &|_, said| said.iter().any(|m| m.contains("held back until you quit")));
    ed.trust_path = Some(w.trust.clone());
    w.reload(&mut ed);
    assert!(!ed.config.llm.sandbox_ports.contains(&5432), "trusted again by a reload");
    w.new_process();
}

/// What failed to be taken back — a kept answer, the project's trust — stays so across `:restart`.
#[test]
fn what_failed_to_be_taken_back_stays_so_across_restart() {
    let w = World::new("caps-take-restart");
    w.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    answer(&mut ed, &Grant::Port(8080), Scope::Always, false, "kept");
    take_back(&w, &mut ed, "port 8080", Failing::Store, "failing");
    trust_take(&w, &mut ed, true, "failing");
    let v = crate::restart::snapshot(&mut ed);
    w.new_process();
    let mut ed = w.editor();
    crate::restart::restore(&mut ed, &v);
    settle(&mut ed, "back", 0, &landed);
    assert!(!granted_now(&ed).contains(&Grant::Port(8080)), "the kept answer back");
    assert!(!ed.config.llm.sandbox_ports.contains(&5432), "the project's settings back");
    w.new_process();
}

/// `:restart` carries its own tarae's trust take-backs only — the process's marks are every editor's (other
/// tests', other seeds'), and one carried would stay marked past its own write.
#[test]
fn restart_carries_its_own_trust_take_backs_only() {
    let (w, other) = (World::new("caps-own-trust"), World::new("caps-own-trust-other"));
    w.project(1);
    other.project(1);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    let mut theirs = other.editor();
    trusted(&mut theirs, &other);
    let place = crate::trust::place_of(&theirs.project_trust.as_ref().unwrap().file);
    crate::trust::taking(place.clone(), 1);
    trust_take(&w, &mut ed, true, "failing");
    let v = crate::restart::snapshot(&mut ed);
    crate::trust::taken(&place, 1);
    let carried = v["trust_taking"].as_array().map_or(0, Vec::len);
    w.new_process();
    assert_eq!(carried, 1, "{}", v["trust_taking"]);
}

/// Review at the end a trusted project turned on goes when its trust is taken back — the chat's edit gate, not
/// only the config.
#[test]
fn review_at_the_end_from_a_trust_goes_with_it() {
    let w = World::new("caps-at-end");
    w.project(2);
    let mut ed = w.editor();
    trusted(&mut ed, &w);
    crate::chat::open(&mut ed);
    settle(&mut ed, "open", 0, &landed);
    assert!(crate::edits::at_end(&ed));
    ed.project_untrust().unwrap();
    assert!(!crate::edits::at_end(&ed), "Claude's edits still go in unseen");
    let file = w.trust.clone();
    let p = ed.project_trust.clone().unwrap();
    settle(&mut ed, "untrusted", 0, &|_, _| {
        crate::trust::Store::read(Some(&file)).state(&p.file, p.hash) == State::Ignored
    });
}

/// Review at the end your `y` turned on stays when the config read again hasn't it (its write failed, a reload
/// read before it, a project's layer) — only what the config turned on follows it.
#[test]
fn review_at_the_end_you_said_yes_to_stays_past_a_reload() {
    let w = World::new("caps-at-end-yours");
    let mut ed = w.editor();
    crate::chat::open(&mut ed);
    settle(&mut ed, "open", 0, &landed);
    // (not kept in a file: the reload below reads it off)
    ed.config_file = None;
    crate::edits::at_end_set(&mut ed, true);
    w.reload(&mut ed);
    assert!(!ed.config.llm.review_at_end, "the config read again has it");
    assert!(crate::edits::at_end(&ed), "your yes taken back by a reload");
}

/// `check` says a read where runs never read, and a write where it runs later — the loop's cards never offer
/// them (`Step::Denied`), so it's shown to fire here.
#[test]
fn the_check_says_what_runs_never_read_or_write() {
    let w = World::new("caps-never");
    let ed = w.editor();
    let yours = w.yours_said();
    let mut r = Reach::of(&ed, &w.named);
    let kept = r.caps.kept.iter().find(|k| k.starts_with(&w.home)).cloned().expect("a kept place in home");
    r.caps.read.push(Entry { what: kept, source: Source::Grant(Scope::Always) });
    assert!(check(&r, &r, &[], &[], &yours).unwrap_err().contains("never read"));
    let mut r = Reach::of(&ed, &w.named);
    let bin = r.caps.later.bins.first().cloned().expect("a folder of programs");
    r.caps.write.push(Entry { what: bin, source: Source::Grant(Scope::Always) });
    assert!(check(&r, &r, &[], &[], &yours).unwrap_err().contains("runs later"));
}

/// What `plus` lets a card's answer add is what the answer adds through the gates (`with_allowed`, `Caps`) —
/// and each adds something.
#[test]
fn the_card_allowance_is_what_with_allowed_adds() {
    let w = World::new("caps-allowance");
    let yours = w.yours_said();
    for g in w.grants.iter().filter(|g| !matches!(g, Grant::Key(_))) {
        let mut ed = w.editor();
        crate::chat::open(&mut ed);
        settle(&mut ed, "open", 0, &landed);
        let b = Reach::of(&ed, &w.named);
        answer(&mut ed, g, Scope::ThisChat, false, "for the conversation");
        let a = Reach::of(&ed, &w.named);
        assert!(!wider(&b, &[], &a, &yours).is_empty(), "{g:?} adds nothing");
        let more = wider(&b, &[plus(&b, &a, &Effect::Grant(g.clone()))], &a, &yours);
        assert!(more.is_empty(), "{g:?} adds more than its allowance: {more:?}");
    }
}

/// A new conversation, or the chat closed, takes the last one's answers out of the session's `Caps` at once —
/// what `:granted` and the chat's count read, not only the gates.
#[test]
fn a_conversation_ended_takes_its_answers_out_of_the_caps_at_once() {
    let w = World::new("caps-chat-ends");
    let mut ed = w.editor();
    let g = Grant::Read(w.at.join("out/b"));
    let shown = |ed: &Editor| ed.session.caps().is_some_and(|c| c.grants().contains(&g));
    for end in [crate::chat::reset as fn(&mut Editor), crate::chat::close] {
        crate::chat::open(&mut ed);
        crate::allow::grant(&mut ed, &g, Scope::ThisChat);
        settle(&mut ed, "granted", 0, &landed);
        assert!(shown(&ed));
        end(&mut ed);
        settle(&mut ed, "ended", 0, &landed);
        assert!(!shown(&ed), "the last conversation's answer still in the session's Caps");
    }
}

/// A trust taken back is held back before the store's write (the store's lock held by the test meanwhile), and
/// stays held back when that write fails — a reload reads the store as not trusting it.
#[test]
fn a_trust_taken_back_is_held_back_before_the_store_is_written_and_after_it_failed() {
    let w = World::new("caps-trust-take");
    w.project(1);
    let mut ed = w.editor();
    let trust = |ed: &mut Editor, w: &World| {
        trusted(ed, w);
        assert!(ed.config.llm.sandbox_ports.contains(&5432));
    };
    trust(&mut ed, &w);
    let place = crate::trust::place_of(&w.root.join(".tarae.toml"));
    let mut lock = std::fs::File::options();
    lock.create(true).truncate(false).write(true);
    let lock = lock.open(w.trust.with_extension("lock")).unwrap();
    lock.lock().unwrap();
    let from = ed.messages.len();
    ed.trust_take(Kind::Tarae, place.clone());
    settle(&mut ed, "held", from, &|_, said| said.iter().any(|m| m.contains("settings are held back")));
    assert!(!ed.config.llm.sandbox_ports.contains(&5432), "held back only after the store's write");
    w.reload(&mut ed);
    assert!(!ed.config.llm.sandbox_ports.contains(&5432), "a reload before the write trusts it again");
    drop(lock);
    settle(&mut ed, "forgotten", from, &|_, _| !crate::trust::is_taking(&place));
    // Its write failing
    trust(&mut ed, &w);
    trust_take(&w, &mut ed, true, "failing");
    w.reload(&mut ed);
    assert!(!ed.config.llm.sandbox_ports.contains(&5432), "trusted again after its write failed");
    w.new_process();
}
