//! tarae's AppArmor profile (step 6h, `target/sb/step6-checks-notes.md` §5): on Ubuntu with its restriction on
//! (`kernel.apparmor_restrict_unprivileged_userns = 1`) a user namespace made by an unconfined program holds no
//! capability, so a session keeper gets none of its own (tier host). A profile naming tarae's binary by path with
//! `userns,` lifts that for tarae alone — measured on 6.8, 7.0 and 7.3 (t5). It must name a **root-owned** path:
//! whatever can write the path gets the permission. So the `.deb` ships it for `/usr/bin/tarae`
//! (`packaging/apparmor/tarae` — `profile` for that path, pinned by a test), and for a tarae at another
//! root-owned place tarae prints the `sudo` step (`step`); for one anywhere else (`cargo install`'s, a user's
//! `bin`) only why not. tarae never writes the profile itself.
//!
//! The text is plain and built everywhere (its tests run on macOS too); only Linux's `tier.rs` says it.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

/// Where the `.deb` puts tarae, and its profile.
pub const PACKAGED: &str = "/usr/bin/tarae";
pub const PACKAGED_PROFILE: &str = "/etc/apparmor.d/tarae";

/// The profile for a tarae at `path` (a real path; `PACKAGED`'s is the packaged file, byte for byte): Ubuntu's
/// documented shape for a program that makes user namespaces — `userns`, otherwise unconfined.
pub fn profile(path: &str) -> String {
    let local = if path == PACKAGED { "tarae".to_string() } else { file_name(path) };
    format!(
        "# tarae: user namespaces for {path} (its sandbox) under the Ubuntu restriction, otherwise unconfined\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         {path} flags=(unconfined) {{\n  \
           userns,\n\
         \n  \
           include if exists <local/{local}>\n\
         }}\n"
    )
}

/// The profile's file name for a tarae at `path` that isn't the packaged one — AppArmor's own way of naming
/// one after its program (`/usr/local/bin/tarae` → `usr.local.bin.tarae`), never the package's `tarae`.
fn file_name(path: &str) -> String {
    path.trim_start_matches('/').replace('/', ".")
}

/// tarae's binary and what its place allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// Its real path (`/proc/self/exe`'s: what AppArmor attaches by).
    pub path: PathBuf,
    /// Only root can change it: the file and every folder above it root's, none writable by its group or
    /// others — else why not.
    pub root_owned: Result<(), String>,
    /// The `.deb`'s profile file is there (installed, not loaded: a container's install, AppArmor off then).
    pub packaged: bool,
}

/// The place of the binary at `exe`, as this file system has it.
pub fn place_of(exe: &Path) -> Place {
    use std::os::unix::fs::MetadataExt;
    let mut chain = Vec::new();
    let mut at = Some(exe);
    while let Some(p) = at {
        match std::fs::symlink_metadata(p) {
            Ok(m) => chain.push(Entry {
                path: p.to_path_buf(),
                uid: m.uid(),
                mode: m.mode() & 0o7777,
                link: m.file_type().is_symlink(),
            }),
            Err(e) => {
                let why = format!("{}: {e}", p.display());
                return Place { path: exe.to_path_buf(), root_owned: Err(why), packaged: false };
            }
        }
        at = p.parent();
    }
    let packaged = exe == Path::new(PACKAGED) && Path::new(PACKAGED_PROFILE).is_file();
    Place { path: exe.to_path_buf(), root_owned: root_only(&chain), packaged }
}

/// One place on the way to the binary, as `root_only` judges it.
struct Entry {
    path: PathBuf,
    uid: u32,
    /// Its permission bits.
    mode: u32,
    link: bool,
}

/// Whether only root can change what `chain` lists — the binary first, then each folder above it. A link anywhere
/// isn't (`place_of` is given a real path: one there means the path moved under it).
fn root_only(chain: &[Entry]) -> Result<(), String> {
    for Entry { path, uid, mode, link } in chain {
        let shown = path.display();
        if *link {
            return Err(format!("{shown} is a link"));
        }
        if *uid != 0 {
            return Err(format!("{shown} is uid {uid}'s"));
        }
        if mode & 0o022 != 0 {
            return Err(format!("{shown} is writable by others than root (mode {mode:o})"));
        }
    }
    Ok(())
}

/// The characters a path in the step may hold: none a profile reads as a pattern (`*`, `{`, `[` …), none the
/// shell or `printf` would read (quotes, `%`, `\`, `$`), no space.
fn plain(path: &str) -> bool {
    path.starts_with('/') && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b))
}

/// The `sudo` step that gives the tarae at `place` its profile — None where no profile should name it (not
/// root-owned) or its path can't be written into one plainly; the caller says why (`hint`).
pub fn step(place: &Place) -> Option<String> {
    let path = place.path.to_str().filter(|p| plain(p))?;
    place.root_owned.as_ref().ok()?;
    if place.packaged {
        return Some(format!("sudo apparmor_parser -r {PACKAGED_PROFILE}"));
    }
    let file = format!("/etc/apparmor.d/{}", file_name(path));
    // (inside single quotes and printf's format: no quote, `%` or `\` of its own)
    let text = Some(profile(path)).filter(|t| !t.contains(['\'', '%', '\\']))?.replace('\n', "\\n");
    Some(format!("printf '{text}' | sudo tee {file} >/dev/null && sudo apparmor_parser -r {file}"))
}

/// The hint (a toast once, and the tier's line) where Ubuntu's restriction keeps a keeper's namespaces from
/// tarae and no profile names it: runs in `tier`, why — and the step that gives them the session's namespaces,
/// or why there's none.
pub fn hint(tier: &str, place: &Place) -> String {
    let why = format!(
        "Claude's commands run in tier {tier} here (no namespaces of their own): Ubuntu lets a program make \
         user namespaces only through an AppArmor profile that names it, and none names tarae"
    );
    let path = place.path.display();
    match (step(place), &place.root_owned) {
        (Some(step), _) => format!(
            "{why}. To give them namespaces of their own, run once, then restart tarae — your terminal and tools keep user \
             namespaces, as VS Code's do; Claude's and plugins' commands don't: {step}"
        ),
        (None, Err(not)) => format!(
            "{why}. A profile is safe only for a tarae only root can replace, and {path} isn't ({not}) — install \
             tarae at a root-owned path (its .deb puts it at {PACKAGED}), start that one, and tarae gives the \
             step there"
        ),
        (None, Ok(())) => format!(
            "{why}. Its path, {path}, has characters a profile would read as a pattern — install tarae at \
             {PACKAGED} (its .deb), and tarae gives the step there"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(path: &str, root_owned: Result<(), String>) -> Place {
        Place { path: PathBuf::from(path), root_owned, packaged: false }
    }

    /// The `.deb` ships exactly what tarae would print for its path: one profile, two ways in.
    #[test]
    fn the_packaged_profile_is_the_one_tarae_writes() {
        assert_eq!(profile(PACKAGED), include_str!("../../packaging/apparmor/tarae"));
    }

    /// Only a path nobody but root can change gets a profile: a binary or any folder above it that's another
    /// user's, writable by a group or by others, or a link, would hand `userns` to whatever is put there (the
    /// restriction's own hole). Each is said with the place that fails.
    #[test]
    fn only_a_root_owned_place_gets_a_profile() {
        let ok = |p: &str, mode| Entry { path: PathBuf::from(p), uid: 0, mode, link: false };
        let usr_bin = [ok("/usr/bin/tarae", 0o755), ok("/usr/bin", 0o755), ok("/", 0o755)];
        assert_eq!(root_only(&usr_bin), Ok(()));
        let cargo = [Entry { uid: 1000, ..ok("/home/u/.cargo/bin/tarae", 0o755) }];
        assert!(root_only(&cargo).is_err_and(|w| w.contains("uid 1000's")));
        // (Debian's /usr/local: root's, but group staff writes it)
        let staff = [ok("/usr/local/bin/tarae", 0o755), ok("/usr/local/bin", 0o2775)];
        assert!(
            root_only(&staff).is_err_and(|w| w.contains("/usr/local/bin is writable") && w.contains("2775"))
        );
        let tmp = [ok("/tmp/tarae", 0o755), ok("/tmp", 0o1777)];
        assert!(root_only(&tmp).is_err(), "a sticky, world-writable folder");
        let link = [Entry { link: true, ..ok("/usr/bin/tarae", 0o777) }];
        assert!(root_only(&link).is_err_and(|w| w.contains("a link")));
        // (this file system: a system folder's own binary, by its real path — root's on macOS and Linux alike)
        assert_eq!(place_of(&std::fs::canonicalize("/bin/sh").unwrap()).root_owned, Ok(()));
        let here = std::env::temp_dir().join(format!("tarae-apparmor-{}", std::process::id()));
        std::fs::write(&here, "").unwrap();
        let mine = place_of(&std::fs::canonicalize(&here).unwrap());
        std::fs::remove_file(&here).ok();
        assert!(mine.root_owned.is_err(), "a file of this user's: {mine:?}");
    }

    /// The step writes the very profile for the binary's real path and loads it — the package's file for its
    /// own path, a file of its own for any other root-owned one (never over the package's). None for a place
    /// that isn't root's, nor for a path a profile or the shell would read as more than a path.
    #[test]
    fn the_step_names_the_binary_it_runs_as() {
        let step = step(&place("/usr/local/bin/tarae", Ok(()))).unwrap();
        let file = "/etc/apparmor.d/usr.local.bin.tarae";
        assert!(step.ends_with(&format!("| sudo tee {file} >/dev/null && sudo apparmor_parser -r {file}")));
        // (what printf writes is the profile: its `\n`s back as lines)
        let written = step.split('\'').nth(1).unwrap().replace("\\n", "\n");
        assert_eq!(written, profile("/usr/local/bin/tarae"));
        assert!(written.contains("/usr/local/bin/tarae flags=(unconfined) {\n  userns,"));
        let packaged = Place { packaged: true, ..place(PACKAGED, Ok(())) };
        assert_eq!(super::step(&packaged).as_deref(), Some("sudo apparmor_parser -r /etc/apparmor.d/tarae"));
        assert!(super::step(&place("/home/u/.cargo/bin/tarae", Err("uid 1000's".into()))).is_none());
        for odd in
            ["/opt/my tarae/tarae", "/opt/t*/tarae", "/opt/it's/tarae", "/opt/100%/tarae", "/opt/{a}/tarae"]
        {
            assert!(super::step(&place(odd, Ok(()))).is_none(), "{odd}");
        }
    }

    /// The hint says the tier and why, then the one way on: the step — saying what the profile changes beyond
    /// the runs: the user's terminal and tools keep user namespaces, Claude's and plugins' commands don't — or,
    /// for a tarae not only root can replace, never a profile for it, only where to install one that can have it.
    #[test]
    fn the_hint_gives_the_step_or_says_why_not() {
        let root = hint("host", &place("/usr/local/bin/tarae", Ok(())));
        assert!(root.starts_with("Claude's commands run in tier host here"), "{root}");
        assert!(root.contains("To give them namespaces of their own, run once"), "{root}");
        assert!(
            root.contains(
                "your terminal and tools keep user namespaces, as VS Code's do; Claude's and plugins' commands \
                 don't: printf '"
            ),
            "{root}"
        );
        let cargo = hint(
            "host",
            &place("/home/u/.cargo/bin/tarae", Err("/home/u/.cargo/bin/tarae is uid 1000's".into())),
        );
        assert!(cargo.contains("isn't (/home/u/.cargo/bin/tarae is uid 1000's)"), "{cargo}");
        assert!(!cargo.contains("sudo") && !cargo.contains("apparmor_parser"), "no step for it: {cargo}");
        let odd = hint("host", &place("/opt/my tarae/tarae", Ok(())));
        assert!(odd.contains("read as a pattern") && !odd.contains("sudo"), "{odd}");
    }
}
