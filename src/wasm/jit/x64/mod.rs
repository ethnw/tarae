//! The x86-64 backend.

mod asm;
mod emit;

/// Baseline x86-64 plus SSE4.1 (`roundss` for ceil/floor/trunc/nearest) — every x86-64 CPU from 2008 on.
pub(super) fn supported() -> bool {
    std::arch::is_x86_feature_detected!("sse4.1")
}

/// Machine code for a module: the entry trampoline and every function, with their byte offsets.
/// `None` for a module whose code would be too big (it then runs on the interpreter).
pub(super) fn module(m: &super::super::module::Module) -> Option<super::Out> {
    let out = emit::module(m)?;
    Some(super::Out { code: out.code, entry: out.entry, funcs: out.funcs })
}
