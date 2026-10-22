//! The `read` tool (toolcalls, 2026-10-13): Claude reads files in the editor — several files and line ranges
//! in one call, a symbol by name from its syntax tree (`outlines.rs`), a file's end — with no process and,
//! inside the project, no card. It runs in tarae, outside every sandbox, so it keeps Read's rules itself
//! (`reads::Gate`): what Read would ask about (outside the project and its allowed places, a file named like a
//! key) it asks on the same card, and runs again once let through. A big file asked whole comes as its outline
//! with line numbers (Zed's rule), saying how to ask for the text — not a page cut short. What it read is
//! what Claude saw of those files (`Edits.seen` — an `edit` of one changed since says so). An image comes as a
//! picture (`media.rs`, readimg 2026-10-20) — so the chat needs no Read of Claude Code's; a PDF says its pages
//! aren't read yet.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::media::Kind;
use crate::tools::{Call, Part, answer};

/// A file asked whole bigger than this (bytes) comes as its outline.
const WHOLE_MAX: usize = 16 * 1024;

/// Lines in one answer at most, and a line's characters.
const LINES_MAX: usize = 1500;
const LINE_MAX: usize = 500;

/// Files and ranges one call asks for at most · symbols one name reads at most · outline rows shown.
const ITEMS_MAX: usize = 30;
const NAMED_MAX: usize = 5;
const OUTLINE_MAX: usize = 300;

/// A file whole asked of one without an outline: its first lines.
const HEAD_LINES: usize = 100;

/// Pictures in one answer at most.
const IMAGES_MAX: usize = 20;

/// The tool's schema. (It no longer sends Claude to Read before an Edit: Claude Code 2.1.296 with this model
/// edits a file it hasn't read — tests/live.rs `chat_edit_then_native_edit`; older models needed the Read.)
pub fn schema() -> Value {
    json!({
        "name": "read",
        "description": "Read files in the editor — no process, and no card inside the project. Several files \
    and ranges in one call: `src/a.rs:10-40,90-120`, `src/a.rs:-30` (its last 30 lines), `src/a.rs:run_search` \
    or `src/a.rs:Editor::open` (a function, type or module by name, from its syntax tree), or a path alone (the \
    whole file — over 16 KB it comes as its outline with line numbers; then ask for ranges or names). Lines \
    come numbered (`12: …`). An image (PNG, JPEG, GIF, WebP) comes as a picture. Ask for enough in one \
    call to skip another turn. Outside the project it asks the user, as Read does.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "`path`, `path:a-b,c-d`, `path:-N` or `path:name` — from the project's root, or absolute"
                }
            },
            "required": ["files"]
        }
    })
}

/// What one item asks of its file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Want {
    Whole,
    /// Line ranges, from 1, both ends in.
    Lines(Vec<(usize, usize)>),
    /// The last N lines.
    Tail(usize),
    Symbol(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    pub(crate) given: String,
    pub(crate) path: String,
    pub(crate) want: Want,
}

/// `path`, `path:10-40,90`, `path:-30`, `path:name` — the `:` after the path's last `/`; what follows that
/// isn't a range, an end or a name leaves the whole as the path. (The chat's row reads it so too.)
pub(crate) fn item(spec: &str) -> Item {
    let spec = spec.trim();
    let whole = || Item { given: spec.into(), path: spec.into(), want: Want::Whole };
    let from = spec.rfind('/').map_or(0, |i| i + 1);
    let Some(at) = spec[from..].find(':').map(|i| from + i) else { return whole() };
    let (path, what) = (&spec[..at], &spec[at + 1..]);
    if path.is_empty() || what.is_empty() {
        return whole();
    }
    let num = |s: &str| s.trim().parse::<usize>().ok().filter(|&n| n > 0);
    let want = if let Some(n) = what.strip_prefix('-').and_then(num) {
        Want::Tail(n)
    } else if let Some(ranges) = what
        .split(',')
        .map(|r| match r.split_once('-') {
            Some((a, b)) => Some((num(a)?, num(b)?)),
            None => num(r).map(|a| (a, a)),
        })
        .collect::<Option<Vec<_>>>()
    {
        Want::Lines(ranges.into_iter().map(|(a, b)| (a.min(b), a.max(b))).collect())
    } else if what.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && what.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':')
    {
        Want::Symbol(what.into())
    } else {
        return whole();
    };
    Item { given: spec.into(), path: path.into(), want }
}

fn items_of(args: &Value) -> Result<Vec<Item>, String> {
    let files =
        args["files"].as_array().ok_or("files is required: a list of `path`, `path:a-b`, `path:name`")?;
    let specs: Vec<&str> = files.iter().filter_map(Value::as_str).filter(|s| !s.trim().is_empty()).collect();
    if specs.is_empty() {
        return Err("files is empty — give `path`, `path:a-b`, `path:-N` or `path:name`".into());
    }
    if specs.len() > ITEMS_MAX {
        return Err(format!("{} items — {ITEMS_MAX} at most in one call", specs.len()));
    }
    Ok(specs.into_iter().map(item).collect())
}

/// The tool: the items checked now, read on a worker.
pub fn call(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let items = items_of(args)?;
    go(ed, call, items, args.clone(), Vec::new());
    Ok(())
}

/// A read that waited on the read card, run again with what it let through (`reads::Then`).
pub fn again(ed: &mut Editor, t: crate::reads::Then) {
    match items_of(&t.args) {
        Ok(items) => go(ed, t.call, items, t.args, t.granted),
        Err(e) => answer(ed, t.call, Err(e)),
    }
}

fn go(ed: &mut Editor, call: Call, items: Vec<Item>, args: Value, granted: Vec<PathBuf>) {
    let root = crate::nav::project_root(ed);
    let unsaved: Vec<PathBuf> =
        ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
    let gate = crate::reads::Gate::of(ed, granted.clone());
    ed.events.jobs().spawn(move || {
        // (taken before it's read: a change while it reads counts as one since)
        let stamps: Vec<(PathBuf, Option<crate::disk::Stamp>)> = items
            .iter()
            .filter_map(|it| std::fs::canonicalize(crate::chat::resolve(&root, &it.path)).ok())
            .map(|p| {
                let s = crate::disk::stamp(&p);
                (p, s)
            })
            .collect();
        let r = read(&root, &items, &gate, &unsaved);
        move |ed: &mut Editor| match r {
            Ok(Ok(mut parts)) => {
                // (another workspace's change that would conflict with a file read — collab C2: a line each)
                let told: Vec<String> =
                    stamps.iter().flat_map(|(p, _)| crate::overlap::told(ed, p)).collect();
                if !told.is_empty() {
                    parts.push(Part::Text(format!("{}\n", told.join("\n"))));
                }
                for (p, s) in stamps {
                    crate::edittool::saw(ed, &p, s);
                }
                crate::tools::answer_parts(ed, call, Ok(parts))
            }
            Ok(Err(ask)) => {
                let then = crate::reads::Then { call, tool: "read", args, granted };
                crate::reads::tool_ask(ed, then, *ask)
            }
            Err(e) => answer(ed, call, Err(e)),
        }
    });
}

/// What a read answers: its text, pictures among it — Ok(Err): a file Read would ask about, the card's question
/// (the whole call waits on it).
type Answer = Result<Result<Vec<Part>, Box<crate::reads::Ask>>, String>;

/// The answer: each item's lines under its heading (`== path:a-b (of N lines)`), an image's picture after its
/// own, or what's said instead. A worker's.
fn read(root: &Path, items: &[Item], gate: &crate::reads::Gate, unsaved: &[PathBuf]) -> Answer {
    read_between(root, items, gate, unsaved, &|| {})
}

/// What an item found: a file opened (read from that descriptor — never by its path again), a folder,
/// nothing there. (Boxed: an `Opened` holds a `Metadata`, 200 bytes on x86_64.)
enum Found {
    File(Box<crate::paths::Opened>),
    Folder,
    Missing,
}

/// `read`, `between` run after every item was judged and before any is read (tests: what a run writing the
/// project could do in between).
fn read_between(
    root: &Path,
    items: &[Item],
    gate: &crate::reads::Gate,
    unsaved: &[PathBuf],
    between: &dyn Fn(),
) -> Answer {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    // Every item opened and judged before any is read: one card, then the whole answer. What's judged is what
    // was opened — the descriptor's own path (`paths::open_read`), and it's read from that descriptor: a
    // folder on the path swapped for a link meanwhile can't send the read somewhere never judged. Outside the
    // project the place is judged before anything is said of it — whether it's there, a file or a folder, is
    // read too (Read asks first as well); inside, a file named like a key.
    let mut found: Vec<Found> = Vec::new();
    for it in items {
        let abs = root.join(&it.path);
        let (judged, f) = match crate::paths::open_read(&abs) {
            Ok(o) => {
                let inside = o.real.starts_with(&root);
                let judged = (!inside || o.meta.is_file()).then(|| (o.real.clone(), !o.meta.is_dir()));
                let f = match () {
                    _ if o.meta.is_file() => Found::File(Box::new(o)),
                    _ if o.meta.is_dir() => Found::Folder,
                    _ => Found::Missing,
                };
                (judged, f)
            }
            // (nothing opened: the place by its path — a `..` past what's there can't be told where it leads,
            // the card says the path as given)
            Err(_) => match crate::paths::real(&abs) {
                Some(r) if r.starts_with(&root) => (None, Found::Missing),
                Some(r) => (Some((r, true)), Found::Missing),
                None => (Some((abs, true)), Found::Missing),
            },
        };
        if let Some((at, file)) = judged
            && let Err(ask) = gate.judge(&at, file)
        {
            return Ok(Err(ask));
        }
        found.push(f);
    }
    between();
    let (mut out, mut parts) = (String::new(), Vec::new());
    let (mut left, mut images) = (LINES_MAX, Room { pictures: IMAGES_MAX, bytes: crate::media::ANSWER_MAX });
    let mut dirty = Vec::new();
    for (it, f) in items.iter().zip(found) {
        let o = match f {
            Found::File(o) => *o,
            Found::Folder => {
                out.push_str(&format!("== {}: a folder — search with output files lists it\n", it.given));
                continue;
            }
            Found::Missing => {
                out.push_str(&format!("== {}: no such file\n", it.given));
                continue;
            }
        };
        let shown = crate::nav::rel(&root, &o.real);
        if unsaved.contains(&o.real) {
            dirty.push(shown.clone());
        }
        for p in section(o, &shown, it, &mut left, &mut images) {
            match p {
                Part::Text(t) => out.push_str(&t),
                Part::Image(i) => {
                    if !out.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut out)));
                    }
                    parts.push(Part::Image(i));
                }
            }
        }
    }
    if !dirty.is_empty() {
        out.push_str(&format!(
            "(unsaved edits in {} aren't here — what's here is from disk)\n",
            dirty.join(", ")
        ));
    }
    let out = out.trim_end();
    if !out.is_empty() || parts.is_empty() {
        parts.push(Part::Text(out.to_string()));
    }
    Ok(Ok(parts))
}

/// What's left of an answer's room for pictures: how many, and their bytes as sent (`media::ANSWER_MAX`).
struct Room {
    pictures: usize,
    bytes: usize,
}

/// One item's part of the answer, read from its opened file, at most `left` lines of text and what's left of
/// `room` for pictures (taken from them).
fn section(
    mut o: crate::paths::Opened,
    shown: &str,
    it: &Item,
    left: &mut usize,
    room: &mut Room,
) -> Vec<Part> {
    use std::io::Read;
    // Its first bytes say what it is: a picture or a PDF past `FILE_MAX` is said before the rest is read
    let mut bytes = Vec::new();
    if let Err(e) = (&mut o.file).take(64).read_to_end(&mut bytes) {
        return vec![Part::Text(format!("== {shown}: {e}\n"))];
    }
    // (a PDF by its name too: one with bytes before its `%PDF-` is still no text to show)
    let pdf = Path::new(shown).extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    let kind = crate::media::kind(&bytes).or(pdf.then_some(Kind::Pdf));
    if let Some(k) = kind.filter(|k| *k != Kind::Pdf && o.meta.len() > crate::media::FILE_MAX) {
        let mb = crate::media::FILE_MAX / (1024 * 1024);
        return vec![Part::Text(format!("== {shown}: a {} file over {mb} MB — not read\n", k.said()))];
    }
    if kind == Some(Kind::Pdf) {
        return pictures(Kind::Pdf, Vec::new(), shown, it, room);
    }
    if let Err(e) = o.file.read_to_end(&mut bytes) {
        return vec![Part::Text(format!("== {shown}: {e}\n"))];
    }
    if let Some(kind) = kind {
        return pictures(kind, bytes, shown, it, room);
    }
    let (real, stamp) = (o.real.as_path(), crate::testcode::stamp_of(&o.meta));
    let outline = |text: &str| crate::outlines::of_text(real, stamp, text);
    vec![Part::Text(section_of(&bytes, &outline, shown, it, left, &mut Vec::new()))]
}

/// An image's part: its heading — what it is, how it's sent, what it costs — then the picture, while `room` has
/// it (taken from it). A PDF's: that its pages aren't read yet (readimg left PDFs to a later piece).
fn pictures(kind: Kind, bytes: Vec<u8>, shown: &str, it: &Item, room: &mut Room) -> Vec<Part> {
    use crate::media::Sent;
    let said = |s: String| vec![Part::Text(s)];
    let k = kind.said();
    if kind == Kind::Pdf {
        return said(format!("== {shown}: a PDF — its pages aren't read yet (only images are)\n"));
    }
    if room.pictures == 0 {
        return said(format!(
            "== {shown}: not sent — {IMAGES_MAX} pictures an answer at most; ask for it in another call\n"
        ));
    }
    let (img, (w, h), sent) = match crate::media::for_model(kind, bytes) {
        Ok(i) => i,
        Err(e) => return said(format!("== {shown}: a {k} image — {e}\n")),
    };
    if img.sent_len() > room.bytes {
        let mb = crate::media::ANSWER_MAX / (1024 * 1024);
        return said(format!(
            "== {shown}: a {k} image, {w}×{h} — not sent: this answer's pictures come to {mb} MB at most; ask \
             for it in another call\n"
        ));
    }
    room.pictures -= 1;
    room.bytes -= img.sent_len();
    let to = match (img.w, img.h) == (w, h) {
        true => "written again".to_string(),
        false => format!("scaled to {}×{}", img.w, img.h),
    };
    let how = match sent {
        Sent::AsIs => "sent as it is".to_string(),
        Sent::Png { best: false } => format!("sent {to} as PNG"),
        Sent::Png { best: true } => format!("sent {to} as PNG, compressed hardest"),
        Sent::Jpeg(q) => format!("sent {to} as JPEG at quality {q}"),
    };
    let whole = match it.want {
        Want::Whole => "",
        _ => " — the whole picture: ranges are for text",
    };
    let tokens = crate::usage::tokens(img.tokens());
    vec![
        Part::Text(format!("== {shown}: a {k} image, {w}×{h}, {how} (≈ {tokens} tokens){whole}\n")),
        Part::Image(img),
    ]
}

/// A file's outline, or why there's none.
type Outline = Result<std::sync::Arc<Vec<crate::nav::Sym>>, String>;

/// `section` of a file's `bytes`, its outline from `outline_of` (given its text) — the lines it gives in `seen`.
fn section_of(
    bytes: &[u8],
    outline_of: &dyn Fn(&str) -> Outline,
    shown: &str,
    it: &Item,
    left: &mut usize,
    seen: &mut Vec<(usize, usize)>,
) -> String {
    if bytes.contains(&0) {
        return format!("== {shown}: binary — not read\n");
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let of = format!("of {n} line{}", if n == 1 { "" } else { "s" });
    let ranges: Vec<(usize, usize, String)> = match &it.want {
        Want::Whole if bytes.len() > WHOLE_MAX => {
            return outline(outline_of(&text), shown, &lines, bytes.len(), left);
        }
        Want::Whole => vec![(1, n, format!("== {shown} ({of})"))],
        Want::Tail(k) => {
            let a = n.saturating_sub(*k) + 1;
            vec![(a, n, format!("== {shown}:{a}-{n} ({of})"))]
        }
        Want::Lines(rs) => {
            rs.iter().map(|&(a, b)| (a, b.min(n), format!("== {shown}:{a}-{} ({of})", b.min(n)))).collect()
        }
        Want::Symbol(name) => match outline_of(&text) {
            Err(e) => return format!("== {shown}:{name}: no symbols — {e}; ask for line ranges\n"),
            Ok(syms) => {
                let found = crate::outlines::named(&syms, name);
                if found.is_empty() {
                    let names: Vec<&str> =
                        syms.iter().filter(|s| s.depth == 0).map(|s| s.name.as_str()).take(30).collect();
                    return format!(
                        "== {shown}:{name}: no symbol named {name} — at its top level: {}; `{shown}` alone gives its outline\n",
                        names.join(", ")
                    );
                }
                found
                    .iter()
                    .take(NAMED_MAX)
                    .map(|s| {
                        (s.lines.0 + 1, s.lines.1 + 1, format!("== {shown}:{name} — {} ({of})", s.said()))
                    })
                    .collect()
            }
        },
    };
    let mut out = String::new();
    for (a, b, head) in ranges {
        out.push_str(&head);
        out.push('\n');
        if a > b || a > n {
            out.push_str(&format!("  (past its end — it has {n} lines)\n"));
            continue;
        }
        let take = (b - a + 1).min(*left);
        for (i, l) in lines[a - 1..a - 1 + take].iter().enumerate() {
            out.push_str(&format!("{}: {}\n", a + i, crate::nav::clip(l, LINE_MAX)));
        }
        if take > 0 {
            seen.push((a, a + take - 1));
        }
        *left -= take;
        if take < b - a + 1 {
            let from = a + take;
            out.push_str(&format!("… cut at {LINES_MAX} lines an answer — `{shown}:{from}-{b}` reads on\n"));
        }
    }
    out
}

/// The reviewer's `read` (bgreview): the items of `args` read from snapshot `snap` (`snapread.rs`) — the texts as
/// it was taken, whatever the files hold now, in the chat's form. A path outside the checkout, a file it leaves
/// out (named like a key, a link, …) or none there is said in its item's place; Err: nothing could be read. A
/// worker's (it reads git).
#[cfg(test)]
pub fn read_snapshot(snap: &crate::snapread::Snap, args: &Value) -> Result<String, String> {
    read_snapshot_seen(snap, args).0
}

/// The lines of a snapshot's files a read gave: each file's path and the ranges it showed (from 1, both ends in).
pub type Seen = Vec<(String, Vec<(usize, usize)>)>;

/// `read_snapshot`, and the lines it gave (`Seen`) — those shown, never those asked: an outline, a refusal or
/// what `LINES_MAX` cut isn't read (the reviewer's files left out of its diff count as read by these).
pub fn read_snapshot_seen(snap: &crate::snapread::Snap, args: &Value) -> (Result<String, String>, Seen) {
    let mut seen = Seen::new();
    let r = read_snapshot_into(snap, args, &mut seen);
    (r, seen)
}

fn read_snapshot_into(snap: &crate::snapread::Snap, args: &Value, seen: &mut Seen) -> Result<String, String> {
    use crate::snapread::Found;
    let items = items_of(args)?;
    let rels: Vec<Option<String>> = items.iter().map(|it| snap.rel_of(&it.path)).collect();
    let wanted: Vec<&str> = rels.iter().flatten().map(String::as_str).collect();
    let mut texts: std::collections::HashMap<String, std::sync::Arc<[u8]>> = Default::default();
    snap.each_text(&wanted, &mut |rel, t| {
        texts.insert(rel.to_string(), t);
        true
    })?;
    let (mut out, mut left, mut read) = (String::new(), LINES_MAX, 0);
    for (it, rel) in items.iter().zip(&rels) {
        let Some(rel) = rel else {
            out.push_str(&format!(
                "== {}: outside the checkout — the reviewer reads only its snapshot\n",
                it.given
            ));
            continue;
        };
        // (an instruction file git ignores: the checkout's, said so in each header)
        if let Some(got) = snap.instruction(rel) {
            match got {
                Ok(bytes) => {
                    read += 1;
                    let none = |_: &str| Err(String::new());
                    for l in section_of(&bytes, &none, rel, it, &mut left, &mut Vec::new()).lines() {
                        let from = if l.starts_with("== ") {
                            " — from the checkout: not in the snapshot"
                        } else {
                            ""
                        };
                        out.push_str(&format!("{l}{from}\n"));
                    }
                }
                Err(why) => out.push_str(&format!("== {rel}: {why}\n")),
            }
            continue;
        }
        match (snap.found(rel), texts.get(rel)) {
            (Found::File, Some(bytes)) => {
                read += 1;
                let outline = |text: &str| snap.outline(rel, text);
                let mut lines = Vec::new();
                out.push_str(&section_of(bytes, &outline, rel, it, &mut left, &mut lines));
                if !lines.is_empty() {
                    seen.push((rel.clone(), lines));
                }
            }
            (Found::Folder, _) => {
                out.push_str(&format!("== {}: a folder — search with output files lists it\n", it.given))
            }
            (Found::Left(why), _) => out.push_str(&format!("== {rel}: {}\n", why.said())),
            _ => out.push_str(&format!("== {}: no such file in the snapshot\n", it.given)),
        }
    }
    let out = out.trim_end().to_string();
    match read {
        0 => Err(out),
        _ => Ok(out),
    }
}

/// A big file asked whole: its outline with line numbers and how to ask for the text — or, with no outline
/// (no grammar for it), its first lines and the same.
fn outline(
    syms: Result<std::sync::Arc<Vec<crate::nav::Sym>>, String>,
    shown: &str,
    lines: &[&str],
    bytes: usize,
    left: &mut usize,
) -> String {
    let n = lines.len();
    let ask = format!(
        "The text isn't here — ask for `{shown}:a-b` line ranges or `{shown}:name` symbols (several in one call)."
    );
    let kb = bytes.div_ceil(1024);
    match syms {
        Ok(syms) if !syms.is_empty() => {
            let mut out = format!("== {shown} — {n} lines, {kb} KB: its outline. {ask}\n");
            let syms: Vec<&crate::nav::Sym> = syms.iter().filter(|s| s.listed()).collect();
            for s in syms.iter().take(OUTLINE_MAX) {
                let (a, b) = (s.lines.0 + 1, s.lines.1 + 1);
                let kind = if s.kind.is_empty() { String::new() } else { format!("{} ", s.kind) };
                out.push_str(&format!("{}{kind}{}  {a}–{b}\n", "  ".repeat(s.depth), s.name));
            }
            if syms.len() > OUTLINE_MAX {
                out.push_str(&format!(
                    "… {} more symbols — `{shown}:a-b` for a part\n",
                    syms.len() - OUTLINE_MAX
                ));
            }
            out
        }
        _ => {
            let take = HEAD_LINES.min(n).min(*left);
            *left -= take;
            let mut out = format!("== {shown}:1-{take} (of {n} lines, {kb} KB — no outline for it). {ask}\n");
            for (i, l) in lines[..take].iter().enumerate() {
                out.push_str(&format!("{}: {}\n", i + 1, crate::nav::clip(l, LINE_MAX)));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_an_item_asks() {
        let it = |s: &str| item(s).want;
        assert_eq!(it("src/a.rs"), Want::Whole);
        assert_eq!(it("src/a.rs:10-40,90"), Want::Lines(vec![(10, 40), (90, 90)]));
        assert_eq!(it("src/a.rs:40-10"), Want::Lines(vec![(10, 40)]), "either way round");
        assert_eq!(it("src/a.rs:-30"), Want::Tail(30));
        assert_eq!(it("src/a.rs:run_search"), Want::Symbol("run_search".into()));
        assert_eq!(item("src/a.rs:Editor::open").path, "src/a.rs");
        assert_eq!(it("src/a.rs:Editor::open"), Want::Symbol("Editor::open".into()));
        assert_eq!(it("notes:2026/x.md"), Want::Whole, "a `:` before the last `/` is the path's");
        assert_eq!(it("a.rs:1-x"), Want::Whole, "neither a range nor a name: the whole is the path");
    }

    /// A project, a place outside it, and a link in the project to that place — under the test's own temp
    /// folder. (`dir`, the project's real path, outside's.)
    fn places(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = crate::scratch::Scratch::new(&format!("readtool-{name}")).into_path();
        let (dir, outside) = (base.join("p"), base.join("outside"));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            dir.join("src/a.rs"),
            "struct Thing;\n\nimpl Thing {\n    fn run(&self) {\n        go();\n    }\n}\n\nfn go() {}\n",
        )
        .unwrap();
        let big: String = (0..800).map(|i| format!("fn f{i}() {{\n    let x = {i};\n}}\n")).collect();
        std::fs::write(dir.join("src/big.rs"), big).unwrap();
        std::fs::write(dir.join(".env"), "API_KEY=tarae-fake-token\n").unwrap();
        std::fs::write(outside.join("o.txt"), "outside marker\n").unwrap();
        std::os::unix::fs::symlink(outside.join("o.txt"), dir.join("src/link.txt")).unwrap();
        let real = |p: &Path| crate::paths::real(p).unwrap();
        (real(&dir), real(&outside), base)
    }

    fn text(r: Answer) -> String {
        match r {
            Ok(Ok(parts)) => crate::tools::said(&parts),
            Ok(Err(ask)) => panic!("asks the user about {}", ask.at_said()),
            Err(e) => panic!("{e}"),
        }
    }

    fn items(specs: &[&str]) -> Vec<Item> {
        specs.iter().map(|s| item(s)).collect()
    }

    /// Ranges, a file's end, a symbol by its type's name, several in one answer, numbered; a big file asked
    /// whole is its outline, saying how to ask for the text; what isn't a file is said in its place.
    #[test]
    fn reads_ranges_ends_symbols_and_outlines_big_files() {
        let (dir, _, base) = places("what");
        let gate = crate::reads::Gate::for_test(&dir);
        let t = text(read(&dir, &items(&["src/a.rs:2-3", "src/a.rs:-1", "src/nope.rs", "src"]), &gate, &[]));
        assert!(t.starts_with("== src/a.rs:2-3 (of 9 lines)\n2: \n3: impl Thing {\n"), "{t}");
        assert!(t.contains("== src/a.rs:9-9 (of 9 lines)\n9: fn go() {}\n"), "{t}");
        assert!(t.contains("== src/nope.rs: no such file\n"), "{t}");
        assert!(t.contains("== src: a folder"), "{t}");
        if crate::outlines::of(&dir.join("src/a.rs")).is_err() {
            return std::fs::remove_dir_all(&base).unwrap(); // (no Rust grammar in this build)
        }
        let t = text(read(&dir, &items(&["src/a.rs:Thing::run", "src/a.rs:nothing"]), &gate, &[]));
        assert!(
            t.starts_with("== src/a.rs:Thing::run — fn run (4–6) (of 9 lines)\n4:     fn run(&self) {\n"),
            "{t}"
        );
        assert!(t.contains("no symbol named nothing — at its top level: Thing, impl Thing, go"), "{t}");
        let t = text(read(&dir, &items(&["src/big.rs"]), &gate, &[]));
        assert!(t.starts_with("== src/big.rs — 2400 lines, "), "{t}");
        assert!(t.contains("its outline. The text isn't here — ask for `src/big.rs:a-b` line ranges or `src/big.rs:name` symbols"), "{t}");
        assert!(t.contains("\nfn f10  31–33\n"), "with its line numbers: {t}");
        assert!(!t.contains("let x"), "no text: {t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Read's rules, kept by the tool itself (it runs outside every sandbox): a file outside the project, a
    /// file named like a key in it, and a link in it to a place outside each ask the user — the link as
    /// where it leads; what the card let through is read.
    #[test]
    fn what_read_would_ask_about_asks() {
        let (dir, outside, base) = places("rules");
        let gate = crate::reads::Gate::for_test(&dir);
        let asks = |specs: &[&str], gate: &crate::reads::Gate| match read(&dir, &items(specs), gate, &[]) {
            Ok(Err(ask)) => ask.at_said(),
            other => panic!("{:?}", other.map(|r| r.map_err(|a| a.at_said()))),
        };
        let o = outside.join("o.txt");
        assert_eq!(asks(&[o.to_str().unwrap()], &gate), o.display().to_string(), "outside");
        assert_eq!(asks(&["src/link.txt"], &gate), o.display().to_string(), "a link in it: where it leads");
        assert_eq!(asks(&[".env"], &gate), dir.join(".env").display().to_string(), "named like a key");
        // Whether a place outside is there, or a folder, is read too: asked before it's said
        let nope = outside.join("nope.txt");
        assert_eq!(asks(&[nope.to_str().unwrap()], &gate), nope.display().to_string(), "missing, outside");
        assert_eq!(
            asks(&[outside.to_str().unwrap()], &gate),
            outside.display().to_string(),
            "a folder outside"
        );
        let t = text(read(&dir, &items(&["src/nope.rs"]), &gate, &[]));
        assert_eq!(t, "== src/nope.rs: no such file", "missing in the project: said");
        assert_eq!(
            asks(&["src/a.rs:1-2", "src/link.txt"], &gate),
            o.display().to_string(),
            "one item asking: the whole call waits, nothing read before"
        );
        let let_through = crate::reads::Gate::for_test(&dir).granting(o.clone());
        let t = text(read(&dir, &items(&["src/link.txt"]), &let_through, &[]));
        assert!(t.ends_with("1: outside marker"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The pictures of an answer, and its text with each as `[image/png w×h]`.
    fn pictures_of(r: Answer) -> (Vec<crate::media::Image>, String) {
        let parts = match r {
            Ok(Ok(parts)) => parts,
            Ok(Err(ask)) => panic!("asks the user about {}", ask.at_said()),
            Err(e) => panic!("{e}"),
        };
        let images = parts.iter().filter_map(|p| if let Part::Image(i) = p { Some(i.clone()) } else { None });
        (images.collect(), crate::tools::said(&parts))
    }

    /// readimg: an image in the project comes as a picture after its heading — its kind, size, how it's sent and
    /// what it costs — beside text read in the same call; a range of one is the whole picture, said so.
    #[test]
    fn an_image_is_read_as_a_picture() {
        let (dir, _, base) = places("image");
        std::fs::create_dir_all(dir.join("shots")).unwrap();
        let png = crate::media::tests::png(400, 300);
        std::fs::write(dir.join("shots/screen.png"), &png).unwrap();
        let gate = crate::reads::Gate::for_test(&dir);
        let (images, t) = pictures_of(read(&dir, &items(&["shots/screen.png", "src/a.rs:1"]), &gate, &[]));
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].mime, &images[0].bytes), ("image/png", &png), "sent as it is");
        assert_eq!(
            t,
            "== shots/screen.png: a PNG image, 400×300, sent as it is (≈ 165 tokens)\n\n[image/png 400×300]\n\
             == src/a.rs:1-1 (of 9 lines)\n1: struct Thing;"
        );
        let (_, t) = pictures_of(read(&dir, &items(&["shots/screen.png:1-5"]), &gate, &[]));
        assert!(t.contains("the whole picture: ranges are for text"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// readimg: a big picture comes scaled to fit, how said; a PDF says its pages aren't read yet — never its
    /// bytes as text; a decompression bomb is refused, said in its place, the rest of the call read.
    #[test]
    fn a_big_image_is_scaled_a_pdf_and_a_bomb_said() {
        let (dir, _, base) = places("big-image");
        std::fs::write(dir.join("wide.png"), crate::media::tests::png(3000, 1500)).unwrap();
        std::fs::write(dir.join("paper.pdf"), "%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\n").unwrap();
        std::fs::write(dir.join("bomb.png"), crate::media::tests::bomb(15_000, 15_000)).unwrap();
        let gate = crate::reads::Gate::for_test(&dir);
        let specs = ["wide.png", "paper.pdf", "bomb.png", "src/a.rs:1"];
        let (images, t) = pictures_of(read(&dir, &items(&specs), &gate, &[]));
        assert_eq!(images.len(), 1, "{t}");
        assert_eq!((images[0].w, images[0].h, images[0].mime), (2000, 1000, "image/png"));
        assert!(
            t.starts_with(
                "== wide.png: a PNG image, 3000×1500, sent scaled to 2000×1000 as PNG (≈ 2.6k tokens)\n"
            ),
            "{t}"
        );
        assert!(t.contains("\n== paper.pdf: a PDF — its pages aren't read yet (only images are)\n"), "{t}");
        assert!(!t.contains("Catalog"), "no PDF bytes as text: {t}");
        assert!(t.contains("\n== bomb.png: a PNG image — too big to decode here (at most 20000 pixels a side, 256 MB decoded) — not sent\n"), "{t}");
        assert!(t.ends_with("1: struct Thing;"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// readimg: what one answer's pictures come to has a cap (`media::ANSWER_MAX`), the request's limit in mind —
    /// the picture past it said in its place, to ask for in another call; and a picture's file past `FILE_MAX` is
    /// said from its size and first bytes (a sparse file: nothing else of it there to read).
    #[test]
    fn an_answers_pictures_have_a_cap_and_a_huge_file_isnt_read() {
        let (dir, _, base) = places("picture-cap");
        // (noise that won't compress: each about 3.6 MB, 4.8 MB as sent)
        let shot = crate::media::tests::noise(1100, 1100);
        assert!(shot.len() <= crate::media::RAW_MAX && shot.len() > 3_000_000);
        std::fs::write(dir.join("shot.png"), &shot).unwrap();
        let huge = std::fs::File::create(dir.join("huge.png")).unwrap();
        std::io::Write::write_all(&mut &huge, &crate::media::tests::png(4, 4)).unwrap();
        huge.set_len(crate::media::FILE_MAX + 1).unwrap();
        let gate = crate::reads::Gate::for_test(&dir);
        let specs = ["shot.png", "shot.png", "shot.png", "huge.png"];
        let (images, t) = pictures_of(read(&dir, &items(&specs), &gate, &[]));
        assert_eq!(images.len(), 2, "{t}");
        assert!(images.iter().map(|i| i.sent_len()).sum::<usize>() <= crate::media::ANSWER_MAX);
        assert!(
            t.contains("\n== shot.png: a PNG image, 1100×1100 — not sent: this answer's pictures come to 10 MB at most; ask for it in another call\n"),
            "{t}"
        );
        assert!(t.ends_with("== huge.png: a PNG file over 100 MB — not read"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// readimg: a picture named like a key is a key — asked on Read's card, nothing of it read before.
    #[test]
    fn a_key_named_image_asks_first() {
        let (dir, _, base) = places("key-image");
        std::fs::write(dir.join("id_rsa"), crate::media::tests::png(4, 4)).unwrap();
        let gate = crate::reads::Gate::for_test(&dir);
        match read(&dir, &items(&["src/a.rs:1", "id_rsa"]), &gate, &[]) {
            Ok(Err(ask)) => assert_eq!(ask.at_said(), dir.join("id_rsa").display().to_string()),
            other => panic!(
                "read unasked: {:?}",
                other.map(|r| r.map(|p| crate::tools::said(&p)).map_err(|a| a.at_said()))
            ),
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// cardauto D3: outside home is read unasked, as runs read it — the system's files here — but not a
    /// mounted volume, nor a file named like a key there, nor anything with the switch off (tests' own:
    /// `Editor.read_outside`).
    #[test]
    fn outside_home_reads_unasked_but_volumes_and_keys() {
        let (dir, _, base) = places("outside-home");
        let gate = crate::reads::Gate::for_test(&dir).reading_outside();
        let hosts = std::fs::canonicalize("/etc/hosts").unwrap();
        let t = text(read(&dir, &items(&[hosts.to_str().unwrap()]), &gate, &[]));
        assert!(t.starts_with(&format!("== {}", hosts.display())), "read: {t}");
        let asks = |spec: &str, gate: &crate::reads::Gate| match read(&dir, &items(&[spec]), gate, &[]) {
            Ok(Err(ask)) => ask.at_said(),
            other => panic!("{spec}: {:?}", other.map(|r| r.map_err(|a| a.at_said()))),
        };
        // (named like a key: asked wherever it is — not there, so nothing's read either way)
        let key = hosts.with_file_name(".env");
        assert_eq!(asks(key.to_str().unwrap(), &gate), key.display().to_string());
        if let Some(v) = ["/Volumes", "/mnt", "/media"].iter().find(|v| Path::new(v).is_dir()) {
            assert_eq!(asks(v, &gate), *v, "a volume");
        }
        let off = crate::reads::Gate::for_test(&dir);
        assert_eq!(asks(hosts.to_str().unwrap(), &off), hosts.display().to_string(), "off: asks");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// What was judged is what's read: a folder on the path swapped for a link to a place outside after the
    /// judging (a run writing the project can) doesn't send the read there — it reads from the descriptor
    /// that was judged.
    #[test]
    fn a_folder_swapped_for_a_link_after_judging_isnt_followed() {
        let (dir, outside, base) = places("swap");
        std::fs::write(dir.join("src/s.txt"), "inside marker\n").unwrap();
        std::fs::create_dir_all(outside.join("src")).unwrap();
        std::fs::write(outside.join("src/s.txt"), "outside marker\n").unwrap();
        let gate = crate::reads::Gate::for_test(&dir);
        let swap = || {
            std::fs::rename(dir.join("src"), dir.join("src.old")).unwrap();
            std::os::unix::fs::symlink(outside.join("src"), dir.join("src")).unwrap();
        };
        let t = text(read_between(&dir, &items(&["src/s.txt"]), &gate, &[], &swap));
        assert!(t.ends_with("1: inside marker"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Calls waiting on the read card are always answered: `a` for the chat lets the one queued behind the
    /// card through as well (each run again, answered with its text); at the turn's end the one queued is
    /// answered at once and the one on the card when its card is found gone (`asking_sync`) — a call left
    /// waiting would mark its session stuck.
    #[test]
    fn calls_waiting_on_the_card_are_always_answered() {
        let (dir, outside, base) = places("queued");
        std::fs::write(outside.join("p.txt"), "second marker\n").unwrap();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        for f in ["q.txt", "r.txt"] {
            std::fs::write(elsewhere.join(f), "marker\n").unwrap();
        }
        let elsewhere = crate::paths::real(&elsewhere).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.chat = Some(crate::chat::Chat::new());
        ed.session.at = crate::session::At::Chat;
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let settle = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !done(ed) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).expect("settled before the timeout");
                ed.handle_event(ev);
            }
        };
        let card = |ed: &Editor| {
            ed.offers.iter().find_map(|o| match &o.what {
                crate::offer::What::Read { pending, .. } => Some((o.id, (**pending).clone())),
                _ => None,
            })
        };
        let queued = |ed: &Editor| ed.session.chat.as_ref().map_or(0, |c| c.reads.len());
        let answered = |id: u64| {
            crate::tools::ANSWERED
                .with(|a| a.borrow().iter().find(|(k, _)| *k == json!(id)).map(|(_, r)| r.clone()))
        };
        let ask = |ed: &mut Editor, n: u64, f: &Path| {
            call(ed, Call::of(generation, "", json!(n)), &json!({ "files": [f.display().to_string()] }))
                .unwrap();
        };
        // `a`: the folder for this chat — the one queued behind goes through without a card
        ask(&mut ed, 911, &outside.join("o.txt"));
        ask(&mut ed, 912, &outside.join("p.txt"));
        // (the card offers their folder once it's been looked through for tokens — `reads::refold`; answered
        // by its own key, so the call is the answer's, not a card gone — `card_answered`)
        let folder = |ed: &Editor| {
            ed.offers.iter().any(|o| matches!(&o.what, crate::offer::What::Read { folder: Some(_), .. }))
        };
        settle(&mut ed, &|ed| folder(ed) && queued(ed) == 1);
        ed.offer_for_chat();
        settle(&mut ed, &|_| answered(911).is_some() && answered(912).is_some());
        assert!(answered(911).unwrap().unwrap().ends_with("1: outside marker"));
        assert!(answered(912).unwrap().unwrap().ends_with("1: second marker"));
        // The turn ends with one on the card and one behind it
        ask(&mut ed, 913, &elsewhere.join("q.txt"));
        ask(&mut ed, 914, &elsewhere.join("r.txt"));
        settle(&mut ed, &|ed| card(ed).is_some() && queued(ed) == 1);
        // (which one is on the card: the first judged — the two are judged on workers, either may be)
        let on_card = card(&ed).map(|(_, p)| p.input.to_string()).unwrap_or_default();
        let (up, behind) = if on_card.contains("q.txt") { (913, 914) } else { (914, 913) };
        crate::reads::turn_over(&mut ed);
        let ended = answered(behind).expect("the one queued: answered at once");
        assert_eq!(ended, Err("the turn ended before the user answered".to_string()));
        ed.sessions_sync();
        settle(&mut ed, &|_| answered(up).is_some());
        assert_eq!(answered(up).unwrap(), Err(crate::offer::LOST.to_string()), "the one on the card");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// frictions29 33: tarae's `search` of a folder outside the project asks on Read's card, which names what it
    /// searches for (it said `for ` and nothing) and the files it keeps to; a pattern too long for the card is cut,
    /// and `v` shows it whole.
    #[test]
    fn a_searchs_card_names_its_pattern() {
        let (dir, outside, base) = places("search-card");
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.chat = Some(crate::chat::Chat::new());
        ed.session.at = crate::session::At::Chat;
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let settle = |ed: &mut Editor| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Read { .. })) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).expect("its card before the timeout");
                ed.handle_event(ev);
            }
        };
        let real = std::fs::canonicalize(&outside).unwrap();
        let search = |ed: &mut Editor, n: u64, pattern: &str| {
            let args = json!({"pattern": pattern, "path": outside.display().to_string(), "glob": "*.txt"});
            crate::nav::search(ed, Call::of(generation, "", json!(n)), &args).unwrap();
            settle(ed);
            (ed.offers[0].question.clone(), ed.offers[0].answers(false))
        };
        let (q, answers) = search(&mut ed, 921, "outside marker");
        let head =
            format!("searches {} and everything in it\nfor outside marker\nin files *.txt\n", real.display());
        assert!(q.starts_with(&head), "{q}");
        assert!(!answers.iter().any(|a| a.0 == 'v'), "{answers:?}");
        ed.offer_dismiss();
        crate::reads::turn_over(&mut ed);
        // Too long for the card: cut, whole on `v`
        let long = format!("terminals{}", "|grant".repeat(50));
        let (q, answers) = search(&mut ed, 922, &long);
        assert!(q.contains(&format!("\nfor {}…\n", &long[..60])), "{q}");
        assert!(answers.contains(&('v', "Pattern")), "{answers:?}");
        ed.offer_view();
        let shown = ed.doc().text.to_string();
        assert!(shown.contains(&format!("The whole pattern:\n\n{long}\n")), "{shown}");
        // (one a card can't show as it is: escaped)
        assert_eq!(crate::reads::pattern_shown("a\nb\u{202E}c"), ("a\\nb\\u{202e}c".to_string(), false));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The whole way: a read outside the project waits on Read's card holding its call; `y` runs it again
    /// and answers with the text, `n` answers no — the call answered either way.
    #[test]
    fn a_read_outside_waits_on_the_card_and_is_answered() {
        let (dir, outside, base) = places("card");
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.chat = Some(crate::chat::Chat::new());
        ed.session.at = crate::session::At::Chat;
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let o = outside.join("o.txt");
        let settle = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !done(ed) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).expect("settled before the timeout");
                ed.handle_event(ev);
            }
        };
        let card = |ed: &Editor| {
            ed.offers.iter().find_map(|o| match &o.what {
                crate::offer::What::Read { pending, .. } => Some((o.id, (**pending).clone())),
                _ => None,
            })
        };
        let answered = |id: u64| {
            crate::tools::ANSWERED
                .with(|a| a.borrow().iter().find(|(k, _)| *k == json!(id)).map(|(_, r)| r.clone()))
        };
        for (n, yes) in [(901, true), (902, false)] {
            let args = json!({ "files": [o.display().to_string()] });
            call(&mut ed, Call::of(generation, "", json!(n)), &args).unwrap();
            settle(&mut ed, &|ed| card(ed).is_some());
            let (_, pending) = card(&ed).unwrap();
            assert!(pending.then.is_some(), "the card holds the call");
            // (by the card's own keys: the call is the answer's then, not a card gone — `card_answered`)
            if yes {
                ed.offer_accept()
            } else {
                ed.offer_dismiss()
            }
            settle(&mut ed, &|_| answered(n).is_some());
            match (yes, answered(n).unwrap()) {
                (true, Ok(t)) => assert!(t.ends_with("1: outside marker"), "{t}"),
                (false, Err(e)) => {
                    assert!(e.starts_with("the user chose not to let you read"), "{e}");
                    assert!(e.ends_with("(this no holds for the rest of the turn)"), "{e}");
                }
                (_, r) => panic!("{r:?}"),
            }
        }
        // frictions29 34: the no holds for that file until the turn ends — asked again, told so at once, no card
        let args = json!({ "files": [o.display().to_string()] });
        call(&mut ed, Call::of(generation, "", json!(903)), &args).unwrap();
        settle(&mut ed, &|_| answered(903).is_some());
        let e = answered(903).unwrap().unwrap_err();
        let real = std::fs::canonicalize(&o).unwrap();
        assert!(e.starts_with(&format!("the user said no to {} this turn", real.display())), "{e}");
        assert!(card(&ed).is_none(), "no card");
        // The turn's end: asked again, a card again
        crate::reads::turn_over(&mut ed);
        call(&mut ed, Call::of(generation, "", json!(904)), &args).unwrap();
        settle(&mut ed, &|ed| card(ed).is_some());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
