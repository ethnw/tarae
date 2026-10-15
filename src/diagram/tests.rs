use super::*;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Whether the Rust grammar is in this build (debug builds have it) — the tests on rebase.rs skip without it.
fn rust() -> bool {
    crate::syntax::Loader::global().load(crate::syntax::spec("rust").unwrap()).is_ok()
}

fn anchor(at: &str) -> Option<Anchor> {
    Some(Anchor::parse(&format!("src/rebase.rs:{at}")).unwrap())
}

/// The diagram plan's sample (`target/sb/rebase.mmd`): a session's rebase onto its base, its states and transitions
/// tied to src/rebase.rs.
fn sample() -> Diagram {
    let node = |id: &str, label: &str, parent: Option<&str>, at: &str| Node {
        id: id.into(),
        label: label.into(),
        parent: parent.map(Into::into),
        at: anchor(at),
    };
    let edge =
        |from: Option<&str>, to: Option<&str>, within: Option<&str>, label: &str, at: Option<&str>| Edge {
            from: from.map(Into::into),
            to: to.map(Into::into),
            within: within.map(Into::into),
            label: label.into(),
            at: at.and_then(anchor),
        };
    let stopped = Some("stopped");
    Diagram {
        name: "rebase".into(),
        title: "a session's rebase onto its base".into(),
        kind: Kind::State,
        nodes: vec![
            node("moved", "base moved · card", None, "Moved::card"),
            node("checking", "checkout looked at", None, "Editor::rebase_check"),
            node("held", "held · card", None, "Held::card"),
            node("queued", "queued behind a run", None, "Editor::rebase_queued"),
            node("running", "git running · step n of m", None, "Editor::rebase_run"),
            node("stopped", "stopped at a conflict", None, "Stage::Stopped"),
            node("resolving", "Claude resolving", stopped, "Editor::rebase_ask"),
            node("markers", "markers left?", stopped, "turn_over"),
            node("resolved", "resolved · review and card", None, "Editor::rebase_checked"),
            node("done", "done · bring card", None, "Moved::bring_card"),
            node("aborted", "aborted", None, "Ended::Aborted"),
            node("failed", "failed to begin", None, "Ended::Failed"),
            node("adopted", "taken up after a restart", None, "Editor::rebase_adopt"),
        ],
        edges: vec![
            edge(None, Some("moved"), None, "base moved — since the session left it", None),
            edge(Some("moved"), Some("checking"), None, "y — rebase", Some("Editor::rebase_answer")),
            edge(Some("checking"), Some("held"), None, "changes — in no commit, or unsaved buffers", None),
            edge(Some("held"), Some("checking"), None, "s autostash", Some("Editor::rebase_held_stash")),
            edge(Some("checking"), Some("queued"), None, "busy — a run in the task panel", None),
            edge(Some("queued"), Some("checking"), None, "its turn — in the queue", None),
            edge(Some("checking"), Some("running"), None, "clean", Some("Editor::rebase_go")),
            edge(
                Some("running"),
                Some("stopped"),
                None,
                "conflicts — git stopped at them",
                Some("Editor::rebase_ended"),
            ),
            edge(
                Some("running"),
                Some("done"),
                None,
                "done — the branch's commits are on the base's tip",
                None,
            ),
            edge(Some("running"), Some("aborted"), None, "aborted — git rebase --abort ended", None),
            edge(Some("running"), Some("failed"), None, "failed — git stopped before it began", None),
            edge(None, Some("resolving"), stopped, "", None),
            edge(Some("resolving"), Some("markers"), None, "turn over — Claude's turn ended", None),
            edge(
                Some("markers"),
                Some("resolving"),
                None,
                "some left — you ask Claude again",
                Some("Editor::rebase_checked"),
            ),
            edge(Some("markers"), None, stopped, "none left", None),
            edge(Some("stopped"), Some("resolved"), None, "none left — the review opens", None),
            edge(
                Some("resolved"),
                Some("running"),
                None,
                "y continue · X abort — or rebase-continue and rebase-abort by hand",
                Some("Editor::rebase_continue"),
            ),
            edge(None, Some("adopted"), None, "restart — a rebase tarae didn't start", None),
            edge(Some("adopted"), Some("stopped"), None, "", None),
            edge(Some("done"), None, None, "y bring · p preflight — then bring", None),
            edge(Some("aborted"), None, None, "", None),
            edge(Some("failed"), None, None, "", None),
        ],
    }
}

/// rebase.rs as `text` says, every other file unread.
fn reading(text: String) -> impl Fn(&Path) -> Option<String> {
    move |p: &Path| p.ends_with("src/rebase.rs").then(|| text.clone())
}

/// Each anchor by what it's on (`Of`'s node or edge), with its status — for messages that say which.
fn statuses(d: &Diagram, checks: &[(Of, Check)]) -> Vec<(String, Status)> {
    checks
        .iter()
        .map(|(of, c)| {
            let on = match *of {
                Of::Node(i) => d.nodes[i].id.clone(),
                Of::Edge(i) => {
                    let e = &d.edges[i];
                    format!("{}→{}", e.from.as_deref().unwrap_or("[*]"), e.to.as_deref().unwrap_or("[*]"))
                }
            };
            (on, c.status.clone())
        })
        .collect()
}

/// An anchor is a part of a file as the read tool takes one — a symbol or one range of lines — from the project's
/// root and inside it.
#[test]
fn anchors_are_written_as_the_read_tool_takes_a_part() {
    let a = Anchor::parse("src/rebase.rs:Stage::Stopped").unwrap();
    assert_eq!(
        (a.path.as_path(), &a.want),
        (Path::new("src/rebase.rs"), &Want::Symbol("Stage::Stopped".into()))
    );
    assert_eq!(
        Anchor::parse("src/a.rs:3-5").unwrap().want,
        Want::Lines(2..5),
        "from 1, both ends in → from 0"
    );
    assert_eq!(Anchor::parse("src/a.rs:7").unwrap().want, Want::Lines(6..7));
    for bad in ["src/a.rs", "src/a.rs:1-2,4", "src/a.rs:-30", "../b/a.rs:f", "/etc/a.rs:f"] {
        assert!(Anchor::parse(bad).is_err(), "{bad}");
    }
}

/// A label draws as its gist, and not at all when the gist only repeats where the edge goes — as the mockup
/// (`target/sb/diagram-cells-2.txt`) draws the sample.
#[test]
fn an_edge_says_its_gist_and_nothing_its_target_says() {
    assert_eq!(gist("changes — in no commit, or unsaved buffers"), "changes");
    assert_eq!(gist("clean"), "clean");
    let d = sample();
    let says: Vec<&str> = d.edges.iter().map(|e| d.says(e)).collect();
    #[rustfmt::skip]
    let want = [
        "", "y", "changes", "s autostash", "busy", "its turn", "clean",
        "conflicts", "", "", "",
        "", "turn over", "some left", "none left", "none left", "y continue · X abort",
        "restart", "", "y bring · p preflight", "", "",
    ];
    assert_eq!(says, want, "base moved, done, aborted, failed: their targets say it");
}

/// The sample's shape is sound; one with a node twice, an unknown parent or end, a composite inside itself or a
/// start straight to an end says each.
#[test]
fn a_diagrams_shape_is_checked() {
    assert_eq!(sample().problems(), Vec::<String>::new());
    let mut d = sample();
    d.kind = Kind::Flow;
    d.nodes[1].id = "moved".into();
    d.nodes[2].parent = Some("nowhere".into());
    d.nodes[5].parent = Some("resolving".into());
    d.edges[0].to = Some("gone".into());
    d.edges[1].from = None;
    d.edges[1].to = None;
    let p = d.problems();
    for said in [
        "moved: two nodes have this id",
        "held: its parent nowhere is no node",
        "stopped: it's inside itself",
        "[*] → gone: gone is no node",
        "[*] → [*]: from a start straight to an end",
    ] {
        assert!(p.iter().any(|x| x == said), "{said} in {p:#?}");
    }
}

/// The sample drawn on tarae's own src/rebase.rs: every anchor found, its variants too (`Stage::Stopped`,
/// `Ended::Failed` — the outline's members), and checked again as drawn.
#[test]
fn the_rebase_sample_resolves_in_taraes_own_code() {
    if !rust() {
        return;
    }
    let mut d = sample();
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let pinned = d.pin(Path::new(ROOT), read);
    assert_eq!(pinned.len(), 19, "13 states, 6 transitions anchored");
    let not_found: Vec<_> = statuses(&d, &pinned).into_iter().filter(|(_, s)| *s != Status::Same).collect();
    assert!(not_found.is_empty(), "{not_found:#?}");
    assert!(d.anchors().all(|(_, a)| a.pin.is_some()));
    let again = d.check(Path::new(ROOT), read);
    assert!(again.iter().all(|(_, c)| c.status == Status::Same), "{:#?}", statuses(&d, &again));
    // (a variant's lines: the one line of `Stopped,` in `enum Stage`)
    let stopped = again.iter().find(|(of, _)| *of == Of::Node(5)).and_then(|(_, c)| c.lines.clone()).unwrap();
    let text = std::fs::read_to_string(Path::new(ROOT).join("src/rebase.rs")).unwrap();
    assert_eq!(text.lines().nth(stopped.start).map(str::trim), Some("Stopped,"), "{stopped:?}");
}

/// Drawn, then the code changes: an anchor whose code moved says moved (lines too, found by their first line),
/// one whose text changed says changed, one renamed away is gone, a file no longer read leaves every anchor gone —
/// and a name never found, or found twice, says so.
#[test]
fn stale_anchors_say_how() {
    if !rust() {
        return;
    }
    let base = std::fs::read_to_string(Path::new(ROOT).join("src/rebase.rs")).unwrap();
    let stage = base.lines().position(|l| l.starts_with("pub enum Stage")).unwrap();
    let mut d = sample();
    d.nodes.push(Node {
        id: "stage".into(),
        label: "the stages".into(),
        parent: None,
        at: Some(Anchor::parse(&format!("src/rebase.rs:{}-{}", stage + 1, stage + 3)).unwrap()),
    });
    let root = Path::new(ROOT);
    d.pin(root, reading(base.clone()));
    let all = |d: &Diagram, text: Option<String>| -> Vec<(String, Status)> {
        let checks = match text {
            Some(t) => d.check(root, reading(t)),
            None => d.check(root, |_: &Path| None::<String>),
        };
        statuses(d, &checks)
    };
    let stale = |got: Vec<(String, Status)>| -> Vec<(String, Status)> {
        got.into_iter().filter(|(_, s)| *s != Status::Same).collect()
    };

    assert!(all(&d, Some(base.clone())).iter().all(|(_, s)| *s == Status::Same));

    let moved = all(&d, Some(format!("// one\n// two\n// three\n{base}")));
    assert!(moved.iter().all(|(_, s)| *s == Status::Moved), "{moved:#?}");

    assert_eq!(base.matches("GIT_EDITOR=true").count(), 1);
    let changed = all(&d, Some(base.replacen("GIT_EDITOR=true", "GIT_EDITOR=:", 1)));
    let changed = stale(changed);
    assert_eq!(
        changed,
        [("resolved→running".into(), Status::Changed)],
        "rebase_continue's text, and nothing else"
    );

    assert_eq!(base.matches("fn rebase_adopt(").count(), 1);
    let gone = stale(all(&d, Some(base.replacen("fn rebase_adopt(", "fn rebase_take_up(", 1))));
    assert_eq!(gone, [("adopted".into(), Status::Gone)]);

    assert!(all(&d, None).iter().all(|(_, s)| *s == Status::Gone), "its file unread");

    let syms = crate::outlines::of_text(&root.join("src/rebase.rs"), None, &base).unwrap();
    let text = Rope::from_str(&base);
    let fresh = |at: &str| anchor(at).unwrap().check(&text, Ok(syms.as_slice())).status;
    assert_eq!(
        fresh("Editor::rebase_nowhere"),
        Status::Unresolved("no symbol named Editor::rebase_nowhere".into())
    );
    assert_eq!(fresh("card"), Status::Ambiguous(2), "Moved::card and Held::card");
    assert_eq!(
        fresh("99999-99999"),
        Status::Unresolved(format!("past its end ({} lines)", text.len_lines()))
    );
    assert!(Status::Changed.stale() && Status::Gone.stale() && !Status::Moved.stale());
}
