//! The off-CPU probe (`bpf/offcpu.bpf.c`): attached to `sched_switch`, it sums the time the chosen processes'
//! threads spend off a CPU, by stack, until `finish`. Choosing and attaching are `attach.rs`'s.

use crate::attach::{self, Attached, COMM_LEN, Error, Probe, Target, bpf, comm, u32_at, u64_at};
use crate::report::{Report, Stack};

use libbpf_rs::{MapCore, MapFlags};

const OBJECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/offcpu.bpf.o"));
/// `errors`' slots in the BPF program, in its order.
const LOST: [&str; 2] = ["start table full", "offcpu table full"];

pub struct Tracer(Attached);

impl Probe for Tracer {
    const NAME: &'static str = "offcpu";
    type Report = Report;

    fn attach(target: &Target) -> Result<Tracer, Error> {
        Attached::attach(OBJECT, target).map(Tracer)
    }

    fn attached(&mut self) -> &mut Attached {
        &mut self.0
    }

    /// A wait still under way (a thread asleep now — the idle main loop of a tarae attached to) counts up to
    /// here, its stack known from its switch out.
    fn finish(mut self, name: &str) -> Result<Report, Error> {
        let (secs, now) = self.0.detach();
        let a = &self.0;
        let (lost, other_ns) = a.losses(&LOST);
        // (ustack, kstack, tgid, tid, preempted, comm, ns, count, max_ns)
        type Raw = (u64, u64, u32, u32, bool, String, u64, u64, u64);
        let mut raw: Vec<Raw> = Vec::new();
        // (`struct oc_key`: ustack, kstack, tgid, tid, preempted, comm; `struct oc_val`: ns, count, max_ns)
        let offcpu = a.map("offcpu")?;
        for k in offcpu.keys() {
            let Some(v) = offcpu.lookup(&k, MapFlags::ANY).map_err(bpf("reading offcpu"))? else { continue };
            raw.push((
                u64_at(&k, 0),
                u64_at(&k, 8),
                u32_at(&k, 16),
                u32_at(&k, 20),
                u32_at(&k, 24) != 0,
                comm(&k[28..28 + COMM_LEN]),
                u64_at(&v, 0),
                u64_at(&v, 8),
                u64_at(&v, 16),
            ));
        }
        // (`struct oc_start`: ts, ustack, kstack, tgid, tid, preempted, comm)
        let starts = a.map("starts")?;
        let mut under_way = 0;
        for k in starts.keys() {
            let Some(s) = starts.lookup(&k, MapFlags::ANY).ok().flatten() else { continue };
            let ns = now.saturating_sub(u64_at(&s, 0));
            under_way += 1;
            raw.push((
                u64_at(&s, 8),
                u64_at(&s, 16),
                u32_at(&s, 24),
                u32_at(&s, 28),
                u32_at(&s, 32) != 0,
                comm(&s[36..36 + COMM_LEN]),
                ns,
                1,
                ns,
            ));
        }
        let mut namer = a.namer();
        let mut out = Vec::new();
        for (ustack, kstack, tgid, tid, preempted, comm, ns, count, max_ns) in raw {
            let (user, own) = namer.user(tgid, ustack);
            let kernel = namer.kernel(kstack);
            out.push(Stack { pid: tgid, tid, comm, preempted, ns, count, max_ns, user, own, kernel });
        }
        Ok(Report {
            probe: Self::NAME.into(),
            name: name.into(),
            kernel: attach::kernel(),
            secs,
            stacks: out,
            lost,
            other_ns,
            under_way,
        }
        .merged())
    }
}
