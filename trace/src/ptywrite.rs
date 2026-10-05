//! The terminal-write probe (`bpf/ptywrite.bpf.c`): the chosen processes' writes to a terminal, entry to
//! return — a write that waits for the reader (the pty full) counts its wait.

use crate::attach::{self, Attached, COMM_LEN, Error, Probe, Target, bpf, comm, u32_at, u64_at};
use crate::ptyreport::{BUCKETS, PtyReport, Writes};

use libbpf_rs::{MapCore, MapFlags};

const OBJECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ptywrite.bpf.o"));
/// `errors`' slots in the BPF program, in its order.
const LOST: [&str; 2] = ["start table full", "writes table full"];

pub struct Tracer(Attached);

impl Probe for Tracer {
    const NAME: &'static str = "pty";
    type Report = PtyReport;

    fn attach(target: &Target) -> Result<Tracer, Error> {
        Attached::attach(OBJECT, target).map(Tracer)
    }

    fn attached(&mut self) -> &mut Attached {
        &mut self.0
    }

    fn finish(mut self, name: &str) -> Result<PtyReport, Error> {
        let (secs, _) = self.0.detach();
        let a = &self.0;
        let (lost, other_ns) = a.losses(&LOST);
        let m = a.map("writes")?;
        let mut namer = a.namer();
        let mut writes = Vec::new();
        // (`struct pw_key`: ustack, tgid, tid, comm; `struct pw_val`: count, ns, max_ns, bytes, hist)
        for k in m.keys() {
            let Some(v) = m.lookup(&k, MapFlags::ANY).map_err(bpf("reading writes"))? else { continue };
            let pid = u32_at(&k, 8);
            let (user, own) = namer.user(pid, u64_at(&k, 0));
            let mut hist = [0; BUCKETS];
            for (b, h) in hist.iter_mut().enumerate() {
                *h = u64_at(&v, 32 + b * 8);
            }
            writes.push(Writes {
                pid,
                tid: u32_at(&k, 12),
                comm: comm(&k[16..16 + COMM_LEN]),
                count: u64_at(&v, 0),
                ns: u64_at(&v, 8),
                max_ns: u64_at(&v, 16),
                bytes: u64_at(&v, 24),
                hist,
                user,
                own,
            });
        }
        Ok(PtyReport { name: name.into(), kernel: attach::kernel(), secs, writes, lost, other_ns }.sorted())
    }
}
