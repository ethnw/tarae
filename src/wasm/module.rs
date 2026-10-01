//! Decoding a module: the sections, checked as they're read, with function bodies compiled (`compile.rs`)
//! when the code section arrives. Imports are refused — a plugin gets nothing from the host but its input.

use std::collections::HashSet;

use super::compile::{self, Code, Op, Target};
use super::read::{R, Reader};
use super::{Error, FuncType, Ty};

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Table {
    pub elem: Ty,
    pub limits: Limits,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Const {
    Bits(u64),
    Global(u32),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Global {
    pub ty: Ty,
    pub mutable: bool,
    pub init: Const,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

#[derive(Clone, Debug)]
pub(super) struct Export {
    pub name: String,
    pub kind: ExportKind,
    pub index: u32,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Mode {
    Passive,
    Active { index: u32, offset: Const },
    Declarative,
}

#[derive(Clone, Debug)]
pub(super) struct Elem {
    pub ty: Ty,
    pub mode: Mode,
    pub items: Vec<Const>,
}

#[derive(Clone, Debug)]
pub(super) struct Data {
    pub mode: Mode,
    pub bytes: Vec<u8>,
}

/// A decoded, validated, compiled module — ready to instantiate.
#[derive(Default)]
pub struct Module {
    pub(super) types: Vec<FuncType>,
    /// Per type index, the first index with an equal signature (what `call_indirect` compares).
    pub(super) canon: Vec<u32>,
    /// Per function, its type index.
    pub(super) func_ty: Vec<u32>,
    pub(super) funcs: Vec<Code>,
    pub(super) tables: Vec<Table>,
    pub(super) memory: Option<Limits>,
    pub(super) globals: Vec<Global>,
    pub(super) exports: Vec<Export>,
    pub(super) start: Option<u32>,
    pub(super) elems: Vec<Elem>,
    pub(super) data_count: Option<u32>,
    pub(super) datas: Vec<Data>,
    pub(super) code: Vec<Op>,
    pub(super) br_tables: Vec<Target>,
    /// Per local of every function (from `Code::floats`): whether it's an f32 or f64.
    pub(super) floats: Vec<bool>,
    /// Functions a body may take a reference to (`ref.func`): those in element segments, exports, and
    /// global initializers.
    pub(super) declared: Vec<bool>,
    /// Machine code for the functions, where there's a JIT for this machine.
    pub(super) jit: Option<super::jit::Jit>,
    /// Names of the custom sections (what built the module leaves there — wit-bindgen, for one).
    pub(super) customs: Vec<String>,
}

/// Params (or results) of a function type — the Web's limit.
const MAX_ARITY: usize = 1000;
/// Tables in a module (each is capped by `Limits::table_elems`).
const MAX_TABLES: usize = 100;

/// Section ids in the order they must appear (custom sections go anywhere).
fn rank(id: u8) -> R<u8> {
    Ok(match id {
        1..=9 => id,
        12 => 10,
        10 => 11,
        11 => 12,
        13 => return Err(Error::new("exceptions (tag section) are not supported")),
        _ => return Err(Error::new(format!("unknown section {id}"))),
    })
}

fn limits(r: &mut Reader, cap: u32) -> R<Limits> {
    let flags = r.u8()?;
    let min = r.u32()?;
    let max = match flags {
        0 => None,
        1 => Some(r.u32()?),
        2 | 3 => return Err(Error::new("shared memory is not supported")),
        _ => return Err(Error::new("64-bit memories and tables are not supported")),
    };
    if min > cap || max.is_some_and(|m| m > cap) {
        return Err(Error::new("size limit too large"));
    }
    if max.is_some_and(|m| m < min) {
        return Err(Error::new("size minimum must not be greater than maximum"));
    }
    Ok(Limits { min, max })
}

impl Module {
    pub fn new(bytes: &[u8]) -> Result<Module, Error> {
        let mut r = Reader::new(bytes);
        if r.bytes(4).ok() != Some(b"\0asm") {
            return Err(Error::new("not a WebAssembly module"));
        }
        if r.bytes(4)? != [1, 0, 0, 0] {
            return Err(Error::new("unsupported WebAssembly version (components aren't modules)"));
        }
        let mut m = Module::default();
        let mut last = 0;
        let mut code_read = false;
        while !r.done() {
            let id = r.u8()?;
            let len = r.u32()? as usize;
            let mut s = r.sub(len)?;
            if id == 0 {
                m.customs.push(s.name()?.to_string());
                continue;
            }
            let rank = rank(id)?;
            if rank <= last {
                return Err(Error::new("unexpected section order"));
            }
            last = rank;
            match id {
                1 => m.types(&mut s)?,
                2 => {
                    if s.count()? > 0 {
                        return Err(Error::new(
                            "the module imports something — plugins get no imports (build for wasm32-unknown-unknown)",
                        ));
                    }
                }
                3 => {
                    for _ in 0..s.count()? {
                        let t = s.u32()?;
                        if t as usize >= m.types.len() {
                            return Err(Error::new("unknown type"));
                        }
                        m.func_ty.push(t);
                    }
                }
                4 => {
                    for _ in 0..s.count()? {
                        let b = s.u8()?;
                        if b == 0x40 {
                            return Err(Error::new("table initializers are not supported"));
                        }
                        let elem = compile::ref_type(b)?;
                        m.tables.push(Table { elem, limits: limits(&mut s, u32::MAX)? });
                        if m.tables.len() > MAX_TABLES {
                            return Err(Error::new("too many tables"));
                        }
                    }
                }
                5 => {
                    for _ in 0..s.count()? {
                        if m.memory.is_some() {
                            return Err(Error::new("multiple memories are not supported"));
                        }
                        m.memory = Some(limits(&mut s, 65536)?);
                    }
                }
                6 => {
                    for _ in 0..s.count()? {
                        let ty = compile::val_type(s.u8()?)?;
                        let mutable = match s.u8()? {
                            0 => false,
                            1 => true,
                            _ => return Err(Error::new("malformed mutability")),
                        };
                        let init = m.const_expr(&mut s, ty)?;
                        m.globals.push(Global { ty, mutable, init });
                    }
                }
                7 => m.exports(&mut s)?,
                8 => {
                    let f = s.u32()?;
                    let t = m.func_ty.get(f as usize).ok_or_else(|| Error::new("unknown function"))?;
                    let t = &m.types[*t as usize];
                    if !t.params.is_empty() || !t.results.is_empty() {
                        return Err(Error::new("start function must take and return nothing"));
                    }
                    m.start = Some(f);
                }
                9 => m.elems(&mut s)?,
                12 => m.data_count = Some(s.u32()?),
                10 => {
                    m.code_section(&mut s)?;
                    code_read = true;
                }
                11 => m.datas(&mut s)?,
                _ => unreachable!("rank() rejects other ids"),
            }
            if !s.done() {
                return Err(Error::new("section size mismatch"));
            }
        }
        if !code_read && !m.func_ty.is_empty() {
            return Err(Error::new("function and code section have inconsistent lengths"));
        }
        if m.data_count.is_some_and(|n| n as usize != m.datas.len()) {
            return Err(Error::new("data count and data section have inconsistent lengths"));
        }
        m.jit = super::jit::Jit::new(&m);
        Ok(m)
    }

    fn declare(&mut self, f: u32) {
        let f = f as usize;
        if self.declared.len() <= f {
            self.declared.resize(f + 1, false);
        }
        self.declared[f] = true;
    }

    /// An exported function's index.
    pub fn func(&self, name: &str) -> Option<u32> {
        self.export(name, ExportKind::Func)
    }

    pub fn export(&self, name: &str, kind: ExportKind) -> Option<u32> {
        self.exports.iter().find(|e| e.name == name && e.kind == kind).map(|e| e.index)
    }

    /// Function `f`'s signature (`f` from `func`, or any index below the function count).
    pub fn func_type(&self, f: u32) -> &FuncType {
        &self.types[self.func_ty[f as usize] as usize]
    }

    /// Whether it runs as machine code (else on the interpreter).
    pub fn compiled(&self) -> bool {
        self.jit.is_some()
    }

    /// Names of the module's custom sections, in order.
    pub fn customs(&self) -> impl Iterator<Item = &str> {
        self.customs.iter().map(String::as_str)
    }

    fn types(&mut self, s: &mut Reader) -> R<()> {
        let mut seen = std::collections::HashMap::new();
        for _ in 0..s.count()? {
            if s.u8()? != 0x60 {
                return Err(Error::new("only function types are supported"));
            }
            let vec = |s: &mut Reader| -> R<Box<[Ty]>> {
                let n = s.count()?;
                if n > MAX_ARITY {
                    return Err(Error::new("too many params or results"));
                }
                (0..n).map(|_| compile::val_type(s.u8()?)).collect()
            };
            let t = FuncType { params: vec(s)?, results: vec(s)? };
            let canon = *seen.entry(t.clone()).or_insert(self.types.len() as u32);
            self.canon.push(canon);
            self.types.push(t);
        }
        Ok(())
    }

    /// A constant expression: one instruction, then `end`.
    fn const_expr(&mut self, s: &mut Reader, want: Ty) -> R<Const> {
        let (c, ty) = match s.u8()? {
            0x41 => (Const::Bits(s.s32()? as u32 as u64), Ty::I32),
            0x42 => (Const::Bits(s.s64()? as u64), Ty::I64),
            0x43 => (Const::Bits(s.f32()? as u64), Ty::F32),
            0x44 => (Const::Bits(s.f64()?), Ty::F64),
            0x23 => {
                let g = s.u32()?;
                let global = self.globals.get(g as usize).ok_or_else(|| Error::new("unknown global"))?;
                if global.mutable {
                    return Err(Error::new("constant expression required"));
                }
                (Const::Global(g), global.ty)
            }
            0xD0 => (Const::Bits(super::NULL), compile::ref_type(s.u8()?)?),
            0xD2 => {
                let f = s.u32()?;
                if f as usize >= self.func_ty.len() {
                    return Err(Error::new("unknown function"));
                }
                self.declare(f);
                (Const::Bits(f as u64 + 1), Ty::FuncRef)
            }
            _ => return Err(Error::new("constant expression required")),
        };
        match s.u8()? {
            0x0B => {}
            0x41..=0x44 | 0x23 | 0x6A..=0x6C | 0x7C..=0x7E => {
                return Err(Error::new("extended constant expressions are not supported"));
            }
            _ => return Err(Error::new("constant expression required")),
        }
        if ty != want {
            return Err(Error::new("type mismatch"));
        }
        Ok(c)
    }

    fn exports(&mut self, s: &mut Reader) -> R<()> {
        let mut seen = HashSet::new();
        for _ in 0..s.count()? {
            let name = s.name()?.to_string();
            let kind = match s.u8()? {
                0 => ExportKind::Func,
                1 => ExportKind::Table,
                2 => ExportKind::Memory,
                3 => ExportKind::Global,
                _ => return Err(Error::new("malformed export kind")),
            };
            let index = s.u32()?;
            let count = match kind {
                ExportKind::Func => self.func_ty.len(),
                ExportKind::Table => self.tables.len(),
                ExportKind::Memory => self.memory.iter().len(),
                ExportKind::Global => self.globals.len(),
            };
            if index as usize >= count {
                return Err(Error::new("unknown export target"));
            }
            if !seen.insert(name.clone()) {
                return Err(Error::new("duplicate export name"));
            }
            if kind == ExportKind::Func {
                self.declare(index);
            }
            self.exports.push(Export { name, kind, index });
        }
        Ok(())
    }

    fn table_of(&self, t: u32) -> R<Ty> {
        self.tables.get(t as usize).map(|t| t.elem).ok_or_else(|| Error::new("unknown table"))
    }

    fn elems(&mut self, s: &mut Reader) -> R<()> {
        for _ in 0..s.count()? {
            let flags = s.u32()?;
            if flags > 7 {
                return Err(Error::new("malformed elements segment kind"));
            }
            let active = flags & 1 == 0;
            let explicit_table = flags & 2 != 0;
            let exprs = flags & 4 != 0;
            let mode = if active {
                let index = if explicit_table { s.u32()? } else { 0 };
                self.table_of(index)?;
                Mode::Active { index, offset: self.const_expr(s, Ty::I32)? }
            } else if explicit_table {
                Mode::Declarative
            } else {
                Mode::Passive
            };
            // Flags 0 and 4 name no element type (funcref); the others do.
            let ty = match (flags, exprs) {
                (0 | 4, _) => Ty::FuncRef,
                (_, false) => match s.u8()? {
                    0x00 => Ty::FuncRef,
                    _ => return Err(Error::new("malformed element kind")),
                },
                (_, true) => compile::ref_type(s.u8()?)?,
            };
            let mut items = Vec::new();
            for _ in 0..s.count()? {
                items.push(if exprs {
                    self.const_expr(s, ty)?
                } else {
                    let f = s.u32()?;
                    if f as usize >= self.func_ty.len() {
                        return Err(Error::new("unknown function"));
                    }
                    self.declare(f);
                    Const::Bits(f as u64 + 1)
                });
            }
            if let Mode::Active { index, .. } = mode
                && self.table_of(index)? != ty
            {
                return Err(Error::new("type mismatch"));
            }
            self.elems.push(Elem { ty, mode, items });
        }
        Ok(())
    }

    fn code_section(&mut self, s: &mut Reader) -> R<()> {
        let n = s.count()?;
        if n != self.func_ty.len() {
            return Err(Error::new("function and code section have inconsistent lengths"));
        }
        let mut code = std::mem::take(&mut self.code);
        let mut targets = std::mem::take(&mut self.br_tables);
        let mut floats = std::mem::take(&mut self.floats);
        let mut work = 0;
        for f in 0..n {
            let len = s.u32()? as usize;
            let body = s.sub(len)?;
            let c = compile::function(
                self,
                self.func_ty[f],
                body,
                &mut code,
                &mut targets,
                &mut floats,
                &mut work,
            )?;
            self.funcs.push(c);
        }
        self.code = code;
        self.br_tables = targets;
        self.floats = floats;
        Ok(())
    }

    fn datas(&mut self, s: &mut Reader) -> R<()> {
        for _ in 0..s.count()? {
            let mode = match s.u32()? {
                0 => Mode::Active { index: 0, offset: self.const_expr(s, Ty::I32)? },
                1 => Mode::Passive,
                2 => {
                    let index = s.u32()?;
                    Mode::Active { index, offset: self.const_expr(s, Ty::I32)? }
                }
                _ => return Err(Error::new("malformed data segment kind")),
            };
            if let Mode::Active { index, .. } = mode
                && (index != 0 || self.memory.is_none())
            {
                return Err(Error::new("unknown memory"));
            }
            let len = s.u32()? as usize;
            let bytes = s.bytes(len)?.to_vec();
            self.datas.push(Data { mode, bytes });
        }
        Ok(())
    }
}

impl Module {
    /// The compiled ops of function `f`, one per line (for looking at what the compiler made).
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    #[allow(dead_code)] // for debugging
    pub fn disassemble(&self, f: u32) -> String {
        let c = &self.funcs[f as usize];
        let end = self.funcs.get(f as usize + 1).map_or(self.code.len(), |n| n.start as usize);
        let mut out = format!("func {f}: params {} locals {} frame {}\n", c.params, c.locals, c.frame);
        for pc in c.start as usize..end {
            out += &format!("{pc:6}  {:?}\n", self.code[pc]);
        }
        out
    }
}
