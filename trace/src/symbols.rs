//! Naming the addresses of a stack, after the process may have gone: user addresses by (file, file offset)
//! from the maps kept while it lived, kernel ones from kallsyms — both through blazesym (ELF + DWARF, inlined
//! calls, Rust demangling). Every name is cached: a run has thousands of stacks over a few hundred addresses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blazesym::symbolize::source::{Elf, Kernel, Source};
use blazesym::symbolize::{CodeInfo, Input, Symbolized, Symbolizer};

use crate::maps::{self, Region};
use crate::report::own_frame;

/// A frame's name and the source file its code is in, when the binary says (DWARF).
type Frame = (String, Option<String>);

pub struct Symbols {
    sym: Symbolizer,
    user: HashMap<(PathBuf, u64), Vec<Frame>>,
    kernel: HashMap<u64, Vec<String>>,
}

fn source(c: Option<&CodeInfo<'_>>) -> Option<String> {
    let c = c?;
    let path = c.dir.as_ref().map_or_else(|| PathBuf::from(&*c.file), |d| d.join(&*c.file));
    Some(path.to_string_lossy().into_owned())
}

/// A Rust symbol's hash suffix (`::h0123456789abcdef`) off — the same function in two builds is one name.
fn clean(name: &str) -> String {
    match name.rsplit_once("::h") {
        Some((head, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => head.into(),
        _ => name.into(),
    }
}

/// A symbol's frames, outer → inner: the function the address is in, then the calls inlined into it — each
/// with the file its code at this address is in.
fn frames(s: Symbolized<'_>, unknown: impl FnOnce() -> String) -> Vec<Frame> {
    match s {
        Symbolized::Sym(sym) => {
            let mut v = vec![(clean(&sym.name), source(sym.code_info.as_deref()))];
            v.extend(sym.inlined.iter().map(|f| (clean(&f.name), source(f.code_info.as_ref()))));
            v
        }
        Symbolized::Unknown(_) => vec![(unknown(), None)],
    }
}

impl Symbols {
    pub fn new() -> Self {
        Symbols { sym: Symbolizer::new(), user: HashMap::new(), kernel: HashMap::new() }
    }

    /// A user stack (leaf first, as the kernel gives it) → frames root first, and whether each is the
    /// program's own code (`report::own_frame`; `exe` is its binary). Return addresses are named by the byte
    /// before them — the call — so a call that ends a function isn't put in the next one.
    pub fn user(
        &mut self,
        regions: Option<&[Region]>,
        exe: Option<&Path>,
        addrs: &[u64],
    ) -> (Vec<String>, Vec<bool>) {
        let mut out: Vec<Vec<(String, bool)>> = Vec::with_capacity(addrs.len());
        for (i, &a) in addrs.iter().enumerate() {
            let a = if i == 0 { a } else { a.saturating_sub(1) };
            out.push(match regions.and_then(|r| maps::locate(r, a)) {
                Some((path, off)) => {
                    let in_exe = exe == Some(path.as_path());
                    let f = self.in_file(&path.clone(), off);
                    f.into_iter()
                        .map(|(n, src)| {
                            let own = own_frame(&n, src.as_deref(), in_exe);
                            (n, own)
                        })
                        .collect()
                }
                None => vec![(format!("[unknown {a:#x}]"), false)],
            });
        }
        out.into_iter().rev().flatten().unzip()
    }

    fn in_file(&mut self, path: &Path, off: u64) -> Vec<Frame> {
        if let Some(f) = self.user.get(&(path.to_path_buf(), off)) {
            return f.clone();
        }
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let unknown = || format!("[{file}+{off:#x}]");
        let src = Source::Elf(Elf::new(path));
        let f = match self.sym.symbolize(&src, Input::FileOffset(&[off][..])) {
            Ok(mut v) if !v.is_empty() => frames(v.remove(0), unknown),
            _ => vec![(unknown(), None)],
        };
        self.user.insert((path.to_path_buf(), off), f.clone());
        f
    }

    /// A kernel stack (leaf first) → frames root first.
    pub fn kernel(&mut self, addrs: &[u64]) -> Vec<String> {
        let mut out: Vec<Vec<String>> = Vec::with_capacity(addrs.len());
        for (i, &a) in addrs.iter().enumerate() {
            let a = if i == 0 { a } else { a.saturating_sub(1) };
            if let Some(f) = self.kernel.get(&a) {
                out.push(f.clone());
                continue;
            }
            let src = Source::Kernel(Kernel::default());
            let unknown = || format!("[k {a:#x}]");
            let f: Vec<String> = match self.sym.symbolize(&src, Input::AbsAddr(&[a][..])) {
                Ok(mut v) if !v.is_empty() => frames(v.remove(0), unknown).into_iter().map(|f| f.0).collect(),
                _ => vec![unknown()],
            };
            self.kernel.insert(a, f.clone());
            out.push(f);
        }
        out.into_iter().rev().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_come_off_rust_names_only() {
        assert_eq!(clean("tarae::editor::Editor::draw::h0123456789abcdef"), "tarae::editor::Editor::draw");
        assert_eq!(clean("tarae::hash::h01"), "tarae::hash::h01");
        assert_eq!(clean("__GI___poll"), "__GI___poll");
    }
}
