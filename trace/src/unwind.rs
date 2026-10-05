//! Unwinding a user stack the BPF side copied (`stacks.bpf.h`'s `stack_copies`: the registers at the event and
//! the stack's bytes above its pointer) by the binaries' .eh_frame — the call frame information compilers emit
//! for exceptions, there whether or not the code keeps frame pointers. A frame-pointer walk loses every frame
//! of code that doesn't keep them (x86_64's std and Debian's glibc: the caller of a cancellable syscall, a
//! sleep's), and everything above a frame whose frame-pointer register holds something else.
//!
//! Each frame: its rule at its address (the CFA — the caller's stack pointer — as a register plus an offset,
//! where the return address and the frame pointer were saved), read against the copy. Where the copy ends (a
//! deep stack) or a binary has no rule, the frame-pointer walk's frames beyond the last one found go on
//! (`merged`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::maps::{self, Region};

/// Frames unwound at most.
const MAX_FRAMES: usize = 128;

/// A stack as the BPF side sent it.
#[derive(Clone, Debug, Default)]
pub struct Copy {
    pub pc: u64,
    pub sp: u64,
    pub fp: u64,
    /// arm64's link register (0 on x86_64).
    pub lr: u64,
    /// From `sp` up.
    pub stack: Vec<u8>,
}

impl Copy {
    /// A record of `stack_copies` (`struct st_copy`: id, pc, sp, fp, lr, len, then the stack's bytes) — its
    /// stack id and the copy.
    pub fn parse(r: &[u8]) -> Option<(u64, Copy)> {
        let w = |i: usize| u64_at(r, i * 8);
        let len = usize::try_from(w(5)?).ok()?;
        let stack = r.get(48..48usize.checked_add(len)?)?.to_vec();
        Some((w(0)?, Copy { pc: w(1)?, sp: w(2)?, fp: w(3)?, lr: w(4)?, stack }))
    }

    /// The 8 bytes at `at`, when the copy has them.
    fn read(&self, at: u64) -> Option<u64> {
        let off = usize::try_from(at.checked_sub(self.sp)?).ok()?;
        let b = self.stack.get(off..off.checked_add(8)?)?;
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }
}

/// An architecture's DWARF register numbers, and whether calls return through a link register.
#[derive(Clone, Copy, Debug)]
pub struct Arch {
    sp: u16,
    fp: u16,
    ra: u16,
    link: bool,
    /// What's an address in a saved return address (arm64's pointer authentication signs the top bits).
    mask: u64,
}

pub const X86_64: Arch = Arch { sp: 7, fp: 6, ra: 16, link: false, mask: u64::MAX };
pub const AARCH64: Arch = Arch { sp: 31, fp: 29, ra: 30, link: true, mask: (1 << 48) - 1 };
#[cfg(target_arch = "x86_64")]
pub const HOST: Arch = X86_64;
#[cfg(not(target_arch = "x86_64"))]
pub const HOST: Arch = AARCH64;

/// The register a CFA is counted from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reg {
    Sp,
    Fp,
}

/// Where a register's value in the caller is, by a frame's rule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Loc {
    /// Unchanged (callee-saved and not touched; arm64's return address still in the link register).
    Same,
    /// Saved at CFA + n.
    At(i64),
    /// Is CFA + n.
    Val(i64),
    /// None to be had: the outermost frame's return address — or a rule this unwinder doesn't follow.
    Gone,
}

/// One frame's rule at an address.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rule {
    pub cfa: (Reg, i64),
    pub ra: Loc,
    pub fp: Loc,
}

/// The frames of `c`, innermost first: its pc, then each return address — while `rule` has a rule for the
/// address (a return address is looked up by the byte before it, the call) and the copy has what it reads.
pub fn walk(arch: &Arch, c: &Copy, mut rule: impl FnMut(u64) -> Option<Rule>) -> Vec<u64> {
    let (mut pc, mut sp, mut fp, mut lr) = (c.pc, c.sp, c.fp, c.lr);
    let mut out = vec![pc];
    while out.len() < MAX_FRAMES {
        let at = if out.len() == 1 { pc } else { pc.saturating_sub(1) };
        let Some(r) = rule(at) else { break };
        let base = match r.cfa.0 {
            Reg::Sp => sp,
            Reg::Fp => fp,
        };
        let Some(cfa) = base.checked_add_signed(r.cfa.1) else { break };
        let value = |loc: Loc, now: u64| match loc {
            Loc::Same => Some(now),
            Loc::At(n) => c.read(cfa.checked_add_signed(n)?),
            Loc::Val(n) => cfa.checked_add_signed(n),
            Loc::Gone => None,
        };
        // (x86_64's return address is always saved: "the same" would be no address)
        let ra = value(r.ra, if arch.link { lr } else { 0 }).map(|a| a & arch.mask);
        let Some(ra) = ra.filter(|&a| a != 0) else { break };
        // A caller's frame is above its callee's; a leaf's (arm64, nothing pushed) may share it, once
        if cfa < sp || cfa == sp && ra == pc {
            break;
        }
        fp = value(r.fp, fp).unwrap_or(0);
        (lr, sp, pc) = (ra, cfa, ra);
        out.push(ra);
    }
    out
}

/// The unwound frames, then the frame-pointer walk's beyond the last of them (where the copy ran out or a
/// binary had no rule) — or the walk's alone when unwinding found no caller.
pub fn merged(unwound: Vec<u64>, walked: &[u64]) -> Vec<u64> {
    let Some(&last) = unwound.last().filter(|_| unwound.len() >= 2) else { return walked.to_vec() };
    let mut out = unwound;
    if let Some(i) = walked.iter().position(|&a| a == last) {
        out.extend(&walked[i + 1..]);
    }
    out
}

/// What of an ELF file unwinding reads: its loaded segments (file offset → address) and its .eh_frame with
/// the index .eh_frame_hdr keeps of it.
struct EhFile {
    /// (file offset, address, size in the file) of each `PT_LOAD`.
    loads: Vec<(u64, u64, u64)>,
    eh_frame: (u64, Vec<u8>),
    hdr: (u64, Vec<u8>),
    text: u64,
}

fn u16_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?).into())
}

fn u32_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?).into())
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

impl EhFile {
    /// A 64-bit little-endian ELF file's segments and unwind sections — None when it has no .eh_frame_hdr.
    /// Only those parts are read (a binary with debug info is big).
    fn read(path: &Path) -> Option<EhFile> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(path).ok()?;
        let at = |off: u64, len: u64| -> Option<Vec<u8>> {
            let mut b = vec![0; usize::try_from(len).ok().filter(|&n| n <= 1 << 30)?];
            f.read_exact_at(&mut b, off).ok()?;
            Some(b)
        };
        let h = at(0, 64)?;
        if h[..4] != *b"\x7fELF" || h[4] != 2 || h[5] != 1 {
            return None;
        }
        let (phoff, phsize, phnum) = (u64_at(&h, 0x20)?, u16_at(&h, 0x36)?, u16_at(&h, 0x38)?);
        let (shoff, shsize, shnum, shstrndx) =
            (u64_at(&h, 0x28)?, u16_at(&h, 0x3a)?, u16_at(&h, 0x3c)?, u16_at(&h, 0x3e)?);
        let ph = at(phoff, phsize * phnum)?;
        let loads = (0..phnum as usize)
            .map(|i| &ph[i * phsize as usize..])
            .filter(|p| u32_at(p, 0) == Some(1))
            .filter_map(|p| Some((u64_at(p, 8)?, u64_at(p, 16)?, u64_at(p, 32)?)))
            .collect();
        let sh = at(shoff, shsize * shnum)?;
        let section = |i: usize| {
            let s = sh.get(i * shsize as usize..)?;
            Some((u32_at(s, 0)?, u64_at(s, 16)?, u64_at(s, 24)?, u64_at(s, 32)?))
        };
        let (_, _, stroff, strsize) = section(shstrndx as usize)?;
        let names = at(stroff, strsize)?;
        let named = |want: &str| {
            (0..shnum as usize).filter_map(section).find(|&(name, ..)| {
                let n = names.get(name as usize..).unwrap_or_default();
                n.split(|&b| b == 0).next() == Some(want.as_bytes())
            })
        };
        let (_, eh_addr, eh_off, eh_size) = named(".eh_frame")?;
        let (_, hdr_addr, hdr_off, hdr_size) = named(".eh_frame_hdr")?;
        let text = named(".text").map_or(0, |(_, a, ..)| a);
        Some(EhFile {
            loads,
            eh_frame: (eh_addr, at(eh_off, eh_size)?),
            hdr: (hdr_addr, at(hdr_off, hdr_size)?),
            text,
        })
    }

    /// The address a file offset is loaded at, as the file numbers it.
    fn address(&self, off: u64) -> Option<u64> {
        let &(o, a, _) = self.loads.iter().find(|&&(o, _, n)| (o..o + n).contains(&off))?;
        Some(off - o + a)
    }

    /// The rule at `address` (the file's numbering), from the FDE .eh_frame_hdr's table finds for it.
    fn rule(&self, arch: &Arch, ctx: &mut gimli::UnwindContext<usize>, address: u64) -> Option<Rule> {
        use gimli::{BaseAddresses, CfaRule, EhFrame, EhFrameHdr, LittleEndian, RegisterRule, UnwindSection};
        let mut eh = EhFrame::new(&self.eh_frame.1, LittleEndian);
        if arch.link {
            // (arm64's signed return addresses: `DW_CFA_AARCH64_negate_ra_state`, read only for this vendor —
            // without, every rule of a function that signs its return address is an error)
            eh.set_vendor(gimli::Vendor::AArch64);
        }
        let bases = BaseAddresses::default()
            .set_eh_frame_hdr(self.hdr.0)
            .set_eh_frame(self.eh_frame.0)
            .set_text(self.text);
        let hdr = EhFrameHdr::new(&self.hdr.1, LittleEndian).parse(&bases, 8).ok()?;
        let row =
            hdr.table()?.unwind_info_for_address(&eh, &bases, ctx, address, EhFrame::cie_from_offset).ok()?;
        let cfa = match row.cfa() {
            CfaRule::RegisterAndOffset { register, offset } if register.0 == arch.sp => (Reg::Sp, *offset),
            CfaRule::RegisterAndOffset { register, offset } if register.0 == arch.fp => (Reg::Fp, *offset),
            _ => return None,
        };
        let loc = |r: u16| match row.register(gimli::Register(r)) {
            None | Some(RegisterRule::SameValue) => Loc::Same,
            Some(RegisterRule::Offset(n)) => Loc::At(n),
            Some(RegisterRule::ValOffset(n)) => Loc::Val(n),
            Some(_) => Loc::Gone,
        };
        Some(Rule { cfa, ra: loc(arch.ra), fp: loc(arch.fp) })
    }
}

/// Unwinds the copies of a run, every binary's unwind info read once and every address's rule found once.
#[derive(Default)]
pub struct Unwinder {
    files: HashMap<PathBuf, Option<EhFile>>,
    rules: HashMap<(PathBuf, u64), Option<Rule>>,
    ctx: Option<gimli::UnwindContext<usize>>,
}

impl Unwinder {
    /// The frames of `c` (innermost first), its addresses located in a process's kept `regions`.
    pub fn frames(&mut self, regions: &[Region], c: &Copy) -> Vec<u64> {
        walk(&HOST, c, |pc| {
            let (path, off) = maps::locate(regions, pc)?;
            self.rule(path, off)
        })
    }

    fn rule(&mut self, path: &Path, off: u64) -> Option<Rule> {
        let key = (path.to_path_buf(), off);
        if let Some(r) = self.rules.get(&key) {
            return *r;
        }
        let file = self.files.entry(key.0.clone()).or_insert_with(|| EhFile::read(path));
        let ctx = self.ctx.get_or_insert_with(gimli::UnwindContext::new);
        let r = file.as_ref().and_then(|f| f.rule(&HOST, ctx, f.address(off)?));
        self.rules.insert(key, r);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy whose stack holds `words` from `sp` up.
    fn copy(pc: u64, sp: u64, fp: u64, lr: u64, words: &[u64]) -> Copy {
        Copy { pc, sp, fp, lr, stack: words.iter().flat_map(|w| w.to_le_bytes()).collect() }
    }

    /// x86_64: each frame's return address read at its CFA − 8 — through a frame that counts its CFA from
    /// rsp (no frame pointer: rbp untouched, here holding nothing) and one that saved rbp. Ends where the
    /// rules end, or the copy does.
    #[test]
    fn x86_64_frames_by_their_rules() {
        // leaf at 0x1000: CFA = rsp+16, ra at CFA−8, rbp untouched
        // mid, returned to at 0x2005: CFA = rsp+32, ra at CFA−8, the caller's rbp saved at CFA−16
        // top, returned to at 0x3005: no rule
        let sp = 0x7000;
        let c = copy(0x1000, sp, 0, 0, &[0xdead, 0x2005, 0, 0, sp + 0x30, 0x3005, 0, 0]);
        let rule = |pc: u64| match pc {
            0x1000 => Some(Rule { cfa: (Reg::Sp, 16), ra: Loc::At(-8), fp: Loc::Same }),
            0x2004 => Some(Rule { cfa: (Reg::Sp, 32), ra: Loc::At(-8), fp: Loc::At(-16) }),
            _ => None,
        };
        assert_eq!(walk(&X86_64, &c, rule), [0x1000, 0x2005, 0x3005]);
        // (the copy too short for the second return address: ends at the first)
        let mut short = c.clone();
        short.stack.truncate(5 * 8);
        assert_eq!(walk(&X86_64, &short, rule), [0x1000, 0x2005]);
        // (no rule at the pc: the pc alone)
        assert_eq!(walk(&X86_64, &c, |_| None), [0x1000]);
    }

    /// arm64: a leaf's return address is still in the link register (a rule of "the same"); a frame that
    /// saved it reads it back — signed bits off — and a frame counted from the frame pointer follows it.
    #[test]
    fn aarch64_leaf_link_register_and_signed_returns() {
        let sp = 0xffff_0000;
        let signed = 0x002a_0000_0000_4008; // (a pointer-authentication code in the top bits)
        let c = copy(0x1000, sp, sp, 0x2008, &[sp + 0x20, signed, 0, 0, 0, 0x6008]);
        let rule = |pc: u64| match pc {
            0x1000 => Some(Rule { cfa: (Reg::Sp, 0), ra: Loc::Same, fp: Loc::Same }),
            0x2000..0x5000 => Some(Rule { cfa: (Reg::Fp, 16), ra: Loc::At(-8), fp: Loc::At(-16) }),
            _ => None,
        };
        assert_eq!(walk(&AARCH64, &c, rule), [0x1000, 0x2008, 0x4008, 0x6008]);
        // A rule that would loop (the same CFA, the same address) ends it
        let looping = |_| Some(Rule { cfa: (Reg::Sp, 0), ra: Loc::Same, fp: Loc::Same });
        assert_eq!(walk(&AARCH64, &copy(0x1000, sp, 0, 0x1000, &[]), looping), [0x1000]);
        // The outermost frame: no return address
        let last = |_| Some(Rule { cfa: (Reg::Sp, 16), ra: Loc::Gone, fp: Loc::Same });
        assert_eq!(walk(&AARCH64, &c, last), [0x1000]);
    }

    /// A ring record as `struct st_copy` lays it out: the bytes it says it holds, no more; a short one is none.
    #[test]
    fn a_copy_as_the_bpf_side_sends_it() {
        let mut r: Vec<u8> =
            [77u64, 0x1000, 0x7000, 0x7010, 0x2000, 16].iter().flat_map(|w| w.to_le_bytes()).collect();
        r.extend(1..=16u8);
        r.extend([0xee; 100]); // (the rest of the record: not read)
        let (id, c) = Copy::parse(&r).unwrap();
        assert_eq!((id, c.pc, c.sp, c.fp, c.lr), (77, 0x1000, 0x7000, 0x7010, 0x2000));
        assert_eq!(c.stack, (1..=16u8).collect::<Vec<_>>());
        assert_eq!(c.read(0x7008), Some(u64::from_le_bytes([9, 10, 11, 12, 13, 14, 15, 16])));
        assert_eq!((c.read(0x7009), c.read(0x6ff8)), (None, None), "outside the copy");
        assert!(Copy::parse(&r[..40]).is_none() && Copy::parse(&r[..60]).is_none());
    }

    /// The frame-pointer walk goes on past the last frame unwound; when unwinding found no caller, the walk's
    /// stack stands; a last frame the walk doesn't have adds nothing.
    #[test]
    fn the_walk_goes_on_where_unwinding_stops() {
        let walked = [0x10, 0x30, 0x40, 0x50];
        assert_eq!(merged(vec![0x10, 0x20, 0x30], &walked), [0x10, 0x20, 0x30, 0x40, 0x50]);
        assert_eq!(merged(vec![0x10], &walked), walked);
        assert_eq!(merged(vec![0x10, 0x20, 0x99], &walked), [0x10, 0x20, 0x99]);
        assert_eq!(merged(vec![0x10, 0x20, 0x30, 0x40, 0x50], &walked).len(), 5);
    }

    /// This test binary's own unwind info: a rule for this function's code, counted from the stack or frame
    /// pointer, with the return address saved.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_binarys_own_eh_frame_has_its_functions_rules() {
        let regions = maps::parse(&std::fs::read_to_string("/proc/self/maps").unwrap());
        let here = a_binarys_own_eh_frame_has_its_functions_rules as *const () as u64;
        let (path, off) = maps::locate(&regions, here + 4).expect("this code in a mapped file");
        let mut u = Unwinder::default();
        let r = u.rule(path, off).expect("a rule");
        assert!(matches!(r.ra, Loc::At(_) | Loc::Same), "{r:?}");
        assert!(u.rule(Path::new("/proc/self/status"), 0).is_none(), "not an ELF file: none");
        // glibc's own: a function that calls others (built for arm64 with signed return addresses, its rules
        // say so from its first instruction — `DW_CFA_AARCH64_negate_ra_state`)
        // SAFETY: looks a symbol up by a NUL-terminated name.
        let qsort = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"qsort_r".as_ptr()) } as u64;
        let (path, off) = maps::locate(&regions, qsort + 16).expect("qsort_r in libc");
        assert!(u.rule(path, off).is_some(), "qsort_r in {}", path.display());
    }
}
