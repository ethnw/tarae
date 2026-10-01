//! Validation + compilation, in one pass per function. The spec's operand-stack type checker (appendix
//! "Validation Algorithm") drives it, and while it checks, it turns the stack code into **register code**:
//! every operand has a fixed slot in the frame (locals first, then the operand stack), so an op names the
//! slots it reads and writes and the interpreter keeps no stack pointer.
//!
//! - `local.get` and constants cost nothing: the compile-time stack remembers "this operand is local 3" or
//!   "this is the constant 7", and the op that consumes it reads the local directly or takes the constant as
//!   an immediate. They're copied into their stack slot ("materialized") only where a value must really be
//!   there: at block boundaries, branches, calls, and before the local they name is overwritten.
//! - `local.set` right after the op that computed the value retargets that op to write the local.
//! - A comparison right before `br_if`/`if` fuses into one compare-and-jump.
//! - Every branch knows its target pc and which slots to move; `drop` and reinterprets emit nothing.
//!
//! Slots hold `u64` bits: i32 zero-extended, floats as their bits, references as function index + 1
//! (`NULL` = 0). Fuel: a `Fuel(n)` op sits at every function entry and loop head, `n` = the body's cost — an
//! op each, a move or return of `k` slots `k`, and at entry the locals zeroed — so fuel bounds time.
//! Compiling is bounded too: a module may take `MAX_WORK` steps (operands scanned and checked), so no
//! small module can take long to load.

use super::module::Module;
use super::read::{R, Reader};
use super::{Error, NULL, Ty};

/// A slot in the current frame.
pub(super) type S = u16;

/// A `br_table` destination: jump to `to` after moving `n` slots from `src` to `dst`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Target {
    pub to: u32,
    pub dst: S,
    pub src: S,
    pub n: S,
}

/// A compiled function.
#[derive(Clone, Copy, Debug)]
pub(super) struct Code {
    pub start: u32,
    pub params: u32,
    /// Params + declared locals — `params..locals` are zeroed on entry.
    pub locals: u32,
    /// Slots the frame uses: locals + the deepest operand stack.
    pub frame: u32,
    /// Where this function's locals start in `Module::floats` (whether each is f32/f64 — the JIT keeps those
    /// in float registers).
    pub floats: u32,
}

const LATER: u32 = u32::MAX;
const MAX_LOCALS: u64 = 50_000;
/// Locals (params included) of all a module's functions together.
const MAX_MODULE_LOCALS: usize = 1 << 22;
/// Compiling steps a module may take (a fraction of a second; a large Rust module takes tens of millions).
const MAX_WORK: u64 = 1 << 27;

macro_rules! ops {
    (
        fixed { $($fixed:tt)* }
        unary { $($ub:literal $un:ident ($uin:ident) -> $uout:ident;)* }
        binary { $($bb:literal $bn:ident ($bin:ident) -> $bout:ident $(, $imm:ident)?;)* }
        saturating { $($sb:literal $sn:ident ($sin:ident) -> $sout:ident;)* }
    ) => {
        // `Debug` (16 KB of code) only where something prints ops: tests and debug builds.
        #[derive(Clone, Copy, PartialEq)]
        #[cfg_attr(any(test, debug_assertions), derive(Debug))]
        pub(super) enum Op {
            $($fixed)*
            $($un { dst: S, a: S },)*
            $($sn { dst: S, a: S },)*
            $($bn { dst: S, a: S, b: S },)*
            $($($imm { dst: S, a: S, imm: i32 },)?)*
        }

        enum Shape {
            Un(fn(S, S) -> Op),
            Bin(fn(S, S, S) -> Op),
        }

        /// `0x45..=0xC4` → (shape, operand type, result type).
        fn numeric(byte: u8) -> Option<(Shape, Ty, Ty)> {
            match byte {
                $($ub => Some((Shape::Un(|dst, a| Op::$un { dst, a }), Ty::$uin, Ty::$uout)),)*
                $($bb => Some((Shape::Bin(|dst, a, b| Op::$bn { dst, a, b }), Ty::$bin, Ty::$bout)),)*
                _ => None,
            }
        }

        /// `0xFC 0..=7`, the non-trapping float → int conversions.
        fn saturating(sub: u32) -> Option<(Shape, Ty, Ty)> {
            match sub {
                $($sb => Some((Shape::Un(|dst, a| Op::$sn { dst, a }), Ty::$sin, Ty::$sout)),)*
                _ => None,
            }
        }

        impl Op {
            /// The same operation with its right operand as an immediate, if it has that form.
            fn with_imm(self, imm: i32) -> Option<Op> {
                match self {
                    $($(Op::$bn { dst, a, .. } => Some(Op::$imm { dst, a, imm }),)?)*
                    _ => None,
                }
            }

            /// Every slot the op reads or writes (for the JIT's choice of what to keep in registers).
            pub(super) fn slots(&self, f: &mut impl FnMut(S)) {
                match *self {
                    $(Op::$un { dst, a } => { f(dst); f(a) })*
                    $(Op::$sn { dst, a } => { f(dst); f(a) })*
                    $(Op::$bn { dst, a, b } => { f(dst); f(a); f(b) })*
                    $($(Op::$imm { dst, a, .. } => { f(dst); f(a) })?)*
                    _ => self.fixed_slots(f),
                }
            }

            /// The slot a single-result op writes (so `local.set` can retarget it).
            fn dst_mut(&mut self) -> Option<&mut S> {
                match self {
                    $(Op::$un { dst, .. } => Some(dst),)*
                    $(Op::$sn { dst, .. } => Some(dst),)*
                    $(Op::$bn { dst, .. } => Some(dst),)*
                    $($(Op::$imm { dst, .. } => Some(dst),)?)*
                    Op::Copy { dst, .. }
                    | Op::Const { dst, .. }
                    | Op::Select { dst, .. }
                    | Op::GlobalGet { dst, .. }
                    | Op::TableGet { dst, .. }
                    | Op::TableSize { dst, .. }
                    | Op::TableGrow { dst, .. }
                    | Op::RefIsNull { dst, .. }
                    | Op::Load64 { dst, .. }
                    | Op::Load32U { dst, .. }
                    | Op::Load8S32 { dst, .. }
                    | Op::Load8U { dst, .. }
                    | Op::Load16S32 { dst, .. }
                    | Op::Load16U { dst, .. }
                    | Op::Load8S64 { dst, .. }
                    | Op::Load16S64 { dst, .. }
                    | Op::Load32S64 { dst, .. }
                    | Op::MemorySize { dst }
                    | Op::MemoryGrow { dst, .. } => Some(dst),
                    _ => None,
                }
            }
        }
    };
}

ops! {
    fixed {
        Unreachable,
        /// Charge this much fuel (a function entry or a loop head).
        Fuel(u32),
        Jump(u32),
        /// Jump when slot `c` (an i32) is nonzero / zero; the `64` forms test an i64.
        JumpIf { c: S, to: u32 },
        JumpIfNot { c: S, to: u32 },
        JumpIf64 { c: S, to: u32 },
        JumpIfNot64 { c: S, to: u32 },
        // A fused i32 comparison and jump: jump when `a <op> b` (or `a <op> imm`).
        JumpIfEq { a: S, b: S, to: u32 },
        JumpIfNe { a: S, b: S, to: u32 },
        JumpIfLtS { a: S, b: S, to: u32 },
        JumpIfLtU { a: S, b: S, to: u32 },
        JumpIfGtS { a: S, b: S, to: u32 },
        JumpIfGtU { a: S, b: S, to: u32 },
        JumpIfLeS { a: S, b: S, to: u32 },
        JumpIfLeU { a: S, b: S, to: u32 },
        JumpIfGeS { a: S, b: S, to: u32 },
        JumpIfGeU { a: S, b: S, to: u32 },
        JumpIfEqImm { a: S, imm: i32, to: u32 },
        JumpIfNeImm { a: S, imm: i32, to: u32 },
        JumpIfLtSImm { a: S, imm: i32, to: u32 },
        JumpIfLtUImm { a: S, imm: i32, to: u32 },
        JumpIfGtSImm { a: S, imm: i32, to: u32 },
        JumpIfGtUImm { a: S, imm: i32, to: u32 },
        JumpIfLeSImm { a: S, imm: i32, to: u32 },
        JumpIfLeUImm { a: S, imm: i32, to: u32 },
        JumpIfGeSImm { a: S, imm: i32, to: u32 },
        JumpIfGeUImm { a: S, imm: i32, to: u32 },
        /// Targets are `br_tables[start..=start+len]`; the last one is the default.
        BrTable { c: S, start: u32, len: u32 },
        /// Copy `n` slots from `src` down to `dst` (a branch carrying values).
        Move { dst: S, src: S, n: S },
        /// Results are `n` slots at `src`; they go to the frame's first slots, where the caller wants them.
        Return { src: S, n: S },
        /// The callee's frame starts at slot `at`, where its arguments already are.
        Call { at: S, f: u32 },
        /// `ty` is the canonical type id (equal signatures share one); `c` holds the table index.
        CallIndirect { at: S, c: S, ty: u32, table: u32 },
        Copy { dst: S, src: S },
        Const { dst: S, v: u64 },
        Select { dst: S, a: S, b: S, c: S },
        GlobalGet { dst: S, g: u32 },
        GlobalSet { src: S, g: u32 },
        TableGet { dst: S, i: S, t: u32 },
        TableSet { i: S, v: S, t: u32 },
        TableSize { dst: S, t: u32 },
        TableGrow { dst: S, init: S, n: S, t: u32 },
        TableFill { i: S, v: S, n: S, t: u32 },
        TableCopy { d: S, s: S, n: S, into: u32, from: u32 },
        TableInit { d: S, s: S, n: S, table: u32, elem: u32 },
        ElemDrop(u32),
        RefIsNull { dst: S, a: S },
        // Loads and stores carry the static offset. `Load8U`/`Load16U`/`Load32U` serve i32 and i64 alike.
        Load64 { dst: S, addr: S, off: u32 },
        Load32U { dst: S, addr: S, off: u32 },
        Load8S32 { dst: S, addr: S, off: u32 },
        Load8U { dst: S, addr: S, off: u32 },
        Load16S32 { dst: S, addr: S, off: u32 },
        Load16U { dst: S, addr: S, off: u32 },
        Load8S64 { dst: S, addr: S, off: u32 },
        Load16S64 { dst: S, addr: S, off: u32 },
        Load32S64 { dst: S, addr: S, off: u32 },
        Store8 { addr: S, val: S, off: u32 },
        Store16 { addr: S, val: S, off: u32 },
        Store32 { addr: S, val: S, off: u32 },
        Store64 { addr: S, val: S, off: u32 },
        MemorySize { dst: S },
        MemoryGrow { dst: S, n: S },
        MemoryCopy { d: S, s: S, n: S },
        MemoryFill { d: S, v: S, n: S },
        MemoryInit { d: S, s: S, n: S, seg: u32 },
        DataDrop(u32),
    }
    unary {
        0x45 I32Eqz (I32) -> I32;
        0x50 I64Eqz (I64) -> I32;
        0x67 I32Clz (I32) -> I32;
        0x68 I32Ctz (I32) -> I32;
        0x69 I32Popcnt (I32) -> I32;
        0x79 I64Clz (I64) -> I64;
        0x7A I64Ctz (I64) -> I64;
        0x7B I64Popcnt (I64) -> I64;
        0x8B F32Abs (F32) -> F32;
        0x8C F32Neg (F32) -> F32;
        0x8D F32Ceil (F32) -> F32;
        0x8E F32Floor (F32) -> F32;
        0x8F F32Trunc (F32) -> F32;
        0x90 F32Nearest (F32) -> F32;
        0x91 F32Sqrt (F32) -> F32;
        0x99 F64Abs (F64) -> F64;
        0x9A F64Neg (F64) -> F64;
        0x9B F64Ceil (F64) -> F64;
        0x9C F64Floor (F64) -> F64;
        0x9D F64Trunc (F64) -> F64;
        0x9E F64Nearest (F64) -> F64;
        0x9F F64Sqrt (F64) -> F64;
        0xA7 I32WrapI64 (I64) -> I32;
        0xA8 I32TruncF32S (F32) -> I32;
        0xA9 I32TruncF32U (F32) -> I32;
        0xAA I32TruncF64S (F64) -> I32;
        0xAB I32TruncF64U (F64) -> I32;
        0xAC I64ExtendI32S (I32) -> I64;
        0xAD I64ExtendI32U (I32) -> I64;
        0xAE I64TruncF32S (F32) -> I64;
        0xAF I64TruncF32U (F32) -> I64;
        0xB0 I64TruncF64S (F64) -> I64;
        0xB1 I64TruncF64U (F64) -> I64;
        0xB2 F32ConvertI32S (I32) -> F32;
        0xB3 F32ConvertI32U (I32) -> F32;
        0xB4 F32ConvertI64S (I64) -> F32;
        0xB5 F32ConvertI64U (I64) -> F32;
        0xB6 F32DemoteF64 (F64) -> F32;
        0xB7 F64ConvertI32S (I32) -> F64;
        0xB8 F64ConvertI32U (I32) -> F64;
        0xB9 F64ConvertI64S (I64) -> F64;
        0xBA F64ConvertI64U (I64) -> F64;
        0xBB F64PromoteF32 (F32) -> F64;
        0xC0 I32Extend8S (I32) -> I32;
        0xC1 I32Extend16S (I32) -> I32;
        0xC2 I64Extend8S (I64) -> I64;
        0xC3 I64Extend16S (I64) -> I64;
        0xC4 I64Extend32S (I64) -> I64;
    }
    binary {
        0x46 I32Eq (I32) -> I32, I32EqImm;
        0x47 I32Ne (I32) -> I32, I32NeImm;
        0x48 I32LtS (I32) -> I32, I32LtSImm;
        0x49 I32LtU (I32) -> I32, I32LtUImm;
        0x4A I32GtS (I32) -> I32, I32GtSImm;
        0x4B I32GtU (I32) -> I32, I32GtUImm;
        0x4C I32LeS (I32) -> I32, I32LeSImm;
        0x4D I32LeU (I32) -> I32, I32LeUImm;
        0x4E I32GeS (I32) -> I32, I32GeSImm;
        0x4F I32GeU (I32) -> I32, I32GeUImm;
        0x51 I64Eq (I64) -> I32, I64EqImm;
        0x52 I64Ne (I64) -> I32, I64NeImm;
        0x53 I64LtS (I64) -> I32, I64LtSImm;
        0x54 I64LtU (I64) -> I32, I64LtUImm;
        0x55 I64GtS (I64) -> I32, I64GtSImm;
        0x56 I64GtU (I64) -> I32, I64GtUImm;
        0x57 I64LeS (I64) -> I32, I64LeSImm;
        0x58 I64LeU (I64) -> I32, I64LeUImm;
        0x59 I64GeS (I64) -> I32, I64GeSImm;
        0x5A I64GeU (I64) -> I32, I64GeUImm;
        0x5B F32Eq (F32) -> I32;
        0x5C F32Ne (F32) -> I32;
        0x5D F32Lt (F32) -> I32;
        0x5E F32Gt (F32) -> I32;
        0x5F F32Le (F32) -> I32;
        0x60 F32Ge (F32) -> I32;
        0x61 F64Eq (F64) -> I32;
        0x62 F64Ne (F64) -> I32;
        0x63 F64Lt (F64) -> I32;
        0x64 F64Gt (F64) -> I32;
        0x65 F64Le (F64) -> I32;
        0x66 F64Ge (F64) -> I32;
        0x6A I32Add (I32) -> I32, I32AddImm;
        0x6B I32Sub (I32) -> I32, I32SubImm;
        0x6C I32Mul (I32) -> I32, I32MulImm;
        0x6D I32DivS (I32) -> I32;
        0x6E I32DivU (I32) -> I32;
        0x6F I32RemS (I32) -> I32;
        0x70 I32RemU (I32) -> I32;
        0x71 I32And (I32) -> I32, I32AndImm;
        0x72 I32Or (I32) -> I32, I32OrImm;
        0x73 I32Xor (I32) -> I32, I32XorImm;
        0x74 I32Shl (I32) -> I32, I32ShlImm;
        0x75 I32ShrS (I32) -> I32, I32ShrSImm;
        0x76 I32ShrU (I32) -> I32, I32ShrUImm;
        0x77 I32Rotl (I32) -> I32, I32RotlImm;
        0x78 I32Rotr (I32) -> I32, I32RotrImm;
        0x7C I64Add (I64) -> I64, I64AddImm;
        0x7D I64Sub (I64) -> I64, I64SubImm;
        0x7E I64Mul (I64) -> I64, I64MulImm;
        0x7F I64DivS (I64) -> I64;
        0x80 I64DivU (I64) -> I64;
        0x81 I64RemS (I64) -> I64;
        0x82 I64RemU (I64) -> I64;
        0x83 I64And (I64) -> I64, I64AndImm;
        0x84 I64Or (I64) -> I64, I64OrImm;
        0x85 I64Xor (I64) -> I64, I64XorImm;
        0x86 I64Shl (I64) -> I64, I64ShlImm;
        0x87 I64ShrS (I64) -> I64, I64ShrSImm;
        0x88 I64ShrU (I64) -> I64, I64ShrUImm;
        0x89 I64Rotl (I64) -> I64, I64RotlImm;
        0x8A I64Rotr (I64) -> I64, I64RotrImm;
        0x92 F32Add (F32) -> F32;
        0x93 F32Sub (F32) -> F32;
        0x94 F32Mul (F32) -> F32;
        0x95 F32Div (F32) -> F32;
        0x96 F32Min (F32) -> F32;
        0x97 F32Max (F32) -> F32;
        0x98 F32Copysign (F32) -> F32;
        0xA0 F64Add (F64) -> F64;
        0xA1 F64Sub (F64) -> F64;
        0xA2 F64Mul (F64) -> F64;
        0xA3 F64Div (F64) -> F64;
        0xA4 F64Min (F64) -> F64;
        0xA5 F64Max (F64) -> F64;
        0xA6 F64Copysign (F64) -> F64;
    }
    saturating {
        0 I32TruncSatF32S (F32) -> I32;
        1 I32TruncSatF32U (F32) -> I32;
        2 I32TruncSatF64S (F64) -> I32;
        3 I32TruncSatF64U (F64) -> I32;
        4 I64TruncSatF32S (F32) -> I64;
        5 I64TruncSatF32U (F32) -> I64;
        6 I64TruncSatF64S (F64) -> I64;
        7 I64TruncSatF64U (F64) -> I64;
    }
}

impl Op {
    fn fixed_slots(&self, f: &mut impl FnMut(S)) {
        use Op::*;
        match *self {
            JumpIf { c, .. } | JumpIfNot { c, .. } | JumpIf64 { c, .. } | JumpIfNot64 { c, .. } => f(c),
            JumpIfEq { a, b, .. }
            | JumpIfNe { a, b, .. }
            | JumpIfLtS { a, b, .. }
            | JumpIfLtU { a, b, .. }
            | JumpIfGtS { a, b, .. }
            | JumpIfGtU { a, b, .. }
            | JumpIfLeS { a, b, .. }
            | JumpIfLeU { a, b, .. }
            | JumpIfGeS { a, b, .. }
            | JumpIfGeU { a, b, .. } => {
                f(a);
                f(b)
            }
            JumpIfEqImm { a, .. }
            | JumpIfNeImm { a, .. }
            | JumpIfLtSImm { a, .. }
            | JumpIfLtUImm { a, .. }
            | JumpIfGtSImm { a, .. }
            | JumpIfGtUImm { a, .. }
            | JumpIfLeSImm { a, .. }
            | JumpIfLeUImm { a, .. }
            | JumpIfGeSImm { a, .. }
            | JumpIfGeUImm { a, .. } => f(a),
            BrTable { c, .. } | CallIndirect { c, .. } => f(c),
            // Wrapping: in dead code, slot numbers can run past `S` (and are never used)
            Move { dst, src, n } => (0..n).for_each(|i| {
                f(dst.wrapping_add(i));
                f(src.wrapping_add(i))
            }),
            Return { src, n } => (0..n).for_each(|i| f(src.wrapping_add(i))),
            Copy { dst, src } => {
                f(dst);
                f(src)
            }
            Const { dst, .. } | GlobalGet { dst, .. } | TableSize { dst, .. } | MemorySize { dst } => f(dst),
            GlobalSet { src, .. } => f(src),
            Select { dst, a, b, c } => [dst, a, b, c].into_iter().for_each(f),
            TableGet { dst, i, .. } => {
                f(dst);
                f(i)
            }
            TableSet { i, v, .. } => {
                f(i);
                f(v)
            }
            TableGrow { dst, init, n, .. } => [dst, init, n].into_iter().for_each(f),
            TableFill { i, v, n, .. } => [i, v, n].into_iter().for_each(f),
            TableCopy { d, s, n, .. }
            | TableInit { d, s, n, .. }
            | MemoryCopy { d, s, n }
            | MemoryInit { d, s, n, .. } => [d, s, n].into_iter().for_each(f),
            MemoryFill { d, v, n } => [d, v, n].into_iter().for_each(f),
            RefIsNull { dst, a } | MemoryGrow { dst, n: a } => {
                f(dst);
                f(a)
            }
            Load64 { dst, addr, .. }
            | Load32U { dst, addr, .. }
            | Load8S32 { dst, addr, .. }
            | Load8U { dst, addr, .. }
            | Load16S32 { dst, addr, .. }
            | Load16U { dst, addr, .. }
            | Load8S64 { dst, addr, .. }
            | Load16S64 { dst, addr, .. }
            | Load32S64 { dst, addr, .. } => {
                f(dst);
                f(addr)
            }
            Store8 { addr, val, .. }
            | Store16 { addr, val, .. }
            | Store32 { addr, val, .. }
            | Store64 { addr, val, .. } => {
                f(addr);
                f(val)
            }
            _ => {}
        }
    }

    fn target_mut(&mut self) -> Option<&mut u32> {
        match self {
            Op::Jump(to)
            | Op::JumpIf { to, .. }
            | Op::JumpIfNot { to, .. }
            | Op::JumpIf64 { to, .. }
            | Op::JumpIfNot64 { to, .. }
            | Op::JumpIfEq { to, .. }
            | Op::JumpIfNe { to, .. }
            | Op::JumpIfLtS { to, .. }
            | Op::JumpIfLtU { to, .. }
            | Op::JumpIfGtS { to, .. }
            | Op::JumpIfGtU { to, .. }
            | Op::JumpIfLeS { to, .. }
            | Op::JumpIfLeU { to, .. }
            | Op::JumpIfGeS { to, .. }
            | Op::JumpIfGeU { to, .. }
            | Op::JumpIfEqImm { to, .. }
            | Op::JumpIfNeImm { to, .. }
            | Op::JumpIfLtSImm { to, .. }
            | Op::JumpIfLtUImm { to, .. }
            | Op::JumpIfGtSImm { to, .. }
            | Op::JumpIfGtUImm { to, .. }
            | Op::JumpIfLeSImm { to, .. }
            | Op::JumpIfLeUImm { to, .. }
            | Op::JumpIfGeSImm { to, .. }
            | Op::JumpIfGeUImm { to, .. } => Some(to),
            _ => None,
        }
    }

    /// A comparison that feeds a conditional jump, as one compare-and-jump (`negate`: jump when false).
    fn fused(self, to: u32, negate: bool) -> Option<Op> {
        macro_rules! pick {
            ($($cmp:ident $cmp_imm:ident => $yes:ident $yes_imm:ident, $no:ident $no_imm:ident;)*) => {
                match self {
                    $(
                        Op::$cmp { a, b, .. } => {
                            Some(if negate { Op::$no { a, b, to } } else { Op::$yes { a, b, to } })
                        }
                        Op::$cmp_imm { a, imm, .. } => {
                            Some(if negate { Op::$no_imm { a, imm, to } } else { Op::$yes_imm { a, imm, to } })
                        }
                    )*
                    Op::I32Eqz { a, .. } => {
                        Some(if negate { Op::JumpIf { c: a, to } } else { Op::JumpIfNot { c: a, to } })
                    }
                    Op::I64Eqz { a, .. } => {
                        Some(if negate { Op::JumpIf64 { c: a, to } } else { Op::JumpIfNot64 { c: a, to } })
                    }
                    _ => None,
                }
            };
        }
        pick! {
            I32Eq I32EqImm => JumpIfEq JumpIfEqImm, JumpIfNe JumpIfNeImm;
            I32Ne I32NeImm => JumpIfNe JumpIfNeImm, JumpIfEq JumpIfEqImm;
            I32LtS I32LtSImm => JumpIfLtS JumpIfLtSImm, JumpIfGeS JumpIfGeSImm;
            I32LtU I32LtUImm => JumpIfLtU JumpIfLtUImm, JumpIfGeU JumpIfGeUImm;
            I32GtS I32GtSImm => JumpIfGtS JumpIfGtSImm, JumpIfLeS JumpIfLeSImm;
            I32GtU I32GtUImm => JumpIfGtU JumpIfGtUImm, JumpIfLeU JumpIfLeUImm;
            I32LeS I32LeSImm => JumpIfLeS JumpIfLeSImm, JumpIfGtS JumpIfGtSImm;
            I32LeU I32LeUImm => JumpIfLeU JumpIfLeUImm, JumpIfGtU JumpIfGtUImm;
            I32GeS I32GeSImm => JumpIfGeS JumpIfGeSImm, JumpIfLtS JumpIfLtSImm;
            I32GeU I32GeUImm => JumpIfGeU JumpIfGeUImm, JumpIfLtU JumpIfLtUImm;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Func,
    Block,
    Loop,
    If,
    Else,
}

#[derive(Clone, Copy)]
enum BlockType {
    Empty,
    One(Ty),
    Func(u32),
}

enum Patch {
    Op(u32),
    Table(u32),
}

struct Ctrl {
    kind: Kind,
    bt: BlockType,
    /// Operand stack height below this block's params.
    height: usize,
    unreachable: bool,
    /// A loop's head (its label).
    start: u32,
    /// Jumps to this block's end, filled in at `end`.
    patches: Vec<Patch>,
    /// An `if`'s jump to its `else` (or `end`).
    if_jump: Option<u32>,
    /// `V::cost` where it starts (for a loop's fuel).
    cost: u64,
}

/// Where a compile-time operand's value is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    /// In its own operand stack slot.
    Stack,
    /// Still in this local (`local.get` not yet copied).
    Local(S),
    /// A constant not yet written anywhere.
    Const(u64),
}

#[derive(Clone, Copy)]
struct Opd {
    /// `None`: unknown (unreachable code).
    ty: Option<Ty>,
    src: Src,
}

fn one(t: Ty) -> &'static [Ty] {
    match t {
        Ty::I32 => &[Ty::I32],
        Ty::I64 => &[Ty::I64],
        Ty::F32 => &[Ty::F32],
        Ty::F64 => &[Ty::F64],
        Ty::FuncRef => &[Ty::FuncRef],
        Ty::ExternRef => &[Ty::ExternRef],
    }
}

fn mismatch() -> Error {
    Error::new("type mismatch")
}

pub(super) fn val_type(b: u8) -> R<Ty> {
    Ok(match b {
        0x7F => Ty::I32,
        0x7E => Ty::I64,
        0x7D => Ty::F32,
        0x7C => Ty::F64,
        0x7B => return Err(Error::new("SIMD (v128) is not supported")),
        _ => return ref_type(b),
    })
}

pub(super) fn ref_type(b: u8) -> R<Ty> {
    match b {
        0x70 => Ok(Ty::FuncRef),
        0x6F => Ok(Ty::ExternRef),
        0x63 | 0x64 => Err(Error::new("typed function references are not supported")),
        0x65..=0x6E | 0x71..=0x74 => Err(Error::new("GC and exception reference types are not supported")),
        _ => Err(Error::new("malformed reference type")),
    }
}

struct V<'a> {
    m: &'a Module,
    code: &'a mut Vec<Op>,
    targets: &'a mut Vec<Target>,
    locals: Vec<Ty>,
    vals: Vec<Opd>,
    ctrls: Vec<Ctrl>,
    max: usize,
    /// The last op emitted computed the operand at this stack index (so `local.set` / `br_if` / `if` can
    /// take it over) — cleared by any other emit.
    last: Option<(usize, usize)>,
    /// What the ops emitted so far cost to run (see the module doc).
    cost: u64,
    /// Compiling steps taken so far in this module.
    work: &'a mut u64,
}

/// A `Fuel` op's amount (a body costing more than `u32::MAX` pays that much).
fn fuel(cost: u64) -> u32 {
    u32::try_from(cost).unwrap_or(u32::MAX)
}

/// Compiles one function body onto the end of `code`.
pub(super) fn function(
    m: &Module,
    type_index: u32,
    mut r: Reader,
    code: &mut Vec<Op>,
    targets: &mut Vec<Target>,
    floats: &mut Vec<bool>,
    work: &mut u64,
) -> R<Code> {
    let ty = &m.types[type_index as usize];
    let mut locals = ty.params.to_vec();
    let mut declared = 0u64;
    for _ in 0..r.count()? {
        let n = r.u32()? as u64;
        let t = val_type(r.u8()?)?;
        declared += n;
        if declared > MAX_LOCALS {
            return Err(Error::new("too many locals"));
        }
        if floats.len() + locals.len() + n as usize > MAX_MODULE_LOCALS {
            return Err(Error::new("too many locals in the module"));
        }
        locals.extend(std::iter::repeat_n(t, n as usize));
    }
    let start = code.len() as u32;
    let floats_at = floats.len() as u32;
    floats.extend(locals.iter().map(|t| matches!(t, Ty::F32 | Ty::F64)));
    let zeroed = (locals.len() - ty.params.len()) as u64;
    let mut v = V {
        m,
        code,
        targets,
        locals,
        vals: Vec::new(),
        ctrls: Vec::new(),
        max: 0,
        last: None,
        cost: 0,
        work,
    };
    v.emit(Op::Fuel(0));
    v.ctrls.push(Ctrl {
        kind: Kind::Func,
        bt: BlockType::Func(type_index),
        height: 0,
        unreachable: false,
        start,
        patches: Vec::new(),
        if_jump: None,
        cost: 0,
    });
    while !v.ctrls.is_empty() {
        let op = r.u8()?;
        v.op(op, &mut r)?;
        if *v.work > MAX_WORK {
            return Err(Error::new("the module is too complex to compile"));
        }
    }
    if !r.done() {
        return Err(Error::new("operators after the function's end"));
    }
    v.code[start as usize] = Op::Fuel(fuel(v.cost + zeroed));
    Ok(Code {
        start,
        params: ty.params.len() as u32,
        locals: v.locals.len() as u32,
        frame: (v.locals.len() + v.max) as u32,
        floats: floats_at,
    })
}

impl<'a> V<'a> {
    fn emit(&mut self, op: Op) -> u32 {
        self.cost += match op {
            Op::Move { n, .. } | Op::Return { n, .. } => (n as u64).max(1),
            Op::BrTable { start, .. } => 1 + self.targets[start as usize].n as u64,
            _ => 1,
        };
        *self.work += 1;
        self.code.push(op);
        self.last = None;
        self.code.len() as u32 - 1
    }

    fn pc(&self) -> u32 {
        self.code.len() as u32
    }

    /// The frame slot of operand stack index `i`.
    fn slot(&self, i: usize) -> S {
        (self.locals.len() + i) as S
    }

    /// Whether the op just emitted computed operand `o` at index `i` — so it can be retargeted or fused.
    fn just_made(&self, o: Opd, i: usize) -> bool {
        o.src == Src::Stack && self.last == Some((self.code.len().wrapping_sub(1), i))
    }

    fn params(&self, bt: BlockType) -> &'a [Ty] {
        match bt {
            BlockType::Func(i) => &self.m.types[i as usize].params,
            _ => &[],
        }
    }

    fn results(&self, bt: BlockType) -> &'a [Ty] {
        match bt {
            BlockType::Empty => &[],
            BlockType::One(t) => one(t),
            BlockType::Func(i) => &self.m.types[i as usize].results,
        }
    }

    fn push(&mut self, o: Opd) -> R<()> {
        self.vals.push(o);
        if self.vals.len() > self.max {
            self.max = self.vals.len();
            if self.locals.len() + self.max > S::MAX as usize {
                return Err(Error::new("function too large (more than 65535 slots)"));
            }
        }
        Ok(())
    }

    fn push_stack(&mut self, ts: &[Ty]) -> R<()> {
        ts.iter().try_for_each(|&t| self.push(Opd { ty: Some(t), src: Src::Stack }))
    }

    /// Emits an op that computes a new top operand into its stack slot.
    fn produce(&mut self, ty: Ty, op: impl FnOnce(S) -> Op) -> R<()> {
        let i = self.vals.len();
        self.push(Opd { ty: Some(ty), src: Src::Stack })?;
        let pc = self.emit(op(self.slot(i)));
        self.last = Some((pc as usize, i));
        Ok(())
    }

    /// Pops an operand: it and the stack index it had.
    fn pop(&mut self) -> R<(Opd, usize)> {
        let c = self.ctrls.last().expect("inside a block");
        if self.vals.len() == c.height {
            return if c.unreachable {
                Ok((Opd { ty: None, src: Src::Stack }, self.vals.len()))
            } else {
                Err(mismatch())
            };
        }
        let o = self.vals.pop().expect("above the block's height");
        Ok((o, self.vals.len()))
    }

    fn pop_ty(&mut self, want: Ty) -> R<(Opd, usize)> {
        let (o, i) = self.pop()?;
        if o.ty.is_some_and(|t| t != want) {
            return Err(mismatch());
        }
        Ok((o, i))
    }

    /// Pops `ts` (checking each), bottom first.
    fn pop_all(&mut self, ts: &[Ty]) -> R<Vec<Opd>> {
        *self.work += ts.len() as u64;
        let mut out = vec![Opd { ty: None, src: Src::Stack }; ts.len()];
        for (i, &t) in ts.iter().enumerate().rev() {
            out[i] = self.pop_ty(t)?.0;
        }
        Ok(out)
    }

    /// Checks the top operands against `ts`, leaving them in place.
    fn peek_all(&mut self, ts: &[Ty]) -> R<()> {
        let got = self.pop_all(ts)?;
        got.into_iter().try_for_each(|o| self.push(o))
    }

    /// The slot an operand can be read from — a constant is written to its stack slot first.
    fn read(&mut self, o: Opd, i: usize) -> S {
        match o.src {
            Src::Stack => self.slot(i),
            Src::Local(x) => x,
            Src::Const(v) => {
                let dst = self.slot(i);
                self.emit(Op::Const { dst, v });
                dst
            }
        }
    }

    fn materialize(&mut self, i: usize) {
        let dst = self.slot(i);
        match self.vals[i].src {
            Src::Stack => return,
            Src::Local(x) => self.emit(Op::Copy { dst, src: x }),
            Src::Const(v) => self.emit(Op::Const { dst, v }),
        };
        self.vals[i].src = Src::Stack;
    }

    /// Puts the top `n` operands (those of the current block) in their stack slots.
    fn materialize_top(&mut self, n: usize) {
        let floor = self.ctrls.last().expect("inside a block").height;
        for i in self.vals.len().saturating_sub(n).max(floor)..self.vals.len() {
            self.materialize(i);
        }
    }

    /// Copies out operands still naming local `x` (all locals if `None`) — before `x` is written, or before
    /// a block, whose code may overwrite a local on some paths only.
    fn materialize_locals(&mut self, x: Option<S>) {
        *self.work += self.vals.len() as u64;
        for i in 0..self.vals.len() {
            if let Src::Local(y) = self.vals[i].src
                && x.is_none_or(|x| x == y)
            {
                self.materialize(i);
            }
        }
    }

    fn unreachable(&mut self) {
        let c = self.ctrls.last_mut().expect("inside a block");
        self.vals.truncate(c.height);
        c.unreachable = true;
        self.last = None;
    }

    /// The ctrl a branch of depth `l` targets, and the types it carries.
    fn label(&self, l: u32) -> R<(usize, &'a [Ty])> {
        let i = self.ctrls.len().checked_sub(1 + l as usize).ok_or_else(|| Error::new("unknown label"))?;
        let c = &self.ctrls[i];
        Ok((i, if c.kind == Kind::Loop { self.params(c.bt) } else { self.results(c.bt) }))
    }

    /// Where a branch to ctrl `i` goes (a loop's head is known; a block's end comes later).
    fn to(&self, i: usize) -> u32 {
        let c = &self.ctrls[i];
        if c.kind == Kind::Loop { c.start } else { LATER }
    }

    fn jump_to(&mut self, i: usize, op: Op) {
        let at = self.emit(op);
        if self.to(i) == LATER {
            self.ctrls[i].patches.push(Patch::Op(at));
        }
    }

    fn patch(&mut self, patches: Vec<Patch>, to: u32) {
        for p in patches {
            match p {
                Patch::Table(i) => self.targets[i as usize].to = to,
                Patch::Op(at) => *self.code[at as usize].target_mut().expect("only jumps are patched") = to,
            }
        }
    }

    /// The move that puts the top `n` operands where the label of ctrl `i` wants them, if they aren't.
    fn move_for(&self, i: usize, n: usize) -> Option<Op> {
        let src = self.slot(self.vals.len().saturating_sub(n));
        let dst = self.slot(self.ctrls[i].height);
        (n > 0 && src != dst).then_some(Op::Move { dst, src, n: n as S })
    }

    /// Emits a conditional jump on operand `c` (index `ci`): fused with the comparison that just computed
    /// it when it can be. `negate`: jump when the condition is false.
    fn jump_if(&mut self, c: Opd, ci: usize, fusable: bool, to: u32, negate: bool) -> u32 {
        if fusable && let Some(f) = self.code.last().and_then(|op| op.fused(to, negate)) {
            self.code.pop();
            return self.emit(f);
        }
        let c = self.read(c, ci);
        self.emit(if negate { Op::JumpIfNot { c, to } } else { Op::JumpIf { c, to } })
    }

    fn block_type(&self, r: &mut Reader) -> R<BlockType> {
        let t = r.s33()?;
        Ok(match t {
            -0x40 => BlockType::Empty,
            // One-byte encodings only (-64..0): `0xFF 0x7E` isn't `i32`
            i if (-0x40..0).contains(&i) => BlockType::One(val_type((i & 0x7F) as u8)?),
            i if i < 0 => return Err(Error::new("malformed block type")),
            i => {
                if i as usize >= self.m.types.len() {
                    return Err(Error::new("unknown type"));
                }
                BlockType::Func(i as u32)
            }
        })
    }

    fn memarg(&self, r: &mut Reader, natural: u32) -> R<u32> {
        self.memory()?;
        let align = r.u32()?;
        if align >= 32 {
            return Err(Error::new("multiple memories are not supported"));
        }
        if align > natural {
            return Err(Error::new("alignment must not be larger than natural"));
        }
        r.u32()
    }

    fn memory(&self) -> R<()> {
        match self.m.memory {
            Some(_) => Ok(()),
            None => Err(Error::new("unknown memory 0")),
        }
    }

    fn zero_byte(r: &mut Reader) -> R<()> {
        if r.u8()? != 0 {
            return Err(Error::new("zero byte expected"));
        }
        Ok(())
    }

    fn table(&self, t: u32) -> R<Ty> {
        self.m.tables.get(t as usize).map(|t| t.elem).ok_or_else(|| Error::new("unknown table"))
    }

    fn data(&self, d: u32) -> R<()> {
        match self.m.data_count {
            None => Err(Error::new("data count section required")),
            Some(n) if d >= n => Err(Error::new("unknown data segment")),
            _ => Ok(()),
        }
    }

    fn elem(&self, e: u32) -> R<Ty> {
        self.m.elems.get(e as usize).map(|e| e.ty).ok_or_else(|| Error::new("unknown elem segment"))
    }

    /// Pops three operands (the bulk ops' dst, src/value, len) and returns their slots.
    fn three(&mut self, second: Ty) -> R<(S, S, S)> {
        let (n, ni) = self.pop_ty(Ty::I32)?;
        let (s, si) = self.pop_ty(second)?;
        let (d, di) = self.pop_ty(Ty::I32)?;
        Ok((self.read(d, di), self.read(s, si), self.read(n, ni)))
    }

    /// `block`, `loop`, `if`.
    fn enter(&mut self, op: u8, r: &mut Reader) -> R<()> {
        let bt = self.block_type(r)?;
        let cond = if op == 0x04 { Some(self.pop_ty(Ty::I32)?) } else { None };
        let fusable = cond.is_some_and(|(c, ci)| self.just_made(c, ci));
        let params = self.params(bt);
        self.peek_all(params)?;
        // Every way into and out of the block must agree on where values are: the params go in their
        // slots, and operands still naming locals are copied out.
        let before = self.code.len();
        self.materialize_locals(None);
        let n = params.len().min(self.vals.len());
        for i in self.vals.len() - n..self.vals.len() {
            self.materialize(i);
        }
        let fusable = fusable && self.code.len() == before;
        let mut c = Ctrl {
            kind: [Kind::Block, Kind::Loop, Kind::If][(op - 0x02) as usize],
            bt,
            height: self.vals.len() - n,
            unreachable: false,
            start: self.pc(),
            patches: Vec::new(),
            if_jump: None,
            cost: self.cost,
        };
        match c.kind {
            Kind::Loop => drop(self.emit(Op::Fuel(0))),
            Kind::If => {
                let (o, i) = cond.expect("an if has a condition");
                c.if_jump = Some(self.jump_if(o, i, fusable, LATER, true));
            }
            _ => {}
        }
        self.last = None;
        self.ctrls.push(c);
        Ok(())
    }

    fn op(&mut self, op: u8, r: &mut Reader) -> R<()> {
        use Ty::*;
        match op {
            0x00 => {
                self.emit(Op::Unreachable);
                self.unreachable();
            }
            0x01 => {}
            0x02..=0x04 => self.enter(op, r)?,
            0x05 => {
                let c = self.ctrls.last().expect("inside a block");
                if c.kind != Kind::If {
                    return Err(Error::new("else without if"));
                }
                let (bt, height) = (c.bt, c.height);
                let results = self.results(bt);
                self.materialize_top(results.len());
                self.pop_all(results)?;
                if self.vals.len() != height {
                    return Err(mismatch());
                }
                let jump = self.emit(Op::Jump(LATER));
                let here = self.pc();
                let c = self.ctrls.last_mut().expect("inside a block");
                c.patches.push(Patch::Op(jump));
                let if_jump = c.if_jump.take().expect("an if has its jump");
                c.kind = Kind::Else;
                c.unreachable = false;
                self.patch(vec![Patch::Op(if_jump)], here);
                self.push_stack(self.params(bt))?;
                self.last = None;
            }
            0x0B => {
                let c = self.ctrls.last().expect("inside a block");
                let (bt, height) = (c.bt, c.height);
                let results = self.results(bt);
                self.materialize_top(results.len());
                self.pop_all(results)?;
                if self.vals.len() != height {
                    return Err(mismatch());
                }
                let mut c = self.ctrls.pop().expect("inside a block");
                if let Some(j) = c.if_jump.take() {
                    // `if` without `else`: the missing else passes the params through as the results.
                    if self.params(bt) != results {
                        return Err(mismatch());
                    }
                    c.patches.push(Patch::Op(j));
                }
                let here = self.pc();
                match c.kind {
                    Kind::Loop => self.code[c.start as usize] = Op::Fuel(fuel(self.cost - c.cost)),
                    Kind::Func => {
                        self.patch(c.patches, here);
                        let src = self.slot(0);
                        self.emit(Op::Return { src, n: results.len() as S });
                        return Ok(());
                    }
                    _ => self.patch(c.patches, here),
                }
                self.push_stack(results)?;
                self.last = None;
            }
            0x0C => {
                let (i, tys) = self.label(r.u32()?)?;
                let n = tys.len();
                self.materialize_top(n);
                self.peek_all(tys)?;
                if self.ctrls[i].kind == Kind::Func {
                    let src = self.slot(self.vals.len().saturating_sub(n));
                    self.emit(Op::Return { src, n: n as S });
                } else {
                    if let Some(mv) = self.move_for(i, n) {
                        self.emit(mv);
                    }
                    self.jump_to(i, Op::Jump(self.to(i)));
                }
                self.unreachable();
            }
            0x0D => {
                let (c, ci) = self.pop_ty(I32)?;
                let fusable = self.just_made(c, ci);
                let (i, tys) = self.label(r.u32()?)?;
                let n = tys.len();
                let before = self.code.len();
                self.materialize_top(n);
                let fusable = fusable && self.code.len() == before;
                self.pop_all(tys)?;
                self.push_stack(tys)?;
                let func = self.ctrls[i].kind == Kind::Func;
                let mv = if func { None } else { self.move_for(i, n) };
                if !func && mv.is_none() {
                    // Nothing to move: one conditional jump.
                    let at = self.jump_if(c, ci, fusable, self.to(i), false);
                    if self.to(i) == LATER {
                        self.ctrls[i].patches.push(Patch::Op(at));
                    }
                } else {
                    let skip = self.jump_if(c, ci, fusable, LATER, true);
                    if func {
                        let src = self.slot(self.vals.len().saturating_sub(n));
                        self.emit(Op::Return { src, n: n as S });
                    } else {
                        self.emit(mv.expect("a move"));
                        self.jump_to(i, Op::Jump(self.to(i)));
                    }
                    let here = self.pc();
                    self.patch(vec![Patch::Op(skip)], here);
                }
                self.last = None;
            }
            0x0E => {
                let count = r.count()?;
                let mut labels = Vec::with_capacity(count + 1);
                for _ in 0..=count {
                    labels.push(r.u32()?);
                }
                let (c, ci) = self.pop_ty(I32)?;
                let n = self.label(labels[count])?.1.len();
                self.materialize_top(n);
                let cs = self.read(c, ci);
                let start = self.targets.len() as u32;
                for &l in &labels {
                    let (i, tys) = self.label(l)?;
                    if tys.len() != n {
                        return Err(mismatch());
                    }
                    self.peek_all(tys)?;
                    let to = self.to(i);
                    if to == LATER {
                        self.ctrls[i].patches.push(Patch::Table(self.targets.len() as u32));
                    }
                    let src = self.slot(self.vals.len().saturating_sub(n));
                    let dst = self.slot(self.ctrls[i].height);
                    self.targets.push(Target { to, dst, src, n: n as S });
                }
                self.emit(Op::BrTable { c: cs, start, len: count as u32 });
                self.pop_all(self.label(labels[count])?.1)?;
                self.unreachable();
            }
            0x0F => {
                let results = self.results(self.ctrls[0].bt);
                self.materialize_top(results.len());
                let src = self.slot(self.vals.len().saturating_sub(results.len()));
                self.pop_all(results)?;
                self.emit(Op::Return { src, n: results.len() as S });
                self.unreachable();
            }
            0x10 => {
                let f = r.u32()?;
                let t = self.m.func_ty.get(f as usize).ok_or_else(|| Error::new("unknown function"))?;
                let t = &self.m.types[*t as usize];
                self.materialize_top(t.params.len());
                self.pop_all(&t.params)?;
                let at = self.slot(self.vals.len());
                self.emit(Op::Call { at, f });
                self.push_stack(&t.results)?;
            }
            0x11 => {
                let ti = r.u32()?;
                let table = r.u32()?;
                if self.table(table)? != FuncRef {
                    return Err(mismatch());
                }
                let t = self.m.types.get(ti as usize).ok_or_else(|| Error::new("unknown type"))?;
                let (c, ci) = self.pop_ty(I32)?;
                self.materialize_top(t.params.len());
                let cs = self.read(c, ci);
                self.pop_all(&t.params)?;
                let at = self.slot(self.vals.len());
                self.emit(Op::CallIndirect { at, c: cs, ty: self.m.canon[ti as usize], table });
                self.push_stack(&t.results)?;
            }
            0x1A => drop(self.pop()?),
            0x1B | 0x1C => {
                let typed = if op == 0x1C {
                    if r.u32()? != 1 {
                        return Err(Error::new("invalid result arity"));
                    }
                    Some(val_type(r.u8()?)?)
                } else {
                    None
                };
                let (c, ci) = self.pop_ty(I32)?;
                let (b, bi) = self.pop()?;
                let (a, ai) = self.pop()?;
                let ty = match typed {
                    Some(t) => {
                        if [a.ty, b.ty].into_iter().flatten().any(|u| u != t) {
                            return Err(mismatch());
                        }
                        Some(t)
                    }
                    None => {
                        if [a.ty, b.ty].into_iter().flatten().any(|t| matches!(t, FuncRef | ExternRef)) {
                            return Err(mismatch());
                        }
                        if a.ty.is_some() && b.ty.is_some() && a.ty != b.ty {
                            return Err(mismatch());
                        }
                        a.ty.or(b.ty)
                    }
                };
                let (a, b, c) = (self.read(a, ai), self.read(b, bi), self.read(c, ci));
                match ty {
                    Some(t) => self.produce(t, |dst| Op::Select { dst, a, b, c })?,
                    None => {
                        let dst = self.slot(ai);
                        self.push(Opd { ty: None, src: Src::Stack })?;
                        self.emit(Op::Select { dst, a, b, c });
                    }
                }
            }
            0x20..=0x22 => {
                let x = r.u32()?;
                let t = *self.locals.get(x as usize).ok_or_else(|| Error::new("unknown local"))?;
                let x = x as S;
                if op == 0x20 {
                    return self.push(Opd { ty: Some(t), src: Src::Local(x) });
                }
                let (v, vi) = self.pop_ty(t)?;
                if v.src != Src::Local(x) {
                    let retarget = self.just_made(v, vi);
                    let before = self.code.len();
                    self.materialize_locals(Some(x));
                    let dst = if retarget && self.code.len() == before {
                        self.code.last_mut().and_then(|o| o.dst_mut())
                    } else {
                        None
                    };
                    match (dst, v.src) {
                        (Some(d), _) => *d = x,
                        (None, Src::Const(c)) => drop(self.emit(Op::Const { dst: x, v: c })),
                        (None, Src::Local(y)) => drop(self.emit(Op::Copy { dst: x, src: y })),
                        (None, Src::Stack) => {
                            let src = self.slot(vi);
                            self.emit(Op::Copy { dst: x, src });
                        }
                    }
                }
                self.last = None;
                if op == 0x22 {
                    self.push(Opd { ty: Some(t), src: Src::Local(x) })?;
                }
            }
            0x23 | 0x24 => {
                let g = r.u32()?;
                let global = self.m.globals.get(g as usize).ok_or_else(|| Error::new("unknown global"))?;
                if op == 0x23 {
                    self.produce(global.ty, |dst| Op::GlobalGet { dst, g })?;
                } else {
                    if !global.mutable {
                        return Err(Error::new("global is immutable"));
                    }
                    let (v, vi) = self.pop_ty(global.ty)?;
                    let src = self.read(v, vi);
                    self.emit(Op::GlobalSet { src, g });
                }
            }
            0x25 => {
                let t = r.u32()?;
                let et = self.table(t)?;
                let (i, ii) = self.pop_ty(I32)?;
                let i = self.read(i, ii);
                self.produce(et, |dst| Op::TableGet { dst, i, t })?;
            }
            0x26 => {
                let t = r.u32()?;
                let et = self.table(t)?;
                let (v, vi) = self.pop_ty(et)?;
                let (i, ii) = self.pop_ty(I32)?;
                let (i, v) = (self.read(i, ii), self.read(v, vi));
                self.emit(Op::TableSet { i, v, t });
            }
            0x28..=0x35 => {
                type Mk = fn(S, S, u32) -> Op;
                let (natural, ty, mk): (u32, Ty, Mk) = match op {
                    0x28 => (2, I32, |dst, addr, off| Op::Load32U { dst, addr, off }),
                    0x29 => (3, I64, |dst, addr, off| Op::Load64 { dst, addr, off }),
                    0x2A => (2, F32, |dst, addr, off| Op::Load32U { dst, addr, off }),
                    0x2B => (3, F64, |dst, addr, off| Op::Load64 { dst, addr, off }),
                    0x2C => (0, I32, |dst, addr, off| Op::Load8S32 { dst, addr, off }),
                    0x2D => (0, I32, |dst, addr, off| Op::Load8U { dst, addr, off }),
                    0x2E => (1, I32, |dst, addr, off| Op::Load16S32 { dst, addr, off }),
                    0x2F => (1, I32, |dst, addr, off| Op::Load16U { dst, addr, off }),
                    0x30 => (0, I64, |dst, addr, off| Op::Load8S64 { dst, addr, off }),
                    0x31 => (0, I64, |dst, addr, off| Op::Load8U { dst, addr, off }),
                    0x32 => (1, I64, |dst, addr, off| Op::Load16S64 { dst, addr, off }),
                    0x33 => (1, I64, |dst, addr, off| Op::Load16U { dst, addr, off }),
                    0x34 => (2, I64, |dst, addr, off| Op::Load32S64 { dst, addr, off }),
                    _ => (2, I64, |dst, addr, off| Op::Load32U { dst, addr, off }),
                };
                let off = self.memarg(r, natural)?;
                let (a, ai) = self.pop_ty(I32)?;
                let addr = self.read(a, ai);
                self.produce(ty, |dst| mk(dst, addr, off))?;
            }
            0x36..=0x3E => {
                type Mk = fn(S, S, u32) -> Op;
                let (natural, ty, mk): (u32, Ty, Mk) = match op {
                    0x36 => (2, I32, |addr, val, off| Op::Store32 { addr, val, off }),
                    0x37 => (3, I64, |addr, val, off| Op::Store64 { addr, val, off }),
                    0x38 => (2, F32, |addr, val, off| Op::Store32 { addr, val, off }),
                    0x39 => (3, F64, |addr, val, off| Op::Store64 { addr, val, off }),
                    0x3A => (0, I32, |addr, val, off| Op::Store8 { addr, val, off }),
                    0x3B => (1, I32, |addr, val, off| Op::Store16 { addr, val, off }),
                    0x3C => (0, I64, |addr, val, off| Op::Store8 { addr, val, off }),
                    0x3D => (1, I64, |addr, val, off| Op::Store16 { addr, val, off }),
                    _ => (2, I64, |addr, val, off| Op::Store32 { addr, val, off }),
                };
                let off = self.memarg(r, natural)?;
                let (v, vi) = self.pop_ty(ty)?;
                let (a, ai) = self.pop_ty(I32)?;
                let (addr, val) = (self.read(a, ai), self.read(v, vi));
                self.emit(mk(addr, val, off));
            }
            0x3F => {
                Self::zero_byte(r)?;
                self.memory()?;
                self.produce(I32, |dst| Op::MemorySize { dst })?;
            }
            0x40 => {
                Self::zero_byte(r)?;
                self.memory()?;
                let (n, ni) = self.pop_ty(I32)?;
                let n = self.read(n, ni);
                self.produce(I32, |dst| Op::MemoryGrow { dst, n })?;
            }
            0x41 => self.push(Opd { ty: Some(I32), src: Src::Const(r.s32()? as u32 as u64) })?,
            0x42 => self.push(Opd { ty: Some(I64), src: Src::Const(r.s64()? as u64) })?,
            0x43 => self.push(Opd { ty: Some(F32), src: Src::Const(r.f32()? as u64) })?,
            0x44 => self.push(Opd { ty: Some(F64), src: Src::Const(r.f64()?) })?,
            0xBC..=0xBF => {
                // Reinterpret: same bits, new type.
                let (from, to) = [(F32, I32), (F64, I64), (I32, F32), (I64, F64)][(op - 0xBC) as usize];
                let (o, _) = self.pop_ty(from)?;
                self.push(Opd { ty: Some(to), src: o.src })?;
            }
            0x45..=0xC4 => {
                let (shape, ty, out) = numeric(op).ok_or_else(|| Error::new("unknown instruction"))?;
                self.arith(shape, ty, out)?;
            }
            0xD0 => {
                let t = ref_type(r.u8()?)?;
                self.push(Opd { ty: Some(t), src: Src::Const(NULL) })?;
            }
            0xD1 => {
                let (a, ai) = self.pop()?;
                if a.ty.is_some_and(|t| !matches!(t, FuncRef | ExternRef)) {
                    return Err(mismatch());
                }
                let a = self.read(a, ai);
                self.produce(I32, |dst| Op::RefIsNull { dst, a })?;
            }
            0xD2 => {
                let f = r.u32()?;
                if f as usize >= self.m.func_ty.len() {
                    return Err(Error::new("unknown function"));
                }
                if !self.m.declared.get(f as usize).is_some_and(|&d| d) {
                    return Err(Error::new("undeclared function reference"));
                }
                self.push(Opd { ty: Some(FuncRef), src: Src::Const(f as u64 + 1) })?;
            }
            0xFC => self.prefixed(r)?,
            0x12 | 0x13 | 0x15 => return Err(Error::new("tail calls are not supported")),
            0x14 | 0xD3..=0xD6 | 0xFB => {
                return Err(Error::new("typed function references and GC are not supported"));
            }
            0x06..=0x0A | 0x18 | 0x19 | 0x1F => return Err(Error::new("exceptions are not supported")),
            0xFD => return Err(Error::new("SIMD is not supported")),
            0xFE => return Err(Error::new("threads are not supported")),
            _ => return Err(Error::new(format!("unknown instruction 0x{op:02x}"))),
        }
        Ok(())
    }

    fn arith(&mut self, shape: Shape, ty: Ty, out: Ty) -> R<()> {
        match shape {
            Shape::Un(mk) => {
                let (a, ai) = self.pop_ty(ty)?;
                let a = self.read(a, ai);
                self.produce(out, |dst| mk(dst, a))
            }
            Shape::Bin(mk) => {
                let (b, bi) = self.pop_ty(ty)?;
                let (a, ai) = self.pop_ty(ty)?;
                let imm = match (b.src, ty) {
                    (Src::Const(v), Ty::I32) => Some(v as u32 as i32),
                    (Src::Const(v), Ty::I64) => i32::try_from(v as i64).ok(),
                    _ => None,
                };
                let a = self.read(a, ai);
                if let Some(op) = imm.and_then(|imm| mk(0, a, 0).with_imm(imm)) {
                    return self.produce(out, |dst| {
                        let mut op = op;
                        *op.dst_mut().expect("an immediate form writes a slot") = dst;
                        op
                    });
                }
                let b = self.read(b, bi);
                self.produce(out, |dst| mk(dst, a, b))
            }
        }
    }

    fn prefixed(&mut self, r: &mut Reader) -> R<()> {
        use Ty::*;
        let sub = r.u32()?;
        match sub {
            0..=7 => {
                let (shape, ty, out) = saturating(sub).expect("the table covers 0..=7");
                self.arith(shape, ty, out)?;
            }
            8 => {
                let seg = r.u32()?;
                Self::zero_byte(r)?;
                self.data(seg)?;
                self.memory()?;
                let (d, s, n) = self.three(I32)?;
                self.emit(Op::MemoryInit { d, s, n, seg });
            }
            9 => {
                let d = r.u32()?;
                self.data(d)?;
                self.emit(Op::DataDrop(d));
            }
            10 | 11 => {
                Self::zero_byte(r)?;
                if sub == 10 {
                    Self::zero_byte(r)?;
                }
                self.memory()?;
                let (d, s, n) = self.three(I32)?;
                self.emit(if sub == 10 { Op::MemoryCopy { d, s, n } } else { Op::MemoryFill { d, v: s, n } });
            }
            12 => {
                let elem = r.u32()?;
                let table = r.u32()?;
                if self.elem(elem)? != self.table(table)? {
                    return Err(mismatch());
                }
                let (d, s, n) = self.three(I32)?;
                self.emit(Op::TableInit { d, s, n, table, elem });
            }
            13 => {
                let e = r.u32()?;
                self.elem(e)?;
                self.emit(Op::ElemDrop(e));
            }
            14 => {
                let into = r.u32()?;
                let from = r.u32()?;
                if self.table(into)? != self.table(from)? {
                    return Err(mismatch());
                }
                let (d, s, n) = self.three(I32)?;
                self.emit(Op::TableCopy { d, s, n, into, from });
            }
            15 => {
                let t = r.u32()?;
                let et = self.table(t)?;
                let (n, ni) = self.pop_ty(I32)?;
                let (init, ii) = self.pop_ty(et)?;
                let (init, n) = (self.read(init, ii), self.read(n, ni));
                self.produce(I32, |dst| Op::TableGrow { dst, init, n, t })?;
            }
            16 => {
                let t = r.u32()?;
                self.table(t)?;
                self.produce(I32, |dst| Op::TableSize { dst, t })?;
            }
            17 => {
                let t = r.u32()?;
                let et = self.table(t)?;
                let (i, v, n) = self.three(et)?;
                self.emit(Op::TableFill { i, v, n, t });
            }
            _ => return Err(Error::new(format!("unknown instruction 0xfc {sub}"))),
        }
        Ok(())
    }
}
