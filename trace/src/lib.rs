//! tarae-trace — eBPF probes that measure tarae on Linux, for its developers (`tarae-trace <probe> --pid …`
//! on a running tarae; `cargo xtask trace` builds and runs it in a container, see `xtask/src/trace.rs`).
//! Probes: `offcpu` (where threads wait), `syscalls` (the syscall mix), `pty` (terminal write latency). The
//! probes and the symbolizing are Linux-only; the reports, the maps arithmetic and the unwinding's walk build
//! and test anywhere.

pub mod maps;
pub mod procs;
pub mod ptyreport;
pub mod report;
pub mod sysreport;
pub mod unwind;

#[cfg(target_os = "linux")]
pub mod attach;
#[cfg(target_os = "linux")]
pub mod offcpu;
#[cfg(target_os = "linux")]
pub mod ptywrite;
#[cfg(target_os = "linux")]
mod symbols;
#[cfg(target_os = "linux")]
pub mod syscalls;

/// A probe's results, as files and words.
pub trait Output {
    /// What `cargo xtask trace` keeps and compares.
    fn to_json(&self) -> serde_json::Value;
    /// Folded stacks (flamegraph.pl / inferno input), for a probe that keeps stacks.
    fn folded(&self) -> Option<String>;
    /// A few lines for a person, the top `top` of each list.
    fn summary(&self, top: usize) -> String;
}

impl Output for report::Report {
    fn to_json(&self) -> serde_json::Value {
        report::Report::to_json(self)
    }

    fn folded(&self) -> Option<String> {
        Some(report::Report::folded(self))
    }

    fn summary(&self, top: usize) -> String {
        report::Report::summary(self, top)
    }
}
