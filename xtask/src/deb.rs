//! `cargo xtask deb [--binary PATH] [--out DIR]` — tarae as a `.deb` (step 6h): the binary at `/usr/bin/tarae`,
//! its AppArmor profile at `/etc/apparmor.d/tarae` (`packaging/apparmor/tarae`, a conffile), loaded by its
//! `postinst` where AppArmor is on and unloaded by its `postrm` (`packaging/deb/`). On Ubuntu with its restriction
//! on, that profile is what gives the sandbox's keeper its namespaces (`src/sandbox/apparmor.rs`) — and only a
//! package puts tarae at a path only root can replace.
//!
//! Made here, with no Debian tool: an `ar` archive of `debian-binary`, `control.tar.gz` and `data.tar.gz`, every
//! entry root's, times zeroed — the same input, the same file. The binary is a Linux one: built here on Linux
//! (`cargo build --release --locked`), else given (`--binary`); its architecture read from its ELF header.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, root};

const USAGE: &str = "usage: cargo xtask deb [--binary PATH] [--out DIR]";

/// The profile, as the package installs it (the app's `apparmor::PACKAGED_PROFILE`).
const PROFILE_AT: &str = "etc/apparmor.d/tarae";

pub fn run(args: &[String]) -> Result<()> {
    let (mut binary, mut out) = (None, None);
    let mut rest = args;
    while let [flag, value, more @ ..] = rest {
        match flag.as_str() {
            "--binary" => binary = Some(PathBuf::from(value)),
            "--out" => out = Some(PathBuf::from(value)),
            _ => return Err(format!("{flag}: {USAGE}")),
        }
        rest = more;
    }
    if let [extra, ..] = rest {
        return Err(format!("{extra}: {USAGE}"));
    }
    let root = root();
    let binary = match binary {
        Some(b) => b,
        None if cfg!(target_os = "linux") => built(&root)?,
        None => return Err(format!("not on Linux: --binary <a Linux build of tarae> — {USAGE}")),
    };
    let bin = std::fs::read(&binary).map_err(|e| format!("{}: {e}", binary.display()))?;
    let arch = arch_of(&bin)?;
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
    let version = version_of(&manifest)?;
    let read = |p: &str| std::fs::read(root.join(p)).map_err(|e| format!("{p}: {e}"));
    let parts = Parts {
        binary: bin,
        version: version.clone(),
        arch,
        profile: read("packaging/apparmor/tarae")?,
        postinst: read("packaging/deb/postinst")?,
        postrm: read("packaging/deb/postrm")?,
    };
    let deb = deb(&parts)?;
    let dir = out.unwrap_or_else(|| root.join("target/deb"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("tarae_{version}_{arch}.deb"));
    std::fs::write(&path, &deb).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("{} ({:.1} MB)", path.display(), deb.len() as f64 / 1e6);
    Ok(())
}

/// tarae built for release here; its binary.
fn built(root: &Path) -> Result<PathBuf> {
    let st = Command::new("cargo")
        .args(["build", "--release", "--locked", "--bin", "tarae"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if !st.success() {
        return Err("cargo build --release failed (above)".into());
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    Ok(target.join("release/tarae"))
}

/// What goes in.
struct Parts {
    binary: Vec<u8>,
    version: String,
    arch: &'static str,
    profile: Vec<u8>,
    postinst: Vec<u8>,
    postrm: Vec<u8>,
}

/// Debian's architecture of an ELF binary, from its header's machine.
fn arch_of(bin: &[u8]) -> Result<&'static str> {
    if bin.len() < 20 || &bin[..4] != b"\x7fELF" {
        return Err("not an ELF binary — a Linux build of tarae".into());
    }
    match u16::from_le_bytes([bin[18], bin[19]]) {
        62 => Ok("amd64"),
        183 => Ok("arm64"),
        m => Err(format!("an ELF binary for machine {m}: only x86_64 and aarch64 are packaged")),
    }
}

/// `[package] version` of tarae's manifest.
fn version_of(manifest: &str) -> Result<String> {
    let t: toml::Table = manifest.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
    t.get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "Cargo.toml: no [package] version".into())
}

/// The `.deb`'s bytes.
fn deb(p: &Parts) -> Result<Vec<u8>> {
    let kib = (p.binary.len() + p.profile.len()).div_ceil(1024);
    let control = format!(
        "Package: tarae\n\
         Version: {}\n\
         Architecture: {}\n\
         Maintainer: tarae <https://github.com/ethnw/tarae>\n\
         Installed-Size: {kib}\n\
         Section: editors\n\
         Priority: optional\n\
         Homepage: https://github.com/ethnw/tarae\n\
         Description: terminal code editor with Helix's keys and Claude built in\n \
         Its sandbox for Claude's commands makes user namespaces; the AppArmor\n \
         profile installed with it lets /usr/bin/tarae make them under Ubuntu's\n \
         restriction on unprivileged user namespaces.\n",
        p.version, p.arch
    );
    let control = tar_gz(&[
        Entry::Dir("./"),
        Entry::File("./control", 0o644, control.as_bytes()),
        Entry::File("./conffiles", 0o644, format!("/{PROFILE_AT}\n").as_bytes()),
        Entry::File("./postinst", 0o755, &p.postinst),
        Entry::File("./postrm", 0o755, &p.postrm),
    ])?;
    let profile_at = format!("./{PROFILE_AT}");
    let data = tar_gz(&[
        Entry::Dir("./"),
        Entry::Dir("./etc/"),
        Entry::Dir("./etc/apparmor.d/"),
        Entry::File(&profile_at, 0o644, &p.profile),
        Entry::Dir("./usr/"),
        Entry::Dir("./usr/bin/"),
        Entry::File("./usr/bin/tarae", 0o755, &p.binary),
    ])?;
    Ok(ar(&[("debian-binary", b"2.0\n"), ("control.tar.gz", &control), ("data.tar.gz", &data)]))
}

enum Entry<'a> {
    Dir(&'a str),
    File(&'a str, u32, &'a [u8]),
}

/// `entries` as a gzipped tar, each root's (uid and gid 0, named so), times zeroed.
fn tar_gz(entries: &[Entry]) -> Result<Vec<u8>> {
    let gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::best());
    let mut tar = tar::Builder::new(gz);
    for e in entries {
        let mut h = tar::Header::new_gnu();
        let (path, mode, data): (&str, u32, &[u8]) = match e {
            Entry::Dir(p) => {
                h.set_entry_type(tar::EntryType::Directory);
                (p, 0o755, &[])
            }
            Entry::File(p, m, d) => (p, *m, d),
        };
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_username("root").map_err(|e| e.to_string())?;
        h.set_groupname("root").map_err(|e| e.to_string())?;
        // (the name as dpkg-deb writes it, `./` and all — `append_data` would take the `./` off)
        let name = &mut h.as_gnu_mut().ok_or("a GNU header")?.name;
        if path.len() >= name.len() {
            return Err(format!("{path}: past a tar header's name"));
        }
        name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_cksum();
        tar.append(&h, data).map_err(|e| format!("{path}: {e}"))?;
    }
    tar.into_inner().and_then(|z| z.finish()).map_err(|e| e.to_string())
}

/// `members` as an `ar` archive, as dpkg reads one: each a 60-byte header (name, time 0, root's, mode 0644, size),
/// its bytes, a newline to an even length.
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        let header = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 0, 0, 0, "100644", data.len());
        out.extend(header.as_bytes());
        out.extend(*data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// An `ar` archive read back: (name, bytes) of each member.
    fn members(deb: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(&deb[..8], b"!<arch>\n");
        let (mut at, mut out) = (8, Vec::new());
        while at < deb.len() {
            let h = std::str::from_utf8(&deb[at..at + 60]).unwrap();
            assert_eq!(&h[58..], "`\n", "a header's end");
            let size: usize = h[48..58].trim().parse().unwrap();
            assert_eq!((h[16..28].trim(), h[28..34].trim(), h[34..40].trim()), ("0", "0", "0"));
            out.push((h[..16].trim().to_string(), deb[at + 60..at + 60 + size].to_vec()));
            at += 60 + size + size % 2;
        }
        out
    }

    /// A gzipped tar read back: (path, mode, uid, gid, owner's name, bytes) of each entry.
    fn entries(gz: &[u8]) -> Vec<(String, u32, u64, u64, String, Vec<u8>)> {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(gz));
        let mut out = Vec::new();
        for e in tar.entries().unwrap() {
            let mut e = e.unwrap();
            let h = e.header().clone();
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap();
            let path = e.path().unwrap().display().to_string();
            let user = h.username().unwrap().unwrap_or_default().to_string();
            out.push((path, h.mode().unwrap(), h.uid().unwrap(), h.gid().unwrap(), user, data));
        }
        out
    }

    fn elf(machine: u16) -> Vec<u8> {
        let mut b = b"\x7fELF".to_vec();
        b.resize(64, 0);
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b
    }

    /// The package dpkg reads: its three members in dpkg's order; the binary at `/usr/bin/tarae` and the profile
    /// — the repository's own file — at `/etc/apparmor.d/tarae`, both root's (a path only root can replace is the
    /// profile's whole condition), the binary executable; the profile a conffile (the user's local edits kept);
    /// the maintainer scripts executable and the repository's; the control naming this version and architecture.
    #[test]
    fn the_package_puts_the_profile_beside_a_root_owned_binary() {
        let root = root();
        let read = |p: &str| std::fs::read(root.join(p)).unwrap();
        let parts = Parts {
            binary: elf(62),
            version: "0.1.0".into(),
            arch: "amd64",
            profile: read("packaging/apparmor/tarae"),
            postinst: read("packaging/deb/postinst"),
            postrm: read("packaging/deb/postrm"),
        };
        let m = members(&deb(&parts).unwrap());
        let names: Vec<&str> = m.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["debian-binary", "control.tar.gz", "data.tar.gz"]);
        assert_eq!(m[0].1, b"2.0\n");
        let data = entries(&m[2].1);
        let file = |p: &str| data.iter().find(|e| e.0 == p).unwrap_or_else(|| panic!("{p} in {data:?}"));
        let bin = file("./usr/bin/tarae");
        assert_eq!((bin.1, bin.2, bin.3, bin.4.as_str(), &bin.5), (0o755, 0, 0, "root", &parts.binary));
        let profile = file("./etc/apparmor.d/tarae");
        assert_eq!((profile.1, profile.2, profile.3, &profile.5), (0o644, 0, 0, &parts.profile));
        assert!(data.iter().all(|e| e.2 == 0 && e.3 == 0), "every entry root's");
        let control = entries(&m[1].1);
        let of = |p: &str| control.iter().find(|e| e.0 == p).unwrap_or_else(|| panic!("{p}"));
        assert_eq!(of("./conffiles").5, b"/etc/apparmor.d/tarae\n");
        assert_eq!((of("./postinst").1, &of("./postinst").5), (0o755, &parts.postinst));
        assert_eq!((of("./postrm").1, &of("./postrm").5), (0o755, &parts.postrm));
        let fields = String::from_utf8(of("./control").5.clone()).unwrap();
        assert!(fields.contains("Package: tarae\nVersion: 0.1.0\nArchitecture: amd64\n"), "{fields}");
        assert!(
            fields.lines().skip_while(|l| !l.starts_with("Description:")).skip(1).all(|l| l.starts_with(' '))
        );
        assert_eq!(deb(&parts).unwrap(), deb(&parts).unwrap(), "the same input, the same file");
    }

    /// The architecture comes from the binary itself, not from where it's packaged: a binary built elsewhere is
    /// labelled as what it is, and anything but a Linux ELF of the two packaged machines is refused.
    #[test]
    fn the_architecture_is_the_binarys_own() {
        assert_eq!(arch_of(&elf(62)), Ok("amd64"));
        assert_eq!(arch_of(&elf(183)), Ok("arm64"));
        assert!(arch_of(&elf(40)).is_err(), "32-bit ARM");
        assert!(arch_of(b"\xcf\xfa\xed\xfe and the rest of a Mach-O").is_err());
        assert_eq!(version_of("[package]\nname = \"tarae\"\nversion = \"1.2.3\"\n"), Ok("1.2.3".into()));
    }
}
