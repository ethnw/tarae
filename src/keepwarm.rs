//! A big conversation that waits on you is kept warm (`target/sb/keepwarm-design.md`, 2026-10-18): its prompt cache
//! lives an hour from the last request that read or wrote it (`usage.rs`), and the turn after it went cold writes the
//! whole context again at twice the input price, where a read is a tenth. A one-word turn a few minutes before the TTL
//! reads it — and a read refreshes it.
//!
//! - **A fork, never the conversation itself**: `--resume <id> --fork-session --no-session-persistence`, on the chat
//!   process's own command line (`Chat.launched`) with the model and effort it has now, tarae's tools listed as the
//!   chat lists them (`tools::list`) — what comes before the history in the cache must be the same to the byte. Measured
//!   (`target/sb/kw.sh`, Claude Code 2.1.302): the fork reads the parent's prefix, a second identical ping reads the
//!   first one's write, and the conversation's next turn reads as if no ping was made — the fork touches neither its
//!   history nor a transcript. The recorded system prompt (`--system-prompt-snapshot`, on by default) keeps a changed
//!   git status from breaking the match.
//! - **When** is `decide`, a pure function of `Facts` — in the five minutes before the TTL (30 s to spare), on the 1h
//!   TTL only, idle, large, expected back (`Back`), at most `llm.keep-warm-max` times a stretch, never while a plan's
//!   window is near its limit. `usage::watch` wakes for it.
//! - **Said, never silent**: the chat's last row says a ping ahead (`keeps warm 14:31`) and after (`kept warm 14:31 ·
//!   $0.12`), the sessions list too, `:usage` lists them, `tarae state` has `keep_warm` and a `kept-warm` event; a ping
//!   that wrote more than it read (the prefix didn't match) is a warning.

use std::time::Duration;

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::llm::{self, StreamEv};
use crate::session::Session;
use crate::usage::Ttl;

/// The window: from this long before the cache goes cold…
pub const LEAD_MS: u64 = 5 * 60_000;
/// …to this long before (the process starts, the request goes out).
pub const MARGIN_MS: u64 = 30_000;
/// Within the window, what can change (a card comes, a window's reading) is looked at again this often.
pub const RECHECK_MS: u64 = 60_000;
/// A ping still running after this is stopped.
pub const PING_MAX: Duration = Duration::from_secs(60);
/// What a ping says — always the same, so the next ping reads this one's write.
pub const PING: &str = "(tarae: this is a fork of the conversation, made only to keep its prompt cache warm while the \
user is away. Use no tools. Reply with the single word: ok)";

/// `llm.keep-warm`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// While something of the session's waits on you, or you marked it.
    #[default]
    Expected,
    /// Only a session you marked (`:keep-warm`).
    Marked,
    Off,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Expected => "expected",
            Mode::Marked => "marked",
            Mode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        [Mode::Expected, Mode::Marked, Mode::Off].into_iter().find(|m| m.name() == s)
    }
}

/// Why the conversation is expected back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Back {
    /// Claude's edits wait for their review, or a review is open in the session.
    Review,
    /// The reviewer reads its change (`space v c`).
    UnderReview,
    /// A card of the session's waits for your answer.
    Card,
    /// Its rebase is under way.
    Rebase,
    /// A run of its Claude's is going, or waits its turn.
    Run,
    /// A message waits to go.
    Queued,
    /// You marked it (`:keep-warm`).
    Marked,
    /// You asked for a ping now (`:keep-warm now`).
    Asked,
}

impl Back {
    /// The state's word, and `:usage`'s.
    pub fn name(self) -> &'static str {
        match self {
            Back::Review => "review",
            Back::UnderReview => "under review",
            Back::Card => "card",
            Back::Rebase => "rebase",
            Back::Run => "run",
            Back::Queued => "queued",
            Back::Marked => "marked",
            Back::Asked => "asked",
        }
    }
}

/// Why no ping goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    Off,
    /// No conversation, cache or chat command line to fork from.
    NoCache,
    /// A 5-minute cache: the window would be all of it.
    FiveMinutes,
    /// Past the window: a ping now would write it all.
    Late,
    /// A turn or a ping under way — its end looks again.
    Busy,
    Small,
    /// `llm.keep-warm-max` pings this stretch.
    Enough,
    NotExpected,
    /// That plan window is near its limit (its label).
    Pressing(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Ping(Back),
    /// Not yet: look again then.
    WakeAt(u64),
    LetGo(Why),
}

/// What `decide` goes by.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub now_ms: u64,
    pub mode: Mode,
    /// When the cache goes cold, and the TTL of its last write (None: not reported).
    pub cold_at: Option<u64>,
    pub ttl: Option<Ttl>,
    /// A conversation to fork, and the chat process's command line to fork it with.
    pub forkable: bool,
    /// A request of the chat's in flight, or a ping under way (or the check before one).
    pub busy: bool,
    /// The context's tokens (None: not reported) · `llm.keep-warm-min-tokens`.
    pub tokens: Option<u64>,
    pub min_tokens: u64,
    /// What waits on you, but a mark · the mark.
    pub back: Option<Back>,
    pub marked: bool,
    /// A plan window near its limit (its label) — None on API billing, which no plan window limits.
    pub pressing: Option<String>,
    /// Pings this stretch · `llm.keep-warm-max`.
    pub kept: u32,
    pub max: u32,
}

/// Whether to ping now. Each rule lets it go, or says when to look again; all of them pass → `Ping`.
pub fn decide(f: &Facts) -> Decision {
    use Decision::{LetGo, Ping, WakeAt};
    if f.mode == Mode::Off {
        return LetGo(Why::Off);
    }
    let (Some(cold), Some(ttl), true) = (f.cold_at, f.ttl, f.forkable) else { return LetGo(Why::NoCache) };
    if ttl != Ttl::Hour1 {
        return LetGo(Why::FiveMinutes);
    }
    let (from, until) = (cold.saturating_sub(LEAD_MS), cold.saturating_sub(MARGIN_MS));
    if f.now_ms > until {
        return LetGo(Why::Late);
    }
    if f.kept >= f.max {
        return LetGo(Why::Enough);
    }
    if f.tokens.is_none_or(|n| n < f.min_tokens) {
        return LetGo(Why::Small);
    }
    if f.now_ms < from {
        return WakeAt(from);
    }
    if f.busy {
        return LetGo(Why::Busy);
    }
    // (what may change within the window is looked at again a minute on, while there's time)
    let again = |why: Why| match f.now_ms + RECHECK_MS <= until {
        true => WakeAt(f.now_ms + RECHECK_MS),
        false => LetGo(why),
    };
    let marked = f.marked.then_some(Back::Marked);
    let back = match f.mode {
        Mode::Marked => marked,
        _ => f.back.or(marked),
    };
    let Some(back) = back else { return again(Why::NotExpected) };
    if let Some(w) = &f.pressing {
        return again(Why::Pressing(w.clone()));
    }
    Ping(back)
}

/// A session's ping state: marked by you (`:keep-warm`), the project's Claude settings being read again before a
/// ping (`claudetrust::still_holds`), and the ping under way.
#[derive(Default)]
pub struct Warm {
    pub marked: bool,
    pub checking: bool,
    pub ping: Option<Ping>,
}

pub struct Ping {
    pid: u32,
    stdin: llm::Lines,
    generation: u64,
    back: Back,
    /// When its request went out (`Requesting`) — what the cache's TTL runs from.
    requested: Option<u64>,
    billing: crate::billing::Billing,
    /// Its result came: the cache was read, whatever its end says (`--max-turns 1` after a tool it tried).
    kept: bool,
    /// Its message went — once its claude named a credential that passed (`billing::checked`).
    sent: bool,
}

/// The facts of session `s` at `now_ms`.
pub fn facts(s: &Session, cfg: &crate::llm::LlmConfig, limits: &crate::limits::Limits, now_ms: u64) -> Facts {
    let u = &s.usage;
    let chat = s.chat.as_ref();
    let conv = u.conv.as_ref();
    let on_plan = s.billing == crate::billing::Billing::Subscription;
    let pressing = limits.known(now_ms).into_iter().find(|r| crate::limits::pressing(r));
    Facts {
        now_ms,
        mode: cfg.keep_warm,
        cold_at: u.cold_at(),
        ttl: conv.and_then(|c| c.ttl),
        forkable: chat.is_some_and(|c| c.launched.is_some()) && conv.is_some_and(|c| !c.id.is_empty()),
        // (a request in flight: its turn under way with no tool call waiting for its result — a turn waiting on a
        // review, a card or a run of its sends nothing until it's answered, and its cache goes cold meanwhile)
        busy: chat.is_some_and(|c| c.busy() && c.calls.is_empty())
            || s.warm.ping.is_some()
            || s.warm.checking,
        tokens: u.context().map(|c| c.0),
        min_tokens: cfg.keep_warm_min_tokens,
        back: back(s),
        marked: s.warm.marked,
        pressing: pressing.filter(|_| on_plan).map(|r| crate::limits::label(&r.window)),
        kept: u.stretch().len() as u32,
        max: cfg.keep_warm_max,
    }
}

/// What reads the session's change for it — the reviewer (`space v c`); the board (`space v C`) is one more line.
fn readers(s: &Session) -> [bool; 1] {
    [s.reviewer.as_ref().is_some_and(|r| r.reading())]
}

/// What of session `s` waits on you (but a mark), the first in this order.
pub fn back(s: &Session) -> Option<Back> {
    let chat = s.chat.as_ref();
    let review = chat.is_some_and(|c| !c.edits.queue.is_empty() || !c.edits.sets.is_empty())
        || s.review.is_some()
        || s.code_review.is_some();
    let claudes_test =
        s.test_run.as_ref().is_some_and(|r| r.by_claude && r.state == crate::testing::RunState::Running);
    let claudes_task =
        s.task_run.as_ref().is_some_and(|r| r.by_claude && r.state == crate::task::State::Running);
    let run = claudes_test
        || claudes_task
        || !s.runs_next.is_empty()
        || !s.tool_waits.is_empty()
        || chat.is_some_and(|c| !c.reruns.is_empty());
    [
        (review, Back::Review),
        (readers(s).into_iter().any(|r| r), Back::UnderReview),
        (!s.asking.is_empty() || !s.parked.is_empty(), Back::Card),
        (s.rebase.is_some(), Back::Rebase),
        (run, Back::Run),
        (chat.is_some_and(|c| c.queued.is_some()), Back::Queued),
    ]
    .into_iter()
    .find_map(|(holds, b)| holds.then_some(b))
}

/// What the chat's row, the sessions list and the state say: the ping ahead, if one will go — when — and this
/// stretch's pings.
#[derive(Default)]
pub struct Said<'a> {
    pub next_at: Option<u64>,
    pub kept: Vec<&'a crate::usage::Kept>,
}

pub fn said<'a>(
    s: &'a Session,
    cfg: &crate::llm::LlmConfig,
    limits: &crate::limits::Limits,
    now_ms: u64,
) -> Said<'a> {
    let f = facts(s, cfg, limits, now_ms);
    // (as things stand now, would the window's start ping? — inside it, now)
    let at = f.cold_at.map_or(now_ms, |c| c.saturating_sub(LEAD_MS).max(now_ms));
    let next_at = matches!(decide(&Facts { now_ms: at, ..f }), Decision::Ping(_)).then_some(at);
    Said { next_at, kept: s.usage.stretch() }
}

impl Said<'_> {
    /// `kept warm 14:31 · ≈$0.12` (`×2` for two; an API estimate, as the turn's) and `keeps warm 15:26` — each
    /// part, empty when there's none.
    pub fn words(&self) -> (String, String) {
        use crate::usage::{dollars, until};
        let kept = match self.kept.last() {
            None => String::new(),
            Some(last) => {
                let n = self.kept.len();
                let times = if n > 1 { format!(" ×{n}") } else { String::new() };
                let costs: Option<f64> = self.kept.iter().map(|k| k.cost).sum();
                let cost = costs.map_or_else(|| "cost not reported".into(), dollars);
                format!("kept warm {}{times} · ≈{cost}", until(last.at_ms))
            }
        };
        let next = self.next_at.map_or_else(String::new, |at| format!("keeps warm {}", until(at)));
        (kept, next)
    }

    /// The state's `keep_warm`.
    pub fn json(&self, mode: Mode, marked: bool) -> Value {
        let kept: Vec<Value> = self.kept.iter().map(|k| k.json()).collect();
        json!({ "mode": mode.name(), "marked": marked, "next_at": self.next_at, "kept": kept })
    }
}

// ── The ping ─────────────────────────────────────────────────────────────────────

/// The usage watch woke inside the cache's life (`usage::woke`): ping, look again later, or let it go.
pub fn woke(ed: &mut Editor) {
    let now = crate::usage::now_ms();
    match decide(&facts(&ed.session, &ed.config.llm, &ed.limits, now)) {
        Decision::Ping(back) => checked_ping(ed, back),
        Decision::WakeAt(at) => crate::usage::sleep_until(ed, at),
        Decision::LetGo(_) => {}
    }
}

/// A ping once the project's Claude settings are read again: a fork is a new claude, which reads them as they are
/// now — only when the chat's claude was started as they hold now does the fork run as it does (and match its
/// cache); else the chat starts again with them first (`claudetrust.rs`), and no ping goes.
fn checked_ping(ed: &mut Editor, back: Back) {
    ed.session.warm.checking = true;
    crate::claudetrust::still_holds(ed, move |ed: &mut Editor, holds: Option<bool>| {
        ed.session.warm.checking = false;
        match holds {
            Some(true) => ping(ed, back),
            Some(false) => {
                let said = "Not kept warm: the project's Claude settings changed since Claude started";
                crate::log::line("claude", "info", &format!("keep-warm: {said}"));
                ed.set_status(said);
            }
            // (its chat closed meanwhile: nothing to keep warm, nothing to say)
            None => crate::log::line("claude", "info", "keep-warm: no ping — the chat closed"),
        }
    });
}

/// The ping's command line: the chat process's own, with the model and effort it runs on now (`Chat.runs_on` —
/// `:model` saves the setting for every session but switches only the chat in front), forking
/// conversation `id` — one turn at most (`--max-turns 1`): a tool it tries is refused, and asking again would be
/// another request reading it all. None of the three changes what's sent: the cache's prefix stays the chat's.
pub fn command(
    launched: &crate::llm::LlmConfig,
    (model, effort): &(Option<String>, String),
    id: &str,
) -> crate::llm::LlmConfig {
    let mut args = llm::with_flag(&launched.args, "--resume", id);
    args.retain(|a| a != "--fork-session" && a != "--no-session-persistence");
    args.extend(["--fork-session".into(), "--no-session-persistence".into()]);
    args = llm::with_flag(&args, "--max-turns", "1");
    crate::llm::LlmConfig { args, model: model.clone(), effort: effort.clone(), ..launched.clone() }
}

/// Starts a ping of the session's conversation, because of `back`.
fn ping(ed: &mut Editor, back: Back) {
    let Some(c) = ed.session.chat.as_ref() else { return };
    let Some((launched, extra)) = c.launched.clone() else { return };
    let Some(id) = ed.session.usage.conv.as_ref().map(|c| c.id.clone()) else { return };
    let billing = c.billing.unwrap_or(ed.session.billing);
    let cfg = command(&launched, &c.runs_on, &id);
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    let started = ed.credential(billing, crate::billing::Role::Chat).and_then(|cred| {
        llm::spawn(&cfg, &extra, &ed.session.root, ed.claude_env.as_ref(), &cred)
            .map_err(|e| e.said(&cfg.command, false))
    });
    let mut child = match started {
        Ok(child) => child,
        Err(e) => return ed.set_warning(format!("Couldn't keep Claude's cache warm: {e}")),
    };
    let Some(stdin) = child.stdin.take().map(llm::Lines::new) else { return };
    // (the message only once its answer to `initialize` names the credential it runs on, checked: no request goes
    // out on another account — `on_event`)
    if let Err(e) = stdin.send(llm::initialize_message(&["tarae"])) {
        crate::event::thread("tarae-reap", move || {
            let _ = child.kill();
            let _ = child.wait();
        });
        return ed.set_warning(format!("Couldn't keep Claude's cache warm: claude: {e}"));
    }
    static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = child.id();
    ed.session.warm.ping =
        Some(Ping { pid, stdin, generation, back, requested: None, billing, kept: false, sent: false });
    crate::log::line("claude", "info", &format!("keep-warm: a ping of {id} ({})", back.name()));
    llm::pump(child, true, ed.events.sender(), move |ev| {
        Some(Box::new(move |ed: &mut Editor| on_event(ed, generation, ev)))
    });
    // (one that hangs is stopped: it would hold the next ping back, and spend — not in tests, whose stand-ins end)
    if cfg!(test) {
        return;
    }
    ed.events.jobs().spawn(move || {
        crate::event::sleep(PING_MAX);
        move |ed: &mut Editor| {
            ed.unchanged = true;
            if ed.session.warm.ping.as_ref().is_some_and(|p| p.generation == generation) {
                llm::kill(pid);
            }
        }
    });
}

fn on_event(ed: &mut Editor, generation: u64, ev: StreamEv) {
    let Some(p) = ed.session.warm.ping.as_mut().filter(|p| p.generation == generation) else { return };
    match ev {
        StreamEv::Requesting(at) => {
            p.requested.get_or_insert(at);
        }
        StreamEv::Control { request_id, request } => {
            let arm = ed.session.chat.as_ref().map_or(ed.config.llm.edit_tools, |c| c.edit_tools);
            let _ = p.stdin.send(llm::control_response(&request_id, control(arm, &request)));
        }
        StreamEv::Spent(s) => {
            let (back, requested) = (p.back, p.requested);
            let Some(k) = ed.session.usage.kept_warm(s, requested, back.name()) else { return };
            p.kept = true;
            // (it wrote more than it read: what comes before the history didn't match the chat's)
            if k.counts.write.unwrap_or(0) > k.counts.read.unwrap_or(0) {
                let said = format!(
                    "Keeping Claude's cache warm read {} and wrote {}: the ping didn't match the conversation's cache",
                    crate::usage::count(k.counts.read),
                    crate::usage::count(k.counts.write)
                );
                crate::log::line("claude", "warn", &said);
                ed.set_warning(said);
            }
            // (a toast too: the chat's row isn't in sight with the chat closed or the session behind)
            let said = kept_said(&k);
            ed.set_status(said);
            ed.calls_record();
            crate::uistate::watch::usage(ed, "kept-warm");
            crate::usage::view_sync(ed);
        }
        // (not the credential the chat runs on: stopped — `billing.rs`; before its message on `initialize`'s
        // answer, so no request goes out on it)
        StreamEv::Conversation { key_source, .. } | StreamEv::Initialized { key_source, .. } => {
            let keep = ed.config.llm.keep_env_key;
            let role = crate::billing::Role::Chat;
            if let Some(said) = crate::billing::checked(p.billing, keep, role, key_source.as_deref()) {
                llm::kill(p.pid);
                ed.set_warning(format!("Couldn't keep Claude's cache warm: {said}"));
            } else if !p.sent {
                p.sent = true;
                if let Err(e) = p.stdin.send(llm::user_message(PING)) {
                    llm::kill(p.pid);
                    ed.set_warning(format!("Couldn't keep Claude's cache warm: claude: {e}"));
                }
            }
        }
        StreamEv::Done(r) => {
            let kept = p.kept;
            ed.session.warm.ping = None;
            // (ended past its one turn — a tool it tried — after its request read the cache: kept all the same)
            if let Err(e) = r
                && !kept
            {
                ed.set_warning(format!("Couldn't keep Claude's cache warm: {e}"));
            }
            // (the cache's new end, and the next ping's window)
            crate::usage::watch(ed);
        }
        StreamEv::Limits(r) => crate::limits::seen(ed, r),
        StreamEv::Thinking
        | StreamEv::Thought(_)
        | StreamEv::Text(_)
        | StreamEv::Tool { .. }
        | StreamEv::ToolResult { .. }
        | StreamEv::Answered(_)
        | StreamEv::Usage(_)
        | StreamEv::Compaction(_)
        | StreamEv::Retry(_)
        | StreamEv::Money(_)
        | StreamEv::Exit(_) => {}
    }
}

/// The toast for a ping (and `:messages`' line).
fn kept_said(k: &crate::usage::Kept) -> String {
    let cost = k.cost.map_or_else(|| "cost not reported".into(), crate::usage::dollars);
    // (short: it's the status line's under a picker or beside the chat — why and the write are `:usage`'s)
    format!("Claude's cache kept warm: read {}, ≈ API {cost}", crate::usage::count(k.counts.read))
}

/// The ping's answer to its claude's control request: tarae's tools listed as the chat lists them (the cache's
/// prefix), none of them run, no permission given.
fn control(arm: crate::edittool::Arm, req: &Value) -> Value {
    const NONE: &str = "this is a ping that keeps the cache warm: no tool runs here";
    if req["subtype"] == "can_use_tool" {
        return json!({ "behavior": "deny", "message": NONE });
    }
    if req["subtype"] != "mcp_message" || req["server_name"] != "tarae" {
        return json!({});
    }
    let msg = &req["message"];
    let Some(id) = msg.get("id").cloned() else {
        return json!({ "mcp_response": { "jsonrpc": "2.0", "result": {}, "id": 0 } });
    };
    let rev = match crate::mcp::rev(msg) {
        Ok(rev) => rev,
        Err(e) => return json!({ "mcp_response": { "jsonrpc": "2.0", "id": id, "error": e } }),
    };
    let method = msg["method"].as_str().unwrap_or_default();
    let result = match method {
        "tools/list" => Ok(crate::tools::list(arm)),
        "tools/call" => Ok(crate::tools::content(Err(NONE.to_string()))),
        m => crate::mcp::common(m, msg, rev, json!({ "tools": {} })).ok_or_else(|| crate::mcp::not_found(m)),
    };
    let response = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": crate::mcp::finish(rev, method, r) }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    };
    json!({ "mcp_response": response })
}

/// `:keep-warm [on|off]` — marks the session's conversation to be kept warm while it waits (None flips it).
pub fn mark(ed: &mut Editor, want: Option<bool>) {
    let on = want.unwrap_or(!ed.session.warm.marked);
    ed.session.warm.marked = on;
    let mode = ed.config.llm.keep_warm;
    // (what will happen, and when the next ping goes — none due: the sentence alone)
    let next = said(&ed.session, &ed.config.llm, &ed.limits, crate::usage::now_ms()).next_at;
    let when = next.map_or_else(String::new, |at| format!(" — next at {}", crate::usage::until(at)));
    let said = match (on, mode) {
        (true, Mode::Off) => "Marked to keep warm — but llm.keep-warm is off".to_string(),
        (true, _) => format!("Claude's cache will be kept warm while this session waits{when}"),
        (false, Mode::Expected) => {
            "Not marked — kept warm only while something here waits on you".to_string()
        }
        (false, _) => "Not kept warm".to_string(),
    };
    ed.set_success(said);
    // (inside the window already: now; else when it comes)
    woke(ed);
    crate::usage::watch(ed);
}

/// `:keep-warm now` — a ping at once, asked for: past the window and the size, but never over a turn or a ping
/// under way, nor with nothing to fork.
pub fn now(ed: &mut Editor) -> Result<(), String> {
    let f = facts(&ed.session, &ed.config.llm, &ed.limits, crate::usage::now_ms());
    if !f.forkable || f.cold_at.is_none() {
        return Err("keep-warm: no conversation here to keep warm yet".into());
    }
    if f.busy {
        return Err("keep-warm: Claude is at work — its turn keeps the cache warm".into());
    }
    checked_ping(ed, Back::Asked);
    Ok(())
}

/// `w` in the sessions list: session `id`'s mark flipped — the one in front as `:keep-warm`, one behind kept for
/// its next look (its window looks again each minute).
pub fn mark_in(ed: &mut Editor, id: crate::event::SessionId) {
    if ed.session.id == id {
        return mark(ed, None);
    }
    let Some(s) = ed.sessions.iter_mut().find(|s| s.id == id) else { return };
    s.warm.marked = !s.warm.marked;
    let (on, name) = (s.warm.marked, s.label().to_string());
    ed.set_success(match (on, ed.config.llm.keep_warm) {
        (true, Mode::Off) => format!("{name}: marked to keep warm — but llm.keep-warm is off"),
        (true, _) => format!("{name}: Claude's cache will be kept warm while it waits"),
        (false, _) => format!("{name}: not marked to keep warm"),
    });
}

#[cfg(test)]
mod tests;
