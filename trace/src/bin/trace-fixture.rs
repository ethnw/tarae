//! What the probes' tests trace. Built with frame pointers like tarae is for tracing.
//!
//! - `trace-fixture MS [after-line]`: a thread named `fixture-wait` sleeps 3 × MS/3 in `wait_in_a_known_place`,
//!   joined by the main thread — after reading a line from stdin with `after-line`.
//! - `trace-fixture tree MS now|later`: a parent that starts one of the above as its child and waits for it —
//!   `now`: at once (the child then waits for the line: it's already running when a tracer attaches);
//!   `later`: once it has read a line itself (the child is forked after the attach).
//! - `trace-fixture syscalls N`: once it has read a line, N × getppid(2) from `call_in_a_known_place` — a
//!   count to find exactly, and where they come from.
//! - `trace-fixture pty STALL_MS BYTES` (Linux): once it has read a line, a thread named `fixture-write`
//!   writes BYTES to a raw pty in one `write_in_a_known_place`, while the main thread, its reader, waits
//!   STALL_MS before draining it — a terminal that reads slowly: the write waits for room about that long.
//! - `trace-fixture hidden MS` (Linux): once it has read a line, `fixture-wait` sleeps MS in
//!   `wait_above_a_hidden_frame`, through
//!   `call_without_a_frame_pointer` — a frame that has unwind info but uses its frame-pointer register for
//!   something else, as code built without frame pointers may: a frame-pointer walk ends under it.

use std::process::Command;
use std::time::Duration;

#[cfg(target_os = "linux")]
#[path = "../../../src/pty.rs"]
mod pty;

#[inline(never)]
fn wait_in_a_known_place(d: Duration) {
    std::thread::sleep(d);
    // (not a tail call: this frame stays on the stack while the sleep waits)
    std::hint::black_box(());
}

// `call_without_a_frame_pointer(f, arg)` calls `f(arg)` with the frame-pointer register zeroed — the chain of
// frame records ends at `f`'s — and describes its own frame in .eh_frame (the .cfi lines), as compilers do.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
std::arch::global_asm!(
    ".text",
    ".globl call_without_a_frame_pointer",
    ".type call_without_a_frame_pointer,@function",
    ".p2align 4",
    "call_without_a_frame_pointer:",
    ".cfi_startproc",
    "push rbp",
    ".cfi_adjust_cfa_offset 8",
    ".cfi_rel_offset rbp, 0",
    "xor ebp, ebp",
    "mov rax, rdi",
    "mov rdi, rsi",
    "call rax",
    "pop rbp",
    ".cfi_adjust_cfa_offset -8",
    ".cfi_restore rbp",
    "ret",
    ".cfi_endproc",
    ".size call_without_a_frame_pointer, .-call_without_a_frame_pointer",
);

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
std::arch::global_asm!(
    ".text",
    ".globl call_without_a_frame_pointer",
    ".type call_without_a_frame_pointer,%function",
    ".p2align 2",
    "call_without_a_frame_pointer:",
    ".cfi_startproc",
    "stp x29, x30, [sp, #-16]!",
    ".cfi_def_cfa_offset 16",
    ".cfi_offset x29, -16",
    ".cfi_offset x30, -8",
    "mov x29, xzr",
    "mov x2, x0",
    "mov x0, x1",
    "blr x2",
    "ldp x29, x30, [sp], #16",
    ".cfi_def_cfa_offset 0",
    ".cfi_restore x29",
    ".cfi_restore x30",
    "ret",
    ".cfi_endproc",
    ".size call_without_a_frame_pointer, .-call_without_a_frame_pointer",
);

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn call_without_a_frame_pointer(f: extern "C" fn(u64), arg: u64);
}

#[cfg(target_os = "linux")]
#[inline(never)]
extern "C" fn sleep_below(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
    std::hint::black_box(());
}

#[cfg(target_os = "linux")]
#[inline(never)]
fn wait_above_a_hidden_frame(ms: u64) {
    // SAFETY: the assembly above calls `sleep_below(ms)` and returns, its frame as its .cfi lines say.
    unsafe { call_without_a_frame_pointer(sleep_below, ms) };
    std::hint::black_box(());
}

/// `n` × getppid(2), all from here.
#[inline(never)]
fn call_in_a_known_place(n: u64) {
    for _ in 0..n {
        std::hint::black_box(std::os::unix::process::parent_id());
    }
    std::hint::black_box(());
}

fn read_line() {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}

/// Writes all of `buf` to `fd` — one write(2) unless the kernel takes less.
#[cfg(target_os = "linux")]
#[inline(never)]
fn write_in_a_known_place(fd: libc::c_int, buf: &[u8]) {
    let mut at = 0;
    while at < buf.len() {
        // SAFETY: writes from a live slice, at most its remaining length.
        let n = unsafe { libc::write(fd, buf[at..].as_ptr().cast(), buf.len() - at) };
        assert!(n > 0, "write: {}", std::io::Error::last_os_error());
        at += n as usize;
    }
    std::hint::black_box(());
}

#[cfg(target_os = "linux")]
fn pty_mode(stall: Duration, bytes: usize) {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let (master, slave) = pty::open(24, 80).expect("pty");
    // Raw: what's written arrives as it is (no \n → \r\n), byte for byte
    // SAFETY: termios filled by tcgetattr from the slave just opened, then set back on it.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut t), 0);
        libc::cfmakeraw(&mut t);
        assert_eq!(libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t), 0);
    }
    let writer = std::thread::Builder::new()
        .name("fixture-write".into())
        .spawn(move || write_in_a_known_place(slave.as_raw_fd(), &vec![b'x'; bytes]))
        .unwrap();
    std::thread::sleep(stall);
    let mut master = std::fs::File::from(master);
    let (mut got, mut buf) = (0, vec![0u8; 64 * 1024]);
    while got < bytes {
        match master.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    writer.join().unwrap();
    assert_eq!(got, bytes, "read back what was written");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).and_then(|a| a.parse::<u64>().ok());
    if args.first().map(String::as_str) == Some("syscalls") {
        read_line();
        call_in_a_known_place(arg(1).unwrap_or(1000));
        return;
    }
    if args.first().map(String::as_str) == Some("pty") {
        read_line();
        #[cfg(target_os = "linux")]
        pty_mode(Duration::from_millis(arg(1).unwrap_or(500)), arg(2).unwrap_or(4 << 20) as usize);
        return;
    }
    if args.first().map(String::as_str) == Some("hidden") {
        read_line();
        #[cfg(target_os = "linux")]
        {
            let ms = arg(1).unwrap_or(500);
            let wait = std::thread::Builder::new().name("fixture-wait".into());
            wait.spawn(move || wait_above_a_hidden_frame(ms)).unwrap().join().unwrap();
        }
        return;
    }
    if args.first().map(String::as_str) == Some("tree") {
        let later = args.get(2).map(String::as_str) == Some("later");
        if later {
            read_line();
        }
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.arg(&args[1]);
        if !later {
            child.arg("after-line");
        }
        assert!(child.status().unwrap().success());
        return;
    }
    let ms: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(900);
    if args.get(1).map(String::as_str) == Some("after-line") {
        read_line();
    }
    std::thread::Builder::new()
        .name("fixture-wait".into())
        .spawn(move || {
            for _ in 0..3 {
                wait_in_a_known_place(Duration::from_millis(ms / 3));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
