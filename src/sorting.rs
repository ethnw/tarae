//! One sort for lists that aren't hot. Every `sort_by(closure)` is a fresh copy of the standard library's
//! sort (drift + quick + small sorts, ~6 KB each — 79 of them were ~0.36 MB of the release binary); these
//! sort indices through `dyn`, so there is one copy, and put the items in that order. Stable, like
//! `sort_by`. A plain `sort()` is a copy per item type too (`sort`). Hot paths and long lists keep the
//! standard sort: the picker's ranking, a frame's highlight spans, a transaction's changes, the cursors,
//! the file tree, search results. clippy.toml keeps it so (2026-10-09: 0.1 MB had crept back): std's
//! sorts — `sort`, `sort_unstable` and the `sort_by` family — are disallowed outside tests, and a hot path says
//! why in its `#[expect]`.

use std::cmp::Ordering;

/// `v.sort_by(cmp)`, without a copy of the sort for it.
pub fn by<T>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let mut order: Vec<usize> = (0..v.len()).collect();
    sort_indices(&mut order, &mut |a, b| cmp(&v[a], &v[b]));
    permute(v, &mut order);
}

/// `v.sort_by_key(key)`, without a copy of the sort for it.
pub fn by_key<T, K: Ord>(v: &mut [T], mut key: impl FnMut(&T) -> K) {
    by(v, |a, b| key(a).cmp(&key(b)))
}

/// `v.sort()` — and `v.sort_unstable()`, stable here — without a copy of the sort for each item type.
pub fn sort<T: Ord>(v: &mut [T]) {
    by(v, T::cmp)
}

/// A folder's entries (those `read_dir` could read) in name order — bytes, on Unix — each with its name; `None`
/// when it holds more than `room`. A walk with a cap takes a folder whole or not at all, in this order: the order
/// `read_dir` lists in is the filesystem's (ext4's hash order, tmpfs' newest first, overlayfs', APFS'), another on
/// each machine, and what a walk cut short found would be too (CI's 7th round: `outside_keys` took a big folder
/// before the one holding a key on ext4, after it on the VM's overlayfs).
pub fn listing(dir: std::fs::ReadDir, room: usize) -> Option<Vec<(std::ffi::OsString, std::fs::DirEntry)>> {
    let mut v = Vec::new();
    for e in dir.flatten() {
        if v.len() == room {
            return None;
        }
        v.push((e.file_name(), e));
    }
    by(&mut v, |a, b| a.0.cmp(&b.0));
    Some(v)
}

/// A folder's first `max` entries as `read_dir` lists them — which ones is the filesystem's choice — in name
/// order, and whether it held more: for a list that shows what's there (a completion), where a folder of millions
/// is read only so far. Never for a walk whose cap decides what's found (`listing`).
pub fn listing_first(
    dir: std::fs::ReadDir,
    max: usize,
) -> (Vec<(std::ffi::OsString, std::fs::DirEntry)>, bool) {
    let mut v = Vec::new();
    let mut more = false;
    for e in dir.flatten() {
        if v.len() == max {
            more = true;
            break;
        }
        v.push((e.file_name(), e));
    }
    by(&mut v, |a, b| a.0.cmp(&b.0));
    (v, more)
}

#[inline(never)]
fn sort_indices(order: &mut [usize], cmp: &mut dyn FnMut(usize, usize) -> Ordering) {
    #[expect(clippy::disallowed_methods, reason = "the one copy of std's sort everything here goes through")]
    order.sort_by(|&a, &b| cmp(a, b));
}

/// Puts `v` in `order` (`v[i]` becomes the old `v[order[i]]`), following each cycle once.
fn permute<T>(v: &mut [T], order: &mut [usize]) {
    for i in 0..v.len() {
        let mut j = i;
        loop {
            let k = order[j];
            order[j] = j;
            if k == i {
                break;
            }
            v.swap(j, k);
            j = k;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sorts_as_the_standard_sort_does_stable_included() {
        let mut seed = 7u64;
        for n in [0, 1, 2, 3, 10, 100, 1000] {
            let v: Vec<(u8, usize)> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((seed >> 59) as u8, i)
                })
                .collect();
            let mut want = v.clone();
            want.sort_by_key(|p| p.0);
            let mut got = v.clone();
            super::by_key(&mut got, |p| p.0);
            assert_eq!(got, want, "n = {n}");
            let mut got = v.clone();
            super::by(&mut got, |a, b| b.0.cmp(&a.0));
            want.sort_by_key(|a| std::cmp::Reverse(a.0));
            assert_eq!(got, want, "n = {n}, reversed");
            let (mut got, mut want) = (v.clone(), v);
            super::sort(&mut got);
            want.sort();
            assert_eq!(got, want, "n = {n}, sort()");
        }
    }

    /// What `listing` adds to a walk (ci5: `outside_keys` sorts each folder before it queues it): the same folders
    /// listed whole, unsorted as `read_dir` gives them and through `listing` — `WALK_MAX`'s 20,000 entries in
    /// folders of 99 (`perf_a_walk_of_the_key_places`' scene), of 999 and one of 20,000; medians of 11. The
    /// difference is the sort and the names held until their folder is done.
    #[test]
    fn perf_a_listing_in_name_order() {
        if crate::test_results::unoptimized("sorting::tests::perf_a_listing_in_name_order") {
            return;
        }
        let t = crate::scratch::Scratch::new("perf-listing");
        let mut line = String::new();
        for (folders, each) in [(200usize, 99usize), (20, 999), (1, 20_000)] {
            let dirs: Vec<std::path::PathBuf> =
                (0..folders).map(|d| t.join(format!("{each}/d{d}"))).collect();
            for d in &dirs {
                std::fs::create_dir_all(d).unwrap();
                for f in 0..each {
                    // (names as a tree has them: not made in their order)
                    std::fs::write(d.join(format!("f{}.txt", (f * 7919) % each)), "").unwrap();
                }
            }
            let time = |sorted: bool| {
                let mut ms: Vec<f64> = (0..11)
                    .map(|_| {
                        let at = std::time::Instant::now();
                        for d in &dirs {
                            let rd = std::fs::read_dir(d).unwrap();
                            match sorted {
                                true => drop(std::hint::black_box(super::listing(rd, usize::MAX))),
                                false => drop(std::hint::black_box(rd.flatten().collect::<Vec<_>>())),
                            }
                        }
                        at.elapsed().as_secs_f64() * 1000.0
                    })
                    .collect();
                ms.sort_by(f64::total_cmp);
                ms[ms.len() / 2]
            };
            let (plain, sorted) = (time(false), time(true));
            line += &format!(
                "  {folders} x {each}: {plain:.2} ms as read_dir lists, {sorted:.2} ms in name order (+{:.2} ms, \
                 {:.3} µs an entry)\n",
                sorted - plain,
                (sorted - plain) * 1000.0 / (folders * each) as f64
            );
        }
        let said = format!("a listing in name order, 20,000 entries; medians of 11:\n{line}");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), said.as_bytes());
    }

    /// A folder's listing is in name order (bytes), whatever order its entries were made in or the file system
    /// lists them in, and whole or none of it past its room (`listing`: what a walk with a cap takes is the same on
    /// every machine — CI's 7th round, ext4's hash order against the VM's overlayfs').
    #[test]
    fn a_listing_is_in_name_order_and_whole_or_none() {
        let t = crate::scratch::Scratch::new("sorting-listing");
        let names = ["b", "a", "C", "a0", "_", "10", "9", "~"];
        let mut want = names.to_vec();
        want.sort();
        for (at, reversed) in [("made", false), ("made-reversed", true)] {
            let dir = t.join(at);
            std::fs::create_dir(&dir).unwrap();
            let mut made = names.to_vec();
            if reversed {
                made.reverse();
            }
            for n in made {
                std::fs::write(dir.join(n), "").unwrap();
            }
            let listed = super::listing(std::fs::read_dir(&dir).unwrap(), names.len()).expect("it fits");
            let got: Vec<String> = listed.into_iter().map(|(n, _)| n.into_string().unwrap()).collect();
            assert_eq!(got, want, "made {at}");
            assert!(
                super::listing(std::fs::read_dir(&dir).unwrap(), names.len() - 1).is_none(),
                "one past its room"
            );
        }
    }

    /// A completion's listing reads only so far (`listing_first`): `max` entries in name order and that there were
    /// more; all of a folder that fits, and none more.
    #[test]
    fn a_first_listing_reads_only_so_far_and_says_there_was_more() {
        let t = crate::scratch::Scratch::new("sorting-first");
        for n in ["c", "a", "b"] {
            std::fs::write(t.join(n), "").unwrap();
        }
        let names = |v: Vec<(std::ffi::OsString, std::fs::DirEntry)>| -> Vec<String> {
            v.into_iter().map(|(n, _)| n.into_string().unwrap()).collect()
        };
        let (all, more) = super::listing_first(std::fs::read_dir(t.path()).unwrap(), 3);
        assert_eq!((names(all), more), (vec!["a".into(), "b".into(), "c".into()], false));
        let (two, more) = super::listing_first(std::fs::read_dir(t.path()).unwrap(), 2);
        let two = names(two);
        assert!(more && two.len() == 2 && two[0] < two[1], "{two:?}");
    }
}
