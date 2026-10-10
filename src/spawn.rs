//! Starting a run's process (a task, tests — and every git tarae runs, `gitrepo`) in a session of its own
//! without forking the editor.
//!
//! `dap::setsid` is a `pre_exec`, and any `pre_exec` makes std fork the whole editor and exec in the child:
//! the fork copies the page tables, so it costs with the editor's memory — measured 2.9 ms at 230 MB and
//! 7.2 ms at 800 MB, on the main loop, every run. std's posix_spawn has no stable setsid (`process_setsid`
//! is unstable in 1.99) and asks it only on glibc. Here: posix_spawn with `POSIX_SPAWN_SETSID` (macOS
//! 10.15+, glibc 2.26+, musl 1.1.24+), the same whatever the memory. The process is what `dap::setsid`
//! made — its own session and process group (pgid = pid, no controlling terminal), so `dap::term_group`
//! stops it and everything it started.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, PipeReader, PipeWriter};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// A process started by `setsid`: wait for it with `wait`.
pub struct Spawned {
    pub pid: u32,
    pub stdout: Option<PipeReader>,
    pub stderr: Option<PipeReader>,
}

/// Whom a process tarae starts acts for — whether it's kept from new user namespaces (Q4, the user's call
/// 2026-10-08). Under tarae's AppArmor profile everything tarae starts carries its label and so its `userns`;
/// where that's so (`tier::runs_refuse_userns`), what acts for Claude or a plugin — sandboxed or not — gets the
/// filter that refuses one (`seccomp::NoUserns`). The user's own tools keep them, as what VS Code starts does under
/// Ubuntu's own `vscode` profile (the same shape: `flags=(unconfined) { userns, }`) — the terminal's shell,
/// language servers, git, formatters, the debugger, the user's tasks and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acts {
    User,
    /// A command of Claude's (`run`, `run_tests`) — outside the sandbox too: once, for the conversation, or with
    /// it off; the user's run again of one.
    Claude,
    /// What a plugin's permission starts (`plugin_spawn_now`).
    Plugin,
}

impl Acts {
    /// Whether its process is kept from new user namespaces, where runs are refused them (`refused_here`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // (Linux's — its test runs everywhere)
    pub fn no_userns(self, refused_here: bool) -> bool {
        refused_here && matches!(self, Acts::Claude | Acts::Plugin)
    }

    /// `no_userns` as this system decides it.
    #[cfg(target_os = "linux")]
    fn kept_here(self) -> bool {
        self.no_userns(crate::sandbox::tier::runs_refuse_userns())
    }
}

/// `setsid`, for whom it acts: kept from new user namespaces where `acts` says so — started from a short-lived
/// thread under the filter (`seccomp::outside`).
pub fn setsid_as(cmd: &Command, acts: Acts) -> io::Result<Spawned> {
    #[cfg(target_os = "linux")]
    if acts.kept_here() {
        return crate::seccomp::outside(|| setsid(cmd));
    }
    let _ = acts;
    setsid(cmd)
}

/// A `Command` std starts (a fork: `pre_exec`) kept from new user namespaces where `acts` says so — the filter
/// made now, put on in the child (`seccomp::NoUserns::apply`: raw calls only).
pub fn keep_from_userns(cmd: &mut Command, acts: Acts) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if acts.kept_here() {
        use std::os::unix::process::CommandExt;
        let filter = crate::seccomp::NoUserns::new()?;
        // SAFETY: `apply` makes raw calls only, between fork and exec.
        unsafe { cmd.pre_exec(move || filter.apply()) };
    }
    let _ = (cmd, acts);
    Ok(())
}

/// `cmd`'s program, arguments, environment changes and folder, started in a session of its own: stdin
/// `/dev/null`, stdout and stderr pipes (what `cmd` says of its stdio isn't read — `Command` doesn't tell;
/// nor `env_clear`: a variable to leave out is removed by name — `sandbox::from_run_env`).
pub fn setsid(cmd: &Command) -> io::Result<Spawned> {
    start(cmd, false).map(|(s, _)| s)
}

/// `setsid` with a pipe for its stdin (tarae's git fed a buffer's text — `gitrepo`): the end to write it,
/// to drop once written.
pub fn setsid_fed(cmd: &Command) -> io::Result<(Spawned, PipeWriter)> {
    let (s, input) = start(cmd, true)?;
    Ok((s, input.ok_or_else(|| io::Error::other("no stdin pipe"))?))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn start(cmd: &Command, fed: bool) -> io::Result<(Spawned, Option<PipeWriter>)> {
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
    let input = if fed { Some(io::pipe()?) } else { None };

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
            match &input {
                Some((r, _)) => ok(libc::posix_spawn_file_actions_adddup2(f, r.as_raw_fd(), 0))?,
                None => ok(libc::posix_spawn_file_actions_addopen(f, 0, null_c.as_ptr(), libc::O_RDONLY, 0))?,
            }
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
        // (the child's end of its stdin closed here: the writer's drop is its end of input)
        let input = input.map(|(_, w)| w);
        Ok((Spawned { pid: pid as u32, stdout: Some(out_r), stderr: Some(err_r) }, input))
    }
}

/// Elsewhere: the fork as before.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start(cmd: &Command, fed: bool) -> io::Result<(Spawned, Option<PipeWriter>)> {
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
    c.stdin(if fed { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = crate::dap::setsid(&mut c).spawn()?;
    let pipe = |fd: Option<OwnedFd>| fd.map(PipeReader::from);
    let input = child.stdin.take().map(|s| PipeWriter::from(OwnedFd::from(s)));
    let spawned = Spawned {
        pid: child.id(),
        stdout: pipe(child.stdout.take().map(OwnedFd::from)),
        stderr: pipe(child.stderr.take().map(OwnedFd::from)),
    };
    Ok((spawned, input))
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

/// The child's environment: the editor's, with `cmd`'s changes (a keeper's run is given it so too —
/// `sandbox::keeper`).
pub(crate) fn environment(cmd: &Command) -> Vec<(OsString, OsString)> {
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
pub(crate) fn resolve(program: &OsStr, cwd: Option<&Path>, path_var: Option<&OsStr>) -> io::Result<PathBuf> {
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

/// Every descriptor past stdio closes on exec. First thing at startup: what tarae was handed open (a
/// parent's leak — seen: four pty masters of another tarae's) would otherwise go on into every program std
/// starts, and std's spawn doesn't close them. And before `:restart`'s exec. The ones it marked, for a failed
/// exec to unmark.
pub fn close_on_exec() -> Vec<i32> {
    let fds: Vec<i32> = std::fs::read_dir("/dev/fd")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    let mut marked = Vec::new();
    for fd in fds {
        // SAFETY: reading and setting a flag on a descriptor number; one that's gone meanwhile (the
        // listing's own) just fails.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags != -1
                && flags & libc::FD_CLOEXEC == 0
                && libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
            {
                marked.push(fd);
            }
        }
    }
    marked
}

/// Waits for `pid` to end (on a run's own thread — never the main loop). A sandboxed run on Linux is its
/// session's keeper's child, not tarae's: its keeper says how it ended (`sandbox::keeper::wait`).
pub fn wait(pid: u32) -> io::Result<ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    #[cfg(target_os = "linux")]
    if let Some(how) = crate::sandbox::keeper::wait(pid) {
        return how;
    }
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

/// Waits for `pid`, a child nothing else holds — unless another `wait_once` already waits for it (a second
/// waiter could, after the first reaped it, wait on a later child given the same pid).
pub fn wait_once(pid: u32) {
    static WAITING: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
    {
        let mut w = WAITING.lock().unwrap_or_else(|e| e.into_inner());
        if w.contains(&pid) {
            return;
        }
        w.push(pid);
    }
    let _ = wait(pid);
    WAITING.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p != pid);
}

/// How long a run's output is still read once its process has ended.
pub const DRAIN_AFTER_EXIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Waits for `pid` — reaped the moment it ends, whoever still holds its output open (#131: a grandchild
/// that ignored the stop, or left its group, kept the pipe, and the process stopped from `:procs` stayed a
/// zombie until tarae exited) — then up to `DRAIN_AFTER_EXIT` for `readers` reader threads to say they
/// reached the end of it (`eof`).
pub fn wait_and_drain(
    pid: u32,
    eof: &std::sync::mpsc::Receiver<()>,
    readers: usize,
) -> io::Result<ExitStatus> {
    let status = wait(pid);
    let end = std::time::Instant::now() + DRAIN_AFTER_EXIT;
    for _ in 0..readers {
        if eof.recv_timeout(end.saturating_duration_since(std::time::Instant::now())).is_err() {
            break;
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::time::{Duration, Instant};

    /// The user's call (2026-10-08): where tarae's profile would lend user namespaces to everything it starts,
    /// what acts for Claude or a plugin is kept from them — sandboxed or not — and the user's own tools keep
    /// them, as VS Code's do under Ubuntu's `vscode` profile. Where runs aren't refused them, nothing is.
    #[test]
    fn only_claudes_and_plugins_processes_are_kept_from_user_namespaces() {
        let kinds = [Acts::User, Acts::Claude, Acts::Plugin];
        assert_eq!(kinds.map(|a| a.no_userns(true)), [false, true, true], "under the profile");
        assert_eq!(kinds.map(|a| a.no_userns(false)), [false; 3], "elsewhere");
    }

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

    /// Fed: what's written reaches its stdin, and dropping the writer ends it — in a session of its own still.
    #[test]
    fn a_fed_run_reads_what_its_given() {
        use std::io::Write as _;
        let (s, mut w) = setsid_fed(&sh("cat; echo end")).unwrap();
        // SAFETY: getsid only reads.
        assert_eq!(unsafe { libc::getsid(s.pid as libc::pid_t) }, s.pid as libc::pid_t);
        w.write_all(b"one\ntwo\n").unwrap();
        drop(w);
        let (out, _, status) = output(s);
        assert_eq!((out.as_str(), status.success()), ("one\ntwo\nend\n", true));
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

    /// A child that lists the descriptors it has open: `/dev/fd` read, then each one still there once the
    /// listing's own has closed (`ls` keeps the folder it started in open besides, on macOS).
    const LIST_FDS: &str = "for f in /dev/fd/*; do [ -e \"$f\" ] && echo \"${f#/dev/fd/}\"; done";

    /// What it printed, as numbers.
    fn numbers(listing: &str) -> Vec<u32> {
        let mut fds: Vec<u32> = listing.split_whitespace().filter_map(|w| w.parse().ok()).collect();
        fds.sort_unstable();
        fds
    }

    /// Every way tarae starts a program hands it its three streams and nothing else — not what tarae was
    /// handed open (a pipe without close-on-exec, made before `close_on_exec` runs as it does at startup),
    /// nor the pty of a terminal panel running meanwhile, nor one just opened (`pty::open`), nor a pipe std
    /// made. Each child is `sh` listing its own (`LIST_FDS`). Run in a process of its own
    /// (`descriptors_in_its_own_process`): `close_on_exec` marks every descriptor of the process it runs
    /// in, and the handed pipe would slip into other tests' children here.
    #[test]
    fn every_way_of_starting_a_program_hands_it_only_its_own_descriptors() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "spawn::tests::descriptors_in_its_own_process", "--ignored", "--nocapture"])
            .env("TARAE_DESCRIPTORS", "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{said}");
        assert!(said.contains("1 passed"), "the child ran it: {said}");
    }

    /// `every_way_of_starting_a_program_hands_it_only_its_own_descriptors`'s child. On macOS std makes a
    /// pipe close-on-exec only a moment after it's made, so a pipe of another thread's can slip into a child
    /// here: a path passes on any of three tries.
    #[test]
    #[ignore]
    fn descriptors_in_its_own_process() {
        use std::process::Stdio;
        if std::env::var_os("TARAE_DESCRIPTORS").is_none() {
            return;
        }
        let mut handed = [0; 2];
        // SAFETY: pipe(2) with a valid two-int array; no close-on-exec, on purpose (closed at the end).
        assert_eq!(unsafe { libc::pipe(handed.as_mut_ptr()) }, 0);
        // (as `main` does first thing — and what the test binary was handed goes the same way)
        close_on_exec();
        let (config, _) = crate::config::parse("");
        let mut ed = crate::editor::Editor::new(config);
        // A terminal panel running, its master held here; a pty and a pipe held besides
        ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), "echo up; sleep 30".into()], Vec::new());
        let end = Instant::now() + Duration::from_secs(5);
        let shows = |ed: &crate::editor::Editor, s: &str| {
            ed.session.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains(s))
        };
        while !shows(&ed, "up") && Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
        assert!(
            shows(&ed, "up"),
            "the terminal panel's program started: {:?}",
            ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        let held = (crate::pty::open(5, 20).unwrap(), io::pipe().unwrap());
        let ls = || {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", LIST_FDS]);
            c
        };
        let out = |o: std::process::Output| String::from_utf8_lossy(&o.stdout).into_owned();
        let paths: [(&str, &dyn Fn() -> String); 5] = [
            // language servers, Claude — std's spawn
            ("std", &|| {
                let cfg = crate::llm::LlmConfig {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), LIST_FDS.into()],
                    ..Default::default()
                };
                out(crate::llm::spawn(&cfg, &[], Path::new("/"), None).unwrap().wait_with_output().unwrap())
            }),
            // the debugger, plugins — std's fork with `pre_exec`
            ("dap::setsid", &|| {
                let mut c = ls();
                c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
                out(crate::dap::setsid(&mut c).output().unwrap())
            }),
            // runs and tests, tarae's git (fed its stdin: `setsid_fed`)
            ("spawn::setsid", &|| output(setsid(&ls()).unwrap()).0),
            ("spawn::setsid_fed", &|| output(setsid_fed(&ls()).unwrap().0).0),
            // the terminal panel's next program
            ("terminal", &|| {
                let mut t = crate::editor::Editor::new(crate::config::parse("").0);
                t.terminal_run(vec!["/bin/sh".into(), "-c".into(), LIST_FDS.into()], Vec::new());
                let end = Instant::now() + Duration::from_secs(5);
                while t.session.terminal.as_ref().is_none_or(|t| t.exited.is_none()) && Instant::now() < end {
                    if let Some(ev) = t.events.recv_timeout(Duration::from_millis(50)) {
                        t.handle_event(ev);
                    }
                }
                t.session.terminal.as_ref().map(|t| t.parser.screen().contents()).unwrap_or_default()
            }),
        ];
        let mut leaked = Vec::new();
        for (path, list) in paths {
            let mut tries = Vec::new();
            while tries.len() < 3 {
                let fds = numbers(&list());
                let clean = fds == [0, 1, 2];
                tries.push(fds);
                if clean {
                    break;
                }
            }
            if tries.last().is_some_and(|fds| fds != &[0, 1, 2]) {
                leaked.push(format!("{path}: {tries:?}"));
            }
        }
        drop(held);
        // SAFETY: closing the two descriptors made above.
        unsafe {
            libc::close(handed[0]);
            libc::close(handed[1]);
        }
        assert!(leaked.is_empty(), "a child had more than 0–2 open:\n{}", leaked.join("\n"));
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
