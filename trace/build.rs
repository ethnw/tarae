//! Compiles the BPF programs (`src/bpf/<name>.bpf.c`) with clang, for a Linux target only — elsewhere the crate
//! builds without them. Their kernel types (`vmlinux.h`) come from the running kernel's BTF through bpftool, or
//! from the file `TARAE_TRACE_VMLINUX_H` names; CO-RE relocations fit the object to other kernels at load.
//! Also the syscall names (`sysnames.rs`, see `syscall_names`).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROGRAMS: [&str; 3] = ["offcpu", "syscalls", "ptywrite"];

fn main() {
    println!("cargo::rerun-if-changed=src/bpf");
    println!("cargo::rerun-if-env-changed=TARAE_TRACE_VMLINUX_H");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let vmlinux = out.join("vmlinux.h");
    if let Some(given) = std::env::var_os("TARAE_TRACE_VMLINUX_H") {
        std::fs::copy(&given, &vmlinux).unwrap_or_else(|e| panic!("TARAE_TRACE_VMLINUX_H: {e}"));
    } else {
        let o = Command::new("bpftool")
            .args(["btf", "dump", "file", "/sys/kernel/btf/vmlinux", "format", "c"])
            .output()
            .unwrap_or_else(|e| panic!("bpftool: {e} — install it, or set TARAE_TRACE_VMLINUX_H"));
        assert!(o.status.success(), "bpftool: {}", String::from_utf8_lossy(&o.stderr));
        std::fs::write(&vmlinux, o.stdout).unwrap();
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "x86_64" => "x86",
        "aarch64" => "arm64",
        a => panic!("no BPF target arch for {a}"),
    };
    for p in PROGRAMS {
        let obj = out.join(format!("{p}.bpf.o"));
        let st = Command::new("clang")
            .args(["-g", "-O2", "-Wall", "-target", "bpf", &format!("-D__TARGET_ARCH_{arch}")])
            .arg("-I")
            .arg(&out)
            .args(["-c", &format!("src/bpf/{p}.bpf.c"), "-o"])
            .arg(&obj)
            .status()
            .unwrap_or_else(|e| panic!("clang: {e}"));
        assert!(st.success(), "clang failed on src/bpf/{p}.bpf.c");
    }
    syscall_names(&out);
}

/// `sysnames.rs`: the target's syscall numbers and names — the `__NR_*` macros of the kernel headers'
/// `<asm/unistd.h>` through the preprocessor (aarch64 and x86_64 number them differently, and the kernel's BTF
/// has no names). A macro naming another (`__NR_fcntl` → `__NR3264_fcntl`) is followed.
fn syscall_names(out: &Path) {
    let mut cpp = Command::new("clang")
        .args(["-E", "-dM", "-x", "c", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("clang: {e}"));
    cpp.stdin.take().unwrap().write_all(b"#include <asm/unistd.h>\n").unwrap();
    let o = cpp.wait_with_output().unwrap();
    assert!(o.status.success(), "the kernel headers' <asm/unistd.h> (linux-libc-dev)");
    let defines: HashMap<String, String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let mut w = l.strip_prefix("#define ")?.split_whitespace();
            Some((w.next()?.to_string(), w.next()?.to_string()))
        })
        .collect();
    let mut names: Vec<(u32, &str)> = defines
        .iter()
        .filter_map(|(k, v)| {
            let name =
                k.strip_prefix("__NR_").filter(|n| *n != "syscalls" && *n != "arch_specific_syscall")?;
            let mut v = v.as_str();
            for _ in 0..4 {
                if let Ok(n) = v.parse() {
                    return Some((n, name));
                }
                v = defines.get(v)?;
            }
            None
        })
        .collect();
    names.sort();
    names.dedup_by_key(|(n, _)| *n);
    assert!(names.len() > 200, "only {} syscall names from <asm/unistd.h>", names.len());
    let rows: String = names.iter().map(|(n, s)| format!("    ({n}, {s:?}),\n")).collect();
    let file = format!(
        "/// The syscalls' names by number, for this target (build.rs).\n\
         pub const NAMES: &[(u32, &str)] = &[\n{rows}];\n"
    );
    std::fs::write(out.join("sysnames.rs"), file).unwrap();
}
