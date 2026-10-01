//! Register ops → x86-64, one op at a time — the same scheme as the AArch64 backend (frame slots behind a
//! per-block register cache, written back at branches, calls and helpers), with x86's shapes:
//!
//! - Two-operand arithmetic: the destination is also the first operand, so results go through `rax` unless
//!   the destination already holds that operand.
//! - `div`/`idiv` use `rdx:rax`, shifts take their count in `cl`: `rax`, `rcx`, `rdx` are scratch and never
//!   cached. `idiv` faults on `MIN / -1`, so both that and `MIN % -1` are handled before it.
//! - SSE's `minss`/`maxss` and float→int conversions don't match wasm on NaN, ±0 and out-of-range values,
//!   so those get explicit sequences.
//! - Only baseline x86-64 and SSE4.1 (`roundss`): population count and leading/trailing zeros are computed
//!   without POPCNT/LZCNT/TZCNT.
//! - The cache's float side is `xmm2`–`xmm11` (`xmm0`/`xmm1` are scratch). The SysV ABI keeps no xmm
//!   register across calls, so float homes (`xmm12`–`xmm15`) are all written back and reloaded around them.
//!
//! Pinned (callee-saved, so helpers keep them): `rbx` frame, `r12` context, `r13` memory base, `r14` fuel
//! left. Memory length, globals, call depth and the value stack's end are read from the context, which
//! leaves `rbp` and `r15` for home registers.

use super::super::super::Trap;
use super::super::super::compile::{Code, Op, S};
use super::super::super::module::Module;
use super::super::{branch_targets, busiest_loop_locals, off, trap_code};
use super::asm::*;

const FR: Reg = RBX;
const CTX: Reg = R12;
const MEM: Reg = R13;
const FUEL: Reg = R14;
/// Registers the slot cache hands out (caller-saved: helpers and calls clobber them).
const POOL: [Reg; 4] = [RSI, RDI, R8, R9];
/// Home registers for a function's busiest loop locals (see the AArch64 backend): callee-saved first,
/// then caller-saved.
const HOMES: [(Reg, bool); 4] = [(RBP, true), (R15, true), (R10, false), (R11, false)];
/// Float registers the slot cache hands out.
const FPOOL: [Reg; 10] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// Home registers for float locals (caller-saved, like every xmm register).
const FHOMES: [Reg; 4] = [12, 13, 14, 15];

/// A local living in a register for the whole function (see the AArch64 backend).
#[derive(Clone, Copy)]
struct Home {
    slot: S,
    reg: Reg,
    callee: bool,
    float: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stub {
    Trap(Trap),
    /// Out of fuel: zero the counter first.
    Fuel,
    /// A helper recorded the trap in the context.
    Ctx,
    /// A helper returned the trap code in `rax`.
    Rax,
}

#[derive(Clone, Copy)]
enum Label {
    Ir(u32),
    Stub(Stub),
}

#[derive(Clone, Copy)]
enum Kind {
    /// A `jmp`/`jcc`/`call` displacement.
    Rel32,
    /// A jump table entry: offset from the table's start.
    Table(usize),
}

struct Fix {
    at: usize,
    kind: Kind,
    to: Label,
}

#[derive(Clone, Copy, Default)]
struct Entry {
    slot: Option<S>,
    dirty: bool,
}

pub(super) struct Out {
    pub code: Vec<u8>,
    pub entry: usize,
    pub funcs: Vec<usize>,
}

struct E<'a> {
    m: &'a Module,
    a: Asm,
    trap_common: usize,
    ir: Vec<usize>,
    labels: Vec<bool>,
    base: u32,
    fixes: Vec<Fix>,
    calls: Vec<(usize, u32)>,
    cache: [Entry; POOL.len()],
    next: usize,
    pinned: u32,
    fcache: [Entry; FPOOL.len()],
    fnext: usize,
    fpinned: u32,
    homes: Vec<Home>,
    /// Moves into home registers of the other kind, due after the current op: (to, from, into xmm).
    pending: Vec<(Reg, Reg, bool)>,
    checked: Vec<(S, u64)>,
    stored: Vec<Stored>,
    /// More code than `MAX_CODE`: the module runs on the interpreter.
    unfit: bool,
}

/// The most machine code a module may compile to (bytes).
const MAX_CODE: usize = 64 << 20;
/// Slot copies longer than this are a loop (else a module could make code grow with its value counts).
const LOOP_COPY: S = 16;

/// Memory at `[addr + off, + 1 << width)` holds the low bytes of slot `val`.
#[derive(Clone, Copy)]
struct Stored {
    addr: S,
    off: u32,
    width: u32,
    val: S,
}

/// `None` if the module would compile to more than `MAX_CODE` — it then runs on the interpreter.
pub(super) fn module(m: &Module) -> Option<Out> {
    let mut e = E {
        m,
        a: Asm::default(),
        trap_common: 0,
        ir: Vec::new(),
        labels: Vec::new(),
        base: 0,
        fixes: Vec::new(),
        calls: Vec::new(),
        cache: [Entry::default(); POOL.len()],
        next: 0,
        pinned: 0,
        fcache: [Entry::default(); FPOOL.len()],
        fnext: 0,
        fpinned: 0,
        homes: Vec::new(),
        pending: Vec::new(),
        checked: Vec::new(),
        stored: Vec::new(),
        unfit: false,
    };
    let entry = e.trampoline();
    let mut funcs = Vec::with_capacity(m.funcs.len());
    for f in 0..m.funcs.len() {
        funcs.push(e.a.len());
        e.func(f);
    }
    if e.unfit {
        return None;
    }
    for &(at, f) in &e.calls {
        e.a.patch_rel32(at, funcs[f as usize]);
    }
    Some(Out { code: e.a.b, entry, funcs })
}

fn slot(s: S) -> M {
    at(FR, s as i32 * 8)
}

fn ctx(off: u32) -> M {
    at(CTX, off as i32)
}

/// Fused compare-and-jump ops → condition.
fn jump_cc(op: &Op) -> Cc {
    use Op::*;
    match op {
        JumpIfEq { .. } | JumpIfEqImm { .. } => Cc::E,
        JumpIfNe { .. } | JumpIfNeImm { .. } => Cc::Ne,
        JumpIfLtS { .. } | JumpIfLtSImm { .. } => Cc::L,
        JumpIfLtU { .. } | JumpIfLtUImm { .. } => Cc::B,
        JumpIfGtS { .. } | JumpIfGtSImm { .. } => Cc::G,
        JumpIfGtU { .. } | JumpIfGtUImm { .. } => Cc::A,
        JumpIfLeS { .. } | JumpIfLeSImm { .. } => Cc::Le,
        JumpIfLeU { .. } | JumpIfLeUImm { .. } => Cc::Be,
        JumpIfGeS { .. } | JumpIfGeSImm { .. } => Cc::Ge,
        _ => Cc::Ae,
    }
}

/// Integer comparison ops → (64-bit, condition).
fn int_cmp(op: &Op) -> Option<(bool, Cc)> {
    use Op::*;
    Some(match op {
        I32Eq { .. } | I32EqImm { .. } => (false, Cc::E),
        I32Ne { .. } | I32NeImm { .. } => (false, Cc::Ne),
        I32LtS { .. } | I32LtSImm { .. } => (false, Cc::L),
        I32LtU { .. } | I32LtUImm { .. } => (false, Cc::B),
        I32GtS { .. } | I32GtSImm { .. } => (false, Cc::G),
        I32GtU { .. } | I32GtUImm { .. } => (false, Cc::A),
        I32LeS { .. } | I32LeSImm { .. } => (false, Cc::Le),
        I32LeU { .. } | I32LeUImm { .. } => (false, Cc::Be),
        I32GeS { .. } | I32GeSImm { .. } => (false, Cc::Ge),
        I32GeU { .. } | I32GeUImm { .. } => (false, Cc::Ae),
        I64Eq { .. } | I64EqImm { .. } => (true, Cc::E),
        I64Ne { .. } | I64NeImm { .. } => (true, Cc::Ne),
        I64LtS { .. } | I64LtSImm { .. } => (true, Cc::L),
        I64LtU { .. } | I64LtUImm { .. } => (true, Cc::B),
        I64GtS { .. } | I64GtSImm { .. } => (true, Cc::G),
        I64GtU { .. } | I64GtUImm { .. } => (true, Cc::A),
        I64LeS { .. } | I64LeSImm { .. } => (true, Cc::Le),
        I64LeU { .. } | I64LeUImm { .. } => (true, Cc::Be),
        I64GeS { .. } | I64GeSImm { .. } => (true, Cc::Ge),
        I64GeU { .. } | I64GeUImm { .. } => (true, Cc::Ae),
        _ => return None,
    })
}

/// Float → int bounds: (exclusive-or-inclusive low bound, high bound, result on NaN / below / above for the
/// saturating form).
struct Range {
    lo: f64,
    lo_inclusive: bool,
    hi: f64,
    min: u64,
    max: u64,
}

fn range(to_x: bool, signed: bool) -> Range {
    match (to_x, signed) {
        (false, true) => Range {
            lo: -2147483649.0,
            lo_inclusive: false,
            hi: 2147483648.0,
            min: i32::MIN as u32 as u64,
            max: i32::MAX as u64,
        },
        (false, false) => {
            Range { lo: -1.0, lo_inclusive: false, hi: 4294967296.0, min: 0, max: u32::MAX as u64 }
        }
        (true, true) => Range {
            lo: -9223372036854775808.0,
            lo_inclusive: true,
            hi: 9223372036854775808.0,
            min: i64::MIN as u64,
            max: i64::MAX as u64,
        },
        (true, false) => {
            Range { lo: -1.0, lo_inclusive: false, hi: 18446744073709551616.0, min: 0, max: u64::MAX }
        }
    }
}

impl E<'_> {
    // ── The slot cache (as in the AArch64 backend) ──────────────────────────

    fn find(&self, s: S) -> Option<usize> {
        self.cache.iter().position(|e| e.slot == Some(s))
    }

    fn pin(&mut self, i: usize) -> Reg {
        self.pinned |= 1 << i;
        POOL[i]
    }

    fn take(&mut self) -> usize {
        let free = (0..POOL.len()).find(|&i| self.cache[i].slot.is_none() && self.pinned & (1 << i) == 0);
        let i = free.unwrap_or_else(|| {
            loop {
                let i = self.next;
                self.next = (self.next + 1) % POOL.len();
                if self.pinned & (1 << i) == 0 {
                    break i;
                }
            }
        });
        if let Entry { slot: Some(s), dirty: true } = self.cache[i] {
            self.a.store(true, slot(s), POOL[i]);
        }
        self.cache[i] = Entry::default();
        i
    }

    fn home(&self, s: S) -> Option<Home> {
        self.homes.iter().find(|h| h.slot == s).copied()
    }

    fn changed(&mut self, s: S) {
        self.checked.retain(|&(c, _)| c != s);
        self.stored.retain(|e| e.addr != s && e.val != s);
    }

    fn spill_homes(&mut self, all: bool) {
        for h in self.homes.clone() {
            if all || !h.callee {
                if h.float {
                    self.a.fstore(true, slot(h.slot), h.reg);
                } else {
                    self.a.store(true, slot(h.slot), h.reg);
                }
            }
        }
    }

    fn reload_homes(&mut self, all: bool) {
        for h in self.homes.clone() {
            if all || !h.callee {
                if h.float {
                    self.a.fload(true, h.reg, slot(h.slot));
                } else {
                    self.a.load(true, h.reg, slot(h.slot));
                }
            }
        }
    }

    fn callee_homes(&self) -> Vec<Reg> {
        self.homes.iter().filter(|h| h.callee).map(|h| h.reg).collect()
    }

    /// Stack space the prologue reserves so helper calls see a 16-byte-aligned `rsp`, given the callee-saved
    /// homes it pushes.
    fn pad(&self) -> i32 {
        if self.callee_homes().len().is_multiple_of(2) { 8 } else { 0 }
    }

    fn choose_homes(&mut self, c: &Code, end: usize) {
        let floats = &self.m.floats[c.floats as usize..][..c.locals as usize];
        let (mut ints, mut fls) = (HOMES.iter(), FHOMES.iter());
        self.homes.clear();
        for s in busiest_loop_locals(self.m, c.start as usize, end, c.locals) {
            let float = floats[s as usize];
            let next = if float { fls.next().map(|&r| (r, false)) } else { ints.next().copied() };
            if let Some((reg, callee)) = next {
                self.homes.push(Home { slot: s, reg, callee, float });
            }
        }
    }

    fn read(&mut self, s: S) -> Reg {
        if let Some(h) = self.home(s) {
            if !h.float {
                return h.reg;
            }
            let i = self.take();
            self.a.movd_from_xmm(true, POOL[i], h.reg);
            return self.pin(i);
        }
        if let Some(i) = self.find(s) {
            return self.pin(i);
        }
        let i = self.take();
        match self.ffind(s) {
            Some(j) => self.a.movd_from_xmm(true, POOL[i], FPOOL[j]),
            None => self.a.load(true, POOL[i], slot(s)),
        }
        self.cache[i] = Entry { slot: Some(s), dirty: false };
        self.pin(i)
    }

    fn write(&mut self, s: S) -> Reg {
        self.changed(s);
        if let Some(h) = self.home(s) {
            if !h.float {
                return h.reg;
            }
            let i = self.take();
            self.pending.push((h.reg, POOL[i], true));
            return self.pin(i);
        }
        if let Some(j) = self.ffind(s) {
            self.fcache[j] = Entry::default();
        }
        let i = self.find(s).unwrap_or_else(|| self.take());
        self.cache[i] = Entry { slot: Some(s), dirty: true };
        self.pin(i)
    }

    fn settle(&mut self) {
        for (to, from, xmm) in std::mem::take(&mut self.pending) {
            if xmm {
                self.a.movd_to_xmm(true, to, from);
            } else {
                self.a.movd_from_xmm(true, to, from);
            }
        }
    }

    fn flush(&mut self) {
        for (i, &r) in POOL.iter().enumerate() {
            if let Entry { slot: Some(s), dirty: true } = self.cache[i] {
                self.a.store(true, slot(s), r);
                self.cache[i].dirty = false;
            }
        }
        for (j, &r) in FPOOL.iter().enumerate() {
            if let Entry { slot: Some(s), dirty: true } = self.fcache[j] {
                self.a.fstore(true, slot(s), r);
                self.fcache[j].dirty = false;
            }
        }
    }

    fn forget(&mut self) {
        self.checked.clear();
        self.stored.clear();
        self.cache = [Entry::default(); POOL.len()];
        self.fcache = [Entry::default(); FPOOL.len()];
    }

    fn sync(&mut self) {
        self.flush();
        self.forget();
    }

    // ── The float side ──────────────────────────────────────────────────────

    fn ffind(&self, s: S) -> Option<usize> {
        self.fcache.iter().position(|e| e.slot == Some(s))
    }

    fn fpin(&mut self, j: usize) -> Reg {
        self.fpinned |= 1 << j;
        FPOOL[j]
    }

    fn ftake(&mut self) -> usize {
        let free = (0..FPOOL.len()).find(|&j| self.fcache[j].slot.is_none() && self.fpinned & (1 << j) == 0);
        let j = free.unwrap_or_else(|| {
            loop {
                let j = self.fnext;
                self.fnext = (self.fnext + 1) % FPOOL.len();
                if self.fpinned & (1 << j) == 0 {
                    break j;
                }
            }
        });
        if let Entry { slot: Some(s), dirty: true } = self.fcache[j] {
            self.a.fstore(true, slot(s), FPOOL[j]);
        }
        self.fcache[j] = Entry::default();
        j
    }

    /// The xmm register holding `s`, loading (or copying) it there if needed.
    fn fsrc(&mut self, s: S) -> Reg {
        if let Some(h) = self.home(s) {
            if h.float {
                return h.reg;
            }
            let j = self.ftake();
            self.a.movd_to_xmm(true, FPOOL[j], h.reg);
            return self.fpin(j);
        }
        if let Some(j) = self.ffind(s) {
            return self.fpin(j);
        }
        let j = self.ftake();
        match self.find(s) {
            Some(i) => self.a.movd_to_xmm(true, FPOOL[j], POOL[i]),
            None => self.a.fload(true, FPOOL[j], slot(s)),
        }
        self.fcache[j] = Entry { slot: Some(s), dirty: false };
        self.fpin(j)
    }

    /// An xmm register that will hold the new value of `s` (call after reading the operands).
    fn fdst(&mut self, s: S) -> Reg {
        self.changed(s);
        if let Some(h) = self.home(s) {
            if h.float {
                return h.reg;
            }
            let j = self.ftake();
            self.pending.push((h.reg, FPOOL[j], false));
            return self.fpin(j);
        }
        if let Some(i) = self.find(s) {
            self.cache[i] = Entry::default();
        }
        let j = self.ffind(s).unwrap_or_else(|| self.ftake());
        self.fcache[j] = Entry { slot: Some(s), dirty: true };
        self.fpin(j)
    }

    /// A float operand into `xmm{x}` (for sequences that need it in a particular register).
    fn fread(&mut self, x: Reg, s: S) {
        let r = self.fsrc(s);
        if r != x {
            self.a.movaps(x, r);
        }
    }

    fn fwrite(&mut self, x: Reg, s: S) {
        let r = self.fdst(s);
        if r != x {
            self.a.movaps(r, x);
        }
    }

    /// `dst = a op b` for SSE's two-operand forms, whichever registers the three are in.
    fn fop3(&mut self, d: bool, op: u8, rd: Reg, ra: Reg, rb: Reg) {
        if rd == ra {
            self.a.fop(d, op, rd, rb);
        } else if rd == rb {
            self.a.movaps(0, ra);
            self.a.fop(d, op, 0, rb);
            self.a.movaps(rd, 0);
        } else {
            self.a.movaps(rd, ra);
            self.a.fop(d, op, rd, rb);
        }
    }

    /// A slot copied to another, on the side where the value is (as in the AArch64 backend).
    fn copy(&mut self, dst: S, src: S) {
        let float_home = |h: Option<Home>| h.is_some_and(|h| h.float);
        if self.ffind(src).is_some() || float_home(self.home(src)) || float_home(self.home(dst)) {
            let r = self.fsrc(src);
            let d = self.fdst(dst);
            if d != r {
                self.a.movaps(d, r);
            }
        } else {
            let r = self.read(src);
            let d = self.write(dst);
            self.a.mov_rr(true, d, r);
        }
    }

    // ── Branches ────────────────────────────────────────────────────────────

    fn jump(&mut self, cc: Option<Cc>, to: Label) {
        let at = match cc {
            Some(cc) => self.a.jcc(cc),
            None => self.a.jmp(),
        };
        self.fixes.push(Fix { at, kind: Kind::Rel32, to });
    }

    fn trap_if(&mut self, cc: Cc, t: Trap) {
        self.jump(Some(cc), Label::Stub(Stub::Trap(t)));
    }

    /// Binds a forward jump (from `jcc`/`jmp`) to here.
    fn here(&mut self, at: usize) {
        let to = self.a.len();
        self.a.patch_rel32(at, to);
    }

    fn trampoline(&mut self) -> usize {
        let entry = self.a.len();
        for r in [RBP, RBX, R12, R13, R14, R15] {
            self.a.push(r);
        }
        // Six pushes after the return address leave rsp 8 off 16-byte alignment; this makes it aligned, so
        // a `call` lands every function with the usual `rsp % 16 == 8`.
        self.a.alu_ri(Alu::Sub, true, RSP, 8);
        self.a.mov_rr(true, CTX, RDI);
        self.a.mov_rr(true, FR, RSI);
        self.a.store(true, ctx(off::ENTRY_SP), RSP);
        self.a.load(true, MEM, ctx(off::MEM));
        self.a.load(true, FUEL, ctx(off::FUEL));
        self.a.call_r(RDX);
        self.a.store(true, ctx(off::FUEL), FUEL);
        self.a.mov_imm(RAX, 0);
        let exit = self.a.len();
        self.a.alu_ri(Alu::Add, true, RSP, 8);
        for r in [R15, R14, R13, R12, RBX, RBP] {
            self.a.pop(r);
        }
        self.a.ret();
        // Trap exit: eax = code. Unwind to the trampoline's frame and leave through the exit.
        self.trap_common = self.a.len();
        self.a.store(true, ctx(off::FUEL), FUEL);
        self.a.load(true, RSP, ctx(off::ENTRY_SP));
        let j = self.a.jmp();
        self.a.patch_rel32(j, exit);
        entry
    }

    fn func(&mut self, f: usize) {
        let c = self.m.funcs[f];
        let end = self.m.funcs.get(f + 1).map_or(self.m.code.len(), |n| n.start as usize);
        let n = end - c.start as usize;
        self.base = c.start;
        self.ir = vec![0; n + 1];
        self.labels = vec![false; n + 1];
        let mut to = Vec::new();
        for op in &self.m.code[c.start as usize..end] {
            branch_targets(self.m, op, &mut to);
        }
        for t in to {
            self.labels[(t - c.start) as usize] = true;
        }
        self.fixes.clear();
        self.forget();
        self.choose_homes(&c, end);

        // Prologue: callee-saved homes kept, 16-byte alignment for helper calls, value stack room, zeroed
        // locals, homes loaded.
        for r in self.callee_homes() {
            self.a.push(r);
        }
        if self.pad() > 0 {
            self.a.alu_ri(Alu::Sub, true, RSP, self.pad());
        }
        self.a.lea(true, RAX, at(FR, c.frame as i32 * 8));
        self.a.cmp_rm(true, RAX, ctx(off::STACK_END));
        self.trap_if(Cc::A, Trap::StackOverflow);
        let zeros = c.locals - c.params;
        if zeros <= 16 {
            for s in c.params..c.locals {
                self.a.store_imm(true, slot(s as S), 0);
            }
        } else {
            self.a.lea(true, RAX, at(FR, c.params as i32 * 8));
            self.a.mov_imm(RCX, zeros as u64);
            let top = self.a.len();
            self.a.store_imm(true, at(RAX, 0), 0);
            self.a.alu_ri(Alu::Add, true, RAX, 8);
            self.a.alu_ri(Alu::Sub, false, RCX, 1);
            let j = self.a.jcc(Cc::Ne);
            self.a.patch_rel32(j, top);
        }
        for h in self.homes.clone() {
            match ((h.slot as u32) < c.params, h.float) {
                (true, false) => self.a.load(true, h.reg, slot(h.slot)),
                (true, true) => self.a.fload(true, h.reg, slot(h.slot)),
                (false, false) => self.a.mov_imm(h.reg, 0),
                (false, true) => {
                    self.a.mov_imm(RAX, 0);
                    self.a.movd_to_xmm(true, h.reg, RAX);
                }
            }
        }

        for pc in c.start as usize..end {
            if self.labels[pc - c.start as usize] {
                self.sync();
            }
            self.ir[pc - c.start as usize] = self.a.len();
            self.op(pc as u32, self.m.code[pc]);
            self.settle();
            self.pinned = 0;
            self.fpinned = 0;
            if self.a.len() > MAX_CODE {
                self.unfit = true;
                return;
            }
        }
        self.ir[n] = self.a.len();

        // Trap stubs, then patch this function's branches.
        let mut placed: Vec<(Stub, usize)> = Vec::new();
        let used: Vec<Stub> = self
            .fixes
            .iter()
            .filter_map(|f| match f.to {
                Label::Stub(s) => Some(s),
                _ => None,
            })
            .collect();
        for s in used {
            if placed.iter().any(|&(p, _)| p == s) {
                continue;
            }
            placed.push((s, self.a.len()));
            match s {
                Stub::Trap(t) => self.a.mov_imm(RAX, trap_code(t) as u64),
                Stub::Fuel => {
                    self.a.mov_imm(FUEL, 0);
                    self.a.mov_imm(RAX, trap_code(Trap::OutOfFuel) as u64);
                }
                Stub::Ctx => self.a.load(true, RAX, ctx(off::TRAP)),
                Stub::Rax => {}
            }
            let j = self.a.jmp();
            self.a.patch_rel32(j, self.trap_common);
        }
        for fix in std::mem::take(&mut self.fixes) {
            let to = match fix.to {
                Label::Ir(pc) => self.ir[(pc - self.base) as usize],
                Label::Stub(s) => placed.iter().find(|&&(p, _)| p == s).expect("placed").1,
            };
            match fix.kind {
                Kind::Rel32 => self.a.patch_rel32(fix.at, to),
                Kind::Table(start) => self.a.patch_i32(fix.at, (to as i64 - start as i64) as i32),
            }
        }
    }

    /// Copies `n` slots from `src` down to `dst` in memory (the cache flushed and empty).
    fn copy_slots(&mut self, dst: S, src: S, n: S) {
        if dst == src {
            return;
        }
        if n <= LOOP_COPY {
            for i in 0..n {
                self.a.load(true, RAX, slot(src + i));
                self.a.store(true, slot(dst + i), RAX);
            }
            return;
        }
        // A loop through `rsi`/`rdi`, free while the cache is empty
        self.a.lea(true, RSI, slot(src));
        self.a.lea(true, RDI, slot(dst));
        self.a.mov_imm(RCX, n as u64);
        let top = self.a.len();
        self.a.load(true, RAX, at(RSI, 0));
        self.a.store(true, at(RDI, 0), RAX);
        self.a.alu_ri(Alu::Add, true, RSI, 8);
        self.a.alu_ri(Alu::Add, true, RDI, 8);
        self.a.alu_ri(Alu::Sub, false, RCX, 1);
        let j = self.a.jcc(Cc::Ne);
        self.a.patch_rel32(j, top);
    }

    /// A call: depth check, frame pointer moved up to the arguments, back down after; `target` is a direct
    /// callee, else the address is in `rax`.
    fn call_to(&mut self, at_slot: S, target: Option<u32>) {
        self.a.alu_mi(Alu::Sub, true, ctx(off::DEPTH), 1);
        self.trap_if(Cc::B, Trap::StackOverflow);
        let shift = at_slot as i32 * 8;
        if shift > 0 {
            self.a.alu_ri(Alu::Add, true, FR, shift);
        }
        match target {
            Some(f) => {
                let at = self.a.call();
                self.calls.push((at, f));
            }
            None => self.a.call_r(RAX),
        }
        if shift > 0 {
            self.a.alu_ri(Alu::Sub, true, FR, shift);
        }
        self.a.alu_mi(Alu::Add, true, ctx(off::DEPTH), 1);
        self.a.load(true, MEM, ctx(off::MEM));
    }

    /// The operand for a memory access: `[r13 + rax + offset]`, with `rax` the zero-extended i32 in slot
    /// `addr`, bounds-checked for `size` bytes. A check covers later accesses in the block through the same
    /// address slot up to the same end (memory never shrinks; writing the slot drops it).
    fn address(&mut self, addr: S, offset: u32, size: u32) -> M {
        let r = self.read(addr);
        let end = offset as u64 + size as u64;
        if !self.checked.iter().any(|&(s, e)| s == addr && e >= end) {
            self.a.mov_rr(false, RCX, r);
            match i32::try_from(end) {
                Ok(e) => self.a.alu_ri(Alu::Add, true, RCX, e),
                Err(_) => {
                    self.a.mov_imm(RAX, end);
                    self.a.alu_rr(Alu::Add, true, RCX, RAX);
                }
            }
            self.a.cmp_rm(true, RCX, ctx(off::MEM_LEN));
            self.trap_if(Cc::A, Trap::MemoryOutOfBounds);
            self.checked.retain(|&(s, _)| s != addr);
            self.checked.push((addr, end));
        }
        self.a.mov_rr(false, RAX, r);
        match i32::try_from(offset) {
            Ok(disp) => M { base: MEM, index: Some(RAX), scale: 0, disp },
            Err(_) => {
                self.a.mov_imm(RCX, offset as u64);
                self.a.alu_rr(Alu::Add, true, RAX, RCX);
                indexed(MEM, RAX)
            }
        }
    }

    /// A load: from what this block just stored there if it can, else from memory.
    fn load(&mut self, width: u32, signed: bool, to_x: bool, dst: S, addr: S, offset: u32) {
        let known =
            self.stored.iter().find(|e| e.addr == addr && e.off == offset && e.width == width).copied();
        if let Some(e) = known {
            let v = self.read(e.val);
            let d = self.write(dst);
            match (width, signed) {
                (3, _) => self.a.mov_rr(true, d, v),
                (2, false) => self.a.mov_rr(false, d, v),
                (2, true) => self.a.movsxd_rr(d, v),
                (1, false) => self.a.movzx16_rr(d, v),
                (1, true) => self.a.movsx16_rr(to_x, d, v),
                (_, false) => self.a.movzx8_rr(d, v),
                (_, true) => self.a.movsx8_rr(to_x, d, v),
            }
            return;
        }
        let m = self.address(addr, offset, 1 << width);
        let d = self.write(dst);
        match (width, signed) {
            (3, _) => self.a.load(true, d, m),
            (2, false) => self.a.load(false, d, m),
            (2, true) => self.a.movsxd(d, m),
            (1, false) => self.a.movzx16(d, m),
            (1, true) => self.a.movsx16(to_x, d, m),
            (_, false) => self.a.movzx8(d, m),
            (_, true) => self.a.movsx8(to_x, d, m),
        }
    }

    fn store(&mut self, width: u32, addr: S, val: S, offset: u32) {
        let m = self.address(addr, offset, 1 << width);
        let v = self.read(val);
        match width {
            3 => self.a.store(true, m, v),
            2 => self.a.store(false, m, v),
            1 => self.a.store16(m, v),
            _ => self.a.store8(m, v),
        }
        // Other remembered stores may overlap — except ones through the same address slot elsewhere.
        let (lo, hi) = (offset as u64, offset as u64 + (1 << width));
        self.stored.retain(|e| e.addr == addr && (e.off as u64 + (1 << e.width) <= lo || e.off as u64 >= hi));
        self.stored.push(Stored { addr, off: offset, width, val });
    }

    /// `dst = a <op> b` for the `Alu` group.
    fn alu(&mut self, w: bool, op: Alu, commutes: bool, dst: S, a: S, b: S) {
        let (ra, rb) = (self.read(a), self.read(b));
        let rd = self.write(dst);
        if rd == ra {
            self.a.alu_rr(op, w, rd, rb);
        } else if rd == rb && commutes {
            self.a.alu_rr(op, w, rd, ra);
        } else {
            self.a.mov_rr(true, RAX, ra);
            self.a.alu_rr(op, w, RAX, rb);
            self.a.mov_rr(true, rd, RAX);
        }
    }

    fn alu_imm(&mut self, w: bool, op: Alu, dst: S, a: S, imm: i32) {
        let ra = self.read(a);
        let rd = self.write(dst);
        if rd != ra {
            self.a.mov_rr(true, rd, ra);
        }
        self.a.alu_ri(op, w, rd, imm);
    }

    fn mul(&mut self, w: bool, dst: S, a: S, b: S) {
        let (ra, rb) = (self.read(a), self.read(b));
        let rd = self.write(dst);
        if rd == ra {
            self.a.imul(w, rd, rb);
        } else if rd == rb {
            self.a.imul(w, rd, ra);
        } else {
            self.a.mov_rr(true, rd, ra);
            self.a.imul(w, rd, rb);
        }
    }

    fn shift(&mut self, w: bool, op: Shift, dst: S, a: S, b: S) {
        let (ra, rb) = (self.read(a), self.read(b));
        self.a.mov_rr(false, RCX, rb);
        self.a.mov_rr(true, RAX, ra);
        self.a.shift_cl(op, w, RAX);
        let rd = self.write(dst);
        self.a.mov_rr(true, rd, RAX);
    }

    fn shift_imm(&mut self, w: bool, op: Shift, dst: S, a: S, imm: i32) {
        let ra = self.read(a);
        let rd = self.write(dst);
        if rd != ra {
            self.a.mov_rr(true, rd, ra);
        }
        self.a.shift_imm(op, w, rd, (imm & if w { 63 } else { 31 }) as u8);
    }

    /// `dst = cond ? 1 : 0` from the flags just set.
    fn set(&mut self, cc: Cc, dst: S) {
        self.a.setcc(cc, RAX);
        let rd = self.write(dst);
        self.a.movzx8_rr(rd, RAX);
    }

    fn compare(&mut self, w: bool, cc: Cc, dst: S, a: S, b: Option<S>, imm: i32) {
        let ra = self.read(a);
        match b {
            Some(b) => {
                let rb = self.read(b);
                self.a.alu_rr(Alu::Cmp, w, ra, rb);
            }
            None => self.a.alu_ri(Alu::Cmp, w, ra, imm),
        }
        self.set(cc, dst);
    }

    fn div(&mut self, w: bool, signed: bool, rem: bool, dst: S, a: S, b: S) {
        let (ra, rb) = (self.read(a), self.read(b));
        self.a.test(w, rb, rb);
        self.trap_if(Cc::E, Trap::DivideByZero);
        let mut skip = None;
        if signed {
            // idiv faults on MIN / -1: the quotient overflows (a trap in wasm), the remainder is 0.
            self.a.alu_ri(Alu::Cmp, w, rb, -1);
            let normal = self.a.jcc(Cc::Ne);
            if rem {
                self.a.mov_imm(RDX, 0);
                skip = Some(self.a.jmp());
            } else {
                if w {
                    self.a.mov_imm(RCX, i64::MIN as u64);
                    self.a.alu_rr(Alu::Cmp, true, ra, RCX);
                } else {
                    self.a.alu_ri(Alu::Cmp, false, ra, i32::MIN);
                }
                self.trap_if(Cc::E, Trap::IntegerOverflow);
            }
            self.here(normal);
        }
        self.a.mov_rr(true, RAX, ra);
        if signed {
            self.a.sign_extend_rax(w);
        } else {
            self.a.mov_imm(RDX, 0);
        }
        self.a.div(w, signed, rb);
        if let Some(j) = skip {
            self.here(j);
        }
        let rd = self.write(dst);
        self.a.mov_rr(w, rd, if rem { RDX } else { RAX });
    }

    fn clz(&mut self, w: bool, dst: S, a: S) {
        let ra = self.read(a);
        self.a.bsr(w, RAX, ra);
        self.a.mov_imm(RCX, u64::MAX);
        self.a.cmov(Cc::E, w, RAX, RCX);
        self.a.mov_imm(RCX, if w { 63 } else { 31 });
        self.a.alu_rr(Alu::Sub, w, RCX, RAX);
        let rd = self.write(dst);
        self.a.mov_rr(w, rd, RCX);
    }

    fn ctz(&mut self, w: bool, dst: S, a: S) {
        let ra = self.read(a);
        self.a.bsf(w, RAX, ra);
        self.a.mov_imm(RCX, if w { 64 } else { 32 });
        self.a.cmov(Cc::E, w, RAX, RCX);
        let rd = self.write(dst);
        self.a.mov_rr(w, rd, RAX);
    }

    /// Population count without POPCNT (the classic bit-slicing sum).
    fn popcnt(&mut self, w: bool, dst: S, a: S) {
        let ra = self.read(a);
        self.a.mov_rr(w, RAX, ra);
        let masks: [u64; 4] = if w {
            [0x5555_5555_5555_5555, 0x3333_3333_3333_3333, 0x0F0F_0F0F_0F0F_0F0F, 0x0101_0101_0101_0101]
        } else {
            [0x5555_5555, 0x3333_3333, 0x0F0F_0F0F, 0x0101_0101]
        };
        // x -= (x >> 1) & 0x55…
        self.a.mov_rr(w, RCX, RAX);
        self.a.shift_imm(Shift::Shr, w, RCX, 1);
        self.a.mov_imm(RDX, masks[0]);
        self.a.alu_rr(Alu::And, w, RCX, RDX);
        self.a.alu_rr(Alu::Sub, w, RAX, RCX);
        // x = (x & 0x33…) + ((x >> 2) & 0x33…)
        self.a.mov_rr(w, RCX, RAX);
        self.a.shift_imm(Shift::Shr, w, RCX, 2);
        self.a.mov_imm(RDX, masks[1]);
        self.a.alu_rr(Alu::And, w, RCX, RDX);
        self.a.alu_rr(Alu::And, w, RAX, RDX);
        self.a.alu_rr(Alu::Add, w, RAX, RCX);
        // x = (x + (x >> 4)) & 0x0F…
        self.a.mov_rr(w, RCX, RAX);
        self.a.shift_imm(Shift::Shr, w, RCX, 4);
        self.a.alu_rr(Alu::Add, w, RAX, RCX);
        self.a.mov_imm(RDX, masks[2]);
        self.a.alu_rr(Alu::And, w, RAX, RDX);
        // (x * 0x01…) >> (bits - 8)
        self.a.mov_imm(RDX, masks[3]);
        self.a.imul(w, RAX, RDX);
        self.a.shift_imm(Shift::Shr, w, RAX, if w { 56 } else { 24 });
        let rd = self.write(dst);
        self.a.mov_rr(w, rd, RAX);
    }

    fn fbin(&mut self, d: bool, op: u8, dst: S, a: S, b: S) {
        let (ra, rb) = (self.fsrc(a), self.fsrc(b));
        let rd = self.fdst(dst);
        self.fop3(d, op, rd, ra, rb);
    }

    /// `dst = op a` (sqrt, precision conversions): SSE's one-source forms write their destination.
    fn fun(&mut self, d: bool, op: u8, dst: S, a: S) {
        let ra = self.fsrc(a);
        let rd = self.fdst(dst);
        self.a.fop(d, op, rd, ra);
    }

    /// wasm's min/max: NaN if either is, and -0 below +0 (SSE's `minss`/`maxss` do neither).
    fn fminmax(&mut self, d: bool, max: bool, dst: S, a: S, b: S) {
        self.fread(0, a);
        self.fread(1, b);
        self.a.ucomis(d, 0, 1);
        let nan = self.a.jcc(Cc::P);
        let differ = self.a.jcc(Cc::Ne);
        // Equal: combine the sign bits (min prefers -0, max +0).
        if max {
            self.a.andps(0, 1);
        } else {
            self.a.orps(0, 1);
        }
        let done1 = self.a.jmp();
        self.here(nan);
        self.a.fop(d, 0x58, 0, 1);
        let done2 = self.a.jmp();
        self.here(differ);
        self.a.fop(d, if max { 0x5F } else { 0x5D }, 0, 1);
        self.here(done1);
        self.here(done2);
        self.fwrite(0, dst);
    }

    fn fcompare(&mut self, d: bool, op: &Op, dst: S, a: S, b: S) {
        use Op::*;
        let (ra, rb) = (self.fsrc(a), self.fsrc(b));
        match op {
            F32Eq { .. } | F64Eq { .. } | F32Ne { .. } | F64Ne { .. } => {
                let eq = matches!(op, F32Eq { .. } | F64Eq { .. });
                self.a.ucomis(d, ra, rb);
                // Unordered sets ZF and PF: equal needs ZF without PF, not-equal is either.
                self.a.setcc(if eq { Cc::E } else { Cc::Ne }, RAX);
                self.a.setcc(if eq { Cc::Np } else { Cc::P }, RCX);
                self.a.alu_rr(if eq { Alu::And } else { Alu::Or }, false, RAX, RCX);
                let rd = self.write(dst);
                self.a.movzx8_rr(rd, RAX);
            }
            // Unordered sets CF and ZF, so "above"/"above or equal" are false on NaN — compare the other way
            // round for less-than.
            F32Lt { .. } | F64Lt { .. } => {
                self.a.ucomis(d, rb, ra);
                self.set(Cc::A, dst);
            }
            F32Le { .. } | F64Le { .. } => {
                self.a.ucomis(d, rb, ra);
                self.set(Cc::Ae, dst);
            }
            F32Gt { .. } | F64Gt { .. } => {
                self.a.ucomis(d, ra, rb);
                self.set(Cc::A, dst);
            }
            _ => {
                self.a.ucomis(d, ra, rb);
                self.set(Cc::Ae, dst);
            }
        }
    }

    /// A float operand as a double in `xmm0`.
    fn as_double(&mut self, from_d: bool, a: S) {
        self.fread(0, a);
        if !from_d {
            self.a.fop(false, 0x5A, 0, 0);
        }
    }

    /// `xmm1` = a double constant.
    fn double(&mut self, v: f64) {
        self.a.mov_imm(RAX, v.to_bits());
        self.a.movd_to_xmm(true, 1, RAX);
    }

    /// `rax` = the double in `xmm0` (in range) converted toward zero.
    fn convert_in_range(&mut self, to_x: bool, signed: bool) {
        if to_x && !signed {
            // Above i64's range: subtract 2^63 first, add it back as the top bit.
            self.double(9223372036854775808.0);
            self.a.ucomis(true, 0, 1);
            let big = self.a.jcc(Cc::Ae);
            self.a.cvtt2si(true, true, RAX, 0);
            let done = self.a.jmp();
            self.here(big);
            self.a.fop(true, 0x5C, 0, 1);
            self.a.cvtt2si(true, true, RAX, 0);
            self.a.mov_imm(RCX, 1 << 63);
            self.a.alu_rr(Alu::Xor, true, RAX, RCX);
            self.here(done);
        } else {
            // u32 goes through the 64-bit conversion (it's within i64) and keeps the low half.
            self.a.cvtt2si(true, to_x || !signed, RAX, 0);
        }
    }

    fn trunc(&mut self, from_d: bool, to_x: bool, signed: bool, dst: S, a: S) {
        let r = range(to_x, signed);
        self.as_double(from_d, a);
        self.a.ucomis(true, 0, 0);
        self.trap_if(Cc::P, Trap::InvalidConversion);
        self.double(r.lo);
        self.a.ucomis(true, 0, 1);
        self.trap_if(if r.lo_inclusive { Cc::B } else { Cc::Be }, Trap::IntegerOverflow);
        self.double(r.hi);
        self.a.ucomis(true, 0, 1);
        self.trap_if(Cc::Ae, Trap::IntegerOverflow);
        self.convert_in_range(to_x, signed);
        let rd = self.write(dst);
        self.a.mov_rr(to_x, rd, RAX);
    }

    fn saturate(&mut self, from_d: bool, to_x: bool, signed: bool, dst: S, a: S) {
        let r = range(to_x, signed);
        self.as_double(from_d, a);
        self.a.ucomis(true, 0, 0);
        let nan = self.a.jcc(Cc::P);
        self.double(r.hi);
        self.a.ucomis(true, 0, 1);
        let high = self.a.jcc(Cc::Ae);
        self.double(r.lo);
        self.a.ucomis(true, 0, 1);
        let low = self.a.jcc(if r.lo_inclusive { Cc::B } else { Cc::Be });
        self.convert_in_range(to_x, signed);
        let d1 = self.a.jmp();
        self.here(nan);
        self.a.mov_imm(RAX, 0);
        let d2 = self.a.jmp();
        self.here(high);
        self.a.mov_imm(RAX, r.max);
        let d3 = self.a.jmp();
        self.here(low);
        self.a.mov_imm(RAX, r.min);
        for d in [d1, d2, d3] {
            self.here(d);
        }
        let rd = self.write(dst);
        self.a.mov_rr(to_x, rd, RAX);
    }

    fn convert(&mut self, x: bool, d: bool, signed: bool, dst: S, a: S) {
        let ra = self.read(a);
        match (x, signed) {
            (false, true) => self.a.cvtsi2(d, false, 0, ra),
            (false, false) => {
                self.a.mov_rr(false, RAX, ra);
                self.a.cvtsi2(d, true, 0, RAX);
            }
            (true, true) => self.a.cvtsi2(d, true, 0, ra),
            (true, false) => {
                // Top bit set: halve (keeping the low bit, for rounding), convert, double.
                self.a.mov_rr(true, RAX, ra);
                self.a.test(true, RAX, RAX);
                let big = self.a.jcc(Cc::S);
                self.a.cvtsi2(d, true, 0, RAX);
                let done = self.a.jmp();
                self.here(big);
                self.a.mov_rr(true, RCX, RAX);
                self.a.shift_imm(Shift::Shr, true, RCX, 1);
                self.a.alu_ri(Alu::And, false, RAX, 1);
                self.a.alu_rr(Alu::Or, true, RCX, RAX);
                self.a.cvtsi2(d, true, 0, RCX);
                self.a.fop(d, 0x58, 0, 0);
                self.here(done);
            }
        }
        self.fwrite(0, dst);
    }

    /// Calls the Rust side for a rare op.
    fn bulk(&mut self, pc: u32) {
        self.sync();
        // The helper reads and writes frame slots.
        self.spill_homes(true);
        self.a.mov_rr(true, RDI, CTX);
        self.a.mov_rr(true, RSI, FR);
        self.a.mov_imm(RDX, pc as u64);
        self.a.load(true, RAX, ctx(off::BULK));
        // It charges fuel for the bytes it touches.
        self.a.store(true, ctx(off::FUEL), FUEL);
        self.a.call_r(RAX);
        self.a.load(true, FUEL, ctx(off::FUEL));
        self.a.test(true, RAX, RAX);
        self.jump(Some(Cc::Ne), Label::Stub(Stub::Rax));
        self.a.load(true, MEM, ctx(off::MEM));
        self.reload_homes(true);
    }

    fn op(&mut self, pc: u32, op: Op) {
        use Op::*;
        match op {
            Unreachable => {
                self.jump(None, Label::Stub(Stub::Trap(Trap::Unreachable)));
                self.forget();
            }
            Fuel(n) => {
                self.a.alu_ri(Alu::Sub, true, FUEL, n as i32);
                self.jump(Some(Cc::B), Label::Stub(Stub::Fuel));
            }
            Jump(to) => {
                self.flush();
                self.jump(None, Label::Ir(to));
                self.forget();
            }
            JumpIf { c, to } | JumpIfNot { c, to } | JumpIf64 { c, to } | JumpIfNot64 { c, to } => {
                let w = matches!(op, JumpIf64 { .. } | JumpIfNot64 { .. });
                let r = self.read(c);
                self.a.test(w, r, r);
                self.flush();
                let cc = if matches!(op, JumpIf { .. } | JumpIf64 { .. }) { Cc::Ne } else { Cc::E };
                self.jump(Some(cc), Label::Ir(to));
            }
            JumpIfEq { a, b, to }
            | JumpIfNe { a, b, to }
            | JumpIfLtS { a, b, to }
            | JumpIfLtU { a, b, to }
            | JumpIfGtS { a, b, to }
            | JumpIfGtU { a, b, to }
            | JumpIfLeS { a, b, to }
            | JumpIfLeU { a, b, to }
            | JumpIfGeS { a, b, to }
            | JumpIfGeU { a, b, to } => {
                let (ra, rb) = (self.read(a), self.read(b));
                self.a.alu_rr(Alu::Cmp, false, ra, rb);
                self.flush();
                self.jump(Some(jump_cc(&op)), Label::Ir(to));
            }
            JumpIfEqImm { a, imm, to }
            | JumpIfNeImm { a, imm, to }
            | JumpIfLtSImm { a, imm, to }
            | JumpIfLtUImm { a, imm, to }
            | JumpIfGtSImm { a, imm, to }
            | JumpIfGtUImm { a, imm, to }
            | JumpIfLeSImm { a, imm, to }
            | JumpIfLeUImm { a, imm, to }
            | JumpIfGeSImm { a, imm, to }
            | JumpIfGeUImm { a, imm, to } => {
                let ra = self.read(a);
                self.a.alu_ri(Alu::Cmp, false, ra, imm);
                self.flush();
                self.jump(Some(jump_cc(&op)), Label::Ir(to));
            }
            BrTable { c, start, len } => {
                let r = self.read(c);
                self.a.mov_rr(false, RAX, r);
                self.sync();
                self.a.mov_imm(RCX, len as u64);
                self.a.alu_rr(Alu::Cmp, false, RAX, RCX);
                self.a.cmov(Cc::Ae, false, RAX, RCX);
                let lea = self.a.lea_rip(RCX);
                self.a.movsxd_table(RAX, RCX, RAX);
                self.a.alu_rr(Alu::Add, true, RAX, RCX);
                self.a.jmp_r(RAX);
                let table = self.a.len();
                self.a.patch_rel32(lea, table);
                for _ in 0..=len {
                    self.a.put_i32(0);
                }
                for k in 0..=len as usize {
                    let t = self.m.br_tables[start as usize + k];
                    let entry = table + 4 * k;
                    if t.n > 0 && t.src != t.dst {
                        let stub = self.a.len();
                        self.a.patch_i32(entry, (stub - table) as i32);
                        self.copy_slots(t.dst, t.src, t.n);
                        self.jump(None, Label::Ir(t.to));
                    } else {
                        self.fixes.push(Fix { at: entry, kind: Kind::Table(table), to: Label::Ir(t.to) });
                    }
                }
            }
            // A long move (block results): through memory, in a loop
            Move { dst, src, n } if n > LOOP_COPY => {
                self.sync();
                self.spill_homes(true);
                self.copy_slots(dst, src, n);
                self.reload_homes(true);
            }
            Move { dst, src, n } => {
                for i in 0..n {
                    self.copy(dst + i, src + i);
                    self.settle();
                    self.pinned = 0;
                    self.fpinned = 0;
                }
            }
            Return { src, n } => {
                self.sync();
                self.copy_slots(0, src, n);
                if self.pad() > 0 {
                    self.a.alu_ri(Alu::Add, true, RSP, self.pad());
                }
                for r in self.callee_homes().into_iter().rev() {
                    self.a.pop(r);
                }
                self.a.ret();
            }
            Call { at, f } => {
                self.sync();
                self.spill_homes(false);
                self.call_to(at, Some(f));
                self.reload_homes(false);
            }
            CallIndirect { at, c, ty, table } => {
                self.sync();
                self.spill_homes(true);
                self.a.mov_rr(true, RDI, CTX);
                self.a.mov_imm(RSI, table as u64);
                self.a.load(false, RDX, slot(c));
                self.a.mov_imm(RCX, ty as u64);
                self.a.load(true, RAX, ctx(off::INDIRECT));
                self.a.call_r(RAX);
                self.a.test(true, RAX, RAX);
                self.jump(Some(Cc::E), Label::Stub(Stub::Ctx));
                self.call_to(at, None);
                self.reload_homes(false);
            }
            Copy { dst, src } => self.copy(dst, src),
            Const { dst, v } => {
                let d = self.write(dst);
                self.a.mov_imm(d, v);
            }
            Select { dst, a, b, c } => {
                let (rc, ra, rb) = (self.read(c), self.read(a), self.read(b));
                self.a.mov_rr(true, RAX, rb);
                self.a.test(false, rc, rc);
                self.a.cmov(Cc::Ne, true, RAX, ra);
                let d = self.write(dst);
                self.a.mov_rr(true, d, RAX);
            }
            GlobalGet { dst, g } => {
                self.a.load(true, RAX, ctx(off::GLOBALS));
                let d = self.write(dst);
                self.a.load(true, d, at(RAX, g as i32 * 8));
            }
            GlobalSet { src, g } => {
                let r = self.read(src);
                self.a.load(true, RAX, ctx(off::GLOBALS));
                self.a.store(true, at(RAX, g as i32 * 8), r);
            }
            TableGet { .. }
            | TableSet { .. }
            | TableSize { .. }
            | TableGrow { .. }
            | TableFill { .. }
            | TableCopy { .. }
            | TableInit { .. }
            | ElemDrop(_)
            | MemoryGrow { .. }
            | MemoryCopy { .. }
            | MemoryFill { .. }
            | MemoryInit { .. }
            | DataDrop(_) => self.bulk(pc),
            RefIsNull { dst, a } => {
                let r = self.read(a);
                self.a.test(true, r, r);
                self.set(Cc::E, dst);
            }
            Load64 { dst, addr, off } => self.load(3, false, true, dst, addr, off),
            Load32U { dst, addr, off } => self.load(2, false, false, dst, addr, off),
            Load32S64 { dst, addr, off } => self.load(2, true, true, dst, addr, off),
            Load16U { dst, addr, off } => self.load(1, false, false, dst, addr, off),
            Load16S32 { dst, addr, off } => self.load(1, true, false, dst, addr, off),
            Load16S64 { dst, addr, off } => self.load(1, true, true, dst, addr, off),
            Load8U { dst, addr, off } => self.load(0, false, false, dst, addr, off),
            Load8S32 { dst, addr, off } => self.load(0, true, false, dst, addr, off),
            Load8S64 { dst, addr, off } => self.load(0, true, true, dst, addr, off),
            Store8 { addr, val, off } => self.store(0, addr, val, off),
            Store16 { addr, val, off } => self.store(1, addr, val, off),
            Store32 { addr, val, off } => self.store(2, addr, val, off),
            Store64 { addr, val, off } => self.store(3, addr, val, off),
            MemorySize { dst } => {
                let d = self.write(dst);
                self.a.load(true, d, ctx(off::MEM_LEN));
                self.a.shift_imm(Shift::Shr, true, d, 16);
            }

            I32Eqz { dst, a } | I64Eqz { dst, a } => {
                let w = matches!(op, I64Eqz { .. });
                let r = self.read(a);
                self.a.test(w, r, r);
                self.set(Cc::E, dst);
            }
            I32Eq { dst, a, b }
            | I32Ne { dst, a, b }
            | I32LtS { dst, a, b }
            | I32LtU { dst, a, b }
            | I32GtS { dst, a, b }
            | I32GtU { dst, a, b }
            | I32LeS { dst, a, b }
            | I32LeU { dst, a, b }
            | I32GeS { dst, a, b }
            | I32GeU { dst, a, b }
            | I64Eq { dst, a, b }
            | I64Ne { dst, a, b }
            | I64LtS { dst, a, b }
            | I64LtU { dst, a, b }
            | I64GtS { dst, a, b }
            | I64GtU { dst, a, b }
            | I64LeS { dst, a, b }
            | I64LeU { dst, a, b }
            | I64GeS { dst, a, b }
            | I64GeU { dst, a, b } => {
                let (w, cc) = int_cmp(&op).expect("a comparison");
                self.compare(w, cc, dst, a, Some(b), 0);
            }
            I32EqImm { dst, a, imm }
            | I32NeImm { dst, a, imm }
            | I32LtSImm { dst, a, imm }
            | I32LtUImm { dst, a, imm }
            | I32GtSImm { dst, a, imm }
            | I32GtUImm { dst, a, imm }
            | I32LeSImm { dst, a, imm }
            | I32LeUImm { dst, a, imm }
            | I32GeSImm { dst, a, imm }
            | I32GeUImm { dst, a, imm }
            | I64EqImm { dst, a, imm }
            | I64NeImm { dst, a, imm }
            | I64LtSImm { dst, a, imm }
            | I64LtUImm { dst, a, imm }
            | I64GtSImm { dst, a, imm }
            | I64GtUImm { dst, a, imm }
            | I64LeSImm { dst, a, imm }
            | I64LeUImm { dst, a, imm }
            | I64GeSImm { dst, a, imm }
            | I64GeUImm { dst, a, imm } => {
                let (w, cc) = int_cmp(&op).expect("a comparison");
                self.compare(w, cc, dst, a, None, imm);
            }
            F32Eq { dst, a, b }
            | F32Ne { dst, a, b }
            | F32Lt { dst, a, b }
            | F32Gt { dst, a, b }
            | F32Le { dst, a, b }
            | F32Ge { dst, a, b } => self.fcompare(false, &op, dst, a, b),
            F64Eq { dst, a, b }
            | F64Ne { dst, a, b }
            | F64Lt { dst, a, b }
            | F64Gt { dst, a, b }
            | F64Le { dst, a, b }
            | F64Ge { dst, a, b } => self.fcompare(true, &op, dst, a, b),

            I32Clz { dst, a } => self.clz(false, dst, a),
            I64Clz { dst, a } => self.clz(true, dst, a),
            I32Ctz { dst, a } => self.ctz(false, dst, a),
            I64Ctz { dst, a } => self.ctz(true, dst, a),
            I32Popcnt { dst, a } => self.popcnt(false, dst, a),
            I64Popcnt { dst, a } => self.popcnt(true, dst, a),
            I32Add { dst, a, b } => self.alu(false, Alu::Add, true, dst, a, b),
            I32Sub { dst, a, b } => self.alu(false, Alu::Sub, false, dst, a, b),
            I32And { dst, a, b } => self.alu(false, Alu::And, true, dst, a, b),
            I32Or { dst, a, b } => self.alu(false, Alu::Or, true, dst, a, b),
            I32Xor { dst, a, b } => self.alu(false, Alu::Xor, true, dst, a, b),
            I64Add { dst, a, b } => self.alu(true, Alu::Add, true, dst, a, b),
            I64Sub { dst, a, b } => self.alu(true, Alu::Sub, false, dst, a, b),
            I64And { dst, a, b } => self.alu(true, Alu::And, true, dst, a, b),
            I64Or { dst, a, b } => self.alu(true, Alu::Or, true, dst, a, b),
            I64Xor { dst, a, b } => self.alu(true, Alu::Xor, true, dst, a, b),
            I32Mul { dst, a, b } => self.mul(false, dst, a, b),
            I64Mul { dst, a, b } => self.mul(true, dst, a, b),
            I32Shl { dst, a, b } => self.shift(false, Shift::Shl, dst, a, b),
            I32ShrS { dst, a, b } => self.shift(false, Shift::Sar, dst, a, b),
            I32ShrU { dst, a, b } => self.shift(false, Shift::Shr, dst, a, b),
            I32Rotl { dst, a, b } => self.shift(false, Shift::Rol, dst, a, b),
            I32Rotr { dst, a, b } => self.shift(false, Shift::Ror, dst, a, b),
            I64Shl { dst, a, b } => self.shift(true, Shift::Shl, dst, a, b),
            I64ShrS { dst, a, b } => self.shift(true, Shift::Sar, dst, a, b),
            I64ShrU { dst, a, b } => self.shift(true, Shift::Shr, dst, a, b),
            I64Rotl { dst, a, b } => self.shift(true, Shift::Rol, dst, a, b),
            I64Rotr { dst, a, b } => self.shift(true, Shift::Ror, dst, a, b),
            I32AddImm { dst, a, imm } | I64AddImm { dst, a, imm } => {
                let w = matches!(op, I64AddImm { .. });
                let ra = self.read(a);
                let rd = self.write(dst);
                self.a.lea(w, rd, at(ra, imm));
            }
            I32SubImm { dst, a, imm } => self.alu_imm(false, Alu::Sub, dst, a, imm),
            I64SubImm { dst, a, imm } => self.alu_imm(true, Alu::Sub, dst, a, imm),
            I32AndImm { dst, a, imm } => self.alu_imm(false, Alu::And, dst, a, imm),
            I32OrImm { dst, a, imm } => self.alu_imm(false, Alu::Or, dst, a, imm),
            I32XorImm { dst, a, imm } => self.alu_imm(false, Alu::Xor, dst, a, imm),
            I64AndImm { dst, a, imm } => self.alu_imm(true, Alu::And, dst, a, imm),
            I64OrImm { dst, a, imm } => self.alu_imm(true, Alu::Or, dst, a, imm),
            I64XorImm { dst, a, imm } => self.alu_imm(true, Alu::Xor, dst, a, imm),
            I32MulImm { dst, a, imm } | I64MulImm { dst, a, imm } => {
                let w = matches!(op, I64MulImm { .. });
                let ra = self.read(a);
                let rd = self.write(dst);
                self.a.imul_imm(w, rd, ra, imm);
            }
            I32ShlImm { dst, a, imm } => self.shift_imm(false, Shift::Shl, dst, a, imm),
            I32ShrSImm { dst, a, imm } => self.shift_imm(false, Shift::Sar, dst, a, imm),
            I32ShrUImm { dst, a, imm } => self.shift_imm(false, Shift::Shr, dst, a, imm),
            I32RotlImm { dst, a, imm } => self.shift_imm(false, Shift::Rol, dst, a, imm),
            I32RotrImm { dst, a, imm } => self.shift_imm(false, Shift::Ror, dst, a, imm),
            I64ShlImm { dst, a, imm } => self.shift_imm(true, Shift::Shl, dst, a, imm),
            I64ShrSImm { dst, a, imm } => self.shift_imm(true, Shift::Sar, dst, a, imm),
            I64ShrUImm { dst, a, imm } => self.shift_imm(true, Shift::Shr, dst, a, imm),
            I64RotlImm { dst, a, imm } => self.shift_imm(true, Shift::Rol, dst, a, imm),
            I64RotrImm { dst, a, imm } => self.shift_imm(true, Shift::Ror, dst, a, imm),
            I32DivS { dst, a, b } => self.div(false, true, false, dst, a, b),
            I32DivU { dst, a, b } => self.div(false, false, false, dst, a, b),
            I32RemS { dst, a, b } => self.div(false, true, true, dst, a, b),
            I32RemU { dst, a, b } => self.div(false, false, true, dst, a, b),
            I64DivS { dst, a, b } => self.div(true, true, false, dst, a, b),
            I64DivU { dst, a, b } => self.div(true, false, false, dst, a, b),
            I64RemS { dst, a, b } => self.div(true, true, true, dst, a, b),
            I64RemU { dst, a, b } => self.div(true, false, true, dst, a, b),

            // Sign-bit ops on the bits, exactly (NaN payloads kept).
            F32Abs { dst, a } | F32Neg { dst, a } => {
                let ra = self.read(a);
                self.a.mov_rr(false, RAX, ra);
                if matches!(op, F32Abs { .. }) {
                    self.a.alu_ri(Alu::And, false, RAX, 0x7FFF_FFFF);
                } else {
                    self.a.alu_ri(Alu::Xor, false, RAX, i32::MIN);
                }
                let rd = self.write(dst);
                self.a.mov_rr(false, rd, RAX);
            }
            F64Abs { dst, a } | F64Neg { dst, a } => {
                let ra = self.read(a);
                self.a.mov_rr(true, RAX, ra);
                if matches!(op, F64Abs { .. }) {
                    self.a.btr(true, RAX, 63);
                } else {
                    self.a.btc(true, RAX, 63);
                }
                let rd = self.write(dst);
                self.a.mov_rr(true, rd, RAX);
            }
            F32Copysign { dst, a, b } => {
                let (ra, rb) = (self.read(a), self.read(b));
                self.a.mov_rr(false, RAX, ra);
                self.a.alu_ri(Alu::And, false, RAX, 0x7FFF_FFFF);
                self.a.mov_rr(false, RCX, rb);
                self.a.alu_ri(Alu::And, false, RCX, i32::MIN);
                self.a.alu_rr(Alu::Or, false, RAX, RCX);
                let rd = self.write(dst);
                self.a.mov_rr(false, rd, RAX);
            }
            F64Copysign { dst, a, b } => {
                let (ra, rb) = (self.read(a), self.read(b));
                self.a.mov_rr(true, RAX, ra);
                self.a.btr(true, RAX, 63);
                self.a.mov_rr(true, RCX, rb);
                self.a.shift_imm(Shift::Shr, true, RCX, 63);
                self.a.shift_imm(Shift::Shl, true, RCX, 63);
                self.a.alu_rr(Alu::Or, true, RAX, RCX);
                let rd = self.write(dst);
                self.a.mov_rr(true, rd, RAX);
            }
            F32Ceil { dst, a } | F32Floor { dst, a } | F32Trunc { dst, a } | F32Nearest { dst, a } => {
                let mode = match op {
                    F32Nearest { .. } => 0,
                    F32Floor { .. } => 1,
                    F32Ceil { .. } => 2,
                    _ => 3,
                };
                let ra = self.fsrc(a);
                let rd = self.fdst(dst);
                self.a.round(false, rd, ra, mode);
            }
            F64Ceil { dst, a } | F64Floor { dst, a } | F64Trunc { dst, a } | F64Nearest { dst, a } => {
                let mode = match op {
                    F64Nearest { .. } => 0,
                    F64Floor { .. } => 1,
                    F64Ceil { .. } => 2,
                    _ => 3,
                };
                let ra = self.fsrc(a);
                let rd = self.fdst(dst);
                self.a.round(true, rd, ra, mode);
            }
            F32Sqrt { dst, a } | F64Sqrt { dst, a } => self.fun(matches!(op, F64Sqrt { .. }), 0x51, dst, a),
            F32Add { dst, a, b } => self.fbin(false, 0x58, dst, a, b),
            F32Sub { dst, a, b } => self.fbin(false, 0x5C, dst, a, b),
            F32Mul { dst, a, b } => self.fbin(false, 0x59, dst, a, b),
            F32Div { dst, a, b } => self.fbin(false, 0x5E, dst, a, b),
            F64Add { dst, a, b } => self.fbin(true, 0x58, dst, a, b),
            F64Sub { dst, a, b } => self.fbin(true, 0x5C, dst, a, b),
            F64Mul { dst, a, b } => self.fbin(true, 0x59, dst, a, b),
            F64Div { dst, a, b } => self.fbin(true, 0x5E, dst, a, b),
            F32Min { dst, a, b } => self.fminmax(false, false, dst, a, b),
            F32Max { dst, a, b } => self.fminmax(false, true, dst, a, b),
            F64Min { dst, a, b } => self.fminmax(true, false, dst, a, b),
            F64Max { dst, a, b } => self.fminmax(true, true, dst, a, b),

            I32WrapI64 { dst, a } | I64ExtendI32U { dst, a } => {
                let ra = self.read(a);
                let rd = self.write(dst);
                self.a.mov_rr(false, rd, ra);
            }
            I64ExtendI32S { dst, a } | I64Extend32S { dst, a } => {
                let ra = self.read(a);
                let rd = self.write(dst);
                self.a.movsxd_rr(rd, ra);
            }
            I32Extend8S { dst, a } | I64Extend8S { dst, a } => {
                let ra = self.read(a);
                let rd = self.write(dst);
                self.a.movsx8_rr(matches!(op, I64Extend8S { .. }), rd, ra);
            }
            I32Extend16S { dst, a } | I64Extend16S { dst, a } => {
                let ra = self.read(a);
                let rd = self.write(dst);
                self.a.movsx16_rr(matches!(op, I64Extend16S { .. }), rd, ra);
            }
            I32TruncF32S { dst, a } => self.trunc(false, false, true, dst, a),
            I32TruncF32U { dst, a } => self.trunc(false, false, false, dst, a),
            I32TruncF64S { dst, a } => self.trunc(true, false, true, dst, a),
            I32TruncF64U { dst, a } => self.trunc(true, false, false, dst, a),
            I64TruncF32S { dst, a } => self.trunc(false, true, true, dst, a),
            I64TruncF32U { dst, a } => self.trunc(false, true, false, dst, a),
            I64TruncF64S { dst, a } => self.trunc(true, true, true, dst, a),
            I64TruncF64U { dst, a } => self.trunc(true, true, false, dst, a),
            I32TruncSatF32S { dst, a } => self.saturate(false, false, true, dst, a),
            I32TruncSatF32U { dst, a } => self.saturate(false, false, false, dst, a),
            I32TruncSatF64S { dst, a } => self.saturate(true, false, true, dst, a),
            I32TruncSatF64U { dst, a } => self.saturate(true, false, false, dst, a),
            I64TruncSatF32S { dst, a } => self.saturate(false, true, true, dst, a),
            I64TruncSatF32U { dst, a } => self.saturate(false, true, false, dst, a),
            I64TruncSatF64S { dst, a } => self.saturate(true, true, true, dst, a),
            I64TruncSatF64U { dst, a } => self.saturate(true, true, false, dst, a),
            F32ConvertI32S { dst, a } => self.convert(false, false, true, dst, a),
            F32ConvertI32U { dst, a } => self.convert(false, false, false, dst, a),
            F32ConvertI64S { dst, a } => self.convert(true, false, true, dst, a),
            F32ConvertI64U { dst, a } => self.convert(true, false, false, dst, a),
            F64ConvertI32S { dst, a } => self.convert(false, true, true, dst, a),
            F64ConvertI32U { dst, a } => self.convert(false, true, false, dst, a),
            F64ConvertI64S { dst, a } => self.convert(true, true, true, dst, a),
            F64ConvertI64U { dst, a } => self.convert(true, true, false, dst, a),
            F32DemoteF64 { dst, a } => self.fun(true, 0x5A, dst, a),
            F64PromoteF32 { dst, a } => self.fun(false, 0x5A, dst, a),
        }
    }
}
