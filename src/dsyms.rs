//! The symbols of the tarae `:restart` runs, kept (macOS). A release build's `tarae.dSYM` (`split-debuginfo =
//! "packed"`, Cargo.toml; the binary itself stripped) is a link into cargo's build-dir, which the next build —
//! or `cargo clean -p tarae`, or the end of the checkout's session — replaces or removes. Before the exec,
//! `keep` clones it to `target/dsyms/<UUID>.dSYM` (the binary's `LC_UUID`, which `sample`'s Binary Images line
//! shows), the newest `KEEP` kept: a running tarae can be symbolized after the next build (CLAUDE.md,
//! "Symbolizing a running tarae"). No dSYM beside the binary (Linux, a build without debug info): nothing. A
//! binary that isn't a cargo build's (`~/bin/tarae`): nothing either — `target/dsyms` is cargo's target
//! folder's, and pruning a folder of someone else's would remove what isn't ours.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How many dSYMs `target/dsyms` keeps — the newest by mtime.
pub const KEEP: usize = 5;

/// The `LC_UUID` of the thin 64-bit Mach-O at `path` (a binary, or the DWARF file in a dSYM), as dwarfdump
/// and sample print it: `108B07D2-A5BD-3AF1-970B-8A9284AFE5CD`.
pub fn uuid(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    // (load commands are a few KB; more is something else)
    const CMDS_MAX: usize = 1 << 20;
    let name = path.display();
    let not = || format!("{name}: not a 64-bit Mach-O");
    let mut f = std::fs::File::open(path).map_err(|e| format!("{name}: {e}"))?;
    // (both architectures tarae builds for are little-endian)
    let word = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let mut head = [0u8; 32];
    f.read_exact(&mut head).map_err(|_| not())?;
    if word(&head, 0) != MH_MAGIC_64 {
        return Err(not());
    }
    let (count, size) = (word(&head, 16), word(&head, 20) as usize);
    if size > CMDS_MAX {
        return Err(not());
    }
    let mut cmds = vec![0u8; size];
    f.read_exact(&mut cmds).map_err(|_| format!("{name}: its load commands are cut short"))?;
    let mut at = 0;
    for _ in 0..count {
        if at + 8 > size {
            break;
        }
        let (cmd, len) = (word(&cmds, at), word(&cmds, at + 4) as usize);
        if cmd == LC_UUID && len >= 24 && at + 24 <= size {
            let hex: String = cmds[at + 8..at + 24].iter().map(|b| format!("{b:02X}")).collect();
            return Ok(format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            ));
        }
        if len < 8 {
            break;
        }
        at += len;
    }
    Err(format!("{name}: no LC_UUID"))
}

/// A dSYM kept (`keep`): where — and, when the older ones couldn't all be removed, why (the log says it on a
/// line of its own; the one kept stays kept).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub at: PathBuf,
    pub pruned: Result<(), String>,
}

/// The dSYM beside `bin` (`<bin>.dSYM`, its link followed) kept as `<target>/dsyms/<UUID>.dSYM` — a clone on
/// APFS — then the kept ones past the newest `KEEP` removed. None: no dSYM beside it. Only a cargo build's
/// (`target_dir`), and the dSYM must be `bin`'s own (the same UUID). On a worker: it reads and writes the disk.
pub fn keep(bin: &Path) -> Result<Option<Kept>, String> {
    let mut name = bin.file_name().ok_or_else(|| format!("{}: no file name", bin.display()))?.to_os_string();
    name.push(".dSYM");
    let beside = bin.with_file_name(name);
    if std::fs::symlink_metadata(&beside).is_err() {
        return Ok(None);
    }
    let target = target_dir(bin)
        .ok_or_else(|| format!("{} isn't a cargo build's (no target folder around it)", bin.display()))?;
    let said = |e: std::io::Error, p: &Path| format!("{}: {e}", p.display());
    // (a link to a build-dir's dSYM that's gone: said)
    let real = std::fs::canonicalize(&beside).map_err(|e| said(e, &beside))?;
    let id = uuid(bin)?;
    let dwarf = real.join("Contents/Resources/DWARF");
    let file = std::fs::read_dir(&dwarf)
        .map_err(|e| said(e, &dwarf))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
        .ok_or_else(|| format!("{}: no DWARF file in it", beside.display()))?;
    let theirs = uuid(&file)?;
    if theirs != id {
        return Err(format!(
            "{} is another build's (its UUID {theirs}, the binary's {id})",
            beside.display()
        ));
    }
    let dir = target.join("dsyms");
    std::fs::create_dir_all(&dir).map_err(|e| said(e, &dir))?;
    let kept = dir.join(format!("{id}.dSYM"));
    if !kept.exists() {
        // (cloned beside, then renamed in: never a half-made one under the UUID; the part is this process's
        // own, so another tarae keeping the same one meanwhile neither removes it nor has it removed)
        let part = dir.join(part_name(&id, std::process::id()));
        let _ = std::fs::remove_dir_all(&part);
        clone_tree(&real, &part)?;
        rename_in(&part, &kept)?;
    }
    // Newest by mtime: the one the tarae about to run needs (a clone keeps the source's mtime)
    std::fs::File::open(&kept).and_then(|f| f.set_modified(SystemTime::now())).map_err(|e| said(e, &kept))?;
    let pruned = prune(&dir, &kept, KEEP).map(drop);
    Ok(Some(Kept { at: kept, pruned }))
}

/// The clone being made of `<id>.dSYM` by process `pid`: hidden (`prune` passes it by) and its own.
fn part_name(id: &str, pid: u32) -> String {
    format!(".{id}.dSYM.{pid}.part")
}

/// `part` renamed to `kept`. Another process that kept the same one first (`kept` there now, a folder — a
/// rename onto it fails as it isn't empty): theirs stays, `part` goes.
fn rename_in(part: &Path, kept: &Path) -> Result<(), String> {
    match std::fs::rename(part, kept) {
        Ok(()) => Ok(()),
        Err(_) if kept.is_dir() => {
            let _ = std::fs::remove_dir_all(part);
            Ok(())
        }
        Err(e) => Err(format!("{}: {e}", kept.display())),
    }
}

/// The target folder `bin` was built into, when it's a cargo build's: its folder a profile folder (cargo's
/// `.cargo-lock` in it) in a target folder (cargo's `CACHEDIR.TAG`) — `target/<profile>` or
/// `target/<triple>/<profile>`.
fn target_dir(bin: &Path) -> Option<&Path> {
    let profile = bin.parent()?;
    if !profile.join(".cargo-lock").is_file() {
        return None;
    }
    profile.ancestors().skip(1).take(2).find(|d| d.join("CACHEDIR.TAG").is_file())
}

/// The kept dSYMs in `dir` past the newest `keep` (by mtime) removed — never `now`, the one just kept, which
/// counts as the newest. What it removed; one it can't remove (a file of that name) doesn't stop the rest,
/// and is said.
fn prune(dir: &Path, now: &Path, keep: usize) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut kept: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".dSYM") && !n.starts_with('.')))
        .map(|e| e.path())
        .filter(|p| p != now)
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), p))
        .collect();
    crate::sorting::by(&mut kept, |a, b| b.cmp(a));
    let mut failed = Vec::new();
    let gone: Vec<PathBuf> = kept
        .into_iter()
        .skip(keep.saturating_sub(1))
        .map(|(_, p)| p)
        .filter(|p| match std::fs::remove_dir_all(p) {
            Ok(()) => true,
            Err(e) => {
                failed.push(format!("{}: {e}", p.display()));
                false
            }
        })
        .collect();
    match failed.is_empty() {
        true => Ok(gone),
        false => Err(failed.join("; ")),
    }
}

/// `from` (a folder) made again at `to`: a clone where the file system makes them (APFS: no data copied,
/// nothing to wait for), else copied.
#[cfg(target_os = "macos")]
fn clone_tree(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|e| e.to_string());
    let (f, t) = (c(from)?, c(to)?);
    // SAFETY: two NUL-terminated paths that live through the call; clonefile only reads them.
    if unsafe { libc::clonefile(f.as_ptr(), t.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    // (another volume, or no clones there: a half-made clone goes first)
    let _ = std::fs::remove_dir_all(to);
    copy_tree(from, to)
}

#[cfg(not(target_os = "macos"))]
fn clone_tree(from: &Path, to: &Path) -> Result<(), String> {
    copy_tree(from, to)
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let said = |e: std::io::Error, p: &Path| format!("{}: {e}", p.display());
    std::fs::create_dir_all(to).map_err(|e| said(e, to))?;
    for e in std::fs::read_dir(from).map_err(|e| said(e, from))?.flatten() {
        let (path, dest) = (e.path(), to.join(e.file_name()));
        match e.file_type() {
            Ok(t) if t.is_dir() => copy_tree(&path, &dest)?,
            Ok(t) if t.is_file() => {
                std::fs::copy(&path, &dest).map_err(|e| said(e, &path))?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tarae-dsyms-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// A thin 64-bit Mach-O's header and load commands (an `LC_SYMTAB`, then an `LC_UUID` of `id` unless None).
    fn macho(id: Option<[u8; 16]>) -> Vec<u8> {
        let mut cmds = Vec::new();
        cmds.extend(2u32.to_le_bytes()); // LC_SYMTAB
        cmds.extend(24u32.to_le_bytes());
        cmds.extend([0; 16]);
        if let Some(id) = id {
            cmds.extend(0x1bu32.to_le_bytes()); // LC_UUID
            cmds.extend(24u32.to_le_bytes());
            cmds.extend(id);
        }
        let n: u32 = if id.is_some() { 2 } else { 1 };
        let mut b = Vec::new();
        for w in [0xfeed_facf, 0x0100_000c, 0, 2, n, cmds.len() as u32, 0, 0] {
            b.extend(u32::to_le_bytes(w));
        }
        b.extend(cmds);
        b.extend([0xAA; 64]); // (the rest of a binary)
        b
    }

    const A: [u8; 16] =
        [0x10, 0x8B, 0x07, 0xD2, 0xA5, 0xBD, 0x3A, 0xF1, 0x97, 0x0B, 0x8A, 0x92, 0x84, 0xAF, 0xE5, 0xCD];
    const A_SAID: &str = "108B07D2-A5BD-3AF1-970B-8A9284AFE5CD";

    fn id(n: u8) -> [u8; 16] {
        [n; 16]
    }

    /// A release build as cargo leaves it: `target/release/tarae` (a Mach-O of `bin`'s UUID; cargo's
    /// `.cargo-lock` beside it, its `CACHEDIR.TAG` in `target`) and `target/release/tarae.dSYM`, a link to the
    /// dSYM in the build-dir (its DWARF file of `dwarf`'s UUID). The build-dir's dSYM.
    fn build(dir: &Path, bin: [u8; 16], dwarf: [u8; 16]) -> PathBuf {
        build_in(dir, &dir.join("target/release"), bin, dwarf)
    }

    /// `build`, its binary in `release`, a folder under `dir/target` (`target/<triple>/release`).
    fn build_in(dir: &Path, release: &Path, bin: [u8; 16], dwarf: [u8; 16]) -> PathBuf {
        let made = dir.join("build/release/deps/tarae-171a287377bc2e94.dSYM");
        dsym(&made, dwarf);
        std::fs::create_dir_all(release).unwrap();
        std::fs::write(release.join(".cargo-lock"), "").unwrap();
        std::fs::write(dir.join("target/CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55\n").ok();
        std::fs::write(release.join("tarae"), macho(Some(bin))).unwrap();
        std::fs::remove_file(release.join("tarae.dSYM")).ok();
        std::os::unix::fs::symlink(&made, release.join("tarae.dSYM")).unwrap();
        made
    }

    /// A dSYM folder at `at`, its DWARF file of `uuid`'s.
    fn dsym(at: &Path, uuid: [u8; 16]) {
        std::fs::create_dir_all(at.join("Contents/Resources/DWARF")).unwrap();
        std::fs::write(at.join("Contents/Info.plist"), "<plist/>").unwrap();
        std::fs::write(at.join("Contents/Resources/DWARF/tarae-171a287377bc2e94"), macho(Some(uuid)))
            .unwrap();
    }

    fn set_mtime(p: &Path, secs: u64) {
        let f = std::fs::File::open(p).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into())
            .collect();
        n.sort();
        n
    }

    /// The UUID comes from the binary's own load commands, written as dwarfdump and sample write it; what
    /// isn't a 64-bit Mach-O with one says so. The test binary itself has one (macOS).
    #[test]
    fn the_uuid_is_read_from_the_binary_as_dwarfdump_says_it() {
        let dir = temp("uuid");
        let f = dir.join("bin");
        std::fs::write(&f, macho(Some(A))).unwrap();
        assert_eq!(uuid(&f).as_deref(), Ok(A_SAID));
        std::fs::write(&f, "#!/bin/sh\necho tarae\n").unwrap();
        assert!(uuid(&f).is_err_and(|e| e.contains("not a 64-bit Mach-O")), "{:?}", uuid(&f));
        std::fs::write(&f, &macho(Some(A))[..40]).unwrap();
        assert!(uuid(&f).is_err(), "cut short: {:?}", uuid(&f));
        std::fs::write(&f, macho(None)).unwrap();
        assert!(uuid(&f).is_err_and(|e| e.contains("no LC_UUID")), "{:?}", uuid(&f));
        if cfg!(target_os = "macos") {
            let me = std::env::current_exe().unwrap();
            let mine = uuid(&me).unwrap();
            // (as dwarfdump reads it, where the developer tools are)
            let said = std::process::Command::new("/usr/bin/dwarfdump").arg("--uuid").arg(&me).output();
            if let Ok(out) = said
                && out.status.success()
            {
                let out = String::from_utf8_lossy(&out.stdout);
                assert!(out.starts_with(&format!("UUID: {mine} ")), "{mine} — dwarfdump: {out}");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The dSYM beside the binary (a link into the build-dir, as cargo makes it) is kept as a folder of its own
    /// under its UUID — still there once the build-dir's is gone. None beside it: nothing kept. Another build's
    /// dSYM (a UUID not the binary's): refused, nothing kept.
    #[test]
    fn the_dsym_beside_the_binary_is_kept_by_its_uuid() {
        let dir = temp("keep");
        let made = build(&dir, A, A);
        let bin = dir.join("target/release/tarae");
        let Kept { at: kept, pruned } = keep(&bin).unwrap().expect("kept");
        assert_eq!((&kept, pruned), (&dir.join(format!("target/dsyms/{A_SAID}.dSYM")), Ok(())));
        assert!(!std::fs::symlink_metadata(&kept).unwrap().file_type().is_symlink(), "a copy, not the link");
        let dwarf = "Contents/Resources/DWARF/tarae-171a287377bc2e94";
        assert_eq!(std::fs::read(kept.join(dwarf)).unwrap(), macho(Some(A)));
        assert_eq!(std::fs::read_to_string(kept.join("Contents/Info.plist")).unwrap(), "<plist/>");
        // (the next build, or `cargo clean -p tarae`, takes the build-dir's away)
        std::fs::remove_dir_all(&made).unwrap();
        assert_eq!(uuid(&kept.join(dwarf)).as_deref(), Ok(A_SAID));
        // Kept again (a :restart! onto the same binary): the same one, nothing else
        build(&dir, A, A);
        assert_eq!(keep(&bin), Ok(Some(Kept { at: kept.clone(), pruned: Ok(()) })));
        assert_eq!(names(&dir.join("target/dsyms")), [format!("{A_SAID}.dSYM")]);

        std::fs::remove_file(dir.join("target/release/tarae.dSYM")).unwrap();
        assert_eq!(keep(&bin), Ok(None), "no dSYM beside it");

        // A build for a target triple (`target/<triple>/release`): in the target folder all the same
        let triple = temp("keep-triple");
        build_in(&triple, &triple.join("target/aarch64-apple-darwin/release"), A, A);
        let k = keep(&triple.join("target/aarch64-apple-darwin/release/tarae")).unwrap().expect("kept");
        assert_eq!(k.at, triple.join(format!("target/dsyms/{A_SAID}.dSYM")));
        std::fs::remove_dir_all(&triple).ok();

        let other = temp("keep-other");
        build(&other, A, id(7));
        let e = keep(&other.join("target/release/tarae")).unwrap_err();
        assert!(e.contains("another build's") && e.contains(A_SAID), "{e}");
        assert!(!other.join("target/dsyms").join(format!("{A_SAID}.dSYM")).exists());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// Two tarae keeping the same dSYM in one target folder at once: each clones into a part of its own (its
    /// pid in the name), so neither removes the other's half-made one; the second to rename in finds the
    /// first's kept, leaves it as it is and drops its own part.
    #[test]
    fn two_keeps_at_once_dont_remove_each_others_part() {
        let dir = temp("parts");
        build(&dir, A, A);
        let dsyms = dir.join("target/dsyms");
        // (another tarae's clone under way: a part of its pid, half-made)
        let theirs = dsyms.join(part_name(A_SAID, std::process::id() + 1));
        std::fs::create_dir_all(theirs.join("Contents")).unwrap();
        std::fs::write(theirs.join("Contents/Info.plist"), "theirs").unwrap();
        let k = keep(&dir.join("target/release/tarae")).unwrap().expect("kept");
        assert_eq!(std::fs::read_to_string(theirs.join("Contents/Info.plist")).unwrap(), "theirs");
        let mine = part_name(A_SAID, std::process::id());
        let their_name = theirs.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(names(&dsyms), [their_name, format!("{A_SAID}.dSYM")]);
        assert!(!dsyms.join(&mine).exists());

        // It renames in second: the first one's kept stays, its own part goes
        std::fs::rename(&theirs, dsyms.join(&mine)).unwrap();
        assert_eq!(rename_in(&dsyms.join(&mine), &k.at), Ok(()));
        assert_eq!(names(&dsyms), [format!("{A_SAID}.dSYM")]);
        assert_eq!(std::fs::read_to_string(k.at.join("Contents/Info.plist")).unwrap(), "<plist/>");
        // (nothing kept there, and the rename fails: said)
        let e = rename_in(&dsyms.join(&mine), &dsyms.join("B.dSYM")).unwrap_err();
        assert!(e.contains("B.dSYM"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A binary that isn't a cargo build's — in a plain folder, or `~/bin` beside a `dsyms` of someone else's —
    /// with its own dSYM beside it: nothing kept, said; no `dsyms` folder made, nothing removed from one there.
    /// A profile folder with no target folder around it (no `CACHEDIR.TAG`) isn't one either.
    #[test]
    fn only_a_cargo_builds_dsym_is_kept() {
        let dir = temp("not-cargo");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("tarae"), macho(Some(A))).unwrap();
        dsym(&plain.join("tarae.dSYM"), A);
        let e = keep(&plain.join("tarae")).unwrap_err();
        assert!(e.contains("isn't a cargo build's"), "{e}");
        assert_eq!(names(&dir), ["plain"], "no dsyms folder made");

        let home = dir.join("home");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("bin/tarae"), macho(Some(A))).unwrap();
        dsym(&home.join("bin/tarae.dSYM"), A);
        let theirs: Vec<String> = (1..=7).map(|n| format!("{}.dSYM", hyphen(id(n)))).collect();
        for (i, n) in theirs.iter().enumerate() {
            std::fs::create_dir_all(home.join("dsyms").join(n)).unwrap();
            set_mtime(&home.join("dsyms").join(n), 1_000 + i as u64);
        }
        assert!(keep(&home.join("bin/tarae")).is_err_and(|e| e.contains("isn't a cargo build's")));
        assert_eq!(names(&home.join("dsyms")), theirs, "nothing removed");
        assert_eq!(names(&home.join("bin")), ["tarae", "tarae.dSYM"]);

        // (cargo's lock in its folder, but no target folder above)
        std::fs::write(home.join("bin/.cargo-lock"), "").unwrap();
        assert!(keep(&home.join("bin/tarae")).is_err_and(|e| e.contains("isn't a cargo build's")));
        assert_eq!(names(&home.join("dsyms")), theirs);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One that can't be pruned (a stray file named `X.dSYM`) doesn't undo the keeping: the dSYM is kept and
    /// said where, the prune's problem said apart — and the rest past `KEEP` still go.
    #[test]
    fn a_prune_that_fails_leaves_the_dsym_kept() {
        let dir = temp("prune-fails");
        let dsyms = dir.join("target/dsyms");
        std::fs::create_dir_all(&dsyms).unwrap();
        std::fs::write(dsyms.join("X.dSYM"), "not a folder").unwrap();
        set_mtime(&dsyms.join("X.dSYM"), 1_000);
        for n in 1..=5 {
            let d = dsyms.join(format!("{}.dSYM", hyphen(id(n))));
            std::fs::create_dir_all(&d).unwrap();
            set_mtime(&d, 1_000 + u64::from(n) * 10);
        }
        build(&dir, A, A);
        let Kept { at, pruned } = keep(&dir.join("target/release/tarae")).unwrap().expect("kept");
        assert_eq!(at, dsyms.join(format!("{A_SAID}.dSYM")));
        assert_eq!(uuid(&at.join("Contents/Resources/DWARF/tarae-171a287377bc2e94")).as_deref(), Ok(A_SAID));
        assert!(pruned.as_ref().is_err_and(|e| e.contains("X.dSYM")), "{pruned:?}");
        // (X, the oldest, stays — the next oldest went past it)
        let said = |n: u8| format!("{}.dSYM", hyphen(id(n)));
        let mut want = vec![said(2), said(3), said(4), said(5), format!("{A_SAID}.dSYM"), "X.dSYM".into()];
        want.sort();
        assert_eq!(names(&dsyms), want);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Past `KEEP`, the oldest by mtime go — never the one just kept, whatever its own mtime; what isn't a
    /// kept dSYM stays.
    #[test]
    fn the_newest_five_are_kept_by_mtime() {
        let dir = temp("prune");
        for i in 0..7u64 {
            let d = dir.join(format!("{}.dSYM", ["G", "B", "E", "A", "F", "C", "D"][i as usize]));
            std::fs::create_dir_all(d.join("Contents")).unwrap();
            set_mtime(&d, 1_000 + i * 10);
        }
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        std::fs::create_dir_all(dir.join(".X.dSYM.part")).unwrap();
        set_mtime(&dir.join(".X.dSYM.part"), 1);
        // (the one just kept, older than all of them by its mtime: it counts as the newest)
        let now = dir.join("G.dSYM");
        let mut gone = prune(&dir, &now, KEEP).unwrap();
        gone.sort();
        assert_eq!(gone, [dir.join("B.dSYM"), dir.join("E.dSYM")]);
        assert_eq!(
            names(&dir),
            [".X.dSYM.part", "A.dSYM", "C.dSYM", "D.dSYM", "F.dSYM", "G.dSYM", "notes.txt"]
        );

        // Through `keep`: six builds, the first kept again last — five, the first among them
        let tree = temp("prune-keep");
        let bin = tree.join("target/release/tarae");
        for n in [1, 2, 3, 4, 5, 6, 1] {
            build(&tree, id(n), id(n));
            keep(&bin).unwrap();
            let kept = std::fs::read_dir(tree.join("target/dsyms")).unwrap().count();
            assert!(kept <= KEEP, "{kept} kept");
            // (mtimes a second apart: some file systems keep whole seconds)
            for e in std::fs::read_dir(tree.join("target/dsyms")).unwrap().flatten() {
                let t = e.metadata().unwrap().modified().unwrap();
                std::fs::File::open(e.path()).unwrap().set_modified(t - Duration::from_secs(1)).unwrap();
            }
        }
        let said = |n: u8| format!("{}.dSYM", hyphen(id(n)));
        let mut want = vec![said(1), said(3), said(4), said(5), said(6)];
        want.sort();
        assert_eq!(names(&tree.join("target/dsyms")), want);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&tree).ok();
    }

    fn hyphen(b: [u8; 16]) -> String {
        let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
        format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
    }
}
