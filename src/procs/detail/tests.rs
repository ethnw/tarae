use std::time::{Duration, SystemTime};

use super::*;

const MB: u64 = 1 << 20;

/// A running row: `pid` under `ppid`, started at epoch second 1,000, up a minute, 1.5 % of a core, 30 MB.
fn row(pid: u32, ppid: u32, kind: Kind, title: &str) -> Row {
    Row {
        pid,
        ppid,
        started: 1_000,
        depth: 0,
        under: kind.clone(),
        kind,
        title: title.into(),
        what: String::new(),
        program: title.into(),
        up: Some(Duration::from_secs(60)),
        cpu: Some(1.5),
        worked: Duration::from_millis(2_500),
        rss: 40 * MB,
        footprint: Some(30 * MB),
        zombie: false,
        flagged: false,
        session: None,
        left: false,
        command: Some(format!("/usr/bin/{title} --flag")),
        cwd: None,
        gone: None,
    }
}

/// tarae (100) › terminal (200) › zsh (300) › cargo (400).
fn tree() -> Vec<Row> {
    vec![
        Row { what: "this editor".into(), ..row(100, 1, Kind::Tarae, "tarae") },
        Row { what: "zsh".into(), ..row(200, 100, Kind::Terminal, "terminal") },
        Row { depth: 1, ..row(300, 200, Kind::Other, "zsh") },
        Row { depth: 2, ..row(400, 300, Kind::Other, "cargo") },
    ]
}

fn said<'a>(lines: &'a [Line], label: &str) -> Option<&'a Line> {
    lines.iter().find(|l| l.label == label)
}

fn check(pid: u32, what: &str, ended: bool) -> Check {
    Check {
        key: format!("k:{what}"),
        pid,
        what: what.into(),
        short: what.into(),
        did: "nothing yet".into(),
        at: SystemTime::UNIX_EPOCH,
        ended: ended.then_some(SystemTime::UNIX_EPOCH),
    }
}

#[test]
fn a_process_is_under_its_parents_up_to_tarae_and_names_what_it_started() {
    let rows = tree();
    let cargo = lines(&rows[3], &rows, &[], None, 80);
    let under = said(&cargo, "under").expect("its parents");
    assert_eq!(under.text, "tarae 100 › terminal 200 › zsh 300", "tarae first, its parent last");
    assert_eq!(said(&cargo, "command").unwrap().text, "/usr/bin/cargo --flag");
    assert_eq!(said(&cargo, "for").unwrap().text, "started by what it's under");
    assert!(said(&cargo, "started").is_none(), "it started nothing");
    let zsh = lines(&rows[2], &rows, &[], None, 80);
    assert_eq!(said(&zsh, "started").unwrap().text, "cargo 400");
    assert_eq!(said(&zsh, "under").unwrap().text, "tarae 100 › terminal 200");
    let terminal = lines(&rows[1], &rows, &[], None, 80);
    assert_eq!(said(&terminal, "for").unwrap().text, "the terminal · zsh", "what it is, then what for");
    // tarae: under nothing of its own; what it started
    let tarae = lines(&rows[0], &rows, &[], None, 80);
    assert!(said(&tarae, "under").is_none());
    assert_eq!(said(&tarae, "for").unwrap().text, "this editor");
    assert_eq!(said(&tarae, "started").unwrap().text, "terminal 200");
    // A parent none of the rows is: its pid, said dim
    let stray = row(500, 7, Kind::Other, "stray");
    let l = lines(&stray, &rows, &[], None, 80);
    let under = said(&l, "under").unwrap();
    assert_eq!((under.text.as_str(), under.tone), ("pid 7 — not one of tarae's", Tone::Dim));
}

#[test]
fn a_parent_chain_that_loops_ends() {
    // (pids reused so that two name each other: the walk stops at the rows' count)
    let rows = vec![row(10, 20, Kind::Other, "a"), row(20, 10, Kind::Other, "b")];
    let l = lines(&rows[0], &rows, &[], None, 80);
    assert_eq!(said(&l, "under").unwrap().text.matches('›').count(), 1, "{l:?}");
}

#[test]
fn a_gone_process_is_said_gone_and_keeps_what_it_was() {
    let mut rows = tree();
    let gone = Row { gone: Some(SystemTime::UNIX_EPOCH), ..rows[3].clone() };
    rows[3] = gone.clone();
    let l = lines(&gone, &rows, &[], None, 80);
    assert_eq!(l[0].label, "gone", "said first: {l:?}");
    assert_eq!(l[0].tone, Tone::Warn);
    assert!(l[0].text.starts_with("it has ended — not seen since "), "{l:?}");
    assert_eq!(said(&l, "command").unwrap().text, "/usr/bin/cargo --flag", "its command line as it was");
    assert_eq!(said(&l, "under").unwrap().text, "tarae 100 › terminal 200 › zsh 300");
    assert!(said(&l, "cpu").unwrap().text.starts_with('—'), "no CPU now: {l:?}");
    // Its parent no longer names it among what it started
    let zsh = lines(&rows[2], &rows, &[], None, 80);
    assert!(said(&zsh, "started").is_none(), "{zsh:?}");
}

#[test]
fn the_command_line_is_whole_wrapped_under_its_label() {
    let long = "/usr/local/bin/rust-analyzer --log-file /tmp/ra.log --some-very-long-option-name-without-breaks-at-all-x";
    let r = Row { command: Some(long.into()), ..row(42, 100, Kind::Other, "ra") };
    let width = 40;
    let l = lines(&r, &tree(), &[], None, width);
    let at = l.iter().position(|l| l.label == "command").unwrap();
    let parts: Vec<&Line> =
        l[at..].iter().take_while(|x| x.label == "command" || x.label.is_empty()).collect();
    assert!(parts.len() > 2, "wrapped: {parts:?}");
    assert!(parts.iter().all(|p| p.text.width() <= width - LABEL_W), "{parts:?}");
    assert!(parts[1..].iter().all(|p| p.label.is_empty()), "its rest under the label's column");
    let whole: String = parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join(" ");
    assert_eq!(whole.replace(' ', ""), long.replace(' ', ""), "nothing of it left out");
    // A path too long for a line breaks after a `/`
    let dir = "/Users/someone/.cache/tarae/runs/2812-0/532/tarae-scene-39988-0/tarae-demo";
    let r = Row { cwd: Some(dir.into()), ..row(42, 100, Kind::Other, "ra") };
    let l = lines(&r, &tree(), &[], None, 60);
    let at = l.iter().position(|l| l.label == "folder").unwrap();
    assert!(l[at].text.ends_with('/') && l[at + 1].label.is_empty(), "{:?}", &l[at..at + 2]);
    assert_eq!(format!("{}{}", l[at].text, l[at + 1].text), crate::allow::tilde(std::path::Path::new(dir)));
}

#[test]
fn memory_says_the_most_since_opened_and_the_checks_about_it() {
    let rows = tree();
    let r = &rows[1];
    let l = lines(r, &rows, &[], Some(20 * MB), 80);
    assert_eq!(
        said(&l, "memory").unwrap().text,
        "30 MB footprint · 40 MB resident",
        "a peak under now: unsaid"
    );
    let l = lines(r, &rows, &[], Some(90 * MB), 80);
    assert_eq!(
        said(&l, "memory").unwrap().text,
        "30 MB footprint · 40 MB resident · 90 MB the most since opened"
    );
    assert_eq!(said(&l, "cpu").unwrap().text, "1.5% of a core now · 2.5s in all");
    let checks =
        [check(200, "terminal busy", false), check(300, "another's", false), check(200, "big", true)];
    let l = lines(r, &rows, &checks, None, 80);
    let about: Vec<(&str, Tone)> =
        l.iter().filter(|l| l.label == "check").map(|l| (l.text.as_str(), l.tone)).collect();
    assert_eq!(about.len(), 2, "its own only, newest first: {about:?}");
    assert!(about[0].0.starts_with("big — ended ") && about[0].1 == Tone::Dim, "{about:?}");
    assert_eq!(about[1], ("▲ terminal busy — nothing yet", Tone::Warn));
}

#[test]
fn the_text_lays_labels_in_their_column_faint() {
    let lines = [
        Line { label: "for", text: "a task".into(), tone: Tone::Plain },
        Line { label: "", text: "more".into(), tone: Tone::Warn },
    ];
    let (text, paint) = text(&lines);
    assert_eq!(text, "for      a task\n         more\n");
    use crate::syntax::capture_id;
    assert_eq!(paint, vec![(0, 3, capture_id("ui.linenr")), (25, 29, capture_id("warning"))]);
}
