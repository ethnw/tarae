//! The `edit` tool (toolcalls, 2026-10-13): Claude's edits made by tarae — Claude Code's Edit fields
//! (`file_path`, `old_string`, `new_string`, `replace_all`), several files and replacements in one call, a new
//! file (an empty `old_string` where there's none), moves and deletes. Worked out on a worker against what the
//! user sees (an open buffer's text, unsaved edits and all; else the disk), every item first: one change set
//! that lands as `rename`'s and `replace`'s do (`refactor::land` — the card, the in-buffer review, or written
//! unseen under review at the end), or nothing, with one error naming each item that failed — the closest
//! lines there, or that `new_string` is there already (Aider's message).
//!
//! - Exact first (Claude's own format: string replacement is what Claude was trained on — Anthropic's text
//!   editor tool; a patch format is GPT's). Then two cheap tiers, each only where it gives the match the call
//!   asks for: trailing whitespace ignored, then curly quotes and dashes read as plain ones (Codex's last
//!   tiers). The answer, the card and the review say when one was used.
//! - No Read first: the match is the proof. A file changed since Claude
//!   last saw it through tarae (`read`, or its own change written — `Edits.seen`) is still changed when the
//!   text matches (Claude Code 2.1.208's rule), and the answer says it has other changes.
//! - A move or a delete is never written unseen (`refactor`: always on the card), and never done to a name
//!   Claude's runs can't write (`edits::protected`, where its path really leads), a file named like a key, a
//!   link, a folder, a file with the user's unsaved edits, or anything outside the project.
//! - Which edit tools the chat has is `llm.edit-tools` (`Arm`): `tarae` — only `edit` — since the A/B
//!   (tests/live.rs `chat_edit_ab`, 2026-10-13: as correct, 31 % fewer tokens, one review per change).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ropey::Rope;
use serde_json::{Value, json};

use crate::editor::Editor;
use crate::nav;
use crate::refactor::{FileChange, Op};
use crate::tools::{Call, answer};

/// Which edit tools the chat has (`llm.edit-tools`): only `edit` (`tarae`, the default), it and Claude Code's
/// Edit and Write (`both`), or those alone (`native`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    Native,
    Both,
    Tarae,
}

impl Arm {
    /// As the setting names it.
    pub fn parse(v: &str) -> Option<Arm> {
        match v.trim() {
            "tarae" => Some(Arm::Tarae),
            "both" => Some(Arm::Both),
            "native" => Some(Arm::Native),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Arm::Tarae => "tarae",
            Arm::Both => "both",
            Arm::Native => "native",
        }
    }

    /// The chat has tarae's `edit`.
    pub fn tarae(self) -> bool {
        self != Arm::Native
    }

    /// The chat has Claude Code's Edit and Write.
    pub fn native(self) -> bool {
        self != Arm::Tarae
    }
}

pub fn schema() -> Value {
    json!({
        "name": "edit",
        "description": "Change files in the editor. Each item replaces old_string with new_string in file_path — \
    an exact match, whitespace and indentation too, found once (or every match with replace_all). Several \
    items, files and replacements in one call, applied in order (each sees the text the earlier ones left). \
    An empty old_string where there's no file makes it with new_string. move_to moves a file, delete: true \
    deletes one. Every item is checked first: the call lands as one change the user reviews, or nothing \
    changes and the error lists each item that failed, with the closest lines in the file. No Read needed \
    first. Paths from the project root, inside it only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file_path": { "type": "string", "description": "The file, from the project root" },
                            "old_string": { "type": "string", "description": "The text to replace — empty: make the file (none is there)" },
                            "new_string": { "type": "string", "description": "What it becomes" },
                            "replace_all": { "type": "boolean", "description": "Every match of old_string (default false)" },
                            "move_to": { "type": "string", "description": "Move the file here instead (no old_string)" },
                            "delete": { "type": "boolean", "description": "Delete the file instead (no old_string)" }
                        },
                        "required": ["file_path"]
                    }
                }
            },
            "required": ["edits"]
        }
    })
}

/// Items one call takes at most.
pub(crate) const ITEMS_MAX: usize = 100;

/// Lines of `old_string` the closest-lines search compares at most.
const CLOSEST_LINES: usize = 30;

/// Lines a not-found error lists where an ambiguous match is (`lines 10, 40, …`).
const AMBIGUOUS_LINES: usize = 10;

/// One item of a call.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    Replace { file: String, old: String, new: String, all: bool },
    Move { file: String, to: String },
    Delete { file: String },
}

impl Item {
    fn file(&self) -> &str {
        match self {
            Item::Replace { file, .. } | Item::Move { file, .. } | Item::Delete { file } => file,
        }
    }
}

fn items_of(args: &Value) -> Result<Vec<Item>, String> {
    let list = args["edits"].as_array().ok_or(
        "edits is required: a list of {file_path, old_string, new_string} (or move_to, or delete: true)",
    )?;
    if list.is_empty() {
        return Err("edits is empty — nothing to change".into());
    }
    if list.len() > ITEMS_MAX {
        return Err(format!("{} items — {ITEMS_MAX} at most in one call", list.len()));
    }
    list.iter().enumerate().map(|(i, e)| item(i, e)).collect()
}

fn item(i: usize, e: &Value) -> Result<Item, String> {
    let s = |k: &str| e[k].as_str();
    let file = s("file_path")
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .ok_or_else(|| format!("edits[{i}]: file_path is required"))?
        .to_string();
    let delete = e["delete"].as_bool() == Some(true);
    let to = s("move_to").map(str::trim).filter(|t| !t.is_empty());
    let text = !e["old_string"].is_null() || !e["new_string"].is_null();
    match (delete, to, text) {
        (true, None, false) => Ok(Item::Delete { file }),
        (false, Some(to), false) => Ok(Item::Move { file, to: to.to_string() }),
        (false, None, true) => {
            let (Some(old), Some(new)) = (s("old_string"), s("new_string")) else {
                return Err(format!("edits[{i}] ({file}): old_string and new_string are both strings"));
            };
            if old == new {
                return Err(format!("edits[{i}] ({file}): old_string and new_string are the same"));
            }
            let all = e["replace_all"].as_bool() == Some(true);
            Ok(Item::Replace { file, old: old.to_string(), new: new.to_string(), all })
        }
        (false, None, false) => {
            Err(format!("edits[{i}] ({file}): give old_string and new_string, or move_to, or delete: true"))
        }
        _ => Err(format!(
            "edits[{i}] ({file}): one of old_string/new_string, move_to or delete: true — not more"
        )),
    }
}

/// What the worker needs from the main loop.
struct Known {
    root: PathBuf,
    /// Open buffers' text, by path (loaded ones).
    open: HashMap<PathBuf, Rope>,
    /// Buffers with the user's unsaved edits.
    dirty: Vec<PathBuf>,
    /// What Claude last saw of each file (`Edits.seen`).
    seen: HashMap<PathBuf, crate::disk::Stamp>,
    keys: crate::reads::Keys,
}

/// The tool: the items checked now (a malformed call is answered at once), worked out on a worker, then
/// landed — or the error.
pub fn call(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let items = items_of(args)?;
    let k = Known {
        root: nav::project_root(ed),
        open: ed
            .docs
            .iter()
            .filter(|d| !d.loading)
            .filter_map(|d| Some((d.path.clone()?, d.text.clone())))
            .collect(),
        dirty: ed
            .docs
            .iter()
            .filter(|d| d.is_modified() && !d.unsaved_is_claudes())
            .filter_map(|d| d.path.clone())
            .collect(),
        seen: ed.session.chat.as_ref().map(|c| c.edits.seen.clone()).unwrap_or_default(),
        keys: crate::reads::Keys::of(ed),
    };
    ed.events.jobs().spawn(move || {
        let r = resolve(&k, &items);
        move |ed: &mut Editor| match r {
            Ok(r) => {
                let what = match r.files.as_slice() {
                    [f] => match &f.op {
                        Op::Move { to_rel, .. } => format!("move {} → {to_rel}", f.rel),
                        Op::Delete => format!("delete {}", f.rel),
                        Op::Create => format!("make {}", f.rel),
                        Op::Text => format!("edit {}", f.rel),
                    },
                    fs => format!("edit {}", nav::count(fs.len(), "file")),
                };
                let mut note = r.told.clone();
                note.extend(r.stale);
                crate::refactor::land_told(
                    ed,
                    call,
                    what,
                    "change",
                    r.files,
                    note.join("\n"),
                    r.told.join("\n"),
                )
            }
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

/// What `call` saw of `path` once Claude's change there is written: its stamp (None: no claim).
pub fn saw(ed: &mut Editor, path: &Path, stamp: Option<crate::disk::Stamp>) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    match stamp {
        Some(s) => c.edits.seen.insert(path.to_path_buf(), s),
        None => c.edits.seen.remove(path),
    };
}

/// No claim of what Claude has seen of `path` (it was saved where its stamp isn't known).
pub fn forget(ed: &mut Editor, path: &Path) {
    saw(ed, path, None)
}

/// A call worked out: the change set, what the user is told of how it matched (`Landing.told`), and
/// what Claude is told of files changed since it last saw them.
#[derive(Debug)]
struct Resolved {
    files: Vec<FileChange>,
    told: Vec<String>,
    stale: Vec<String>,
}

/// One file as the call goes through its items.
#[derive(Debug)]
struct State {
    rel: String,
    /// When the call began — None: no file.
    before: Option<String>,
    /// After the items so far — None: no file.
    now: Option<String>,
    /// Moved here from there, in this call.
    from: Option<PathBuf>,
    /// Moved away or deleted, in this call (said when a later item names it).
    went: Option<&'static str>,
    places: usize,
    protected: bool,
}

/// How loosely a replacement matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Exact,
    /// Trailing whitespace ignored.
    Trailing,
    /// … and curly quotes and dashes read as plain ones.
    Quotes,
}

impl Tier {
    fn said(self) -> &'static str {
        match self {
            Tier::Exact => "",
            Tier::Trailing => "with trailing whitespace ignored",
            Tier::Quotes => "with curly quotes and dashes read as plain ones",
        }
    }
}

/// Every item, in order, against the files as the user has them — the change set, or one error naming each
/// item that failed. A worker's (it reads the disk).
fn resolve(k: &Known, items: &[Item]) -> Result<Resolved, String> {
    let real_root = std::fs::canonicalize(&k.root).map_err(|e| e.to_string())?;
    let mut states: Vec<(PathBuf, State)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut told: Vec<String> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let head = format!("edits[{i}] ({})", it.file());
        if let Err(e) = apply(k, &real_root, &mut states, it, &mut told, &head) {
            failed.push(format!("{head}: {e}"));
        }
    }
    if !failed.is_empty() {
        return Err(format!(
            "nothing changed — {} of {} failed:\n{}\nFix those and call edit again with every item (none was applied).",
            failed.len(),
            nav::count(items.len(), "item"),
            failed.join("\n")
        ));
    }
    let mut files = Vec::new();
    let mut stale = Vec::new();
    for (path, s) in &states {
        if s.from.is_some() {
            continue;
        }
        let target = states.iter().find(|(_, t)| t.from.as_ref() == Some(path));
        let rope = |t: &str| Rope::from_str(t);
        let change = |before: &str, after: &str, places: usize, op: Op, protected: bool| FileChange {
            path: path.clone(),
            rel: s.rel.clone(),
            before: rope(before),
            after: rope(after),
            places,
            op,
            protected,
        };
        let f = match (&s.before, &s.now, target) {
            (Some(b), None, Some((to, t))) => match &t.now {
                Some(a) => {
                    let op = Op::Move { to: to.clone(), to_rel: t.rel.clone() };
                    change(b, a, s.places + t.places, op, s.protected || t.protected)
                }
                None => change(b, "", 1, Op::Delete, s.protected),
            },
            (Some(b), None, None) => change(b, "", 1, Op::Delete, s.protected),
            (None, Some(a), _) => change("", a, s.places, Op::Create, s.protected),
            (Some(b), Some(a), _) if a != b => change(b, a, s.places, Op::Text, s.protected),
            _ => continue,
        };
        // Changed since Claude last saw it through tarae: what's there now was matched — said
        if f.op != Op::Create
            && let Some(was) = k.seen.get(path)
            && crate::disk::stamp(path).as_ref() != Some(was)
        {
            stale.push(format!(
                "({} has changed since you last read it — your edit matched what's there now; read it again before \
                 more edits)",
                s.rel
            ));
        }
        files.push(f);
    }
    Ok(Resolved { files, told, stale })
}

/// One item against the files so far (`states`). Err: why it fails.
fn apply(
    k: &Known,
    real_root: &Path,
    states: &mut Vec<(PathBuf, State)>,
    it: &Item,
    told: &mut Vec<String>,
    head: &str,
) -> Result<(), String> {
    let (path, rel, protected) = place(k, real_root, it.file())?;
    let at = state(k, states, &path, rel.clone(), protected)?;
    let gone = |s: &State| s.went.map(|w| format!("{} was {w} earlier in this call", s.rel));
    match it {
        Item::Replace { old, new, all, .. } => {
            let s = &mut states[at].1;
            if let Some(why) = gone(s) {
                return Err(why);
            }
            match &s.now {
                None if old.is_empty() => s.now = Some(new.clone()),
                None => return Err(format!("{rel} doesn't exist — an empty old_string makes it")),
                Some(t) if old.is_empty() && t.is_empty() => s.now = Some(new.clone()),
                Some(_) if old.is_empty() => {
                    return Err(format!(
                        "old_string is empty, but {rel} is there and isn't empty — give the text to replace"
                    ));
                }
                Some(t) => {
                    let (after, n, tier) = replace_in(t, old, new, *all)?;
                    if tier != Tier::Exact {
                        told.push(format!("{head} matched only {} — check it", tier.said()));
                    }
                    s.now = Some(after);
                    s.places += n;
                    return Ok(());
                }
            }
            s.places += 1;
            Ok(())
        }
        Item::Delete { .. } => {
            path_op_allowed(k, it.file(), &path, &rel, protected)?;
            let s = &mut states[at].1;
            if let Some(why) = gone(s) {
                return Err(why);
            }
            if s.now.is_none() {
                return Err(format!("{rel} doesn't exist"));
            }
            s.now = None;
            s.went = Some("deleted");
            Ok(())
        }
        Item::Move { to, .. } => {
            path_op_allowed(k, it.file(), &path, &rel, protected)?;
            let (to_path, to_rel, to_protected) = place(k, real_root, to)?;
            path_op_allowed(k, to, &to_path, &to_rel, to_protected)?;
            if to_path == path {
                return Err(format!("{rel} is already there"));
            }
            if let Some(why) = gone(&states[at].1) {
                return Err(why);
            }
            if states[at].1.now.is_none() {
                return Err(format!("{rel} doesn't exist"));
            }
            if states.iter().any(|(p, _)| *p == to_path) {
                return Err(format!("{to_rel} is changed earlier in this call — move first, then edit"));
            }
            if to_path.is_dir() {
                return Err(format!("{to_rel} is a folder — give the file's new path, name and all"));
            }
            if load(k, &to_path)?.is_some() {
                return Err(format!("{to_rel} is there already — a move doesn't overwrite"));
            }
            let s = &mut states[at].1;
            // (its text and its count go with it)
            let (text, places) = (s.now.take(), std::mem::take(&mut s.places));
            s.went = Some("moved");
            states.push((
                to_path,
                State {
                    rel: to_rel,
                    before: None,
                    now: text,
                    from: Some(path),
                    went: None,
                    places: places.max(1),
                    protected: to_protected,
                },
            ));
            Ok(())
        }
    }
}

/// Where `given` really is: its real path, from the project's root, and whether it's a name Claude's runs
/// can't write — Err when it's outside the project, the project itself, or a file named like a key Claude
/// hasn't been let read.
fn place(k: &Known, real_root: &Path, given: &str) -> Result<(PathBuf, String, bool), String> {
    place_in(&k.root, &k.keys, real_root, given)
}

/// `place` for `root` and the files named like keys Claude hasn't been let read — also where an edit being written
/// goes (`writing.rs`). A worker's (it resolves links).
pub(crate) fn place_in(
    root: &Path,
    keys: &crate::reads::Keys,
    real_root: &Path,
    given: &str,
) -> Result<(PathBuf, String, bool), String> {
    let abs = crate::chat::resolve(root, given);
    let real = crate::paths::inside(real_root, &abs)
        .ok_or_else(|| format!("{given} is outside the project — edit changes files inside it only"))?;
    if real == real_root {
        return Err("that's the project folder itself".into());
    }
    let rel = nav::rel(real_root, &real);
    if keys.shut(&real) {
        return Err(crate::reads::key_left_out(&rel));
    }
    Ok((real, rel, crate::edits::protected(root, &abs)))
}

/// A move or delete only of the project's own plain files: not a name Claude's runs can't write, not a link
/// (it'd move or delete what it leads to), not a buffer with the user's unsaved edits.
fn path_op_allowed(k: &Known, given: &str, real: &Path, rel: &str, protected: bool) -> Result<(), String> {
    if protected {
        return Err(format!(
            "{rel} is a build script, manifest or setting Claude's runs can't write — this tool doesn't move or \
             delete it; ask the user"
        ));
    }
    let abs = crate::chat::resolve(&k.root, given);
    if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{given} is a link — this tool doesn't move or delete links"));
    }
    if k.dirty.iter().any(|d| d == real) {
        return Err(format!("{rel} has the user's unsaved edits — not moved or deleted"));
    }
    Ok(())
}

/// The index of `path`'s state, read the first time (an open buffer's text, else the disk's).
fn state(
    k: &Known,
    states: &mut Vec<(PathBuf, State)>,
    path: &Path,
    rel: String,
    protected: bool,
) -> Result<usize, String> {
    if let Some(i) = states.iter().position(|(p, _)| p == path) {
        return Ok(i);
    }
    if path.is_dir() {
        return Err(format!("{rel} is a folder"));
    }
    let before = load(k, path)?;
    let s = State { rel, now: before.clone(), before, from: None, went: None, places: 0, protected };
    states.push((path.to_path_buf(), s));
    Ok(states.len() - 1)
}

/// `path`'s text as the user has it: an open buffer's, else the disk's — None: no file.
fn load(k: &Known, path: &Path) -> Result<Option<String>, String> {
    if let Some(t) = k.open.get(path) {
        return Ok(Some(t.to_string()));
    }
    match std::fs::read(path) {
        Ok(b) => String::from_utf8(b)
            .map(Some)
            .map_err(|_| format!("{} isn't UTF-8 text — not this tool's", nav::rel(&k.root, path))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// `old` → `new` in `text`: once (it must match once) or every match — exactly, else at the first looser
/// tier that matches. The text after, how many, and the tier.
fn replace_in(text: &str, old: &str, new: &str, all: bool) -> Result<(String, usize, Tier), String> {
    if let Some((spans, tier)) = matches(text, old) {
        if spans.len() > 1 && !all {
            let lines: Vec<String> =
                spans.iter().take(AMBIGUOUS_LINES).map(|(a, _)| line_of(text, *a).to_string()).collect();
            let more = if spans.len() > AMBIGUOUS_LINES { ", …" } else { "" };
            return Err(format!(
                "old_string matches {} times (lines {}{more}) — give more of the text around the one you mean, \
                 or replace_all: true",
                spans.len(),
                lines.join(", ")
            ));
        }
        let mut out = text.to_string();
        for (a, b) in spans.iter().rev() {
            out.replace_range(*a..*b, new);
        }
        return Ok((out, spans.len(), tier));
    }
    Err(not_found(text, old, new))
}

/// Where `old` is in `text` (byte spans in the text as it is), at the first tier it matches at.
fn matches(text: &str, old: &str) -> Option<(Vec<(usize, usize)>, Tier)> {
    for tier in [Tier::Exact, Tier::Trailing, Tier::Quotes] {
        let (hay, map) = normalized(text, tier);
        let (needle, _) = normalized(old, tier);
        if needle.is_empty() {
            continue;
        }
        let found: Vec<usize> = hay.match_indices(&needle).map(|(at, _)| at).collect();
        if found.is_empty() {
            continue;
        }
        return Some((
            found.iter().map(|&a| (map[a], end_of(text, map[a + needle.len() - 1]))).collect(),
            tier,
        ));
    }
    None
}

/// The one place `old` is in `text`, as `call` will find it — None where it's in none or more than one (an edit
/// being written is drawn where it goes only when that's one place: `writing.rs`).
pub(crate) fn one_place(text: &str, old: &str) -> Option<(usize, usize)> {
    match matches(text, old)?.0.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// The byte after the character holding byte `at` of `text`.
fn end_of(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at + text[at..].chars().next().map_or(0, char::len_utf8)
}

/// `s` as a tier reads it, and for each of its bytes the byte of `s` it came from (one more: `s`'s end).
fn normalized(s: &str, tier: Tier) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().expect("a character");
        if tier >= Tier::Trailing && (c == ' ' || c == '\t') {
            // A run of blanks before a line's end (or the text's) is left out
            let run = s[i..].len() - s[i..].trim_start_matches([' ', '\t']).len();
            let after = &s[i + run..];
            if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
                i += run;
                continue;
            }
        }
        if tier >= Tier::Quotes
            && let Some(plain) = plain(c)
        {
            out.push(plain);
            map.push(i);
            i += c.len_utf8();
            continue;
        }
        out.push(c);
        map.extend(i..i + c.len_utf8());
        i += c.len_utf8();
    }
    map.push(s.len());
    (out, map)
}

/// A curly quote, a dash or a no-break space as the plain character it stands for.
fn plain(c: char) -> Option<char> {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => Some('\''),
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => Some('"'),
        '\u{2013}' | '\u{2014}' | '\u{2212}' => Some('-'),
        '\u{00A0}' => Some(' '),
        _ => None,
    }
}

/// The line (from 1) byte `at` of `text` is on.
fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// Why `old` isn't in `text`: `new` is there already (the edit made before?), and the lines most like it.
fn not_found(text: &str, old: &str, new: &str) -> String {
    let mut msg = "old_string isn't in the file".to_string();
    if !new.trim().is_empty()
        && let Some(at) = text.find(new)
    {
        msg.push_str(&format!(
            " — new_string is already there (line {}): was this edit made already?",
            line_of(text, at)
        ));
    }
    match closest(text, old) {
        Some((from, lines)) => {
            msg.push_str(&format!("\n  the closest lines, from line {from}:"));
            for (i, l) in lines.iter().enumerate() {
                msg.push_str(&format!("\n  {}: {}", from + i, nav::clip(l, 200)));
            }
        }
        None => msg.push_str(" — nor anything like it"),
    }
    msg
}

/// The run of `text`'s lines most like `old`'s (as many, up to `CLOSEST_LINES`) — each line's likeness its
/// characters' pairs in common, ends trimmed — from which line (from 1), if any is alike at all.
fn closest<'t>(text: &'t str, old: &str) -> Option<(usize, Vec<&'t str>)> {
    let want: Vec<Vec<(char, char)>> = old.lines().take(CLOSEST_LINES).map(|l| pairs(l.trim())).collect();
    let lines: Vec<&str> = text.lines().collect();
    if want.is_empty() || lines.is_empty() {
        return None;
    }
    let have: Vec<Vec<(char, char)>> = lines.iter().map(|l| pairs(l.trim())).collect();
    let k = want.len().min(lines.len());
    let mut best = (0.0, 0);
    for j in 0..=lines.len() - k {
        let score: f64 = (0..k).map(|i| likeness(&have[j + i], &want[i])).sum::<f64>() / want.len() as f64;
        if score > best.0 {
            best = (score, j);
        }
    }
    (best.0 >= 0.3).then(|| (best.1 + 1, lines[best.1..best.1 + k].to_vec()))
}

/// A line's character pairs, sorted.
fn pairs(s: &str) -> Vec<(char, char)> {
    let cs: Vec<char> = s.chars().collect();
    let mut p: Vec<(char, char)> = cs.windows(2).map(|w| (w[0], w[1])).collect();
    crate::sorting::sort(&mut p);
    p
}

/// Two lines' likeness (Dice's coefficient on character pairs): 1 the same, 0 nothing in common.
fn likeness(a: &[(char, char)], b: &[(char, char)]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => (common, i, j) = (common + 1, i + 1, j + 1),
        }
    }
    2.0 * common as f64 / (a.len() + b.len()) as f64
}

#[cfg(test)]
mod tests;
