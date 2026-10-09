//! tarae runtime — `core` grammars (4; debug builds also the `dev` one, rust; plus the `bundle` ones with
//! `--features bundled-grammars`)
//! are inside the binary (`build.rs`), the other grammars (.so) in
//! `~/.local/share/tarae/grammars/` (`tarae grammar install` fetches and builds them),
//! queries (.scm) inside the binary (`build.rs` embeds `runtime/queries/`). Never reads a Helix install.
//!
//! Lookup order: `$TARAE_RUNTIME` (when developing queries) → `$XDG_DATA_HOME/tarae`
//! (default `~/.local/share/tarae`).
//! A query with the same name under these folders' `queries/` wins; otherwise the built-in one.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/queries.rs"));
include!(concat!(env!("OUT_DIR"), "/packs.rs"));
include!(concat!(env!("OUT_DIR"), "/builtin_grammars.rs"));

/// Language function of a grammar built into the binary (None if absent — look for a downloaded .so).
pub fn builtin_grammar(name: &str) -> Option<unsafe extern "C" fn() -> *const ()> {
    BUILTIN_GRAMMARS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

/// tarae's cache folder: `$XDG_CACHE_HOME/tarae`, else `~/.cache/tarae` (a server's own state per project —
/// jdtls's `-data`, through its pack's `{cache}`).
pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_of(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

pub(crate) fn cache_dir_of(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    xdg.map(PathBuf::from).or_else(|| home.map(|h| PathBuf::from(h).join(".cache"))).map(|d| d.join("tarae"))
}

#[cfg(test)]
#[test]
fn the_cache_folder_is_xdgs_else_homes() {
    let p = |s: &str| Some(std::ffi::OsString::from(s));
    assert_eq!(cache_dir_of(p("/x/cache"), p("/h")), Some(PathBuf::from("/x/cache/tarae")));
    assert_eq!(cache_dir_of(None, p("/h")), Some(PathBuf::from("/h/.cache/tarae")));
    assert_eq!(cache_dir_of(None, None), None);
}

/// Where grammars are downloaded to (`tarae grammar install`).
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("tarae"))
}

pub fn dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut v: Vec<PathBuf> = Vec::new();
        v.extend(std::env::var_os("TARAE_RUNTIME").map(PathBuf::from));
        v.extend(data_dir());
        v.dedup();
        v
    })
}

/// Finds `rel` in the runtime folders (the first that exists).
pub fn find(rel: impl AsRef<Path>) -> Option<PathBuf> {
    dirs().iter().map(|d| d.join(rel.as_ref())).find(|p| p.exists())
}

/// Query source: `queries/<rel>` in a runtime folder if present, otherwise built-in.
pub fn query(rel: &str) -> Option<String> {
    if let Some(p) = find(format!("queries/{rel}"))
        && let Ok(s) = std::fs::read_to_string(p)
    {
        return Some(s);
    }
    let &(_, start, end) = QUERIES.iter().find(|(r, ..)| *r == rel)?;
    Some(queries_text()[start..end].to_string())
}

/// The built-in queries, unpacked at their first use (deflated in the binary — 233 KB of text is 52 KB so).
fn queries_text() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let bytes = miniz_oxide::inflate::decompress_to_vec(QUERIES_Z).expect("built-in queries unpack");
        String::from_utf8(bytes).expect("built-in queries are UTF-8")
    })
}

/// A language pack's file (`go/pack.toml`, `go/tests.scm`): `packs/<rel>` in a runtime folder if present,
/// otherwise built-in.
pub fn pack(rel: &str) -> Option<String> {
    pack_user(rel).or_else(|| pack_builtin(rel))
}

/// The pack file in a runtime folder only.
pub fn pack_user(rel: &str) -> Option<String> {
    find(format!("packs/{rel}")).and_then(|p| std::fs::read_to_string(p).ok())
}

/// The built-in pack file only.
pub fn pack_builtin(rel: &str) -> Option<String> {
    PACKS.iter().find(|(r, _)| *r == rel).map(|(_, s)| s.to_string())
}

/// Every built-in pack's `pack.toml`: (language, text) — no disk.
pub fn pack_manifests_builtin() -> impl Iterator<Item = (&'static str, &'static str)> {
    PACKS.iter().filter_map(|(r, s)| Some((r.strip_suffix("/pack.toml")?, *s)))
}

/// Every runtime folder's `packs/<pack>/pack.toml`: (pack, text) — the first folder's where two have one.
/// Reads the disk: `languages::get` once, before the first frame.
pub fn pack_manifests_user() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for d in dirs() {
        let mut found: Vec<_> = std::fs::read_dir(d.join("packs")).into_iter().flatten().flatten().collect();
        crate::sorting::by_key(&mut found, |e| e.file_name());
        for e in found {
            let pack = e.file_name().to_string_lossy().into_owned();
            if out.iter().any(|(p, _)| *p == pack) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(e.path().join("pack.toml")) {
                out.push((pack, text));
            }
        }
    }
    out
}

/// Every language with a pack: built in, or in a runtime folder's `packs/`.
pub fn pack_langs() -> Vec<String> {
    let mut out: Vec<String> =
        PACKS.iter().filter_map(|(r, _)| r.strip_suffix("/pack.toml")).map(str::to_string).collect();
    for d in dirs() {
        for e in std::fs::read_dir(d.join("packs")).into_iter().flatten().flatten() {
            if e.path().join("pack.toml").is_file() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    crate::sorting::sort(&mut out);
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn queries_are_embedded() {
        assert!(super::QUERIES.len() > 100);
        assert!(super::query("rust/highlights.scm").is_some_and(|q| q.contains("@keyword")));
        assert!(super::query("typescript/highlights.scm").is_some_and(|q| q.contains("inherits")));
    }
}
