//! What Claude's work costs and how warm its prompt cache is — per session, from what claude's stream-json
//! says (measured 2026-10-05, Claude Code 2.1.288; `usage_stream.jsonl` is a recorded stream):
//!
//! - Each API request: `system` `status: "requesting"` as it goes out, then `stream_event` `message_start`
//!   whose `message.usage` has `input_tokens`, `cache_creation_input_tokens` (split in `cache_creation`'s
//!   `ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` — the TTL the write got),
//!   `cache_read_input_tokens` and `output_tokens` so far; `message_delta` (no id: the message started last)
//!   has the final `output_tokens`. The `assistant` messages carry the same usage once per content block,
//!   with the output as it was then — merged by message id, the largest kept.
//! - The turn's `result`: `usage` is this turn's (the sum of its requests), `duration_ms`; `total_cost_usd`
//!   and `modelUsage.<model>` (`costUSD`, `contextWindow`) add up over the whole conversation — across turns
//!   and across `--resume` — so a turn's cost is the difference from that conversation's last result (a
//!   fresh one's first turn: all of it).
//! - Nothing splits the cost into read / write / input / output (those are token counts), and `initialize`
//!   names no context window — it comes with a result. What isn't sent (another CLI, a turn cut off) is
//!   "not reported", never a guess.
//! - The cache lives its TTL from the last request that wrote or read it (a read refreshes it): warm until
//!   then, the TTL that of its last write.
//!
//! Kept in memory per session (`Session.usage`); a conversation's turns, cost so far and cache go with it in
//! `$state/chats/` (`chats.rs`), so one continued (`:restart`, `C-p`) comes back with them.
//!
//! What tarae itself sends — each tool result of its own, each user message and its editor context — is
//! measured as it goes (`tokens` lines in the log, sizes only; `:usage`'s table of tool results): every later
//! request reads it again, so that's where a turn's cost grows.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::editor::Editor;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// How long a cache write lives (`cache_creation`'s split says which).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ttl {
    Min5,
    Hour1,
}

impl Ttl {
    pub fn ms(self) -> u64 {
        match self {
            Ttl::Min5 => 5 * 60_000,
            Ttl::Hour1 => 3_600_000,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Ttl::Min5 => "5m",
            Ttl::Hour1 => "1h",
        }
    }

    fn named(s: &str) -> Option<Ttl> {
        [Ttl::Min5, Ttl::Hour1].into_iter().find(|t| t.name() == s)
    }

    /// The kind of write a `usage` object reports — None: it wrote nothing, or didn't say which.
    pub fn of(usage: &Value) -> Option<Ttl> {
        let split = &usage["cache_creation"];
        let n = |k: &str| split[k].as_u64().unwrap_or(0);
        match (n("ephemeral_1h_input_tokens"), n("ephemeral_5m_input_tokens")) {
            (h, _) if h > 0 => Some(Ttl::Hour1),
            (_, m) if m > 0 => Some(Ttl::Min5),
            _ => None,
        }
    }
}

/// Token counts as reported — None where nothing was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub input: Option<u64>,
    /// Cache write · cache read.
    pub write: Option<u64>,
    pub read: Option<u64>,
    pub output: Option<u64>,
}

impl Counts {
    /// From an Anthropic `usage` object.
    pub fn of(u: &Value) -> Counts {
        Counts {
            input: u["input_tokens"].as_u64(),
            write: u["cache_creation_input_tokens"].as_u64(),
            read: u["cache_read_input_tokens"].as_u64(),
            output: u["output_tokens"].as_u64(),
        }
    }

    fn fields(&mut self) -> [&mut Option<u64>; 4] {
        [&mut self.input, &mut self.write, &mut self.read, &mut self.output]
    }

    /// The same request said again: each count its largest (output grows as it streams).
    fn merge(&mut self, mut o: Counts) {
        for (a, b) in self.fields().into_iter().zip(o.fields()) {
            *a = match (*a, *b) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (x, y) => x.or(y),
            };
        }
    }

    /// Summed — a count none of them reported stays None.
    fn add(&mut self, mut o: Counts) {
        for (a, b) in self.fields().into_iter().zip(o.fields()) {
            *a = match (*a, *b) {
                (Some(x), Some(y)) => Some(x + y),
                (x, y) => x.or(y),
            };
        }
    }

    pub fn any(&self) -> bool {
        [self.input, self.write, self.read, self.output].iter().any(Option::is_some)
    }

    /// What the request carried: input + cache read + cache write.
    pub fn context(&self) -> Option<u64> {
        let parts = [self.input, self.read, self.write];
        parts.iter().any(Option::is_some).then(|| parts.iter().flatten().sum())
    }

    /// The share of it read from the cache.
    pub fn hit(&self) -> Option<f64> {
        let all = self.context().filter(|&n| n > 0)?;
        Some(self.read? as f64 / all as f64)
    }

    pub fn json(&self) -> Value {
        json!({ "input": self.input, "write": self.write, "read": self.read, "output": self.output })
    }

    fn from_json(v: &Value) -> Counts {
        Counts {
            input: v["input"].as_u64(),
            write: v["write"].as_u64(),
            read: v["read"].as_u64(),
            output: v["output"].as_u64(),
        }
    }
}

/// Usage one stream line reports (`llm::usage_line`): its message (None: the one started last), model,
/// counts, the kind of cache write, when it was read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub id: Option<String>,
    pub model: String,
    pub counts: Counts,
    pub ttl: Option<Ttl>,
    pub at_ms: u64,
}

/// A turn's `result` as far as usage goes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spent {
    pub conversation: String,
    pub counts: Counts,
    /// The conversation's cost so far (`total_cost_usd`).
    pub total_cost: Option<f64>,
    /// The models it ran (`modelUsage`'s keys) and their context windows.
    pub models: Vec<(String, Option<u64>)>,
    /// Its total is this process's requests alone: `modelUsage` counts the tokens `usage` does (None: not said).
    /// A process that resumed a conversation adds what claude restored of its cost — or nothing (`keepwarm.rs`).
    pub alone: Option<bool>,
    pub ms: Option<u64>,
    pub at_ms: u64,
}

/// A ping that kept a conversation's cache warm (`keepwarm.rs`): the conversation, when its request went out (the
/// TTL runs from there), what it read and wrote, its own cost (None: not reported), why it went.
#[derive(Clone, Debug, PartialEq)]
pub struct Kept {
    pub conversation: String,
    pub at_ms: u64,
    pub counts: Counts,
    pub cost: Option<f64>,
    pub why: &'static str,
}

impl Kept {
    pub fn json(&self) -> Value {
        let n = &self.counts;
        json!({ "at_ms": self.at_ms, "why": self.why, "cache_read": n.read, "cache_write": n.write,
                "input": n.input, "output": n.output, "cost": self.cost })
    }
}

/// One API request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub id: String,
    pub model: String,
    pub sent_ms: u64,
    pub counts: Counts,
    pub ttl: Option<Ttl>,
}

impl Request {
    fn touched(&self) -> bool {
        self.counts.write.unwrap_or(0) > 0 || self.counts.read.unwrap_or(0) > 0
    }

    fn json(&self) -> Value {
        json!({ "id": self.id, "model": self.model, "sent": self.sent_ms, "counts": self.counts.json(),
                "ttl": self.ttl.map(Ttl::name) })
    }

    fn from_json(v: &Value) -> Option<Request> {
        Some(Request {
            id: v["id"].as_str()?.to_string(),
            model: v["model"].as_str().unwrap_or_default().to_string(),
            sent_ms: v["sent"].as_u64()?,
            counts: Counts::from_json(&v["counts"]),
            ttl: v["ttl"].as_str().and_then(Ttl::named),
        })
    }
}

/// One turn, as its result (else its requests) said.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Turn {
    /// When it ended (unix ms).
    pub at_ms: u64,
    pub conversation: String,
    pub model: String,
    pub counts: Counts,
    /// Its share of the conversation's cost — None: not reported, or the conversation's earlier cost isn't
    /// known here (one continued that tarae didn't keep the cost of).
    pub cost: Option<f64>,
    pub requests: usize,
    pub ms: Option<u64>,
    /// Claude compacted the conversation in it — its trigger (`auto` · `manual`): a `/compact` turn reports
    /// 0 tokens and the summary's cost, said so rather than as a turn that read nothing.
    pub compacted: Option<String>,
    /// Its tool calls (`calls.rs`) — None: a one-turn process's, or a turn kept before they were counted.
    pub calls: Option<crate::calls::Calls>,
}

impl Turn {
    fn json(&self) -> Value {
        let mut v = json!({ "at": self.at_ms, "model": self.model, "counts": self.counts.json(), "cost": self.cost,
                "requests": self.requests, "ms": self.ms, "compacted": self.compacted });
        if let Some(c) = &self.calls {
            v["calls"] = c.json();
        }
        v
    }

    /// Its entry in the call-order record (`calls::DIR`, `cargo xtask turns`): numbers and tool names only —
    /// never a call's arguments, a result or a word of the conversation. `what`: `chat`, or an aside's
    /// (`reviewer`, `ask`, `draft`); `steps`: its requests in order, each with its calls' tools in order
    /// (`calls::Tally::take_steps`), beside each request's context and output. An aside's requests aren't
    /// counted (no tally: `calls` None) — `null`, not 0.
    pub fn entry(&self, what: &str, steps: &[Vec<&'static str>], requests: &[Request]) -> Value {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.at_ms);
        let steps: Vec<Value> = (0..steps.len().max(requests.len()))
            .map(|i| {
                let r = requests.get(i).map(|r| r.counts);
                json!({ "tools": steps.get(i).cloned().unwrap_or_default(),
                        "context": r.and_then(|c| c.context()), "output": r.and_then(|c| c.output) })
            })
            .collect();
        json!({
            "v": crate::calls::RECORD_V,
            "at": crate::log::stamp(at),
            "what": what,
            "model": self.model,
            "requests": self.calls.as_ref().map(|_| self.requests),
            "counts": self.counts.json(),
            "cost": self.cost,
            "ms": self.ms,
            "compacted": self.compacted,
            "calls": self.calls.as_ref().map(|c| c.sums.json()),
            "steps": steps,
        })
    }

    /// It compacted and reported no tokens of its own (`/compact`'s turn).
    pub fn only_compacted(&self) -> bool {
        self.compacted.is_some() && self.counts.context().unwrap_or(0) + self.counts.output.unwrap_or(0) == 0
    }

    fn from_json(conversation: &str, v: &Value) -> Option<Turn> {
        Some(Turn {
            at_ms: v["at"].as_u64()?,
            conversation: conversation.to_string(),
            model: v["model"].as_str().unwrap_or_default().to_string(),
            counts: Counts::from_json(&v["counts"]),
            cost: v["cost"].as_f64(),
            requests: v["requests"].as_u64().unwrap_or(0) as usize,
            ms: v["ms"].as_u64(),
            compacted: v["compacted"].as_str().map(String::from),
            calls: crate::calls::Calls::from_json(&v["calls"]),
        })
    }
}

/// The conversation in the chat now: its cache, its context, what each request starts with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Conv {
    pub id: String,
    /// Its latest request — what the context holds.
    pub last: Option<Request>,
    /// When a request last wrote or read its cache (the TTL runs from there) · the TTL of its last write
    /// (None: none seen, or one that didn't say).
    pub touched_ms: Option<u64>,
    pub ttl: Option<Ttl>,
    /// Its first request — system prompt, tools, CLAUDE.md and the first message, what every later request
    /// reads again.
    pub fixed: Option<Request>,
    /// A fresh conversation whose first request hasn't come yet.
    first_next: bool,
    /// Its cost so far, as its last result said (None: not known here).
    pub cost_so_far: Option<f64>,
    /// Compacted since its latest request: what the context holds now (`post_tokens`) — until the next
    /// request says.
    pub compacted_to: Option<u64>,
    /// What tarae's own tools handed it since tarae started, by tool: (calls, bytes) — `:usage`'s table.
    pub tool_results: BTreeMap<String, (u64, u64)>,
}

/// Warm (time left) · cold (since when) · not reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    Warm { left_ms: u64, ttl: Ttl },
    Cold { since_ms: u64 },
    NotReported,
}

impl Cache {
    /// `warm` · `cold` · `not reported` — the state's word.
    pub fn word(self) -> &'static str {
        match self {
            Cache::Warm { .. } => "warm",
            Cache::Cold { .. } => "cold",
            Cache::NotReported => "not reported",
        }
    }
}

/// Turns summed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub turns: usize,
    pub counts: Counts,
    /// The cost of the turns that reported one · how many didn't.
    pub cost: f64,
    pub unpriced: usize,
    /// The tool calls of the turns that counted them · how many those were.
    pub calls: crate::calls::Sums,
    pub counted: usize,
}

impl Totals {
    fn add(&mut self, t: &Turn) {
        self.turns += 1;
        self.counts.add(t.counts);
        if let Some(c) = &t.calls {
            self.calls.add(c.sums);
            self.counted += 1;
        }
        match t.cost {
            Some(c) => self.cost += c,
            None => self.unpriced += 1,
        }
    }

    /// `$1.23` · `$1.23 + 2 not reported` · `not reported`.
    pub fn cost_said(&self) -> String {
        match (self.turns - self.unpriced, self.unpriced) {
            (0, _) => "not reported".into(),
            (_, 0) => dollars(self.cost),
            (_, n) => format!("{} + {n} not reported", dollars(self.cost)),
        }
    }

    /// `cost_said` where nothing around it says what the number is (#161): `≈ API $1.23`.
    pub fn api_said(&self) -> String {
        match self.turns == self.unpriced {
            true => self.cost_said(),
            false => format!("≈ API {}", self.cost_said()),
        }
    }
}

/// What the costs are, once where they're shown at length (#161): claude's `total_cost_usd` is the API
/// price by its own table — on a subscription, not what's paid.
pub fn price_note(cli: Option<&str>) -> String {
    let table = match cli {
        Some(v) => format!("Claude Code {v}'s own table"),
        None => "Claude Code's own table".into(),
    };
    format!("Costs are the API price, per {table} — on a subscription, not what you pay.")
}

/// A session's Claude usage.
#[derive(Debug, Default)]
pub struct Usage {
    /// Its turns, oldest first — every conversation it had since tarae started (and a continued one's
    /// earlier turns).
    pub turns: Vec<Turn>,
    /// The requests of the turn under way.
    pub requests: Vec<Request>,
    pub conv: Option<Conv>,
    /// The context windows results named, by model.
    pub windows: Vec<(String, u64)>,
    /// A request said to go out, its message not here yet.
    requesting: Option<u64>,
    /// When the TTL watch asleep wakes (`watch`).
    pub watch: Option<u64>,
    /// The Claude Code version its chat's process runs (`system init`) — whose price table the costs are.
    pub cli: Option<String>,
    /// The credential its chat's claude last reported it runs on (`system init`'s `apiKeySource` — `limits.rs`).
    pub key_source: Option<String>,
    /// Its chat's claude sent a `rate_limit_event` — which Claude Code sends to claude.ai plan users only
    /// (`limits::on_plan`).
    pub plan_event: bool,
    /// The turn under way compacted the conversation — its trigger, for its `Turn`.
    compacting: Option<String>,
    /// The turn under way's tool calls, counted as they come (`calls.rs`).
    pub tally: crate::calls::Tally,
    /// Turns ended, their call-order record entries not yet handed to the record (`Editor::calls_record`).
    pub to_record: Vec<Value>,
    /// The pings that kept its conversations warm since tarae started (`keepwarm.rs`), oldest first.
    pub kept: Vec<Kept>,
}

impl Usage {
    /// A conversation this process began (no `--resume`): it has cost nothing before, its first request is
    /// what every request starts with.
    pub fn fresh(&mut self, id: &str) {
        if self.conv.as_ref().is_some_and(|c| c.id == id) {
            return;
        }
        self.conv = Some(Conv { id: id.into(), first_next: true, cost_so_far: Some(0.0), ..Conv::default() });
    }

    /// A conversation continued (`--resume`) — what came before is known only if it was kept (`restore`).
    pub fn resumed(&mut self, id: &str) {
        if self.conv.as_ref().is_some_and(|c| c.id == id) {
            return;
        }
        self.conv = Some(Conv { id: id.into(), ..Conv::default() });
    }

    pub fn requesting(&mut self, at_ms: u64) {
        self.requesting = Some(at_ms);
    }

    /// A stream line's usage: a new request, or more of one under way.
    pub fn seen(&mut self, s: Seen) {
        let at = match &s.id {
            Some(id) => self.requests.iter().position(|r| r.id == *id),
            None => self.requests.len().checked_sub(1),
        };
        let r = match (at, s.id) {
            (Some(i), _) => {
                let r = &mut self.requests[i];
                r.counts.merge(s.counts);
                r.ttl = r.ttl.or(s.ttl);
                if r.model.is_empty() {
                    r.model = s.model;
                }
                r.clone()
            }
            (None, Some(id)) => {
                self.tally.request();
                let sent_ms = self.requesting.take().unwrap_or(s.at_ms);
                let r = Request { id, model: s.model, sent_ms, counts: s.counts, ttl: s.ttl };
                self.requests.push(r.clone());
                r
            }
            (None, None) => return,
        };
        let Some(c) = self.conv.as_mut() else { return };
        if c.first_next || c.fixed.as_ref().is_some_and(|f| f.id == r.id) {
            c.first_next = false;
            c.fixed = Some(r.clone());
        }
        if r.touched() {
            c.touched_ms = Some(r.sent_ms);
        }
        if r.counts.write.unwrap_or(0) > 0 {
            c.ttl = r.ttl;
        }
        // (a request after a compaction says what the context holds again)
        if c.last.as_ref().is_none_or(|l| l.id != r.id) {
            c.compacted_to = None;
        }
        c.last = Some(r);
    }

    /// The conversation compacted (`compaction.rs`): the context holds what it kept from now on, and the
    /// turn under way says it compacted.
    pub fn compacted(&mut self, k: &crate::compaction::Compaction) {
        self.compacting = Some(k.trigger.clone());
        self.tally.forget();
        if let Some(c) = self.conv.as_mut() {
            c.compacted_to = k.post_tokens;
        }
    }

    /// The turn's result: a turn of what it says (else of what its requests said).
    pub fn spent(&mut self, s: Spent) -> &Turn {
        let requests = std::mem::take(&mut self.requests);
        let mut counts = s.counts;
        let mut summed = Counts::default();
        for r in &requests {
            summed.add(r.counts);
        }
        for (a, b) in counts.fields().into_iter().zip(summed.fields()) {
            *a = a.or(*b);
        }
        for (m, w) in &s.models {
            if let Some(w) = w {
                self.windows.retain(|(k, _)| k != m);
                self.windows.push((m.clone(), *w));
            }
        }
        let conv = match self.conv.as_mut() {
            Some(c) if c.id == s.conversation || s.conversation.is_empty() => Some(c),
            _ => None,
        };
        let cost = match (conv, s.total_cost) {
            (Some(c), Some(total)) => {
                let before = c.cost_so_far.replace(total);
                before.map(|b| (total - b).max(0.0))
            }
            _ => None,
        };
        let mut model: Vec<String> = s.models.iter().map(|m| m.0.clone()).collect();
        if model.is_empty() {
            model = requests.iter().map(|r| r.model.clone()).filter(|m| !m.is_empty()).collect();
            model.dedup();
        }
        let contexts: Vec<u64> = requests.iter().filter_map(|r| r.counts.context()).collect();
        let steps = self.tally.take_steps();
        let calls = self.tally.end(&contexts, s.ms, s.at_ms);
        self.turns.push(Turn {
            at_ms: s.at_ms,
            conversation: if s.conversation.is_empty() {
                self.conv.as_ref().map(|c| c.id.clone()).unwrap_or_default()
            } else {
                s.conversation
            },
            model: model.join(", "),
            counts,
            cost,
            requests: requests.len(),
            ms: s.ms,
            compacted: self.compacting.take(),
            calls: Some(calls),
        });
        let turn = self.turns.last().expect("just pushed");
        self.to_record.push(turn.entry("chat", &steps, &requests));
        turn
    }

    /// A one-turn process's result (`space i`, a commit draft — `what`): a turn of its own, beside the
    /// chat's conversation; its process began with it, so its total is its cost.
    pub fn aside(&mut self, s: Spent, what: &str) {
        let model: Vec<String> = s.models.iter().map(|m| m.0.clone()).collect();
        self.turns.push(Turn {
            at_ms: s.at_ms,
            conversation: what.into(),
            model: model.join(", "),
            counts: s.counts,
            cost: s.total_cost,
            requests: 0,
            ms: s.ms,
            compacted: None,
            calls: None,
        });
        let turn = self.turns.last().expect("just pushed");
        self.to_record.push(turn.entry(what, &[], &[]));
    }

    /// A ping's result (`keepwarm.rs`), its request out at `requested_ms`: the conversation's cache warm from
    /// then, and a turn of its own (`aside`). Its cost is its total when that's its own request alone (`alone`:
    /// measured live, the chat's process running — claude restored nothing of the conversation's cost); when
    /// claude restored some (`target/sb/kw.sh`: each earlier process had exited) its own can't be told from it —
    /// not reported, never the difference from what tarae saw: what claude restores isn't that. None: no
    /// conversation, or the ping reported no request.
    pub fn kept_warm(&mut self, mut s: Spent, requested_ms: Option<u64>, why: &'static str) -> Option<Kept> {
        let c = self.conv.as_mut()?;
        if !s.counts.any() {
            return None;
        }
        let cost = s.total_cost.filter(|_| s.alone == Some(true));
        let at = requested_ms.unwrap_or(s.at_ms.saturating_sub(s.ms.unwrap_or(0)));
        let touched = s.counts.read.unwrap_or(0) + s.counts.write.unwrap_or(0) > 0;
        if touched && c.touched_ms.is_none_or(|t| t < at) {
            c.touched_ms = Some(at);
        }
        let k = Kept { conversation: c.id.clone(), at_ms: at, counts: s.counts, cost, why };
        s.total_cost = cost;
        self.aside(s, "keep-warm");
        self.kept.push(k.clone());
        Some(k)
    }

    /// The pings since the conversation's last turn of its own — the stretch it has waited.
    pub fn stretch(&self) -> Vec<&Kept> {
        let Some(c) = &self.conv else { return Vec::new() };
        let since = self.turns.iter().rev().find(|t| t.conversation == c.id).map_or(0, |t| t.at_ms);
        self.kept.iter().filter(|k| k.conversation == c.id && k.at_ms >= since).collect()
    }

    /// The turn ended with no result (claude exited, cut off): what its requests said, no cost.
    pub fn cut_off(&mut self, at_ms: u64) {
        if self.requests.is_empty() && self.compacting.is_none() {
            // (no request: no call either — whatever was counted goes with it)
            self.tally.end(&[], None, at_ms);
            return;
        }
        let conversation = self.conv.as_ref().map(|c| c.id.clone()).unwrap_or_default();
        self.spent(Spent { conversation, at_ms, ..Spent::default() });
    }

    /// A new conversation is about to begin (`C-l`): the last one's cache and context aren't this one's.
    pub fn new_conversation(&mut self) {
        self.conv = None;
        self.requests.clear();
        self.compacting = None;
        self.tally = crate::calls::Tally::default();
    }

    /// Whether anything was reported at all.
    pub fn reported(&self) -> bool {
        !self.turns.is_empty()
            || !self.requests.is_empty()
            || self.conv.as_ref().is_some_and(|c| c.last.is_some())
    }

    /// When the conversation's cache goes cold (unix ms) — None: not reported.
    pub fn cold_at(&self) -> Option<u64> {
        let c = self.conv.as_ref()?;
        Some(c.touched_ms? + c.ttl?.ms())
    }

    pub fn cache(&self, now_ms: u64) -> Cache {
        let (Some(at), Some(ttl)) = (self.cold_at(), self.conv.as_ref().and_then(|c| c.ttl)) else {
            return Cache::NotReported;
        };
        if now_ms < at { Cache::Warm { left_ms: at - now_ms, ttl } } else { Cache::Cold { since_ms: at } }
    }

    /// What the latest request carried — since a compaction, what that kept — and its model's context
    /// window if a result named it.
    pub fn context(&self) -> Option<(u64, Option<u64>)> {
        let c = self.conv.as_ref()?;
        let last = c.last.as_ref()?;
        let used = c.compacted_to.or(last.counts.context())?;
        let window = self.windows.iter().find(|(m, _)| *m == last.model).map(|w| w.1);
        Some((used, window))
    }

    pub fn last_turn(&self) -> Option<&Turn> {
        self.turns.last()
    }

    pub fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for turn in &self.turns {
            t.add(turn);
        }
        t
    }

    /// The conversation `id` as kept with it (`chats.rs`) — None: nothing about it here.
    pub fn conv_json(&self, id: &str) -> Option<Value> {
        let turns: Vec<Value> = self.turns.iter().filter(|t| t.conversation == id).map(Turn::json).collect();
        let c = self.conv.as_ref().filter(|c| c.id == id);
        if turns.is_empty() && c.is_none() {
            return None;
        }
        let mut v = json!({ "turns": turns, "windows": self.windows.iter().map(|(m, w)| json!([m, w]))
            .collect::<Vec<_>>() });
        if let Some(c) = c {
            v["cost_so_far"] = json!(c.cost_so_far);
            v["last"] = c.last.as_ref().map_or(Value::Null, Request::json);
            v["fixed"] = c.fixed.as_ref().map_or(Value::Null, Request::json);
            v["touched"] = json!(c.touched_ms);
            v["ttl"] = json!(c.ttl.map(Ttl::name));
            if let Some(n) = c.compacted_to {
                v["compacted_to"] = json!(n);
            }
        }
        Some(v)
    }

    /// Conversation `id` continued from what was kept: its turns (unless they're here already), its cache,
    /// its cost so far.
    pub fn restore(&mut self, id: &str, v: &Value) {
        if !self.turns.iter().any(|t| t.conversation == id) {
            let kept = v["turns"].as_array().into_iter().flatten().filter_map(|t| Turn::from_json(id, t));
            self.turns.extend(kept);
            crate::sorting::by_key(&mut self.turns, |t| t.at_ms);
        }
        for w in v["windows"].as_array().into_iter().flatten() {
            if let (Some(m), Some(n)) = (w[0].as_str(), w[1].as_u64())
                && !self.windows.iter().any(|(k, _)| k == m)
            {
                self.windows.push((m.to_string(), n));
            }
        }
        self.requests.clear();
        // (what it read before isn't known here)
        self.tally = crate::calls::Tally::default();
        self.conv = Some(Conv {
            id: id.into(),
            last: Request::from_json(&v["last"]),
            touched_ms: v["touched"].as_u64(),
            ttl: v["ttl"].as_str().and_then(Ttl::named),
            fixed: Request::from_json(&v["fixed"]),
            first_next: false,
            cost_so_far: v["cost_so_far"].as_f64(),
            compacted_to: v["compacted_to"].as_u64(),
            tool_results: BTreeMap::new(),
        });
    }

    /// A result of tarae's tool `tool` went to session `session`'s conversation: `sent` bytes — of `output`, a
    /// run's whole output. One `tokens` log line, and counted for `:usage`.
    pub fn tool_result(
        &mut self,
        session: &crate::log::Session,
        tool: &str,
        sent: usize,
        output: Option<usize>,
    ) {
        let extra = output.map(|o| format!(" output={o}")).unwrap_or_default();
        size_line(session, &format!("tool={tool}"), sent, &extra);
        if let Some(c) = &mut self.conv {
            let (calls, bytes) = c.tool_results.entry(tool.to_string()).or_default();
            *calls += 1;
            *bytes += sent as u64;
        }
    }
}

// ── Sizes ───────────────────────────────────────────────────────────────────

/// Bytes a token is, about (English and code) — sizes said before claude counts them.
const BYTES_PER_TOKEN: usize = 4;

/// The `tokens` log line for `bytes` of `what` (`tool=search`, `message`) sent to session `session`'s
/// conversation, `extra` after. Sizes only, never content.
fn size_line(session: &crate::log::Session, what: &str, bytes: usize, extra: &str) {
    let est = bytes / BYTES_PER_TOKEN;
    session.line("tokens", "info", &format!("{what} bytes={bytes} est_tokens={est}{extra}"));
}

/// A user message went to session `session`'s conversation: `bytes` in all, `context` of them the editor
/// context (with what rides along: notes, a screen, what was pointed at).
pub fn message_sent(session: &crate::log::Session, bytes: usize, context: usize) {
    size_line(session, "message", bytes, &format!(" context={context}"));
}

/// The turns of every session that ended today (local time).
pub fn today<'a>(all: impl Iterator<Item = &'a Usage>, now_ms: u64) -> Totals {
    let day = |ms: u64| crate::log::day(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
    let today = day(now_ms);
    let mut t = Totals::default();
    for turn in all.flat_map(|u| &u.turns).filter(|t| day(t.at_ms) == today) {
        t.add(turn);
    }
    t
}

// ── Said ────────────────────────────────────────────────────────────────────

/// `191` · `2.3k` · `23k` · `1.2M` · `1M` (no `.0`).
pub fn tokens(n: u64) -> String {
    let s = match n {
        0..1000 => return n.to_string(),
        1000..10_000 => format!("{:.1}k", n as f64 / 1000.0),
        10_000..1_000_000 => return format!("{}k", (n + 500) / 1000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    };
    s.replace(".0", "")
}

/// `$0.0012` · `$0.023` · `$0.50` · `$1.23`.
pub fn dollars(x: f64) -> String {
    match x {
        x if x < 0.01 => format!("${x:.4}"),
        x if x < 0.1 => format!("${x:.3}"),
        x => format!("${x:.2}"),
    }
}

/// Time left, whole minutes up (never `0m` while warm): `54m` · `5m`.
pub fn left(ms: u64) -> String {
    format!("{}m", ms.div_ceil(60_000))
}

/// When a cache warm until `at_ms` goes cold, local time (`14:32`) — what the footer, the list and `:usage`
/// say: never stale while nothing redraws, as a countdown would be.
pub fn until(at_ms: u64) -> String {
    crate::log::clock(std::time::UNIX_EPOCH + std::time::Duration::from_millis(at_ms))
}

/// `1 turn` · `3 turns`.
pub fn turns(n: usize) -> String {
    if n == 1 { "1 turn".into() } else { format!("{n} turns") }
}

/// A token count, or `not reported`.
pub fn count(n: Option<u64>) -> String {
    n.map_or_else(|| "not reported".into(), tokens)
}

/// `opus 5.5` for `claude-opus-5-5` · `haiku 4.5` for `claude-haiku-4-5-20251001` — another name as it is.
pub fn model_short(m: &str) -> String {
    let Some(rest) = m.strip_prefix("claude-") else { return m.to_string() };
    let parts: Vec<&str> = rest.split('-').filter(|p| p.len() < 8).collect();
    match parts.split_first() {
        Some((family, version)) if !version.is_empty() => format!("{family} {}", version.join(".")),
        _ => m.to_string(),
    }
}

// ── `:usage` ────────────────────────────────────────────────────────────────

/// `:usage`'s plan limits (the account's) and the credential the session's claude reported it runs on.
fn limits_section(u: &Usage, limits: &crate::limits::Limits, now_ms: u64) -> String {
    use crate::limits::{Answer, label, resets, used};
    let clock = |ms: u64| crate::log::clock(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
    let mut out = String::from("## Plan limits\n\n");
    let known = limits.known(now_ms);
    if known.is_empty() {
        out += "Not known yet — a claude says where the weekly window stands when it starts.\n";
    } else {
        let rows: Vec<Vec<String>> = known
            .iter()
            .map(|r| {
                let status = match r.status {
                    Some(crate::limits::Status::Warning) => " (warning)",
                    _ => "",
                };
                vec![
                    label(&r.window),
                    format!("{}{status}", used(r)),
                    r.resets_at_ms.map_or_else(|| "not said".into(), resets),
                    clock(r.seen_at_ms),
                    r.from.name().to_string(),
                ]
            })
            .collect();
        out += &table(&["window", "used", "resets", "as of", "from"], &rows, 2);
    }
    if let Some(plan) = &limits.plan {
        out += &format!("\nPlan: {plan}.\n");
    }
    match limits.answer.as_ref().map(|a| &a.1) {
        Some(Answer::NotAvailable) => {
            out += "\nget_usage: no plan limits for that claude (an API key or a cloud provider).\n"
        }
        Some(Answer::Unknown(why)) => {
            out += &format!(
                "\nget_usage (experimental) answered in a shape tarae doesn't read ({why}) — unknown.\n"
            )
        }
        _ => {}
    }
    out += "\n## Credential\n\n";
    match u.key_source.as_deref() {
        None => out += "Not reported yet — claude says it as a turn starts.\n",
        Some(k) => {
            out += &format!(
                "This session's claude runs on {} (`apiKeySource` `{k}`).\n",
                crate::limits::key_said(k)
            );
            if !crate::limits::on_plan(Some(k), u.plan_event) {
                out += "Billed to the key's Console organization — Console, not tarae, is where billing is \
                        authoritative; the costs here are Claude Code's estimates, and the plan limits above \
                        aren't this session's.\n";
            }
        }
    }
    out
}

/// `:usage`'s text for session `name` (Markdown): the conversation now, the plan's limits and the credential, a
/// row per turn, the session's total, what each request starts with, and today's total across `all` sessions.
pub fn report(
    name: &str,
    u: &Usage,
    limits: &crate::limits::Limits,
    all: &[(&str, &Usage)],
    now_ms: u64,
) -> String {
    let clock = |ms: u64| crate::log::clock(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
    let pct = |n: u64, w: u64| n * 100 / w.max(1);
    let mut out = format!("# Claude usage — {name}\n\n");
    let cache = match u.cache(now_ms) {
        Cache::Warm { left_ms, ttl } => {
            format!("cache warm until {} ({} writes)", until(now_ms + left_ms), ttl.name())
        }
        Cache::Cold { since_ms } => format!("cache cold since {}", clock(since_ms)),
        Cache::NotReported => "cache not reported".into(),
    };
    let context = match u.context() {
        Some((n, Some(w))) => format!("context {} of {} ({}%)", tokens(n), tokens(w), pct(n, w)),
        Some((n, None)) => format!("context {} (window not reported yet)", tokens(n)),
        None => "context not reported".into(),
    };
    out += &format!("As of {} · {cache} · {context}\n\n", clock(now_ms));
    out += &format!("{}\n\n", price_note(u.cli.as_deref()));
    out += &limits_section(u, limits, now_ms);
    out += "\n## Turns\n\n";
    if u.turns.is_empty() {
        out += "No turn has ended yet.\n";
    } else {
        let hit =
            |c: &Counts| c.hit().map_or_else(|| "not reported".into(), |h| format!("{:.0}%", h * 100.0));
        let cells = |c: &Counts| [count(c.input), count(c.write), count(c.read), count(c.output), hit(c)];
        let mut rows: Vec<Vec<String>> = Vec::new();
        for t in &u.turns {
            let models: Vec<String> =
                t.model.split(", ").filter(|m| !m.is_empty()).map(model_short).collect();
            let mut model = if models.is_empty() { "not reported".to_string() } else { models.join(", ") };
            // (a one-turn process's: `space i`'s, a commit draft's, the reviewer's, a board's — `reviewer.rs` —, a
            // keep-warm ping's)
            let c = t.conversation.as_str();
            if matches!(c, "ask" | "draft" | "reviewer" | "keep-warm") || c.starts_with("board:") {
                model = format!("{} · {model}", t.conversation);
            }
            // (a `/compact` turn's 0 tokens and its summary's cost, said for what they are)
            if t.compacted.is_some() {
                model = format!("{model} · compacted");
            }
            let mut row = vec![clock(t.at_ms), model];
            row.extend(cells(&t.counts));
            row.push(t.cost.map_or_else(|| "not reported".into(), dollars));
            rows.push(row);
        }
        let totals = u.totals();
        let mut row = vec!["total".to_string(), turns(totals.turns)];
        row.extend(cells(&totals.counts));
        row.push(totals.cost_said());
        rows.push(row);
        out += &table(
            &["time", "model", "input", "cache write", "cache read", "output", "hit", "API price"],
            &rows,
            2,
        );
    }
    out += &kept_section(u);
    out += &calls_section(u);
    if let Some(c) = &u.conv {
        out += "\n## Every request starts with\n\n";
        out += &match &c.fixed {
            Some(f) => format!(
                "{} tokens (cache write {} · cache read {}) — what this conversation's first request carried: \
                 the system prompt, the tools, CLAUDE.md and the first message, read again by every request \
                 after it.\n",
                f.counts.context().map_or_else(|| "not reported".into(), tokens),
                count(f.counts.write),
                count(f.counts.read)
            ),
            None => {
                "Not known: this conversation's first request wasn't seen here (it was continued).\n".into()
            }
        };
        out += "\n## Tool results\n\n";
        out += &tool_table(&c.tool_results);
    }
    let today_all = today(all.iter().map(|(_, u)| *u), now_ms);
    out += "\n## Today\n\n";
    if today_all.turns == 0 {
        out += "Nothing yet today.\n";
    } else {
        out += &format!("Every session: {} in {}\n", today_all.cost_said(), turns(today_all.turns));
        if all.len() > 1 {
            out += "\n";
            for (n, u) in all {
                let t = today(std::iter::once(*u), now_ms);
                if t.turns > 0 {
                    out += &format!("- {n}: {} in {}\n", t.cost_said(), turns(t.turns));
                }
            }
        }
    }
    out
}

/// `:usage`'s tool calls (`calls.rs`): a row per turn that counted them and the total — calls a request, what
/// a request carried, where the time went, the calls wasted, the runs that only read — then the calls by tool
/// and the read-only runs by program. Nothing when no turn counted calls.
fn calls_section(u: &Usage) -> String {
    use crate::calls::Sums;
    let counted: Vec<(&Turn, &crate::calls::Calls)> =
        u.turns.iter().filter_map(|t| Some((t, t.calls.as_ref()?))).collect();
    if counted.is_empty() {
        return String::new();
    }
    let clock = |ms: u64| crate::log::clock(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
    let secs = |ms: u64| crate::term::duration(ms as f32 / 1000.0);
    let cells = |s: &Sums| {
        vec![
            s.requests.to_string(),
            s.calls.to_string(),
            s.per_request().map_or_else(|| "—".into(), |p| format!("{p:.1}")),
            s.context_mean().map_or_else(|| "—".into(), tokens),
            secs(s.model_ms),
            secs(s.tool_ms),
            secs(s.waiting_ms),
            s.reread.to_string(),
            s.edit_failed.to_string(),
            s.run_refused.to_string(),
            s.read_only.to_string(),
            s.parallel.to_string(),
        ]
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut tools: BTreeMap<&str, u64> = BTreeMap::new();
    let mut programs: BTreeMap<&str, u64> = BTreeMap::new();
    for (t, c) in &counted {
        let mut row = vec![clock(t.at_ms)];
        row.extend(cells(&c.sums));
        rows.push(row);
        for (k, n) in &c.tools {
            *tools.entry(k).or_default() += n;
        }
        for (k, n) in &c.programs {
            *programs.entry(k).or_default() += n;
        }
    }
    let total = u.totals();
    let mut row = vec!["total".to_string()];
    row.extend(cells(&total.calls));
    rows.push(row);
    let mut out = "\n## Tool calls\n\n".to_string();
    out += &table(
        &[
            "time",
            "requests",
            "calls",
            "a request",
            "context a request",
            "model",
            "tools",
            "of it waiting",
            "read again",
            "edits failed",
            "runs refused",
            "read-only runs",
            "side by side",
        ],
        &rows,
        1,
    );
    let listed = |by: &BTreeMap<&str, u64>| {
        let mut v: Vec<(&&str, &u64)> = by.iter().collect();
        crate::sorting::by_key(&mut v, |(_, n)| std::cmp::Reverse(**n));
        v.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(" · ")
    };
    out += &format!("\nBy tool: {}\n", listed(&tools));
    if !programs.is_empty() {
        out += &format!(
            "\nRead-only runs by program (each a process tarae's reads could spare): {}\n",
            listed(&programs)
        );
    }
    if total.calls.declined > 0 {
        out += &format!("\nDeclined by you or tarae (not counted as wasted): {}\n", total.calls.declined);
    }
    out
}

/// What tarae's own tools handed the conversation, by tool, the most bytes first (Claude's own Read, Grep and
/// Edit aren't tarae's to count).
fn tool_table(by: &BTreeMap<String, (u64, u64)>) -> String {
    if by.is_empty() {
        return "None of tarae's tools has answered in this conversation since tarae started.\n".into();
    }
    let est = |b: u64| tokens(b / BYTES_PER_TOKEN as u64);
    let mut tools: Vec<(&String, &(u64, u64))> = by.iter().collect();
    crate::sorting::by_key(&mut tools, |(_, (_, bytes))| std::cmp::Reverse(*bytes));
    let mut rows: Vec<Vec<String>> =
        tools.iter().map(|(t, (n, b))| vec![t.to_string(), n.to_string(), tokens(*b), est(*b)]).collect();
    let (n, b) = by.values().fold((0, 0), |(n, b), (cn, cb)| (n + cn, b + cb));
    rows.push(vec!["total".into(), n.to_string(), tokens(b), est(b)]);
    let mut out = "Since tarae started — every request after one reads it again:\n\n".to_string();
    out += &table(&["tool", "calls", "bytes", "≈ tokens"], &rows, 1);
    out
}

/// A Markdown table, its columns padded (numbers right-aligned — all but the first `left`).
fn table(head: &[&str], rows: &[Vec<String>], left: usize) -> String {
    use unicode_width::UnicodeWidthStr as _;
    let widths: Vec<usize> = (0..head.len())
        .map(|c| rows.iter().map(|r| r[c].width()).chain([head[c].width()]).max().unwrap_or(0))
        .collect();
    let line = |cells: Vec<String>| format!("| {} |\n", cells.join(" | "));
    let pad = |c: usize, s: &str| {
        let gap = " ".repeat(widths[c] - s.width());
        if c < left { format!("{s}{gap}") } else { format!("{gap}{s}") }
    };
    let mut out = line(head.iter().enumerate().map(|(c, h)| pad(c, h)).collect());
    let rule = widths
        .iter()
        .enumerate()
        .map(|(c, w)| if c < left { "-".repeat(*w) } else { format!("{}:", "-".repeat(w - 1)) });
    out += &line(rule.collect());
    for r in rows {
        out += &line(r.iter().enumerate().map(|(c, s)| pad(c, s)).collect());
    }
    out
}

/// The report for session `sid` as things are now.
fn report_now(ed: &Editor, sid: crate::event::SessionId) -> Option<String> {
    let sessions = ed.sessions_in_order();
    let all: Vec<(&str, &Usage)> = sessions.iter().map(|l| (l.label(), &l.usage)).collect();
    let l = sessions.iter().find(|l| l.id == sid)?;
    Some(report(l.label(), &l.usage, &ed.limits, &all, now_ms()))
}

/// `:usage` — the front session's usage in a buffer of its own (Markdown, nothing to save); said again as
/// its turns end while it's open, and as the plan's limits come — asked of its chat's claude when nothing is fresh.
pub fn open(ed: &mut Editor) {
    let sid = ed.session.id;
    crate::limits::ask_chat(ed);
    let Some(text) = report_now(ed, sid) else { return };
    let at = ed.usage_view.and_then(|(id, _)| ed.docs.iter().position(|d| d.id == id));
    let id = match at {
        Some(i) => {
            crate::disk::reload_text(&mut ed.docs[i], &ropey::Rope::from_str(&text));
            ed.current = i;
            ed.docs[i].id
        }
        None => {
            let id = ed.open_virtual("usage", &text);
            if let Some(spec) = crate::syntax::spec("markdown") {
                ed.load_language(id, spec);
            }
            id
        }
    };
    ed.session_keeps(id);
    ed.usage_view = Some((id, sid));
}

/// A turn ended or a cache went cold: `:usage`'s buffer, while it's open, says it.
pub fn view_sync(ed: &mut Editor) {
    let Some((id, sid)) = ed.usage_view else { return };
    let Some(i) = ed.docs.iter().position(|d| d.id == id) else {
        ed.usage_view = None;
        return;
    };
    if let Some(text) = report_now(ed, sid) {
        crate::disk::reload_text(&mut ed.docs[i], &ropey::Rope::from_str(&text));
    }
}

// ── Cold, said once ─────────────────────────────────────────────────────────

/// `:usage`'s pings that kept a conversation warm (`keepwarm.rs`) — none: nothing.
fn kept_section(u: &Usage) -> String {
    if u.kept.is_empty() {
        return String::new();
    }
    let clock = |ms: u64| crate::log::clock(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
    let mut rows: Vec<Vec<String>> = u
        .kept
        .iter()
        .map(|k| {
            let cost = k.cost.map_or_else(|| "not reported".into(), dollars);
            vec![clock(k.at_ms), k.why.to_string(), count(k.counts.read), count(k.counts.write), cost]
        })
        .collect();
    let costs: Option<f64> = u.kept.iter().map(|k| k.cost).sum();
    let n = u.kept.len();
    rows.push(vec![
        "total".into(),
        format!("{n} ping{}", if n == 1 { "" } else { "s" }),
        String::new(),
        String::new(),
        costs.map_or_else(|| "not reported".into(), dollars),
    ]);
    let mut out = "\n## Kept warm\n\n".to_string();
    out += "A one-word ping of a fork of the conversation before its cache went cold, while it waited on you \
             (llm.keep-warm) — its read refreshes the cache.\n\n";
    out += &table(&["time", "why", "cache read", "cache write", "API price"], &rows, 2);
    out
}

/// After a turn: the main loop is woken when this session's cache goes cold — one frame then (the footer and
/// the list say so) and a `usage` event for watchers; nothing before — but, when a ping may keep it warm, at the
/// window before (`keepwarm.rs`), to decide. One watch asleep per session: one that wakes no later looks again
/// when it wakes.
pub fn watch(ed: &mut Editor) {
    if let Some(at) = wakes_at(&ed.session.usage, ed.config.llm.keep_warm, now_ms()) {
        sleep_until(ed, at);
    }
}

/// When the watch of `u` wakes after `now_ms`: the window a ping may go in (on the 1h TTL, pings not off), else
/// when its cache goes cold — None: cold already, or not reported.
pub fn wakes_at(u: &Usage, keep_warm: crate::keepwarm::Mode, now_ms: u64) -> Option<u64> {
    let cold = u.cold_at().filter(|&at| at > now_ms)?;
    let hour = u.conv.as_ref().is_some_and(|c| c.ttl == Some(Ttl::Hour1));
    let window = cold.saturating_sub(crate::keepwarm::LEAD_MS);
    let pings = keep_warm != crate::keepwarm::Mode::Off && hour;
    Some(if pings && window > now_ms { window } else { cold })
}

/// The session's watch wakes at `at` (unix ms) — unless one sleeping wakes no later.
pub fn sleep_until(ed: &mut Editor, at: u64) {
    let now = now_ms();
    if at <= now || ed.session.usage.watch.is_some_and(|w| w <= at) || cfg!(test) {
        return;
    }
    ed.session.usage.watch = Some(at);
    ed.events.jobs().spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(at - now));
        woke
    });
}

/// What the watch finds when it wakes at `now_ms`.
#[derive(Debug, PartialEq, Eq)]
pub enum Woke {
    /// Gone cold — said.
    Cold,
    /// A later request moved it: asleep again.
    Later,
    Nothing,
}

pub fn woke_at(u: &mut Usage, now_ms: u64) -> Woke {
    u.watch = None;
    match u.cold_at() {
        Some(at) if at > now_ms => Woke::Later,
        Some(_) => Woke::Cold,
        None => Woke::Nothing,
    }
}

fn woke(ed: &mut Editor) {
    match woke_at(&mut ed.session.usage, now_ms()) {
        Woke::Cold => {
            crate::uistate::watch::usage(ed, "cold");
            view_sync(ed);
            // (behind, nothing on screen says it unless the list is open)
            if ed.background() && !ed.picker.as_ref().is_some_and(|p| p.kind == "sessions") {
                ed.unchanged = true;
            }
        }
        Woke::Later => {
            // (inside the window: a ping, a look again later, or nothing)
            crate::keepwarm::woke(ed);
            watch(ed);
            ed.unchanged = true;
        }
        Woke::Nothing => ed.unchanged = true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorded stream (`usage_stream.jsonl`: two turns in one process, then a third after `--resume`)
    /// read as the chat reads it.
    fn recorded() -> (Usage, Vec<Spent>) {
        let mut u = Usage::default();
        u.fresh("cda055cc-67c1-4da7-9120-68435b027d4f");
        let mut results = Vec::new();
        for (i, line) in include_str!("usage_stream.jsonl").lines().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            match crate::llm::usage_line(&v, 1_000 * i as u64) {
                Some(crate::llm::StreamEv::Requesting(at)) => u.requesting(at),
                Some(crate::llm::StreamEv::Usage(s)) => u.seen(s),
                Some(crate::llm::StreamEv::Spent(s)) => {
                    results.push(s.clone());
                    u.spent(s);
                }
                _ => {}
            }
        }
        (u, results)
    }

    /// A turn's call-order record entry: its requests in order, each with its calls' tool names in the order
    /// they came (an unknown tool `other`) and its context and output — a request without calls is a step
    /// with none; never a call's input. An aside's entry has no steps.
    #[test]
    fn a_turn_is_recorded_as_its_requests_tool_names_in_order() {
        let mut u = Usage::default();
        u.fresh("c1");
        let usage = |out: u64| {
            json!({ "input_tokens": 10, "cache_read_input_tokens": 20_000,
                                       "cache_creation_input_tokens": 500, "output_tokens": out })
        };
        let line = |u: &mut Usage, v: Value| match crate::llm::usage_line(&v, 0) {
            Some(crate::llm::StreamEv::Usage(s)) => u.seen(s),
            Some(crate::llm::StreamEv::Spent(s)) => {
                u.spent(s);
            }
            _ => {}
        };
        let call = |u: &mut Usage, id: &str, name: &str| {
            let input = json!({ "pattern": "needle-in-the-input", "file_path": "/p/secret.rs" });
            u.tally.called(id, name, &input, std::path::Path::new("/p"), None, 0);
        };
        for (id, tools) in
            [("m1", &["mcp__tarae__search", "mcp__other__fetch"][..]), ("m2", &["Read"]), ("m3", &[])]
        {
            let start = json!({ "type": "stream_event", "event": { "type": "message_start",
                "message": { "id": id, "model": "claude-opus-5-5", "usage": usage(1) } } });
            line(&mut u, start);
            let whole = json!({ "type": "assistant", "message": { "id": id, "model": "claude-opus-5-5",
                "content": [], "usage": usage(120) } });
            line(&mut u, whole);
            for (i, t) in tools.iter().enumerate() {
                call(&mut u, &format!("{id}-{i}"), t);
            }
        }
        line(
            &mut u,
            json!({ "type": "result", "session_id": "c1", "total_cost_usd": 0.05, "duration_ms": 9_000,
                             "usage": usage(360) }),
        );
        assert_eq!(u.to_record.len(), 1);
        let e = &u.to_record[0];
        assert_eq!(
            (e["v"].as_u64(), e["what"].as_str(), e["requests"].as_u64()),
            (Some(1), Some("chat"), Some(3))
        );
        let steps: Vec<Value> = e["steps"].as_array().unwrap().iter().map(|s| s["tools"].clone()).collect();
        assert_eq!(steps, [json!(["search", "other"]), json!(["Read"]), json!([])]);
        assert_eq!(
            (e["steps"][0]["context"].as_u64(), e["steps"][0]["output"].as_u64()),
            (Some(20_510), Some(120))
        );
        assert_eq!((e["calls"]["calls"].as_u64(), e["cost"].as_f64()), (Some(3), Some(0.05)));
        let text = e.to_string();
        assert!(!text.contains("needle") && !text.contains("secret") && !text.contains("fetch"), "{text}");
        // The next turn starts with no steps; an aside's entry has none
        u.aside(
            Spent { conversation: "reviewer".into(), total_cost: Some(0.2), ..Spent::default() },
            "reviewer",
        );
        assert_eq!(u.to_record.len(), 2);
        assert_eq!(
            (u.to_record[1]["what"].as_str(), u.to_record[1]["steps"].clone()),
            (Some("reviewer"), json!([]))
        );
        assert!(u.to_record[1]["requests"].is_null(), "its requests weren't counted: not 0");
        assert!(u.tally.take_steps().is_empty());
    }

    #[test]
    fn a_turns_requests_add_up_to_its_result() {
        let (u, results) = recorded();
        assert_eq!(u.turns.len(), 3);
        assert_eq!(u.turns.iter().map(|t| t.requests).collect::<Vec<_>>(), [2, 1, 1]);
        // Each turn's requests, summed as they streamed, are what its result says
        let mut u2 = Usage::default();
        let mut sums = vec![Counts::default()];
        for (i, line) in include_str!("usage_stream.jsonl").lines().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            match crate::llm::usage_line(&v, i as u64) {
                Some(crate::llm::StreamEv::Usage(s)) => u2.seen(s),
                Some(crate::llm::StreamEv::Spent(_)) => {
                    let mut sum = Counts::default();
                    for r in std::mem::take(&mut u2.requests) {
                        sum.add(r.counts);
                    }
                    *sums.last_mut().unwrap() = sum;
                    sums.push(Counts::default());
                }
                _ => {}
            }
        }
        sums.pop();
        let said: Vec<Counts> = results.iter().map(|s| s.counts).collect();
        assert_eq!(sums, said, "the requests' final counts (message_delta's output) sum to the result's");
        assert_eq!(
            said[0],
            Counts { input: Some(18), write: Some(8125), read: Some(7924), output: Some(185) },
            "as recorded"
        );
        // The costs: the result's total is the conversation's so far — a turn's is the difference
        let costs: Vec<f64> = u.turns.iter().map(|t| t.cost.unwrap()).collect();
        let totals: Vec<f64> = results.iter().map(|s| s.total_cost.unwrap()).collect();
        assert!((costs.iter().sum::<f64>() - totals[2]).abs() < 1e-9, "{costs:?} vs {totals:?}");
        assert!((costs[1] - (totals[1] - totals[0])).abs() < 1e-12);
        assert!((u.totals().cost - totals[2]).abs() < 1e-9);
        let mut whole = Counts::default();
        for s in &results {
            whole.add(s.counts);
        }
        assert_eq!(u.totals().counts, whole);
        // The model's window came with the result; the context is the last request's
        assert_eq!(u.context(), Some((10 + 75 + 8211, Some(200_000))));
        let c = u.conv.as_ref().unwrap();
        assert_eq!(c.fixed.as_ref().unwrap().counts.write, Some(7924), "the first request's cache write");
        assert_eq!(c.ttl, Some(Ttl::Hour1), "it wrote to the 1h cache");
        assert_eq!(u.turns[0].model, "claude-haiku-4-5-20251001");
    }

    /// A conversation continued without what tarae kept: its first turn's cost isn't known (the total is the
    /// whole conversation's) — said as not reported; the next one's is.
    #[test]
    fn a_resumed_conversations_earlier_cost_isnt_guessed() {
        let mut u = Usage::default();
        u.resumed("c");
        let spent = |total: f64, at_ms: u64| Spent {
            conversation: "c".into(),
            counts: Counts { input: Some(1), ..Counts::default() },
            total_cost: Some(total),
            at_ms,
            ..Spent::default()
        };
        assert_eq!(u.spent(spent(5.0, 1)).cost, None);
        assert_eq!(u.spent(spent(5.5, 2)).cost, Some(0.5));
        assert_eq!(u.totals().cost_said(), "$0.50 + 1 not reported");
        // Kept and continued: the cost so far comes back with it
        let kept = u.conv_json("c").unwrap();
        let mut back = Usage::default();
        back.restore("c", &kept);
        assert_eq!(back.turns.len(), 2);
        assert_eq!(back.spent(spent(6.0, 3)).cost, Some(0.5));
    }

    #[test]
    fn the_cache_goes_cold_at_its_ttl() {
        let mut u = Usage::default();
        u.fresh("c");
        let usage = |w5: u64, w1: u64, read: u64| {
            json!({ "input_tokens": 3, "cache_creation_input_tokens": w5 + w1, "cache_read_input_tokens": read,
                    "cache_creation": { "ephemeral_5m_input_tokens": w5, "ephemeral_1h_input_tokens": w1 } })
        };
        let seen = |id: &str, v: &Value, at_ms: u64| Seen {
            id: Some(id.into()),
            model: "m".into(),
            counts: Counts::of(v),
            ttl: Ttl::of(v),
            at_ms,
        };
        assert_eq!(u.cache(0), Cache::NotReported, "nothing sent yet");
        u.requesting(1_000);
        u.seen(seen("a", &usage(500, 0, 0), 1_400));
        let five = Ttl::Min5.ms();
        assert_eq!(
            u.cache(1_000 + five - 1),
            Cache::Warm { left_ms: 1, ttl: Ttl::Min5 },
            "from when it went out"
        );
        assert_eq!(u.cache(1_000 + five), Cache::Cold { since_ms: 1_000 + five }, "cold at the boundary");
        assert_eq!(left(1), "1m", "never 0m while warm");
        // A read refreshes it; its TTL stays the last write's
        u.seen(seen("b", &usage(0, 0, 500), 60_000));
        assert_eq!(u.cache(1_000 + five), Cache::Warm { left_ms: 59_000, ttl: Ttl::Min5 });
        u.seen(seen("c", &usage(0, 200, 700), 120_000));
        assert_eq!(u.cold_at(), Some(120_000 + Ttl::Hour1.ms()), "a 1h write: an hour");
        // The watch: woken early (a later request moved it) it sleeps again; at the time, cold
        assert_eq!(woke_at(&mut u, 120_000 + five), Woke::Later);
        assert_eq!(woke_at(&mut u, 120_000 + Ttl::Hour1.ms()), Woke::Cold);
    }

    /// Another CLI's stream with no usage in it: nothing made up.
    #[test]
    fn what_isnt_reported_stays_so() {
        let mut u = Usage::default();
        u.fresh("c");
        let t = u.spent(Spent { conversation: "c".into(), at_ms: 1, ..Spent::default() });
        assert_eq!((t.counts, t.cost), (Counts::default(), None));
        assert_eq!(u.cache(5), Cache::NotReported);
        assert_eq!(u.context(), None);
        assert_eq!(u.totals().cost_said(), "not reported");
        assert_eq!(count(None), "not reported");
        // A write that doesn't say its kind (no `cache_creation` split, as parsed from the stream): the TTL isn't
        // guessed
        let v = json!({ "cache_creation_input_tokens": 9 });
        assert_eq!(Ttl::of(&v), None);
        u.seen(Seen { id: Some("x".into()), counts: Counts::of(&v), ttl: Ttl::of(&v), ..Seen::default() });
        assert_eq!(u.cache(5), Cache::NotReported);
    }

    #[test]
    fn numbers_are_said_short() {
        assert_eq!(
            [tokens(191), tokens(2_317), tokens(23_456), tokens(1_234_567)],
            ["191", "2.3k", "23k", "1.2M"]
        );
        assert_eq!([tokens(1_000_000), tokens(2_040)], ["1M", "2k"]);
        assert_eq!([dollars(0.0011445), dollars(0.0233754), dollars(1.234)], ["$0.0011", "$0.023", "$1.23"]);
        assert_eq!(model_short("claude-opus-5-5"), "opus 5.5");
        assert_eq!(model_short("claude-haiku-4-5-20251001"), "haiku 4.5");
        assert_eq!(model_short("gemini-2.5-pro"), "gemini-2.5-pro");
    }
}
