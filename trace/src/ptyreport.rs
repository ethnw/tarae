//! The terminal-write probe's results: per (thread, user stack) the writes to a terminal, their bytes and
//! time, the longest, and a log2 histogram — so a write that waited for room (a slow reader, the pty full)
//! shows as a long one, with where it was written from.

use std::collections::BTreeMap;

use serde_json::json;

use crate::report::where_user;

/// Histogram buckets: [b] counts writes of 2^b ≤ µs < 2^(b+1) (b = 0 also under 1 µs).
pub const BUCKETS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Writes {
    pub pid: u32,
    pub tid: u32,
    pub comm: String,
    pub count: u64,
    pub ns: u64,
    pub max_ns: u64,
    pub bytes: u64,
    pub hist: [u64; BUCKETS],
    pub user: Vec<String>,
    pub own: Vec<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct PtyReport {
    pub name: String,
    pub kernel: String,
    pub secs: f64,
    /// The most time first.
    pub writes: Vec<Writes>,
    pub lost: BTreeMap<String, u64>,
    pub other_ns: u64,
}

fn ms(ns: u64) -> f64 {
    (ns as f64 / 1e4).round() / 1e2
}

/// The upper bound in µs of the bucket that holds the `q` quantile (0–1) of a histogram — "≤ that".
pub fn quantile_us(hist: &[u64; BUCKETS], q: f64) -> u64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0;
    }
    let want = ((total as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (b, n) in hist.iter().enumerate() {
        seen += n;
        if seen >= want {
            return 1 << (b + 1);
        }
    }
    1 << BUCKETS
}

impl PtyReport {
    pub fn sorted(mut self) -> Self {
        self.writes.sort_by(|a, b| b.ns.cmp(&a.ns).then(a.tid.cmp(&b.tid)));
        self
    }

    /// Per thread, its stacks' writes added up (the most time first).
    pub fn threads(&self) -> Vec<Writes> {
        let mut by: BTreeMap<(u32, u32), Writes> = BTreeMap::new();
        for w in &self.writes {
            let t = by.entry((w.pid, w.tid)).or_insert_with(|| Writes {
                user: vec![],
                own: vec![],
                count: 0,
                ns: 0,
                max_ns: 0,
                bytes: 0,
                hist: [0; BUCKETS],
                ..w.clone()
            });
            t.count += w.count;
            t.ns += w.ns;
            t.bytes += w.bytes;
            t.max_ns = t.max_ns.max(w.max_ns);
            for (h, n) in t.hist.iter_mut().zip(w.hist) {
                *h += n;
            }
        }
        let mut v: Vec<Writes> = by.into_values().collect();
        v.sort_by_key(|w| std::cmp::Reverse(w.ns));
        v
    }
}

impl crate::Output for PtyReport {
    fn to_json(&self) -> serde_json::Value {
        let row = |w: &Writes| {
            json!({ "pid": w.pid, "tid": w.tid, "comm": w.comm, "writes": w.count, "bytes": w.bytes,
                "ms": ms(w.ns), "max_ms": ms(w.max_ns), "p50_us_at_most": quantile_us(&w.hist, 0.5),
                "p99_us_at_most": quantile_us(&w.hist, 0.99), "hist_log2_us": w.hist.to_vec() })
        };
        let threads: Vec<_> = self.threads().iter().map(row).collect();
        let stacks: Vec<_> = self
            .writes
            .iter()
            .map(|w| {
                let mut r = row(w);
                r["user"] = json!(w.user);
                r["own"] = json!(w.own);
                r
            })
            .collect();
        json!({ "probe": "pty", "name": self.name, "kernel": self.kernel,
            "secs": (self.secs * 100.0).round() / 100.0, "lost": self.lost, "other_namespaces": self.other_ns,
            "threads": threads, "stacks": stacks })
    }

    /// `comm;user… ns` — the time writing, by where it was written from.
    fn folded(&self) -> Option<String> {
        let mut by: BTreeMap<String, u64> = BTreeMap::new();
        for w in &self.writes {
            let mut line = vec![w.comm.replace(';', ":")];
            line.extend(w.user.iter().map(|f| f.replace(';', ":").replace(' ', "_")));
            *by.entry(line.join(";")).or_default() += w.ns;
        }
        Some(by.into_iter().map(|(l, ns)| format!("{l} {ns}\n")).collect())
    }

    /// Per thread: writes, bytes, time writing (and its share of the run), the longest, p50/p99; the
    /// writing threads' histograms; the slowest stacks.
    fn summary(&self, top: usize) -> String {
        let mut s = format!("pty {} — {:.1} s\n", self.name, self.secs);
        for (cause, n) in self.lost.iter().filter(|(_, n)| **n > 0) {
            s += &format!("  LOST {n} ({cause}) — the numbers below are short\n");
        }
        if self.other_ns > 0 {
            s += &format!("  ({} writes of {} in other containers left out)\n", self.other_ns, self.name);
        }
        let threads = self.threads();
        if threads.is_empty() {
            s += "  no writes to a terminal\n";
            return s;
        }
        s += "  threads — writes, KB, time writing (share of the run), longest, p50 ≤, p99 ≤\n";
        for t in &threads {
            s += &format!(
                "    {:>6} {:<14} {:>6}× {:>9.1} KB {:>9.1} ms ({:>4.1}%)  max {:>8.2} ms  p50 ≤ {} µs  p99 ≤ {} µs\n",
                t.tid,
                t.comm,
                t.count,
                t.bytes as f64 / 1024.0,
                ms(t.ns),
                t.ns as f64 / 1e9 / self.secs.max(1e-9) * 100.0,
                ms(t.max_ns),
                quantile_us(&t.hist, 0.5),
                quantile_us(&t.hist, 0.99),
            );
            let bars: Vec<String> = t
                .hist
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                .map(|(b, n)| format!("<{}µs:{n}", 1u64 << (b + 1)))
                .collect();
            s += &format!("           {}\n", bars.join(" "));
        }
        s += &format!("  top {top} stacks by time writing\n");
        for w in self.writes.iter().take(top) {
            s += &format!(
                "    {:>9.1} ms {:>5}× max {:>8.2} ms  {}  {}\n",
                ms(w.ns),
                w.count,
                ms(w.max_ns),
                w.comm,
                where_user(&w.user, &w.own)
            );
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;

    fn w(tid: u32, count: u64, ns: u64, max_ns: u64, buckets: &[(usize, u64)]) -> Writes {
        let mut hist = [0; BUCKETS];
        for &(b, n) in buckets {
            hist[b] = n;
        }
        Writes {
            pid: 1,
            tid,
            comm: "tarae".into(),
            count,
            ns,
            max_ns,
            bytes: count * 1000,
            hist,
            user: vec![
                "main".into(),
                "event_loop".into(),
                "render".into(),
                "<std::io::stdio::Stdout>::flush".into(),
            ],
            own: vec![true, true, true, false],
        }
    }

    /// Quantiles come from the histogram's buckets, as "at most"; threads add their stacks up.
    #[test]
    fn quantiles_threads_and_where() {
        let mut h = [0; BUCKETS];
        h[3] = 98; // 8–16 µs
        h[16] = 2; // 65–131 ms
        assert_eq!((quantile_us(&h, 0.5), quantile_us(&h, 0.99)), (16, 131_072));
        assert_eq!(quantile_us(&[0; BUCKETS], 0.5), 0);
        let r = PtyReport {
            name: "slow".into(),
            secs: 1.0,
            writes: vec![
                w(7, 98, 1_000_000, 15_000, &[(3, 98)]),
                w(7, 2, 200_000_000, 120_000_000, &[(16, 2)]),
            ],
            ..Default::default()
        }
        .sorted();
        let t = r.threads();
        assert_eq!((t.len(), t[0].count, t[0].ns, t[0].max_ns), (1, 100, 201_000_000, 120_000_000));
        assert_eq!(t[0].hist[3] + t[0].hist[16], 100);
        let s = r.summary(5);
        assert!(s.contains("201.0 ms (20.1%)") && s.contains("p99 ≤ 131072 µs"), "{s}");
        assert!(s.contains("main → event_loop → render → <std::io::stdio::Stdout>::flush"), "{s}");
        assert!(
            r.folded()
                .unwrap()
                // (both writes came from one stack: one line, their time added)
                .contains("tarae;main;event_loop;render;<std::io::stdio::Stdout>::flush 201000000\n")
        );
        assert_eq!(r.to_json()["threads"][0]["writes"], 100);
    }
}
