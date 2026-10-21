//! Claude's Read in the chat, and the reviewer's Read, Grep and Glob (the chat has had no Grep or Glob
//! since 2026-10-13 — `chat::TOOLS`). Measured 2026-10-02 (`target/sb/r1.sh`): named in
//! `--allowedTools` they read anywhere without asking; left out, every call is a `can_use_tool` ask. So
//! they're left out (`chat::ALLOWED`) and answered here: inside the project (the real path, links
//! followed) or in a place the user allowed (`allow.rs`) at once; anything else on a card that says
//! exactly what's read — `y` once · `a` this chat · `A` always · `n` no. One card at a time; the rest wait
//! in `Chat.reads`, and an answer for longer lets the ones it covers through. A file in the project named
//! like a key (`sandbox::key_named_in` — what a run can't read either, 7a/7b) asks too, Read's or one a
//! Grep would search (`Look.keys`); `a`/`A` open it as the run's panel does (`Grant::Key`), for both.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::allow::Scope;
use crate::editor::Editor;

/// The tools whose asks come here.
pub const TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

/// What a call reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub tool: String,
    /// The file read (Read), or the folder searched (Grep, Glob) — real path; None: where it leads can't be
    /// told (a `..` after a wildcard).
    pub at: Option<PathBuf>,
    /// Grep's pattern, Glob's pattern.
    pub pattern: String,
    /// Grep's file filter (`glob`).
    pub filter: String,
    /// The path as Claude gave it (shown when `at` is None).
    pub given: String,
    /// `at` is a single file (Grep searches just it — not "everything in it").
    pub file: bool,
    /// The files named like keys in the project it opens (`sandbox::key_named_in`, real paths): Read's file,
    /// those a Grep would search (`grep_keys`) — Glob only lists names, so none. They ask unless opened
    /// (`Grant::Key` — the run's panel's or this card's); `decide` leaves only those still closed.
    pub keys: Vec<PathBuf>,
    /// A folder outside the project searched where it would go unasked, too big to look through for files named
    /// like keys (past `KEYS_WALK_MAX` entries): it asks, and never goes unasked (`covered`).
    pub cut: bool,
}

/// The entries a walk of a folder outside the project looks through for files named like keys at most
/// (`decide_with`, on the worker that decides) — past them it can't tell, and the search asks. As many as a
/// look through a place for tokens visits (`tokens::found_under`'s `VISIT_MAX`).
const KEYS_WALK_MAX: usize = 10_000;

#[cfg(test)]
thread_local! {
    /// (Tests: `KEYS_WALK_MAX` for this thread's `decide` — a walk cut short without 10,000 files.)
    static WALK_MAX: std::cell::Cell<usize> = const { std::cell::Cell::new(KEYS_WALK_MAX) };
}

/// `KEYS_WALK_MAX` (tests: this thread's `WALK_MAX`).
fn keys_walk_max() -> usize {
    #[cfg(test)]
    return WALK_MAX.with(|m| m.get());
    #[cfg(not(test))]
    KEYS_WALK_MAX
}

/// An ask waiting for its answer.
#[derive(Clone, Debug)]
pub struct Pending {
    pub request_id: String,
    pub generation: u64,
    pub input: Value,
    pub look: Look,
    /// Where it reads, as the capability model sorts places (its card's `card` line — `cards.rs`).
    pub class: crate::cards::Class,
    /// When it came to wait — its place in the inbox (`inbox.rs`).
    pub since: std::time::Instant,
    /// What looks like a token where it reads (`tokens::found_in`) — its card says so.
    pub found: crate::tokens::Found,
    /// The repository outside the project it reads in, `a`/`A` may allow whole (`repository_of`, piece J).
    pub repository: Option<PathBuf>,
    /// The repository it reads in that `a`/`A` don't offer whole, and why (`repository_or_why`) — its card
    /// says so (frictions29 11).
    pub not_whole: Option<(PathBuf, String)>,
    /// Not Claude's call: what the user's own act would send with a file named like a key (`Own`) — no call
    /// waits on it.
    pub own: Option<Own>,
    /// A call of tarae's own tools (`search`, `read`) waits on it, not a permission ask: run again once
    /// allowed, else answered no.
    pub then: Option<Then>,
}

/// A call of tarae's in-process tools that reads where Read would ask (`Gate::judge`): it waits on the same
/// card, and runs again with what the card let through (`granted`) — the tools run in tarae, outside every
/// sandbox, so the read rules are theirs to keep.
#[derive(Clone, Debug)]
pub struct Then {
    pub call: crate::tools::Call,
    /// `search` · `read`.
    pub tool: &'static str,
    pub args: Value,
    /// Real paths the user let this call read (`y` once — `a`/`A` are kept grants the gate sees anyway).
    pub granted: Vec<PathBuf>,
}

/// What tarae's own tools that read go by — Read's rules (`decide`): gathered on the main thread, judged on a
/// worker.
pub struct Gate {
    /// What the session's `Caps` is made from (outside home read unasked where the capability model reads it:
    /// `Editor.read_outside`).
    basis: crate::caps::Basis,
    made: Made,
    granted: Vec<PathBuf>,
}

/// What a gate asks the user about: the read, where it is as the cards sort places, what looks like a token
/// there.
pub struct Ask {
    look: Look,
    class: crate::cards::Class,
    found: crate::tokens::Found,
    repository: Option<PathBuf>,
    not_whole: Option<(PathBuf, String)>,
}

impl Ask {
    /// What it would read, as the card names it. (Tests.)
    #[cfg(test)]
    pub fn at_said(&self) -> String {
        self.look.at.as_ref().map_or_else(|| self.look.given.clone(), |a| a.display().to_string())
    }
}

impl Gate {
    /// (Tests: the project `root` alone — nothing allowed, nothing kept, no temp folders.)
    #[cfg(test)]
    pub fn for_test(root: &Path) -> Gate {
        let root = crate::paths::real(root).unwrap_or_else(|| root.to_path_buf());
        let basis = crate::caps::Basis { root, ..Default::default() };
        Gate { basis, made: Made::default(), granted: Vec::new() }
    }

    /// (Tests: reading outside home where the capability model does, as tarae does.)
    #[cfg(test)]
    pub fn reading_outside(mut self) -> Gate {
        self.basis.outside = true;
        self
    }

    /// (Tests: as after the card let `at` through.)
    #[cfg(test)]
    pub fn granting(mut self, at: PathBuf) -> Gate {
        self.granted.push(at);
        self
    }

    /// For session `ed.session`'s chat, with what the user already let this call read.
    pub fn of(ed: &Editor, granted: Vec<PathBuf>) -> Gate {
        let made = ed.session.chat.as_ref().map(|c| Made::of(&c.ran)).unwrap_or_default();
        Gate { basis: ed.caps_basis(), made, granted }
    }

    /// Whether reading `at` (real, outside the project or named like a key in it) goes without asking —
    /// `file`: a file read whole (Read's rule), else a place searched (Grep's: the keys in it count). A
    /// worker's.
    pub fn judge(&self, at: &Path, file: bool) -> Result<(), Box<Ask>> {
        if self.granted.iter().any(|g| at.starts_with(g)) {
            return Ok(());
        }
        let (tool, input) = match file {
            true => ("Read", json!({ "file_path": at })),
            false => ("Grep", json!({ "path": at })),
        };
        let caps = self.basis.build();
        // (a folder's own files named like keys don't count: tarae's tools leave them out by name — `Keys::shut`)
        let (look, covered, class) = decide_with(tool, &input, &caps, &self.made, file);
        if covered || by_default(class, &caps) {
            return Ok(());
        }
        let found = look.at.as_deref().map(crate::tokens::found_in).unwrap_or_default();
        let (repository, not_whole) = split(look.at.as_deref().and_then(|at| repository_or_why(at, &caps)));
        Err(Box::new(Ask { look, class, found, repository, not_whole }))
    }
}

/// A call of tarae's own tools waits on the read card for `ask` (`Then`).
pub fn tool_ask(ed: &mut Editor, then: Then, ask: Ask) {
    let generation = then.call.generation();
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation() == generation) else {
        return crate::tools::answer(ed, then.call, Err("the chat it came from is gone".into()));
    };
    let Ask { mut look, class, found, repository, not_whole } = ask;
    // (tarae's search: its card says what it searches for — frictions29 33: `Gate::judge` knows only the place)
    if then.tool == "search" {
        let s = |k: &str| then.args[k].as_str().unwrap_or_default().to_string();
        (look.tool, look.pattern, look.filter) = ("Grep".into(), s("pattern"), s("glob"));
    }
    if let Some(no) = said_no(c, &look) {
        return crate::tools::answer(ed, then.call, Err(no));
    }
    c.reads.push_back(Pending {
        request_id: String::new(),
        generation,
        input: then.args.clone(),
        look,
        class,
        since: std::time::Instant::now(),
        found,
        repository,
        not_whole,
        own: None,
        then: Some(then),
    });
    refold(ed);
    next(ed);
}

/// What a read at or under a place the user said `n` to this turn (`Chat.read_no`) is told at once, with no card;
/// None: no such no.
fn said_no(c: &crate::chat::Chat, look: &Look) -> Option<String> {
    said_no_in(&c.read_no, look)
}

/// `said_no`, from the places said no to.
fn said_no_in(read_no: &[(PathBuf, bool)], look: &Look) -> Option<String> {
    let at = look.at.as_ref()?;
    let (no, _) = read_no.iter().find(|(n, under)| if *under { at.starts_with(n) } else { at == n })?;
    Some(format!(
        "the user said no to {} this turn — they aren't asked again until it ends; ask them in your answer if you \
         need it",
        no.display()
    ))
}

/// The card's answer to whoever asked: a permission ask allowed or denied (`no` its message), a tool's call
/// run again with the place let through, or answered no.
fn reply(ed: &mut Editor, p: Pending, allowed: bool, no: &str) {
    match p.then {
        Some(mut t) if allowed => {
            t.granted.extend(p.look.at);
            match t.tool {
                "search" => crate::nav::search_again(ed, t),
                _ => crate::readtool::again(ed, t),
            }
        }
        Some(t) => crate::tools::answer(ed, t.call, Err(no.to_string())),
        None if allowed => crate::chat::respond(ed, &p.request_id, allow(&p.input)),
        None => crate::chat::respond(ed, &p.request_id, json!({ "behavior": "deny", "message": no })),
    }
}

/// What the user's own act sends that holds a file named like a key Claude may not read — asked on the same
/// card (`ask_own`) before it goes: `y` its text goes with that one thing, `a`/`A` open the file, `n` its name
/// goes alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Own {
    /// An element pointed at (`C-o`), by its chip's id: the chat input's message with that chip (`point`).
    Chip(String),
    /// A note's message, its reply `text` (`notes::send`): the lines it's on.
    Note { id: u64, text: String, fresh: bool, why: Option<String> },
    /// The code review's comments (`codereview::send_as`): the lines they're on.
    Review,
}

/// Where the call reads — `cwd` is where relative paths start (claude's, the project's root). (Tests: `decide`
/// goes through `look_with`.)
#[cfg(test)]
pub fn look(tool: &str, input: &Value, cwd: &Path) -> Look {
    look_with(tool, input, cwd, true)
}

/// `look`; `folder_keys` false: a folder searched names none of the files named like keys in it (tarae's own
/// search leaves them out by name), only itself when it's one.
fn look_with(tool: &str, input: &Value, cwd: &Path, folder_keys: bool) -> Look {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let rel = |p: &str| if p.is_empty() { cwd.to_path_buf() } else { cwd.join(p) };
    let (given, pattern, filter) = match tool {
        "Read" => (s("file_path"), String::new(), String::new()),
        "Grep" => (s("path"), s("pattern"), s("glob")),
        _ => (s("path"), s("pattern"), String::new()),
    };
    let at = match tool {
        // Glob: the pattern's fixed folders are part of where it looks (`../../x/*.txt`, `/etc/*`)
        "Glob" => glob_root(&rel(&given), &pattern).and_then(|p| crate::paths::real(&p)),
        _ => crate::paths::real(&rel(&given)),
    };
    let file = at.as_ref().is_some_and(|p| p.is_file());
    // (a folder outside the project, or holding it, is walked only where it would go unasked — `decide_with`)
    let keys = match (tool, &at) {
        ("Read", Some(at)) if crate::sandbox::key_named(cwd, at) => vec![at.clone()],
        ("Grep", Some(at)) if at.is_file() || folder_keys && at.starts_with(cwd) => {
            grep_keys(cwd, at, &filter, &s("type"), None, None).unwrap_or_default()
        }
        // (a folder holding the project: the project's own, as ever — the rest of it where it'd go unasked)
        ("Grep", Some(at)) if folder_keys && cwd.starts_with(at) => {
            grep_keys(cwd, cwd, &filter, &s("type"), None, None).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    Look { tool: tool.to_string(), at, pattern, filter, given, file, keys, cut: false }
}

/// The files named like keys (`sandbox::key_named`) a Grep of `at` (real) opens, the project at `root`: `at`
/// itself, filters or not, or those in the folder — the project's own rule inside it, by name alone outside it
/// (a folder holding the project too) — that its file filters (`glob`, `type`) let through (`grep_opens`).
/// What else Claude Code's Grep may leave out (ignored or hidden files, links) isn't guessed: every one counts.
/// Not below a folder of tools' output, nor in `.git` (git's own files — neither walked:
/// `perf_grep_keys_in_this_repository`), nor in `skip` (the project, when `at` holds it: its own were looked for
/// already — a big one would cut the walk short). None: past `limit` entries, it can't tell.
fn grep_keys(
    root: &Path,
    at: &Path,
    glob: &str,
    kind: &str,
    limit: Option<usize>,
    skip: Option<&Path>,
) -> Option<Vec<PathBuf>> {
    // (a file named on ripgrep's command line is searched whatever `-g`/`-t` say: they filter what a walk
    // finds, never what it starts from)
    if at.is_file() {
        return Some(crate::sandbox::key_named(root, at).then(|| at.to_path_buf()).into_iter().collect());
    }
    let from = at;
    let outputs = crate::sandbox::outputs();
    let skip = skip.map(Path::to_path_buf);
    let walk = ignore::WalkBuilder::new(from)
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(move |e| {
            let name = e.file_name();
            e.depth() == 0
                || !e.file_type().is_some_and(|t| t.is_dir())
                || (name != ".git"
                    && !outputs.iter().any(|o| name == o.as_str())
                    && skip.as_deref() != Some(e.path()))
        })
        .build();
    let mut keys: Vec<PathBuf> = Vec::new();
    for (seen, e) in walk.flatten().enumerate() {
        if limit.is_some_and(|l| seen >= l) {
            return None;
        }
        // (a link to a file counts as the file it leads to)
        let file = match e.path_is_symlink() {
            true => crate::paths::real(e.path()).filter(|p| p.is_file()),
            false => e.file_type().is_some_and(|t| t.is_file()).then(|| e.into_path()),
        };
        keys.extend(
            file.filter(|p| crate::sandbox::key_named(root, p) && grep_opens(p, glob, kind, [from, root])),
        );
    }
    crate::sorting::sort(&mut keys);
    keys.dedup();
    Some(keys)
}

/// Whether Grep's file filters could let `file` through — where it can't be told, they do. Its `glob`, as
/// ripgrep's `--glob`: how the tool splits it isn't known, so each reading counts — whole, its words, its
/// parts between commas outside braces (`*.rs,*.pem`) — each part alone (several globs let through at most
/// what one of them alone does: the last that matches decides, and none matching lets through only when all
/// are `!`), from either of `roots` (where the search starts, the project), in either case. Its `type`:
/// ripgrep's own types. A filter that doesn't build lets it through.
fn grep_opens(file: &Path, glob: &str, kind: &str, roots: [&Path; 2]) -> bool {
    let readings = glob_readings(glob);
    let globbed = readings.is_empty()
        || readings.iter().any(|g| {
            roots.iter().any(|r| {
                [false, true].into_iter().any(|insensitive| {
                    let mut b = ignore::overrides::OverrideBuilder::new(r);
                    let built =
                        b.case_insensitive(insensitive).and_then(|b| b.add(g)).and_then(|b| b.build());
                    built.map_or(true, |o| !o.matched(file, false).is_ignore())
                })
            })
        });
    let typed = kind.is_empty() || {
        let mut t = ignore::types::TypesBuilder::new();
        t.add_defaults();
        t.select(kind);
        t.build().map_or(true, |t| !t.matched(file, false).is_ignore())
    };
    globbed && typed
}

/// Every glob `glob` may stand for: itself, its words, and each of those cut at commas outside braces.
fn glob_readings(glob: &str) -> Vec<&str> {
    let commas = |s: &str| -> Vec<(usize, usize)> {
        let (mut depth, mut from, mut out) = (0i32, 0, Vec::new());
        for (i, c) in s.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                ',' if depth <= 0 => {
                    out.push((from, i));
                    from = i + 1;
                }
                _ => {}
            }
        }
        out.push((from, s.len()));
        out
    };
    let mut all = vec![glob];
    for word in std::iter::once(glob).chain(glob.split_whitespace()) {
        all.push(word);
        all.extend(commas(word).into_iter().map(|(a, b)| word[a..b].trim()));
    }
    all.retain(|g| !g.is_empty());
    all.dedup();
    all
}

/// The folder a Glob pattern walks from `base`: its folders up to the first one with a wildcard (never its
/// last part — that's the names matched). None: a `..` past a wildcard (it could climb anywhere).
fn glob_root(base: &Path, pattern: &str) -> Option<PathBuf> {
    let wild = |c: &str| c.contains(['*', '?', '[', '{']);
    let parts: Vec<&str> = pattern.split('/').collect();
    let mut root = if pattern.starts_with('/') { PathBuf::from("/") } else { base.to_path_buf() };
    let fixed = parts.iter().take(parts.len().saturating_sub(1)).take_while(|c| !wild(c)).count();
    if parts[fixed..].contains(&"..") {
        return None;
    }
    for c in parts[..fixed].iter().filter(|c| !c.is_empty()) {
        root.push(c);
    }
    Some(root)
}

/// One of Claude's runs this turn (#81 — 12 shots under `/tmp` were 12 cards): what it made in the temp
/// folders while it went, and anything in its own folder (`sandbox::RunDir`, kept until the turn ends), is
/// read unasked. A run reads what it likes and could have printed it, so what it wrote holds nothing it
/// couldn't have said; a file made over there by something else in that time goes through too.
#[derive(Clone, Debug)]
pub struct Ran {
    pub from: SystemTime,
    /// None: still going.
    pub to: Option<SystemTime>,
    pub dir: Option<std::sync::Arc<crate::sandbox::RunDir>>,
}

/// What a worker checks a read against: each run's time and folder.
#[derive(Clone, Debug, Default)]
pub struct Made {
    pub runs: Vec<(SystemTime, Option<SystemTime>)>,
    pub dirs: Vec<PathBuf>,
    /// Where a run's files count (real paths): `/tmp` and the user's temp folder — not what holds
    /// that (`$TMPDIR`'s parent has the apps' caches), nor the toolchain's folders.
    pub temp: Vec<PathBuf>,
}

impl Made {
    pub fn of(ran: &[Ran]) -> Made {
        let temp = [
            crate::sandbox::profile_path(std::path::Path::new("/tmp")),
            crate::sandbox::profile_path(&std::env::temp_dir()),
        ];
        Made {
            runs: ran.iter().map(|r| (r.from, r.to)).collect(),
            dirs: ran.iter().filter_map(|r| r.dir.as_ref().map(|d| d.path().to_path_buf())).collect(),
            temp: temp.into(),
        }
    }

    /// Whether `at` (a real path) is in a run's folder, or was made in the temp folders while a run went
    /// (a folder made then counts with what's in it — a Grep or Glob of it). Made = its birth time.
    pub fn covers(&self, at: &Path, now: SystemTime) -> bool {
        self.covers_with(at, now, |p| std::fs::symlink_metadata(p).and_then(|m| m.created()).ok())
    }

    /// `covers` with the birth times `born` gives (tests stand in the kernel's stamps).
    fn covers_with(&self, at: &Path, now: SystemTime, born: impl Fn(&Path) -> Option<SystemTime>) -> bool {
        if self.dirs.iter().any(|d| at.starts_with(d)) {
            return true;
        }
        if !self.temp.iter().any(|t| at.starts_with(t) && at != t) || self.runs.is_empty() {
            return false;
        }
        let during = |t: SystemTime| self.runs.iter().any(|&(from, to)| from <= t && t <= to.unwrap_or(now));
        // The path, else a folder it's in made then (up to the temp folder itself)
        at.ancestors().take_while(|p| !self.temp.iter().any(|t| t == p)).any(|p| born(p).is_some_and(during))
    }
}

/// The files named like keys in the project Claude may read — opened on a read card or the run's panel
/// (`Grant::Key`): this conversation's, and the project's kept ones (read from the store the first time a
/// key-named file comes up, on the worker asking). tarae's own tools (`nav.rs`, `refactor.rs` — allowed
/// without asking, `chat::ALLOWED`) leave the others out, as Claude's Read asks for them.
#[derive(Clone, Debug)]
pub struct Keys {
    root: PathBuf,
    chat: Vec<PathBuf>,
    store: Option<PathBuf>,
    kept: std::sync::OnceLock<Vec<PathBuf>>,
}

impl Keys {
    pub fn of(ed: &Editor) -> Keys {
        Keys {
            root: ed.session.root.clone(),
            chat: ed.session.chat.as_ref().map(|c| c.allowed.keys.clone()).unwrap_or_default(),
            store: ed.allow_path.clone(),
            kept: Default::default(),
        }
    }

    /// None opened, in the project at `root`, nothing kept.
    #[cfg(test)]
    pub fn none_opened(root: &Path) -> Keys {
        Keys { root: root.to_path_buf(), chat: Vec::new(), store: None, kept: Default::default() }
    }

    /// These with `key` (real) opened for this conversation.
    #[cfg(test)]
    pub fn opening(mut self, key: PathBuf) -> Keys {
        self.chat.push(key);
        self
    }

    /// Whether `path` — where its links lead, as Read's `Look.keys` — is a file named like a key (in the
    /// project, or outside it by its name — `sandbox::key_named`) that isn't open to Claude. A worker's: it
    /// resolves the path, and may read the store.
    pub fn shut(&self, path: &Path) -> bool {
        let real = crate::paths::real(path).unwrap_or_else(|| path.to_path_buf());
        if !crate::sandbox::key_named(&self.root, &real) {
            return false;
        }
        let kept =
            self.kept.get_or_init(|| crate::allow::Store::read(self.store.as_deref()).get(&self.root).keys);
        !self.chat.contains(&real) && !kept.contains(&real)
    }
}

/// What tarae's tools say of a file `Keys` keeps out (`shown`: its path as the answer names it).
pub fn key_left_out(shown: &str) -> String {
    format!("{shown} is named like a key — tarae's tools leave it out; read asks the user for it on its card")
}

/// The text a place in such a file shows instead of its line.
pub const KEY_LINE: &str = "(named like a key — not read)";

/// What the message's context says of such a file in place of its text (`chat::context`).
pub const NOT_SENT: &str = "named like a key — not sent";

/// Whether the file at `path` (a document's — real since it was opened, `Editor::open`) is named like a key
/// (in the project, or outside it by its name — `sandbox::key_named`) and not open to Claude: not opened for this conversation nor kept for the project
/// (`Grant::Key` — the read card's `a`/`A`, the run's panel), nor in `pass` (what this one message may carry
/// — `input_pass`). The main thread's, for the message's context and the screen: no disk — the kept ones are
/// the chat's copy (`Chat.kept_keys`; not read yet, none counts as kept).
pub fn key_shut(ed: &Editor, path: &Path, pass: &[PathBuf]) -> bool {
    if !crate::sandbox::key_named(&ed.session.root, path) {
        return false;
    }
    let path = path.to_path_buf();
    let Some(c) = ed.session.chat.as_ref() else { return true };
    !(c.allowed.keys.contains(&path)
        || pass.contains(&path)
        || c.kept_keys.as_ref().is_some_and(|k| k.contains(&path)))
}

/// The files the chat input's next message may carry on a `y` (`Chat.key_pass`) — each only while the chip it
/// was given for is still in the input. Nothing else sent uses them (`chat::submit`).
pub fn input_pass(ed: &Editor) -> Vec<PathBuf> {
    let Some(c) = ed.session.chat.as_ref() else { return Vec::new() };
    c.key_pass.iter().filter(|(id, _)| c.pointed.contains(id)).map(|(_, k)| k.clone()).collect()
}

/// The panes on the last frame showing a file named like a key Claude may not read, as parts of the screen's
/// text to blank (`screen::text_hiding`) — what `C-s` attaches and Claude's `screen` tool gets, with the
/// context's marker in their place.
pub fn key_panes(ed: &Editor) -> Vec<crate::screen::Hidden> {
    let s = &ed.screen;
    let hidden = |path: PathBuf, (x, y, w, h): (usize, usize, usize, usize)| {
        if !key_shut(ed, &path, &[]) || h == 0 || w == 0 {
            return None;
        }
        let name = path.strip_prefix(&ed.session.root).unwrap_or(&path).display().to_string();
        // (too narrow for the file's name — the side panel: the marker alone, its heading names the file)
        let note = match format!("[{name} — {NOT_SENT}]") {
            n if n.chars().count() <= w => n,
            _ => format!("[{NOT_SENT}]"),
        };
        Some(crate::screen::Hidden { rows: (y, y + h - 1), cols: (x, x + w), note })
    };
    let docs = s.pane_docs.iter().map(Some).chain(std::iter::repeat(None));
    let panes = s.panes.iter().zip(docs).filter_map(|((view, rect, _, _), doc)| {
        // (the document the pane drew, else the one its view shows)
        let id = doc.copied().or_else(|| ed.session.views.iter().find(|v| v.id == *view).map(|v| v.doc))?;
        let path = ed.docs.iter().find(|d| d.id == id)?.path.clone()?;
        hidden(path, (rect.x, rect.y, rect.w, rect.h))
    });
    // The picker's preview card, over a file its row in hand shows
    let preview = s.picker_preview.and_then(|rect| {
        let action = &ed.picker.as_ref()?.current()?.action;
        previewed(ed, action).into_iter().find_map(|path| hidden(path, rect))
    });
    // The picker's rows carrying a line of one (a search hit's `path:12: text`, a reference, a diagnostic, a
    // jump, a note's line) — each row whole, its file named by the marker; headings and names stay
    let rows = ed.picker.as_ref().zip(s.picker_list.zip(s.picker_list_cols));
    let rows = rows.into_iter().flat_map(|(p, ((top, n), (x0, x1)))| {
        (0..n).filter_map(move |line| {
            let (path, _) = row_file(ed, Row::Picker(&p.item_at(p.row_at(line)?)?.action))?;
            hidden(path, (x0, top + line, x1.saturating_sub(x0), 1))
        })
    });
    // So are the side panel's (the list's items, the git tab's hunks), as drawn — not under a picker
    let lines =
        ed.session.sidebar.as_ref().filter(|_| s.side_w > 0 && s.picker_list.is_none()).map(|sb| sb.lines());
    let side: Vec<_> = match &lines {
        Some(lines) => s
            .sidebar
            .borrow()
            .iter()
            .filter_map(|&(y, i)| {
                let (path, _) = row_file(ed, Row::Side(&lines.get(i)?.target))?;
                hidden(path, (0, y, s.side_w, 1))
            })
            .collect(),
        None => Vec::new(),
    };
    panes.chain(preview).chain(rows).chain(side).collect()
}

/// A row of a list on screen, by what it stands for: a picker's, the side panel's.
#[derive(Clone, Copy)]
pub enum Row<'a> {
    Picker(&'a crate::picker::Action),
    Side(&'a crate::sidebar::Target),
}

/// The file whose line a list row shows, and that line (0-based) when known — the one rule for every list:
/// a picker's search hit, reference, diagnostic, note or jump; the side panel's list item (`path:12: text`)
/// or git hunk (its changed line). None for a row of names, a heading, a command.
pub fn row_file(ed: &Editor, row: Row) -> Option<(PathBuf, Option<usize>)> {
    use crate::picker::Action as A;
    use crate::sidebar::Target as T;
    let doc = |id| ed.docs.iter().find(|d| d.id == id).and_then(|d| d.path.clone());
    match row {
        Row::Picker(A::Goto { path, line, .. } | A::Note { path, line, .. }) => {
            Some((path.clone(), Some(*line)))
        }
        Row::Picker(A::Jump { doc: id, line, .. }) => Some((doc(*id)?, Some(*line))),
        Row::Side(T::Item(i)) => {
            ed.session.qlist.as_ref()?.items.get(*i).map(|it| (it.path.clone(), Some(it.line)))
        }
        Row::Side(T::Hunk { doc: id, .. }) => Some((doc(*id)?, None)),
        _ => None,
    }
}

/// What Claude's `ui` tool lists in place of a row's label when the row (`row_file`) shows a line of a file
/// named like a key Claude may not read: its path (and line) kept, the text the context's marker
/// (`uistate/claude.rs`) — None: the row as it is.
pub fn row_shut(ed: &Editor, row: Row) -> Option<String> {
    let (path, at) = row_file(ed, row)?;
    let name = path.strip_prefix(&ed.session.root).unwrap_or(&path).display().to_string();
    let at = at.map(|l| format!(":{}", l + 1)).unwrap_or_default();
    key_shut(ed, &path, &[]).then(|| format!("{name}{at}: ({NOT_SENT})"))
}

/// `row_shut` for the picker's visible row `line`.
pub fn picker_row_shut(ed: &Editor, line: usize) -> Option<String> {
    let p = ed.picker.as_ref()?;
    row_shut(ed, Row::Picker(&p.item_at(p.row_at(line)?)?.action))
}

/// `row_shut` for each side panel row drawn, by its id (`uistate::panels::side_id`).
pub fn side_rows_shut(ed: &Editor) -> Vec<(String, String)> {
    let Some(sb) = ed.session.sidebar.as_ref() else { return Vec::new() };
    let tab = crate::uistate::side_tab(sb.mode);
    let lines = sb.lines();
    let drawn = ed.screen.sidebar.borrow();
    drawn
        .iter()
        .filter_map(|&(_, i)| {
            let l = lines.get(i)?;
            Some((crate::uistate::panels::side_id(ed, tab, l), row_shut(ed, Row::Side(&l.target))?))
        })
        .collect()
}

/// The files a picker's preview card shows text of for a row's `action` (as `term.rs` draws it): a file, a
/// buffer or a jump's, a changed file's diff, a replace's, a code action's edits.
fn previewed(ed: &Editor, action: &crate::picker::Action) -> Vec<PathBuf> {
    use crate::picker::Action as A;
    let doc = |id| ed.docs.iter().find(|d| d.id == id).and_then(|d| d.path.clone());
    match action {
        A::Open(p) | A::Goto { path: p, .. } | A::Note { path: p, .. } => vec![p.clone()],
        A::Buffer(id) | A::Jump { doc: id, .. } => doc(*id).into_iter().collect(),
        A::ChangedFile(n) => ed.session.changed_files.get(*n).map(|c| c.path.clone()).into_iter().collect(),
        A::ReplaceFile(n) => ed.replace_plan.get(*n).map(|f| f.path.clone()).into_iter().collect(),
        A::Code(i) => match ed.action_preview(*i) {
            Some(crate::lsp_editor::ActionPreview::Ready(files)) => {
                files.iter().map(|f| f.path.clone()).collect()
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Reads the files named like keys the project keeps open (`A`) into the chat's copy, on a worker — when the
/// chat opens and after every change to what's kept (`allow::keep_queued`), so `key_shut` never waits on disk.
pub fn kept_keys_load(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.kept_keys_asked += 1;
    let asked = c.kept_keys_asked;
    let (store, root) = (ed.allow_path.clone(), ed.session.root.clone());
    // (but those taken back, their write not landed or failed — as the `Caps` leaves them out)
    let taking = ed.taking_for(&root);
    // (the project's, whatever claude process runs by then)
    ed.events.jobs().spawn(move || {
        let mut kept = crate::allow::Store::read(store.as_deref()).get(&root);
        taking.iter().for_each(|w| kept.remove(w));
        move |ed: &mut Editor| {
            let taking = ed.taking_for(&root);
            let Some(c) = ed.session.chat.as_mut() else { return };
            // (one asked since read what's kept after this — a take-back's write landed)
            if c.kept_keys_asked != asked {
                return;
            }
            let shut = |k: &PathBuf| {
                taking.iter().any(|w| crate::allow::takes_back(w, &crate::allow::Grant::Key(k.clone())))
            };
            c.kept_keys = Some(kept.keys.into_iter().filter(|k| !shut(k)).collect());
            // What waited for them goes now, decided on them (`wait_for_kept`)
            for own in std::mem::take(&mut c.after_kept) {
                match own {
                    Own::Note { id, text, fresh, why } => crate::notes::send(ed, id, &text, fresh, why, None),
                    Own::Review => crate::codereview::send_as(ed, None),
                    Own::Chip(_) => {}
                }
            }
        }
    });
}

/// Whether the user's own act `own` (a note, the review) waits for the project's kept keys — its `paths` hold
/// a file named like a key, and the chat's copy (`Chat.kept_keys`) isn't read yet (the chat just opened): it'd
/// ask about one already opened for the project (`A`). It goes when they're read (`kept_keys_load`).
pub fn wait_for_kept(ed: &mut Editor, paths: &[PathBuf], own: Own) -> bool {
    let root = ed.session.root.clone();
    let Some(c) = ed.session.chat.as_mut() else { return false };
    if c.kept_keys.is_some() || !paths.iter().any(|p| crate::sandbox::key_named(&root, p)) {
        return false;
    }
    let first = c.after_kept.is_empty();
    c.after_kept.push(own);
    // (one read under way is enough; a chat made elsewhere may have none)
    if first {
        kept_keys_load(ed);
    }
    true
}

/// The message went without these files' text (`chat::context`): said once a conversation in the chat, with
/// how to send it.
pub fn not_sent(ed: &mut Editor, keys: &[PathBuf]) {
    let root = ed.session.root.clone();
    let Some(c) = ed.session.chat.as_mut() else { return };
    let new: Vec<&PathBuf> = keys.iter().filter(|k| !c.keys_said.contains(k)).collect();
    if new.is_empty() {
        return;
    }
    let names: Vec<String> =
        new.iter().map(|k| k.strip_prefix(&root).unwrap_or(k).display().to_string()).collect();
    let (it, is) = if new.len() == 1 { ("its", "is") } else { ("their", "are") };
    let row = format!(
        "{} {is} named like a key — {it} text wasn't sent to Claude. To send it, point at it (C-o): y this \
         message, a this conversation, A always — as when Claude asks to read it, or the run's panel opens it",
        names.join(", ")
    );
    c.keys_said.extend(new.into_iter().cloned());
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Note, row, ""));
}

/// The file named like a key in the project, not open to Claude, whose text an element the user can point at
/// (`C-o`) carries or stands for: a pane showing it, a row of the list in it, a row of its outline. Names
/// alone (the files and git tabs, the review's rail) carry none.
pub fn pointed_key(ed: &Editor, id: &str, pass: &[PathBuf]) -> Option<PathBuf> {
    let path = match id.strip_prefix("pane#") {
        Some(_) => {
            let v = ed.session.views.iter().find(|v| format!("pane#{}", v.id) == id)?;
            ed.docs.iter().find(|d| d.id == v.doc)?.path.clone()?
        }
        None => {
            use crate::sidebar::Target as T;
            let sb = ed.session.sidebar.as_ref()?;
            let tab = crate::uistate::side_tab(sb.mode);
            let lines = sb.lines();
            let l = lines.iter().find(|l| crate::uistate::panels::side_id(ed, tab, l) == id)?;
            // (a row showing a file's line — `row_file`, every list's one rule — or an outline row of it)
            match &l.target {
                T::At(_) => ed.doc().path.clone()?,
                t => row_file(ed, Row::Side(t))?.0,
            }
        }
    };
    key_shut(ed, &path, pass).then_some(path)
}

/// The card holding `own` (the user's own act — `ask_own`), if one is up.
fn own_card(w: &crate::offer::What, own: impl Fn(&Own) -> bool) -> bool {
    matches!(w, crate::offer::What::Read { pending, .. } if pending.own.as_ref().is_some_and(own))
}

/// The user pointed (`C-o`) at `id`, and it carries a file named like a key Claude may not read: the card asks
/// (`ask_own`), and the chat input's message with its chip waits for the answer (`chat::send`).
pub fn point(ed: &mut Editor, id: &str) {
    if let Some(key) = pointed_key(ed, id, &input_pass(ed)) {
        ask_own(ed, vec![key], Own::Chip(id.to_string()));
    }
}

/// The user's own act would send `keys` (files named like keys in the project, not open to Claude) — their
/// act, so they aren't sent silently, nor left out silently: the read card asks first, holding `own` until
/// the answer (`own_answer`) — `y` this one thing · `a` this conversation · `A` always · `n` their names only.
pub fn ask_own(ed: &mut Editor, keys: Vec<PathBuf>, own: Own) {
    let Some(c) = ed.session.chat.as_ref() else { return };
    // (one card for the same thing — a chip, a note, the review)
    let same = |o: &Own| match (o, &own) {
        (Own::Chip(a), Own::Chip(b)) => a == b,
        (Own::Note { id: a, .. }, Own::Note { id: b, .. }) => a == b,
        (Own::Review, Own::Review) => true,
        _ => false,
    };
    if ed.session_cards().any(|o| own_card(&o.what, same)) {
        return;
    }
    let root = &ed.session.root;
    let names: Vec<String> =
        keys.iter().map(|k| k.strip_prefix(root).unwrap_or(k).display().to_string()).collect();
    let given = names.join(", ");
    let (it, is) = if keys.len() == 1 { ("it", "It's") } else { ("them", "They're") };
    let (what, y) = match &own {
        Own::Chip(_) => (
            format!("you pointed at {it}"),
            "y: your message carries its text, as it would any file's — n: its name only",
        ),
        Own::Note { .. } => {
            ("your note is on its lines".into(), "y: the note goes with them — n: with its name only")
        }
        Own::Review => (
            format!("your review comments are on lines of {it}"),
            "y: the review goes with them — n: with the names only",
        ),
    };
    let look = Look {
        tool: "Read".into(),
        at: keys.first().cloned(),
        pattern: String::new(),
        filter: String::new(),
        given,
        file: true,
        keys,
        cut: false,
    };
    let p = Pending {
        request_id: String::new(),
        generation: c.generation(),
        input: Value::Null,
        look,
        class: crate::cards::Class::Secret,
        since: std::time::Instant::now(),
        found: Default::default(),
        repository: None,
        not_whole: None,
        own: Some(own),
        then: None,
    };
    let keep = p
        .look
        .keys
        .iter()
        .all(|k| crate::caps::Caps::storable(&crate::allow::Grant::Key(k.clone())) == Scope::Always);
    let mut q = format!("{}\n\n{is} named like a key, and {what}. {y}.", p.look.given);
    if keep {
        let only = if p.look.keys.len() == 1 { "that file only" } else { "those files only" };
        q.push_str(&format!("\n\na, s and A open {it} to Claude and its commands too — {only}."));
    }
    let asked = crate::cards::Asked::new(crate::cards::Why::Read, crate::cards::Request::Read(p.class))
        .about(&p.look.given);
    let what = crate::offer::What::Read { pending: Box::new(p), keep, folder: None };
    ed.push_offer(crate::offer::Offer::new(POINTED_TITLE.into(), &q, "", String::new(), what).asked(asked));
}

/// A chip taken off the chat's input: its `y` goes, and a card its pointing raised goes with it.
pub fn unpointed(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    let pointed = c.pointed.clone();
    c.key_pass.retain(|(id, _)| pointed.contains(id));
    ed.drop_cards_for(
        |w| own_card(w, |o| matches!(o, Own::Chip(id) if !pointed.contains(id))),
        "dropped — what pointed at it was taken off",
    );
}

/// A new conversation: a `y` given for the last one's message goes, and so does a card pointing raised (a
/// note's or the review's waits — they go into the new one).
pub fn conversation_new(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        (c.key_pass, c.keys_said) = (Vec::new(), Vec::new());
    }
    ed.drop_cards_for(|w| own_card(w, |o| matches!(o, Own::Chip(_))), "dropped — a new conversation");
}

/// Whether a card asks about a file named like a key the user pointed at — the chat input's message waits
/// for it.
pub fn pointing_asks(ed: &Editor) -> bool {
    ed.session_cards().any(|o| own_card(&o.what, |o| matches!(o, Own::Chip(_))))
}

/// Whether the call may go without asking: inside the project, in a place read by default or allowed
/// (`caps`), tarae's own log (the file itself — `log::is_log_real`), or made by one of Claude's runs this turn
/// (`Made`) — and every file named like a key it opens opened (`Grant::Key`, the same grant a run's reads go
/// by).
pub fn covered(look: &Look, caps: &crate::caps::Caps, made: &Made) -> bool {
    !look.cut && look.keys.iter().all(|k| caps.opens_key(k)) && place_covered(look, caps, made)
}

/// `covered`'s place alone, whatever files named like keys it opens.
fn place_covered(look: &Look, caps: &crate::caps::Caps, made: &Made) -> bool {
    look.at
        .as_ref()
        .is_some_and(|at| caps.reads(at) || crate::log::is_log_real(at) || made.covers(at, SystemTime::now()))
}

/// One of Claude's runs started (`tools::go`) — by the clock its files are stamped by (`file_clock`).
pub fn run_started(ed: &mut Editor, dir: Option<std::sync::Arc<crate::sandbox::RunDir>>) {
    let from = ed.run_clock.unwrap_or(file_clock)();
    if let Some(c) = ed.session.chat.as_mut() {
        c.ran.push(Ran { from, to: None, dir });
    }
}

/// Now, as the kernel stamps a file made now. Linux stamps from its coarse clock — the time of the last
/// tick, 1–10 ms behind the fine one — so a file a run made a moment after `SystemTime::now()` was read
/// for its start was born "before" it, and asked (seen in the Linux container: a shot written right after
/// the start). Its own coarse clock is never past such a stamp. macOS stamps from the fine clock.
#[cfg(target_os = "linux")]
pub fn file_clock() -> SystemTime {
    // SAFETY: a timespec is plain integers (all-zero is valid); clock_gettime writes the one it's given.
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut t) } != 0 {
        return SystemTime::now();
    }
    SystemTime::UNIX_EPOCH + std::time::Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

#[cfg(not(target_os = "linux"))]
pub fn file_clock() -> SystemTime {
    SystemTime::now()
}

/// Claude's run ended (or was dropped): its time is over.
pub fn run_ended(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        let now = SystemTime::now();
        for r in c.ran.iter_mut().filter(|r| r.to.is_none()) {
            r.to = Some(now);
        }
    }
}

/// A `can_use_tool` ask for one of `TOOLS` — decided on a worker (paths resolved, the kept answers read),
/// then allowed, or put to the user.
pub fn request(ed: &mut Editor, request_id: &str, req: &Value) -> Option<Value> {
    let tool = req["tool_name"].as_str().unwrap_or_default().to_string();
    let c = ed.session.chat.as_ref()?;
    let (generation, made) = (c.generation(), Made::of(&c.ran));
    let input = req["input"].clone();
    let request_id = request_id.to_string();
    // (the root, a real path, is where claude runs — its relative paths are from there)
    let basis = ed.caps_basis();
    let (llm, here) = (ed.config.llm.clone(), ed.sandbox_here.clone());
    ed.events.jobs().spawn(move || {
        let here = || here.map_or_else(crate::sandbox::Here::now, |h| h());
        let caps = caps_for(&tool, &input, basis.build(), &llm, here);
        let (look, covered, class) = decide(&tool, &input, &caps, &made);
        let ok = covered || by_default(class, &caps);
        // (asked: its card says first what looks like a token there — a folder in the project too: its yes
        // reads more than the keys it names)
        let (found, (repository, not_whole)) = match (ok, &look.at) {
            (false, Some(at)) => (crate::tokens::found_in(at), split(repository_or_why(at, &caps))),
            _ => Default::default(),
        };
        Box::new(move |ed: &mut Editor| {
            let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation() == generation) else { return };
            if ok {
                return crate::chat::respond(ed, &request_id, allow(&input));
            }
            if let Some(no) = said_no(c, &look) {
                return crate::chat::respond(ed, &request_id, json!({ "behavior": "deny", "message": no }));
            }
            let since = std::time::Instant::now();
            let own = None;
            c.reads.push_back(Pending {
                request_id,
                generation,
                input,
                look,
                class,
                since,
                found,
                repository,
                not_whole,
                own,
                then: None,
            });
            refold(ed);
            next(ed);
        })
    });
    None
}

/// What `tool` reading `input` (from `caps.root`, where claude runs) reads, and whether it may go without
/// asking: inside the checkout, a place read by default or allowed — a file named like a key, wherever it is,
/// only once opened (`Look.keys`, `sandbox::key_named`: frictions29, a key made in a granted folder was read
/// unasked) — — this conversation's, kept for always (`caps`) —, or made by a run of this turn (`made`)
/// — its class too (the card's line says it): the chat's reads go without asking where the capability model
/// reads by default as well (`by_default`). The chat asks the rest on a card; the reviewer refuses it
/// (`reviewer.rs`: the checkout only).
pub fn decide(
    tool: &str,
    input: &Value,
    caps: &crate::caps::Caps,
    made: &Made,
) -> (Look, bool, crate::cards::Class) {
    decide_with(tool, input, caps, made, true)
}

/// `decide`, a folder's own files named like keys counted or not (`look_with`).
fn decide_with(
    tool: &str,
    input: &Value,
    caps: &crate::caps::Caps,
    made: &Made,
    folder_keys: bool,
) -> (Look, bool, crate::cards::Class) {
    let mut look = look_with(tool, input, &caps.root, folder_keys);
    // (a folder outside the project, or holding it, searched where it would go unasked — a place read, or one
    // read by default: its files named like keys looked for now, so many entries at most; past them, it asks.
    // Where a card would ask for the place, nothing is walked: no Grep reaches that card today — the chat has no
    // Grep or Glob (user decision 2026-10-13, `chat::tools`), and the reviewer, whose Grep comes here, never asks
    // (`reviewer::read` refuses what isn't covered). A Grep back in the chat would need the walk before its card,
    // or the card's yes would let the keys there through — frictions29 review)
    if let Some(at) = look
        .at
        .clone()
        .filter(|at| folder_keys && look.tool == "Grep" && !look.file && !at.starts_with(&caps.root))
        && (place_covered(&look, caps, made) || by_default(caps.class(Some(&at)), caps))
        && may_hold_closed(caps, &at)
    {
        let kind = input["type"].as_str().unwrap_or_default();
        // (a folder holding the project: the project's own were named by `look_with` — not walked again)
        let skip = caps.root.starts_with(&at).then_some(caps.root.as_path());
        match grep_keys(&caps.root, &at, &look.filter, kind, Some(keys_walk_max()), skip) {
            Some(keys) => look.keys.extend(keys),
            None => look.cut = true,
        }
        crate::sorting::sort(&mut look.keys);
        look.keys.dedup();
    }
    // (named like one, closed where runs close it — one answer)
    look.keys.retain(|k| key_closed(caps, k));
    let covered = covered(&look, caps, made);
    // (the card names those still closed)
    look.keys.retain(|k| !caps.opens_key(k));
    let class = class_of(&look, caps);
    (look, covered, class)
}

/// Whether `real`, named like a key (`sandbox::key_named`), stays closed to Claude's reads until it's opened —
/// in home where runs close it (frictions29 round 3: one answer): in the project by its rule there, and outside it
/// by the runs' rule as `caps` would give a run now (`Caps.key_rules`, `with_key_rules`): closed where another
/// program or session could have put it (a granted folder, another checkout), read where runs read it (a
/// toolchain's folders, an install). Outside home always (cardauto D3: what's read there by default is never a
/// name like a key) — runs read one there that isn't in a place they close (threat model §1.3 "not kept"); the
/// temp folders, a write grant there, close it for both. Unknown (None): closed by its name alone.
pub fn key_closed(caps: &crate::caps::Caps, real: &Path) -> bool {
    if !crate::sandbox::key_named(&caps.root, real) {
        return false;
    }
    let outside_home = caps.home.as_ref().is_none_or(|h| !real.starts_with(h));
    // (a setting runs read from a copy: the file itself is a key's to Claude)
    real.starts_with(&caps.root)
        || outside_home
        || caps.key_rules.as_ref().is_none_or(|r| r.closes(real) || r.copied(real))
}

/// Whether a folder `at` outside the project may hold a file `key_closed` closes: outside home any; in home only
/// where the runs' rule may close one below it (`KeyRules::may_close_below` — not a toolchain's folder, where a
/// big walk would only cut the search short: review). Not known: any.
fn may_hold_closed(caps: &crate::caps::Caps, at: &Path) -> bool {
    let outside_home = caps.home.as_ref().is_none_or(|h| !at.starts_with(h));
    outside_home || caps.key_rules.as_ref().is_none_or(|r| r.may_close_below(at))
}

/// `caps` for deciding `tool` reading `input` — with the runs' key rule (`with_key_rules`) when the call may open a
/// file named like a key outside the project (`may_open_key`): the chat's reads and the reviewer's go by it.
pub(crate) fn caps_for(
    tool: &str,
    input: &Value,
    caps: crate::caps::Caps,
    llm: &crate::llm::LlmConfig,
    here: impl FnOnce() -> crate::sandbox::Here,
) -> crate::caps::Caps {
    match may_open_key(tool, input, &caps.root) {
        true => with_key_rules(caps, llm, &here()),
        false => caps,
    }
}

/// `caps` with the runs' rule for the files named like keys a run would have now (`sandbox::key_rules_for`), as
/// macOS's profile has it on every system — Linux's runs close them outside the project only in tier own, by the
/// mounts that hide what's found before each run (D-i; Landlock only grants), the rule Claude's reads go by is the
/// one designed. A worker's: it looks at the toolchains' folders.
pub fn with_key_rules(
    mut caps: crate::caps::Caps,
    llm: &crate::llm::LlmConfig,
    here: &crate::sandbox::Here,
) -> crate::caps::Caps {
    let w = crate::tools::with_allowed(llm, &caps, None);
    let rules = crate::sandbox::key_rules_for(&caps.root, w, here).designed();
    caps.key_rules = Some(std::sync::Arc::new(rules));
    caps
}

/// Whether `tool` reading `input` (from `root`) may open a file named like a key outside the project — a Read of
/// one by its name, a Grep of a place outside the project or holding it — so its decision needs the runs' rule
/// (`with_key_rules`): looked for only then (in the project `key_closed` never reads it — review).
fn may_open_key(tool: &str, input: &Value, root: &Path) -> bool {
    let at = |k: &str| crate::paths::real(&root.join(input[k].as_str().unwrap_or_default()));
    match tool {
        "Read" => {
            at("file_path").is_some_and(|p| !p.starts_with(root) && crate::sandbox::key_named(root, &p))
        }
        "Grep" => at("path").is_some_and(|p| !p.starts_with(root)),
        _ => false,
    }
}

/// Whether the capability model reads a place of `class` without asking (`cards::decide` — the one rule, by
/// the session's `caps`): outside home too where it reads it, as runs do (D3) — but others' homes, volumes, and
/// a file named like a key.
pub fn by_default(class: crate::cards::Class, caps: &crate::caps::Caps) -> bool {
    crate::cards::decide(&crate::cards::Request::Read(class), caps) == crate::cards::Decision::None
}

/// Where a read goes, as the capability model sorts places: what's named like a key — in the project, still
/// closed (`Look.keys`), or the place itself, wherever (there or not) — is `Secret`; else its place's class
/// (`cards::class`).
fn class_of(look: &Look, caps: &crate::caps::Caps) -> crate::cards::Class {
    // (a file is in `Look.keys` where it's closed — by its name alone only a folder)
    let named_key = !look.file && look.at.as_deref().is_some_and(crate::secretnames::named_secret);
    match look.keys.is_empty() && !named_key && !look.cut {
        true => caps.class(look.at.as_deref()),
        false => crate::cards::Class::Secret,
    }
}

/// A read come to wait beside the one on the card up (it came after the card): the card offers their folder
/// now — said on it, and settled again before `a` counts (#102: Claude's reads come one after another) —
/// once the folder's been looked through for tokens (`scanned`), so the card says what it holds first.
fn refold(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_ref() else { return };
    let others = others(c);
    let home = crate::allow::home();
    let front = !ed.background();
    // (each card offering its place alone)
    let alone: Vec<(u64, Pending)> = ed
        .session
        .parked
        .iter()
        .chain(ed.offers.iter().filter(|_| front))
        .filter_map(|o| match &o.what {
            crate::offer::What::Read { pending, keep: true, folder: None } => {
                Some((o.id, (**pending).clone()))
            }
            _ => None,
        })
        .collect();
    for (id, p) in alone {
        let offered = folder_for(ed, &p, &others, home.as_deref());
        // (none offered yet, but why not its repository is known now — its look through for tokens came back)
        let why = not_whole(ed, &p, offered.as_ref().map(|(f, _)| f.as_path()));
        if offered.is_none() && why.is_none() {
            continue;
        }
        let (mut q, warning) =
            card_text(&p, true, offered.as_ref().map(|(f, found)| (f.as_path(), found)), why.as_deref());
        let f = offered.map(|(f, _)| f);
        let what = crate::offer::What::Read { pending: Box::new(p), keep: true, folder: f.clone() };
        if let Some(line) = ed.always_line(&what) {
            q.push_str(&format!("\n\n{line}"));
        }
        let cards = ed.session.parked.iter_mut().chain(ed.offers.iter_mut().filter(|_| front));
        for o in cards.filter(|o| o.id == id && o.question != q) {
            let crate::offer::What::Read { folder, .. } = &mut o.what else { continue };
            (o.question, o.warning) = (q.clone(), warning.clone());
            *folder = f.clone();
            o.rev += 1;
        }
    }
}

/// Why `a`/`A` on `p`'s card don't open the repository it reads in, as the card says it — `folder`: what they
/// open instead (None: its place alone). None: they open the repository, or there's none to speak of, or the
/// read is a key's (opened one by one). frictions29 11: a reference repository came a card per path, and
/// nothing said why it wasn't offered whole.
fn not_whole(ed: &Editor, p: &Pending, folder: Option<&Path>) -> Option<String> {
    let at = p.look.at.as_ref().filter(|_| p.look.keys.is_empty())?;
    if p.class == crate::cards::Class::Secret || crate::secretnames::named_secret(at) {
        return None;
    }
    if let Some((top, why)) = &p.not_whole {
        return Some(format!("Not offered whole, the repository {}: {why}.", crate::allow::tilde(top)));
    }
    let r = p.repository.as_ref().filter(|r| folder != Some(r.as_path()))?;
    let found = ed.session.chat.as_ref()?.scanned.get(r)?.as_ref()?;
    let n = found.files.len() + found.named.len();
    let why = match (found.cut, n) {
        (true, _) => "too big to look through for tokens".to_string(),
        (false, 0) => return None,
        (false, 1) => "a file in it looks like it holds a token".to_string(),
        (false, n) => format!("{n} files in it look like they hold a token"),
    };
    Some(format!("Not offered whole, the repository {}: {why}.", crate::allow::tilde(r)))
}

/// What `a`/`A` allow on `p`'s card besides its place, once looked through for tokens (`scanned` — till then
/// None: the card offers the place alone, `refold` the rest later): the repository it reads in (piece J,
/// `Pending.repository`) when all of it was looked through and nothing there looks like a token; else, for a
/// file beside other reads, its folder (#102), with what that holds said.
fn folder_for(
    ed: &mut Editor,
    p: &Pending,
    others: &[PathBuf],
    home: Option<&Path>,
) -> Option<(PathBuf, crate::tokens::Found)> {
    let at = p.look.at.as_ref().filter(|_| p.look.keys.is_empty())?;
    // (never for a read that's itself a key's or named like one — review J 5; `look.keys` names those outside
    // the project too, frictions29)
    let secret = p.class == crate::cards::Class::Secret || crate::secretnames::named_secret(at);
    if let Some(r) = p.repository.as_ref().filter(|_| !secret) {
        let found = scanned(ed, r)?;
        if found.files.is_empty() && found.named.is_empty() && !found.cut {
            return Some((r.clone(), found));
        }
    }
    let places = crate::allow::grant_places(std::slice::from_ref(at), others, home);
    let f = places.into_iter().next_back().filter(|f| f != at && p.look.file)?;
    let found = scanned(ed, &f)?;
    Some((f, found))
}

/// The top of the git checkout `at` (real, outside the project) is in, which `a`/`A` may allow whole (piece J): a
/// repository's (git's links both ways — `allow::common_dir`), `at`'s folder or above, neither the project nor
/// holding it, not home or above, not in or holding a place that holds keys or that runs never read
/// (`allow::keepable`, `caps.kept`), nor where what's there runs later (`caps.later`). Whether it holds what looks
/// like a token is looked at before it's offered (`folder_for`). A worker's: it reads the disk. (Tests' form of
/// `repository_or_why`.)
#[cfg(test)]
pub fn repository_of(at: &Path, caps: &crate::caps::Caps) -> Option<PathBuf> {
    repository_or_why(at, caps)?.ok()
}

/// The repository `at` is in that `a`/`A` may allow whole (`repository_of`'s rules) — and when there's one it
/// doesn't offer, which and why (Err): the read card says it (frictions29 11). None: no repository to speak of
/// (none, the project's, or the place read is its top).
pub fn repository_or_why(at: &Path, caps: &crate::caps::Caps) -> Option<Result<PathBuf, (PathBuf, String)>> {
    let home = caps.home.as_deref();
    if caps.root.as_os_str().is_empty() || at.starts_with(&caps.root) {
        return None;
    }
    // (a folder searched is where the climb starts: one that's a repository's top is that repository — nothing
    // wider is offered, review J 7)
    let start = if at.is_dir() { at } else { at.parent()? };
    let top = start
        .ancestors()
        .take_while(|a| home.is_none_or(|h| !h.starts_with(a)))
        .find(|a| crate::allow::has_git(a))?;
    if top == at {
        return None;
    }
    let why = if crate::allow::common_dir(top).is_none() {
        Some("its git folder doesn't name it as one of its checkouts".to_string())
    } else if caps.root.starts_with(top) {
        Some("it holds this project".into())
    } else if !crate::allow::keepable(top, home) {
        Some("it's a place whose reads are never kept".into())
    } else if caps.kept.iter().any(|k| k.starts_with(top) || top.starts_with(k)) {
        Some("it's in or holds a place commands never read".into())
    } else {
        caps.later.refuses(top)
    };
    Some(match why {
        None => Ok(top.to_path_buf()),
        Some(why) => Err((top.to_path_buf(), why)),
    })
}

/// `repository_or_why`'s answer as a pending read keeps it: what `a`/`A` may open whole, what they don't and why.
fn split(r: Option<Result<PathBuf, (PathBuf, String)>>) -> (Option<PathBuf>, Option<(PathBuf, String)>) {
    match r {
        Some(Ok(top)) => (Some(top), None),
        Some(Err(why)) => (None, Some(why)),
        None => (None, None),
    }
}

/// What folder `f` holds that looks like a token, once it's been looked through; else None — and it's
/// looked through on a worker, the cards offering it once it has (`refold`).
fn scanned(ed: &mut Editor, f: &Path) -> Option<crate::tokens::Found> {
    let c = ed.session.chat.as_mut()?;
    if let Some(found) = c.scanned.get(f) {
        return found.clone();
    }
    c.scanned.insert(f.to_path_buf(), None);
    let (generation, f) = (c.generation(), f.to_path_buf());
    ed.events.jobs().spawn(move || {
        let found = crate::tokens::found_in(&f);
        move |ed: &mut Editor| {
            let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation() == generation) else { return };
            c.scanned.insert(f, Some(found));
            refold(ed);
        }
    });
    None
}

/// The card's text, and its lines that warn: what `y` reads — or what `a`/`A` open (`folder`, with what it
/// holds) — holds a token-like line (names, never values).
pub fn card_text(
    p: &Pending,
    keep: bool,
    folder: Option<(&Path, &crate::tokens::Found)>,
    not_whole: Option<&str>,
) -> (String, Option<std::ops::Range<usize>>) {
    let whole = folder.map(|(f, _)| (f, p.repository.as_deref() == Some(f)));
    let mut q = question(&p.look, keep, whole);
    if let Some(why) = not_whole.filter(|_| keep) {
        q.push_str(&format!("\n\n{why}"));
    }
    let said =
        [p.look.at.as_deref().and_then(|at| p.found.said(at)), folder.and_then(|(f, found)| found.said(f))];
    let mut lines = Vec::new();
    for s in said.into_iter().flatten() {
        q.push_str("\n\n");
        lines.push(q.split('\n').count() - 1);
        q.push_str(&s);
    }
    let warning = lines.first().zip(lines.last()).map(|(a, b)| *a..*b + 1);
    (q, warning)
}

fn allow(input: &Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": input })
}

pub const TITLE: &str = "Claude wants to read — outside the project";

/// A read that opens files named like keys (`Look.keys`) — in the project, or outside it by their names.
pub const KEYS_TITLE: &str = "Claude wants to read — named like a key";

/// The user pointed at a file named like a key (`point`).
pub const POINTED_TITLE: &str = "Send to Claude — named like a key";

/// The places other reads go (waiting, or let through once) — a card's folder for longer is offered beside
/// them (#102). Not the files named like keys: those are opened one by one.
fn others(c: &crate::chat::Chat) -> Vec<PathBuf> {
    let waiting = c.reads.iter().filter(|w| w.look.keys.is_empty()).filter_map(|w| w.look.at.clone());
    waiting.chain(c.read_once.iter().cloned()).collect()
}

/// The next waiting ask on its card, once none is up.
pub fn next(ed: &mut Editor) {
    if ed.session_cards().any(|o| matches!(o.what, crate::offer::What::Read { .. })) {
        return;
    }
    let Some(c) = ed.session.chat.as_mut() else { return };
    let Some(p) = c.reads.pop_front() else { return };
    let home = crate::allow::home();
    let keepable = |at: &Path| crate::allow::keepable(at, home.as_deref());
    // (a file named like a key: once at a time — `Caps::storable`)
    let key_kept =
        |k: &PathBuf| crate::caps::Caps::storable(&crate::allow::Grant::Key(k.clone())) == Scope::Always;
    // (nor a search too big to look through for them: a longer yes would hold for nothing — it asks again)
    let keep = p.look.at.as_deref().is_some_and(keepable) && p.look.keys.iter().all(key_kept) && !p.look.cut;
    // (its repository, or — beside another read waiting or let through once — its folder for longer: #102;
    // once it's been looked through for tokens — till then the place alone: `refold`)
    let others = others(c);
    let folder = keep.then(|| folder_for(ed, &p, &others, home.as_deref())).flatten();
    let why = not_whole(ed, &p, folder.as_ref().map(|(f, _)| f.as_path()));
    let (q, warning) =
        card_text(&p, keep, folder.as_ref().map(|(f, found)| (f.as_path(), found)), why.as_deref());
    let asked = crate::cards::Asked::new(crate::cards::Why::Read, crate::cards::Request::Read(p.class))
        .about(&p.look.given);
    // (a file named like a key — in the project, or outside it by its name: the key card, `y` once)
    let title = if !p.look.keys.is_empty() { KEYS_TITLE } else { TITLE };
    let what = crate::offer::What::Read { pending: Box::new(p), keep, folder: folder.map(|(f, _)| f) };
    let mut o = crate::offer::Offer::new(title.into(), &q, "", String::new(), what).asked(asked);
    o.warning = warning;
    ed.push_offer(o);
}

/// The files named like keys a card lists by name; the rest are counted.
const KEYS_SHOWN: usize = 5;

/// The card's text: exactly what's read (the files named like keys it opens by name), and where it goes —
/// and, when `a`/`A` allow more than its place (`folder`: a folder, and whether it's the repository it reads
/// in), that.
pub fn question(look: &Look, keep: bool, folder: Option<(&Path, bool)>) -> String {
    let at = match &look.at {
        Some(at) => at.display().to_string(),
        None => format!("{} (where it leads can't be told)", look.given),
    };
    let mut q = match look.tool.as_str() {
        "Read" => at,
        "Grep" => {
            let all = if look.file { "" } else { " and everything in it" };
            let mut s = format!("searches {at}{all}\nfor {}", pattern_shown(&look.pattern).0);
            if !look.filter.is_empty() {
                s.push_str(&format!("\nin files {}", look.filter));
            }
            s
        }
        _ => format!("lists {} in {at} and everything in it", look.pattern),
    };
    match look.keys.as_slice() {
        [] => {}
        [k] if look.at.as_ref() == Some(k) => q.push_str("\n\nIt's named like a key."),
        keys => {
            q.push_str("\n\nIt opens files named like keys:");
            for k in keys.iter().take(KEYS_SHOWN) {
                let k = look.at.as_deref().and_then(|at| k.strip_prefix(at).ok()).unwrap_or(k);
                q.push_str(&format!("\n    {}", k.display()));
            }
            if keys.len() > KEYS_SHOWN {
                q.push_str(&format!("\n    and {} more", keys.len() - KEYS_SHOWN));
            }
        }
    }
    if look.cut {
        q.push_str("\n\nIt holds too much to look through for files named like keys — a yes lets it search any there.");
    }
    q.push_str("\n\nWhat it reads goes to Claude.");
    if keep && !look.keys.is_empty() {
        let (them, only) = if look.keys.len() == 1 { ("it", "that file") } else { ("them", "those files") };
        q.push_str(&format!("\n\na, s and A open {them} to its commands too — {only} only."));
    }
    if !keep && look.at.is_some() {
        q.push_str(" Only once — a place like this is never kept.");
    }
    let only = if look.file || look.tool == "Read" { "this file" } else { "this folder" };
    match folder {
        Some((f, true)) => q.push_str(&format!(
            "\n\na, s and A allow everything in {} — the repository it's in, not only {only}.",
            f.display()
        )),
        Some((f, false)) => {
            q.push_str(&format!("\n\na, s and A allow everything in {} — not only {only}.", f.display()))
        }
        None => {}
    }
    q
}

/// A search's pattern a card shows at most this many characters of — `v` shows it whole.
const PATTERN_SHOWN: usize = 60;

/// A search's pattern as a card or its `v` shows it: as it is, or escaped when it holds what a card can't show as it
/// is (a line's end, a tab, control or bidi marks — `tools::draws_as_itself`).
pub fn pattern_text(pattern: &str) -> String {
    match crate::tools::draws_as_itself(pattern) && !pattern.contains(['\n', '\t']) {
        true => pattern.to_string(),
        false => pattern.escape_debug().to_string(),
    }
}

/// A search's pattern as its card shows it (frictions29 33: it showed none) — `pattern_text`, at most
/// `PATTERN_SHOWN` characters and `…` — and whether it was cut.
pub fn pattern_shown(pattern: &str) -> (String, bool) {
    use unicode_segmentation::UnicodeSegmentation as _;
    let text = pattern_text(pattern);
    let mut g = text.graphemes(true);
    let shown: String = g.by_ref().take(PATTERN_SHOWN).collect();
    match g.next() {
        Some(_) => (format!("{shown}…"), true),
        None => (shown, false),
    }
}

/// What a read card's `v` shows after its text: a search's whole pattern, when the card cut it.
pub fn details(look: &Look) -> Option<String> {
    let cut = look.tool == "Grep" && pattern_shown(&look.pattern).1;
    cut.then(|| format!("The whole pattern:\n\n{}\n", pattern_text(&look.pattern)))
}

/// The user's answer on the card: None = no; else allowed, for once or longer (`a`, `A` — only when the
/// card offered it; `folder`: what they allow, when the card said so).
pub fn answer(
    ed: &mut Editor,
    id: u64,
    p: Pending,
    scope: Option<Scope>,
    keep: bool,
    folder: Option<PathBuf>,
) {
    ed.offer_finished(id, Ok(()));
    // (the user's own ask is answered whatever claude process runs now — no call of one waits on it)
    if let Some(own) = p.own.clone() {
        return own_answer(ed, p, own, scope, keep);
    }
    if ed.session.chat.as_ref().is_none_or(|c| c.generation() != p.generation) {
        return;
    }
    let shown = p.look.at.as_ref().map_or_else(|| p.look.given.clone(), |a| a.display().to_string());
    let Some(scope) = scope else {
        let no = match p.look.keys.as_slice() {
            [] => format!("the user chose not to let you read {shown} — ask them if you need it"),
            [k] if p.look.at.as_ref() == Some(k) => {
                format!(
                    "the user chose not to let you read {shown}, named like a key — ask them if you need it"
                )
            }
            keys => {
                let named: Vec<String> = keys.iter().map(|k| k.display().to_string()).collect();
                format!(
                    "the user chose not to let you search {shown}: it would read files named like keys ({}) — \
                     search with a path or glob that leaves them out, or ask them",
                    named.join(", ")
                )
            }
        };
        // (the no holds for that place until the turn ends: an ask for it waiting, or come again, is told so — no card)
        // (outside the project, what's under it too; in it, that file or search — the project isn't refused whole)
        let mut again = Vec::new();
        let root = ed.session.root.clone();
        if let (Some(c), Some(at)) = (ed.session.chat.as_mut(), p.look.at.clone()) {
            c.read_no.push((at.clone(), !at.starts_with(&root)));
            let read_no = c.read_no.clone();
            let (no, wait): (Vec<_>, Vec<_>) =
                c.reads.drain(..).partition(|w| said_no_in(&read_no, &w.look).is_some());
            c.reads.extend(wait);
            again = no.into_iter().filter_map(|w| Some((said_no_in(&read_no, &w.look)?, w))).collect();
        }
        reply(ed, p, false, &format!("{no} (this no holds for the rest of the turn)"));
        for (no, w) in again {
            reply(ed, w, false, &no);
        }
        return next(ed);
    };
    reply(ed, p.clone(), true, "");
    if scope == Scope::Once
        && p.look.keys.is_empty()
        && let (Some(c), Some(at)) = (ed.session.chat.as_mut(), p.look.at.clone())
        && !c.read_once.contains(&at)
    {
        c.read_once.push(at);
    }
    let root = ed.session.root.clone();
    let longer = keep && scope != Scope::Once;
    // (in the project only its files named like keys are opened; outside, the place too)
    let at = folder.or_else(|| p.look.at.clone()).filter(|at| longer && !at.starts_with(&root));
    if let Some(at) = &at {
        crate::allow::grant_reads(ed, std::slice::from_ref(at), scope);
    }
    if longer {
        crate::allow::grant_keys(ed, &p.look.keys, scope);
    }
    // The ones waiting that this lets through
    let session = ed.session.allowed.clone();
    if let Some(c) = ed.session.chat.as_mut().filter(|_| longer) {
        let opened = |w: &Pending| {
            w.look.at.as_ref().is_some_and(|a| a.starts_with(&root) || c.allowed.reads(a) || session.reads(a))
                && w.look.keys.iter().all(|k| c.allowed.keys.contains(k) || session.keys.contains(k))
        };
        let (through, wait): (Vec<_>, Vec<_>) = c.reads.drain(..).partition(opened);
        c.reads.extend(wait);
        for w in through {
            reply(ed, w, true, "");
        }
    }
    next(ed);
}

/// The answer on the card asking about files named like keys the user's own act would send (`ask_own`):
/// `a`/`A` open them as the read card's do; then a chip's `y` lets the chat input's message with that chip
/// carry them — that message only (`input_pass`) —, `n` keeps them to their names, said where the user looks,
/// the message still waiting in the input; a note or the review goes now, with their lines or (`n`) without.
fn own_answer(ed: &mut Editor, p: Pending, own: Own, scope: Option<Scope>, keep: bool) {
    if let Some(scope) = scope.filter(|s| *s != Scope::Once && keep) {
        crate::allow::grant_keys(ed, &p.look.keys, scope);
    }
    let lines = scope.is_some();
    match own {
        Own::Chip(id) => {
            let Some(c) = ed.session.chat.as_mut() else { return };
            match scope {
                None => ed.set_status(format!("{} stays out — your message names it only", p.look.given)),
                Some(Scope::Once) => {
                    c.key_pass.extend(p.look.keys.iter().map(|k| (id.clone(), k.clone())));
                    ed.set_status(format!("your next message carries {}", p.look.given));
                }
                Some(_) => {}
            }
        }
        Own::Note { id, text, fresh, why } => crate::notes::send(ed, id, &text, fresh, why, Some(lines)),
        Own::Review => crate::codereview::send_as(ed, Some(lines)),
    }
    next(ed);
    // (a message sent while Claude worked waited for this answer too)
    if ed.session.chat.as_ref().is_some_and(|c| !c.busy()) {
        crate::chat::send_queued(ed);
    }
}

/// The turn ended: nothing waits for these asks any more, and what its runs made asks again. (A card the
/// user's pointing raised waits for them, not for the turn.)
pub fn turn_over(ed: &mut Editor) {
    let mut waiting = Vec::new();
    if let Some(c) = ed.session.chat.as_mut() {
        waiting.extend(c.reads.drain(..).filter_map(|p| p.then));
        c.ran.clear();
        c.read_no.clear();
        (c.also_asked, c.also_yes) = (0, false);
    }
    // (a call of tarae's tools is always answered — its card's going answers the one up: `asking_sync`)
    for t in waiting {
        crate::tools::answer(ed, t.call, Err("the turn ended before the user answered".into()));
    }
    ed.drop_cards(|w| matches!(w, crate::offer::What::Read { pending, .. } if pending.own.is_none()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allow::Allowed;

    /// Where each tool reads: Read its file, Grep its folder (the project when none), Glob its folder with
    /// the pattern's fixed folders — and none when a `..` comes after a wildcard.
    #[test]
    fn where_each_tool_reads() {
        let dir = std::env::temp_dir().join(format!("tarae-reads-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("p/src")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let p = dir.join("p");
        let at = |tool: &str, input: Value| look(tool, &input, &p).at;
        assert_eq!(at("Read", json!({"file_path": "src/a.rs"})), Some(p.join("src/a.rs")));
        assert_eq!(at("Read", json!({"file_path": "../out/m.txt"})), Some(dir.join("out/m.txt")));
        assert_eq!(at("Grep", json!({"pattern": "x"})), Some(p.clone()), "no path: the project");
        assert_eq!(at("Grep", json!({"pattern": "x", "path": dir.join("out")})), Some(dir.join("out")));
        assert_eq!(at("Glob", json!({"pattern": "**/*.rs"})), Some(p.clone()));
        assert_eq!(
            at("Glob", json!({"pattern": "../out/*.txt"})),
            Some(dir.join("out")),
            "fixed folders count"
        );
        assert_eq!(at("Glob", json!({"pattern": format!("{}/out/*", dir.display())})), Some(dir.join("out")));
        assert_eq!(at("Glob", json!({"pattern": "src/*/../../../out/*"})), None, "`..` past a wildcard");
        assert_eq!(at("Glob", json!({"pattern": "*.txt", "path": "../out"})), Some(dir.join("out")));
        // A link inside the project that leads out is where it leads
        std::os::unix::fs::symlink(dir.join("out"), p.join("link")).unwrap();
        assert_eq!(at("Read", json!({"file_path": "link/m.txt"})), Some(dir.join("out/m.txt")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A read's card says first when what it opens holds a token-like line — the file `y` reads, then the
    /// folder `a`/`A` would, offered beside a second read once it's been looked through (a worker's) and
    /// settled again — by name, never the value, in the warning colour. (Fake values in the test's temp
    /// folder.)
    #[test]
    fn a_reads_card_says_when_a_token_is_there() {
        let scratch = crate::scratch::Scratch::new("reads-tok");
        let base = scratch.to_path_buf();
        let out = base.join("out");
        for (f, text) in [
            ("p/README", "marker\n"),
            ("out/hosts.yml", "oauth_token: tarae-fake-token\n"),
            ("out/notes.txt", "marker\n"),
            ("out/.env", "GITHUB_TOKEN=tarae-fake-token\n"),
        ] {
            std::fs::create_dir_all(base.join(f).parent().unwrap()).unwrap();
            std::fs::write(base.join(f), text).unwrap();
        }
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = base.join("p");
        ed.session.chat = Some(crate::chat::Chat::new());
        let settle = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !done(ed) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                ed.handle_event(ed.events.recv_timeout(left).expect("done before the timeout"));
            }
        };
        let warned = |ed: &Editor| {
            let o = &ed.offers[0];
            let r = o.warning.clone().unwrap_or_default();
            o.question.split('\n').skip(r.start).take(r.len()).collect::<Vec<_>>().join("\n")
        };
        let ask = |ed: &mut Editor, id: &str, f: &str| {
            request(ed, id, &json!({"tool_name": "Read", "input": {"file_path": out.join(f)}}));
        };
        ask(&mut ed, "1", "hosts.yml");
        settle(&mut ed, &|ed| !ed.offers.is_empty());
        // (as the card shows them, `~` for home: the test's temp folder is in home in a sandboxed run)
        let file = format!("{} holds a token-like line.", crate::allow::tilde(&out.join("hosts.yml")));
        assert_eq!(warned(&ed), file, "{}", ed.offers[0].question);
        // Beside a second read: the folder, once looked through — said too
        let rev = ed.offers[0].rev;
        ask(&mut ed, "2", "notes.txt");
        settle(&mut ed, &|ed| matches!(&ed.offers[0].what, crate::offer::What::Read { folder: Some(_), .. }));
        let folder = format!("In {}, .env and hosts.yml hold token-like lines.", crate::allow::tilde(&out));
        let q = ed.offers[0].question.clone();
        assert!(warned(&ed).starts_with(&file) && warned(&ed).ends_with(&folder), "{q}");
        assert!(q.contains(&format!("a, s and A allow everything in {}", out.display())), "{q}");
        assert!(ed.offers[0].rev > rev && !q.contains("tarae-fake-token"), "{q}");
        std::fs::remove_dir_all(&base).ok();
    }

    /// The card says what's searched: a folder "and everything in it", a single file just itself.
    #[test]
    fn a_search_of_one_file_isnt_a_folders() {
        let dir = std::env::temp_dir().join(format!("tarae-reads-q-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("out/m.txt"), "marker\n").unwrap();
        let q =
            |path: PathBuf| question(&look("Grep", &json!({"pattern": "x", "path": path}), &dir), true, None);
        let (folder, file) = (dir.join("out"), dir.join("out/m.txt"));
        assert!(
            q(folder.clone())
                .starts_with(&format!("searches {} and everything in it\nfor x", folder.display()))
        );
        assert!(q(file.clone()).starts_with(&format!("searches {}\nfor x", file.display())), "{}", q(file));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inside_or_allowed_goes_without_asking() {
        let root = Path::new("/r/p");
        let l = |at: Option<&str>| Look {
            tool: "Read".into(),
            at: at.map(PathBuf::from),
            pattern: String::new(),
            filter: String::new(),
            given: String::new(),
            file: true,
            keys: Vec::new(),
            cut: false,
        };
        let none = Allowed::default();
        let some = Allowed { read: vec!["/t/x".into()], ..Default::default() };
        let covered = |l: &Look, kept: &Allowed, chat: &Allowed| {
            covered(l, &crate::caps::Caps::granted(root, kept, &Allowed::default(), chat), &Made::default())
        };
        assert!(covered(&l(Some("/r/p/src/a.rs")), &none, &none));
        assert!(!covered(&l(Some("/r/pq/a.rs")), &none, &none), "by path parts");
        assert!(!covered(&l(Some("/t/x/m.txt")), &none, &none));
        assert!(covered(&l(Some("/t/x/m.txt")), &none, &some), "this chat");
        assert!(covered(&l(Some("/t/x/m.txt")), &some, &none), "kept");
        assert!(!covered(&l(None), &some, &some), "can't be told: asks");
    }

    /// P0 of docs/design/cards.md: Claude reads the repository's other checkouts and the temp folders
    /// unasked (`allow::defaults`, as `request` adds them) — the temp folders only where they're on
    /// (`Editor.read_temp`); and a card's read is sorted where the capability model sorts it.
    #[test]
    fn other_checkouts_and_the_temp_folders_read_unasked() {
        let scratch = crate::scratch::Scratch::new("dflt");
        let base = scratch.to_path_buf();
        let (main, wt) = crate::allow::tests::repo_with_worktree(&base);
        std::fs::write(main.join("notes.md"), "marker\n").unwrap();
        std::fs::create_dir_all(base.join("shot")).unwrap();
        std::fs::write(base.join("shot/a.png"), "marker\n").unwrap();
        let reads = |file: &Path, temp: bool| {
            let caps = crate::caps::Basis { root: wt.clone(), temp, ..Default::default() }.build();
            let l = look("Read", &json!({ "file_path": file }), &wt);
            (covered(&l, &caps, &Made::default()), caps.class(l.at.as_deref()))
        };
        use crate::cards::Class;
        assert_eq!(reads(&main.join("notes.md"), false), (true, Class::OtherCheckout), "the main checkout");
        assert_eq!(reads(&base.join("shot/a.png"), true), (true, Class::Temp), "the temp folder");
        let (unasked, class) = reads(&base.join("shot/a.png"), false);
        assert!(!unasked && class != Class::Temp, "temp off: it asks, and isn't temp");
        std::fs::remove_dir_all(&base).ok();
    }

    /// #81: what a run made in the temp folders while it went reads unasked — a file, or a folder made then
    /// with what's in it — and anything in the run's own folder; not what was there before it, nor what
    /// came after it ended, nor what it made outside the temp folders, nor the temp folder itself.
    #[test]
    fn what_a_run_made_reads_unasked() {
        let scratch = crate::scratch::Scratch::new("made");
        let base = scratch.to_path_buf();
        let (temp, elsewhere, own) = (base.join("tmp"), base.join("elsewhere"), base.join("tmp/run-1"));
        for d in [&temp, &elsewhere, &own] {
            std::fs::create_dir_all(d).unwrap();
        }
        // (apart in time: birth times are what's compared)
        let pause = || std::thread::sleep(std::time::Duration::from_millis(20));
        let before = temp.join("before.png");
        std::fs::write(&before, "x").unwrap();
        std::fs::write(own.join("frame.txt"), "x").unwrap();
        pause();
        let from = SystemTime::now();
        pause();
        let shot = temp.join("shot.png");
        std::fs::write(&shot, "x").unwrap();
        std::fs::create_dir(temp.join("shots")).unwrap();
        std::fs::write(temp.join("shots/1.png"), "x").unwrap();
        std::fs::write(elsewhere.join("out.png"), "x").unwrap();
        pause();
        let to = SystemTime::now();
        pause();
        let after = temp.join("after.png");
        std::fs::write(&after, "x").unwrap();
        let made = Made { runs: vec![(from, Some(to))], dirs: vec![own.clone()], temp: vec![temp.clone()] };
        let now = SystemTime::now();
        assert!(made.covers(&shot, now), "made while it went");
        assert!(made.covers(&temp.join("shots/1.png"), now) && made.covers(&temp.join("shots"), now));
        assert!(made.covers(&own.join("frame.txt"), now), "its own folder, whenever");
        assert!(!made.covers(&before, now), "there before it ran");
        assert!(!made.covers(&after, now), "made after it ended");
        assert!(!made.covers(&elsewhere.join("out.png"), now), "outside the temp folders");
        assert!(!made.covers(&temp, now), "not the temp folder itself");
        // Still going: up to now
        let going = Made { runs: vec![(from, None)], ..made.clone() };
        assert!(going.covers(&after, SystemTime::now()));
        // No run this turn: nothing
        assert!(!Made { runs: vec![], dirs: vec![], ..made }.covers(&shot, now));
        std::fs::remove_dir_all(&base).ok();
    }

    const TICK: std::time::Duration = std::time::Duration::from_millis(4);

    /// `t` as a coarse clock tells it: the time of the tick before (Linux stamps new files so).
    fn coarse(t: SystemTime) -> SystemTime {
        let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        t - std::time::Duration::from_nanos((d.as_nanos() % TICK.as_nanos()) as u64)
    }

    /// A fine "now" 3 ms past a tick.
    fn fine_start() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_000_003)
    }

    fn coarse_start() -> SystemTime {
        coarse(fine_start())
    }

    /// A file a run makes 50 µs after it starts is stamped one coarse tick before the fine start (Linux):
    /// a run started by the coarse clock (`file_clock` there) counts it as its own; one started by the
    /// fine clock doesn't, as before (it asked).
    #[test]
    fn a_file_stamped_by_the_coarse_clock_is_the_runs_that_started_by_it() {
        let temp = PathBuf::from("/t/tmp");
        let shot = temp.join("shot.png");
        let born = |_: &Path| Some(coarse(fine_start() + std::time::Duration::from_micros(50)));
        assert!(born(&shot).unwrap() < fine_start(), "stamped before the fine start");
        let mut ed = Editor::new(crate::config::Config::default());
        let mut made = |clock: fn() -> SystemTime| {
            ed.session.chat = Some(crate::chat::Chat::new());
            ed.run_clock = Some(clock);
            run_started(&mut ed, None);
            run_ended(&mut ed);
            Made { temp: vec![temp.clone()], ..Made::of(&ed.session.chat.as_ref().unwrap().ran) }
        };
        let now = SystemTime::now();
        assert!(made(coarse_start).covers_with(&shot, now, born), "by the coarse clock: the run's");
        assert!(!made(fine_start).covers_with(&shot, now, born), "by the fine clock: before it");
    }

    /// The real clock: a file made right after `file_clock` is read is never born before it — on Linux the
    /// coarse clock against the kernel's own stamps (`SystemTime::now()` failed this there).
    #[test]
    fn a_file_made_after_the_file_clock_is_read_is_never_older() {
        let scratch = crate::scratch::Scratch::new("file-clock");
        let dir = scratch.to_path_buf();
        for i in 0..200 {
            let t = file_clock();
            let p = dir.join(i.to_string());
            std::fs::write(&p, "x").unwrap();
            // (a filesystem with no birth times: nothing to compare)
            let Ok(born) = std::fs::metadata(&p).and_then(|m| m.created()) else { break };
            assert!(born >= t, "file {i} born {born:?}, the clock read {t:?} before it");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #45: tarae's own log is read unasked — Read, a Grep of it, a read-only command naming it — but only
    /// the file: not the folder it's in, nor the previous run's beside it.
    #[test]
    fn the_editors_own_log_is_read_unasked() {
        let Some(log) = crate::log::path() else { return };
        let root = Path::new("/r/p");
        let none = Allowed::default();
        let caps = crate::caps::Caps::granted(root, &none, &none, &none);
        let at = |tool: &str, input: Value| covered(&look(tool, &input, root), &caps, &Made::default());
        assert!(at("Read", json!({ "file_path": log })));
        assert!(at("Grep", json!({ "pattern": "ready", "path": log })));
        let dir = log.parent().unwrap();
        assert!(!at("Read", json!({ "file_path": dir.join("tarae.log.old") })));
        assert!(!at("Read", json!({ "file_path": dir.join("trust.json") })));
        assert!(!at("Grep", json!({ "pattern": "x", "path": dir })));
        // (a command's words are only paths when they're there: the log must be)
        if log.exists() {
            let cmd = format!("grep -n ready {}", log.display());
            assert_eq!(crate::readonly::outside(&cmd, root, root, &none, &none, &[]), Ok(vec![]));
            let cmd = format!("tail -n 40 {} {}", log.display(), dir.display());
            let out = crate::readonly::outside(&cmd, root, root, &none, &none, &[]).unwrap();
            assert_eq!(out, vec![std::fs::canonicalize(dir).unwrap()], "the folder still asks");
        }
    }

    /// 7c: a Grep's filters leave a key file out only where they surely do. Its glob is read every way the
    /// tool might split it — whole, by words, by commas outside braces — from either root, in either case; its
    /// type is ripgrep's; a glob or type that doesn't build counts the file in.
    #[test]
    fn a_greps_filters_leave_a_key_out_only_where_they_surely_do() {
        let (root, from) = (Path::new("/p"), Path::new("/p/deploy"));
        let pem = Path::new("/p/deploy/prod.pem");
        let opens = |glob: &str, kind: &str| grep_opens(pem, glob, kind, [from, root]);
        for glob in
            ["", "*.pem", "*.rs,*.pem", "*.rs *.pem", "*.{rs,pem}", "*.PEM", "deploy/*.pem", "*.pem,", "["]
        {
            assert!(opens(glob, ""), "{glob:?} could let it through");
        }
        for glob in ["*.rs", "*.{rs,ts}", "!*.pem", "src/*.pem", "*.rs, *.ts"] {
            assert!(!opens(glob, ""), "{glob:?} leaves it out");
        }
        assert!(!opens("", "rust"), "a type that isn't it");
        assert!(opens("", "no-such-type"), "a type that doesn't build");
        assert!(!opens("*.pem", "rust"), "both filters must let it through");
    }

    /// 7c: a link in the project counts as where it leads — Read of a link (named like nothing) to `.env`
    /// asks for `.env`, and a Grep over the folder names `.env` once, though two names lead to it. A key file
    /// in `.git` or below tools' output isn't one a Grep is asked about. A Grep of the file itself (or of the
    /// link) counts it whatever its glob or type: ripgrep searches a file it's given by name.
    #[test]
    fn a_link_to_a_key_file_is_the_key_file() {
        let scratch = crate::scratch::Scratch::new("key-link");
        let dir = scratch.to_path_buf();
        for d in [".git", "node_modules/pkg"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let root = std::fs::canonicalize(&dir).unwrap();
        for f in [".env", ".git/server.key", "node_modules/pkg/test.key", "a.rs"] {
            std::fs::write(root.join(f), "x\n").unwrap();
        }
        std::os::unix::fs::symlink(root.join(".env"), root.join("notes.txt")).unwrap();
        let env = root.join(".env");
        let read = look("Read", &json!({"file_path": "notes.txt"}), &root);
        assert_eq!(read.keys, std::slice::from_ref(&env), "where the link leads");
        let grep = look("Grep", &json!({"pattern": "x"}), &root);
        assert_eq!(grep.keys, std::slice::from_ref(&env), "once, and not in .git or node_modules");
        let to_rs = look("Grep", &json!({"pattern": "x", "glob": "*.rs"}), &root);
        assert!(to_rs.keys.is_empty(), "{:?}", to_rs.keys);
        // A file named as the path is searched whatever its filters say (ripgrep filters what a walk finds)
        for (path, filter) in [
            (".env", json!({"glob": "*.rs"})),
            (".env", json!({"type": "rust"})),
            ("notes.txt", json!({"glob": "*.rs"})),
        ] {
            let mut input = json!({"pattern": "x", "path": path});
            input.as_object_mut().unwrap().extend(filter.as_object().unwrap().clone());
            let named = look("Grep", &input, &root);
            assert_eq!(named.keys, std::slice::from_ref(&env), "{input}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Piece J: a read in a git checkout outside the project offers that checkout's top — but never home or above
    /// it, nor one in or holding a place that holds keys (`SECRETS`, what runs never read), nor where something
    /// runs later, nor the project or one holding it, nor a folder that isn't a repository's.
    #[test]
    fn a_reads_repository_is_offered_but_never_home_keys_or_what_runs_later() {
        let s = crate::scratch::Scratch::new("reads-repo");
        let base = std::fs::canonicalize(s.path()).unwrap();
        let home = base.join("top/home");
        let init = |dir: &Path| {
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub/README"), "marker").unwrap();
            let out =
                std::process::Command::new("git").args(["init", "-q"]).current_dir(dir).output().unwrap();
            assert!(out.status.success());
        };
        let (refr, keys, plain, project) =
            (home.join("ref"), home.join(".ssh/repo"), home.join("plain"), home.join("work/p"));
        for r in [&refr, &keys, &home.join("work")] {
            init(r);
        }
        std::fs::create_dir_all(plain.join("sub")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let caps =
            crate::caps::Caps { root: project.clone(), home: Some(home.clone()), ..Default::default() };
        let file = |r: &Path| r.join("sub/README");
        assert_eq!(repository_of(&file(&refr), &caps), Some(refr.clone()));
        assert_eq!(repository_of(&refr.join("sub"), &caps), Some(refr.clone()), "a folder searched too");
        assert_eq!(repository_of(&file(&keys), &caps), None, "in ~/.ssh");
        assert_eq!(repository_of(&file(&plain), &caps), None, "no repository");
        assert_eq!(repository_of(&home.join("work/x"), &caps), None, "holding the project");
        assert_eq!(repository_of(&file(&project), &caps), None, "in the project");
        let kept = crate::caps::Caps { kept: vec![refr.join("sub")], ..caps.clone() };
        assert_eq!(repository_of(&file(&refr), &kept), None, "holding what runs never read");
        let later = crate::sandbox::RunsLater { homes: vec![refr.clone()], ..Default::default() };
        let later = crate::caps::Caps { later, ..caps.clone() };
        assert_eq!(repository_of(&file(&refr), &later), None, "a toolchain's home");
        // Each refusal says why, for the card (frictions29 11); no repository, the project's: nothing to say
        let why = |at: &Path, caps: &crate::caps::Caps| match repository_or_why(at, caps) {
            Some(Err((top, why))) => Some((top, why)),
            _ => None,
        };
        assert_eq!(why(&file(&keys), &caps).map(|w| w.0), Some(keys.clone()), "in ~/.ssh");
        assert_eq!(why(&home.join("work/x"), &caps).map(|w| w.1).as_deref(), Some("it holds this project"));
        let kept_why = why(&file(&refr), &kept).map(|w| w.1);
        assert_eq!(kept_why.as_deref(), Some("it's in or holds a place commands never read"));
        assert!(why(&file(&refr), &later).is_some_and(|w| w.1 == later.later.refuses(&refr).unwrap()));
        assert_eq!(why(&file(&plain), &caps), None, "no repository");
        assert_eq!(why(&file(&project), &caps), None, "in the project");
        // A folder searched that's a repository's top itself: nothing wider; a file in it: that one (review J 7)
        init(&refr.join("sub"));
        assert_eq!(repository_of(&refr.join("sub"), &caps), None, "the folder is that repository");
        assert_eq!(repository_of(&refr.join("sub/sub/README"), &caps), Some(refr.join("sub")));
        // Home and above repositories (dotfiles), the project elsewhere — so holding it refuses nothing: never
        // home or above (review J 14)
        let elsewhere = base.join("proj");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let caps = crate::caps::Caps { root: elsewhere, ..caps.clone() };
        init(&base.join("top"));
        assert_eq!(repository_of(&file(&plain), &caps), None, "above home");
        init(&home);
        assert_eq!(repository_of(&file(&plain), &caps), None, "home");
        assert!(!crate::allow::keepable(&home, Some(&home)));
    }

    /// Review J 16: tarae's own tools reading in a repository outside the project wait on the read card with that
    /// repository found on the gate's worker (`Gate::judge` → `Ask`).
    #[test]
    fn the_gate_finds_the_repository_a_read_is_in() {
        let s = crate::scratch::Scratch::new("reads-gate-repo");
        let base = std::fs::canonicalize(s.path()).unwrap();
        let (project, repo) = (base.join("p"), base.join("ref"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/README"), "marker").unwrap();
        let out = std::process::Command::new("git").args(["init", "-q"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success());
        let ask = Gate::for_test(&project).judge(&repo.join("docs/README"), true).expect_err("it asks");
        assert_eq!(ask.repository, Some(repo));
    }

    /// What a Grep of this whole repository waits for before it's answered: the walk for files named like
    /// keys (`grep_keys`; `.git` and tools' output not walked).
    #[test]
    fn perf_grep_keys_in_this_repository() {
        if crate::test_results::unoptimized("reads::tests::perf_grep_keys_in_this_repository") {
            return;
        }
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut times: Vec<f64> = (0..5)
            .map(|_| {
                let t0 = std::time::Instant::now();
                std::hint::black_box(grep_keys(&root, &root, "", "", None, None));
                t0.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        crate::sorting::by(&mut times, |a, b| a.total_cmp(b));
        let ms = times[times.len() / 2];
        let line = format!("PERF grep_keys_this_repository_ms {ms:.3} noise=5\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }

    /// frictions29: a file named like a key outside the project — made in a folder kept as read after the
    /// grant, or in a temp folder read by default — asks for itself (`Look.keys`, the key card's class: `y` once),
    /// read or searched; the files beside it still go unasked, and tarae's own search of the folder (which leaves
    /// it out by name: `Keys::shut`) isn't asked for it. Opened, it goes. (Before: read unasked — only the
    /// project's were named.) Stand-in keys in the test's own folders.
    #[test]
    fn a_key_named_file_outside_the_project_asks_for_itself() {
        let scratch = crate::scratch::Scratch::new("reads-outside-key");
        let dir = scratch.to_path_buf();
        let (project, granted) = (dir.join("project"), dir.join("reference"));
        for d in [&project, &granted] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(granted.join("README.md"), "x\n").unwrap();
        let store = dir.join("state/sandbox.json");
        crate::allow::keep(&store, &project, |a| a.add_read(granted.clone())).unwrap();
        // (made after the grant)
        let key = granted.join("id_ed25519");
        std::fs::write(&key, "tarae-fake-token\n").unwrap();
        let basis =
            crate::caps::Basis { root: project.clone(), store: Some(store.clone()), ..Default::default() };
        let none = Made::default();
        let read =
            |b: &crate::caps::Basis, f: &Path| decide("Read", &json!({ "file_path": f }), &b.build(), &none);
        assert!(read(&basis, &granted.join("README.md")).1, "the folder granted");
        let (look, covered, class) = read(&basis, &key);
        assert!(!covered, "a key made there after the grant");
        assert_eq!((look.keys, class), (vec![key.clone()], crate::cards::Class::Secret));
        let (look, covered, _) =
            decide("Grep", &json!({ "pattern": "x", "path": granted }), &basis.build(), &none);
        assert!(!covered && look.keys == [key.clone()], "a search of the folder: {:?}", look.keys);
        // tarae's own search of the folder leaves it out by name: not asked for it — its own read is
        let gate = Gate { basis: basis.clone(), made: Made::default(), granted: Vec::new() };
        assert!(gate.judge(&granted, false).is_ok(), "the search");
        assert!(gate.judge(&key, true).is_err(), "the read");
        let keys = Keys {
            root: project.clone(),
            chat: Vec::new(),
            store: Some(store.clone()),
            kept: Default::default(),
        };
        assert!(keys.shut(&key) && !keys.shut(&granted.join("README.md")));
        // A temp folder, read by default: the same
        let temp = crate::scratch::Scratch::new("reads-temp-key");
        let (notes, env) = (temp.to_path_buf().join("notes.txt"), temp.to_path_buf().join(".env"));
        std::fs::write(&notes, "x\n").unwrap();
        std::fs::write(&env, "TOKEN=tarae-fake-token\n").unwrap();
        let by_default = crate::caps::Basis { temp: true, ..basis.clone() };
        assert!(read(&by_default, &notes).1, "the temp folder: read by default");
        assert!(!read(&by_default, &env).1, "a key there");
        // Opened for the project (as the run's panel keeps one): it goes
        crate::allow::keep(&store, &project, |a| a.add(&crate::allow::Grant::Key(key.clone()))).unwrap();
        assert!(read(&basis, &key).1, "opened");
        // A folder granted that holds the project: its keys beside the project are named too (review: only the
        // project was walked)
        let beside = dir.join("other/.env");
        std::fs::create_dir_all(beside.parent().unwrap()).unwrap();
        std::fs::write(&beside, "TOKEN=tarae-fake-token\n").unwrap();
        crate::allow::keep(&store, &project, |a| a.add_read(dir.clone())).unwrap();
        let (look, covered, _) =
            decide("Grep", &json!({ "pattern": "x", "path": dir }), &basis.build(), &none);
        assert!(!covered && look.keys.contains(&beside), "{:?}", look.keys);
        // …and the project's own, named by its rule, aren't walked again: a big project doesn't cut it short
        // (review: its files counted toward the cap)
        let own = project.join(".env");
        std::fs::write(&own, "TOKEN=tarae-fake-token\n").unwrap();
        for i in 0..20 {
            std::fs::write(project.join(format!("f{i}.rs")), "x\n").unwrap();
        }
        WALK_MAX.with(|m| m.set(12));
        let (look, _, _) = decide("Grep", &json!({ "pattern": "x", "path": dir }), &basis.build(), &none);
        WALK_MAX.with(|m| m.set(KEYS_WALK_MAX));
        assert!(!look.cut && look.keys.contains(&beside) && look.keys.contains(&own), "{look:?}");
        // A place a card would ask for isn't walked (review: every search outside walked, unbounded) — nothing
        // sends a Grep that reaches that card: the chat has no Grep (2026-10-13), the reviewer refuses what isn't
        // covered; were one to, the card's yes would let the keys there through unnamed
        let unread = crate::scratch::Scratch::new("reads-unread-key");
        std::fs::write(unread.to_path_buf().join("id_rsa"), "tarae-fake-token\n").unwrap();
        let input = json!({ "pattern": "x", "path": unread.to_path_buf() });
        let (look, covered, _) = decide("Grep", &input, &basis.build(), &none);
        assert!(!covered && look.keys.is_empty() && !look.cut, "{look:?}");
        // …and one too big to look through can't tell: it asks, and says why — as `decide` walks it (review N14:
        // only a hand-made cut was looked at)
        assert_eq!(grep_keys(&project, &granted, "", "", Some(1), None), None);
        WALK_MAX.with(|m| m.set(1));
        let input = json!({ "pattern": "x", "path": granted });
        let (look, covered, _) = decide("Grep", &input, &basis.build(), &none);
        WALK_MAX.with(|m| m.set(KEYS_WALK_MAX));
        assert!(look.cut && !covered, "{look:?}");
        let cut = Look { cut: true, ..read(&basis, &granted.join("README.md")).0 };
        assert!(!covered_by(&cut, &basis.build()), "a walk cut short asks");
        assert!(question(&cut, false, None).contains("too much to look through"));
    }

    /// frictions29 round 3, through the requests' own path (`caps_for` — the chat's `request`, `reviewer::read`) and
    /// a stand-in home: a crate's test key in cargo's registry reads unasked, as runs read it; one in a granted folder
    /// still asks; a search of the registry, bigger than the walk's cap, isn't walked (nothing there can close) nor
    /// cut short, while one of the granted folder is (review: only the name was tested, and a big toolchain folder's
    /// search was refused). Stand-in keys in the test's own folders.
    #[test]
    fn the_runs_key_rule_decides_a_read_through_the_requests_path() {
        // (never in the shared temp folder — Linux's is `/tmp`, a key place itself: everything in it closed)
        let tmp = std::env::temp_dir();
        let shared = tmp.starts_with("/tmp") || tmp.starts_with("/private/tmp");
        let tmp = if shared { PathBuf::from("/var/tmp") } else { tmp };
        let scratch = crate::scratch::Scratch::in_dir(&tmp, "reads-key-rule-path");
        let dir = crate::sandbox::profile_path(&scratch);
        let (home, granted) = (dir.join("home"), dir.join("home/ref"));
        let project = home.join("code/p");
        let crate_key = home.join(".cargo/registry/src/x/tests/key.pem");
        for d in [&project, &granted, &dir.join("tmp"), &crate_key.parent().unwrap().to_path_buf()] {
            std::fs::create_dir_all(d).unwrap();
        }
        let key = granted.join("id_ed25519");
        for f in [&key, &crate_key] {
            std::fs::write(f, "tarae-fake-token\n").unwrap();
        }
        let store = dir.join("state/sandbox.json");
        crate::allow::keep(&store, &project, |a| a.add_read(granted.clone())).unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("HOME".to_string(), home.clone().into_os_string());
        let here = crate::sandbox::Here { env, tmp: Some(dir.join("tmp")), ..Default::default() };
        let basis = crate::caps::Basis {
            root: project.clone(),
            store: Some(store),
            home: Some(home.clone()),
            ..Default::default()
        };
        let llm = crate::llm::LlmConfig::default();
        let ask = |tool: &str, input: Value| {
            let caps = caps_for(tool, &input, basis.build_in(Some(&home), &here), &llm, || here.clone());
            assert!(caps.key_rules.is_some(), "{tool} {input}: the rule worked out");
            decide(tool, &input, &caps, &Made::default())
        };
        let (look, covered, _) = ask("Read", json!({ "file_path": crate_key }));
        assert!(covered && look.keys.is_empty(), "a crate's test key: runs read it, so does Read: {look:?}");
        let (look, covered, _) = ask("Read", json!({ "file_path": key }));
        assert!(!covered && look.keys == [key.clone()], "a key in a granted folder asks: {look:?}");
        WALK_MAX.with(|m| m.set(1));
        let (registry, searched, _) =
            ask("Grep", json!({ "pattern": "x", "path": home.join(".cargo/registry/src") }));
        let (reference, _, _) = ask("Grep", json!({ "pattern": "x", "path": granted }));
        WALK_MAX.with(|m| m.set(KEYS_WALK_MAX));
        assert!(
            searched && !registry.cut && registry.keys.is_empty(),
            "the registry isn't walked: {registry:?}"
        );
        assert!(reference.cut, "the granted folder is: {reference:?}");
        // (in the project, nothing outside it: the rule isn't worked out)
        let input = json!({ "pattern": "x", "path": "src" });
        assert!(
            caps_for("Grep", &input, basis.build_in(Some(&home), &here), &llm, || here.clone())
                .key_rules
                .is_none()
        );
    }

    /// `covered`, nothing made by a run.
    fn covered_by(look: &Look, caps: &crate::caps::Caps) -> bool {
        covered(look, caps, &Made::default())
    }
}
