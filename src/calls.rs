//! What a turn's tool calls cost, beside its tokens (`usage.rs` keeps it with the turn): how many a model
//! request, what each request carried, how long calls were open against the model's time and the user's
//! answers, the calls wasted, and the runs that only read text — what tarae's own tools could have done
//! without a process. All from what tarae already sees: claude's `tool_use` / `tool_result` blocks, the
//! permission answers tarae gives, the session's waiting state — nothing asked of claude.
//!
//! Wasted:
//! - a range read again — Read's offset/limit, a read-only run's `sed -n 'a,bp'`, `head`, `cat` — that an
//!   earlier read in this conversation covered with nothing written since: an edit of Claude's, a save, a
//!   run that isn't read-only, a compaction forget what was read (a file another program changes isn't
//!   seen — rare within a conversation's stretch between writes);
//! - an edit claude reported failed (`old_string` not found, not read first) — not one tarae or the user
//!   declined: that's `declined`, apart;
//! - a run of tarae's that didn't run (refused, a card's `n`) or that the sandbox blocked.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

/// The call-order record's folder in tarae's state (`$state/calls/2026-10.jsonl`, a file a month, as the card
/// record's): a line a turn — its requests in order with their calls' tool names, and its numbers
/// (`usage::Turn::entry`). Names and numbers only; read by `cargo xtask turns` (code mode's baseline,
/// `target/sb/codemode-plan.md`).
pub const DIR: &str = "calls";
/// The record's version, in each entry (`v`).
pub const RECORD_V: u64 = 1;

/// The lines Claude's Read reads when it's given no limit.
const READ_LINES: u64 = 2000;

/// claude's tools that change files.
const EDITS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// The numbers, summed over turns (`Totals` carries them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sums {
    pub calls: u64,
    /// The model requests of the turns counted, and what they carried (input + cache read + cache write):
    /// summed, and the largest one.
    pub requests: u64,
    pub context: u64,
    pub context_max: u64,
    /// Time (ms) with a call open · of it waiting for the user (a card, an edit to review) · the rest of the
    /// turn — the model's (only turns whose result said their time).
    pub tool_ms: u64,
    pub waiting_ms: u64,
    pub model_ms: u64,
    pub reread: u64,
    pub edit_failed: u64,
    pub declined: u64,
    pub run_refused: u64,
    /// Runs that only read (`readonly::read_only`).
    pub read_only: u64,
    /// Calls of tarae's tools that came while another of them was unanswered — claude ran them side by side
    /// (it does for tools marked `readOnlyHint`, else one after another).
    pub parallel: u64,
}

impl Sums {
    pub fn add(&mut self, o: Sums) {
        self.calls += o.calls;
        self.requests += o.requests;
        self.context += o.context;
        self.context_max = self.context_max.max(o.context_max);
        self.tool_ms += o.tool_ms;
        self.waiting_ms += o.waiting_ms;
        self.model_ms += o.model_ms;
        self.reread += o.reread;
        self.edit_failed += o.edit_failed;
        self.declined += o.declined;
        self.run_refused += o.run_refused;
        self.read_only += o.read_only;
        self.parallel += o.parallel;
    }

    /// Calls a request — None: no request.
    pub fn per_request(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.calls as f64 / self.requests as f64)
    }

    /// What a request carried, on average.
    pub fn context_mean(&self) -> Option<u64> {
        (self.requests > 0).then(|| self.context / self.requests)
    }

    /// As kept and as the state says it: the counts, and what they make (`per_request`, `context_mean`).
    pub fn json(&self) -> Value {
        json!({
            "calls": self.calls, "requests": self.requests,
            "per_request": self.per_request().map(|p| (p * 100.0).round() / 100.0),
            "context": self.context, "context_mean": self.context_mean(), "context_max": self.context_max,
            "ms": { "tools": self.tool_ms, "waiting": self.waiting_ms, "model": self.model_ms },
            "wasted": { "reread": self.reread, "edit_failed": self.edit_failed, "run_refused": self.run_refused },
            "declined": self.declined, "read_only_runs": self.read_only, "parallel": self.parallel,
        })
    }

    fn from_json(v: &Value) -> Sums {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        Sums {
            calls: n(&v["calls"]),
            requests: n(&v["requests"]),
            context: n(&v["context"]),
            context_max: n(&v["context_max"]),
            tool_ms: n(&v["ms"]["tools"]),
            waiting_ms: n(&v["ms"]["waiting"]),
            model_ms: n(&v["ms"]["model"]),
            reread: n(&v["wasted"]["reread"]),
            edit_failed: n(&v["wasted"]["edit_failed"]),
            run_refused: n(&v["wasted"]["run_refused"]),
            declined: n(&v["declined"]),
            read_only: n(&v["read_only_runs"]),
            parallel: n(&v["parallel"]),
        }
    }
}

/// One turn's calls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calls {
    pub sums: Sums,
    /// Calls by tool (tarae's without `mcp__tarae__`).
    pub tools: BTreeMap<String, u64>,
    /// Read-only runs by their first program (`grep`, `sed`).
    pub programs: BTreeMap<String, u64>,
}

impl Calls {
    pub fn json(&self) -> Value {
        let mut v = self.sums.json();
        v["tools"] = json!(self.tools);
        v["programs"] = json!(self.programs);
        v
    }

    /// None: a turn kept before tarae counted calls (or one of a process without tools).
    pub fn from_json(v: &Value) -> Option<Calls> {
        let map = |v: &Value| -> BTreeMap<String, u64> {
            v.as_object().into_iter().flatten().filter_map(|(k, n)| Some((k.clone(), n.as_u64()?))).collect()
        };
        v.is_object().then(|| Calls {
            sums: Sums::from_json(v),
            tools: map(&v["tools"]),
            programs: map(&v["programs"]),
        })
    }
}

/// The turn under way, counted as its calls come and go; what the conversation read, for reads again.
#[derive(Debug, Default)]
pub struct Tally {
    turn: Calls,
    /// Calls not answered yet: id → tool.
    open: BTreeMap<String, String>,
    /// Since when a call is open · since when the user is waited for (unix ms).
    busy_since: Option<u64>,
    waiting_since: Option<u64>,
    /// Permission asks (`can_use_tool`) not answered yet: request id → tool.
    asked: BTreeMap<String, String>,
    /// Asks tarae said no to whose failed result hasn't come yet, by tool (the result follows the answer).
    denied: BTreeMap<String, u64>,
    /// What the conversation read since anything was written, by file: line ranges, merged.
    reads: BTreeMap<PathBuf, Vec<(u64, u64)>>,
    /// The session's last write (`Session.wrote`) as the reads above knew it.
    wrote: Option<Instant>,
    /// Calls of tarae's tools come and not answered yet (`tools::mcp` → `tools::sized`).
    mcp_open: usize,
    /// The turn's model requests in order, each with its calls' tool names in order (`features::claude_tool`:
    /// names only, an unknown one `other`) — the call-order record's (`usage::Turn::entry`).
    steps: Vec<Vec<&'static str>>,
}

impl Tally {
    /// claude called `name` (a `tool_use` block) with `input`; `root` = where its relative paths start,
    /// `wrote` = the session's last save or write.
    pub fn called(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        root: &Path,
        wrote: Option<Instant>,
        now: u64,
    ) {
        let tool = name.strip_prefix("mcp__tarae__").unwrap_or(name).to_string();
        match self.steps.last_mut() {
            Some(step) => step.push(crate::features::claude_tool(name)),
            // (a call before its request's usage was seen: a request of its own)
            None => self.steps.push(vec![crate::features::claude_tool(name)]),
        }
        *self.turn.tools.entry(tool.clone()).or_default() += 1;
        self.turn.sums.calls += 1;
        if self.open.is_empty() {
            self.busy_since = Some(now);
        }
        self.open.insert(id.to_string(), tool.clone());
        if wrote != self.wrote {
            self.reads.clear();
            self.wrote = wrote;
        }
        let path = |p: &str| normal(&root.join(p));
        match tool.as_str() {
            "Read" => {
                let Some(file) = input["file_path"].as_str() else { return };
                let from = input["offset"].as_u64().unwrap_or(1).max(1);
                let lines = input["limit"].as_u64().unwrap_or(READ_LINES).max(1);
                self.read(path(file), (from, from + lines - 1));
            }
            t if EDITS.contains(&t) => {
                if let Some(file) = input["file_path"].as_str().or(input["notebook_path"].as_str()) {
                    self.reads.remove(&path(file));
                }
            }
            // tarae's `edit`: every file an item changes or moves (what's read of them since is new)
            "edit" => {
                for e in input["edits"].as_array().into_iter().flatten() {
                    for file in ["file_path", "move_to"].iter().filter_map(|k| e[*k].as_str()) {
                        self.reads.remove(&path(file));
                    }
                }
            }
            "run" => {
                let cmd = input["command"].as_str().unwrap_or_default();
                if !crate::readonly::read_only(cmd) {
                    self.reads.clear();
                    return;
                }
                self.turn.sums.read_only += 1;
                *self.turn.programs.entry(program(cmd)).or_default() += 1;
                let at = input["cwd"].as_str().map_or_else(|| root.to_path_buf(), |c| root.join(c));
                for (file, range) in ranges(cmd, &at) {
                    self.read(file, range);
                }
            }
            "run_tests" => self.reads.clear(),
            _ => {}
        }
    }

    /// Lines `range` of `file` read: again, when what was read since the last write holds them all.
    fn read(&mut self, file: PathBuf, (a, b): (u64, u64)) {
        let had = self.reads.entry(file).or_default();
        if had.iter().any(|&(x, y)| x <= a && b <= y) {
            self.turn.sums.reread += 1;
            return;
        }
        had.push((a, b));
        crate::sorting::sort(had);
        // (merged — touching ranges too: 1-40 and 41-80 hold 1-80)
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(had.len());
        for &(x, y) in had.iter() {
            match merged.last_mut() {
                Some(last) if x <= last.1.saturating_add(1) => last.1 = last.1.max(y),
                _ => merged.push((x, y)),
            }
        }
        *had = merged;
    }

    /// claude's result for call `id` (a `tool_result` block; `error`: `is_error`).
    pub fn answered(&mut self, id: &str, error: bool, now: u64) {
        let Some(tool) = self.open.remove(id) else { return };
        if self.open.is_empty()
            && let Some(since) = self.busy_since.take()
        {
            self.turn.sums.tool_ms += now.saturating_sub(since);
        }
        if !error {
            return;
        }
        let s = &mut self.turn.sums;
        match self.denied.get_mut(&tool).filter(|n| **n > 0) {
            Some(n) => {
                *n -= 1;
                s.declined += 1;
            }
            None if EDITS.contains(&tool.as_str()) => s.edit_failed += 1,
            None if matches!(tool.as_str(), "run" | "run_tests") => s.run_refused += 1,
            None => {}
        }
    }

    /// claude asks whether it may use `tool` (`can_use_tool`, request `request_id`).
    pub fn asked(&mut self, request_id: &str, tool: &str) {
        self.asked.insert(request_id.to_string(), tool.to_string());
    }

    /// tarae answered request `request_id` — `deny`: no (an edit declined, a read refused).
    pub fn answer(&mut self, request_id: &str, deny: bool) {
        if let Some(tool) = self.asked.remove(request_id)
            && deny
        {
            *self.denied.entry(tool).or_default() += 1;
        }
    }

    /// A call of tarae's tools came (its MCP request): side by side when another is unanswered.
    pub fn mcp_came(&mut self) {
        if self.mcp_open > 0 {
            self.turn.sums.parallel += 1;
        }
        self.mcp_open += 1;
    }

    /// One of tarae's tools answered its call.
    pub fn mcp_answered(&mut self) {
        self.mcp_open = self.mcp_open.saturating_sub(1);
    }

    /// A run of tarae's ran and the sandbox blocked something in it.
    pub fn blocked(&mut self) {
        self.turn.sums.run_refused += 1;
    }

    /// Whether the user is being waited for (a card of Claude's, an edit to review) while a turn is under way.
    pub fn waiting(&mut self, on: bool, now: u64) {
        match (on, self.waiting_since) {
            (true, None) => self.waiting_since = Some(now),
            (false, Some(since)) => {
                self.waiting_since = None;
                self.turn.sums.waiting_ms += now.saturating_sub(since);
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub fn is_waiting(&self) -> bool {
        self.waiting_since.is_some()
    }

    /// The conversation compacted: what it read is no longer in its context.
    pub fn forget(&mut self) {
        self.reads.clear();
    }

    /// A model request of the turn began (its first usage seen): the calls from now on are its.
    pub fn request(&mut self) {
        self.steps.push(Vec::new());
    }

    /// The turn's requests so far, each with its calls' tool names in order — taken (`end` drops them).
    pub fn take_steps(&mut self) -> Vec<Vec<&'static str>> {
        std::mem::take(&mut self.steps)
    }

    /// The turn ended at `now`: its calls, with its requests' contexts and its time (`ms`: as its result
    /// said). Calls still open end with it.
    pub fn end(&mut self, contexts: &[u64], ms: Option<u64>, now: u64) -> Calls {
        self.waiting(false, now);
        if let Some(since) = self.busy_since.take() {
            self.turn.sums.tool_ms += now.saturating_sub(since);
        }
        self.open.clear();
        self.asked.clear();
        self.denied.clear();
        self.mcp_open = 0;
        self.steps.clear();
        let mut c = std::mem::take(&mut self.turn);
        c.sums.requests = contexts.len() as u64;
        c.sums.context = contexts.iter().sum();
        c.sums.context_max = contexts.iter().copied().max().unwrap_or(0);
        c.sums.model_ms = ms.map_or(0, |ms| ms.saturating_sub(c.sums.tool_ms));
        c
    }
}

impl crate::editor::Editor {
    /// Hands the turns ended in the session in front to the call-order record (`DIR`), on a worker with
    /// nothing to bring back — none kept where there's no record (tests: none unless given).
    pub fn calls_record(&mut self) {
        let entries = std::mem::take(&mut self.session.usage.to_record);
        let Some(dir) = self.calls_dir.clone().filter(|_| !entries.is_empty()) else { return };
        let file = crate::cards::month_file(std::time::SystemTime::now());
        self.events.jobs().background(move || {
            if let Err(e) = crate::cards::append(&dir, &file, &entries) {
                crate::log::line("calls", "warn", &format!("{}: {e}", dir.join(&file).display()));
            }
        });
    }
}

/// The record's lines in `dir` once there are `n` whole ones (written on workers; 5 s at most), in no
/// particular order.
#[cfg(test)]
pub fn recorded(dir: &Path, n: usize) -> Vec<Value> {
    let file = dir.join(crate::cards::month_file(std::time::SystemTime::now()));
    let end = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let whole: Vec<&str> = text.split_inclusive('\n').filter(|l| l.ends_with('\n')).collect();
        if whole.len() >= n {
            return whole.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        }
        assert!(Instant::now() < end, "{} of {n} lines at {}: {text}", whole.len(), file.display());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A path with `.` and `..` taken out by its words — nothing looked up (the main thread reads no disk).
fn normal(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// A read-only run's first program, by its name (`grep` for `/usr/bin/grep`).
fn program(cmd: &str) -> String {
    let first = crate::readonly::flat(cmd).into_iter().flatten().find_map(|p| match p {
        crate::readonly::Part::Cmd(w) => w.into_iter().next(),
        crate::readonly::Part::Cd(_) => None,
    });
    let name = first.map(|w| w.text).unwrap_or_default();
    name.rsplit('/').next().unwrap_or_default().to_string()
}

/// The line ranges a read-only `cmd` run in `at` reads of files it names: `sed -n 'a,bp;cp' f`,
/// `head [-n N | -N] f`, `cat f` — what's piped into them reads no file.
fn ranges(cmd: &str, at: &Path) -> Vec<(PathBuf, (u64, u64))> {
    use crate::readonly::Part;
    let mut at = at.to_path_buf();
    let mut out = Vec::new();
    for part in crate::readonly::flat(cmd).unwrap_or_default() {
        let words = match part {
            Part::Cd(w) if !w.glob => {
                at = normal(&at.join(&w.text));
                continue;
            }
            // (a glob's folder isn't known without looking)
            Part::Cd(_) => return out,
            Part::Cmd(w) => w,
        };
        let words: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        let Some((first, args)) = words.split_first() else { continue };
        let read = match first.rsplit('/').next().unwrap_or_default() {
            "sed" => sed(args),
            "head" => head(args),
            "cat" => cat(args),
            _ => None,
        };
        for (file, lines) in read.into_iter().flatten() {
            out.push((normal(&at.join(file)), lines));
        }
    }
    out
}

/// `sed -n 'a,bp;cp' files…` — each file × each range; anything else sed may do: None.
fn sed<'a>(args: &[&'a str]) -> Option<Vec<(&'a str, (u64, u64))>> {
    let (opts, rest): (Vec<&str>, Vec<&str>) = args.iter().partition(|a| a.starts_with('-'));
    if opts != ["-n"] {
        return None;
    }
    let (script, files) = rest.split_first()?;
    let mut lines = Vec::new();
    for cmd in script.split(';').map(str::trim).filter(|c| !c.is_empty()) {
        let span = cmd.strip_suffix('p')?;
        let (a, b) = span.split_once(',').unwrap_or((span, span));
        let (a, b) = (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?);
        lines.push((a.max(1), b.max(a)));
    }
    Some(files.iter().flat_map(|f| lines.iter().map(move |&r| (*f, r))).collect())
}

/// `head [-n N | -nN | -N] files…` — the first N lines (10 if not said); any other option (bytes): None.
fn head<'a>(args: &[&'a str]) -> Option<Vec<(&'a str, (u64, u64))>> {
    let mut n = 10;
    let mut files = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "-n" => n = it.next()?.parse().ok()?,
            a if a.starts_with("-n") => n = a[2..].parse().ok()?,
            a if a.starts_with('-') => n = a[1..].parse().ok()?,
            a => files.push(a),
        }
    }
    Some(files.into_iter().map(|f| (f, (1, n.max(1)))).collect())
}

/// `cat [-n] files…` — each whole.
fn cat<'a>(args: &[&'a str]) -> Option<Vec<(&'a str, (u64, u64))>> {
    let (opts, files): (Vec<&str>, Vec<&str>) = args.iter().partition(|a| a.starts_with('-'));
    if opts.iter().any(|o| *o != "-n") {
        return None;
    }
    Some(files.into_iter().map(|f| (f, (1, u64::MAX))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(t: &mut Tally, id: &str, name: &str, input: Value, now: u64) {
        t.called(id, name, &input, Path::new("/p"), None, now);
    }

    /// Time with a call open is the union of the calls' spans (parallel calls aren't counted twice); the
    /// user waited for is part of it; the model's is the rest of the turn as its result said.
    #[test]
    fn a_turns_time_splits_into_model_tools_and_waiting() {
        let mut t = Tally::default();
        call(&mut t, "a", "Read", json!({ "file_path": "/p/a.rs" }), 1_000);
        call(&mut t, "b", "mcp__tarae__search", json!({ "pattern": "x" }), 1_100);
        t.answered("a", false, 1_300);
        t.answered("b", false, 1_500);
        call(&mut t, "c", "mcp__tarae__run", json!({ "command": "cargo test" }), 2_000);
        t.waiting(true, 2_100);
        t.waiting(false, 2_600);
        t.answered("c", false, 3_000);
        let c = t.end(&[100, 300], Some(5_000), 3_100);
        assert_eq!((c.sums.tool_ms, c.sums.waiting_ms, c.sums.model_ms), (500 + 1_000, 500, 3_500));
        assert_eq!((c.sums.calls, c.sums.requests), (3, 2));
        assert_eq!(c.sums.per_request(), Some(1.5));
        assert_eq!((c.sums.context_mean(), c.sums.context_max), (Some(200), 300));
        assert_eq!(
            c.tools.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(),
            [("Read", 1), ("run", 1), ("search", 1)]
        );
        // A call never answered ends with the turn; the next turn starts from nothing
        call(&mut t, "d", "Read", json!({ "file_path": "/p/b.rs" }), 10_000);
        assert_eq!(t.end(&[], None, 10_400).sums.tool_ms, 400);
        assert_eq!(t.end(&[], None, 11_000), Calls::default());
    }

    /// A range is read again when an earlier read since the last write holds it — Read's offset/limit and a
    /// read-only run's `sed -n`, `head`, `cat` alike, merged across reads; a write forgets.
    #[test]
    fn a_range_read_again_with_nothing_written_is_wasted() {
        let mut t = Tally::default();
        let read = |t: &mut Tally, id: &str, offset: u64, limit: u64| {
            call(t, id, "Read", json!({ "file_path": "/p/src/a.rs", "offset": offset, "limit": limit }), 0)
        };
        let run =
            |t: &mut Tally, id: &str, cmd: &str| call(t, id, "mcp__tarae__run", json!({ "command": cmd }), 0);
        read(&mut t, "1", 1, 40);
        run(&mut t, "2", "sed -n '41,80p;200,210p' src/a.rs");
        assert_eq!(t.turn.sums.reread, 0, "new lines");
        read(&mut t, "3", 10, 60);
        assert_eq!(t.turn.sums.reread, 1, "1-80 was read in two pieces");
        run(&mut t, "4", "cd src && head -n 5 a.rs | grep x; cat b.rs");
        assert_eq!(t.turn.sums.reread, 2, "head's 1-5 of src/a.rs, by its cd; b.rs is new");
        run(&mut t, "5", "grep -n x src/b.rs | head -3");
        assert_eq!(t.turn.sums.reread, 2, "grep isn't a range read, and head reads its pipe");
        assert_eq!(
            (t.turn.sums.read_only, t.turn.programs.get("sed"), t.turn.programs.get("cd")),
            (3, Some(&1), None)
        );
        assert_eq!(t.turn.programs.get("grep"), Some(&1), "a run's first program, past its cd");
        // Claude's edit of the file forgets that file; a run that may write forgets every file
        call(&mut t, "6", "Edit", json!({ "file_path": "/p/src/a.rs" }), 0);
        read(&mut t, "7", 1, 40);
        run(&mut t, "8", "cat src/b.rs");
        assert_eq!(t.turn.sums.reread, 3, "b.rs still read; a.rs read anew");
        run(&mut t, "9", "cargo fmt");
        read(&mut t, "10", 1, 40);
        assert_eq!(t.turn.sums.reread, 3);
        // tarae's edit forgets each file an item changes, and where one moves to — those only (paths from the
        // root, as its items give them)
        let read_at = |t: &mut Tally, id: &str, file: &str| {
            call(t, id, "Read", json!({ "file_path": file, "offset": 1, "limit": 40 }), 0)
        };
        read_at(&mut t, "e1", "/p/src/c.rs");
        read_at(&mut t, "e2", "/p/src/d.rs");
        let edit = json!({ "edits": [
            { "file_path": "src/c.rs", "old_string": "x", "new_string": "y" },
            { "file_path": "src/e.rs", "move_to": "src/d.rs" },
        ] });
        call(&mut t, "e3", "mcp__tarae__edit", edit, 0);
        read_at(&mut t, "e4", "/p/src/c.rs");
        read_at(&mut t, "e5", "/p/src/d.rs");
        assert_eq!(t.turn.sums.reread, 3, "c.rs changed and d.rs moved onto: read anew");
        read(&mut t, "e6", 1, 40);
        assert_eq!(t.turn.sums.reread, 4, "a.rs wasn't in the edit: still read");
        // A save (the session's write) forgets too
        t.called(
            "11",
            "Read",
            &json!({ "file_path": "/p/src/a.rs", "offset": 1, "limit": 40 }),
            Path::new("/p"),
            Some(Instant::now()),
            0,
        );
        assert_eq!(t.turn.sums.reread, 4);
        // A compaction does: the context no longer holds them
        t.forget();
        read(&mut t, "12", 1, 40);
        assert_eq!(t.turn.sums.reread, 4);
    }

    /// An edit's failure is claude's (counted wasted) unless tarae said no first — declined, apart; a run
    /// that didn't run, or that the sandbox blocked, is refused.
    #[test]
    fn failed_edits_are_told_from_declined_ones_and_refused_runs_counted() {
        let mut t = Tally::default();
        call(&mut t, "e1", "Edit", json!({ "file_path": "/p/a.rs" }), 0);
        t.asked("r1", "Edit");
        t.answer("r1", true);
        t.answered("e1", true, 0);
        call(&mut t, "e2", "Edit", json!({ "file_path": "/p/a.rs" }), 0);
        t.asked("r2", "Edit");
        t.answer("r2", false);
        t.answered("e2", true, 0);
        call(&mut t, "e3", "Read", json!({ "file_path": "/etc/hosts" }), 0);
        t.asked("r3", "Read");
        t.answer("r3", true);
        t.answered("e3", true, 0);
        // (an answer to another kind of request — tarae's own tools — isn't an ask)
        t.answer("mcp-1", true);
        call(&mut t, "u1", "mcp__tarae__run", json!({ "command": "rm -rf target/x" }), 0);
        t.answered("u1", true, 0);
        call(&mut t, "u2", "mcp__tarae__run", json!({ "command": "cargo build" }), 0);
        t.blocked();
        t.answered("u2", false, 0);
        let s = t.end(&[1], None, 0).sums;
        assert_eq!((s.edit_failed, s.declined, s.run_refused), (1, 2, 2));
    }

    /// Calls of tarae's tools side by side: one coming while another is unanswered counts; one after
    /// another doesn't.
    #[test]
    fn calls_side_by_side_are_counted() {
        let mut t = Tally::default();
        t.mcp_came();
        t.mcp_answered();
        t.mcp_came();
        t.mcp_came();
        t.mcp_came();
        t.mcp_answered();
        t.mcp_answered();
        t.mcp_answered();
        assert_eq!(t.end(&[], None, 0).sums.parallel, 2);
    }

    /// Kept with the conversation and read back the same; a turn kept before calls were counted has none.
    #[test]
    fn calls_are_kept_and_read_back() {
        let mut t = Tally::default();
        call(&mut t, "a", "mcp__tarae__run", json!({ "command": "grep -n x src/a.rs" }), 0);
        t.answered("a", false, 70);
        let c = t.end(&[1_000, 2_000], Some(900), 70);
        assert_eq!(Calls::from_json(&c.json()), Some(c));
        assert_eq!(Calls::from_json(&Value::Null), None);
    }

    #[test]
    fn what_sed_head_and_cat_read() {
        assert_eq!(sed(&["-n", "1,40p;100,140p", "a", "b"]).unwrap().len(), 4);
        assert_eq!(sed(&["-n", "7p", "a"]), Some(vec![("a", (7, 7))]));
        assert_eq!(sed(&["-n", "/fn x/,/^}/p", "a"]), None, "a pattern's range isn't lines");
        assert_eq!(sed(&["1,4d", "a"]), None, "without -n it prints everything");
        assert_eq!(head(&["-n", "30", "a"]), Some(vec![("a", (1, 30))]));
        assert_eq!(head(&["-30", "a"]), Some(vec![("a", (1, 30))]));
        assert_eq!(head(&["a"]), Some(vec![("a", (1, 10))]));
        assert_eq!(head(&["-c", "9", "a"]), None, "bytes aren't lines");
        assert_eq!(cat(&["-n", "a"]), Some(vec![("a", (1, u64::MAX))]));
        assert_eq!(normal(Path::new("/p/./src/../a.rs")), PathBuf::from("/p/a.rs"));
    }
}
