//! collab C2 — an overlap said the moment it happens (`target/sb/collab-plan.md` §2.3): two workspaces whose changes
//! would conflict when merged, or a workspace's changes against its base's tip — git decides, as at the rebase, but
//! early.
//!
//! A round (one `shared_jobs` job, on a worker — never git on the main thread) once a workspace's tree moved
//! (`Session.wrote`: a save, Claude's edit, a change set; its HEAD moved) and stayed quiet `QUIET`:
//! 1. each moved workspace's working tree as a commit on its HEAD (`snapshot::snap_apart` + `commit-tree`) — its
//!    index and every object git writes in a folder of tarae's own (`Folder`, in tarae's cache), the repository's
//!    objects read as an alternate: nothing is written in the repository, and the folder goes when the workspace
//!    ends;
//! 2. its changed paths (`diff-tree` against its merge base with its base's tip; the home checkout's against its
//!    HEAD);
//! 3. `git merge-tree --write-tree` against its base's tip, and against each other workspace's latest snapshot —
//!    each only when the two changed a path in common (most pairs share none: they're never merged);
//! 4. the conflicted files both changed, and the lines on each side (`regions`: the two sides' hunks that meet).
//!
//! Said (`landed`): a new overlap — a pair and a file not said while it lasts, or its lines elsewhere — is a toast
//! once (`fix ≈ main · src/term.rs L210–260 — would conflict`); gone and back, it's said again; the strip and the session list say `≈ main` after the name, the other side in its
//! colour; the side panel's file row `≈m` (`presence::file_marks`). One that goes is said in `:messages` only. Claude hears of
//! an overlap only in the answer of the tool that touches that file (`edit`, `read` — `told`): one line, its own
//! file's lines, the other side's name — never its lines or its text, never a feed of the others.
//!
//! What another workspace changed is data here (`docs/threat-model.md`): a path is shown, never read as anything, and
//! one named like a key is never named.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::editor::Editor;
use crate::event::SessionId;

/// How long a workspace's tree stays still before it's taken (a save, then another: one round).
pub const QUIET: Duration = Duration::from_secs(3);

/// A conflicted file's text read for its lines at most (larger: the file named, no lines).
const TEXT_MAX: usize = 1 << 20;

/// The name a snapshot's commit is made under — and its time, fixed: the same tree on the same HEAD is the same
/// commit, so a merge already worked out is kept (`State.merges`).
const WHO: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "tarae"),
    ("GIT_AUTHOR_EMAIL", "tarae@localhost"),
    ("GIT_AUTHOR_DATE", "@0 +0000"),
    ("GIT_COMMITTER_NAME", "tarae"),
    ("GIT_COMMITTER_EMAIL", "tarae@localhost"),
    ("GIT_COMMITTER_DATE", "@0 +0000"),
];

/// Given to every git of a round before its subcommand: no fsmonitor (a program of the user's), no renormalizing
/// merge (it runs the filters) — then the filters and merge drivers taken out (`guards`).
const PRE: [&str; 4] = ["-c", "core.fsmonitor=false", "-c", "merge.renormalize=false"];

/// The files a snapshot takes: those no `filter` attribute names.
const SCOPE: &str = ":(attr:!filter)";

/// The files a `filter` attribute names (any value): a snapshot has them as HEAD has them — a git-lfs file its
/// pointer, never read as changed, even staged, and nothing (git-lfs's `.git/lfs/objects`) is written for it.
const FILTERED: &str = ":(exclude,attr:!filter)";

/// A merge driver of the user's or the repository's, as the merge runs it instead: git's own (held.rs's value).
const OWN_DRIVER: &str = "git merge-file -L %X -L %S -L %Y --marker-size=%L %A %O %B";

/// A folder of a gone tarae's with no lock in it yet, as young as this, may be one a tarae is making: left.
const UNLOCKED_KEPT: Duration = Duration::from_secs(60);

/// The lock in this tarae's folder, held while it lives (`State.lock`).
const LOCK: &str = ".lock";

/// A workspace's objects at most (`State.objects_max`): every take of a moved tree and every merge writes some, for
/// the session's whole life — past this its folder starts anew (`renew`), its next take seeding it again (this
/// repository's: 0.7 s once).
pub const OBJECTS_MAX: u64 = 64 << 20;

/// How many times a workspace whose take failed is taken again on the quiet (a lock of git's, a file mid-write) before
/// it waits for its next change — one that fails for good (no commit yet) doesn't wake tarae every few seconds.
const TRIES: u8 = 3;

/// This tarae's own folder in `base` (tarae's cache's `overlap`): its pid and a number no one can guess — another
/// tarae on the same repository has its own, so neither removes what the other's snapshots stand on.
pub fn own_in(base: &Path) -> PathBuf {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    base.join(format!("{}-{:016x}", std::process::id(), h.finish()))
}

/// A workspace's folder of tarae's own: its index and the objects its snapshots and merges write. One at a time in it
/// (`one`, the path's — `PATHS`); ended, nothing more is written there and it's removed, unless a session opened
/// there since has it (`born`).
#[derive(Debug)]
pub struct Folder {
    pub path: PathBuf,
    one: Arc<Mutex<()>>,
    born: u64,
    ended: AtomicBool,
}

/// Each folder's lock and its newest `Folder`'s generation — a checkout's session closed and opened again shares
/// its path: the first one's removal leaves the second's folder alone.
static PATHS: Mutex<Vec<PathLock>> = Mutex::new(Vec::new());

/// A folder's path, its newest `Folder`'s generation, its lock.
type PathLock = (PathBuf, u64, Arc<Mutex<()>>);

impl Folder {
    fn new(path: PathBuf) -> Folder {
        let mut paths = PATHS.lock().unwrap_or_else(|e| e.into_inner());
        let (born, one) = match paths.iter_mut().find(|p| p.0 == path) {
            Some(p) => {
                p.1 += 1;
                (p.1, p.2.clone())
            }
            None => {
                let one = Arc::new(Mutex::new(()));
                paths.push((path.clone(), 0, one.clone()));
                (0, one)
            }
        };
        Folder { path, one, born, ended: AtomicBool::new(false) }
    }

    /// Whether no `Folder` of this path came after this one.
    fn newest(&self) -> bool {
        let paths = PATHS.lock().unwrap_or_else(|e| e.into_inner());
        paths.iter().find(|p| p.0 == self.path).is_none_or(|p| p.1 == self.born)
    }

    fn objects(&self) -> PathBuf {
        self.path.join("objects")
    }

    fn index(&self) -> PathBuf {
        self.path.join("index")
    }
}

/// The folder for the checkout at `root`, in `dir` (by its path's hash, as the chats' files are).
fn folder_of(dir: &Path, root: &Path) -> PathBuf {
    let h = root
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    dir.join(format!("{h:016x}"))
}

/// A workspace's working tree as last taken.
#[derive(Clone, Debug, PartialEq)]
pub struct Snap {
    pub commit: String,
    /// Its HEAD — the commit's parent.
    head: String,
    /// The repository's git folder (its checkouts share it): two workspaces are merged only within one.
    common: PathBuf,
    /// The checkout's top: the paths below are from there.
    top: PathBuf,
    /// What it changed: against its merge base with the base's tip (a workspace), against its HEAD (the home one).
    changed: Vec<String>,
    base: Option<Base>,
}

/// A workspace's base as the last round saw it: its branch, its tip, and what the tip changed since the two parted.
#[derive(Clone, Debug, PartialEq)]
struct Base {
    branch: String,
    tip: String,
    changed: Vec<String>,
}

/// The other side of an overlap: another session's workspace, or the base's tip (its branch).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Other {
    Session(SessionId),
    Branch(String),
}

/// Two sides that would conflict in one file — `a`'s lines and the other's (None: the file as a whole — a delete
/// against a change, a mode, lines too far apart to tell).
#[derive(Clone, Debug, PartialEq)]
pub struct Overlap {
    pub a: SessionId,
    pub b: Other,
    /// From the checkout's top.
    pub path: String,
    pub lines: Option<Range<usize>>,
    pub other_lines: Option<Range<usize>>,
}

/// A merge's conflicted file and the lines on each side (`a` the first one merged).
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub path: String,
    pub a: Option<Range<usize>>,
    pub b: Option<Range<usize>>,
}

/// A workspace as the main loop keeps it.
struct Place {
    folder: Arc<Folder>,
    /// `Session.wrote` as last seen.
    wrote: Option<Instant>,
    /// Moved since it was last taken — when (`clock`): it's taken once it has been still `quiet`.
    moved: Option<Instant>,
    snap: Option<Snap>,
    /// Takes failed in a row since its last change (`TRIES`).
    tries: u8,
}

/// Every workspace's overlaps (`Editor.overlap`).
pub struct State {
    pub now: Vec<Overlap>,
    places: HashMap<SessionId, Place>,
    /// Said while it lasts, by its two sides (in order) and its file, with the lines it was said at: one whose lines
    /// no longer meet those is another conflict, said again; one gone is dropped (back later: said again).
    said: HashMap<Said, Option<Range<usize>>>,
    /// A round's wait (the quiet) or the round itself under way.
    waiting: bool,
    running: bool,
    /// Merges worked out, by the two commits — those of the last round only.
    merges: HashMap<(String, String), Arc<Vec<Conflict>>>,
    /// Where the folders are (tarae's cache's `overlap`) — None: never a round (tests but those that set it).
    pub dir: Option<PathBuf>,
    /// The lock in `dir`, held while this tarae lives (taken by the first round) — what tells another tarae's prune
    /// the folder is in use.
    lock: Option<Arc<std::fs::File>>,
    pub quiet: Duration,
    /// A workspace's objects at most (`OBJECTS_MAX`; tests make it small).
    pub objects_max: u64,
    /// Rounds landed, merges run and folders started anew in them, since tarae started (tests count them).
    pub rounds: u64,
    pub merged: u64,
    pub renewed: u64,
}

/// What a said overlap is kept by: its sides (the lower session first) and its file.
type Said = (SessionId, Other, String);

impl Default for State {
    fn default() -> State {
        State {
            now: Vec::new(),
            places: HashMap::new(),
            said: HashMap::new(),
            waiting: false,
            running: false,
            merges: HashMap::new(),
            dir: match cfg!(test) {
                true => None,
                false => crate::runtime::cache_dir().map(|c| own_in(&c.join("overlap"))),
            },
            lock: None,
            quiet: QUIET,
            objects_max: OBJECTS_MAX,
            rounds: 0,
            merged: 0,
            renewed: 0,
        }
    }
}

#[cfg(test)]
impl State {
    /// The folder of session `id`'s workspace, while it's kept (tests look in it).
    pub fn folder(&self, id: SessionId) -> Option<PathBuf> {
        self.places.get(&id).map(|p| p.folder.path.clone())
    }

    /// Whether every workspace's tree is as last taken — no save since that the next event will see — and no
    /// round waits or runs.
    pub fn settled(&self, ed: &Editor) -> bool {
        let seen = |l: &crate::session::Session| self.places.get(&l.id).is_some_and(|p| p.wrote == l.wrote);
        !self.waiting
            && !self.running
            && self.places.values().all(|p| p.moved.is_none() && p.snap.is_some())
            && sessions(ed).all(seen)
    }

    /// A tree moved with no round coming for it — said.
    pub fn unarmed(&self, ed: &Editor) -> Option<String> {
        // (one session, or one not seen yet — opened, or saved, since the last event: the next arms it)
        let seen = |l: &crate::session::Session| self.places.get(&l.id).is_some_and(|p| p.wrote == l.wrote);
        if ed.sessions.is_empty() || !sessions(ed).all(seen) {
            return None;
        }
        let moved = self.places.iter().find(|(_, p)| p.moved.is_some()).map(|(id, _)| *id);
        moved
            .filter(|_| !self.waiting && !self.running)
            .map(|id| format!("session {id} moved, no round coming"))
    }
}

/// The sessions, the front one first.
fn sessions(ed: &Editor) -> impl Iterator<Item = &crate::session::Session> {
    std::iter::once(&ed.session).chain(&ed.sessions)
}

/// Every event: the workspaces whose tree moved noted, and a round armed once they're quiet — nothing with one
/// session open, nothing while nothing moved (an idle tarae isn't woken).
pub fn sync(ed: &mut Editor) {
    let Some(dir) = ed.overlap.dir.clone() else { return };
    if ed.sessions.is_empty() {
        if !ed.overlap.now.is_empty() {
            ed.overlap.now.clear();
        }
        return;
    }
    let now = crate::clock::now();
    let seen: Vec<(SessionId, PathBuf, Option<Instant>)> =
        sessions(ed).map(|l| (l.id, l.root.clone(), l.wrote)).collect();
    for (id, root, wrote) in seen {
        let p = ed.overlap.places.entry(id).or_insert_with(|| Place {
            folder: Arc::new(Folder::new(folder_of(&dir, &root))),
            wrote,
            moved: Some(now),
            snap: None,
            tries: 0,
        });
        if p.wrote != wrote {
            p.wrote = wrote;
            p.moved = Some(now);
            p.tries = 0;
        }
    }
    arm(ed);
}

/// A tree moved without a save tarae saw: the checkouts under `dir` (its HEAD moved — a commit, a rebase, a bring).
pub fn moved(ed: &mut Editor, dir: &Path) {
    let now = crate::clock::now();
    let ids: Vec<SessionId> = sessions(ed).filter(|l| l.root.starts_with(dir)).map(|l| l.id).collect();
    for id in ids {
        if let Some(p) = ed.overlap.places.get_mut(&id) {
            p.moved = Some(now);
            p.tries = 0;
        }
    }
}

/// The quiet waited for, on a worker, while a workspace has moved and no round waits or runs.
fn arm(ed: &mut Editor) {
    let o = &ed.overlap;
    if o.waiting || o.running {
        return;
    }
    let Some(last) = o.places.values().filter_map(|p| p.moved).max() else { return };
    let wait = o.quiet.saturating_sub(crate::clock::since(last));
    ed.overlap.waiting = true;
    ed.events.shared_jobs().spawn(move || {
        crate::event::sleep(wait);
        |ed: &mut Editor| {
            ed.overlap.waiting = false;
            ed.unchanged = true;
            start(ed);
        }
    });
}

/// The quiet over: a round over every workspace, the moved ones taken again — or the wait again, if one moved
/// meanwhile.
fn start(ed: &mut Editor) {
    let quiet = ed.overlap.quiet;
    // (one session left: nothing to compare — nothing armed until a second opens: `sync`)
    if ed.sessions.is_empty() {
        return;
    }
    let stirring =
        ed.overlap.places.values().any(|p| p.moved.is_some_and(|m| crate::clock::since(m) < quiet));
    if stirring {
        return arm(ed);
    }
    let home = sessions(ed).find(|l| l.id == 0).and_then(|l| l.git_branch.clone());
    let mut ws = Vec::new();
    for l in std::iter::once(&ed.session).chain(&ed.sessions) {
        let Some(p) = ed.overlap.places.get(&l.id) else { continue };
        // (a workspace's base: its own, else the branch of the checkout tarae started in — the home one has none)
        let base = match l.id {
            0 => None,
            _ => l.base.clone().or_else(|| home.clone()),
        };
        ws.push(Ws {
            id: l.id,
            root: l.root.clone(),
            base,
            folder: p.folder.clone(),
            take: p.moved.is_some() || p.snap.is_none(),
            had: p.snap.clone(),
            max: ed.overlap.objects_max,
        });
    }
    // (taken now — one whose take fails is moved again as it lands: `landed`)
    for p in ed.overlap.places.values_mut() {
        p.moved = None;
    }
    ed.overlap.running = true;
    let merges = ed.overlap.merges.clone();
    // (the first round makes this tarae's folder and takes its lock — and prunes the gone taraes')
    let own = ed.overlap.dir.clone().filter(|_| ed.overlap.lock.is_none());
    ed.events.shared_jobs().spawn(move || {
        let r = round(ws, &merges, own.as_deref());
        move |ed: &mut Editor| landed(ed, r)
    });
}

/// A workspace as a round takes it.
struct Ws {
    id: SessionId,
    root: PathBuf,
    base: Option<String>,
    folder: Arc<Folder>,
    /// Taken again (it moved, or was never taken).
    take: bool,
    had: Option<Snap>,
    /// Its objects at most (`State.objects_max`).
    max: u64,
}

/// What a round found.
pub struct Round {
    snaps: Vec<(SessionId, Arc<Folder>, Option<Snap>)>,
    /// The workspaces whose take failed (all of them when its lock couldn't be taken): taken again on the quiet.
    failed: Vec<SessionId>,
    /// The two sides of each merge that failed: their overlaps as the last round found them stand.
    unknown: Vec<(SessionId, Other)>,
    /// Folders started anew (`renew`).
    renewed: u64,
    now: Vec<Overlap>,
    merges: HashMap<(String, String), Arc<Vec<Conflict>>>,
    merged: usize,
    /// This tarae's lock, taken by its first round.
    lock: Option<std::fs::File>,
    /// False: its lock couldn't be taken — nothing was taken or merged, what's said stays as it was.
    held: bool,
}

impl Default for Round {
    fn default() -> Round {
        Round {
            snaps: Vec::new(),
            failed: Vec::new(),
            unknown: Vec::new(),
            renewed: 0,
            now: Vec::new(),
            merges: HashMap::new(),
            merged: 0,
            lock: None,
            held: true,
        }
    }
}

/// The round (a worker's): each workspace taken or kept, its base's tip looked at, the merges of what shares a
/// path — the first one (`own`: this tarae's folder) makes the folder, takes its lock and prunes the gone taraes'.
fn round(ws: Vec<Ws>, kept: &HashMap<(String, String), Arc<Vec<Conflict>>>, own: Option<&Path>) -> Round {
    let started = crate::clock::now();
    let mut lock = None;
    if let Some(own) = own {
        match hold(own) {
            Ok(f) => lock = Some(f),
            // (no lock: nothing written — a folder no `.lock` marks is one a prune removes; the next move tries
            // again)
            Err(e) => {
                crate::log::line("overlap", "info", &format!("{}: {e} — nothing taken", own.display()));
                let failed = ws.iter().map(|w| w.id).collect();
                return Round { held: false, failed, ..Round::default() };
            }
        }
        if let Some(base) = own.parent() {
            prune(base, own);
        }
    }
    let (mut snaps, mut failed, mut renewed) = (Vec::new(), Vec::new(), 0);
    for w in &ws {
        let _one = w.folder.one.lock().unwrap_or_else(|e| e.into_inner());
        let snap = match w.folder.ended.load(Ordering::SeqCst) {
            true => None,
            false => {
                // (past its bound: started anew — the tree taken again on the same HEAD is the same commit)
                let anew = renew(&w.folder, w.max);
                renewed += u64::from(anew);
                match take(w, anew) {
                    Ok(s) => Some(s),
                    // (its tree as last taken stands for it this round — a lock of git's held a moment isn't its
                    // overlaps gone; taken again on the quiet)
                    Err(e) => {
                        crate::log::line("overlap", "info", &format!("{}: {e}", w.root.display()));
                        failed.push(w.id);
                        w.had.clone()
                    }
                }
            }
        };
        snaps.push((w.id, w.folder.clone(), snap));
    }
    let mut now = Vec::new();
    let mut merges = HashMap::new();
    let mut merged = 0;
    let mut unknown = Vec::new();
    // (Ok(None): one of the two ended — Err: the merge failed, what was found of the two before stands)
    let mut merge =
        |a: (&Snap, &Folder), b: (&str, Option<&Folder>)| -> Result<Option<Arc<Vec<Conflict>>>, ()> {
            let key = (a.0.commit.clone(), b.0.to_string());
            if let Some(c) = kept.get(&key).or_else(|| merges.get(&key)) {
                let c: Arc<Vec<Conflict>> = c.clone();
                merges.insert(key, c.clone());
                return Ok(Some(c));
            }
            let _one = a.1.one.lock().unwrap_or_else(|e| e.into_inner());
            if a.1.ended.load(Ordering::SeqCst) || b.1.is_some_and(|f| f.ended.load(Ordering::SeqCst)) {
                return Ok(None);
            }
            merged += 1;
            match conflicts(a.0, a.1, b.0, b.1) {
                Ok(c) => {
                    let c = Arc::new(c);
                    merges.insert(key, c.clone());
                    Ok(Some(c))
                }
                Err(e) => {
                    crate::log::line("overlap", "info", &format!("{}: merge: {e}", a.0.top.display()));
                    Err(())
                }
            }
        };
    // Each against its base's tip, where both changed a path
    for (id, folder, snap) in &snaps {
        let Some(s) = snap else { continue };
        let Some(base) = s.base.as_ref() else { continue };
        let shared = shared(&s.changed, &base.changed);
        if shared.is_empty() {
            continue;
        }
        let b = Other::Branch(base.branch.clone());
        let found = merge((s, folder), (&base.tip, None)).unwrap_or_else(|()| {
            unknown.push((*id, b.clone()));
            None
        });
        for c in found.iter().flat_map(|c| c.iter()) {
            if shared.contains(&c.path) && !key_named(&s.top, &c.path) {
                let (lines, other_lines) = (c.a.clone(), c.b.clone());
                now.push(Overlap { a: *id, b: b.clone(), path: c.path.clone(), lines, other_lines });
            }
        }
    }
    // Each pair of one repository, where both changed a path
    for (i, (ia, fa, sa)) in snaps.iter().enumerate() {
        for (ib, fb, sb) in &snaps[i + 1..] {
            let (Some(sa), Some(sb)) = (sa, sb) else { continue };
            if sa.common != sb.common {
                continue;
            }
            let shared = shared(&sa.changed, &sb.changed);
            if shared.is_empty() {
                continue;
            }
            let found = merge((sa, fa), (&sb.commit, Some(fb))).unwrap_or_else(|()| {
                unknown.push((*ia, Other::Session(*ib)));
                None
            });
            for c in found.iter().flat_map(|c| c.iter()) {
                if shared.contains(&c.path) && !key_named(&sa.top, &c.path) {
                    let (lines, other_lines) = (c.a.clone(), c.b.clone());
                    now.push(Overlap {
                        a: *ia,
                        b: Other::Session(*ib),
                        path: c.path.clone(),
                        lines,
                        other_lines,
                    });
                }
            }
        }
    }
    let line = format!(
        "{} workspaces, {} taken, {merged} merged, {} overlaps — {}",
        snaps.len(),
        ws.iter().filter(|w| w.take).count(),
        now.len(),
        crate::term::run_duration(crate::clock::since(started))
    );
    crate::log::line("overlap", "info", &line);
    Round { snaps, failed, unknown, renewed, now, merges, merged, lock, held: true }
}

/// A workspace's folder started anew when its objects are past `max` bytes: its objects and index removed (a folder
/// of this tarae's own, no link followed in it) — its next take writes them again. Whether it was.
fn renew(folder: &Folder, max: u64) -> bool {
    let size = size_of(&folder.objects());
    if size <= max {
        return false;
    }
    let gone = std::fs::remove_dir_all(folder.objects());
    let _ = std::fs::remove_file(folder.index());
    let line = format!(
        "{}: {size} bytes of objects (at most {max}) — started anew: {gone:?}",
        folder.path.display()
    );
    crate::log::line("overlap", "info", &line);
    true
}

/// The bytes of the files under `dir` (git's `objects`: its fan-out folders, `pack`, `info`) — links not followed.
fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in entries.flatten() {
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        n += match m.is_dir() {
            true => size_of(&e.path()),
            false => m.len(),
        };
    }
    n
}

/// The paths two lists both have.
fn shared(a: &[String], b: &[String]) -> HashSet<String> {
    let b: HashSet<&String> = b.iter().collect();
    a.iter().filter(|p| b.contains(p)).cloned().collect()
}

/// A path named like a key is never named (`sandbox::key_named_in`).
fn key_named(top: &Path, path: &str) -> bool {
    crate::sandbox::key_named_in(top, &top.join(path))
}

/// git's variables for a folder's work: its objects written there, the repository's (and `also`'s) read.
fn env(folder: &Folder, common: &Path, also: Option<&Folder>) -> Vec<(&'static str, OsString)> {
    let mut alt = alternate(&common.join("objects"));
    if let Some(f) = also {
        alt.push(":");
        alt.push(alternate(&f.objects()));
    }
    // (and never a fetch: a partial clone's missing blob stays missing — no remote, no transport, no credential
    // helper started after a save; the merge that needs it fails, logged)
    let mut v = vec![
        ("GIT_OBJECT_DIRECTORY", folder.objects().into_os_string()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt),
        (crate::gitrepo::NO_LAZY_FETCH.0, OsString::from(crate::gitrepo::NO_LAZY_FETCH.1)),
    ];
    v.extend(WHO.iter().map(|(k, val)| (*k, OsString::from(val))));
    v
}

/// A folder as git's list of alternates reads it: quoted where a `:` or a quote would split or start one.
fn alternate(p: &Path) -> OsString {
    let s = p.to_string_lossy();
    match s.contains(':') || s.starts_with('"') {
        true => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")).into(),
        false => p.as_os_str().to_os_string(),
    }
}

/// A workspace taken: its working tree as a commit on its HEAD (or the last one, unmoved), its base's tip, what each
/// changed. Its folder's lock held; `anew`: its folder was just started anew (`renew`) — made again, moved or not.
fn take(w: &Ws, anew: bool) -> Result<Snap, String> {
    let repo = crate::gitrepo::repo(&w.root)?;
    let top = repo.work_tree.clone().unwrap_or_else(|| w.root.clone());
    let env = env(&w.folder, &repo.common_dir, None);
    let guards = guards(&|args| crate::snapshot::command_env(&w.root, args, None, &env))?;
    let pre: Vec<&str> = guards.iter().map(String::as_str).collect();
    let git = |args: &[&str]| {
        let all: Vec<&str> = pre.iter().chain(args).copied().collect();
        crate::snapshot::command_env(&w.root, &all, None, &env)
            .and_then(|c| crate::gitrepo::finish(&c, None))
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
    };
    let (commit, head) = match (&w.had, w.take || anew) {
        (Some(h), false) => (h.commit.clone(), h.head.clone()),
        _ => {
            make(&w.folder.path)?;
            let how = crate::snapshot::Apart { env: &env, pre: &pre, scope: SCOPE, as_head: Some(FILTERED) };
            let tree = crate::snapshot::snap_apart(&w.root, &w.folder.index(), &how)?.id;
            let head = git(&["rev-parse", "--verify", "-q", "HEAD^{commit}"]).map_err(|_| "no commit yet")?;
            let commit = git(&["commit-tree", "--no-gpg-sign", &tree, "-p", &head, "-m", "tarae: overlap"])?;
            (commit, head)
        }
    };
    let names = |from: &str, to: &str| -> Result<Vec<String>, String> {
        let out = git(&["diff-tree", "-r", "--name-only", "--no-renames", "-z", from, to])?;
        Ok(out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
    };
    let tip = w.base.as_ref().and_then(|b| {
        git(&["rev-parse", "--verify", "-q", &format!("refs/heads/{b}^{{commit}}")]).ok().map(|t| (b, t))
    });
    let unmoved = w.had.as_ref().filter(|h| h.commit == commit);
    let (changed, base) = match tip {
        // (neither moved: what was worked out stands)
        Some((b, t))
            if unmoved.is_some_and(|h| h.base.as_ref().is_some_and(|x| x.tip == t && x.branch == *b)) =>
        {
            let h = unmoved.unwrap();
            (h.changed.clone(), h.base.clone())
        }
        Some((b, t)) => {
            let mb = git(&["merge-base", &commit, &t])?;
            let changed = names(&mb, &commit)?;
            (changed, Some(Base { branch: b.clone(), changed: names(&mb, &t)?, tip: t }))
        }
        None => match unmoved {
            Some(h) if h.base.is_none() => (h.changed.clone(), None),
            _ => (names(&head, &commit)?, None),
        },
    };
    Ok(Snap { commit, head, common: repo.common_dir.clone(), top, changed, base })
}

/// A workspace's folder made (0700, in this tarae's).
fn make(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    if path.join("objects").is_dir() {
        return Ok(());
    }
    let said = |e: std::io::Error| format!("tarae's overlap folder: {e}");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path.join("objects")).map_err(said)
}

/// This tarae's folder made (0700) and its lock taken — an flock, let go when the process ends or execs, or this
/// `State` goes. Err: another holds it, or it can't be made.
fn hold(own: &Path) -> std::io::Result<std::fs::File> {
    hold_with(own, &|| {})
}

/// How many times `hold` tries again when another tarae's prune is at its folder — and how long it waits before
/// each (a prune holds the lock only while it removes the folder).
const HOLD_TRIES: usize = 5;
const HOLD_WAIT: Duration = Duration::from_millis(20);

/// `hold`, `between` run after the lock file is made and before it's locked (a test's race). Another tarae's
/// `prune` there finds the lock free and removes the folder: the flock taken then is on a file no longer there —
/// so the lock is looked at after, by the descriptor (its device and inode) against `own/.lock` as it is now, and
/// made again when they differ.
fn hold_with(own: &Path, between: &dyn Fn()) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
    for n in 0..HOLD_TRIES {
        if n > 0 {
            crate::event::sleep(HOLD_WAIT);
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(own)?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(own.join(LOCK))?;
        between();
        // (still `own/.lock`: the file this descriptor is, by device and inode, is the one at that name)
        let still = |f: &std::fs::File| {
            let (held, there) = (f.metadata().ok(), std::fs::symlink_metadata(own.join(LOCK)).ok());
            held.zip(there).is_some_and(|(h, t)| h.dev() == t.dev() && h.ino() == t.ino())
        };
        if !still(&f) {
            continue;
        }
        // SAFETY: flock on a descriptor of ours, kept open in `State.lock` while this tarae lives.
        let locked = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        // (refused: this name is no one else's, so it's another tarae's prune — just before its removal, the file
        // still there; or its lock on the file it removed, outliving it a moment in a child forked meanwhile
        // until it execs. Either way: tried again, after it)
        if locked && still(&f) {
            return Ok(f);
        }
    }
    Err(std::io::Error::other("another tarae's prune kept taking it"))
}

/// The folders gone taraes left in `base` (named as `own_in` names them) removed: their lock free — the tarae that
/// held it ended, or exec'd — or none, past `UNLOCKED_KEPT`; this user's alone, a folder (no link), no one else's to
/// read; never `own`. Each round that takes this tarae's lock (its first). How many.
fn prune(base: &Path, own: &Path) -> usize {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let Ok(entries) = std::fs::read_dir(base) else { return 0 };
    // SAFETY: getuid only reads.
    let uid = unsafe { libc::getuid() };
    let mut n = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some((pid, hex)) = name.to_str().and_then(|n| n.split_once('-')) else { continue };
        let named =
            pid.parse::<u32>().is_ok() && hex.len() == 16 && hex.chars().all(|c| c.is_ascii_hexdigit());
        if !named || own.file_name() == Some(name.as_os_str()) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
            continue;
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(e.path().join(LOCK));
        // (the lock taken here is held until after the removal: `_held`)
        let (free, _held) = match lock {
            // SAFETY: flock on a descriptor of ours.
            Ok(f) => ((unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }) == 0, Some(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let age = m.modified().ok().and_then(|t| crate::clock::system().duration_since(t).ok());
                (age.is_some_and(|age| age > UNLOCKED_KEPT), None)
            }
            Err(_) => (false, None),
        };
        // (`remove_dir_all` follows no link inside it)
        if free && std::fs::remove_dir_all(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// What every git of a round is given before its subcommand: `PRE`, and each program git's config names that it
/// could run here (the user's, the repository's) taken out — a filter's commands emptied, not required (git then
/// reads the file as it is: a racy entry's check, a refresh, hashes it so, nothing run), a merge driver git's own.
/// Err: a name git's `-c` can't carry (a `=` in it): that workspace isn't taken.
fn guards(cmd: &dyn Fn(&[&str]) -> Result<std::process::Command, String>) -> Result<Vec<String>, String> {
    let keys = r"^(filter\..+\.(clean|smudge|process)|merge\..+\.driver)$";
    // (none: git's exit 1)
    let out = crate::gitrepo::ran(&cmd(&["config", "--name-only", "--get-regexp", keys])?, None)?;
    let mut args: Vec<String> = PRE.iter().map(|s| s.to_string()).collect();
    let mut set = |key: String| args.extend(["-c".to_string(), key]);
    for key in String::from_utf8_lossy(&out.stdout).lines().filter(|k| !k.is_empty()) {
        if key.contains('=') {
            return Err(format!("a filter or merge driver named with `=` ({key}): not taken"));
        }
        match key.strip_prefix("filter.") {
            Some(rest) => {
                let driver = &rest[..rest.rfind('.').unwrap_or(rest.len())];
                set(format!("{key}="));
                set(format!("filter.{driver}.required=false"));
            }
            None => set(format!("{key}={OWN_DRIVER}")),
        }
    }
    Ok(args)
}

/// `a` merged with `b` (a commit — another folder's, else the repository's): the conflicted files and each one's
/// lines on both sides. Objects written in `a`'s folder.
fn conflicts(a: &Snap, fa: &Folder, b: &str, fb: Option<&Folder>) -> Result<Vec<Conflict>, String> {
    let env = env(fa, &a.common, fb);
    let guards = guards(&|args| crate::snapshot::command_env(&a.top, args, None, &env))?;
    let cmd = |args: &[&str]| {
        let all: Vec<&str> = guards.iter().map(String::as_str).chain(args.iter().copied()).collect();
        crate::snapshot::command_env(&a.top, &all, None, &env)
    };
    let merge = ["merge-tree", "--write-tree", "--name-only", "-z", "--no-messages", &a.commit, b];
    let r = crate::gitrepo::ran(&cmd(&merge)?, None)?;
    let out = String::from_utf8_lossy(&r.stdout).into_owned();
    let mut parts = out.split('\0');
    // (a tree first, then the conflicted names — none past an error)
    let tree = parts.next().unwrap_or("");
    if tree.len() < 40 || !tree.bytes().all(|c| c.is_ascii_hexdigit()) {
        let err = String::from_utf8_lossy(&r.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("merge-tree failed").to_string());
    }
    let paths: Vec<String> = parts.filter(|p| !p.is_empty()).map(str::to_string).collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let base = crate::gitrepo::finish(&cmd(&["merge-base", &a.commit, b])?, None)
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())?;
    Ok(paths
        .into_iter()
        .map(|path| {
            let texts = texts(&cmd, [&base, &a.commit, b], &path);
            let (a, b) = match texts {
                Some([m, x, y]) => regions(&m, &x, &y),
                None => (None, None),
            };
            Conflict { path, a, b }
        })
        .collect())
}

/// `path`'s text at three commits (missing at one: empty) — None past `TEXT_MAX`, or a name a line can't carry.
fn texts(
    cmd: &dyn Fn(&[&str]) -> Result<std::process::Command, String>,
    revs: [&str; 3],
    path: &str,
) -> Option<[String; 3]> {
    if path.contains('\n') {
        return None;
    }
    let input: String = revs.iter().map(|r| format!("{r}:{path}\n")).collect();
    let out = crate::gitrepo::finish(&cmd(&["cat-file", "--batch"]).ok()?, Some(input.as_bytes())).ok()?;
    let mut rest = out.as_slice();
    let mut texts: Vec<String> = Vec::new();
    for _ in 0..3 {
        let nl = rest.iter().position(|&b| b == b'\n')?;
        let head = String::from_utf8_lossy(&rest[..nl]).into_owned();
        rest = &rest[nl + 1..];
        if head.ends_with(" missing") {
            texts.push(String::new());
            continue;
        }
        let size: usize = head.rsplit(' ').next()?.parse().ok()?;
        if size > TEXT_MAX || rest.len() < size {
            return None;
        }
        texts.push(String::from_utf8_lossy(&rest[..size]).into_owned());
        rest = rest.get(size + 1..).unwrap_or_default();
    }
    texts.try_into().ok()
}

/// The lines where `a` and `b`'s changes of `base` meet (or touch: git conflicts on those too), on each side — one
/// span each, from the first to the last. None: no two changes meet (a conflict of another kind).
pub fn regions(base: &str, a: &str, b: &str) -> (Option<Range<usize>>, Option<Range<usize>>) {
    let (ha, hb) = (hunks(base, a), hunks(base, b));
    let meet = |x: &Range<u32>, y: &Range<u32>| x.start <= y.end && y.start <= x.end;
    let widen = |r: &mut Option<Range<usize>>, x: &Range<u32>| {
        let x = x.start as usize..(x.end.max(x.start + 1)) as usize;
        *r = Some(match r.take() {
            Some(r) => r.start.min(x.start)..r.end.max(x.end),
            None => x,
        });
    };
    let (mut ra, mut rb) = (None, None);
    for (xb, xa) in &ha {
        for (yb, ya) in &hb {
            if meet(xb, yb) {
                widen(&mut ra, xa);
                widen(&mut rb, ya);
            }
        }
    }
    (ra, rb)
}

/// A side's changes of `base`: each one's lines there and its own.
fn hunks(base: &str, side: &str) -> Vec<(Range<u32>, Range<u32>)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let input = InternedInput::new(base, side);
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |before: Range<u32>, after: Range<u32>| out.push((before, after)));
    out
}

/// A round landed: the snapshots kept (of workspaces still open), the overlaps now — a new one said once, one
/// gone in `:messages` — and the quiet armed again if something moved meanwhile, or a take failed.
fn landed(ed: &mut Editor, r: Round) {
    ed.overlap.running = false;
    ed.overlap.rounds += 1;
    // (a take that failed: moved again, taken on the quiet as any change — `TRIES` in a row at most)
    let now = crate::clock::now();
    for id in &r.failed {
        if let Some(p) = ed.overlap.places.get_mut(id).filter(|p| p.tries < TRIES) {
            p.tries += 1;
            p.moved = Some(now);
        }
    }
    if !r.held {
        ed.unchanged = true;
        return arm(ed);
    }
    ed.overlap.merged += r.merged as u64;
    ed.overlap.renewed += r.renewed;
    if let Some(f) = r.lock {
        ed.overlap.lock = Some(Arc::new(f));
    }
    for (id, folder, snap) in r.snaps {
        if let Some(p) = ed.overlap.places.get_mut(&id).filter(|p| Arc::ptr_eq(&p.folder, &folder))
            && let Some(s) = snap
        {
            p.snap = Some(s);
            // (taken: its tries start over — not one whose last take stood in for it)
            if !r.failed.contains(&id) {
                p.tries = 0;
            }
        }
    }
    ed.overlap.merges = r.merges;
    let open: HashSet<SessionId> = sessions(ed).map(|l| l.id).collect();
    let mut now: Vec<Overlap> = Vec::new();
    for mut o in r.now.into_iter().filter(|o| open.contains(&o.a)) {
        o.b = as_session(ed, o.a, o.b);
        if matches!(o.b, Other::Session(id) if !open.contains(&id)) {
            continue;
        }
        if !now.iter().any(|n| same(n, &o)) {
            now.push(o);
        }
    }
    // (a merge that failed: its two sides' overlaps as found before stand — not gone, not said again)
    let unknown: Vec<(SessionId, Other)> =
        r.unknown.into_iter().map(|(a, b)| (a, as_session(ed, a, b))).collect();
    let stood: Vec<Overlap> = ed
        .overlap
        .now
        .iter()
        .filter(|o| unknown.iter().any(|(a, b)| same_pair(o, *a, b)) && !now.iter().any(|n| same(n, o)))
        .cloned()
        .collect();
    now.extend(stood);
    let was = std::mem::take(&mut ed.overlap.now);
    // (what was said and is gone: dropped — said again when it's back)
    let mut said = std::mem::take(&mut ed.overlap.said);
    said.retain(|k, _| now.iter().any(|o| said_key(o).0 == *k));
    for o in &now {
        let (key, lines) = said_key(o);
        let new = match said.get(&key) {
            None => true,
            // (the same pair and file, its lines elsewhere now: another conflict there)
            Some(was) => apart(was, &lines),
        };
        if new {
            said.insert(key, lines);
            let msg = toast(ed, o);
            let paint = toast_paint(ed, o);
            ed.set_warning(msg.clone());
            // (yours: a Claude hears of it only in the tool that touches the file — never from the `ui:` line; the
            // names in their colours, `≈` in warning, as the strip and the list say it)
            for t in ed.toasts.iter_mut().filter(|t| t.text.ends_with(&msg)) {
                t.persons = true;
                t.paint = paint.clone();
            }
        }
    }
    ed.overlap.said = said;
    for o in was.iter().filter(|w| !now.iter().any(|o| same(w, o))) {
        let msg = format!("{} — no longer conflicts", pair_said(ed, o, false));
        ed.recorded(crate::editor::ToastKind::Info, msg);
    }
    if now == was {
        ed.unchanged = true;
    }
    ed.overlap.now = now;
    arm(ed);
}

/// The other side as said: the base's tip as the session whose checkout has that branch (its name, its colour).
fn as_session(ed: &Editor, a: SessionId, b: Other) -> Other {
    match &b {
        Other::Branch(br) => {
            match sessions(ed).find(|l| l.id != a && l.git_branch.as_deref() == Some(br.as_str())) {
                Some(l) => Other::Session(l.id),
                None => b,
            }
        }
        Other::Session(_) => b,
    }
}

/// Whether overlap `o` is of sides `a` and `b` (either way round).
fn same_pair(o: &Overlap, a: SessionId, b: &Other) -> bool {
    (o.a == a && o.b == *b) || (*b == Other::Session(o.a) && o.b == Other::Session(a))
}

/// Two overlaps of one pair and one file (either way round).
fn same(x: &Overlap, y: &Overlap) -> bool {
    let flip = |o: &Overlap| match o.b {
        Other::Session(b) => Some((b, Other::Session(o.a))),
        Other::Branch(_) => None,
    };
    x.path == y.path && ((x.a == y.a && x.b == y.b) || flip(x).is_some_and(|(a, b)| a == y.a && b == y.b))
}

/// What a toast said is kept by — its two sides by session (the lower one first: either way round is one; never by
/// name, which a rename or a session opened again would share or lose) and its file — and the first side's lines.
fn said_key(o: &Overlap) -> (Said, Option<Range<usize>>) {
    match o.b {
        Other::Session(b) if b < o.a => ((b, Other::Session(o.a), o.path.clone()), o.other_lines.clone()),
        _ => ((o.a, o.b.clone(), o.path.clone()), o.lines.clone()),
    }
}

/// Whether two spans of lines are apart — neither meets nor touches the other (a whole file's meets every span).
fn apart(x: &Option<Range<usize>>, y: &Option<Range<usize>>) -> bool {
    match (x, y) {
        (Some(x), Some(y)) => x.end < y.start || y.end < x.start,
        _ => false,
    }
}

/// The base's branch as said: the session whose checkout has it (its name, its colour), else the branch, plain.
pub fn named_branch(ed: &Editor, branch: &str) -> (String, Option<u8>) {
    name(ed, &as_session(ed, ed.session.id, Other::Branch(branch.to_string())))
}

/// A file of session `id`'s to show merged (the session list's `v`): one where it would conflict — with the front
/// session first — and the other side there; else the first it changed (no side).
pub fn file_of(ed: &Editor, id: SessionId) -> Option<(String, Option<Other>)> {
    let me = ed.session.id;
    let with = |o: &&Overlap| o.a == id || o.b == Other::Session(id);
    let theirs = |o: &Overlap| match o.a == id {
        true => as_session(ed, id, o.b.clone()),
        false => Other::Session(o.a),
    };
    let first = ed.overlap.now.iter().filter(with).find(|o| o.a == me || o.b == Other::Session(me));
    if let Some(o) = first.or_else(|| ed.overlap.now.iter().find(with)) {
        let side = match id == me {
            true => theirs(o),
            false => Other::Session(id),
        };
        return Some((o.path.clone(), Some(side)));
    }
    let snap = ed.overlap.places.get(&id)?.snap.as_ref()?;
    snap.changed.iter().find(|p| !key_named(&snap.top, p)).map(|p| (p.clone(), None))
}

/// Session `id`'s working tree as last taken (its commit) — the woven view asks again when it moves.
pub fn snapshot_commit(ed: &Editor, id: SessionId) -> Option<String> {
    ed.overlap.places.get(&id)?.snap.as_ref().map(|s| s.commit.clone())
}

/// Session `id`'s working tree as last taken, to read a file of (`weave.rs`, when its disk text can't be read).
pub fn snapshot_reader(ed: &Editor, id: SessionId) -> Option<SnapRead> {
    let p = ed.overlap.places.get(&id)?;
    let s = p.snap.as_ref()?;
    Some(SnapRead {
        commit: s.commit.clone(),
        top: s.top.clone(),
        common: s.common.clone(),
        folder: p.folder.clone(),
    })
}

/// A snapshot to read a file of: its commit's objects in its folder, the repository's read too — never a fetch.
pub struct SnapRead {
    commit: String,
    top: PathBuf,
    common: PathBuf,
    folder: Arc<Folder>,
}

impl SnapRead {
    /// `rel`'s text in it (None: not there, unreadable, past `TEXT_MAX`, binary, or its workspace gone).
    pub fn read(&self, rel: &str) -> Option<String> {
        let _one = self.folder.one.lock().unwrap_or_else(|e| e.into_inner());
        if self.folder.ended.load(Ordering::SeqCst) || rel.contains('\n') {
            return None;
        }
        let env = env(&self.folder, &self.common, None);
        let spec = format!("{}:{rel}", self.commit);
        let cmd = crate::snapshot::command_env(&self.top, &["cat-file", "blob", &spec], None, &env).ok()?;
        let b = crate::gitrepo::finish(&cmd, None).ok()?;
        (b.len() <= TEXT_MAX && !b.contains(&0)).then(|| String::from_utf8_lossy(&b).into_owned())
    }
}

/// A side by its name and its colour (a branch no session has: its name, no colour).
pub fn name(ed: &Editor, side: &Other) -> (String, Option<u8>) {
    match side {
        Other::Session(id) => match sessions(ed).find(|l| l.id == *id) {
            Some(l) => (l.label().to_string(), Some(l.colour)),
            None => (String::new(), None),
        },
        Other::Branch(b) => (b.clone(), None),
    }
}

/// `L210–260` · `L30` (from 1).
pub fn lines_said(r: &Range<usize>) -> String {
    match r.end <= r.start + 1 {
        true => format!("L{}", r.start + 1),
        false => format!("L{}–{}", r.start + 1, r.end),
    }
}

/// The two sides as said, the front session's first when it's one of the two — and the first's lines.
fn sides<'o>(ed: &Editor, o: &'o Overlap) -> (Other, Other, &'o Option<Range<usize>>) {
    match o.b == Other::Session(ed.session.id) {
        true => (o.b.clone(), Other::Session(o.a), &o.other_lines),
        false => (Other::Session(o.a), o.b.clone(), &o.lines),
    }
}

/// What a new overlap's toast paints (`Toast.paint`): the two names in their participant colours, `≈` in warning.
fn toast_paint(ed: &Editor, o: &Overlap) -> Vec<(String, &'static str)> {
    let (first, second, _) = sides(ed, o);
    let named = |side: &Other| {
        let (name, c) = name(ed, side);
        (name, c.map_or("", |c| crate::session::PARTICIPANTS[c as usize % crate::session::COLOURS]))
    };
    vec![named(&first), ("≈".to_string(), "warning"), named(&second)]
}

/// `fix ≈ main · src/term.rs L210–260` — the front session's side first when it's one of the two (its lines then).
fn pair_said(ed: &Editor, o: &Overlap, with_lines: bool) -> String {
    let (first, second, lines) = sides(ed, o);
    let lines =
        lines.as_ref().filter(|_| with_lines).map(|l| format!(" {}", lines_said(l))).unwrap_or_default();
    format!("{} ≈ {} · {}{lines}", name(ed, &first).0, name(ed, &second).0, o.path)
}

/// A new overlap's toast: `fix ≈ main · src/term.rs L210–260 — would conflict`.
fn toast(ed: &Editor, o: &Overlap) -> String {
    format!("{} — would conflict", pair_said(ed, o, true))
}

/// The other sides session `id`'s workspace would conflict with, each once (the strip and the list: `≈ main`).
pub fn others(ed: &Editor, id: SessionId) -> Vec<(String, Option<u8>)> {
    let mut out: Vec<(String, Option<u8>)> = Vec::new();
    for o in &ed.overlap.now {
        let other = match &o.b {
            _ if o.a == id => o.b.clone(),
            Other::Session(b) if *b == id => Other::Session(o.a),
            _ => continue,
        };
        let n = name(ed, &other);
        if !n.0.is_empty() && !out.iter().any(|x| x.0 == n.0) {
            out.push(n);
        }
    }
    out
}

/// Session `id`'s overlaps as the UI state lists them: `main · src/a.rs L3–5` (its own lines).
pub fn listed(ed: &Editor, id: SessionId) -> Vec<String> {
    ed.overlap
        .now
        .iter()
        .filter_map(|o| match &o.b {
            _ if o.a == id => Some((o.b.clone(), &o.lines, &o.path)),
            Other::Session(b) if *b == id => Some((Other::Session(o.a), &o.other_lines, &o.path)),
            _ => None,
        })
        .map(|(other, lines, path)| {
            let lines = lines.as_ref().map(|l| format!(" {}", lines_said(l))).unwrap_or_default();
            format!("{} · {path}{lines}", name(ed, &other).0)
        })
        .collect()
}

/// Session `id`'s overlaps in file `rel` (from its checkout's top): the other side and the lines on `id`'s side.
fn in_file_of(ed: &Editor, id: SessionId, rel: &str) -> Vec<(Other, Option<Range<usize>>)> {
    ed.overlap
        .now
        .iter()
        .filter(|o| o.path == rel)
        .filter_map(|o| match &o.b {
            _ if o.a == id => Some((o.b.clone(), o.lines.clone())),
            Other::Session(b) if *b == id => Some((Other::Session(o.a), o.other_lines.clone())),
            _ => None,
        })
        .collect()
}

/// The front workspace's overlaps in file `rel` (the side panel's row: `≈m` — `presence::file_marks`): each other
/// side once, its name, its colour, the lines on your side.
pub fn in_file(ed: &Editor, rel: &Path) -> Vec<(String, Option<u8>, Option<Range<usize>>)> {
    let rel = rel.to_string_lossy();
    let mut out: Vec<(String, Option<u8>, Option<Range<usize>>)> = Vec::new();
    for (other, lines) in in_file_of(ed, ed.session.id, &rel) {
        let (n, c) = name(ed, &other);
        if !out.iter().any(|x| x.0 == n) {
            out.push((n, c, lines));
        }
    }
    out
}

/// What Claude's `edit` or `read` answer says of `path` (in its session's checkout) — the woven view's words where
/// one of the file was worked out (`weave::told`), else a line per other side its file would conflict with —
/// `src/a.rs: fix's change would conflict here: L30–34` — its own lines, the other's name, nothing else. None: no
/// overlap there.
pub fn told(ed: &Editor, path: &Path) -> Vec<String> {
    // (where the file was woven in memory — the buffers, unsaved too — that weave says it: `weave::told`)
    if let Some(lines) = crate::weave::told(ed, path) {
        return lines;
    }
    if ed.overlap.now.is_empty() {
        return Vec::new();
    }
    let top = ed.overlap.places.get(&ed.session.id).and_then(|p| p.snap.as_ref()).map(|s| s.top.clone());
    let top = top.unwrap_or_else(|| ed.session.root.clone());
    let Ok(rel) = path.strip_prefix(&top) else { return Vec::new() };
    let rel = rel.to_string_lossy();
    in_file_of(ed, ed.session.id, &rel)
        .into_iter()
        .map(|(other, lines)| {
            let who = crate::cut::fit_ellipsis(&name(ed, &other).0, 32);
            match lines {
                Some(l) => format!("{rel}: {who}'s change would conflict here: {}", lines_said(&l)),
                None => format!("{rel}: {who}'s change would conflict with this file"),
            }
        })
        .collect()
}

/// Session `id`'s workspace ended (`session_close`): its overlaps gone, its folder removed on a worker — once no
/// round is writing in it, and never written again (`Folder.ended`).
pub fn ended(ed: &mut Editor, id: SessionId) {
    ed.overlap.now.retain(|o| o.a != id && o.b != Other::Session(id));
    ed.overlap.said.retain(|k, _| k.0 != id && k.1 != Other::Session(id));
    let Some(p) = ed.overlap.places.remove(&id) else { return };
    p.folder.ended.store(true, Ordering::SeqCst);
    let folder = p.folder;
    ed.events.jobs().background(move || {
        let _one = folder.one.lock().unwrap_or_else(|e| e.into_inner());
        if folder.newest() && folder.path.exists() {
            std::fs::remove_dir_all(&folder.path).ok();
        }
    });
}

#[cfg(test)]
mod tests;
