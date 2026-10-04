//! `cargo xtask stream [--model M] [prompt]` — how Claude's streamed answer arrives, measured: one real answer
//! (`claude -p` with the chat's stream-json flags and its model and effort — `llm.model`/`llm.effort` from
//! tarae's config, the user's then this project's; `--model` wins; it costs usage), every stdout line
//! timestamped as it's read, kept in `target/stream/last.tsv` (ms, line). `cargo xtask stream --file F` reads
//! a kept one again instead. The report names the model that answered (claude's `init` line).
//!
//! Says: the text deltas' sizes and the gaps between them (bursts = runs closer than `BURST_GAP`), and what
//! tarae's reader (`llm::pump`) makes of them — it batches text and sends it on when a delta comes at least
//! `FLUSH` after its last send; one that came sooner waits for the next delta (or a tool call, or the end),
//! however long that is. That wait is printed per delta, to tell bursts in claude's stream from ones tarae adds.

use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::Result;

/// tarae's text batching (`llm::pump`).
const FLUSH: f64 = 25.0;
/// Deltas closer than this are one burst.
const BURST_GAP: f64 = 100.0;

const PROMPT: &str = "Explain in about 300 words, in plain prose with no lists, how a rope data structure \
                      keeps edits in the middle of a large text cheap.";

pub fn run(args: &[String]) -> Result<()> {
    let lines = match args {
        [flag, file] if flag == "--file" => read(file)?,
        [flag, model, rest @ ..] if flag == "--model" => record(&rest.join(" "), Some(model.clone()))?,
        _ => record(&args.join(" "), None)?,
    };
    report(&lines);
    Ok(())
}

/// The chat's `llm.model` and `llm.effort` as tarae reads them: the user's config, then the project's
/// `.tarae.toml` and `.tarae.local.toml` over it ("" = the CLI's default).
fn chat_settings() -> (Option<String>, Option<String>) {
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    let files = home
        .map(|h| h.join("tarae/config.toml"))
        .into_iter()
        .chain([crate::root().join(".tarae.toml"), crate::root().join(".tarae.local.toml")]);
    let (mut model, mut effort) = (None, None);
    for file in files {
        let Some(table) = std::fs::read_to_string(&file).ok().and_then(|t| t.parse::<toml::Table>().ok())
        else {
            continue;
        };
        let llm = table.get("llm");
        let get = |k: &str| llm.and_then(|l| l.get(k)).and_then(|v| v.as_str()).map(str::to_string);
        model = get("model").or(model);
        effort = get("effort").or(effort);
    }
    let set = |v: Option<String>| v.filter(|v| !v.is_empty());
    (set(model), set(effort))
}

/// One answer, every line with the ms it was read at.
fn record(prompt: &str, model: Option<String>) -> Result<Vec<(f64, String)>> {
    let prompt = if prompt.trim().is_empty() { PROMPT } else { prompt };
    let (chat_model, effort) = chat_settings();
    let model = model.or(chat_model);
    eprintln!(
        "asking {} (effort {})",
        model.as_deref().unwrap_or("the CLI's default model"),
        effort.as_deref().unwrap_or("default")
    );
    let mut cmd = Command::new("claude");
    if let Some(m) = &model {
        cmd.args(["--model", m]);
    }
    if let Some(e) = &effort {
        cmd.args(["--effort", e]);
    }
    let mut child = cmd
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--no-session-persistence",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("claude: {e}"))?;
    let message = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}",
        prompt.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    writeln!(stdin, "{message}").map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let mut lines = Vec::new();
    for line in BufReader::new(child.stdout.take().ok_or("no stdout")?).lines().map_while(|l| l.ok()) {
        let at = t0.elapsed().as_secs_f64() * 1000.0;
        let done = line.contains("\"type\":\"result\"");
        lines.push((at, line));
        if done {
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let dir = crate::root().join("target/stream");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let kept: String = lines.iter().map(|(t, l)| format!("{t:.1}\t{l}\n")).collect();
    std::fs::write(dir.join("last.tsv"), kept).map_err(|e| e.to_string())?;
    eprintln!("kept in target/stream/last.tsv");
    Ok(lines)
}

fn read(file: &str) -> Result<Vec<(f64, String)>> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let (t, rest) = l.split_once('\t')?;
            Some((t.parse().ok()?, rest.to_string()))
        })
        .collect())
}

/// A text delta's text, decoded (the JSON string after `"text":` in a `text_delta` line).
fn delta_text(line: &str) -> Option<String> {
    if !line.contains("\"stream_event\"") || !line.contains("\"text_delta\"") {
        return None;
    }
    let at = line.find("\"text\":\"")? + 8;
    let mut out = String::new();
    let mut chars = line[at..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('?'));
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

fn pct(v: &[f64], p: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
}

fn report(lines: &[(f64, String)]) {
    // (arrived at, chars) per text delta; and every line that makes tarae's reader send what it holds
    let deltas: Vec<(f64, usize)> =
        lines.iter().filter_map(|(t, l)| delta_text(l).map(|d| (*t, d.chars().count()))).collect();
    if deltas.len() < 2 {
        return eprintln!("{} text deltas — nothing to measure", deltas.len());
    }
    let flushers: Vec<f64> = lines
        .iter()
        .filter(|(_, l)| {
            l.contains("\"type\":\"assistant\"") && l.contains("\"tool_use\"")
                || l.contains("\"type\":\"control_request\"")
                || l.contains("\"type\":\"result\"")
        })
        .map(|(t, _)| *t)
        .collect();
    let first = deltas[0].0;
    let span = deltas[deltas.len() - 1].0 - first;
    let gaps: Vec<f64> = deltas.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let sizes: Vec<f64> = deltas.iter().map(|d| d.1 as f64).collect();
    let chars: usize = deltas.iter().map(|d| d.1).sum();
    let bursts = 1 + gaps.iter().filter(|g| **g > BURST_GAP).count();
    // (who answered: the `model` of claude's `system init` line)
    let model = lines.iter().find(|(_, l)| l.contains("\"subtype\":\"init\"")).and_then(|(_, l)| {
        let at = l.find("\"model\":\"")? + 9;
        Some(l[at..].split('"').next()?.to_string())
    });
    println!("model: {}", model.as_deref().unwrap_or("unknown (no init line)"));
    println!(
        "claude's stream: {} text deltas, {chars} chars over {:.1} s ({:.0} chars/s)",
        deltas.len(),
        span / 1000.0,
        chars as f64 / (span / 1000.0)
    );
    println!(
        "  chars a delta  median {:.0}  p90 {:.0}  max {:.0}",
        pct(&sizes, 0.5),
        pct(&sizes, 0.9),
        pct(&sizes, 1.0)
    );
    println!(
        "  gap between    median {:.0} ms  p90 {:.0} ms  max {:.0} ms",
        pct(&gaps, 0.5),
        pct(&gaps, 0.9),
        pct(&gaps, 1.0)
    );
    println!(
        "  bursts (deltas < {BURST_GAP:.0} ms apart): {bursts}, gaps over 250 ms: {}",
        gaps.iter().filter(|g| **g > 250.0).count()
    );
    // tarae's reader: a delta is sent when it comes ≥ FLUSH after the last send — else it waits for the next
    // delta that is, or a tool call, a control request, the end
    let mut last_send = f64::NEG_INFINITY;
    let mut held: Vec<f64> = Vec::new();
    let mut waiting: Vec<f64> = Vec::new();
    for &(t, _) in &deltas {
        // (a tool call or the end in between sent what waited)
        if let Some(&f) = flushers.iter().find(|&&f| waiting.first().is_some_and(|&w| f >= w) && f <= t) {
            held.extend(waiting.drain(..).map(|w| f - w));
            last_send = f;
        }
        waiting.push(t);
        if t - last_send >= FLUSH {
            held.extend(waiting.drain(..).map(|w| t - w));
            last_send = t;
        }
    }
    if let Some(&end) = flushers.last() {
        held.extend(waiting.drain(..).map(|w| (end - w).max(0.0)));
    }
    println!("tarae's reader (sends on a delta ≥ {FLUSH:.0} ms after its last send):");
    println!(
        "  a delta waits   median {:.0} ms  p90 {:.0} ms  max {:.0} ms; over 100 ms: {} of {}",
        pct(&held, 0.5),
        pct(&held, 0.9),
        pct(&held, 1.0),
        held.iter().filter(|h| **h > 100.0).count(),
        held.len()
    );
    // The gaps' timeline, a char a 50 ms (· = a delta came, space = quiet), the first 6 s
    let mut row = String::new();
    for k in 0..120 {
        let (a, b) = (first + k as f64 * 50.0, first + (k + 1) as f64 * 50.0);
        row.push(if deltas.iter().any(|d| d.0 >= a && d.0 < b) { '·' } else { ' ' });
    }
    println!("  first 6 s, 50 ms a cell: |{}|", row.trim_end());
}
