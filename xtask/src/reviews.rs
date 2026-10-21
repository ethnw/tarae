//! `cargo xtask reviews [PATH …]` — the review record (`src/reviews.rs`: `$state/reviews/<yyyy-mm>.jsonl`, every
//! file in a folder given, the state folder's when none is) summed up as the review lens's plan reads it
//! (`target/sb/reviewlens-plan.md` §4):
//!
//! - **time per change** by grade and size: the median time on screen with you there, per change and per changed
//!   line, and the share **acted on** (rejected, undone, edited or commented — the guard: faster mustn't mean
//!   blinder);
//! - **what changes ended as**, by the kind of review;
//! - **reviews**: active minutes per 100 changed lines;
//! - **lens on and off**: the same numbers by which lens pieces were on (`lens`, none yet: `off`);
//! - **high-risk rules**: each rule's marks and what came of them — one that only ever got a look is a candidate
//!   to narrow, by hand.
//!
//! A line that isn't an entry (a write cut short) is counted and skipped.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::Result;
use crate::cards::{jsonl_in, median, pct, s, secs, table};

/// What a person did to a change, beyond reading it — the guard's count.
const ACTED: [&str; 4] = ["rejected", "undone", "edited", "commented"];

pub fn run(args: &[String]) -> Result<()> {
    let paths: Vec<PathBuf> = match args.is_empty() {
        true => {
            vec![state_reviews().ok_or("neither XDG_STATE_HOME nor HOME is set: give the record's folder")?]
        }
        false => args.iter().map(PathBuf::from).collect(),
    };
    let mut files = Vec::new();
    for p in &paths {
        files.extend(jsonl_in(p)?);
    }
    if files.is_empty() {
        return Err(format!("no review record in {}", paths[0].display()));
    }
    let mut text = String::new();
    for f in &files {
        text += &std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    print!("{}", report(&names.join(", "), &text));
    Ok(())
}

/// `$XDG_STATE_HOME/tarae/reviews` (`~/.local/state/tarae/reviews`) — as `config::state_dir` finds it.
fn state_reviews() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("tarae").join("reviews"))
}

fn n(e: &Value, k: &str) -> u64 {
    e[k].as_u64().unwrap_or(0)
}

/// A change's lines, added and removed.
fn lines(e: &Value) -> u64 {
    n(&e["lines"], "added") + n(&e["lines"], "removed")
}

fn acted(e: &Value) -> bool {
    ACTED.contains(&s(e, "outcome"))
}

/// The lens pieces that were on (`off`: none).
fn lens(e: &Value) -> String {
    let on: Vec<&str> = e["lens"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if on.is_empty() { "off".into() } else { on.join("+") }
}

fn size(lines: u64) -> &'static str {
    match lines {
        0..=5 => "1–5",
        6..=30 => "6–30",
        _ => "31+",
    }
}

/// `3 accepted · 1 rejected` — how often each outcome, most first.
fn outcomes(es: &[&Value]) -> String {
    let mut c: BTreeMap<&str, usize> = BTreeMap::new();
    for e in es {
        *c.entry(s(e, "outcome")).or_default() += 1;
    }
    let mut v: Vec<(&str, usize)> = c.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.iter().map(|(o, n)| format!("{n} {o}")).collect::<Vec<_>>().join(" · ")
}

/// The time columns of a group of changes: how many, median active, median active per line, acted on.
fn times(es: &[&Value]) -> Vec<String> {
    let active: Vec<u64> = es.iter().map(|e| n(e, "active_ms")).collect();
    let per_line: Vec<u64> = es.iter().map(|e| n(e, "active_ms") / lines(e).max(1)).collect();
    let acted = es.iter().filter(|e| acted(e)).count();
    vec![
        es.len().to_string(),
        median(active).map_or_else(|| "—".into(), secs),
        median(per_line).map_or_else(|| "—".into(), secs),
        format!("{acted} ({})", pct(acted, es.len())),
    ]
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
    let changes: Vec<&Value> = all.iter().filter(|e| s(e, "entry") == "change").collect();
    let reviews: Vec<&Value> = all.iter().filter(|e| s(e, "entry") == "review").collect();
    let mut out = format!("# Reviews\n\nFrom {from}: {} changes in {} reviews", changes.len(), reviews.len());
    if let (Some(first), Some(last)) = (all.first(), all.last()) {
        out += &format!(", {} – {}", s(first, "at"), s(last, "at"));
    }
    if broken > 0 {
        out += &format!(" ({broken} line{} not an entry, skipped)", if broken == 1 { "" } else { "s" });
    }
    out += ".\n";

    // Time per change, by grade and size
    out += "\n## Time per change\n\nOn screen with you there (a key, click, wheel or paste within 20 s). Acted on = \
            rejected, undone, edited or commented. An edit's review has no grade.\n\n";
    let mut rows: BTreeMap<(String, &str), Vec<&Value>> = BTreeMap::new();
    for e in &changes {
        let grade = match s(e, "grade") {
            "" => format!("({})", s(e, "kind")),
            g => g.to_string(),
        };
        rows.entry((grade, size(lines(e)))).or_default().push(e);
    }
    let t: Vec<Vec<String>> = rows
        .iter()
        .map(|((grade, size), es)| [vec![grade.clone(), size.to_string()], times(es)].concat())
        .collect();
    out += &table(&["Grade", "Lines", "Changes", "Median active", "Per line", "Acted on"], &t);

    // What changes ended as, by kind
    out += "\n## What changes ended as\n\n";
    let mut kinds: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for e in &changes {
        kinds.entry(s(e, "kind")).or_default().push(e);
    }
    let t: Vec<Vec<String>> =
        kinds.iter().map(|(k, es)| vec![k.to_string(), es.len().to_string(), outcomes(es)]).collect();
    out += &table(&["Kind", "Changes", "Outcomes"], &t);

    // Reviews: active minutes per 100 changed lines
    out += "\n## Reviews\n\n";
    let mut kinds: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for e in &reviews {
        kinds.entry(s(e, "kind")).or_default().push(e);
    }
    let t: Vec<Vec<String>> = kinds
        .iter()
        .map(|(k, es)| {
            let per100: Vec<u64> = es.iter().map(|e| n(e, "active_ms") * 100 / lines(e).max(1)).collect();
            let active: Vec<u64> = es.iter().map(|e| n(e, "active_ms")).collect();
            vec![
                k.to_string(),
                es.len().to_string(),
                median(active).map_or_else(|| "—".into(), secs),
                median(per100).map_or_else(|| "—".into(), |ms| format!("{:.1} min", ms as f64 / 60_000.0)),
            ]
        })
        .collect();
    out += &table(&["Kind", "Reviews", "Median active", "Per 100 lines"], &t);

    // Lens on and off
    out += "\n## Lens on and off\n\n";
    let mut arms: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for e in &changes {
        arms.entry(lens(e)).or_default().push(e);
    }
    let t: Vec<Vec<String>> = arms.iter().map(|(l, es)| [vec![l.clone()], times(es)].concat()).collect();
    out += &table(&["Lens", "Changes", "Median active", "Per line", "Acted on"], &t);

    // Findings: one line each (`finding`: by whom, sent, answered — the review panel adds its lens, skeptic
    // and outcome there)
    let findings: Vec<&Value> = all.iter().filter(|e| s(e, "entry") == "finding").collect();
    let flag = |k: &str| findings.iter().filter(|e| e["finding"][k] == true).count();
    let on = findings.iter().filter(|e| !s(e, "change").is_empty()).count();
    out += &format!(
        "\n## Findings: {} — {on} on a change followed, {} sent, {} answered\n",
        findings.len(),
        flag("sent"),
        flag("answered")
    );

    // High-risk rules
    let high: Vec<&Value> = changes.iter().copied().filter(|e| s(e, "grade") == "high").collect();
    out += &format!("\n## High-risk rules: {} changes\n", high.len());
    let mut rules: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for e in &high {
        rules.entry(s(e, "rule")).or_default().push(e);
    }
    if !rules.is_empty() {
        out += "\nLooked only = looked at and nothing else; a rule whose marks never led anywhere is a candidate to \
                narrow (pack data, by hand).\n\n";
        let t: Vec<Vec<String>> = rules
            .iter()
            .map(|(r, es)| {
                let only = es.iter().filter(|e| s(e, "outcome") == "looked" && e["finding"] != true).count();
                let acted = es.iter().filter(|e| acted(e)).count();
                let found = es.iter().filter(|e| e["finding"] == true).count();
                vec![
                    r.to_string(),
                    es.len().to_string(),
                    only.to_string(),
                    acted.to_string(),
                    found.to_string(),
                ]
            })
            .collect();
        out += &table(&["Rule", "Changes", "Looked only", "Acted on", "Reviewer finding"], &t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's record, summed up: time per change by grade and size (an edit review's by its kind), the
    /// share acted on, outcomes by kind, the reviews' minutes per 100 lines, the lens arms, each high-risk rule's
    /// marks — and the broken line skipped, said.
    #[test]
    fn the_record_reads_as_the_lens_tables() {
        let r = report("reviews.jsonl", include_str!("../fixtures/reviews.jsonl"));
        let has = |s: &str| assert!(r.contains(s), "{s:?} not in:\n{r}");
        has(
            "From reviews.jsonl: 6 changes in 2 reviews, 2026-10-20 10:00:00 – 2026-10-20 11:00:00 (1 line not an entry, skipped).",
        );
        has("| (edit)    | 1–5   | 2       | 6.0 s         | 3.0 s    | 1 (50 %) |");
        has("| behaviour | 6–30  | 1       | 30.0 s        | 3.0 s    | 0 (0 %)  |");
        has("| high      | 1–5   | 2       | 12.0 s        | 6.0 s    | 1 (50 %) |");
        has("| edit | 2       | 1 accepted · 1 rejected                 |");
        has("| turn | 4       | 1 left · 1 looked · 1 undone · 1 viewed |");
        has("| turn | 1       | 50.0 s        | 5.6 min       |");
        has("| off  | 6       | 8.0 s         | 3.0 s    | 2 (33 %) |");
        has("## Findings: 2 — 1 on a change followed, 1 sent, 0 answered");
        has("## High-risk rules: 2 changes");
        has("| rust pack #0 | 2       | 1           | 1        | 0                |");
    }
}
