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
}
