//! tarae's WebAssembly runtime, for plugins.
//!
//! - **Scope**: WebAssembly 1.0 plus what rustc turns on by default for `wasm32-unknown-unknown` — bulk memory,
//!   multi-value, mutable globals, non-trapping float → int, reference types, sign extension. No SIMD,
//!   threads, exceptions, tail calls, 64-bit or multiple memories, and **no imports**: a plugin reads what
//!   tarae puts in its memory and answers there — no WASI, no host functions.
//! - **Shape**: `module.rs` decodes, `compile.rs` validates each function while compiling it to register ops
//!   (operands in fixed frame slots, branch targets and moves resolved). `jit/` turns those into machine code
//!   (AArch64, x86-64) where it can; `exec.rs` interprets them everywhere else — and is the reference the
//!   JIT is fuzzed against (`fuzz/`).
//! - **Safety**: `unsafe` only in `jit/mod.rs` (executable memory, and calling into it). Every malformed input
//!   is an `Error`, every fault a `Trap`.
//! - **Limits**: fuel is charged at every function entry and loop head for what the body costs (moves by
//!   their length, calls by the locals they zero), and per byte (or element) by the ops that copy, fill or
//!   grow — so every way to run long pays. Memory, tables, the value stack, and call depth have caps, and so
//!   does loading (types' arity, locals per module, tables, compiling steps).

#![deny(unsafe_code)] // allowed only in `jit`

mod compile;
mod exec;
mod jit;
mod module;
mod read;

use std::fmt;

pub use module::Module;

use module::{Const, ExportKind, Mode};

/// The null reference in a slot. A reference to function `f` is `f + 1` — so zeroed locals, tables and
/// globals start out null.
pub(crate) const NULL: u64 = 0;
const PAGE: usize = 65536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    I32,
    I64,
    F32,
    F64,
    FuncRef,
    ExternRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FuncType {
    pub params: Box<[Ty]>,
    pub results: Box<[Ty]>,
}

/// A value going in or out of a call. Floats travel as their bits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Val {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    /// A function index, or null.
    FuncRef(Option<u32>),
    ExternRef(Option<u32>),
}

impl Val {
    fn ty(self) -> Ty {
        match self {
            Val::I32(_) => Ty::I32,
            Val::I64(_) => Ty::I64,
            Val::F32(_) => Ty::F32,
            Val::F64(_) => Ty::F64,
            Val::FuncRef(_) => Ty::FuncRef,
            Val::ExternRef(_) => Ty::ExternRef,
        }
    }

    fn to_slot(self) -> u64 {
        match self {
            Val::I32(v) => v as u32 as u64,
            Val::I64(v) => v as u64,
            Val::F32(v) => v as u64,
            Val::F64(v) => v,
            Val::FuncRef(r) | Val::ExternRef(r) => r.map_or(NULL, |i| i as u64 + 1),
        }
    }

    fn from_slot(ty: Ty, s: u64) -> Val {
        let r = (s != NULL).then(|| (s - 1) as u32);
        match ty {
            Ty::I32 => Val::I32(s as u32 as i32),
            Ty::I64 => Val::I64(s as i64),
            Ty::F32 => Val::F32(s as u32),
            Ty::F64 => Val::F64(s),
            Ty::FuncRef => Val::FuncRef(r),
            Ty::ExternRef => Val::ExternRef(r),
        }
    }
}

/// A module that can't be used: malformed, invalid, or using something unsupported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    fn new(s: impl Into<String>) -> Error {
        Error(s.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    Unreachable,
    MemoryOutOfBounds,
    TableOutOfBounds,
    /// `call_indirect` past the table's end.
    UndefinedElement,
    UninitializedElement,
    IndirectCallType,
    DivideByZero,
    IntegerOverflow,
    InvalidConversion,
    StackOverflow,
    OutOfFuel,
    /// The host called with the wrong arguments, or something that isn't a function.
    BadCall,
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Trap::Unreachable => "unreachable",
            Trap::MemoryOutOfBounds => "out of bounds memory access",
            Trap::TableOutOfBounds => "out of bounds table access",
            Trap::UndefinedElement => "undefined element",
            Trap::UninitializedElement => "uninitialized element",
            Trap::IndirectCallType => "indirect call type mismatch",
            Trap::DivideByZero => "integer divide by zero",
            Trap::IntegerOverflow => "integer overflow",
            Trap::InvalidConversion => "invalid conversion to integer",
            Trap::StackOverflow => "call stack exhausted",
            Trap::OutOfFuel => "out of fuel",
            Trap::BadCall => "wrong arguments",
        })
    }
}

impl std::error::Error for Trap {}

/// Caps on what an instance may use.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub table_elems: u32,
    /// Value stack slots (8 bytes each), shared by every frame.
    pub stack_slots: usize,
    /// Calls deep. Compiled code nests on the calling thread's native stack, up to 96 bytes a call
    /// (AArch64; 32 on x86-64): the thread needs `call_depth` × 96 bytes to spare, 1.9 MB for the default.
    pub call_depth: usize,
    /// Run compiled machine code when there is some (else the interpreter).
    pub jit: bool,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            memory_bytes: 256 << 20,
            table_elems: 100_000,
            stack_slots: 1 << 19,
            call_depth: 20_000,
            jit: true,
        }
    }
}

#[derive(Clone, Copy)]
struct Frame {
    ret: u32,
    base: u32,
}

/// A running module: its memory, tables, and globals.
pub struct Instance {
    module: Module,
    limits: Limits,
    memory: Vec<u8>,
    memory_max: usize,
    globals: Vec<u64>,
    tables: Vec<Vec<u64>>,
    table_max: Vec<u32>,
    /// Element segments as evaluated references — emptied when dropped.
    elems: Vec<Vec<u64>>,
    data_live: Vec<bool>,
    stack: Vec<u64>,
    frames: Vec<Frame>,
    fuel: u64,
}

fn eval(c: Const, globals: &[u64]) -> u64 {
    match c {
        Const::Bits(b) => b,
        Const::Global(g) => globals[g as usize],
    }
}

impl Instance {
    /// Sets up memory, tables, and globals, applies the active segments, and runs the start function
    /// (with `fuel`).
    pub fn new(module: Module, limits: Limits, fuel: u64) -> Result<Instance, Trap> {
        let mut globals = Vec::with_capacity(module.globals.len());
        for g in &module.globals {
            globals.push(eval(g.init, &globals));
        }
        let pages_cap = limits.memory_bytes / PAGE;
        let (memory, memory_max) = match module.memory {
            None => (Vec::new(), 0),
            Some(l) => {
                if l.min as usize > pages_cap {
                    return Err(Trap::MemoryOutOfBounds);
                }
                let max = l.max.map_or(pages_cap, |m| (m as usize).min(pages_cap));
                (vec![0; l.min as usize * PAGE], max * PAGE)
            }
        };
        let mut tables = Vec::new();
        let mut table_max = Vec::new();
        for t in &module.tables {
            if t.limits.min > limits.table_elems {
                return Err(Trap::TableOutOfBounds);
            }
            tables.push(vec![NULL; t.limits.min as usize]);
            table_max.push(t.limits.max.unwrap_or(u32::MAX).min(limits.table_elems));
        }
        let elems =
            module.elems.iter().map(|e| e.items.iter().map(|&c| eval(c, &globals)).collect()).collect();
        let data_live = vec![true; module.datas.len()];
        let mut inst = Instance {
            stack: vec![0; limits.stack_slots],
            module,
            limits,
            memory,
            memory_max,
            globals,
            tables,
            table_max,
            elems,
            data_live,
            frames: Vec::new(),
            fuel,
        };
        for i in 0..inst.module.elems.len() {
            let e = &inst.module.elems[i];
            if let Mode::Active { index, offset } = e.mode {
                let at = eval(offset, &inst.globals) as u32 as usize;
                let items = std::mem::take(&mut inst.elems[i]);
                let table = &mut inst.tables[index as usize];
                let dst = table.get_mut(at..at + items.len()).ok_or(Trap::TableOutOfBounds)?;
                dst.copy_from_slice(&items);
            } else if let Mode::Declarative = e.mode {
                inst.elems[i].clear();
            }
        }
        for i in 0..inst.module.datas.len() {
            let d = &inst.module.datas[i];
            if let Mode::Active { offset, .. } = d.mode {
                let at = eval(offset, &inst.globals) as u32 as usize;
                let dst = inst.memory.get_mut(at..at + d.bytes.len()).ok_or(Trap::MemoryOutOfBounds)?;
                dst.copy_from_slice(&d.bytes);
                inst.data_live[i] = false;
            }
        }
        if let Some(f) = inst.module.start {
            inst.call(f, &[])?;
        }
        Ok(inst)
    }

    pub fn module(&self) -> &Module {
        &self.module
    }

    #[cfg_attr(not(test), allow(dead_code))] // for fuzz/ and the tests
    pub fn fuel(&self) -> u64 {
        self.fuel
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.fuel = fuel;
    }

    pub fn memory(&self) -> &[u8] {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut [u8] {
        &mut self.memory
    }

    /// An exported global's value.
    #[allow(dead_code)] // for fuzz/ (the spec tests read globals)
    pub fn global(&self, name: &str) -> Option<Val> {
        let g = self.module.export(name, ExportKind::Global)? as usize;
        Some(Val::from_slot(self.module.globals[g].ty, self.globals[g]))
    }

    /// Calls function `f` (an index — `Module::func` finds exported ones by name).
    pub fn call(&mut self, f: u32, args: &[Val]) -> Result<Vec<Val>, Trap> {
        let Some(&ty) = self.module.func_ty.get(f as usize) else { return Err(Trap::BadCall) };
        let ty = &self.module.types[ty as usize];
        if ty.params.len() != args.len() || ty.params.iter().zip(args).any(|(&t, a)| a.ty() != t) {
            return Err(Trap::BadCall);
        }
        // A function reference must name a function of this module: code trusts the ones in tables
        let funcs = self.module.func_ty.len();
        if args.iter().any(|a| matches!(a, Val::FuncRef(Some(f)) if *f as usize >= funcs)) {
            return Err(Trap::BadCall);
        }
        if args.len() > self.stack.len() {
            return Err(Trap::StackOverflow);
        }
        for (slot, a) in self.stack.iter_mut().zip(args) {
            *slot = a.to_slot();
        }
        let results = ty.results.clone();
        if self.limits.jit && self.module.jit.is_some() {
            jit::run(self, f)?;
        } else {
            self.run(f)?;
        }
        Ok(results.iter().zip(&self.stack).map(|(&t, &s)| Val::from_slot(t, s)).collect())
    }
}

#[cfg(test)]
mod tests;
