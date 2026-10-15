//! `cargo xtask cards [PATH …]` — the card record (`src/cards.rs`: `$state/cards/<yyyy-mm>.jsonl`, every
//! file in a folder given, the state folder's when none is) summed up as cardauto's §4.2 reads it:
//!
//! - **the P3 table** — Claude's cards by kind · `why` (its program after `:` folded into the row) × `shadow`:
//!   how many, their answers, how many stamped (answered within 1.5 s of being drawn), the median drawn →
//!   answered;
//! - **removed by P3** — Claude's cards with `shadow=none` of all of Claude's (`why` ≠ `own`, `spend`);
//! - **P3 wrong yes** — `shadow=none` ∧ `n`, each with its command, to be read by hand;
//! - **the stamp rate** by `why`; dropped and never-drawn cards are left out of stamps and times;
//! - **refused with no card**, each with its command and what Claude was told;
//! - **tarae's own cards**, by kind and answer.
//!
//! A line that isn't an entry (a write cut short) is counted and skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Result;

/// Answered within this long of being drawn: a stamp, not a read (cards.md §1.3).
const STAMP_MS: u64 = 1500;

pub fn run(args: &[String]) -> Result<()> {
    let paths: Vec<PathBuf> = match args.is_empty() {
        true => {
            vec![state_cards().ok_or("neither XDG_STATE_HOME nor HOME is set: give the record's folder")?]
        }
        false => args.iter().map(PathBuf::from).collect(),
    };
    let mut files = Vec::new();
    for p in &paths {
        files.extend(jsonl_in(p)?);
    }
    if files.is_empty() {
        return Err(format!("no card record in {}", paths[0].display()));
    }
    let mut text = String::new();
    for f in &files {
        text += &std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    print!("{}", report(&names.join(", "), &text));
    Ok(())
}

/// `$XDG_STATE_HOME/tarae/cards` (`~/.local/state/tarae/cards`) — as `config::state_dir` finds it.
fn state_cards() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("tarae").join("cards"))
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

/// A field as text ("" when absent).
fn s<'a>(e: &'a Value, k: &str) -> &'a str {
    e[k].as_str().unwrap_or("")
}

/// Its `why` without the program after `:` (`not-a-check:awk` → `not-a-check`), and that program.
fn why_of(e: &Value) -> (&str, Option<&str>) {
    match s(e, "why").split_once(':') {
        Some((w, p)) => (w, Some(p)),
        None => (s(e, "why"), None),
    }
}

fn claudes(e: &Value) -> bool {
    !matches!(why_of(e).0, "own" | "spend") && s(e, "answer") != "refused"
}

/// Answered by a key, after being drawn: what stamps and times count.
fn timed(e: &Value) -> Option<u64> {
    e["drawn_ms"].as_u64().filter(|_| s(e, "answer") != "dropped")
}

fn stamped(e: &Value) -> bool {
    timed(e).is_some_and(|ms| ms <= STAMP_MS)
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied().filter(|_| !v.is_empty())
}

fn secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn pct(n: usize, of: usize) -> String {
    (n * 100 + of / 2).checked_div(of).map_or_else(|| "—".into(), |p| format!("{p} %"))
}

/// `24 y · 1 n` — how often each answer, most first.
fn answers(es: &[&Value]) -> String {
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for e in es {
        *n.entry(s(e, "answer")).or_default() += 1;
    }
    let mut v: Vec<(&str, usize)> = n.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.iter().map(|(a, n)| format!("{n} {a}")).collect::<Vec<_>>().join(" · ")
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

/// One entry, for a list read by hand: when, the session, the card and run, the command and where it ran.
fn listed(e: &Value) -> String {
    let card = e["card"].as_u64().map(|c| format!(" card#{c}")).unwrap_or_default();
    let run = e["run"].as_u64().map(|r| format!(" run#{r}")).unwrap_or_default();
    let cwd = match s(e, "cwd") {
        "" => String::new(),
        c => format!(" (in {c})"),
    };
    format!("- {} {}{card}{run} · `{}`{cwd}", s(e, "at"), s(e, "session"), s(e, "what"))
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
    let mut out = format!("# Cards\n\nFrom {from}: {} entries", all.len());
    if let (Some(first), Some(last)) = (all.first(), all.last()) {
        out += &format!(", {} – {}", s(first, "at"), s(last, "at"));
    }
    if broken > 0 {
        out += &format!(" ({broken} line{} not an entry, skipped)", if broken == 1 { "" } else { "s" });
    }
    out += ".\n";
    let claude: Vec<&Value> = all.iter().filter(|e| claudes(e)).collect();

    // The P3 table
    out += &format!(
        "\n## The P3 table (`why` × `shadow` × answer × stamp)\n\nClaude's cards. Stamped = answered within {} of \
         being drawn; times are drawn → answered (dropped and never-drawn cards left out of both).\n\n",
        secs(STAMP_MS)
    );
    let mut rows: BTreeMap<(String, String), Vec<&Value>> = BTreeMap::new();
    for e in &claude {
        let class = e["class"].as_array().map(|c| {
            let c: Vec<&str> = c.iter().filter_map(Value::as_str).collect();
            format!(" (`class={}`)", c.join(","))
        });
        let key = format!("{} · `{}`{}", s(e, "kind"), why_of(e).0, class.unwrap_or_default());
        rows.entry((key, s(e, "shadow").to_string())).or_default().push(e);
    }
    let mut t: Vec<Vec<String>> = rows
        .iter()
        .map(|((key, shadow), es)| {
            let mut programs: Vec<&str> = es.iter().filter_map(|e| why_of(e).1).collect();
            programs.sort_unstable();
            programs.dedup();
            let key = match programs.is_empty() {
                true => key.clone(),
                false => format!(
                    "{key} ({})",
                    programs.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
                ),
            };
            let times: Vec<u64> = es.iter().filter_map(|e| timed(e)).collect();
            vec![
                key,
                shadow.clone(),
                es.len().to_string(),
                answers(es),
                es.iter().filter(|e| stamped(e)).count().to_string(),
                median(times).map_or_else(|| "—".into(), secs),
            ]
        })
        .collect();
    t.push(vec![
        "**Total**".into(),
        String::new(),
        format!("**{}**", claude.len()),
        answers(&claude),
        format!("**{}**", claude.iter().filter(|e| stamped(e)).count()),
        String::new(),
    ]);
    out += &table(&["Kind · `why`", "`shadow`", "Cards", "Answers", "Stamped", "Median time"], &t);

    // The rates
    let none: Vec<&Value> = claude.iter().copied().filter(|e| s(e, "shadow") == "none").collect();
    out += &format!(
        "\n- **Removed by P3** (`shadow=none`): {} of {} ({}).\n",
        none.len(),
        claude.len(),
        pct(none.len(), claude.len())
    );
    let wrong: Vec<&Value> = none.iter().copied().filter(|e| s(e, "answer") == "n").collect();
    out += &format!("- **P3 wrong yes** (`shadow=none` ∧ `n`): {}", wrong.len());
    out += match wrong.is_empty() {
        true => ".\n",
        false => {
            " — read each by hand: did the person only not want it now, or could it hurt inside the \
                  capabilities?\n"
        }
    };
    for e in &wrong {
        out += &format!("  {}\n", listed(e));
    }
    let mut by_why: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for e in claude.iter().filter(|e| timed(e).is_some()) {
        let n = by_why.entry(why_of(e).0).or_default();
        n.0 += usize::from(stamped(e));
        n.1 += 1;
    }
    let rates: Vec<String> =
        by_why.iter().map(|(w, (st, of))| format!("`{w}` {st} of {of} ({})", pct(*st, *of))).collect();
    out += &format!(
        "- **Stamp rate** by `why`: {}.\n",
        if rates.is_empty() { "—".into() } else { rates.join(" · ") }
    );

    // Refused with no card
    let refused: Vec<&Value> = all.iter().filter(|e| s(e, "answer") == "refused").collect();
    out += &format!("\n## Refused with no card: {}\n", refused.len());
    if !refused.is_empty() {
        out += "\n";
    }
    for e in &refused {
        out += &format!("{} — {}\n", listed(e), s(e, "said"));
    }

    // tarae's own
    let own: Vec<&Value> = all.iter().filter(|e| !claudes(e) && s(e, "answer") != "refused").collect();
    out += &format!("\n## tarae's own cards: {}\n", own.len());
    let mut kinds: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for e in &own {
        kinds.entry(s(e, "kind")).or_default().push(e);
    }
    if !kinds.is_empty() {
        out += "\n";
        let t: Vec<Vec<String>> = kinds
            .iter()
            .map(|(k, es)| {
                let times: Vec<u64> = es.iter().filter_map(|e| timed(e)).collect();
                vec![
                    k.to_string(),
                    es.len().to_string(),
                    answers(es),
                    median(times).map_or_else(|| "—".into(), secs),
                ]
            })
            .collect();
        out += &table(&["Kind", "Cards", "Answers", "Median time"], &t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's record, summed up: the P3 table's rows by kind · why × shadow (programs folded in, a
    /// dropped card counted but neither stamped nor timed), the removed share, the one wrong yes with its
    /// command, the stamp rate, the refusal with what Claude was told, tarae's own apart — and the broken
    /// line skipped, said.
    #[test]
    fn the_record_reads_as_the_p3_table() {
        let r = report("cards.jsonl", include_str!("../fixtures/cards.jsonl"));
        let has = |s: &str| assert!(r.contains(s), "{s:?} not in:\n{r}");
        has(
            "From cards.jsonl: 9 entries, 2026-10-15 09:01:02 – 2026-10-15 10:10:00 (1 line not an entry, skipped).",
        );
        has(
            "| read · `read` (`class=home`)               | grant:read | 1     | 1 n                   | 0       | 394.3 s     |",
        );
        has(
            "| run · `not-a-check` (`awk`, `mkdir`, `rm`) | none       | 3     | 1 dropped · 1 n · 1 y | 1       | 6.0 s       |",
        );
        has(
            "| run · `reads-outside` (`class=home`)       | grant:read | 1     | 1 y                   | 0       | 1.7 s       |",
        );
        has(
            "| run · `unreadable` (`$`)                   | none       | 1     | 1 y                   | 0       | 11.7 s      |",
        );
        has(
            "| **Total**                                  |            | **6** | 3 y · 2 n · 1 dropped | **1**   |             |",
        );
        has("- **Removed by P3** (`shadow=none`): 4 of 6 (67 %).");
        has("- **P3 wrong yes** (`shadow=none` ∧ `n`): 1 — read each by hand");
        has("  - 2026-10-15 09:07:00 cardauto card#5 run#13 · `rm -rf target/sb`\n");
        has(
            "- **Stamp rate** by `why`: `not-a-check` 1 of 2 (50 %) · `read` 0 of 1 (0 %) · `reads-outside` 0 of 1 (0 %) · `unreadable` 0 of 1 (0 %).",
        );
        has(
            "## Refused with no card: 1\n\n- 2026-10-15 10:10:00 cardauto run#31 · `sh -n x.sh` — x.sh was written",
        );
        has("## tarae's own cards: 2");
        has("| rebased     | 1     | 1 p     | 1.2 s       |");
    }
}
