//! `:session-apply [--base <rev>] <patch>` — an outside tree's change brought into this session as one change
//! set (frictions30 5, `target/sb/sessapply-plan.md`). The patch is read on a worker — the user's own read, of
//! the file they named: the card says what was read — and each file's hunks are laid on (`patch.rs`): from the
//! patch's base when it's known (`--base`, else the diff's `index` lines; read with `git cat-file`), exactly,
//! then merged three-way with the file as this session has it; with no base, each hunk exactly where it says
//! or the one place its lines all are — never loosened, never picked among several. It lands as the user's own
//! change set (`refactor::land_yours`: the card, the review, writes through `beneath.rs`, a `space u` step).
//! Every file inside the project or none; a binary change, a name inside `.git` or named like a key, a move or
//! delete of a protected name, a link or a buffer with unsaved edits refuse the patch, each said; conflicts
//! land as diff3 markers, said in the warning colour.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ropey::Rope;

use crate::editor::Editor;
use crate::nav;
use crate::patch::{FilePatch, Unlaid};
use crate::refactor::{FileChange, Op};

/// A patch bigger than this isn't read.
const PATCH_MAX: u64 = 16 << 20;

const USAGE: &str = "usage: :session-apply [--base <rev>] <patch>";

/// What the worker needs from the main loop.
struct Here {
    root: PathBuf,
    /// Open buffers' text, by path (loaded ones).
    open: HashMap<PathBuf, Rope>,
    /// Buffers with the user's unsaved edits.
    dirty: Vec<PathBuf>,
    keys: crate::reads::Keys,
}

/// A patch worked out: its files' changes, what was read (`read ~/x.patch · 515 B · 2 files`), and what the card
/// tells of how (a line each).
struct Worked {
    files: Vec<FileChange>,
    read: String,
    told: Vec<String>,
}

impl Editor {
    /// `:session-apply [--base <rev>] <patch>` (see the module's doc). One patch at a time.
    pub fn session_apply(&mut self, args: &str) -> Result<(), String> {
        let (base, file) = args_of(args)?;
        if self.session.applying.is_some() {
            return Err("a patch is landing here already — answer its card (or end its review) first".into());
        }
        let path = crate::typed::typed_path(self, file);
        let name = path.file_name().map_or_else(|| file.to_string(), |n| n.to_string_lossy().into_owned());
        let here = Here {
            root: nav::project_root(self),
            open: (self.docs.iter().filter(|d| !d.loading))
                .filter_map(|d| Some((d.path.clone()?, d.text.clone())))
                .collect(),
            dirty: (self.docs.iter().filter(|d| d.is_modified() && !d.unsaved_is_claudes()))
                .filter_map(|d| d.path.clone())
                .collect(),
            keys: crate::reads::Keys::of(self),
        };
        let jobs = self.events.jobs();
        // (as typed: its real path is the worker's to find, said on the card)
        let said = format!("reading {file}…");
        let label = name.clone();
        let work = move || work(&here, &path, base.as_deref(), &label);
        self.step(jobs, "apply", said, work, move |ed: &mut Editor, r: Result<Worked, String>| match r {
            Ok(_) if ed.session.applying.is_some() => {
                ed.set_error(format!("{name}: another patch is landing here — nothing changed"))
            }
            Ok(w) => crate::refactor::land_yours(
                ed,
                name.clone(),
                format!("apply {name}"),
                w.files,
                w.read,
                w.told.join("\n"),
            ),
            Err(e) => ed.set_error(format!("{name}: {e}")),
        });
        Ok(())
    }
}

/// `[--base <rev>] <patch>` — the base (None: none given) and the patch's path as typed.
fn args_of(args: &str) -> Result<(Option<String>, &str), String> {
    let args = args.trim();
    let (base, rest) = match args.strip_prefix("--base") {
        // (`--base` alone: no revision, no patch — the usage, not a file named `--base`)
        Some(r) if r.is_empty() || r.starts_with('=') || r.starts_with(char::is_whitespace) => {
            // (the separator may be any space, a wide one too)
            let r = r.strip_prefix('=').unwrap_or(r).trim_start();
            let (b, rest) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
            (Some(b), rest.trim())
        }
        _ => (None, args),
    };
    if rest.is_empty() {
        return Err(USAGE.into());
    }
    match base {
        Some(b) if b.is_empty() || b.starts_with('-') => Err(format!("{b:?} isn't a commit — {USAGE}")),
        _ => Ok((base.map(String::from), rest)),
    }
}

/// A size as said: `812 B`, `42 KB`, `3.1 MB`.
fn size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{} KB", n.div_ceil(1024)),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// The patch at `path` read and worked out against the project as the user has it — every file, or Err naming
/// each that can't be applied (nothing changes then). `name`: the patch's, for the conflict blocks. A worker's.
fn work(h: &Here, path: &Path, base: Option<&str>, name: &str) -> Result<Worked, String> {
    let shown = crate::allow::tilde(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let meta = std::fs::metadata(path).map_err(|e| format!("can't read {shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown} isn't a file"));
    }
    if meta.len() > PATCH_MAX {
        return Err(format!("{shown} is {} — a patch here is {} at most", size(meta.len()), size(PATCH_MAX)));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("can't read {shown}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} isn't UTF-8 text"))?;
    let p = crate::patch::parse(&text).map_err(|e| format!("{e} — nothing changed"))?;
    let real_root = std::fs::canonicalize(&h.root).map_err(|e| e.to_string())?;
    // (`--base`: resolved once, to its commit)
    let rev = match base {
        Some(b) => {
            let spec = format!("{b}^{{commit}}");
            let sha = crate::gitrepo::git(&h.root, &["rev-parse", "--verify", "-q", &spec], None)
                .map_err(|_| format!("there's no commit {b} here — nothing changed"))?;
            Some((sha.trim().to_string(), b.to_string()))
        }
        None => None,
    };
    let read = format!("read {shown} · {} · {}", size(meta.len()), nav::count(p.files.len(), "file"));
    let mut told = Vec::new();
    match &rev {
        Some((_, b)) => told.push(format!("merged three-way from {b}, the patch's base")),
        None if p.files.iter().any(|f| f.base.is_some()) => {
            told.push("merged three-way from each file's base (the diff's index lines)".into())
        }
        None => told.push("no base named — each change laid on where its lines are".into()),
    }
    let (mut files, mut failed) = (Vec::new(), Vec::new());
    for f in &p.files {
        match one(h, &real_root, f, rev.as_ref(), name, &mut told) {
            Ok(Some(c)) => files.push(c),
            Ok(None) => {}
            Err(e) => failed.push(format!("{}: {e}", f.shown())),
        }
    }
    if !failed.is_empty() {
        return Err(format!(
            "nothing changed — {} of {} can't be applied:\n{}",
            failed.len(),
            nav::count(p.files.len(), "file"),
            failed.join("\n")
        ));
    }
    Ok(Worked { files, read, told })
}

/// Where a name of the patch is here.
struct Place {
    /// Its real path.
    real: PathBuf,
    rel: String,
    /// A name Claude's runs can't write (`edits::protected`): reviewed with the card's warning.
    protected: bool,
    /// It's a link (never moved or deleted).
    link: bool,
}

/// `given` (from the patch's top) in the project — Err: outside it (absolute, or `..` past it), the project
/// itself, inside `.git`, named like a key, a folder.
fn place(h: &Here, real_root: &Path, given: &str) -> Result<Place, String> {
    if Path::new(given).is_absolute() {
        return Err(format!("{given} is an absolute path — a patch's names are from its tree's top"));
    }
    if Path::new(given).components().any(|c| c.as_os_str() == ".git") {
        return Err(format!("{given} is inside .git — not a patch's to change"));
    }
    let abs = h.root.join(given);
    let real = crate::paths::inside(real_root, &abs).ok_or_else(|| {
        format!("{given} is outside the project — a patch here changes files inside it only")
    })?;
    if real == real_root {
        return Err("that's the project folder itself".into());
    }
    let rel = nav::rel(real_root, &real);
    // (where it really leads: a link to `.git` in the project mustn't carry a write into it)
    if Path::new(&rel).components().any(|c| c.as_os_str() == ".git") {
        return Err(format!("{given} leads inside .git ({rel}) — not a patch's to change"));
    }
    if h.keys.shut(&real) {
        return Err(format!("{rel} is named like a key — a patch doesn't change it here"));
    }
    if real.is_dir() {
        return Err(format!("{rel} is a folder"));
    }
    let link = std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink());
    Ok(Place { real, rel, protected: crate::edits::protected(&h.root, &abs), link })
}

/// A move or delete only of the project's own plain files: not a protected name, not a link, not a buffer
/// with the user's unsaved edits.
fn path_op(h: &Here, p: &Place, what: &str) -> Result<(), String> {
    match () {
        _ if p.protected => Err(format!(
            "{} is a build script, manifest or setting — a patch doesn't {what} it; do that by hand",
            p.rel
        )),
        _ if p.link => Err(format!("{} is a link — a patch doesn't {what} links", p.rel)),
        _ if h.dirty.contains(&p.real) => Err(format!("{} has your unsaved edits — save them first", p.rel)),
        _ => Ok(()),
    }
}

/// `path`'s text as the user has it: an open buffer's, else the disk's — None: no file.
fn load(h: &Here, path: &Path) -> Result<Option<String>, String> {
    if let Some(t) = h.open.get(path) {
        return Ok(Some(t.to_string()));
    }
    match std::fs::read(path) {
        Ok(b) => String::from_utf8(b).map(Some).map_err(|_| "isn't UTF-8 text here".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// One file of the patch: its change (None: nothing to do — already as the patch has it, said), or why not.
fn one(
    h: &Here,
    real_root: &Path,
    f: &FilePatch,
    rev: Option<&(String, String)>,
    name: &str,
    told: &mut Vec<String>,
) -> Result<Option<FileChange>, String> {
    let old = f.old.as_deref().map(|p| place(h, real_root, p)).transpose()?;
    let new = f.new.as_deref().map(|p| place(h, real_root, p)).transpose()?;
    if let (Some(m), Some(at)) = (&f.mode, &new) {
        told.push(format!("{}: mode {m} not set (the patch sets it)", at.rel));
    }
    let change = |at: &Place, before: String, after: String, op: Op, protected: bool| FileChange {
        path: at.real.clone(),
        rel: at.rel.clone(),
        before: Rope::from_str(&before),
        after: Rope::from_str(&after),
        places: f.hunks.len().max(1),
        op,
        protected,
    };
    match (old, new) {
        (None, None) => Err("a file with no name".into()),
        // Made: nothing there yet (or already the patch's text)
        (None, Some(at)) => {
            let after = crate::patch::lay("", &f.hunks, false).map_err(|e| unlaid(f, &e, "its lines"))?.0;
            match load(h, &at.real)? {
                None => Ok(Some(change(&at, String::new(), after, Op::Create, at.protected))),
                Some(t) if t == after => {
                    told.push(format!("{}: there already, as the patch makes it", at.rel));
                    Ok(None)
                }
                Some(_) => Err("there already, with other text — the patch makes it".into()),
            }
        }
        // Deleted: only while it's the text the patch deletes
        (Some(at), None) => {
            path_op(h, &at, "delete")?;
            let Some(this) = load(h, &at.real)? else {
                told.push(format!("{}: gone already", at.rel));
                return Ok(None);
            };
            let base = base_of(h, f, rev)?;
            let as_patch = crate::patch::lay(&this, &f.hunks, false).is_ok_and(|(t, _)| t.is_empty());
            if !as_patch && base.as_ref().is_none_or(|(b, _)| *b != this) {
                return Err("changed here since the patch's text of it — not deleted".into());
            }
            Ok(Some(change(&at, this, String::new(), Op::Delete, at.protected)))
        }
        (Some(from), Some(to)) => {
            let moving = from.real != to.real;
            if moving {
                path_op(h, &from, "move")?;
                path_op(h, &to, "move a file to")?;
                if load(h, &to.real)?.is_some() {
                    return Err(format!("{} is there already — a move doesn't overwrite", to.rel));
                }
            }
            let Some(this) = load(h, &from.real)? else {
                return Err(format!("{} isn't here", from.rel));
            };
            let after = merged(h, f, rev, &this, name, &to.rel, told)?;
            match moving {
                true => {
                    let op = Op::Move { to: to.real.clone(), to_rel: to.rel.clone() };
                    Ok(Some(change(&from, this, after, op, from.protected || to.protected)))
                }
                false if after == this => {
                    told.push(format!("{}: already as the patch has it", to.rel));
                    Ok(None)
                }
                false => Ok(Some(change(&to, this, after, Op::Text, to.protected))),
            }
        }
    }
}

/// The file's base — its text in `--base`'s commit, else the blob its `index` line names — and how it's said
/// (`dbe7086c`). None: no base named, or (an `index` line's) not in this repository — then it's laid on as
/// it is. Err: `--base` doesn't have it.
fn base_of(
    h: &Here,
    f: &FilePatch,
    rev: Option<&(String, String)>,
) -> Result<Option<(String, String)>, String> {
    let blob = |spec: &str| crate::gitrepo::git(&h.root, &["cat-file", "blob", spec], None);
    match (rev, f.old.as_deref()) {
        (_, None) => Ok(None),
        (Some((sha, said)), Some(old)) => match blob(&format!("{sha}:{old}")) {
            Ok(t) => Ok(Some((t, said.clone()))),
            Err(_) => Err(format!("{old} isn't in {said} — is that the patch's base?")),
        },
        (None, Some(_)) => {
            let id =
                f.base.as_deref().filter(|id| id.len() >= 4 && id.bytes().all(|b| b.is_ascii_hexdigit()));
            Ok(id.and_then(|id| Some((blob(id).ok()?, id.to_string()))))
        }
    }
}

/// The file's text after the patch, from `this` (as the user has it): three-way from its base when that's known,
/// else its hunks laid on `this` — each said where it wasn't plain (`told`).
fn merged(
    h: &Here,
    f: &FilePatch,
    rev: Option<&(String, String)>,
    this: &str,
    name: &str,
    rel: &str,
    told: &mut Vec<String>,
) -> Result<String, String> {
    match base_of(h, f, rev)? {
        Some((base, said)) => {
            let theirs = crate::patch::lay(&base, &f.hunks, false)
                .map_err(|e| unlaid(f, &e, &format!("it doesn't lay on its base {said}")))?
                .0;
            let (text, n) =
                crate::patch::merge3(&base, this, &theirs, ["this session", &format!("base {said}"), name]);
            if n > 0 {
                told.push(format!(
                    "▲ {rel}: {} — they land as conflict markers, to resolve in the buffer",
                    nav::count(n, "conflict")
                ));
            }
            Ok(text)
        }
        None => {
            let (text, moved) =
                crate::patch::lay(this, &f.hunks, true).map_err(|e| unlaid(f, &e, "it doesn't lay on"))?;
            for (i, by) in moved {
                let way = if by > 0 { "lower" } else { "higher" };
                told.push(format!(
                    "{rel}: hunk {} laid on {} {way}",
                    i + 1,
                    nav::count(by.unsigned_abs(), "line")
                ));
            }
            Ok(text)
        }
    }
}

/// Why `f`'s hunks didn't go on, each by its number, its line in the patch and the line it names.
fn unlaid(f: &FilePatch, failed: &[(usize, Unlaid)], head: &str) -> String {
    let each: Vec<String> = failed
        .iter()
        .map(|(i, why)| {
            let h = &f.hunks[*i];
            let at = format!("hunk {} (patch line {}, at line {})", i + 1, h.line, h.start + 1);
            match why {
                Unlaid::Missing => format!("{at}: its lines aren't there"),
                Unlaid::Several(ls) => {
                    let ls: Vec<String> = ls.iter().map(usize::to_string).collect();
                    format!("{at}: its lines are at lines {} — not one place", ls.join(", "))
                }
                Unlaid::NoContext => format!("{at}: no context lines to place it by — give --base"),
            }
        })
        .collect();
    format!("{head} — {}", each.join("; "))
}

#[cfg(test)]
mod tests;
