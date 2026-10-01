//! The JIT: compiles the register ops (`compile.rs`) to machine code — AArch64 (`a64/`) or x86-64 (`x64/`) —
//! op by op, when a module loads. This is the one place in the runtime with `unsafe`: executable memory, and
//! calling into it.
//!
//! - **Frames** are the interpreter's: slots in the instance's value stack, a pinned register pointing at the
//!   current frame. Within a block values stay in registers (see the backends); memory is brought up to date
//!   at every branch, call and helper.
//! - **Context** (`Ctx`): what the code reads besides the frame — memory base and length, fuel, call depth,
//!   the value stack's end, globals — shared with the helpers.
//! - **Traps** branch to a stub that records the code and unwinds to the entry trampoline's saved stack
//!   pointer.
//! - **Rare ops** (tables, bulk memory, `memory.grow`) and `call_indirect`'s lookup call back into Rust
//!   (`exec::bulk`, `indirect_helper`), which then refresh the memory base and length.
//! - Every check the interpreter makes happens here too: memory accesses against the length, calls against
//!   the value stack and the depth, fuel at every function entry and loop head.
//! - **Executable memory** is never writable and executable at once: MAP_JIT's per-thread switch on Apple
//!   Silicon, read-write then read-execute elsewhere.

#![allow(unsafe_code)]

#[cfg(target_arch = "aarch64")]
mod a64;
#[cfg(target_arch = "aarch64")]
use a64 as arch;
#[cfg(target_arch = "x86_64")]
mod x64;
#[cfg(target_arch = "x86_64")]
use x64 as arch;

/// A backend's output: the entry trampoline and every function, as byte offsets into `code`.
pub(super) struct Out {
    pub code: Vec<u8>,
    pub entry: usize,
    pub funcs: Vec<usize>,
}

use super::compile::{Op, S};
use super::exec::{Bulk, bulk};
use super::module::Module;
use super::{Instance, NULL, Trap};

/// Shared between the running code and the helpers. `emit.rs` hard-codes these offsets.
#[repr(C)]
pub(super) struct Ctx {
    pub mem_base: *mut u8,
    pub mem_len: u64,
    pub fuel: u64,
    pub depth: u64,
    pub stack_end: *mut u64,
    pub globals: *mut u64,
    pub entry_sp: u64,
    pub trap: u64,
    pub inst: *mut Instance,
    pub funcs: *const usize,
    pub bulk: usize,
    pub indirect: usize,
}

#[allow(dead_code)] // the whole layout, whether or not this machine's backend names every field
mod off {
    pub const MEM: u32 = 0;
    pub const MEM_LEN: u32 = 8;
    pub const FUEL: u32 = 16;
    pub const DEPTH: u32 = 24;
    pub const STACK_END: u32 = 32;
    pub const GLOBALS: u32 = 40;
    pub const ENTRY_SP: u32 = 48;
    pub const TRAP: u32 = 56;
    pub const BULK: u32 = 80;
    pub const INDIRECT: u32 = 88;
}

/// The IR pcs an op may branch to.
pub(super) fn branch_targets(m: &Module, op: &Op, out: &mut Vec<u32>) {
    use Op::*;
    match *op {
        Jump(to) => out.push(to),
        JumpIf { to, .. }
        | JumpIfNot { to, .. }
        | JumpIf64 { to, .. }
        | JumpIfNot64 { to, .. }
        | JumpIfEq { to, .. }
        | JumpIfNe { to, .. }
        | JumpIfLtS { to, .. }
        | JumpIfLtU { to, .. }
        | JumpIfGtS { to, .. }
        | JumpIfGtU { to, .. }
        | JumpIfLeS { to, .. }
        | JumpIfLeU { to, .. }
        | JumpIfGeS { to, .. }
        | JumpIfGeU { to, .. }
        | JumpIfEqImm { to, .. }
        | JumpIfNeImm { to, .. }
        | JumpIfLtSImm { to, .. }
        | JumpIfLtUImm { to, .. }
        | JumpIfGtSImm { to, .. }
        | JumpIfGtUImm { to, .. }
        | JumpIfLeSImm { to, .. }
        | JumpIfLeUImm { to, .. }
        | JumpIfGeSImm { to, .. }
        | JumpIfGeUImm { to, .. } => out.push(to),
        BrTable { start, len, .. } => {
            out.extend(m.br_tables[start as usize..=(start + len) as usize].iter().map(|t| t.to))
        }
        _ => {}
    }
}

/// A function's locals by how much loops use them, busiest first (each level of loop nesting weighs 8×
/// more; locals used only outside loops aren't listed) — the ones worth a home register.
pub(super) fn busiest_loop_locals(m: &Module, start: usize, end: usize, locals: u32) -> Vec<S> {
    // Loop bodies are the spans a branch jumps back over.
    let mut depth = vec![0i32; end - start + 1];
    let mut to = Vec::new();
    for pc in start..end {
        to.clear();
        branch_targets(m, &m.code[pc], &mut to);
        for &t in &to {
            if (t as usize) <= pc {
                depth[t as usize - start] += 1;
                depth[pc - start + 1] -= 1;
            }
        }
    }
    let mut weight = vec![0u64; locals as usize];
    let mut nesting = 0;
    for pc in start..end {
        nesting += depth[pc - start];
        if nesting > 0 {
            let w = 8u64.pow(nesting.min(5) as u32);
            m.code[pc].slots(&mut |s| {
                if let Some(x) = weight.get_mut(s as usize) {
                    *x += w;
                }
            });
        }
    }
    let mut order: Vec<usize> = (0..weight.len()).filter(|&s| weight[s] > 0).collect();
    order.sort_unstable_by_key(|&s| (std::cmp::Reverse(weight[s]), s));
    order.into_iter().map(|s| s as S).collect()
}

/// Trap codes the code hands back (0 = returned normally).
pub(super) fn trap_code(t: Trap) -> u32 {
    t as u32 + 1
}

fn trap_of(code: u64) -> Trap {
    const ALL: [Trap; 12] = [
        Trap::Unreachable,
        Trap::MemoryOutOfBounds,
        Trap::TableOutOfBounds,
        Trap::UndefinedElement,
        Trap::UninitializedElement,
        Trap::IndirectCallType,
        Trap::DivideByZero,
        Trap::IntegerOverflow,
        Trap::InvalidConversion,
        Trap::StackOverflow,
        Trap::OutOfFuel,
        Trap::BadCall,
    ];
    debug_assert!(ALL.iter().enumerate().all(|(i, &t)| trap_code(t) == i as u32 + 1));
    ALL.get((code as usize).wrapping_sub(1)).copied().unwrap_or(Trap::Unreachable)
}

/// Executable memory holding one module's code.
struct ExecMem {
    ptr: *mut u8,
    len: usize,
}

// The code is immutable once written; sharing it across threads is as safe as sharing a `&[u8]`.
unsafe impl Send for ExecMem {}
unsafe impl Sync for ExecMem {}

impl ExecMem {
    fn new(code: &[u8]) -> Option<ExecMem> {
        let page = 16384;
        let len = code.len().div_ceil(page).max(1) * page;
        // Apple Silicon: a MAP_JIT mapping, made writable for this thread while we copy. Elsewhere: map it
        // read-write, copy, then flip it to read-execute. Never writable and executable at once.
        const APPLE_ARM: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));
        // SAFETY: a fresh anonymous mapping; we write `code` into it and then only execute it.
        unsafe {
            let (prot, flags) = if APPLE_ARM {
                (
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANON | map_jit(),
                )
            } else {
                (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON)
            };
            let ptr = libc::mmap(std::ptr::null_mut(), len, prot, flags, -1, 0);
            if ptr == libc::MAP_FAILED {
                return None;
            }
            let ptr = ptr as *mut u8;
            jit_write_protect(false);
            std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
            jit_write_protect(true);
            if !APPLE_ARM
                && libc::mprotect(ptr as *mut libc::c_void, len, libc::PROT_READ | libc::PROT_EXEC) != 0
            {
                libc::munmap(ptr as *mut libc::c_void, len);
                return None;
            }
            flush_icache(ptr, code.len());
            Some(ExecMem { ptr, len })
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn map_jit() -> libc::c_int {
    libc::MAP_JIT
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn map_jit() -> libc::c_int {
    0
}

/// Apple Silicon's per-thread switch between writing and executing MAP_JIT memory.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe fn jit_write_protect(on: bool) {
    // SAFETY: only affects this thread's view of MAP_JIT pages.
    unsafe { libc::pthread_jit_write_protect_np(on as libc::c_int) }
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
unsafe fn jit_write_protect(_on: bool) {}

/// AArch64 doesn't keep instruction and data caches coherent; x86 does.
#[cfg(target_arch = "aarch64")]
unsafe fn flush_icache(ptr: *mut u8, len: usize) {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
        }
        sys_icache_invalidate(ptr as *mut libc::c_void, len);
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        unsafe extern "C" {
            fn __clear_cache(start: *mut libc::c_char, end: *mut libc::c_char);
        }
        __clear_cache(ptr as *mut libc::c_char, ptr.add(len) as *mut libc::c_char);
    }
}
#[cfg(not(target_arch = "aarch64"))]
unsafe fn flush_icache(_ptr: *mut u8, _len: usize) {}

impl Drop for ExecMem {
    fn drop(&mut self) {
        // SAFETY: our own mapping; nothing runs in it once its module is gone.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// A module's machine code.
pub(super) struct Jit {
    /// Owned for its lifetime: dropping it unmaps the code.
    #[allow(dead_code)]
    mem: ExecMem,
    /// The entry trampoline: `fn(ctx, frame, target) -> trap code`.
    entry: usize,
    /// Each function's address.
    funcs: Vec<usize>,
}

impl Jit {
    /// Compiles a validated module. `None` when this machine can't (no backend, no executable memory, or code
    /// too big for the backend's branches).
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    pub fn new(m: &Module) -> Option<Jit> {
        if !arch::supported() {
            return None;
        }
        let out = arch::module(m)?;
        let mem = ExecMem::new(&out.code)?;
        let base = mem.ptr as usize;
        Some(Jit { entry: base + out.entry, funcs: out.funcs.iter().map(|&b| base + b).collect(), mem })
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub fn new(_m: &Module) -> Option<Jit> {
        None
    }
}

/// Runs function `f` of `inst` with its arguments in the value stack's first slots.
pub(super) fn run(inst: &mut Instance, f: u32) -> Result<(), Trap> {
    let jit = inst.module.jit.as_ref().expect("compiled");
    let (entry, target, funcs) = (jit.entry, jit.funcs[f as usize], jit.funcs.as_ptr());
    let stack = inst.stack.as_mut_ptr();
    let mut ctx = Ctx {
        mem_base: inst.memory.as_mut_ptr(),
        mem_len: inst.memory.len() as u64,
        fuel: inst.fuel,
        depth: inst.limits.call_depth as u64,
        // SAFETY: one past the end of the value stack.
        stack_end: unsafe { stack.add(inst.stack.len()) },
        globals: inst.globals.as_mut_ptr(),
        entry_sp: 0,
        trap: 0,
        inst,
        funcs,
        bulk: bulk_helper as *const () as usize,
        indirect: indirect_helper as *const () as usize,
    };
    // SAFETY: `entry` is the trampoline `emit::module` generated with this signature; the code only touches
    // what `ctx` points to, within the bounds it checks.
    let code = unsafe {
        let entry: extern "C" fn(*mut Ctx, *mut u64, usize) -> u64 = std::mem::transmute(entry);
        entry(&mut ctx, stack, target)
    };
    // SAFETY: the code has returned; `ctx.inst` is `inst` again.
    let inst = unsafe { &mut *ctx.inst };
    inst.fuel = ctx.fuel;
    if code == 0 { Ok(()) } else { Err(trap_of(code)) }
}

/// Called by the code for a rare op (at `pc` in the module's ops) on the frame at `fr`.
extern "C" fn bulk_helper(ctx: *mut Ctx, fr: *mut u64, pc: u32) -> u64 {
    // SAFETY: called only from running code, with the context `run` made and the current frame.
    let ctx = unsafe { &mut *ctx };
    let inst = unsafe { &mut *ctx.inst };
    let slots = (ctx.stack_end as usize - fr as usize) / 8;
    let fr = unsafe { std::slice::from_raw_parts_mut(fr, slots) };
    let op: Op = inst.module.code[pc as usize];
    let env = Bulk {
        m: &inst.module,
        mem: &mut inst.memory,
        memory_max: inst.memory_max,
        tables: &mut inst.tables,
        table_max: &inst.table_max,
        elems: &mut inst.elems,
        data_live: &mut inst.data_live,
        // The code stored its fuel register here before the call, and reloads it after.
        fuel: &mut ctx.fuel,
    };
    let r = bulk(op, fr, env);
    ctx.mem_base = inst.memory.as_mut_ptr();
    ctx.mem_len = inst.memory.len() as u64;
    match r {
        Ok(()) => 0,
        Err(t) => trap_code(t) as u64,
    }
}

/// Resolves a `call_indirect`: the target's address, or 0 with the trap recorded.
extern "C" fn indirect_helper(ctx: *mut Ctx, table: u32, i: u32, ty: u32) -> usize {
    // SAFETY: as in `bulk_helper`.
    let ctx = unsafe { &mut *ctx };
    let inst = unsafe { &*ctx.inst };
    let m = &inst.module;
    let trap = match inst.tables[table as usize].get(i as usize) {
        None => Trap::UndefinedElement,
        Some(&NULL) => Trap::UninitializedElement,
        Some(&r) if m.canon[m.func_ty[r as usize - 1] as usize] != ty => Trap::IndirectCallType,
        // SAFETY: `funcs` has an entry per function, and `r - 1` is one (tables hold function refs).
        Some(&r) => return unsafe { *ctx.funcs.add(r as usize - 1) },
    };
    ctx.trap = trap_code(trap) as u64;
    0
}
