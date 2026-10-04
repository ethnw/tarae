//! Starting a run's process (a task, tests) in a session of its own without forking the editor.
//!
//! `dap::setsid` is a `pre_exec`, and any `pre_exec` makes std fork the whole editor and exec in the child:
//! the fork copies the page tables, so it costs with the editor's memory — measured 2.9 ms at 230 MB and
//! 7.2 ms at 800 MB, on the main loop, every run. std's posix_spawn has no stable setsid (`process_setsid`
//! is unstable in 1.99) and asks it only on glibc. Here: posix_spawn with `POSIX_SPAWN_SETSID` (macOS
//! 10.15+, glibc 2.26+, musl 1.1.24+), the same whatever the memory. The process is what `dap::setsid`
//! made — its own session and process group (pgid = pid, no controlling terminal), so `dap::term_group`
//! stops it and everything it started.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, PipeReader};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// A process started by `setsid`: wait for it with `wait`.
pub struct Spawned {
    pub pid: u32,
    pub stdout: Option<PipeReader>,
    pub stderr: Option<PipeReader>,
}

/// `cmd`'s program, arguments, environment changes and folder, started in a session of its own: stdin
/// `/dev/null`, stdout and stderr pipes (what `cmd` says of its stdio isn't read — `Command` doesn't tell;
/// nor `env_clear`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn setsid(cmd: &Command) -> io::Result<Spawned> {
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;

    let env = environment(cmd);
    let cwd = cmd.get_current_dir().map(std::path::absolute).transpose()?;
    let path_var = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_os_str());
    let program = resolve(cmd.get_program(), cwd.as_deref(), path_var)?;
    let c = |s: &OsStr| CString::new(s.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
    let program_c = c(program.as_os_str())?;
    let argv: Vec<CString> =
        std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(c).collect::<io::Result<_>>()?;
    let envp: Vec<CString> = env
        .iter()
        .map(|(k, v)| {
            let mut kv = k.clone();
            kv.push("=");
            kv.push(v);
            c(&kv)
        })
        .collect::<io::Result<_>>()?;
    let cwd_c = cwd.as_deref().map(|p| c(p.as_os_str())).transpose()?;
    let null_c = c(OsStr::new("/dev/null"))?;
    let ptrs = |v: &[CString]| -> Vec<*mut libc::c_char> {
        v.iter().map(|s| s.as_ptr().cast_mut()).chain(std::iter::once(std::ptr::null_mut())).collect()
    };
    let (argv_p, envp_p) = (ptrs(&argv), ptrs(&envp));
    // (std's pipes: close-on-exec on both ends — the child gets the write ends as its 1 and 2 only)
    let (out_r, out_w) = io::pipe()?;
    let (err_r, err_w) = io::pipe()?;

    let ok = |r: libc::c_int| if r == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(r)) };
    // SAFETY: every pointer handed over outlives the call (the CStrings and their arrays are held above);
    // the attributes and file actions are initialised before use and destroyed after.
    unsafe {
        let mut attr = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
        ok(libc::posix_spawnattr_init(attr.as_mut_ptr()))?;
        let mut actions = MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
        if let Err(e) = ok(libc::posix_spawn_file_actions_init(actions.as_mut_ptr())) {
            libc::posix_spawnattr_destroy(attr.as_mut_ptr());
            return Err(e);
        }
        let (a, f) = (attr.as_mut_ptr(), actions.as_mut_ptr());
        let spawned = (|| -> io::Result<libc::pid_t> {
            ok(libc::posix_spawn_file_actions_addopen(f, 0, null_c.as_ptr(), libc::O_RDONLY, 0))?;
            ok(libc::posix_spawn_file_actions_adddup2(f, out_w.as_raw_fd(), 1))?;
            ok(libc::posix_spawn_file_actions_adddup2(f, err_w.as_raw_fd(), 2))?;
            if let Some(dir) = &cwd_c {
                ok(addchdir(f, dir.as_ptr()))?;
            }
            // SIGPIPE back to its default, as std does for a child (tarae ignores it); the mask inherited
            let mut sigs = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(sigs.as_mut_ptr());
            libc::sigaddset(sigs.as_mut_ptr(), libc::SIGPIPE);
            ok(libc::posix_spawnattr_setsigdefault(a, sigs.as_ptr()))?;
            ok(libc::posix_spawnattr_setflags(a, FLAGS as _))?;
            let mut pid: libc::pid_t = 0;
            ok(libc::posix_spawn(&mut pid, program_c.as_ptr(), f, a, argv_p.as_ptr(), envp_p.as_ptr()))?;
            Ok(pid)
        })();
        libc::posix_spawn_file_actions_destroy(f);
        libc::posix_spawnattr_destroy(a);
        let pid = spawned?;
        Ok(Spawned { pid: pid as u32, stdout: Some(out_r), stderr: Some(err_r) })
    }
}

/// Elsewhere: the fork as before.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn setsid(cmd: &Command) -> io::Result<Spawned> {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;
    let mut c = Command::new(cmd.get_program());
    c.args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        c.current_dir(dir);
    }
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = crate::dap::setsid(&mut c).spawn()?;
    let pipe = |fd: Option<OwnedFd>| fd.map(PipeReader::from);
    Ok(Spawned {
        pid: child.id(),
        stdout: pipe(child.stdout.take().map(OwnedFd::from)),
        stderr: pipe(child.stderr.take().map(OwnedFd::from)),
    })
}

#[cfg(target_os = "macos")]
const FLAGS: libc::c_int = POSIX_SPAWN_SETSID
    | libc::POSIX_SPAWN_SETSIGDEF as libc::c_int
    // (and nothing but 0–2 open in it: a descriptor someone opened without close-on-exec — a pty's —
    // doesn't leak into every run)
    | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
#[cfg(target_os = "linux")]
const FLAGS: libc::c_int =
    libc::POSIX_SPAWN_SETSID as libc::c_int | libc::POSIX_SPAWN_SETSIGDEF as libc::c_int;

/// `<sys/spawn.h>` — not in the libc crate for Apple.
#[cfg(target_os = "macos")]
const POSIX_SPAWN_SETSID: libc::c_int = 0x0400;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    // (macOS 10.15+; `posix_spawn_file_actions_addchdir` only from 26)
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
unsafe fn addchdir(f: *mut libc::posix_spawn_file_actions_t, dir: *const libc::c_char) -> libc::c_int {
    unsafe { posix_spawn_file_actions_addchdir_np(f, dir) }
}

#[cfg(target_os = "linux")]
unsafe fn addchdir(f: *mut libc::posix_spawn_file_actions_t, dir: *const libc::c_char) -> libc::c_int {
    unsafe { libc::posix_spawn_file_actions_addchdir_np(f, dir) }
}

/// The child's environment: the editor's, with `cmd`'s changes.
fn environment(cmd: &Command) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (k, v) in cmd.get_envs() {
        env.retain(|(name, _)| name != k);
        if let Some(v) = v {
            env.push((k.to_os_string(), v.to_os_string()));
        }
    }
    env
}

/// The program's path as `execvp` in the child would find it (std's fork did that after `chdir`, with the
/// child's `PATH`): with a `/`, from the run's folder; else the first executable along `PATH`. Absolute,
/// so posix_spawn needn't resolve anything (macOS's misreports a relative one with a chdir as ENOENT).
fn resolve(program: &OsStr, cwd: Option<&Path>, path_var: Option<&OsStr>) -> io::Result<PathBuf> {
    let from = |p: &Path| match cwd {
        Some(dir) if p.is_relative() => dir.join(p),
        _ => p.to_path_buf(),
    };
    if program.as_bytes().contains(&b'/') {
        return std::path::absolute(from(Path::new(program)));
    }
    let not_found = || io::Error::from_raw_os_error(libc::ENOENT);
    if program.is_empty() {
        return Err(not_found());
    }
    for dir in std::env::split_paths(path_var.unwrap_or(OsStr::new(""))) {
        // (an empty entry is the folder it runs in)
        let full = from(&dir).join(program);
        if executable(&full) {
            return std::path::absolute(full);
        }
    }
    Err(not_found())
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Waits for `pid` to end (on a run's own thread — never the main loop).
pub fn wait(pid: u32) -> io::Result<ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let mut status = 0;
    loop {
        // SAFETY: waitpid on our own child, with a valid out pointer.
        if unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } != -1 {
            return Ok(ExitStatus::from_raw(status));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::time::{Duration, Instant};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarae-spawn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    /// Everything it printed, and how it ended.
    fn output(s: Spawned) -> (String, String, ExitStatus) {
        let (mut out, mut err) = (String::new(), String::new());
        s.stdout.unwrap().read_to_string(&mut out).unwrap();
        s.stderr.unwrap().read_to_string(&mut err).unwrap();
        (out, err, wait(s.pid).unwrap())
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only asks whether the process is there.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// A run is the leader of a session and a process group of its own (as `dap::setsid` made it), and
    /// `term_group` stops it with what it started.
    #[test]
    fn a_run_is_its_own_session_and_group() {
        let s = setsid(&sh("sleep 30 & echo $!; wait")).unwrap();
        let pid = s.pid;
        let mut line = String::new();
        BufReader::new(s.stdout.unwrap()).read_line(&mut line).unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        // SAFETY: getsid/getpgid only read.
        let (sid, pgid, its_pgid, ours) = unsafe {
            let p = pid as libc::pid_t;
            (libc::getsid(p), libc::getpgid(p), libc::getpgid(grandchild as libc::pid_t), libc::getsid(0))
        };
        assert_eq!(sid, pid as libc::pid_t, "a session of its own");
        assert_ne!(sid, ours);
        assert_eq!(pgid, pid as libc::pid_t, "its own process group");
        assert_eq!(its_pgid, pid as libc::pid_t, "what it starts is in that group");
        crate::dap::term_group(pid);
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(wait(pid).unwrap().signal(), Some(libc::SIGTERM));
        let end = Instant::now() + Duration::from_secs(5);
        while alive(grandchild) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(grandchild), "what it started stopped with it");
    }

    /// The folder, the environment's changes, stdin at its end, stdout and stderr apart, the exit code.
    #[test]
    fn a_run_gets_its_folder_environment_and_pipes() {
        let d = dir("env");
        let mut c = sh(
            "pwd; echo \"$TARAE_SPAWN_A\"; echo \"${HOME-unset}\"; echo err >&2; read x || echo eof; exit 3",
        );
        c.current_dir(&d).env("TARAE_SPAWN_A", "one").env_remove("HOME");
        let (out, err, status) = output(setsid(&c).unwrap());
        let real = d.canonicalize().unwrap();
        assert_eq!(out, format!("{}\none\nunset\neof\n", real.display()));
        assert_eq!(err, "err\n");
        assert_eq!(status.code(), Some(3));
        std::fs::remove_dir_all(d).ok();
    }

    /// The program is found as execvp in the child found it: `./x` from the run's folder, a bare name
    /// along the child's own `PATH`; one that isn't there is the error std gave.
    #[test]
    fn a_program_is_found_where_the_child_would_find_it() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("find");
        std::fs::create_dir_all(d.join("bin")).unwrap();
        let script = d.join("bin/tarae-spawn-probe");
        std::fs::write(&script, "#!/bin/sh\necho \"probe $1\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut rel = Command::new("./bin/tarae-spawn-probe");
        rel.arg("rel").current_dir(&d);
        assert_eq!(output(setsid(&rel).unwrap()).0, "probe rel\n");
        let mut on_path = Command::new("tarae-spawn-probe");
        on_path.arg("path").env("PATH", d.join("bin"));
        assert_eq!(output(setsid(&on_path).unwrap()).0, "probe path\n");
        let missing = Command::new("tarae-no-such-program");
        let e = setsid(&missing).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(e.to_string(), "No such file or directory (os error 2)");
        std::fs::remove_dir_all(d).ok();
    }

    /// A descriptor the editor holds without close-on-exec (a pty's, say) isn't open in a run.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_run_inherits_only_its_three_streams() {
        let mut fds = [0; 2];
        // SAFETY: pipe(2) with a valid two-int array; no close-on-exec, on purpose.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let script = format!("[ -e /dev/fd/{} ] && echo open || echo closed", fds[1]);
        let (out, _, _) = output(setsid(&sh(&script)).unwrap());
        // SAFETY: closing the two descriptors made above.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        assert_eq!(out, "closed\n");
    }

    /// A run's start costs the same whatever the editor's memory: posix_spawn, not a fork of the editor —
    /// measured beside the fork it replaces (`dap::setsid`'s `pre_exec`) with 230 MB touched and 64 parked
    /// threads (tarae's size in the design doc's measure). The median of 20 for `cargo perf`; the fork's said.
    /// At opt-level 0 (the test profile, and a full parallel run) it skips itself, saying so past libtest's
    /// capture — its 1 ms budget is for the `perf` profile, run alone.
    #[test]
    fn perf_run_spawn_with_a_big_heap() {
        if crate::test_results::unoptimized("spawn::tests::perf_run_spawn_with_a_big_heap") {
            return;
        }
        const MB: usize = 1 << 20;
        let mut heap = vec![0u8; 230 * MB];
        for b in heap.iter_mut().step_by(4096) {
            *b = 1;
        }
        let (park, parked) = std::sync::mpsc::channel::<()>();
        let parked = std::sync::Arc::new(std::sync::Mutex::new(parked));
        let threads: Vec<_> = (0..64)
            .map(|_| {
                let p = parked.clone();
                std::thread::spawn(move || {
                    let _ = p.lock().map(|r| r.recv());
                })
            })
            .collect();
        let median = |mut v: Vec<Duration>| {
            v.sort();
            v[v.len() / 2]
        };
        let (mut posix, mut fork) = (Vec::new(), Vec::new());
        for _ in 0..20 {
            let t = Instant::now();
            let s = setsid(&Command::new("true")).unwrap();
            posix.push(t.elapsed());
            wait(s.pid).unwrap();
            let mut c = Command::new("true");
            let t = Instant::now();
            let mut child = crate::dap::setsid(&mut c).spawn().unwrap();
            fork.push(t.elapsed());
            child.wait().unwrap();
        }
        let (posix, fork) = (median(posix), median(fork));
        std::hint::black_box(&heap);
        drop(park);
        for t in threads {
            let _ = t.join();
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let line = format!(
            "PERF run_spawn_with_230mb {:.3} budget=1 noise=0.1\n(the fork it replaced: {:.3} ms)\n",
            ms(posix),
            ms(fork)
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(posix < Duration::from_millis(1), "{posix:?} (the fork: {fork:?})");
    }
}
