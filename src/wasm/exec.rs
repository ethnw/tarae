//! The interpreter: one loop over register ops. All frames share one `u64` slot stack — a frame is the
//! callee's params, then its locals, then its operand slots — and ops index the current frame directly. The
//! call stack is explicit (no Rust recursion), so deep wasm recursion is a `StackOverflow` trap rather than
//! a crashed thread.

use super::compile::Op;
use super::module::Module;
use super::{Frame, Instance, NULL, PAGE, Trap};

/// Reads a slot as a type.
macro_rules! get {
    (u32, $v:expr) => {
        $v as u32
    };
    (i32, $v:expr) => {
        $v as u32 as i32
    };
    (u64, $v:expr) => {
        $v
    };
    (i64, $v:expr) => {
        $v as i64
    };
    (f32, $v:expr) => {
        f32::from_bits($v as u32)
    };
    (f64, $v:expr) => {
        f64::from_bits($v)
    };
}

/// An immediate (a sign-extended i32) as a type.
macro_rules! imm {
    (u32, $v:expr) => {
        $v as u32
    };
    (i32, $v:expr) => {
        $v
    };
    (u64, $v:expr) => {
        $v as i64 as u64
    };
    (i64, $v:expr) => {
        $v as i64
    };
}

/// A value as slot bits.
macro_rules! put {
    (u32, $v:expr) => {
        $v as u64
    };
    (i32, $v:expr) => {
        $v as u32 as u64
    };
    (u64, $v:expr) => {
        $v
    };
    (i64, $v:expr) => {
        $v as u64
    };
    (f32, $v:expr) => {
        ($v).to_bits() as u64
    };
    (f64, $v:expr) => {
        ($v).to_bits()
    };
    (bool, $v:expr) => {
        ($v) as u64
    };
}

/// The main `match`: the control arms as given, then an arm per arithmetic op from its one-line definition
/// (a binary op with an immediate form gets that arm too).
macro_rules! dispatch {
    (
        $op:expr, $fr:ident;
        control { $($control:tt)* }
        unary { $($un:ident: $uf:ident -> $ut:ident, |$ux:ident| $ue:expr;)* }
        binary { $($bn:ident $(/ $bi:ident)?: $bf:ident -> $bt:ident, |$bx:ident, $by:ident| $be:expr;)* }
    ) => {
        match $op {
            $($control)*
            $(Op::$un { dst, a } => {
                let $ux = get!($uf, $fr[a as usize]);
                $fr[dst as usize] = put!($ut, $ue);
            })*
            $(
                Op::$bn { dst, a, b } => {
                    let $bx = get!($bf, $fr[a as usize]);
                    let $by = get!($bf, $fr[b as usize]);
                    $fr[dst as usize] = put!($bt, $be);
                }
                $(Op::$bi { dst, a, imm } => {
                    let $bx = get!($bf, $fr[a as usize]);
                    let $by = imm!($bf, imm);
                    $fr[dst as usize] = put!($bt, $be);
                })?
            )*
        }
    };
}

/// A NaN result made quiet. Where the rounding ops aren't one instruction (x86-64 without SSE4.1 — Linux's
/// baseline — calls libm), a signaling NaN comes back as it went in, but wasm wants an arithmetic (quiet) NaN.
fn quiet32(x: f32) -> f32 {
    if x.is_nan() { f32::from_bits(x.to_bits() | 0x0040_0000) } else { x }
}

fn quiet64(x: f64) -> f64 {
    if x.is_nan() { f64::from_bits(x.to_bits() | 0x0008_0000_0000_0000) } else { x }
}

fn fmin<F: Copy + PartialOrd>(a: F, b: F, nan: bool, equal: impl Fn() -> F) -> Option<F> {
    // None = NaN; equal values (±0) combine their sign bits.
    if nan {
        None
    } else if a == b {
        Some(equal())
    } else if a < b {
        Some(a)
    } else {
        Some(b)
    }
}

fn f32_min(a: f32, b: f32) -> f32 {
    fmin(a, b, a.is_nan() || b.is_nan(), || f32::from_bits(a.to_bits() | b.to_bits())).unwrap_or(f32::NAN)
}

fn f32_max(a: f32, b: f32) -> f32 {
    fmin(-a, -b, a.is_nan() || b.is_nan(), || -f32::from_bits(a.to_bits() & b.to_bits()))
        .map_or(f32::NAN, |v| -v)
}

fn f64_min(a: f64, b: f64) -> f64 {
    fmin(a, b, a.is_nan() || b.is_nan(), || f64::from_bits(a.to_bits() | b.to_bits())).unwrap_or(f64::NAN)
}

fn f64_max(a: f64, b: f64) -> f64 {
    fmin(-a, -b, a.is_nan() || b.is_nan(), || -f64::from_bits(a.to_bits() & b.to_bits()))
        .map_or(f64::NAN, |v| -v)
}

/// Float → int with the spec's traps: NaN is an invalid conversion, out of range an overflow. `lo`/`hi` are
/// exclusive bounds on the float (so values that truncate into range pass).
fn trunc(x: f64, lo: f64, hi: f64) -> Result<f64, Trap> {
    if x.is_nan() {
        Err(Trap::InvalidConversion)
    } else if x > lo && x < hi {
        Ok(x)
    } else {
        Err(Trap::IntegerOverflow)
    }
}

const I64_MIN_F: f64 = -9223372036854775808.0;

fn trunc_i64(x: f64) -> Result<i64, Trap> {
    if x == I64_MIN_F { Ok(i64::MIN) } else { Ok(trunc(x, I64_MIN_F, 9223372036854775808.0)? as i64) }
}

/// What the rare ops touch — tables, segments, and memory growth.
pub(super) struct Bulk<'a> {
    pub m: &'a Module,
    pub mem: &'a mut Vec<u8>,
    pub memory_max: usize,
    pub tables: &'a mut [Vec<u64>],
    pub table_max: &'a [u32],
    pub elems: &'a mut [Vec<u64>],
    pub data_live: &'a mut [bool],
    pub fuel: &'a mut u64,
}

/// The table ops, bulk memory ops, and `memory.grow`: rare enough to live outside the hot loop, and shared
/// with the JIT, which calls here for them.
///
/// The ones that copy, fill or grow pay a unit of fuel per byte (or table element), once they're known to do
/// the work — their instruction's fuel alone would let a loop of big copies run for ever.
#[inline(never)]
pub(super) fn bulk(op: Op, fr: &mut [u64], env: Bulk) -> Result<(), Trap> {
    let Bulk { m, mem, memory_max, tables, table_max, elems, data_live, fuel } = env;
    let mut pay = |n: usize| {
        let n = n as u64;
        if *fuel < n {
            return Err(Trap::OutOfFuel);
        }
        *fuel -= n;
        Ok(())
    };
    let s = |x: u16| fr[x as usize];
    let three = |d: u16, s_: u16, n: u16| {
        (fr[d as usize] as u32 as usize, fr[s_ as usize], fr[n as usize] as u32 as usize)
    };
    match op {
        Op::TableGet { dst, i, t } => {
            fr[dst as usize] = *tables[t as usize].get(s(i) as u32 as usize).ok_or(Trap::TableOutOfBounds)?;
        }
        Op::TableSet { i, v, t } => {
            let (i, v) = (s(i) as u32 as usize, s(v));
            *tables[t as usize].get_mut(i).ok_or(Trap::TableOutOfBounds)? = v;
        }
        Op::TableSize { dst, t } => fr[dst as usize] = tables[t as usize].len() as u64,
        Op::TableGrow { dst, init, n, t } => {
            let (n, init) = (s(n) as u32 as usize, s(init));
            let table = &mut tables[t as usize];
            let old = table.len();
            fr[dst as usize] = if old + n <= table_max[t as usize] as usize {
                pay(n)?;
                table.resize(old + n, init);
                old as u64
            } else {
                u32::MAX as u64
            };
        }
        Op::TableFill { i, v, n, t } => {
            let (i, v, n) = three(i, v, n);
            let dst = tables[t as usize].get_mut(i..i + n).ok_or(Trap::TableOutOfBounds)?;
            pay(n)?;
            dst.fill(v);
        }
        Op::TableCopy { d, s: src, n, into, from } => {
            let (d, src, n) = three(d, src, n);
            let src = src as u32 as usize;
            if src + n > tables[from as usize].len() || d + n > tables[into as usize].len() {
                return Err(Trap::TableOutOfBounds);
            }
            pay(n)?;
            let items = tables[from as usize][src..src + n].to_vec();
            tables[into as usize][d..d + n].copy_from_slice(&items);
        }
        Op::TableInit { d, s: src, n, table, elem } => {
            let (d, src, n) = three(d, src, n);
            let src = src as u32 as usize;
            let items = elems[elem as usize].get(src..src + n).ok_or(Trap::TableOutOfBounds)?;
            let dst = tables[table as usize].get_mut(d..d + n).ok_or(Trap::TableOutOfBounds)?;
            pay(n)?;
            dst.copy_from_slice(items);
        }
        Op::ElemDrop(e) => elems[e as usize] = Vec::new(),
        Op::MemoryGrow { dst, n } => {
            let n = s(n) as u32 as usize;
            let old = mem.len();
            fr[dst as usize] = match n.checked_mul(PAGE).map(|b| old + b) {
                Some(new) if new <= memory_max => {
                    pay(new - old)?;
                    mem.resize(new, 0);
                    (old / PAGE) as u64
                }
                _ => u32::MAX as u64,
            };
        }
        Op::MemoryCopy { d, s: src, n } => {
            let (d, src, n) = three(d, src, n);
            let src = src as u32 as usize;
            if src + n > mem.len() || d + n > mem.len() {
                return Err(Trap::MemoryOutOfBounds);
            }
            pay(n)?;
            mem.copy_within(src..src + n, d);
        }
        Op::MemoryFill { d, v, n } => {
            let (d, v, n) = three(d, v, n);
            let dst = mem.get_mut(d..d + n).ok_or(Trap::MemoryOutOfBounds)?;
            pay(n)?;
            dst.fill(v as u8);
        }
        Op::MemoryInit { d, s: src, n, seg } => {
            let (d, src, n) = three(d, src, n);
            let src = src as u32 as usize;
            let data: &[u8] = if data_live[seg as usize] { &m.datas[seg as usize].bytes } else { &[] };
            let from = data.get(src..src + n).ok_or(Trap::MemoryOutOfBounds)?;
            let dst = mem.get_mut(d..d + n).ok_or(Trap::MemoryOutOfBounds)?;
            pay(n)?;
            dst.copy_from_slice(from);
        }
        Op::DataDrop(seg) => data_live[seg as usize] = false,
        _ => unreachable!("not a bulk op"),
    }
    Ok(())
}

impl Instance {
    /// Runs function `f` with its arguments in `stack[0..]`; its results end up there too.
    pub(super) fn run(&mut self, f: u32) -> Result<(), Trap> {
        let Instance {
            module: m,
            limits,
            memory: mem,
            memory_max,
            globals,
            tables,
            table_max,
            elems,
            data_live,
            stack,
            frames,
            fuel,
        } = self;
        let code = &m.code[..];
        let stack: &mut [u64] = stack;
        let depth = limits.call_depth;
        frames.clear();
        let mut base: usize;
        let mut pc: usize;
        let mut fr: &mut [u64];

        macro_rules! enter {
            ($f:expr, $at:expr) => {{
                let func = &m.funcs[$f as usize];
                let at = $at;
                if at + func.frame as usize > stack.len() {
                    return Err(Trap::StackOverflow);
                }
                base = at;
                fr = &mut stack[at..];
                fr[func.params as usize..func.locals as usize].fill(0);
                pc = func.start as usize;
            }};
        }
        macro_rules! call {
            ($f:expr, $at:expr) => {{
                if frames.len() >= depth {
                    return Err(Trap::StackOverflow);
                }
                frames.push(Frame { ret: pc as u32, base: base as u32 });
                enter!($f, base + $at as usize);
            }};
        }
        macro_rules! s {
            ($x:expr) => {
                fr[$x as usize]
            };
        }
        macro_rules! jump {
            ($cond:expr, $to:expr) => {
                if $cond {
                    pc = $to as usize;
                }
            };
        }
        macro_rules! load {
            ($dst:expr, $addr:expr, $off:expr, $n:literal, |$b:ident| $v:expr) => {{
                let a = s!($addr) as u32 as usize + $off as usize;
                let $b = *mem.get(a..).and_then(|s| s.first_chunk::<$n>()).ok_or(Trap::MemoryOutOfBounds)?;
                s!($dst) = $v;
            }};
        }
        macro_rules! store {
            ($addr:expr, $val:expr, $off:expr, $n:literal, |$v:ident| $bytes:expr) => {{
                let a = s!($addr) as u32 as usize + $off as usize;
                let $v = s!($val);
                *mem.get_mut(a..).and_then(|s| s.first_chunk_mut::<$n>()).ok_or(Trap::MemoryOutOfBounds)? =
                    $bytes;
            }};
        }
        macro_rules! i32s {
            ($x:expr) => {
                s!($x) as u32 as i32
            };
        }
        macro_rules! u32s {
            ($x:expr) => {
                s!($x) as u32
            };
        }

        enter!(f, 0);
        loop {
            let op = code[pc];
            pc += 1;
            dispatch! { op, fr;
                control {
                    Op::Unreachable => return Err(Trap::Unreachable),
                    Op::Fuel(n) => {
                        let n = n as u64;
                        if *fuel < n {
                            return Err(Trap::OutOfFuel);
                        }
                        *fuel -= n;
                    }
                    Op::Jump(to) => pc = to as usize,
                    Op::JumpIf { c, to } => jump!(u32s!(c) != 0, to),
                    Op::JumpIfNot { c, to } => jump!(u32s!(c) == 0, to),
                    Op::JumpIf64 { c, to } => jump!(s!(c) != 0, to),
                    Op::JumpIfNot64 { c, to } => jump!(s!(c) == 0, to),
                    Op::JumpIfEq { a, b, to } => jump!(u32s!(a) == u32s!(b), to),
                    Op::JumpIfNe { a, b, to } => jump!(u32s!(a) != u32s!(b), to),
                    Op::JumpIfLtS { a, b, to } => jump!(i32s!(a) < i32s!(b), to),
                    Op::JumpIfLtU { a, b, to } => jump!(u32s!(a) < u32s!(b), to),
                    Op::JumpIfGtS { a, b, to } => jump!(i32s!(a) > i32s!(b), to),
                    Op::JumpIfGtU { a, b, to } => jump!(u32s!(a) > u32s!(b), to),
                    Op::JumpIfLeS { a, b, to } => jump!(i32s!(a) <= i32s!(b), to),
                    Op::JumpIfLeU { a, b, to } => jump!(u32s!(a) <= u32s!(b), to),
                    Op::JumpIfGeS { a, b, to } => jump!(i32s!(a) >= i32s!(b), to),
                    Op::JumpIfGeU { a, b, to } => jump!(u32s!(a) >= u32s!(b), to),
                    Op::JumpIfEqImm { a, imm, to } => jump!(i32s!(a) == imm, to),
                    Op::JumpIfNeImm { a, imm, to } => jump!(i32s!(a) != imm, to),
                    Op::JumpIfLtSImm { a, imm, to } => jump!(i32s!(a) < imm, to),
                    Op::JumpIfLtUImm { a, imm, to } => jump!(u32s!(a) < imm as u32, to),
                    Op::JumpIfGtSImm { a, imm, to } => jump!(i32s!(a) > imm, to),
                    Op::JumpIfGtUImm { a, imm, to } => jump!(u32s!(a) > imm as u32, to),
                    Op::JumpIfLeSImm { a, imm, to } => jump!(i32s!(a) <= imm, to),
                    Op::JumpIfLeUImm { a, imm, to } => jump!(u32s!(a) <= imm as u32, to),
                    Op::JumpIfGeSImm { a, imm, to } => jump!(i32s!(a) >= imm, to),
                    Op::JumpIfGeUImm { a, imm, to } => jump!(u32s!(a) >= imm as u32, to),
                    Op::BrTable { c, start, len } => {
                        let t = m.br_tables[(start + u32s!(c).min(len)) as usize];
                        let (src, n) = (t.src as usize, t.n as usize);
                        fr.copy_within(src..src + n, t.dst as usize);
                        pc = t.to as usize;
                    }
                    Op::Move { dst, src, n } => {
                        let (src, n) = (src as usize, n as usize);
                        fr.copy_within(src..src + n, dst as usize);
                    }
                    Op::Return { src, n } => {
                        let (src, n) = (src as usize, n as usize);
                        fr.copy_within(src..src + n, 0);
                        match frames.pop() {
                            Some(caller) => {
                                base = caller.base as usize;
                                fr = &mut stack[base..];
                                pc = caller.ret as usize;
                            }
                            None => return Ok(()),
                        }
                    }
                    Op::Call { at, f } => call!(f, at),
                    Op::CallIndirect { at, c, ty, table } => {
                        let r = *tables[table as usize].get(u32s!(c) as usize).ok_or(Trap::UndefinedElement)?;
                        let Some(f) = r.checked_sub(1) else { return Err(Trap::UninitializedElement) };
                        if m.canon[m.func_ty[f as usize] as usize] != ty {
                            return Err(Trap::IndirectCallType);
                        }
                        call!(f, at);
                    }
                    Op::Copy { dst, src } => s!(dst) = s!(src),
                    Op::Const { dst, v } => s!(dst) = v,
                    Op::Select { dst, a, b, c } => s!(dst) = if u32s!(c) != 0 { s!(a) } else { s!(b) },
                    Op::GlobalGet { dst, g } => s!(dst) = globals[g as usize],
                    Op::GlobalSet { src, g } => globals[g as usize] = s!(src),
                    op @ (Op::TableGet { .. }
                    | Op::TableSet { .. }
                    | Op::TableSize { .. }
                    | Op::TableGrow { .. }
                    | Op::TableFill { .. }
                    | Op::TableCopy { .. }
                    | Op::TableInit { .. }
                    | Op::ElemDrop(_)
                    | Op::MemoryGrow { .. }
                    | Op::MemoryCopy { .. }
                    | Op::MemoryFill { .. }
                    | Op::MemoryInit { .. }
                    | Op::DataDrop(_)) => {
                        let env =
                            Bulk { m, mem, memory_max: *memory_max, tables, table_max, elems, data_live, fuel };
                        bulk(op, fr, env)?;
                    }
                    Op::RefIsNull { dst, a } => s!(dst) = (s!(a) == NULL) as u64,
                    Op::Load64 { dst, addr, off } => load!(dst, addr, off, 8, |b| u64::from_le_bytes(b)),
                    Op::Load32U { dst, addr, off } => load!(dst, addr, off, 4, |b| u32::from_le_bytes(b) as u64),
                    Op::Load8S32 { dst, addr, off } => load!(dst, addr, off, 1, |b| b[0] as i8 as i32 as u32 as u64),
                    Op::Load8U { dst, addr, off } => load!(dst, addr, off, 1, |b| b[0] as u64),
                    Op::Load16S32 { dst, addr, off } => {
                        load!(dst, addr, off, 2, |b| i16::from_le_bytes(b) as i32 as u32 as u64)
                    }
                    Op::Load16U { dst, addr, off } => load!(dst, addr, off, 2, |b| u16::from_le_bytes(b) as u64),
                    Op::Load8S64 { dst, addr, off } => load!(dst, addr, off, 1, |b| b[0] as i8 as i64 as u64),
                    Op::Load16S64 { dst, addr, off } => load!(dst, addr, off, 2, |b| i16::from_le_bytes(b) as i64 as u64),
                    Op::Load32S64 { dst, addr, off } => load!(dst, addr, off, 4, |b| i32::from_le_bytes(b) as i64 as u64),
                    Op::Store8 { addr, val, off } => store!(addr, val, off, 1, |v| [v as u8]),
                    Op::Store16 { addr, val, off } => store!(addr, val, off, 2, |v| (v as u16).to_le_bytes()),
                    Op::Store32 { addr, val, off } => store!(addr, val, off, 4, |v| (v as u32).to_le_bytes()),
                    Op::Store64 { addr, val, off } => store!(addr, val, off, 8, |v| v.to_le_bytes()),
                    Op::MemorySize { dst } => s!(dst) = (mem.len() / PAGE) as u64,
                }
                unary {
                    I32Eqz: u32 -> bool, |x| x == 0;
                    I64Eqz: u64 -> bool, |x| x == 0;
                    I32Clz: u32 -> u32, |x| x.leading_zeros();
                    I32Ctz: u32 -> u32, |x| x.trailing_zeros();
                    I32Popcnt: u32 -> u32, |x| x.count_ones();
                    I64Clz: u64 -> u64, |x| x.leading_zeros() as u64;
                    I64Ctz: u64 -> u64, |x| x.trailing_zeros() as u64;
                    I64Popcnt: u64 -> u64, |x| x.count_ones() as u64;
                    F32Abs: u32 -> u32, |x| x & 0x7FFF_FFFF;
                    F32Neg: u32 -> u32, |x| x ^ 0x8000_0000;
                    F32Ceil: f32 -> f32, |x| quiet32(x.ceil());
                    F32Floor: f32 -> f32, |x| quiet32(x.floor());
                    F32Trunc: f32 -> f32, |x| quiet32(x.trunc());
                    F32Nearest: f32 -> f32, |x| quiet32(x.round_ties_even());
                    F32Sqrt: f32 -> f32, |x| x.sqrt();
                    F64Abs: u64 -> u64, |x| x & !(1 << 63);
                    F64Neg: u64 -> u64, |x| x ^ (1 << 63);
                    F64Ceil: f64 -> f64, |x| quiet64(x.ceil());
                    F64Floor: f64 -> f64, |x| quiet64(x.floor());
                    F64Trunc: f64 -> f64, |x| quiet64(x.trunc());
                    F64Nearest: f64 -> f64, |x| quiet64(x.round_ties_even());
                    F64Sqrt: f64 -> f64, |x| x.sqrt();
                    I32WrapI64: u64 -> u32, |x| x as u32;
                    I32TruncF32S: f32 -> i32, |x| trunc(x as f64, -2147483649.0, 2147483648.0)? as i32;
                    I32TruncF32U: f32 -> u32, |x| trunc(x as f64, -1.0, 4294967296.0)? as u32;
                    I32TruncF64S: f64 -> i32, |x| trunc(x, -2147483649.0, 2147483648.0)? as i32;
                    I32TruncF64U: f64 -> u32, |x| trunc(x, -1.0, 4294967296.0)? as u32;
                    I64ExtendI32S: i32 -> i64, |x| x as i64;
                    I64ExtendI32U: u32 -> u64, |x| x as u64;
                    I64TruncF32S: f32 -> i64, |x| trunc_i64(x as f64)?;
                    I64TruncF32U: f32 -> u64, |x| trunc(x as f64, -1.0, 18446744073709551616.0)? as u64;
                    I64TruncF64S: f64 -> i64, |x| trunc_i64(x)?;
                    I64TruncF64U: f64 -> u64, |x| trunc(x, -1.0, 18446744073709551616.0)? as u64;
                    F32ConvertI32S: i32 -> f32, |x| x as f32;
                    F32ConvertI32U: u32 -> f32, |x| x as f32;
                    F32ConvertI64S: i64 -> f32, |x| x as f32;
                    F32ConvertI64U: u64 -> f32, |x| x as f32;
                    F32DemoteF64: f64 -> f32, |x| x as f32;
                    F64ConvertI32S: i32 -> f64, |x| x as f64;
                    F64ConvertI32U: u32 -> f64, |x| x as f64;
                    F64ConvertI64S: i64 -> f64, |x| x as f64;
                    F64ConvertI64U: u64 -> f64, |x| x as f64;
                    F64PromoteF32: f32 -> f64, |x| x as f64;
                    I32Extend8S: u32 -> i32, |x| x as i8 as i32;
                    I32Extend16S: u32 -> i32, |x| x as i16 as i32;
                    I64Extend8S: u64 -> i64, |x| x as i8 as i64;
                    I64Extend16S: u64 -> i64, |x| x as i16 as i64;
                    I64Extend32S: u64 -> i64, |x| x as i32 as i64;
                    // `as` saturates and maps NaN to 0 — exactly the non-trapping conversions.
                    I32TruncSatF32S: f32 -> i32, |x| x as i32;
                    I32TruncSatF32U: f32 -> u32, |x| x as u32;
                    I32TruncSatF64S: f64 -> i32, |x| x as i32;
                    I32TruncSatF64U: f64 -> u32, |x| x as u32;
                    I64TruncSatF32S: f32 -> i64, |x| x as i64;
                    I64TruncSatF32U: f32 -> u64, |x| x as u64;
                    I64TruncSatF64S: f64 -> i64, |x| x as i64;
                    I64TruncSatF64U: f64 -> u64, |x| x as u64;
                }
                binary {
                    I32Eq / I32EqImm: u32 -> bool, |x, y| x == y;
                    I32Ne / I32NeImm: u32 -> bool, |x, y| x != y;
                    I32LtS / I32LtSImm: i32 -> bool, |x, y| x < y;
                    I32LtU / I32LtUImm: u32 -> bool, |x, y| x < y;
                    I32GtS / I32GtSImm: i32 -> bool, |x, y| x > y;
                    I32GtU / I32GtUImm: u32 -> bool, |x, y| x > y;
                    I32LeS / I32LeSImm: i32 -> bool, |x, y| x <= y;
                    I32LeU / I32LeUImm: u32 -> bool, |x, y| x <= y;
                    I32GeS / I32GeSImm: i32 -> bool, |x, y| x >= y;
                    I32GeU / I32GeUImm: u32 -> bool, |x, y| x >= y;
                    I64Eq / I64EqImm: u64 -> bool, |x, y| x == y;
                    I64Ne / I64NeImm: u64 -> bool, |x, y| x != y;
                    I64LtS / I64LtSImm: i64 -> bool, |x, y| x < y;
                    I64LtU / I64LtUImm: u64 -> bool, |x, y| x < y;
                    I64GtS / I64GtSImm: i64 -> bool, |x, y| x > y;
                    I64GtU / I64GtUImm: u64 -> bool, |x, y| x > y;
                    I64LeS / I64LeSImm: i64 -> bool, |x, y| x <= y;
                    I64LeU / I64LeUImm: u64 -> bool, |x, y| x <= y;
                    I64GeS / I64GeSImm: i64 -> bool, |x, y| x >= y;
                    I64GeU / I64GeUImm: u64 -> bool, |x, y| x >= y;
                    F32Eq: f32 -> bool, |x, y| x == y;
                    F32Ne: f32 -> bool, |x, y| x != y;
                    F32Lt: f32 -> bool, |x, y| x < y;
                    F32Gt: f32 -> bool, |x, y| x > y;
                    F32Le: f32 -> bool, |x, y| x <= y;
                    F32Ge: f32 -> bool, |x, y| x >= y;
                    F64Eq: f64 -> bool, |x, y| x == y;
                    F64Ne: f64 -> bool, |x, y| x != y;
                    F64Lt: f64 -> bool, |x, y| x < y;
                    F64Gt: f64 -> bool, |x, y| x > y;
                    F64Le: f64 -> bool, |x, y| x <= y;
                    F64Ge: f64 -> bool, |x, y| x >= y;
                    I32Add / I32AddImm: u32 -> u32, |x, y| x.wrapping_add(y);
                    I32Sub / I32SubImm: u32 -> u32, |x, y| x.wrapping_sub(y);
                    I32Mul / I32MulImm: u32 -> u32, |x, y| x.wrapping_mul(y);
                    I32DivS: i32 -> i32, |x, y| {
                        if y == 0 {
                            return Err(Trap::DivideByZero);
                        }
                        x.checked_div(y).ok_or(Trap::IntegerOverflow)?
                    };
                    I32DivU: u32 -> u32, |x, y| x.checked_div(y).ok_or(Trap::DivideByZero)?;
                    I32RemS: i32 -> i32, |x, y| {
                        if y == 0 {
                            return Err(Trap::DivideByZero);
                        }
                        x.wrapping_rem(y)
                    };
                    I32RemU: u32 -> u32, |x, y| x.checked_rem(y).ok_or(Trap::DivideByZero)?;
                    I32And / I32AndImm: u32 -> u32, |x, y| x & y;
                    I32Or / I32OrImm: u32 -> u32, |x, y| x | y;
                    I32Xor / I32XorImm: u32 -> u32, |x, y| x ^ y;
                    I32Shl / I32ShlImm: u32 -> u32, |x, y| x.wrapping_shl(y);
                    I32ShrS / I32ShrSImm: i32 -> i32, |x, y| x.wrapping_shr(y as u32);
                    I32ShrU / I32ShrUImm: u32 -> u32, |x, y| x.wrapping_shr(y);
                    I32Rotl / I32RotlImm: u32 -> u32, |x, y| x.rotate_left(y & 31);
                    I32Rotr / I32RotrImm: u32 -> u32, |x, y| x.rotate_right(y & 31);
                    I64Add / I64AddImm: u64 -> u64, |x, y| x.wrapping_add(y);
                    I64Sub / I64SubImm: u64 -> u64, |x, y| x.wrapping_sub(y);
                    I64Mul / I64MulImm: u64 -> u64, |x, y| x.wrapping_mul(y);
                    I64DivS: i64 -> i64, |x, y| {
                        if y == 0 {
                            return Err(Trap::DivideByZero);
                        }
                        x.checked_div(y).ok_or(Trap::IntegerOverflow)?
                    };
                    I64DivU: u64 -> u64, |x, y| x.checked_div(y).ok_or(Trap::DivideByZero)?;
                    I64RemS: i64 -> i64, |x, y| {
                        if y == 0 {
                            return Err(Trap::DivideByZero);
                        }
                        x.wrapping_rem(y)
                    };
                    I64RemU: u64 -> u64, |x, y| x.checked_rem(y).ok_or(Trap::DivideByZero)?;
                    I64And / I64AndImm: u64 -> u64, |x, y| x & y;
                    I64Or / I64OrImm: u64 -> u64, |x, y| x | y;
                    I64Xor / I64XorImm: u64 -> u64, |x, y| x ^ y;
                    I64Shl / I64ShlImm: u64 -> u64, |x, y| x.wrapping_shl(y as u32);
                    I64ShrS / I64ShrSImm: i64 -> i64, |x, y| x.wrapping_shr(y as u32);
                    I64ShrU / I64ShrUImm: u64 -> u64, |x, y| x.wrapping_shr(y as u32);
                    I64Rotl / I64RotlImm: u64 -> u64, |x, y| x.rotate_left((y & 63) as u32);
                    I64Rotr / I64RotrImm: u64 -> u64, |x, y| x.rotate_right((y & 63) as u32);
                    F32Add: f32 -> f32, |x, y| x + y;
                    F32Sub: f32 -> f32, |x, y| x - y;
                    F32Mul: f32 -> f32, |x, y| x * y;
                    F32Div: f32 -> f32, |x, y| x / y;
                    F32Min: f32 -> f32, |x, y| f32_min(x, y);
                    F32Max: f32 -> f32, |x, y| f32_max(x, y);
                    F32Copysign: u32 -> u32, |x, y| (x & 0x7FFF_FFFF) | (y & 0x8000_0000);
                    F64Add: f64 -> f64, |x, y| x + y;
                    F64Sub: f64 -> f64, |x, y| x - y;
                    F64Mul: f64 -> f64, |x, y| x * y;
                    F64Div: f64 -> f64, |x, y| x / y;
                    F64Min: f64 -> f64, |x, y| f64_min(x, y);
                    F64Max: f64 -> f64, |x, y| f64_max(x, y);
                    F64Copysign: u64 -> u64, |x, y| (x & !(1 << 63)) | (y & (1 << 63));
                }
            }
        }
    }
}
