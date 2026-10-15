//! Files' outlines from their syntax trees (`nav::outline_syms`, any language with its grammar), kept while
//! a file is unchanged — by `testcode`'s stamp and racy rule, symbols only, no trees: what a search hit is
//! in, what `read path:symbol` reads, what a big file read whole shows. Read and parsed on workers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::nav::Sym;

type Kept = HashMap<PathBuf, (crate::testcode::Stamp, Arc<Vec<Sym>>)>;

static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

/// Files kept at most — past it the table starts over.
const KEPT_MAX: usize = 4096;

/// `path`'s outline (a real path — one judged or walked), read from that very file as opened
/// (`paths::open_read` — one swapped for a link since isn't read) — Err: why there's none (no grammar for it,
/// not readable).
pub fn of(path: &Path) -> Result<Arc<Vec<Sym>>, String> {
    use std::io::Read;
    let mut o = crate::paths::open_read(path).map_err(|e| e.to_string())?;
    if o.real != path || !o.meta.is_file() {
        return Err("it changed while it was read".into());
    }
    let stamp = crate::testcode::stamp_of(&o.meta);
    if let Some(syms) = kept(path, stamp) {
        return Ok(syms);
    }
    let mut text = String::new();
    o.file.read_to_string(&mut text).map_err(|e| e.to_string())?;
    of_text(path, stamp, &text)
}

fn kept(path: &Path, stamp: Option<crate::testcode::Stamp>) -> Option<Arc<Vec<Sym>>> {
    let stamp = stamp?;
    let kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    kept.as_ref()?.get(path).filter(|(s, _)| *s == stamp).map(|(_, syms)| syms.clone())
}

/// The outline of `text`, read from `path` (as opened, its stamp `stamp`) — kept as `of` keeps it.
pub fn of_text(
    path: &Path,
    stamp: Option<crate::testcode::Stamp>,
    text: &str,
) -> Result<Arc<Vec<Sym>>, String> {
    if let Some(syms) = kept(path, stamp) {
        return Ok(syms);
    }
    let syms = Arc::new(crate::nav::outline_syms(path, Some(ropey::Rope::from_str(text)))?);
    if let Some(stamp) = stamp {
        let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
        let table = kept.get_or_insert_with(HashMap::new);
        if table.len() >= KEPT_MAX {
            table.clear();
        }
        table.insert(path.to_path_buf(), (stamp, syms.clone()));
    }
    Ok(syms)
}

/// (Tests: `path` no longer kept.)
#[cfg(test)]
fn forget(path: &Path) {
    if let Some(k) = KEPT.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        k.remove(path);
    }
}

/// The innermost symbol holding line `line` (from 0) — a definition: a variant's line is in its enum.
pub fn holding(syms: &[Sym], line: usize) -> Option<&Sym> {
    syms.iter()
        .filter(|s| s.listed() && s.lines.0 <= line && line <= s.lines.1)
        .max_by_key(|s| (s.depth, s.lines.0))
}

/// The symbols named `name` — `Type::name` (or `a::b::name`): one inside a symbol whose name ends with the
/// part before (`impl Type`, `impl Trait for Type`, `mod a`).
pub fn named<'a>(syms: &'a [Sym], name: &str) -> Vec<&'a Sym> {
    let (outer, last) = match name.rsplit_once("::") {
        Some((o, l)) => (Some(o.rsplit("::").next().unwrap_or(o)), l),
        None => (None, name),
    };
    syms.iter()
        .filter(|s| s.name == last || s.name.rsplit("::").next() == Some(last))
        .filter(|s| {
            outer.is_none_or(|o| {
                syms.iter().any(|p| {
                    p.depth < s.depth
                        && p.lines.0 <= s.lines.0
                        && s.lines.1 <= p.lines.1
                        && (p.name == o || p.name.ends_with(&format!(" {o}")))
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(depth: usize, kind: &'static str, name: &str, a: usize, b: usize) -> Sym {
        Sym { depth, kind, name: name.into(), lines: (a, b) }
    }

    /// A file whose folder was swapped for a link since its path was found (walked, judged) isn't read for its
    /// outline nor its test code — what's opened must be that very file.
    #[test]
    fn a_file_swapped_for_a_link_isnt_read_for_its_symbols() {
        let scratch = crate::scratch::Scratch::new("outlines-swap");
        let base = scratch.to_path_buf();
        for d in ["p/src", "outside/src"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
            std::fs::write(base.join(d).join("a.rs"), "#[test]\nfn secret_name() {}\n").unwrap();
        }
        let path = std::fs::canonicalize(base.join("p/src/a.rs")).unwrap();
        let before = of(&path);
        assert!(crate::testcode::read_opened(&path).is_some(), "as it was: read");
        std::fs::rename(base.join("p/src"), base.join("p/src.old")).unwrap();
        std::os::unix::fs::symlink(base.join("outside/src"), base.join("p/src")).unwrap();
        // (kept from before or not, nothing is read through the link)
        forget(&path);
        assert_eq!(of(&path).map(|_| ()), Err("it changed while it was read".into()), "{before:?}");
        assert_eq!(crate::testcode::read_opened(&path), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The innermost symbol around a line; a method found by its type's name too (`impl X` holds it).
    #[test]
    fn a_line_is_in_its_innermost_symbol_and_names_find_methods() {
        let syms = [
            sym(0, "type", "Editor", 0, 3),
            sym(0, "", "impl Editor", 5, 20),
            sym(1, "fn", "open", 6, 10),
            sym(0, "", "impl Drop for Editor", 22, 30),
            sym(1, "fn", "drop", 23, 29),
            sym(0, "fn", "open", 40, 44),
        ];
        assert_eq!(holding(&syms, 8).map(|s| s.name.as_str()), Some("open"));
        assert_eq!(holding(&syms, 12).map(|s| s.name.as_str()), Some("impl Editor"));
        assert_eq!(holding(&syms, 35), None);
        assert_eq!(named(&syms, "open").len(), 2, "the method and the free function");
        assert_eq!(named(&syms, "Editor::open")[0].lines, (6, 10));
        assert_eq!(named(&syms, "Editor::drop")[0].lines, (23, 29), "impl Trait for Type");
        assert!(named(&syms, "Other::open").is_empty());
        assert_eq!(syms[2].said(), "fn open (7–11)");
    }

    /// A variant is found by its enum's name (`read path:Stage::Stopped`, a diagram's anchor), but a line in it
    /// is in its enum — search's "in …" doesn't change.
    #[test]
    fn variants_are_found_by_name_and_hold_no_line() {
        let syms = [
            sym(0, "type", "Stage", 0, 4),
            sym(1, crate::nav::VARIANT, "Running", 1, 2),
            sym(1, crate::nav::VARIANT, "Stopped", 3, 3),
            sym(0, "type", "Ended", 6, 8),
            sym(1, crate::nav::VARIANT, "Stopped", 7, 7),
        ];
        assert_eq!(named(&syms, "Stage::Stopped").iter().map(|s| s.lines).collect::<Vec<_>>(), [(3, 3)]);
        assert_eq!(named(&syms, "Stopped").len(), 2);
        assert_eq!(holding(&syms, 3).map(|s| s.name.as_str()), Some("Stage"));
    }
}
