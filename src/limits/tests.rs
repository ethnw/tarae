use super::*;
use crate::llm::StreamEv;
use std::time::{Duration, Instant};

/// `limits_stream.jsonl`'s line `i` (its `//` lines skipped): written by hand from the fields the user's L2 run
/// shared — `init`, the `rate_limit_event` that came at start, the `get_usage` answer (asked as `u`), the turn's
/// `result`; the fixture's head says where each shape comes from and which values are made up.
fn line(i: usize) -> Value {
    let mut lines = include_str!("../limits_stream.jsonl").lines().filter(|l| !l.starts_with("//"));
    let text = lines.nth(i).expect("the fixture's line");
    serde_json::from_str(text).unwrap()
}

/// The weekly window resets 2026-10-13T01:00:00Z (`resetsAt` 1791853200) — readings are taken before it.
const RESETS_MS: u64 = 1_791_853_200_000;
const BEFORE: u64 = RESETS_MS - 3 * 86_400_000;

/// The fixture's line `i` read as the stream reader reads it, at `at_ms`.
fn report_of(i: usize, at_ms: u64) -> Report {
    let v = line(i);
    let ev = crate::llm::usage_line(&v, at_ms).or_else(|| crate::llm::parse_line(&v));
    match ev {
        Some(StreamEv::Limits(r)) => r,
        // (the answer to `u`: asked by tarae it comes back as `tarae-usage` — read the same)
        _ => of_answer(&v["response"], at_ms),
    }
}

/// Claude Code's limit words name the plan's window that ran out (the errors reference's four), not a spend limit
/// (the account's, no window); and the event says whether the plan's usage credits pay (`isUsingOverage`).
#[test]
fn the_limit_words_name_the_window_and_the_event_says_who_pays_past_it() {
    assert_eq!(of_words("You've hit your session limit · resets 3:45pm"), Some("five_hour"));
    assert_eq!(of_words("API Error: You've hit your weekly limit · resets Mon 12:00am"), Some("seven_day"));
    assert_eq!(of_words("You've hit your Opus limit · resets 3:45pm"), Some("seven_day_opus"));
    assert_eq!(of_words("You've hit your Sonnet limit · resets 3:45pm"), Some("seven_day_sonnet"));
    assert_eq!(of_words("You've hit your monthly spend limit · raise it at https://claude.ai"), None);
    assert_eq!(of_words("stopped"), None);
    let ev = serde_json::json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected",
        "rateLimitType": "seven_day", "isUsingOverage": true}});
    let mut l = Limits::default();
    l.seen(of_event(&ev, BEFORE).unwrap());
    assert_eq!(l.overage, Some(true));
    // (an answer that doesn't say leaves what the event said)
    l.seen(Report::default());
    assert_eq!(l.overage, Some(true));
}

/// The `rate_limit_event` a claude sends as it starts says where the weekly window stands: its fraction as a
/// whole percent, its reset (seconds) in ms, and claude's warning.
#[test]
fn a_rate_limit_event_at_start_says_the_weekly_window() {
    let r = report_of(1, BEFORE);
    assert_eq!(
        r.readings,
        [Reading {
            window: "seven_day".into(),
            percent: Some(92),
            resets_at_ms: Some(RESETS_MS),
            status: Some(Status::Warning),
            seen_at_ms: BEFORE,
            from: Source::Event,
        }]
    );
}

/// One window in two units: the event's `utilization` is a fraction (0.92), `get_usage`'s a percent (93) — both
/// come out whole percents, so a reader that forgot to scale one says 1 % or 9300 % and fails here. The later
/// reading wins; the status the event said stays for the same period (`get_usage` says none).
#[test]
fn get_usage_reports_percent_and_the_event_a_fraction() {
    let mut l = Limits::default();
    l.seen(report_of(1, BEFORE));
    assert_eq!(l.get(WEEKLY, BEFORE).unwrap().percent, Some(92));
    let answer = report_of(2, BEFORE + 1000);
    assert_eq!(answer.answer, Some(Answer::Read));
    assert_eq!(answer.plan.as_deref(), Some("max"));
    l.seen(answer);
    let w = l.get(WEEKLY, BEFORE + 2000).unwrap();
    assert_eq!((w.percent, w.resets_at_ms, w.from), (Some(93), Some(RESETS_MS), Source::Usage));
    assert_eq!(w.status, Some(Status::Warning), "the event's warning, kept for the same period");
    assert_eq!(said(w), format!("weekly 93 % · resets {}", resets(RESETS_MS)));
}

/// A window `get_usage` sends as null is unknown — no reading, never 0 % — while one that says 0 is 0 %.
#[test]
fn a_null_window_is_unknown_not_zero() {
    let r = report_of(2, BEFORE);
    let windows: Vec<(&str, Option<u32>)> =
        r.readings.iter().map(|w| (w.window.as_str(), w.percent)).collect();
    assert_eq!(windows, [("five_hour", Some(0)), ("seven_day", Some(93))], "no opus, no sonnet");
    let mut nulls = line(2)["response"].clone();
    nulls["response"]["rate_limits"]["seven_day"]["utilization"] = Value::Null;
    let r = of_answer(&nulls, BEFORE);
    assert!(r.readings.iter().all(|w| w.window != WEEKLY), "a null utilization: unknown");
}

/// What claude adds that `sdk.d.ts` doesn't declare — L2's `unifiedWindows` and `seven_day_breakdown`, and one
/// added here — changes nothing: the reader takes the fields it names.
#[test]
fn fields_claude_adds_are_ignored() {
    let plain = |mut v: Value| {
        v["rate_limit_info"].as_object_mut().map(|o| o.remove("unifiedWindows"));
        v["response"]["response"]["rate_limits"].as_object_mut().map(|o| o.remove("seven_day_breakdown"));
        v
    };
    let mut added = line(2);
    added["response"]["response"]["rate_limits"]["stand_in_new"] =
        json!({"utilization": 50, "resets_at": null});
    added["response"]["response"]["stand_in_top"] = json!(true);
    assert_eq!(of_event(&line(1), 1), of_event(&plain(line(1)), 1));
    assert_eq!(of_answer(&line(2)["response"], 1), of_answer(&plain(line(2))["response"], 1));
    assert_eq!(of_answer(&added["response"], 1), of_answer(&line(2)["response"], 1));
}

/// `init` says the credential claude runs on: the subscription's is `none`; L1's run on the key through a
/// helper said `apiKeyHelper` — and only the first is on the plan, so its limits are that session's. Nothing said
/// yet is unknown, not the plan. Its answer to `initialize` says it too (`account.apiKeySource`, the SDK's
/// `SDKControlInitializeResponse`), so it's known as claude starts.
#[test]
fn the_subscription_says_none_as_its_key_source() {
    let key = |v: &Value| match crate::llm::parse_line(v) {
        Some(StreamEv::Conversation { key_source, .. }) => key_source,
        Some(StreamEv::Initialized { key_source, .. }) => key_source,
        _ => panic!("not read as the conversation's start nor the answer to initialize"),
    };
    assert_eq!(key(&line(0)).as_deref(), Some("none"));
    let mut l1 = line(0);
    l1["apiKeySource"] = json!("apiKeyHelper");
    assert_eq!(key(&l1).as_deref(), Some("apiKeyHelper"));
    let initialized = json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": "tarae-init", "response": {"models": [], "account": {"apiKeySource": "none"}}}});
    assert_eq!(key(&initialized).as_deref(), Some("none"));
    assert!(on_plan(Some("none"), false) && !on_plan(None, false), "none: the plan's; not said yet: unknown");
    assert!(!on_plan(Some("apiKeyHelper"), false) && !on_plan(Some("ANTHROPIC_API_KEY"), false));
    // (its own rate_limit_event, nothing said of its credential yet: the plan's — sent to plan users only; a
    // key it said outranks it)
    assert!(on_plan(None, true) && !on_plan(Some("apiKeyHelper"), true));
}

/// `get_usage` is experimental: an error, a missing `rate_limits_available`, a `rate_limits` of another shape
/// are unknown (said why) and change no reading; `false` is a claude with no plan limits (a key).
#[test]
fn an_answer_tarae_cant_read_is_unknown_never_a_guess() {
    let ok = line(2)["response"].clone();
    let with = |f: &dyn Fn(&mut Value)| {
        let mut v = ok.clone();
        f(&mut v);
        of_answer(&v, BEFORE)
    };
    let unknown = |r: &Report| matches!(r.answer, Some(Answer::Unknown(_))) && r.readings.is_empty();
    assert!(unknown(&with(&|v| *v = json!({"subtype": "error", "error": "no such method"}))));
    assert!(unknown(&with(&|v| {
        v["response"].as_object_mut().unwrap().remove("rate_limits_available");
    })));
    assert!(unknown(&with(&|v| v["response"]["rate_limits"] = json!("changed"))));
    let none = with(&|v| v["response"]["rate_limits_available"] = json!(false));
    assert_eq!((none.answer, none.readings.len()), (Some(Answer::NotAvailable), 0));
    let mut l = Limits::default();
    l.seen(report_of(1, BEFORE));
    l.seen(with(&|v| v["response"]["rate_limits"] = json!("changed")));
    assert_eq!(l.get(WEEKLY, BEFORE).unwrap().percent, Some(92), "the reading before stays");
}

/// A window past its reset is unknown again — not its old percent, not 0 %; and a reading older than the one
/// held never replaces it (lines from two claudes cross).
#[test]
fn a_window_past_its_reset_is_unknown_and_the_latest_reading_wins() {
    let mut l = Limits::default();
    l.seen(report_of(2, BEFORE));
    assert!(l.get(WEEKLY, RESETS_MS - 1).is_some());
    assert!(l.get(WEEKLY, RESETS_MS).is_none() && l.known(RESETS_MS).iter().all(|w| w.window != WEEKLY));
    l.seen(report_of(1, BEFORE - 1000));
    assert_eq!(l.get(WEEKLY, BEFORE).unwrap().percent, Some(93), "the older event doesn't replace it");
}

/// The times `get_usage` writes, as unix ms: an offset or `Z`, a fraction cut to ms, minutes alone; anything
/// else is no time.
#[test]
fn iso_times_as_claude_writes_them() {
    for s in [
        "2026-10-13T01:00:00+00:00",
        "2026-10-13T01:00:00Z",
        "2026-10-13T10:00:00+09:00",
        "2026-10-12T20:00:00-05:00",
        "2026-10-13T01:00Z",
    ] {
        assert_eq!(iso_ms(s), Some(RESETS_MS), "{s}");
    }
    assert_eq!(iso_ms("2026-10-13T01:00:00.999999+00:00"), Some(RESETS_MS + 999));
    assert_eq!(iso_ms("1970-01-01T00:00:00Z"), Some(0));
    for bad in ["", "2026-10-13", "2026-13-01T00:00:00Z", "2026-10-13T01:00:00", "2026-10-13T01:00:00+0x:00"]
    {
        assert_eq!(iso_ms(bad), None, "{bad:?}");
    }
}

/// A retry (`system/api_retry`, Claude Code's headless docs' fields) is said as it goes — attempt, why, when.
#[test]
fn a_retry_says_its_attempt_and_why() {
    let v = json!({"type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 10,
                   "retry_delay_ms": 3500, "error_status": 429, "error": "rate_limit",
                   "uuid": "stand-in", "session_id": "stand-in"});
    let Some(StreamEv::Retry(r)) = crate::llm::usage_line(&v, 1) else { panic!("not read as a retry") };
    assert_eq!(r.said(), "API retry 2 of 10 — rate_limit (429), next in 4s");
    let none = json!({"type": "system", "subtype": "api_retry", "attempt": 1, "error_status": null,
                      "error": "overloaded"});
    assert_eq!(of_retry(&none).unwrap().said(), "API retry 1 — overloaded (no response)");
}

/// The windows by name, as people say them.
#[test]
fn windows_are_said_as_people_say_them() {
    let names = ["five_hour", "seven_day", "seven_day_opus", "seven_day:Stand-in", "overage"];
    let said: Vec<String> = names.iter().map(|n| label(n)).collect();
    assert_eq!(said, ["5-hour", "weekly", "weekly opus", "weekly Stand-in", "overage"]);
}

// ── In the editor ───────────────────────────────────────────────────────────

/// A project in its own temp folder, its chat's claude a stand-in in `sh` that writes what it's sent to `stdin`
/// and answers `script`'s cases (`case "$line" in …`).
fn chat_with(name: &str, cases: &str) -> (Editor, std::path::PathBuf) {
    let root = crate::scratch::Scratch::new(&format!("limits-{name}")).into_path();
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec![
        "-c".into(),
        format!(
            "while read -r line; do printf '%s\\n' \"$line\" >> stdin; case \"$line\" in {cases} esac; done"
        ),
    ];
    (ed, root)
}

fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(ed) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// A chat's claude is asked `get_usage` (no transcript scan) when nothing is fresh, its answer lands in the
/// account's limits — and the next claude within `FRESH_MS` isn't asked again.
#[test]
fn a_chat_asks_get_usage_only_when_nothing_is_fresh() {
    // (the fixture's answer under tarae's id, its reset far off — the test doesn't depend on today's date)
    let mut answer = line(2);
    answer["response"]["request_id"] = json!(REQUEST_ID);
    answer["response"]["response"]["rate_limits"]["seven_day"]["resets_at"] =
        json!("2099-01-06T01:00:00+00:00");
    let (mut ed, root) = chat_with("ask", &format!("*get_usage*) printf '%s\\n' '{answer}';;"));
    ed.limits.ask = true;
    crate::chat::open(&mut ed);
    until(&mut ed, "the answer", |ed| ed.limits.get(WEEKLY, crate::usage::now_ms()).is_some());
    let sent = std::fs::read_to_string(root.join("stdin")).unwrap();
    let asked: Vec<&str> = sent.lines().filter(|l| l.contains("get_usage")).collect();
    assert_eq!(asked.len(), 1, "{sent}");
    let q: Value = serde_json::from_str(asked[0]).unwrap();
    assert_eq!(q["request"], json!({"subtype": "get_usage", "skip_behaviors": true}));
    assert_eq!(ed.limits.get(WEEKLY, crate::usage::now_ms()).unwrap().percent, Some(93));
    // A new conversation's claude, within FRESH_MS: not asked
    crate::chat::reset(&mut ed);
    until(&mut ed, "the second claude's initialize", |_| {
        std::fs::read_to_string(root.join("stdin")).is_ok_and(|s| s.matches("tarae-init").count() == 2)
    });
    let sent = std::fs::read_to_string(root.join("stdin")).unwrap();
    assert_eq!(sent.matches("get_usage").count(), 1, "fresh: asked once — {sent}");
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// claude's retries are said in the chat as they come, each in place of the one before — a turn that retries
/// ten times is one row, not ten.
#[test]
fn retries_are_said_in_place_in_the_chat() {
    let retry = |n: u64| {
        json!({"type": "system", "subtype": "api_retry", "attempt": n, "max_retries": 10, "retry_delay_ms": 1000,
               "error_status": 529, "error": "overloaded"})
    };
    let (mut ed, root) =
        chat_with("retry", &format!("*'\"type\":\"user\"'*) printf '%s\\n' '{}' '{}';;", retry(1), retry(2)));
    crate::chat::open(&mut ed);
    ed.session.chat.as_mut().unwrap().input = "hello".into();
    crate::chat::send(&mut ed);
    let said = |ed: &Editor| -> Vec<String> {
        let c = ed.session.chat.as_ref().unwrap();
        c.msgs.iter().filter(|m| m.text.starts_with(RETRY_SAID)).map(|m| m.text.clone()).collect()
    };
    until(&mut ed, "the second retry", |ed| said(ed).iter().any(|s| s.starts_with("API retry 2")));
    assert_eq!(said(&ed), ["API retry 2 of 10 — overloaded (529), next in 1s"]);
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// The chat's last row says the weekly window — before any turn — as `weekly 93 % · resets Tue 10:00`, once the
/// session's claude said it runs on the plan: its answer to `initialize` says so as it starts
/// (`account.apiKeySource`). Not before it said (a key session's claude would show another session's reading of
/// the plan), nor while it runs on a key.
#[test]
fn the_chats_row_says_the_weekly_window_while_on_the_plan() {
    let mut ed = Editor::new(crate::config::Config::default());
    let initialized = json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": "tarae-init", "response": {"models": [], "account": {"apiKeySource": "none"}}}});
    // (first a claude that says nothing of its credential)
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut ed);
    let silent = ed.config.llm.args.clone();
    ed.config.llm.args = vec![
        "-c".into(),
        format!(
            "while read -r line; do case \"$line\" in *tarae-init*) printf '%s\\n' '{initialized}';; esac; done"
        ),
    ];
    let far = iso_ms("2099-01-06T01:00:00Z").unwrap();
    let weekly = Reading {
        window: WEEKLY.into(),
        percent: Some(93),
        resets_at_ms: Some(far),
        status: Some(Status::Warning),
        seen_at_ms: crate::usage::now_ms(),
        from: Source::Event,
    };
    let rows = |ed: &mut Editor| -> String {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 200, 40).unwrap();
        let mut vt = vt100::Parser::new(40, 200, 0);
        vt.process(&bytes);
        vt.screen().contents()
    };
    ed.limits.seen(Report { readings: vec![weekly], ..Report::default() });
    assert!(ed.session.chat.as_ref().unwrap().running() && silent != ed.config.llm.args);
    assert!(!rows(&mut ed).contains("weekly"), "its claude said no credential: no row");
    // (a new conversation — a new claude, one that answers initialize as Claude Code does)
    crate::chat::reset(&mut ed);
    until(&mut ed, "its answer to initialize", |ed| ed.session.usage.key_source.is_some());
    let shown = format!("weekly 93 % · resets {}", resets(far));
    assert!(rows(&mut ed).contains(&shown), "{shown:?} in\n{}", rows(&mut ed));
    assert!(!rows(&mut ed).contains("not reported"), "before a turn, the weekly window alone");
    ed.session.usage.key_source = Some("apiKeyHelper".into());
    assert!(!rows(&mut ed).contains("weekly"), "on a key: not this session's limits");
}

/// A new claude for the session hasn't said its credential yet: the last one's isn't carried over (it may run
/// on another), so until it says, nothing is shown as the plan's.
#[test]
fn a_new_claude_starts_with_no_credential_said() {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.session.usage.key_source = Some("none".into());
    ed.session.usage.plan_event = true;
    crate::chat::open(&mut ed);
    assert!(ed.session.chat.as_ref().unwrap().running());
    assert_eq!(ed.session.usage.key_source, None, "the last claude's credential isn't this one's");
    assert!(!ed.session.usage.plan_event, "nor its rate_limit_event");
}

/// A reading that names no reset (`get_usage`'s `five_hour` with nothing used) is current for `FRESH_MS` after
/// it was said, then unknown — never a stale number shown as current.
#[test]
fn a_reading_with_no_reset_is_current_only_while_fresh() {
    let mut l = Limits::default();
    l.seen(report_of(2, BEFORE));
    assert_eq!(l.get("five_hour", BEFORE + FRESH_MS - 1).and_then(|w| w.percent), Some(0));
    assert!(l.get("five_hour", BEFORE + FRESH_MS).is_none());
    assert!(l.known(BEFORE + FRESH_MS).iter().all(|w| w.window != "five_hour"));
    assert!(l.get(WEEKLY, BEFORE + FRESH_MS).is_some(), "one with a reset: until it");
}

/// A status said for one period never carries into another: a reading for the next week, or one naming no
/// reset, without a status of its own has none — an old `rejected` would say `limit reached` all next week.
#[test]
fn a_status_never_carries_into_another_period() {
    let rejected = Reading {
        window: WEEKLY.into(),
        percent: Some(100),
        resets_at_ms: Some(RESETS_MS),
        status: Some(Status::Rejected),
        seen_at_ms: BEFORE,
        from: Source::Event,
    };
    let next = |resets_at_ms: Option<u64>| Reading {
        percent: Some(3),
        resets_at_ms,
        status: None,
        seen_at_ms: BEFORE + 1000,
        from: Source::Usage,
        ..rejected.clone()
    };
    for (what, resets) in [("the next week", Some(RESETS_MS + 7 * 86_400_000)), ("no reset named", None)] {
        let mut l = Limits::default();
        l.seen(Report { readings: vec![rejected.clone()], ..Report::default() });
        l.seen(Report { readings: vec![next(resets)], ..Report::default() });
        let w = l.get(WEEKLY, BEFORE + 2000).unwrap();
        assert_eq!((w.status, used(w).as_str()), (None, "3 %"), "{what}");
    }
}

/// frictions29 #81: the event a claude sends as it starts names a window with no figure — it keeps the figure said
/// for that period, as old as it is (then `get_usage` is asked again: no fresh figure), never one of another period.
#[test]
fn an_event_with_no_figure_keeps_the_one_said_and_isnt_fresh() {
    let said = Reading {
        window: "five_hour".into(),
        percent: Some(4),
        resets_at_ms: Some(RESETS_MS),
        status: None,
        seen_at_ms: BEFORE,
        from: Source::Usage,
    };
    let event = |resets_at_ms| Reading {
        percent: None,
        resets_at_ms,
        status: Some(Status::Allowed),
        seen_at_ms: BEFORE + FRESH_MS * 2,
        from: Source::Event,
        ..said.clone()
    };
    let mut l = Limits::default();
    l.seen(Report { readings: vec![said.clone()], ..Report::default() });
    l.seen(Report { readings: vec![event(Some(RESETS_MS))], ..Report::default() });
    let w = l.get("five_hour", BEFORE + FRESH_MS * 2).unwrap();
    assert_eq!((used(w).as_str(), w.seen_at_ms, w.status), ("4 %", BEFORE, Some(Status::Allowed)));
    assert!(!l.fresh(BEFORE + FRESH_MS * 2), "a figure that old isn't fresh");
    // Another period: no figure carried
    let mut l = Limits::default();
    l.seen(Report { readings: vec![said.clone()], ..Report::default() });
    l.seen(Report { readings: vec![event(Some(RESETS_MS + 5 * 3_600_000))], ..Report::default() });
    let w = l.get("five_hour", BEFORE + FRESH_MS * 2).unwrap();
    assert_eq!(used(w), "no figure yet");
    assert!(!l.fresh(BEFORE + FRESH_MS * 2), "no figure: not fresh");
}

/// frictions29 #81: with the chat's claude running and a figure fresh for another window, an event naming a window
/// with none asks `get_usage` again — once while that question is young.
#[test]
fn an_event_with_no_figure_asks_get_usage() {
    let mut answer = line(2);
    answer["response"]["request_id"] = json!(REQUEST_ID);
    answer["response"]["response"]["rate_limits"]["seven_day"]["resets_at"] =
        json!("2099-01-06T01:00:00+00:00");
    let (mut ed, root) = chat_with("no-figure", &format!("*get_usage*) printf '%s\\n' '{answer}';;"));
    ed.limits.ask = true;
    crate::chat::open(&mut ed);
    until(&mut ed, "the answer", |ed| ed.limits.get(WEEKLY, crate::usage::now_ms()).is_some());
    let asked = |root: &std::path::Path| {
        std::fs::read_to_string(root.join("stdin")).unwrap().matches("get_usage").count()
    };
    assert_eq!(asked(&root), 1);
    let event = |at| Reading {
        window: "five_hour".into(),
        percent: None,
        resets_at_ms: None,
        status: Some(Status::Allowed),
        seen_at_ms: at,
        from: Source::Event,
    };
    // (the question just asked is young: not again)
    seen(&mut ed, Report { readings: vec![event(crate::usage::now_ms())], ..Report::default() });
    assert_eq!(asked(&root), 1, "young: not again");
    ed.limits.asked_ms = ed.limits.asked_ms.map(|t| t - FRESH_MS * 2);
    seen(&mut ed, Report { readings: vec![event(crate::usage::now_ms())], ..Report::default() });
    until(&mut ed, "asked again", |_| asked(&root) == 2);
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// A window with a reset and no figure says what's known, not "not said" beside a time (frictions29 6).
#[test]
fn a_window_with_no_figure_says_so_beside_its_reset() {
    let r = Reading {
        window: "five_hour".into(),
        percent: None,
        resets_at_ms: Some(RESETS_MS),
        status: None,
        seen_at_ms: BEFORE,
        from: Source::Event,
    };
    assert_eq!(said(&r), format!("5-hour no figure yet · resets {}", resets(RESETS_MS)));
    assert_eq!(used(&Reading { percent: Some(4), ..r }), "4 %");
}

/// Opening `:usage` asks the running chat's claude `get_usage` when nothing is fresh — a control request, which
/// claude answers alongside a turn under way (as it does `interrupt`) — not again while it's fresh, never twice
/// while that claude hasn't answered (in flight, however long), and again once it answered and nothing is fresh.
#[test]
fn opening_usage_asks_get_usage_only_when_nothing_is_fresh_and_never_twice_in_flight() {
    let (mut ed, root) = chat_with("usage", "");
    crate::chat::open(&mut ed);
    until(&mut ed, "its initialize", |_| {
        std::fs::read_to_string(root.join("stdin")).is_ok_and(|s| s.contains("tarae-init"))
    });
    // (the start asked nothing: tests don't ask unless they say)
    ed.limits.ask = true;
    // `:usage`, then a sentinel line after it: once the stand-in wrote the sentinel, whatever `:usage` sent is
    // written too — how many times `get_usage` went, exactly
    let mut n = 0;
    let mut asked = |ed: &mut Editor| -> usize {
        crate::usage::open(ed);
        n += 1;
        let sentinel = format!("{{\"sentinel\":{n}}}");
        assert!(ed.session.chat.as_ref().unwrap().to_claude(sentinel.clone()));
        until(ed, "the sentinel", |_| {
            std::fs::read_to_string(root.join("stdin")).is_ok_and(|s| s.contains(&sentinel))
        });
        std::fs::read_to_string(root.join("stdin")).unwrap().matches("get_usage").count()
    };
    assert_eq!(asked(&mut ed), 1, "nothing fresh: asked");
    assert_eq!(asked(&mut ed), 1, "fresh: not asked again");
    // No longer fresh, still unanswered: in flight — not asked twice
    let stale = crate::usage::now_ms() - FRESH_MS - 1;
    ed.limits.asked_ms = Some(stale);
    assert_eq!(asked(&mut ed), 1, "in flight: not asked twice");
    // Answered (no plan limits for it — nothing fresh), and the question long ago: asked again
    seen(
        &mut ed,
        of_answer(&json!({"subtype": "success", "response": {"rate_limits_available": false}}), stale),
    );
    ed.limits.asked_ms = Some(stale);
    assert_eq!(asked(&mut ed), 2, "answered, nothing fresh: asked again");
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// A session's own claude that sent a `rate_limit_event` — which Claude Code sends to claude.ai plan users only —
/// is on the plan before it says its credential: its row says the weekly window. Another claude's event (the
/// reviewer's, another session's) puts no session on the plan but its own.
#[test]
fn its_own_rate_limit_event_puts_a_session_on_the_plan() {
    let mut event = line(1);
    event["rate_limit_info"]["resetsAt"] = json!(4_071_000_000u64);
    let (mut ed, root) = chat_with("event", &format!("*tarae-init*) printf '%s\\n' '{event}';;"));
    let rows = |ed: &mut Editor| -> String {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 200, 40).unwrap();
        let mut vt = vt100::Parser::new(40, 200, 0);
        vt.process(&bytes);
        vt.screen().contents()
    };
    // (another claude's reading of the plan, this session's claude not started yet)
    seen(&mut ed, of_event(&event, crate::usage::now_ms()).unwrap());
    assert!(!ed.session.usage.plan_event);
    crate::chat::open(&mut ed);
    until(&mut ed, "its own event", |ed| ed.session.usage.plan_event);
    assert_eq!(ed.session.usage.key_source, None, "it said no credential");
    assert!(rows(&mut ed).contains("weekly 92 %"), "{}", rows(&mut ed));
    drop(ed);
    std::fs::remove_dir_all(&root).ok();
}

/// After a turn the row is crowded (the cache, the context, the turn's cost): near the limit the weekly window's
/// reset outlasts the cost and the context — when it's back is what matters then — and far from it the reset goes
/// first, the row as it was before limits were said.
#[test]
fn near_the_limit_the_reset_outlasts_the_turns_cost() {
    use crate::usage::{Counts, Seen, Spent, Ttl};
    let row = |percent: u32, status: Status| -> String {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let now = crate::usage::now_ms();
        let u = &mut ed.session.usage;
        // (its claude said it runs on the plan)
        u.key_source = Some("none".into());
        u.fresh("c");
        let counts = Counts { input: Some(18), write: Some(8125), read: Some(7924), output: Some(185) };
        u.seen(Seen { id: Some("m".into()), model: "m".into(), counts, ttl: Some(Ttl::Hour1), at_ms: now });
        let models = vec![("m".to_string(), Some(200_000))];
        u.spent(Spent {
            conversation: "c".into(),
            counts,
            total_cost: Some(0.018),
            models,
            alone: None,
            ms: None,
            at_ms: now,
        });
        let far = iso_ms("2099-01-06T01:00:00Z").unwrap();
        let weekly = Reading {
            window: WEEKLY.into(),
            percent: Some(percent),
            resets_at_ms: Some(far),
            status: Some(status),
            seen_at_ms: now,
            from: Source::Event,
        };
        ed.limits.seen(Report { readings: vec![weekly], ..Report::default() });
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 150, 30).unwrap();
        let mut vt = vt100::Parser::new(30, 150, 0);
        vt.process(&bytes);
        vt.screen().rows(0, 150).find(|r| r.contains("warm until")).expect("the row").trim().to_string()
    };
    let near = row(92, Status::Warning);
    assert!(near.contains("weekly 92 % · resets") && !near.contains('$'), "{near:?}");
    let far = row(30, Status::Allowed);
    assert!(far.contains("weekly 30 %") && far.contains("≈$0.018") && !far.contains("resets"), "{far:?}");
}

/// A fake claude whose answer to `initialize` says it runs on the plan, on `email`'s login in `org` — and whose
/// turn says its account's weekly window is at 50 % (a `rate_limit_event`), then ends a moment later.
fn on_login(ed: &mut Editor, email: &str, org: &str) {
    let initialized = json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": "tarae-init", "response": {"models": [], "account": {"email": email, "organization": org,
        "subscriptionType": "max", "tokenSource": "claude.ai", "apiKeySource": "none"}}}});
    let event = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed",
        "resetsAt": 4_071_000_000u64, "rateLimitType": "seven_day", "utilization": 0.5}});
    let result = json!({"type": "result", "result": "ok"});
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec![
        "-c".into(),
        format!(
            "while read -r line; do case \"$line\" in *tarae-init*) printf '%s\\n' '{initialized}';; \
             '{{\"message\"'*) printf '%s\\n' '{event}'; sleep 0.3; printf '%s\\n' '{result}';; esac; done"
        ),
    ];
}

/// frictions29 #3 + #7: which account a chat runs on — its organization and plan in `:usage`, `tarae state` and
/// (the plan) on the chat's last row, never the email; a claude on another login makes the limits unknown and is
/// said on the chat still on the old one; `:claude-restart` starts every chat's claude again on its
/// conversation.
#[test]
fn a_chat_says_its_account_and_another_login_is_said_and_restarted_into() {
    let scratch = crate::scratch::Scratch::new("limits-login");
    let mut ed = Editor::new(crate::config::Config::default());
    on_login(&mut ed, "first@example.test", "Acme");
    crate::chat::open(&mut ed);
    until(&mut ed, "its account", |ed| ed.session.usage.account.is_some());
    let a = ed.session.usage.account.clone().unwrap();
    assert_eq!((a.organization.as_deref(), a.plan.as_deref()), (Some("Acme"), Some("max")));
    // Said: the organization and the plan — the email nowhere
    let far = iso_ms("2099-01-06T01:00:00Z").unwrap();
    let weekly = Reading {
        window: WEEKLY.into(),
        percent: Some(93),
        resets_at_ms: Some(far),
        status: None,
        seen_at_ms: crate::usage::now_ms(),
        from: Source::Event,
    };
    ed.limits.seen(Report { readings: vec![weekly], ..Report::default() });
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 200, 40).unwrap();
    let mut vt = vt100::Parser::new(40, 200, 0);
    vt.process(&bytes);
    let shown = vt.screen().contents();
    assert!(shown.contains("max · weekly 93 %"), "{shown}");
    let now = crate::usage::now_ms();
    let r = crate::usage::report("s", &ed.session.usage, &ed.limits, &[], now);
    assert!(r.contains("Account: Acme · plan max."), "{r}");
    let state = ed.ui_state().json();
    assert_eq!(state["sessions"][0]["account"], json!({"organization": "Acme", "plan": "max"}));
    for said in [shown, r, state.to_string(), ed.ui_state_claude().json().to_string(), format!("{a:?}")] {
        assert!(!said.contains("first@"), "the email: {said}");
    }
    // Another session's claude on another login: the limits unknown, said on the chat still on the first
    let first = ed.session.id;
    let other = ed.session_open(scratch.path()).unwrap();
    ed.session_switch(other);
    on_login(&mut ed, "second@example.test", "Acme");
    crate::chat::open(&mut ed);
    until(&mut ed, "the other's account", |ed| ed.session.usage.account.is_some());
    assert!(ed.limits.get(WEEKLY, crate::usage::now_ms()).is_none(), "the other account's limits");
    ed.session_switch(first);
    let note = "another Claude login since this claude started — :claude-restart moves every chat to it";
    let c = ed.session.chat.as_ref().unwrap();
    assert!(
        c.msgs.iter().any(|m| m.text == note),
        "{:?}",
        c.msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    let r = crate::usage::report("s", &ed.session.usage, &ed.limits, &[], now);
    assert!(r.contains("Another login since this claude started"), "{r}");
    // The new login's reading (93 %): its own chat's row shows it, the one still on the old login doesn't
    let rows = |ed: &mut Editor| -> String {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 200, 40).unwrap();
        let mut vt = vt100::Parser::new(40, 200, 0);
        vt.process(&bytes);
        vt.screen().contents()
    };
    let weekly = Reading {
        window: WEEKLY.into(),
        percent: Some(93),
        resets_at_ms: Some(far),
        status: None,
        seen_at_ms: crate::usage::now_ms(),
        from: Source::Event,
    };
    ed.limits.seen(Report { readings: vec![weekly], ..Report::default() });
    assert!(!rows(&mut ed).contains("weekly"), "the old login's row: {}", rows(&mut ed));
    ed.session_switch(other);
    assert!(rows(&mut ed).contains("max · weekly 93 %"), "{}", rows(&mut ed));
    ed.session_switch(first);
    // A turn of the chat still on the old login: its claude's reading (50 %) is that account's — not kept
    let generation = ed.session.chat.as_ref().unwrap().generation();
    assert!(crate::chat::submit(&mut ed, "hello", None, None));
    // `:claude-restart` meanwhile: the idle one (the other) started again at once, this busy one at its turn's
    // end — counted apart (typed in the code, not the chat's input)
    ed.session.chat.as_mut().unwrap().focused = false;
    for c in ":claude-restart".chars() {
        ed.handle_key(crate::key::Key::plain(crate::key::Code::Char(c)));
    }
    ed.handle_key(crate::key::Key::plain(crate::key::Code::Enter));
    let status = "1 chat started again on the current login · 1 once its turn ends";
    assert!(ed.messages.iter().any(|m| m.2 == status), "{:?}", ed.messages);
    assert_eq!(ed.session.chat.as_ref().unwrap().generation(), generation, "not before its turn ends");
    until(&mut ed, "a new claude after its turn", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| c.generation() != generation)
    });
    let kept = ed.limits.get(WEEKLY, crate::usage::now_ms()).and_then(|r| r.percent);
    assert_eq!(kept, Some(93), "the old login's 50 % isn't the new account's");
    until(&mut ed, "its account again", |ed| {
        ed.session.usage.account.as_ref().is_some_and(|b| b.login != a.login)
    });
}

/// Review of frictions31: `:claude-restart` with no chat running says so; with only a busy one, that it starts
/// again once its turn ends (in words that agree) — and it does, not before.
#[test]
fn claude_restart_says_what_it_did_with_none_and_with_a_busy_one() {
    let mut ed = Editor::new(crate::config::Config::default());
    crate::chat::restart_all(&mut ed);
    assert!(
        ed.messages.iter().any(|m| m.2 == "no chat's claude is running — each starts on the current login")
    );
    on_login(&mut ed, "first@example.test", "Acme");
    crate::chat::open(&mut ed);
    until(&mut ed, "its account", |ed| ed.session.usage.account.is_some());
    let generation = ed.session.chat.as_ref().unwrap().generation();
    assert!(crate::chat::submit(&mut ed, "hello", None, None));
    crate::chat::restart_all(&mut ed);
    let said = "1 chat starts again on the current login once its turn ends";
    assert!(ed.messages.iter().any(|m| m.2 == said), "{:?}", ed.messages);
    assert_eq!(ed.session.chat.as_ref().unwrap().generation(), generation, "not before its turn ends");
    until(&mut ed, "a new claude", |ed| {
        ed.session.chat.as_ref().is_some_and(|c| c.generation() != generation)
    });
}

/// Review of frictions31: a claude started again on an API key says no account (no email, no organization) —
/// the last one's organization and plan don't stand in `:usage`, the state or the row.
#[test]
fn a_claude_started_on_a_key_keeps_no_account_of_before() {
    let mut ed = Editor::new(crate::config::Config::default());
    on_login(&mut ed, "first@example.test", "Acme");
    crate::chat::open(&mut ed);
    until(&mut ed, "its account", |ed| ed.session.usage.account.is_some());
    let initialized = json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": "tarae-init", "response": {"models": [], "account": {"apiKeySource": "apiKeyHelper"}}}});
    ed.config.llm.args = vec![
        "-c".into(),
        format!(
            "while read -r line; do case \"$line\" in *tarae-init*) printf '%s\\n' '{initialized}';; esac; done"
        ),
    ];
    crate::chat::reset(&mut ed);
    until(&mut ed, "its credential", |ed| ed.session.usage.key_source.as_deref() == Some("apiKeyHelper"));
    assert_eq!(ed.session.usage.account, None);
    let r = crate::usage::report("s", &ed.session.usage, &ed.limits, &[], crate::usage::now_ms());
    assert!(!r.contains("Account:"), "{r}");
    assert_eq!(ed.ui_state().json()["sessions"][0]["account"], Value::Null);
}

/// `tarae state` says the account's `limits` (whole percents, unix ms) and each session's `key_source_seen`;
/// `:usage` says the windows, the credential, and — on a key — that Console is where billing is authoritative.
#[test]
fn the_state_and_usage_say_the_limits_and_the_credential() {
    let mut ed = Editor::new(crate::config::Config::default());
    let far = iso_ms("2099-01-06T01:00:00Z").unwrap();
    let now = crate::usage::now_ms();
    let reading = |window: &str, percent| Reading {
        window: window.into(),
        percent: Some(percent),
        resets_at_ms: Some(far),
        status: None,
        seen_at_ms: now,
        from: Source::Usage,
    };
    ed.limits.seen(Report {
        readings: vec![reading("seven_day", 93), reading("five_hour", 4)],
        ..Report::default()
    });
    ed.session.usage.key_source = Some("none".into());
    let v = ed.ui_state().json();
    assert_eq!(
        v["limits"]["seven_day"],
        json!({"percent": 93, "resets_at": far, "status": null, "seen_at": now,
                                                 "from": "get_usage"})
    );
    assert_eq!(v["limits"]["five_hour"]["percent"], 4);
    assert_eq!(v["sessions"][0]["key_source_seen"], "none");
    let report = |ed: &Editor| crate::usage::report("s", &ed.session.usage, &ed.limits, &[], now);
    let r = report(&ed);
    assert!(r.contains("| 5-hour ") && r.contains("| weekly ") && r.contains("| 93 %"), "{r}");
    assert!(r.contains("`apiKeySource` `none`") && !r.contains("Console"), "{r}");
    ed.session.usage.key_source = Some("apiKeyHelper".into());
    assert!(report(&ed).contains("Console, not tarae, is where billing is authoritative"), "{}", report(&ed));
}
