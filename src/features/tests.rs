use super::*;
use crate::config::Config;
use crate::key::{Code, Key};

/// An editor not counting (the default), its project the temp folder — never this repository (its git).
fn editor() -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.session.root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    ed
}

/// An editor counting (`features.count` on), saving nowhere until a test gives it a path.
fn counting() -> Editor {
    let mut ed = editor();
    ed.config.features_count = true;
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed
}

/// Feeds "jj" as keys; `<down>`, `<ret>`, `<space>` are named keys.
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

fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).expect("event before timeout");
        ed.handle_event(ev);
    }
}

fn count_of(ed: &Editor, key: &str) -> u32 {
    ed.features.pending().iter().find(|(k, _)| k == key).map_or(0, |(_, c)| c.n)
}

fn temp(name: &str) -> PathBuf {
    crate::scratch::Scratch::new(&format!("features-{name}")).into_path()
}

/// What a key counts is the command it ran: `j` and `↓` are two keys of one command, one row with both —
/// and nothing names a key.
#[test]
fn a_key_counts_its_command_not_its_key() {
    let mut ed = counting();
    ed.docs[0].text = ropey::Rope::from_str("a\nb\nc\nd\n");
    let j = ed.keymaps.lookup(crate::editor::Mode::Normal, &[Key::plain(Code::Char('j'))]);
    let crate::keymap::Lookup::Matched(cmds) = j else { panic!("j is bound") };
    let crate::keymap::MappableCommand::Static(cmd) = cmds[0] else { panic!("j runs a command") };
    feed(&mut ed, "jj<down>");
    let pending = ed.features.pending();
    assert_eq!(count_of(&ed, &format!("command:{}", cmd.name)), 3, "{pending:?}");
    assert_eq!(pending.len(), 1, "one command, one row: {pending:?}");
}

/// `:vs` and `:vsplit` are one command, counted by its first name; a line naming nothing tarae has isn't
/// counted at all (it could be anything typed).
#[test]
fn a_typed_command_counts_by_its_name_aliases_as_one() {
    let mut ed = counting();
    feed(&mut ed, ":vs<ret>:vsplit<ret>:no-such-thing here<ret>");
    let name = typed_name("vs").expect("vs is an alias");
    assert_eq!(name, typed_name("vsplit").unwrap());
    assert_eq!(count_of(&ed, &format!("typed:{name}")), 2, "{:?}", ed.features.pending());
    assert!(!ed.features.pending().iter().any(|(k, _)| k.contains("no-such")));
}

/// A card is counted when asked, its answer by the card's kind and letter; a side panel tab when shown, a
/// picker when opened — its kind, never its title.
#[test]
fn cards_panels_and_pickers_count_what_they_are() {
    let mut ed = counting();
    feed(&mut ed, ":features-reset<ret>n");
    assert_eq!(count_of(&ed, "card:features-reset"), 1, "{:?}", ed.features.pending());
    assert_eq!(count_of(&ed, "answer:features-reset n"), 1, "{:?}", ed.features.pending());
    assert!(ed.offers.is_empty(), "n answered it");
    feed(&mut ed, "<space>e");
    assert_eq!(count_of(&ed, "panel:files"), 1);
    // (shown again: closing it isn't a showing)
    feed(&mut ed, "<space>e");
    assert_eq!(count_of(&ed, "panel:files"), 1);
    feed(&mut ed, "<esc><space>b");
    assert_eq!(count_of(&ed, "picker:buffers"), 1, "{:?}", ed.features.pending());
}

/// A card asked again while it's still up only refreshes the one there — asked once, not twice (else "asked"
/// drifts above its answers).
#[test]
fn a_card_asked_again_while_up_counts_once() {
    let mut ed = counting();
    ed.features_reset();
    ed.features_reset();
    assert_eq!(ed.offers.len(), 1, "the same card");
    assert_eq!(count_of(&ed, "card:features-reset"), 1, "{:?}", ed.features.pending());
}

/// `y` on the reset's card empties what's counted here and removes the file (on a worker).
#[test]
fn the_reset_empties_the_counts_and_the_file() {
    let dir = temp("reset");
    let path = dir.join(FILE);
    save(&path, &[("typed:write".into(), Count { n: 3, last: 9 })], 9).unwrap();
    let mut ed = counting();
    ed.features.path = Some(path.clone());
    feed(&mut ed, "jj:features-reset<ret>y");
    assert!(ed.offers.iter().all(|o| o.kind() != "features-reset" || o.installing()), "answered");
    assert!(ed.features.pending().is_empty(), "{:?}", ed.features.pending());
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while path.exists() {
        assert!(std::time::Instant::now() < end, "removed on a worker");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A `:` line bound to a key counts as that `:` command — by its name, not the key's line.
#[test]
fn a_typed_command_bound_to_a_key_counts_by_name() {
    let mut ed = counting();
    let keys: toml::Table = toml::from_str("[normal]\nX = \":vs\"\n").unwrap();
    ed.keymaps.merge_user(&keys, &mut Vec::new());
    feed(&mut ed, "X");
    assert_eq!(count_of(&ed, "typed:vsplit"), 1, "{:?}", ed.features.pending());
}

/// A command chosen in the palette is the palette's count, not a key's.
#[test]
fn the_palette_counts_apart_from_keys() {
    let mut ed = counting();
    feed(&mut ed, "<space>?");
    assert_eq!(ed.picker.as_ref().map(|p| p.kind), Some("commands"));
    let row = ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.action.clone());
    let Some(crate::picker::Action::Command(name)) = row else { panic!("a command first: {row:?}") };
    feed(&mut ed, "<ret>");
    assert_eq!(count_of(&ed, &format!("palette:{name}")), 1, "{:?}", ed.features.pending());
    assert_eq!(count_of(&ed, &format!("command:{name}")), 0, "not a key's");
}

/// Off (the default), nothing is counted, and nothing is written — no file at all.
#[test]
fn off_counts_nothing_and_writes_nothing() {
    let dir = temp("off");
    let mut ed = editor();
    assert!(!ed.config.features_count, "off unless your config turns it on");
    ed.features.path = Some(dir.join(FILE));
    feed(&mut ed, "jj:vs<ret><space>e");
    assert!(ed.features.pending().is_empty(), "{:?}", ed.features.pending());
    ed.features_turn();
    ed.features_flush();
    assert!(!dir.join(FILE).exists());
    // Counted while on, then turned off: forgotten at the next batch, never written
    ed.config.features_count = true;
    feed(&mut ed, "j");
    ed.config.features_count = false;
    ed.features_turn();
    ed.config.features_count = true;
    ed.features_flush();
    assert!(!dir.join(FILE).exists(), "what was counted before it went off isn't written");
    std::fs::remove_dir_all(&dir).ok();
}

/// What's counted goes to disk with the first batch `SAVE_EVERY_S` after the last save — on a worker — and
/// when tarae ends; a batch sooner writes nothing.
#[test]
fn counts_are_saved_with_a_batch_after_a_while_and_at_the_end() {
    let dir = temp("turn");
    let path = dir.join(FILE);
    let mut ed = counting();
    ed.features.path = Some(path.clone());
    feed(&mut ed, "j");
    ed.features_turn();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!path.exists(), "too soon");
    ed.features.saved_s -= SAVE_EVERY_S;
    ed.features_turn();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while read(&path).counts.is_empty() {
        assert!(std::time::Instant::now() < end, "saved on a worker");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(ed.features.pending().is_empty(), "handed over");
    feed(&mut ed, "jj");
    ed.features_flush();
    let saved = read(&path);
    assert_eq!(saved.counts.values().map(|c| c.n).sum::<u32>(), 3, "{saved:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Taraes saving side by side each add theirs: what one saved is never written over by another's (the lock
/// around read-add-write).
#[test]
fn saves_side_by_side_add_up() {
    let dir = temp("together");
    let path = dir.join(FILE);
    let saves = |key: &'static str| {
        let path = path.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                save(&path, &[(key.to_string(), Count { n: 1, last: 20_000 })], 20_000).unwrap();
            }
        })
    };
    let (a, b) = (saves("command:move_line_down"), saves("command:move_line_down"));
    let c = saves("typed:write");
    for t in [a, b, c] {
        t.join().unwrap();
    }
    let saved = read(&path);
    assert_eq!(saved.counts["command:move_line_down"].n, 200, "{saved:?}");
    assert_eq!(saved.counts["typed:write"].n, 100);
    assert_eq!(saved.since, Some(20_000));
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "only yours");
    std::fs::remove_dir_all(&dir).ok();
}

/// A file that isn't counts (hand-edited, cut short) is set aside, not written over — and a line of an area
/// tarae doesn't have isn't read as a feature.
#[test]
fn a_broken_file_is_set_aside_and_strange_lines_left_out() {
    let dir = temp("broken");
    let path = dir.join(FILE);
    std::fs::write(&path, "{\"counts\": ").unwrap();
    save(&path, &[("typed:write".into(), Count { n: 2, last: 9 })], 9).unwrap();
    assert_eq!(std::fs::read_to_string(path.with_extension("json.broken")).unwrap(), "{\"counts\": ");
    assert_eq!(read(&path).counts["typed:write"].n, 2);
    // Broken again: set aside beside the first copy, which stays as it was
    std::fs::write(&path, "[1, 2").unwrap();
    save(&path, &[("typed:write".into(), Count { n: 1, last: 9 })], 9).unwrap();
    assert_eq!(std::fs::read_to_string(path.with_extension("json.broken")).unwrap(), "{\"counts\": ");
    assert_eq!(std::fs::read_to_string(path.with_extension("json.broken-2")).unwrap(), "[1, 2");
    assert_eq!(read(&path).counts["typed:write"].n, 1);
    std::fs::write(&path, r#"{"v":1,"counts":{"typed:write":[2,9],"secret:/home/me/notes.txt":[1,9]}}"#)
        .unwrap();
    assert_eq!(read(&path).counts.keys().collect::<Vec<_>>(), ["typed:write"]);
    remove(&path).unwrap();
    assert!(!path.exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// The report has every feature there is, so one never used shows as 0 — sorted by use, then by name — and a
/// card's answers under it.
#[test]
fn the_report_lists_the_never_used_too() {
    let universe = Universe {
        rows: vec![
            (Area::Command, "move_line_down".into(), Some("j".into())),
            (Area::Command, "move_line_up".into(), Some("k".into())),
            (Area::Command, "goto_file_start".into(), Some("g g".into())),
            (Area::Card, "run".into(), None),
            (Area::Card, "grammars".into(), None),
        ],
    };
    let saved = Saved {
        since: Some(20_000),
        counts: [("command:move_line_up".to_string(), Count { n: 3, last: 20_005 })].into_iter().collect(),
    };
    let pending = [
        ("command:move_line_down".to_string(), Count { n: 5, last: 20_010 }),
        ("card:run".to_string(), Count { n: 4, last: 20_010 }),
        ("answer:run y".to_string(), Count { n: 3, last: 20_010 }),
        ("answer:run n".to_string(), Count { n: 1, last: 20_009 }),
    ];
    let r = Report::new(true, &universe, &saved, &pending, 20_010);
    let names: Vec<(&str, u32)> = r.rows.iter().map(|r| (r.name.as_str(), r.total())).collect();
    assert_eq!(
        names,
        [("move_line_down", 5), ("move_line_up", 3), ("goto_file_start", 0), ("run", 4), ("grammars", 0)]
    );
    assert_eq!(r.rows[3].answers, [("y".to_string(), 3), ("n".to_string(), 1)]);
    let text = r.markdown();
    // (the columns' names are the heading's, its text right over the rows')
    assert!(text.contains("\n##  Commands         by key   palette   last         key\n"), "{text}");
    assert!(text.contains("    move_line_up          3         ·   5 days ago   k\n"), "{text}");
    assert!(
        text.contains("\n##  Cards   asked   last    answered\n    run         4   today   y 3 · n 1\n"),
        "{text}"
    );
    assert!(text.contains("\n##  Never used\n"), "{text}");
    assert!(text.contains("\n### Commands · 1      key\n    goto_file_start   g g\n"), "{text}");
    assert!(text.contains("\n### Cards · 1\n    grammars\n"), "{text}");
}

/// `tarae state --features --json`, pinned (`src/snapshots/features.json`; `TARAE_BLESS=1` rewrites it).
#[test]
fn the_json_is_pinned() {
    let universe = Universe {
        rows: vec![
            (Area::Command, "move_line_down".into(), Some("j".into())),
            (Area::Command, "file_picker".into(), Some("space f".into())),
            (Area::Typed, "write".into(), None),
            (Area::Plugin, "k8s-pods".into(), None),
            (Area::Card, "run".into(), None),
            (Area::Panel, "files".into(), None),
            (Area::Picker, "files".into(), None),
            (Area::Claude, "read".into(), None),
        ],
    };
    let saved = Saved::default();
    let pending = [
        ("command:move_line_down".to_string(), Count { n: 12, last: 20_374 }),
        ("palette:file_picker".to_string(), Count { n: 1, last: 20_373 }),
        ("typed:write".to_string(), Count { n: 4, last: 20_374 }),
        ("card:run".to_string(), Count { n: 2, last: 20_370 }),
        ("answer:run y".to_string(), Count { n: 2, last: 20_370 }),
        ("claude:read".to_string(), Count { n: 30, last: 20_374 }),
        ("claude:other".to_string(), Count { n: 1, last: 20_374 }),
    ];
    let r = Report::new(true, &universe, &saved, &pending, 20_374);
    let fresh = serde_json::to_string_pretty(&r.json()).unwrap() + "\n";
    crate::settings::check_generated(
        "src/snapshots/features.json",
        &fresh,
        "features::tests::the_json_is_pinned",
    );
}

#[test]
fn days_are_the_calendars() {
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(19_723), "2024-01-01");
    assert_eq!(date(19_782), "2024-02-29");
    assert_eq!(date(20_374), "2025-10-13");
    // (local midnight: a day starts at the offset's midnight, not UTC's)
    assert_eq!(day_of(86_400 - 1, 0), 0);
    assert_eq!(day_of(86_400 - 3600, 3600), 1);
    assert_eq!(thousands(1_204), "1,204");
    assert_eq!(thousands(999), "999");
}

/// Every card's kind (`Offer::kind`) is in `KINDS`, so a card never asked shows as 0 — read from `kind`'s code.
#[test]
fn every_card_kind_is_listed() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/offer.rs")).unwrap();
    let body = src.split("pub fn kind(&self) -> &'static str {").nth(1).unwrap();
    let body = &body[..body.find("\n    }\n").unwrap()];
    let kinds: Vec<&str> = body.split("=> \"").skip(1).map(|s| &s[..s.find('"').unwrap()]).collect();
    assert!(kinds.len() > 40, "{kinds:?}");
    for k in &kinds {
        assert!(crate::offer::KINDS.contains(k), "{k} isn't in offer::KINDS");
    }
    assert_eq!(kinds.len(), crate::offer::KINDS.len(), "KINDS lists a kind `kind` doesn't give");
}

/// Every picker's kind (`Picker::of`'s first argument) is in `PICKERS`, and every one listed is opened somewhere.
#[test]
fn every_picker_kind_is_listed() {
    let mut found = Vec::new();
    let mut dirs = vec![PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("features/tests.rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                // (rustfmt may put the arguments on the next line)
                for s in text.split("Picker::of(").skip(1).filter_map(|s| s.trim_start().strip_prefix('"')) {
                    found.push(s[..s.find('"').unwrap()].to_string());
                }
            }
        }
    }
    for k in &found {
        assert!(PICKERS.contains(&k.as_str()), "picker kind {k} isn't in features::PICKERS");
    }
    for k in PICKERS {
        assert!(found.iter().any(|f| f == k), "{k} is listed but no picker is of it");
    }
}

/// Every tool tarae lists for the chat's claude is counted by its own name (not `other`).
#[test]
fn every_tool_claude_has_is_counted_by_name() {
    let mut ed = counting();
    ed.config.llm.edit_tools = crate::edittool::Arm::Both;
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let list = crate::tools::mcp(&mut ed, 0, "r1", &msg).expect("answered");
    let tools = list["result"]["tools"].as_array().expect("a list");
    assert!(tools.len() > 10, "{list}");
    for t in tools {
        let name = t["name"].as_str().unwrap();
        assert_eq!(claude_tool(&format!("mcp__tarae__{name}")), name, "{name} isn't in CLAUDE_TOOLS");
    }
    assert_eq!(claude_tool("Read"), "Read");
    assert_eq!(claude_tool("mcp__tarae__/home/me/secret"), "other", "a made-up name isn't kept");
}

/// Claude's level of the UI state (its `ui` tool, the context's `ui:` line) holds no counts — they aren't part
/// of `UiState` at all.
#[test]
fn claudes_level_has_no_counts() {
    let mut ed = counting();
    feed(&mut ed, "jj<space>e");
    assert!(!ed.features.pending().is_empty());
    let seen = crate::uistate::claude::tool_call(&mut ed).unwrap_or_else(|e| e);
    let state = ed.ui_state().json().to_string();
    for said in [&seen, &state] {
        assert!(!said.contains("move_") && !said.contains("panel:") && !said.contains("features"), "{said}");
    }
}

/// The counter has no way off this machine: nothing in it reaches for the network or runs a program.
#[test]
fn the_counter_has_no_way_out() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/features.rs")).unwrap();
    for word in ["std::net", "TcpStream", "UdpSocket", "UnixStream", "Command::new", "process::", "http"] {
        assert!(!src.contains(word), "features.rs names {word}");
    }
}

/// `:features` while counting is off says so and how to turn it on — and nothing else.
#[test]
fn features_off_says_how_to_turn_it_on() {
    let mut ed = editor();
    feed(&mut ed, ":features<ret>");
    settle_until(&mut ed, |ed| ed.doc().title.as_deref() == Some("features"));
    let text = ed.doc().text.to_string();
    assert!(text.contains("Counting is off") && text.contains("count = true"), "{text}");
    assert!(!text.contains("## "), "no tables while nothing was counted: {text}");
}

/// Performance budget: a key's count is an index and an add — under 5 ns, so the key path doesn't feel it;
/// a batch of events reads the clock once for today — under 200 ns.
#[test]
fn perf_budget_feature_count() {
    if crate::test_results::unoptimized("features::tests::perf_budget_feature_count") {
        return;
    }
    let mut c = Counts::new(None);
    let cmds: Vec<&'static StaticCommand> = crate::commands::COMMANDS.iter().take(64).collect();
    const N: usize = 4_000_000;
    let mut runs: Vec<f64> = (0..9)
        .map(|_| {
            let t = std::time::Instant::now();
            for i in 0..N {
                c.key(std::hint::black_box(cmds[i & 63]));
            }
            t.elapsed().as_secs_f64() * 1e9 / N as f64
        })
        .collect();
    crate::sorting::by(&mut runs, f64::total_cmp);
    let ns = runs[runs.len() / 2];
    let line = format!("PERF feature_count_ns {ns:.2} budget=5 noise=0.5\n");
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    assert!(ns < 5.0, "{ns:.2} ns a count (budget 5)");
    // A batch of events: today read (a clock read), nothing to save yet
    let mut ed = counting();
    const B: usize = 200_000;
    let mut runs: Vec<f64> = (0..9)
        .map(|_| {
            let t = std::time::Instant::now();
            for _ in 0..B {
                ed.features_turn();
            }
            t.elapsed().as_secs_f64() * 1e9 / B as f64
        })
        .collect();
    crate::sorting::by(&mut runs, f64::total_cmp);
    let turn = runs[runs.len() / 2];
    let line = format!("PERF feature_turn_ns {turn:.1} budget=200 noise=10\n");
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    assert!(turn < 200.0, "{turn:.1} ns a batch (budget 200)");
}
