//! A process's file mappings (`/proc/<pid>/maps`), kept so its addresses can be named after it has exited:
//! an address → (file, offset in that file), which is what the symbolizer reads the file by.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub path: PathBuf,
}

/// The file-backed regions of a `maps` text (anonymous ones, `[heap]`, `[vdso]` … left out).
pub fn parse(text: &str) -> Vec<Region> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (range, _perms, offset, _dev, _inode) =
                (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
            let path = f.collect::<Vec<_>>().join(" ");
            if !path.starts_with('/') || path.ends_with(" (deleted)") {
                return None;
            }
            let (s, e) = range.split_once('-')?;
            Some(Region {
                start: u64::from_str_radix(s, 16).ok()?,
                end: u64::from_str_radix(e, 16).ok()?,
                offset: u64::from_str_radix(offset, 16).ok()?,
                path: path.into(),
            })
        })
        .collect()
}

/// Where `addr` is: the file and the offset in it.
pub fn locate(regions: &[Region], addr: u64) -> Option<(&PathBuf, u64)> {
    regions.iter().find(|r| (r.start..r.end).contains(&addr)).map(|r| (&r.path, addr - r.start + r.offset))
}

/// A newer reading of a process's maps over an older one: the newer's regions, and the older's that it no
/// longer has (a library unloaded since still names the addresses taken while it was there).
pub fn merge(old: &[Region], new: Vec<Region>) -> Vec<Region> {
    let mut out = new;
    for r in old {
        if !out.iter().any(|n| n.start < r.end && r.start < n.end) {
            out.push(r.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
aaaab0000000-aaaab0100000 r--p 00000000 00:2f 123  /target/perf/tarae
aaaab0100000-aaaab0900000 r-xp 00100000 00:2f 123  /target/perf/tarae
aaaac0000000-aaaac0021000 rw-p 00000000 00:00 0    [heap]
ffff80000000-ffff80001000 r-xp 00000000 00:00 0    [vdso]
ffff90000000-ffff90180000 r-xp 00000000 fe:01 77   /usr/lib/aarch64-linux-gnu/libc.so.6
ffffa0000000-ffffa0001000 rw-p 00000000 00:00 0
ffffb0000000-ffffb0001000 r-xp 00002000 fe:01 9    /tmp/a dir/lib x.so
ffffc0000000-ffffc0001000 r-xp 00000000 fe:01 9    /tmp/gone.so (deleted)
";

    /// File-backed regions only, paths with spaces whole; an address → its file and the offset in it.
    #[test]
    fn regions_and_file_offsets() {
        let r = parse(MAPS);
        let paths: Vec<_> = r.iter().map(|r| r.path.to_str().unwrap()).collect();
        assert_eq!(
            paths,
            [
                "/target/perf/tarae",
                "/target/perf/tarae",
                "/usr/lib/aarch64-linux-gnu/libc.so.6",
                "/tmp/a dir/lib x.so"
            ]
        );
        let (p, off) = locate(&r, 0xaaaab0100010).unwrap();
        assert_eq!((p.to_str().unwrap(), off), ("/target/perf/tarae", 0x100010));
        assert_eq!(locate(&r, 0xffffb0000004).unwrap().1, 0x2004);
        assert_eq!(locate(&r, 0xaaaac0000010), None, "the heap");
    }

    /// A later reading wins; a region gone since stays for the addresses taken while it was there.
    #[test]
    fn a_newer_reading_keeps_what_it_lost() {
        let old = parse(MAPS);
        let new = parse("aaaab0000000-aaaab0100000 r--p 00000000 00:2f 123  /target/perf/tarae2\n");
        let m = merge(&old, new);
        assert_eq!(m[0].path.to_str().unwrap(), "/target/perf/tarae2");
        assert_eq!(m.len(), old.len(), "the overlapped one replaced, the rest kept");
    }
}
