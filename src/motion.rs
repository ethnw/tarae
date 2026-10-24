//! `editor.motion` — how much moves on screen: `full` (default) · `calm` · `none`. What moves on a clock (the
//! waiting wave, a toast's fade) moves only at `full`; at `calm` the wave holds still, its time counted by the
//! second, and a toast comes and goes whole. Bleeding ink (번짐 — the head of what's being written, a function of
//! where the stream is, never of time) stays at `calm` and goes at `none`; the rows of an edit being written and
//! a decision's stamp stay at all three — shown at the stream's pace (`writing::Pace`: it takes the lumps' jumps out)
//! at `full` and `calm`, as they come at `none`. macOS's Reduce Motion (read once at start, on a worker) makes `full`
//! behave as `calm`; an explicit `none` stays `none`.
//!
//! Motions on a clock (`Moving`, `Editor.motions`): what an invisible act touched flashes (undo, redo, `.`, a
//! yank, a paste that leaves no selection) — made where the act is, drawn by `term::drying` on one ease-out
//! (`strength`), a frame a `TICK` step of its own clock (`motion_wake`) until it's gone, then none. At `calm` a
//! flash shows whole for its time and goes in one frame; at `none` none is made (nothing to say). Unfocused, or
//! from before a switch to `none`, one ends by its time: a frame as it goes. Made in tests too (drawn only where
//! `motion_drawn` — off in tests unless one turns it on).
//!
//! A stamp your key makes in sight (§5: an edit review's change head, the turn review rail's file, the chat's knot
//! row) is pressed (`press`, `What::Press` by its `Place`): it lands lit and dries to its dry ink over `PRESS` on the
//! flash's drying (`pressed`, `term::stamp_spans`); on the row still in hand it stays lit. Only at `full`: at `calm`
//! and `none` it's dry at once (nothing on a clock).
//!
//! What a flash says (`flash_saying`: a yank's "yanked …") is a note only where a frame drew that flash: kept in
//! `:messages` at once, its toast after the frame that didn't draw it (the chat's transcript draws no flash, a
//! buffer moved or out of sight, named colours) or as it goes undrawn (`words_drawn`, `words_unseen`).

use std::cell::Cell;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::document::DocId;
use crate::editor::{Editor, TICK, ToastKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Full,
    Calm,
    None,
}

pub const VALUES: &[&str] = &["full", "calm", "none"];

impl Editor {
    /// How much moves now: the setting, `full` as `calm` under the system's Reduce Motion.
    pub fn motion(&self) -> Motion {
        match self.config.motion.as_str() {
            "none" => Motion::None,
            "calm" => Motion::Calm,
            _ if self.reduce_motion => Motion::Calm,
            _ => Motion::Full,
        }
    }

    /// Whether what moves on a clock moves (the wave, a toast's fade).
    pub fn moving(&self) -> bool {
        self.motion() == Motion::Full
    }

    /// Whether the head of what's being written is drawn bleeding (off in tests unless one turns it on — `bleed_ink`).
    pub fn bleeds(&self) -> bool {
        self.bleed_ink && self.motion() != Motion::None
    }

    /// How old a motion is: its time on the clock, or `TARAE_MOTION_AT`'s pinned age (shots).
    pub fn motion_age(&self, m: &Moving) -> Duration {
        self.motion_at.unwrap_or_else(|| crate::clock::since(m.born))
    }

    /// How strong a motion is drawn now, 0 ‥ 1 — at `full` its drying (`strength`), at `calm` whole while it shows,
    /// at `none` (and where nothing is drawn — tests) nothing.
    pub fn motion_strength(&self, m: &Moving) -> f32 {
        if !self.motion_drawn {
            return 0.0;
        }
        let age = self.motion_age(m);
        match self.motion() {
            Motion::Full => strength(age, m.life),
            Motion::Calm if age < shown_for(m.life) => 1.0,
            _ => 0.0,
        }
    }

    /// What an act touched in the current document flashes: `ranges` (byte ranges at its version now — sorted
    /// and merged here, an empty one widened to the character there, past `RANGES_MAX` none). None at `none`;
    /// a newer flash on that document takes the place of the one before.
    pub fn flash(&mut self, act: Act, ranges: Vec<Range<usize>>) {
        if let Some(what) = self.flash_of(act, ranges) {
            self.motion_start(what);
        }
    }

    /// A flash of the current document's `ranges`, not yet begun (`flash`; a yank to `+` carries it with its copy
    /// until the clipboard took it — `clipboard::copy_flashing`). None at `none`.
    pub fn flash_of(&self, act: Act, ranges: Vec<Range<usize>>) -> Option<What> {
        if self.motion() == Motion::None {
            return None;
        }
        let doc = self.doc();
        let ranges = flash_ranges(&doc.text, ranges)?;
        Some(What::Flash { doc: doc.id, version: doc.version(), ranges, act })
    }

    /// A motion begins now, in the place of one of its kind on the same thing (whose words, unsaid, are a toast).
    pub fn motion_start(&mut self, what: What) {
        let (gone, kept): (Vec<Moving>, Vec<Moving>) =
            std::mem::take(&mut self.motions).into_iter().partition(|m| m.what.same(&what));
        self.motions = kept;
        self.words_unseen(gone);
        let life = match what {
            What::Flash { .. } => FLASH,
            What::Press(_) => PRESS,
        };
        self.motions.push(Moving {
            what,
            born: crate::clock::now(),
            life,
            seen: Cell::new(false),
            words: None,
        });
    }

    /// Your key stamped a decision at `place`, in sight (§5): the stamp lands lit and dries over `PRESS`. Only at
    /// `full` — at `calm` and `none` it's dry at once, as before.
    pub fn press(&mut self, place: Place) {
        if self.motion() == Motion::Full {
            self.motion_start(What::Press(place));
        }
    }

    /// The diff of `doc` at `version` landed (`git_schedule`): a hunk reset waiting on it (`press_on_diff`) presses
    /// its turn file's `u` now, the frame it first shows — where that diff is empty; a later version's lets it go.
    pub fn diff_landed(&mut self, doc: DocId, version: u64) {
        let Some((id, at)) = self.press_on_diff.filter(|&(id, at)| id == doc && version >= at) else {
            return;
        };
        self.press_on_diff = None;
        let d = self.docs.iter().find(|d| d.id == id);
        if version == at
            && let Some(path) = d.filter(|d| d.git_hunks.is_empty()).and_then(|d| d.path.clone())
        {
            crate::codereview::press_turn_file(self, &path);
        }
    }

    /// How lit the stamp at `place` is drawn now, 0 (dry) ‥ 1 (lit) — its press drying; 0 with none under way, or
    /// where nothing moves on a clock.
    pub fn pressed(&self, place: &Place) -> f32 {
        if !self.moving() {
            return 0.0;
        }
        (self.motions.iter())
            .find(|m| matches!(&m.what, What::Press(p) if p == place))
            .map_or(0.0, |m| self.motion_strength(m))
    }

    /// `flash` begins saying `msg` (a yank's "yanked …"): kept in `:messages` and a note now, a toast only if no
    /// frame draws it (`words_drawn`); no flash, a toast at once.
    pub fn flash_saying(&mut self, flash: Option<What>, kind: ToastKind, msg: String) {
        let Some(what) = flash else { return self.say(kind, msg) };
        self.motion_start(what);
        let msg = self.flashed_note(kind, msg);
        if let Some(m) = self.motions.last_mut() {
            m.words = Some((kind, msg));
        }
    }

    /// After a frame: the words of a flash it drew stay a note; of one it didn't, a toast — whether one was made
    /// (the frame is drawn again with it).
    pub fn words_drawn(&mut self) -> bool {
        let unseen: Vec<(ToastKind, String)> =
            (self.motions.iter_mut()).filter_map(|m| m.words.take().filter(|_| !m.seen.get())).collect();
        let said = !unseen.is_empty();
        unseen.into_iter().for_each(|(kind, msg)| self.flash_unseen(kind, msg));
        said
    }

    /// Motions gone before a frame drew them: their words a toast.
    fn words_unseen(&mut self, gone: Vec<Moving>) {
        for (kind, msg) in gone.into_iter().filter_map(|m| m.words) {
            self.flash_unseen(kind, msg);
        }
    }

    /// What the current document's transactions write is recorded from now on (`Document.touched`) — whether
    /// this call armed it (an act inside another's, a paste in a `.`, leaves it to the outer one).
    pub fn record_touched(&mut self) -> bool {
        let doc = self.doc_mut();
        let armed = doc.touched.is_none();
        if armed {
            doc.touched = Some(Vec::new());
        }
        armed
    }

    /// What was recorded since `record_touched` flashes as `act`'s, where that call armed it.
    pub fn flash_touched(&mut self, armed: bool, act: Act) {
        if !armed {
            return;
        }
        let ranges = self.doc_mut().touched.take().unwrap_or_default();
        self.flash(act, ranges);
    }

    /// The flash to draw on `doc` now, if one is — its ranges (at the version they were made at), strength, and
    /// where the drawer says it drew it (`Moving.seen`, for its words).
    pub fn flash_on(&self, doc: &crate::document::Document) -> Option<ToDraw<'_>> {
        self.motions.iter().find_map(|m| match &m.what {
            What::Flash { doc: d, version, ranges, .. } if *d == doc.id && *version == doc.version() => {
                let k = self.motion_strength(m);
                (k > 0.0).then_some((ranges.as_slice(), k, &m.seen))
            }
            _ => None,
        })
    }

    /// Whether a motion moves on screen now, step by step: moving at all, in sight.
    fn motions_tick(&self) -> bool {
        self.motion() != Motion::None && !self.unfocused
    }

    /// When the next frame a motion wants is due — on its own clock, `born + k·TICK` (2 ms past), the first step
    /// a full `TICK` after its act; at `calm`, unfocused or at `none` once, as it goes (let go by its time). None
    /// past the last — every wake ends — and while pinned (`TARAE_MOTION_AT`).
    pub fn motion_wake(&self) -> Option<Duration> {
        if self.motion_at.is_some() {
            return None;
        }
        let full = self.moving() && self.motions_tick();
        self.motions
            .iter()
            .map(|m| {
                let (age, over) = (crate::clock::since(m.born), shown_for(m.life));
                if age >= over {
                    // (over and not yet let go: the frame that draws it gone, now)
                    return Duration::ZERO;
                }
                let at = if full {
                    (age.as_nanos() / TICK.as_nanos() + 1) * TICK.as_nanos()
                } else {
                    over.as_nanos()
                };
                Duration::from_nanos((at - age.as_nanos()) as u64) + Duration::from_millis(2)
            })
            .min()
    }

    /// Motions over let go (`schedule_tick`) — whether any went (a frame draws them gone). Pinned ages
    /// (`TARAE_MOTION_AT`) stay.
    pub(crate) fn motions_prune(&mut self) -> bool {
        if self.motion_at.is_some() || self.motions.is_empty() {
            return false;
        }
        let (gone, kept): (Vec<Moving>, Vec<Moving>) = std::mem::take(&mut self.motions)
            .into_iter()
            .partition(|m| crate::clock::since(m.born) >= shown_for(m.life));
        self.motions = kept;
        let any = !gone.is_empty();
        self.words_unseen(gone);
        any
    }
}

#[cfg(test)]
mod tests;

/// How long a flash lives (§1): its peak, near half at the next `TICK`, a trace, gone — a blink with a soft edge
/// (Neovim's yank highlight is a 150 ms hard cut; at 200 ms the curve left the step after the peak at a quarter,
/// unseen on the sheets).
pub const FLASH: Duration = Duration::from_millis(300);
/// How long a stamp's press dries (§5): lit on its key's frame, dry at its end — the flash's drying, its time.
pub const PRESS: Duration = Duration::from_millis(300);
/// The most ranges a flash keeps (a `.` over thousands of cursors flashes none).
pub const RANGES_MAX: usize = 256;
/// The most lines an undo's span may cover and still flash (past it, its line diff would be the cost).
pub const LINES_MAX: usize = 2000;
/// Under this strength a motion is gone: no frame spent on a change nobody can see.
const GONE: f32 = 0.05;

/// `TARAE_MOTION_AT=<ms>`: every motion drawn at that age and none ending — a shot's picture of a motion at a given
/// step (docshots take the peak, `0`). Changes when a picture is taken, never what one shows.
pub fn pinned_age() -> Option<Duration> {
    std::env::var("TARAE_MOTION_AT").ok()?.trim().parse().ok().map(Duration::from_millis)
}

/// A flash to draw (`Editor::flash_on`): its ranges, strength, and where the drawer says it drew it.
pub type ToDraw<'a> = (&'a [Range<usize>], f32, &'a Cell<bool>);

/// A motion under way: what moves, since when, for how long — and what it says, until a frame drew it or not.
#[derive(Clone, Debug)]
pub struct Moving {
    pub what: What,
    pub born: Instant,
    pub life: Duration,
    /// A frame drew it (`term::draw_text`, through `flash_on`).
    pub seen: Cell<bool>,
    /// Its words (`flash_saying`), kept and a note, waiting on the frame that draws it or not.
    pub words: Option<(ToastKind, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum What {
    /// What an invisible act touched (§1): byte ranges of a document at its version — sorted, apart.
    Flash { doc: DocId, version: u64, ranges: Vec<Range<usize>>, act: Act },
    /// A stamp your key pressed (§5), where it's drawn.
    Press(Place),
}

impl What {
    /// Whether `other` is a motion of the same thing — the one it takes the place of: a flash on the same document,
    /// a press at the same place.
    fn same(&self, other: &What) -> bool {
        match (self, other) {
            (What::Flash { doc: a, .. }, What::Flash { doc: b, .. }) => a == b,
            (What::Press(a), What::Press(b)) => a == b,
            _ => false,
        }
    }

    /// What the state names its act: a flash's act, a press `stamp`.
    pub fn act(&self) -> &'static str {
        match self {
            What::Flash { act, .. } => act.name(),
            What::Press(_) => "stamp",
        }
    }
}

/// Where a stamp is pressed — the place it's drawn, its key (§5: the press time is kept by it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// An edit review's change head: the review's document, the change's index.
    Head { doc: DocId, change: usize },
    /// The turn review rail's file.
    File(std::path::PathBuf),
    /// The chat's knot row of call `id` (and the follow card's, the same call).
    Knot(String),
}

/// The act a flash shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Undo,
    Redo,
    Repeat,
    Yank,
    Paste,
}

impl Act {
    pub fn name(self) -> &'static str {
        match self {
            Act::Undo => "undo",
            Act::Redo => "redo",
            Act::Repeat => "repeat",
            Act::Yank => "yank",
            Act::Paste => "paste",
        }
    }
}

impl Moving {
    /// Its kind, as the state names it: `flash`, `press`.
    pub fn kind(&self) -> &'static str {
        match self.what {
            What::Flash { .. } => "flash",
            What::Press(_) => "press",
        }
    }
}

/// The drying's strength at `age` of `life` — one ease-out, `(1 − t)²`, read at whole `TICK` steps (one colour a
/// step, the frames come on them): full on the act's own frame, a quick fall, a long last step, as ink dries.
/// Over 1 s: 1 · .81 · .64 · .49 · .36 · .25 · .16 · .09 · gone; over 0.2 s: 1 · .25 · gone. 0 under `GONE`.
pub fn strength(age: Duration, life: Duration) -> f32 {
    let step = age.as_nanos() / TICK.as_nanos() * TICK.as_nanos();
    let t = step as f64 / life.as_nanos().max(1) as f64;
    let k = (1.0 - t).max(0.0).powi(2) as f32;
    if k < GONE { 0.0 } else { k }
}

/// How long a motion of `life` shows: to its first step under `GONE`.
pub fn shown_for(life: Duration) -> Duration {
    let mut at = Duration::ZERO;
    while at < life && strength(at, life) > 0.0 {
        at += TICK;
    }
    at.min(life)
}

/// A flash's ranges in `text`: sorted, merged where they meet, each on character boundaries; an empty one (what
/// was taken out) the character there — the join —, or the one before at the end. None when there's nothing to
/// show or more than `RANGES_MAX`.
pub fn flash_ranges(text: &ropey::Rope, mut ranges: Vec<Range<usize>>) -> Option<Vec<Range<usize>>> {
    let n = text.len_bytes();
    if ranges.len() > RANGES_MAX || n == 0 {
        return None;
    }
    for r in &mut ranges {
        let start = crate::graphemes::snap(text, r.start.min(n));
        let end = crate::graphemes::snap(text, r.end.min(n)).max(start);
        *r = match start == end {
            true if start < n => start..crate::graphemes::next_boundary(text, start),
            true => crate::graphemes::prev_boundary(text, start)..start,
            false => start..end,
        };
    }
    crate::sorting::by_key(&mut ranges, |r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(l) if r.start <= l.end => l.end = l.end.max(r.end),
            _ => out.push(r),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// What undo or redo changed, from the text before it to the text after (the one span between their common ends):
/// within one line that span; across lines only the hunks of a line diff of the span's lines — a multi-cursor undo
/// doesn't wash the text it left alone between its cursors. Past `LINES_MAX` lines, none.
pub fn undone(old: &ropey::Rope, new: &ropey::Rope) -> Vec<Range<usize>> {
    let Some(c) = crate::disk::minimal_change(old, new) else { return Vec::new() };
    let (from, to) = (c.from, c.from + c.insert.len());
    let (l0, l1) = (new.byte_to_line(from), new.byte_to_line(to));
    if l0 == l1 {
        return std::iter::once(from..to).collect();
    }
    let (o0, o1) = (old.byte_to_line(c.from), old.byte_to_line(c.to));
    if l1 - l0 > LINES_MAX || o1 - o0 > LINES_MAX {
        return Vec::new();
    }
    // (the span's lines whole, both sides: a line diff of them; each hunk's new lines, or the join where it only took out)
    let line_end = |t: &ropey::Rope, l: usize| t.line_to_byte((l + 1).min(t.len_lines()));
    let (s_old, s_new) = (old.line_to_byte(o0), new.line_to_byte(l0));
    let a = old.byte_slice(s_old..line_end(old, o1)).to_string();
    let b = new.byte_slice(s_new..line_end(new, l1)).to_string();
    let at = |rel: usize| new.line_to_byte((l0 + rel).min(new.len_lines()));
    let mut out = Vec::new();
    for (_, h) in crate::replay::hunks(&a, &b) {
        // (as many lines each way: each line's own span, between what it kept at its ends — else its lines whole)
        if h.removed.len() != h.added.len() {
            out.push(at(h.lines.start)..at(h.lines.end));
            continue;
        }
        for (k, (was, now)) in h.removed.iter().zip(&h.added).enumerate() {
            let pre: usize =
                was.chars().zip(now.chars()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
            let room = was.len().min(now.len()) - pre;
            let suf: usize = was
                .chars()
                .rev()
                .zip(now.chars().rev())
                .take_while(|(x, y)| x == y)
                .map(|(x, _)| x.len_utf8())
                .scan(0, |n, l| {
                    *n += l;
                    (*n <= room).then_some(l)
                })
                .sum();
            let start = at(h.lines.start + k);
            out.push(start + pre..start + now.len() - suf);
        }
    }
    out
}

/// Whether macOS's Reduce Motion is on: `defaults read com.apple.universalaccess reduceMotion` says `1` (the
/// key it sets — measured on macOS 27, 2026-10-21: `0` with it off). A worker's: it runs `defaults`. Never in
/// tests; off macOS, no.
pub fn reduce_motion() -> bool {
    if cfg!(test) || !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
}
