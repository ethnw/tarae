//! Kept run homes (the packs' `run-home` with `keep` — `sandbox::kept_homes`): their stamps, and the prune that
//! takes the ones nobody needs any more (`target/sb/kept-homes-plan.md` §3, §5).
//!
//! Each kept home `run-homes/<tool>/<key>` has a stamp beside it, `<key>.stamp`, in tarae's own folder (runs
//! write the home, never what holds it): the project's real path, and — its mtime — the last time a run got
//! the home. A run's policy holds the stamp under a shared `flock` while it lives (`Held`), taken before the
//! home is made, so a prune can't take a home between. At tarae's start (`prune_now`, a worker) a home goes
//! when no run has had it for `llm.sandbox-kept-days`, or a newer one of the same tool for the same project
//! replaced it a day ago and more (a policy changed: Gradle's are per policy), or its project is surely gone —
//! never while its stamp is held, nor while a process has its working folder or a file open in it (a daemon
//! a crashed tarae left behind holds no stamp). Nothing is gone through a link: a link where a home or a
//! tool's folder should be is removed as a link, and `remove_dir_all` doesn't follow the links inside (the
//! wrapper's distributions point into the user's own Gradle home).

use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A kept home's stamp held shared (`stamp`) while a clone of it lives — what `prune` asks first. Compared and
/// shown as nothing: a policy is the same whoever holds what.
#[derive(Clone, Default)]
pub struct Held(Option<Arc<File>>);

impl std::fmt::Debug for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Held" } else { "Held(no)" })
    }
}

impl PartialEq for Held {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Held {}

/// A replaced home's grace: one a policy just left may come back (a port opened, then closed again).
const REPLACED_AFTER: Duration = Duration::from_secs(24 * 3600);

/// `home`'s stamp: `<home>.stamp` beside it.
fn stamp_of(home: &Path) -> Option<PathBuf> {
    let name = home.file_name()?.to_string_lossy();
    Some(home.with_file_name(format!("{name}.stamp")))
}

/// Opens (makes) `path` through no link, only tarae's (0600).
fn open_stamp(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// `flock` on `f` — Err(WouldBlock) when `wait` is off and another holds it.
fn lock(f: &File, how: libc::c_int, wait: bool) -> io::Result<()> {
    let how = if wait { how } else { how | libc::LOCK_NB };
    // SAFETY: an open file of ours.
    match unsafe { libc::flock(f.as_raw_fd(), how) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Run of the project at `root` gets kept `home`: its stamp made (what holds it too, only tarae's), held shared,
/// and written — the project, the time. Waits while a prune holds it (a moment); a stamp a prune removed
/// meanwhile is made again. Blocks (a worker's).
pub fn stamp(home: &Path, root: &Path) -> io::Result<Held> {
    use std::os::unix::fs::DirBuilderExt;
    let path = stamp_of(home).ok_or_else(|| io::Error::other("no name"))?;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    for _ in 0..3 {
        let mut f = open_stamp(&path)?;
        lock(&f, libc::LOCK_SH, true)?;
        // (the one there now — not one a prune removed while this waited)
        let here = std::fs::symlink_metadata(&path).map(|m| (m.dev(), m.ino()));
        let held = f.metadata().map(|m| (m.dev(), m.ino()));
        if here.is_ok() && here.ok() == held.ok() {
            f.set_len(0)?;
            writeln!(f, "{}", root.display())?;
            return Ok(Held(Some(Arc::new(f))));
        }
    }
    Err(io::Error::other(format!("{}: taken away again and again", path.display())))
}

/// Why a kept home went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// No run had it for the days asked.
    Unused,
    /// A newer one of the same tool for the same project (a policy changed), a day since.
    Replaced,
    /// Its project isn't there any more.
    ProjectGone,
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Why::Unused => "unused",
            Why::Replaced => "replaced",
            Why::ProjectGone => "its project is gone",
        })
    }
}

/// A kept home as `prune` finds it.
struct Found {
    home: PathBuf,
    project: Option<PathBuf>,
    used: SystemTime,
}

/// The kept homes in `root` (`run-homes`, real) that go: no run had one for `unused`, a newer one of the same
/// tool for the same project replaced it (and it's a day unused), or its project is gone — but those whose
/// stamp is held, or that `in_use` says a process is in. Removed, through no link: (the home, why, the bytes
/// it held). Links where a tool's folder or a home should be are removed as links. Blocks (a worker's).
pub fn prune(
    root: &Path,
    now: SystemTime,
    unused: Duration,
    in_use: impl Fn(&Path) -> bool,
) -> Vec<(PathBuf, Why, u64)> {
    let mut out = Vec::new();
    if !std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) {
        return out;
    }
    for tool in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let Ok(t) = tool.file_type() else { continue };
        if t.is_symlink() {
            let _ = std::fs::remove_file(tool.path());
            continue;
        }
        if t.is_dir() {
            out.extend(prune_tool(&tool.path(), now, unused, &in_use));
        }
    }
    out
}

/// `prune` in one tool's folder.
fn prune_tool(
    dir: &Path,
    now: SystemTime,
    unused: Duration,
    in_use: &impl Fn(&Path) -> bool,
) -> Vec<(PathBuf, Why, u64)> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let (path, Ok(t)) = (e.path(), e.file_type()) else { continue };
        if path.extension().is_some_and(|x| x == "stamp") {
            continue;
        }
        if t.is_symlink() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !t.is_dir() {
            continue;
        }
        let stamp = stamp_of(&path).and_then(|s| std::fs::symlink_metadata(&s).ok().map(|m| (s, m)));
        let stamp = stamp.filter(|(_, m)| m.is_file());
        let project = stamp.as_ref().and_then(|(s, _)| {
            let text = std::fs::read_to_string(s).ok()?;
            Some(PathBuf::from(text.lines().next()?)).filter(|p| p.is_absolute())
        });
        // (a home from before stamps: its folder's age)
        let used = match &stamp {
            Some((_, m)) => m.modified(),
            None => e.metadata().and_then(|m| m.modified()),
        };
        found.push(Found { home: path, project, used: used.unwrap_or(now) });
    }
    let age = |f: &Found| now.duration_since(f.used).unwrap_or_default();
    let mut out = Vec::new();
    for f in &found {
        let gone = f.project.as_ref().is_some_and(|p| {
            std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        });
        let newer =
            |o: &Found| o.home != f.home && o.project.is_some() && o.project == f.project && o.used > f.used;
        let why = if gone {
            Why::ProjectGone
        } else if age(f) > unused {
            Why::Unused
        } else if age(f) > REPLACED_AFTER && found.iter().any(newer) {
            Why::Replaced
        } else {
            continue;
        };
        // In use: its stamp held (a run going, or about to), or a process in it
        let Some(stamp) = stamp_of(&f.home) else { continue };
        let had = std::fs::symlink_metadata(&stamp).is_ok();
        let Ok(held) = open_stamp(&stamp) else { continue };
        if lock(&held, libc::LOCK_EX, false).is_err() || in_use(&f.home) {
            // (one made only to lock with goes again: a home's age is its stamp's)
            if !had {
                let _ = std::fs::remove_file(&stamp);
            }
            continue;
        }
        let bytes = size(&f.home);
        if std::fs::remove_dir_all(&f.home).is_ok() {
            let _ = std::fs::remove_file(&stamp);
            out.push((f.home.clone(), why, bytes));
        }
        drop(held);
    }
    out
}

/// The bytes the files under `dir` hold, links not followed.
fn size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            match m.is_dir() {
                true => todo.push(e.path()),
                false => total += m.len(),
            }
        }
    }
    total
}

/// Whether a process has its working folder in `home`, or a file open in it (where that's told:
/// `procs::open_files`).
pub fn process_inside(home: &Path) -> bool {
    crate::procs::all().into_iter().any(|pid| {
        crate::procs::cwd(pid).is_some_and(|c| c.starts_with(home))
            || crate::procs::open_files(pid).iter().any(|f| f.starts_with(home))
    })
}

/// The prune at tarae's start, for this user's kept homes (`sandbox::kept_root`), `days` unused (the setting) —
/// what went said in the log. Blocks (a worker's).
pub fn prune_now(days: u64) {
    let here = crate::sandbox::Here::now();
    let home = here.path("HOME").and_then(|h| std::fs::canonicalize(h).ok());
    let Some(root) = crate::sandbox::kept_root(home.as_deref(), &here) else { return };
    let unused = Duration::from_secs(days.saturating_mul(24 * 3600));
    for (home, why, bytes) in prune(&root, SystemTime::now(), unused, process_inside) {
        let mb = bytes as f64 / 1_048_576.0;
        let line = format!("pruned the kept run home {} ({why}, {mb:.1} MB)", home.display());
        crate::log::line("sandbox", "info", &line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool's folder in a test's own `run-homes`, a home in it with a marker file, its stamp naming `project`
    /// and dated `ago` before `now`.
    fn home_in(root: &Path, key: &str, project: &Path, now: SystemTime, ago: Duration) -> PathBuf {
        let home = root.join("tool").join(key);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("marker"), "x").unwrap();
        drop(stamp(&home, project).unwrap());
        let f = File::options().write(true).open(stamp_of(&home).unwrap()).unwrap();
        f.set_modified(now - ago).unwrap();
        home
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// What goes and what stays: unused past the days asked; replaced by a newer home of the same project a day
    /// since (not sooner); its project gone; but never one whose stamp is held, nor one a process is in. A
    /// link where a home is goes as a link, what it pointed at stays; a link inside a home isn't followed. (All
    /// in the test's own temp folder.)
    #[test]
    fn what_a_prune_takes_and_what_it_leaves() {
        let t = std::env::temp_dir().join(format!("tarae-runhomes-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let (root, project, gone) = (t.join("run-homes"), t.join("p"), t.join("gone"));
        let outside = t.join("outside");
        for d in [&project, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(outside.join("keep-me"), "x").unwrap();
        let now = SystemTime::now();
        let old = home_in(&root, "old", &project, now, 20 * DAY);
        let recent = home_in(&root, "recent", &project, now, DAY / 2);
        let replaced = home_in(&root, "replaced", &project, now, 2 * DAY);
        let replaced_soon = home_in(&root, "replaced-soon", &t.join("q"), now, DAY / 2);
        home_in(&root, "newer-q", &t.join("q"), now, Duration::ZERO);
        let orphan = home_in(&root, "orphan", &gone, now, Duration::ZERO);
        let held_home = home_in(&root, "held", &gone, now, Duration::ZERO);
        let held = stamp(&held_home, &gone).unwrap();
        let busy = home_in(&root, "busy", &gone, now, Duration::ZERO);
        // (a link inside a home, to a folder outside: not followed)
        std::os::unix::fs::symlink(&outside, old.join("dists")).unwrap();
        // (a link where a home would be, to the same folder: removed as a link)
        std::os::unix::fs::symlink(&outside, root.join("tool/linked")).unwrap();
        std::fs::create_dir_all(t.join("q")).unwrap();
        // (a process another test forks shares a stamp's lock until its `exec` closes it: a home only looks held
        // then, and goes on a later look — looked again while one that should go is there)
        let mut went = Vec::new();
        for _ in 0..100 {
            went.extend(prune(&root, now, 14 * DAY, |h| h == busy));
            if [&old, &orphan, &replaced].iter().all(|h| !h.exists()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        went.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<(String, Why)> =
            went.iter().map(|(h, w, _)| (h.file_name().unwrap().to_string_lossy().into(), *w)).collect();
        assert_eq!(
            names,
            [
                ("old".into(), Why::Unused),
                ("orphan".into(), Why::ProjectGone),
                ("replaced".into(), Why::Replaced),
            ]
        );
        assert!(went.iter().all(|(_, _, bytes)| *bytes >= 1), "{went:?}");
        assert!(!old.exists() && !stamp_of(&old).unwrap().exists() && !orphan.exists() && !replaced.exists());
        for kept in [&recent, &replaced_soon, &held_home, &busy] {
            assert!(kept.join("marker").exists(), "{} kept", kept.display());
        }
        assert!(std::fs::symlink_metadata(root.join("tool/linked")).is_err(), "the link went");
        assert!(outside.join("keep-me").exists(), "nothing gone through a link");
        // The stamp no longer held: it goes too
        drop(held);
        let mut went = Vec::new();
        for _ in 0..100 {
            went.extend(prune(&root, now, 14 * DAY, |_| false));
            if !held_home.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(went.iter().any(|(h, w, _)| *h == held_home && *w == Why::ProjectGone), "{went:?}");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A process whose working folder is in a home keeps it (a `sleep` of the test's own, in a home in its temp
    /// folder); none in it, nothing says so.
    #[test]
    fn a_process_in_a_home_keeps_it() {
        let t = std::env::temp_dir().join(format!("tarae-runhomes-in-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let (home, other) = (t.join("home"), t.join("other"));
        for d in [&home, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let home = std::fs::canonicalize(&home).unwrap();
        let mut child = std::process::Command::new("sleep").arg("30").current_dir(&home).spawn().unwrap();
        // (its working folder is told once it's started: looked for a moment)
        let seen = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            process_inside(&home)
        });
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen, "the sleep in it");
        let other = std::fs::canonicalize(&other).unwrap();
        assert!(!process_inside(&other));
        // Linux: a file held open in it keeps it too, the process working elsewhere (a shell of the test's own
        // holding a file in its temp folder)
        if cfg!(target_os = "linux") {
            std::fs::write(other.join("held"), "").unwrap();
            let script = format!("exec 3< '{}'; exec sleep 30", other.join("held").display());
            let mut child =
                std::process::Command::new("sh").args(["-c", &script]).current_dir(&t).spawn().unwrap();
            let seen = (0..100).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                process_inside(&other)
            });
            let _ = child.kill();
            let _ = child.wait();
            assert!(seen, "a file open in it");
        }
        std::fs::remove_dir_all(&t).ok();
    }

    /// A stamp is held while its `Held` lives, the project written in it; a prune can't take its lock then.
    #[test]
    fn a_held_stamp_says_its_project() {
        let t = std::env::temp_dir().join(format!("tarae-runhomes-stamp-{}", std::process::id()));
        std::fs::remove_dir_all(&t).ok();
        let home = t.join("tool/k");
        let held = stamp(&home, Path::new("/w/p")).unwrap();
        let path = stamp_of(&home).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/w/p\n");
        let other = open_stamp(&path).unwrap();
        assert!(lock(&other, libc::LOCK_EX, false).is_err(), "held");
        let again = stamp(&home, Path::new("/w/p")).unwrap();
        drop((held, again));
        // (a process another test forks shares the lock until its `exec` closes it: asked again while that's so)
        let free = (0..100).any(|_| {
            let ok = lock(&other, libc::LOCK_EX | libc::LOCK_NB, true).is_ok();
            if !ok {
                std::thread::sleep(Duration::from_millis(20));
            }
            ok
        });
        assert!(free, "let go");
        std::fs::remove_dir_all(&t).ok();
    }
}
