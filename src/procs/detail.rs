//! `:procs`' detail (panes P3 step 4, plan §2.2): what the table's selected row is — what it's for, its command
//! line whole, the folder it works in, the processes it's under up to tarae and the ones it started, how long it has
//! run, its CPU and memory now and the most since `:procs` opened, tarae's checks on it; one gone since the selection
//! came to it is said gone. A read-only document beside the table (`rundoc.rs`'s, `Run::Procs` — the run panels'
//! keys: search, select, `y`, `enter` quotes), made from the rows the sampling thread brings: nothing is read here.

use std::time::{Duration, SystemTime};

use unicode_width::UnicodeWidthStr;

use super::{Check, Kind, Row, age, mem};
use crate::editor::Editor;

/// How a line's text reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// What says less: an ended check, a word where nothing could be read.
    Dim,
    /// What needs a look: gone, a check that holds, a CPU or memory a check found.
    Warn,
}

/// A line of the detail: its label (on a wrapped line's rest, none), its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub label: &'static str,
    pub text: String,
    pub tone: Tone,
}

/// The labels' column: the longest (`command`, `started`) and two cells.
pub const LABEL_W: usize = 9;

/// What row `r` of `rows` is, a line each, the long ones wrapped to `width` (the label's column kept): `checks` are
/// tarae's (those about it said), `peak` the most memory it took since `:procs` opened.
pub fn lines(r: &Row, rows: &[Row], checks: &[Check], peak: Option<u64>, width: usize) -> Vec<Line> {
    let room = width.saturating_sub(LABEL_W).max(16);
    let mut out = Vec::new();
    let mut put = |label: &'static str, text: String, tone: Tone, wrap: bool| {
        let parts = if wrap { wrapped(&text, room) } else { vec![text] };
        for (i, part) in parts.into_iter().enumerate() {
            out.push(Line { label: if i == 0 { label } else { "" }, text: part, tone });
        }
    };
    if let Some(at) = r.gone {
        put("gone", format!("it has ended — not seen since {}", crate::log::clock(at)), Tone::Warn, true);
    }
    put("for", for_words(r), Tone::Plain, true);
    let process = r.pid != 0;
    if process {
        match &r.command {
            Some(c) => put("command", c.clone(), Tone::Plain, true),
            None => put("command", "not read — it isn't ours to read, or it ended".into(), Tone::Dim, false),
        }
        if let Some(dir) = &r.cwd {
            put("folder", crate::allow::tilde(dir), Tone::Plain, true);
        }
        if r.kind != Kind::Tarae {
            let chain = chain(r, rows);
            match chain.is_empty() {
                true => put("under", format!("pid {} — not one of tarae's", r.ppid), Tone::Dim, false),
                false => put("under", chain.join(" › "), Tone::Plain, true),
            }
        }
        let kids: Vec<String> = live(rows).filter(|c| c.ppid == r.pid).map(named).collect();
        if !kids.is_empty() {
            put("started", kids.join(", "), Tone::Plain, true);
        }
    } else if let Kind::Session(id) = r.kind {
        let n = live(rows).filter(|c| c.session == Some(id) && c.pid != 0).count();
        put("under it", format!("{n} process{}", if n == 1 { "" } else { "es" }), Tone::Plain, false);
    }
    let up = match (&r.kind, r.up) {
        (Kind::Held(..), _) => Some("held — not running".to_string()),
        (Kind::Queued(_), _) => Some("waiting in line — no process yet".to_string()),
        (_, Some(d)) => {
            let since = SystemTime::UNIX_EPOCH + Duration::from_secs(r.started);
            Some(format!("{} · since {}", age(d), crate::log::clock(since)))
        }
        _ => None,
    };
    if let Some(up) = up {
        put("up", up, Tone::Plain, false);
    }
    if process || matches!(r.kind, Kind::Session(_)) {
        let cpu = match (r.cpu, r.gone) {
            (_, Some(_)) => "—".to_string(),
            (Some(c), _) => format!("{c:.1}% of a core now"),
            (None, _) => "— until the next look".to_string(),
        };
        let worked = (process && r.worked > Duration::ZERO).then(|| format!(" · {} in all", busy(r.worked)));
        let hot = r.cpu.is_some_and(|c| c >= 80.0) && r.gone.is_none();
        put("cpu", cpu + &worked.unwrap_or_default(), if hot { Tone::Warn } else { Tone::Plain }, false);
        let mut memory = match r.footprint {
            Some(f) => format!("{} footprint · {} resident", mem(f), mem(r.rss)),
            None => format!("{} resident", mem(r.rss)),
        };
        if let Some(p) = peak.filter(|&p| p > r.memory()) {
            memory.push_str(&format!(" · {} the most since opened", mem(p)));
        }
        put("memory", memory, if r.flagged { Tone::Warn } else { Tone::Plain }, true);
    }
    // tarae's checks on it, newest first: what holds, then what ended
    for c in checks.iter().rev().filter(|c| process && c.pid == r.pid) {
        match c.ended {
            None => put("check", format!("▲ {} — {}", c.what, c.did), Tone::Warn, true),
            Some(e) => put("check", format!("{} — ended {}", c.what, crate::log::clock(e)), Tone::Dim, true),
        }
    }
    out
}

/// What it's for, in words: what it is to tarae, then what for (a root, a command, a file).
fn for_words(r: &Row) -> String {
    let is = match &r.kind {
        Kind::Tarae => return "this editor".into(),
        Kind::Lsp(_) => "a language server".into(),
        Kind::Held(..) => "a language server, held — it exited again and again; r starts it".into(),
        Kind::Debug => "the debugger".into(),
        Kind::Helper => "what a debug attach started first".into(),
        Kind::Task => "a task".into(),
        Kind::Test => "tests".into(),
        Kind::Terminal => "the terminal".into(),
        Kind::Chat => "Claude's chat".into(),
        Kind::Ask => "space i's Claude, answering".into(),
        Kind::Reviewer => "the session's reviewer".into(),
        Kind::Spare => "a Claude kept started for space i".into(),
        Kind::Plugin(p, _) => format!("plugin {p}'s program"),
        Kind::Other if r.left => "left by a program tarae ran".into(),
        Kind::Other if r.depth > 0 => "started by what it's under".into(),
        Kind::Other => "not one tarae knows".into(),
        Kind::Session(_) => "a session — what's under it, summed".into(),
        Kind::Queued(_) => "a run waiting in line — s takes it out".into(),
    };
    match r.what.is_empty() {
        true => is,
        false => format!("{is} · {}", r.what),
    }
}

/// The rows of what runs: no heading, no held server, no gone one.
fn live(rows: &[Row]) -> impl Iterator<Item = &Row> {
    rows.iter().filter(|r| r.pid != 0 && r.gone.is_none())
}

/// A process as the detail names it: `zsh 41262`.
fn named(r: &Row) -> String {
    format!("{} {}", r.title, r.pid)
}

/// The processes `r` is under, tarae first, its parent last (`tarae 41231 › terminal 41250`) — by the parents the
/// rows hold; empty when its parent is none of them.
fn chain(r: &Row, rows: &[Row]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = r.ppid;
    // (each step one row up; never more than there are rows — a pid reused as its own ancestor can't loop it)
    for _ in 0..rows.len() {
        let Some(p) = live(rows).find(|p| p.pid == at) else { break };
        out.push(named(p));
        if p.kind == Kind::Tarae {
            break;
        }
        at = p.ppid;
    }
    out.reverse();
    out
}

/// CPU time: `run_duration`'s (`400ms`, `12.0s`, `3m 05s`).
fn busy(d: Duration) -> String {
    crate::term::run_duration(d)
}

/// `text` in lines of `room` cells at most, broken between words (a word wider than that, cut).
fn wrapped(text: &str, room: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        loop {
            let sep = usize::from(!line.is_empty());
            if line.width() + sep + word.width() <= room {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                continue;
            }
            // (alone and too wide: its first `room` cells, the rest on — a path broken after a `/` that fits)
            let (mut head, mut rest) = split_at_width(&word, room);
            if let Some(at) = head.rfind('/').filter(|&at| at > 0 && at + 1 < head.len()) {
                rest = format!("{}{rest}", &head[at + 1..]);
                head.truncate(at + 1);
            }
            out.push(head);
            word = rest;
        }
    }
    out.push(line);
    out
}

/// `s` cut after `w` cells (a character never split), and the rest.
fn split_at_width(s: &str, w: usize) -> (String, String) {
    let mut used = 0;
    for (i, g) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(s, true) {
        let gw = g.width();
        if used + gw > w && i > 0 {
            return (s[..i].to_string(), s[i..].to_string());
        }
        used += gw;
    }
    (s.to_string(), String::new())
}

/// The detail's text and colours (`Document.paint`: byte ranges, global capture ids): each line its label, padded to
/// `LABEL_W`, faint, then its text in its tone.
pub fn text(lines: &[Line]) -> Painted {
    use crate::syntax::capture_id;
    let (mut text, mut paint) = (String::new(), Vec::new());
    for l in lines {
        let at = text.len();
        text.push_str(&format!("{:<LABEL_W$}", l.label));
        if !l.label.is_empty() {
            paint.push((at, at + l.label.len(), capture_id("ui.linenr")));
        }
        let from = text.len();
        text.push_str(&l.text);
        match l.tone {
            Tone::Plain => {}
            Tone::Dim => paint.push((from, text.len(), capture_id("ui.virtual"))),
            Tone::Warn => paint.push((from, text.len(), capture_id("warning"))),
        }
        text.push('\n');
    }
    (text, paint)
}

/// Which row the detail is of: its process (pid, start), or for one with none its title.
pub type Of = (u32, u64, String);

/// A document's text and its colours (`Document.paint`).
type Painted = (String, Vec<(usize, usize, u32)>);

/// The detail as `:procs` is now, `width` cells wide: which row it's of, its text and colours. None while the panel is
/// closed.
pub fn of(ed: &Editor, width: usize) -> Option<(Of, Painted)> {
    let p = ed.procs.panel.as_ref()?;
    let Some(r) = ed.procs.rows.get(p.selected) else {
        // (before the first look: as the table says)
        let text = "looking…\n".to_string();
        let paint = vec![(0, text.len() - 1, crate::syntax::capture_id("ui.linenr"))];
        return Some(((0, 0, String::new()), (text, paint)));
    };
    let peak = p.peak.get(&r.key()).copied();
    let lines = lines(r, &ed.procs.rows, &ed.procs.checks, peak, width);
    Some(((r.pid, r.started, r.title.clone()), text(&lines)))
}

/// The row the detail is of, as a header names it: `zsh 41262`, a session's or a held server's title.
pub fn title(ed: &Editor) -> Option<String> {
    let p = ed.procs.panel.as_ref()?;
    let r = ed.procs.rows.get(p.selected)?;
    Some(if r.pid == 0 { r.title.clone() } else { named(r) })
}

#[cfg(test)]
mod tests;
