use std::path::Path;

use super::*;
use crate::diagram::{Edge, Kind, Node};

/// A diagram written as the drawings read: `name: title`, then nodes `id [in parent[.region]] = label` with
/// ` @ anchor` (tied), ` ! anchor` (changed) or ` ~ anchor` (untied: in more than one place), and edges
/// `a -> b : label` — `*` a start or end, `* in c` of composite c, ` | next` into the next run.
fn diagram(text: &str) -> (Diagram, Vec<Look>) {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let (name, title) = lines.next().unwrap().split_once(": ").unwrap();
    let mut d =
        Diagram { name: name.into(), title: title.into(), kind: Kind::State, nodes: vec![], edges: vec![] };
    let mut looks = Vec::new();
    for line in lines {
        if let Some((ends, label)) = line.split_once(" -> ").map(|(a, rest)| {
            let (b, label) = rest.split_once(" : ").unwrap_or((rest, ""));
            ((a, b), label)
        }) {
            let end = |s: &str| -> (Option<String>, Option<String>) {
                match s.strip_prefix("* in ") {
                    Some(c) => (None, Some(c.into())),
                    None if s == "*" => (None, None),
                    None => (Some(s.into()), None),
                }
            };
            let ((from, w1), (to, w2)) = (end(ends.0), end(ends.1));
            let (label, next_run) = label.strip_suffix(" | next").map_or((label, false), |l| (l, true));
            d.edges.push(Edge { from, to, within: w1.or(w2), label: label.into(), next_run, at: None });
            continue;
        }
        let (head, rest) = line.split_once(" = ").unwrap();
        let (id, parent) = head.split_once(" in ").map_or((head, None), |(i, p)| (i, Some(p)));
        let (parent, region) = match parent.and_then(|p| p.split_once('.')) {
            Some((p, r)) => (Some(p), r.parse().unwrap()),
            None => (parent, 0),
        };
        let look = [(" @ ", Mark::Tied), (" ! ", Mark::Changed), (" ~ ", Mark::Untied)].into_iter().find_map(
            |(sep, mark)| rest.split_once(sep).map(|(l, a)| (l, Look { mark, anchor: Some(a.into()) })),
        );
        let (label, look) = look.unwrap_or((rest, Look { mark: Mark::Untied, anchor: None }));
        d.nodes.push(Node {
            id: id.into(),
            label: label.into(),
            parent: parent.map(Into::into),
            region,
            at: None,
        });
        looks.push(look);
    }
    assert_eq!(d.problems(), Vec::<String>::new());
    (d, looks)
}

fn node(d: &Diagram, id: &str) -> usize {
    d.nodes.iter().position(|n| n.id == id).unwrap()
}

fn drawn(text: &str, selected: &str, anchors: bool, width: usize, height: usize) -> Drawing {
    let (d, looks) = diagram(text);
    let selected = (!selected.is_empty()).then(|| node(&d, selected));
    draw(&d, &looks, selected, anchors, Room { width, height })
}

/// The drawing as the snapshots keep it: a head saying its room, size and folds, then its lines.
fn shown(text: &str, selected: &str, anchors: bool, width: usize, height: usize) -> String {
    let dr = drawn(text, selected, anchors, width, height);
    format!(
        "== {width} × {height}{}: {} rows × {} columns, folds {:?}\n{}\n",
        if anchors { ", the anchor column on" } else { "" },
        dr.lines.len(),
        dr.width,
        dr.folds,
        dr.text()
    )
}

/// Compared with `src/snapshots/<name>`, or written there with `TARAE_BLESS=1`.
fn pinned(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots").join(name);
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    if expected != actual {
        let first = expected.lines().zip(actual.lines()).position(|(a, b)| a != b);
        panic!(
            "{} differs (first at line {}) — if intended: TARAE_BLESS=1 cargo test diagram::layout\n\n{actual}",
            path.display(),
            first.map_or("past the shorter's end".into(), |l| (l + 1).to_string()),
        );
    }
}

/// The plan's sample (cells-2): a session's rebase onto its base.
const REBASE: &str = "
rebase: a session's rebase onto its base
moved = base moved · card @ Moved::card
checking = checkout looked at @ Editor::rebase_check
held = held · card @ Held::card
queued = queued behind a run @ Editor::rebase_queued
running = git running · 2/5 @ Editor::rebase_run
stopped = stopped at a conflict @ Stage::Stopped
resolving in stopped = Claude resolving @ Editor::rebase_ask
markers in stopped = markers left? @ turn_over
resolved = resolved · review and card @ Editor::rebase_checked
done = done · bring card @ Moved::bring_card
aborted = aborted @ Ended::Aborted
failed = failed to begin ! Ended::Failed · changed
adopted = taken up after a restart @ rebase_adopt
* -> moved : base moved — since the session left it
moved -> checking : y — rebase
checking -> held : changes — in no commit, or unsaved buffers
held -> checking : s autostash
checking -> queued : busy — a run in the task panel
queued -> checking : its turn — in the queue
checking -> running : clean
running -> stopped : conflicts — git stopped at them
running -> done : done — the branch's commits are on the base's tip
running -> aborted : aborted — git rebase --abort ended
running -> failed : failed — git stopped before it began
* in stopped -> resolving
resolving -> markers : turn over — Claude's turn ended
markers -> resolving : some left — you ask Claude again
markers -> * in stopped : none left
stopped -> resolved : none left — the review opens
resolved -> running : y continue · X abort — or rebase-continue and rebase-abort by hand
* -> adopted : restart — a rebase tarae didn't start
adopted -> stopped
done -> * : y bring · p preflight — then bring
aborted -> *
failed -> *
";

/// cells-3 §1: a run of Claude's, tools.rs + queued.rs. The card's way is written before the judge's — the
/// layout puts the judge above (else their rails interleave).
const RUN: &str = "
run: a run of Claude's, its ask to its answer
until = until it starts @ queued::turn_over
asked in until = asked · run tool @ tools::run
judged in until = judged where it reads @ tools::read_only
card in until = run card @ tools::card
admitted in until = admitted @ queued::admit
line in until = in line · 2nd @ queued::push_at
made in until = made a protected name? @ tools::guard_then_go · 2622
madecard in until = made card @ tools::ask_made
tree in until = its tree written @ basetree::export
sandbox in until = sandbox made @ tools::guard_then_go · 2669
nosandbox in until = no-sandbox card @ tools::ask_unprotected
saved in until = saved · snapshot @ tools::go
started = started @ tools::go_now
running = running · task panel @ tools::run_start
changed = what it changed @ tools::run_changes_now
answered = answered @ tools::run_done
refused = refused
joined = joined @ queued::join
told = told no
notrun = not run @ queued::leave
notstarted = not started @ tools::not_started
dropped = dropped
* -> asked
until -> dropped : turn ended — nobody waits for it
asked -> refused : too long
asked -> joined : same run
asked -> card : asks
asked -> judged : reads only
asked -> admitted : allowed
judged -> admitted : inside
judged -> card : past
card -> told : n
card -> admitted : y · a · A
admitted -> joined : same run
admitted -> line : slot busy — another run in its slot, or Claude's before it in line
admitted -> made : free
line -> notrun : s in :procs — out of the line, not run
line -> made : its turn — the slot freed (run_next)
made -> madecard : yes
made -> tree : a commit
made -> sandbox : clear
madecard -> made : y keep · r remove
madecard -> told : n
tree -> made
sandbox -> nosandbox : partial · none
sandbox -> notstarted : policy failed
sandbox -> saved : in it
nosandbox -> told : n
nosandbox -> saved : y · a
saved -> started
started -> line : slot taken — it got ready and the slot went meanwhile: back at the front
started -> running
running -> changed : ended
changed -> made : made protected | next
changed -> answered : report
answered -> *
";

/// cells-3 §2: a card's life, offer.rs — regions three deep.
const CARD: &str = "
card: a card's life, asked to gone
asked = asked · push_offer @ Editor::push_offer
merged = merged @ push_offer · 1085
parked = parked @ push_offer · 1076
up = up @ Editor::drop_cards_for
stacked in up = stacked @ Editor::offer_place
front in up = in front @ Editor::offer_to_front
settling in front = settling @ Editor::offer_settled
settled in front = settled @ offer_settled · 2000
elsewhere in front.1 = keys elsewhere @ Editor::card_has_keys
keys in front.1 = has the keys @ Editor::card_key
whole in front.2 = whole
open in front.2 = text open @ Editor::offer_viewing
aside in front.2 = aside @ Editor::offer_bar
asking in up.1 = asking @ OfferState::Asking
downloading in up.1 = downloading @ OfferState::Installing
failed in up.1 = failed @ OfferState::Failed
gone = gone · answered @ Editor::offer_finished
dropped = dropped
* -> asked
asked -> merged : same title
asked -> parked : session behind
asked -> stacked : placed by rank
parked -> stacked : session in front
up -> dropped : turn ended
stacked -> front : first
front -> stacked : one ranking first
settling -> settled : in sight
settled -> settling : text replaced
elsewhere -> keys : keys free · C-q — normal mode, no prompt, picker or review, the chat and terminal unfocused
keys -> elsewhere : prompt · chat — a prompt opened, the chat or a panel took them
keys -> gone : y · a · A · n · r — its answers (Offer::answers)
whole -> open : v
whole -> aside : h
open -> whole : read to its end
aside -> whole : h
asking -> downloading : y on a download
downloading -> failed : failed
failed -> downloading : y again
downloading -> gone : done
gone -> *
";

/// cells-3 §3: the keeper, sandbox/keeper.rs + channel.rs — a region per run, one crossing no order removes.
const KEEPER: &str = "
keeper: a session's keeper, as tarae sees it — Linux
asked = asked for · a run's policy @ keeper::ready
refused = refused @ keeper::ready · 195
born = born · clone3 @ Keeper::born_lo
couldnt = couldn't start
waiting = waiting for Ready @ keeper::said_ready
planting = planting · its sockets @ Keeper::plant
alive = alive @ Keeper::heard
run in alive = a run asked · ×n @ channel::Msg::Run
notstarted = not started
child in alive = its child readies ~ keeper::child · 2 places
failedat = failed at it @ channel::Step
running in alive = running · its pidfd @ channel::Msg::Started
exited in alive = exited · its status @ channel::Msg::Exited
serving in alive.1 = serving @ Keeper::run
draining in alive.1 = draining · its runs go on @ keeper::ready · 179
ending in alive.1 = ending · End sent @ Keeper::end
gone = gone @ Keeper::gone
* -> asked
asked -> refused : died too often
asked -> alive : one fits
asked -> born : none · another key
born -> couldnt : a step failed
born -> waiting : exec'd
waiting -> alive : Ready — its device handles with it
waiting -> gone : none — no Ready: thrown away (SIGKILL through its pidfd), gone
waiting -> planting : own network
planting -> gone : failed
planting -> alive : planted
run -> notstarted : no Started
run -> child : forked
child -> failedat : a step failed
child -> running : exec'd
running -> running : signal
running -> exited : reaped
exited -> * in alive
serving -> draining : another key
draining -> ending : the last exited
ending -> gone : exited
gone -> asked : the next ask
";

/// Five ways back up from one node, its targets' names short: rails abreast, each fold to a reference narrows.
const RAILS: &str = "
rails: five ways back
a = a
b = b
c = c
d = d
e = e
f = f
* -> a
a -> b : one
b -> c : two
c -> d : three
d -> e : four
e -> f : five
f -> a : v
f -> b : w
f -> c : x
f -> d : y
f -> e : z
f -> *
";

/// The four machines as the drawings show them: at 100 with the anchor column, at 60 as they open.
#[test]
fn the_drawings_are_pinned() {
    let mut out = String::new();
    for (name, text, selected, height) in [
        ("rebase", REBASE, "running", 24),
        ("run", RUN, "line", 30),
        ("card", CARD, "keys", 24),
        ("keeper", KEEPER, "waiting", 30),
    ] {
        out += &format!("### {name}\n");
        out += &(shown(text, selected, true, 100, 50) + &shown(text, selected, false, 60, height));
    }
    // The width folds in their order, the anchor column toggled on.
    out += "### keeper, narrowing\n";
    for width in [60, 40, 36, 30, 24] {
        out += &shown(KEEPER, "waiting", true, width, 50);
    }
    out += "### rails, narrowing\n";
    for width in [100, 15] {
        out += &shown(RAILS, "", false, width, 50);
    }
    pinned("diagram_layout.txt", &out);
}

fn rows(dr: &Drawing) -> Vec<String> {
    dr.lines.iter().skip(1).map(Line::text).collect()
}

fn row<'a>(rows: &'a [String], has: &str) -> &'a str {
    rows.iter()
        .find(|r| r.contains(has))
        .unwrap_or_else(|| panic!("no row has {has:?}:\n{}", rows.join("\n")))
}

fn at(rows: &[String], has: &str) -> usize {
    rows.iter().position(|r| r.contains(has)).unwrap_or_else(|| panic!("no row has {has:?}"))
}

/// Every fixture at both widths.
fn every() -> Vec<Drawing> {
    let all = [(REBASE, "running"), (RUN, "line"), (CARD, "keys"), (KEEPER, "waiting")];
    all.iter()
        .flat_map(|&(t, s)| {
            [drawn(t, s, true, 100, 50), drawn(t, s, false, 60, 30), drawn(t, s, false, 48, 24)]
        })
        .collect()
}

#[test]
fn a_way_out_hangs_as_a_side_when_it_leads_back_to_the_spine() {
    let run = rows(&drawn(RUN, "", false, 100, 50));
    assert!(row(&run, "judged where it reads").starts_with(" ╎ ├─reads only─▶ ●"));
    assert!(row(&run, "run card").starts_with(" ╎ ├─asks─▶ ●"));
    assert!(row(&run, "admitted ◀").starts_with(" ╎ ● admitted"), "the spine's next");
    assert!(row(&run, "in line").starts_with(" ╎ ├─slot busy─▶ ●"));
    // A way that leads past the spine's next node keeps its node on the spine: the keeper's chain.
    let keeper = rows(&drawn(KEEPER, "", false, 100, 50));
    for spine in ["born · clone3", "waiting for Ready", "planting · its sockets"] {
        assert!(row(&keeper, spine).starts_with(" ● "), "{spine}");
    }
}

#[test]
fn the_scopes_way_out_stays_on_the_spine() {
    let run = rows(&drawn(RUN, "", false, 100, 50));
    assert!(row(&run, "saved · snapshot").starts_with(" ╎ ● saved"));
    assert!(run[at(&run, "saved · snapshot") - 1].starts_with(" ╎ │ in it "));
}

#[test]
fn sides_that_lead_to_each_other_go_in_that_order() {
    // RUN lists the card's way before the judge's; the judge leads to the card, so it's drawn above it.
    let run = rows(&drawn(RUN, "", false, 100, 50));
    assert!(at(&run, "judged where it reads") < at(&run, "run card"));
    assert!(row(&run, "╰─past").ends_with("╮  │"), "the past rail inside the diamond's, not across it");
}

#[test]
fn ends_itself_the_end_and_the_next_run_are_words() {
    let run = rows(&drawn(RUN, "", false, 100, 50));
    assert_eq!(run.iter().filter(|r| r.contains("─▶ joined")).count(), 2, "at each place it's reached from");
    assert_eq!(run.iter().filter(|r| r.contains("─▶ told no")).count(), 3);
    assert!(!run.iter().any(|r| r.contains("● joined") || r.contains("● told no")));
    assert!(row(&run, "made protected").contains("─▶ ↑ made a protected name? · next run"));
    let keeper = rows(&drawn(KEEPER, "", false, 100, 50));
    assert!(row(&keeper, "signal").contains("├─signal─▶ itself"));
    let rebase = rows(&drawn(REBASE, "", false, 100, 50));
    assert!(row(&rebase, "done · bring card").contains("─y bring · p preflight─▶ end"));
    // a gist that repeats its target says nothing; an unlabelled way to the end isn't drawn
    let aborted = row(&rebase, "aborted");
    assert!(aborted.starts_with(" ├─▶ ● aborted ") && !aborted.contains("end"), "{aborted}");
}

#[test]
fn a_row_takes_its_first_way_inline_only_when_free() {
    let run = rows(&drawn(RUN, "", false, 100, 50));
    assert!(row(&run, "● started").contains("started ─slot taken─"), "its only way but the spine's");
    assert!(row(&run, "made card").contains("card ─y keep · r remove─"), "back up");
    assert!(row(&run, "no-sandbox card").contains("card ─n─▶ told no"), "a word");
    assert!(row(&run, "until it starts").contains("starts ─turn ended─▶ dropped"), "a composite's title row");
    assert_eq!(row(&run, "judged where it reads").trim_end(), " ╎ ├─reads only─▶ ● judged where it reads");
    assert_eq!(row(&run, "● what it changed").trim_end(), " ● what it changed", "a label row follows");
    let card = rows(&drawn(CARD, "", false, 100, 50));
    assert!(row(&card, "● has the keys").contains("keys ─prompt · chat─"));
    assert!(row(&card, "● has the keys").ends_with('╯'));
    let keeper = rows(&drawn(KEEPER, "", false, 100, 50));
    assert!(row(&keeper, "● gone").contains("gone ◀─"), "something comes in");
    assert!(row(&keeper, "the next ask").starts_with(" ╰─the next ask─"));
}

#[test]
fn one_rail_per_target_each_label_on_its_source_row() {
    let rebase = rows(&drawn(REBASE, "", false, 100, 50));
    let col = |r: &str, glyph: char| r.chars().position(|c| c == glyph);
    let checkout = row(&rebase, "checkout looked at");
    assert_eq!(checkout.matches('◀').count(), 1);
    assert!(row(&rebase, "held · card").contains("card ─s autostash─"));
    assert!(row(&rebase, "queued behind").contains("run ─its turn─"));
    let rail = col(checkout, '╮');
    assert!(rail.is_some());
    assert_eq!(col(row(&rebase, "held · card"), '┤'), rail);
    assert_eq!(col(row(&rebase, "queued behind"), '╯'), rail);
    // back up too: the label on resolved's row, not on running's
    assert!(row(&rebase, "resolved ·").contains("card ─y continue · X abort─"));
    assert!(!row(&rebase, "git running").contains("continue"));
}

#[test]
fn rails_hug_what_they_pass() {
    let hug = |b: &str| {
        let text = format!(
            "t: hug\na = a @ x\nb = {b} @ x\nc = ccccc @ x\n* -> a\na -> b\nb -> c\nc -> a : back\nc -> *"
        );
        rows(&drawn(&text, "", false, 100, 50))
    };
    let dashes = |n: usize| "─".repeat(n);
    let tight = [format!("▸● a ◀{}╮", dashes(9)), " ● bb          │".into(), " ● ccccc ─back─╯".into()];
    assert_eq!(hug("bb"), tight, "its taps' runs reach it");
    let wide = [
        format!("▸● a ◀{}╮", dashes(14)),
        " ● bbbbbbbbbbbbbbbb │".into(),
        format!(" ● ccccc ─back{}╯", dashes(6)),
    ];
    assert_eq!(hug("bbbbbbbbbbbbbbbb"), wide, "a cell after the text it passes");
    // two cells after a rail it passes
    let keeper = rows(&drawn(KEEPER, "", false, 100, 50));
    assert!(row(&keeper, "one fits").ends_with("╮  │"));
}

#[test]
fn the_inner_rail_is_the_one_crossing_least_and_a_crossing_is_a_hop() {
    // The keeper's ways into alive and its ways to gone interleave: one crossing no order removes — at
    // alive's row, where the gone rail's │ runs through.
    let keeper = rows(&drawn(KEEPER, "", false, 100, 50));
    let hops: Vec<&String> = keeper.iter().filter(|r| r.contains("─│─")).collect();
    assert_eq!(hops.len(), 1, "{}", keeper.join("\n"));
    assert!(hops[0].contains("alive ◀"));
    // A node's own rails nest when the diagram's order would cross them: ways up nearest first.
    let rails = rows(&drawn(RAILS, "", false, 100, 50));
    assert!(!rails.iter().any(|r| r.contains("─│─")), "{}", rails.join("\n"));
    assert!(row(&rails, "◦ f").contains("f ─z─╯"));
    for dr in every() {
        assert!(!dr.text().contains('┼'), "never ┼:\n{}", dr.text());
    }
}

#[test]
fn rails_never_end_and_begin_in_one_column_on_adjacent_rows() {
    for dr in every() {
        let grid: Vec<Vec<char>> = rows(&dr).iter().map(|r| r.chars().collect()).collect();
        for r in 1..grid.len() {
            for (c, &ch) in grid[r].iter().enumerate() {
                let above = grid[r - 1].get(c).copied().unwrap_or(' ');
                assert!(!(ch == '╮' && above == '╯'), "row {r} column {c}:\n{}", dr.text());
            }
        }
    }
}

#[test]
fn composites_indent_behind_a_rule_and_regions_part_at_once() {
    let card = rows(&drawn(CARD, "", false, 100, 50));
    assert!(row(&card, " up ─turn ended").starts_with(" ╎ up"));
    assert!(row(&card, "in front ─").starts_with(" ╎ ╎ in front"), "nested: ╎ a level");
    let parts: Vec<&String> = card.iter().filter(|r| r.contains("┄ at once ┄")).collect();
    assert_eq!(parts.len(), 3);
    assert!(
        at(&card, "settled ─") < at(&card, "┄ at once")
            && at(&card, "┄ at once") < at(&card, "keys elsewhere")
    );
    assert!(card.iter().any(|r| r.starts_with(" ╎ ┄ at once ┄")), "up's own regions, one level out");
    let keeper = rows(&drawn(KEEPER, "", false, 100, 50));
    assert!(keeper[at(&keeper, "● gone") - 1].starts_with(" │ exited "), "its way out after its block");
}

#[test]
fn a_second_way_in_rides_on_its_targets_row() {
    let rebase = drawn(REBASE, "", true, 100, 50);
    let stopped = row(&rows(&rebase), "stopped at a conflict").to_string();
    assert!(stopped.contains("conflict ◀─ ▸● taken up after a restart"));
    assert!(stopped.ends_with("ƒ Stage::Stopped · rebase_adopt"));
    // — unless a rail comes in there: then it's a node of its own
    let text =
        "t: in\na = a\nb = b\nc = c\nx = x\n* -> a\na -> b\nb -> c\nc -> b : again\n* -> x\nx -> b\nc -> *";
    let r = rows(&drawn(text, "", false, 100, 50));
    assert!(!r.iter().any(|r| r.contains("◀─ ▸")), "{}", r.join("\n"));
    assert!(row(&r, "◦ x").starts_with("▸◦ x"));
}

#[test]
fn a_composites_own_start_is_no_entry() {
    // x's only way in is its composite's start: it starts c's spine, it doesn't ride on y's row.
    let text = "t: inner\na = a\nc = c\nx in c = x\ny in c = y\nb = b\n* -> a\na -> c : go\n* in c -> x\nx -> y\ny -> * in c\nc -> b\nb -> *";
    let r = rows(&drawn(text, "", false, 100, 50));
    assert!(row(&r, "◦ x").starts_with(" ╎ ◦ x"), "{}", r.join("\n"));
    assert_eq!(
        r.iter().filter(|r| r.contains('▸')).count(),
        1,
        "only the diagram's way in:\n{}",
        r.join("\n")
    );
}

#[test]
fn a_riding_entry_is_selected_on_its_row() {
    let (d, looks) = diagram(REBASE);
    let (adopted, stopped) = (node(&d, "adopted"), node(&d, "stopped"));
    let dr = draw(&d, &looks, Some(adopted), false, Room { width: 100, height: 50 });
    let line = dr.lines.iter().find(|l| l.text().contains("taken up after a restart")).unwrap();
    assert!(line.text().starts_with('▎'), "{}", line.text());
    assert_eq!(line.nodes, [stopped, adopted]);
}

#[test]
fn folds_come_in_the_users_order_each_kept_only_if_it_narrows() {
    let rank = |f: &Fold| match f {
        Fold::Anchors => 0,
        Fold::Gists(n) if *n >= GIST_FLOOR => 1,
        Fold::Refs(_) => 2,
        Fold::Gists(_) => 3,
        Fold::Names(_) => 4,
        Fold::Moved(_) => 5,
    };
    // Narrowing, each fixture takes its folds in order and keeps what it took.
    let mut seen = [false; 5];
    for text in [REBASE, RUN, CARD, KEEPER, RAILS] {
        let mut last = Vec::new();
        for width in (12..=100).rev() {
            let dr = drawn(text, "", true, width, 50);
            let ranks: Vec<usize> = dr.folds.iter().map(rank).collect();
            assert!(ranks.windows(2).all(|w| w[0] < w[1]), "at {width}: {:?}", dr.folds);
            assert!(last.iter().all(|r| ranks.contains(r)), "at {width}: {:?} after {last:?}", dr.folds);
            for &r in &ranks {
                seen[r] = true;
            }
            last = ranks;
        }
    }
    assert_eq!(seen, [true; 5], "every width fold is taken somewhere");
    // Rails abreast fold into references that are narrower than they were.
    let rails = drawn(RAILS, "", false, 15, 50);
    assert!(rails.folds.iter().any(|f| matches!(f, Fold::Refs(_))) && rails.width <= 15, "{}", rails.text());
    // A reference names its target, so it can be wider than its rail: kept only where it narrows.
    let text = "t: wide\na = a\nb = the node with a very long name\nc = c\n* -> a\na -> b\nb -> c\nc -> b : x\nc -> *";
    let (d, looks) = diagram(text);
    let full = draw(&d, &looks, None, false, Room { width: 100, height: 50 });
    let narrow = draw(&d, &looks, None, false, Room { width: full.width - 1, height: 50 });
    assert!(
        !narrow.folds.iter().any(|f| matches!(f, Fold::Refs(_))),
        "{:?}\n{}",
        narrow.folds,
        narrow.text()
    );
}

#[test]
fn labels_move_as_the_height_needs_bare_nodes_first_in_the_width() {
    let run = drawn(RUN, "", false, 60, 34);
    assert_eq!(run.folds, [Fold::Moved(2)]);
    let r = rows(&run);
    assert!(row(&r, "● running").ends_with("  ↓ ended"), "a node without branches first");
    assert!(row(&r, "● asked").ends_with("  ↓ allowed"), "then from the top");
    assert!(row(&r, "● admitted").starts_with(" ╎ ● admitted ◀"));
    // a moved label goes before what comes in on its row
    assert!(rows(&drawn(RUN, "", false, 60, 30)).iter().any(|r| r.contains("● admitted  ↓ free ◀")));
    // a label stays a row where moving it would leave the room; then the view scrolls
    let keeper = drawn(KEEPER, "", false, 50, 25);
    assert!(keeper.width <= 50, "{}", keeper.text());
    assert!(keeper.lines.len() > 25);
    assert!(rows(&keeper).iter().any(|r| r.trim_start().starts_with("│ none · another key")));
}

#[test]
fn the_selected_nodes_names_and_gists_are_never_cut() {
    // The keeper at 36 cuts gists to the floor and names to 23 cells.
    let keeper = drawn(KEEPER, "asked", false, 36, 50);
    assert!(keeper.folds.contains(&Fold::Gists(GIST_FLOOR)), "{:?}", keeper.folds);
    assert!(keeper.folds.iter().any(|f| matches!(f, Fold::Names(_))), "{:?}", keeper.folds);
    let r = rows(&keeper);
    assert!(row(&r, "asked for · a run's policy").starts_with('▎'));
    for way in ["died too often", "none · another key", "the next ask"] {
        assert!(r.iter().any(|r| r.contains(way)), "{way}: its ways out and in");
    }
    assert!(!r.iter().any(|r| r.contains("the last exited")), "others are cut");
}

#[test]
fn the_anchor_column_is_off_until_toggled() {
    let off = drawn(REBASE, "", false, 100, 50);
    assert!(!off.text().contains('ƒ'));
    let on = drawn(REBASE, "", true, 100, 50);
    let r = rows(&on);
    let col = r[0].chars().position(|c| c == 'ƒ');
    assert_eq!(col, Some(off.width + 3));
    assert!(row(&r, "failed to begin").ends_with("▲ Ended::Failed · changed"));
    let card = rows(&drawn(CARD, "", true, 100, 50));
    assert!(row(&card, "◦ whole").ends_with("◦ untied"));
    assert!(row(&card, "same title").ends_with("ƒ push_offer · 1085"), "an end's anchor where it's first");
}

#[test]
fn the_title_drops_counts_then_its_title_to_the_gist() {
    let title = |t: &str, w: usize| drawn(t, "", false, w, 50).lines[0].text();
    assert!(title(KEEPER, 100).ends_with("12 states · 4 ends · 20 transitions"));
    assert_eq!(title(CARD, 60), " card · a card's life, asked to gone      14 states · 2 ends");
    assert_eq!(title(KEEPER, 60), " keeper · a session's keeper, as tarae sees it     12 states");
    assert_eq!(title(KEEPER, 100).width(), 100);
}
