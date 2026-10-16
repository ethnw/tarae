use super::*;

/// The soft limit goes up to the hard one, capped by the system — never down, never to unlimited.
#[test]
fn the_soft_limit_goes_up_to_the_hard_one_never_down() {
    let inf = libc::RLIM_INFINITY;
    assert_eq!(target(1024, 1_048_576, Some(1_048_576)), Some(1_048_576), "Linux: up to the hard one");
    assert_eq!(target(256, inf, Some(138_240)), Some(138_240), "macOS: hard unlimited, capped by the system");
    assert_eq!(target(256, 10_240, Some(138_240)), Some(10_240), "the hard one below the cap");
    assert_eq!(target(1_048_576, inf, Some(138_240)), None, "already above the cap: never lowered");
    assert_eq!(target(4096, 4096, None), None, "soft = hard: nothing to raise");
    assert_eq!(target(256, inf, None), None, "unlimited isn't a number setrlimit takes");
}

/// The shim takes exactly `--nofile N <absolute path> <argv0> args…`: anything else of that shape is no run.
#[test]
fn the_shim_takes_only_a_number_an_absolute_path_and_an_argv0() {
    let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
    let ok = a(&["--nofile", "1024", "/bin/sh", "sh", "-c", "true"]);
    let (n, program, argv0, rest) = parse(&ok).unwrap();
    assert_eq!((n, program, argv0, rest), (1024, OsStr::new("/bin/sh"), OsStr::new("sh"), &ok[4..]));
    assert!(parse(&a(&["--nofile", "1024", "/bin/true", "true"])).is_some(), "no arguments: fine");
    for bad in [
        &["--nofile"][..],
        &["--nofile", "1024"],
        &["--nofile", "1024", "/bin/sh"],
        &["--nofile", "", "/bin/sh", "sh"],
        &["--nofile", "1k", "/bin/sh", "sh"],
        &["--nofile", "-1", "/bin/sh", "sh"],
        &["--nofile", "1024", "sh", "sh"],
        &["--nofile", "1024", "./sh", "sh"],
        &["--nofile=1024", "/bin/sh", "sh"],
        &["--other", "1024", "/bin/sh", "sh"],
    ] {
        assert!(parse(&a(bad)).is_none(), "{bad:?}");
    }
}

/// The shim's binary is pinned (macOS's `posix_spawn` takes a path): what's pinned stays the build that was
/// started, when a build replaces the file as cargo does (a new file renamed over it) and when it's removed (`cargo
/// clean`); pinning again (a `:restart`, the same pid) replaces the pin. (A stand-in binary in the test's folder.)
#[test]
fn the_pinned_binary_stays_the_one_started_whatever_happens_to_its_path() {
    let dir = crate::scratch::Scratch::new("nofile-pin");
    let exe = dir.join("tarae");
    std::fs::write(&exe, "build one").unwrap();
    let pins = dir.join("pins");
    let (at, id) = pin(&exe, &pins, "4242").unwrap();
    assert_eq!(at, pins.join("4242"));
    assert_eq!(identity(&at), Some(id), "its identity, as checked before each start");
    std::fs::write(dir.join("tarae.tmp"), "build two").unwrap();
    std::fs::rename(dir.join("tarae.tmp"), &exe).unwrap();
    assert_eq!(
        std::fs::read_to_string(&at).unwrap(),
        "build one",
        "replaced as cargo does: still the one started"
    );
    std::fs::remove_file(&exe).unwrap();
    assert_eq!(std::fs::read_to_string(&at).unwrap(), "build one", "removed: still there");
    std::fs::write(&exe, "build three").unwrap();
    pin(&exe, &pins, "4242").unwrap();
    assert_eq!(std::fs::read_to_string(&at).unwrap(), "build three", "pinned again: the new one");
    assert!(!pins.join("4242.new").exists(), "nothing left beside it");
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(std::fs::metadata(&pins).unwrap().permissions().mode() & 0o777, 0o700, "tarae's own folder");
}

/// Pinning again (a `:restart`'s new image keeps its pid) isn't held up by a child forked meanwhile — anywhere in
/// tarae, a server, a run, git: from its fork to its exec it holds a copy of every descriptor, the lock's too, and
/// a lock that belonged to that open file would still be held. (The child is this test's own `true`, waiting
/// between its fork and its exec until the pin is taken again.)
#[test]
fn pinning_again_isnt_held_up_by_a_child_forked_meanwhile() {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    let dir = crate::scratch::Scratch::new("nofile-repin");
    let exe = dir.join("tarae");
    std::fs::write(&exe, "build one").unwrap();
    let pins = dir.join("pins");
    pin(&exe, &pins, "4243").unwrap();
    // (the child says it's forked, then waits a second before its exec)
    let (mut forked, told) = std::io::pipe().unwrap();
    let child = std::thread::spawn(move || {
        let told_fd = told.as_raw_fd();
        let mut c = std::process::Command::new("true");
        // SAFETY: write and nanosleep only, between fork and exec.
        unsafe {
            c.pre_exec(move || {
                libc::write(told_fd, b"f".as_ptr().cast(), 1);
                let t = libc::timespec { tv_sec: 1, tv_nsec: 0 };
                libc::nanosleep(&t, std::ptr::null_mut());
                Ok(())
            });
        }
        let status = c.status();
        drop(told);
        status
    });
    forked.read_exact(&mut [0u8]).unwrap();
    std::fs::write(&exe, "build two").unwrap();
    let again = pin(&exe, &pins, "4243");
    assert!(child.join().unwrap().unwrap().success());
    let (at, _) = again.expect("pinned again while a child forked before it still held its copies");
    assert_eq!(std::fs::read_to_string(&at).unwrap(), "build two");
}

/// A pin is made through no link: a folder of pins that's a link is refused (not gone through), and so is one
/// others may enter; a `.new` left there as a link to elsewhere is replaced, what it pointed at untouched.
#[test]
fn a_pin_goes_through_no_link() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::scratch::Scratch::new("nofile-pin-links");
    std::fs::create_dir(dir.join("elsewhere")).unwrap();
    // (this user's alone, as a pins folder must be: only going through no link keeps it out)
    std::fs::set_permissions(dir.join("elsewhere"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let exe = dir.join("tarae");
    std::fs::write(&exe, "build").unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("pins-link")).unwrap();
    assert!(pin(&exe, &dir.join("pins-link"), "1").is_err(), "a folder of pins that's a link: refused");
    assert!(!dir.join("elsewhere/1").exists(), "nothing made through it");
    std::fs::create_dir(dir.join("pins-open")).unwrap();
    std::fs::set_permissions(dir.join("pins-open"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(pin(&exe, &dir.join("pins-open"), "1").is_err(), "a folder others may enter: refused");
    let pins = dir.join("pins");
    std::fs::create_dir(&pins).unwrap();
    std::fs::set_permissions(&pins, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(dir.join("elsewhere/target"), "theirs").unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere/target"), pins.join("1.new")).unwrap();
    let (at, _) = pin(&exe, &pins, "1").unwrap();
    assert_eq!(std::fs::read_to_string(&at).unwrap(), "build");
    assert!(!std::fs::symlink_metadata(&at).unwrap().file_type().is_symlink(), "a file, not a link");
    assert_eq!(
        std::fs::read_to_string(dir.join("elsewhere/target")).unwrap(),
        "theirs",
        "never gone through"
    );
}

/// A pin whose tarae is gone — its lock free, or no lock at all — is removed with its lock and any `.new`; a
/// running one's (its lock held by another process, as that tarae holds it: here the test's own `sleep`, the
/// lock taken between its fork and its exec and kept across it) and this tarae's own stay, and what isn't a pin
/// is left alone.
#[test]
fn the_pins_of_a_tarae_thats_gone_are_removed() {
    use std::os::unix::process::CommandExt as _;
    let dir = crate::scratch::Scratch::new("nofile-prune");
    for name in ["10", "10.lock", "11", "11.lock", "12", "12.lock", "12.new", "13", "notes.txt"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    let lock = CString::new(dir.join("11.lock").as_os_str().as_bytes()).unwrap();
    let mut c = std::process::Command::new("sleep");
    c.arg("30");
    // SAFETY: open and fcntl only, between fork and exec (the descriptor left open: the lock goes with the exec).
    unsafe {
        c.pre_exec(move || {
            let fd = libc::open(lock.as_ptr(), libc::O_RDWR);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            lock_now(fd)
        });
    }
    // (spawn returns once it exec'd: the lock is held by then)
    let mut held = c.spawn().unwrap();
    let gone = prune_pins_in(&dir, 10);
    let mut left: Vec<String> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    held.kill().ok();
    held.wait().ok();
    assert_eq!(gone, 2, "12 (its lock free) and 13 (none)");
    assert_eq!(left, ["10", "10.lock", "11", "11.lock", "notes.txt"], "its own, a running one's, not a pin");
}

/// No pin (a hard link can't cross volumes; no state folder): the binary itself is the shim, checked before each
/// start — said once. On Linux, where `/dev/shm` is another file system than the temp folder, a real link across
/// is tried and must fail so (elsewhere: the choice alone).
#[test]
fn without_a_pin_the_binary_itself_is_checked() {
    let dir = std::env::temp_dir().join(format!("tarae-nofile-nopin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("tarae");
    std::fs::write(&exe, "build").unwrap();
    let across = Err(std::io::Error::from_raw_os_error(libc::EXDEV));
    let (at, id, note) = shim_exe(&exe, across);
    assert_eq!((at.as_path(), id), (exe.as_path(), identity(&exe)), "the binary, its identity kept");
    let note = note.expect("said");
    assert!(note.contains("not pinned") && note.contains("keeps the raised limit"), "{note}");
    let shm = Path::new("/dev/shm");
    if cfg!(target_os = "linux") && shm.is_dir() && identity(shm).map(|i| i.0) != identity(&dir).map(|i| i.0)
    {
        let pins = shm.join(format!("tarae-nofile-nopin-{}", std::process::id()));
        let tried = pin(&exe, &pins, "1");
        std::fs::remove_dir_all(&pins).ok();
        assert_eq!(
            tried.err().and_then(|e| e.raw_os_error()),
            Some(libc::EXDEV),
            "a hard link across volumes"
        );
        eprintln!("without_a_pin_the_binary_itself_is_checked: a link across to /dev/shm tried — EXDEV");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The pins' folder is out of every sandboxed run's reach: no place a run may write (`sandbox::write_places`, the
/// policy's own — on this system's sandbox) is it, holds it or is inside it, nor are tarae's cache's run homes
/// near it. So between the check before a start and the shim's exec, nothing inside the sandbox can swap the pin.
#[test]
fn no_sandboxed_run_can_write_where_pins_are() {
    let root = std::env::temp_dir().join(format!("tarae-nofile-places-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let Some(pins) = pins_dir() else {
        eprintln!("nofile::tests::no_sandboxed_run_can_write_where_pins_are: no state folder here");
        return;
    };
    let pins = crate::sandbox::profile_path(&pins);
    let places = crate::sandbox::write_places(&root, &crate::llm::LlmConfig::default());
    std::fs::remove_dir_all(&root).ok();
    assert!(!places.is_empty(), "a run writes somewhere");
    for w in &places {
        assert!(!pins.starts_with(w) && !w.starts_with(&pins), "{} reaches {}", w.display(), pins.display());
    }
}

/// What tells `raised_scene` its test started it.
const SCENE: &str = "TARAE_NOFILE_SCENE";

/// Started with a soft limit of 256 in a process of its own (the test binary on `raised_scene`, the limit set in
/// its child before the exec — this process's own stays as it is), tarae's raise lifts it, and each way tarae
/// starts a process gives the 256 back but git's: a fork's (`dap::setsid`'s `pre_exec`), posix_spawn's
/// (`spawn::setsid`, and `nofile::command` for std's — a relative path from the folder it runs in, a debug build's
/// too) through the shim — here a stand-in of it (a script doing what `shim_main` does, saying the `argv[0]` it
/// passes on: the test binary isn't tarae; tarae's own is tests/e2e.rs's), each program given the `argv[0]` it
/// was given — and git's (`spawn::setsid_git`) left raised. `:restart`'s exec gets the 256 too, and a failed one
/// puts the raised one back. Last, the shim's binary replaced behind tarae's back: no shim from then on (the
/// raised limit), said in the log.
#[test]
fn a_raised_limit_goes_back_to_what_tarae_starts_but_git() {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "nofile::tests::raised_scene", "--ignored", "--nocapture", "--test-threads=1"])
        .env(SCENE, "1");
    // SAFETY: getrlimit/setrlimit only, between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut l);
            l.rlim_cur = 256;
            libc::setrlimit(libc::RLIMIT_NOFILE, &l);
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let said: std::collections::HashMap<&str, &str> =
        text.lines().filter_map(|l| l.split_once("scene: ")?.1.split_once('=')).collect();
    let err = String::from_utf8_lossy(&out.stderr);
    let get = |k: &str| *said.get(k).unwrap_or_else(|| panic!("the scene didn't say {k}:\n{text}\n{err}"));
    let l = limit();
    let want = target(256, l.rlim_max, cap()).map(|n| n.to_string());
    let Some(want) = want else {
        eprintln!(
            "nofile::tests::a_raised_limit_goes_back_to_what_tarae_starts_but_git: nothing to raise to here"
        );
        return;
    };
    assert_eq!(get("raised"), want, "up to the hard limit (capped)");
    assert_eq!(get("original"), "256");
    assert_eq!(get("fork"), "256 none", "a pre_exec child (no shim)");
    assert_eq!(get("spawn"), "256 sh", "spawn::setsid, through the shim, its argv[0] the name given");
    assert_eq!(get("command"), "256 sh", "nofile::command, through the shim, its argv[0] the name given");
    let relative = "256 bin/server";
    assert_eq!(
        get("relative"),
        relative,
        "a relative path found from the folder it runs in, through the shim"
    );
    assert_eq!(get("build"), "256", "a debug build (debugpack::run_build), through the shim");
    assert_eq!(get("git"), format!("{want} none"), "git's left raised");
    assert_eq!(get("exec"), "256", "for an exec of tarae itself");
    assert_eq!(get("undone"), want, "the exec failed: raised again");
    assert_eq!(
        get("replaced"),
        format!("{want} none"),
        "the shim's binary replaced: not run, the limit raised"
    );
    assert_eq!(get("said"), "1", "said once in the log, not at every start");
}

/// `a_raised_limit_goes_back_to_what_tarae_starts_but_git`'s process.
#[test]
#[ignore = "run by a_raised_limit_goes_back_to_what_tarae_starts_but_git, in a process of its own"]
fn raised_scene() {
    if std::env::var_os(SCENE).is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("tarae-nofile-scene-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // (the shim's stand-in: the flag, the limit, the program, its argv[0] — said in `TARAE_TEST_ARGV0`: a script
    // can't set it — and its arguments, as `shim_main`)
    let shim = dir.join("shim");
    let script = "#!/bin/sh\n[ \"$1\" = --nofile ] || exit 9\nulimit -Sn \"$2\" || exit 8\npath=$3\n\
                  TARAE_TEST_ARGV0=$4\nexport TARAE_TEST_ARGV0\nshift 4\nexec \"$path\" \"$@\"\n";
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    remember_exe_for_test(&shim);
    let said = raise();
    let soft = || limit().rlim_cur.to_string();
    println!("scene: raised={}", soft());
    println!("scene: original={}", original().map_or("none".into(), |n| n.to_string()));
    // (its limit, and the argv[0] the shim passed on — none without one)
    const LIMIT_AND_ARGV0: &str = "echo $(ulimit -Sn) ${TARAE_TEST_ARGV0:-none}";
    let sh = || {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", LIMIT_AND_ARGV0]);
        c
    };
    let first_line = |b: &[u8]| String::from_utf8_lossy(b).lines().next().unwrap_or("").to_string();
    let fork = crate::dap::setsid(&mut sh()).output().unwrap();
    println!("scene: fork={}", first_line(&fork.stdout));
    let read = |s: crate::spawn::Spawned| {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut s.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(s.pid).unwrap();
        first_line(&out)
    };
    println!("scene: spawn={}", read(crate::spawn::setsid(&sh()).unwrap()));
    println!("scene: git={}", read(crate::spawn::setsid_git(&sh()).unwrap()));
    let commanded = command("sh", None).args(["-c", LIMIT_AND_ARGV0]).output().unwrap();
    println!("scene: command={}", first_line(&commanded.stdout));
    // (a relative path from the folder it runs in — as a project's `[lsp.*] command = "bin/server"`; this process
    // runs elsewhere)
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/server"), format!("#!/bin/sh\n{LIMIT_AND_ARGV0}\n")).unwrap();
    std::fs::set_permissions(dir.join("bin/server"), std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();
    let relative = command("bin/server", Some(&dir)).output().unwrap();
    println!("scene: relative={}", first_line(&relative.stdout));
    // (a debug build — the project's code runs in it: what it prints as built is its limit)
    let build = crate::debugpack::Build {
        run: vec![
            "sh".into(),
            "-c".into(),
            "printf '{\"k\":\"v\",\"p\":\"%s\"}\\n' \"$(ulimit -Sn)\"".into(),
        ],
        env: Vec::new(),
        pick: vec![("k".into(), serde_json::json!("v"))],
        take: "p".into(),
        error: None,
        none: "nothing built".into(),
    };
    let built = crate::debugpack::run_build(&build, &dir).map(|p| p.display().to_string());
    println!("scene: build={}", built.unwrap_or_else(|e| e));
    let exec = for_exec();
    println!("scene: exec={}", soft());
    exec.undo();
    println!("scene: undone={}", soft());
    // (last: the stand-in replaced as a build replaces a binary — a new file renamed over it — by one that would
    // fail; twice, said once)
    let log = crate::log::capture();
    std::fs::write(dir.join("shim.new"), "#!/bin/sh\nexit 7\n").unwrap();
    std::fs::set_permissions(dir.join("shim.new"), std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();
    std::fs::rename(dir.join("shim.new"), &shim).unwrap();
    println!("scene: replaced={}", read(crate::spawn::setsid(&sh()).unwrap()));
    let _ = read(crate::spawn::setsid(&sh()).unwrap());
    let told = log.lines().iter().filter(|l| l.contains("is no longer the one remembered")).count();
    println!("scene: said={told}");
    std::fs::remove_dir_all(&dir).ok();
    drop(said);
}

/// What a program using `select()` meets on macOS with a descriptor past `FD_SETSIZE` (1024), as measured on
/// macOS 27 (2026-10-13) — the record of why macOS gets the shim too. A program built with a current SDK: `FD_SET`
/// skips it (the SDK's `__darwin_check_fd_set_overflow`, compiled into the program: no write past the set, no
/// abort) and `select` refuses such an `nfds` with `EINVAL`. One that passes `FD_SETSIZE` as `nfds` then never
/// hears of that descriptor, and one built with an SDK older than macOS 11 (or setting bits itself) still writes
/// past its set — so a raised limit isn't harmless there either. A C program built here (in the test's own
/// folder; no C compiler → skipped, said) opens 1100 descriptors, then `FD_SET`s the last into an `fd_set` with a
/// canary after it, or `select`s on it with a set big enough to hold it.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_a_descriptor_past_fd_setsize_is_skipped_by_fd_set_and_refused_by_select() {
    const PROGRAM: &str = r#"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/select.h>
int main(int argc, char **argv) {
    struct rlimit l;
    getrlimit(RLIMIT_NOFILE, &l);
    if (l.rlim_max < 2048) return 3;
    l.rlim_cur = 2048;
    if (setrlimit(RLIMIT_NOFILE, &l) != 0) return 3;
    int fd = -1;
    for (int i = 0; i < 1100; i++) if ((fd = open("/dev/null", O_RDONLY)) < 0) return 4;
    if (fd < FD_SETSIZE) return 5;
    if (strcmp(argv[1], "set") == 0) {
        struct { fd_set set; unsigned char canary[512]; } s;
        memset(s.canary, 0xAA, sizeof s.canary);
        FD_ZERO(&s.set);
        FD_SET(fd, &s.set);
        for (size_t i = 0; i < sizeof s.canary; i++) if (s.canary[i] != 0xAA) { puts("corrupted"); return 0; }
        puts("set");
        return 0;
    }
    fd_set *big = calloc(1, 4096);
    big->fds_bits[fd / 32] |= 1u << (fd % 32);
    struct timeval none = {0, 0};
    int r = select(fd + 1, big, NULL, NULL, &none);
    printf("select %d %s\n", r, r < 0 ? strerror(errno) : "");
    return 0;
}
"#;
    let scratch = crate::scratch::Scratch::new("nofile-select");
    let dir = scratch.to_path_buf();
    std::fs::write(dir.join("p.c"), PROGRAM).unwrap();
    let bin = dir.join("p");
    let built = std::process::Command::new("cc").arg("-O0").arg("-o").arg(&bin).arg(dir.join("p.c")).output();
    if !built.is_ok_and(|o| o.status.success()) {
        std::fs::remove_dir_all(&dir).ok();
        eprintln!(
            "nofile::tests::on_macos_a_descriptor_past_fd_setsize_is_skipped_by_fd_set_and_refused_by_select: \
             no C compiler"
        );
        return;
    }
    let run = |how: &str| std::process::Command::new(&bin).arg(how).output().unwrap();
    let (set, select) = (run("set"), run("select"));
    std::fs::remove_dir_all(&dir).ok();
    use std::os::unix::process::ExitStatusExt as _;
    let said = |o: &std::process::Output| {
        format!(
            "{:?} signal {:?}: {}",
            o.status.code(),
            o.status.signal(),
            String::from_utf8_lossy(&o.stdout)
        )
    };
    assert_eq!(
        String::from_utf8_lossy(&set.stdout).trim(),
        "set",
        "FD_SET past FD_SETSIZE: skipped, nothing past the set written, no abort — {}",
        said(&set)
    );
    assert!(
        String::from_utf8_lossy(&select.stdout).starts_with("select -1 Invalid argument"),
        "select with nfds past FD_SETSIZE: EINVAL — {}",
        said(&select)
    );
}
