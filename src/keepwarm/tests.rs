use super::*;
use crate::mutate::Rng;
use std::time::Instant;

/// When the cache goes cold in these facts.
const COLD: u64 = 50_000_000;

/// A state of things around the window, any of them: each field one of the values a rule turns on.
fn any(rng: &mut Rng) -> Facts {
    let span = 2 * LEAD_MS + 120_000;
    Facts {
        now_ms: COLD + 60_000 - rng.below(span as usize) as u64,
        mode: *rng.pick(&[Mode::Expected, Mode::Expected, Mode::Marked, Mode::Off]),
        cold_at: *rng.pick(&[Some(COLD), Some(COLD), Some(COLD), None]),
        ttl: *rng.pick(&[Some(Ttl::Hour1), Some(Ttl::Hour1), Some(Ttl::Min5), None]),
        forkable: rng.below(5) != 0,
        busy: rng.below(4) == 0,
        tokens: *rng.pick(&[None, Some(99_999), Some(100_000), Some(449_000)]),
        min_tokens: 100_000,
        back: *rng.pick(&[
            None,
            None,
            Some(Back::Review),
            Some(Back::UnderReview),
            Some(Back::Card),
            Some(Back::Run),
        ]),
        marked: rng.below(3) == 0,
        pressing: rng
            .pick(&[None, None, None, Some("5-hour".to_string()), Some("weekly opus".to_string())])
            .clone(),
        kept: rng.below(5) as u32,
        max: 3,
    }
}

/// The facts where every rule holds: inside the window, big, a review waiting.
fn holds() -> Facts {
    Facts {
        now_ms: COLD - LEAD_MS + 1,
        mode: Mode::Expected,
        cold_at: Some(COLD),
        ttl: Some(Ttl::Hour1),
        forkable: true,
        busy: false,
        tokens: Some(449_000),
        min_tokens: 100_000,
        back: Some(Back::Review),
        marked: false,
        pressing: None,
        kept: 0,
        max: 3,
    }
}

/// `rule` holds for `decide` over many seeded facts — and some facts made it matter (`applies`), so it isn't
/// true only for want of a case.
fn property(name: &str, applies: impl Fn(&Facts) -> bool, rule: impl Fn(&Facts, &Decision) -> bool) {
    let mut rng = Rng(0x6b65_6570_7761_726d);
    let mut seen = 0;
    for _ in 0..20_000 {
        let f = any(&mut rng);
        let d = decide(&f);
        if applies(&f) {
            seen += 1;
            assert!(rule(&f, &d), "{name}: {d:?} for {f:#?}");
        }
    }
    assert!(seen > 10, "{name}: only {seen} cases");
}

fn pings(d: &Decision) -> bool {
    matches!(d, Decision::Ping(_))
}

#[test]
fn off_never_pings() {
    property("off", |f| f.mode == Mode::Off, |_, d| *d == Decision::LetGo(Why::Off));
}

#[test]
fn nothing_to_fork_never_pings() {
    property("no cache", |f| f.cold_at.is_none() || f.ttl.is_none() || !f.forkable, |_, d| !pings(d));
}

#[test]
fn a_five_minute_cache_is_never_pinged() {
    property("5m", |f| f.ttl == Some(Ttl::Min5), |_, d| !pings(d));
}

/// Only in `[cold − LEAD, cold − MARGIN]`: before it, a wake at its start (or a let-go that holds whenever);
/// after it, let go.
#[test]
fn a_ping_goes_only_inside_the_window() {
    let outside = |f: &Facts| f.now_ms < COLD - LEAD_MS || f.now_ms > COLD - MARGIN_MS;
    property("window", outside, |_, d| !pings(d));
    property(
        "after it",
        |f| f.now_ms > COLD - MARGIN_MS && f.cold_at.is_some(),
        |_, d| matches!(d, Decision::LetGo(_)),
    );
}

#[test]
fn a_wake_is_later_and_never_past_the_window() {
    property(
        "wake",
        |f| matches!(decide(f), Decision::WakeAt(_)),
        |f, d| matches!(d, Decision::WakeAt(t) if *t > f.now_ms && *t <= COLD - MARGIN_MS),
    );
}

#[test]
fn a_turn_or_a_ping_under_way_holds_it_back() {
    property("busy", |f| f.busy, |_, d| !pings(d));
}

#[test]
fn a_small_conversation_is_let_go() {
    property("small", |f| f.tokens.is_none_or(|n| n < f.min_tokens), |_, d| !pings(d));
}

#[test]
fn at_most_max_pings_a_stretch() {
    property("enough", |f| f.kept >= f.max, |_, d| !pings(d));
}

/// Nothing waits on you and no mark → no ping; on `marked`, only a mark counts.
#[test]
fn only_a_conversation_expected_back_is_pinged() {
    property("not expected", |f| f.back.is_none() && !f.marked, |_, d| !pings(d));
    property("marked only", |f| f.mode == Mode::Marked && !f.marked, |_, d| !pings(d));
    property(
        "marked mode's why",
        |f| f.mode == Mode::Marked && pings(&decide(f)),
        |_, d| *d == Decision::Ping(Back::Marked),
    );
}

#[test]
fn never_near_a_plan_windows_limit() {
    property("pressing", |f| f.pressing.is_some(), |_, d| !pings(d));
}

/// Every rule holding → a ping, for what waits (a mark when nothing else does) — the other side of each rule above.
#[test]
fn every_rule_holding_pings() {
    assert_eq!(decide(&holds()), Decision::Ping(Back::Review));
    assert_eq!(decide(&Facts { back: None, marked: true, ..holds() }), Decision::Ping(Back::Marked));
    assert_eq!(decide(&Facts { now_ms: COLD - MARGIN_MS, kept: 2, ..holds() }), Decision::Ping(Back::Review));
    property(
        "all hold",
        |f| {
            f.mode != Mode::Off
                && f.cold_at.is_some()
                && f.ttl == Some(Ttl::Hour1)
                && f.forkable
                && !f.busy
                && f.tokens.is_some_and(|n| n >= f.min_tokens)
                && f.kept < f.max
                && f.pressing.is_none()
                && (f.marked || (f.mode == Mode::Expected && f.back.is_some()))
                && (COLD - LEAD_MS..=COLD - MARGIN_MS).contains(&f.now_ms)
        },
        |_, d| pings(d),
    );
}

/// What can change within the window (a card comes, a window resets) is looked at again a minute on — while a
/// minute is left; the rest lets it go.
#[test]
fn what_may_change_is_looked_at_again() {
    let early = Facts { back: None, ..holds() };
    assert_eq!(decide(&early), Decision::WakeAt(early.now_ms + RECHECK_MS));
    let pressed = Facts { pressing: Some("5-hour".into()), ..holds() };
    assert_eq!(decide(&pressed), Decision::WakeAt(pressed.now_ms + RECHECK_MS));
    let late = Facts { back: None, now_ms: COLD - MARGIN_MS - 1_000, ..holds() };
    assert_eq!(decide(&late), Decision::LetGo(Why::NotExpected));
    assert_eq!(
        decide(&Facts { now_ms: 0, ..holds() }),
        Decision::WakeAt(COLD - LEAD_MS),
        "before: its start"
    );
}

/// The watch wakes at the window when a ping may keep the cache warm, else when it goes cold.
#[test]
fn the_watch_wakes_at_the_window() {
    use crate::usage::{Counts, Seen, Usage, wakes_at};
    let mut u = Usage::default();
    u.fresh("c");
    let usage = |ttl: &str| json!({ "cache_creation_input_tokens": 9, "cache_creation": { ttl: 9 } });
    let seen = |u: &mut Usage, ttl: &str| {
        let v = usage(ttl);
        u.seen(Seen {
            id: Some("m".into()),
            counts: Counts::of(&v),
            ttl: Ttl::of(&v),
            at_ms: 1_000,
            ..Seen::default()
        });
    };
    seen(&mut u, "ephemeral_1h_input_tokens");
    let cold = 1_000 + Ttl::Hour1.ms();
    assert_eq!(wakes_at(&u, Mode::Expected, 2_000), Some(cold - LEAD_MS));
    assert_eq!(wakes_at(&u, Mode::Off, 2_000), Some(cold), "off: only to say it went cold");
    assert_eq!(wakes_at(&u, Mode::Expected, cold - LEAD_MS), Some(cold), "the window come: then cold");
    assert_eq!(wakes_at(&u, Mode::Expected, cold), None);
    let mut five = Usage::default();
    five.fresh("c");
    seen(&mut five, "ephemeral_5m_input_tokens");
    assert_eq!(wakes_at(&five, Mode::Expected, 2_000), Some(1_000 + Ttl::Min5.ms()), "5m: no window");
}

/// A ping's result: the cache warm from its request, its cost its total when that's its own alone (else not
/// reported), a turn of its own in the totals; a turn of the conversation's starts the stretch again.
#[test]
fn a_ping_keeps_the_cache_warm_from_its_request_at_its_own_cost() {
    use crate::usage::{Counts, Spent, Usage};
    let mut u = Usage::default();
    u.fresh("conv");
    let turn = Spent { conversation: "conv".into(), total_cost: Some(1.0), at_ms: 1_000, ..Spent::default() };
    u.spent(turn.clone());
    let counts =
        Counts::of(&json!({ "cache_read_input_tokens": 449_000, "cache_creation_input_tokens": 4_100 }));
    let ping = Spent {
        conversation: "fork".into(),
        counts,
        total_cost: Some(0.12),
        alone: Some(true),
        at_ms: 9_000,
        ..Spent::default()
    };
    let k = u.kept_warm(ping.clone(), Some(8_000), "review").expect("kept");
    assert!((k.cost.unwrap() - 0.12).abs() < 1e-9, "its own: {:?}", k.cost);
    // (a total with what claude restored of the conversation's cost in it: its own can't be told)
    let restored = Spent { total_cost: Some(1.12), alone: Some(false), ..ping.clone() };
    assert_eq!(u.kept_warm(restored, Some(8_500), "review").expect("kept").cost, None, "not reported");
    assert_eq!((k.at_ms, k.conversation.as_str()), (8_000, "conv"));
    assert_eq!(u.conv.as_ref().unwrap().touched_ms, Some(8_500), "warm from the later ping's request");
    let t = u.totals();
    assert_eq!((t.turns, t.unpriced), (3, 1), "the turn and both pings, one not priced");
    assert!((t.cost - 1.12).abs() < 1e-9, "the turn and the ping that said its own: {}", t.cost);
    assert_eq!(u.stretch().len(), 2);
    u.spent(Spent { at_ms: 10_000, total_cost: Some(2.0), ..turn });
    assert!(u.stretch().is_empty(), "a turn of its own: a new stretch");
    // (nothing reported: nothing kept, nothing moved)
    assert_eq!(u.kept_warm(Spent::default(), None, "review"), None);
}

/// What waits on you, by the session's state: nothing → None; a message queued; the reviewer reading its change
/// (the board's line goes beside it in `readers`).
#[test]
fn what_waits_is_read_from_the_session() {
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    assert_eq!(back(&ed.session), None);
    let root = ed.session.root.clone();
    ed.session.reviewer =
        Some(crate::reviewer::Reviewer::for_test(root, crate::reviewer::State::Reading, Vec::new(), ""));
    assert_eq!(back(&ed.session), Some(Back::UnderReview));
    ed.session.reviewer.as_mut().unwrap().state = crate::reviewer::State::Done;
    assert_eq!(back(&ed.session), None, "done reading: nothing waits on it");
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    ed.session.chat.as_mut().unwrap().queued = Some("next".into());
    assert_eq!(back(&ed.session), Some(Back::Queued));
}

/// The ping's command line is the chat process's own with the fork's three, and the model it has now.
#[test]
fn the_ping_forks_on_the_chats_own_command_line() {
    let launched = crate::llm::LlmConfig {
        args: ["-p", "--resume", "old", "--settings", "{}"].map(String::from).to_vec(),
        model: Some("before".into()),
        ..Default::default()
    };
    let c = command(&launched, &(Some("after".into()), "high".into()), "conv");
    assert_eq!(
        c.args,
        [
            "-p",
            "--settings",
            "{}",
            "--resume",
            "conv",
            "--fork-session",
            "--no-session-persistence",
            "--max-turns",
            "1"
        ]
    );
    assert_eq!((c.model.as_deref(), c.effort.as_str()), (Some("after"), "high"));
}

// ── With a stand-in claude ─────────────────────────────────────────────────────────────────

/// A stand-in claude: each start writes its arguments, one a line, to `argv.<n>` (a fork its pid to `forks` too);
/// every line it's sent goes to
/// `stdin.<n>`. The chat's (no `--fork-session`) answers a message with a turn that wrote 150k tokens to the 1h
/// cache, costing $1.00 so far — asking `tools/list` first. A fork asks `tools/list`, calls a tool and asks to use
/// Edit; once those are answered, its request read 9k and wrote 4.1k (`fork.miss`: read 0, wrote 150k), and its
/// result's total is its own $0.12, `modelUsage` counting its own request alone — as a fork's is while the chat's
/// process runs (measured live, tests/live.rs `a_ping_reads_the_chats_cache`).
const STAND_IN: &str = r#"n=$(ls | grep -c '^argv\.'); printf '%s\n' "$@" > argv.$n
fork=; case " $* " in *" --fork-session "*) fork=1;; esac
[ -n "$fork" ] && echo "$$" >> forks
r=9000; w=4100; [ -f fork.miss ] && r=0 && w=150000
list='{"type":"control_request","request_id":"kw-list","request":{"subtype":"mcp_message","server_name":"tarae","message":{"jsonrpc":"2.0","id":1,"method":"tools/list"}}}'
while read -r line; do
  printf '%s\n' "$line" >> stdin.$n
  case "$line" in
    *tarae-init*) src=none; [ -n "$fork" ] && [ -f fork.source ] && src=$(cat fork.source)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"tarae-init","response":{"models":[],"account":{"apiKeySource":"%s"}}}}\n' "$src";;
    *'"type":"user"'*) printf '%s\n' "$list"
      if [ -n "$fork" ]; then
        printf '{"type":"control_request","request_id":"kw-call","request":{"subtype":"mcp_message","server_name":"tarae","message":{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"command":"true"}}}}}\n'
        printf '{"type":"control_request","request_id":"kw-edit","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{}}}\n'
      else
        printf '{"type":"system","subtype":"init","session_id":"conv-1","apiKeySource":"none","model":"m"}\n'
        printf '{"type":"system","subtype":"status","status":"requesting"}\n'
        printf '{"type":"stream_event","event":{"type":"message_start","message":{"id":"c1","model":"m","usage":{"input_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":150000,"cache_creation":{"ephemeral_1h_input_tokens":150000},"output_tokens":1}}}}\n'
        printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"conv-1","total_cost_usd":1.0}\n'
      fi;;
    *kw-edit*)
      printf '{"type":"system","subtype":"init","session_id":"fork-1","apiKeySource":"none","model":"m"}\n'
      printf '{"type":"system","subtype":"status","status":"requesting"}\n'
      printf '{"type":"stream_event","event":{"type":"message_start","message":{"id":"f1","model":"m","usage":{"input_tokens":2,"cache_read_input_tokens":%s,"cache_creation_input_tokens":%s,"cache_creation":{"ephemeral_1h_input_tokens":%s},"output_tokens":1}}}}\n' $r $w $w
      printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"fork-1","total_cost_usd":0.12,"modelUsage":{"m":{"inputTokens":2,"cacheReadInputTokens":%s,"cacheCreationInputTokens":%s,"outputTokens":1}},"usage":{"input_tokens":2,"cache_read_input_tokens":%s,"cache_creation_input_tokens":%s,"output_tokens":1}}\n' $r $w $r $w;;
  esac
done"#;

struct Place {
    root: std::path::PathBuf,
    _scratch: crate::scratch::Scratch,
}

impl Place {
    fn new(name: &str) -> Place {
        let scratch = crate::scratch::Scratch::new(&format!("keepwarm-{name}"));
        Place { root: scratch.path().to_path_buf(), _scratch: scratch }
    }

    fn editor(&self) -> Editor {
        let mut ed = Editor::with_root(crate::config::Config::default(), self.root.clone());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), STAND_IN.into(), "stand-in".into()];
        ed.config.llm.chat_settings = false;
        ed
    }

    fn argv(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.root.join(format!("argv.{n}"))).unwrap_or_default();
        text.lines().map(String::from).collect()
    }

    /// What tarae answered start `n`'s control request `id`.
    fn answer(&self, n: usize, id: &str) -> Value {
        let text = std::fs::read_to_string(self.root.join(format!("stdin.{n}"))).unwrap_or_default();
        let line = text.lines().find(|l| l.contains(&format!("\"request_id\":\"{id}\"")));
        let v: Value =
            serde_json::from_str(line.unwrap_or_else(|| panic!("no answer to {id}: {text}"))).unwrap();
        v["response"]["response"].clone()
    }
}

fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + crate::test_results::PATIENCE;
    while !done(ed) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// A chat that had one turn — 150k tokens on the 1h cache — and now four minutes before its cache goes cold.
fn waiting(place: &Place) -> Editor {
    let mut ed = place.editor();
    crate::chat::open(&mut ed);
    ed.session.chat.as_mut().unwrap().input = "hello".into();
    crate::chat::send(&mut ed);
    until(&mut ed, "the turn", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| !c.busy())
            && ed.session.usage.conv.as_ref().is_some_and(|c| c.ttl.is_some())
    });
    let near = crate::usage::now_ms() - Ttl::Hour1.ms() + 4 * 60_000;
    ed.session.usage.conv.as_mut().unwrap().touched_ms = Some(near);
    ed
}

/// The ping, end to end with a stand-in: its command line is the chat's with the fork's three; it lists tarae's
/// tools as the chat's process got them and runs none, permits nothing; its result keeps the cache warm from its
/// request, at its own cost, said on the chat's last row, in `:usage`, in the state; the next ping waits its window.
#[test]
fn a_ping_forks_the_conversation_and_says_what_it_cost() {
    let place = Place::new("ping");
    let mut ed = waiting(&place);
    let before = ed.session.usage.cold_at().unwrap();
    // (no ping yet: the turn's cost alone on the row keeps its short form somewhere)
    let short = (18..160).map(|w| row_text(&ed, w)).any(|r| r.contains("≈$1.00") && !r.contains("turn"));
    assert!(short, "`≈$1.00` alone");
    // Nothing waits on you: nothing goes
    woke(&mut ed);
    assert!(ed.session.warm.ping.is_none() && place.argv(1).is_empty(), "not expected back");
    // Marked, with a model this chat switched to since it started (`:model`): a ping — on that model, not the
    // setting another session's `:model` saved since
    crate::models::set(&mut ed, "claude-later").unwrap();
    ed.config.llm.model = Some("claude-other".into());
    mark(&mut ed, Some(true));
    assert!(ed.session.warm.ping.is_some(), "inside the window: at once");
    until(&mut ed, "the ping's end", |ed| ed.session.warm.ping.is_none());
    // Its command line: the chat's, forked, on the model of now
    let (chat, fork) = (place.argv(0), place.argv(1));
    let model = fork.iter().position(|a| a == "--model").expect("a model");
    assert_eq!(fork[model + 1], "claude-later");
    let forked: Vec<&String> = fork
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            let after = |flag: &str| *i > 0 && fork[i - 1] == flag;
            !matches!(a.as_str(), "--fork-session" | "--no-session-persistence" | "--model" | "claude-later")
                && !matches!(a.as_str(), "--resume" | "--max-turns")
                && !after("--resume")
                && !after("--max-turns")
        })
        .map(|(_, a)| a)
        .collect();
    assert_eq!(forked, chat.iter().collect::<Vec<_>>(), "the chat's own, but the fork's: {fork:?}");
    let resume = fork.iter().position(|a| a == "--resume").expect("resumed");
    assert_eq!(fork[resume + 1], "conv-1");
    // tarae's tools, as the chat's process got them; none run, nothing permitted
    assert_eq!(place.answer(1, "kw-list"), place.answer(0, "kw-list"), "the same list, to the byte");
    assert!(
        place.answer(1, "kw-list")["mcp_response"]["result"]["tools"].as_array().is_some_and(|t| t.len() > 5)
    );
    assert_eq!(place.answer(1, "kw-call")["mcp_response"]["result"]["isError"], true);
    assert_eq!(place.answer(1, "kw-edit")["behavior"], "deny");
    // Kept warm: from its request, at its own cost, said
    let u = &ed.session.usage;
    let k = u.stretch();
    assert_eq!(k.len(), 1);
    assert!((k[0].cost.unwrap() - 0.12).abs() < 1e-9, "{:?}", k[0].cost);
    assert_eq!(k[0].why, "marked");
    let after = u.cold_at().unwrap();
    assert!(after > before + 50 * 60_000, "warm another hour: {before} → {after}");
    let said = crate::usage::until(k[0].at_ms);
    let row = row_text(&ed, 140);
    assert!(row.contains(&format!("kept warm {said} · ≈$0.12 · keeps warm")), "{row}");
    assert!(row.contains("turn ≈ API $1.00"), "the turn's cost beside it: {row}");
    // (narrow: the next goes first, the context and the turn's cost next — what was kept stays, short last)
    let narrow = row_text(&ed, 46);
    assert!(
        narrow.contains("kept warm") && !narrow.contains("keeps warm") && !narrow.contains("ctx"),
        "{narrow}"
    );
    let narrowest = row_text(&ed, 34);
    assert!(narrowest.ends_with("kept ≈$0.12") && !narrowest.contains("$1.00"), "{narrowest}");
    // One order of folds: a narrower row shows no group a wider one leaves out
    let groups = ["warm until", "kept", "keeps warm", "ctx", "$1.00", "$0.12"];
    for w in 18..160 {
        let (narrow, wide) = (row_text(&ed, w), row_text(&ed, w + 1));
        for g in groups.iter().filter(|g| narrow.contains(*g)) {
            assert!(wide.contains(g), "`{g}` at {w} but not at {}: {narrow:?} → {wide:?}", w + 1);
        }
    }
    // (beside the ping's cost, the turn's says it's the turn's at every width — the word goes only with the number)
    for w in 18..160 {
        let r = row_text(&ed, w);
        assert!(
            !r.contains("$1.00") || r.contains("turn ≈$1.00") || r.contains("turn ≈ API $1.00"),
            "{w}: {r}"
        );
    }
    assert!((18..160).any(|w| row_text(&ed, w).contains("turn ≈$1.00")), "its short form somewhere");
    // (every cost on it an API estimate, said so)
    let estimates = row.matches("≈$").count() + row.matches("≈ API $").count();
    assert_eq!(row.matches('$').count(), estimates, "{row}");
    let report = crate::usage::report("s", u, &ed.limits, &[], crate::usage::now_ms());
    assert!(report.contains("## Kept warm") && report.contains("marked"), "{report}");
    let state =
        crate::uistate::UsageState::of_session(&ed.session, &ed.config.llm, &ed.limits, 0).unwrap().json();
    assert_eq!(state["keep_warm"]["kept"][0]["cache_read"], 9000, "{state}");
    assert_eq!(state["keep_warm"]["marked"], true);
    let told = |ed: &Editor| ed.toasts.iter().any(|t| t.text.contains("Claude's cache kept warm: read 9k"));
    assert!(told(&ed), "said");
    assert!(!ed.toasts.iter().any(|t| t.text.contains("didn't match")), "it read the cache");
    // The next one waits for its window — unless you ask for it now
    woke(&mut ed);
    assert!(ed.session.warm.ping.is_none(), "an hour from now");
    now(&mut ed).expect("asked");
    assert!(now(&mut ed).is_err(), "one at a time");
    until(&mut ed, "the asked ping's end", |ed| ed.session.warm.ping.is_none());
    assert_eq!(ed.session.usage.stretch().iter().map(|k| k.why).collect::<Vec<_>>(), ["marked", "asked"]);
}

/// A turn waiting on you — a tool call of its unanswered (a review, a card, a run) — sends no request until it's
/// answered: not busy, and its cache goes cold meanwhile unless pinged. A request in flight (no call waiting) is.
#[test]
fn a_turn_waiting_on_a_tool_call_is_not_a_request_in_flight() {
    let place = Place::new("waits");
    let mut ed = waiting(&place);
    let busy = |ed: &Editor| facts(&ed.session, &ed.config.llm, &ed.limits, 0).busy;
    assert!(!busy(&ed), "idle");
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    assert!(busy(&ed), "a request in flight");
    let call = crate::chat::Called { id: "t1".into(), name: "mcp__tarae__edit".into(), input: json!({}) };
    ed.session.chat.as_mut().unwrap().calls.push(call);
    assert!(!busy(&ed), "waiting for its call's result");
    ed.session.warm.checking = true;
    assert!(busy(&ed), "a ping's check under way");
}

/// A ping reads the project's Claude settings again first: as the chat's claude was started with them (here held
/// back, their card waiting), it goes; once what the gate saw isn't what the files say, it doesn't — a fork would
/// read them as they are, past the chat's `--settings`.
#[test]
fn a_ping_waits_for_the_projects_claude_settings_to_hold() {
    let place = Place::new("trust");
    std::fs::create_dir_all(place.root.join(".claude")).unwrap();
    std::fs::write(place.root.join(".claude/settings.json"), r#"{"env":{"KW_TEST":"1"}}"#).unwrap();
    let mut ed = place.editor();
    ed.claude_settings_gate = true;
    ed.config.llm.chat_settings = true;
    crate::chat::open(&mut ed);
    until(&mut ed, "the settings read", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| c.settings.seen.is_some())
    });
    ed.session.chat.as_mut().unwrap().input = "hello".into();
    crate::chat::send(&mut ed);
    until(&mut ed, "the turn", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| !c.busy())
            && ed.session.usage.conv.as_ref().is_some_and(|c| c.ttl.is_some())
    });
    let starts = |p: &Place| (0..9).filter(|n| !p.argv(*n).is_empty()).count();
    assert_eq!(starts(&place), 1, "the chat");
    now(&mut ed).expect("asked");
    until(&mut ed, "the ping", |ed| !ed.session.warm.checking && ed.session.warm.ping.is_none());
    assert_eq!(starts(&place), 2, "held back as the chat's claude was started: it went");
    // What the gate saw is no longer what's there (a change its watch hasn't read yet)
    let seen = ed.session.chat.as_mut().unwrap().settings.seen.as_mut().unwrap();
    seen.file = place.root.join(".claude/other.json");
    now(&mut ed).expect("asked");
    until(&mut ed, "the check", |ed| !ed.session.warm.checking);
    assert!(ed.session.warm.ping.is_none() && starts(&place) == 2, "no fork");
    assert!(ed.toasts.iter().any(|t| t.text.contains("settings changed since Claude started")), "said");
}

/// A fork whose claude names another credential than the chat's (`billing::checked`) is stopped before its message
/// goes: no request on that account. The message waits for `initialize`'s answer.
#[test]
fn a_ping_on_another_credential_sends_nothing() {
    let place = Place::new("credential");
    let mut ed = waiting(&place);
    std::fs::write(place.root.join("fork.source"), "ANTHROPIC_API_KEY").unwrap();
    now(&mut ed).expect("asked");
    until(&mut ed, "the ping's end", |ed| ed.session.warm.ping.is_none());
    let sent = std::fs::read_to_string(place.root.join("stdin.1")).unwrap_or_default();
    assert!(sent.contains("tarae-init") && !sent.contains("\"type\":\"user\""), "no message: {sent}");
    assert!(ed.session.usage.stretch().is_empty(), "nothing kept");
    assert!(ed.toasts.iter().any(|t| t.text.starts_with("Couldn't keep Claude's cache warm")), "said");
}

/// Whether a result's total is its own request's alone (`Spent.alone`, what a ping's cost goes by): `modelUsage`
/// counting what `usage` does — more (claude restored the conversation's earlier cost into it) is not, none is
/// not said.
#[test]
fn a_results_total_is_its_own_only_when_model_usage_counts_its_request() {
    let usage = json!({ "input_tokens": 2, "cache_read_input_tokens": 9000, "cache_creation_input_tokens": 400,
                        "output_tokens": 4 });
    let model = |read: u64| {
        json!({ "m": { "inputTokens": 2, "cacheReadInputTokens": read, "cacheCreationInputTokens": 400,
                       "outputTokens": 4 } })
    };
    let alone = |model_usage: Option<Value>| {
        let mut ev = json!({ "type": "result", "session_id": "f", "total_cost_usd": 0.1, "usage": usage });
        if let Some(m) = model_usage {
            ev["modelUsage"] = m;
        }
        match crate::llm::usage_line(&ev, 0) {
            Some(StreamEv::Spent(s)) => s.alone,
            _ => panic!("a result is a Spent"),
        }
    };
    assert_eq!(alone(Some(model(9000))), Some(true), "its own");
    assert_eq!(alone(Some(model(29_000))), Some(false), "restored");
    assert_eq!(alone(None), None, "not said");
}

/// A session behind: its ping's check and its events land in it, not in the one in front — the check (on a worker
/// with the settings gate on) and the ping's stream come through that session's senders. (`sleep_until`'s wakes
/// and `PING_MAX`'s stop go through the same `jobs()`; tests don't sleep, so they're off here.)
#[test]
fn a_ping_behind_lands_in_its_own_session() {
    let place = Place::new("behind");
    let other = crate::scratch::Scratch::new("keepwarm-behind-front");
    let mut ed = waiting(&place);
    ed.claude_settings_gate = true;
    ed.config.llm.chat_settings = true;
    let a = ed.session.id;
    // (the gate's first read, as the chat's start would have had it)
    ed.session.chat.as_mut().unwrap().settings.seen = None;
    let read = |ed: &Editor| ed.session.chat.as_ref().is_some_and(|c| c.settings.seen.is_some());
    crate::claudetrust::ready(&mut ed, crate::chat::CHAT_SETTINGS);
    until(&mut ed, "the settings read", read);
    crate::claudetrust::ready(&mut ed, crate::chat::CHAT_SETTINGS);
    now(&mut ed).expect("asked");
    assert!(ed.session.warm.checking, "its check under way");
    // Another session to the front before the check lands
    let b = ed.session_open(other.path()).unwrap();
    assert_eq!(ed.session.id, b);
    let a_of = |ed: &Editor| -> (bool, bool, usize) {
        let s = ed.sessions.iter().find(|s| s.id == a).expect("behind");
        (s.warm.checking, s.warm.ping.is_some(), s.usage.stretch().len())
    };
    until(&mut ed, "the ping kept behind", |ed| a_of(ed) == (false, false, 1));
    assert!(!ed.session.warm.checking && ed.session.warm.ping.is_none(), "nothing of it in front");
    assert!(ed.session.usage.kept.is_empty(), "the front's usage untouched");
    // (the gate's read may start the chat again as the fork starts — two starts can take one `argv.<n>`: the
    // stand-in's own line per fork counts them)
    let forks = std::fs::read_to_string(place.root.join("forks")).unwrap_or_default();
    assert_eq!(forks.lines().count(), 1, "the fork ran, once");
}

/// A ping that wrote more than it read didn't match the conversation's cache: a warning says so.
#[test]
fn a_ping_that_missed_the_cache_is_a_warning() {
    let place = Place::new("miss");
    let mut ed = waiting(&place);
    std::fs::write(place.root.join("fork.miss"), "").unwrap();
    mark(&mut ed, Some(true));
    until(&mut ed, "the ping's end", |ed| ed.session.warm.ping.is_none());
    assert!(ed.toasts.iter().any(|t| t.text.contains("didn't match the conversation's cache")), "warned");
}

/// The chat's last row, as text, at `w` cells.
fn row_text(ed: &Editor, w: usize) -> String {
    let warm = said(&ed.session, &ed.config.llm, &ed.limits, crate::usage::now_ms());
    let spans = crate::term::usage_row_for_test(ed, &warm, w);
    spans.into_iter().map(|s| s.text).collect()
}
