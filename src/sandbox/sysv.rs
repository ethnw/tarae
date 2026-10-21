//! What a run leaves in the Mac's System V shared memory (frictions 56). Seatbelt asks the profile about
//! `shmget` only for a segment that's there already: a run makes a new one unasked — under srt's profile as
//! under tarae's (both deny every `ipc-sysv-*`; srt allows POSIX's `ipc-posix-shm`/`-sem` only, as `BASE`) — and
//! is then refused `shmat` and `IPC_RMID` on it. A segment lasts until it's removed or the Mac restarts, and
//! `kern.sysv.shmmni` (32) of them make every program's `shmget` fail (ENOSPC, before any sandbox is asked): a
//! build, a test or a hostile script fills them in a moment, and nothing was refused for the report to say. No
//! rule refuses the making (XNU's `shmget_allocate` asks the MAC policy nothing).
//!
//! tarae never deletes what it can't tie to the run (the user's decision, 2026-10-20), and it can't tie a
//! segment to one: a segment names only its maker's pid (`shm_cpid`), and the run's processes can't be named
//! reliably — the looks at a run's tree (`procs`, every few seconds) miss a maker that lives milliseconds (a
//! script's `shmget`), macOS's process events don't name a fork's child (no `NOTE_TRACK`), and Endpoint
//! Security needs an entitlement. So a run's end (`after_run`, on the run's own thread) only names what was
//! made while it ran: a segment of the user's made since it began (`shm_ctime`, whole seconds) that nothing has
//! attached (`shm_lpid` and `shm_nattch` 0 — a sandboxed process can't attach one) — its report says them,
//! with the `ipcrm` that frees them, as made by the run or another program of the user's (`RunEnd::sysv_said`),
//! and the user decides. Listed by `sysctl kern.sysv.ipcs.shm` as `ipcs` lists them — refused inside the
//! sandbox: tarae's own look.

use std::time::SystemTime;

/// A segment as the system lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub id: libc::c_int,
    pub key: libc::key_t,
    pub cuid: libc::uid_t,
    pub cpid: libc::pid_t,
    pub lpid: libc::pid_t,
    pub nattch: libc::shmatt_t,
    pub ctime: libc::time_t,
}

/// `struct IPCS_command` (`sys/ipcs.h`), a 64-bit process's: read and written in place by the sysctl.
#[repr(C)]
struct IpcsCommand {
    magic: libc::c_int,
    op: libc::c_int,
    cursor: libc::c_int,
    datalen: libc::c_int,
    data: *mut libc::c_void,
}

const IPCS_MAGIC: libc::c_int = 1;
const IPCS_SHM_ITER: libc::c_int = 2;

/// The system's segments, as `ipcs -m` lists them. Err: they can't be listed (inside a sandbox).
pub fn listed() -> std::io::Result<Vec<Seg>> {
    let mut out = Vec::new();
    // SAFETY: a `shmid_ds` of zeroes is a valid one (integers, and a pointer the kernel writes, never read here).
    let mut ds: libc::shmid_ds = unsafe { std::mem::zeroed() };
    let mut ic = IpcsCommand {
        magic: IPCS_MAGIC,
        op: IPCS_SHM_ITER,
        cursor: 0,
        datalen: size_of::<libc::shmid_ds>() as libc::c_int,
        data: (&raw mut ds).cast(),
    };
    loop {
        let mut size = size_of::<IpcsCommand>();
        // SAFETY: `ipcs`'s own protocol — the command read and written in place, its data our `ds`, both live.
        let r = unsafe {
            libc::sysctlbyname(
                c"kern.sysv.ipcs.shm".as_ptr(),
                (&raw mut ic).cast(),
                &mut size,
                (&raw mut ic).cast(),
                size_of::<IpcsCommand>(),
            )
        };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            // (past the last one: ENOENT, or ERANGE once the cursor is past the last slot)
            return match e.raw_os_error() {
                Some(libc::ENOENT | libc::ERANGE) => Ok(out),
                _ => Err(e),
            };
        }
        // (the cursor is past the slot read: that slot's index and the segment's sequence make its id, as `ipcs`'s
        // `IXSEQ_TO_IPCID`)
        let (perm, ix) = (ds.shm_perm, ic.cursor - 1);
        out.push(Seg {
            id: (libc::c_int::from(perm._seq) << 16) | (ix & 0xffff),
            key: perm._key,
            cuid: perm.cuid,
            cpid: ds.shm_cpid,
            lpid: ds.shm_lpid,
            nattch: ds.shm_nattch,
            ctime: ds.shm_ctime,
        });
    }
}

/// The ids of the segments made while a run that began at `began` ran (the module's rule) — named, never removed.
/// On the run's thread, once it's over. (Tests: only the segments they named — `TEST_IDS` — never the machine's.)
pub fn after(began: SystemTime) -> Vec<libc::c_int> {
    let Ok(segs) = listed() else { return Vec::new() };
    let since = began.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as libc::time_t);
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let made = segs.iter().filter(|s| s.cuid == uid && s.ctime >= since && s.nattch == 0 && s.lpid == 0);
    #[cfg(test)]
    let made = made.filter(|s| TEST_IDS.with(|ids| ids.borrow().contains(&s.id)));
    made.map(|s| s.id).collect()
}

#[cfg(test)]
thread_local! {
    /// Tests: the ids of the segments this test made — the only ones `after` names in a test build, on the test's
    /// own thread (where its run's end is asked): tests at once never name each other's (the review's finding).
    pub static TEST_IDS: std::cell::RefCell<Vec<libc::c_int>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Env, Policy, after_run, command, skipped_inside_for};

    /// A key of this test's own.
    fn own_key(n: libc::key_t) -> libc::key_t {
        0x7a30_0000 + (n << 16) + (std::process::id() as libc::key_t & 0xffff)
    }

    fn by_id(id: libc::c_int) -> Option<Seg> {
        listed().unwrap().into_iter().find(|s| s.id == id)
    }

    /// The test's own segments, named for `after` and removed when the test ends however it ends — a failing
    /// check never leaves one until a reboot.
    struct Own(Vec<libc::c_int>);

    impl Own {
        fn add(&mut self, id: libc::c_int) {
            TEST_IDS.with(|ids| ids.borrow_mut().push(id));
            self.0.push(id);
        }
    }

    impl Drop for Own {
        fn drop(&mut self) {
            for &id in &self.0 {
                if listed().is_ok_and(|l| l.iter().any(|s| s.id == id)) {
                    // SAFETY: IPC_RMID on the test's own segment, by the id it was made with.
                    unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
                }
            }
        }
    }

    /// The listing is `ipcs`'s: a segment this test makes is there with its maker, its user and nothing attached,
    /// and a run's end names it, made meanwhile, and leaves it. Harmless: the test's own segment, removed by the
    /// test.
    #[test]
    fn a_segment_is_listed_with_its_maker_and_left_while_that_runs() {
        let why = "the system's System V segments can't be listed inside a sandbox";
        if skipped_inside_for(
            "sandbox::sysv::tests::a_segment_is_listed_with_its_maker_and_left_while_that_runs",
            why,
        ) {
            return;
        }
        let (began, mut own) = (SystemTime::now(), Own(Vec::new()));
        // SAFETY: a new private segment of the test's own, removed below.
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, 4096, libc::IPC_CREAT | 0o600) };
        assert!(id >= 0, "shmget: {}", std::io::Error::last_os_error());
        own.add(id);
        let s = by_id(id).expect("listed");
        let left = after(began);
        // SAFETY: getuid can't fail.
        let uid = unsafe { libc::getuid() };
        assert_eq!(
            (s.key, s.cuid, s.cpid, s.lpid, s.nattch),
            (libc::IPC_PRIVATE, uid, std::process::id() as libc::pid_t, 0, 0)
        );
        assert_eq!(left, [id], "named");
        assert!(by_id(id).is_some(), "left");
    }

    /// A segment the user's own program made while a run ran (outside the sandbox, its maker gone at once, never
    /// attached — just like one the run left) is never removed: the run's report names it as the run's or another
    /// program's, and the user decides. Harmless: the test's own program's segment, removed by the test.
    #[test]
    fn a_segment_the_users_own_program_made_during_a_run_is_never_removed() {
        if skipped_inside_for(
            "sandbox::sysv::tests::a_segment_the_users_own_program_made_during_a_run_is_never_removed",
            "the system's System V segments can't be listed inside a sandbox",
        ) {
            return;
        }
        let s = crate::scratch::Scratch::new("sb-sysv-own");
        let root = crate::paths::real(s.path()).unwrap();
        let p = Policy::for_project(&Env { root, ..Env::default() });
        let mut own = Own(Vec::new());
        let ended = after_run(Some(&p));
        let script = "use IPC::SysV qw(IPC_PRIVATE IPC_CREAT S_IRUSR S_IWUSR); \
                      print shmget(IPC_PRIVATE, 4096, IPC_CREAT|S_IRUSR|S_IWUSR) // die \"shmget: $!\\n\"";
        let made = std::process::Command::new("/usr/bin/perl").args(["-e", script]).output().unwrap();
        let id: libc::c_int = String::from_utf8_lossy(&made.stdout).trim().parse().expect("its id");
        own.add(id);
        let end = ended();
        assert!(by_id(id).is_some(), "never removed");
        assert_eq!(end.sysv, [id], "named");
        let report = end.sysv_said().unwrap_or_default();
        assert!(
            report.contains("another program of the user's") && report.contains("removes none"),
            "{report}"
        );
    }

    /// For real (frictions 56): a sandboxed run makes two segments — Seatbelt lets it — can't remove them, and
    /// ends; its end removes neither (nothing ties them to the run) and its report names both with the `ipcrm`
    /// that frees them. Harmless: the test's run's own segments, named to `after` by their ids, removed by the
    /// test however it ends.
    #[test]
    fn the_segments_a_run_left_are_said_with_how_to_free_them() {
        if skipped_inside_for(
            "sandbox::sysv::tests::the_segments_a_run_left_are_said_with_how_to_free_them",
            "Seatbelt can't start inside a sandbox",
        ) {
            return;
        }
        let s = crate::scratch::Scratch::new("sb-sysv");
        let root = crate::paths::real(s.path()).unwrap();
        let p = Policy::for_project(&Env { root: root.clone(), ..Env::default() });
        let key = own_key(2);
        let mut own = Own(Vec::new());
        let script = format!(
            "use IPC::SysV qw(IPC_PRIVATE IPC_CREAT IPC_RMID S_IRUSR S_IWUSR); for my $k (IPC_PRIVATE, {key}) {{ \
             my $id = shmget($k, 4096, IPC_CREAT|S_IRUSR|S_IWUSR) // die \"shmget: $!\\n\"; \
             shmctl($id, IPC_RMID, 0) and die \"removed\\n\"; print \"$id \" }}"
        );
        let ended = after_run(Some(&p));
        let ran =
            command(Some(&p), "/usr/bin/perl", &["-e".into(), script]).current_dir(&root).output().unwrap();
        let said = String::from_utf8_lossy(&ran.stderr);
        let ids: Vec<libc::c_int> =
            String::from_utf8_lossy(&ran.stdout).split_whitespace().filter_map(|i| i.parse().ok()).collect();
        assert_eq!(ids.len(), 2, "both made, their removal refused: {said}");
        ids.iter().for_each(|&id| own.add(id));
        assert!(ids.iter().all(|&id| by_id(id).is_some()), "there after the run");
        let end = ended();
        assert!(by_id(ids[0]).is_some() && by_id(ids[1]).is_some_and(|s| s.key == key), "both left");
        let mut named = end.sysv.clone();
        named.sort();
        let mut made = ids.clone();
        made.sort();
        assert_eq!(named, made);
        let report = end.sysv_said().unwrap_or_default();
        let free = format!("`ipcrm -m {} -m {}`", end.sysv[0], end.sysv[1]);
        assert!(
            report.contains("While it ran, 2 System V shared memory segments were made")
                && report.contains(&free),
            "{report}"
        );
    }
}
