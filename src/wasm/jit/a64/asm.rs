//! AArch64 instruction encodings — just the ones the JIT emits. Each function returns one 32-bit
//! instruction; `Asm` collects them. Register 31 is `sp` or `zr` depending on the instruction.

pub(super) type Reg = u32;

pub(super) const ZR: Reg = 31;
pub(super) const SP: Reg = 31;

/// Condition codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum Cond {
    Eq = 0,
    Ne = 1,
    Hs = 2,
    Lo = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    Hi = 8,
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
}

impl Cond {
    pub fn invert(self) -> Cond {
        match self {
            Cond::Eq => Cond::Ne,
            Cond::Ne => Cond::Eq,
            Cond::Hs => Cond::Lo,
            Cond::Lo => Cond::Hs,
            Cond::Mi => Cond::Pl,
            Cond::Pl => Cond::Mi,
            Cond::Vs => Cond::Vc,
            Cond::Vc => Cond::Vs,
            Cond::Hi => Cond::Ls,
            Cond::Ls => Cond::Hi,
            Cond::Ge => Cond::Lt,
            Cond::Lt => Cond::Ge,
            Cond::Gt => Cond::Le,
            Cond::Le => Cond::Gt,
        }
    }
}

/// Load/store kinds, as their unsigned-offset opcode; `scale` = log2 of the access size.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mem {
    pub op: u32,
    pub scale: u32,
}

pub(super) const LDR_X: Mem = Mem { op: 0xF940_0000, scale: 3 };
pub(super) const STR_X: Mem = Mem { op: 0xF900_0000, scale: 3 };
pub(super) const LDR_W: Mem = Mem { op: 0xB940_0000, scale: 2 };
pub(super) const STR_W: Mem = Mem { op: 0xB900_0000, scale: 2 };
pub(super) const LDRB: Mem = Mem { op: 0x3940_0000, scale: 0 };
pub(super) const STRB: Mem = Mem { op: 0x3900_0000, scale: 0 };
pub(super) const LDRH: Mem = Mem { op: 0x7940_0000, scale: 1 };
pub(super) const STRH: Mem = Mem { op: 0x7900_0000, scale: 1 };
pub(super) const LDRSB_W: Mem = Mem { op: 0x39C0_0000, scale: 0 };
pub(super) const LDRSB_X: Mem = Mem { op: 0x3980_0000, scale: 0 };
pub(super) const LDRSH_W: Mem = Mem { op: 0x79C0_0000, scale: 1 };
pub(super) const LDRSH_X: Mem = Mem { op: 0x7980_0000, scale: 1 };
pub(super) const LDRSW: Mem = Mem { op: 0xB980_0000, scale: 2 };
pub(super) const LDR_D: Mem = Mem { op: 0xFD40_0000, scale: 3 };
pub(super) const STR_D: Mem = Mem { op: 0xFD00_0000, scale: 3 };

/// `[rn, #off]` if the offset fits the scaled 12-bit field.
pub(super) fn ldst_imm(m: Mem, rt: Reg, rn: Reg, off: u32) -> Option<u32> {
    let size = 1 << m.scale;
    (off.is_multiple_of(size) && off / size < 4096).then(|| m.op | (off / size) << 10 | rn << 5 | rt)
}

/// `[rn, rm]`.
pub(super) fn ldst_reg(m: Mem, rt: Reg, rn: Reg, rm: Reg) -> u32 {
    (m.op - 0x0100_0000 + 0x0020_6800) | rm << 16 | rn << 5 | rt
}

/// `cmp xn, wm, uxtw` (flags of xn − the zero-extended wm).
pub(super) fn cmp_uxtw(rn: Reg, rm: Reg) -> u32 {
    0xEB20_4000 | rm << 16 | rn << 5 | ZR
}

/// `[rn, wm, uxtw]`: a base plus a zero-extended 32-bit index (unscaled).
pub(super) fn ldst_uxtw(m: Mem, rt: Reg, rn: Reg, rm: Reg) -> u32 {
    (m.op - 0x0100_0000 + 0x0020_4800) | rm << 16 | rn << 5 | rt
}

/// Data-processing sizes: `true` = 64-bit (x), `false` = 32-bit (w).
fn sf(x: bool) -> u32 {
    if x { 1 << 31 } else { 0 }
}

pub(super) fn add_imm(x: bool, rd: Reg, rn: Reg, imm: u32) -> u32 {
    debug_assert!(imm < 4096);
    sf(x) | 0x1100_0000 | imm << 10 | rn << 5 | rd
}

pub(super) fn sub_imm(x: bool, rd: Reg, rn: Reg, imm: u32) -> u32 {
    debug_assert!(imm < 4096);
    sf(x) | 0x5100_0000 | imm << 10 | rn << 5 | rd
}

pub(super) fn subs_imm(x: bool, rd: Reg, rn: Reg, imm: u32) -> u32 {
    debug_assert!(imm < 4096);
    sf(x) | 0x7100_0000 | imm << 10 | rn << 5 | rd
}

pub(super) fn adds_imm(x: bool, rd: Reg, rn: Reg, imm: u32) -> u32 {
    debug_assert!(imm < 4096);
    sf(x) | 0x3100_0000 | imm << 10 | rn << 5 | rd
}

/// Three-register data processing with a fixed opcode (`op` is the 32-bit form).
fn rrr(op: u32, x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    sf(x) | op | rm << 16 | rn << 5 | rd
}

pub(super) fn add(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x0B00_0000, x, rd, rn, rm)
}
pub(super) fn sub(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x4B00_0000, x, rd, rn, rm)
}
pub(super) fn subs(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x6B00_0000, x, rd, rn, rm)
}
pub(super) fn and(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x0A00_0000, x, rd, rn, rm)
}
pub(super) fn bic(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x0A20_0000, x, rd, rn, rm)
}
pub(super) fn orr(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x2A00_0000, x, rd, rn, rm)
}
pub(super) fn eor(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x4A00_0000, x, rd, rn, rm)
}
pub(super) fn mul(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1B00_7C00, x, rd, rn, rm)
}
/// `rd = ra - rn * rm`.
pub(super) fn msub(x: bool, rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    rrr(0x1B00_8000, x, rd, rn, rm) | ra << 10
}
pub(super) fn sdiv(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1AC0_0C00, x, rd, rn, rm)
}
pub(super) fn udiv(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1AC0_0800, x, rd, rn, rm)
}
pub(super) fn lslv(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1AC0_2000, x, rd, rn, rm)
}
pub(super) fn lsrv(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1AC0_2400, x, rd, rn, rm)
}
pub(super) fn asrv(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1AC0_2800, x, rd, rn, rm)
}
pub(super) fn rorv(x: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    rrr(0x1AC0_2C00, x, rd, rn, rm)
}
pub(super) fn clz(x: bool, rd: Reg, rn: Reg) -> u32 {
    sf(x) | 0x5AC0_1000 | rn << 5 | rd
}
pub(super) fn rbit(x: bool, rd: Reg, rn: Reg) -> u32 {
    sf(x) | 0x5AC0_0000 | rn << 5 | rd
}

/// `mov rd, rm` (register).
pub(super) fn mov(x: bool, rd: Reg, rm: Reg) -> u32 {
    orr(x, rd, ZR, rm)
}

/// `mov rd, sp` / `mov sp, rn`.
pub(super) fn mov_sp(rd: Reg, rn: Reg) -> u32 {
    add_imm(true, rd, rn, 0)
}

pub(super) fn csel(x: bool, rd: Reg, rn: Reg, rm: Reg, c: Cond) -> u32 {
    rrr(0x1A80_0000, x, rd, rn, rm) | (c as u32) << 12
}

/// `rd = cond ? 1 : 0` (32-bit).
pub(super) fn cset(rd: Reg, c: Cond) -> u32 {
    0x1A9F_07E0 | (c.invert() as u32) << 12 | rd
}

/// Sign extensions: `sxtb`, `sxth` (to w or x), `sxtw`.
pub(super) fn sxtb(x: bool, rd: Reg, rn: Reg) -> u32 {
    if x { 0x9340_1C00 | rn << 5 | rd } else { 0x1300_1C00 | rn << 5 | rd }
}
pub(super) fn sxth(x: bool, rd: Reg, rn: Reg) -> u32 {
    if x { 0x9340_3C00 | rn << 5 | rd } else { 0x1300_3C00 | rn << 5 | rd }
}
/// Zero extensions of a byte / halfword (32-bit, which clears the rest).
pub(super) fn uxtb(rd: Reg, rn: Reg) -> u32 {
    0x5300_1C00 | rn << 5 | rd
}
pub(super) fn uxth(rd: Reg, rn: Reg) -> u32 {
    0x5300_3C00 | rn << 5 | rd
}
pub(super) fn sxtw(rd: Reg, rn: Reg) -> u32 {
    0x9340_7C00 | rn << 5 | rd
}
/// `lsr xd, xn, #16` (memory pages from bytes).
pub(super) fn lsr16(rd: Reg, rn: Reg) -> u32 {
    0xD350_FC00 | rn << 5 | rd
}

/// `movz`/`movk`/`movn` with a 16-bit chunk at `hw * 16`.
pub(super) fn movz(x: bool, rd: Reg, imm: u32, hw: u32) -> u32 {
    sf(x) | 0x5280_0000 | hw << 21 | imm << 5 | rd
}
pub(super) fn movk(x: bool, rd: Reg, imm: u32, hw: u32) -> u32 {
    sf(x) | 0x7280_0000 | hw << 21 | imm << 5 | rd
}
pub(super) fn movn(x: bool, rd: Reg, imm: u32, hw: u32) -> u32 {
    sf(x) | 0x1280_0000 | hw << 21 | imm << 5 | rd
}

pub(super) fn stp_pre(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xA980_0000 | ((off / 8) as u32 & 0x7F) << 15 | rt2 << 10 | rn << 5 | rt
}
pub(super) fn ldp_post(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xA8C0_0000 | ((off / 8) as u32 & 0x7F) << 15 | rt2 << 10 | rn << 5 | rt
}
pub(super) fn stp(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xA900_0000 | ((off / 8) as u32 & 0x7F) << 15 | rt2 << 10 | rn << 5 | rt
}
pub(super) fn ldp(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xA940_0000 | ((off / 8) as u32 & 0x7F) << 15 | rt2 << 10 | rn << 5 | rt
}
/// `str xt, [rn, #off]!`
pub(super) fn str_pre(rt: Reg, rn: Reg, off: i32) -> u32 {
    0xF800_0C00 | (off as u32 & 0x1FF) << 12 | rn << 5 | rt
}
/// `ldr xt, [rn], #off`
pub(super) fn ldr_post(rt: Reg, rn: Reg, off: i32) -> u32 {
    0xF840_0400 | (off as u32 & 0x1FF) << 12 | rn << 5 | rt
}
/// `str xt, [rn], #off`
pub(super) fn str_post(rt: Reg, rn: Reg, off: i32) -> u32 {
    0xF800_0400 | (off as u32 & 0x1FF) << 12 | rn << 5 | rt
}

/// Branches take a word offset (instructions, not bytes).
pub(super) fn b(off: i32) -> u32 {
    0x1400_0000 | (off as u32 & 0x03FF_FFFF)
}
pub(super) fn bl(off: i32) -> u32 {
    0x9400_0000 | (off as u32 & 0x03FF_FFFF)
}
pub(super) fn b_cond(c: Cond, off: i32) -> u32 {
    0x5400_0000 | (off as u32 & 0x7FFFF) << 5 | c as u32
}
pub(super) fn cbz(x: bool, rt: Reg, off: i32) -> u32 {
    sf(x) | 0x3400_0000 | (off as u32 & 0x7FFFF) << 5 | rt
}
pub(super) fn cbnz(x: bool, rt: Reg, off: i32) -> u32 {
    sf(x) | 0x3500_0000 | (off as u32 & 0x7FFFF) << 5 | rt
}
pub(super) fn br(rn: Reg) -> u32 {
    0xD61F_0000 | rn << 5
}
pub(super) fn blr(rn: Reg) -> u32 {
    0xD63F_0000 | rn << 5
}
pub(super) const RET: u32 = 0xD65F_03C0;
/// `adr rd, #bytes`
pub(super) fn adr(rd: Reg, bytes: i32) -> u32 {
    let b = bytes as u32;
    0x1000_0000 | (b & 3) << 29 | ((b >> 2) & 0x7FFFF) << 5 | rd
}
/// `add xd, xn, wm, uxtw` (memory base + a 32-bit address).
pub(super) fn add_uxtw(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x8B20_4000 | rm << 16 | rn << 5 | rd
}
/// `add xd, xn, wm, uxtw #2` (jump tables).
pub(super) fn add_uxtw2(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x8B20_4800 | rm << 16 | rn << 5 | rd
}

// Floating point: `d` = double (else single).
fn ftype(d: bool) -> u32 {
    if d { 0x0040_0000 } else { 0 }
}
pub(super) fn fadd(d: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1E20_2800 | ftype(d) | rm << 16 | rn << 5 | rd
}
pub(super) fn fsub(d: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1E20_3800 | ftype(d) | rm << 16 | rn << 5 | rd
}
pub(super) fn fmul(d: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1E20_0800 | ftype(d) | rm << 16 | rn << 5 | rd
}
pub(super) fn fdiv(d: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1E20_1800 | ftype(d) | rm << 16 | rn << 5 | rd
}
pub(super) fn fmax(d: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1E20_4800 | ftype(d) | rm << 16 | rn << 5 | rd
}
pub(super) fn fmin(d: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1E20_5800 | ftype(d) | rm << 16 | rn << 5 | rd
}
pub(super) fn fcmp(d: bool, rn: Reg, rm: Reg) -> u32 {
    0x1E20_2000 | ftype(d) | rm << 16 | rn << 5
}
/// One-source ops: `op` is the single-precision form.
fn f1(op: u32, d: bool, rd: Reg, rn: Reg) -> u32 {
    op | ftype(d) | rn << 5 | rd
}
pub(super) fn fabs(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E20_C000, d, rd, rn)
}
pub(super) fn fneg(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E21_4000, d, rd, rn)
}
pub(super) fn fsqrt(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E21_C000, d, rd, rn)
}
pub(super) fn frintn(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E24_4000, d, rd, rn)
}
pub(super) fn frintp(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E24_C000, d, rd, rn)
}
pub(super) fn frintm(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E25_4000, d, rd, rn)
}
pub(super) fn frintz(d: bool, rd: Reg, rn: Reg) -> u32 {
    f1(0x1E25_C000, d, rd, rn)
}
/// `fcvt dd, sn` (widen) / `fcvt sd, dn` (narrow).
pub(super) fn fcvt_widen(rd: Reg, rn: Reg) -> u32 {
    0x1E22_C000 | rn << 5 | rd
}
pub(super) fn fcvt_narrow(rd: Reg, rn: Reg) -> u32 {
    0x1E62_4000 | rn << 5 | rd
}
/// int → float: `x` = 64-bit source, `d` = double destination.
pub(super) fn scvtf(x: bool, d: bool, rd: Reg, rn: Reg) -> u32 {
    sf(x) | 0x1E22_0000 | ftype(d) | rn << 5 | rd
}
pub(super) fn ucvtf(x: bool, d: bool, rd: Reg, rn: Reg) -> u32 {
    sf(x) | 0x1E23_0000 | ftype(d) | rn << 5 | rd
}
/// float → int, rounding toward zero and saturating (NaN → 0): `x` = 64-bit destination, `d` = double
/// source.
pub(super) fn fcvtzs(x: bool, d: bool, rd: Reg, rn: Reg) -> u32 {
    sf(x) | 0x1E38_0000 | ftype(d) | rn << 5 | rd
}
pub(super) fn fcvtzu(x: bool, d: bool, rd: Reg, rn: Reg) -> u32 {
    sf(x) | 0x1E39_0000 | ftype(d) | rn << 5 | rd
}
/// `fmov dd, dn` — a float register copy (all 64 bits, so f32s too).
pub(super) fn fmov_dd(rd: Reg, rn: Reg) -> u32 {
    0x1E60_4000 | rn << 5 | rd
}
/// `fmov dd, xn` / `fmov sd, wn` / `fmov wd, sn`.
pub(super) fn fmov_to_d(rd: Reg, rn: Reg) -> u32 {
    0x9E67_0000 | rn << 5 | rd
}
pub(super) fn fmov_s_from_w(rd: Reg, rn: Reg) -> u32 {
    0x1E27_0000 | rn << 5 | rd
}
pub(super) fn fmov_x_from_d(rd: Reg, rn: Reg) -> u32 {
    0x9E66_0000 | rn << 5 | rd
}
pub(super) fn fmov_w_from_s(rd: Reg, rn: Reg) -> u32 {
    0x1E26_0000 | rn << 5 | rd
}
/// `cnt vd.8b, vn.8b` and `addv bd, vn.8b` — population count through the vector unit.
pub(super) fn cnt8b(rd: Reg, rn: Reg) -> u32 {
    0x0E20_5800 | rn << 5 | rd
}
pub(super) fn addv8b(rd: Reg, rn: Reg) -> u32 {
    0x0E31_B800 | rn << 5 | rd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodings checked against clang's assembler (`clang -c -target arm64-apple-macos`).
    #[test]
    fn encodings_match_the_assembler() {
        let cases: &[(u32, u32, &str)] = &[
            (ldst_imm(LDR_W, 9, 19, 16).unwrap(), 0xb9401269, "ldr w9, [x19, #16]"),
            (ldst_imm(STR_X, 9, 19, 8).unwrap(), 0xf9000669, "str x9, [x19, #8]"),
            (ldst_imm(LDR_D, 0, 19, 24).unwrap(), 0xfd400e60, "ldr d0, [x19, #24]"),
            (ldst_reg(LDR_W, 11, 21, 9), 0xb8696aab, "ldr w11, [x21, x9]"),
            (ldst_reg(LDRSB_X, 11, 21, 9), 0x38a96aab, "ldrsb x11, [x21, x9]"),
            (ldst_reg(STRH, 11, 21, 9), 0x78296aab, "strh w11, [x21, x9]"),
            (ldst_reg(LDR_X, 9, 19, 17), 0xf8716a69, "ldr x9, [x19, x17]"),
            (ldst_uxtw(LDR_W, 11, 21, 9), 0xb8694aab, "ldr w11, [x21, w9, uxtw]"),
            (cmp_uxtw(22, 9), 0xeb2942df, "cmp x22, w9, uxtw"),
            (add_imm(true, 9, 22, 4095), 0x913ffec9, "add x9, x22, #4095"),
            (sub_imm(true, 22, 22, 4095), 0xd13ffed6, "sub x22, x22, #4095"),
            (ldst_uxtw(STRB, 10, 21, 8), 0x38284aaa, "strb w10, [x21, w8, uxtw]"),
            (ldst_uxtw(LDR_X, 9, 21, 9), 0xf8694aa9, "ldr x9, [x21, w9, uxtw]"),
            (ldst_uxtw(LDRSB_X, 3, 21, 4), 0x38a44aa3, "ldrsb x3, [x21, w4, uxtw]"),
            (ldst_uxtw(LDRSH_W, 3, 21, 4), 0x78e44aa3, "ldrsh w3, [x21, w4, uxtw]"),
            (ldst_uxtw(STR_X, 5, 21, 6), 0xf8264aa5, "str x5, [x21, w6, uxtw]"),
            (add_imm(false, 9, 9, 3), 0x11000d29, "add w9, w9, #3"),
            (subs_imm(true, 23, 23, 5), 0xf10016f7, "subs x23, x23, #5"),
            (subs_imm(false, ZR, 9, 7), 0x71001d3f, "cmp w9, #7"),
            (adds_imm(false, ZR, 9, 1), 0x3100053f, "cmn w9, #1"),
            (subs(false, ZR, 9, 10), 0x6b0a013f, "cmp w9, w10"),
            (add(true, 9, 19, 9), 0x8b090269, "add x9, x19, x9"),
            (mul(false, 9, 9, 10), 0x1b0a7d29, "mul w9, w9, w10"),
            (msub(true, 9, 11, 10, 9), 0x9b0aa569, "msub x9, x11, x10, x9"),
            (sdiv(false, 11, 9, 10), 0x1aca0d2b, "sdiv w11, w9, w10"),
            (udiv(true, 11, 9, 10), 0x9aca092b, "udiv x11, x9, x10"),
            (lslv(false, 9, 9, 10), 0x1aca2129, "lsl w9, w9, w10"),
            (asrv(true, 9, 9, 10), 0x9aca2929, "asr x9, x9, x10"),
            (rorv(false, 9, 9, 10), 0x1aca2d29, "ror w9, w9, w10"),
            (clz(true, 9, 9), 0xdac01129, "clz x9, x9"),
            (rbit(false, 9, 9), 0x5ac00129, "rbit w9, w9"),
            (bic(false, 9, 9, 11), 0x0a2b0129, "bic w9, w9, w11"),
            (mov(true, 20, 0), 0xaa0003f4, "mov x20, x0"),
            (mov_sp(29, SP), 0x910003fd, "mov x29, sp"),
            (mov_sp(SP, 9), 0x9100013f, "mov sp, x9"),
            (csel(true, 9, 10, 11, Cond::Ne), 0x9a8b1149, "csel x9, x10, x11, ne"),
            (cset(9, Cond::Lt), 0x1a9fa7e9, "cset w9, lt"),
            (cset(9, Cond::Mi), 0x1a9f57e9, "cset w9, mi"),
            (sxtb(false, 9, 9), 0x13001d29, "sxtb w9, w9"),
            (sxth(true, 9, 9), 0x93403d29, "sxth x9, w9"),
            (sxtw(9, 9), 0x93407d29, "sxtw x9, w9"),
            (uxtb(9, 10), 0x53001d49, "uxtb w9, w10"),
            (uxth(9, 10), 0x53003d49, "uxth w9, w10"),
            (lsr16(9, 22), 0xd350fec9, "lsr x9, x22, #16"),
            (movz(true, 9, 0x1234, 1), 0xd2a24689, "movz x9, #0x1234, lsl #16"),
            (movk(true, 9, 0x8000, 3), 0xf2f00009, "movk x9, #0x8000, lsl #48"),
            (movn(false, 9, 0, 0), 0x12800009, "movn w9, #0"),
            (stp_pre(29, 30, SP, -96), 0xa9ba7bfd, "stp x29, x30, [sp, #-96]!"),
            (ldp_post(29, 30, SP, 96), 0xa8c67bfd, "ldp x29, x30, [sp], #96"),
            (stp(19, 20, SP, 16), 0xa90153f3, "stp x19, x20, [sp, #16]"),
            (ldp(21, 22, 20, 0), 0xa9405a95, "ldp x21, x22, [x20]"),
            (str_pre(19, SP, -16), 0xf81f0ff3, "str x19, [sp, #-16]!"),
            (ldr_post(19, SP, 16), 0xf84107f3, "ldr x19, [sp], #16"),
            (str_post(ZR, 9, 8), 0xf800853f, "str xzr, [x9], #8"),
            (ldr_post(16, 9, 8), 0xf8408530, "ldr x16, [x9], #8"),
            (str_post(16, 10, 8), 0xf8008550, "str x16, [x10], #8"),
            (b_cond(Cond::Ne, -3), 0x54ffffa1, "b.ne #-12"),
            (b(2), 0x14000002, "b #8"),
            (bl(-4), 0x97fffffc, "bl #-16"),
            (b_cond(Cond::Hi, 3), 0x54000068, "b.hi #12"),
            (cbnz(false, 9, 2), 0x35000049, "cbnz w9, #8"),
            (cbz(true, 0, 2), 0xb4000040, "cbz x0, #8"),
            (br(10), 0xd61f0140, "br x10"),
            (blr(16), 0xd63f0200, "blr x16"),
            (RET, 0xd65f03c0, "ret"),
            (adr(10, 12), 0x1000006a, "adr x10, #12"),
            (add_uxtw2(10, 10, 9), 0x8b29494a, "add x10, x10, w9, uxtw #2"),
            (add_uxtw(9, 21, 25), 0x8b3942a9, "add x9, x21, w25, uxtw"),
            (fadd(false, 0, 0, 1), 0x1e212800, "fadd s0, s0, s1"),
            (fdiv(true, 0, 0, 1), 0x1e611800, "fdiv d0, d0, d1"),
            (fmin(true, 0, 0, 1), 0x1e615800, "fmin d0, d0, d1"),
            (fmax(false, 0, 0, 1), 0x1e214800, "fmax s0, s0, s1"),
            (fcmp(true, 0, 1), 0x1e612000, "fcmp d0, d1"),
            (fsqrt(false, 0, 0), 0x1e21c000, "fsqrt s0, s0"),
            (frintn(true, 0, 0), 0x1e644000, "frintn d0, d0"),
            (frintz(false, 0, 0), 0x1e25c000, "frintz s0, s0"),
            (fabs(true, 0, 0), 0x1e60c000, "fabs d0, d0"),
            (fneg(false, 0, 0), 0x1e214000, "fneg s0, s0"),
            (fcvt_widen(0, 0), 0x1e22c000, "fcvt d0, s0"),
            (fcvt_narrow(0, 0), 0x1e624000, "fcvt s0, d0"),
            (scvtf(true, false, 0, 9), 0x9e220120, "scvtf s0, x9"),
            (ucvtf(false, true, 0, 9), 0x1e630120, "ucvtf d0, w9"),
            (fcvtzs(false, true, 9, 0), 0x1e780009, "fcvtzs w9, d0"),
            (fcvtzu(true, false, 9, 0), 0x9e390009, "fcvtzu x9, s0"),
            (fmov_to_d(1, 9), 0x9e670121, "fmov d1, x9"),
            (fmov_to_d(9, ZR), 0x9e6703e9, "fmov d9, xzr"),
            (fmov_dd(1, 2), 0x1e604041, "fmov d1, d2"),
            (fmov_dd(8, 0), 0x1e604008, "fmov d8, d0"),
            (ldst_imm(STR_D, 8, SP, 8).unwrap(), 0xfd0007e8, "str d8, [sp, #8]"),
            (ldst_imm(LDR_D, 11, SP, 24).unwrap(), 0xfd400feb, "ldr d11, [sp, #24]"),
            (sub_imm(true, SP, SP, 32), 0xd10083ff, "sub sp, sp, #32"),
            (add_imm(true, SP, SP, 32), 0x910083ff, "add sp, sp, #32"),
            (fmov_w_from_s(9, 0), 0x1e260009, "fmov w9, s0"),
            (fmov_s_from_w(0, 9), 0x1e270120, "fmov s0, w9"),
            (fmov_x_from_d(9, 0), 0x9e660009, "fmov x9, d0"),
            (cnt8b(0, 0), 0x0e205800, "cnt v0.8b, v0.8b"),
            (addv8b(0, 0), 0x0e31b800, "addv b0, v0.8b"),
        ];
        for &(got, want, text) in cases {
            assert_eq!(got, want, "{text}: got {got:#010x}, want {want:#010x}");
        }
    }
}
