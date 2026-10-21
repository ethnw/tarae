//! `cargo xtask turns [PATH …]` — the call-order record (`src/calls.rs`: `$state/calls/<yyyy-mm>.jsonl`, every
//! file in a folder given, the state folder's when none is) summed up as code mode's baseline reads it
//! (`target/sb/codemode-plan.md` §1, §4's gate):
//!
//! - **by kind** — a chat turn is a question (no change made: no `edit`, `Edit`, `Write`, `rename`, `replace` or
//!   `commit`, every `run` read-only), a refactor (a `rename` or `replace`) or a change (the rest); a `/compact`
//!   turn (compacted, no request) is its own row, as are the reviewer's, `space i`'s and a commit draft's — none
//!   of those in the numbers below. Requests (an aside's aren't counted: `—`), calls a request, context a
//!   request, $, and the cost's split estimated at Opus 5.5's list prices;
//! - **round trips in a row** — requests with calls one after another: each waited on the one before's results,
//!   so a run of them is a chain of round trips; the share of requests in runs of 3 or more is the gate's first
//!   number;
//! - **step pairs** — which tools follow which, request after request (`search → read`);
//! - **large results** — a request's context over the one before's plus its output: what that one's calls
//!   brought back; turns with one over 8k tokens are the gate's second number.
//!
//! Names and numbers only: the record holds no call's arguments or results. A line that isn't an entry (a
//! write cut short) is counted and skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Result;

/// Opus 5.5's list prices, $ per million tokens: input · cache write (5 min) · cache read · output.
const PRICES: [f64; 4] = [4.0, 5.0, 0.20, 20.0];
/// A run of round trips this long or longer counts toward the gate.
const CHAIN: usize = 3;
/// A request's calls brought back more than this many tokens: a large result.
const LARGE: u64 = 8_000;
/// The tools that change something.
const CHANGES: [&str; 6] = ["edit", "Edit", "Write", "rename", "replace", "commit"];

pub fn run(args: &[String]) -> Result<()> {
    let paths: Vec<PathBuf> = match args.is_empty() {
        true => {
            vec![state_calls().ok_or("neither XDG_STATE_HOME nor HOME is set: give the record's folder")?]
        }
        false => args.iter().map(PathBuf::from).collect(),
    };
    let mut files = Vec::new();
    for p in &paths {
        files.extend(jsonl_in(p)?);
    }
    if files.is_empty() {
        return Err(format!("no call-order record in {}", paths[0].display()));
    }
    let mut text = String::new();
    for f in &files {
        text += &std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    print!("{}", report(&names.join(", "), &text));
    Ok(())
}

/// `$XDG_STATE_HOME/tarae/calls` (`~/.local/state/tarae/calls`) — as `config::state_dir` finds it.
fn state_calls() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("tarae").join("calls"))
}

/// `p` itself, or the `.jsonl` files in it, oldest month first.
fn jsonl_in(p: &Path) -> Result<Vec<PathBuf>> {
    if !p.is_dir() {
        return Ok(vec![p.to_path_buf()]);
    }
    let rd = std::fs::read_dir(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut v: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|f| f.extension().is_some_and(|x| x == "jsonl")).collect();
    v.sort();
    Ok(v)
}

/// One turn, as the report reads it.
struct Turn {
    kind: &'static str,
    /// Each request's tool names, in order.
    steps: Vec<Vec<String>>,
    /// Each request's context and output (None: not reported).
    sizes: Vec<(Option<u64>, Option<u64>)>,
    /// None: not counted (an aside's).
    requests: Option<u64>,
    calls: u64,
    context: u64,
    cost: Option<f64>,
    /// input · cache write · cache read · output tokens.
    counts: [u64; 4],
}

impl Turn {
    fn of(e: &Value) -> Turn {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let steps: Vec<Vec<String>> = e["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                s["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .collect();
        let sizes = e["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| (s["context"].as_u64(), s["output"].as_u64()))
            .collect();
        let tools = || steps.iter().flatten().map(String::as_str);
        let runs = tools().filter(|t| *t == "run").count() as u64;
        let kind = match e["what"].as_str().unwrap_or("chat") {
            "reviewer" => "reviewer",
            "ask" => "ask (space i)",
            "draft" => "commit draft",
            // (`/compact`'s turn: no request of its own, the summary's cost)
            _ if e["compacted"].is_string() && e["requests"].as_u64() == Some(0) => "compaction (/compact)",
            _ if tools().any(|t| t == "rename" || t == "replace") => "refactor",
            _ if tools().any(|t| CHANGES.contains(&t)) || runs > n(&e["calls"]["read_only_runs"]) => "change",
            _ => "question",
        };
        let c = &e["counts"];
        Turn {
            kind,
            sizes,
            requests: e["requests"].as_u64(),
            calls: n(&e["calls"]["calls"]),
            context: n(&e["calls"]["context"]),
            cost: e["cost"].as_f64(),
            counts: [n(&c["input"]), n(&c["write"]), n(&c["read"]), n(&c["output"])],
            steps,
        }
    }

    /// Runs of requests with calls one after another, by their lengths.
    fn chains(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut run = 0;
        for s in &self.steps {
            match s.is_empty() {
                true if run > 0 => {
                    out.push(run);
                    run = 0;
                }
                true => {}
                false => run += 1,
            }
        }
        if run > 0 {
            out.push(run);
        }
        out
    }

    /// What each request's calls brought back: the next request's context over this one's and its output.
    fn results(&self) -> Vec<u64> {
        self.sizes
            .windows(2)
            .filter_map(|w| match (w[0], w[1]) {
                ((Some(c), Some(o)), (Some(next), _)) => Some(next.saturating_sub(c + o)),
                _ => None,
            })
            .collect()
    }
}

/// A request's calls as a label: `search`, `read×3`, `search+read`.
fn label(step: &[String]) -> String {
    let mut parts: Vec<(&str, usize)> = Vec::new();
    for t in step {
        match parts.last_mut() {
            Some((last, n)) if last == t => *n += 1,
            _ => parts.push((t, 1)),
        }
    }
    let parts: Vec<String> =
        parts.iter().map(|(t, n)| if *n > 1 { format!("{t}×{n}") } else { t.to_string() }).collect();
    parts.join("+")
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// The 90th percentile (nearest rank).
fn p90(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    let i = (v.len() * 9).div_ceil(10).checked_sub(1)?;
    v.get(i).copied()
}

fn pct(n: u64, of: u64) -> String {
    (n * 100 + of / 2).checked_div(of).map_or_else(|| "—".into(), |p| format!("{p} %"))
}

fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

fn dollars(x: f64) -> String {
    format!("${x:.3}")
}

/// A Markdown table, its columns padded to line up in a terminal too.
fn table(head: &[&str], rows: &[Vec<String>]) -> String {
    let w: Vec<usize> = (0..head.len())
        .map(|c| {
            rows.iter().map(|r| r[c].chars().count()).chain([head[c].chars().count()]).max().unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<String>| {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(c, x)| format!("{x}{}", " ".repeat(w[c] - x.chars().count())))
            .collect();
        format!("| {} |\n", padded.join(" | "))
    };
    let mut out = line(head.iter().map(|h| h.to_string()).collect());
    out += &line(w.iter().map(|w| "-".repeat(*w)).collect());
    for r in rows {
        out += &line(r.clone());
    }
    out
}

/// The whole report for the record's `text` (its files named `from`).
fn report(from: &str, text: &str) -> String {
    let mut broken = 0;
    let all: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let v = serde_json::from_str::<Value>(l).ok().filter(Value::is_object);
            broken += usize::from(v.is_none());
            v
        })
        .collect();
    let mut out = format!("# Turns\n\nFrom {from}: {} entries", all.len());
    if let (Some(first), Some(last)) = (all.first(), all.last()) {
        out += &format!(", {} – {}", first["at"].as_str().unwrap_or(""), last["at"].as_str().unwrap_or(""));
    }
    if broken > 0 {
        out += &format!(" ({broken} line{} not an entry, skipped)", if broken == 1 { "" } else { "s" });
    }
    out += ".\n";
    let turns: Vec<Turn> = all.iter().map(Turn::of).collect();

    // By kind
    out += "\n## By kind\n\nThe cost's split is estimated at Opus 5.5's list prices (5-minute cache writes); $ is \
            what claude reported.\n\n";
    let mut kinds: BTreeMap<&str, Vec<&Turn>> = BTreeMap::new();
    for t in &turns {
        kinds.entry(t.kind).or_default().push(t);
    }
    let rows: Vec<Vec<String>> = kinds
        .iter()
        .map(|(k, ts)| {
            let requests: Vec<u64> = ts.iter().filter_map(|t| t.requests).collect();
            let (calls, reqs, ctx) = ts
                .iter()
                .fold((0, 0, 0), |a, t| (a.0 + t.calls, a.1 + t.requests.unwrap_or(0), a.2 + t.context));
            let costs: Vec<f64> = ts.iter().filter_map(|t| t.cost).collect();
            let mut sorted = costs.clone();
            sorted.sort_by(f64::total_cmp);
            let parts: Vec<f64> =
                (0..4).map(|i| ts.iter().map(|t| t.counts[i] as f64 * PRICES[i]).sum()).collect();
            let whole: f64 = parts.iter().sum();
            let share = |i: usize| match whole > 0.0 {
                true => format!("{:.0} %", parts[i] * 100.0 / whole),
                false => "—".into(),
            };
            let or_dash = |v: Option<u64>| v.map_or_else(|| "—".into(), |v| v.to_string());
            vec![
                k.to_string(),
                ts.len().to_string(),
                format!("{} · {}", or_dash(median(requests.clone())), or_dash(p90(requests))),
                match reqs {
                    0 => "—".into(),
                    r => format!("{:.1}", calls as f64 / r as f64),
                },
                ctx.checked_div(reqs).map_or_else(|| "—".into(), tokens),
                sorted.get(sorted.len() / 2).map_or_else(|| "—".into(), |c| dollars(*c)),
                dollars(costs.iter().sum()),
                format!("{} · {} · {} · {}", share(2), share(1), share(3), share(0)),
            ]
        })
        .collect();
    out += &table(
        &[
            "Kind",
            "Turns",
            "Requests (median · p90)",
            "Calls a request",
            "Context a request",
            "$ median",
            "$ total",
            "Re-read · write · output · input",
        ],
        &rows,
    );

    // Round trips in a row
    let chat: Vec<&Turn> =
        turns.iter().filter(|t| matches!(t.kind, "question" | "change" | "refactor")).collect();
    let requests: u64 = chat.iter().map(|t| t.steps.len() as u64).sum();
    let chains: Vec<usize> = chat.iter().flat_map(|t| t.chains()).collect();
    let in_long: u64 = chains.iter().filter(|n| **n >= CHAIN).map(|n| *n as u64).sum();
    let mut lengths: BTreeMap<usize, usize> = BTreeMap::new();
    for n in &chains {
        *lengths.entry(*n).or_default() += 1;
    }
    out += &format!(
        "\n## Round trips in a row\n\nRequests with calls one after another (each waited on the last one's results), \
         in the chat's turns: {requests} requests, {} runs.\n\n",
        chains.len()
    );
    let rows: Vec<Vec<String>> = lengths
        .iter()
        .map(|(n, c)| vec![n.to_string(), c.to_string(), pct((*n * *c) as u64, requests)])
        .collect();
    out += &table(&["Run of", "Runs", "Requests in them"], &rows);

    // Step pairs
    // (a pair of step labels → how often, and the second's contexts)
    type Pairs = BTreeMap<(String, String), (usize, Vec<u64>)>;
    let mut pairs = Pairs::new();
    for t in &chat {
        for (i, w) in t.steps.windows(2).enumerate() {
            if w[0].is_empty() || w[1].is_empty() {
                continue;
            }
            let p = pairs.entry((label(&w[0]), label(&w[1]))).or_default();
            p.0 += 1;
            p.1.extend(t.sizes.get(i + 1).and_then(|s| s.0));
        }
    }
    let mut pairs: Vec<_> = pairs.into_iter().collect();
    pairs.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
    out += "\n## Step pairs\n\nOne request's calls, then the next one's — the most frequent first (20 at most).\n\n";
    let rows: Vec<Vec<String>> = pairs
        .iter()
        .take(20)
        .map(|((a, b), (n, ctx))| {
            vec![format!("{a} → {b}"), n.to_string(), median(ctx.clone()).map_or_else(|| "—".into(), tokens)]
        })
        .collect();
    out += &table(&["Pair", "Times", "Context at the second (median)"], &rows);

    // Large results
    let large = chat.iter().filter(|t| t.results().iter().any(|r| *r > LARGE)).count() as u64;
    let results: Vec<u64> = chat.iter().flat_map(|t| t.results()).collect();
    out += &format!(
        "\n## Large results\n\nWhat a request's calls brought back (the next context over this one's and its output): \
         median {}, p90 {}; turns with one over {}: {large} of {} ({}).\n",
        median(results.clone()).map_or_else(|| "—".into(), tokens),
        p90(results).map_or_else(|| "—".into(), tokens),
        tokens(LARGE),
        chat.len(),
        pct(large, chat.len() as u64),
    );

    // The gate
    out += &format!(
        "\n## The gate for (b)\n\n- Requests in runs of {CHAIN} or more: {} (opens at 25 %).\n- Turns with a result over \
         {}: {} (opens at 10 %).\n",
        pct(in_long, requests),
        tokens(LARGE),
        pct(large, chat.len() as u64),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's record summed up: kinds (a run that may write makes a change; a rename a refactor; a
    /// `/compact` turn, the reviewer and a commit draft apart, their requests `—` where they weren't counted),
    /// runs of round trips and their share, step pairs with repeats folded (`read×2`), what calls brought
    /// back, the gate (none of the rows apart in it) — and the broken line skipped, said.
    #[test]
    fn the_record_reads_as_the_baseline() {
        let r = report("calls.jsonl", include_str!("../fixtures/calls.jsonl"));
        let has = |s: &str| assert!(r.contains(s), "{s:?} not in:\n{r}");
        // (a kind's row: its turns and requests, the padding left out)
        let row = |kind: &str| -> Vec<String> {
            let line = r.lines().find(|l| l.split('|').nth(1).is_some_and(|c| c.trim() == kind));
            let line = line.unwrap_or_else(|| panic!("no row {kind}:\n{r}"));
            line.split('|').skip(2).take(2).map(|c| c.trim().to_string()).collect()
        };
        has(
            "From calls.jsonl: 7 entries, 2026-10-17 10:00:00 – 2026-10-17 11:10:00 (1 line not an entry, skipped).",
        );
        assert_eq!(row("change"), ["1", "3 · 3"]);
        assert_eq!(row("question"), ["2", "5 · 5"]);
        assert_eq!(row("refactor"), ["1", "2 · 2"]);
        assert_eq!(row("compaction (/compact)"), ["1", "0 · 0"]);
        assert_eq!(row("reviewer"), ["1", "— · —"]);
        assert_eq!(row("commit draft"), ["1", "— · —"]);
        has("in the chat's turns: 14 requests, 4 runs.");
        has("| 4      | 1    | 29 %");
        has("| search → read×2     | 2     | 21.0k");
        has("| read×2 → references | 1     | 32.0k");
        has("turns with one over 8.0k: 1 of 4 (25 %).");
        has("- Requests in runs of 3 or more: 50 % (opens at 25 %).");
    }
}
