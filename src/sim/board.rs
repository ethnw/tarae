//! The board (`space v C`) and the one reviewer, simulated (detsim P1): a review of one change, its reviewer
//! asked at random for a board or the one pass as the keys ask (`codereview::ask_board`/`ask_claude`: the review
//! opened first, refused while one reads; a board's card asking first, answered by a later step — frictions29 32), its members stand-ins (`Editor.reviewer_stand_in`: no process —
//! a process's reader threads aren't the simulation's; what a member says is the scenario's step) — an angle's
//! claim, a skeptic's verdict, a member's end or failure — among the line's jobs and timers: the memory read
//! before each start, the retry while it's critical, the other angles' stagger, the skeptics the angles' end
//! brings in; `:reviewer-stop`, a review opened, closed or replaced (bgreview: it reads a snapshot of its own),
//! you editing the file it read, a spell of memory pressure, time passing, a
//! frame drawn. After every step: no more members read than may at once, none started past what the memory
//! allowed, a later angle only after the stagger or the first one's end, a reviewer over has none waiting or
//! reading, every claim pinned stands (confirmed or unchecked), one failure one note — a skeptic judging any claim
//! of its group, at times as the same failure as another; no review step moved a member; and a finding placed
//! with no edit of yours between its pin and its placing sits where the snapshot's text is (its lines as read),
//! one on lines changed since marked so — every one inside its file.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use super::sessions::git;
use super::{Choices, Sim};
use crate::board::Verdict;
use crate::editor::Editor;
use crate::llm::StreamEv;
use crate::reviewer::{Role, State};

/// The seeds every `cargo test` runs, each twice — the first to catch each break the scenario keeps (picked again
/// by bgreview's "tarae native", 2026-10-20, `target/sb/bgreview-seeds-all.sh`: every run reads its snapshot now,
/// the reading copy's coin gone, so every seed's choices moved — frictions32's `f32-board-seeds.sh` and
/// reviewpanel's before it): 0 a board started without its card · 3 a refuted claim pinned, no stagger · 6 a
/// review closed stopping its reviewer · 7 a stop that leaves members waiting, the line ignoring the memory · 18 a
/// finding not placed on the lines now · 169 a same failure pinned apart (its skeptic's `same_as` landing on a
/// claim it confirmed — none of the first 168 catch it).
const SEEDS: &[u64] = &[0, 3, 6, 7, 18, 169];

/// The scenario's own steps a run (the jobs' and results' come between them).
const INPUTS: usize = 40;

/// A run's real time at most.
const LIMIT: Duration = crate::test_results::PATIENCE;

/// The board's members reading at once at most (`review.board-at-once`): fewer than its angles, so the line
/// holds some.
const AT_ONCE: usize = 2;

/// The other angles' wait for the first one's prompt, as shipped (`reviewer::STAGGER` is none in tests).
const STAGGER: Duration = Duration::from_secs(3);

/// The memory as the line reads it (`Editor.reviewer_room` is a plain function): under pressure until a time
/// on the simulated clock — a spell, never for good: while it's critical the line retries for as long, and a
/// run must go quiet after its last input.
static PRESSURE: Mutex<Option<(crate::share::Pressure, Instant)>> = Mutex::new(None);

/// What the run saw so far (`held`): the members' generations seen reading; each finding's step pinned and
/// whether it was checked placed; the steps you edited the file at; a review step that moved a member.
#[derive(Default)]
struct Seen {
    started: BTreeSet<u64>,
    pinned: std::collections::BTreeMap<u64, (usize, bool)>,
    edits: Vec<usize>,
    moved: Option<String>,
    step: usize,
}

fn room() -> crate::share::Room {
    let now = crate::clock::now();
    let spell = *PRESSURE.lock().unwrap_or_else(|e| e.into_inner());
    let pressure = spell.filter(|s| now < s.1).map_or(crate::share::Pressure::Normal, |s| s.0);
    crate::share::Room { pressure, headroom: None }
}

#[test]
fn the_board_and_its_line() {
    super::seeds("the_board_and_its_line", SEEDS, run);
}

/// One run of the scenario; its trace.
fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-board");
    let repo = dir.join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
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
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    *PRESSURE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut sim = Sim::new("the_board_and_its_line", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (repo.clone(), "proj".into());
    // (half the runs in a conversation whose board was said yes to already: boards start at once, as before the
    // card, and reach their skeptics as often; the other half ask on it first — frictions32)
    if sim.choices.below(2) == 0 {
        ed.session.board_yes = Some(None);
    }
    (ed.reviewer_stand_in, ed.reviewer_room) = (true, room);
    ed.config.review_board_at_once = AT_ONCE;
    // (as shipped, not the tests' none: the other angles wait for the first one's prompt — a timer stepped)
    ed.reviewer_stagger = STAGGER;
    let (shown, seen) = (dir.display().to_string(), std::cell::RefCell::new(Seen::default()));
    let check = |ed: &Editor| held(ed, &repo, &mut seen.borrow_mut());
    let step = |sim: &mut Sim, ed: &mut Editor| input(sim, ed, &repo, &mut seen.borrow_mut());
    sim.walk(&mut ed, INPUTS, step, check, |ed| print(ed, &shown));
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// One of the scenario's steps, said.
fn input(sim: &mut Sim, ed: &mut Editor, repo: &std::path::Path, seen: &mut Seen) -> String {
    let n = sim.steps;
    seen.step = n;
    let c = &mut sim.choices;
    // (by arm: a board or its card's answer 3, the one pass 1, a member says something 12, one ends 6, one fails 1,
    // a stop 1, a review opened, closed or replaced 2, the memory 1, time passing 2, a frame 1, you editing the file
    // 2 — members speak most, so claims reach skeptics; a board takes two steps since its card (frictions32), so it
    // weighs three; with none reading, a second passes instead: the stagger and the line move on)
    let none = |sim: &mut Sim| {
        sim.wait(Duration::from_secs(1));
        "(none reads) 1000 ms pass".to_string()
    };
    match c.weighted(&[3, 1, 12, 6, 1, 1, 2, 1, 2, 1, 2]) {
        // (as the keys do: the review opened first if it isn't, refused while its reviewer reads — a board's card
        // asks first, its cost read on a worker; up, it's answered: mostly yes, at times a key as the user presses it
        // — refused until the card's drawn and settled — or no)
        0 => match ed.offers.first().is_some_and(|o| matches!(o.what, crate::offer::What::Board(_))) {
            true => match c.below(4) {
                0 => {
                    ed.offer_dismiss();
                    "n on the board's card".into()
                }
                1 => {
                    let taken = ed.offer_key(crate::key::Key::plain(crate::key::Code::Char('y')));
                    format!("y pressed on the board's card ({})", if taken { "taken" } else { "not taken" })
                }
                _ => {
                    ed.offer_accept();
                    "y on the board's card".into()
                }
            },
            false => {
                crate::codereview::ask_board(ed, None);
                "space v C".into()
            }
        },
        1 => {
            crate::codereview::ask_claude(ed, None);
            "space v c".into()
        }
        2 => {
            let Some((i, generation, role)) = reading(ed, c) else { return none(sim) };
            let id = format!("r{n}");
            let (tool, args) = match role {
                // (any claim of its group, at times the same failure as another — refused unless that one was
                // confirmed first, as `judge` says)
                Role::Skeptic(g) => {
                    let b = ed.session.reviewer.as_ref().and_then(|r| r.board.as_deref());
                    let group = b.and_then(|b| b.groups.get(g)).cloned().unwrap_or_default();
                    let k = group.len().max(1);
                    let claim = 1 + c.below(k);
                    let verdict = *c.pick(&["confirmed", "confirmed", "refuted"]);
                    let mut args = json!({"claim": claim, "verdict": verdict, "trace": "a.txt:2 reads it"});
                    // (the same failure as one it confirmed already, mostly — at times any claim, refused then)
                    let confirmed: Vec<usize> = (0..group.len())
                        .filter(|&n| {
                            b.is_some_and(|b| b.claims[group[n]].verdict == Some(Verdict::Confirmed))
                        })
                        .collect();
                    match c.below(4) {
                        0 | 1 if !confirmed.is_empty() => args["same_as"] = json!(1 + *c.pick(&confirmed)),
                        2 if k > 1 => args["same_as"] = json!(1 + c.below(k)),
                        _ => {}
                    }
                    ("judge", args)
                }
                _ => {
                    let line = 1 + c.below(3) as u64;
                    ("note", json!({"path": "a.txt", "line": line, "text": format!("Line {line} is wrong")}))
                }
            };
            let message = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call",
                "params": {"name": tool, "arguments": args}});
            let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": message});
            crate::reviewer::on_event(ed, generation, StreamEv::Control { request_id: id, request });
            format!("{} {tool}s", member(ed, i))
        }
        k @ (3 | 4) => {
            let Some((i, generation, _)) = reading(ed, c) else { return none(sim) };
            let who = member(ed, i);
            let end = if k == 3 { Ok("Done.".to_string()) } else { Err("claude: it ended".to_string()) };
            crate::reviewer::on_event(ed, generation, StreamEv::Done(end));
            format!("{who} {}", if k == 3 { "ends" } else { "fails" })
        }
        5 => {
            crate::reviewer::stop(ed);
            ":reviewer-stop".into()
        }
        // (bgreview: nothing a review does starts or stops a member — checked at once, the step's own part)
        6 => {
            let states = |ed: &Editor| {
                ed.session
                    .reviewer
                    .as_ref()
                    .map(|r| r.members.iter().map(|m| m.state.clone()).collect::<Vec<_>>())
            };
            let before = states(ed);
            let said = match c.below(3) {
                0 => {
                    crate::codereview::close(ed);
                    "space v q"
                }
                1 => {
                    crate::codereview::start_in(ed, repo.to_path_buf(), None);
                    "space v v"
                }
                _ => {
                    crate::codereview::start_in(ed, repo.to_path_buf(), Some("main".into()));
                    ":review main"
                }
            };
            if states(ed) != before {
                seen.moved = Some(format!("{said} moved a member: {before:?} → {:?}", states(ed)));
            }
            said.into()
        }
        7 => {
            use crate::share::Pressure;
            let (p, word) = *c.pick(&[(Pressure::Warn, "warn"), (Pressure::Critical, "critical")]);
            // (a test's retry comes each 30 ms, four steps: a spell past ~10 s outlasts the quiet a run must reach)
            let secs = *c.pick(&[1, 2, 5]);
            let until = crate::clock::now() + Duration::from_secs(secs);
            *PRESSURE.lock().unwrap_or_else(|e| e.into_inner()) = Some((p, until));
            format!("memory {word} for {secs} s")
        }
        8 => {
            let ms = *c.pick(&[100, 1000, 3000, 31000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        9 => {
            crate::term::render(ed, &mut Vec::new(), 120, 30).unwrap();
            "a frame".into()
        }
        // (you go on with the file it read: a line put in above, or one changed — never fewer lines)
        10 => {
            let path = repo.join("a.txt");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let k = c.below(lines.len().max(1));
            let (now, said) = match c.below(2) {
                0 => (format!("zero\n{text}"), "a line put in at the top".to_string()),
                _ => {
                    let mut l: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
                    if let Some(x) = l.get_mut(k) {
                        x.push('!');
                    }
                    (l.join("\n") + "\n", format!("line {} changed", k + 1))
                }
            };
            super::write(&path, now);
            seen.edits.push(n);
            format!("you edit a.txt: {said}")
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// A member reading, picked: its index, its events' generation, its role.
fn reading(ed: &Editor, c: &mut Choices) -> Option<(usize, u64, Role)> {
    let r = ed.session.reviewer.as_ref()?;
    let at: Vec<usize> = (0..r.members.len()).filter(|&i| r.members[i].state == State::Reading).collect();
    let i = *c.pick(at.as_slice().get(..).filter(|a| !a.is_empty())?);
    Some((i, r.members[i].generation, r.members[i].role))
}

fn member(ed: &Editor, i: usize) -> String {
    ed.session.reviewer.as_ref().map_or_else(String::new, |r| r.member_name(i))
}

/// What holds after every step: no more members read than may at once — and a member that started in it
/// started within what the memory allowed, as the line last read it (`started`: the generations seen reading
/// so far; warn: one at a time, critical: none); a reviewer over has none waiting or reading; every claim
/// pinned stands; no review step moved a member; a finding placed is inside its file, and — no edit of yours
/// between its pin and its placing — sits on its lines as read, or is marked changed since.
fn held(ed: &Editor, repo: &std::path::Path, seen: &mut Seen) -> Result<(), String> {
    use crate::share::Pressure;
    if let Some(m) = seen.moved.take() {
        return Err(m);
    }
    placed(ed, repo, seen)?;
    let started = &mut seen.started;
    // (the key that spends money: no board reads without a yes on its card — frictions29 32)
    if ed.session.reviewer.as_ref().is_some_and(|r| r.board.is_some()) && ed.session.board_yes.is_none() {
        return Err("a board started with no yes on its card".into());
    }
    let Some(r) = ed.session.reviewer.as_ref() else { return Ok(()) };
    let reading = r.members.iter().filter(|m| m.state == State::Reading).count();
    if reading > AT_ONCE {
        return Err(format!("{reading} members read at once, past {AT_ONCE}"));
    }
    let new: Vec<u64> = r
        .members
        .iter()
        .filter(|m| m.state == State::Reading && !started.contains(&m.generation))
        .map(|m| m.generation)
        .collect();
    let allowed = match ed.reviewer_pressure {
        Pressure::Critical => 0,
        Pressure::Warn => 1,
        Pressure::Normal | Pressure::Unknown => AT_ONCE,
    };
    if !new.is_empty() && reading > allowed {
        return Err(format!(
            "a member started with {reading} reading, the memory {:?}",
            ed.reviewer_pressure
        ));
    }
    started.extend(new);
    // (a board's later angle starts once the first one's prompt is in the cache: the stagger gone by since the
    // first started, or the first over — a stand-in's first request is never answered)
    let first = r.members.first().filter(|m| m.role == Role::Angle(0));
    let first_end = first.and_then(|f| f.started.zip(f.took)).map(|(s, t)| s + t);
    for m in r.members.iter().filter(|m| matches!(m.role, Role::Angle(a) if a > 0)) {
        let (Some(s), Some(f)) = (m.started, first.and_then(|f| f.started)) else { continue };
        if s < f + STAGGER && first_end.is_none_or(|e| e > s) {
            return Err(format!(
                "an angle started {} ms after the first, its prompt not cached",
                (s - f).as_millis()
            ));
        }
    }
    if !r.reading()
        && let Some(m) = r.members.iter().find(|m| !m.over())
    {
        return Err(format!("the reviewer is {:?}, a member still {:?}", r.state, m.state));
    }
    let pinned = r.board.as_deref().map_or(&[][..], |b| &b.claims[..]).iter().filter(|c| c.note.is_some());
    if let Some(c) =
        pinned.clone().find(|c| !matches!(c.verdict, Some(Verdict::Confirmed | Verdict::Unchecked)))
    {
        return Err(format!("a claim pinned {:?}: {}", c.verdict, c.text));
    }
    // (a claim its skeptic called the same failure as another is that one's note, never one of its own: every
    // note the board pinned is a claim's, and a claim's is the one it folds into)
    if let Some(b) = r.board.as_deref() {
        let notes: BTreeSet<u64> = b.claims.iter().filter_map(|c| c.note).collect();
        if let Some(id) = r.findings.iter().find(|id| !notes.contains(id)) {
            return Err(format!("note#{id} pinned for no claim — a same failure pinned apart"));
        }
        let apart = |i: usize| b.claims[b.root(i)].note.is_some_and(|n| b.claims[i].note != Some(n));
        if let Some(i) = (0..b.claims.len()).find(|&i| b.claims[i].note.is_some() && apart(i)) {
            return Err(format!("claim {i} pinned apart from the one it's the same failure as"));
        }
    }
    Ok(())
}

/// The findings' places (`held`): each inside a.txt as it is now; one first seen placed with no edit since it
/// was pinned sits on lines whose text is the snapshot's (its digest, `pinned`) — unless placed as changed
/// since read.
fn placed(ed: &Editor, repo: &std::path::Path, seen: &mut Seen) -> Result<(), String> {
    // (the file as tarae has it: its open document's text — a review opened it — else what's on disk)
    let path = repo.join("a.txt");
    let text = match ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path()) && !d.loading) {
        Some(d) => d.text.clone(),
        None => ropey::Rope::from_str(&std::fs::read_to_string(&path).unwrap_or_default()),
    };
    for n in crate::reviewer::findings_in(&ed.session).unwrap_or_default() {
        let step = seen.step;
        let (at, checked) = *seen.pinned.entry(n.id).or_insert((step, false));
        if n.placing || checked {
            continue;
        }
        seen.pinned.insert(n.id, (at, true));
        if n.lines.end > text.len_lines().max(1) {
            return Err(format!("note#{} at {:?}, past a.txt's {} lines", n.id, n.lines, text.len_lines()));
        }
        // (an edit of yours since it was pinned: where it was read is no longer where the text is — not judged)
        if seen.edits.iter().any(|&e| e >= at) || n.path != repo.join("a.txt") {
            continue;
        }
        // (one said changed since sits in a change's region — a line of it may read as it did: not judged)
        let same = crate::notes::lines_digest(&text, &n.lines) == n.pinned;
        let changed = crate::notes::changed_since_read(ed, n.id);
        if n.pinned.is_some() && !same && !changed {
            return Err(format!(
                "note#{} at {:?} ({:?} now): its text as read {same}, said changed since {changed} — snapshot {:?}",
                n.id,
                n.lines,
                text.get_line(n.lines.start).map(|l| l.to_string()),
                ed.session.reviewer.as_ref().map(|r| r.snap.is_some()),
            ));
        }
    }
    Ok(())
}

/// What a trace line ends with: the reviewer's state, each member's, the claims, the last thing said.
fn print(ed: &Editor, dir: &str) -> String {
    let last = ed.messages.last().map(|m| m.2.replace(dir, "<dir>")).unwrap_or_default();
    let up = ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Board(_)));
    let card = if up { " · the board's card up" } else { "" };
    let Some(r) = ed.session.reviewer.as_ref() else { return format!("no reviewer · said {last:?}{card}") };
    let word = |s: &State| match s {
        State::Waiting => "waits".to_string(),
        State::Reading => "reads".to_string(),
        State::Done => "done".to_string(),
        State::Failed(why) => format!("failed ({why})"),
        State::Stopped => "stopped".to_string(),
    };
    let members: Vec<String> =
        (0..r.members.len()).map(|i| format!("{} {}", r.member_name(i), word(&r.members[i].state))).collect();
    let claims = r.board.as_deref().map_or(0, |b| b.claims.len());
    format!(
        "{} {} [{}] · {claims} claims · {} findings · said {last:?}{card}",
        if r.board.is_some() { "board" } else { "reviewer" },
        word(&r.state),
        members.join(", "),
        r.findings.len()
    )
}
