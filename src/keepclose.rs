//! collab C3 — workspaces kept close to their base (`target/sb/collab-plan.md` §2.4): the longer one stays behind,
//! the bigger its conflicts.
//!
//! - **The base watched**: a thread per base (its repository's git folder and branch — `watch`), its loose ref and
//!   `packed-refs`, the tip read from those files (no git run); the first read is the baseline. Never in tests: they
//!   say a tip by `tip`, as the watch's job does.
//! - **On a move**: an overlap round on the new tip (`overlap::ask`, no tree taken again), then a look at every
//!   workspace on that base (`look`, a worker): how far behind and ahead, its changes in no commit, its instruction
//!   files behind the main checkout's — and where it would conflict, from that round (`Editor.overlap.now`).
//! - **One card for all of them** (`What::BaseMoved`, the person's one decision for every workspace — what would
//!   conflict and where, in words): `y` moves the clean ones, `c` rebases the conflicting ones too through the
//!   resolve loop (`rebase_check`), `v` the instruction files' changes, `n` not now. A newer tip takes the card's
//!   place (one title a base). `workspace.sync = "idle"`: the clean ones move unasked, a card only for the
//!   conflicting; `"off"`: nothing.
//! - **Moved only when idle** (`busy`): never mid-turn, mid-review, mid-rebase or mid-run, nor with unsaved buffers
//!   there; one at a time (`next`, every event). The move (`move_onto`, a worker): `git rebase --autostash` onto the
//!   tip read as it starts — the newest wins; a conflict after all is aborted (its changes back) and said; changes
//!   that didn't go back are said from git's own words. While it runs the workspace's messages, runs and saves wait
//!   (`Session.moving`, `hold_save`), what else would write there is refused, said (`refused`: `space u`, `:w` to a
//!   new name, a change set); its reloads are one line (`disk_batch_start`).
//!
//! Claude hears nothing of it (the user, 2026-10-23): a file a move changed is "changed since you last read it".

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::document::DocId;
use crate::editor::{Editor, ToastKind};
use crate::event::SessionId;
use crate::offer::{Offer, What};
use crate::session::Session;

/// `workspace.sync`'s values: a card for every move · the clean idle ones moved unasked, a card for the conflicting
/// ones · nothing.
pub const MODES: &[&str] = &["ask", "idle", "off"];

/// A base: its repository's git folder (real path) and its branch.
pub type Base = (PathBuf, String);

/// What's kept close (`Editor.keep_close`).
pub struct State {
    /// Each base's tip as last read — the first read is the baseline, never a move.
    tips: HashMap<Base, String>,
    /// Bases with a watch going.
    watched: HashSet<Base>,
    /// A move not yet answered, by base.
    pending: HashMap<Base, Pending>,
    /// Workspaces to move once idle (answered, or unasked by `idle`), in the order they came.
    pub waits: Vec<Wait>,
    /// The one move under way.
    pub moving: Option<SessionId>,
    /// The overlap round last landed (`overlap::State.started` as it began).
    round: u64,
    /// Whether bases are watched (a thread each) — never in tests.
    pub watch: bool,
    /// Saves into the checkout being moved, held until it lands (`hold_save`): git's rebase would stop on one, and
    /// its abort put the file's old text back.
    pub saves: Vec<Held>,
    /// The moving checkout's buffers' disk stamps as the move began (`start`): one still at its stamp, its file
    /// one the move left as it was, is written as it is (`release`).
    stamps: HashMap<DocId, Option<crate::disk::Stamp>>,
    /// Moves landed, looks landed (tests count them).
    pub moved: u64,
    pub looked: u64,
}

/// A save held while its checkout moves (`hold_save`), as it was asked.
#[derive(Debug)]
pub struct Held {
    pub id: DocId,
    session: SessionId,
    force: bool,
    then: crate::saver::Then,
    beneath: Option<PathBuf>,
}

impl Default for State {
    fn default() -> State {
        State {
            tips: HashMap::new(),
            watched: HashSet::new(),
            pending: HashMap::new(),
            waits: Vec::new(),
            moving: None,
            round: 0,
            watch: !cfg!(test),
            saves: Vec::new(),
            stamps: HashMap::new(),
            moved: 0,
            looked: 0,
        }
    }
}

#[cfg(test)]
impl State {
    /// Each base's move unanswered, as a trace says it: `main 1a2b3c4 after 3` (`looking` while a look is out).
    pub fn said(&self) -> String {
        let mut v: Vec<String> = self
            .pending
            .iter()
            .map(|((_, b), p)| {
                let looking = if p.looking { " looking" } else { "" };
                format!("{b} {} after {}{looking}", &p.to[..7.min(p.to.len())], p.after)
            })
            .collect();
        v.sort();
        format!("[{}] round {}", v.join(", "), self.round)
    }
}

/// A base's move not answered yet: the tip before it (its first unanswered move's), the tip now, the overlap rounds
/// started by the time it came (one begun after it predicts on the new tip), and a look out for it.
struct Pending {
    from: String,
    to: String,
    after: u64,
    looking: bool,
}

/// A workspace to move once it's idle — its rebase through the resolve loop when it would conflict.
#[derive(Clone, Debug, PartialEq)]
pub struct Wait {
    pub base: Base,
    pub ws: Ws,
    pub conflicting: bool,
    /// Moved unasked (`workspace.sync = "idle"`): said so.
    pub unasked: bool,
}

/// A base's move, as its card asks it.
#[derive(Clone, Debug, PartialEq)]
pub struct Moves {
    pub base: Base,
    pub from: String,
    pub to: String,
    /// The base's commits since `from`.
    pub commits: usize,
    /// The workspaces behind its tip, the card's.
    pub ws: Vec<Ws>,
    /// The clean ones `idle` moves unasked (their names) — the card is the conflicting ones'.
    pub unasked: Vec<String>,
}

/// Where a workspace would conflict: a file and its lines there (None: the file as a whole).
pub type Conflicts = Vec<(String, Option<Range<usize>>)>;

/// A workspace behind its base's new tip.
#[derive(Clone, Debug, PartialEq)]
pub struct Ws {
    pub id: SessionId,
    pub name: String,
    pub root: PathBuf,
    pub branch: String,
    /// The base's commits it hasn't · its own the base hasn't.
    pub behind: usize,
    pub ahead: usize,
    /// Files changed in no commit there (they go with it: git's autostash).
    pub changed: usize,
    /// Where it would conflict on the new tip — a file and its lines there (None: the file as a whole), as the
    /// overlap round on that tip found.
    pub conflicts: Conflicts,
    /// Its instruction files the main checkout changed since its copy, each with the changed lines (`v`).
    pub instructions: Vec<(String, Vec<String>)>,
}

impl Ws {
    /// `fix — 4 behind, its 2 changed files go with it` · `db — would conflict in src/term.rs L210–260`.
    fn row(&self) -> String {
        let mut parts = vec![format!("{} behind", self.behind)];
        if !self.conflicts.is_empty() {
            let files: Vec<String> = self
                .conflicts
                .iter()
                .take(3)
                .map(|(p, l)| match l {
                    Some(l) => format!("{p} {}", crate::overlap::lines_said(l)),
                    None => p.clone(),
                })
                .collect();
            let more = match self.conflicts.len() {
                n if n > 3 => format!(" +{}", n - 3),
                _ => String::new(),
            };
            parts.push(format!("would conflict in {}{more}", files.join(", ")));
        }
        match self.changed {
            0 => {}
            1 => parts.push("its changed file goes with it".into()),
            n => parts.push(format!("its {n} changed files go with it")),
        }
        if !self.instructions.is_empty() {
            let names: Vec<&str> = self.instructions.iter().map(|(r, _)| r.as_str()).collect();
            parts.push(format!("{} newer in the main checkout", names.join(", ")));
        }
        format!("  {} — {}", self.name, parts.join(", "))
    }
}

impl Moves {
    fn clean(&self) -> impl Iterator<Item = &Ws> {
        self.ws.iter().filter(|w| w.conflicts.is_empty())
    }

    fn conflicting(&self) -> impl Iterator<Item = &Ws> {
        self.ws.iter().filter(|w| !w.conflicts.is_empty())
    }

    /// Whether `c` answers its card: some clean, some conflicting (all conflicting: `y` rebases them).
    pub fn mixed(&self) -> bool {
        self.clean().next().is_some() && self.conflicting().next().is_some()
    }

    /// Its card's answers.
    pub fn answers(&self, shows: bool) -> Vec<(char, &'static str)> {
        let mut v = match (self.clean().next().is_some(), self.conflicting().next().is_some()) {
            (true, true) => vec![('y', "Move the clean ones"), ('c', "Conflicting ones too")],
            (false, true) => vec![('y', "Rebase them now")],
            _ => vec![(
                'y',
                match self.ws.len() {
                    1 => "Move it",
                    _ => "Move them",
                },
            )],
        };
        if shows {
            v.push(('v', "Instruction files"));
        }
        v.push(('n', "Not now"));
        v
    }

    /// The card: the base's move, a row a workspace — in words, what would conflict and where — and what each
    /// answer does. One title a base, so a newer tip's card takes its place.
    pub fn card(&self) -> Offer {
        let base = &self.base.1;
        let n = self.ws.len();
        let them = if n == 1 { "1 workspace".to_string() } else { format!("{n} workspaces") };
        // (the title says the base moved: the first line, by how much and who's behind)
        let commits = match self.commits {
            1 => "1 new commit".to_string(),
            k => format!("{k} new commits"),
        };
        let mut q = vec![format!("{commits} — {them} behind it:")];
        q.push(String::new());
        let first = q.len();
        q.extend(self.ws.iter().map(Ws::row));
        let rows = first..q.len();
        let warned: Vec<Range<usize>> = self
            .ws
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.conflicts.is_empty())
            .map(|(i, _)| first + i..first + i + 1)
            .collect();
        q.push(String::new());
        let one = |n: usize, a: &'static str, b: &'static str| if n == 1 { a } else { b };
        let idle = |n: usize| {
            format!(
                "once {} idle — its Claude between turns, no review, rebase or run under way",
                one(n, "it's", "each is")
            )
        };
        q.push(match (self.clean().count(), self.conflicting().count()) {
            (_, 0) => format!(
                "y moves {} onto {base}'s tip {}: {} commits replayed there, {} changes in no commit taken along \
                 (git's autostash).",
                one(n, "it", "them"),
                idle(n),
                one(n, "its", "their"),
                one(n, "its", "their"),
            ),
            (0, c) => format!(
                "y rebases {} onto {base}'s tip {}. A conflict goes to {} own Claude to resolve, and you review \
                 that before the rebase goes on.",
                one(c, "it", "them"),
                idle(c),
                one(c, "its", "each one's"),
            ),
            (k, c) => format!(
                "y moves the clean {} onto {base}'s tip {}, changes in no commit taken along (git's autostash). c \
                 rebases the conflicting {} too, {} own Claude resolving it, while it's small.",
                one(k, "one", "ones"),
                idle(k),
                one(c, "one", "ones"),
                one(c, "its", "each one's"),
            ),
        });
        if !self.unasked.is_empty() {
            q.push(String::new());
            q.push(format!(
                "{} {} moving by {} (workspace.sync = \"idle\").",
                self.unasked.join(", "),
                one(self.unasked.len(), "is", "are"),
                one(self.unasked.len(), "itself", "themselves"),
            ));
        }
        let mut card = Offer::new(
            format!("{base} moved"),
            &q.join("\n"),
            "Moving them…",
            format!("Couldn't move them onto {base}"),
            What::BaseMoved(Box::new(self.clone())),
        );
        card.decided = Some(0..rows.end);
        card.warnings = warned;
        let shown: Vec<String> = self
            .ws
            .iter()
            .flat_map(|w| w.instructions.iter().map(move |i| (w, i)))
            .flat_map(|(w, (rel, lines))| {
                std::iter::once(format!("{} · {rel} — the main checkout's changes since its copy", w.name))
                    .chain(lines.iter().cloned())
                    .chain(std::iter::once(String::new()))
            })
            .collect();
        if !shown.is_empty() {
            card.output = Some((format!("instruction files · {base} moved"), shown));
        }
        card
    }
}

/// The sessions, the front one first.
fn sessions(ed: &Editor) -> impl Iterator<Item = &Session> {
    std::iter::once(&ed.session).chain(&ed.sessions)
}

fn mode(ed: &Editor) -> &str {
    ed.config.workspace_sync.as_str()
}

/// A session's base branch: its own, else the branch of the checkout tarae started in (the home one has none).
fn base_of(ed: &Editor, l: &Session) -> Option<String> {
    let home = || sessions(ed).find(|s| s.id == 0).and_then(|s| s.git_branch.clone());
    match l.id {
        0 => None,
        _ => l.base.clone().or_else(home),
    }
}

/// Every event: each base watched (its repository known — `gitrepo::cached`, no git here), and the next workspace
/// waiting moved once it's idle.
pub fn sync(ed: &mut Editor) {
    if mode(ed) == "off" || ed.sessions.is_empty() {
        return;
    }
    if ed.keep_close.watch {
        let bases: Vec<Base> = sessions(ed)
            .filter_map(|l| Some((crate::gitrepo::cached(&l.root)?.common_dir.clone(), base_of(ed, l)?)))
            .collect();
        for b in bases {
            if ed.keep_close.watched.insert(b.clone()) {
                watch(ed.events.shared_sender(), b);
            }
        }
    }
    next(ed);
}

/// `branch`'s tip as git's files have it — its loose ref, else `packed-refs`' line (no git run). None: no such
/// branch, or a name that could leave `refs/heads`.
pub fn read_tip(common: &Path, branch: &str) -> Option<String> {
    let parts_ok = branch.split('/').all(|p| !p.is_empty() && p != "." && p != "..");
    if !parts_ok || branch.contains(['\0', '\\']) {
        return None;
    }
    let hex = |s: &str| (s.len() >= 40 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_string());
    if let Ok(s) = std::fs::read_to_string(common.join("refs/heads").join(branch)) {
        return hex(s.trim());
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    let name = format!("refs/heads/{branch}");
    packed.lines().find_map(|l| l.split_once(' ').filter(|(_, r)| *r == name).and_then(|(sha, _)| hex(sha)))
}

/// A thread watching `base`'s ref — its loose file and `packed-refs` — saying each tip it reads (`tip`). A send
/// failing (tarae going) ends it.
fn watch(tx: crate::event::Tx, base: Base) {
    crate::event::thread("tarae-basewatch", move || {
        let (common, branch) = base.clone();
        let mut last = None;
        loop {
            let mut w = crate::fswatch::Watcher::new(); // (armed before the read — see `fswatch`)
            w.file(&common.join("refs/heads").join(&branch));
            w.file(&common.join("packed-refs"));
            let now = read_tip(&common, &branch);
            if let Some(t) = now.clone().filter(|_| now != last) {
                last = now;
                let b = base.clone();
                let said = move |ed: &mut Editor| tip(ed, b, t);
                if tx.send(crate::event::Event::Job(Box::new(said))).is_err() {
                    break;
                }
            }
            w.wait();
        }
    });
}

/// `base`'s tip read (the watch's news): the first is the baseline; a move asks an overlap round on the new tip,
/// then a look at the workspaces on it.
pub fn tip(ed: &mut Editor, base: Base, tip: String) {
    ed.unchanged = true;
    let st = &mut ed.keep_close;
    let Some(was) = st.tips.insert(base.clone(), tip.clone()).filter(|w| *w != tip) else { return };
    if mode(ed) == "off" {
        return;
    }
    let after = ed.overlap.started;
    let p = ed.keep_close.pending.entry(base.clone()).or_insert(Pending {
        from: was,
        to: tip.clone(),
        after,
        looking: false,
    });
    (p.to, p.after) = (tip, after);
    // (no rounds — overlaps off, or one session: nothing to predict by, looked at now)
    if ed.overlap.dir.is_none() || ed.sessions.is_empty() {
        return look(ed, &base);
    }
    crate::overlap::ask(ed);
}

/// An overlap round landed (`seq`: begun as the `seq`th): each base's move it predicts on looked at.
pub fn landed(ed: &mut Editor, seq: u64) {
    ed.keep_close.round = ed.keep_close.round.max(seq);
    let ready: Vec<Base> = ed
        .keep_close
        .pending
        .iter()
        .filter(|(_, p)| !p.looking && seq > p.after)
        .map(|(b, _)| b.clone())
        .collect();
    for b in ready {
        look(ed, &b);
    }
}

/// The workspaces on `base` looked at on a worker — where each stands against the new tip.
fn look(ed: &mut Editor, base: &Base) {
    let Some(p) = ed.keep_close.pending.get_mut(base) else { return };
    p.looking = true;
    let (from, to) = (p.from.clone(), p.to.clone());
    let on: Vec<(SessionId, String, PathBuf, Conflicts)> = sessions(ed)
        .filter(|l| base_of(ed, l).as_deref() == Some(base.1.as_str()))
        .map(|l| (l.id, l.label().to_string(), l.root.clone(), conflicts(ed, l.id, &base.1)))
        .collect();
    let base = base.clone();
    ed.events.shared_jobs().spawn(move || {
        let mut ws = Vec::new();
        for (id, name, root, conflicts) in on {
            let Ok(repo) = crate::gitrepo::repo(&root) else { continue };
            if repo.common_dir != base.0 {
                continue;
            }
            let branch = crate::gitrepo::git(&root, &["symbolic-ref", "-q", "--short", "HEAD"], None);
            let Ok(branch) = branch.map(|b| b.trim().to_string()) else { continue };
            let main = crate::worktree::main_checkout(&base.0, &root);
            ws.push(Ws {
                id,
                name,
                branch,
                behind: crate::worktree::count(&root, &format!("HEAD..{to}")),
                ahead: crate::worktree::count(&root, &format!("{to}..HEAD")),
                changed: crate::rebase::changed(&root).map_or(0, |c| c.len()),
                conflicts,
                instructions: main.map(|m| crate::instructions::behind(&root, &m)).unwrap_or_default(),
                root,
            });
        }
        let commits = ws.first().map_or(0, |w| crate::worktree::count(&w.root, &format!("{from}..{to}")));
        let moves = Moves { base, from, to, commits, ws, unasked: Vec::new() };
        move |ed: &mut Editor| looked(ed, moves)
    });
}

/// Where session `id`'s workspace would conflict on its base `branch`'s tip — the last round's overlaps against
/// the tip, or the checkout that has it (the home one).
fn conflicts(ed: &Editor, id: SessionId, branch: &str) -> Conflicts {
    use crate::overlap::Other;
    let tip_side = |o: &Other| match o {
        Other::Branch(b) => b == branch,
        Other::Session(s) => sessions(ed).any(|l| l.id == *s && l.git_branch.as_deref() == Some(branch)),
    };
    let mut v: Conflicts = Vec::new();
    for o in &ed.overlap.now {
        let mine = match (
            o.a == id && tip_side(&o.b),
            o.b == Other::Session(id) && tip_side(&Other::Session(o.a)),
        ) {
            (true, _) => Some(o.lines.clone()),
            (_, true) => Some(o.other_lines.clone()),
            _ => None,
        };
        if let Some(lines) = mine.filter(|_| !v.iter().any(|(p, _)| *p == o.path)) {
            v.push((o.path.clone(), lines));
        }
    }
    v
}

/// A look landed: the workspaces behind the tip on the card (`idle`: the clean ones moved unasked, the card the
/// conflicting ones') — a tip newer than it looked at: looked at again. None behind: no card, and one up for that
/// base goes.
fn looked(ed: &mut Editor, mut moves: Moves) {
    ed.keep_close.looked += 1;
    let base = moves.base.clone();
    let Some(p) = ed.keep_close.pending.get_mut(&base) else { return };
    p.looking = false;
    if p.to != moves.to {
        if ed.keep_close.round > p.after || ed.overlap.dir.is_none() || ed.sessions.is_empty() {
            look(ed, &base);
        }
        return;
    }
    let open: Vec<SessionId> = sessions(ed).map(|l| l.id).collect();
    moves.ws.retain(|w| w.behind > 0 && open.contains(&w.id));
    if mode(ed) == "idle" {
        let (clean, conflicting): (Vec<Ws>, Vec<Ws>) =
            moves.ws.drain(..).partition(|w| w.conflicts.is_empty());
        moves.unasked = clean.iter().map(|w| w.name.clone()).collect();
        for ws in clean {
            wait(ed, Wait { base: base.clone(), ws, conflicting: false, unasked: true });
        }
        moves.ws = conflicting;
    }
    if moves.ws.is_empty() {
        ed.keep_close.pending.remove(&base);
        card_gone(ed, &base, "nothing behind it now");
        return next(ed);
    }
    ed.unchanged = false;
    // (its workspaces named in their colours, as the strip and the list name them)
    let mut card = moves.card();
    card.paint = (moves.ws.iter())
        .filter_map(|w| {
            let l = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == w.id)?;
            let scope = crate::session::PARTICIPANTS[l.colour as usize % crate::session::COLOURS];
            Some((w.name.clone(), scope))
        })
        .collect();
    ed.push_offer(card);
}

/// The card up for `base`, gone — said why (the gone ring).
fn card_gone(ed: &mut Editor, base: &Base, why: &str) {
    let mine = |o: &Offer| matches!(&o.what, What::BaseMoved(m) if m.base == *base);
    let ids: Vec<u64> = ed.offers.iter().filter(|o| mine(o)).map(|o| o.id).collect();
    ed.offers.retain(|o| !mine(o));
    for id in ids {
        ed.card_how(id, why.to_string());
    }
}

/// `w` waits its turn — in place of one of that workspace's waiting (the newest wins).
fn wait(ed: &mut Editor, w: Wait) {
    let waits = &mut ed.keep_close.waits;
    waits.retain(|x| x.ws.id != w.ws.id);
    waits.push(w);
}

/// The card answered: `y` the clean ones (all conflicting: them), `c` the conflicting ones too — each once it's
/// idle. The base's move is answered: its next one asks again.
pub fn answered(ed: &mut Editor, m: Moves, c: char) {
    ed.keep_close.pending.remove(&m.base);
    let all = c == 'c' || m.clean().next().is_none();
    let mut n = 0;
    for ws in m.ws.iter().filter(|w| all || w.conflicts.is_empty()) {
        let conflicting = !ws.conflicts.is_empty();
        wait(ed, Wait { base: m.base.clone(), ws: ws.clone(), conflicting, unasked: false });
        n += 1;
    }
    let them = if n == 1 { "1 workspace".to_string() } else { format!("{n} workspaces") };
    let busy: Vec<String> =
        m.ws.iter()
            .filter(|w| all || w.conflicts.is_empty())
            .filter_map(|w| {
                sessions(ed)
                    .find(|l| l.id == w.id)
                    .and_then(|l| busy(ed, l))
                    .map(|why| format!("{} — {why}", w.name))
            })
            .collect();
    let waiting = match busy.is_empty() {
        true => String::new(),
        false => format!("; waiting: {}", busy.join(", ")),
    };
    ed.set_status(format!("Moving {them} onto {}, each once it's idle{waiting}", m.base.1));
    next(ed);
}

/// `n`: left where they are — the base's next move asks again.
pub fn dismissed(ed: &mut Editor, m: &Moves) {
    ed.keep_close.pending.remove(&m.base);
    let base = &m.base.1;
    ed.set_status(format!("Left behind {base} — its next move asks again (or :session-bring one)"));
}

/// Why session `l`'s workspace isn't moved now — None: it's idle.
pub fn busy(ed: &Editor, l: &Session) -> Option<&'static str> {
    use crate::session::Doing;
    if l.rebase.is_some() || l.moving.is_some() {
        return Some("a rebase under way");
    }
    if !matches!(ed.doing(l), Doing::Idle | Doing::Done) {
        return Some("its Claude at work");
    }
    if l.review.is_some() || l.code_review.is_some() {
        return Some("a review open");
    }
    let task = l.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running);
    let tests = l.test_run.as_ref().is_some_and(|r| r.state == crate::testing::RunState::Running);
    if task || tests || !l.runs_next.is_empty() {
        return Some("a run under way");
    }
    if l.applying.is_some() {
        return Some("a change set landing");
    }
    let queue = ed.bring_queue.run.as_ref();
    if queue.is_some_and(|r| r.now.is_some_and(|(id, _)| id == l.id) || r.left.contains(&l.id)) {
        return Some("in the bring queue");
    }
    let unsaved = |d: &crate::document::Document| {
        d.is_modified() && d.path.as_ref().is_some_and(|p| p.starts_with(&l.root))
    };
    if ed.docs.iter().any(unsaved) {
        return Some("unsaved buffers there");
    }
    None
}

/// The first workspace waiting that's idle now moved — one at a time; one whose session ended dropped.
fn next(ed: &mut Editor) {
    if ed.keep_close.moving.is_some() || ed.keep_close.waits.is_empty() {
        return;
    }
    let open: Vec<SessionId> = sessions(ed).map(|l| l.id).collect();
    ed.keep_close.waits.retain(|w| open.contains(&w.ws.id));
    let idle = ed
        .keep_close
        .waits
        .iter()
        .position(|w| sessions(ed).find(|l| l.id == w.ws.id).is_some_and(|l| busy(ed, l).is_none()));
    let Some(i) = idle else { return };
    let w = ed.keep_close.waits.remove(i);
    if w.conflicting {
        // (today's resolve loop: its rebase, a conflict to its own Claude, your review before it goes on)
        let m = crate::rebase::Moved {
            root: w.ws.root.clone(),
            name: w.ws.name.clone(),
            branch: w.ws.branch.clone(),
            base: w.base.1.clone(),
            moved: w.ws.behind,
            ahead: w.ws.ahead,
        };
        ed.with_session(w.ws.id, |ed| ed.rebase_check(m, true));
        return next(ed);
    }
    start(ed, w);
}

/// The move started on a worker: its session's messages and runs wait meanwhile, its reloads said as one line.
fn start(ed: &mut Editor, w: Wait) {
    let token = crate::disk::batch_token();
    let (id, base) = (w.ws.id, w.base.1.clone());
    ed.keep_close.moving = Some(id);
    let under = |d: &&crate::document::Document| d.path.as_ref().is_some_and(|p| p.starts_with(&w.ws.root));
    ed.keep_close.stamps = ed.docs.iter().filter(under).map(|d| (d.id, d.disk)).collect();
    ed.with_session(id, |ed| {
        ed.session.moving = Some(base.clone());
        ed.disk_batch_start(&format!("moved onto {base}"), token);
    });
    let (root, branch) = (w.ws.root.clone(), base);
    ed.events.shared_jobs().spawn(move || {
        let r = move_onto(&root, &branch, &|| {});
        move |ed: &mut Editor| moved(ed, w, token, r)
    });
}

/// The session whose checkout is being moved with `path` in it (`Session.moving`): its id, name and base. What
/// writes into a checkout asks: a save is held (`hold_save`); `space u`, a change set, `:w` to a new name, the
/// watch's write-back are refused or left until it lands (`refused`).
pub fn moving_at(ed: &Editor, path: &Path) -> Option<(SessionId, String, String)> {
    sessions(ed).find_map(|l| {
        let base = l.moving.as_ref().filter(|_| path.starts_with(&l.root))?;
        Some((l.id, l.label().to_string(), base.clone()))
    })
}

/// Why a write into `paths` waits for a move — `fix is moving onto main — {then} once it lands` — None: none
/// of them is in a moving checkout.
pub fn refused<'p>(ed: &Editor, paths: impl IntoIterator<Item = &'p Path>, then: &str) -> Option<String> {
    let (_, name, base) = paths.into_iter().find_map(|p| moving_at(ed, p))?;
    Some(format!("{name} is moving onto {base} — {then} once it lands"))
}

/// A save into a checkout being moved (`Session.moving`) held until the move lands, said — whether it was. Every
/// save asks (`Editor::save_job`): `:w`, auto-save, one Claude caused.
pub fn hold_save(
    ed: &mut Editor,
    id: DocId,
    force: bool,
    then: &crate::saver::Then,
    beneath: &Option<PathBuf>,
) -> bool {
    let Some(d) = ed.docs.iter().find(|d| d.id == id) else { return false };
    let Some((session, name, base)) = d.path.as_deref().and_then(|p| moving_at(ed, p)) else { return false };
    let file = d.display_name_short();
    let saves = &mut ed.keep_close.saves;
    // (asked again meanwhile: once, with `:w!`'s force if either had it)
    let force = force || saves.iter().any(|h| h.id == id && h.force);
    saves.retain(|h| h.id != id);
    saves.push(Held { id, session, force, then: then.clone(), beneath: beneath.clone() });
    ed.set_status(format!("{file} waits — {name} is moving onto {base}; written once it lands"));
    true
}

/// The saves held while session `id`'s checkout moved, written now. First every buffer there still at its file's
/// stamp as the move began (one with edits the disk watch left alone meanwhile), its file one the move left as it
/// was (`same`), takes the file's stamp now — git rewrote it with the same text (its autostash; a stat, as the
/// save's own check makes): a save or `space u` then isn't refused as "changed on disk". Else the save's check, or
/// the watch, says what changed.
fn release(ed: &mut Editor, id: SessionId, open: bool, same: &dyn Fn(&Path) -> bool) {
    let stamps = std::mem::take(&mut ed.keep_close.stamps);
    for d in ed.docs.iter_mut().filter(|d| stamps.get(&d.id).is_some_and(|s| *s == d.disk)) {
        if let Some(p) = d.path.clone().filter(|p| same(p)) {
            d.disk = crate::disk::stamp(&p);
            d.disk_conflict = false;
        }
    }
    let (held, rest): (Vec<Held>, Vec<Held>) =
        std::mem::take(&mut ed.keep_close.saves).into_iter().partition(|h| h.session == id);
    ed.keep_close.saves = rest;
    for h in held {
        let Some(d) = ed.docs.iter().find(|d| d.id == h.id) else { continue };
        let name = d.display_name_short();
        if let Err(e) = ed.save_job(h.id, h.force, h.then, h.beneath) {
            let msg = format!("{name} not saved after the move: {e}");
            match open {
                true => ed.said_for(id, ToastKind::Error, msg),
                false => ed.recorded(ToastKind::Error, msg),
            }
        }
    }
}

/// How a move ended.
#[derive(Debug)]
enum Landed {
    /// Onto `tip` (short), the base's `under` commits now under it; its changes put back — or, `kept`, left in git's
    /// stash (they didn't go back cleanly). `changed`: the files the commits under it changed (None: can't tell).
    Done {
        tip: String,
        under: usize,
        kept: bool,
        changed: Option<Vec<PathBuf>>,
    },
    /// On its tip already.
    There,
    /// It would conflict after all: aborted, as it was.
    Conflicts,
    Failed(String),
}

/// The checkout at `root` rebased onto `base`'s tip as it is now — the newest wins — its changes in no commit
/// taken along (`--autostash`), no hooks (a worker). `between` runs as the rebase starts (a test's stash pushed
/// elsewhere then).
fn move_onto(root: &Path, base: &str, between: &dyn Fn()) -> Landed {
    let tip = match crate::worktree::sha(root, &format!("refs/heads/{base}^{{commit}}")) {
        Ok(t) => t,
        Err(e) => return Landed::Failed(e),
    };
    let under = crate::worktree::count(root, &format!("HEAD..{tip}"));
    if under == 0 {
        return Landed::There;
    }
    if crate::rebase::under_way(root).is_some() {
        return Landed::Failed("a rebase is under way there".into());
    }
    let repo = match crate::gitrepo::repo(root) {
        Ok(r) => r,
        Err(e) => return Landed::Failed(e),
    };
    let old = match crate::worktree::sha(root, "HEAD^{commit}") {
        Ok(h) => h,
        Err(e) => return Landed::Failed(e),
    };
    between();
    let args: Vec<&str> =
        crate::gitrepo::NO_HOOKS.iter().copied().chain(["rebase", "--autostash", "--quiet", &tip]).collect();
    let mut cmd = repo.command(&args);
    // (git's own words, read below: an autostash that didn't go back is said only there — `refs/stash` is every
    // checkout's, a stash pushed in another meanwhile isn't this one's)
    cmd.env("LC_ALL", "C").env_remove("LANGUAGE");
    let r = match crate::gitrepo::ran(&cmd, None) {
        Ok(r) => r,
        Err(e) => return Landed::Failed(e),
    };
    let said = String::from_utf8_lossy(&r.stderr);
    match r.ok {
        true => {
            let kept = autostash_kept(&said);
            let top = repo.work_tree.clone().unwrap_or_else(|| root.to_path_buf());
            let names = ["diff-tree", "-r", "--name-only", "--no-renames", "-z", &old, "HEAD"];
            let changed = crate::gitrepo::git_no_hooks(root, &names)
                .ok()
                .map(|o| o.split('\0').filter(|p| !p.is_empty()).map(|p| top.join(p)).collect());
            Landed::Done { tip: tip.chars().take(7).collect(), under, kept, changed }
        }
        false if crate::rebase::under_way(root).is_some() => {
            // (puts back what it stashed, and the branch where it was)
            match crate::gitrepo::git_no_hooks(root, &["rebase", "--abort"]) {
                Ok(_) => Landed::Conflicts,
                Err(e) => Landed::Failed(format!("it conflicted, and the abort failed: {e}")),
            }
        }
        false => {
            let why = said.lines().find(|l| !l.trim().is_empty()).unwrap_or("git rebase failed");
            Landed::Failed(why.to_string())
        }
    }
}

/// Whether git's words after `rebase --autostash` say its changes stayed in the stash (they didn't go back cleanly):
/// git ≤ 2.54 says "Applying autostash resulted in conflicts.", 2.55 on "Your local changes are stashed, however
/// applying them\nresulted in conflicts." — the stash itself is `autostash` in both.
fn autostash_kept(said: &str) -> bool {
    said.contains("Applying autostash resulted in conflicts")
        || said.contains("Your local changes are stashed, however applying them")
}

/// A move landed: in its session, the reloads said as one line, its buffers' git bases and its instruction files
/// followed, what waited for it let go — said once, named — then the next one.
fn moved(ed: &mut Editor, w: Wait, token: u64, r: Landed) {
    ed.keep_close.moving = None;
    ed.keep_close.moved += 1;
    let (id, base) = (w.ws.id, w.base.1.clone());
    let mut name = w.ws.name.clone();
    let open = sessions(ed).any(|l| l.id == id);
    if open {
        ed.with_session(id, |ed| {
            ed.session.moving = None;
            name = ed.session.label().to_string();
            ed.disk_batch_end(token);
            if matches!(r, Landed::Done { .. }) {
                let root = ed.session.root.clone();
                ed.git_refresh();
                ed.git_reload_bases_in(&root);
                crate::instructions::follow(ed);
            }
            crate::chat::send_queued(ed);
            crate::queued::run_next(ed);
        });
    }
    // (its saves held meanwhile written — a file the move left as it was: as is; aborted, it's as it was)
    let same = |p: &Path| match &r {
        Landed::Done { kept: false, changed: Some(c), .. } => !c.iter().any(|x| x == p),
        Landed::There | Landed::Conflicts => true,
        _ => false,
    };
    release(ed, id, open, &same);
    let by = if w.unasked { " (workspace.sync = \"idle\")" } else { "" };
    let (kind, msg) = match r {
        Landed::Done { tip, under, kept: false, .. } => (
            ToastKind::Success,
            format!(
                "{name} · moved onto {base}{by} — {} under it, now at {tip}",
                crate::worktree::commits(under)
            ),
        ),
        Landed::Done { tip, under, kept: true, .. } => (
            ToastKind::Warning,
            format!(
                "{name} · moved onto {base}{by} — {} under it, now at {tip}; its changes didn't go back cleanly — \
                 they're in git's stash (git stash list)",
                crate::worktree::commits(under)
            ),
        ),
        Landed::There => return next(ed),
        Landed::Conflicts => (
            ToastKind::Warning,
            format!(
                "{name} · would conflict on {base} after all — left as it was; :session-bring {name} rebases it, \
                 its Claude resolving"
            ),
        ),
        Landed::Failed(e) => (ToastKind::Error, format!("{name} · not moved onto {base}: {e}")),
    };
    match open {
        true => ed.said_for(id, kind, msg),
        false => ed.recorded(kind, msg),
    }
    next(ed);
}

#[cfg(test)]
mod tests;
