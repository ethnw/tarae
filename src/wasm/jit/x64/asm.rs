//! x86-64 instruction encodings — just the ones the JIT emits (baseline x86-64 plus SSE4.1's `roundss`).
//! Instructions are variable length: an optional legacy prefix (`66`/`F2`/`F3`), an optional REX prefix,
//! the opcode, a ModRM byte naming a register and a register-or-memory operand, maybe a SIB byte and a
//! displacement, then any immediate.

pub(super) type Reg = u8;

pub(super) const RAX: Reg = 0;
pub(super) const RCX: Reg = 1;
pub(super) const RDX: Reg = 2;
pub(super) const RBX: Reg = 3;
pub(super) const RSP: Reg = 4;
pub(super) const RBP: Reg = 5;
pub(super) const RSI: Reg = 6;
pub(super) const RDI: Reg = 7;
pub(super) const R8: Reg = 8;
pub(super) const R9: Reg = 9;
pub(super) const R10: Reg = 10;
pub(super) const R11: Reg = 11;
pub(super) const R12: Reg = 12;
pub(super) const R13: Reg = 13;
pub(super) const R14: Reg = 14;
pub(super) const R15: Reg = 15;

/// Condition codes (the low nibble of `jcc`/`setcc`/`cmovcc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Cc {
    B = 2,
    Ae = 3,
    E = 4,
    Ne = 5,
    Be = 6,
    A = 7,
    S = 8,
    P = 10,
    Np = 11,
    L = 12,
    Ge = 13,
    Le = 14,
    G = 15,
}

/// A memory operand: `[base + index + disp]`.
#[derive(Clone, Copy, Debug)]
pub(super) struct M {
    pub base: Reg,
    pub index: Option<Reg>,
    /// log2 of the index's scale.
    pub scale: u8,
    pub disp: i32,
}

pub(super) fn at(base: Reg, disp: i32) -> M {
    M { base, index: None, scale: 0, disp }
}

pub(super) fn indexed(base: Reg, index: Reg) -> M {
    M { base, index: Some(index), scale: 0, disp: 0 }
}

/// The operand ModRM's r/m field names.
#[derive(Clone, Copy)]
enum Rm {
    Reg(Reg),
    Mem(M),
}

/// Which operand, if any, is an 8-bit register.
#[derive(Clone, Copy)]
enum Byte {
    None,
    Reg,
    Rm,
}

/// Legacy prefixes.
pub(super) const P66: u8 = 0x66;
pub(super) const PF2: u8 = 0xF2;
pub(super) const PF3: u8 = 0xF3;

#[derive(Default)]
pub(super) struct Asm {
    pub b: Vec<u8>,
}

impl Asm {
    pub fn len(&self) -> usize {
        self.b.len()
    }

    fn u8(&mut self, x: u8) {
        self.b.push(x);
    }

    fn i32(&mut self, x: i32) {
        self.b.extend_from_slice(&x.to_le_bytes());
    }

    /// The general form. `w`: 64-bit operand (REX.W). `byte`: which operand is an 8-bit register, which
    /// needs a REX prefix to mean `sil`/`dil`/`spl`/`bpl` rather than `dh`/`bh`/`ah`/`ch`.
    fn enc(&mut self, prefix: Option<u8>, w: bool, byte: Byte, op: &[u8], reg: Reg, rm: Rm) {
        if let Some(p) = prefix {
            self.u8(p);
        }
        let (b, x) = match rm {
            Rm::Reg(r) => (r >> 3, 0),
            Rm::Mem(m) => (m.base >> 3, m.index.map_or(0, |i| i >> 3)),
        };
        let rex = 0x40 | (w as u8) << 3 | (reg >> 3) << 2 | x << 1 | b;
        let low_byte = |r: Reg| (4..8).contains(&r);
        let needs = match byte {
            Byte::None => false,
            Byte::Reg => low_byte(reg),
            Byte::Rm => matches!(rm, Rm::Reg(r) if low_byte(r)),
        };
        if rex != 0x40 || needs {
            self.u8(rex);
        }
        self.b.extend_from_slice(op);
        let reg = (reg & 7) << 3;
        match rm {
            Rm::Reg(r) => self.u8(0xC0 | reg | (r & 7)),
            Rm::Mem(m) => {
                let base = m.base & 7;
                // rbp/r13 as a base always need a displacement; rsp/r12 always need a SIB byte.
                let md = if m.disp == 0 && base != 5 {
                    0
                } else if i8::try_from(m.disp).is_ok() {
                    1
                } else {
                    2
                };
                match m.index {
                    Some(i) => {
                        self.u8(md << 6 | reg | 4);
                        self.u8(m.scale << 6 | (i & 7) << 3 | base);
                    }
                    None if base == 4 => {
                        self.u8(md << 6 | reg | 4);
                        self.u8(0x24);
                    }
                    None => self.u8(md << 6 | reg | base),
                }
                match md {
                    1 => self.u8(m.disp as u8),
                    2 => self.i32(m.disp),
                    _ => {}
                }
            }
        }
    }

    // ── Moves ───────────────────────────────────────────────────────────────

    pub fn mov_rr(&mut self, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[0x89], src, Rm::Reg(dst));
    }
    pub fn load(&mut self, w: bool, dst: Reg, m: M) {
        self.enc(None, w, Byte::None, &[0x8B], dst, Rm::Mem(m));
    }
    pub fn store(&mut self, w: bool, m: M, src: Reg) {
        self.enc(None, w, Byte::None, &[0x89], src, Rm::Mem(m));
    }
    pub fn store8(&mut self, m: M, src: Reg) {
        self.enc(None, false, Byte::Reg, &[0x88], src, Rm::Mem(m));
    }
    pub fn store16(&mut self, m: M, src: Reg) {
        self.enc(Some(P66), false, Byte::None, &[0x89], src, Rm::Mem(m));
    }
    /// `mov qword/dword [m], imm32` (sign-extended for a qword).
    pub fn store_imm(&mut self, w: bool, m: M, imm: i32) {
        self.enc(None, w, Byte::None, &[0xC7], 0, Rm::Mem(m));
        self.i32(imm);
    }
    /// Loads that widen: `movzx`/`movsx` from 8 or 16 bits, `movsxd` from 32.
    pub fn movzx8(&mut self, dst: Reg, m: M) {
        self.enc(None, false, Byte::None, &[0x0F, 0xB6], dst, Rm::Mem(m));
    }
    pub fn movzx16(&mut self, dst: Reg, m: M) {
        self.enc(None, false, Byte::None, &[0x0F, 0xB7], dst, Rm::Mem(m));
    }
    pub fn movsx8(&mut self, w: bool, dst: Reg, m: M) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBE], dst, Rm::Mem(m));
    }
    pub fn movsx16(&mut self, w: bool, dst: Reg, m: M) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBF], dst, Rm::Mem(m));
    }
    pub fn movsxd(&mut self, dst: Reg, m: M) {
        self.enc(None, true, Byte::None, &[0x63], dst, Rm::Mem(m));
    }
    /// Register forms of the widening moves (sign extension ops, `setcc` results).
    pub fn movzx8_rr(&mut self, dst: Reg, src: Reg) {
        self.enc(None, false, Byte::Rm, &[0x0F, 0xB6], dst, Rm::Reg(src));
    }
    pub fn movsx8_rr(&mut self, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::Rm, &[0x0F, 0xBE], dst, Rm::Reg(src));
    }
    pub fn movzx16_rr(&mut self, dst: Reg, src: Reg) {
        self.enc(None, false, Byte::None, &[0x0F, 0xB7], dst, Rm::Reg(src));
    }
    pub fn movsx16_rr(&mut self, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBF], dst, Rm::Reg(src));
    }
    pub fn movsxd_rr(&mut self, dst: Reg, src: Reg) {
        self.enc(None, true, Byte::None, &[0x63], dst, Rm::Reg(src));
    }

    /// Any constant, in the fewest bytes.
    pub fn mov_imm(&mut self, dst: Reg, v: u64) {
        if v == 0 {
            self.alu_rr(Alu::Xor, false, dst, dst);
        } else if v <= u32::MAX as u64 {
            if dst >= 8 {
                self.u8(0x41);
            }
            self.u8(0xB8 + (dst & 7));
            self.i32(v as u32 as i32);
        } else if let Ok(v) = i32::try_from(v as i64) {
            self.enc(None, true, Byte::None, &[0xC7], 0, Rm::Reg(dst));
            self.i32(v);
        } else {
            self.u8(0x48 | (dst >> 3));
            self.u8(0xB8 + (dst & 7));
            self.b.extend_from_slice(&v.to_le_bytes());
        }
    }

    pub fn lea(&mut self, w: bool, dst: Reg, m: M) {
        self.enc(None, w, Byte::None, &[0x8D], dst, Rm::Mem(m));
    }

    // ── Arithmetic ──────────────────────────────────────────────────────────

    pub fn alu_rr(&mut self, op: Alu, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[op as u8 * 8 + 1], src, Rm::Reg(dst));
    }
    pub fn alu_ri(&mut self, op: Alu, w: bool, dst: Reg, imm: i32) {
        self.alu_i(op, w, Rm::Reg(dst), imm);
    }
    pub fn alu_mi(&mut self, op: Alu, w: bool, m: M, imm: i32) {
        self.alu_i(op, w, Rm::Mem(m), imm);
    }
    fn alu_i(&mut self, op: Alu, w: bool, rm: Rm, imm: i32) {
        match i8::try_from(imm) {
            Ok(i) => {
                self.enc(None, w, Byte::None, &[0x83], op as u8, rm);
                self.u8(i as u8);
            }
            Err(_) if matches!(rm, Rm::Reg(RAX)) => {
                // The accumulator's short form.
                if w {
                    self.u8(0x48);
                }
                self.u8(op as u8 * 8 + 5);
                self.i32(imm);
            }
            Err(_) => {
                self.enc(None, w, Byte::None, &[0x81], op as u8, rm);
                self.i32(imm);
            }
        }
    }
    /// `cmp reg, [m]`
    pub fn cmp_rm(&mut self, w: bool, reg: Reg, m: M) {
        self.enc(None, w, Byte::None, &[0x3B], reg, Rm::Mem(m));
    }
    pub fn test(&mut self, w: bool, a: Reg, b: Reg) {
        self.enc(None, w, Byte::None, &[0x85], b, Rm::Reg(a));
    }
    pub fn imul(&mut self, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[0x0F, 0xAF], dst, Rm::Reg(src));
    }
    pub fn imul_imm(&mut self, w: bool, dst: Reg, src: Reg, imm: i32) {
        match i8::try_from(imm) {
            Ok(i) => {
                self.enc(None, w, Byte::None, &[0x6B], dst, Rm::Reg(src));
                self.u8(i as u8);
            }
            Err(_) => {
                self.enc(None, w, Byte::None, &[0x69], dst, Rm::Reg(src));
                self.i32(imm);
            }
        }
    }
    /// `div`/`idiv` of rdx:rax by `src`.
    pub fn div(&mut self, w: bool, signed: bool, src: Reg) {
        self.enc(None, w, Byte::None, &[0xF7], if signed { 7 } else { 6 }, Rm::Reg(src));
    }
    /// `cdq`/`cqo`: sign-extend rax into rdx.
    pub fn sign_extend_rax(&mut self, w: bool) {
        if w {
            self.u8(0x48);
        }
        self.u8(0x99);
    }
    pub fn shift_cl(&mut self, op: Shift, w: bool, dst: Reg) {
        self.enc(None, w, Byte::None, &[0xD3], op as u8, Rm::Reg(dst));
    }
    pub fn shift_imm(&mut self, op: Shift, w: bool, dst: Reg, n: u8) {
        self.enc(None, w, Byte::None, &[0xC1], op as u8, Rm::Reg(dst));
        self.u8(n);
    }
    #[cfg(test)] // encoded and checked, not used yet
    pub fn neg(&mut self, w: bool, r: Reg) {
        self.enc(None, w, Byte::None, &[0xF7], 3, Rm::Reg(r));
    }
    pub fn bsr(&mut self, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBD], dst, Rm::Reg(src));
    }
    pub fn bsf(&mut self, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBC], dst, Rm::Reg(src));
    }
    /// `btr`/`btc r, imm8` — clear / flip one bit.
    pub fn btr(&mut self, w: bool, r: Reg, bit: u8) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBA], 6, Rm::Reg(r));
        self.u8(bit);
    }
    pub fn btc(&mut self, w: bool, r: Reg, bit: u8) {
        self.enc(None, w, Byte::None, &[0x0F, 0xBA], 7, Rm::Reg(r));
        self.u8(bit);
    }
    pub fn setcc(&mut self, cc: Cc, dst: Reg) {
        self.enc(None, false, Byte::Rm, &[0x0F, 0x90 + cc as u8], 0, Rm::Reg(dst));
    }
    pub fn cmov(&mut self, cc: Cc, w: bool, dst: Reg, src: Reg) {
        self.enc(None, w, Byte::None, &[0x0F, 0x40 + cc as u8], dst, Rm::Reg(src));
    }

    // ── Control ─────────────────────────────────────────────────────────────

    /// `jmp rel32`; returns where the displacement is, to patch.
    pub fn jmp(&mut self) -> usize {
        self.u8(0xE9);
        self.i32(0);
        self.len() - 4
    }
    pub fn jcc(&mut self, cc: Cc) -> usize {
        self.u8(0x0F);
        self.u8(0x80 + cc as u8);
        self.i32(0);
        self.len() - 4
    }
    pub fn call(&mut self) -> usize {
        self.u8(0xE8);
        self.i32(0);
        self.len() - 4
    }
    pub fn call_r(&mut self, r: Reg) {
        self.enc(None, false, Byte::None, &[0xFF], 2, Rm::Reg(r));
    }
    pub fn jmp_r(&mut self, r: Reg) {
        self.enc(None, false, Byte::None, &[0xFF], 4, Rm::Reg(r));
    }
    pub fn ret(&mut self) {
        self.u8(0xC3);
    }
    pub fn push(&mut self, r: Reg) {
        if r >= 8 {
            self.u8(0x41);
        }
        self.u8(0x50 + (r & 7));
    }
    pub fn pop(&mut self, r: Reg) {
        if r >= 8 {
            self.u8(0x41);
        }
        self.u8(0x58 + (r & 7));
    }
    /// `lea rcx, [rip + rel32]`; returns where the displacement is.
    pub fn lea_rip(&mut self, dst: Reg) -> usize {
        self.u8(0x48 | (dst >> 3) << 2);
        self.u8(0x8D);
        self.u8(0x05 | (dst & 7) << 3);
        self.i32(0);
        self.len() - 4
    }
    /// `movsxd rax, dword [base + index*4]` (jump tables).
    pub fn movsxd_table(&mut self, dst: Reg, base: Reg, index: Reg) {
        self.enc(
            None,
            true,
            Byte::None,
            &[0x63],
            dst,
            Rm::Mem(M { base, index: Some(index), scale: 2, disp: 0 }),
        );
    }
    pub fn patch_rel32(&mut self, at: usize, to: usize) {
        let rel = to as i64 - (at as i64 + 4);
        self.b[at..at + 4].copy_from_slice(&(rel as i32).to_le_bytes());
    }
    pub fn put_i32(&mut self, v: i32) {
        self.i32(v);
    }
    pub fn patch_i32(&mut self, at: usize, v: i32) {
        self.b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    // ── SSE ─────────────────────────────────────────────────────────────────

    /// `movss`/`movsd` xmm ← [m] and [m] ← xmm (`d`: double).
    pub fn fload(&mut self, d: bool, x: Reg, m: M) {
        self.enc(Some(if d { PF2 } else { PF3 }), false, Byte::None, &[0x0F, 0x10], x, Rm::Mem(m));
    }
    pub fn fstore(&mut self, d: bool, m: M, x: Reg) {
        self.enc(Some(if d { PF2 } else { PF3 }), false, Byte::None, &[0x0F, 0x11], x, Rm::Mem(m));
    }
    /// Scalar arithmetic: `op` is the second opcode byte (`58` add, `59` mul, `5C` sub, `5D` min, `5E` div,
    /// `5F` max, `51` sqrt, `5A` convert between precisions).
    pub fn fop(&mut self, d: bool, op: u8, dst: Reg, src: Reg) {
        self.enc(Some(if d { PF2 } else { PF3 }), false, Byte::None, &[0x0F, op], dst, Rm::Reg(src));
    }
    /// `ucomiss`/`ucomisd a, b`.
    pub fn ucomis(&mut self, d: bool, a: Reg, b: Reg) {
        let prefix = if d { Some(P66) } else { None };
        self.enc(prefix, false, Byte::None, &[0x0F, 0x2E], a, Rm::Reg(b));
    }
    /// `movd`/`movq` xmm ← gpr and gpr ← xmm.
    pub fn movd_to_xmm(&mut self, w: bool, x: Reg, r: Reg) {
        self.enc(Some(P66), w, Byte::None, &[0x0F, 0x6E], x, Rm::Reg(r));
    }
    pub fn movd_from_xmm(&mut self, w: bool, r: Reg, x: Reg) {
        self.enc(Some(P66), w, Byte::None, &[0x0F, 0x7E], x, Rm::Reg(r));
    }
    /// int → float (`w`: 64-bit source).
    pub fn cvtsi2(&mut self, d: bool, w: bool, x: Reg, r: Reg) {
        self.enc(Some(if d { PF2 } else { PF3 }), w, Byte::None, &[0x0F, 0x2A], x, Rm::Reg(r));
    }
    /// float → int, truncating (`w`: 64-bit result).
    pub fn cvtt2si(&mut self, d: bool, w: bool, r: Reg, x: Reg) {
        self.enc(Some(if d { PF2 } else { PF3 }), w, Byte::None, &[0x0F, 0x2C], r, Rm::Reg(x));
    }
    /// `roundss`/`roundsd` with a rounding mode (exceptions suppressed).
    pub fn round(&mut self, d: bool, x: Reg, src: Reg, mode: u8) {
        self.enc(Some(P66), false, Byte::None, &[0x0F, 0x3A, if d { 0x0B } else { 0x0A }], x, Rm::Reg(src));
        self.u8(8 | mode);
    }
    /// `movaps` xmm ← xmm: a float register copy.
    pub fn movaps(&mut self, x: Reg, src: Reg) {
        self.enc(None, false, Byte::None, &[0x0F, 0x28], x, Rm::Reg(src));
    }
    /// `orps`/`andps` (bitwise, for ±0 in min/max).
    pub fn orps(&mut self, x: Reg, src: Reg) {
        self.enc(None, false, Byte::None, &[0x0F, 0x56], x, Rm::Reg(src));
    }
    pub fn andps(&mut self, x: Reg, src: Reg) {
        self.enc(None, false, Byte::None, &[0x0F, 0x54], x, Rm::Reg(src));
    }
}

/// The `81 /digit` group.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum Alu {
    Add = 0,
    Or = 1,
    And = 4,
    Sub = 5,
    Xor = 6,
    Cmp = 7,
}

/// The `D3 /digit` group.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum Shift {
    Rol = 0,
    Ror = 1,
    Shl = 4,
    Shr = 5,
    Sar = 7,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(f: impl FnOnce(&mut Asm)) -> String {
        let mut a = Asm::default();
        f(&mut a);
        a.b.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Encodings checked against clang's assembler (`clang -c -target x86_64-apple-macos`).
    #[test]
    fn encodings_match_the_assembler() {
        let cases: Vec<(String, &str, &str)> = vec![
            (one(|a| a.mov_rr(true, RAX, RSI)), "4889f0", "mov rax, rsi"),
            (one(|a| a.mov_rr(false, R9, RAX)), "4189c1", "mov r9d, eax"),
            (one(|a| a.load(true, RSI, at(RBX, 16))), "488b7310", "mov rsi, [rbx+16]"),
            (one(|a| a.load(true, R8, at(RBX, 4096))), "4c8b8300100000", "mov r8, [rbx+4096]"),
            (one(|a| a.load(true, RAX, at(R12, 8))), "498b442408", "mov rax, [r12+8]"),
            (one(|a| a.load(true, R13, at(R12, 0))), "4d8b2c24", "mov r13, [r12]"),
            (one(|a| a.load(true, RAX, at(RBP, 0))), "488b4500", "mov rax, [rbp]"),
            (one(|a| a.store(true, at(RBX, 24), RDI)), "48897b18", "mov [rbx+24], rdi"),
            (one(|a| a.load(false, RSI, indexed(R13, RAX))), "418b740500", "mov esi, [r13+rax]"),
            (one(|a| a.store8(indexed(R13, RAX), RSI)), "4188740500", "mov [r13+rax], sil"),
            (one(|a| a.store16(indexed(R13, RAX), R9)), "6645894c0500", "mov [r13+rax], r9w"),
            (one(|a| a.store_imm(true, at(RBX, 8), 0)), "48c7430800000000", "mov qword ptr [rbx+8], 0"),
            (one(|a| a.movzx8(RSI, indexed(R13, RAX))), "410fb6740500", "movzx esi, byte ptr [r13+rax]"),
            (
                one(|a| a.movsx16(true, RDI, indexed(R13, RAX))),
                "490fbf7c0500",
                "movsx rdi, word ptr [r13+rax]",
            ),
            (one(|a| a.movsxd(R8, indexed(R13, RAX))), "4d63440500", "movsxd r8, dword ptr [r13+rax]"),
            (one(|a| a.movzx8_rr(RSI, RAX)), "0fb6f0", "movzx esi, al"),
            (one(|a| a.movsx8_rr(false, RAX, RSI)), "400fbec6", "movsx eax, sil"),
            (one(|a| a.movsxd_rr(RSI, RDI)), "4863f7", "movsxd rsi, edi"),
            (one(|a| a.movzx16_rr(RAX, R9)), "410fb7c1", "movzx eax, r9w"),
            (
                one(|a| a.load(true, RSI, M { base: R13, index: Some(RAX), scale: 0, disp: 12 })),
                "498b74050c",
                "mov rsi, [r13+rax+12]",
            ),
            (one(|a| a.mov_imm(RAX, 0)), "31c0", "xor eax, eax"),
            (one(|a| a.mov_imm(R10, 31)), "41ba1f000000", "mov r10d, 31"),
            (one(|a| a.mov_imm(RCX, u64::MAX)), "48c7c1ffffffff", "mov rcx, -1"),
            (
                one(|a| a.mov_imm(RDX, 0x5555_5555_5555_5555)),
                "48ba5555555555555555",
                "movabs rdx, 0x5555555555555555",
            ),
            (one(|a| a.lea(false, RSI, at(RDI, 1))), "8d7701", "lea esi, [rdi+1]"),
            (one(|a| a.alu_rr(Alu::Add, false, RAX, R8)), "4401c0", "add eax, r8d"),
            (one(|a| a.alu_rr(Alu::Cmp, true, RSI, RDI)), "4839fe", "cmp rsi, rdi"),
            (one(|a| a.alu_ri(Alu::Sub, true, R14, 5)), "4983ee05", "sub r14, 5"),
            (one(|a| a.alu_ri(Alu::And, false, RAX, 0x7FFF_FFFF)), "25ffffff7f", "and eax, 0x7fffffff"),
            (one(|a| a.alu_mi(Alu::Sub, true, at(R12, 24), 1)), "49836c241801", "sub qword ptr [r12+24], 1"),
            (one(|a| a.cmp_rm(true, RAX, at(R12, 32))), "493b442420", "cmp rax, [r12+32]"),
            (one(|a| a.test(false, RSI, RSI)), "85f6", "test esi, esi"),
            (one(|a| a.imul(false, RAX, R9)), "410fafc1", "imul eax, r9d"),
            (one(|a| a.imul_imm(true, RSI, RDI, 31)), "486bf71f", "imul rsi, rdi, 31"),
            (one(|a| a.div(false, true, R8)), "41f7f8", "idiv r8d"),
            (one(|a| a.div(true, false, RSI)), "48f7f6", "div rsi"),
            (one(|a| a.sign_extend_rax(true)), "4899", "cqo"),
            (one(|a| a.shift_cl(Shift::Shl, false, RAX)), "d3e0", "shl eax, cl"),
            (one(|a| a.shift_imm(Shift::Sar, true, RAX, 3)), "48c1f803", "sar rax, 3"),
            (one(|a| a.shift_imm(Shift::Rol, false, RAX, 5)), "c1c005", "rol eax, 5"),
            (one(|a| a.neg(false, RCX)), "f7d9", "neg ecx"),
            (one(|a| a.bsr(true, RAX, RSI)), "480fbdc6", "bsr rax, rsi"),
            (one(|a| a.bsf(false, RAX, R11)), "410fbcc3", "bsf eax, r11d"),
            (one(|a| a.btr(true, RAX, 63)), "480fbaf03f", "btr rax, 63"),
            (one(|a| a.btc(true, RAX, 63)), "480fbaf83f", "btc rax, 63"),
            (one(|a| a.setcc(Cc::L, RAX)), "0f9cc0", "setl al"),
            (one(|a| a.cmov(Cc::E, true, RAX, RCX)), "480f44c1", "cmove rax, rcx"),
            (one(|a| a.call_r(RAX)), "ffd0", "call rax"),
            (one(|a| a.jmp_r(RAX)), "ffe0", "jmp rax"),
            (one(|a| a.push(R15)), "4157", "push r15"),
            (one(|a| a.pop(RBX)), "5b", "pop rbx"),
            (one(|a| a.movsxd_table(RAX, RCX, RAX)), "48630481", "movsxd rax, dword ptr [rcx+rax*4]"),
            (one(|a| a.fload(false, 0, at(RBX, 16))), "f30f104310", "movss xmm0, [rbx+16]"),
            (one(|a| a.fstore(true, at(RBX, 8), 1)), "f20f114b08", "movsd [rbx+8], xmm1"),
            (one(|a| a.fop(false, 0x58, 0, 1)), "f30f58c1", "addss xmm0, xmm1"),
            (one(|a| a.fop(true, 0x5A, 0, 0)), "f20f5ac0", "cvtsd2ss xmm0, xmm0"),
            (one(|a| a.ucomis(true, 0, 1)), "660f2ec1", "ucomisd xmm0, xmm1"),
            (one(|a| a.ucomis(false, 1, 0)), "0f2ec8", "ucomiss xmm1, xmm0"),
            (one(|a| a.movd_to_xmm(true, 1, RCX)), "66480f6ec9", "movq xmm1, rcx"),
            (one(|a| a.movd_to_xmm(false, 0, RSI)), "660f6ec6", "movd xmm0, esi"),
            (one(|a| a.movd_from_xmm(false, RAX, 0)), "660f7ec0", "movd eax, xmm0"),
            (one(|a| a.cvtsi2(true, true, 0, RAX)), "f2480f2ac0", "cvtsi2sd xmm0, rax"),
            (one(|a| a.cvtt2si(true, false, RAX, 0)), "f20f2cc0", "cvttsd2si eax, xmm0"),
            (one(|a| a.round(false, 0, 0, 2)), "660f3a0ac00a", "roundss xmm0, xmm0, 10"),
            (one(|a| a.orps(0, 1)), "0f56c1", "orps xmm0, xmm1"),
            (one(|a| a.movaps(12, 3)), "440f28e3", "movaps xmm12, xmm3"),
            (one(|a| a.movaps(2, 14)), "410f28d6", "movaps xmm2, xmm14"),
            (one(|a| a.movaps(0, 9)), "410f28c1", "movaps xmm0, xmm9"),
            (one(|a| a.fop(true, 0x58, 12, 9)), "f2450f58e1", "addsd xmm12, xmm9"),
            (one(|a| a.fload(true, 12, at(RBX, 16))), "f2440f106310", "movsd xmm12, [rbx+16]"),
            (one(|a| a.fstore(true, at(RBX, 16), 9)), "f2440f114b10", "movsd [rbx+16], xmm9"),
            (one(|a| a.movd_from_xmm(true, R8, 13)), "664d0f7ee8", "movq r8, xmm13"),
            (one(|a| a.movd_to_xmm(true, 12, RSI)), "664c0f6ee6", "movq xmm12, rsi"),
            (one(|a| a.ucomis(true, 9, 2)), "66440f2eca", "ucomisd xmm9, xmm2"),
            (one(|a| a.fop(true, 0x51, 3, 12)), "f2410f51dc", "sqrtsd xmm3, xmm12"),
            (one(|a| a.fop(true, 0x5A, 10, 4)), "f2440f5ad4", "cvtsd2ss xmm10, xmm4"),
            (one(|a| a.round(true, 12, 2, 1)), "66440f3a0be209", "roundsd xmm12, xmm2, 9"),
            (one(|a| a.cvtsi2(true, true, 11, RDI)), "f24c0f2adf", "cvtsi2sd xmm11, rdi"),
            (one(|a| a.andps(0, 1)), "0f54c1", "andps xmm0, xmm1"),
        ];
        for (got, want, text) in cases {
            assert_eq!(got, want, "{text}");
        }
    }
}
