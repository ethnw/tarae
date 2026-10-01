//! The AArch64 backend.

mod asm;
mod emit;

/// Every AArch64 CPU has what this backend uses.
pub(super) fn supported() -> bool {
    true
}

/// Machine code for a module: the entry trampoline and every function, with their byte offsets.
pub(super) fn module(m: &super::super::module::Module) -> Option<super::Out> {
    let out = emit::module(m)?;
    Some(super::Out {
        code: out.words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        entry: out.entry * 4,
        funcs: out.funcs.iter().map(|&w| w * 4).collect(),
    })
}
