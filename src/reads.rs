//! Claude's Read, Grep and Glob in the chat. Measured 2026-10-02 (`target/sb/r1.sh`): named in
//! `--allowedTools` they read anywhere without asking; left out, every call is a `can_use_tool` ask. So
//! they're left out (`chat::ALLOWED`) and answered here: inside the project (the real path, links
//! followed) or in a place the user allowed (`allow.rs`) at once; anything else on a card that says
//! exactly what's read — `y` once · `a` this chat · `A` always · `n` no. One card at a time; the rest wait
//! in `Chat.reads`, and an answer for longer lets the ones it covers through.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::allow::{Allowed, Scope};
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
}

/// An ask waiting for its answer.
#[derive(Clone, Debug)]
pub struct Pending {
    pub request_id: String,
    pub generation: u64,
    pub input: Value,
    pub look: Look,
}

/// Where the call reads — `cwd` is where relative paths start (claude's, the project's root).
pub fn look(tool: &str, input: &Value, cwd: &Path) -> Look {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let rel = |p: &str| if p.is_empty() { cwd.to_path_buf() } else { cwd.join(p) };
    let (given, pattern, filter) = match tool {
        "Read" => (s("file_path"), String::new(), String::new()),
        "Grep" => (s("path"), s("pattern"), s("glob")),
        _ => (s("path"), s("pattern"), String::new()),
    };
    let at = match tool {
        // Glob: the pattern's fixed folders are part of where it looks (`../../x/*.txt`, `/etc/*`)
        "Glob" => glob_root(&rel(&given), &pattern).and_then(|p| crate::edits::real(&p)),
        _ => crate::edits::real(&rel(&given)),
    };
    let file = at.as_ref().is_some_and(|p| p.is_file());
    Look { tool: tool.to_string(), at, pattern, filter, given, file }
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
    /// Where a run's files count (real paths): `/private/tmp` and the user's temp folder — not what holds
    /// that (`$TMPDIR`'s parent has the apps' caches), nor the toolchain's folders.
    pub temp: Vec<PathBuf>,
}

impl Made {
    pub fn of(ran: &[Ran]) -> Made {
        let temp = [PathBuf::from("/private/tmp"), crate::sandbox::real(&std::env::temp_dir())];
        Made {
            runs: ran.iter().map(|r| (r.from, r.to)).collect(),
            dirs: ran.iter().filter_map(|r| r.dir.as_ref().map(|d| d.path().to_path_buf())).collect(),
            temp: temp.into(),
        }
    }

    /// Whether `at` (a real path) is in a run's folder, or was made in the temp folders while a run went
    /// (a folder made then counts with what's in it — a Grep or Glob of it). Made = its birth time.
    pub fn covers(&self, at: &Path, now: SystemTime) -> bool {
        if self.dirs.iter().any(|d| at.starts_with(d)) {
            return true;
        }
        if !self.temp.iter().any(|t| at.starts_with(t) && at != t) || self.runs.is_empty() {
            return false;
        }
        let born = |p: &Path| std::fs::symlink_metadata(p).and_then(|m| m.created()).ok();
        let during = |t: SystemTime| self.runs.iter().any(|&(from, to)| from <= t && t <= to.unwrap_or(now));
        // The path, else a folder it's in made then (up to the temp folder itself)
        at.ancestors().take_while(|p| !self.temp.iter().any(|t| t == p)).any(|p| born(p).is_some_and(during))
    }
}

/// Whether the call may go without asking: inside the project, in a place allowed, tarae's own log
/// (the file itself — `log::is_log_real`), or made by one of Claude's runs this turn (`Made`).
pub fn covered(look: &Look, root: &Path, chat: &Allowed, kept: &Allowed, made: &Made) -> bool {
    look.at.as_ref().is_some_and(|at| {
        at.starts_with(root)
            || chat.reads(at)
            || kept.reads(at)
            || crate::log::is_log_real(at)
            || made.covers(at, SystemTime::now())
    })
}

/// One of Claude's runs started (`tools::go`).
pub fn run_started(ed: &mut Editor, dir: Option<std::sync::Arc<crate::sandbox::RunDir>>) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.ran.push(Ran { from: SystemTime::now(), to: None, dir });
    }
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
    let (generation, chat, made) = (c.generation(), c.allowed.clone(), Made::of(&c.ran));
    let input = req["input"].clone();
    let request_id = request_id.to_string();
    let store = ed.allow_path.clone();
    // (the root, a real path, is where claude runs — its relative paths are from there)
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let look = look(&tool, &input, &root);
        let kept = crate::allow::Store::read(store.as_deref()).get(&root);
        let ok = covered(&look, &root, &chat, &kept, &made);
        move |ed: &mut Editor| {
            let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation() == generation) else { return };
            if ok {
                return crate::chat::respond(ed, &request_id, allow(&input));
            }
            c.reads.push_back(Pending { request_id, generation, input, look });
            next(ed);
        }
    });
    None
}

fn allow(input: &Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": input })
}

pub const TITLE: &str = "Claude wants to read — outside the project";

/// The next waiting ask on its card, once none is up.
pub fn next(ed: &mut Editor) {
    if ed.session_cards().any(|o| matches!(o.what, crate::offer::What::Read { .. })) {
        return;
    }
    let Some(p) = ed.session.chat.as_mut().and_then(|c| c.reads.pop_front()) else { return };
    let keep =
        p.look.at.as_deref().is_some_and(|at| crate::allow::keepable(at, crate::allow::home().as_deref()));
    let q = question(&p.look, keep);
    let what = crate::offer::What::Read { pending: Box::new(p), keep };
    ed.push_offer(crate::offer::Offer::new(TITLE.into(), &q, "", String::new(), what));
}

/// The card's text: exactly what's read, and where it goes.
pub fn question(look: &Look, keep: bool) -> String {
    let at = match &look.at {
        Some(at) => at.display().to_string(),
        None => format!("{} (where it leads can't be told)", look.given),
    };
    let mut q = match look.tool.as_str() {
        "Read" => at,
        "Grep" => {
            let all = if look.file { "" } else { " and everything in it" };
            let mut s = format!("searches {at}{all}\nfor {}", look.pattern);
            if !look.filter.is_empty() {
                s.push_str(&format!("\nin files {}", look.filter));
            }
            s
        }
        _ => format!("lists {} in {at} and everything in it", look.pattern),
    };
    q.push_str("\n\nWhat it reads goes to Claude.");
    if !keep && look.at.is_some() {
        q.push_str(" Only once — a place like this is never kept.");
    }
    q
}

/// The user's answer on the card: None = no; else allowed, for once or longer (`a`, `A` — only when the
/// card offered it).
pub fn answer(ed: &mut Editor, id: u64, p: Pending, scope: Option<Scope>, keep: bool) {
    ed.offer_finished(id, Ok(()));
    if ed.session.chat.as_ref().is_none_or(|c| c.generation() != p.generation) {
        return;
    }
    let shown = p.look.at.as_ref().map_or_else(|| p.look.given.clone(), |a| a.display().to_string());
    let Some(scope) = scope else {
        let no = format!("the user chose not to let you read {shown} — ask them if you need it");
        crate::chat::respond(ed, &p.request_id, json!({ "behavior": "deny", "message": no }));
        return next(ed);
    };
    crate::chat::respond(ed, &p.request_id, allow(&p.input));
    let at = p.look.at.clone().filter(|_| keep && scope != Scope::Once);
    if let Some(at) = at {
        crate::allow::grant_reads(ed, &[at], scope);
        // The ones waiting that this lets through
        if let Some(c) = ed.session.chat.as_mut() {
            let (through, wait): (Vec<_>, Vec<_>) =
                c.reads.drain(..).partition(|w| w.look.at.as_ref().is_some_and(|a| c.allowed.reads(a)));
            c.reads.extend(wait);
            for w in through {
                crate::chat::respond(ed, &w.request_id, allow(&w.input));
            }
        }
    }
    next(ed);
}

/// The turn ended: nothing waits for these asks any more, and what its runs made asks again.
pub fn turn_over(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.reads.clear();
        c.ran.clear();
    }
    ed.drop_cards(|w| matches!(w, crate::offer::What::Read { .. }));
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The card says what's searched: a folder "and everything in it", a single file just itself.
    #[test]
    fn a_search_of_one_file_isnt_a_folders() {
        let dir = std::env::temp_dir().join(format!("tarae-reads-q-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("out/m.txt"), "marker\n").unwrap();
        let q = |path: PathBuf| question(&look("Grep", &json!({"pattern": "x", "path": path}), &dir), true);
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
        };
        let none = Allowed::default();
        let some = Allowed { read: vec!["/t/x".into()], ..Default::default() };
        assert!(covered(&l(Some("/r/p/src/a.rs")), root, &none, &none, &Made::default()));
        assert!(!covered(&l(Some("/r/pq/a.rs")), root, &none, &none, &Made::default()), "by path parts");
        assert!(!covered(&l(Some("/t/x/m.txt")), root, &none, &none, &Made::default()));
        assert!(covered(&l(Some("/t/x/m.txt")), root, &some, &none, &Made::default()), "this chat");
        assert!(covered(&l(Some("/t/x/m.txt")), root, &none, &some, &Made::default()), "kept");
        assert!(!covered(&l(None), root, &some, &some, &Made::default()), "can't be told: asks");
    }

    /// #81: what a run made in the temp folders while it went reads unasked — a file, or a folder made then
    /// with what's in it — and anything in the run's own folder; not what was there before it, nor what
    /// came after it ended, nor what it made outside the temp folders, nor the temp folder itself.
    #[test]
    fn what_a_run_made_reads_unasked() {
        let base =
            crate::sandbox::real(&std::env::temp_dir()).join(format!("tarae-made-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
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

    /// #45: tarae's own log is read unasked — Read, a Grep of it, a read-only command naming it — but only
    /// the file: not the folder it's in, nor the previous run's beside it.
    #[test]
    fn the_editors_own_log_is_read_unasked() {
        let Some(log) = crate::log::path() else { return };
        let root = Path::new("/r/p");
        let none = Allowed::default();
        let at = |tool: &str, input: Value| {
            covered(&look(tool, &input, root), root, &none, &none, &Made::default())
        };
        assert!(at("Read", json!({ "file_path": log })));
        assert!(at("Grep", json!({ "pattern": "ready", "path": log })));
        let dir = log.parent().unwrap();
        assert!(!at("Read", json!({ "file_path": dir.join("tarae.log.old") })));
        assert!(!at("Read", json!({ "file_path": dir.join("trust.json") })));
        assert!(!at("Grep", json!({ "pattern": "x", "path": dir })));
        // (a command's words are only paths when they're there: the log must be)
        if log.exists() {
            let cmd = format!("grep -n ready {}", log.display());
            assert_eq!(crate::readonly::outside(&cmd, root, root, &none, &none), Ok(vec![]));
            let cmd = format!("tail -n 40 {} {}", log.display(), dir.display());
            let out = crate::readonly::outside(&cmd, root, root, &none, &none).unwrap();
            assert_eq!(out, vec![std::fs::canonicalize(dir).unwrap()], "the folder still asks");
        }
    }
}
