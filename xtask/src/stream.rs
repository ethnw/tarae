//! `cargo xtask stream [--model M] [--edit] [prompt]` — how Claude's streamed answer arrives, measured: one real
//! answer (`claude -p` with the chat's stream-json flags and its model and effort — `llm.model`/`llm.effort` from
//! tarae's config, the user's then this project's; `--model` wins; it costs usage), every stdout line
//! timestamped as it's read, kept in `target/stream/last.tsv` (ms, line). `--edit` offers it the Write tool and
//! asks for a file, to measure an edit's input as it streams (`input_json_delta` — what `writing.rs` draws); the
//! write itself is refused (`-p` asks no one), nothing is written. `cargo xtask stream --file F` reads a kept one
//! again instead. The report names the model that answered (claude's `init` line).
//!
//! Says, for the text deltas and the input deltas: their sizes and the gaps between them (bursts = runs closer
//! than `BURST_GAP`), and what tarae's reader (`llm::pump`) makes of them — it batches deltas and sends them
//! `FLUSH` after its last send. Both rules are printed: the old one, where a delta that came sooner waited for the
//! next delta that didn't (or a tool call, the end) however long that was, and today's, where it goes when its
//! time is up — to tell bursts in claude's stream from ones tarae adds. Then what the main loop gets: the batches'
//! gaps, which `writing.rs`'s reveal paces itself by.

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

const EDIT_PROMPT: &str = "Write target/stream/probe.rs with the Write tool, in one call and no prose: about 80 \
                           lines of Rust, a small tokenizer for arithmetic expressions with a few unit tests.";

pub fn run(args: &[String]) -> Result<()> {
    let mut args = args.to_vec();
    let edit = args.iter().position(|a| a == "--edit").map(|k| args.remove(k)).is_some();
    let lines = match args.as_slice() {
        [flag, file] if flag == "--file" => read(file)?,
        [flag, model, rest @ ..] if flag == "--model" => record(&rest.join(" "), Some(model.clone()), edit)?,
        _ => record(&args.join(" "), None, edit)?,
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
fn record(prompt: &str, model: Option<String>, edit: bool) -> Result<Vec<(f64, String)>> {
    let prompt = match prompt.trim().is_empty() {
        true if edit => EDIT_PROMPT,
        true => PROMPT,
        false => prompt,
    };
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
            if edit { "Write" } else { "" },
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

/// What a line is to tarae's reader.
#[derive(Clone, Copy, PartialEq)]
enum Seen {
    /// A text delta of so many chars.
    Text(usize),
    /// A piece of a tool call's input (`input_json_delta`), so many chars of its JSON.
    Input(usize),
    /// What makes the reader send what it holds: a tool call's input ended, a whole tool call, a control request,
    /// the end.
    Flush,
    Other,
}

/// Every line, as the reader sees it.
fn seen(lines: &[(f64, String)]) -> Vec<(f64, Seen)> {
    // (the content blocks of the message under way that are tool calls: their end flushes)
    let mut tools: Vec<u64> = Vec::new();
    lines
        .iter()
        .map(|(t, l)| {
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(l) else { return (*t, Seen::Other) };
            let e = &ev["event"];
            let what = match (ev["type"].as_str(), e["type"].as_str()) {
                (Some("stream_event"), Some("message_start")) => {
                    tools.clear();
                    Seen::Other
                }
                (Some("stream_event"), Some("content_block_start")) => {
                    if e["content_block"]["type"] == "tool_use" {
                        tools.extend(e["index"].as_u64());
                    }
                    Seen::Other
                }
                (Some("stream_event"), Some("content_block_delta")) => {
                    let d = &e["delta"];
                    let chars = |k: &str| d[k].as_str().map_or(0, |s| s.chars().count());
                    match d["type"].as_str() {
                        Some("text_delta") => Seen::Text(chars("text")),
                        Some("input_json_delta") => Seen::Input(chars("partial_json")),
                        _ => Seen::Other,
                    }
                }
                (Some("stream_event"), Some("content_block_stop"))
                    if e["index"].as_u64().is_some_and(|i| tools.contains(&i)) =>
                {
                    Seen::Flush
                }
                (Some("assistant"), _) if l.contains("\"tool_use\"") => Seen::Flush,
                (Some("control_request" | "result"), _) => Seen::Flush,
                _ => Seen::Other,
            };
            (*t, what)
        })
        .collect()
}

/// What tarae's reader makes of a stream (`reader`).
struct Read {
    /// How long each delta waited, and what it was.
    waits: Vec<(f64, Seen)>,
    /// When each batch went out, and its chars.
    batches: Vec<(f64, usize)>,
}

/// How long each delta waits in tarae's reader, and when each batch goes out — by the old rule (`timer` false: a
/// delta that came within `FLUSH` of the last send waits for the next delta that doesn't, or a flush) or today's
/// (`timer`: it goes when its `FLUSH` is up).
fn reader(seen: &[(f64, Seen)], timer: bool) -> Read {
    let mut last_send = f64::NEG_INFINITY;
    let mut waiting: Vec<(f64, Seen)> = Vec::new();
    let (mut waits, mut batches) = (Vec::new(), Vec::new());
    let mut send = |at: f64, waiting: &mut Vec<(f64, Seen)>, last_send: &mut f64| {
        if waiting.is_empty() {
            return;
        }
        let chars =
            waiting.iter().map(|(_, s)| if let Seen::Text(n) | Seen::Input(n) = s { *n } else { 0 }).sum();
        batches.push((at, chars));
        waits.extend(waiting.drain(..).map(|(t, s)| (at - t, s)));
        *last_send = at;
    };
    for &(t, s) in seen {
        if timer && !waiting.is_empty() && last_send + FLUSH <= t {
            send(last_send + FLUSH, &mut waiting, &mut last_send);
        }
        match s {
            Seen::Text(_) | Seen::Input(_) => {
                waiting.push((t, s));
                if t - last_send >= FLUSH {
                    send(t, &mut waiting, &mut last_send);
                }
            }
            Seen::Flush => send(t, &mut waiting, &mut last_send),
            Seen::Other => {}
        }
    }
    if let Some(&(end, _)) = seen.last() {
        send(end, &mut waiting, &mut last_send);
    }
    Read { waits, batches }
}

fn pct(v: &[f64], p: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
}

fn report(lines: &[(f64, String)]) {
    // (who answered: the `model` of claude's `system init` line)
    let model = lines.iter().find(|(_, l)| l.contains("\"subtype\":\"init\"")).and_then(|(_, l)| {
        let at = l.find("\"model\":\"")? + 9;
        Some(l[at..].split('"').next()?.to_string())
    });
    println!("model: {}", model.as_deref().unwrap_or("unknown (no init line)"));
    let seen = seen(lines);
    let old = reader(&seen, false);
    let new = reader(&seen, true);
    for (name, input) in [("text", false), ("input", true)] {
        let deltas: Vec<(f64, usize)> = seen
            .iter()
            .filter_map(|&(t, s)| match s {
                Seen::Text(n) if !input => Some((t, n)),
                Seen::Input(n) if input => Some((t, n)),
                _ => None,
            })
            .collect();
        if deltas.len() < 2 {
            println!("{} {name} deltas — nothing to measure", deltas.len());
            continue;
        }
        let of = |waits: &[(f64, Seen)]| -> Vec<f64> {
            waits.iter().filter(|(_, s)| matches!(s, Seen::Input(_)) == input).map(|w| w.0).collect()
        };
        kind(name, &deltas, &of(&old.waits), &of(&new.waits));
    }
    let gaps: Vec<f64> = new.batches.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let sizes: Vec<f64> = new.batches.iter().map(|b| b.1 as f64).collect();
    println!(
        "batches to the main loop (today's rule): {}, gap median {:.0} ms  p90 {:.0} ms; chars a batch median {:.0}  p90 \
         {:.0}",
        new.batches.len(),
        pct(&gaps, 0.5),
        pct(&gaps, 0.9),
        pct(&sizes, 0.5),
        pct(&sizes, 0.9)
    );
}

/// One kind of delta: claude's stream, then what each rule of the reader makes it wait.
fn kind(name: &str, deltas: &[(f64, usize)], old: &[f64], new: &[f64]) {
    let first = deltas[0].0;
    let span = deltas[deltas.len() - 1].0 - first;
    let gaps: Vec<f64> = deltas.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let sizes: Vec<f64> = deltas.iter().map(|d| d.1 as f64).collect();
    let chars: usize = deltas.iter().map(|d| d.1).sum();
    let bursts = 1 + gaps.iter().filter(|g| **g > BURST_GAP).count();
    println!(
        "{name}: claude's stream: {} deltas, {chars} chars over {:.1} s ({:.0} chars/s)",
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
    // tarae's reader, both rules
    for (rule, held) in [("old, held for the next line", old), ("today's, sent when its time is up", new)] {
        println!(
            "  tarae's reader ({rule}): a delta waits median {:.0} ms  p90 {:.0} ms  max {:.0} ms; over 100 ms: {} \
             of {}",
            pct(held, 0.5),
            pct(held, 0.9),
            pct(held, 1.0),
            held.iter().filter(|h| **h > 100.0).count(),
            held.len()
        );
    }
    // The gaps' timeline, a char a 50 ms (· = a delta came, space = quiet), the first 6 s
    let mut row = String::new();
    for k in 0..120 {
        let (a, b) = (first + k as f64 * 50.0, first + (k + 1) as f64 * 50.0);
        row.push(if deltas.iter().any(|d| d.0 >= a && d.0 < b) { '·' } else { ' ' });
    }
    println!("  first 6 s, 50 ms a cell: |{}|", row.trim_end());
}
