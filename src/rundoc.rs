//! The run panels' output as documents (panes P2c, `target/sb/panes-plan.md` §2.1e): the task's output, a
//! preflight's, the tests' detail (the failure picked, else the output) — each a read-only `Document` of the
//! session's (`Session.outputs`), never saved, never in a buffer list or restore. Its text follows the run (`sync`,
//! every event): what it printed appended, one transaction a batch; trimmed (`task::Run::keep`), the one change
//! between the two texts — positions, marks and the view's top follow it; another run or failure, made anew. The
//! code's drawer draws it (`term::draw_output`), its lines coloured as the panels coloured them (`Document.paint`).
//!
//! With the keys (`At::Slot` on its panel) it's normal mode on that document — the editor's moves, selections,
//! search, counts, `z`, yank — with the panel's own keys (`[run]`) laid over it. It refuses edits and says so: a key
//! that changed its text is put back (`Document::put_back`), one that went into insert mode back to normal. `C-w`
//! and `:` are the editor's, as from every panel; the commands of `EDITORS` (the slot's tabs, `]x`, the palette …)
//! run on the editor once the key is done. It holds still while it has the keys — what's printed comes in below
//! the rows in sight, said in its head — and `G` catches up.
//!
//! The code keeps `doc()` (§2.3): each key's commands run with the output's document swapped in as `current`
//! (`docpane::swap` — the pattern every document pane shares) and put back before the key's handler returns —
//! frames, jobs, servers and `context()` see the code's. A key whose command took the code to another buffer
//! (`C-o`, `ga`, `gf`) gives the code the keys.

use std::borrow::Cow;

use crate::docpane::{After, Pane, Which};
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::MappableCommand;
use crate::movement as mv;
use crate::selection::{Range, Selection};
use crate::session::At;
use crate::sizes::Slot;
use crate::transaction::{Assoc, Change, Transaction};

/// What a run panel's document is of: the task's output, a preflight's, the tests' detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Run {
    Task,
    Preflight,
    Tests,
}

impl Run {
    /// Its name in the state (`output:tests`) and on the status line (`● normal · tests`).
    pub fn name(self) -> &'static str {
        match self {
            Run::Task => "task",
            Run::Preflight => "preflight",
            Run::Tests => "tests",
        }
    }

    pub fn slot(self) -> Slot {
        match self {
            Run::Tests => Slot::Test,
            Run::Task | Run::Preflight => Slot::Task,
        }
    }

    /// The run line it waits in (`queued.rs`).
    fn line(self) -> crate::queued::Slot {
        match self {
            Run::Tests => crate::queued::Slot::Tests,
            Run::Task | Run::Preflight => crate::queued::Slot::Task,
        }
    }
}

/// The commands a key on an output runs on the editor, its document put back first: the slot's and the panel's,
/// the failures' and the list's (`]x`, `]q` — the tests' detail is made again of the failure picked), the chat's,
/// the inbox, the palette — and every command of the window table (`Keymaps::in_window_table`). They act on the
/// session, never on the text in sight.
pub const EDITORS: &[&str] = &[
    "slot_next",
    "slot_prev",
    "wclose",
    "run_where",
    "preflight_output",
    "goto_next_test_failure",
    "goto_prev_test_failure",
    "goto_next_quickfix",
    "goto_prev_quickfix",
    "chat_open",
    "inbox",
    "command_palette",
];

/// What a run panel shows, as its document holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Made {
    generation: u64,
    /// The tests' failure picked (its index) — None: the output.
    picked: Option<usize>,
    /// Nothing printed yet: its one line is the command, faint, as the panel shows it.
    waiting: bool,
    /// A preflight's: its steps' headings recede.
    preflight: bool,
    /// How many of its run's trims it has (`task::Trims::count`) — more: replayed on it, line for line (a
    /// failure's message: none apply).
    trimmed: usize,
    /// The run's lines the document holds.
    lines: usize,
}

impl Made {
    /// The same thing shown — printed on, or trimmed, it's the same document.
    fn same(self, o: Made) -> bool {
        (self.generation, self.picked, self.waiting, self.preflight)
            == (o.generation, o.picked, o.waiting, o.preflight)
    }
}

/// A line of a run panel's, and how it's coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The run's output (its stderr flag).
    Out(bool),
    /// A failure's message: its first line in the error colour.
    Message { first: bool },
    /// The command, nothing printed yet; a failure with no message.
    Faint,
}

/// How an output line reads (`term::output_style` draws it so): passing, failing, a warning, stderr, plain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Pass,
    Fail,
    Warn,
    Err,
    Plain,
}

/// A line of a test's or task's output: passing in green, failing (and errors) in red, a warning bold, stderr faint.
pub fn class(err: bool, l: &str) -> Class {
    let l = l.trim_start();
    let passing = l.starts_with("test result: ok")
        || l.starts_with("BUILD SUCCESS")
        || (l.starts_with("test ") && l.ends_with(" ok"))
        || l.starts_with("--- PASS")
        || l.starts_with("ok ")
        || l == "PASS"
        || l.starts_with("Finished")
        || (l.starts_with("===") && l.contains(" passed") && !l.contains(" failed"));
    let failing = !passing
        && (l.contains("FAILED")
            || l.contains("panicked at")
            || l.starts_with("--- FAIL")
            || l.starts_with("FAIL")
            || l.starts_with("E ")
            || l.starts_with("error")
            || l.contains(": error")
            || (l.contains(" failed") && !l.contains(" 0 failed")));
    let warning = !passing && !failing && (l.starts_with("warning") || l.contains(": warning"));
    match (passing, failing, warning, err) {
        (true, ..) => Class::Pass,
        (_, true, ..) => Class::Fail,
        (_, _, true, _) => Class::Warn,
        (.., true) => Class::Err,
        _ => Class::Plain,
    }
}

/// A preflight step's heading's name (`── fmt ──`).
pub fn heading(l: &str) -> Option<&str> {
    l.strip_prefix("── ").and_then(|l| l.strip_suffix(" ──"))
}

/// A run panel's document pane (its document, keys' state, search — `docpane::Pane`) and what it was made of.
pub struct Output {
    pub pane: Pane,
    made: Made,
    /// Its panel had the keys at the last `sync` — another now: they came (landed in sight) or went.
    keyed: bool,
    /// Its lines when the keys came, or `G` caught up — more since: what came waits below (`came`).
    seen: usize,
    /// Its cursor was put on a line by a group's list (`show_line`, the link) since the keys were last here: where
    /// they land, line 0 too (a fresh document's cursor there is no place).
    placed: bool,
}

impl std::ops::Deref for Output {
    type Target = Pane;
    fn deref(&self) -> &Pane {
        &self.pane
    }
}

impl std::ops::DerefMut for Output {
    fn deref_mut(&mut self) -> &mut Pane {
        &mut self.pane
    }
}

impl Output {
    /// Something more came since the keys came or `G` (`↓ output after` in its head).
    pub fn came(&self) -> bool {
        self.keyed && self.lines() > self.seen
    }

    /// The tests' detail shows a failure's message (None: the output).
    pub fn failure(&self) -> bool {
        self.made.picked.is_some()
    }
}

/// The session's two: the task panel's (a task, a preflight) and the tests'.
#[derive(Default)]
pub struct Outputs {
    task: Option<Output>,
    tests: Option<Output>,
}

impl Outputs {
    pub fn get(&self, slot: Slot) -> Option<&Output> {
        match slot {
            Slot::Task => self.task.as_ref(),
            Slot::Test => self.tests.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn get_mut(&mut self, slot: Slot) -> Option<&mut Output> {
        match slot {
            Slot::Task => self.task.as_mut(),
            Slot::Test => self.tests.as_mut(),
            _ => None,
        }
    }

    fn place(&mut self, slot: Slot) -> &mut Option<Output> {
        match slot {
            Slot::Test => &mut self.tests,
            _ => &mut self.task,
        }
    }

    /// Its document, while it's in (not swapped in for a key).
    pub fn doc(&self, slot: Slot) -> Option<&Document> {
        self.get(slot).and_then(|o| o.doc_in())
    }
}

/// A key's command runs with an output's document as `current` (`commands.rs` asks before it acts on the panel).
pub fn swapped(ed: &Editor) -> bool {
    matches!(crate::docpane::swapped(ed), Some(Which::Output(_)))
}

/// What the task panel's run is: a preflight's while the preflight's run is it.
fn task_run(ed: &Editor) -> Option<Run> {
    let r = ed.session.task_run.as_ref()?;
    Some(match ed.session.preflight.as_ref().is_some_and(|p| p.generation == r.generation()) {
        true => Run::Preflight,
        false => Run::Task,
    })
}

/// What the panel `slot` is of, while it has a run.
pub fn run_of(ed: &Editor, slot: Slot) -> Option<Run> {
    match slot {
        Slot::Task => task_run(ed),
        Slot::Test => ed.session.test_run.as_ref().map(|_| Run::Tests),
        _ => None,
    }
}

/// The run panel whose document has the keys now: tarae's keys on the slot, in front, a run panel with its
/// document (the tests' detail — the list beside it is a pane of its own, `At::List`).
pub fn has_keys(ed: &Editor) -> Option<Slot> {
    if !ed.session.is_at(At::Slot) {
        return None;
    }
    let slot = ed.slot().filter(|s| matches!(s, Slot::Task | Slot::Test))?;
    ed.session.outputs.get(slot).map(|_| slot)
}

/// The output with the keys, and what it's of.
pub fn keyed(ed: &Editor) -> Option<(Run, &Output)> {
    let slot = has_keys(ed)?;
    Some((run_of(ed, slot)?, ed.session.outputs.get(slot)?))
}

/// The rows a run panel shows its output (the tests' detail) in, as the last frame drew it (not drawn yet: ten).
pub fn rows(ed: &Editor, slot: Slot) -> usize {
    let p = match slot {
        Slot::Test => ed.screen.test_panel.get(),
        _ => ed.screen.task_panel.get(),
    };
    p.map_or(10, |p| p.2).max(1)
}

/// The rows and columns its text has, as the last frame drew it.
fn viewport(ed: &Editor, slot: Slot) -> (usize, usize) {
    let cols = ed.screen.run_rows.get().filter(|r| r.0 == slot).map_or(60, |_| ed.screen.run_cols.get());
    (rows(ed, slot), cols.max(1))
}

/// What `slot`'s document should hold now.
fn made(ed: &Editor, slot: Slot) -> Option<Made> {
    match slot {
        Slot::Test => {
            let r = ed.session.test_run.as_ref()?;
            let picked = r.picked().map(|_| r.selected);
            let lines = r.picked().map_or(r.output.len(), |c| c.message.len().max(1));
            let waiting = picked.is_none() && r.output.is_empty();
            let trimmed = r.trims.count();
            Some(Made { generation: r.generation(), picked, waiting, preflight: false, trimmed, lines })
        }
        _ => {
            let r = ed.session.task_run.as_ref()?;
            Some(Made {
                generation: r.generation(),
                picked: None,
                waiting: r.output.is_empty(),
                preflight: task_run(ed) == Some(Run::Preflight),
                trimmed: r.trims().count(),
                lines: r.output.len(),
            })
        }
    }
}

/// `slot`'s lines from the run's line `from` on: the task's output; the tests' detail — the failure picked's
/// message, else the output; nothing printed yet, the command (a command of several lines: its first and `…`, as
/// the panel shows it).
fn lines(ed: &Editor, slot: Slot, from: usize) -> Vec<(Kind, Cow<'_, str>)> {
    fn out(o: &crate::runout::Output, from: usize) -> Vec<(Kind, Cow<'_, str>)> {
        o.iter_from(from).map(|(e, l)| (Kind::Out(e), l)).collect()
    }
    match slot {
        Slot::Test => {
            let Some(r) = ed.session.test_run.as_ref() else { return Vec::new() };
            match r.picked() {
                Some(c) if c.message.is_empty() => vec![(Kind::Faint, "failed (no message)".into())],
                Some(c) => (c.message.iter().enumerate().skip(from))
                    .map(|(i, m)| (Kind::Message { first: i == 0 }, m.as_str().into()))
                    .collect(),
                None if r.output.is_empty() => Vec::new(),
                None => out(&r.output, from),
            }
        }
        _ => ed.session.task_run.as_ref().map_or_else(Vec::new, |r| out(&r.output, from)),
    }
}

/// The command a run panel shows while nothing is printed.
fn command(ed: &Editor, slot: Slot) -> String {
    match slot {
        Slot::Test => ed.session.test_run.as_ref().map_or_else(String::new, |r| r.target.argv().join(" ")),
        _ => {
            let cmd = ed.session.task_run.as_ref().map_or("", |r| r.task.cmd.as_str());
            let mut lines = cmd.lines();
            let first = lines.next().unwrap_or_default();
            if lines.next().is_some() { format!("{first} …") } else { first.to_string() }
        }
    }
}

/// The text and colours of `lines`, from byte `at` on: a line each as printed (a tab stays one — drawn as the
/// code's are), each ending in its line break; its colours as global capture ids (`Document.paint`).
fn text_of(lines: &[(Kind, Cow<'_, str>)], at: usize, preflight: bool) -> (String, Vec<(usize, usize, u32)>) {
    let text = lines.iter().flat_map(|(_, l)| [l.as_ref(), "\n"]).collect();
    (text, paint_of(lines.iter().map(|(k, l)| (*k, Cow::Borrowed(l.as_ref()))), at, preflight))
}

/// The colours of `lines` laid out as `text_of` lays them, from byte `at` on — one line at a time, no text made.
fn paint_of<'a>(
    lines: impl Iterator<Item = (Kind, Cow<'a, str>)>,
    at: usize,
    preflight: bool,
) -> Vec<(usize, usize, u32)> {
    use crate::syntax::capture_id;
    let mut paint = Vec::new();
    let mut from = at;
    for (kind, l) in lines {
        let line: &str = &l;
        let end = from + line.len();
        let whole = |c: &str| (from < end).then(|| (from, end, capture_id(c)));
        match kind {
            Kind::Out(_) if preflight && let Some(name) = heading(line) => {
                // (a step's heading recedes, its name dim)
                let (a, b) = (from + "── ".len(), from + "── ".len() + name.len());
                paint.push((from, a, capture_id("ui.linenr")));
                paint.push((a, b, capture_id("ui.virtual")));
                paint.push((b, end, capture_id("ui.linenr")));
            }
            Kind::Out(err) => paint.extend(match class(err, line) {
                Class::Pass => whole("diff.plus"),
                Class::Fail => whole("error"),
                Class::Warn => whole("markup.bold"),
                Class::Err => whole("ui.linenr"),
                Class::Plain => None,
            }),
            Kind::Message { first: true } => paint.extend(whole("error")),
            Kind::Message { first: false } => {}
            Kind::Faint => paint.extend(whole("ui.linenr")),
        }
        from = end + 1;
    }
    paint
}

/// The run's output `slot`'s document holds as it is — its own rope (`share`): not a failure's message, nor the
/// command while nothing is printed.
fn shared(ed: &Editor, slot: Slot, m: Made) -> Option<&crate::runout::Output> {
    if m.picked.is_some() || m.waiting {
        return None;
    }
    match slot {
        Slot::Test => ed.session.test_run.as_ref().map(|r| &r.output),
        _ => ed.session.task_run.as_ref().map(|r| &r.output),
    }
}

/// `slot`'s whole text and colours as its run holds it now — a run's output its own rope from the start, nodes
/// shared: a long output is never copied first (the freed copy's pages stayed in the footprint).
fn whole(ed: &Editor, slot: Slot, m: Made) -> (ropey::Rope, Vec<(usize, usize, u32)>) {
    if let Some(o) = shared(ed, slot, m) {
        let lines = o.iter().map(|(e, l)| (Kind::Out(e), l));
        return (o.text().clone(), paint_of(lines, 0, m.preflight));
    }
    let (text, paint) = match m.waiting {
        true => text_of(&[(Kind::Faint, command(ed, slot).into())], 0, false),
        false => text_of(&lines(ed, slot, 0), 0, m.preflight),
    };
    (ropey::Rope::from_str(&text), paint)
}

/// Every event (`focus_sync`) and before a frame: each run panel's document follows its run, and its keys coming
/// or going are taken in. Never inside a key's command (its document swapped in): after it.
pub fn sync(ed: &mut Editor) {
    if swapped(ed) {
        return;
    }
    for slot in [Slot::Task, Slot::Test] {
        follow(ed, slot);
        keys_moved(ed, slot);
    }
}

/// The run's lines a document holding `lines` of them holds after `trims` (`Trim::Drain` past its end takes what
/// came after it — never in it).
fn after_trims(mut lines: usize, trims: &[crate::task::Trim]) -> usize {
    use crate::task::Trim;
    for t in trims {
        match *t {
            Trim::Insert(at) if at <= lines => lines += 1,
            Trim::Drain(from, to) => lines -= to.min(lines).saturating_sub(from),
            _ => {}
        }
    }
    lines
}

/// `trims` replayed on `d`, which holds `lines` of the run's lines — one transaction each, line for line, so its
/// selections, marks, colours and top follow what stays. Its top's position, followed.
fn replay(d: &mut Document, mut lines: usize, trims: &[crate::task::Trim], mut top: usize) -> usize {
    use crate::task::Trim;
    let start = |d: &Document, at: usize, lines: usize| match at < lines {
        true => mv::line_start(&d.text, at),
        false => d.text.len_bytes(),
    };
    for t in trims {
        let change = match t {
            Trim::Insert(at) if *at <= lines => {
                lines += 1;
                Change::insert(start(d, *at, lines - 1), "\n")
            }
            Trim::Drain(from, to) if *from < (*to).min(lines) => {
                let to = (*to).min(lines);
                let c = Change::delete(start(d, *from, lines), start(d, to, lines));
                lines -= to - from;
                c
            }
            Trim::Set(at, text) if *at < lines => {
                let (a, b) = (mv::line_start(&d.text, *at), mv::line_end(&d.text, *at));
                Change { from: a, to: b, insert: text.clone() }
            }
            _ => continue,
        };
        let tx = Transaction::new(vec![change]);
        top = tx.map_pos(top, Assoc::Before);
        d.apply(&tx);
    }
    top
}

/// `slot`'s document as its run is now: printed on — appended; trimmed — the run's trims replayed on it, line
/// for line (what you read stays where it is, the view's top on its line); another run, another failure picked, the
/// first lines printed — made anew. No run: gone (freed on a worker).
fn follow(ed: &mut Editor, slot: Slot) {
    let Some(m) = made(ed, slot) else {
        if let Some(o) = ed.session.outputs.place(slot).take() {
            crate::event::drop_later(o.pane.into_doc());
        }
        return;
    };
    let was = ed.session.outputs.get(slot).map(|o| o.made);
    // (the run's trims after the document's first `from` — None: some forgotten, made anew; a failure's message has
    // none)
    let trims = |ed: &Editor, from: usize| {
        let t = match slot {
            Slot::Task => ed.session.task_run.as_ref().map(|r| r.trims()),
            _ => ed.session.test_run.as_ref().filter(|_| m.picked.is_none()).map(|r| &r.trims),
        };
        t.map_or(Some(Vec::new()), |t| t.since(from).map(<[_]>::to_vec))
    };
    let (held, trims) = match was.filter(|w| w.same(m) && m.trimmed >= w.trimmed) {
        Some(w) => trims(ed, w.trimmed).map_or((None, Vec::new()), |t| (Some(w), t)),
        None => (None, Vec::new()),
    };
    // (the document has them all now, one way or the other: the run keeps none of them)
    let forget = |ed: &mut Editor| match slot {
        Slot::Task => ed.session.task_run.as_mut().map(|r| r.trims_mut().forget(m.trimmed)),
        _ => ed.session.test_run.as_mut().map(|r| r.trims.forget(m.trimmed)),
    };
    forget(ed);
    match held {
        Some(w) if w == m => {}
        Some(w) if after_trims(w.lines, &trims) <= m.lines => {
            let kept = after_trims(w.lines, &trims);
            let (text, paint) = text_of(&lines(ed, slot, kept), 0, m.preflight);
            let o = ed.session.outputs.get_mut(slot).expect("an output, above");
            let d = o.doc_mut();
            if !trims.is_empty() {
                let top = mv::line_start(&d.text, d.top.min(mv::last_line(&d.text)));
                let top = replay(d, w.lines, &trims, top);
                let top = mv::line_of(&d.text, top);
                d.set_top(top);
            }
            let end = d.text.len_bytes();
            // (the colours of what came: from where it goes — after the trims)
            let paint = paint.into_iter().map(|(a, b, c)| (a + end, b + end, c));
            // (a point at the end — an empty output's — stays where it is: what came goes after it)
            let sel = d.selection().clone();
            d.apply_with(&Transaction::new(vec![Change::insert(end, text)]), sel);
            d.paint.extend(paint);
            o.made = m;
            // (what you were reading may have gone with it: the cursor where it went, kept in sight; held still
            // otherwise, the run's scroll with it)
            match (o.keyed, trims.is_empty()) {
                (true, false) => in_sight(ed, slot),
                (true, true) => pin(ed, slot),
                _ => {}
            }
        }
        _ => {
            let (text, paint) = whole(ed, slot, m);
            let mut doc = Document::from_str(ed.alloc_id(), "");
            doc.text = text;
            doc.wrap = Some(false);
            doc.throwaway = true;
            doc.paint = paint;
            let place = ed.session.outputs.place(slot);
            match place.as_mut() {
                Some(o) => {
                    // (its search stays the pane's; the keys land on it again, as they came)
                    crate::event::drop_later(o.pane.replace(doc));
                    (o.made, o.keyed, o.placed) = (m, false, false);
                }
                None => {
                    *place =
                        Some(Output { pane: Pane::new(doc), made: m, keyed: false, seen: 0, placed: false })
                }
            }
        }
    }
    share(ed, slot, m);
}

/// A document of the run's output holds its lines as the run keeps them (`runout.rs`): its text is the run's own
/// rope, nodes shared — one copy of a long output, not two. (The same text by how it's made — its length checked, a
/// slip left unshared rather than shown wrong.)
fn share(ed: &mut Editor, slot: Slot, m: Made) {
    let Some(rope) = shared(ed, slot, m).map(|o| o.text().clone()) else { return };
    if let Some(d) = ed.session.outputs.get_mut(slot).and_then(|o| o.doc_in_mut())
        && d.text.len_bytes() == rope.len_bytes()
    {
        d.text = rope;
    }
}

/// The keys came to `slot`'s document: the cursor stays where it was when it's in sight, else lands there — a
/// failure's first line, else the output's last row in sight — the rows as the panel showed them. They went: the
/// panel shows the rows you read (at the end, it follows its output again), a prompt of its goes, its `v` ends.
fn keys_moved(ed: &mut Editor, slot: Slot) {
    let keys = has_keys(ed) == Some(slot);
    let rows = rows(ed, slot);
    let so = ed.config.scrolloff.min((rows - 1) / 2);
    let (shown, running) = match slot {
        Slot::Test => (
            ed.session.test_run.as_ref().map(|r| r.detail_top(rows)),
            ed.session.test_run.as_ref().is_some_and(|r| r.state == crate::testing::RunState::Running),
        ),
        _ => (
            ed.session.task_run.as_ref().map(|r| r.top(rows)),
            ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running),
        ),
    };
    let Some(o) = ed.session.outputs.get_mut(slot).filter(|o| o.keyed != keys) else { return };
    o.keyed = keys;
    if !keys {
        let top = o.doc().top;
        if o.pane.dropped() {
            ed.prompt = None;
        }
        match slot {
            Slot::Test => ed.session.test_run.as_mut().map(|r| r.scroll_to(top, rows)),
            _ => ed.session.task_run.as_mut().map(|r| r.scroll_to(top, rows)),
        };
        return;
    }
    o.seen = o.lines();
    let placed = std::mem::take(&mut o.placed);
    let (top, failure) = (shown.unwrap_or(0), o.failure());
    let d = o.doc_mut();
    let last = mv::last_line(&d.text);
    let top = top.min(last);
    d.set_top(top);
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    // (in sight — a step's heading the link put at the top too: the rows as they were, the margin kept from the
    // next move on; on line 0 only where the link put it — a fresh document's cursor is there by default)
    if (top..top + rows).contains(&line) && (line > 0 || placed) {
        return sight(ed, slot, 0);
    }
    let line = match () {
        _ if failure => top,
        // (its last row in sight — inside the scrolloff margin, the view as it was, but at an ended run's end; one
        // still printing puts lines below, and the first key would scroll to keep the margin)
        _ if top + rows > last && !running => last,
        _ => (top + rows - 1 - so).max(top),
    };
    d.set_selection(Selection::point(mv::line_start(&d.text, line.min(last))));
    in_sight(ed, slot);
}

/// The cursor's line in sight, the scrolloff margin kept (one line a row: nothing wraps), and its column.
fn in_sight(ed: &mut Editor, slot: Slot) {
    sight(ed, slot, ed.config.scrolloff);
}

/// `in_sight` with a margin of `so` lines.
fn sight(ed: &mut Editor, slot: Slot, so: usize) {
    let size = viewport(ed, slot);
    let tab = ed.config.tab_width;
    let Some(o) = ed.session.outputs.get_mut(slot) else { return };
    o.size = Some(size);
    let Some(d) = o.doc_in_mut() else { return };
    crate::docpane::in_sight(d, size, so, tab);
    pin(ed, slot);
}

/// While it has the keys the panel's scroll is its document's top — its lines are the run's, line for line: what
/// `o` reads as the step in view, where it's left; at the end, it follows its run again.
fn pin(ed: &mut Editor, slot: Slot) {
    let rows = rows(ed, slot);
    let Some(top) = ed.session.outputs.doc(slot).map(|d| d.top) else { return };
    match slot {
        Slot::Test => ed.session.test_run.as_mut().map(|r| r.scroll_to(top, rows)),
        _ => ed.session.task_run.as_mut().map(|r| r.scroll_to(top, rows)),
    };
}

/// After a frame (`term::render`): the output with the keys laid out for another size than its panel drew before
/// (`C-w z`, the panel growing with its run) kept in sight at it — true when that moved it (drawn again). Only
/// then: what its run printed meanwhile moves nothing (held still).
pub fn resized(ed: &mut Editor) -> bool {
    let Some(slot) = has_keys(ed) else { return false };
    let size = viewport(ed, slot);
    if ed.session.outputs.get(slot).is_none_or(|o| o.size == Some(size)) {
        return false;
    }
    let at = |ed: &Editor| ed.session.outputs.doc(slot).map(|d| (d.top, d.left));
    let before = at(ed);
    in_sight(ed, slot);
    at(ed) != before
}

/// Before drawing: the matches its search shows — the pattern its `/` prompt is typing, else its last search
/// while shown.
pub fn refresh_search(ed: &mut Editor) {
    let typed = ed.prompt.as_ref().filter(|p| matches!(p.kind, crate::editor::PromptKind::Search { .. }));
    let typed = typed.map(|p| p.text.clone());
    let Some(slot) = has_keys(ed) else { return };
    if let Some(o) = ed.session.outputs.get_mut(slot) {
        o.pane.refresh_search(typed);
    }
}

/// `esc` with its keys: its matches no longer shown (the code's stay as they are).
pub fn search_unshown(ed: &mut Editor) {
    if let Some(o) = has_keys(ed).and_then(|s| ed.session.outputs.get_mut(s)) {
        o.keys.search_hl = false;
    }
}

/// Runs `f` with `slot`'s document swapped in (`docpane::swap`): an edit or typing put back is said (it's
/// read-only), the cursor kept in sight after.
fn swap<R>(ed: &mut Editor, slot: Slot, f: impl FnOnce(&mut Editor) -> R) -> Option<R> {
    let size = viewport(ed, slot);
    let (r, done) = crate::docpane::swap(ed, Which::Output(slot), size, f)?;
    if done.refused() {
        let what = run_of(ed, slot).map_or("this output", what);
        let out = bound(ed, "view_leave", "esc").map(|k| format!(" — {k} code")).unwrap_or_default();
        ed.set_status(format!("{what} is read-only{out}"));
    }
    in_sight(ed, slot);
    Some(r)
}

/// What an output is, in a sentence.
fn what(r: Run) -> &'static str {
    match r {
        Run::Task => "the task's output",
        Run::Preflight => "the preflight's output",
        Run::Tests => "the tests' output",
    }
}

/// Whether `key` is `C-w` (or `A-C-w`) or `:`: the editor's, from an output as from every panel.
fn passes(key: Key) -> bool {
    (key.ctrl && key.code == Code::Char('w')) || key.plain_char() == Some(':')
}

/// The keys an output with nothing under way lets go on down the chain (`keys_to`'s layer).
pub fn passed() -> Vec<Key> {
    let ctrl = Key { code: Code::Char('w'), ctrl: true, alt: false, shift: false };
    vec![ctrl, Key { alt: true, ..ctrl }, Key::plain(Code::Char(':'))]
}

/// A key while an output has them: its prompt's (`/`), else normal mode's with `[run]` over it, on its document.
/// `C-w`, `:` and a sequence of the editor's under way go on. Returns the handler that took it.
pub fn key(ed: &mut Editor, key: Key) -> Option<crate::uistate::Handler> {
    use crate::uistate::Handler;
    let slot = has_keys(ed)?;
    if !ed.pending.is_empty() {
        return None;
    }
    let o = ed.session.outputs.get(slot)?;
    if ed.prompt.is_some() {
        if !o.prompt {
            return None;
        }
        swap(ed, slot, |ed| ed.handle_prompt_key(key));
        return Some(Handler::Prompt);
    }
    if o.keys.idle() && passes(key) {
        return None;
    }
    swap(ed, slot, |ed| {
        crate::docpane::key_in(ed, key, |ed, mode, keys| ed.keymaps.run_lookup(mode, keys), editors)
    });
    after(ed, slot);
    Some(Handler::Output)
}

/// A command a key on an output runs on the editor (`EDITORS`, the window table's, a `:` line bound to a key).
fn editors(ed: &Editor, c: &MappableCommand) -> bool {
    match c {
        MappableCommand::Static(s) => EDITORS.contains(&s.name) || ed.keymaps.in_window_table(s.name),
        MappableCommand::Typed(_) => true,
    }
}

/// What a command asked for, once its key is done.
fn after(ed: &mut Editor, slot: Slot) {
    let Some(o) = ed.session.outputs.get_mut(slot) else { return };
    let Some((a, deferred)) = o.take_after() else { return };
    match a {
        After::Leave => ed.session.leave(At::Slot),
        After::Latest => {
            if let Some(o) = ed.session.outputs.get_mut(slot) {
                o.seen = o.lines();
                let d = o.doc_mut();
                let last = mv::last_line(&d.text);
                d.set_selection(Selection::point(mv::line_start(&d.text, last)));
            }
            in_sight(ed, slot);
        }
        After::Take => take(ed, slot),
        // (your own run waiting behind Claude's there, now — Claude's stopping, `queued.rs`; none waits: the chat's
        // input, to ask about what's read)
        After::Enter => {
            let line = run_of(ed, slot).map_or(crate::queued::Slot::Task, Run::line);
            if !crate::queued::run_now(ed, line) {
                ed.session.leave(At::Slot);
                crate::chat::open(ed);
            }
        }
        // (`i` isn't bound on an output — normal mode's, refused — but bound by yours: the chat's input)
        After::Input => {
            ed.session.leave(At::Slot);
            crate::chat::open(ed);
        }
        After::ChatView => crate::chatdoc::view(ed),
        After::Editor => ed.run(&deferred, None),
    }
}

/// `enter` (`space >`) on a selection: each range a quote over the chat's input (`quote.rs`), the input with the
/// keys — tagged with what ran (the task's name, the tests' target, the failure the detail is of) and its lines.
fn take(ed: &mut Editor, slot: Slot) {
    let Some(run) = run_of(ed, slot) else { return };
    let Some(d) = ed.session.outputs.doc(slot) else { return };
    let (from, title, case) = match run {
        Run::Tests => {
            let r = ed.session.test_run.as_ref();
            let case = r.and_then(|r| r.picked().map(|c| c.name.clone()));
            ("test", r.map_or_else(String::new, |r| r.target.label.clone()), case)
        }
        _ => {
            let name = ed.session.task_run.as_ref().map_or_else(String::new, |r| r.task.name.clone());
            (if run == Run::Preflight { "preflight" } else { "task" }, name, None)
        }
    };
    let what = match (run, &case) {
        (Run::Tests, Some(_)) => "failure",
        (Run::Tests, None) => "test output",
        (Run::Preflight, _) => "preflight",
        _ => "task output",
    };
    let total = mv::last_line(&d.text) + 1;
    let quote = |r: &Range| {
        use crate::quote::{Taken, lines, text_of};
        // (an output's lines are its own: none wrapped, none cut)
        let (text, l0, l1) = text_of(&d.text, (r.from(), r.to()), &|_| (0, None), &|_| None);
        let text = text.trim_start_matches('\n').trim_end().to_string();
        if text.trim().is_empty() {
            return None;
        }
        let n = lines(text.lines().count());
        let mut attrs = vec![("from", from.to_string()), ("title", title.clone())];
        attrs.extend(case.clone().map(|c| ("test", c)));
        attrs.push(("lines", format!("{}-{} of {total}", l0 + 1, l1 + 1)));
        Some(Taken { attrs, label: format!("{what} {n}"), text, redact: true, code: None })
    };
    // (only the ranges wider than a character: a plain cursor beside them quotes nothing)
    let wide = |r: &&Range| crate::graphemes::next_boundary(&d.text, r.from()) < r.to();
    let taken: Vec<_> = d.selection().ranges().iter().filter(wide).filter_map(quote).collect();
    if taken.is_empty() {
        return ed.set_status("only blanks are selected — nothing to quote");
    }
    crate::quote::add(ed, taken);
}

/// A run panel's `K`: where Claude's command in it ran, and why — from its run's record (cardburden 4b).
pub fn run_where(ed: &mut Editor) {
    let ran = match has_keys(ed).and_then(|s| run_of(ed, s)) {
        Some(Run::Task | Run::Preflight) => ed
            .session
            .task_run
            .as_ref()
            .and_then(|r| Some((crate::task::traced(&r.task.cmd), r.ran.clone()?))),
        _ => None,
    };
    match ran {
        Some((cmd, ran)) => crate::chat::say_ran(ed, &cmd, &ran),
        None if has_keys(ed).is_none() => {
            ed.set_status("K says where Claude's command ran — on the task panel, C-w j")
        }
        None => ed.set_status("K says where a command of Claude's ran — this run isn't one"),
    }
}

/// The key an output's command is bound to, as a hint names it: `like` when that's one of them, else the shortest
/// — None when nothing is (a remap took it away).
pub fn bound(ed: &Editor, name: &str, like: &str) -> Option<String> {
    let keys = ed.keymaps.run_keys_for(name);
    let k = keys.iter().find(|k| k.as_str() == like).or(keys.first())?;
    Some(if k == "ret" { "enter".into() } else { k.clone() })
}

/// Whether `enter` quotes on the output with the keys now: something selected.
pub fn quotes(ed: &Editor) -> bool {
    keyed(ed).is_some_and(|(_, o)| crate::docpane::wide(o.doc()))
}

/// Whether `enter` runs your run waiting behind on the output with the keys (nothing selected).
pub fn run_waits(ed: &Editor) -> bool {
    keyed(ed).is_some_and(|(r, _)| crate::queued::mine_waits(ed, r.line()))
}

/// A prompt the output with the keys opened (`/`, `?`, `s`) is up: its word on the status line under it.
pub fn prompt_from(ed: &Editor) -> Option<Run> {
    keyed(ed).filter(|(_, o)| o.prompt && ed.prompt.is_some()).map(|(r, _)| r)
}

/// The wheel over a run panel's output: three rows of its document while it has the keys, the cursor kept in
/// sight (`C-e`/`C-y` thrice); without them the panel scrolls (back at the end, it follows its output again).
pub fn wheel(ed: &mut Editor, slot: Slot, up: bool) {
    if has_keys(ed) == Some(slot) {
        let by = if up { -3 } else { 3 };
        swap(ed, slot, |ed| ed.scroll_view(by, false));
        return;
    }
    let rows = rows(ed, slot);
    let by = if up { -3 } else { 3 };
    match slot {
        Slot::Test => ed.session.test_run.as_mut().map(|r| r.scroll_by(by, rows)),
        _ => ed.session.task_run.as_mut().map(|r| r.scroll_by(by, rows)),
    };
}

/// A press on a run panel's row `row` of its output at column `col` of its text: the keys its, the cursor on that
/// character, as a click in the code puts it.
pub fn click(ed: &mut Editor, slot: Slot, row: usize, col: usize) {
    ed.bottom_take_keys();
    let Some(d) = ed.session.outputs.get_mut(slot).and_then(|o| o.doc_in_mut()) else { return };
    let line = (d.top + row).min(mv::last_line(&d.text));
    let (ls, le) = (mv::line_start(&d.text, line), mv::line_end(&d.text, line));
    let text = d.text.byte_slice(ls..le).to_string();
    // (the character whose cells hold the column — display width, a wide one's second cell its own)
    let (mut w, mut at) = (0, 0);
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true) {
        let gw = unicode_width::UnicodeWidthStr::width(g);
        if w + gw > col + d.left {
            break;
        }
        (w, at) = (w + gw, at + g.len());
    }
    // (in sight already: a press inside the scrolloff margin doesn't move the rows under the pointer)
    d.set_selection(Selection::point(crate::graphemes::snap(&d.text, (ls + at).min(le))));
}

/// A preflight step's chip: the task panel's keys, its document from that step's heading at the top, the cursor on
/// it — said when the step hasn't printed yet.
pub fn step(ed: &mut Editor, name: &str) {
    let heading = format!("── {name} ──");
    let at = ed.session.task_run.as_ref().and_then(|r| r.output.iter().position(|l| l.1 == heading));
    let Some(at) = at else { return ed.set_status("that step hasn't printed anything yet") };
    ed.bottom_take_keys();
    show_line(ed, at, true);
}

/// The task panel's document at its line `at`, the cursor there — at the top (`top`: a step's heading, as `zt` puts
/// it), else with the scrolloff margin above it (a place's line, what printed it before it in sight) — and the
/// panel scrolled there. Where the keys are doesn't change: a group's list keeps them (the link, `grouplist.rs`).
pub fn show_line(ed: &mut Editor, at: usize, top: bool) {
    let rows = rows(ed, Slot::Task);
    let so = ed.config.scrolloff.min((rows - 1) / 2);
    let Some(o) = ed.session.outputs.get_mut(Slot::Task) else { return };
    // (for the keys coming — with them here already, it's where they are)
    o.placed = !o.keyed;
    let Some(d) = o.doc_in_mut() else { return };
    let last = mv::last_line(&d.text);
    let at = at.min(last);
    d.set_selection(Selection::point(mv::line_start(&d.text, at)));
    // (the next move keeps the scrolloff margin)
    let want = if top { at } else { at.saturating_sub(so) };
    let top = want.min((last + 1).saturating_sub(rows));
    d.set_top(top);
    if let Some(r) = ed.session.task_run.as_mut() {
        r.scroll_to(top, rows);
    }
}

/// The line of `slot`'s document its cursor is on.
pub fn cursor_line(ed: &Editor, slot: Slot) -> Option<usize> {
    let d = ed.session.outputs.doc(slot)?;
    Some(mv::line_of(&d.text, d.selection().primary().cursor(&d.text)))
}

/// The state's `view` while an output has the keys: what it's of, the cursor's line, selections, whether more came
/// since — shapes, never text.
pub fn state(ed: &Editor) -> Option<serde_json::Value> {
    let (run, o) = keyed(ed)?;
    let d = o.doc_in()?;
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    Some(serde_json::json!({
        "surface": run.name(),
        "line": line + 1,
        "lines": o.lines(),
        "selections": d.selection().ranges().len(),
        "selects": o.selects(),
        "mode": if o.keys.mode == Mode::Select { "select" } else { "normal" },
        "held": o.came(),
    }))
}

#[cfg(test)]
mod tests;
