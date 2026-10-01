//! Register ops → AArch64, one op at a time.
//!
//! Values live in frame slots (`[x19, #slot*8]`), but within a basic block a **slot cache** keeps them in
//! registers: an op reads its operands from the registers holding them (loading a slot on first use) and
//! writes its result to a register, marked dirty. Dirty registers are written back only where memory must be
//! current — before a branch, a call, a helper, a return — and the cache is forgotten at every label (where
//! paths meet) and around calls (which clobber the registers). So a chain of arithmetic doesn't bounce
//! through memory, which would cost a store→load round trip per op.
//!
//! The cache has two sides, general registers and float registers, so float arithmetic stays in float
//! registers too. A slot is dirty on one side at most: writing it on one side drops it from the other, and
//! reading it on the other side copies it across (`fmov`), clean.
//!
//! Branches inside a function are patched once its code is laid out; calls between functions once the whole
//! module is.

use super::super::super::Trap;
use super::super::super::compile::{Code, Op, S};
use super::super::super::module::Module;
use super::super::{branch_targets, busiest_loop_locals, off, trap_code};
use super::asm::*;

const FR: Reg = 19;
const CTX: Reg = 20;
const MEM: Reg = 21;
/// Memory length − `GAP` (signed: negative for a memory under `GAP` bytes). An address at most this is in
/// bounds for any access ending within `GAP` bytes of it: most accesses are checked with one compare.
const LIMIT: Reg = 22;
const GAP: u32 = 4095;
const FUEL: Reg = 23;
const DEPTH: Reg = 24;
const GLOBALS: Reg = 26;
/// Scratch.
const T0: Reg = 9;
const T1: Reg = 10;
const T2: Reg = 11;
/// For offsets too big for an immediate.
const ADDR: Reg = 17;
const CALLEE: Reg = 15;
/// Registers the slot cache hands out (caller-saved: helpers and calls clobber them, so the cache is
/// written back and forgotten around those).
const POOL: [Reg; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
/// Home registers for a function's busiest loop locals: the value lives there for the whole function, so
/// paths meeting at a label agree without going through memory. Callee-saved first (calls and helpers keep
/// them; saved in the prologue), then caller-saved (written back and reloaded around calls).
const HOMES: [(Reg, bool); 6] = [(25, true), (27, true), (28, true), (12, false), (13, false), (14, false)];
/// Float registers the slot cache hands out (caller-saved; v0 and v1 stay scratch).
const FPOOL: [Reg; 12] = [2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21];
/// Home registers for float locals: d8–d11 (whose low 64 bits the ABI makes callee-saved), then caller-saved.
const FHOMES: [(Reg, bool); 6] = [(8, true), (9, true), (10, true), (11, true), (22, false), (23, false)];

/// A local living in a register for the whole function.
#[derive(Clone, Copy)]
struct Home {
    slot: S,
    reg: Reg,
    /// Kept across calls and helpers (saved in the prologue) — else written back and reloaded around them.
    callee: bool,
    /// A float register, for an f32/f64 local.
    float: bool,
}

pub(super) struct Out {
    pub words: Vec<u32>,
    pub entry: usize,
    pub funcs: Vec<usize>,
}

/// Where a trap branch goes, per function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stub {
    Trap(Trap),
    /// Out of fuel: zero the counter first.
    Fuel,
    /// A helper recorded the trap in the context.
    Ctx,
    /// A helper returned the trap code in `x0`.
    X0,
}

#[derive(Clone, Copy)]
enum Label {
    Ir(u32),
    Stub(Stub),
    /// The exact bounds check for a fast one that failed (index into `E::slow`).
    Slow(usize),
}

#[derive(Clone, Copy)]
enum Field {
    /// `b`/`bl`: 26 bits at 0.
    B26,
    /// `b.cond`/`cbz`/`cbnz`: 19 bits at 5.
    B19,
}

struct Fix {
    at: usize,
    field: Field,
    to: Label,
}

#[derive(Clone, Copy, Default)]
struct Entry {
    slot: Option<S>,
    dirty: bool,
}

struct E<'a> {
    m: &'a Module,
    w: Vec<u32>,
    trap_common: usize,
    /// Per function: native word of each op, the IR pcs branches land on, the branches to patch.
    ir: Vec<usize>,
    labels: Vec<bool>,
    base: u32,
    fixes: Vec<Fix>,
    /// `bl` sites: (word, callee).
    calls: Vec<(usize, u32)>,
    cache: [Entry; POOL.len()],
    next: usize,
    /// Cache entries the current op is using (can't be evicted).
    pinned: u32,
    /// The float side of the cache.
    fcache: [Entry; FPOOL.len()],
    fnext: usize,
    fpinned: u32,
    /// This function's locals living in home registers.
    homes: Vec<Home>,
    /// Moves into home registers of the other kind, due once the current op has written its result:
    /// (to, from, into a float register).
    pending: Vec<(Reg, Reg, bool)>,
    /// Bounds checks done in this block: (address slot, end checked).
    checked: Vec<(S, u64)>,
    /// Stores done in this block whose value a load of the same place can reuse.
    stored: Vec<Stored>,
    /// Exact bounds checks placed after the function: (address register, access end, where to go back).
    slow: Vec<(Reg, u64, usize)>,
    /// The code can't be used: a branch out of reach (a function over 1 MB, a module over 128 MB), or more
    /// code than `MAX_CODE`. The module then runs on the interpreter.
    unfit: bool,
}

/// The most machine code a module may compile to (bytes).
const MAX_CODE: usize = 64 << 20;
/// Slot copies longer than this are a loop (else a module could make code grow with its value counts).
const LOOP_COPY: S = 16;

/// Whether a word offset fits a branch's `bits`-bit signed field.
fn fits(off: i64, bits: u32) -> bool {
    (-(1i64 << (bits - 1))..1i64 << (bits - 1)).contains(&off)
}

/// Memory at `[addr + off, + 1 << width)` holds the low bytes of slot `val`.
#[derive(Clone, Copy)]
struct Stored {
    addr: S,
    off: u32,
    width: u32,
    val: S,
}

/// `None` if a branch can't reach its target — the module then runs on the interpreter.
pub(super) fn module(m: &Module) -> Option<Out> {
    let mut e = E {
        m,
        w: Vec::new(),
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
        slow: Vec::new(),
        unfit: false,
    };
    let entry = e.trampoline();
    let mut funcs = Vec::with_capacity(m.funcs.len());
    for f in 0..m.funcs.len() {
        funcs.push(e.w.len());
        e.func(f);
    }
    for &(at, f) in &e.calls {
        let off = funcs[f as usize] as i64 - at as i64;
        e.unfit |= !fits(off, 26);
        e.w[at] |= off as u32 & 0x03FF_FFFF;
    }
    (!e.unfit).then_some(Out { words: e.w, entry, funcs })
}

fn jump_cond(op: &Op) -> Cond {
    use Op::*;
    match op {
        JumpIfEq { .. } | JumpIfEqImm { .. } => Cond::Eq,
        JumpIfNe { .. } | JumpIfNeImm { .. } => Cond::Ne,
        JumpIfLtS { .. } | JumpIfLtSImm { .. } => Cond::Lt,
        JumpIfLtU { .. } | JumpIfLtUImm { .. } => Cond::Lo,
        JumpIfGtS { .. } | JumpIfGtSImm { .. } => Cond::Gt,
        JumpIfGtU { .. } | JumpIfGtUImm { .. } => Cond::Hi,
        JumpIfLeS { .. } | JumpIfLeSImm { .. } => Cond::Le,
        JumpIfLeU { .. } | JumpIfLeUImm { .. } => Cond::Ls,
        JumpIfGeS { .. } | JumpIfGeSImm { .. } => Cond::Ge,
        _ => Cond::Hs,
    }
}

/// Integer comparison ops → (64-bit, condition).
fn int_cmp(op: &Op) -> Option<(bool, Cond)> {
    use Op::*;
    Some(match op {
        I32Eq { .. } | I32EqImm { .. } => (false, Cond::Eq),
        I32Ne { .. } | I32NeImm { .. } => (false, Cond::Ne),
        I32LtS { .. } | I32LtSImm { .. } => (false, Cond::Lt),
        I32LtU { .. } | I32LtUImm { .. } => (false, Cond::Lo),
        I32GtS { .. } | I32GtSImm { .. } => (false, Cond::Gt),
        I32GtU { .. } | I32GtUImm { .. } => (false, Cond::Hi),
        I32LeS { .. } | I32LeSImm { .. } => (false, Cond::Le),
        I32LeU { .. } | I32LeUImm { .. } => (false, Cond::Ls),
        I32GeS { .. } | I32GeSImm { .. } => (false, Cond::Ge),
        I32GeU { .. } | I32GeUImm { .. } => (false, Cond::Hs),
        I64Eq { .. } | I64EqImm { .. } => (true, Cond::Eq),
        I64Ne { .. } | I64NeImm { .. } => (true, Cond::Ne),
        I64LtS { .. } | I64LtSImm { .. } => (true, Cond::Lt),
        I64LtU { .. } | I64LtUImm { .. } => (true, Cond::Lo),
        I64GtS { .. } | I64GtSImm { .. } => (true, Cond::Gt),
        I64GtU { .. } | I64GtUImm { .. } => (true, Cond::Hi),
        I64LeS { .. } | I64LeSImm { .. } => (true, Cond::Le),
        I64LeU { .. } | I64LeUImm { .. } => (true, Cond::Ls),
        I64GeS { .. } | I64GeSImm { .. } => (true, Cond::Ge),
        I64GeU { .. } | I64GeUImm { .. } => (true, Cond::Hs),
        _ => return None,
    })
}

impl E<'_> {
    fn put(&mut self, w: u32) {
        self.w.push(w);
    }

    fn mov_imm(&mut self, x: bool, rd: Reg, v: u64) {
        let n = if x { 4 } else { 2 };
        let v = if x { v } else { v as u32 as u64 };
        let hw = |i: u32| ((v >> (16 * i)) & 0xFFFF) as u32;
        let ones = (0..n).filter(|&i| hw(i) == 0xFFFF).count();
        let zeros = (0..n).filter(|&i| hw(i) == 0).count();
        if ones > zeros {
            let first = (0..n).find(|&i| hw(i) != 0xFFFF).unwrap_or(0);
            self.put(movn(x, rd, !hw(first) & 0xFFFF, first));
            for i in (0..n).filter(|&i| i != first && hw(i) != 0xFFFF) {
                self.put(movk(x, rd, hw(i), i));
            }
        } else {
            let first = (0..n).find(|&i| hw(i) != 0).unwrap_or(0);
            self.put(movz(x, rd, hw(first), first));
            for i in (0..n).filter(|&i| i != first && hw(i) != 0) {
                self.put(movk(x, rd, hw(i), i));
            }
        }
    }

    /// `rd = rn + off` (64-bit).
    fn add_off(&mut self, rd: Reg, rn: Reg, off: u64) {
        if off < 4096 {
            self.put(add_imm(true, rd, rn, off as u32));
        } else {
            self.mov_imm(true, ADDR, off);
            self.put(add(true, rd, rn, ADDR));
        }
    }

    fn ldst(&mut self, m: Mem, rt: Reg, rn: Reg, off: u32) {
        match ldst_imm(m, rt, rn, off) {
            Some(w) => self.put(w),
            None => {
                self.mov_imm(true, ADDR, off as u64);
                self.put(ldst_reg(m, rt, rn, ADDR));
            }
        }
    }

    /// A frame slot, straight from/to memory.
    fn slot(&mut self, m: Mem, rt: Reg, slot: S) {
        self.ldst(m, rt, FR, slot as u32 * 8);
    }

    // ── The slot cache ──────────────────────────────────────────────────────

    fn find(&self, slot: S) -> Option<usize> {
        self.cache.iter().position(|e| e.slot == Some(slot))
    }

    fn pin(&mut self, i: usize) -> Reg {
        self.pinned |= 1 << i;
        POOL[i]
    }

    /// A cache entry to (re)use: an empty one, else the next unpinned one round-robin (written back if dirty).
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
        self.spill(i);
        i
    }

    fn spill(&mut self, i: usize) {
        if let Entry { slot: Some(s), dirty: true } = self.cache[i] {
            self.slot(STR_X, POOL[i], s);
        }
        self.cache[i] = Entry::default();
    }

    /// The register holding `slot` (all 64 bits of it), loading it if needed.
    fn read(&mut self, slot: S) -> Reg {
        if let Some(h) = self.home(slot) {
            if !h.float {
                return h.reg;
            }
            // A float home read as bits: a copy, for this op only
            let i = self.take();
            self.put(fmov_x_from_d(POOL[i], h.reg));
            return self.pin(i);
        }
        if let Some(i) = self.find(slot) {
            return self.pin(i);
        }
        let i = self.take();
        match self.ffind(slot) {
            Some(j) => self.put(fmov_x_from_d(POOL[i], FPOOL[j])),
            None => self.slot(LDR_X, POOL[i], slot),
        }
        self.cache[i] = Entry { slot: Some(slot), dirty: false };
        self.pin(i)
    }

    /// A register that will hold the new value of `slot` (call after reading the operands).
    fn write(&mut self, slot: S) -> Reg {
        self.changed(slot);
        if let Some(h) = self.home(slot) {
            if !h.float {
                return h.reg;
            }
            // Written as bits into a float home: moved over once the op has written it
            let i = self.take();
            self.pending.push((h.reg, POOL[i], true));
            return self.pin(i);
        }
        if let Some(j) = self.ffind(slot) {
            self.fcache[j] = Entry::default();
        }
        let i = self.find(slot).unwrap_or_else(|| self.take());
        self.cache[i] = Entry { slot: Some(slot), dirty: true };
        self.pin(i)
    }

    /// The moves `read`/`write`/`fsrc`/`fdst` left for after the op.
    fn settle(&mut self) {
        for (to, from, float) in std::mem::take(&mut self.pending) {
            self.put(if float { fmov_to_d(to, from) } else { fmov_x_from_d(to, from) });
        }
    }

    /// Writes dirty registers back to their slots (keeping them cached, now clean).
    fn flush(&mut self) {
        for (i, &r) in POOL.iter().enumerate() {
            if let Entry { slot: Some(s), dirty: true } = self.cache[i] {
                self.slot(STR_X, r, s);
                self.cache[i].dirty = false;
            }
        }
        for (j, &r) in FPOOL.iter().enumerate() {
            if let Entry { slot: Some(s), dirty: true } = self.fcache[j] {
                self.slot(STR_D, r, s);
                self.fcache[j].dirty = false;
            }
        }
    }

    /// Forgets what's cached (after a flush, or where the registers are no longer trusted).
    fn forget(&mut self) {
        self.checked.clear();
        self.stored.clear();
        self.cache = [Entry::default(); POOL.len()];
        self.fcache = [Entry::default(); FPOOL.len()];
    }

    /// Slot `slot` is being overwritten: what was known about it, or through it, no longer holds.
    fn changed(&mut self, slot: S) {
        self.checked.retain(|&(s, _)| s != slot);
        self.stored.retain(|e| e.addr != slot && e.val != slot);
    }

    fn home(&self, slot: S) -> Option<Home> {
        self.homes.iter().find(|h| h.slot == slot).copied()
    }

    /// Homes' values to their slots (`all`: callee-saved ones too, for code that reads the frame).
    fn spill_homes(&mut self, all: bool) {
        for h in self.homes.clone() {
            if all || !h.callee {
                self.slot(if h.float { STR_D } else { STR_X }, h.reg, h.slot);
            }
        }
    }

    fn reload_homes(&mut self, all: bool) {
        for h in self.homes.clone() {
            if all || !h.callee {
                self.slot(if h.float { LDR_D } else { LDR_X }, h.reg, h.slot);
            }
        }
    }

    /// The busiest loop locals get home registers of their kind: general ones for integers and references,
    /// float ones for f32/f64.
    fn choose_homes(&mut self, c: &Code, end: usize) {
        let floats = &self.m.floats[c.floats as usize..][..c.locals as usize];
        let (mut ints, mut fls) = (HOMES.iter(), FHOMES.iter());
        self.homes.clear();
        for s in busiest_loop_locals(self.m, c.start as usize, end, c.locals) {
            let float = floats[s as usize];
            let next = if float { fls.next() } else { ints.next() };
            if let Some(&(reg, callee)) = next {
                self.homes.push(Home { slot: s, reg, callee, float });
            }
        }
    }

    // ── The float side ──────────────────────────────────────────────────────

    fn ffind(&self, slot: S) -> Option<usize> {
        self.fcache.iter().position(|e| e.slot == Some(slot))
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
            self.slot(STR_D, FPOOL[j], s);
        }
        self.fcache[j] = Entry::default();
        j
    }

    /// The float register holding `slot`, loading (or copying) it there if needed.
    fn fsrc(&mut self, slot: S) -> Reg {
        if let Some(h) = self.home(slot) {
            if h.float {
                return h.reg;
            }
            let j = self.ftake();
            self.put(fmov_to_d(FPOOL[j], h.reg));
            return self.fpin(j);
        }
        if let Some(j) = self.ffind(slot) {
            return self.fpin(j);
        }
        let j = self.ftake();
        match self.find(slot) {
            Some(i) => self.put(fmov_to_d(FPOOL[j], POOL[i])),
            None => self.slot(LDR_D, FPOOL[j], slot),
        }
        self.fcache[j] = Entry { slot: Some(slot), dirty: false };
        self.fpin(j)
    }

    /// A float register that will hold the new value of `slot` (call after reading the operands).
    fn fdst(&mut self, slot: S) -> Reg {
        self.changed(slot);
        if let Some(h) = self.home(slot) {
            if h.float {
                return h.reg;
            }
            let j = self.ftake();
            self.pending.push((h.reg, FPOOL[j], false));
            return self.fpin(j);
        }
        if let Some(i) = self.find(slot) {
            self.cache[i] = Entry::default();
        }
        let j = self.ffind(slot).unwrap_or_else(|| self.ftake());
        self.fcache[j] = Entry { slot: Some(slot), dirty: true };
        self.fpin(j)
    }

    /// A float operand into `v{f}` (for sequences that need it in a particular register).
    fn fread(&mut self, f: Reg, slot: S) {
        let r = self.fsrc(slot);
        if r != f {
            self.put(fmov_dd(f, r));
        }
    }

    // ── Branches ────────────────────────────────────────────────────────────

    /// A branch to be patched: `w` with a zero offset field.
    fn branch(&mut self, w: u32, field: Field, to: Label) {
        self.fixes.push(Fix { at: self.w.len(), field, to });
        self.put(w);
    }

    fn trap_if(&mut self, c: Cond, stub: Stub) {
        self.branch(b_cond(c, 0), Field::B19, Label::Stub(stub));
    }

    fn trap(&mut self, stub: Stub) {
        self.branch(b(0), Field::B26, Label::Stub(stub));
    }

    /// `cmp rn, #imm` for any immediate.
    fn cmp_imm(&mut self, x: bool, rn: Reg, imm: i64) {
        if (0..4096).contains(&imm) {
            self.put(subs_imm(x, ZR, rn, imm as u32));
        } else if (-4095..0).contains(&imm) {
            self.put(adds_imm(x, ZR, rn, (-imm) as u32));
        } else {
            self.mov_imm(x, T1, imm as u64);
            self.put(subs(x, ZR, rn, T1));
        }
    }

    /// The entry trampoline, `extern "C" fn(ctx, frame, target) -> trap code`, and the shared trap exit.
    fn trampoline(&mut self) -> usize {
        let entry = self.w.len();
        for w in [
            stp_pre(29, 30, SP, -128),
            mov_sp(29, SP),
            stp(19, 20, SP, 16),
            stp(21, 22, SP, 32),
            stp(23, 24, SP, 48),
            stp(25, 26, SP, 64),
            stp(27, 28, SP, 80),
            mov(true, CTX, 0),
            mov(true, FR, 1),
            mov_sp(T0, SP),
        ] {
            self.put(w);
        }
        self.ldst(STR_X, T0, CTX, off::ENTRY_SP);
        // The float homes' callee-saved registers: a trap unwinds past the epilogues that restore them.
        for (k, &(r, _)) in FHOMES.iter().filter(|h| h.1).enumerate() {
            self.ldst(STR_D, r, SP, 96 + k as u32 * 8);
        }
        self.load_memory();
        self.put(ldp(FUEL, DEPTH, CTX, off::FUEL as i32));
        self.ldst(LDR_X, GLOBALS, CTX, off::GLOBALS);
        self.put(blr(2));
        self.ldst(STR_X, FUEL, CTX, off::FUEL);
        self.put(movz(true, 0, 0, 0));
        let exit = self.w.len();
        for w in [
            ldp(19, 20, SP, 16),
            ldp(21, 22, SP, 32),
            ldp(23, 24, SP, 48),
            ldp(25, 26, SP, 64),
            ldp(27, 28, SP, 80),
        ] {
            self.put(w);
        }
        for (k, &(r, _)) in FHOMES.iter().filter(|h| h.1).enumerate() {
            self.ldst(LDR_D, r, SP, 96 + k as u32 * 8);
        }
        for w in [ldp_post(29, 30, SP, 128), RET] {
            self.put(w);
        }
        // Trap exit: x0 = code. Unwind to the trampoline's frame and leave through the exit.
        self.trap_common = self.w.len();
        self.ldst(STR_X, FUEL, CTX, off::FUEL);
        self.ldst(LDR_X, T0, CTX, off::ENTRY_SP);
        self.put(mov_sp(SP, T0));
        let here = self.w.len() as i32;
        self.put(b(exit as i32 - here));
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
        self.slow.clear();
        self.forget();
        self.choose_homes(&c, end);

        // Prologue: frame record, value stack room for this frame, zeroed locals.
        self.put(stp_pre(29, 30, SP, -16));
        self.put(mov_sp(29, SP));
        self.add_off(T0, FR, c.frame as u64 * 8);
        self.ldst(LDR_X, T1, CTX, off::STACK_END);
        self.put(subs(true, ZR, T0, T1));
        self.trap_if(Cond::Hi, Stub::Trap(Trap::StackOverflow));
        let zeros = c.locals - c.params;
        if zeros <= 16 {
            for s in c.params..c.locals {
                self.slot(STR_X, ZR, s as S);
            }
        } else {
            self.add_off(T0, FR, c.params as u64 * 8);
            self.mov_imm(true, T1, zeros as u64);
            self.put(str_post(ZR, T0, 8));
            self.put(subs_imm(true, T1, T1, 1));
            self.put(b_cond(Cond::Ne, -2));
        }
        // Homes: callee-saved ones saved (general ones two per 16 bytes, then the float ones below them,
        // keeping sp aligned), then loaded with the params they hold or zero.
        for pair in self.callee_homes().chunks(2) {
            self.put(stp_pre(pair[0], *pair.get(1).unwrap_or(&ZR), SP, -16));
        }
        let fsaved = self.fcallee_homes();
        if !fsaved.is_empty() {
            self.put(sub_imm(true, SP, SP, fsaved.len().div_ceil(2) as u32 * 16));
            for (k, &r) in fsaved.iter().enumerate() {
                self.ldst(STR_D, r, SP, k as u32 * 8);
            }
        }
        for h in self.homes.clone() {
            match ((h.slot as u32) < c.params, h.float) {
                (true, false) => self.slot(LDR_X, h.reg, h.slot),
                (true, true) => self.slot(LDR_D, h.reg, h.slot),
                (false, false) => self.put(movz(true, h.reg, 0, 0)),
                (false, true) => self.put(fmov_to_d(h.reg, ZR)),
            }
        }

        for pc in c.start as usize..end {
            if self.labels[pc - c.start as usize] {
                self.flush();
                self.forget();
            }
            self.ir[pc - c.start as usize] = self.w.len();
            self.op(pc as u32, self.m.code[pc]);
            self.settle();
            self.pinned = 0;
            self.fpinned = 0;
            if self.w.len() * 4 > MAX_CODE {
                self.unfit = true;
                return;
            }
        }
        self.ir[n] = self.w.len();

        // Exact bounds checks for the fast ones that failed, each going back where it came from.
        let mut slow_at = Vec::new();
        for (a, end, back) in std::mem::take(&mut self.slow) {
            slow_at.push(self.w.len());
            self.exact_check(a, end);
            let here = self.w.len() as i64;
            self.unfit |= !fits(back as i64 - here, 26);
            self.put(b((back as i64 - here) as i32));
        }

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
            placed.push((s, self.w.len()));
            match s {
                Stub::Trap(t) => self.mov_imm(true, 0, trap_code(t) as u64),
                Stub::Fuel => {
                    self.put(movz(true, FUEL, 0, 0));
                    self.mov_imm(true, 0, trap_code(Trap::OutOfFuel) as u64);
                }
                Stub::Ctx => self.ldst(LDR_X, 0, CTX, off::TRAP),
                Stub::X0 => {}
            }
            let here = self.w.len() as i32;
            self.unfit |= !fits(self.trap_common as i64 - here as i64, 26);
            self.put(b(self.trap_common as i32 - here));
        }
        let fixes = std::mem::take(&mut self.fixes);
        for fix in &fixes {
            let to = match fix.to {
                Label::Ir(pc) => self.ir[(pc - self.base) as usize],
                Label::Stub(s) => placed.iter().find(|&&(p, _)| p == s).expect("placed").1,
                Label::Slow(k) => slow_at[k],
            };
            let off = to as i64 - fix.at as i64;
            self.unfit |= !fits(off, if matches!(fix.field, Field::B26) { 26 } else { 19 });
            self.w[fix.at] |= match fix.field {
                Field::B26 => off as u32 & 0x03FF_FFFF,
                Field::B19 => (off as u32 & 0x7FFFF) << 5,
            };
        }
    }

    /// Copies `n` slots from `src` down to `dst` in memory (the cache must be flushed and empty).
    fn copy_slots(&mut self, dst: S, src: S, n: S) {
        if dst == src {
            return;
        }
        if n <= LOOP_COPY {
            for i in 0..n {
                self.slot(LDR_X, T0, src + i);
                self.slot(STR_X, T0, dst + i);
            }
            return;
        }
        self.add_off(T0, FR, src as u64 * 8);
        self.add_off(T1, FR, dst as u64 * 8);
        self.mov_imm(true, T2, n as u64);
        self.put(ldr_post(16, T0, 8));
        self.put(str_post(16, T1, 8));
        self.put(subs_imm(true, T2, T2, 1));
        self.put(b_cond(Cond::Ne, -3));
    }

    /// Before leaving the cache's view (a helper, a call): memory current, registers untrusted.
    fn sync(&mut self) {
        self.flush();
        self.forget();
    }

    /// Callee-saved general homes (saved in the prologue).
    fn callee_homes(&self) -> Vec<Reg> {
        self.homes.iter().filter(|h| h.callee && !h.float).map(|h| h.reg).collect()
    }

    /// Callee-saved float homes (saved in the prologue, below the general ones).
    fn fcallee_homes(&self) -> Vec<Reg> {
        self.homes.iter().filter(|h| h.callee && h.float).map(|h| h.reg).collect()
    }

    fn call_to(&mut self, at: S, target: Option<u32>) {
        self.branch(cbz(true, DEPTH, 0), Field::B19, Label::Stub(Stub::Trap(Trap::StackOverflow)));
        self.put(sub_imm(true, DEPTH, DEPTH, 1));
        self.put(str_pre(FR, SP, -16));
        self.add_off(FR, FR, at as u64 * 8);
        match target {
            Some(f) => {
                self.calls.push((self.w.len(), f));
                self.put(bl(0));
            }
            None => self.put(blr(CALLEE)),
        }
        self.put(ldr_post(FR, SP, 16));
        self.put(add_imm(true, DEPTH, DEPTH, 1));
        self.load_memory();
    }

    /// The register holding the i32 address in slot `addr`, checked that `offset..offset+size` past it is
    /// inside memory: one compare against `LIMIT` when the access ends within `GAP` bytes (the exact check,
    /// for when that fails, is placed after the function), else the exact check here. Within a block, a
    /// check covers later accesses through the same address slot up to the same end: memory never shrinks,
    /// and writing the slot drops the check.
    fn address(&mut self, addr: S, offset: u32, size: u32) -> Reg {
        let a = self.read(addr);
        let end = offset as u64 + size as u64;
        if !self.checked.iter().any(|&(s, e)| s == addr && e >= end) {
            if end <= GAP as u64 {
                // a ≤ length − GAP covers any access ending within GAP; else the exact check, out of line
                self.put(cmp_uxtw(LIMIT, a));
                let k = self.slow.len();
                self.branch(b_cond(Cond::Lt, 0), Field::B19, Label::Slow(k));
                self.slow.push((a, end, self.w.len()));
            } else {
                self.exact_check(a, end);
            }
            self.checked.retain(|&(s, _)| s != addr);
            self.checked.push((addr, end));
        }
        a
    }

    /// Memory base and `LIMIT` from the context (at entry, and after anything that may grow memory).
    fn load_memory(&mut self) {
        self.put(ldp(MEM, LIMIT, CTX, off::MEM as i32));
        self.put(sub_imm(true, LIMIT, LIMIT, GAP));
    }

    /// `a` + `end` ≤ memory length, or a trap.
    fn exact_check(&mut self, a: Reg, end: u64) {
        self.put(mov(false, T1, a));
        self.add_off(T1, T1, end);
        self.put(add_imm(true, T2, LIMIT, GAP));
        self.put(subs(true, ZR, T1, T2));
        self.trap_if(Cond::Hi, Stub::Trap(Trap::MemoryOutOfBounds));
    }

    /// A load or store at memory base + the i32 in `a` + `offset`: one instruction when there's no offset.
    fn access(&mut self, kind: Mem, rt: Reg, a: Reg, offset: u32) {
        if offset == 0 {
            self.put(ldst_uxtw(kind, rt, MEM, a));
            return;
        }
        self.put(add_uxtw(T0, MEM, a));
        match ldst_imm(kind, rt, T0, offset) {
            Some(w) => self.put(w),
            None => {
                self.mov_imm(true, T1, offset as u64);
                self.put(ldst_reg(kind, rt, T0, T1));
            }
        }
    }

    fn load(&mut self, kind: Mem, dst: S, addr: S, offset: u32) {
        let width = kind.scale;
        let known =
            self.stored.iter().find(|e| e.addr == addr && e.off == offset && e.width == width).copied();
        if let Some(e) = known {
            // Just stored there in this block: take the value (already bounds-checked), extended as the load
            // would.
            let v = self.read(e.val);
            let d = self.write(dst);
            self.put(match kind.op {
                op if op == LDR_X.op => mov(true, d, v),
                op if op == LDR_W.op => mov(false, d, v),
                op if op == LDRSW.op => sxtw(d, v),
                op if op == LDRH.op => uxth(d, v),
                op if op == LDRSH_W.op => sxth(false, d, v),
                op if op == LDRSH_X.op => sxth(true, d, v),
                op if op == LDRB.op => uxtb(d, v),
                op if op == LDRSB_W.op => sxtb(false, d, v),
                _ => sxtb(true, d, v),
            });
            return;
        }
        let a = self.address(addr, offset, 1 << width);
        let d = self.write(dst);
        self.access(kind, d, a, offset);
    }

    fn store(&mut self, kind: Mem, addr: S, val: S, offset: u32) {
        let a = self.address(addr, offset, 1 << kind.scale);
        let v = self.read(val);
        self.access(kind, v, a, offset);
        // Any other remembered store may overlap this one — except ones through the same address slot at
        // bytes this store doesn't touch.
        let (lo, hi) = (offset as u64, offset as u64 + (1 << kind.scale));
        self.stored.retain(|e| e.addr == addr && (e.off as u64 + (1 << e.width) <= lo || e.off as u64 >= hi));
        self.stored.push(Stored { addr, off: offset, width: kind.scale, val });
    }

    /// `dst = f(a, b)` on w (or x) registers.
    fn bin(&mut self, x: bool, dst: S, a: S, b: S, f: fn(bool, Reg, Reg, Reg) -> u32) {
        let (a, b) = (self.read(a), self.read(b));
        let d = self.write(dst);
        self.put(f(x, d, a, b));
    }

    /// `dst = f(a, imm)`.
    fn bin_imm(&mut self, x: bool, dst: S, a: S, imm: i64, f: fn(bool, Reg, Reg, Reg) -> u32) {
        let a = self.read(a);
        self.mov_imm(x, T1, imm as u64);
        let d = self.write(dst);
        self.put(f(x, d, a, T1));
    }

    fn add_sub_imm(&mut self, x: bool, dst: S, a: S, imm: i64) {
        let a = self.read(a);
        let d = self.write(dst);
        if (0..4096).contains(&imm) {
            self.put(add_imm(x, d, a, imm as u32));
        } else if (-4095..0).contains(&imm) {
            self.put(sub_imm(x, d, a, (-imm) as u32));
        } else {
            self.mov_imm(x, T1, imm as u64);
            self.put(add(x, d, a, T1));
        }
    }

    fn compare(&mut self, x: bool, c: Cond, dst: S, a: S, b: Option<S>, imm: i64) {
        let a = self.read(a);
        match b {
            Some(b) => {
                let b = self.read(b);
                self.put(subs(x, ZR, a, b));
            }
            None => self.cmp_imm(x, a, imm),
        }
        let d = self.write(dst);
        self.put(cset(d, c));
    }

    /// `dst = f(a)` on a register, 32- or 64-bit as `f` decides.
    fn unary(&mut self, dst: S, a: S, f: impl FnOnce(&mut Self, Reg, Reg)) {
        let a = self.read(a);
        let d = self.write(dst);
        f(self, d, a);
    }

    /// A slot copied to another, on the side where the value is: float registers if it's in one (or is
    /// going to a float home), general registers otherwise.
    fn copy(&mut self, dst: S, src: S) {
        let float_home = |h: Option<Home>| h.is_some_and(|h| h.float);
        if self.ffind(src).is_some() || float_home(self.home(src)) || float_home(self.home(dst)) {
            let r = self.fsrc(src);
            let d = self.fdst(dst);
            if d != r {
                self.put(fmov_dd(d, r));
            }
        } else {
            let r = self.read(src);
            let d = self.write(dst);
            self.put(mov(true, d, r));
        }
    }

    fn fbin(&mut self, d: bool, dst: S, a: S, b: S, f: fn(bool, Reg, Reg, Reg) -> u32) {
        let (a, b) = (self.fsrc(a), self.fsrc(b));
        let r = self.fdst(dst);
        self.put(f(d, r, a, b));
    }

    fn fun(&mut self, d: bool, dst: S, a: S, f: fn(bool, Reg, Reg) -> u32) {
        let a = self.fsrc(a);
        let r = self.fdst(dst);
        self.put(f(d, r, a));
    }

    fn fcompare(&mut self, d: bool, c: Cond, dst: S, a: S, b: S) {
        let (a, b) = (self.fsrc(a), self.fsrc(b));
        self.put(fcmp(d, a, b));
        let r = self.write(dst);
        self.put(cset(r, c));
    }

    /// Integer division and remainder, with the spec's traps.
    fn div(&mut self, x: bool, signed: bool, rem: bool, dst: S, a: S, b: S) {
        let (a, b) = (self.read(a), self.read(b));
        self.branch(cbz(x, b, 0), Field::B19, Label::Stub(Stub::Trap(Trap::DivideByZero)));
        if signed && !rem {
            // MIN / -1 overflows.
            self.put(adds_imm(x, ZR, b, 1));
            self.put(b_cond(Cond::Ne, 4));
            self.put(movz(x, T2, 0x8000, if x { 3 } else { 1 }));
            self.put(subs(x, ZR, a, T2));
            self.trap_if(Cond::Eq, Stub::Trap(Trap::IntegerOverflow));
        }
        let divide = if signed { sdiv } else { udiv };
        if rem {
            self.put(divide(x, T2, a, b));
            let d = self.write(dst);
            self.put(msub(x, d, T2, b, a));
        } else {
            let d = self.write(dst);
            self.put(divide(x, d, a, b));
        }
    }

    /// Float → int that traps on NaN and out-of-range values.
    fn trunc(&mut self, from_d: bool, to_x: bool, signed: bool, dst: S, a: S) {
        self.fread(0, a);
        if !from_d {
            self.put(fcvt_widen(0, 0));
        }
        self.put(fcmp(true, 0, 0));
        self.trap_if(Cond::Vs, Stub::Trap(Trap::InvalidConversion));
        let (lo, lo_inclusive, hi): (f64, bool, f64) = match (to_x, signed) {
            (false, true) => (-2147483649.0, false, 2147483648.0),
            (false, false) => (-1.0, false, 4294967296.0),
            (true, true) => (-9223372036854775808.0, true, 9223372036854775808.0),
            (true, false) => (-1.0, false, 18446744073709551616.0),
        };
        self.mov_imm(true, T0, lo.to_bits());
        self.put(fmov_to_d(1, T0));
        self.put(fcmp(true, 0, 1));
        self.trap_if(if lo_inclusive { Cond::Mi } else { Cond::Ls }, Stub::Trap(Trap::IntegerOverflow));
        self.mov_imm(true, T0, hi.to_bits());
        self.put(fmov_to_d(1, T0));
        self.put(fcmp(true, 0, 1));
        self.trap_if(Cond::Ge, Stub::Trap(Trap::IntegerOverflow));
        let r = self.write(dst);
        self.put(if signed { fcvtzs(to_x, true, r, 0) } else { fcvtzu(to_x, true, r, 0) });
    }

    fn convert(&mut self, x: bool, d: bool, signed: bool, dst: S, a: S) {
        let a = self.read(a);
        let r = self.fdst(dst);
        self.put(if signed { scvtf(x, d, r, a) } else { ucvtf(x, d, r, a) });
    }

    fn saturate(&mut self, from_d: bool, to_x: bool, signed: bool, dst: S, a: S) {
        let a = self.fsrc(a);
        let r = self.write(dst);
        self.put(if signed { fcvtzs(to_x, from_d, r, a) } else { fcvtzu(to_x, from_d, r, a) });
    }

    /// Calls the Rust side for a rare op.
    fn bulk(&mut self, pc: u32) {
        self.sync();
        // The helper reads and writes frame slots.
        self.spill_homes(true);
        self.put(mov(true, 0, CTX));
        self.put(mov(true, 1, FR));
        self.mov_imm(false, 2, pc as u64);
        self.ldst(LDR_X, 16, CTX, off::BULK);
        // It charges fuel for the bytes it touches.
        self.ldst(STR_X, FUEL, CTX, off::FUEL);
        self.put(blr(16));
        self.ldst(LDR_X, FUEL, CTX, off::FUEL);
        self.branch(cbnz(true, 0, 0), Field::B19, Label::Stub(Stub::X0));
        self.load_memory();
        self.reload_homes(true);
    }

    fn op(&mut self, pc: u32, op: Op) {
        use Op::*;
        match op {
            Unreachable => {
                self.trap(Stub::Trap(Trap::Unreachable));
                self.forget();
            }
            Fuel(n) => {
                if n < 4096 {
                    self.put(subs_imm(true, FUEL, FUEL, n));
                } else {
                    self.mov_imm(true, T0, n as u64);
                    self.put(subs(true, FUEL, FUEL, T0));
                }
                self.trap_if(Cond::Lo, Stub::Fuel);
            }
            Jump(to) => {
                self.flush();
                self.branch(b(0), Field::B26, Label::Ir(to));
                self.forget();
            }
            JumpIf { c, to } | JumpIfNot { c, to } | JumpIf64 { c, to } | JumpIfNot64 { c, to } => {
                let x = matches!(op, JumpIf64 { .. } | JumpIfNot64 { .. });
                let r = self.read(c);
                self.flush();
                let w =
                    if matches!(op, JumpIf { .. } | JumpIf64 { .. }) { cbnz(x, r, 0) } else { cbz(x, r, 0) };
                self.branch(w, Field::B19, Label::Ir(to));
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
                let (a, b) = (self.read(a), self.read(b));
                self.put(subs(false, ZR, a, b));
                self.flush();
                self.branch(b_cond(jump_cond(&op), 0), Field::B19, Label::Ir(to));
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
                let a = self.read(a);
                self.cmp_imm(false, a, imm as i64);
                self.flush();
                self.branch(b_cond(jump_cond(&op), 0), Field::B19, Label::Ir(to));
            }
            BrTable { c, start, len } => {
                let r = self.read(c);
                self.put(mov(false, T0, r));
                self.sync();
                self.mov_imm(false, T1, len as u64);
                self.put(subs(false, ZR, T0, T1));
                self.put(csel(false, T0, T0, T1, Cond::Lo));
                // adr → the table (3 words on), then one `b` per entry.
                self.put(adr(T1, 12));
                self.put(add_uxtw2(T1, T1, T0));
                self.put(br(T1));
                let table = self.w.len();
                for _ in 0..=len {
                    self.put(b(0));
                }
                for k in 0..=len as usize {
                    let t = self.m.br_tables[start as usize + k];
                    if t.n > 0 && t.src != t.dst {
                        let stub = self.w.len();
                        self.unfit |= !fits((stub - (table + k)) as i64, 26);
                        self.w[table + k] = b((stub - (table + k)) as i32);
                        self.copy_slots(t.dst, t.src, t.n);
                        self.branch(b(0), Field::B26, Label::Ir(t.to));
                    } else {
                        self.fixes.push(Fix { at: table + k, field: Field::B26, to: Label::Ir(t.to) });
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
                let fsaved = self.fcallee_homes();
                if !fsaved.is_empty() {
                    for (k, &r) in fsaved.iter().enumerate() {
                        self.ldst(LDR_D, r, SP, k as u32 * 8);
                    }
                    self.put(add_imm(true, SP, SP, fsaved.len().div_ceil(2) as u32 * 16));
                }
                for pair in self.callee_homes().chunks(2).rev() {
                    self.put(ldp_post(pair[0], *pair.get(1).unwrap_or(&ZR), SP, 16));
                }
                self.put(ldp_post(29, 30, SP, 16));
                self.put(RET);
            }
            Call { at, f } => {
                self.sync();
                self.spill_homes(false);
                self.call_to(at, Some(f));
                self.reload_homes(false);
            }
            CallIndirect { at, c, ty, table } => {
                // Arguments go in x0–x3, which the cache uses: write it back first, then load from memory.
                self.sync();
                self.spill_homes(true);
                self.slot(LDR_W, 2, c);
                self.put(mov(true, 0, CTX));
                self.mov_imm(false, 1, table as u64);
                self.mov_imm(false, 3, ty as u64);
                self.ldst(LDR_X, 16, CTX, off::INDIRECT);
                self.put(blr(16));
                self.branch(cbz(true, 0, 0), Field::B19, Label::Stub(Stub::Ctx));
                self.put(mov(true, CALLEE, 0));
                self.call_to(at, None);
                self.reload_homes(false);
            }
            Copy { dst, src } => self.copy(dst, src),
            Const { dst, v } => {
                let d = self.write(dst);
                self.mov_imm(true, d, v);
            }
            Select { dst, a, b, c } => {
                let (c, a, b) = (self.read(c), self.read(a), self.read(b));
                self.put(subs_imm(false, ZR, c, 0));
                let d = self.write(dst);
                self.put(csel(true, d, a, b, Cond::Ne));
            }
            GlobalGet { dst, g } => {
                let d = self.write(dst);
                self.ldst(LDR_X, d, GLOBALS, g * 8);
            }
            GlobalSet { src, g } => {
                let r = self.read(src);
                self.ldst(STR_X, r, GLOBALS, g * 8);
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
                let a = self.read(a);
                self.put(subs_imm(true, ZR, a, 0));
                let d = self.write(dst);
                self.put(cset(d, Cond::Eq));
            }
            Load64 { dst, addr, off } => self.load(LDR_X, dst, addr, off),
            Load32U { dst, addr, off } => self.load(LDR_W, dst, addr, off),
            Load8S32 { dst, addr, off } => self.load(LDRSB_W, dst, addr, off),
            Load8U { dst, addr, off } => self.load(LDRB, dst, addr, off),
            Load16S32 { dst, addr, off } => self.load(LDRSH_W, dst, addr, off),
            Load16U { dst, addr, off } => self.load(LDRH, dst, addr, off),
            Load8S64 { dst, addr, off } => self.load(LDRSB_X, dst, addr, off),
            Load16S64 { dst, addr, off } => self.load(LDRSH_X, dst, addr, off),
            Load32S64 { dst, addr, off } => self.load(LDRSW, dst, addr, off),
            Store8 { addr, val, off } => self.store(STRB, addr, val, off),
            Store16 { addr, val, off } => self.store(STRH, addr, val, off),
            Store32 { addr, val, off } => self.store(STR_W, addr, val, off),
            Store64 { addr, val, off } => self.store(STR_X, addr, val, off),
            MemorySize { dst } => {
                let d = self.write(dst);
                self.put(add_imm(true, d, LIMIT, GAP));
                self.put(lsr16(d, d));
            }

            I32Eqz { dst, a } => self.compare(false, Cond::Eq, dst, a, None, 0),
            I64Eqz { dst, a } => self.compare(true, Cond::Eq, dst, a, None, 0),
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
                let (x, c) = int_cmp(&op).expect("a comparison");
                self.compare(x, c, dst, a, Some(b), 0);
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
                let (x, c) = int_cmp(&op).expect("a comparison");
                self.compare(x, c, dst, a, None, imm as i64);
            }
            F32Eq { dst, a, b } => self.fcompare(false, Cond::Eq, dst, a, b),
            F32Ne { dst, a, b } => self.fcompare(false, Cond::Ne, dst, a, b),
            F32Lt { dst, a, b } => self.fcompare(false, Cond::Mi, dst, a, b),
            F32Gt { dst, a, b } => self.fcompare(false, Cond::Gt, dst, a, b),
            F32Le { dst, a, b } => self.fcompare(false, Cond::Ls, dst, a, b),
            F32Ge { dst, a, b } => self.fcompare(false, Cond::Ge, dst, a, b),
            F64Eq { dst, a, b } => self.fcompare(true, Cond::Eq, dst, a, b),
            F64Ne { dst, a, b } => self.fcompare(true, Cond::Ne, dst, a, b),
            F64Lt { dst, a, b } => self.fcompare(true, Cond::Mi, dst, a, b),
            F64Gt { dst, a, b } => self.fcompare(true, Cond::Gt, dst, a, b),
            F64Le { dst, a, b } => self.fcompare(true, Cond::Ls, dst, a, b),
            F64Ge { dst, a, b } => self.fcompare(true, Cond::Ge, dst, a, b),

            I32Clz { dst, a } => self.unary(dst, a, |e, d, a| e.put(clz(false, d, a))),
            I64Clz { dst, a } => self.unary(dst, a, |e, d, a| e.put(clz(true, d, a))),
            I32Ctz { dst, a } => self.unary(dst, a, |e, d, a| {
                e.put(rbit(false, d, a));
                e.put(clz(false, d, d));
            }),
            I64Ctz { dst, a } => self.unary(dst, a, |e, d, a| {
                e.put(rbit(true, d, a));
                e.put(clz(true, d, d));
            }),
            I32Popcnt { dst, a } | I64Popcnt { dst, a } => {
                let x = matches!(op, I64Popcnt { .. });
                self.unary(dst, a, |e, d, a| {
                    e.put(if x { fmov_to_d(0, a) } else { fmov_s_from_w(0, a) });
                    e.put(cnt8b(0, 0));
                    e.put(addv8b(0, 0));
                    e.put(fmov_w_from_s(d, 0));
                });
            }
            I32Add { dst, a, b } => self.bin(false, dst, a, b, add),
            I32Sub { dst, a, b } => self.bin(false, dst, a, b, sub),
            I32Mul { dst, a, b } => self.bin(false, dst, a, b, mul),
            I32And { dst, a, b } => self.bin(false, dst, a, b, and),
            I32Or { dst, a, b } => self.bin(false, dst, a, b, orr),
            I32Xor { dst, a, b } => self.bin(false, dst, a, b, eor),
            I32Shl { dst, a, b } => self.bin(false, dst, a, b, lslv),
            I32ShrS { dst, a, b } => self.bin(false, dst, a, b, asrv),
            I32ShrU { dst, a, b } => self.bin(false, dst, a, b, lsrv),
            I32Rotr { dst, a, b } => self.bin(false, dst, a, b, rorv),
            I32Rotl { dst, a, b } | I64Rotl { dst, a, b } => {
                let x = matches!(op, I64Rotl { .. });
                let (a, b) = (self.read(a), self.read(b));
                self.put(sub(x, T1, ZR, b));
                let d = self.write(dst);
                self.put(rorv(x, d, a, T1));
            }
            I64Add { dst, a, b } => self.bin(true, dst, a, b, add),
            I64Sub { dst, a, b } => self.bin(true, dst, a, b, sub),
            I64Mul { dst, a, b } => self.bin(true, dst, a, b, mul),
            I64And { dst, a, b } => self.bin(true, dst, a, b, and),
            I64Or { dst, a, b } => self.bin(true, dst, a, b, orr),
            I64Xor { dst, a, b } => self.bin(true, dst, a, b, eor),
            I64Shl { dst, a, b } => self.bin(true, dst, a, b, lslv),
            I64ShrS { dst, a, b } => self.bin(true, dst, a, b, asrv),
            I64ShrU { dst, a, b } => self.bin(true, dst, a, b, lsrv),
            I64Rotr { dst, a, b } => self.bin(true, dst, a, b, rorv),
            I32AddImm { dst, a, imm } => self.add_sub_imm(false, dst, a, imm as i64),
            I32SubImm { dst, a, imm } => self.add_sub_imm(false, dst, a, -(imm as i64)),
            I64AddImm { dst, a, imm } => self.add_sub_imm(true, dst, a, imm as i64),
            I64SubImm { dst, a, imm } => self.add_sub_imm(true, dst, a, -(imm as i64)),
            I32MulImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, mul),
            I32AndImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, and),
            I32OrImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, orr),
            I32XorImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, eor),
            I32ShlImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, lslv),
            I32ShrSImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, asrv),
            I32ShrUImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, lsrv),
            I32RotrImm { dst, a, imm } => self.bin_imm(false, dst, a, imm as i64, rorv),
            I32RotlImm { dst, a, imm } => self.bin_imm(false, dst, a, ((32 - (imm & 31)) & 31) as i64, rorv),
            I64MulImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, mul),
            I64AndImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, and),
            I64OrImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, orr),
            I64XorImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, eor),
            I64ShlImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, lslv),
            I64ShrSImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, asrv),
            I64ShrUImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, lsrv),
            I64RotrImm { dst, a, imm } => self.bin_imm(true, dst, a, imm as i64, rorv),
            I64RotlImm { dst, a, imm } => self.bin_imm(true, dst, a, ((64 - (imm & 63)) & 63) as i64, rorv),
            I32DivS { dst, a, b } => self.div(false, true, false, dst, a, b),
            I32DivU { dst, a, b } => self.div(false, false, false, dst, a, b),
            I32RemS { dst, a, b } => self.div(false, true, true, dst, a, b),
            I32RemU { dst, a, b } => self.div(false, false, true, dst, a, b),
            I64DivS { dst, a, b } => self.div(true, true, false, dst, a, b),
            I64DivU { dst, a, b } => self.div(true, false, false, dst, a, b),
            I64RemS { dst, a, b } => self.div(true, true, true, dst, a, b),
            I64RemU { dst, a, b } => self.div(true, false, true, dst, a, b),

            F32Abs { dst, a } => self.fun(false, dst, a, fabs),
            F32Neg { dst, a } => self.fun(false, dst, a, fneg),
            F32Ceil { dst, a } => self.fun(false, dst, a, frintp),
            F32Floor { dst, a } => self.fun(false, dst, a, frintm),
            F32Trunc { dst, a } => self.fun(false, dst, a, frintz),
            F32Nearest { dst, a } => self.fun(false, dst, a, frintn),
            F32Sqrt { dst, a } => self.fun(false, dst, a, fsqrt),
            F64Abs { dst, a } => self.fun(true, dst, a, fabs),
            F64Neg { dst, a } => self.fun(true, dst, a, fneg),
            F64Ceil { dst, a } => self.fun(true, dst, a, frintp),
            F64Floor { dst, a } => self.fun(true, dst, a, frintm),
            F64Trunc { dst, a } => self.fun(true, dst, a, frintz),
            F64Nearest { dst, a } => self.fun(true, dst, a, frintn),
            F64Sqrt { dst, a } => self.fun(true, dst, a, fsqrt),
            F32Add { dst, a, b } => self.fbin(false, dst, a, b, fadd),
            F32Sub { dst, a, b } => self.fbin(false, dst, a, b, fsub),
            F32Mul { dst, a, b } => self.fbin(false, dst, a, b, fmul),
            F32Div { dst, a, b } => self.fbin(false, dst, a, b, fdiv),
            F32Min { dst, a, b } => self.fbin(false, dst, a, b, fmin),
            F32Max { dst, a, b } => self.fbin(false, dst, a, b, fmax),
            F64Add { dst, a, b } => self.fbin(true, dst, a, b, fadd),
            F64Sub { dst, a, b } => self.fbin(true, dst, a, b, fsub),
            F64Mul { dst, a, b } => self.fbin(true, dst, a, b, fmul),
            F64Div { dst, a, b } => self.fbin(true, dst, a, b, fdiv),
            F64Min { dst, a, b } => self.fbin(true, dst, a, b, fmin),
            F64Max { dst, a, b } => self.fbin(true, dst, a, b, fmax),
            F32Copysign { dst, a, b } | F64Copysign { dst, a, b } => {
                let x = matches!(op, F64Copysign { .. });
                let (a, b) = (self.read(a), self.read(b));
                self.put(movz(x, T2, 0x8000, if x { 3 } else { 1 }));
                self.put(bic(x, T0, a, T2));
                self.put(and(x, T1, b, T2));
                let d = self.write(dst);
                self.put(orr(x, d, T0, T1));
            }

            I32WrapI64 { dst, a } | I64ExtendI32U { dst, a } => {
                self.unary(dst, a, |e, d, a| e.put(mov(false, d, a)))
            }
            I64ExtendI32S { dst, a } | I64Extend32S { dst, a } => {
                self.unary(dst, a, |e, d, a| e.put(sxtw(d, a)))
            }
            I32Extend8S { dst, a } => self.unary(dst, a, |e, d, a| e.put(sxtb(false, d, a))),
            I32Extend16S { dst, a } => self.unary(dst, a, |e, d, a| e.put(sxth(false, d, a))),
            I64Extend8S { dst, a } => self.unary(dst, a, |e, d, a| e.put(sxtb(true, d, a))),
            I64Extend16S { dst, a } => self.unary(dst, a, |e, d, a| e.put(sxth(true, d, a))),
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
            F32DemoteF64 { dst, a } => {
                let a = self.fsrc(a);
                let r = self.fdst(dst);
                self.put(fcvt_narrow(r, a));
            }
            F64PromoteF32 { dst, a } => {
                let a = self.fsrc(a);
                let r = self.fdst(dst);
                self.put(fcvt_widen(r, a));
            }
        }
    }
}
