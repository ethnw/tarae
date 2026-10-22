//! collab C1 — presence: where every session is reading and writing, shown wherever that file is shown, in any
//! checkout of the repository (`target/sb/collab-plan.md` §2). Gathered every event from what's already there — each
//! Claude's gaze (`follow::Gaze`), the edit it's writing (`writing::Live`), the edits its turn wrote (the replay's
//! stops) — never a process or a poll of its own, and nothing while one session is open. A session's presence lasts
//! its turn: the turn over, it goes.
//!
//! A file in another checkout of the same repository at the same path is the same file, its lines maybe not: a
//! presence there is mapped through a line diff of the two texts (`LineMap`), worked out on a worker — one at a time
//! for a file and a buffer, the last one used meanwhile — and whether the two checkouts are of one repository is
//! asked there too (`worktree::common_dir`, git's own answer, kept).
//!
//! Drawn (`term::draw_text`): a `▎` in a code pane's sign column on its lines (under a stopped line, a breakpoint and
//! a diagnostic) and a name tag at its first line's end (`‹main writing›`) — in the session's own participant colour
//! (`ui.participant.N`, the user 2026-10-22), at full strength for the one you follow, dim for the others, the name
//! always beside it (never the colour alone); a click on the tag follows it
//! (`follow::follow_session`). The session list says where each is (`at`), the strip its file, the side panel's files
//! and git tabs `◦ name` on a file another session is in (`in_file`).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::document::{DocId, Document};
use crate::editor::Editor;
use crate::event::SessionId;

/// What a session does at a place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Reading,
    /// Its turn changed these lines (an edit written), or it's looking at an edit of its.
    Editing,
    /// An edit being written there (`writing.rs`).
    Writing,
}

impl Act {
    pub fn word(self) -> &'static str {
        match self {
            Act::Reading => "reading",
            Act::Editing => "editing",
            Act::Writing => "writing",
        }
    }
}

/// One session at one place of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub session: SessionId,
    /// What the tabs and the list call it (`Session::label`), and its participant colour (`Session.colour`).
    pub name: String,
    pub colour: u8,
    /// Its checkout, and the file (a real path in it).
    pub root: PathBuf,
    pub path: PathBuf,
    /// 0-based lines — None: the whole file (a read with no range, a search's file).
    pub lines: Option<Range<usize>>,
    pub act: Act,
    /// Where it is now (its tag goes here) — else lines its turn changed.
    pub here: bool,
    /// Its file's text as of now (what a line map is worked out from).
    stamp: Stamp,
}

/// A file's text as of now: its open buffer at a version, else the disk's — told apart by the edits the turn wrote
/// there (at once) and the file's own stamp as a worker last saw it (`Seen`: a save in another editor, a `git
/// checkout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Stamp {
    Doc(DocId, u64),
    Disk(usize, Option<crate::disk::Stamp>),
}

/// A file of another checkout no buffer holds: its stamp (`disk::stamp`, mtime and size) as a worker last read it,
/// when, and whether a look is on its way.
#[derive(Default)]
struct Seen {
    stamp: Option<crate::disk::Stamp>,
    at: Option<std::time::Instant>,
    asking: bool,
}

/// How often a file in sight that another checkout's Claude is in is looked at again on disk (on events only — an idle
/// tarae isn't woken).
const LOOK: std::time::Duration = std::time::Duration::from_millis(500);

/// The lines of one text as lines of another: each line of the first, the line of the second it is (kept), or the
/// one where it went (changed: the change's first line on the other side).
#[derive(Debug, PartialEq, Eq)]
pub struct LineMap {
    to: Vec<u32>,
    /// Lines of the second text.
    lines: u32,
}

impl LineMap {
    /// `from`'s lines in `to`, by a line diff (git's histogram, as the rest of tarae diffs).
    pub fn of(from: &str, to: &str) -> LineMap {
        use imara_diff::intern::InternedInput;
        use imara_diff::{Algorithm, diff};
        let input = InternedInput::new(from, to);
        let (n, lines) = (input.before.len() as u32, input.after.len() as u32);
        let mut map = Vec::with_capacity(n as usize);
        let (mut a, mut b) = (0u32, 0u32);
        diff(Algorithm::Histogram, &input, |before: Range<u32>, after: Range<u32>| {
            // (the lines kept before it, one for one; then its own, onto its first line there)
            map.extend((a..before.start).map(|l| b + (l - a)));
            let last = after.end.max(after.start + 1) - 1;
            map.extend((before.start..before.end).map(|l| (after.start + (l - before.start)).min(last)));
            (a, b) = (before.end, after.end);
        });
        map.extend((a..n).map(|l| b + (l - a)));
        LineMap { to: map, lines }
    }

    /// Lines `r` of the first text as lines of the second (never empty, within it).
    pub fn range(&self, r: Range<usize>) -> Range<usize> {
        let last = self.lines.saturating_sub(1) as usize;
        let at = |l: usize| self.to.get(l).map_or(last, |&t| t as usize).min(last);
        let start = at(r.start);
        let end = at(r.end.saturating_sub(1).max(r.start)).max(start) + 1;
        start..end
    }
}

/// A file of one checkout and a buffer of another: the line map last worked out, for what (the file's text, the
/// buffer's version), and whether one is being worked out.
struct Kept {
    of: Option<(Stamp, u64)>,
    map: Option<Arc<LineMap>>,
    asking: bool,
}

/// Every session's presence (`Editor.presence`): as the last event left it, and what draws it in other checkouts.
#[derive(Default)]
pub struct State {
    pub now: Vec<Presence>,
    /// By (the file, a buffer of another checkout).
    maps: HashMap<(PathBuf, DocId), Kept>,
    /// Two checkouts (in order) of one repository, as git says.
    same: HashMap<(PathBuf, PathBuf), bool>,
    asking_same: HashSet<(PathBuf, PathBuf)>,
    /// By the file: the disk's stamps of the files the maps are worked out from.
    disk: HashMap<PathBuf, Seen>,
}

#[cfg(test)]
impl State {
    /// The buffers line maps are kept for (each in sight when the last event ended — `ask`).
    pub fn mapped(&self) -> Vec<DocId> {
        self.maps.keys().map(|k| k.1).collect()
    }
}

/// The pair of checkouts as `State.same` keeps it.
fn pair(a: &Path, b: &Path) -> (PathBuf, PathBuf) {
    if a <= b { (a.to_path_buf(), b.to_path_buf()) } else { (b.to_path_buf(), a.to_path_buf()) }
}

/// Every session's place now — none with one session open (there's no one else to show).
fn gather(ed: &Editor) -> Vec<Presence> {
    let mut out = Vec::new();
    if ed.sessions.is_empty() {
        return out;
    }
    for l in std::iter::once(&ed.session).chain(&ed.sessions) {
        let Some(c) = l.chat.as_ref().filter(|c| c.busy()) else { continue };
        let at = |p: &Path| if p.is_absolute() { p.to_path_buf() } else { l.root.join(p) };
        let turn = c.follow.replay.turns.back();
        let written = |path: &Path| {
            turn.map_or(0, |t| {
                t.stops
                    .iter()
                    .filter(|s| !s.hunks.is_empty() && s.path.as_deref().map(at).as_deref() == Some(path))
                    .count()
            })
        };
        let stamp = |path: &Path| match ed.docs.iter().find(|d| d.path.as_deref() == Some(path)) {
            Some(d) => Stamp::Doc(d.id, d.version()),
            None => Stamp::Disk(written(path), ed.presence.disk.get(path).and_then(|s| s.stamp)),
        };
        let mut put = |path: PathBuf, lines: Option<Range<usize>>, act: Act, here: bool| {
            let stamp = stamp(&path);
            out.push(Presence {
                session: l.id,
                name: l.label().to_string(),
                colour: l.colour,
                root: l.root.clone(),
                path,
                lines,
                act,
                here,
                stamp,
            });
        };
        // The edits its turn wrote: the lines each left
        for s in turn.into_iter().flat_map(|t| &t.stops) {
            let Some(path) = s.path.as_deref().map(at) else { continue };
            for h in &s.hunks {
                let lines = h.lines.start..h.lines.end.max(h.lines.start + 1);
                put(path.clone(), Some(lines), Act::Editing, false);
            }
        }
        // Where it is: the edit it's writing (its items each, the last one where it is), else where it looks
        let writing: Vec<(PathBuf, Range<usize>)> = c
            .writing
            .calls
            .iter()
            .flat_map(|call| &call.items)
            .filter_map(|it| match &it.place {
                crate::writing::Place::Found(f) => Some((f.path.clone(), f.lines.0..f.lines.1 + 1)),
                _ => None,
            })
            .collect();
        let n = writing.len();
        for (k, (path, lines)) in writing.into_iter().enumerate() {
            put(path, Some(lines), Act::Writing, k + 1 == n);
        }
        if n == 0
            && let Some(g) = c.follow.gaze.as_ref()
            && let Some(path) = g.path.as_deref().map(at)
        {
            let act = if g.edit.is_empty() { Act::Reading } else { Act::Editing };
            put(path, g.lines.clone(), act, true);
        }
    }
    out
}

/// Whether `p`'s session is still there with its turn under way — what was gathered at the last event, read by a frame
/// that may come before the next (a session closed, a turn ended between): never drawn then.
fn live(ed: &Editor, p: &Presence) -> bool {
    std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .any(|l| l.id == p.session && l.chat.as_ref().is_some_and(|c| c.busy()))
}

/// The session you follow: the Claude of the workspace in front while its follow is on (following another takes you
/// into its workspace — `follow::follow_session`).
pub fn followed(ed: &Editor) -> Option<SessionId> {
    let c = ed.session.chat.as_ref()?;
    (c.follow.active() && !ed.background()).then_some(ed.session.id)
}

/// Every event (`handle_event`): the presence as things are now, and the line maps the buffers in sight need asked for
/// — nothing when nothing moved (an idle tarae isn't woken).
pub fn sync(ed: &mut Editor) {
    let now = gather(ed);
    if now != ed.presence.now {
        ed.presence.now = now;
    }
    ask(ed);
}

/// The documents of the panes in front, by id.
fn in_sight(ed: &Editor) -> Vec<&Document> {
    let mut ids: Vec<DocId> = ed.session.views.iter().map(|v| v.doc).collect();
    ids.push(ed.doc().id);
    crate::sorting::sort(&mut ids);
    ids.dedup();
    ed.docs.iter().filter(|d| ids.contains(&d.id)).collect()
}

/// A presence in another checkout at a document's path there (relative to each checkout).
fn same_place(p: &Presence, root: &Path, path: &Path) -> bool {
    p.root != root
        && p.path.strip_prefix(&p.root).ok().is_some_and(|rel| path.strip_prefix(root).ok() == Some(rel))
}

/// A line map in sight: (the file, a buffer here), what it's of (the file's text, the buffer's version), the file's
/// checkout, the buffer's text.
type Wanted = ((PathBuf, DocId), (Stamp, u64), PathBuf, ropey::Rope);

/// Line maps for what's in sight: a presence in another checkout at a file shown here, worked out on a worker when
/// its text or the buffer moved (one at a time each: the last one draws meanwhile); the ones nothing needs dropped.
fn ask(ed: &mut Editor) {
    let root = ed.session.root.clone();
    let mut wanted: Vec<Wanted> = Vec::new();
    for d in in_sight(ed) {
        let Some(path) = d.path.as_deref() else { continue };
        for p in ed.presence.now.iter().filter(|p| same_place(p, &root, path)) {
            let key = (p.path.clone(), d.id);
            if !wanted.iter().any(|(k, ..)| *k == key) {
                wanted.push((key, (p.stamp, d.version()), p.root.clone(), d.text.clone()));
            }
        }
    }
    // The checkouts beside this one: whether each is of this repository (the side panel's `◦` needs it too)
    let roots: Vec<PathBuf> = ed
        .presence
        .now
        .iter()
        .filter(|p| p.root != root)
        .map(|p| p.root.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for r in roots {
        let k = pair(&root, &r);
        if ed.presence.same.contains_key(&k) || !ed.presence.asking_same.insert(k.clone()) {
            continue;
        }
        let (a, b) = (root.clone(), r);
        ed.events.shared_jobs().spawn(move || {
            let same = same_repository(&a, &b);
            move |ed: &mut Editor| {
                ed.presence.asking_same.remove(&k);
                ed.presence.same.insert(k, same);
            }
        });
    }
    let state = &mut ed.presence;
    state.maps.retain(|k, _| wanted.iter().any(|(w, ..)| w == k));
    state
        .disk
        .retain(|p, _| wanted.iter().any(|((w, _), (s, _), ..)| w == p && matches!(s, Stamp::Disk(..))));
    for (key, want, other, to) in wanted {
        let kept = state.maps.entry(key.clone()).or_insert(Kept { of: None, map: None, asking: false });
        if kept.asking || kept.of == Some(want) {
            // (a file on disk: looked at again now and then — a change from elsewhere moves its stamp)
            if !kept.asking && matches!(want.0, Stamp::Disk(..)) {
                let seen = state.disk.entry(key.0.clone()).or_default();
                if !seen.asking && seen.at.is_none_or(|t| crate::clock::since(t) >= LOOK) {
                    seen.asking = true;
                    let path = key.0.clone();
                    ed.events.shared_jobs().spawn(move || {
                        let stamp = crate::disk::stamp(&path);
                        move |ed: &mut Editor| {
                            let Some(seen) = ed.presence.disk.get_mut(&path) else {
                                ed.unchanged = true;
                                return;
                            };
                            (seen.asking, seen.at) = (false, Some(crate::clock::now()));
                            // (the same: nothing to work out again, nothing to draw)
                            if seen.stamp == stamp {
                                ed.unchanged = true;
                            }
                            seen.stamp = stamp;
                        }
                    });
                }
            }
            continue;
        }
        kept.asking = true;
        let from = match want.0 {
            Stamp::Doc(id, _) => ed.docs.iter().find(|d| d.id == id).map(|d| d.text.clone()),
            Stamp::Disk(..) => None,
        };
        let same = state.same.get(&pair(&root, &other)).copied();
        let (here, path) = (root.clone(), key.0.clone());
        ed.events.shared_jobs().spawn(move || {
            let same = same.unwrap_or_else(|| same_repository(&here, &other));
            // (the disk's stamp taken before its text: a write between is seen at the next look)
            let mut stamp = None;
            let from = match from {
                Some(t) => Some(t.to_string()),
                None if same => {
                    stamp = crate::disk::stamp(&path);
                    std::fs::read_to_string(&path).ok()
                }
                None => None,
            };
            let map = from.filter(|_| same).map(|f| Arc::new(LineMap::of(&f, &to.to_string())));
            move |ed: &mut Editor| {
                ed.presence.same.insert(pair(&here, &other), same);
                // (what it was worked out from: the stamp as read, which the next gather takes up)
                let want = match want.0 {
                    Stamp::Disk(n, _) if same => {
                        let seen = ed.presence.disk.entry(path).or_default();
                        (seen.stamp, seen.at) = (stamp, Some(crate::clock::now()));
                        (Stamp::Disk(n, stamp), want.1)
                    }
                    _ => want,
                };
                let Some(kept) = ed.presence.maps.get_mut(&key) else {
                    ed.unchanged = true;
                    return;
                };
                kept.asking = false;
                kept.of = Some(want);
                // (nothing new to draw: the frame stays as it is)
                if kept.map.as_deref() == map.as_deref() {
                    ed.unchanged = true;
                }
                kept.map = map;
            }
        });
    }
}

/// Whether two checkouts are of one repository: git's common folder the same.
fn same_repository(a: &Path, b: &Path) -> bool {
    match (crate::worktree::common_dir(a), crate::worktree::common_dir(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// A presence as a code pane draws it: its lines in the document shown, whose, doing what, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub lines: Range<usize>,
    pub session: SessionId,
    pub name: String,
    pub act: Act,
    /// Its participant colour — at full strength for the session you follow (`followed`), the others dim.
    pub colour: u8,
    pub followed: bool,
    /// Where it is now: its name tag at the first line's end.
    pub here: bool,
}

impl Mark {
    /// Its name tag, as wide as `room` allows: `‹main writing›`, else `‹main›`, else none.
    pub fn tag(&self, room: usize) -> Option<String> {
        use unicode_width::UnicodeWidthStr;
        [format!("‹{} {}›", self.name, self.act.word()), format!("‹{}›", self.name)]
            .into_iter()
            .find(|t| t.width() <= room)
    }
}

/// The presences drawn in `doc` — its own checkout's at their lines, another checkout's of the repository through its
/// line map (none until that's worked out). None with one session open.
pub fn marks(ed: &Editor, doc: &Document) -> Vec<Mark> {
    if ed.sessions.is_empty() || ed.presence.now.is_empty() {
        return Vec::new();
    }
    let Some(path) = doc.path.as_deref() else { return Vec::new() };
    let last = crate::movement::last_line(&doc.text);
    let followed = followed(ed);
    let mut out: Vec<Mark> = ed
        .presence
        .now
        .iter()
        .filter(|p| live(ed, p))
        .filter_map(|p| {
            let lines = p.lines.clone()?;
            let lines = match p.path == path {
                true => lines,
                false => ed.presence.maps.get(&(p.path.clone(), doc.id))?.map.as_ref()?.range(lines),
            };
            let start = lines.start.min(last);
            Some(Mark {
                lines: start..lines.end.min(last + 1).max(start + 1),
                session: p.session,
                name: p.name.clone(),
                act: p.act,
                colour: p.colour,
                followed: followed == Some(p.session),
                here: p.here,
            })
        })
        .collect();
    // (the one you follow first: on a line two share, its mark is the one drawn)
    crate::sorting::by_key(&mut out, |m| (!m.followed, !m.here));
    out
}

/// Whose caret and place `doc` shows (`term::draw_text`): the participant colour of the session whose checkout holds
/// it (the front's for one with no path, or in none), and whether you follow that session.
pub fn colour_of(ed: &Editor, doc: &Document) -> (u8, bool) {
    let sessions = || std::iter::once(&ed.session).chain(&ed.sessions);
    let owner = doc
        .path
        .as_deref()
        .and_then(|p| sessions().filter(|l| p.starts_with(&l.root)).max_by_key(|l| l.root.as_os_str().len()))
        .unwrap_or(&ed.session);
    (owner.colour, followed(ed) == Some(owner.id))
}

/// Where session `id` is now, as the list says it: `src/term.rs:3614 · writing` (its path from its checkout).
pub fn at(ed: &Editor, id: SessionId) -> Option<String> {
    let p = ed.presence.now.iter().find(|p| p.session == id && p.here && live(ed, p))?;
    let rel = p.path.strip_prefix(&p.root).unwrap_or(&p.path).display().to_string();
    Some(match &p.lines {
        Some(l) => format!("{rel}:{} · {}", l.start + 1, p.act.word()),
        None => format!("{rel} · {}", p.act.word()),
    })
}

/// The file session `id` is in now, by its name (`term.rs` — the strip).
pub fn file_of(ed: &Editor, id: SessionId) -> Option<String> {
    let p = ed.presence.now.iter().find(|p| p.session == id && p.here && live(ed, p))?;
    Some(p.path.file_name()?.to_string_lossy().into_owned())
}

/// The other sessions in file `rel` (from the front checkout's top) — its own checkout's or another of the repository's,
/// each named once, where it is first.
pub fn in_file(ed: &Editor, rel: &Path) -> Vec<String> {
    let root = &ed.session.root;
    let mut names: Vec<String> = Vec::new();
    let mut ps: Vec<&Presence> =
        ed.presence.now.iter().filter(|p| p.session != ed.session.id && live(ed, p)).collect();
    crate::sorting::by_key(&mut ps, |p| !p.here);
    for p in ps {
        let same = p.root == *root || ed.presence.same.get(&pair(root, &p.root)).copied().unwrap_or(false);
        if same && p.path.strip_prefix(&p.root).ok() == Some(rel) && !names.contains(&p.name) {
            names.push(p.name.clone());
        }
    }
    names
}

/// `◦ main` · `◦ main, fix` — a file row's hint for the sessions in it (`in_file`), before what the row says.
pub fn hint(names: &[String], rest: &str) -> String {
    if names.is_empty() {
        return rest.to_string();
    }
    let who = format!("◦ {}", names.join(", "));
    if rest.is_empty() { who } else { format!("{who} · {rest}") }
}

#[cfg(test)]
mod tests;
