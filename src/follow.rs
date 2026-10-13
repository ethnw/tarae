//! Follow mode — while Claude explores in the chat, the editor goes where it is reading and a thought card
//! beside the code says what it is thinking (`C-f` in the chat · `llm.follow`).
//!
//! - Read → that file, its range placed as a whole (`Editor::show_range`), the lines read on a faint accent
//!   wash. A search or a lookup → its first hit once its answer is in, the count on the card (`12 hits in
//!   4 files`). A note → its lines (its card opens there). An edit → its changed lines once it's written
//!   (review at the end too): an open buffer takes the file then, the lines on the review's wash and the
//!   ones they replaced above them; the card names it (`● edit src/trust.rs L30–34`).
//! - Every call is a stop of the turn's replay (`replay.rs`): `]r`/`[r` step through them at any time.
//! - While it thinks and writes, the code it's talking about: every `path:line` / `path:start-end` as soon as
//!   it's written, and a `name` in backticks → that definition in a file open here (outlines worked out on
//!   a worker). The answer's words move the editor at most every PACE (they stream faster than anyone reads).
//! - A walk-through: Claude writes `<show at="path:start-end">why</show>` before each piece it explains
//!   (`chat::show_tags`; the chat shows them as link lines). They're queued and played one by one, each for
//!   its explanation's reading time — on after the answer is done; what's merely mentioned then doesn't move.
//! - The turn ends on the answer's first `path:line` (unless a walk-through plays); the stops go into the
//!   list (if it's empty or holds the last walk-through — `]q`). Then scrolling the chat stops the tour and
//!   moves the editor to the first place the chat shows (`reading`, after each frame).
//! - Files follow opened are previews: unedited ones close when it moves on (they stay once the turn ends).
//!   Only files in the project (`in_project`): what Claude reads elsewhere (its memory, a temp file) shows on
//!   the card but is never opened, nor put in the list.
//! - The user wins: moving the cursor or switching files in the editor pauses following — a key, a click
//!   or a paste did it (a reload moving the cursor isn't you; switching sessions isn't either). It follows
//!   again after `llm.follow-resume` seconds without one while Claude still works, on `C-f`, or with the
//!   next question. `C-o` goes back to where you were before the turn (one jump point per turn).
//! - The card (drawn by `term::draw_follow_card`, one card in one place — the bottom right of the code)
//!   shows what Claude is doing and its latest words — the thinking summary (`--thinking-display
//!   summarized`) or what it says between lookups; while the panel is minimized, the question and keys too.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::chat::{Look, Role, refs_in};
use crate::document::DocId;
use crate::editor::Editor;
use crate::selection::Selection;

/// Where Claude is looking right now.
#[derive(Clone, Debug)]
pub struct Gaze {
    pub look: Look,
    /// The file, when the lookup has one.
    pub path: Option<PathBuf>,
    /// 0-based lines read (washed) — none for whole-file reads and searches.
    pub lines: Option<Range<usize>>,
    /// Why it's showing this (a walk-through's stop) · which stop of how many.
    pub why: Option<String>,
    pub step: Option<(usize, usize)>,
    /// An edit's changes, drawn while the buffer is still the version they were seen in (`shown_in`).
    pub edit: Vec<crate::replay::Hunk>,
    pub shown_in: Option<(DocId, u64)>,
    /// The tool call it's for ("" for words and walk-throughs) — its answer may move it (a search's hit).
    pub call: String,
}

impl Gaze {
    pub fn new(look: Look, path: Option<PathBuf>, lines: Option<Range<usize>>) -> Gaze {
        Gaze {
            look,
            path,
            lines,
            why: None,
            step: None,
            edit: Vec::new(),
            shown_in: None,
            call: String::new(),
        }
    }
}

/// A place a walk-through stopped at — kept for the list at the end.
#[derive(Clone, Debug, PartialEq)]
struct Stop {
    path: PathBuf,
    lines: Range<usize>,
    why: String,
    /// Claude showed it (`<show>`), rather than mentioning it.
    shown: bool,
}

#[derive(Default)]
pub struct Follow {
    pub on: bool,
    /// The user took over this turn.
    pub paused: bool,
    pub gaze: Option<Gaze>,
    /// Tool call id → name and input (to read its answer when it comes back).
    calls: Vec<(String, String, serde_json::Value)>,
    /// This turn's and the last few turns' stops (`]r`/`[r`) · the one showing while you step through them.
    pub replay: crate::replay::Replay,
    pub replaying: Option<Gaze>,
    /// Where replay put the cursor — you moving away ends its showing.
    replay_placed: Option<(DocId, usize, usize)>,
    /// The texts files had before an edit allowed this turn (real path; None = no such file) — its stop's
    /// changes are worked out against them once it's written.
    befores: Vec<(PathBuf, Option<String>)>,
    /// The editor's input count `watch` saw last (a cursor moved with no input since isn't the user).
    seen_input: u64,
    /// Paused: when the user last pressed something · a wake-up to follow again is on its way.
    last_input: Option<std::time::Instant>,
    resume_armed: bool,
    /// A jump point was left this turn.
    jumped: bool,
    /// Documents follow opened this turn (closed when it moves on, unless edited).
    previews: Vec<DocId>,
    /// Where follow last put the cursor — anything else means the user moved.
    placed: Option<(DocId, usize, usize)>,
    /// A big file follow opened is still loading — the cursor lands when its text is there.
    awaiting: Option<DocId>,
    /// How far the words of this turn were read for places: (message index, byte).
    scanned: (usize, usize),
    /// Outlines of open documents (for `name`s) — per version · being worked out · the latest name that
    /// waits for one.
    defs: Vec<(DocId, u64, Vec<crate::syntax::Def>)>,
    outlining: Vec<(DocId, u64)>,
    wanted: Option<(String, Option<String>, String, bool)>,
    stops: Vec<Stop>,
    /// When follow last moved the editor for words (an answer streams faster than anyone reads — paced).
    moved: Option<std::time::Instant>,
    /// The chat's scroll the editor last followed while you read the answer.
    pub read_at: Option<usize>,
    /// The walk-through's stops still to show, and how many tags of which message were taken · the one
    /// showing, until when (its wake-up is this generation's).
    queue: std::collections::VecDeque<(usize, usize, Stop, usize)>,
    tags_seen: (usize, usize),
    /// Stops the walk-through has so far (each queued one knows its number).
    tour_len: usize,
    playing: Option<(std::time::Instant, u64)>,
    tour_gen: u64,
    /// The tour's list waits to say itself until the tour is over.
    announce: bool,
}

/// While the answer streams, the editor moves at most this often (it records every place — `]q`).
const PACE: std::time::Duration = std::time::Duration::from_millis(1500);

impl Follow {
    /// A new question: follow again, forget this turn's previews (they're the user's now).
    pub fn new_turn(&mut self) {
        self.paused = false;
        self.gaze = None;
        self.calls.clear();
        self.jumped = false;
        self.previews.clear();
        self.placed = None;
        self.awaiting = None;
        self.scanned = (0, 0);
        self.wanted = None;
        self.stops.clear();
        self.moved = None;
        self.read_at = None;
        self.queue.clear();
        self.tags_seen = (0, 0);
        self.tour_len = 0;
        self.playing = None;
        self.tour_gen += 1;
        self.announce = false;
        self.replay.begin();
        self.replay.at = None;
        self.replaying = None;
        self.befores.clear();
        self.last_input = None;
    }

    /// What the editor shows of Claude's: the replay's stop while you step through them, else where it is.
    pub fn shown(&self) -> Option<&Gaze> {
        self.replaying.as_ref().or(self.gaze.as_ref())
    }

    /// A walk-through is showing its stops.
    pub fn touring(&self) -> bool {
        self.playing.is_some() || !self.queue.is_empty()
    }

    pub fn active(&self) -> bool {
        self.on && !self.paused
    }

    /// `id` is the user's now (an edit was reviewed there) — follow won't close it as a preview.
    pub fn keep(&mut self, id: DocId) {
        self.previews.retain(|&p| p != id);
    }
}

fn follow(ed: &mut Editor) -> Option<&mut Follow> {
    ed.session.chat.as_mut().map(|c| &mut c.follow)
}

/// Where the cursor is (document, selection) — compared with `placed`.
fn cursor_key(ed: &Editor) -> (DocId, usize, usize) {
    let r = ed.doc().selection().primary();
    (ed.doc().id, r.anchor, r.head)
}

/// A tool call started: a stop of the turn's replay, and the editor there when it has a place now (an edit
/// once it's written, a search at its first hit once its answer is in).
pub fn on_tool(ed: &mut Editor, id: &str, name: &str, input: &serde_json::Value, look: &Look) {
    // (its words right before this call — the replay's card says them at this stop)
    let said = ed.session.chat.as_ref().map(crate::edits::reason).unwrap_or_default();
    let Some(f) = follow(ed) else { return };
    f.calls.push((id.to_string(), name.to_string(), input.clone()));
    let (path, lines, line) = match name {
        "Read" => {
            let n = |k: &str| input[k].as_u64().map(|v| v as usize);
            let from = n("offset").unwrap_or(1).max(1) - 1;
            let lines = n("offset").or(n("limit")).map(|_| from..from + n("limit").unwrap_or(2000));
            (look.link.as_ref().map(|l| l.path.clone()), lines, from)
        }
        // A test run: to the test (the file's top for all of them); references: to the name asked about
        "mcp__tarae__run_tests"
        | "mcp__tarae__references"
        | "mcp__tarae__symbols"
        | "mcp__tarae__definition"
        | "mcp__tarae__rename"
        | "mcp__tarae__hover" => match &look.link {
            Some(l) => (Some(l.path.clone()), input["line"].is_u64().then(|| l.line..l.line + 1), l.line),
            None => (None, None, 0),
        },
        // A note (or a reply on one): to its lines — the card shows there
        "mcp__tarae__note" => match &look.link {
            Some(l) => {
                let end = input["end_line"].as_u64().map_or(l.line + 1, |e| (e as usize).max(l.line + 1));
                (Some(l.path.clone()), Some(l.line..end), l.line)
            }
            None => (None, None, 0),
        },
        _ => (None, None, 0),
    };
    if !matches!(name, "mcp__tarae__plan" | "mcp__tarae__screen" | "mcp__tarae__procs" | "mcp__tarae__ui") {
        let in_file = matches!(name, "Edit" | "Write").then(|| look.link.as_ref().map(|l| l.path.clone()));
        f.replay.add(crate::replay::Stop {
            call: id.to_string(),
            look: look.clone(),
            path: path.clone().or(in_file.flatten()),
            lines: lines.clone().or_else(|| path.as_ref().map(|_| line..line + 1)),
            hunks: Vec::new(),
            why: said,
        });
    }
    // (a `name` still being looked for is older than this)
    f.wanted = None;
    f.gaze = Some(Gaze { call: id.to_string(), ..Gaze::new(look.clone(), path.clone(), lines.clone()) });
    if let Some(p) = path {
        go_range(ed, &p, lines.unwrap_or(line..line + 1));
    }
}

/// An edit to `real` (its real path) was allowed: what the file is now (None: no such file) — its stop's
/// changes are worked out against it once the edit is written (`on_result`).
pub fn before_edit(ed: &mut Editor, real: PathBuf, before: Option<String>) {
    if let Some(f) = follow(ed) {
        f.befores.push((real, before));
    }
}

/// The walk-through's stop the cursor is on (its file, lines and why) — `space n` there asks about it.
pub fn stop_here(ed: &Editor) -> Option<(PathBuf, Range<usize>, String)> {
    let g = ed.session.chat.as_ref()?.follow.gaze.as_ref()?;
    let (path, lines, why) = (g.path.as_ref()?, g.lines.clone()?, g.why.clone()?);
    g.step?;
    let doc = ed.doc();
    let line = doc.text.byte_to_line(doc.selection().primary().cursor(&doc.text));
    (doc.path.as_ref() == Some(path) && lines.contains(&line)).then(|| (path.clone(), lines, why))
}

/// The user is asking about what's showing: the walk-through holds still (the stop stays on the card).
pub fn hold(ed: &mut Editor) {
    if let Some(f) = follow(ed)
        && f.active()
        && f.touring()
    {
        f.paused = true;
        f.placed = None;
    }
}

/// Something the editor brings to Claude's attention and the user's alike (a test Claude ran failed there):
/// `lines` of `path` washed, the card saying `verb what` and `why`.
pub fn point(
    ed: &mut Editor,
    verb: &'static str,
    what: String,
    path: &Path,
    lines: Range<usize>,
    why: Option<String>,
) {
    let Some(f) = follow(ed) else { return };
    let link = Some(crate::chat::Link { path: path.to_path_buf(), line: lines.start });
    let look = Look { verb, what, pattern: false, range: String::new(), link };
    f.wanted = None;
    f.replay.add(crate::replay::Stop {
        call: String::new(),
        look: look.clone(),
        path: Some(path.to_path_buf()),
        lines: Some(lines.clone()),
        hunks: Vec::new(),
        why: why.clone().unwrap_or_default(),
    });
    f.gaze = Some(Gaze { why, ..Gaze::new(look, Some(path.to_path_buf()), Some(lines.clone())) });
    go_range(ed, path, lines);
}

/// What Claude said that points at code: a place (`path:line`, `path:start-end`) or a `name`.
#[derive(Debug, PartialEq)]
enum Mention {
    Place(PathBuf, Range<usize>),
    Name(String, Option<String>),
}

/// Mentions in `text` (with where they end), in order — relative places from `root`. `in_code`: the text
/// starts inside a `code span`.
fn mentions(root: &Path, text: &str, in_code: bool) -> Vec<(usize, Mention)> {
    let mut out: Vec<(usize, Mention)> = refs_in(root, text)
        .into_iter()
        .map(|(r, link)| {
            // `path:12-30` / `path:12–30`: to 30
            let rest = &text[r.end..];
            let dash = rest.strip_prefix('-').or_else(|| rest.strip_prefix('–'));
            let end = dash
                .map(|d| d.bytes().take_while(u8::is_ascii_digit).count())
                .filter(|&n| n > 0)
                .and_then(|n| dash?[..n].parse::<usize>().ok())
                .filter(|&e| e > link.line);
            (r.end, Mention::Place(link.path, link.line..end.unwrap_or(link.line + 1)))
        })
        .collect();
    // `name`, `Type::name`, `name()`, `obj.name` — a single identifier path, not code with spaces
    let mut pos = 0;
    for (k, piece) in text.split('`').enumerate() {
        let start = pos;
        pos += piece.len() + 1;
        if (k % 2 == 1) != !in_code || piece.len() > 60 {
            continue;
        }
        let ident = piece.trim_end_matches("()");
        let parts: Vec<&str> = ident.split([':', '.']).filter(|p| !p.is_empty()).collect();
        let word = |w: &str| {
            w.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && w.chars().all(|c| c.is_alphanumeric() || c == '_')
        };
        if parts.is_empty() || !parts.iter().all(|p| word(p)) || piece.contains(char::is_whitespace) {
            continue;
        }
        let name = parts[parts.len() - 1].to_string();
        let parent = (parts.len() > 1).then(|| parts[parts.len() - 2].to_string());
        out.push((start + piece.len(), Mention::Name(name, parent)));
    }
    crate::sorting::by_key(&mut out, |m| m.0);
    out
}

/// Every chat event while it thinks or writes: the newest place or `name` in its words since the last
/// look — the editor goes there. Words are read up to their last blank (a half-written one waits) — all of
/// them once the turn is over (`all`).
pub fn on_words(ed: &mut Editor, all: bool) {
    let Some(c) = ed.session.chat.as_ref() else { return };
    if !c.follow.active() {
        return;
    }
    let Some(i) = c.msgs.iter().rposition(|m| matches!(m.role, Role::Thought | Role::Assistant)) else {
        return;
    };
    let text = &c.msgs[i].text;
    let from = if c.follow.scanned.0 == i { c.follow.scanned.1.min(text.len()) } else { 0 };
    let Some(cut) =
        (if all { Some(text.len()) } else { text[from..].rfind(char::is_whitespace).map(|k| from + k) })
    else {
        return;
    };
    let in_code = text[..from].matches('`').count() % 2 == 1;
    // The walk-through's stops it wrote (`<show at="…">why</show>`) — queued, shown one by one
    let tags = crate::chat::show_tags(text);
    let seen = if c.follow.tags_seen.0 == i { c.follow.tags_seen.1 } else { 0 };
    let new: Vec<(usize, Stop)> = tags
        .iter()
        .enumerate()
        .skip(seen)
        .filter_map(|(k, (_, at, why))| {
            let (_, m) = mentions(&ed.session.root, at, false)
                .into_iter()
                .find(|m| matches!(m.1, Mention::Place(..)))?;
            let Mention::Place(path, lines) = m else { return None };
            let ours = path.is_file() && in_project(ed, &path);
            ours.then(|| (k, Stop { path, lines, why: why.clone(), shown: true }))
        })
        .collect();
    let taken = tags.len();
    // Places inside a tag are the tag's
    let found: Vec<(usize, Mention)> = mentions(&ed.session.root, &text[from..cut], in_code)
        .into_iter()
        .filter(|(at, _)| !tags.iter().any(|t| t.0.contains(&(from + at - 1))))
        .collect();
    // The line it's written on — what the stop says in the list
    let said = |at: usize| {
        let a = text[..from + at].rfind('\n').map_or(0, |k| k + 1);
        let b = text[from + at..].find('\n').map_or(text.len(), |k| from + at + k);
        text[a..b].trim().trim_start_matches(['-', '*', ' ']).to_string()
    };
    let found: Vec<(Mention, String)> = found.into_iter().map(|(at, m)| (m, said(at))).collect();
    // Its thinking comes at its own pace; an answer's words come faster than anyone reads them
    let paced = c.msgs[i].role == Role::Assistant;
    if let Some(f) = follow(ed) {
        f.scanned = (i, cut);
        f.tags_seen = (i, taken);
        for (k, stop) in new {
            f.stops.push(stop.clone());
            f.tour_len += 1;
            f.queue.push_back((i, k, stop, f.tour_len));
        }
    }
    play(ed);
    // All of them in order (a batch of words can hold several) — each a stop
    for (m, why) in found {
        match m {
            Mention::Place(path, lines) => look_at(ed, &path, lines, why, paced),
            Mention::Name(name, parent) => {
                if let Some(f) = follow(ed) {
                    f.wanted = Some((name, parent, why, paced));
                }
                find_wanted(ed);
            }
        }
    }
}

/// `lines` of `path` as a stop of the walk-through — the editor there, unless `paced` and it moved less
/// than PACE ago.
fn look_at(ed: &mut Editor, path: &Path, lines: Range<usize>, why: String, paced: bool) {
    // (nor does a place outside the project go into the list)
    if !path.is_file() || !in_project(ed, path) {
        return;
    }
    let what = crate::chat::shown(&ed.session.root, path);
    let Some(f) = follow(ed) else { return };
    f.wanted = None;
    let stop = Stop { path: path.to_path_buf(), lines: lines.clone(), why, shown: false };
    if f.stops.last().is_some_and(|s| s.path == stop.path && s.lines == stop.lines) {
        return;
    }
    f.stops.push(stop);
    // A walk-through leads — what it merely mentions doesn't pull the editor away
    if f.stops.iter().any(|s| s.shown) {
        return;
    }
    let range = if lines.len() > 1 {
        format!("L{}–{}", lines.start + 1, lines.end)
    } else {
        format!("L{}", lines.start + 1)
    };
    let look = Look {
        verb: "look",
        what,
        pattern: false,
        range,
        link: Some(crate::chat::Link { path: path.to_path_buf(), line: lines.start }),
    };
    if paced && f.moved.is_some_and(|t| t.elapsed() < PACE) {
        return;
    }
    f.moved = Some(std::time::Instant::now());
    f.gaze = Some(Gaze::new(look, Some(path.to_path_buf()), Some(lines.clone())));
    go_range(ed, path, lines);
}

/// The walk-through's next stop, once the one showing has had its time (the explanation after it, at
/// reading speed: 2.5–8 s) — then it wakes itself for the one after. Not while the user has taken over.
fn play(ed: &mut Editor) {
    let root = ed.session.root.clone();
    let background = ed.background();
    let Some(f) = follow(ed) else { return };
    if !f.active() || f.playing.is_some_and(|(until, _)| std::time::Instant::now() < until) {
        return;
    }
    // A session behind: the tour waits where it is, and goes on when the session is in front (`session_in`)
    if background {
        f.playing = None;
        return;
    }
    let Some((msg, k, stop, step)) = f.queue.pop_front() else {
        f.playing = None;
        // The tour is over: now the list it left says how to walk it again
        if std::mem::take(&mut f.announce) && ed.session.qlist.as_ref().is_some_and(|l| l.title == TOUR) {
            ed.qlist_announce();
        }
        return;
    };
    let n = f.tour_len;
    let lines = stop.lines.clone();
    let range = if lines.len() > 1 {
        format!("L{}–{}", lines.start + 1, lines.end)
    } else {
        format!("L{}", lines.start + 1)
    };
    let look = Look {
        verb: "show",
        what: crate::chat::shown(&root, &stop.path),
        pattern: false,
        range,
        link: Some(crate::chat::Link { path: stop.path.clone(), line: lines.start }),
    };
    f.gaze = Some(Gaze {
        why: Some(stop.why.clone()),
        step: Some((step, n)),
        ..Gaze::new(look, Some(stop.path.clone()), Some(lines.clone()))
    });
    f.tour_gen += 1;
    let generation = f.tour_gen;
    // How long its explanation is — the words from its tag to the next (or the end, so far)
    let said = ed.session.chat.as_ref().and_then(|c| c.msgs.get(msg)).map_or(0, |m| {
        let tags = crate::chat::show_tags(&m.text);
        let from = tags.get(k).map_or(m.text.len(), |t| t.0.end);
        let to = tags.get(k + 1).map_or(m.text.len(), |t| t.0.start);
        m.text.get(from..to.max(from)).map_or(0, |t| t.chars().count())
    });
    let dwell = if cfg!(test) {
        std::time::Duration::from_millis(30)
    } else {
        std::time::Duration::from_secs_f32((said as f32 / 25.0).clamp(2.5, 8.0))
    };
    if let Some(f) = follow(ed) {
        f.playing = Some((std::time::Instant::now() + dwell, generation));
    }
    // The list (a finished turn's) marks where the tour is
    if let Some(l) = ed.session.qlist.as_mut().filter(|l| l.title == TOUR) {
        l.current = l.items.iter().position(|it| it.path == stop.path && it.line == lines.start);
    }
    go_range(ed, &stop.path, lines);
    ed.events.jobs().spawn(move || {
        std::thread::sleep(dwell);
        move |ed: &mut Editor| {
            let Some(f) = follow(ed) else { return };
            if f.playing.is_some_and(|p| p.1 == generation) {
                f.playing = None;
                play(ed);
            }
        }
    });
}

/// The session came to the front: where its editor is counts as follow's (switching sessions isn't the user
/// taking over — it once stayed paused for nine minutes after a tab click), it catches up with where
/// Claude is now, a walk-through that waited behind goes on, and one paused takes over again a quiet while
/// after you came back (never while behind).
pub fn session_in(ed: &mut Editor) {
    let now = cursor_key(ed);
    let input = ed.input_seq;
    let Some(f) = follow(ed) else { return };
    f.seen_input = input;
    if f.placed.is_some() {
        f.placed = Some(now);
    }
    let target = f
        .active()
        .then(|| f.gaze.as_ref().and_then(|g| Some((g.path.clone()?, g.lines.clone().unwrap_or(0..1)))))
        .flatten();
    if let Some((path, lines)) = target {
        go_range(ed, &path, lines);
    }
    play(ed);
    // Paused before it went behind: follow takes over again a quiet while from now (coming back is
    // looking — not yanked away at once)
    if let Some(f) = follow(ed).filter(|f| f.paused) {
        f.last_input = Some(std::time::Instant::now());
        arm_resume(ed);
    }
}

/// The `name` Claude last mentioned, in the outline of a file open here (the one shown first) — worked out
/// on a worker when it isn't yet, and looked for again when it lands.
fn find_wanted(ed: &mut Editor) {
    let Some((name, parent, why, paced)) = ed.session.chat.as_ref().and_then(|c| c.follow.wanted.clone())
    else {
        return;
    };
    let current = ed.doc().id;
    let mut order: Vec<DocId> = vec![current];
    order.extend(ed.docs.iter().map(|d| d.id).filter(|&id| id != current));
    let mut missing = Vec::new();
    for id in order {
        let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { continue };
        let (Some(path), Some(tree)) = (doc.path.clone(), doc.syntax.as_ref().and_then(|s| s.tree.clone()))
        else {
            continue;
        };
        let version = doc.version();
        let Some(f) = ed.session.chat.as_ref().map(|c| &c.follow) else { return };
        let Some((_, _, defs)) = f.defs.iter().find(|d| d.0 == id && d.1 == version) else {
            if !f.outlining.contains(&(id, version)) {
                missing.push((id, version, tree, doc.text.clone()));
            }
            continue;
        };
        // `Type::name` → a `name` inside a definition that says `Type` first
        let inside = |d: &crate::syntax::Def| {
            parent.as_ref().is_none_or(|p| {
                defs.iter()
                    .any(|o| o.depth < d.depth && o.range.contains(&d.at) && o.name.contains(p.as_str()))
            })
        };
        if let Some(d) =
            defs.iter().find(|d| d.name == name && inside(d)).or_else(|| defs.iter().find(|d| d.name == name))
        {
            let lines = doc.text.byte_to_line(d.range.start)
                ..doc.text.byte_to_line(d.range.end.saturating_sub(1)) + 1;
            return look_at(ed, &path, lines, why, paced);
        }
    }
    for (id, version, tree, text) in missing {
        if let Some(f) = follow(ed) {
            f.outlining.push((id, version));
        }
        ed.events.jobs().spawn(move || {
            let defs = crate::syntax::outline(&tree, &text);
            move |ed: &mut Editor| {
                let Some(f) = follow(ed) else { return };
                f.outlining.retain(|o| *o != (id, version));
                f.defs.retain(|d| d.0 != id);
                f.defs.push((id, version, defs));
                find_wanted(ed);
            }
        });
    }
}

/// A tool finished (`error`: it failed, or was denied): its stop takes the row's verdict; an edit is
/// followed once written; a search or a lookup goes to its first hit, the count on the card.
pub fn on_result(ed: &mut Editor, id: &str, text: &str, error: bool) {
    // The row's verdict (a run's `ok`, `12 references in 3 files`) is the stop's too
    let row = ed
        .session
        .chat
        .as_ref()
        .and_then(|c| c.msgs.iter().rev().find(|m| m.call == id))
        .and_then(|m| m.look.as_ref())
        .map(|l| l.range.clone());
    let Some(f) = follow(ed) else { return };
    let Some((_, name, input)) = f.calls.iter().find(|c| c.0 == id).cloned() else { return };
    if let (Some(range), Some(stop)) = (row, f.replay.by_call(id)) {
        stop.look.range = range;
    }
    match name.as_str() {
        "Edit" | "Write" => {
            let path =
                crate::chat::resolve(&ed.session.root, input["file_path"].as_str().unwrap_or_default());
            edit_written(ed, id, &name, &input, path, error);
        }
        "Grep" | "Glob" | "mcp__tarae__search" | "mcp__tarae__references" | "mcp__tarae__definition"
            if !error =>
        {
            found(ed, id, text);
        }
        _ => {}
    }
}

/// A search's or a lookup's answer: its first hit and its count — on its stop, its row and (while it's
/// still where Claude is) the card, the editor at the hit.
fn found(ed: &mut Editor, id: &str, text: &str) {
    let root = ed.session.root.clone();
    let hits = crate::replay::hits(&root, text);
    let count = crate::replay::count(text, &hits, text.len() >= 4000);
    let first = hits.into_iter().find(|(p, _)| p.starts_with(&root) || !p.is_absolute());
    if let Some(m) = ed.session.chat.as_mut().and_then(|c| c.msgs.iter_mut().rev().find(|m| m.call == id))
        && let Some(l) = m.look.as_mut()
    {
        if !count.is_empty() {
            l.range = count.clone();
        }
        // (a row with a place is a link — a click goes to its stop)
        if let Some((p, line)) = first.as_ref().filter(|_| l.link.is_none()) {
            l.link = Some(crate::chat::Link { path: p.clone(), line: *line });
        }
        m.cache.replace(None);
    }
    let Some(f) = follow(ed) else { return };
    if let Some(stop) = f.replay.by_call(id) {
        if !count.is_empty() {
            stop.look.range = count.clone();
        }
        if let Some((p, l)) = &first {
            stop.path = Some(p.clone());
            stop.lines = Some(*l..*l + 1);
        }
    }
    let Some(g) = f.gaze.as_mut().filter(|g| g.call == id) else { return };
    if !count.is_empty() {
        g.look.range = count;
    }
    if let Some((path, line)) = first {
        g.path = Some(path.clone());
        g.lines = Some(line..line + 1);
        go_range(ed, &path, line..line + 1);
    }
}

/// An edit or a write is done (claude's tool answered): what the file is now is read on a worker, its
/// changes worked out against what it was when the edit was allowed (else from the edit itself), and kept
/// on its stop; the editor shows them while it's still where Claude is — an open buffer taking the file now,
/// as the disk watch would a moment later. Failed or denied: its kept text goes.
fn edit_written(
    ed: &mut Editor,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    path: PathBuf,
    error: bool,
) {
    let generation = ed.session.chat.as_ref().map(crate::chat::Chat::generation);
    let (id, write) = (id.to_string(), name == "Write");
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let (old, new, all) = (s("old_string"), s("new_string"), input["replace_all"].as_bool() == Some(true));
    ed.events.jobs().spawn(move || {
        let real = std::fs::canonicalize(&path).unwrap_or(path);
        let now = (!error).then(|| std::fs::read_to_string(&real).ok()).flatten();
        let stamp = crate::disk::stamp(&real);
        move |ed: &mut Editor| {
            if ed.session.chat.as_ref().map(crate::chat::Chat::generation) != generation {
                return;
            }
            let Some(f) = follow(ed) else { return };
            let kept = f.befores.iter().position(|b| b.0 == real).map(|i| f.befores.remove(i).1);
            let Some(after) = now else { return };
            // (allowed without tarae asking — then what it was comes from the edit itself)
            let before = match kept {
                Some(b) => Some(b.unwrap_or_default()),
                None if write => None,
                None => unedit(&after, &old, &new, all),
            };
            landed(ed, &id, real, before, after, stamp);
        }
    });
}

/// The text before an Edit, from the text after it: `new` back to `old` — only where that's certain (once
/// in the file, or all of them for `replace_all`).
fn unedit(after: &str, old: &str, new: &str, all: bool) -> Option<String> {
    if new.is_empty() {
        return None;
    }
    match after.matches(new).count() {
        0 => None,
        _ if all => Some(after.replace(new, old)),
        1 => Some(after.replacen(new, old, 1)),
        _ => None,
    }
}

/// An edit of `real` landed: `before` → `after` as its stop's changes (the stops before it in that file
/// moved along); followed now if the editor is with Claude.
fn landed(
    ed: &mut Editor,
    id: &str,
    real: PathBuf,
    before: Option<String>,
    after: String,
    stamp: Option<crate::disk::Stamp>,
) {
    let changes = before.as_deref().map(|b| crate::replay::hunks(b, &after)).unwrap_or_default();
    let moves: Vec<(Range<usize>, Range<usize>)> =
        changes.iter().map(|(o, h)| (o.clone(), h.lines.clone())).collect();
    let hunks: Vec<crate::replay::Hunk> = changes.into_iter().map(|c| c.1).collect();
    let lines = hunks.first().map_or(0..1, |h| h.lines.start..h.lines.end.max(h.lines.start + 1));
    let range = match (lines.len(), hunks.len()) {
        (_, 0) => String::new(),
        (1, n) => format!("L{}{}", lines.start + 1, more(n)),
        (_, n) => format!("L{}–{}{}", lines.start + 1, lines.end, more(n)),
    };
    // The row says where (it was the file's top until now)
    if let Some(m) = ed.session.chat.as_mut().and_then(|c| c.msgs.iter_mut().rev().find(|m| m.call == id))
        && let Some(l) = m.look.as_mut()
    {
        l.range = range.clone();
        if let Some(link) = l.link.as_mut() {
            link.line = lines.start;
        }
        m.cache.replace(None);
    }
    let background = ed.background();
    let Some(f) = follow(ed) else { return };
    f.replay.edited(&real, &moves);
    let mut look = None;
    if let Some(stop) = f.replay.by_call(id) {
        stop.path = Some(real.clone());
        stop.lines = Some(lines.clone());
        stop.hunks = hunks.clone();
        stop.look.range = range.clone();
        if let Some(link) = stop.look.link.as_mut() {
            link.line = lines.start;
        }
        look = Some(stop.look.clone());
    }
    // Followed while the editor is with Claude and nothing newer took its place
    let ours = f.gaze.as_ref().is_some_and(|g| g.call == id || g.call.is_empty());
    let Some(look) = look.filter(|_| ours && f.active() && !background) else { return };
    if !real.is_file() || !in_project(ed, &real) {
        return;
    }
    ed.disk_take(&real, &ropey::Rope::from_str(&after), stamp);
    if let Some(f) = follow(ed) {
        f.gaze =
            Some(Gaze { call: id.to_string(), ..Gaze::new(look, Some(real.clone()), Some(lines.clone())) });
    }
    go_range(ed, &real, lines.clone());
    // Its changes drawn while the buffer is the file as written (one with unsaved edits of yours isn't)
    let shown_in = ed
        .docs
        .iter()
        .find(|d| d.path.as_deref() == Some(real.as_path()) && !d.loading && d.text == after.as_str())
        .map(|d| (d.id, d.version()));
    if let Some(g) = follow(ed).and_then(|f| f.gaze.as_mut()).filter(|g| g.call == id) {
        g.shown_in = shown_in;
        g.edit = if shown_in.is_some() { hunks } else { Vec::new() };
    }
    sync_shown(ed);
    // (placed again: its removed rows above it count now)
    if shown_in.is_some() && ed.doc().path.as_deref() == Some(real.as_path()) {
        ed.show_range(lines);
    }
}

/// `+2` — more changes in the edit than the first, which the range names.
fn more(n: usize) -> String {
    if n > 1 { format!(" +{}", n - 1) } else { String::new() }
}

/// The answer is complete. A walk-through goes on showing its stops (they came with the answer, faster
/// than anyone reads) and they go into the list — when it's empty or holds the last walk-through, never
/// over a build's errors. Without one: to the answer's first reference, the places it mentioned in the list.
/// Scrolling the chat from now on moves the editor (`reading`).
pub fn on_done(ed: &mut Editor) {
    on_words(ed, true);
    let scroll = ed.session.chat.as_ref().map(|c| c.scroll);
    let Some(f) = follow(ed) else { return };
    let mut stops = std::mem::take(&mut f.stops);
    let touring = f.touring();
    f.wanted = None;
    f.read_at = scroll;
    // (a walk-through's last stop stays — its card waits there while you read it, to ask about it)
    if !touring && f.gaze.as_ref().is_none_or(|g| g.step.is_none()) {
        f.gaze = None;
    }
    if stops.iter().any(|s| s.shown) {
        stops.retain(|s| s.shown);
    } else {
        // (the last of its words this turn that point anywhere — a closing line may not)
        let first = ed.session.chat.as_ref().and_then(|c| {
            let turn = c.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
            c.msgs[turn..]
                .iter()
                .rev()
                .filter(|m| m.role == Role::Assistant)
                .find_map(|m| refs_in(&ed.session.root, &m.text).into_iter().next())
                .map(|(_, l)| l)
        });
        if let Some(link) = &first {
            go(ed, &link.path, link.line);
        }
    }
    let free = ed.session.qlist.as_ref().is_none_or(|l| l.items.is_empty() || l.title == TOUR);
    if stops.len() < 2 || !free {
        return;
    }
    let showing = ed
        .session
        .chat
        .as_ref()
        .and_then(|c| c.follow.gaze.as_ref())
        .and_then(|g| Some((g.path.clone()?, g.lines.as_ref()?.start)));
    let items: Vec<crate::quickfix::Item> = stops
        .into_iter()
        .map(|s| crate::quickfix::Item {
            path: s.path,
            line: s.lines.start,
            col: 0,
            message: s.why,
            severity: 0,
            mark: None,
        })
        .collect();
    let at = showing.and_then(|(p, l)| items.iter().position(|it| it.path == p && it.line == l));
    // While the tour plays, its card has the screen — the list says itself when the tour is over
    let touring = ed.session.chat.as_ref().is_some_and(|c| c.follow.touring());
    if touring {
        ed.qlist_put(TOUR, items);
    } else {
        ed.qlist_set(TOUR, items);
    }
    if let Some(f) = follow(ed) {
        f.announce = touring;
    }
    if let Some(l) = ed.session.qlist.as_mut() {
        l.current = at;
    }
}

/// After the turn, while you read the answer: when the chat scrolls, the editor shows the first place the
/// chat now shows (its links, as the last frame drew them). True when it moved — draw again.
pub fn reading(ed: &mut Editor) -> bool {
    let Some(c) = ed.session.chat.as_ref() else { return false };
    // (read_at is set when a turn ends — before that there's no answer to read along)
    if c.busy()
        || !c.follow.active()
        || ed.screen.chat_x.is_none()
        || c.follow.read_at.is_none_or(|r| r == c.scroll)
    {
        return false;
    }
    let scroll = c.scroll;
    let first = ed.screen.chat_links.borrow().iter().min_by_key(|l| (l.0, l.1)).map(|l| l.3.clone());
    if let Some(f) = follow(ed) {
        f.read_at = Some(scroll);
    }
    let Some(link) = first else { return false };
    // You're reading now — the walk-through stops
    if let Some(f) = follow(ed) {
        f.queue.clear();
        f.playing = None;
        f.gaze = None;
    }
    let before = cursor_key(ed);
    go(ed, &link.path, link.line);
    cursor_key(ed) != before
}

/// The list a walk-through leaves.
const TOUR: &str = "Claude's walk-through";

/// Whether `path` is in the project (`Editor::root` — the folder tarae started in, links resolved):
/// follow goes nowhere else — a file Claude reads elsewhere (its memory under `~/.claude`, a temp file)
/// isn't opened; the card still says what it reads.
fn in_project(ed: &Editor, path: &Path) -> bool {
    std::fs::canonicalize(path).is_ok_and(|p| p.starts_with(&ed.session.root))
}

/// Moves the editor to `line` (when following and the user hasn't taken over) — in the project only.
fn go(ed: &mut Editor, path: &Path, line: usize) {
    go_range(ed, path, line..line + 1);
}

/// Moves the editor to `lines` of `path`, the cursor on the first, the range placed as a whole
/// (`Editor::show_range`) — when following and the user hasn't taken over, in the project only.
fn go_range(ed: &mut Editor, path: &Path, lines: Range<usize>) {
    // (a session behind keeps its trail; the editor in front is someone else's)
    if ed.background()
        || !ed.session.chat.as_ref().is_some_and(|c| c.follow.active())
        || !path.is_file()
        || !in_project(ed, path)
    {
        return;
    }
    let line = lines.start;
    // (live again: a replay's stop isn't what's showing any more)
    if let Some(f) = follow(ed) {
        f.replaying = None;
        f.replay.at = None;
    }
    sync_shown(ed);
    let already = ed.docs.iter().any(|d| d.path.as_deref() == Some(path));
    if !ed.session.chat.as_ref().is_some_and(|c| c.follow.jumped) {
        ed.push_jump();
    }
    if let Err(e) = ed.open_for_claude(path) {
        return ed.set_error(format!("{e:#}"));
    }
    let here = ed.doc().id;
    // (one in the build output goes when the turn ends, unless you take it up — #46)
    let aside = ed.doc().path.as_deref().is_some_and(|p| crate::edits::aside_path(&ed.session.root, p));
    if !already && aside {
        ed.doc_mut().aside = true;
    }
    // A big file loads in the background: the spot is applied when its text arrives (`watch` then settles)
    let loading = ed.doc().loading;
    if loading {
        ed.goto_after_load = Some((here, line, 0));
    } else {
        let doc = ed.doc_mut();
        let line = line.min(crate::movement::last_line(&doc.text));
        doc.set_selection(Selection::point(crate::movement::line_start(&doc.text, line)));
        ed.show_range(lines);
    }
    let placed = cursor_key(ed);
    let Some(f) = follow(ed) else { return };
    f.jumped = true;
    f.placed = (!loading).then_some(placed);
    f.awaiting = loading.then_some(here);
    // Previews it opened before and nobody edited go away
    let stale: Vec<DocId> = f.previews.iter().copied().filter(|&id| id != here).collect();
    f.previews.retain(|&id| id == here);
    if !already {
        f.previews.push(here);
    }
    for id in stale {
        if ed.docs.iter().any(|d| d.id == id && !d.is_modified()) {
            ed.close_doc(id);
        }
    }
}

/// Every event: the user moved the cursor or switched files since follow last did → pause. Only a key, a
/// click or a paste is the user: a cursor moved with none since (Claude's edit reloading the open file
/// shifts it — that paused follow for the rest of a turn, the editor left on the last read, its edits never
/// shown) is still follow's. Paused, each press counts toward following again after a quiet while.
pub fn watch(ed: &mut Editor) {
    let now = cursor_key(ed);
    let input_seq = ed.input_seq;
    sync_shown(ed);
    // Waiting for a big file: once loaded (the editor put the cursor there), that's where follow is
    if let Some(id) = ed.session.chat.as_ref().and_then(|c| c.follow.awaiting) {
        let loaded = ed.docs.iter().any(|d| d.id == id && !d.loading);
        let gone = !ed.docs.iter().any(|d| d.id == id);
        if let Some(f) = follow(ed)
            && (loaded || gone)
        {
            f.awaiting = None;
            f.placed = (loaded && now.0 == id).then_some(now);
        }
        return;
    }
    let Some(f) = follow(ed) else { return };
    let input = std::mem::replace(&mut f.seen_input, input_seq) != input_seq;
    // You moving away from a replay's stop ends its showing (where it is in the replay stays)
    if input && f.replaying.is_some() && f.replay_placed.is_some_and(|p| p != now) {
        f.replaying = None;
        f.replay_placed = None;
    }
    if let Some(p) = f.placed
        && p != now
    {
        if !input {
            f.placed = Some(now);
        } else if f.active() {
            f.paused = true;
            f.placed = None;
        }
    }
    if input && f.paused {
        f.last_input = Some(std::time::Instant::now());
        arm_resume(ed);
    }
}

/// Follow's edit on the document it's in: its changes on the review's wash, the lines they replaced above
/// them (`Document::shown`) — while the buffer is still the version they were seen in; none elsewhere.
fn sync_shown(ed: &mut Editor) {
    let want = ed.session.chat.as_ref().and_then(|c| c.follow.shown()).and_then(|g| {
        let (id, version) = g.shown_in?;
        (!g.edit.is_empty()).then(|| (id, version, g.edit.clone()))
    });
    for d in ed.docs.iter_mut() {
        let here = want.as_ref().filter(|(id, v, _)| *id == d.id && *v == d.version()).map(|w| &w.2);
        if d.shown.as_ref() != here {
            d.shown = here.cloned();
        }
    }
}

/// How long after your last key follow takes over again while Claude works (`llm.follow-resume`; 0 =
/// only `C-f`).
fn quiet(ed: &Editor) -> Option<std::time::Duration> {
    let secs = ed.config.llm.follow_resume;
    // (tests: a second is 20 ms)
    let ms = if cfg!(test) { 20 } else { 1000 };
    (secs > 0).then(|| std::time::Duration::from_millis(secs * ms))
}

/// One wake-up at a time for following again: when it comes, follow takes over if you've been quiet that
/// long and Claude still works — else it waits the rest (or again, while you type or a card has the keys).
fn arm_resume(ed: &mut Editor) {
    let Some(wait) = quiet(ed) else { return };
    let Some(f) = follow(ed).filter(|f| !f.resume_armed) else { return };
    let since = f.last_input.map_or(wait, |t| t.elapsed());
    f.resume_armed = true;
    let left = wait.saturating_sub(since).max(std::time::Duration::from_millis(10));
    let session = ed.session.id;
    ed.events.jobs().spawn(move || {
        std::thread::sleep(left);
        move |ed: &mut Editor| {
            // The session behind now (swapped in for this — `with_session`) or gone: its follow waits for it to
            // come back, and `session_in` arms it again (left armed, no wake-up would ever come for it)
            let here = ed.session.id == session;
            if !here || ed.background() {
                if let Some(f) = follow(ed).filter(|_| here) {
                    f.resume_armed = false;
                }
                return;
            }
            resume_due(ed)
        }
    });
}

fn resume_due(ed: &mut Editor) {
    let busy = ed.session.chat.as_ref().is_some_and(|c| c.busy());
    let free = ed.mode == crate::editor::Mode::Normal
        && ed.prompt.is_none()
        && ed.picker.is_none()
        && ed.session.review.is_none()
        && !ed.session.chat.as_ref().is_some_and(|c| c.focused && !c.input.is_empty());
    let wait = quiet(ed);
    let Some(f) = follow(ed) else { return };
    f.resume_armed = false;
    if !f.paused || !f.on || !busy {
        ed.unchanged = true;
        return;
    }
    let quiet_enough = wait.is_some_and(|w| f.last_input.is_none_or(|t| t.elapsed() >= w));
    if !quiet_enough || !free {
        if !quiet_enough {
            ed.unchanged = true;
        } else if let Some(f) = follow(ed) {
            // (typing, a prompt or a card: looked at again a while later)
            f.last_input = Some(std::time::Instant::now());
        }
        return arm_resume(ed);
    }
    f.paused = false;
    f.replaying = None;
    f.replay.at = None;
    let target = f.gaze.as_ref().and_then(|g| Some((g.path.clone()?, g.lines.clone().unwrap_or(0..1))));
    ed.set_status("following Claude again — C-f stops it");
    if let Some((path, lines)) = target {
        go_range(ed, &path, lines);
    }
    play(ed);
}

/// `]r` / `[r`: the next or previous stop of Claude's turns — the editor at its file and lines, an edit's
/// changes shown as the review shows them, the card saying which stop of how many. While Claude works it
/// holds follow (you're looking back); past the newest stop it follows again.
pub fn replay_step(ed: &mut Editor, forward: bool) {
    let Some(c) = ed.session.chat.as_mut() else { return ed.set_status("no chat — nothing to replay") };
    let busy = c.busy();
    let f = &mut c.follow;
    match f.replay.step(forward) {
        Some(at) => show_stop(ed, at),
        None if forward && f.replay.at.is_some() => {
            f.replay.at = None;
            f.replaying = None;
            f.replay_placed = None;
            if busy && f.on {
                f.paused = false;
                f.placed = None;
                let target =
                    f.gaze.as_ref().and_then(|g| Some((g.path.clone()?, g.lines.clone().unwrap_or(0..1))));
                ed.set_status("the newest stop — following Claude again");
                if let Some((path, lines)) = target {
                    go_range(ed, &path, lines);
                }
            } else {
                ed.set_status("that was the newest stop");
            }
            sync_shown(ed);
        }
        None if f.replay.latest() == 0 => {
            ed.set_status("nothing to replay yet — Claude's reads, edits and searches show here")
        }
        None => ed.set_status("that was the first stop kept"),
    }
}

/// A click on a chat row of a call, or `C-]` there: that call's stop. False: it has none.
pub fn replay_call(ed: &mut Editor, call: &str) -> bool {
    let Some(at) = ed.session.chat.as_ref().and_then(|c| c.follow.replay.place_of(call)) else {
        return false;
    };
    let has_place =
        ed.session.chat.as_ref().and_then(|c| c.follow.replay.get(at)).is_some_and(|s| s.path.is_some());
    if has_place {
        show_stop(ed, at);
    }
    has_place
}

/// Shows stop `at`: its file and lines (in the project), its changes if the buffer still has them as they
/// were written, the card on it.
fn show_stop(ed: &mut Editor, at: (usize, usize)) {
    let root = ed.session.root.clone();
    let Some(c) = ed.session.chat.as_mut() else { return };
    let busy = c.busy();
    let f = &mut c.follow;
    let Some(stop) = f.replay.get(at).cloned() else { return };
    let (k, n, _) = f.replay.position(at);
    f.replay.at = Some(at);
    // Looking back while Claude works holds follow (it takes over again a quiet while after your last key)
    if busy && f.active() {
        f.paused = true;
        f.placed = None;
    }
    if f.paused {
        f.last_input = Some(std::time::Instant::now());
    }
    let why = (!stop.why.is_empty()).then(|| stop.why.clone());
    f.replaying = Some(Gaze {
        why,
        step: Some((k, n)),
        call: stop.call.clone(),
        ..Gaze::new(stop.look.clone(), stop.path.clone(), stop.lines.clone())
    });
    let place = stop
        .path
        .clone()
        .filter(|p| p.is_file() && std::fs::canonicalize(p).is_ok_and(|p| p.starts_with(&root)));
    if let Some(path) = place {
        let lines = stop.lines.clone().unwrap_or(0..1);
        if let Err(e) = ed.open_for_claude(&path) {
            return ed.set_error(format!("{e:#}"));
        }
        if !ed.doc().loading {
            let doc = ed.doc();
            // Its changes, if the buffer still has them where they were written
            let same = !stop.hunks.is_empty()
                && stop.hunks.iter().all(|h| {
                    h.added.iter().enumerate().all(|(i, l)| {
                        let at = h.lines.start + i;
                        at < doc.text.len_lines()
                            && doc.text.line(at).to_string().trim_end_matches(['\n', '\r']) == l
                    })
                });
            let shown_in = same.then(|| (doc.id, doc.version()));
            let doc = ed.doc_mut();
            let line = lines.start.min(crate::movement::last_line(&doc.text));
            doc.set_selection(Selection::point(crate::movement::line_start(&doc.text, line)));
            if let Some(g) = follow(ed).and_then(|f| f.replaying.as_mut()) {
                g.shown_in = shown_in;
                g.edit = if same { stop.hunks.clone() } else { Vec::new() };
            }
            sync_shown(ed);
            ed.show_range(lines);
        } else {
            ed.goto_after_load = Some((ed.doc().id, lines.start, 0));
        }
    } else {
        sync_shown(ed);
    }
    let now = cursor_key(ed);
    if let Some(f) = follow(ed) {
        f.replay_placed = Some(now);
    }
    arm_resume(ed);
}

/// Where the cursor is now counts as follow's (a review of Claude's edit moved it — not the user).
pub fn settle(ed: &mut Editor) {
    let now = cursor_key(ed);
    if let Some(f) = follow(ed)
        && f.active()
    {
        f.placed = Some(now);
    }
}

/// `C-f`: off → on · paused → following again · on → off. Turning it on goes to the current gaze.
pub fn toggle(ed: &mut Editor) {
    let Some(f) = follow(ed) else { return };
    let resume = !f.active();
    if resume {
        f.on = true;
        f.paused = false;
    } else {
        f.on = false;
    }
    let target =
        f.gaze.as_ref().and_then(|g| Some((g.path.clone()?, g.lines.as_ref().map_or(0, |l| l.start))));
    if resume {
        ed.set_status("following Claude — move in the editor to take over");
        if let Some((path, line)) = target {
            go(ed, &path, line);
        }
        play(ed);
    } else {
        ed.set_status("follow off");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_and_names_in_what_claude_says() {
        let cwd = Path::new("/proj");
        let m = mentions(
            cwd,
            "Built in src/filetree.rs:120-140, then `Tree::rows()` lays it out (see `open`).",
            false,
        );
        assert_eq!(
            m.iter().map(|x| &x.1).collect::<Vec<_>>(),
            [
                &Mention::Place(cwd.join("src/filetree.rs"), 119..140),
                &Mention::Name("rows".into(), Some("Tree".into())),
                &Mention::Name("open".into(), None),
            ]
        );
        assert!(
            mentions(cwd, "a `let x = 1` and ```rust\nfn a() {}\n```", false).is_empty(),
            "code, not names"
        );
        assert_eq!(
            mentions(cwd, "src/a.rs:7–9 and 12:30", false)[0].1,
            Mention::Place(cwd.join("src/a.rs"), 6..9)
        );
        // Started inside a span: the next backtick closes it
        assert_eq!(mentions(cwd, "still code` then `wind`", true).len(), 1);
    }
}
