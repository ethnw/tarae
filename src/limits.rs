//! What claude says of the plan's limits, of retries, and of the credential it runs on (apibill D2/D5's
//! subscription half; sources in `target/sb/apibill-design.md` §2):
//!
//! - **`rate_limit_event`** (`rate_limit_info`): `status` `allowed` · `allowed_warning` · `rejected`,
//!   `rateLimitType` (the window: `five_hour`, `seven_day`, `seven_day_<family>` …), `utilization` a *fraction*
//!   (0.92), `resetsAt` unix *seconds*. Claude Code sends one when a claude starts too, not only on a change (L2).
//! - **`get_usage`**, a control request (`skip_behaviors: true` — no transcript scan), answered with
//!   `rate_limits.<window>` = `{utilization, resets_at}`: a *percent* (93) and an ISO 8601 time;
//!   `rate_limits_available: false` for a key or a cloud provider. Claude Code calls it experimental — "may change
//!   or be removed in any release": an answer tarae can't read is *unknown*, never a guess, and said so.
//! - **`system/api_retry`**: a request failed and claude retries — attempt, the most it tries, when, why.
//! - **`system/init`'s `apiKeySource`**: `none` (claude.ai login, a token, a cloud provider), `apiKeyHelper`,
//!   `ANTHROPIC_API_KEY`, `/login managed key` — the credential claude reported it runs on.
//!
//! The limits are the account's, not a session's: one `Limits` (`Editor::limits`), each window's latest reading
//! winning whichever claude said it — the two units brought to whole percents. A window past its reset is
//! unknown again. Fields claude adds that these don't name are left alone (L2 saw `unifiedWindows` and
//! `seven_day_breakdown`).

use serde_json::{Value, json};

use crate::editor::Editor;

/// The request id tarae asks `get_usage` under — its answer comes back with it.
pub const REQUEST_ID: &str = "tarae-usage";

/// A reading younger than this, or a question asked that recently, isn't asked for again.
pub const FRESH_MS: u64 = 10 * 60_000;

/// The weekly window over every model — what the chat's row says.
pub const WEEKLY: &str = "seven_day";

/// `get_usage`'s windows that are one `{utilization, resets_at}` each (`SDKControlGetUsageResponse`) — the
/// per-model rows of `model_scoped` come as `seven_day:<their name>`.
const WINDOWS: [&str; 5] =
    ["five_hour", "seven_day", "seven_day_oauth_apps", "seven_day_opus", "seven_day_sonnet"];

/// How near the limit a window is, as `rate_limit_event` says (`get_usage` doesn't).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Allowed,
    Warning,
    Rejected,
}

impl Status {
    fn of(s: &str) -> Option<Status> {
        match s {
            "allowed" => Some(Status::Allowed),
            "allowed_warning" => Some(Status::Warning),
            "rejected" => Some(Status::Rejected),
            _ => None,
        }
    }

    /// Claude Code's own word.
    pub fn name(self) -> &'static str {
        match self {
            Status::Allowed => "allowed",
            Status::Warning => "allowed_warning",
            Status::Rejected => "rejected",
        }
    }
}

/// Where a reading came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Event,
    Usage,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Event => "rate_limit_event",
            Source::Usage => "get_usage",
        }
    }
}

/// One window as one line said it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub window: String,
    /// Whole percent used — None: the line didn't say.
    pub percent: Option<u32>,
    /// When the window resets (unix ms) — None: not said.
    pub resets_at_ms: Option<u64>,
    pub status: Option<Status>,
    pub seen_at_ms: u64,
    pub from: Source,
}

/// How `get_usage` answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Read — its windows are among the readings.
    Read,
    /// No plan limits for this claude (a key, a cloud provider — `rate_limits_available: false`).
    NotAvailable,
    /// An answer tarae can't read (an error, a shape that changed): why.
    Unknown(String),
}

/// What one line said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub readings: Vec<Reading>,
    pub answer: Option<Answer>,
    /// The plan `get_usage` named (`subscription_type`: `max`, `pro` …).
    pub plan: Option<String>,
}

/// Every window known, the account's.
#[derive(Debug, Default)]
pub struct Limits {
    pub windows: Vec<Reading>,
    /// `get_usage`'s last answer, and when.
    pub answer: Option<(u64, Answer)>,
    pub plan: Option<String>,
    /// When tarae last asked `get_usage`.
    pub asked_ms: Option<u64>,
    /// The chat claude (its process's generation) asked `get_usage` and not answered yet.
    pub pending: Option<u64>,
    /// tarae asks `get_usage` itself — off in tests but their own (a stand-in claude's input stays as each test
    /// writes it).
    pub ask: bool,
}

impl Limits {
    pub fn new() -> Limits {
        Limits { ask: !cfg!(test), ..Limits::default() }
    }

    /// A line's readings: each window's latest wins. One that doesn't say the status keeps the one said for the
    /// same period (the event says it, `get_usage` doesn't). Whether anything changed.
    pub fn seen(&mut self, r: Report) -> bool {
        let mut changed = false;
        for new in r.readings {
            match self.windows.iter_mut().find(|w| w.window == new.window) {
                Some(old) if old.seen_at_ms > new.seen_at_ms => {}
                Some(old) => {
                    let status = new.status.or(old.status.filter(|_| same_period(old, &new)));
                    let new = Reading { status, ..new };
                    changed |= (old.percent, old.resets_at_ms, old.status)
                        != (new.percent, new.resets_at_ms, status);
                    *old = new;
                }
                None => {
                    self.windows.push(new);
                    changed = true;
                }
            }
        }
        if let Some(a) = r.answer {
            // (answered: no question in flight)
            self.pending = None;
            let at = self.windows.iter().map(|w| w.seen_at_ms).max().unwrap_or(0);
            changed |= self.answer.as_ref().is_none_or(|(_, old)| *old != a);
            self.answer = Some((at, a));
        }
        if r.plan.is_some() {
            self.plan = r.plan;
        }
        changed
    }

    /// Window `name`, while it's current (`current`) — else unknown, not 0 %.
    pub fn get(&self, name: &str, now_ms: u64) -> Option<&Reading> {
        self.windows.iter().find(|w| w.window == name && current(w, now_ms))
    }

    /// Every window current now: the 5-hour, the weekly, then the rest by name.
    pub fn known(&self, now_ms: u64) -> Vec<&Reading> {
        let mut v: Vec<&Reading> = self.windows.iter().filter(|w| current(w, now_ms)).collect();
        let rank = |w: &str| match w {
            "five_hour" => 0,
            WEEKLY => 1,
            _ => 2,
        };
        crate::sorting::by(&mut v, |a, b| (rank(&a.window), &a.window).cmp(&(rank(&b.window), &b.window)));
        v
    }

    /// Whether `get_usage` need not be asked: a reading, or a question, younger than `FRESH_MS`.
    pub fn fresh(&self, now_ms: u64) -> bool {
        let young = |at: u64| now_ms.saturating_sub(at) < FRESH_MS;
        self.windows.iter().any(|w| young(w.seen_at_ms)) || self.asked_ms.is_some_and(young)
    }

    /// The state's `limits` (`json`) as of `now_ms`.
    pub fn json(&self, now_ms: u64) -> Value {
        json(&self.known(now_ms))
    }
}

/// The state's `limits`: each window by its name — `percent`, `resets_at` and `seen_at` (unix ms), `status`
/// (Claude Code's word, or null: not said), `from`.
pub fn json(readings: &[&Reading]) -> Value {
    let mut out = serde_json::Map::new();
    for w in readings {
        out.insert(
            w.window.clone(),
            json!({ "percent": w.percent, "resets_at": w.resets_at_ms, "status": w.status.map(Status::name),
                    "seen_at": w.seen_at_ms, "from": w.from.name() }),
        );
    }
    Value::Object(out)
}

/// A reading still says where its window stands: until its reset — or, one that names none (`get_usage`'s
/// `five_hour` with nothing used: `resets_at` null), for `FRESH_MS` after it was said; then it's unknown until
/// something says it again (and, nothing fresh, `get_usage` is asked again).
fn current(w: &Reading, now_ms: u64) -> bool {
    match w.resets_at_ms {
        Some(at) => at > now_ms,
        None => now_ms.saturating_sub(w.seen_at_ms) < FRESH_MS,
    }
}

/// The two readings are of one period of the window: their resets agree (to a minute — seconds and ISO
/// 8601's fraction).
fn same_period(a: &Reading, b: &Reading) -> bool {
    matches!((a.resets_at_ms, b.resets_at_ms), (Some(x), Some(y)) if x.abs_diff(y) < 60_000)
}

/// `weekly` · `5-hour` · `weekly opus` · `weekly <model_scoped's name>` — a window as people say it.
pub fn label(window: &str) -> String {
    match window {
        "five_hour" => "5-hour".into(),
        WEEKLY => "weekly".into(),
        w => match w.strip_prefix("seven_day:").or_else(|| w.strip_prefix("seven_day_")) {
            Some(rest) => format!("weekly {}", rest.replace('_', " ")),
            None => w.replace('_', " "),
        },
    }
}

/// `93 %` · `limit reached` (rejected) · `not said`.
pub fn used(r: &Reading) -> String {
    match (r.status, r.percent) {
        (Some(Status::Rejected), _) => "limit reached".into(),
        (_, Some(p)) => format!("{p} %"),
        (_, None) => "not said".into(),
    }
}

/// `weekly 93 % · resets Tue 10:00` — a window whole, as the chat's row says it given room.
pub fn said(r: &Reading) -> String {
    let resets = r.resets_at_ms.map(|at| format!(" · resets {}", resets(at))).unwrap_or_default();
    format!("{} {}{resets}", label(&r.window), used(r))
}

/// Near enough the limit to say so in the warning colour: claude warns, or 80 % and over.
pub fn pressing(r: &Reading) -> bool {
    matches!(r.status, Some(Status::Warning | Status::Rejected)) || r.percent.is_some_and(|p| p >= 80)
}

/// `Tue 10:00` — when a window resets, local time.
pub fn resets(at_ms: u64) -> String {
    crate::log::day_clock(std::time::UNIX_EPOCH + std::time::Duration::from_millis(at_ms))
}

/// A `rate_limit_event` line, read at `at_ms` — None: not one, or no window named.
pub fn of_event(ev: &Value, at_ms: u64) -> Option<Report> {
    if ev["type"] != "rate_limit_event" {
        return None;
    }
    let i = &ev["rate_limit_info"];
    let reading = Reading {
        window: i["rateLimitType"].as_str()?.to_string(),
        // (a fraction: 0.92 is 92 %)
        percent: i["utilization"].as_f64().and_then(|u| whole(u * 100.0)),
        resets_at_ms: i["resetsAt"].as_f64().filter(|s| *s > 0.0).map(|s| (s * 1000.0) as u64),
        status: i["status"].as_str().and_then(Status::of),
        seen_at_ms: at_ms,
        from: Source::Event,
    };
    Some(Report { readings: vec![reading], ..Report::default() })
}

/// `get_usage`'s answer (the `control_response`'s `response`: `subtype`, `response`), read at `at_ms`.
pub fn of_answer(response: &Value, at_ms: u64) -> Report {
    let unknown = |why: &str| Report { answer: Some(Answer::Unknown(why.into())), ..Report::default() };
    if response["subtype"] != "success" {
        let said = response["error"].as_str().unwrap_or("an error");
        return unknown(&format!("claude answered {said}"));
    }
    let r = &response["response"];
    let plan = r["subscription_type"].as_str().map(String::from);
    match r["rate_limits_available"].as_bool() {
        Some(false) => return Report { answer: Some(Answer::NotAvailable), plan, ..Report::default() },
        Some(true) => {}
        None => return unknown("no rate_limits_available in it"),
    }
    let Some(limits) = r["rate_limits"].as_object() else {
        return unknown("its rate_limits isn't an object");
    };
    // (a percent: 93 is 93 %; a window that's null, or says no utilization, is unknown — never 0 %)
    let reading = |window: String, w: &Value| {
        Some(Reading {
            window,
            percent: Some(whole(w["utilization"].as_f64()?)?),
            resets_at_ms: w["resets_at"].as_str().and_then(iso_ms),
            status: None,
            seen_at_ms: at_ms,
            from: Source::Usage,
        })
    };
    let mut readings: Vec<Reading> =
        WINDOWS.iter().filter_map(|&k| reading(k.to_string(), limits.get(k)?)).collect();
    for m in limits.get("model_scoped").and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = m["display_name"].as_str() {
            readings.extend(reading(format!("seven_day:{name}"), m));
        }
    }
    Report { readings, answer: Some(Answer::Read), plan }
}

/// A percent rounded to a whole one — None: not a number one can be (negative, not finite).
fn whole(p: f64) -> Option<u32> {
    (p.is_finite() && p >= 0.0).then(|| p.round().min(u32::MAX as f64) as u32)
}

/// The `get_usage` question (no transcript scan).
pub fn request() -> String {
    json!({"type": "control_request", "request_id": REQUEST_ID,
           "request": {"subtype": "get_usage", "skip_behaviors": true}})
    .to_string()
}

/// An ISO 8601 time (`2026-10-13T01:00:00.123+00:00`, `…Z`) as unix ms — None: not one.
pub fn iso_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    if !at(4, b'-') || !at(7, b'-') || !matches!(b.get(10), Some(b'T' | b't' | b' ')) || !at(13, b':') {
        return None;
    }
    let (y, mo, d, h, mi) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?);
    let (mut i, mut sec, mut ms) = (16, 0, 0);
    if at(i, b':') {
        sec = num(17..19)?;
        i = 19;
        if at(i, b'.') {
            let end = (i + 1..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
            let digits = format!("{:0<3}", &s[i + 1..end]);
            ms = digits[..3].parse::<i64>().ok()?;
            i = end;
        }
    }
    let offset_min = match s.get(i..)? {
        "Z" | "z" => 0,
        tz => {
            let sign = match tz.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hm = tz[1..].replace(':', "");
            if hm.len() != 4 || !hm.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            sign * (hm[..2].parse::<i64>().ok()? * 60 + hm[2..].parse::<i64>().ok()?)
        }
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    u64::try_from(secs * 1000 + ms).ok()
}

/// Days from 1970-01-01 to a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ── Retries ─────────────────────────────────────────────────────────────────

/// A request failed and claude retries it (`system/api_retry`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retry {
    pub attempt: u64,
    pub max: Option<u64>,
    pub delay_ms: Option<u64>,
    /// The HTTP status — None: no response.
    pub status: Option<u64>,
    /// Claude Code's category (`rate_limit`, `overloaded`, `billing_error` …).
    pub error: String,
}

/// The line's retry — None: not one.
pub fn of_retry(ev: &Value) -> Option<Retry> {
    if ev["type"] != "system" || ev["subtype"] != "api_retry" {
        return None;
    }
    Some(Retry {
        attempt: ev["attempt"].as_u64()?,
        max: ev["max_retries"].as_u64(),
        delay_ms: ev["retry_delay_ms"].as_u64(),
        status: ev["error_status"].as_u64(),
        error: ev["error"].as_str().unwrap_or("unknown").to_string(),
    })
}

/// What starts the chat's retry note — the next one takes its place.
pub const RETRY_SAID: &str = "API retry";

impl Retry {
    /// `API retry 2 of 10 — rate_limit (429), next in 4s`.
    pub fn said(&self) -> String {
        let of = self.max.map(|m| format!(" of {m}")).unwrap_or_default();
        let status = self.status.map(|s| format!(" ({s})")).unwrap_or_else(|| " (no response)".into());
        let next = self.delay_ms.map(|d| format!(", next in {}s", d.div_ceil(1000))).unwrap_or_default();
        format!("{RETRY_SAID} {}{of} — {}{status}{next}", self.attempt, self.error)
    }
}

// ── The credential ──────────────────────────────────────────────────────────

/// Whether the session's own claude said it runs on the plan — so the plan's limits are its:
/// - its `apiKeySource` is `none` (a claude.ai login, a token, a cloud provider) — in its answer to `initialize`
///   as it starts (`account.apiKeySource`), and in every turn's `init`; another source is a key, billed to its
///   Console organization: not the plan, whatever else came;
/// - none said yet, it sent a `rate_limit_event` itself (`plan_event`): Claude Code sends those to plan users
///   only — "Rate limit information for claude.ai subscription users" (`sdk-ts-sdk.d.ts:5670, 5678`).
///
/// Neither: unknown, not the plan — another session's reading of the plan isn't this one's.
pub fn on_plan(key_source: Option<&str>, plan_event: bool) -> bool {
    match key_source {
        Some(s) => s == "none",
        None => plan_event,
    }
}

/// `apiKeySource` as people read it.
pub fn key_said(source: &str) -> String {
    match source {
        "none" => "no API key — your Claude Code login (or a token, or a cloud provider)".into(),
        "apiKeyHelper" => "an API key from apiKeyHelper".into(),
        "ANTHROPIC_API_KEY" => "an API key from ANTHROPIC_API_KEY".into(),
        "/login managed key" => "an API key from a Console login".into(),
        other => format!("{other} (a source tarae doesn't know)"),
    }
}

// ── On the main loop ────────────────────────────────────────────────────────

/// A line of any claude's said where the limits stand: kept, `:usage` says it, watchers hear it.
pub fn seen(ed: &mut Editor, r: Report) {
    if let Some(Answer::Unknown(why)) = &r.answer {
        crate::log::line("claude", "warn", &format!("get_usage (experimental): {why} — limits unknown"));
    }
    if ed.limits.seen(r) {
        crate::uistate::watch::limits(ed);
        crate::usage::view_sync(ed);
    }
}

/// Asks the session's chat's claude, if it runs, `get_usage` — when nothing is fresh, and never while a question
/// to that same claude is unanswered (in flight: `Limits::pending`; a question to a claude that's gone blocks no
/// other).
pub fn ask_chat(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_ref().filter(|c| c.running()) else { return };
    let (now, l) = (crate::usage::now_ms(), &mut ed.limits);
    if !l.ask || l.pending == Some(c.generation()) || l.fresh(now) {
        return;
    }
    if c.to_claude(request()) {
        (l.asked_ms, l.pending) = (Some(now), Some(c.generation()));
    }
}

#[cfg(test)]
mod tests;
