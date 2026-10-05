//! The syscall probe (`bpf/syscalls.bpf.c`): the chosen processes' syscalls, per thread and syscall.

use crate::attach::{self, Attached, COMM_LEN, Error, Probe, Target, bpf, comm, u32_at, u64_at};
use crate::sysreport::{Call, SyscallReport};

use libbpf_rs::{MapCore, MapFlags};

mod names {
    include!(concat!(env!("OUT_DIR"), "/sysnames.rs"));
}

const OBJECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/syscalls.bpf.o"));
/// `errors`' slots in the BPF program, in its order.
const LOST: [&str; 2] = ["start table full", "calls table full"];

/// A syscall's name on this target, or `#nr`.
pub fn name(nr: u32) -> String {
    match names::NAMES.binary_search_by_key(&nr, |(n, _)| *n) {
        Ok(i) => names::NAMES[i].1.to_string(),
        Err(_) => format!("#{nr}"),
    }
}

/// A syscall's number on this target.
pub fn number(name: &str) -> Option<u32> {
    names::NAMES.iter().find(|(_, n)| *n == name).map(|(nr, _)| *nr)
}

pub struct Tracer(Attached);

impl Probe for Tracer {
    const NAME: &'static str = "syscalls";
    type Report = SyscallReport;

    fn attach(target: &Target) -> Result<Tracer, Error> {
        Attached::attach(OBJECT, target).map(Tracer)
    }

    fn attached(&mut self) -> &mut Attached {
        &mut self.0
    }

    /// Marks the syscalls (by name, this target's numbers) whose callers' user stacks are kept.
    fn keep_stacks_for(&mut self, syscalls: &[String]) -> Result<(), Error> {
        let m = self.0.map("stack_for")?;
        for s in syscalls {
            let nr = number(s).ok_or_else(|| Error::Other(format!("--stacks: no syscall {s:?} here")))?;
            m.update(&nr.to_ne_bytes(), &[1], MapFlags::ANY).map_err(bpf("stack_for"))?;
        }
        Ok(())
    }

    fn finish(mut self, name: &str) -> Result<SyscallReport, Error> {
        let (secs, _) = self.0.detach();
        let a = &self.0;
        let (lost, other_ns) = a.losses(&LOST);
        // (`struct sc_key`: ustack, tgid, tid, nr, comm; `struct sc_val`: count, ns, max_ns, failed)
        let m = a.map("calls")?;
        let mut namer = a.namer();
        let mut calls = Vec::new();
        for k in m.keys() {
            let Some(v) = m.lookup(&k, MapFlags::ANY).map_err(bpf("reading calls"))? else { continue };
            let (ustack, pid, nr) = (u64_at(&k, 0), u32_at(&k, 8), u32_at(&k, 16));
            // (0: its stack wasn't asked for)
            let (user, own) = if ustack == 0 { (vec![], vec![]) } else { namer.user(pid, ustack) };
            calls.push(Call {
                pid,
                tid: u32_at(&k, 12),
                comm: comm(&k[20..20 + COMM_LEN]),
                nr,
                name: self::name(nr),
                count: u64_at(&v, 0),
                ns: u64_at(&v, 8),
                max_ns: u64_at(&v, 16),
                failed: u64_at(&v, 24),
                user,
                own,
            });
        }
        Ok(SyscallReport { name: name.into(), kernel: attach::kernel(), secs, calls, lost, other_ns }
            .sorted())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table from the kernel headers has this target's numbers.
    #[test]
    fn names_by_number_and_back() {
        let n = number("getppid").expect("getppid");
        assert_eq!(name(n), "getppid");
        #[cfg(target_arch = "aarch64")]
        assert_eq!((number("read"), number("write"), number("getppid")), (Some(63), Some(64), Some(173)));
        #[cfg(target_arch = "x86_64")]
        assert_eq!((number("read"), number("write"), number("getppid")), (Some(0), Some(1), Some(110)));
        assert_eq!(name(99_999), "#99999");
    }
}
